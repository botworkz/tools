use anyhow::{bail, Context, Result};
use serde::de::{self, Deserializer};
use serde::Deserialize;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use crate::util::run_command;

/// A forwarded port specification: a bind address and a port number.
///
/// Deserializes from either a bare integer (loopback bind) or a `"<addr>:<port>"` string.
#[derive(Debug, PartialEq)]
pub(crate) struct PortSpec {
    pub(crate) addr: String,
    pub(crate) port: u16,
}

impl<'de> Deserialize<'de> for PortSpec {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Int(u16),
            Str(String),
        }
        match Raw::deserialize(deserializer)? {
            Raw::Int(port) => Ok(PortSpec {
                addr: "127.0.0.1".into(),
                port,
            }),
            Raw::Str(s) => {
                let colon = s.rfind(':').ok_or_else(|| {
                    de::Error::custom(format!(
                        "invalid port spec {s:?}: expected \"<addr>:<port>\" or a bare integer"
                    ))
                })?;
                let addr = s[..colon].to_string();
                let port_str = &s[colon + 1..];
                if addr.is_empty() {
                    return Err(de::Error::custom(format!(
                        "invalid port spec {s:?}: address must not be empty"
                    )));
                }
                let port = port_str.parse::<u16>().map_err(|_| {
                    de::Error::custom(format!(
                        "invalid port spec {s:?}: port {port_str:?} is not a valid port number (1-65535)"
                    ))
                })?;
                Ok(PortSpec { addr, port })
            }
        }
    }
}

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
    extra_ports: &[PortSpec],
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
    let mut netdev = format!("user,id=net0,hostfwd=tcp:0.0.0.0:{ssh_port}-:22");
    for spec in extra_ports {
        netdev.push_str(&format!(
            ",hostfwd=tcp:{}:{}-:{}",
            spec.addr, spec.port, spec.port
        ));
    }

    args.extend([
        "-netdev".into(),
        netdev,
        "-device".into(),
        "virtio-net-pci,netdev=net0".into(),
        "-nographic".into(),
    ]);
    args
}

