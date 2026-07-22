use anyhow::{bail, Context, Result};
use serde::de::{self, Deserializer};
use serde::Deserialize;
use std::fs::File;
use std::io::IsTerminal;
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use crate::compress::create_qcow2_overlay;

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
    create_qcow2_overlay(base_image, overlay_image)
}

pub(crate) fn qemu_run_args(
    overlay_image: &Path,
    seed_iso: &Path,
    extra_isos: &[PathBuf],
    ssh_port: u16,
    extra_ports: &[PortSpec],
    memsize: u32,
    smp: u32,
) -> Vec<String> {
    let mut args = vec![
        "-accel".into(),
        "kvm".into(),
        "-m".into(),
        memsize.to_string(),
        "-smp".into(),
        smp.to_string(),
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
    crate::plan::print_phase("vm", "Starting vm");
    let log = File::create(log_path)
        .with_context(|| format!("cannot create VM log file: {}", log_path.display()))?;
    let log_err = log
        .try_clone()
        .with_context(|| format!("cannot clone VM log file handle: {}", log_path.display()))?;
    Command::new("qemu-system-x86_64")
        .args(args)
        // Place QEMU in its own process group so that keyboard-generated
        // signals (Ctrl-C SIGINT, SIGQUIT, SIGTSTP) from the controlling
        // terminal are NOT delivered directly to QEMU by the kernel.  Only
        // botforge (the foreground process) receives the signal; botforge's
        // interrupt-aware teardown is then responsible for shutting down QEMU.
        // process_group(0) sets the child's PGID to its own PID.
        .process_group(0)
        // Detach stdin from the controlling TTY.  Background QEMU (-nographic)
        // does not need interactive serial console input; with stdin null it
        // cannot be stopped by SIGTTIN/SIGTTOU when it is in a non-foreground
        // process group.  (The --attach path uses spawn_qemu_attached which
        // intentionally inherits stdio for the interactive serial console.)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err))
        .spawn()
        .context("failed to launch qemu in background")
}

/// RAII guard that saves and restores the terminal's raw-mode state.
///
/// Constructed by [`spawn_qemu_attached`] before QEMU is spawned with inherited
/// stdio.  On drop (normal exit, early return, or panic) the original terminal
/// attributes are restored via `tcsetattr(STDIN_FILENO, TCSANOW, …)`.
///
/// When stdin is not a TTY the guard is a no-op (`None` variant), which is why
/// [`spawn_qemu_attached`] returns `Option<TerminalGuard>` rather than the
/// guard directly.
pub(crate) struct TerminalGuard {
    saved: nix::sys::termios::Termios,
}

impl TerminalGuard {
    /// Save the current terminal attributes for stdin.  Returns `None` when
    /// stdin is not a TTY (i.e. when `tcgetattr` would fail).
    fn save() -> Option<Self> {
        use nix::sys::termios;
        // STDIN_FILENO = 0
        termios::tcgetattr(std::io::stdin())
            .ok()
            .map(|saved| Self { saved })
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        use nix::sys::termios::{tcsetattr, SetArg};
        // Restore terminal on every exit path (normal, early return, or panic).
        // Ignore errors — we are in a destructor and cannot propagate them.
        let _ = tcsetattr(std::io::stdin(), SetArg::TCSANOW, &self.saved);
    }
}

/// Spawn QEMU with an interactive serial console attached to the process stdio.
///
/// Compared to [`spawn_qemu_with_log`] this function:
/// - Appends `-serial mon:stdio` to `args` (guest serial console + QEMU monitor
///   multiplexed on stdio; use `Ctrl-A c` to toggle between them).
/// - Launches QEMU with stdin/stdout/stderr **inherited** from the current
///   process rather than redirected to a log file.
/// - If stdin is a TTY: saves the current terminal attributes and returns a
///   [`TerminalGuard`] that restores them on drop, so that the terminal is
///   always returned to its original state even if QEMU is killed with SIGKILL.
///   QEMU itself also calls `cfmakeraw` internally; the guard covers the case
///   where QEMU is killed before it can clean up.
///
/// **Tradeoff vs. non-attach mode**: in attach mode stdout/stderr are inherited,
/// so no VM console output is written to `log_path`.  The `log_path` file is
/// still created (empty) so that call sites that call `print_log_tail` on
/// failure do not panic on a missing path; the tail will simply be empty.
///
/// # Non-TTY fallback
/// If stdin is **not** a TTY (e.g. under CI without a PTY), a warning is printed
/// and the function falls back to non-interactive background mode — identical to
/// [`spawn_qemu_with_log`] — and returns `None` for the guard.
pub(crate) fn spawn_qemu_attached(
    args: &[String],
    log_path: &Path,
) -> Result<(Child, Option<TerminalGuard>)> {
    if !std::io::stdin().is_terminal() {
        eprintln!(
            "warning: --attach requested but stdin is not a TTY; \
             falling back to non-interactive background mode"
        );
        let child = spawn_qemu_with_log(args, log_path)?;
        return Ok((child, None));
    }

    crate::plan::print_phase(
        "vm",
        "Starting vm (attached console — Ctrl-A c for QEMU monitor)",
    );

    // Create an empty log file so that call sites that call `print_log_tail`
    // on the failure path do not fail on a missing file.
    File::create(log_path)
        .with_context(|| format!("cannot create VM log file: {}", log_path.display()))?;

    // Save terminal state before QEMU takes over raw mode.
    let guard = TerminalGuard::save();

    let mut qemu_args = args.to_vec();
    qemu_args.push("-serial".into());
    qemu_args.push("mon:stdio".into());

    let child = Command::new("qemu-system-x86_64")
        .args(&qemu_args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .context("failed to launch qemu with attached console")?;

    Ok((child, guard))
}

#[cfg(test)]
mod tests {
    use super::{qemu_run_args, PortSpec};
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
    fn qemu_run_args_match_expected_argv() {
        let args = qemu_run_args(
            Path::new("/overlay.qcow2"),
            Path::new("/seed.iso"),
            &[PathBuf::from("/payload.iso")],
            2222,
            &[],
            4096,
            4,
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
                "4096",
                "-smp",
                "4",
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
            4096,
            4,
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
            4096,
            4,
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
            4096,
            4,
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
            4096,
            4,
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

    // --- attach path argv tests ---
    //
    // These tests verify that:
    // 1. The non-attach argv (qemu_run_args / qemu_build_args) is NOT modified.
    // 2. The attach path appends `-serial mon:stdio` on top of the base args.

    #[test]
    fn attach_path_run_args_include_serial_mon_stdio() {
        // The base args from qemu_run_args must NOT contain -serial.
        let base = qemu_run_args(
            Path::new("/overlay.qcow2"),
            Path::new("/seed.iso"),
            &[],
            2222,
            &[],
            4096,
            4,
        );
        assert!(
            !base.iter().any(|a| a == "-serial"),
            "non-attach qemu_run_args must not contain -serial: {base:?}"
        );
        assert!(
            base.contains(&"-nographic".to_string()),
            "non-attach qemu_run_args must still have -nographic: {base:?}"
        );

        // The attach variant appends -serial mon:stdio.
        let mut attach_args = base.clone();
        attach_args.push("-serial".into());
        attach_args.push("mon:stdio".into());

        let serial_idx = attach_args
            .iter()
            .position(|a| a == "-serial")
            .expect("-serial must be present in attach args");
        assert_eq!(
            attach_args[serial_idx + 1],
            "mon:stdio",
            "attach args must have mon:stdio after -serial"
        );
        assert!(
            attach_args.contains(&"-nographic".to_string()),
            "attach args must keep -nographic"
        );
    }

    #[test]
    fn attach_path_build_args_include_serial_mon_stdio() {
        use super::qemu_build_args;

        let base = qemu_build_args(
            Path::new("/partial.qcow2"),
            Path::new("/seed.iso"),
            2222,
            4096,
            4,
            false,
        );
        assert!(
            !base.iter().any(|a| a == "-serial"),
            "non-attach qemu_build_args must not contain -serial: {base:?}"
        );
        assert!(
            base.contains(&"-nographic".to_string()),
            "non-attach qemu_build_args must keep -nographic"
        );

        // Attach variant appends -serial mon:stdio.
        let mut attach_args = base.clone();
        attach_args.push("-serial".into());
        attach_args.push("mon:stdio".into());

        let serial_idx = attach_args
            .iter()
            .position(|a| a == "-serial")
            .expect("-serial must be present in attach args");
        assert_eq!(
            attach_args[serial_idx + 1],
            "mon:stdio",
            "attach args must have mon:stdio after -serial"
        );
    }

    // ── Background-spawn process-group/stdin regression ──────────────────────

    /// A background child spawned in its own process group with stdin=null must
    /// not be left in the stopped (`T`) state.  Before the fix, inheriting the
    /// controlling TTY stdin while in a non-foreground process group caused the
    /// kernel to deliver SIGTTIN and stop QEMU before it emitted any output,
    /// producing the silent hang at "(vm) Waiting for SSH".
    ///
    /// This test uses `sleep 5` as a stand-in for `qemu-system-x86_64` so
    /// it runs without KVM.
    #[test]
    #[cfg(target_os = "linux")]
    fn background_child_with_null_stdin_is_not_stopped() {
        use std::fs::File;
        use std::os::unix::process::CommandExt as _;
        use std::process::Stdio;

        let tmp = std::env::temp_dir().join(format!("botforge-spawn-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&tmp);
        let log_path = tmp.join("test.log");
        let log = File::create(&log_path).expect("cannot create test log");
        let log_err = log.try_clone().expect("cannot clone log handle");

        // Spawn `sleep 5` with the same settings as spawn_qemu_with_log:
        // process_group(0) + stdin(null).  It must not be stopped by SIGTTIN.
        let mut child = std::process::Command::new("sleep")
            .arg("5")
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(log_err))
            .spawn()
            .expect("failed to spawn 'sleep 5'");

        // Give the kernel a moment to potentially deliver SIGTTIN.
        std::thread::sleep(std::time::Duration::from_millis(150));

        // Read /proc/<pid>/stat to check the process state character.
        let pid = child.id();
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
        // The stat file has the format: "pid (comm) state ..."
        // Find the closing ')' then take the next non-whitespace token.
        let state = stat
            .find(')')
            .and_then(|i| stat[i + 1..].split_whitespace().next())
            .unwrap_or("");
        assert_ne!(
            state, "T",
            "background child must not be in stopped state \
             (T means SIGTTIN/SIGTTOU stopped it); /proc stat: {stat:?}"
        );

        let _ = child.kill();
        let _ = child.wait();
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
