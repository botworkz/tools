//! BUILD: booted-VM image builder using the shared `crate::plan` guest/host step runtime.
//! Produces a qcow2 by booting the source image under qemu, provisioning it via plan steps,
//! then gracefully shutting down and committing the disk as the output artifact.

use anyhow::{bail, Context, Result};
use clap::Args;
use shasset::fetch::{fetch_asset, FetchParams, MaterializeMode, Transport};
use shasset::manifest::load;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use crate::iso::{
    build_iso, detect_iso_tool, generate_installer_username, render_user_data, write_seed_files,
};
use crate::qemu::{qemu_build_args, require_kvm, spawn_qemu_with_log};
use crate::ssh::{scp_with_retry, ssh_with_retry, SshOptions, TemporarySshKeypair};
use crate::util::{
    create_temp_dir, default_cache_dir, ensure_command, materialize_flat, resolve_under_root,
    unique_suffix,
};

use crate::plan::step::{ArchiveStep, StepTarget, TestStep, UploadStep};
use crate::plan::{
    load_build_config, preserve_failed_build_disk, print_log_tail, run_step_flow,
    shutdown_build_vm, validate_build_steps, vm::StepTimeoutPolicy,
};

#[derive(Args, Debug)]
pub(crate) struct BuildArgs {
    /// Path to type-build YAML spec.
    #[arg(long, required = true)]
    spec: PathBuf,
    /// Source qcow2 image path (optional local override). When provided, overrides the
    /// `image:` shasset resolution and boots this file directly. Read-only; copied
    /// to <output>.partial before any modification.
    #[arg(long)]
    source: Option<PathBuf>,
    /// Output qcow2 path. Materialized atomically from <output>.partial on success.
    #[arg(long, required = true)]
    output: PathBuf,
    /// Repo root for resolving relative spec/source/output/step paths (default: current dir).
    #[arg(long)]
    repo_root: Option<PathBuf>,
    /// Cache directory for resolved shasset assets (default: ~/.cache/shasset).
    /// Honours SHASSET_CACHE / XDG_CACHE_HOME / HOME, same as `botforge deps`.
    #[arg(long)]
    cache_dir: Option<PathBuf>,
    /// SSH host forwarded port.
    #[arg(long, default_value_t = 2222)]
    ssh_port: u16,
    /// SSH host.
    #[arg(long, default_value = "127.0.0.1")]
    ssh_host: String,
    /// SSH user. When omitted (the default), botforge generates a private ephemeral installer
    /// account (`botforge-<suffix>`) with passwordless sudo, seeds it via cloud-init, provisions
    /// as that user, and removes it before committing the image. When explicitly provided,
    /// botforge connects as that existing user without creating or deleting it (the caller is
    /// responsible for ensuring the user exists in the base image and can accept connections).
    #[arg(long)]
    ssh_user: Option<String>,
}

