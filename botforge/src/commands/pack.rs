use anyhow::{bail, Context, Result};
use clap::Args;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::util::{command_exists, repo_relative_path, resolve_under_root, run_command};

#[derive(Args, Debug)]
pub(crate) struct PackArgs {
    /// VM checkout root containing compose.yaml and images/ (default: current directory).
    #[arg(long)]
    repo_root: Option<PathBuf>,
    /// Compress the qcow2 output with qemu-img convert -c.
    #[arg(long)]
    compress: bool,
    /// SSH private key path (default: <repo-root>/build/packer_ssh_key).
    #[arg(long)]
    key: Option<PathBuf>,
    /// Docker compose service to run packer/qemu-img in.
    #[arg(long, default_value = "tools-kvm")]
    compose_service: String,
    /// Docker compose file path (default: <repo-root>/compose.yaml).
    #[arg(long)]
    compose_file: Option<PathBuf>,
}

/// Run the simplified v1 Packer flow in docker compose.
///
/// This intentionally does not build or stage dependencies/images; callers must
/// arrange that beforehand. KVM is required.
pub(crate) fn cmd_pack(args: PackArgs) -> Result<()> {
    if !Path::new("/dev/kvm").exists() {
        bail!("botforge pack requires KVM: /dev/kvm not found");
    }
    if !command_exists("docker") {
        bail!("'docker' is not available on PATH");
    }

    let repo_root = std::fs::canonicalize(
        args.repo_root
            .unwrap_or(std::env::current_dir().context("failed to determine current directory")?),
    )
    .context("failed to resolve repo root")?;
    if !repo_root.is_dir() {
        bail!("repo root is not a directory: {}", repo_root.display());
    }

    let compose_file = resolve_under_root(
        &repo_root,
        args.compose_file
            .unwrap_or_else(|| PathBuf::from("compose.yaml")),
    );
    let build_dir = repo_root.join("build");
    let build_rel = repo_relative_path(&repo_root, &build_dir)?;
    if build_rel != "build" {
        bail!(
            "refusing to use non-standard build directory under repo root: {}",
            build_dir.display()
        );
    }
    std::fs::create_dir_all(&build_dir)
        .with_context(|| format!("cannot create build dir: {}", build_dir.display()))?;

    let build_output_dir = build_dir.join("output");
    if build_output_dir.exists() {
        let build_output_real = std::fs::canonicalize(&build_output_dir).with_context(|| {
            format!(
                "cannot resolve build output directory: {}",
                build_output_dir.display()
            )
        })?;
        let build_output_rel = repo_relative_path(&repo_root, &build_output_real)?;
        if build_output_rel != "build/output" {
            bail!(
                "refusing to remove non-standard build output path: {}",
                build_output_dir.display()
            );
        }
        std::fs::remove_dir_all(&build_output_dir).with_context(|| {
            format!(
                "cannot remove prior build output directory: {}",
                build_output_dir.display()
            )
        })?;
    }

    let default_key = build_dir.join("packer_ssh_key");
    let key_path = resolve_under_root(&repo_root, args.key.clone().unwrap_or(default_key.clone()));
    let uses_default_key = key_path == default_key;
    if uses_default_key && !key_path.exists() {
        println!("generating ephemeral SSH key at {}", key_path.display());
        run_command(
            "ssh-keygen",
            &[
                "-t".into(),
                "ed25519".into(),
                "-N".into(),
                "".into(),
                "-f".into(),
                key_path.display().to_string(),
            ],
            &[],
            "failed to generate default packer SSH key",
        )?;
    }

    let public_key_path = PathBuf::from(format!("{}.pub", key_path.display()));
    if !key_path.is_file() {
        bail!("SSH private key not found: {}", key_path.display());
    }
    if !public_key_path.is_file() {
        bail!("SSH public key not found: {}", public_key_path.display());
    }
    let public_key = std::fs::read_to_string(&public_key_path)
        .with_context(|| format!("cannot read SSH public key: {}", public_key_path.display()))?
        .trim()
        .to_string();
    let key_real = std::fs::canonicalize(&key_path)
        .with_context(|| format!("cannot resolve SSH private key: {}", key_path.display()))?;
    let _ = repo_relative_path(&repo_root, &key_real)?;
    let rel_key_path = repo_relative_path(&repo_root, &key_path)?;

    let host_uid = std::env::var("HOST_UID").unwrap_or(run_capture("id", &["-u"])?);
    let host_gid = std::env::var("HOST_GID").unwrap_or(run_capture("id", &["-g"])?);
    let host_kvm_gid = resolve_host_kvm_gid(
        std::env::var("HOST_KVM_GID").ok(),
        getent_group_kvm_output(),
    );
    let env_pairs = [
        ("HOST_UID", host_uid.as_str()),
        ("HOST_GID", host_gid.as_str()),
        ("HOST_KVM_GID", host_kvm_gid.as_str()),
    ];

    let compose_base = compose_base_args(&repo_root, &compose_file);

    println!(
        "running packer init in compose service {}",
        args.compose_service
    );
    run_command(
        "docker",
        &packer_init_args(&compose_base, &args.compose_service),
        &env_pairs,
        "packer init failed",
    )?;

    println!(
        "running packer build in compose service {}",
        args.compose_service
    );
    run_command(
        "docker",
        &packer_build_args(
            &compose_base,
            &args.compose_service,
            &rel_key_path,
            &public_key,
        ),
        &env_pairs,
        "packer build failed",
    )?;

    if args.compress {
        let source = build_output_dir.join("debian-13-botwork.qcow2");
        if !source.is_file() {
            bail!(
                "qcow2 source image not found for compression: {}",
                source.display()
            );
        }
        let target = build_dir.join("debian-13-botwork-compressed.qcow2");
        let rel_source = repo_relative_path(&repo_root, &source)?;
        let rel_target = repo_relative_path(&repo_root, &target)?;
        println!("compressing qcow2 to {}", target.display());
        run_command(
            "docker",
            &compress_args(
                &compose_base,
                &args.compose_service,
                &rel_source,
                &rel_target,
            ),
            &env_pairs,
            "qcow2 compression failed",
        )?;
    }

    println!("pack complete");
    Ok(())
}

