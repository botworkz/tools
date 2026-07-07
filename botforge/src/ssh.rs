use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

#[derive(Clone)]
pub(crate) struct SshOptions {
    pub(crate) host: String,
    pub(crate) port: u16,
    pub(crate) user: String,
    pub(crate) key: PathBuf,
}

/// Returns true when ssh/scp should use full OpenSSH verbosity instead of the
/// quiet default (`-o LogLevel=ERROR`).  Set `BOTFORGE_SSH_VERBOSE=1` (or
/// `true` / `yes`) to restore the "Permanently added …" warnings and other
/// informational output.  `BOTFORGE_DEBUG=1` is accepted as an alias so that
/// a single debug switch turns everything chatty at once.
pub(crate) fn ssh_verbose_enabled() -> bool {
    fn truthy(var: &str) -> bool {
        std::env::var(var)
            .map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
            .unwrap_or(false)
    }
    truthy("BOTFORGE_SSH_VERBOSE") || truthy("BOTFORGE_DEBUG")
}

fn ssh_command_args_with_verbose(
    ssh: &SshOptions,
    remote_command: &str,
    connect_timeout_secs: u64,
    verbose: bool,
) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "-o".into(),
        "StrictHostKeyChecking=no".into(),
        "-o".into(),
        "UserKnownHostsFile=/dev/null".into(),
        "-o".into(),
        format!("ConnectTimeout={connect_timeout_secs}"),
    ];
    if !verbose {
        args.push("-o".into());
        args.push("LogLevel=ERROR".into());
    }
    args.extend([
        "-i".into(),
        ssh.key.display().to_string(),
        "-p".into(),
        ssh.port.to_string(),
        format!("{}@{}", ssh.user, ssh.host),
        remote_command.into(),
    ]);
    args
}

fn scp_command_args_with_verbose(
    ssh: &SshOptions,
    src: &Path,
    dest: &str,
    verbose: bool,
) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "-o".into(),
        "StrictHostKeyChecking=no".into(),
        "-o".into(),
        "UserKnownHostsFile=/dev/null".into(),
    ];
    if !verbose {
        args.push("-o".into());
        args.push("LogLevel=ERROR".into());
    }
    args.extend([
        "-i".into(),
        ssh.key.display().to_string(),
        "-P".into(),
        ssh.port.to_string(),
        src.display().to_string(),
        format!("{}@{}:{dest}", ssh.user, ssh.host),
    ]);
    args
}

pub(crate) fn ssh_command_args(
    ssh: &SshOptions,
    remote_command: &str,
    connect_timeout_secs: u64,
) -> Vec<String> {
    ssh_command_args_with_verbose(
        ssh,
        remote_command,
        connect_timeout_secs,
        ssh_verbose_enabled(),
    )
}

pub(crate) fn scp_command_args(ssh: &SshOptions, src: &Path, dest: &str) -> Vec<String> {
    scp_command_args_with_verbose(ssh, src, dest, ssh_verbose_enabled())
}

pub(crate) fn journalctl_command(units: &[String]) -> String {
    if units.is_empty() {
        return "sudo journalctl --no-pager -n 200".into();
    }
    let mut parts = vec!["sudo journalctl".to_string()];
    for unit in units {
        parts.push(format!("-u {unit}"));
    }
    parts.push("--no-pager -n 200".to_string());
    parts.join(" ")
}

pub(crate) fn wait_for_ssh(ssh: &SshOptions, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        if ssh_with_retry(
            ssh,
            "true",
            1,
            Duration::from_secs(0),
            Duration::from_secs(10),
        )
        .is_ok()
        {
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!("timed out waiting for SSH");
        }
        std::thread::sleep(Duration::from_secs(2));
    }
}

pub(crate) fn require_stable_ssh(
    ssh: &SshOptions,
    attempts: usize,
    required_consecutive: usize,
) -> Result<()> {
    let mut consecutive = 0usize;
    for _ in 0..attempts {
        if ssh_with_retry(
            ssh,
            "true",
            1,
            Duration::from_secs(0),
            Duration::from_secs(10),
        )
        .is_ok()
        {
            consecutive += 1;
            if consecutive >= required_consecutive {
                return Ok(());
            }
        } else {
            consecutive = 0;
        }
        std::thread::sleep(Duration::from_secs(2));
    }
    bail!("SSH was not stable enough after {attempts} probes")
}

pub(crate) fn ssh_with_retry(
    ssh: &SshOptions,
    remote_command: &str,
    retries: usize,
    retry_delay: Duration,
    connect_timeout: Duration,
) -> Result<()> {
    let args = ssh_command_args(ssh, remote_command, connect_timeout.as_secs());
    retry_transport_cmd("ssh", &args, retries, retry_delay, "ssh command failed")
}

pub(crate) fn scp_with_retry(
    ssh: &SshOptions,
    src: &Path,
    dest: &str,
    retries: usize,
    retry_delay: Duration,
) -> Result<()> {
    let args = scp_command_args(ssh, src, dest);
    retry_transport_cmd("scp", &args, retries, retry_delay, "scp command failed")
}

pub(crate) fn retry_transport_cmd(
    program: &str,
    args: &[String],
    retries: usize,
    retry_delay: Duration,
    failure_context: &str,
) -> Result<()> {
    let mut attempts = 0usize;
    loop {
        let status = Command::new(program)
            .args(args)
            .status()
            .with_context(|| format!("failed to execute {program}"))?;
        if status.success() {
            return Ok(());
        }
        attempts += 1;
        if status.code() != Some(255) || attempts >= retries {
            bail!("{failure_context} (exit status: {status})");
        }
        std::thread::sleep(retry_delay);
    }
}

