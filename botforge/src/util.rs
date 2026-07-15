use anyhow::{bail, Context, Result};
use std::ffi::OsString;
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread::JoinHandle;

pub(crate) fn command_exists(program: &str) -> bool {
    Command::new(program)
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok()
}

pub(crate) fn ensure_command(program: &str) -> Result<()> {
    if !command_exists(program) {
        bail!("'{program}' is not available on PATH");
    }
    Ok(())
}

pub(crate) fn run_command(
    program: &str,
    args: &[String],
    envs: &[(&str, &str)],
    failure_context: &str,
) -> Result<()> {
    let status = Command::new(program)
        .args(args)
        .envs(envs.iter().copied())
        .status()
        .with_context(|| format!("failed to execute {program}"))?;
    if !status.success() {
        bail!("{failure_context} (exit status: {status})");
    }
    Ok(())
}

/// Returns `true` when `BOTFORGE_DEBUG` is set to a truthy value (`1`, `true`, or `yes`,
/// case-insensitive).  Mirrors the `BOTFORGE_SSH_VERBOSE` convention in `ssh.rs`.
pub(crate) fn botforge_debug_enabled() -> bool {
    std::env::var("BOTFORGE_DEBUG")
        .map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
        .unwrap_or(false)
}

/// Run an external command, capturing its stdout+stderr so they do not leak to the console.
///
/// On success returns `Ok(())` silently.  On failure the captured output is included in the
/// returned error so failures remain debuggable.
///
/// When `BOTFORGE_DEBUG=1` the child's stdout and stderr are forwarded **live** to the
/// console as they arrive (teed), in addition to being captured — matching how the step
/// runner surfaces debug output.
///
/// # Why `Command::output()` must NOT be used here
///
/// `output()` creates OS pipes and blocks until every copy of the write-end fd is closed
/// (i.e. until the read side sees EOF).  A tool such as `xorriso` may finish and exit
/// while a forked background process still holds the pipe write-end open, so `output()`
/// blocks indefinitely.  This is the classic pipe-drain deadlock.
///
/// # The fix: drain-on-threads
///
/// Dedicated OS threads actively drain stdout and stderr as bytes arrive.  The pipe
/// buffer can never fill and block the child, and `child.wait()` returns as soon as
/// the **foreground child** exits regardless of any grandchildren that may still hold
/// fds.  On a successful exit we detach the drain threads (they finish naturally when
/// grandchildren eventually close their inherited fds) so we never block waiting for
/// them.  On a failing exit we join the threads to collect the captured output for the
/// error message — in the common case (tools that do not fork long-lived background
/// processes) the threads have already reached EOF by the time the child has exited.
/// Return `path` rendered relative to `context` when possible, or just the
/// final file-name component otherwise.  An absolute path is **never** returned.
pub(crate) fn context_relative_display(context: &Path, path: &Path) -> String {
    if let Ok(rel) = path.strip_prefix(context) {
        return rel.display().to_string();
    }
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

pub(crate) fn run_command_capture(
    program: &str,
    args: &[String],
    envs: &[(&str, &str)],
    failure_context: &str,
) -> Result<()> {
    // When debug is enabled, inherit stdio so raw tool output appears live on the
    // console (matching the pre-thread-drain behaviour).
    if botforge_debug_enabled() {
        return run_command(program, args, envs, failure_context);
    }

    // Non-debug path: capture stdout/stderr via drain threads so we never
    // block on a pipe buffer (no grandchild deadlock) and have output on failure.
    let mut child = Command::new(program)
        .args(args)
        .envs(envs.iter().copied())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("failed to execute {program}"))?;

    let stdout_pipe = child
        .stdout
        .take()
        .context("failed to capture child stdout")?;
    let stderr_pipe = child
        .stderr
        .take()
        .context("failed to capture child stderr")?;

    let stdout_thread = spawn_drain_forwarder(stdout_pipe, false, false);
    let stderr_thread = spawn_drain_forwarder(stderr_pipe, false, true);

    let status = child
        .wait()
        .with_context(|| format!("failed to wait for {program}"))?;

    if status.success() {
        // Detach the drain threads: we don't need their captured output.
        // Dropping a JoinHandle detaches the thread; it will drain whatever
        // remains (including data written by any grandchildren that inherited
        // the pipe write-end) and exit naturally — we must not join here or
        // we risk blocking indefinitely waiting for grandchildren.
        drop(stdout_thread);
        drop(stderr_thread);
        return Ok(());
    }

    // On failure: join to collect captured output for the error message.
    // In the common case the tool exited without long-lived grandchildren, so
    // the threads have already seen EOF and return promptly.
    let stdout_captured = stdout_thread
        .join()
        .map_err(|_| anyhow::anyhow!("stdout drain thread panicked"))??;
    let stderr_captured = stderr_thread
        .join()
        .map_err(|_| anyhow::anyhow!("stderr drain thread panicked"))??;

    let mut combined = stdout_captured;
    if !stderr_captured.is_empty() {
        if !combined.is_empty() {
            combined.push('\n');
        }
        combined.push_str(&stderr_captured);
    }

    let mut msg = format!("{failure_context} (exit status: {status})");
    let trimmed = combined.trim();
    if !trimmed.is_empty() {
        msg.push_str("\noutput:\n");
        msg.push_str(trimmed);
    }
    bail!("{msg}");
}

