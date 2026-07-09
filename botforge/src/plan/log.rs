use anyhow::{Context, Result};
use serde::Serialize;
use std::fs::File;
use std::io::{BufWriter, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

#[derive(Clone, Copy)]
pub(super) enum StepOutputStream {
    Stdout,
    Stderr,
}

impl StepOutputStream {
    fn as_str(self) -> &'static str {
        match self {
            Self::Stdout => "stdout",
            Self::Stderr => "stderr",
        }
    }
}

#[derive(Serialize)]
struct StepLogRecord<'a> {
    ts: String,
    stream: &'a str,
    line: String,
}

pub(super) struct StepLogWriter {
    inner: Mutex<BufWriter<File>>,
}

impl StepLogWriter {
    pub(super) fn create(path: &Path) -> Result<Self> {
        let file = File::create(path)
            .with_context(|| format!("failed to create step log file: {}", path.display()))?;
        Ok(Self {
            inner: Mutex::new(BufWriter::new(file)),
        })
    }

    pub(super) fn log_line(&self, stream: StepOutputStream, line: &[u8]) -> Result<()> {
        // Compute the timestamp and lossy-convert the line *before* acquiring
        // the lock so formatting latency is not serialised across both threads.
        let ts = step_log_timestamp()?;
        let line_str = String::from_utf8_lossy(line).into_owned();
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| anyhow::anyhow!("step log writer mutex poisoned"))?;
        serde_json::to_writer(
            &mut *inner,
            &StepLogRecord {
                ts,
                stream: stream.as_str(),
                line: line_str,
            },
        )
        .context("failed to serialize step log record")?;
        inner
            .write_all(b"\n")
            .context("failed to write step log newline")?;
        // Flush after each line for crash-safety: if the step is killed, all
        // prior lines are still visible in the JSONL file.
        inner.flush().context("failed to flush step log")?;
        Ok(())
    }
}

fn step_log_timestamp() -> Result<String> {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .context("failed to format step log timestamp")
}

fn sanitize_step_log_name(name: &str) -> String {
    name.chars()
        .map(|c| match c {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '.' | '_' | '-' => c,
            _ => '_',
        })
        .collect()
}

pub(super) fn step_log_path(log_dir: &Path, step_idx: usize, step_name: &str) -> PathBuf {
    log_dir.join(format!(
        "step-{step_idx}-{}.log",
        sanitize_step_log_name(step_name)
    ))
}

fn stderr_color_enabled() -> bool {
    std::io::stderr().is_terminal() && std::env::var_os("NO_COLOR").is_none()
}

fn step_title_line(step_idx: usize, name: &str, id: Option<&str>, color: bool) -> String {
    let counter = match id {
        Some(id) => format!("{step_idx}/{id}"),
        None => format!("{step_idx}"),
    };
    if color {
        format!("🤖 ({counter}) \x1b[1m{name}\x1b[0m")
    } else {
        format!("🤖 ({counter}) {name}")
    }
}

fn step_status_marker(step_idx: usize, name: &str, success: bool, id: Option<&str>, color: bool) -> String {
    let counter = match id {
        Some(id) => format!("{step_idx}/{id}"),
        None => format!("{step_idx}"),
    };
    if color {
        if success {
            format!(" \x1b[32m✓\x1b[0m ({counter}) \x1b[2m{name}\x1b[0m")
        } else {
            format!(" \x1b[31m✗\x1b[0m ({counter}) {name}")
        }
    } else {
        let tick = if success { '✓' } else { '✗' };
        format!(" {tick} ({counter}) {name}")
    }
}

pub(super) fn print_step_title(step_idx: usize, step_name: &str, step_id: Option<&str>) {
    eprintln!(
        "{}",
        step_title_line(step_idx, step_name, step_id, stderr_color_enabled())
    );
}