pub(crate) fn cmd_build(config: &Path, args: BuildArgs) -> Result<()> {
    require_kvm()?;
    ensure_command("qemu-system-x86_64")?;
    ensure_command("qemu-img")?;
    ensure_command("ssh")?;
    ensure_command("scp")?;
    ensure_command("ssh-keygen")?;
    detect_iso_tool()?;

    let repo_root = std::fs::canonicalize(
        args.repo_root
            .unwrap_or(std::env::current_dir().context("failed to determine current directory")?),
    )
    .context("failed to resolve repo root")?;

    let spec_path = resolve_under_root(&repo_root, args.spec.clone());
    let output = resolve_under_root(&repo_root, args.output.clone());

    let build_config = load_build_config(&repo_root, &spec_path)?;
    validate_build_steps(&build_config.steps)?;
    if build_config
        .steps
        .iter()
        .any(|step| matches!(step, TestStep::Archive(_)))
    {
        ensure_command("tar")?;
    }

    // Resolve the source qcow2: --source wins; otherwise fetch image via shasset.
    let source = if let Some(src) = args.source {
        resolve_under_root(&repo_root, src)
    } else {
        let crate::plan::config::ImageRef::ShassetDefault(ref shasset_name) = build_config.image;
        resolve_base_image(config, shasset_name, args.cache_dir.as_deref())?
    };

    if !source.is_file() {
        bail!("source qcow2 not found: {}", source.display());
    }
    if let Some(parent) = output.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("cannot create output dir: {}", parent.display()))?;
    }

    // ---------------------------------------------------------------------------
    // Determine installer identity: ephemeral (botforge-owned) or caller-supplied.
    // ---------------------------------------------------------------------------
    // When --ssh-user is omitted, botforge generates a private per-run installer
    // account (botforge-<suffix>) with passwordless sudo, creates it via cloud-init,
    // and removes it before committing the image so it never ships.
    // When --ssh-user is provided, botforge connects as that existing user and does
    // NOT create or delete it (caller is responsible for the account's existence).
    let (installer_user, botforge_owned) = match args.ssh_user {
        Some(user) => (user, false),
        None => (generate_installer_username(), true),
    };

    // ---------------------------------------------------------------------------
    // Disk lifecycle step 1: copy source → <output>.partial
    // ---------------------------------------------------------------------------
    let partial = partial_path(&output);
    let failed_partial = failed_partial_path(&output);
    // Clear any stale .partial from a previous failed run.
    if partial.exists() {
        std::fs::remove_file(&partial).with_context(|| {
            format!("cannot remove stale partial output: {}", partial.display())
        })?;
    }
    copy_qcow2(&source, &partial)?;

    // ---------------------------------------------------------------------------
    // Disk lifecycle step 2: qemu-img resize <partial> <disk_size>
    // ---------------------------------------------------------------------------
    resize_qcow2(&partial, &build_config.disk_size)?;

    // ---------------------------------------------------------------------------
    // Disk lifecycle step 3: boot <partial> read-write directly (no CoW overlay)
    // ---------------------------------------------------------------------------
    let build_dir = repo_root.join("build");
    std::fs::create_dir_all(&build_dir)
        .with_context(|| format!("cannot create build dir: {}", build_dir.display()))?;
    let output_stem = output_stem(&output);
    let seed_iso = build_dir.join(format!("{output_stem}-seed.iso"));
    let vm_log = build_dir.join(format!("{output_stem}-vm.log"));
    let ssh_keypair = TemporarySshKeypair::generate("botforge-build-ssh")?;
    let seed_dir = create_temp_dir("botforge-build-seed")?;

    let ssh_public_key = std::fs::read_to_string(ssh_keypair.public_key()).with_context(|| {
        format!(
            "cannot read SSH public key: {}",
            ssh_keypair.public_key().display()
        )
    })?;

    // When botforge owns the installer user, seed it with passwordless sudo so the harness
    // can run `sudo cloud-init status --wait`, provisioner steps, and final teardown.
    // When the caller supplied an explicit --ssh-user, we do NOT create that user in
    // cloud-init (the caller owns the account); just inject the ephemeral key for the
    // default cloud-init user via the top-level ssh_authorized_keys path.
    let user_data = if botforge_owned {
        render_user_data(None, ssh_public_key.trim(), Some(installer_user.as_str()))
    } else {
        render_user_data(None, ssh_public_key.trim(), None)
    };
    write_seed_files(&seed_dir, &user_data)?;
    build_iso(&seed_dir, &seed_iso, "cidata")?;
    std::fs::remove_dir_all(&seed_dir)
        .with_context(|| format!("cannot remove temp seed dir: {}", seed_dir.display()))?;

    let qemu_args = qemu_build_args(
        &partial,
        &seed_iso,
        args.ssh_port,
        build_config.memsize,
        build_config.smp,
    );
    let mut vm_child = Some(spawn_qemu_with_log(&qemu_args, &vm_log)?);
    let ssh_options = SshOptions {
        host: args.ssh_host.clone(),
        port: args.ssh_port,
        user: installer_user.clone(),
        key: ssh_keypair.private_key().to_path_buf(),
    };

    // ---------------------------------------------------------------------------
    // Disk lifecycle step 4: run build steps via shared flow
    // ---------------------------------------------------------------------------
    let mut archive_executor = |step_idx: usize, step: &ArchiveStep| -> Result<()> {
        run_archive_step(
            config,
            &build_dir,
            args.cache_dir.as_deref(),
            step_idx,
            step,
            &ssh_options,
        )
    };
    let mut upload_executor = |step_idx: usize, step: &UploadStep| -> Result<()> {
        run_upload_step(
            config,
            &repo_root,
            args.cache_dir.as_deref(),
            step_idx,
            step,
            &ssh_options,
        )
    };
    let step_result = run_step_flow(
        &repo_root,
        &build_config.steps,
        &ssh_options,
        &[],
        StepTimeoutPolicy {
            overall_timeout: std::time::Duration::from_secs(build_config.timeout),
            default_step_timeout: std::time::Duration::from_secs(build_config.step_timeout),
            cloud_init_timeout: std::time::Duration::from_secs(build_config.cloud_init_timeout),
        },
        Some(&mut archive_executor),
        Some(&mut upload_executor),
    );
    let overall_deadline = match step_result {
        Ok(overall_deadline) => overall_deadline,
        Err(err) => {
            eprintln!("build steps failed: {err:#}");
            print_log_tail(&vm_log, 200);
            // Kill the VM. On step failure the partial is tainted — preserve it at
            // <output>.partial.failed for post-mortem instead of the stale .partial path.
            if let Some(child) = vm_child.as_mut() {
                let _ = child.kill();
                let _ = child.wait();
            }
            if let Err(preserve_err) = preserve_failed_build_disk(&partial, &failed_partial) {
                return Err(err.context(format!(
                    "additionally failed to preserve tainted partial at {}: {preserve_err:#}",
                    failed_partial.display()
                )));
            }
            eprintln!(
                "tainted partial disk left at {} for post-mortem",
                failed_partial.display()
            );
            return Err(err);
        }
    };

    // ---------------------------------------------------------------------------
    // Disk lifecycle step 5: installer teardown (botforge-owned identity only)
    //
    // Remove the ephemeral installer account from the guest before committing the
    // image, so it never ships. This is botforge's last guest action: it queues a
    // detached root-owned systemd service that waits for the SSH caller to return,
    // deletes the installer, and only then powers off. A failure here is a hard
    // error: a shipped image must not contain the installer account.
    //
    // On the failure path (step 4 or teardown error), the VM is killed and the
    // tainted disk is preserved at <output>.partial.failed for post-mortem;
    // leaving the installer present in that tainted disk is acceptable.
    // ---------------------------------------------------------------------------
    if botforge_owned {
        let teardown_result =
            run_installer_teardown(&ssh_options, &installer_user, overall_deadline);
        if let Err(err) = teardown_result {
            eprintln!("installer teardown failed: {err:#}");
            print_log_tail(&vm_log, 200);
            if let Some(child) = vm_child.as_mut() {
                let _ = child.kill();
                let _ = child.wait();
            }
            if let Err(preserve_err) = preserve_failed_build_disk(&partial, &failed_partial) {
                return Err(err.context(format!(
                    "additionally failed to preserve tainted partial at {}: {preserve_err:#}",
                    failed_partial.display()
                )));
            }
            eprintln!(
                "tainted partial disk left at {} for post-mortem",
                failed_partial.display()
            );
            return Err(err);
        }
        // Teardown queued the final cleanup/poweroff service; shutdown_build_vm
        // only needs to wait for the VM to exit.
    }

    // ---------------------------------------------------------------------------
    // Disk lifecycle step 6 (was 5): graceful shutdown
    // ---------------------------------------------------------------------------
    let shutdown_result = shutdown_build_vm(
        &mut vm_child,
        &partial,
        &failed_partial,
        &ssh_options,
        !botforge_owned,
        overall_deadline,
        std::time::Duration::from_secs(build_config.timeout),
    );
    if let Err(err) = shutdown_result {
        eprintln!("build VM shutdown failed: {err:#}");
        print_log_tail(&vm_log, 200);
        return Err(err);
    }

    // ---------------------------------------------------------------------------
    // Disk lifecycle step 7 (was 6): atomic rename partial → output
    // ---------------------------------------------------------------------------
    std::fs::rename(&partial, &output).with_context(|| {
        format!(
            "cannot atomically materialize output from {} to {}",
            partial.display(),
            output.display()
        )
    })?;

    println!("built image at {}", output.display());
    Ok(())
}