/// Spawn a thread that actively drains `reader` to EOF, collecting all bytes into a
/// `String`.  When `forward_to_console` is true the bytes are also written live to the
/// process's own stdout (`is_stderr = false`) or stderr (`is_stderr = true`) using the
/// resilient writer so non-blocking fds (e.g. under a PTY or container runtime) are
/// handled gracefully.
fn spawn_drain_forwarder<R: Read + Send + 'static>(
    mut reader: R,
    forward_to_console: bool,
    is_stderr: bool,
) -> JoinHandle<Result<String>> {
    std::thread::spawn(move || {
        let mut chunk = [0u8; 8192];
        let mut captured: Vec<u8> = Vec::new();
        loop {
            let n = reader
                .read(&mut chunk)
                .context("failed to read child process output")?;
            if n == 0 {
                break;
            }
            let slice = &chunk[..n];
            captured.extend_from_slice(slice);
            if forward_to_console {
                if is_stderr {
                    write_all_resilient(&mut std::io::stderr().lock(), slice)
                        .context("failed to forward child stderr to console")?;
                } else {
                    write_all_resilient(&mut std::io::stdout().lock(), slice)
                        .context("failed to forward child stdout to console")?;
                }
            }
        }
        Ok(String::from_utf8_lossy(&captured).into_owned())
    })
}

