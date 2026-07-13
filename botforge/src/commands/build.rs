//! BUILD: booted-VM image builder using the shared `crate::plan` guest/host step runtime.
//! Produces a qcow2 by booting the source image under qemu, provisioning it via plan steps,
//! then gracefully shutting down and committing the disk as the output artifact.

use anyhow::{bail, Context, Result};
use clap::Args;
use serde_yaml::Value;
use shasset::fetch::{fetch_asset, FetchParams, MaterializeMode};
use shasset::manifest::load;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use crate::iso::{
    build_iso, detect_iso_tool, generate_installer_username, render_user_data, write_seed_files,
};
use crate::qemu::{qemu_build_args, require_kvm, spawn_qemu_with_log};
use crate::resolver::{AssetKind, ResolveFileContext, ResolveSpec, ARTIFACT_DIR};
use crate::ssh::{scp_with_retry, ssh_with_retry, SshOptions, TemporarySshKeypair};
use crate::util::{
    botforge_debug_enabled, create_temp_dir, default_cache_dir, ensure_command, format_bytes_human,
    repo_relative_display, resolve_under_root, unique_suffix,
};

use crate::plan::config::{CompressConfig, CompressionType, ReclaimMode};
use crate::plan::step::{ArchiveStep, StepTarget, TestStep};
use crate::plan::{
    load_build_config, preserve_failed_build_disk, print_log_tail, run_step_flow,
    shutdown_build_vm, validate_build_steps, vm::StepFlowPlan, vm::StepTimeoutPolicy,
};
use crate::qcow2::{
    compress_qcow2_image, read_qcow2_image_stats, read_virtual_sector0, sparsify_zero_clusters,
};

/// Parsed `--cpus` value: either a specific positive count or `auto`.
///
/// `auto` resolves at runtime to the host's available CPU count via
/// [`std::thread::available_parallelism`], falling back to `1` on error.
#[derive(Debug, Clone)]
pub(crate) enum CpusArg {
    Count(u32),
    Auto,
}

impl CpusArg {
    /// Resolve to a concrete vCPU count.
    pub(crate) fn resolve(self) -> u32 {
        match self {
            CpusArg::Count(n) => n,
            CpusArg::Auto => std::thread::available_parallelism()
                .map(|n| n.get() as u32)
                .unwrap_or(1),
        }
    }
}

impl std::str::FromStr for CpusArg {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        if s == "auto" {
            return Ok(CpusArg::Auto);
        }
        match s.parse::<u32>() {
            Ok(0) => Err("--cpus must be at least 1 (or 'auto')".to_string()),
            Ok(n) => Ok(CpusArg::Count(n)),
            Err(_) => Err(format!(
                "invalid --cpus value {s:?}: expected a positive integer or 'auto'"
            )),
        }
    }
}

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
    /// Repo root for resolving relative spec/source/step paths (default: current dir).
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
    /// Guest RAM in MiB. Controls the runner VM only; does not affect the output image.
    #[arg(long, default_value_t = 4096)]
    memory: u32,
    /// Number of vCPUs for the runner VM, or 'auto' to use all available host CPUs.
    /// Controls the runner VM only; does not affect the output image.
    #[arg(long, default_value = "4")]
    cpus: CpusArg,
}