/// Resolve the `image:` shasset asset key to a local qcow2 path.
///
/// Mirrors `deps.rs`'s `fetch_asset` pattern: loads the manifest, looks up the
/// asset, fetches + verifies + caches it via the shasset library, then materializes
/// a copy into the build cache dir so the cached blob is never mutated by qemu.
fn resolve_base_image(
    manifest_path: &Path,
    asset_key: &str,
    cache_dir_override: Option<&Path>,
) -> Result<PathBuf> {
    resolve_base_image_with_transport(manifest_path, asset_key, cache_dir_override, || None)
}

fn resolve_base_image_with_transport<F>(
    manifest_path: &Path,
    asset_key: &str,
    cache_dir_override: Option<&Path>,
    mut transport_factory: F,
) -> Result<PathBuf>
where
    F: FnMut() -> Option<Box<dyn Transport>>,
{
    let manifest = load(manifest_path)
        .with_context(|| format!("cannot load shasset manifest: {}", manifest_path.display()))?;
    let cache_dir = cache_dir_override
        .map(|p| p.to_path_buf())
        .unwrap_or_else(default_cache_dir);

    let asset = manifest.assets.get(asset_key).with_context(|| {
        format!(
            "asset '{asset_key}' not found in manifest {}",
            manifest_path.display()
        )
    })?;

    let uri = asset.expanded_uri();
    if uri.starts_with("oci://") {
        bail!(
            "image asset '{asset_key}' is an oci:// image; \
             image must resolve to a qcow2 file asset"
        );
    }
    if asset.checksum.is_none() {
        eprintln!(
            "warning: image asset '{asset_key}' has no checksum; \
             integrity will not be verified"
        );
    }

    let fetched = fetch_asset(FetchParams {
        name: asset_key,
        asset,
        out_dir: None,
        cache_dir: &cache_dir,
        retries: manifest.settings.retries,
        backoff: &manifest.settings.backoff,
        compute_checksum: true,
        no_reverify: false,
        materialize_mode: MaterializeMode::Copy,
        transport: transport_factory(),
    })
    .with_context(|| format!("failed to fetch image asset '{asset_key}'"))?;

    // Materialize a copy into the cache dir so qemu boots a mutable copy
    // without polluting the shasset blob cache.
    let filename = asset
        .output_filename()
        .with_context(|| format!("image asset '{asset_key}': cannot determine output filename"))?;
    let out_dir = cache_dir.join("base-images");
    let qcow2_path = materialize_flat(&fetched.blob_path, &out_dir, &filename, false)
        .with_context(|| format!("failed to stage image asset '{asset_key}'"))?;

    Ok(qcow2_path)
}

