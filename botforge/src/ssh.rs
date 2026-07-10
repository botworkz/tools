//! Pure-Rust SSH/SFTP via `russh` + `russh-sftp`.
//!
//! All public functions here are **synchronous**.  Each call creates a
//! lightweight current-thread Tokio runtime, runs the async work inside
//! `block_on`, and returns.  A fresh runtime per public call is the simplest
//! correct option: no shared state, no cross-thread concerns, and the
//! current-thread variant carries no OS thread-pool overhead.
//!
//! # Retry semantics
//! `ssh_with_retry` and `scp_with_retry` retry only on **transport** errors
//! (connection refused, handshake failure, auth error — the server is still
//! coming up).  A remote command that actually ran and returned a non-zero
//! exit code is a `RemoteFailure`; it fails fast and is not retried, mirroring
//! the old `exit_code == 255` gate on the OpenSSH binary.

use anyhow::{bail, Context, Result};
use russh::client::{self, Config, Handle};
use russh::keys::{PrivateKeyWithHashAlg, load_secret_key};
use russh::{ChannelMsg, Disconnect};
use russh_sftp::client::SftpSession;
use russh_sftp::protocol::OpenFlags;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::AsyncWriteExt;

use crate::util::create_temp_dir;

// ---------------------------------------------------------------------------
// Public structs (unchanged public interface)
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub(crate) struct SshOptions {
    pub(crate) host: String,
    pub(crate) port: u16,
    pub(crate) user: String,
    pub(crate) key: PathBuf,
}

pub(crate) struct TemporarySshKeypair {
    dir: PathBuf,
    private_key: PathBuf,
    public_key: PathBuf,
}

impl TemporarySshKeypair {
    /// Generate an ephemeral ed25519 keypair in-process (no `ssh-keygen` subprocess).
    ///
    /// Writes:
    /// - `<dir>/id_ed25519`     — OpenSSH PEM private key (loadable by russh)
    /// - `<dir>/id_ed25519.pub` — one-line `ssh-ed25519 AAAA…` public key
    ///   (injected verbatim into cloud-init `ssh_authorized_keys`)
    pub(crate) fn generate(prefix: &str) -> Result<Self> {
        use getrandom::{SysRng, rand_core::UnwrapErr};
        use russh::keys::ssh_key::{Algorithm, LineEnding, PrivateKey};

        let dir = create_temp_dir(prefix)?;
        let private_key = dir.join("id_ed25519");
        let public_key = dir.join("id_ed25519.pub");

        // Generate ed25519 keypair using the OS RNG (getrandom's SysRng).
        let mut rng = UnwrapErr(SysRng);
        let key = PrivateKey::random(&mut rng, Algorithm::Ed25519)
            .context("failed to generate ed25519 keypair")?;

        // Write private key in OpenSSH PEM format.
        let private_pem = key
            .to_openssh(LineEnding::LF)
            .context("failed to serialise private key to OpenSSH format")?;
        std::fs::write(&private_key, private_pem.as_bytes())
            .context("failed to write private key")?;

        // Write public key in authorized_keys one-line format.
        let pub_openssh = key
            .public_key()
            .to_openssh()
            .context("failed to serialise public key to OpenSSH format")?;
        // Ensure there is a trailing newline so callers that read the file can
        // trim() safely, and the format exactly matches `ssh-keygen` output.
        std::fs::write(&public_key, format!("{pub_openssh}\n"))
            .context("failed to write public key")?;

        Ok(Self {
            dir,
            private_key,
            public_key,
        })
    }

    pub(crate) fn private_key(&self) -> &Path {
        &self.private_key
    }

    pub(crate) fn public_key(&self) -> &Path {
        &self.public_key
    }
}