/// Write all bytes of `buf` to `writer`, retrying on non-blocking back-pressure.
///
/// Unlike the stdlib `write_all`, this handles:
/// - Partial writes (advances past the written bytes and continues).
/// - `WouldBlock` / `EAGAIN` (fd not ready): sleeps a short backoff and retries
///   the **same remaining bytes** — no data is dropped or reordered.
/// - `Interrupted` / `EINTR`: retries immediately.
/// - Any other error: returned to the caller.
///
/// This is used for the live-console tee path, where botforge's own inherited
/// stdout/stderr fd may be in non-blocking mode (common under PTYs, process
/// supervisors, and some container runtimes).
pub(crate) fn write_all_resilient<W: Write>(writer: &mut W, mut buf: &[u8]) -> std::io::Result<()> {
    use std::io::ErrorKind;
    let mut backoff = std::time::Duration::from_millis(1);
    while !buf.is_empty() {
        match writer.write(buf) {
            Ok(0) => {
                return Err(std::io::Error::new(
                    ErrorKind::WriteZero,
                    "write returned zero bytes",
                ));
            }
            Ok(n) => {
                buf = &buf[n..];
                // Reset backoff after making forward progress.
                backoff = std::time::Duration::from_millis(1);
            }
            Err(e) if e.kind() == ErrorKind::Interrupted => {
                // EINTR: retry immediately with the same slice.
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock => {
                // EAGAIN: fd not ready. Sleep briefly and retry the same bytes.
                std::thread::sleep(backoff);
                backoff = (backoff * 2).min(std::time::Duration::from_millis(10));
            }
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Format a byte count as a human-readable string using binary prefixes (KiB, MiB, GiB).
///
/// Uses one decimal place for all prefixed forms and no prefix for values below 1 KiB.
pub(crate) fn format_bytes_human(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * KIB;
    const GIB: u64 = 1024 * MIB;
    if bytes >= GIB {
        format!("{:.1} GiB", bytes as f64 / GIB as f64)
    } else if bytes >= MIB {
        format!("{:.1} MiB", bytes as f64 / MIB as f64)
    } else if bytes >= KIB {
        format!("{:.1} KiB", bytes as f64 / KIB as f64)
    } else {
        format!("{bytes} B")
    }
}

pub(crate) fn resolve_under_root(context: &Path, path: PathBuf) -> PathBuf {
    if path.is_absolute() {
        normalize_path(&path)
    } else {
        normalize_path(&context.join(path))
    }
}

pub(crate) fn normalize_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                let _ = normalized.pop();
            }
            Component::Normal(part) => normalized.push(part),
        }
    }
    normalized
}

pub(crate) fn resolve_cache_dir(
    shasset_cache: Option<OsString>,
    xdg_cache_home: Option<OsString>,
    home: Option<OsString>,
) -> PathBuf {
    if let Some(dir) = shasset_cache.filter(|s| !s.is_empty()) {
        return PathBuf::from(dir);
    }
    if let Some(xdg) = xdg_cache_home.filter(|s| !s.is_empty()) {
        return PathBuf::from(xdg).join("shasset");
    }
    if let Some(home) = home.filter(|s| !s.is_empty()) {
        return PathBuf::from(home).join(".cache").join("shasset");
    }
    PathBuf::from(".cache").join("shasset")
}

pub(crate) fn default_cache_dir() -> PathBuf {
    resolve_cache_dir(
        std::env::var_os("SHASSET_CACHE"),
        std::env::var_os("XDG_CACHE_HOME"),
        std::env::var_os("HOME"),
    )
}

pub(crate) fn materialize_flat(
    blob_path: &Path,
    out_dir: &Path,
    filename: &str,
    executable: bool,
) -> Result<PathBuf> {
    validate_flat_filename(filename)?;

    std::fs::create_dir_all(out_dir)
        .with_context(|| format!("cannot create output dir: {}", out_dir.display()))?;
    let out_path = out_dir.join(filename);
    let tmp_out = out_dir.join(format!(
        ".{}-{}.tmp",
        filename,
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));

    std::fs::copy(blob_path, &tmp_out).with_context(|| {
        format!(
            "cannot materialize cached blob from {} to {}",
            blob_path.display(),
            tmp_out.display()
        )
    })?;

    #[cfg(unix)]
    if executable {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&tmp_out)
            .with_context(|| format!("cannot stat temp output: {}", tmp_out.display()))?
            .permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&tmp_out, perms)
            .with_context(|| format!("cannot set executable mode on {}", tmp_out.display()))?;
    }

    #[cfg(not(unix))]
    let _ = executable;

    if out_path.exists() {
        std::fs::remove_file(&out_path)
            .with_context(|| format!("cannot replace output file: {}", out_path.display()))?;
    }

    std::fs::rename(&tmp_out, &out_path).with_context(|| {
        format!(
            "cannot atomically materialize output from {} to {}",
            tmp_out.display(),
            out_path.display()
        )
    })?;

    Ok(out_path)
}