fn run_archive_step(
    manifest_path: &Path,
    build_dir: &Path,
    cache_dir_override: Option<&Path>,
    step_idx: usize,
    step: &ArchiveStep,
    ssh: &SshOptions,
) -> Result<()> {
    use crate::util::shell_single_quote;

    let src = step.archive.src.trim();
    let asset_key = parse_archive_asset_key(src)?;

    let manifest = load(manifest_path)
        .with_context(|| format!("cannot load shasset manifest: {}", manifest_path.display()))?;
    let cache_dir = cache_dir_override
        .map(|p| p.to_path_buf())
        .unwrap_or_else(default_cache_dir);
    let asset = manifest.assets.get(asset_key).with_context(|| {
        format!(
            "archive step '{}': asset '{}' not found in manifest {}",
            step.archive.name.as_deref().unwrap_or(src),
            asset_key,
            manifest_path.display()
        )
    })?;

    let fetched = fetch_asset(FetchParams {
        name: asset_key,
        asset,
        out_dir: None,
        cache_dir: &cache_dir,
        retries: manifest.settings.retries,
        backoff: &manifest.settings.backoff,
        compute_checksum: true,
        no_reverify: false,
        materialize_mode: MaterializeMode::Copy,
        transport: None,
    })
    .with_context(|| format!("failed to fetch archive asset '{asset_key}'"))?;

    // Retry constants mirror the vm.rs run-step upload path.
    const RETRIES: usize = 10;
    const RETRY_DELAY: Duration = Duration::from_secs(2);

    if step.target == Some(StepTarget::Guest) {
        // ------------------------------------------------------------------
        // Guest mode: scp the fetched archive blob into the guest, then
        // untar it there via SSH using `sudo tar`.
        // ------------------------------------------------------------------
        let dest = step
            .archive
            .dest
            .as_deref()
            .expect("dest validated as present for on: guest");

        // Derive a temp filename that preserves the archive extension so that
        // tar can auto-detect the compression format.
        let ext = fetched
            .blob_path
            .extension()
            .map(|e| format!(".{}", e.to_string_lossy()))
            .unwrap_or_default();
        let suffix = unique_suffix();
        let remote_archive = format!("/tmp/botforge-archive-{step_idx}-{suffix}{ext}");

        // Step 1: Verify that `tar` is available in the guest before doing any
        // transport, so the error is clear if the guest image lacks tar.
        ssh_with_retry(
            ssh,
            "command -v tar >/dev/null 2>&1",
            1,
            Duration::from_secs(0),
            Duration::from_secs(10),
        )
        .with_context(|| {
            format!(
                "archive step '{}': `tar` is not available in the guest; \
                 install tar in the base image before using `on: guest` archive steps",
                step.archive.name.as_deref().unwrap_or(src)
            )
        })?;

        // Step 2: scp the archive blob to the guest temp path.
        scp_with_retry(
            ssh,
            &fetched.blob_path,
            &remote_archive,
            RETRIES,
            RETRY_DELAY,
        )
        .with_context(|| {
            format!(
                "archive step '{}': failed to scp archive to guest",
                step.archive.name.as_deref().unwrap_or(src)
            )
        })?;

        // Step 3: mkdir + untar inside the guest (sudo for system paths).
        let dest_q = shell_single_quote(dest);
        let remote_archive_q = shell_single_quote(&remote_archive);
        let untar_cmd = guest_untar_command(&dest_q, &remote_archive_q);
        let untar_result = ssh_with_retry(
            ssh,
            &untar_cmd,
            1,
            Duration::from_secs(0),
            Duration::from_secs(300),
        )
        .with_context(|| {
            format!(
                "archive step '{}': tar extraction into guest path '{}' failed",
                step.archive.name.as_deref().unwrap_or(src),
                dest
            )
        });

        // Step 4: Best-effort cleanup of the remote temp archive.
        let _ = ssh_with_retry(
            ssh,
            &format!("rm -f {remote_archive_q}"),
            1,
            Duration::from_secs(0),
            Duration::from_secs(10),
        );

        untar_result?;

        println!(
            "archive step {} ('{}') extracted into guest at {}",
            step_idx + 1,
            step.archive.name.as_deref().unwrap_or(src),
            dest
        );
    } else {
        // ------------------------------------------------------------------
        // Host mode (default): unpack into build/archives/<id>/ (unchanged).
        // ------------------------------------------------------------------
        let relative_unpacked = archive_unpack_relative_path(
            asset_key,
            step.archive.into.as_deref(),
            asset.checksum.as_deref(),
        );
        let relative_within_build = relative_unpacked
            .strip_prefix(Path::new("build"))
            .unwrap_or(&relative_unpacked);
        let unpack_dir = build_dir.join(relative_within_build);
        unpack_archive_to_dir(&fetched.blob_path, &unpack_dir).with_context(|| {
            format!(
                "archive step '{}': failed to unpack {} into {}",
                step.archive.name.as_deref().unwrap_or(src),
                fetched.blob_path.display(),
                unpack_dir.display()
            )
        })?;

        // TODO: Thread archive outputs into step-input resolution so future steps can consume
        // them directly via @://build/<path> without manual path wiring.
        println!(
            "archive step {} ('{}') unpacked to {}",
            step_idx + 1,
            step.archive.name.as_deref().unwrap_or(src),
            relative_unpacked.display()
        );
    }

    Ok(())
}