/// Like [`retry_transport_cmd`] but captures and returns stdout on success.
fn retry_transport_cmd_capture(
    program: &str,
    args: &[String],
    retries: usize,
    retry_delay: Duration,
    failure_context: &str,
) -> Result<String> {
    let mut attempts = 0usize;
    loop {
        let output = Command::new(program)
            .args(args)
            .output()
            .with_context(|| format!("failed to execute {program}"))?;
        if output.status.success() {
            return Ok(String::from_utf8_lossy(&output.stdout).into_owned());
        }
        attempts += 1;
        if output.status.code() != Some(255) || attempts >= retries {
            bail!("{failure_context} (exit status: {})", output.status);
        }
        std::thread::sleep(retry_delay);
    }
}

/// Run a remote SSH command and return its stdout as a `String`.
///
/// Uses the same retry/timeout semantics as [`ssh_with_retry`] but captures
/// stdout instead of inheriting it.
pub(crate) fn ssh_capture_stdout(
    ssh: &SshOptions,
    remote_command: &str,
    retries: usize,
    retry_delay: Duration,
    connect_timeout: Duration,
) -> Result<String> {
    let args = ssh_command_args(ssh, remote_command, connect_timeout.as_secs());
    retry_transport_cmd_capture("ssh", &args, retries, retry_delay, "ssh command failed")
}

#[cfg(test)]
mod tests {
    use super::{
        journalctl_command, retry_transport_cmd_capture, scp_command_args_with_verbose,
        ssh_command_args_with_verbose, SshOptions,
    };
    use std::path::Path;
    use std::time::Duration;

    fn test_ssh_opts() -> SshOptions {
        SshOptions {
            host: "127.0.0.1".into(),
            port: 2222,
            user: "bot".into(),
            key: Path::new("/tmp/key").to_path_buf(),
        }
    }

    #[test]
    fn ssh_command_args_match_expected_argv() {
        assert_eq!(
            ssh_command_args_with_verbose(&test_ssh_opts(), "true", 10, false),
            vec![
                "-o",
                "StrictHostKeyChecking=no",
                "-o",
                "UserKnownHostsFile=/dev/null",
                "-o",
                "ConnectTimeout=10",
                "-o",
                "LogLevel=ERROR",
                "-i",
                "/tmp/key",
                "-p",
                "2222",
                "bot@127.0.0.1",
                "true"
            ]
        );
    }

    #[test]
    fn ssh_command_args_verbose_omits_log_level() {
        let args = ssh_command_args_with_verbose(&test_ssh_opts(), "true", 10, true);
        assert!(!args.contains(&"LogLevel=ERROR".to_string()));
        assert_eq!(
            args,
            vec![
                "-o",
                "StrictHostKeyChecking=no",
                "-o",
                "UserKnownHostsFile=/dev/null",
                "-o",
                "ConnectTimeout=10",
                "-i",
                "/tmp/key",
                "-p",
                "2222",
                "bot@127.0.0.1",
                "true"
            ]
        );
    }

    #[test]
    fn scp_command_args_match_expected_argv() {
        assert_eq!(
            scp_command_args_with_verbose(
                &test_ssh_opts(),
                Path::new("/tmp/local"),
                "/tmp/remote",
                false
            ),
            vec![
                "-o",
                "StrictHostKeyChecking=no",
                "-o",
                "UserKnownHostsFile=/dev/null",
                "-o",
                "LogLevel=ERROR",
                "-i",
                "/tmp/key",
                "-P",
                "2222",
                "/tmp/local",
                "bot@127.0.0.1:/tmp/remote"
            ]
        );
    }

    #[test]
    fn scp_command_args_verbose_omits_log_level() {
        let args = scp_command_args_with_verbose(
            &test_ssh_opts(),
            Path::new("/tmp/local"),
            "/tmp/remote",
            true,
        );
        assert!(!args.contains(&"LogLevel=ERROR".to_string()));
        assert_eq!(
            args,
            vec![
                "-o",
                "StrictHostKeyChecking=no",
                "-o",
                "UserKnownHostsFile=/dev/null",
                "-i",
                "/tmp/key",
                "-P",
                "2222",
                "/tmp/local",
                "bot@127.0.0.1:/tmp/remote"
            ]
        );
    }

    #[test]
    fn journalctl_command_includes_units() {
        let cmd = journalctl_command(&["ssh".to_string(), "botwork-launcher".to_string()]);
        assert_eq!(
            cmd,
            "sudo journalctl -u ssh -u botwork-launcher --no-pager -n 200"
        );
    }

    #[test]
    fn retry_transport_cmd_capture_returns_stdout_on_success() {
        let args: Vec<String> = vec!["hello from capture".to_string()];
        let result =
            retry_transport_cmd_capture("echo", &args, 1, Duration::from_secs(0), "echo failed");
        assert!(result.is_ok());
        assert_eq!(result.unwrap().trim(), "hello from capture");
    }

    #[test]
    fn retry_transport_cmd_capture_fails_on_nonzero_exit() {
        let result =
            retry_transport_cmd_capture("false", &[], 1, Duration::from_secs(0), "expected fail");
        assert!(result.is_err());
    }
}
