use anyhow::{bail, Context, Result};
use clap::Args;
use std::path::{Path, PathBuf};

use crate::util::{
    ensure_command, repo_relative_path, resolve_under_root, run_command, run_command_in_dir,
};

#[derive(Args, Debug)]
pub(crate) struct PackArgs {
    /// VM checkout root containing images/ (default: current directory).
    #[arg(long)]
    repo_root: Option<PathBuf>,
    /// Compress the qcow2 output with qemu-img convert -c.
    #[arg(long)]
    compress: bool,
    /// SSH private key path (default: <repo-root>/build/packer_ssh_key).
    #[arg(long)]
    key: Option<PathBuf>,
    /// Packer template path (file or directory), relative to --repo-root.
    /// Defaults to "images/" for backwards compatibility.
    #[arg(long, default_value = "images/")]
    template: PathBuf,
}

/// Run the KVM-only Packer flow natively inside the botforge container.
///
/// This intentionally does not build or stage dependencies/images; callers must
/// arrange that beforehand. KVM is required.
pub(crate) fn cmd_pack(args: PackArgs) -> Result<()> {
    if !Path::new("/dev/kvm").exists() {
        bail!("botforge pack requires KVM: /dev/kvm not found");
    }

    let repo_root = std::fs::canonicalize(
        args.repo_root
            .unwrap_or(std::env::current_dir().context("failed to determine current directory")?),
    )
    .context("failed to resolve repo root")?;
    if !repo_root.is_dir() {
        bail!("repo root is not a directory: {}", repo_root.display());
    }

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

    let packer_plugins_dir = build_dir.join("packer-plugins");
    std::fs::create_dir_all(&packer_plugins_dir).with_context(|| {
        format!(
            "cannot create PACKER_PLUGIN_PATH directory: {}",
            packer_plugins_dir.display()
        )
    })?;
    let packer_cache_dir = build_dir.join("packer-cache");
    std::fs::create_dir_all(&packer_cache_dir).with_context(|| {
        format!(
            "cannot create PACKER_CACHE_DIR directory: {}",
            packer_cache_dir.display()
        )
    })?;

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
    let template_abs = resolve_under_root(&repo_root, args.template.clone());
    let template_rel = repo_relative_path(&repo_root, &template_abs)
        .context("packer template path escapes repo root")?;
    if !template_abs.exists() {
        bail!("packer template not found: {}", template_abs.display());
    }

    ensure_command("packer")?;
    ensure_command("qemu-img")?;

    let plugin_path = packer_plugins_dir.display().to_string();
    let cache_path = packer_cache_dir.display().to_string();
    let packer_env = [
        ("PACKER_PLUGIN_PATH", plugin_path.as_str()),
        ("PACKER_CACHE_DIR", cache_path.as_str()),
    ];

    println!("running packer init");
    run_command_in_dir(
        "packer",
        &packer_init_args(&template_rel),
        &packer_env,
        &repo_root,
        "packer init failed",
    )?;
    println!("running packer build");
    run_command_in_dir(
        "packer",
        &packer_build_args(&rel_key_path, &public_key, &template_rel),
        &packer_env,
        &repo_root,
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
        run_command_in_dir(
            "qemu-img",
            &qemu_img_compress_args(&rel_source, &rel_target),
            &[],
            &repo_root,
            "qcow2 compression failed",
        )?;
    }

    println!("pack complete");
    Ok(())
}

fn packer_init_args(template: &str) -> Vec<String> {
    vec!["init".into(), template.into()]
}

fn packer_build_args(rel_key_path: &str, public_key: &str, template: &str) -> Vec<String> {
    vec![
        "build".into(),
        "-var".into(),
        "accelerator=kvm".into(),
        "-var".into(),
        format!("ssh_private_key_file={rel_key_path}"),
        "-var".into(),
        format!("ssh_public_key={public_key}"),
        template.into(),
    ]
}

fn qemu_img_compress_args(rel_source: &str, rel_target: &str) -> Vec<String> {
    vec![
        "convert".into(),
        "-O".into(),
        "qcow2".into(),
        "-c".into(),
        rel_source.into(),
        rel_target.into(),
    ]
}

#[cfg(test)]
mod tests {
    use super::{packer_build_args, packer_init_args, qemu_img_compress_args};

    #[test]
    fn packer_init_args_match_expected_argv() {
        let args = packer_init_args("images/");
        assert_eq!(args, vec!["init", "images/"]);
    }

    #[test]
    fn packer_build_args_match_expected_argv() {
        let args = packer_build_args(
            "build/packer_ssh_key",
            "ssh-ed25519 AAAA example",
            "images/",
        );
        assert_eq!(
            args,
            vec![
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

    #[test]
    fn packer_args_pass_through_custom_template_path() {
        assert_eq!(
            packer_init_args("images/botwork"),
            vec!["init", "images/botwork"]
        );
        assert_eq!(
            packer_build_args(
                "build/packer_ssh_key",
                "ssh-ed25519 AAAA example",
                "images/botwork",
            ),
            vec![
                "build",
                "-var",
                "accelerator=kvm",
                "-var",
                "ssh_private_key_file=build/packer_ssh_key",
                "-var",
                "ssh_public_key=ssh-ed25519 AAAA example",
                "images/botwork",
            ]
        );
    }

    #[test]
    fn qemu_img_compress_args_match_expected_argv() {
        let args = qemu_img_compress_args("build/output/base.qcow2", "build/base-compressed.qcow2");
        assert_eq!(
            args,
            vec![
                "convert",
                "-O",
                "qcow2",
                "-c",
                "build/output/base.qcow2",
                "build/base-compressed.qcow2"
            ]
        );
    }
}
