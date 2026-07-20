use anyhow::{Context, Result};
use serde::Serialize;
use std::fs::File;
use std::io::{BufWriter, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
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
        format!("🤖 \x1b[2m({counter})\x1b[0m \x1b[1m{name}\x1b[0m")
    } else {
        format!("🤖 ({counter}) {name}")
    }
}

fn step_status_marker(
    step_idx: usize,
    name: &str,
    success: bool,
    id: Option<&str>,
    color: bool,
) -> String {
    let counter = match id {
        Some(id) => format!("{step_idx}/{id}"),
        None => format!("{step_idx}"),
    };
    if color {
        if success {
            format!(" \x1b[32m✓\x1b[0m \x1b[2m({counter})\x1b[0m \x1b[2m{name}\x1b[0m")
        } else {
            format!(" \x1b[31m✗\x1b[0m \x1b[2m({counter})\x1b[0m {name}")
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

fn phase_title_line(label: &str, description: &str, color: bool) -> String {
    if color {
        format!("🤖 \x1b[2m({label})\x1b[0m \x1b[1m{description}\x1b[0m")
    } else {
        format!("🤖 ({label}) {description}")
    }
}

/// Print a lifecycle phase title to stderr: `🤖 (<label>) <description>`.
///
/// Uses the same TTY-aware coloring (`stderr_color_enabled`, `NO_COLOR`-aware) as
/// [`print_step_title`].  Use this for named build phases (e.g. `"setup"`,
/// `"compress"`, `"output"`) rather than the numbered per-step titles.
pub(crate) fn print_phase(label: &str, description: &str) {
    eprintln!(
        "{}",
        phase_title_line(label, description, stderr_color_enabled())
    );
}

fn phase_status_marker(label: &str, description: &str, success: bool, color: bool) -> String {
    if color {
        if success {
            format!(" \x1b[32m✓\x1b[0m \x1b[2m({label})\x1b[0m \x1b[2m{description}\x1b[0m")
        } else {
            format!(" \x1b[31m✗\x1b[0m \x1b[2m({label})\x1b[0m {description}")
        }
    } else {
        let tick = if success { '✓' } else { '✗' };
        format!(" {tick} ({label}) {description}")
    }
}

/// Print a lifecycle phase completion status to stderr: ` ✓ (<label>) <description>` or
/// ` ✗ (<label>) <description>`.
///
/// Mirrors [`print_step_status`] but uses a plain string label instead of a step index.
pub(crate) fn print_phase_status(label: &str, description: &str, success: bool) {
    eprintln!(
        "{}",
        phase_status_marker(label, description, success, stderr_color_enabled())
    );
}

pub(super) fn print_step_status(
    step_idx: usize,
    step_name: &str,
    step_id: Option<&str>,
    success: bool,
) {
    eprintln!(
        "{}",
        step_status_marker(
            step_idx,
            step_name,
            success,
            step_id,
            stderr_color_enabled()
        )
    );
}

fn step_skipped_marker(step_idx: usize, name: &str, id: Option<&str>, color: bool) -> String {
    let counter = match id {
        Some(id) => format!("{step_idx}/{id}"),
        None => format!("{step_idx}"),
    };
    if color {
        format!(" ⊘ \x1b[2m({counter})\x1b[0m \x1b[2m{name}\x1b[0m")
    } else {
        format!(" ⊘ ({counter}) {name}")
    }
}

pub(super) fn print_step_skipped(step_idx: usize, step_name: &str, step_id: Option<&str>) {
    eprintln!(
        "{}",
        step_skipped_marker(step_idx, step_name, step_id, stderr_color_enabled())
    );
}

use crate::util::write_all_resilient;

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

/// Like `stream_child_output` but also accumulates all bytes into `cap` for
/// post-execution expectation matching.
fn stream_child_output_capturing<R: Read, W: Write>(
    mut reader: R,
    mut writer: W,
    stream: StepOutputStream,
    logger: &StepLogWriter,
    cap: &Arc<Mutex<Vec<u8>>>,
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
        write_all_resilient(&mut writer, slice)
            .context("failed to forward child process output")?;
        cap.lock()
            .map_err(|_| anyhow::anyhow!("capture buffer mutex poisoned"))?
            .extend_from_slice(slice);
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

/// Like `spawn_output_forwarder` but also captures all output in a shared buffer.
///
/// Returns the join handle and the capture buffer; the buffer is fully populated
/// once the handle has been joined.
pub(super) fn spawn_capturing_forwarder<R: Read + Send + 'static>(
    reader: R,
    stream: StepOutputStream,
    logger: Arc<StepLogWriter>,
) -> (JoinHandle<Result<()>>, Arc<Mutex<Vec<u8>>>) {
    let cap: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
    let cap_clone = Arc::clone(&cap);
    let handle = std::thread::spawn(move || match stream {
        StepOutputStream::Stdout => {
            let stdout = std::io::stdout();
            stream_child_output_capturing(reader, stdout.lock(), stream, &logger, &cap_clone)
        }
        StepOutputStream::Stderr => {
            let stderr = std::io::stderr();
            stream_child_output_capturing(reader, stderr.lock(), stream, &logger, &cap_clone)
        }
    });
    (handle, cap)
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
    use super::{
        phase_status_marker, phase_title_line, step_log_path, step_skipped_marker,
        step_status_marker, step_title_line,
    };
    use crate::util::write_all_resilient;
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
            success_color.contains("\x1b[2m(4)\x1b[0m"),
            "success color should dim marker: {success_color:?}"
        );
        assert!(
            success_color.contains("\x1b[2mmcp-smoke\x1b[0m"),
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
            failure_color.contains("\x1b[2m(4)\x1b[0m"),
            "failure color should dim marker: {failure_color:?}"
        );
        assert!(
            !failure_color.contains("\x1b[2mmcp-smoke\x1b[0m"),
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
            success_color.contains("\x1b[2m(4/build)\x1b[0m"),
            "success color with id should dim counter: {success_color:?}"
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
            failure_color.contains("\x1b[2m(4/build)\x1b[0m"),
            "failure color with id should dim counter: {failure_color:?}"
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
            colored.contains("🤖 \x1b[2m(4)\x1b[0m "),
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
            colored.contains("\x1b[2m(4/build)\x1b[0m"),
            "colored title with id should dim counter: {colored:?}"
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

    #[test]
    fn test_step_skipped_marker_formats() {
        assert_eq!(
            step_skipped_marker(4, "mcp-smoke", None, false),
            " ⊘ (4) mcp-smoke"
        );
        let colored = step_skipped_marker(4, "mcp-smoke", None, true);
        assert!(
            colored.contains("\x1b[2m(4)\x1b[0m"),
            "colored skipped marker should dim counter: {colored:?}"
        );
        assert!(
            colored.contains("\x1b[2mmcp-smoke\x1b[0m"),
            "colored skipped marker should dim name: {colored:?}"
        );
        assert_eq!(
            step_skipped_marker(4, "mcp-smoke", Some("build"), false),
            " ⊘ (4/build) mcp-smoke"
        );
        let colored_with_id = step_skipped_marker(4, "mcp-smoke", Some("build"), true);
        assert!(
            colored_with_id.contains("\x1b[2m(4/build)\x1b[0m"),
            "colored skipped marker with id should dim counter: {colored_with_id:?}"
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

    // --- phase title line ---

    #[test]
    fn test_phase_title_line_no_color() {
        assert_eq!(
            phase_title_line("setup", "Preparing build environment (seed image)", false),
            "🤖 (setup) Preparing build environment (seed image)"
        );
        assert_eq!(
            phase_title_line(
                "output",
                "Final image written to /out.qcow2 (344.0 MiB)",
                false
            ),
            "🤖 (output) Final image written to /out.qcow2 (344.0 MiB)"
        );
    }

    #[test]
    fn test_phase_title_line_color() {
        let colored = phase_title_line(
            "compress",
            "Compressing image (reclaim, sparsify, compression)",
            true,
        );
        assert!(
            colored.contains("🤖 \x1b[2m(compress)\x1b[0m "),
            "colored phase title should contain robot prefix: {colored:?}"
        );
        assert!(
            colored.contains("\x1b[1m"),
            "colored phase title should contain bold: {colored:?}"
        );
        assert!(
            colored.contains("Compressing image (reclaim, sparsify, compression)"),
            "colored phase title should contain description: {colored:?}"
        );
        assert!(
            colored.contains("\x1b[0m"),
            "colored phase title should contain reset: {colored:?}"
        );
    }

    // --- phase status marker ---

    #[test]
    fn test_phase_status_marker_no_color_success() {
        assert_eq!(
            phase_status_marker(
                "setup",
                "Preparing build environment (seed image)",
                true,
                false
            ),
            " ✓ (setup) Preparing build environment (seed image)"
        );
    }

    #[test]
    fn test_phase_status_marker_no_color_failure() {
        assert_eq!(
            phase_status_marker(
                "setup",
                "Preparing build environment (seed image)",
                false,
                false
            ),
            " ✗ (setup) Preparing build environment (seed image)"
        );
    }

    #[test]
    fn test_phase_status_marker_color_success() {
        let s = phase_status_marker("vm", "Stopping vm", true, true);
        assert!(s.starts_with(' '), "should start with space: {s:?}");
        assert!(s.contains("\x1b[32m"), "should contain green: {s:?}");
        assert!(s.contains('✓'), "should contain tick: {s:?}");
        assert!(s.contains("\x1b[2m(vm)\x1b[0m"), "should dim label: {s:?}");
        assert!(
            s.contains("\x1b[2mStopping vm\x1b[0m"),
            "should dim description: {s:?}"
        );
        assert!(s.contains("\x1b[0m"), "should reset: {s:?}");
    }

    #[test]
    fn test_phase_status_marker_color_failure() {
        let s = phase_status_marker("vm", "Stopping vm", false, true);
        assert!(s.starts_with(' '), "should start with space: {s:?}");
        assert!(s.contains("\x1b[31m"), "should contain red: {s:?}");
        assert!(s.contains('✗'), "should contain cross: {s:?}");
        assert!(s.contains("\x1b[2m(vm)\x1b[0m"), "should dim label: {s:?}");
        assert!(
            !s.contains("\x1b[2mStopping vm\x1b[0m"),
            "failure should NOT dim name: {s:?}"
        );
    }
}
