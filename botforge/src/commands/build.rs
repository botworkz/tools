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
use crate::ssh::{ssh_with_retry, SshOptions, TemporarySshKeypair};
use crate::util::{
    create_temp_dir, default_cache_dir, ensure_command, materialize_flat, resolve_under_root,
};

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
    /// `base-image:` shasset resolution and boots this file directly. Read-only; copied
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

    // Resolve the source qcow2: --source wins; otherwise fetch base-image via shasset.
    let source = if let Some(src) = args.source {
        resolve_under_root(&repo_root, src)
    } else {
        resolve_base_image(config, &build_config.base_image, args.cache_dir.as_deref())?
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

/// Resolve the `base-image:` shasset asset key to a local qcow2 path.
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
            "base-image asset '{asset_key}' is an oci:// image; \
             base-image must resolve to a qcow2 file asset"
        );
    }
    if asset.checksum.is_none() {
        eprintln!(
            "warning: base-image asset '{asset_key}' has no checksum; \
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
    .with_context(|| format!("failed to fetch base-image asset '{asset_key}'"))?;

    // Materialize a copy into the cache dir so qemu boots a mutable copy
    // without polluting the shasset blob cache.
    let filename = asset.output_filename().with_context(|| {
        format!("base-image asset '{asset_key}': cannot determine output filename")
    })?;
    let out_dir = cache_dir.join("base-images");
    let qcow2_path = materialize_flat(&fetched.blob_path, &out_dir, &filename, false)
        .with_context(|| format!("failed to stage base-image asset '{asset_key}'"))?;

    Ok(qcow2_path)
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
        failed_partial_path, installer_teardown_command, output_stem, partial_path,
        resolve_base_image_with_transport,
    };
    use crate::cli::Cli;
    use clap::Parser;
    use shasset::fetch::{DownloadResponse, FetchError, Transport};
    use std::io::Cursor;
    use std::path::Path;
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
        // --source is now optional (overrides base-image resolution); not required.
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
}