pub(crate) fn validate_flat_filename(filename: &str) -> Result<()> {
    let file_path = Path::new(filename);
    let components: Vec<Component<'_>> = file_path.components().collect();
    if components.len() != 1 || !matches!(components[0], Component::Normal(_)) {
        bail!("asset filename must be a flat filename, got: {filename}");
    }
    Ok(())
}

pub(crate) fn create_temp_dir(prefix: &str) -> Result<PathBuf> {
    let base = std::env::temp_dir();
    let path = base.join(format!("{prefix}-{}", unique_suffix()));
    std::fs::create_dir_all(&path)
        .with_context(|| format!("cannot create temp dir: {}", path.display()))?;
    Ok(path)
}

pub(crate) fn unique_suffix() -> String {
    format!(
        "{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    )
}

/// Wrap `value` in POSIX single-quotes, escaping any embedded single-quotes so that the
/// result is safe to embed in a shell command string passed to `sh -c` or `ssh`.
///
/// ```text
/// shell_single_quote("it's fine")  →  'it'"'"'s fine'
/// ```
pub(crate) fn shell_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

#[cfg(test)]
mod tests {
    use super::{
        context_relative_display, format_bytes_human, materialize_flat, run_command_capture,
    };
    use std::path::Path;
    use tempfile::TempDir;

    #[test]
    fn materialize_flat_writes_expected_path() {
        let tmp = TempDir::new().unwrap();
        let blob = tmp.path().join("blob");
        let out = tmp.path().join("out");
        std::fs::write(&blob, b"hello").unwrap();

        let path = materialize_flat(&blob, &out, "tool.bin", false).unwrap();
        assert_eq!(path, out.join("tool.bin"));
        assert_eq!(std::fs::read(&path).unwrap(), b"hello");
        assert!(Path::new(&out.join("tool.bin")).is_file());
    }

    #[test]
    fn materialize_flat_replaces_existing_file() {
        let tmp = TempDir::new().unwrap();
        let blob = tmp.path().join("blob");
        let out = tmp.path().join("out");
        std::fs::create_dir_all(&out).unwrap();
        std::fs::write(&blob, b"new-bytes").unwrap();
        std::fs::write(out.join("asset"), b"old-bytes").unwrap();

        materialize_flat(&blob, &out, "asset", false).unwrap();
        assert_eq!(std::fs::read(out.join("asset")).unwrap(), b"new-bytes");
    }

    #[test]
    fn materialize_flat_rejects_non_flat_name() {
        let tmp = TempDir::new().unwrap();
        let blob = tmp.path().join("blob");
        let out = tmp.path().join("out");
        std::fs::write(&blob, b"hello").unwrap();

        assert!(materialize_flat(&blob, &out, "nested/asset", false).is_err());
        assert!(materialize_flat(&blob, &out, "../asset", false).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn materialize_flat_sets_executable_mode() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new().unwrap();
        let blob = tmp.path().join("blob");
        let out = tmp.path().join("out");
        std::fs::write(&blob, b"hello").unwrap();

        let path = materialize_flat(&blob, &out, "tool", true).unwrap();
        let mode = std::fs::metadata(path).unwrap().permissions().mode();
        assert_eq!(mode & 0o111, 0o111);
    }

    // --- run_command_capture ---

    #[test]
    fn run_command_capture_succeeds_silently() {
        // `true` exits 0; its output (none) should not leak.
        let result = run_command_capture("true", &[], &[], "true failed");
        assert!(result.is_ok(), "expected Ok, got: {result:?}");
    }

    #[test]
    fn run_command_capture_error_includes_output() {
        // `sh -c 'echo hello; exit 1'` exits non-zero with stdout output.
        let args = vec!["-c".to_string(), "echo captured-stdout; exit 1".to_string()];
        let err = run_command_capture("sh", &args, &[], "sh failed").unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("captured-stdout"),
            "error should include captured stdout: {msg}"
        );
        assert!(
            msg.contains("sh failed"),
            "error should include failure context: {msg}"
        );
    }

    #[test]
    fn run_command_capture_error_includes_stderr() {
        let args = vec![
            "-c".to_string(),
            "echo captured-stderr >&2; exit 1".to_string(),
        ];
        let err = run_command_capture("sh", &args, &[], "sh failed").unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("captured-stderr"),
            "error should include captured stderr: {msg}"
        );
    }

    #[test]
    fn run_command_capture_nulls_stdin() {
        // stdin is always redirected from /dev/null so tools that read stdin (e.g. cat
        // with no args) see immediate EOF and exit cleanly without blocking.
        let result = run_command_capture("cat", &[], &[], "cat failed");
        assert!(
            result.is_ok(),
            "cat with null stdin should exit immediately, got: {result:?}"
        );
    }

    #[test]
    fn run_command_capture_does_not_hang_when_grandchild_holds_fd() {
        // Regression guard for the pipe-drain deadlock that caused `botforge build`
        // to hang after xorriso printed "completed successfully".
        //
        // Command::output() creates OS pipes and blocks until every copy of the
        // write-end fd is closed.  A tool like xorriso may finish and exit while a
        // forked background process still holds the write-end open, so output() blocks
        // indefinitely.
        //
        // With the drain-threads approach: child.wait() returns as soon as the
        // foreground child exits.  On success we detach the drain threads (drop the
        // JoinHandles) so we never block waiting for grandchildren to close their
        // inherited pipe fds — they finish on their own in the background.
        //
        // The command below forks a background process (sleep 30) that holds the pipe
        // write-end open, then the foreground sh exits immediately with status 0.
        // The function must return well under the 30 s sleep duration.
        use std::time::Instant;
        let start = Instant::now();
        let args = vec!["-c".to_string(), "{ sleep 30 & }; echo done".to_string()];
        let result = run_command_capture("sh", &args, &[], "sh failed");
        let elapsed = start.elapsed();
        assert!(result.is_ok(), "expected Ok, got: {result:?}");
        assert!(
            elapsed.as_secs() < 5,
            "run_command_capture blocked for {elapsed:?} — expected to return promptly \
             (pipe-drain deadlock regression)"
        );
    }

    // --- format_bytes_human ---

    #[test]
    fn format_bytes_human_below_kib() {
        assert_eq!(format_bytes_human(0), "0 B");
        assert_eq!(format_bytes_human(512), "512 B");
        assert_eq!(format_bytes_human(1023), "1023 B");
    }

    #[test]
    fn format_bytes_human_kib() {
        assert_eq!(format_bytes_human(1024), "1.0 KiB");
        assert_eq!(format_bytes_human(1536), "1.5 KiB");
    }

    #[test]
    fn format_bytes_human_mib() {
        assert_eq!(format_bytes_human(1024 * 1024), "1.0 MiB");
        assert_eq!(format_bytes_human(361 * 1024 * 1024), "361.0 MiB");
    }

    #[test]
    fn format_bytes_human_gib() {
        assert_eq!(format_bytes_human(1024 * 1024 * 1024), "1.0 GiB");
        assert_eq!(format_bytes_human(12 * 1024 * 1024 * 1024), "12.0 GiB");
    }

    // --- context_relative_display ---

    #[test]
    fn repo_relative_display_under_root_returns_relative() {
        let root = Path::new("/repo/root");
        let path = Path::new("/repo/root/images/build.yaml");
        assert_eq!(context_relative_display(root, path), "images/build.yaml");
    }

    #[test]
    fn repo_relative_display_outside_root_returns_filename() {
        let root = Path::new("/repo/root");
        let path = Path::new("/other/dir/base.qcow2");
        assert_eq!(context_relative_display(root, path), "base.qcow2");
    }

    #[test]
    fn repo_relative_display_root_itself_returns_empty_string() {
        let root = Path::new("/repo/root");
        // A path equal to the root strips to "" (the empty relative path).
        let result = context_relative_display(root, root);
        assert_eq!(result, "");
    }
}