impl Drop for TemporarySshKeypair {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

// ---------------------------------------------------------------------------
// Verbosity flag (unchanged)
// ---------------------------------------------------------------------------

/// Returns true when SSH logging should be verbose.
/// Set `BOTFORGE_SSH_VERBOSE=1` (or `true` / `yes`) to enable.
/// `BOTFORGE_DEBUG=1` is accepted as an alias.
pub(crate) fn ssh_verbose_enabled() -> bool {
    fn truthy(var: &str) -> bool {
        std::env::var(var)
            .map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
            .unwrap_or(false)
    }
    truthy("BOTFORGE_SSH_VERBOSE") || truthy("BOTFORGE_DEBUG")
}

// ---------------------------------------------------------------------------
// Pure string builder — unchanged
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// Outcome enum for streaming exec (used by plan/vm.rs)
// ---------------------------------------------------------------------------

/// Outcome of a single SSH exec attempt with output streaming.
#[derive(Debug)]
pub(crate) enum SshExecOutcome {
    /// Remote command exited with status 0.
    Success,
    /// Remote command ran and exited with a non-zero status.  Do **not** retry.
    RemoteFailure(u32),
    /// The per-step timeout expired during this attempt.
    StepTimeout,
    /// The overall run deadline expired during this attempt.
    OverallTimeout,
    /// SSH transport error (connection refused, handshake failure, auth failure).
    /// The caller should retry if the attempt budget permits.
    TransportError(anyhow::Error),
}

// ---------------------------------------------------------------------------
// Tokio runtime helper
// ---------------------------------------------------------------------------

fn make_rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("failed to build Tokio runtime for SSH")
}

// ---------------------------------------------------------------------------
// russh handler — accept all host keys (these are ephemeral throwaway VMs)
// ---------------------------------------------------------------------------

struct AcceptAllKeys;

impl client::Handler for AcceptAllKeys {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        _server_public_key: &russh::keys::ssh_key::PublicKey,
    ) -> std::result::Result<bool, Self::Error> {
        // Equivalent to OpenSSH -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null.
        // These VMs are ephemeral; accepting any host key is intentional.
        Ok(true)
    }
}

// ---------------------------------------------------------------------------
// Async internals
// ---------------------------------------------------------------------------

/// Connect and authenticate.  Returns the session handle on success.
async fn connect_async(
    ssh: &SshOptions,
    connect_timeout: Duration,
) -> Result<Handle<AcceptAllKeys>> {
    let config = Arc::new(Config {
        // No inactivity timeout — individual operations impose their own timeouts.
        inactivity_timeout: None,
        ..<_>::default()
    });

    let addr = format!("{}:{}", ssh.host, ssh.port);
    let connect_fut = client::connect(Arc::clone(&config), addr.as_str(), AcceptAllKeys);
    let mut session = tokio::time::timeout(connect_timeout, connect_fut)
        .await
        .context("SSH connect timed out")?
        .context("SSH connect failed")?;

    let key_pair =
        load_secret_key(&ssh.key, None).context("failed to load SSH private key")?;
    let best_hash = session
        .best_supported_rsa_hash()
        .await
        .context("failed to query server's supported hash algorithms")?
        .flatten();
    let auth_result = session
        .authenticate_publickey(
            ssh.user.as_str(),
            PrivateKeyWithHashAlg::new(Arc::new(key_pair), best_hash),
        )
        .await
        .context("SSH authentication failed")?;

    if !auth_result.success() {
        bail!("SSH authentication rejected by server");
    }
    Ok(session)
}

/// Attempt classification used for retry logic.
enum AttemptResult {
    /// Exit status 0 — success.
    Ok(String),
    /// Transport failure (connection/auth) — suitable for retry.
    Transport(anyhow::Error),
    /// Remote command ran and exited non-zero — fail fast.
    NonZero(u32),
}

/// Execute `command` on the session, returning captured stdout on success.
/// stdout is also forwarded to the process stdout when `capture` is false.
async fn exec_simple_async(
    ssh: &SshOptions,
    remote_command: &str,
    connect_timeout: Duration,
    capture: bool,
) -> AttemptResult {
    let mut session = match connect_async(ssh, connect_timeout).await {
        Ok(s) => s,
        Err(e) => return AttemptResult::Transport(e),
    };

    let mut channel = match session.channel_open_session().await {
        Ok(c) => c,
        Err(e) => {
            return AttemptResult::Transport(
                anyhow::Error::new(e).context("failed to open SSH channel"),
            )
        }
    };
    if let Err(e) = channel.exec(true, remote_command).await {
        return AttemptResult::Transport(
            anyhow::Error::new(e).context("failed to exec remote command"),
        );
    }

    let mut stdout: Vec<u8> = Vec::new();
    let mut exit_code: Option<u32> = None;
    let verbose = ssh_verbose_enabled();

    loop {
        let Some(msg) = channel.wait().await else {
            break;
        };
        match msg {
            ChannelMsg::Data { ref data } => {
                if capture {
                    stdout.extend_from_slice(data);
                } else if verbose {
                    let _ = std::io::Write::write_all(&mut std::io::stdout(), data);
                }
            }
            ChannelMsg::ExtendedData { ref data, ext: 1 } => {
                if verbose {
                    let _ = std::io::Write::write_all(&mut std::io::stderr(), data);
                }
            }
            ChannelMsg::ExitStatus {
                exit_status: code,
            } => {
                exit_code = Some(code);
            }
            _ => {}
        }
    }

    let _ = session
        .disconnect(Disconnect::ByApplication, "", "English")
        .await;

    match exit_code {
        Some(0) => AttemptResult::Ok(String::from_utf8_lossy(&stdout).into_owned()),
        Some(code) => AttemptResult::NonZero(code),
        None => AttemptResult::Transport(anyhow::anyhow!(
            "SSH channel closed without exit status"
        )),
    }
}