fn run_upload_step(
    manifest_path: &Path,
    repo_root: &Path,
    cache_dir_override: Option<&Path>,
    step_idx: usize,
    step: &UploadStep,
    ssh: &SshOptions,
) -> Result<()> {
    use crate::util::shell_single_quote;

    let spec = &step.upload;
    let src = spec.src.trim();
    let dest = spec.dest.as_str();
    let name = spec.name.as_deref().unwrap_or(src);

    // Retry constants mirror the archive step and run-step upload paths.
    const RETRIES: usize = 10;
    const RETRY_DELAY: Duration = Duration::from_secs(2);

    // Resolve the source blob: shasset ref (`@<name>`) or repo-relative path.
    let local_blob: PathBuf = if let Some(asset_key) = src.strip_prefix('@') {
        // External: fetch from shasset manifest.
        let manifest = load(manifest_path).with_context(|| {
            format!("cannot load shasset manifest: {}", manifest_path.display())
        })?;
        let cache_dir = cache_dir_override
            .map(|p| p.to_path_buf())
            .unwrap_or_else(default_cache_dir);
        let asset = manifest.assets.get(asset_key).with_context(|| {
            format!(
                "upload step '{}': asset '{}' not found in manifest {}",
                name,
                asset_key,
                manifest_path.display()
            )
        })?;
        let fetched = fetch_asset(FetchParams {
            name: asset_key,
            asset,
            out_dir: None,
            cache_dir: &cache_dir,
            retries: manifest.settings.retries,
            backoff: &manifest.settings.backoff,
            compute_checksum: true,
            no_reverify: false,
            materialize_mode: MaterializeMode::Copy,
            transport: None,
        })
        .with_context(|| format!("failed to fetch upload asset '{asset_key}'"))?;
        fetched.blob_path
    } else {
        // Internal: repo-relative path, resolved under repo_root.
        resolve_under_root(repo_root, PathBuf::from(src))
    };

    // SCP the resolved blob verbatim to `dest` in the guest (no extraction).
    let dest_q = shell_single_quote(dest);
    let parent = std::path::Path::new(dest)
        .parent()
        .map(|p| p.to_string_lossy().into_owned())
        .filter(|p| !p.is_empty() && p != "/");
    if let Some(parent_dir) = parent {
        ssh_with_retry(
            ssh,
            &format!("sudo mkdir -p {}", shell_single_quote(&parent_dir)),
            1,
            Duration::from_secs(0),
            Duration::from_secs(30),
        )
        .with_context(|| {
            format!(
                "upload step '{}': failed to create parent directory '{}' in guest",
                name, parent_dir
            )
        })?;
    }

    // Step 1: SCP blob to a temp path in the guest.
    let suffix = unique_suffix();
    let remote_tmp = format!("/tmp/botforge-upload-{step_idx}-{suffix}");
    scp_with_retry(ssh, &local_blob, &remote_tmp, RETRIES, RETRY_DELAY)
        .with_context(|| format!("upload step '{}': failed to scp file to guest", name))?;

    // Step 2: Move the temp file to the final destination (sudo for system paths).
    let remote_tmp_q = shell_single_quote(&remote_tmp);
    let mv_result = ssh_with_retry(
        ssh,
        &format!("sudo mv {remote_tmp_q} {dest_q}"),
        1,
        Duration::from_secs(0),
        Duration::from_secs(30),
    )
    .with_context(|| {
        format!(
            "upload step '{}': failed to move file to '{}' in guest",
            name, dest
        )
    });

    // Best-effort cleanup of the remote temp file on failure.
    if mv_result.is_err() {
        let _ = ssh_with_retry(
            ssh,
            &format!("rm -f {remote_tmp_q}"),
            1,
            Duration::from_secs(0),
            Duration::from_secs(10),
        );
    }

    mv_result?;

    println!(
        "upload step {} ('{}') placed at {}",
        step_idx + 1,
        name,
        dest
    );

    Ok(())
}

fn parse_archive_asset_key(src: &str) -> Result<&str> {
    if src.is_empty() {
        bail!("archive `src` is required");
    }
    if src.starts_with("@://") {
        bail!("archive `src` does not support '@://' traversal");
    }
    let Some(asset_key) = src.strip_prefix('@') else {
        bail!("archive `src` must start with '@'");
    };
    if asset_key.trim().is_empty() {
        bail!("archive `src` must include a shasset name after '@'");
    }
    Ok(asset_key)
}

fn archive_unpack_relative_path(
    asset_key: &str,
    into_hint: Option<&str>,
    checksum: Option<&str>,
) -> PathBuf {
    let hint = into_hint
        .map(str::trim)
        .filter(|hint| !hint.is_empty())
        .unwrap_or(asset_key);
    let slug = sanitize_archive_hint(hint);
    // Deterministic, reasonably collision-resistant id from archive identity inputs.
    let hash = archive_identity_hash_hex(asset_key, into_hint, checksum);
    PathBuf::from("build")
        .join("archives")
        .join(format!("{slug}-{hash}"))
}

fn sanitize_archive_hint(hint: &str) -> String {
    let mut out = String::with_capacity(hint.len());
    for ch in hint.chars() {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
            out.push(ch);
        } else {
            out.push('-');
        }
    }
    let trimmed = out.trim_matches('-');
    if trimmed.is_empty() {
        "archive".to_string()
    } else {
        trimmed.to_string()
    }
}

