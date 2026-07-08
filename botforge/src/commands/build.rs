//! BUILD: booted-VM image builder using the shared `crate::plan` guest/host step runtime.
//! Produces a qcow2 by booting the source image under qemu, provisioning it via plan steps,
//! then gracefully shutting down and committing the disk as the output artifact.

use anyhow::{bail, Context, Result};
use clap::Args;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::iso::{build_iso, detect_iso_tool, render_user_data, write_seed_files};
use crate::qemu::{qemu_build_args, require_kvm, spawn_qemu_with_log};
use crate::ssh::SshOptions;
use crate::util::{create_temp_dir, ensure_command, resolve_under_root};

use crate::plan::{
    load_build_config, print_log_tail, run_step_flow, shutdown_build_vm, validate_build_steps,
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
    /// SSH private key path for guest access.
    #[arg(long, required = true)]
    ssh_key: PathBuf,
    /// SSH host forwarded port.
    #[arg(long, default_value_t = 2222)]
    ssh_port: u16,
    /// SSH host.
    #[arg(long, default_value = "127.0.0.1")]
    ssh_host: String,
    /// SSH user.
    #[arg(long, default_value = "bot")]
    ssh_user: String,
}

pub(crate) fn cmd_build(args: BuildArgs) -> Result<()> {
    require_kvm()?;
    ensure_command("qemu-system-x86_64")?;
    ensure_command("qemu-img")?;
    ensure_command("ssh")?;
    ensure_command("scp")?;
    detect_iso_tool()?;

    let repo_root = std::fs::canonicalize(
        args.repo_root
            .unwrap_or(std::env::current_dir().context("failed to determine current directory")?),
    )
    .context("failed to resolve repo root")?;

    let spec_path = resolve_under_root(&repo_root, args.spec.clone());
    let source = resolve_under_root(&repo_root, args.source.clone());
    let output = resolve_under_root(&repo_root, args.output.clone());
    let ssh_key = resolve_under_root(&repo_root, args.ssh_key.clone());
    let ssh_pub = PathBuf::from(format!("{}.pub", ssh_key.display()));

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
    // Disk lifecycle step 1: copy source → <output>.partial
    // ---------------------------------------------------------------------------
    let partial = partial_path(&output);
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
    let seed_iso = build_dir.join("build-seed.iso");
    let vm_log = build_dir.join("build-vm.log");
    let seed_dir = create_temp_dir("botforge-build-seed")?;

    let ssh_public_key = std::fs::read_to_string(&ssh_pub)
        .with_context(|| format!("cannot read SSH public key: {}", ssh_pub.display()))?;
    let user_data = render_user_data(None, ssh_public_key.trim(), Some(args.ssh_user.as_str()));
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
        user: args.ssh_user.clone(),
        key: ssh_key.clone(),
    };

    // ---------------------------------------------------------------------------
    // Disk lifecycle step 4: run build steps via shared flow
    // ---------------------------------------------------------------------------
    let step_result = run_step_flow(&repo_root, &build_config.steps, &ssh_options, &[]);
    if let Err(err) = step_result {
        eprintln!("build steps failed: {err:#}");
        print_log_tail(&vm_log, 200);
        // Kill the VM. On step failure the partial is tainted — leave it for post-mortem.
        if let Some(child) = vm_child.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
        return Err(err);
    }

    // ---------------------------------------------------------------------------
    // Disk lifecycle step 5: graceful shutdown
    // ---------------------------------------------------------------------------
    let shutdown_result = shutdown_build_vm(&mut vm_child, &partial, &ssh_options);
    if let Err(err) = shutdown_result {
        eprintln!("build VM shutdown failed: {err:#}");
        print_log_tail(&vm_log, 200);
        return Err(err);
    }

    // ---------------------------------------------------------------------------
    // Disk lifecycle step 6: atomic rename partial → output
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
    use super::partial_path;
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
    }

    #[test]
    fn build_cli_requires_spec_source_output_ssh_key() {
        let err = Cli::try_parse_from(["botforge", "build"]).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);
    }
}