fn resolve_host_kvm_gid(env_value: Option<String>, getent_output: Option<String>) -> String {
    if let Some(value) = env_value.filter(|value| !value.trim().is_empty()) {
        return value;
    }

    if let Some(line) = getent_output {
        if let Some(gid) = line
            .lines()
            .find_map(|entry| entry.split(':').nth(2))
            .map(str::trim)
            .filter(|gid| !gid.is_empty())
        {
            return gid.to_string();
        }
    }

    "993".to_string()
}

fn compose_base_args(repo_root: &Path, compose_file: &Path) -> Vec<String> {
    vec![
        "compose".into(),
        "--project-directory".into(),
        repo_root.display().to_string(),
        "-f".into(),
        compose_file.display().to_string(),
    ]
}

fn packer_init_args(base_args: &[String], service: &str) -> Vec<String> {
    let mut args = base_args.to_vec();
    args.extend([
        "run".into(),
        "--rm".into(),
        service.into(),
        "packer".into(),
        "init".into(),
        "images/".into(),
    ]);
    args
}

fn packer_build_args(
    base_args: &[String],
    service: &str,
    rel_key_path: &str,
    public_key: &str,
) -> Vec<String> {
    let mut args = base_args.to_vec();
    args.extend([
        "run".into(),
        "--rm".into(),
        service.into(),
        "packer".into(),
        "build".into(),
        "-var".into(),
        "accelerator=kvm".into(),
        "-var".into(),
        format!("ssh_private_key_file={rel_key_path}"),
        "-var".into(),
        format!("ssh_public_key={public_key}"),
        "images/".into(),
    ]);
    args
}

fn compress_args(
    base_args: &[String],
    service: &str,
    rel_source: &str,
    rel_target: &str,
) -> Vec<String> {
    let mut args = base_args.to_vec();
    args.extend([
        "run".into(),
        "--rm".into(),
        service.into(),
        "qemu-img".into(),
        "convert".into(),
        "-O".into(),
        "qcow2".into(),
        "-c".into(),
        rel_source.into(),
        rel_target.into(),
    ]);
    args
}

fn run_capture(program: &str, args: &[&str]) -> Result<String> {
    let output = Command::new(program)
        .args(args)
        .output()
        .with_context(|| format!("failed to execute {program}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("{program} failed: {}", stderr.trim());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn getent_group_kvm_output() -> Option<String> {
    Command::new("getent")
        .args(["group", "kvm"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::{packer_build_args, resolve_host_kvm_gid};

    #[test]
    fn resolve_host_kvm_gid_prefers_env_override() {
        let gid = resolve_host_kvm_gid(Some("1234".into()), Some("kvm:x:55:".into()));
        assert_eq!(gid, "1234");
    }

    #[test]
    fn resolve_host_kvm_gid_parses_getent_output() {
        let gid = resolve_host_kvm_gid(None, Some("kvm:x:77:qemu".into()));
        assert_eq!(gid, "77");
    }

    #[test]
    fn resolve_host_kvm_gid_falls_back_to_default() {
        let gid = resolve_host_kvm_gid(None, None);
        assert_eq!(gid, "993");
    }

    #[test]
    fn packer_build_args_match_expected_argv() {
        let args = packer_build_args(
            &[
                "compose".into(),
                "--project-directory".into(),
                "/repo/root".into(),
                "-f".into(),
                "/repo/root/compose.yaml".into(),
            ],
            "tools-kvm",
            "build/packer_ssh_key",
            "ssh-ed25519 AAAA example",
        );
        assert_eq!(
            args,
            vec![
                "compose",
                "--project-directory",
                "/repo/root",
                "-f",
                "/repo/root/compose.yaml",
                "run",
                "--rm",
                "tools-kvm",
                "packer",
                "build",
                "-var",
                "accelerator=kvm",
                "-var",
                "ssh_private_key_file=build/packer_ssh_key",
                "-var",
                "ssh_public_key=ssh-ed25519 AAAA example",
                "images/",
            ]
        );
    }
}