/// Build qemu arguments for a `botforge build` run.
///
/// The primary drive is `partial_image` opened **read-write directly** — no
/// CoW overlay is created.  VM writes land in the partial image, which becomes
/// the output artifact after a clean shutdown.
pub(crate) fn qemu_build_args(
    partial_image: &Path,
    seed_iso: &Path,
    ssh_port: u16,
    memsize: u32,
    smp: u32,
    enable_discard_unmap: bool,
) -> Vec<String> {
    let netdev = format!("user,id=net0,hostfwd=tcp:0.0.0.0:{ssh_port}-:22");
    let mut primary_drive = format!("file={},if=virtio,format=qcow2", partial_image.display());
    if enable_discard_unmap {
        primary_drive.push_str(",discard=unmap");
    }
    vec![
        "-accel".into(),
        "kvm".into(),
        "-m".into(),
        memsize.to_string(),
        "-smp".into(),
        smp.to_string(),
        "-cpu".into(),
        "host".into(),
        "-drive".into(),
        primary_drive,
        "-drive".into(),
        format!("file={},media=cdrom,readonly=on", seed_iso.display()),
        "-netdev".into(),
        netdev,
        "-device".into(),
        "virtio-net-pci,netdev=net0".into(),
        "-nographic".into(),
    ]
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
    use super::{qemu_img_create_args, qemu_run_args, PortSpec};
    use std::path::{Path, PathBuf};

    fn loopback(port: u16) -> PortSpec {
        PortSpec {
            addr: "127.0.0.1".into(),
            port,
        }
    }

    fn allif(port: u16) -> PortSpec {
        PortSpec {
            addr: "0.0.0.0".into(),
            port,
        }
    }

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
            &[],
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
                "user,id=net0,hostfwd=tcp:0.0.0.0:2222-:22",
                "-device",
                "virtio-net-pci,netdev=net0",
                "-nographic"
            ]
        );
    }

    #[test]
    fn qemu_run_args_default_ssh_forward_binds_all_interfaces() {
        let args = qemu_run_args(
            Path::new("/overlay.qcow2"),
            Path::new("/seed.iso"),
            &[],
            2222,
            &[],
        );
        let netdev_index = args.iter().position(|arg| arg == "-netdev").unwrap() + 1;
        assert!(
            args[netdev_index].starts_with("user,id=net0,hostfwd=tcp:0.0.0.0:2222-:22"),
            "netdev should begin with built-in SSH forward bound to all interfaces"
        );
    }

    #[test]
    fn qemu_run_args_includes_extra_host_forwards() {
        let args = qemu_run_args(
            Path::new("/overlay.qcow2"),
            Path::new("/seed.iso"),
            &[PathBuf::from("/payload.iso")],
            2222,
            &[loopback(80)],
        );
        let netdev_index = args.iter().position(|arg| arg == "-netdev").unwrap() + 1;
        assert_eq!(
            args[netdev_index],
            "user,id=net0,hostfwd=tcp:0.0.0.0:2222-:22,hostfwd=tcp:127.0.0.1:80-:80"
        );
    }

    #[test]
    fn qemu_run_args_extra_host_forward_with_custom_addr() {
        let args = qemu_run_args(
            Path::new("/overlay.qcow2"),
            Path::new("/seed.iso"),
            &[],
            2222,
            &[allif(9901)],
        );
        let netdev_index = args.iter().position(|arg| arg == "-netdev").unwrap() + 1;
        assert_eq!(
            args[netdev_index],
            "user,id=net0,hostfwd=tcp:0.0.0.0:2222-:22,hostfwd=tcp:0.0.0.0:9901-:9901"
        );
    }

    #[test]
    fn qemu_run_args_extra_host_forwards_mixed() {
        let args = qemu_run_args(
            Path::new("/overlay.qcow2"),
            Path::new("/seed.iso"),
            &[],
            2222,
            &[loopback(80), allif(9901)],
        );
        let netdev_index = args.iter().position(|arg| arg == "-netdev").unwrap() + 1;
        assert_eq!(
            args[netdev_index],
            "user,id=net0,hostfwd=tcp:0.0.0.0:2222-:22,hostfwd=tcp:127.0.0.1:80-:80,hostfwd=tcp:0.0.0.0:9901-:9901"
        );
    }

    #[test]
    fn qemu_build_args_match_expected_argv() {
        use super::qemu_build_args;
        let args = qemu_build_args(
            Path::new("/partial.qcow2"),
            Path::new("/seed.iso"),
            2222,
            4096,
            4,
            false,
        );
        assert_eq!(
            args,
            vec![
                "-accel",
                "kvm",
                "-m",
                "4096",
                "-smp",
                "4",
                "-cpu",
                "host",
                "-drive",
                "file=/partial.qcow2,if=virtio,format=qcow2",
                "-drive",
                "file=/seed.iso,media=cdrom,readonly=on",
                "-netdev",
                "user,id=net0,hostfwd=tcp:0.0.0.0:2222-:22",
                "-device",
                "virtio-net-pci,netdev=net0",
                "-nographic"
            ]
        );
    }

    #[test]
    fn qemu_build_args_partial_image_no_overlay() {
        use super::qemu_build_args;
        // The partial image path must appear directly in the drive argument,
        // not behind a qcow2 backing-file overlay.
        let args = qemu_build_args(
            Path::new("/build/out.qcow2.partial"),
            Path::new("/seed.iso"),
            2222,
            4096,
            4,
            false,
        );
        let drive_arg = args.iter().skip_while(|a| *a != "-drive").nth(1).unwrap();
        assert!(
            drive_arg.contains("/build/out.qcow2.partial"),
            "partial image must appear in first drive arg: {drive_arg}"
        );
        // No backing-file= present — this is a direct read-write drive.
        assert!(
            !drive_arg.contains("backing-file"),
            "build drive must not use a backing file: {drive_arg}"
        );
    }

    #[test]
    fn qemu_build_args_include_discard_unmap_when_requested() {
        use super::qemu_build_args;
        let args = qemu_build_args(
            Path::new("/build/out.qcow2.partial"),
            Path::new("/seed.iso"),
            2222,
            4096,
            4,
            true,
        );
        let drive_arg = args.iter().skip_while(|a| *a != "-drive").nth(1).unwrap();
        assert_eq!(
            drive_arg,
            "file=/build/out.qcow2.partial,if=virtio,format=qcow2,discard=unmap"
        );
    }
}
