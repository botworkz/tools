use anyhow::{bail, Context, Result};
use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use crate::util::run_command;

pub(crate) fn require_kvm() -> Result<()> {
    if !Path::new("/dev/kvm").exists() {
        bail!("KVM is required: /dev/kvm not found");
    }
    Ok(())
}

pub(crate) fn create_overlay_image(base_image: &Path, overlay_image: &Path) -> Result<()> {
    if !base_image.is_file() {
        bail!("base image not found: {}", base_image.display());
    }
    if let Some(parent) = overlay_image.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("cannot create overlay dir: {}", parent.display()))?;
    }
    let args = qemu_img_create_args(base_image, overlay_image);
    run_command("qemu-img", &args, &[], "qemu-img create overlay failed")
}

pub(crate) fn qemu_img_create_args(base_image: &Path, overlay_image: &Path) -> Vec<String> {
    vec![
        "create".into(),
        "-f".into(),
        "qcow2".into(),
        "-F".into(),
        "qcow2".into(),
        "-b".into(),
        base_image.display().to_string(),
        overlay_image.display().to_string(),
    ]
}

pub(crate) fn qemu_run_args(
    overlay_image: &Path,
    seed_iso: &Path,
    extra_isos: &[PathBuf],
    ssh_port: u16,
) -> Vec<String> {
    let mut args = vec![
        "-accel".into(),
        "kvm".into(),
        "-m".into(),
        "2048".into(),
        "-smp".into(),
        "2".into(),
        "-cpu".into(),
        "host".into(),
        "-drive".into(),
        format!("file={},if=virtio,format=qcow2", overlay_image.display()),
        "-drive".into(),
        format!("file={},media=cdrom,readonly=on", seed_iso.display()),
    ];
    for iso in extra_isos {
        args.push("-drive".into());
        args.push(format!("file={},media=cdrom,readonly=on", iso.display()));
    }
    args.extend([
        "-netdev".into(),
        format!("user,id=net0,hostfwd=tcp:127.0.0.1:{ssh_port}-:22"),
        "-device".into(),
        "virtio-net-pci,netdev=net0".into(),
        "-nographic".into(),
    ]);
    args
}

pub(crate) fn spawn_qemu_with_log(args: &[String], log_path: &Path) -> Result<Child> {
    let log = File::create(log_path)
        .with_context(|| format!("cannot create VM log file: {}", log_path.display()))?;
    let log_err = log
        .try_clone()
        .with_context(|| format!("cannot clone VM log file handle: {}", log_path.display()))?;
    Command::new("qemu-system-x86_64")
        .args(args)
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err))
        .spawn()
        .context("failed to launch qemu in background")
}

#[cfg(test)]
mod tests {
    use super::{qemu_img_create_args, qemu_run_args};
    use std::path::{Path, PathBuf};

    #[test]
    fn qemu_img_create_args_match_expected_argv() {
        assert_eq!(
            qemu_img_create_args(Path::new("/base.qcow2"), Path::new("/overlay.qcow2")),
            vec![
                "create",
                "-f",
                "qcow2",
                "-F",
                "qcow2",
                "-b",
                "/base.qcow2",
                "/overlay.qcow2"
            ]
        );
    }

    #[test]
    fn qemu_run_args_match_expected_argv() {
        let args = qemu_run_args(
            Path::new("/overlay.qcow2"),
            Path::new("/seed.iso"),
            &[PathBuf::from("/payload.iso")],
            2222,
        );
        assert!(
            !args.iter().any(|a| a.contains("/base.qcow2")),
            "base image must not appear in qemu args"
        );
        assert_eq!(
            args,
            vec![
                "-accel",
                "kvm",
                "-m",
                "2048",
                "-smp",
                "2",
                "-cpu",
                "host",
                "-drive",
                "file=/overlay.qcow2,if=virtio,format=qcow2",
                "-drive",
                "file=/seed.iso,media=cdrom,readonly=on",
                "-drive",
                "file=/payload.iso,media=cdrom,readonly=on",
                "-netdev",
                "user,id=net0,hostfwd=tcp:127.0.0.1:2222-:22",
                "-device",
                "virtio-net-pci,netdev=net0",
                "-nographic"
            ]
        );
    }
}