pub(crate) fn cmd_build(config: &Path, args: BuildArgs) -> Result<()> {
    require_kvm()?;
    ensure_command("qemu-system-x86_64")?;
    ensure_command("qemu-img")?;
    detect_iso_tool()?;

    let repo_root = std::fs::canonicalize(
        args.repo_root
            .unwrap_or(std::env::current_dir().context("failed to determine current directory")?),
    )
    .context("failed to resolve repo root")?;

    let spec_path = resolve_under_root(&repo_root, args.spec.clone());

    let build_config = load_build_config(&repo_root, &spec_path)?;
    let output = derive_artifact_output_path(&repo_root, &spec_path, &build_config.output)?;
    check_same_directory_output_filename_clash(&spec_path, &build_config.output)?;
    let reclaim_mode = build_config
        .compress
        .as_ref()
        .map(|c| c.reclaim)
        .unwrap_or_default();
    let guest_reclaim_uses_discard = matches!(reclaim_mode, ReclaimMode::Fstrim);
    validate_build_steps(&build_config.steps)?;
    if build_config
        .steps
        .iter()
        .any(|step| matches!(step, TestStep::Archive(_)))
    {
        ensure_command("tar")?;
    }
    if matches!(reclaim_mode, ReclaimMode::Discard) {
        ensure_command("qemu-nbd")?;
    }

    // Resolve the source qcow2: --source wins; otherwise resolve image: via the shared resolver.
    let source = if let Some(src) = args.source {
        resolve_under_root(&repo_root, src)
    } else {
        build_config.image.resolve_one_validated(
            &ResolveFileContext {
                repo_root: &repo_root,
                manifest_path: config,
                cache_dir_override: args.cache_dir.as_deref(),
            },
            &ResolveSpec {
                deny_kinds: vec![AssetKind::OciImage],
                ..Default::default()
            },
        )?
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
        render_user_data(
            None,
            ssh_public_key.trim(),
            Some(installer_user.as_str()),
            build_config.cloud_init.as_ref(),
        )
    } else {
        render_user_data(
            None,
            ssh_public_key.trim(),
            None,
            build_config.cloud_init.as_ref(),
        )
    };
    crate::plan::print_phase("setup", "Preparing build environment (seed image)");
    write_seed_files(&seed_dir, &user_data)?;
    build_iso(&seed_dir, &seed_iso, "cidata")?;
    std::fs::remove_dir_all(&seed_dir)
        .with_context(|| format!("cannot remove temp seed dir: {}", seed_dir.display()))?;
    crate::plan::print_phase_status("setup", "Preparing build environment (seed image)", true);

    let qemu_args = qemu_build_args(
        &partial,
        &seed_iso,
        args.ssh_port,
        args.memory,
        args.cpus.resolve(),
        guest_reclaim_uses_discard,
    );
    let spec_display = repo_relative_display(&repo_root, &spec_path);
    let source_display = repo_relative_display(&repo_root, &source);
    crate::plan::print_phase(
        "vm",
        &format!("Starting vm (spec: {spec_display}, image: {source_display})"),
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
    let step_result = run_step_flow(
        &repo_root,
        StepFlowPlan {
            files: &build_config.files,
            steps: &build_config.steps,
            bootstraps: &[],
            manifest_path: config,
            cache_dir_override: args.cache_dir.as_deref(),
        },
        &ssh_options,
        StepTimeoutPolicy {
            overall_timeout: std::time::Duration::from_secs(build_config.timeout),
            default_step_timeout: std::time::Duration::from_secs(build_config.step_timeout),
            cloud_init_timeout: std::time::Duration::from_secs(build_config.cloud_init_timeout),
        },
        Some(&mut archive_executor),
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
    // Disk lifecycle step 5: guest reclaim (optional)
    //
    // `reclaim: fstrim` must run while the guest is still fully available for
    // SSH. If botforge owns the installer account, that means reclaim must
    // happen before the detached teardown service is queued.
    // ---------------------------------------------------------------------------
    let compress_phase_runs = !matches!(reclaim_mode, ReclaimMode::None)
        || build_config.compress.as_ref().is_some_and(|c| c.enabled);
    if compress_phase_runs {
        crate::plan::print_phase(
            "compress",
            "Compressing image (reclaim, sparsify, compression)",
        );
    }
    if matches!(reclaim_mode, ReclaimMode::Fstrim) {
        if let Err(err) = run_guest_reclaim_fstrim(&ssh_options, overall_deadline) {
            eprintln!("guest reclaim fstrim failed: {err:#}");
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
    }

    if let Err(err) = run_guest_cloud_init_clean(&ssh_options, overall_deadline) {
        eprintln!("guest cloud-init clean failed: {err:#}");
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

    // ---------------------------------------------------------------------------
    // Disk lifecycle step 6: installer teardown (botforge-owned identity only)
    //
    // Remove the ephemeral installer account from the guest before committing the
    // image, so it never ships. This is botforge's last guest action: it queues a
    // detached root-owned systemd service that waits for the SSH caller to return,
    // deletes the installer, and only then powers off. A failure here is a hard
    // error: a shipped image must not contain the installer account.
    //
    // On the failure path (step 4, reclaim, or teardown error), the VM is killed
    // and the tainted disk is preserved at <output>.partial.failed for
    // post-mortem; leaving the installer present in that tainted disk is
    // acceptable.
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
    // Disk lifecycle step 7 (was 5): graceful shutdown
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
        crate::plan::print_phase_status("vm", "Stopping vm", false);
        eprintln!("build VM shutdown failed: {err:#}");
        print_log_tail(&vm_log, 200);
        return Err(err);
    }
    crate::plan::print_phase_status("vm", "Stopping vm", true);

    if matches!(reclaim_mode, ReclaimMode::Discard) {
        reclaim_host_discard_offline(&partial)?;
    }

    let zero_cluster_stats = if should_run_zero_cluster_sparsify(reclaim_mode) {
        let stats = sparsify_zero_clusters(&partial).with_context(|| {
            format!("failed to sparsify zero clusters in {}", partial.display())
        })?;
        if botforge_debug_enabled() {
            eprintln!(
                "qcow2 zero-cluster sparsify: scanned={} deallocated={} skipped_compressed={}",
                stats.scanned_clusters,
                stats.deallocated_clusters,
                stats.skipped_compressed_clusters
            );
        }
        Some(stats)
    } else {
        None
    };

    // ---------------------------------------------------------------------------
    // Disk lifecycle step 8 (was 6): commit partial → output (plain rename or
    // compress-and-rename depending on the spec's `compress:` config).
    // ---------------------------------------------------------------------------
    commit_output(&partial, &output, build_config.compress.as_ref())?;

    let output_stats = read_qcow2_image_stats(&output)?;
    if botforge_debug_enabled() {
        let deallocated = zero_cluster_stats
            .map(|s| s.deallocated_clusters)
            .unwrap_or(0);
        eprintln!(
            "final image stats: virtual_size={} disk_size={} cluster_size={} allocated_data_clusters={} zero_clusters_deallocated={}",
            output_stats.virtual_size,
            output_stats.disk_size,
            output_stats.cluster_size,
            output_stats.allocated_data_clusters,
            deallocated
        );
    }
    crate::plan::print_phase(
        "output",
        &format!(
            "Final image written to {} ({})",
            output.display(),
            format_bytes_human(output_stats.disk_size)
        ),
    );
    Ok(())
}

fn should_run_zero_cluster_sparsify(mode: ReclaimMode) -> bool {
    !matches!(mode, ReclaimMode::None)
}

/// Commit `partial` to `output`, optionally compressing via the native qcow2 writer.
///
/// - `compress` is `None` or `Some { enabled: false, .. }` → plain atomic
///   rename (byte-identical to prior behaviour).
/// - `compress` is `Some { enabled: true, .. }` → botforge rewrites the qcow2
///   in-process using the configured compressor, then atomically renames the
///   result to `output` and removes `partial`.
fn commit_output(partial: &Path, output: &Path, compress: Option<&CompressConfig>) -> Result<()> {
    match compress {
        Some(c) if c.enabled => {
            let pre_compress_sector0 = read_virtual_sector0(partial).with_context(|| {
                format!(
                    "failed to read guest sector 0 before compression from {}",
                    partial.display()
                )
            })?;
            if pre_compress_sector0.iter().all(|b| *b == 0) {
                bail!(
                    "refusing to compress qcow2: guest sector 0 is all-zero before compression ({})",
                    partial.display()
                );
            }
            // Write to a temp path beside the output so the final rename is
            // atomic (same filesystem as the intended output location).
            let tmp = output.with_extension("partial.compress");
            compress_qcow2_image(
                partial,
                &tmp,
                c.compressor,
                &c.compressor_args,
                &c.compressor_opts,
            )
            .with_context(|| {
                format!(
                    "failed to compress qcow2 output with {}",
                    match c.compressor {
                        CompressionType::Zstd => "zstd",
                        CompressionType::Zlib => "zlib",
                    }
                )
            })?;
            let post_compress_sector0 = read_virtual_sector0(&tmp).with_context(|| {
                format!(
                    "failed to read guest sector 0 after compression from {}",
                    tmp.display()
                )
            })?;
            if post_compress_sector0.iter().all(|b| *b == 0) {
                bail!(
                    "refusing to publish compressed qcow2: guest sector 0 became all-zero after compression ({})",
                    tmp.display()
                );
            }
            std::fs::rename(&tmp, output).with_context(|| {
                format!(
                    "cannot atomically materialize compressed output from {} to {}",
                    tmp.display(),
                    output.display()
                )
            })?;
            std::fs::remove_file(partial).with_context(|| {
                format!(
                    "cannot remove partial disk after compression: {}",
                    partial.display()
                )
            })?;
        }
        _ => {
            // No compression — plain atomic rename (unchanged from prior behaviour).
            std::fs::rename(partial, output).with_context(|| {
                format!(
                    "cannot atomically materialize output from {} to {}",
                    partial.display(),
                    output.display()
                )
            })?;
        }
    }
    Ok(())
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

fn parse_archive_asset_key(src: &str) -> Result<&str> {
    use crate::resolver::Reference;
    if src.is_empty() {
        bail!("archive `src` is required");
    }
    let reference =
        Reference::parse(src).map_err(|_| anyhow::anyhow!("archive `src` must start with '@'"))?;
    match reference {
        Reference::Asset { path: None, .. } => {
            // src is `@<name>`; return the name as a slice of `src`.
            Ok(&src[1..])
        }
        _ => {
            if src.contains("://") {
                bail!("archive `src` does not support '@://' traversal")
            } else {
                bail!("archive `src` must include a shasset name after '@'")
            }
        }
    }
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

fn fstrim_guest_command() -> &'static str {
    "sudo fstrim -av"
}

fn cloud_init_clean_guest_command() -> &'static str {
    "if command -v cloud-init >/dev/null 2>&1; then \
     sudo cloud-init clean --logs --seed || sudo cloud-init clean --logs; \
     else echo 'cloud-init not installed; skipping clean'; fi"
}

fn run_guest_reclaim_fstrim(ssh: &SshOptions, overall_deadline: Instant) -> Result<()> {
    let timeout = overall_deadline
        .saturating_duration_since(Instant::now())
        .min(Duration::from_secs(30));
    let timeout = if timeout.is_zero() {
        Duration::from_secs(1)
    } else {
        timeout
    };
    ssh_with_retry(
        ssh,
        fstrim_guest_command(),
        1,
        Duration::from_secs(0),
        timeout,
    )
    .context("guest fstrim reclaim failed")
}

fn run_guest_cloud_init_clean(ssh: &SshOptions, overall_deadline: Instant) -> Result<()> {
    let timeout = overall_deadline
        .saturating_duration_since(Instant::now())
        .min(Duration::from_secs(30));
    let timeout = if timeout.is_zero() {
        Duration::from_secs(1)
    } else {
        timeout
    };
    ssh_with_retry(
        ssh,
        cloud_init_clean_guest_command(),
        1,
        Duration::from_secs(0),
        timeout,
    )
    .context("guest cloud-init clean failed")
}

fn qemu_nbd_connect_args(partial: &Path, nbd_device: &Path) -> Vec<String> {
    vec![
        "--discard=unmap".to_string(),
        format!("--connect={}", nbd_device.display()),
        partial.display().to_string(),
    ]
}

fn qemu_nbd_disconnect_args(nbd_device: &Path) -> Vec<String> {
    vec!["--disconnect".to_string(), nbd_device.display().to_string()]
}

fn mount_discard_args(block_device: &Path, mountpoint: &Path) -> Vec<String> {
    vec![
        "-o".to_string(),
        "discard".to_string(),
        block_device.display().to_string(),
        mountpoint.display().to_string(),
    ]
}

fn fstrim_mount_args(mountpoint: &Path) -> Vec<String> {
    vec!["-v".to_string(), mountpoint.display().to_string()]
}

fn probe_nbd_device_ready(nbd_device: &Path, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    let mut last_error = None;
    while Instant::now() < deadline {
        let output = Command::new("blockdev")
            .arg("--getsize64")
            .arg(nbd_device)
            .output();
        match output {
            Ok(output) if output.status.success() => {
                let size = String::from_utf8_lossy(&output.stdout)
                    .trim()
                    .parse::<u64>()
                    .unwrap_or(0);
                if size > 0 {
                    return Ok(());
                }
            }
            Ok(output) => {
                last_error = Some(format!(
                    "blockdev exited with status {}: {}",
                    output.status,
                    String::from_utf8_lossy(&output.stderr).trim()
                ));
            }
            Err(err) => {
                last_error = Some(format!("failed to execute blockdev: {err}"));
            }
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    match last_error {
        Some(err) => bail!(
            "timed out waiting for nbd device {} to become ready: {err}",
            nbd_device.display()
        ),
        None => bail!(
            "timed out waiting for nbd device {} to become ready",
            nbd_device.display()
        ),
    }
}

fn root_device_for_discard(nbd_device: &Path) -> Result<PathBuf> {
    let output = Command::new("lsblk")
        .arg("-brno")
        .arg("PATH,TYPE,SIZE")
        .arg(nbd_device)
        .output()
        .with_context(|| format!("failed to execute lsblk for {}", nbd_device.display()))?;
    if !output.status.success() {
        bail!(
            "lsblk failed for {} (exit status: {})",
            nbd_device.display(),
            output.status
        );
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut largest_part: Option<(u64, PathBuf)> = None;
    let mut disk_path: Option<PathBuf> = None;
    for line in stdout.lines() {
        let mut cols = line.split_whitespace();
        let Some(path) = cols.next() else { continue };
        let Some(kind) = cols.next() else { continue };
        let Some(size) = cols.next() else { continue };
        let Ok(size) = size.parse::<u64>() else {
            continue;
        };
        let path = PathBuf::from(path);
        if kind == "disk" {
            disk_path = Some(path.clone());
        }
        if kind == "part" {
            match &largest_part {
                Some((current, _)) if *current >= size => {}
                _ => largest_part = Some((size, path)),
            }
        }
    }

    if let Some((_, part)) = largest_part {
        return Ok(part);
    }
    if let Some(disk) = disk_path {
        return Ok(disk);
    }
    bail!(
        "could not determine partition layout for {} from lsblk output",
        nbd_device.display()
    )
}

fn ensure_nbd_devices_loaded() -> Result<()> {
    let status = Command::new("modprobe")
        .args(["nbd", "max_part=8"])
        .status();
    if let Err(err) = status {
        eprintln!("warning: failed to run modprobe nbd max_part=8: {err}");
    }
    let nbd0 = Path::new("/dev/nbd0");
    if !nbd0.exists() {
        bail!(
            "nbd device nodes are unavailable ({} missing) after attempting `modprobe nbd max_part=8`",
            nbd0.display()
        );
    }
    Ok(())
}

struct DiscardCleanup {
    mountpoint: Option<PathBuf>,
    mounted: bool,
    nbd_device: Option<PathBuf>,
}

impl DiscardCleanup {
    fn new() -> Self {
        Self {
            mountpoint: None,
            mounted: false,
            nbd_device: None,
        }
    }
}

impl Drop for DiscardCleanup {
    fn drop(&mut self) {
        if self.mounted {
            if let Some(mountpoint) = &self.mountpoint {
                let status = Command::new("umount").arg(mountpoint).status();
                if let Err(err) = status {
                    eprintln!(
                        "warning: failed to unmount discard temp mountpoint {}: {err}",
                        mountpoint.display()
                    );
                }
            }
        }
        if let Some(nbd_device) = &self.nbd_device {
            let args = qemu_nbd_disconnect_args(nbd_device);
            let status = Command::new("qemu-nbd").args(&args).status();
            if let Err(err) = status {
                eprintln!(
                    "warning: failed to disconnect qemu-nbd device {}: {err}",
                    nbd_device.display()
                );
            }
        }
        if let Some(mountpoint) = &self.mountpoint {
            if let Err(err) = std::fs::remove_dir_all(mountpoint) {
                eprintln!(
                    "warning: failed to remove discard temp mountpoint {}: {err}",
                    mountpoint.display()
                );
            }
        }
    }
}

fn reclaim_host_discard_offline(partial: &Path) -> Result<()> {
    ensure_nbd_devices_loaded()?;
    let mut cleanup = DiscardCleanup::new();

    let nbd_device = (0..16)
        .map(|i| PathBuf::from(format!("/dev/nbd{i}")))
        .find(|candidate| {
            let args = qemu_nbd_connect_args(partial, candidate);
            Command::new("qemu-nbd")
                .args(&args)
                .status()
                .map(|status| status.success())
                .unwrap_or(false)
        })
        .context("failed to attach image via qemu-nbd: no free /dev/nbd0..15 device")?;
    cleanup.nbd_device = Some(nbd_device.clone());

    probe_nbd_device_ready(&nbd_device, Duration::from_secs(10))?;
    let root_device = root_device_for_discard(&nbd_device)?;

    let mountpoint = create_temp_dir("botforge-reclaim-discard-mnt")?;
    cleanup.mountpoint = Some(mountpoint.clone());

    let mount_args = mount_discard_args(&root_device, &mountpoint);
    let mount_status = Command::new("mount")
        .args(&mount_args)
        .status()
        .with_context(|| {
            format!(
                "failed to execute mount for discard reclaim ({})",
                root_device.display()
            )
        })?;
    if !mount_status.success() {
        bail!(
            "mount failed for discard reclaim of {} (exit status: {mount_status})",
            root_device.display()
        );
    }
    cleanup.mounted = true;

    let fstrim_args = fstrim_mount_args(&mountpoint);
    let fstrim_status = Command::new("fstrim")
        .args(&fstrim_args)
        .status()
        .with_context(|| {
            format!(
                "failed to execute host fstrim for discard reclaim at {}",
                mountpoint.display()
            )
        })?;
    if !fstrim_status.success() {
        bail!("host fstrim failed during discard reclaim (exit status: {fstrim_status})");
    }
    Ok(())
}

fn derive_artifact_output_path(
    repo_root: &Path,
    spec_path: &Path,
    output_filename: &str,
) -> Result<PathBuf> {
    let spec_relative = spec_path.strip_prefix(repo_root).with_context(|| {
        format!(
            "spec path must be under repo root (spec: {}, repo_root: {})",
            spec_path.display(),
            repo_root.display()
        )
    })?;
    let spec_relative_dir = spec_relative.parent().unwrap_or_else(|| Path::new(""));
    Ok(repo_root
        .join(ARTIFACT_DIR)
        .join(spec_relative_dir)
        .join(output_filename))
}

fn check_same_directory_output_filename_clash(
    spec_path: &Path,
    output_filename: &str,
) -> Result<()> {
    let spec_dir = spec_path.parent().unwrap_or_else(|| Path::new("."));
    let spec_file_name = spec_path.file_name().map(|n| n.to_owned());
    for entry in std::fs::read_dir(spec_dir).with_context(|| {
        format!(
            "cannot read spec directory for output clash check: {}",
            spec_dir.display()
        )
    })? {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        let sibling_path = entry.path();
        if !sibling_path.is_file() {
            continue;
        }
        let ext = sibling_path.extension().and_then(|s| s.to_str());
        if !matches!(ext, Some("yaml" | "yml")) {
            continue;
        }
        if spec_file_name
            .as_ref()
            .is_some_and(|name| sibling_path.file_name() == Some(name))
        {
            continue;
        }
        // Intentionally same-directory only; cross-directory coordination is out of scope
        // until global output coordination is introduced.
        let Some(sibling_output) = read_top_level_output_filename(&sibling_path) else {
            continue;
        };
        if sibling_output == output_filename {
            bail!(
                "output filename clash in spec directory: '{}' is declared by both '{}' and '{}'",
                output_filename,
                spec_path.display(),
                sibling_path.display()
            );
        }
    }
    Ok(())
}

fn read_top_level_output_filename(path: &Path) -> Option<String> {
    let yaml = std::fs::read_to_string(path).ok()?;
    let value: Value = serde_yaml::from_str(&yaml).ok()?;
    let map = value.as_mapping()?;
    let output_key = Value::String("output".to_string());
    match map.get(&output_key) {
        Some(Value::String(s)) => Some(s.clone()),
        _ => None,
    }
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
        archive_unpack_relative_path, check_same_directory_output_filename_clash,
        cloud_init_clean_guest_command, derive_artifact_output_path, failed_partial_path,
        fstrim_guest_command, fstrim_mount_args, guest_untar_command, installer_teardown_command,
        mount_discard_args, output_stem, parse_archive_asset_key, partial_path,
        qemu_nbd_connect_args, qemu_nbd_disconnect_args, should_run_zero_cluster_sparsify,
        unpack_archive_to_dir,
    };
    use crate::cli::Cli;
    use crate::plan::config::ReclaimMode;
    use clap::Parser;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use tempfile::TempDir;

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
    fn cloud_init_clean_guest_command_is_non_fatal_when_missing() {
        let cmd = cloud_init_clean_guest_command();
        assert!(cmd.contains("command -v cloud-init"));
        assert!(cmd.contains("cloud-init clean --logs --seed"));
        assert!(cmd.contains("cloud-init clean --logs"));
        assert!(cmd.contains("cloud-init not installed; skipping clean"));
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
    fn build_cli_shows_spec_source_repo_root() {
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
        assert!(
            help.contains("--memory"),
            "--memory missing from help: {help}"
        );
        assert!(help.contains("--cpus"), "--cpus missing from help: {help}");
    }

    #[test]
    fn cpus_arg_parses_positive_integer() {
        use super::CpusArg;
        use std::str::FromStr;
        let cpus = CpusArg::from_str("8").unwrap();
        assert_eq!(cpus.resolve(), 8);
    }

    #[test]
    fn cpus_arg_parses_auto() {
        use super::CpusArg;
        use std::str::FromStr;
        let cpus = CpusArg::from_str("auto").unwrap();
        // auto resolves to available_parallelism() >= 1
        assert!(cpus.resolve() >= 1);
    }

    #[test]
    fn cpus_arg_rejects_zero() {
        use super::CpusArg;
        use std::str::FromStr;
        let err = CpusArg::from_str("0").unwrap_err();
        assert!(
            err.contains("at least 1") || err.contains("auto"),
            "error should reject 0: {err}"
        );
    }

    #[test]
    fn cpus_arg_rejects_invalid_string() {
        use super::CpusArg;
        use std::str::FromStr;
        let err = CpusArg::from_str("bogus").unwrap_err();
        assert!(
            err.contains("invalid") || err.contains("bogus"),
            "error should mention invalid value: {err}"
        );
    }

    #[test]
    fn build_cli_requires_spec() {
        let err = Cli::try_parse_from(["botforge", "build"]).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);
        let err_text = err.to_string();
        assert!(
            err_text.contains("--spec"),
            "expected --spec in error: {err_text}"
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
        // Parsing with only --spec succeeds (source omitted).
        let result = Cli::try_parse_from(["botforge", "build", "--spec", "build.yaml"]);
        assert!(
            result.is_ok(),
            "build should parse without --source: {result:?}"
        );
    }

    #[test]
    fn build_cli_rejects_removed_output_flag() {
        let err = Cli::try_parse_from([
            "botforge",
            "build",
            "--spec",
            "build.yaml",
            "--output",
            "out.qcow2",
        ])
        .unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::UnknownArgument);
        assert!(
            err.to_string().contains("--output"),
            "error should mention removed --output flag: {err}"
        );
    }

    #[test]
    fn derive_artifact_output_path_from_spec_relative_dir() {
        let output = derive_artifact_output_path(
            Path::new("/repo"),
            Path::new("/repo/foo/bar/baz/build.yaml"),
            "something.qcow2",
        )
        .unwrap();
        assert_eq!(
            output,
            Path::new("/repo/build/artifact/foo/bar/baz/something.qcow2")
        );
    }

    #[test]
    fn output_filename_clash_check_allows_distinct_outputs() {
        let tmp = TempDir::new().unwrap();
        let spec = tmp.path().join("build.yaml");
        std::fs::write(&spec, "type: build\noutput: one.qcow2\n").unwrap();
        std::fs::write(
            tmp.path().join("other.yaml"),
            "type: build\noutput: two.qcow2\n",
        )
        .unwrap();

        check_same_directory_output_filename_clash(&spec, "one.qcow2").unwrap();
    }

    #[test]
    fn output_filename_clash_check_rejects_matching_sibling_output() {
        let tmp = TempDir::new().unwrap();
        let spec = tmp.path().join("build.yaml");
        let sibling = tmp.path().join("other.yaml");
        std::fs::write(&spec, "type: build\noutput: one.qcow2\n").unwrap();
        std::fs::write(&sibling, "type: build\noutput: one.qcow2\n").unwrap();

        let err = check_same_directory_output_filename_clash(&spec, "one.qcow2").unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("one.qcow2"), "error should name output: {msg}");
        assert!(
            msg.contains(&spec.display().to_string())
                && msg.contains(&sibling.display().to_string()),
            "error should name both files: {msg}"
        );
    }

    #[test]
    fn output_filename_clash_check_ignores_sibling_without_output() {
        let tmp = TempDir::new().unwrap();
        let spec = tmp.path().join("build.yaml");
        std::fs::write(&spec, "type: build\noutput: one.qcow2\n").unwrap();
        std::fs::write(
            tmp.path().join("other.yaml"),
            "type: build\nimage: \"@base\"\n",
        )
        .unwrap();

        check_same_directory_output_filename_clash(&spec, "one.qcow2").unwrap();
    }

    #[test]
    fn output_filename_clash_check_ignores_invalid_yaml_sibling() {
        let tmp = TempDir::new().unwrap();
        let spec = tmp.path().join("build.yaml");
        std::fs::write(&spec, "type: build\noutput: one.qcow2\n").unwrap();
        std::fs::write(tmp.path().join("other.yaml"), "type: [\n").unwrap();

        check_same_directory_output_filename_clash(&spec, "one.qcow2").unwrap();
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

    #[test]
    fn fstrim_guest_command_matches_expected_literal() {
        assert_eq!(fstrim_guest_command(), "sudo fstrim -av");
    }

    #[test]
    fn qemu_nbd_connect_args_include_discard_unmap() {
        let args = qemu_nbd_connect_args(
            Path::new("/build/out.qcow2.partial"),
            Path::new("/dev/nbd3"),
        );
        assert_eq!(
            args,
            vec![
                "--discard=unmap",
                "--connect=/dev/nbd3",
                "/build/out.qcow2.partial",
            ]
        );
    }

    #[test]
    fn qemu_nbd_disconnect_args_match_expected_argv() {
        let args = qemu_nbd_disconnect_args(Path::new("/dev/nbd3"));
        assert_eq!(args, vec!["--disconnect", "/dev/nbd3"]);
    }

    #[test]
    fn mount_discard_args_match_expected_argv() {
        let args = mount_discard_args(Path::new("/dev/nbd3p2"), Path::new("/tmp/mnt"));
        assert_eq!(args, vec!["-o", "discard", "/dev/nbd3p2", "/tmp/mnt"]);
    }

    #[test]
    fn fstrim_mount_args_match_expected_argv() {
        let args = fstrim_mount_args(Path::new("/tmp/mnt"));
        assert_eq!(args, vec!["-v", "/tmp/mnt"]);
    }

    #[test]
    fn zero_cluster_sparsify_follows_reclaim_mode() {
        assert!(!should_run_zero_cluster_sparsify(ReclaimMode::None));
        assert!(should_run_zero_cluster_sparsify(ReclaimMode::Fstrim));
        assert!(should_run_zero_cluster_sparsify(ReclaimMode::Discard));
    }
}