/// Execute `remote_command` with streaming output to `on_output(is_stderr, chunk)`.
/// Enforces `step_timeout` and `overall_deadline`.
async fn exec_logged_async(
    ssh: &SshOptions,
    remote_command: &str,
    connect_timeout: Duration,
    step_timeout: Duration,
    overall_deadline: Instant,
    on_output: &mut dyn FnMut(bool, &[u8]),
) -> SshExecOutcome {
    // Compute tokio-compatible deadlines from the std::time::Instant passed in.
    let now_std = Instant::now();
    let now_tokio = tokio::time::Instant::now();
    let overall_remaining = overall_deadline
        .checked_duration_since(now_std)
        .unwrap_or(Duration::ZERO);

    if overall_remaining.is_zero() {
        return SshExecOutcome::OverallTimeout;
    }

    let step_dl = now_tokio + step_timeout;
    let overall_dl = now_tokio + overall_remaining;

    // Connect within the connect_timeout, but also bounded by the overall deadline.
    let effective_connect_timeout =
        connect_timeout.min(overall_remaining);
    let mut session = match connect_async(ssh, effective_connect_timeout).await {
        Ok(s) => s,
        Err(e) => {
            // If overall deadline expired during connect, report that.
            if Instant::now() >= overall_deadline {
                return SshExecOutcome::OverallTimeout;
            }
            return SshExecOutcome::TransportError(e);
        }
    };

    let mut channel = match session.channel_open_session().await {
        Ok(c) => c,
        Err(e) => {
            return SshExecOutcome::TransportError(
                anyhow::Error::new(e).context("failed to open SSH channel"),
            )
        }
    };
    if let Err(e) = channel.exec(true, remote_command).await {
        return SshExecOutcome::TransportError(
            anyhow::Error::new(e).context("failed to exec remote command"),
        );
    }

    let mut exit_code: Option<u32> = None;

    loop {
        tokio::select! {
            msg = channel.wait() => {
                match msg {
                    None => break,
                    Some(ChannelMsg::Data { ref data }) => {
                        on_output(false, data);
                    }
                    Some(ChannelMsg::ExtendedData { ref data, ext: 1 }) => {
                        on_output(true, data);
                    }
                    Some(ChannelMsg::ExitStatus { exit_status: code }) => {
                        exit_code = Some(code);
                        // Do not break immediately; drain remaining data first.
                    }
                    Some(_) => {}
                }
            }
            _ = tokio::time::sleep_until(step_dl) => {
                return SshExecOutcome::StepTimeout;
            }
            _ = tokio::time::sleep_until(overall_dl) => {
                return SshExecOutcome::OverallTimeout;
            }
        }
    }

    let _ = session
        .disconnect(Disconnect::ByApplication, "", "English")
        .await;

    match exit_code {
        Some(0) => SshExecOutcome::Success,
        Some(code) => SshExecOutcome::RemoteFailure(code),
        None => SshExecOutcome::TransportError(anyhow::anyhow!(
            "SSH channel closed without exit status"
        )),
    }
}