fn archive_identity_hash_hex(
    asset_key: &str,
    into_hint: Option<&str>,
    checksum: Option<&str>,
) -> String {
    const FNV_OFFSET_BASIS: u64 = 0xcbf29ce484222325;
    const FNV_PRIME: u64 = 0x100000001b3;
    let mut hash = FNV_OFFSET_BASIS;
    for byte in asset_key
        .as_bytes()
        .iter()
        .chain([0xff].iter())
        .chain(into_hint.unwrap_or("").as_bytes().iter())
        .chain([0xfe].iter())
        .chain(checksum.unwrap_or("").as_bytes().iter())
    {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    format!("{hash:016x}")
}

/// Build the guest-side untar command for an `on: guest` archive step.
///
/// Both `dest_q` and `remote_archive_q` must already be POSIX single-quoted
/// (via [`crate::util::shell_single_quote`]).
fn guest_untar_command(dest_q: &str, remote_archive_q: &str) -> String {
    format!("sudo mkdir -p {dest_q} && sudo tar -xf {remote_archive_q} -C {dest_q}")
}

fn unpack_archive_to_dir(archive_path: &Path, destination_dir: &Path) -> Result<()> {
    std::fs::create_dir_all(destination_dir).with_context(|| {
        format!(
            "cannot create archive unpack directory: {}",
            destination_dir.display()
        )
    })?;
    let status = Command::new("tar")
        .arg("-xf")
        .arg(archive_path)
        .arg("-C")
        .arg(destination_dir)
        .arg("--no-same-owner")
        .arg("--no-same-permissions")
        .status()
        .context("failed to execute tar")?;
    if !status.success() {
        bail!("tar extraction failed (exit status: {status})");
    }
    Ok(())
}

/// Build the remote command that removes the botforge-owned installer and powers off.
///
/// The command launches a detached transient system service as root. That service
/// waits briefly for the calling SSH command to return, terminates any remaining
/// installer processes, deletes the installer account + home, removes the
/// cloud-init sudoers drop-in, and only then powers off the guest.
fn installer_teardown_command(installer: &str) -> String {
    format!(
        "sudo systemd-run --quiet --unit botforge-installer-teardown-{installer} --collect \
         /bin/bash -lc 'set -euo pipefail; \
         sleep 2; \
         loginctl terminate-user {installer} >/dev/null 2>&1 || true; \
         while pgrep -u {installer} >/dev/null 2>&1; do sleep 0.2; done; \
         userdel -f {installer}; \
         rm -rf /home/{installer}; \
         rm -f /etc/sudoers.d/90-cloud-init-users; \
         systemctl poweroff'"
    )
}

/// Remove the botforge-owned ephemeral installer user from the guest and power off.
///
/// The deletion cannot run directly inside the installer's own live SSH session:
/// `userdel` will refuse while the session's sshd/logind processes still exist.
/// Instead botforge queues a detached root-owned systemd service that performs the
/// deletion after the SSH caller has returned, then powers off the guest.
///
/// Returns `Err` if any step fails so the caller can surface it as a hard error
/// (the image must not ship with the installer present).
fn run_installer_teardown(
    ssh: &SshOptions,
    installer: &str,
    overall_deadline: Instant,
) -> Result<()> {
    let timeout = overall_deadline
        .saturating_duration_since(Instant::now())
        .min(Duration::from_secs(60));

    let cmd = installer_teardown_command(installer);

    ssh_with_retry(ssh, &cmd, 1, Duration::from_secs(0), timeout).with_context(|| {
        format!(
            "installer teardown failed: could not remove installer user '{installer}'; \
             the committed image must not contain the installer account"
        )
    })
}

/// Returns `<output>.partial` — the in-progress disk path during the build.
fn partial_path(output: &Path) -> PathBuf {
    let mut name = output
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    name.push(".partial");
    let parent = output.parent().unwrap_or_else(|| Path::new("."));
    parent.join(name)
}

/// Returns `<output>.partial.failed` — the tainted disk path preserved on build failure.
fn failed_partial_path(output: &Path) -> PathBuf {
    let mut name = output
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    name.push(".partial.failed");
    let parent = output.parent().unwrap_or_else(|| Path::new("."));
    parent.join(name)
}

fn output_stem(output: &Path) -> String {
    output
        .file_stem()
        .or_else(|| output.file_name())
        .map(|stem| stem.to_string_lossy().into_owned())
        .filter(|stem| !stem.is_empty())
        .unwrap_or_else(|| "build".to_string())
}

/// Copy a qcow2 image, preferring `cp --reflink=auto` for instant CoW on
/// btrfs/xfs; falls back to a regular `fs::copy` when reflink isn't available.
fn copy_qcow2(source: &Path, partial: &Path) -> Result<()> {
    let status = Command::new("cp")
        .arg("--reflink=auto")
        .arg(source)
        .arg(partial)
        .status()
        .context("failed to execute cp")?;
    if status.success() {
        return Ok(());
    }
    std::fs::copy(source, partial).with_context(|| {
        format!(
            "cannot copy source qcow2 from {} to {}",
            source.display(),
            partial.display()
        )
    })?;
    Ok(())
}

fn resize_qcow2(disk: &Path, size: &str) -> Result<()> {
    let status = Command::new("qemu-img")
        .arg("resize")
        .arg(disk)
        .arg(size)
        .status()
        .context("failed to execute qemu-img resize")?;
    if !status.success() {
        bail!("qemu-img resize failed (exit status: {status})");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        archive_unpack_relative_path, failed_partial_path, guest_untar_command,
        installer_teardown_command, output_stem, parse_archive_asset_key, partial_path,
        resolve_base_image_with_transport, unpack_archive_to_dir,
    };
    use crate::cli::Cli;
    use clap::Parser;
    use shasset::fetch::{DownloadResponse, FetchError, Transport};
    use std::io::Cursor;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use tempfile::TempDir;

    struct MockTransport {
        expected_uri: String,
        body: Vec<u8>,
    }

    impl Transport for MockTransport {
        fn get(
            &self,
            uri: &str,
            _auth: Option<&str>,
            accept: Option<&str>,
        ) -> std::result::Result<DownloadResponse, FetchError> {
            assert_eq!(uri, self.expected_uri);
            assert!(accept.is_none());
            Ok(DownloadResponse {
                body: Box::new(Cursor::new(self.body.clone())),
                content_length: Some(self.body.len() as u64),
            })
        }
    }

    #[test]
    fn partial_path_appends_partial_suffix() {
        let out = partial_path(Path::new("/build/out.qcow2"));
        assert_eq!(out, Path::new("/build/out.qcow2.partial"));
    }

    #[test]
    fn partial_path_nested_directory() {
        let out = partial_path(Path::new("/a/b/c/image.qcow2"));
        assert_eq!(out, Path::new("/a/b/c/image.qcow2.partial"));
    }

    #[test]
    fn failed_partial_path_appends_failed_suffix() {
        let out = failed_partial_path(Path::new("/build/out.qcow2"));
        assert_eq!(out, Path::new("/build/out.qcow2.partial.failed"));
    }

    #[test]
    fn installer_teardown_command_queues_detached_cleanup_service() {
        let cmd = installer_teardown_command("botforge-deadbeefdeadbeefdead");
        assert_eq!(
            cmd,
            "sudo systemd-run --quiet --unit botforge-installer-teardown-botforge-deadbeefdeadbeefdead --collect \
             /bin/bash -lc 'set -euo pipefail; \
             sleep 2; \
             loginctl terminate-user botforge-deadbeefdeadbeefdead >/dev/null 2>&1 || true; \
             while pgrep -u botforge-deadbeefdeadbeefdead >/dev/null 2>&1; do sleep 0.2; done; \
             userdel -f botforge-deadbeefdeadbeefdead; \
             rm -rf /home/botforge-deadbeefdeadbeefdead; \
             rm -f /etc/sudoers.d/90-cloud-init-users; \
             systemctl poweroff'"
        );
    }

    #[test]
    fn output_stem_uses_output_file_stem() {
        assert_eq!(output_stem(Path::new("/build/out.qcow2")), "out");
    }

    #[test]
    fn archive_asset_key_requires_at_prefix() {
        let err = parse_archive_asset_key("tool").unwrap_err();
        assert!(format!("{err:#}").contains("must start with '@'"));
    }

    #[test]
    fn archive_asset_key_rejects_traversal_scheme() {
        let err = parse_archive_asset_key("@://build/tool").unwrap_err();
        assert!(format!("{err:#}").contains("@://"));
    }

    #[test]
    fn archive_asset_key_strips_prefix() {
        let key = parse_archive_asset_key("@some-tool").unwrap();
        assert_eq!(key, "some-tool");
    }

    #[test]
    fn archive_unpack_relative_path_is_deterministic_and_under_build_archives() {
        let first =
            archive_unpack_relative_path("some-tool", Some("tool"), Some("sha256:deadbeef"));
        let second =
            archive_unpack_relative_path("some-tool", Some("tool"), Some("sha256:deadbeef"));
        let different =
            archive_unpack_relative_path("some-tool", Some("different"), Some("sha256:deadbeef"));
        assert_eq!(first, second, "same input should produce same path");
        assert_ne!(
            first, different,
            "different inputs should produce distinct output paths"
        );
        assert!(
            first.starts_with(Path::new("build").join("archives")),
            "archive path should live under build/archives: {}",
            first.display()
        );
    }

    #[test]
    fn unpack_archive_to_dir_extracts_tar_contents() {
        let tar_available = Command::new("tar")
            .arg("--version")
            .output()
            .map(|output| output.status.success())
            .unwrap_or(false);
        if !tar_available {
            return;
        }

        let tmp = TempDir::new().unwrap();
        let src_dir = tmp.path().join("src");
        let payload_dir = src_dir.join("payload");
        std::fs::create_dir_all(&payload_dir).unwrap();
        std::fs::write(payload_dir.join("hello.txt"), "hello archive\n").unwrap();

        let archive_path = tmp.path().join("payload.tar");
        let status = Command::new("tar")
            .arg("-cf")
            .arg(&archive_path)
            .arg("-C")
            .arg(&src_dir)
            .arg("payload")
            .status()
            .unwrap();
        assert!(status.success(), "failed to create test tar archive");

        let dest = tmp.path().join("dest");
        unpack_archive_to_dir(&archive_path, &dest).unwrap();

        let unpacked = dest.join(PathBuf::from("payload").join("hello.txt"));
        let body = std::fs::read_to_string(&unpacked).unwrap();
        assert_eq!(body, "hello archive\n");
    }

    #[test]
    fn build_cli_shows_spec_source_output_repo_root() {
        // Verify --help includes all documented args.
        let help = Cli::try_parse_from(["botforge", "build", "--help"])
            .unwrap_err()
            .to_string();
        assert!(help.contains("--spec"), "--spec missing from help: {help}");
        assert!(
            help.contains("--source"),
            "--source missing from help: {help}"
        );
        assert!(
            help.contains("--output"),
            "--output missing from help: {help}"
        );
        assert!(
            help.contains("--repo-root"),
            "--repo-root missing from help: {help}"
        );
        assert!(
            help.contains("--cache-dir"),
            "--cache-dir missing from help: {help}"
        );
        assert!(
            help.contains("--ssh-port"),
            "--ssh-port missing from help: {help}"
        );
        assert!(
            help.contains("--ssh-host"),
            "--ssh-host missing from help: {help}"
        );
        assert!(
            help.contains("--ssh-user"),
            "--ssh-user missing from help: {help}"
        );
        assert!(
            !help.contains("--ssh-key"),
            "--ssh-key should not appear in help: {help}"
        );
    }

    #[test]
    fn build_cli_requires_spec_and_output() {
        let err = Cli::try_parse_from(["botforge", "build"]).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);
        let err_text = err.to_string();
        assert!(
            err_text.contains("--spec"),
            "expected --spec in error: {err_text}"
        );
        assert!(
            err_text.contains("--output"),
            "expected --output in error: {err_text}"
        );
        // --source is now optional (overrides image: resolution); not required.
        assert!(
            !err_text.contains("--source"),
            "--source should not be required: {err_text}"
        );
        assert!(
            !err_text.contains("--ssh-key"),
            "--ssh-key should not be required: {err_text}"
        );
    }

    #[test]
    fn build_cli_source_is_optional() {
        // Parsing with only --spec and --output succeeds (source omitted).
        let result = Cli::try_parse_from([
            "botforge",
            "build",
            "--spec",
            "build.yaml",
            "--output",
            "out.qcow2",
        ]);
        assert!(
            result.is_ok(),
            "build should parse without --source: {result:?}"
        );
    }

    #[test]
    fn resolve_base_image_fails_on_unknown_asset() {
        let tmp = TempDir::new().unwrap();
        let manifest = tmp.path().join("shasset.yaml");
        std::fs::write(
            &manifest,
            "settings:\n  retries: 0\nassets:\n  other-asset:\n    uri: https://example.com/img.qcow2\n    version: \"1\"\n",
        )
        .unwrap();
        let err = resolve_base_image_with_transport(
            &manifest,
            "nonexistent-key",
            Some(tmp.path()),
            || None,
        )
        .unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("nonexistent-key") && msg.contains("not found"),
            "error should name the missing key: {msg}"
        );
    }

    #[test]
    fn resolve_base_image_fails_on_oci_asset() {
        let tmp = TempDir::new().unwrap();
        let manifest = tmp.path().join("shasset.yaml");
        std::fs::write(
            &manifest,
            "settings:\n  retries: 0\nassets:\n  my-image:\n    uri: oci://ghcr.io/example/img@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n    version: \"1\"\n",
        )
        .unwrap();
        let err =
            resolve_base_image_with_transport(&manifest, "my-image", Some(tmp.path()), || None)
                .unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("oci://") || msg.contains("qcow2"),
            "error should mention oci or qcow2: {msg}"
        );
    }

    #[test]
    fn resolve_base_image_fetches_and_materializes() {
        let tmp = TempDir::new().unwrap();
        let manifest = tmp.path().join("shasset.yaml");
        let cache = tmp.path().join("cache");
        let body = b"fake-qcow2-content".to_vec();
        let uri = "https://example.com/v1/base.qcow2".to_string();
        let checksum = "34cb20b33d115697e75baf0d12172c7c3b42a5f04b047c64f38d0aa2b57c988f";
        std::fs::write(
            &manifest,
            format!(
                "settings:\n  retries: 0\nassets:\n  debian-base:\n    uri: {uri}\n    version: \"13\"\n    checksum: sha256:{checksum}\n    filename: debian-13.qcow2\n"
            ),
        )
        .unwrap();

        let mut transport = Some(Box::new(MockTransport {
            expected_uri: uri,
            body: body.clone(),
        }) as Box<dyn Transport>);

        let path =
            resolve_base_image_with_transport(&manifest, "debian-base", Some(&cache), || {
                transport.take()
            })
            .unwrap();

        assert!(path.exists(), "materialized qcow2 should exist: {path:?}");
        assert_eq!(std::fs::read(&path).unwrap(), body);
        assert_eq!(path.file_name().unwrap(), "debian-13.qcow2");
    }

    #[test]
    fn guest_untar_command_constructs_expected_ssh_command() {
        use crate::util::shell_single_quote;
        let dest = "/var/lib/foo";
        let remote_archive = "/tmp/botforge-archive-2-12345.tar.gz";
        let cmd = guest_untar_command(
            &shell_single_quote(dest),
            &shell_single_quote(remote_archive),
        );
        assert_eq!(
            cmd,
            "sudo mkdir -p '/var/lib/foo' && sudo tar -xf '/tmp/botforge-archive-2-12345.tar.gz' -C '/var/lib/foo'"
        );
    }

    #[test]
    fn guest_untar_command_quotes_paths_with_spaces() {
        use crate::util::shell_single_quote;
        let dest = "/var/lib/some tool";
        let remote_archive = "/tmp/my archive.tar";
        let cmd = guest_untar_command(
            &shell_single_quote(dest),
            &shell_single_quote(remote_archive),
        );
        assert_eq!(
            cmd,
            "sudo mkdir -p '/var/lib/some tool' && sudo tar -xf '/tmp/my archive.tar' -C '/var/lib/some tool'"
        );
    }

    #[test]
    fn guest_archive_temp_path_has_expected_prefix_and_extension() {
        // Reproduce the temp-path derivation logic from run_archive_step
        // to verify it starts with the botforge prefix and ends with the extension.
        let step_idx: usize = 3;
        let suffix = "99999-123456789";
        let ext = ".tar.gz";
        let path = format!("/tmp/botforge-archive-{step_idx}-{suffix}{ext}");
        assert!(
            path.starts_with("/tmp/botforge-archive-"),
            "temp path should be under /tmp: {path}"
        );
        assert!(
            path.ends_with(".tar.gz"),
            "temp path should preserve extension: {path}"
        );
        assert!(
            path.contains(&format!("-{step_idx}-")),
            "temp path should embed step index: {path}"
        );
    }
}
