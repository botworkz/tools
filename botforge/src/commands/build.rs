//! BUILD: booted-VM image builder using the shared `crate::plan` guest/host step runtime.
//! Produces a qcow2 by booting the source image under qemu, provisioning it via plan steps,
//! then gracefully shutting down and committing the disk as the output artifact.

use anyhow::{bail, Context, Result};
use clap::Args;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use crate::iso::{
    build_iso, detect_iso_tool, generate_installer_username, render_user_data, write_seed_files,
};
use crate::qemu::{qemu_build_args, require_kvm, spawn_qemu_with_log};
use crate::ssh::{ssh_with_retry, SshOptions, TemporarySshKeypair};
use crate::util::{create_temp_dir, ensure_command, resolve_under_root};

use crate::plan::{
    load_build_config, preserve_failed_build_disk, print_log_tail, run_step_flow,
    shutdown_build_vm, validate_build_steps, vm::StepTimeoutPolicy,
};

#[derive(Args, Debug)]
pub(crate) struct BuildArgs {
    /// Path to type-build YAML spec.
    #[arg(long, required = true)]
    spec: PathBuf,
    /// Source qcow2 image path. Read-only; copied to <output>.partial before any modification.
    #[arg(long, required = true)]
    source: PathBuf,
    /// Output qcow2 path. Materialized atomically from <output>.partial on success.
    #[arg(long, required = true)]
    output: PathBuf,
    /// Repo root for resolving relative spec/source/output/step paths (default: current dir).
    #[arg(long)]
    repo_root: Option<PathBuf>,
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

pub(crate) fn cmd_build(args: BuildArgs) -> Result<()> {
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
    let source = resolve_under_root(&repo_root, args.source.clone());
    let output = resolve_under_root(&repo_root, args.output.clone());

    let build_config = load_build_config(&repo_root, &spec_path)?;
    validate_build_steps(&build_config.steps)?;

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
    // image, so it never ships. This is botforge's last guest action and runs as
    // a single sudo bash invocation so the entire sequence (userdel, sudoers
    // cleanup, poweroff) executes as root — user deletion does not affect the
    // still-running SSH session, and systemctl poweroff is issued while root
    // context is still active. A failure here is a hard error: a shipped image
    // must not contain the installer account.
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
        // Teardown issued poweroff; shutdown_build_vm waits for the VM to exit.
    }

    // ---------------------------------------------------------------------------
    // Disk lifecycle step 6 (was 5): graceful shutdown
    // ---------------------------------------------------------------------------
    let shutdown_result = shutdown_build_vm(
        &mut vm_child,
        &partial,
        &failed_partial,
        &ssh_options,
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

/// Remove the botforge-owned ephemeral installer user from the guest and power off.
///
/// Runs as a single `sudo bash -c '...'` so all commands execute as root: user
/// deletion does not affect the calling SSH session, and `systemctl poweroff` is
/// issued while root context is still active (i.e. before the sudoers drop-in is
/// removed). `systemctl poweroff` returns immediately (exit 0); the shutdown
/// happens asynchronously so the SSH command exits cleanly.
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

    // Single root shell: delete the installer user, wipe its home explicitly
    // (tolerates missing home with rm -rf), remove any cloud-init sudoers drop-in
    // that names it, then power off. All commands are chained with && so a failure
    // aborts early and surfaces a non-zero exit to the SSH caller.
    let cmd = format!(
        "sudo bash -c 'userdel -f {installer} && \
         rm -rf /home/{installer} && \
         rm -f /etc/sudoers.d/90-cloud-init-users && \
         systemctl poweroff'"
    );

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
    use super::{failed_partial_path, output_stem, partial_path};
    use crate::cli::Cli;
    use clap::Parser;
    use std::path::Path;

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
    fn output_stem_uses_output_file_stem() {
        assert_eq!(output_stem(Path::new("/build/out.qcow2")), "out");
    }

    #[test]
    fn build_cli_shows_spec_source_output_repo_root() {
        // Verify --help includes all four documented args.
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
    fn build_cli_requires_spec_source_output() {
        let err = Cli::try_parse_from(["botforge", "build"]).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);
        let err_text = err.to_string();
        assert!(
            err_text.contains("--spec"),
            "expected --spec in error: {err_text}"
        );
        assert!(
            err_text.contains("--source"),
            "expected --source in error: {err_text}"
        );
        assert!(
            err_text.contains("--output"),
            "expected --output in error: {err_text}"
        );
        assert!(
            !err_text.contains("--ssh-key"),
            "--ssh-key should not be required: {err_text}"
        );
    }
}