pub(super) fn print_step_status(step_idx: usize, step_name: &str, step_id: Option<&str>, success: bool) {
    eprintln!(
        "{}",
        step_status_marker(step_idx, step_name, success, step_id, stderr_color_enabled())
    );
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
fn write_all_resilient<W: Write>(writer: &mut W, mut buf: &[u8]) -> std::io::Result<()> {
    use std::io::ErrorKind;
    let mut backoff = Duration::from_millis(1);
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
                backoff = Duration::from_millis(1);
            }
            Err(e) if e.kind() == ErrorKind::Interrupted => {
                // EINTR: retry immediately with the same slice.
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock => {
                // EAGAIN: fd not ready. Sleep briefly and retry the same bytes.
                std::thread::sleep(backoff);
                backoff = (backoff * 2).min(Duration::from_millis(10));
            }
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

fn stream_child_output<R: Read, W: Write>(
    mut reader: R,
    mut writer: W,
    stream: StepOutputStream,
    logger: &StepLogWriter,
) -> Result<()> {
    let mut chunk = [0u8; 8192];
    let mut pending = Vec::new();
    loop {
        let bytes_read = reader
            .read(&mut chunk)
            .context("failed to read child process output")?;
        if bytes_read == 0 {
            break;
        }
        let slice = &chunk[..bytes_read];
        // Use the resilient writer so a non-blocking console fd (EAGAIN / os
        // error 11) causes a brief back-off retry instead of a fatal error.
        write_all_resilient(&mut writer, slice)
            .context("failed to forward child process output")?;
        // No per-chunk flush on the console side: the terminal/pipe receives
        // bytes as written; flushing every chunk adds syscall pressure without
        // correctness benefit. The JSONL log is flushed per-line below.
        pending.extend_from_slice(slice);
        while let Some(pos) = pending.iter().position(|&b| b == b'\n') {
            let mut line = pending.drain(..=pos).collect::<Vec<_>>();
            line.pop();
            logger.log_line(stream, &line)?;
        }
    }
    if !pending.is_empty() {
        logger.log_line(stream, &pending)?;
    }
    Ok(())
}

pub(super) fn spawn_output_forwarder<R: Read + Send + 'static>(
    reader: R,
    stream: StepOutputStream,
    logger: Arc<StepLogWriter>,
) -> JoinHandle<Result<()>> {
    std::thread::spawn(move || match stream {
        StepOutputStream::Stdout => {
            let stdout = std::io::stdout();
            stream_child_output(reader, stdout.lock(), stream, &logger)
        }
        StepOutputStream::Stderr => {
            let stderr = std::io::stderr();
            stream_child_output(reader, stderr.lock(), stream, &logger)
        }
    })
}

pub(super) fn join_output_forwarders(handles: Vec<JoinHandle<Result<()>>>) -> Result<()> {
    for handle in handles {
        handle
            .join()
            .map_err(|_| anyhow::anyhow!("step output forwarder panicked"))??;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{step_log_path, step_status_marker, step_title_line, write_all_resilient};
    use std::path::PathBuf;
    use std::time::Duration;

    // --- step log path ---

    #[test]
    fn test_step_log_path_sanitizes_name() {
        let log_dir = PathBuf::from("/tmp/botforge-step-logs");
        let path = step_log_path(&log_dir, 7, "name with/slash\tand*chars");
        assert_eq!(path, log_dir.join("step-7-name_with_slash_and_chars.log"));
    }

    // --- step status marker and title line ---

    #[test]
    fn test_step_status_marker_formats_result() {
        assert_eq!(
            step_status_marker(4, "mcp-smoke", false, None, false),
            " ✗ (4) mcp-smoke"
        );
        assert_eq!(
            step_status_marker(4, "mcp-smoke", true, None, false),
            " ✓ (4) mcp-smoke"
        );
        let success_color = step_status_marker(4, "mcp-smoke", true, None, true);
        assert!(
            success_color.starts_with(' '),
            "success color marker should start with a space: {success_color:?}"
        );
        assert!(
            success_color.contains("\x1b[32m"),
            "success color should contain green: {success_color:?}"
        );
        assert!(
            success_color.contains('✓'),
            "success color should contain tick: {success_color:?}"
        );
        assert!(
            success_color.contains("\x1b[2m"),
            "success color should dim name: {success_color:?}"
        );
        assert!(
            success_color.contains("\x1b[0m"),
            "success color should reset: {success_color:?}"
        );
        let failure_color = step_status_marker(4, "mcp-smoke", false, None, true);
        assert!(
            failure_color.starts_with(' '),
            "failure color marker should start with a space: {failure_color:?}"
        );
        assert!(
            failure_color.contains("\x1b[31m"),
            "failure color should contain red: {failure_color:?}"
        );
        assert!(
            failure_color.contains('✗'),
            "failure color should contain cross: {failure_color:?}"
        );
        assert!(
            !failure_color.contains("\x1b[2m"),
            "failure color should NOT dim name: {failure_color:?}"
        );
    }

    #[test]
    fn test_step_status_marker_with_id() {
        assert_eq!(
            step_status_marker(4, "mcp-smoke", true, Some("build"), false),
            " ✓ (4/build) mcp-smoke"
        );
        assert_eq!(
            step_status_marker(4, "mcp-smoke", false, Some("build"), false),
            " ✗ (4/build) mcp-smoke"
        );
        let success_color = step_status_marker(4, "mcp-smoke", true, Some("build"), true);
        assert!(
            success_color.contains("(4/build)"),
            "success color with id should contain counter: {success_color:?}"
        );
        assert!(
            success_color.contains("\x1b[32m"),
            "success color with id should contain green: {success_color:?}"
        );
        assert!(
            success_color.contains('✓'),
            "success color with id should contain tick: {success_color:?}"
        );
        assert!(
            success_color.contains("\x1b[0m"),
            "success color with id should contain reset: {success_color:?}"
        );
        let failure_color = step_status_marker(4, "mcp-smoke", false, Some("build"), true);
        assert!(
            failure_color.contains("(4/build)"),
            "failure color with id should contain counter: {failure_color:?}"
        );
        assert!(
            failure_color.contains("\x1b[31m"),
            "failure color with id should contain red: {failure_color:?}"
        );
        assert!(
            failure_color.contains('✗'),
            "failure color with id should contain cross: {failure_color:?}"
        );
    }

    #[test]
    fn test_step_title_line_formats() {
        assert_eq!(
            step_title_line(4, "mcp-smoke", None, false),
            "🤖 (4) mcp-smoke"
        );
        let colored = step_title_line(4, "mcp-smoke", None, true);
        assert!(
            colored.contains("🤖 (4) "),
            "colored title should contain robot prefix: {colored:?}"
        );
        assert!(
            colored.contains("\x1b[1m"),
            "colored title should contain bold code: {colored:?}"
        );
        assert!(
            colored.contains("mcp-smoke"),
            "colored title should contain name: {colored:?}"
        );
        assert!(
            colored.contains("\x1b[0m"),
            "colored title should contain reset: {colored:?}"
        );
    }

    #[test]
    fn test_step_title_line_with_id() {
        assert_eq!(
            step_title_line(4, "mcp-smoke", Some("build"), false),
            "🤖 (4/build) mcp-smoke"
        );
        let colored = step_title_line(4, "mcp-smoke", Some("build"), true);
        assert!(
            colored.contains("(4/build)"),
            "colored title with id should contain counter: {colored:?}"
        );
        assert!(
            colored.contains("\x1b[1m"),
            "colored title with id should contain bold code: {colored:?}"
        );
        assert!(
            colored.contains("mcp-smoke"),
            "colored title with id should contain name: {colored:?}"
        );
        assert!(
            colored.contains("\x1b[0m"),
            "colored title with id should contain reset: {colored:?}"
        );
    }

    /// Verify that write_all_resilient successfully forwards all bytes to a
    /// non-blocking Unix-domain socket, handling EAGAIN (WouldBlock) without
    /// dropping or reordering data.
    #[cfg(unix)]
    #[test]
    fn test_write_all_resilient_nonblocking_fd() {
        use std::io::Read;
        use std::os::unix::net::UnixStream;

        let (mut writer_end, mut reader_end) = UnixStream::pair().unwrap();
        // Make the write end non-blocking so writes may return WouldBlock.
        writer_end.set_nonblocking(true).unwrap();

        // 512 KiB — well above the socket buffer (typically 128–212 KiB).
        let data: Vec<u8> = (0u8..=255u8).cycle().take(512 * 1024).collect();
        let data_for_check = data.clone();

        // Drain slowly in a background thread to force repeated WouldBlock.
        let reader_handle = std::thread::spawn(move || {
            let mut received = Vec::new();
            let mut buf = [0u8; 4096];
            loop {
                match reader_end.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => received.extend_from_slice(&buf[..n]),
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    Err(e) => panic!("reader error: {e}"),
                }
            }
            received
        });

        // Must not error; must not drop or reorder bytes.
        write_all_resilient(&mut writer_end, &data_for_check).unwrap();
        // Drop the write end so the reader sees EOF and terminates.
        drop(writer_end);

        let received = reader_handle.join().unwrap();
        assert_eq!(
            received, data_for_check,
            "write_all_resilient must forward all bytes without loss or reordering"
        );
    }
}