/// Upload `src` to `dest` on the remote via SFTP.
async fn upload_async(ssh: &SshOptions, src: &Path, dest: &str) -> Result<()> {
    // SFTP uploads are preceded by wait_for_ssh, so the server is already up.
    // Use a generous default connect timeout.
    let connect_timeout = Duration::from_secs(30);
    let session = connect_async(ssh, connect_timeout).await?;

    let channel = session
        .channel_open_session()
        .await
        .context("failed to open SFTP channel")?;
    channel
        .request_subsystem(true, "sftp")
        .await
        .context("failed to request SFTP subsystem")?;

    let sftp = SftpSession::new(channel.into_stream())
        .await
        .context("failed to establish SFTP session")?;

    let data =
        std::fs::read(src).with_context(|| format!("failed to read: {}", src.display()))?;

    let mut remote_file = sftp
        .open_with_flags(
            dest,
            OpenFlags::CREATE | OpenFlags::TRUNCATE | OpenFlags::WRITE,
        )
        .await
        .with_context(|| format!("SFTP open failed for remote path: {dest}"))?;

    remote_file
        .write_all(&data)
        .await
        .with_context(|| format!("SFTP write failed for: {dest}"))?;

    remote_file
        .shutdown()
        .await
        .with_context(|| format!("SFTP close failed for: {dest}"))?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Public synchronous functions (unchanged signatures)
// ---------------------------------------------------------------------------

/// Poll until SSH connect+auth succeeds or `timeout` elapses.
pub(crate) fn wait_for_ssh(ssh: &SshOptions, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    let rt = make_rt();
    loop {
        if rt
            .block_on(connect_async(ssh, Duration::from_secs(10)))
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

/// Run `remote_command` over SSH.  Returns `Ok(())` on exit-status 0.
/// Retries on transport errors; fails fast on remote non-zero exit.
pub(crate) fn ssh_with_retry(
    ssh: &SshOptions,
    remote_command: &str,
    retries: usize,
    retry_delay: Duration,
    connect_timeout: Duration,
) -> Result<()> {
    let rt = make_rt();
    let mut attempts = 0usize;
    loop {
        match rt.block_on(exec_simple_async(ssh, remote_command, connect_timeout, false)) {
            AttemptResult::Ok(_) => return Ok(()),
            AttemptResult::NonZero(code) => {
                bail!("ssh command failed (exit status: {code})");
            }
            AttemptResult::Transport(e) => {
                attempts += 1;
                if attempts >= retries {
                    return Err(e.context("ssh command failed"));
                }
                if !retry_delay.is_zero() {
                    std::thread::sleep(retry_delay);
                }
            }
        }
    }
}

/// Upload a local file to `dest` on the remote via SFTP.
/// Retries on transport errors.
pub(crate) fn scp_with_retry(
    ssh: &SshOptions,
    src: &Path,
    dest: &str,
    retries: usize,
    retry_delay: Duration,
) -> Result<()> {
    let rt = make_rt();
    let mut attempts = 0usize;
    loop {
        match rt.block_on(upload_async(ssh, src, dest)) {
            Ok(()) => return Ok(()),
            Err(e) => {
                attempts += 1;
                if attempts >= retries {
                    return Err(e.context("scp command failed"));
                }
                if !retry_delay.is_zero() {
                    std::thread::sleep(retry_delay);
                }
            }
        }
    }
}

/// Run `remote_command` and return captured stdout on success.
/// Same retry semantics as `ssh_with_retry`.
pub(crate) fn ssh_capture_stdout(
    ssh: &SshOptions,
    remote_command: &str,
    retries: usize,
    retry_delay: Duration,
    connect_timeout: Duration,
) -> Result<String> {
    let rt = make_rt();
    let mut attempts = 0usize;
    loop {
        match rt.block_on(exec_simple_async(ssh, remote_command, connect_timeout, true)) {
            AttemptResult::Ok(stdout) => return Ok(stdout),
            AttemptResult::NonZero(code) => {
                bail!("ssh command failed (exit status: {code})");
            }
            AttemptResult::Transport(e) => {
                attempts += 1;
                if attempts >= retries {
                    return Err(e.context("ssh command failed"));
                }
                if !retry_delay.is_zero() {
                    std::thread::sleep(retry_delay);
                }
            }
        }
    }
}

/// Execute `remote_command` with live output streaming and deadline tracking.
///
/// `on_output(is_stderr, chunk)` is called for each data chunk from the
/// remote command (called synchronously from the calling thread).
///
/// This is used by `plan/vm.rs`'s `run_ssh_step_with_step_log` to forward
/// remote output to both the console and the step JSONL log file while
/// honouring per-step and overall run deadlines.
pub(crate) fn ssh_exec_logged(
    ssh: &SshOptions,
    remote_command: &str,
    connect_timeout: Duration,
    step_timeout: Duration,
    overall_deadline: Instant,
    on_output: &mut dyn FnMut(bool, &[u8]),
) -> SshExecOutcome {
    let rt = make_rt();
    rt.block_on(exec_logged_async(
        ssh,
        remote_command,
        connect_timeout,
        step_timeout,
        overall_deadline,
        on_output,
    ))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::{journalctl_command, SshExecOutcome, TemporarySshKeypair};

    // ── journalctl builder (unchanged) ──────────────────────────────────────

    #[test]
    fn journalctl_command_includes_units() {
        let cmd = journalctl_command(&["ssh".to_string(), "botwork-launcher".to_string()]);
        assert_eq!(
            cmd,
            "sudo journalctl -u ssh -u botwork-launcher --no-pager -n 200"
        );
    }

    #[test]
    fn journalctl_command_no_units() {
        let cmd = journalctl_command(&[]);
        assert_eq!(cmd, "sudo journalctl --no-pager -n 200");
    }

    // ── Ephemeral keypair generation ─────────────────────────────────────────

    #[test]
    fn keypair_generate_writes_both_files() {
        let kp = TemporarySshKeypair::generate("botforge-test-kp").unwrap();

        assert!(
            kp.private_key().exists(),
            "private key file must exist: {}",
            kp.private_key().display()
        );
        assert!(
            kp.public_key().exists(),
            "public key file must exist: {}",
            kp.public_key().display()
        );
    }

    #[test]
    fn keypair_public_key_is_openssh_ed25519_format() {
        let kp = TemporarySshKeypair::generate("botforge-test-kp-fmt").unwrap();
        let pub_contents = std::fs::read_to_string(kp.public_key()).unwrap();
        let trimmed = pub_contents.trim();

        // Must start with "ssh-ed25519 " followed by base64 key material.
        assert!(
            trimmed.starts_with("ssh-ed25519 "),
            "public key should start with 'ssh-ed25519 ', got: {trimmed:?}"
        );
        // Must be a single line (authorised_keys format injected into cloud-init).
        assert!(
            !trimmed.contains('\n'),
            "public key should be a single line, got: {trimmed:?}"
        );
    }

    #[test]
    fn keypair_private_key_round_trips_via_russh() {
        use russh::keys::{ssh_key::Algorithm, load_secret_key};

        let kp = TemporarySshKeypair::generate("botforge-test-kp-rt").unwrap();

        let loaded = load_secret_key(kp.private_key(), None)
            .expect("russh must be able to load the generated private key");

        assert_eq!(
            loaded.algorithm(),
            Algorithm::Ed25519,
            "loaded key must be ed25519"
        );
    }

    #[test]
    fn keypair_drop_removes_files() {
        let kp = TemporarySshKeypair::generate("botforge-test-kp-drop").unwrap();
        let priv_path = kp.private_key().to_path_buf();
        let pub_path = kp.public_key().to_path_buf();

        drop(kp);

        assert!(
            !priv_path.exists(),
            "private key must be removed on drop: {}",
            priv_path.display()
        );
        assert!(
            !pub_path.exists(),
            "public key must be removed on drop: {}",
            pub_path.display()
        );
    }

    // ── Retry decision logic ─────────────────────────────────────────────────

    /// Returns true when a transport error should trigger another attempt.
    fn should_retry(outcome: &SshExecOutcome, attempts: usize, max_retries: usize) -> bool {
        matches!(outcome, SshExecOutcome::TransportError(_)) && attempts < max_retries
    }

    #[test]
    fn retry_on_transport_error_within_budget() {
        let err = SshExecOutcome::TransportError(anyhow::anyhow!("connection refused"));
        assert!(
            should_retry(&err, 0, 3),
            "first transport error with retries remaining should retry"
        );
        assert!(
            should_retry(&err, 2, 3),
            "third transport error with one retry remaining should retry"
        );
    }

    #[test]
    fn no_retry_when_budget_exhausted() {
        let err = SshExecOutcome::TransportError(anyhow::anyhow!("connection refused"));
        assert!(
            !should_retry(&err, 3, 3),
            "transport error when attempts == max_retries must not retry"
        );
    }

    #[test]
    fn no_retry_on_remote_failure() {
        let outcome = SshExecOutcome::RemoteFailure(1);
        assert!(
            !should_retry(&outcome, 0, 10),
            "remote non-zero exit must not be retried"
        );
    }

    #[test]
    fn no_retry_on_success() {
        assert!(
            !should_retry(&SshExecOutcome::Success, 0, 10),
            "success must not trigger a retry"
        );
    }
}
