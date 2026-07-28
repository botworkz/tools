use anyhow::{Context, Result};
use serde::Serialize;
use std::fs::File;
use std::io::{BufWriter, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

/// Process-global flag set by `--color` / [`init_force_color`].  When `true`,
/// ANSI color is always emitted regardless of TTY detection and wins over
/// `NO_COLOR`.
static FORCE_COLOR_FLAG: AtomicBool = AtomicBool::new(false);

/// Set the force-color flag from the parsed `--color` CLI argument.
///
/// Call this once at program startup, before any logging output is produced.
/// The flag is write-once in practice (it is only set during argument parsing)
/// but uses `Relaxed` ordering because color decisions are purely cosmetic and
/// require no memory-ordering guarantees.
pub(crate) fn init_force_color(force: bool) {
    FORCE_COLOR_FLAG.store(force, Ordering::Relaxed);
}

/// Returns `true` when `FORCE_COLOR` or `CLICOLOR_FORCE` is set to a truthy
/// value (`1`, `true`, or `yes`, case-insensitive).
fn is_force_color_env() -> bool {
    for var in &["FORCE_COLOR", "CLICOLOR_FORCE"] {
        if let Ok(val) = std::env::var(var) {
            if matches!(
                val.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes"
            ) {
                return true;
            }
        }
    }
    false
}

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

pub(super) fn step_log_path(log_dir: &Path, step_display: &str, step_name: &str) -> PathBuf {
    // Replace '.' in hierarchical indices (e.g. "3.2") with '-' so the filename
    // stays unambiguous as a path component on all filesystems.
    let safe_display = step_display.replace('.', "-");
    log_dir.join(format!(
        "step-{safe_display}-{}.log",
        sanitize_step_log_name(step_name)
    ))
}

fn stderr_color_enabled() -> bool {
    // Explicit --color flag or FORCE_COLOR/CLICOLOR_FORCE env → always on,
    // even if NO_COLOR is set (explicit user intent wins).
    if FORCE_COLOR_FLAG.load(Ordering::Relaxed) || is_force_color_env() {
        return true;
    }
    // NO_COLOR opt-out (only when no explicit force-color).
    if std::env::var_os("NO_COLOR").is_some() {
        return false;
    }
    // Fall back to TTY detection.
    std::io::stderr().is_terminal()
}

fn step_title_line(step_display: &str, name: &str, id: Option<&str>, color: bool) -> String {
    let counter = match id {
        Some(id) => format!("{step_display}/{id}"),
        None => step_display.to_string(),
    };
    if color {
        format!("🤖 \x1b[2m({counter})\x1b[0m \x1b[1m{name}\x1b[0m")
    } else {
        format!("🤖 ({counter}) {name}")
    }
}

fn step_status_marker(
    step_display: &str,
    name: &str,
    success: bool,
    id: Option<&str>,
    color: bool,
    duration: Option<Duration>,
) -> String {
    let counter = match id {
        Some(id) => format!("{step_display}/{id}"),
        None => step_display.to_string(),
    };
    let duration_suffix = completion_duration_suffix(duration);
    let description = format!("{name}{duration_suffix}");
    if color {
        if success {
            format!(" \x1b[32m✓\x1b[0m \x1b[2m({counter})\x1b[0m \x1b[2m{description}\x1b[0m")
        } else {
            format!(" \x1b[31m✗\x1b[0m \x1b[2m({counter})\x1b[0m {description}")
        }
    } else {
        let tick = if success { '✓' } else { '✗' };
        format!(" {tick} ({counter}) {description}")
    }
}

pub(super) fn print_step_title(step_display: &str, step_name: &str, step_id: Option<&str>) {
    eprintln!(
        "{}",
        step_title_line(step_display, step_name, step_id, stderr_color_enabled())
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

fn phase_status_marker_with_duration(
    label: &str,
    description: &str,
    success: bool,
    color: bool,
    duration: Option<Duration>,
) -> String {
    let duration_suffix = completion_duration_suffix(duration);
    let full_description = format!("{description}{duration_suffix}");
    if color {
        if success {
            format!(" \x1b[32m✓\x1b[0m \x1b[2m({label})\x1b[0m \x1b[2m{full_description}\x1b[0m")
        } else {
            format!(" \x1b[31m✗\x1b[0m \x1b[2m({label})\x1b[0m {full_description}")
        }
    } else {
        let tick = if success { '✓' } else { '✗' };
        format!(" {tick} ({label}) {full_description}")
    }
}

/// Print a lifecycle phase completion status to stderr: ` ✓ (<label>) <description>` or
/// ` ✗ (<label>) <description>`.
///
/// Mirrors [`print_step_status`] but uses a plain string label instead of a step index.
pub(crate) fn print_phase_status(
    label: &str,
    description: &str,
    success: bool,
    duration: Option<Duration>,
) {
    eprintln!(
        "{}",
        phase_status_marker_with_duration(
            label,
            description,
            success,
            stderr_color_enabled(),
            duration
        )
    );
}

pub(super) fn print_step_status(
    step_display: &str,
    step_name: &str,
    step_id: Option<&str>,
    success: bool,
    duration: Option<Duration>,
) {
    eprintln!(
        "{}",
        step_status_marker(
            step_display,
            step_name,
            success,
            step_id,
            stderr_color_enabled(),
            duration
        )
    );
}

fn format_completion_duration(duration: Duration) -> String {
    let total_seconds = duration.as_secs();
    if total_seconds >= 60 {
        let minutes = total_seconds / 60;
        let seconds = total_seconds % 60;
        format!("{minutes}m {seconds:02}s")
    } else {
        format!("{total_seconds}s")
    }
}

fn completion_duration_suffix(duration: Option<Duration>) -> String {
    duration
        .map(|elapsed| format!(" (completed in {})", format_completion_duration(elapsed)))
        .unwrap_or_default()
}

fn final_outcome_line(command: &str, success: bool, color: bool) -> String {
    let summary = if success {
        format!("{command} completed")
    } else {
        format!("{command} failed")
    };
    let emoji = if success { '😎' } else { '😩' };
    if color && success {
        format!("{emoji} \x1b[2m{summary}\x1b[0m")
    } else {
        format!("{emoji} {summary}")
    }
}

pub(crate) fn print_final_outcome(command: &str, success: bool) {
    eprintln!(
        "{}",
        final_outcome_line(command, success, stderr_color_enabled())
    );
}

fn step_skipped_marker(step_display: &str, name: &str, id: Option<&str>, color: bool) -> String {
    let counter = match id {
        Some(id) => format!("{step_display}/{id}"),
        None => step_display.to_string(),
    };
    if color {
        format!(" ⊘ \x1b[2m({counter})\x1b[0m \x1b[2m{name}\x1b[0m")
    } else {
        format!(" ⊘ ({counter}) {name}")
    }
}

pub(super) fn print_step_skipped(step_display: &str, step_name: &str, step_id: Option<&str>) {
    eprintln!(
        "{}",
        step_skipped_marker(step_display, step_name, step_id, stderr_color_enabled())
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
        final_outcome_line, format_completion_duration, phase_status_marker_with_duration,
        phase_title_line, step_log_path, step_skipped_marker, step_status_marker, step_title_line,
    };
    use crate::util::write_all_resilient;
    use std::path::PathBuf;
    use std::time::Duration;

    // --- step log path ---

    #[test]
    fn test_step_log_path_sanitizes_name() {
        let log_dir = PathBuf::from("/tmp/botforge-step-logs");
        let path = step_log_path(&log_dir, "7", "name with/slash\tand*chars");
        assert_eq!(path, log_dir.join("step-7-name_with_slash_and_chars.log"));
    }

    #[test]
    fn test_step_log_path_hierarchical_index() {
        let log_dir = PathBuf::from("/tmp/botforge-step-logs");
        let path = step_log_path(&log_dir, "3.2", "inner-step");
        // Dots in hierarchical index are replaced with '-' in the filename.
        assert_eq!(path, log_dir.join("step-3-2-inner-step.log"));
    }

    // --- step status marker and title line ---

    #[test]
    fn test_step_status_marker_formats_result() {
        assert_eq!(
            step_status_marker("4", "mcp-smoke", false, None, false, None),
            " ✗ (4) mcp-smoke"
        );
        assert_eq!(
            step_status_marker("4", "mcp-smoke", true, None, false, None),
            " ✓ (4) mcp-smoke"
        );
        let success_color = step_status_marker("4", "mcp-smoke", true, None, true, None);
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
        let failure_color = step_status_marker("4", "mcp-smoke", false, None, true, None);
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
            step_status_marker("4", "mcp-smoke", true, Some("build"), false, None),
            " ✓ (4/build) mcp-smoke"
        );
        assert_eq!(
            step_status_marker("4", "mcp-smoke", false, Some("build"), false, None),
            " ✗ (4/build) mcp-smoke"
        );
        let success_color = step_status_marker("4", "mcp-smoke", true, Some("build"), true, None);
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
        let failure_color = step_status_marker("4", "mcp-smoke", false, Some("build"), true, None);
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
            step_title_line("4", "mcp-smoke", None, false),
            "🤖 (4) mcp-smoke"
        );
        let colored = step_title_line("4", "mcp-smoke", None, true);
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
            step_title_line("4", "mcp-smoke", Some("build"), false),
            "🤖 (4/build) mcp-smoke"
        );
        let colored = step_title_line("4", "mcp-smoke", Some("build"), true);
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
            step_skipped_marker("4", "mcp-smoke", None, false),
            " ⊘ (4) mcp-smoke"
        );
        let colored = step_skipped_marker("4", "mcp-smoke", None, true);
        assert!(
            colored.contains("\x1b[2m(4)\x1b[0m"),
            "colored skipped marker should dim counter: {colored:?}"
        );
        assert!(
            colored.contains("\x1b[2mmcp-smoke\x1b[0m"),
            "colored skipped marker should dim name: {colored:?}"
        );
        assert_eq!(
            step_skipped_marker("4", "mcp-smoke", Some("build"), false),
            " ⊘ (4/build) mcp-smoke"
        );
        let colored_with_id = step_skipped_marker("4", "mcp-smoke", Some("build"), true);
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
            phase_status_marker_with_duration(
                "setup",
                "Preparing build environment (seed image)",
                true,
                false,
                None
            ),
            " ✓ (setup) Preparing build environment (seed image)"
        );
    }

    #[test]
    fn test_phase_status_marker_no_color_failure() {
        assert_eq!(
            phase_status_marker_with_duration(
                "setup",
                "Preparing build environment (seed image)",
                false,
                false,
                None
            ),
            " ✗ (setup) Preparing build environment (seed image)"
        );
    }

    #[test]
    fn test_phase_status_marker_color_success() {
        let s = phase_status_marker_with_duration("vm", "Stopping vm", true, true, None);
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
        let s = phase_status_marker_with_duration("vm", "Stopping vm", false, true, None);
        assert!(s.starts_with(' '), "should start with space: {s:?}");
        assert!(s.contains("\x1b[31m"), "should contain red: {s:?}");
        assert!(s.contains('✗'), "should contain cross: {s:?}");
        assert!(s.contains("\x1b[2m(vm)\x1b[0m"), "should dim label: {s:?}");
        assert!(
            !s.contains("\x1b[2mStopping vm\x1b[0m"),
            "failure should NOT dim name: {s:?}"
        );
    }

    #[test]
    fn test_format_completion_duration() {
        assert_eq!(format_completion_duration(Duration::from_millis(250)), "0s");
        assert_eq!(format_completion_duration(Duration::from_secs(9)), "9s");
        assert_eq!(
            format_completion_duration(Duration::from_secs(60)),
            "1m 00s"
        );
        assert_eq!(
            format_completion_duration(Duration::from_secs(125)),
            "2m 05s"
        );
    }

    #[test]
    fn test_step_status_marker_duration_suffix_only_when_present() {
        assert_eq!(
            step_status_marker("7", "warmup", true, None, false, None),
            " ✓ (7) warmup"
        );
        assert_eq!(
            step_status_marker(
                "7",
                "warmup",
                true,
                None,
                false,
                Some(Duration::from_secs(125))
            ),
            " ✓ (7) warmup (completed in 2m 05s)"
        );
    }

    #[test]
    fn test_phase_status_marker_duration_suffix_only_when_present() {
        assert_eq!(
            phase_status_marker_with_duration("vm", "Waiting for SSH", true, false, None),
            " ✓ (vm) Waiting for SSH"
        );
        assert_eq!(
            phase_status_marker_with_duration(
                "vm",
                "Waiting for SSH",
                true,
                false,
                Some(Duration::from_secs(13))
            ),
            " ✓ (vm) Waiting for SSH (completed in 13s)"
        );
    }

    #[test]
    fn test_final_outcome_line() {
        assert_eq!(
            final_outcome_line("build", true, false),
            "😎 build completed"
        );
        assert_eq!(final_outcome_line("test", false, false), "😩 test failed");
        let colored_success = final_outcome_line("build", true, true);
        assert!(
            colored_success.contains("\x1b[2mbuild completed\x1b[0m"),
            "success outcome should dim summary in color mode: {colored_success:?}"
        );
    }

    // --- color-gating precedence ---

    /// Global mutex to serialize tests that mutate environment variables.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Run `f` with the named environment variable set to `val` (or unset when
    /// `val` is `None`), then restore the previous value.  The `ENV_LOCK` mutex
    /// is acquired for the duration so concurrent tests cannot race.
    fn with_env<F: FnOnce()>(vars: &[(&str, Option<&str>)], f: F) {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // Save current values and apply new ones.
        let saved: Vec<(&str, Option<std::ffi::OsString>)> = vars
            .iter()
            .map(|(k, _)| (*k, std::env::var_os(k)))
            .collect();
        for (k, v) in vars {
            match v {
                Some(val) => std::env::set_var(k, val),
                None => std::env::remove_var(k),
            }
        }
        f();
        // Restore.
        for (k, prev) in &saved {
            match prev {
                Some(p) => std::env::set_var(k, p),
                None => std::env::remove_var(k),
            }
        }
    }

    #[test]
    fn color_gating_no_color_disables_when_no_force() {
        use super::{is_force_color_env, stderr_color_enabled, FORCE_COLOR_FLAG};
        use std::sync::atomic::Ordering;

        with_env(
            &[
                ("NO_COLOR", Some("1")),
                ("FORCE_COLOR", None),
                ("CLICOLOR_FORCE", None),
            ],
            || {
                FORCE_COLOR_FLAG.store(false, Ordering::SeqCst);
                assert!(
                    !is_force_color_env(),
                    "is_force_color_env should be false with no force vars"
                );
                // NO_COLOR is set and force is off → color disabled.
                // In the test environment stderr is not a TTY, so the final
                // is_terminal() fallback is also false.
                assert!(
                    !stderr_color_enabled(),
                    "NO_COLOR with no force-color should disable color"
                );
                FORCE_COLOR_FLAG.store(false, Ordering::SeqCst);
            },
        );
    }

    #[test]
    fn color_gating_force_color_flag_wins_over_no_color() {
        use super::{stderr_color_enabled, FORCE_COLOR_FLAG};
        use std::sync::atomic::Ordering;

        // FORCE_COLOR_FLAG is stored INSIDE the env-lock so no concurrent
        // test can race the global.
        with_env(
            &[
                ("NO_COLOR", Some("1")),
                ("FORCE_COLOR", None),
                ("CLICOLOR_FORCE", None),
            ],
            || {
                FORCE_COLOR_FLAG.store(true, Ordering::SeqCst);
                assert!(
                    stderr_color_enabled(),
                    "FORCE_COLOR_FLAG=true should force color even when NO_COLOR is set"
                );
                FORCE_COLOR_FLAG.store(false, Ordering::SeqCst);
            },
        );
    }

    #[test]
    fn color_gating_force_color_env_var_enables_color() {
        use super::{is_force_color_env, stderr_color_enabled, FORCE_COLOR_FLAG};
        use std::sync::atomic::Ordering;

        // FORCE_COLOR=1 => color enabled regardless of NO_COLOR.
        with_env(
            &[
                ("FORCE_COLOR", Some("1")),
                ("CLICOLOR_FORCE", None),
                ("NO_COLOR", Some("1")),
            ],
            || {
                FORCE_COLOR_FLAG.store(false, Ordering::SeqCst);
                assert!(
                    is_force_color_env(),
                    "is_force_color_env should be true with FORCE_COLOR=1"
                );
                assert!(
                    stderr_color_enabled(),
                    "FORCE_COLOR=1 should enable color even when NO_COLOR is set"
                );
                FORCE_COLOR_FLAG.store(false, Ordering::SeqCst);
            },
        );

        // CLICOLOR_FORCE=1 => color enabled regardless of NO_COLOR.
        with_env(
            &[
                ("CLICOLOR_FORCE", Some("1")),
                ("FORCE_COLOR", None),
                ("NO_COLOR", Some("1")),
            ],
            || {
                FORCE_COLOR_FLAG.store(false, Ordering::SeqCst);
                assert!(
                    is_force_color_env(),
                    "is_force_color_env should be true with CLICOLOR_FORCE=1"
                );
                assert!(
                    stderr_color_enabled(),
                    "CLICOLOR_FORCE=1 should enable color even when NO_COLOR is set"
                );
                FORCE_COLOR_FLAG.store(false, Ordering::SeqCst);
            },
        );
    }

    #[test]
    fn color_gating_force_color_env_truthy_values() {
        use super::{is_force_color_env, FORCE_COLOR_FLAG};
        use std::sync::atomic::Ordering;

        for truthy in &["1", "true", "TRUE", "True", "yes", "YES", "Yes"] {
            with_env(
                &[("FORCE_COLOR", Some(truthy)), ("CLICOLOR_FORCE", None)],
                || {
                    FORCE_COLOR_FLAG.store(false, Ordering::SeqCst);
                    assert!(
                        is_force_color_env(),
                        "is_force_color_env should be true for FORCE_COLOR={truthy:?}"
                    );
                    FORCE_COLOR_FLAG.store(false, Ordering::SeqCst);
                },
            );
        }

        for falsy in &["0", "false", "no", "", "off"] {
            with_env(
                &[("FORCE_COLOR", Some(falsy)), ("CLICOLOR_FORCE", None)],
                || {
                    FORCE_COLOR_FLAG.store(false, Ordering::SeqCst);
                    assert!(
                        !is_force_color_env(),
                        "is_force_color_env should be false for FORCE_COLOR={falsy:?}"
                    );
                    FORCE_COLOR_FLAG.store(false, Ordering::SeqCst);
                },
            );
        }
    }

    #[test]
    fn color_gating_default_no_tty_no_force() {
        use super::{stderr_color_enabled, FORCE_COLOR_FLAG};
        use std::sync::atomic::Ordering;

        // In the test environment stderr is not a TTY, so with no force and no
        // NO_COLOR, color is disabled.
        with_env(
            &[
                ("FORCE_COLOR", None),
                ("CLICOLOR_FORCE", None),
                ("NO_COLOR", None),
            ],
            || {
                FORCE_COLOR_FLAG.store(false, Ordering::SeqCst);
                // stderr is not a TTY in the test runner, so color is off.
                assert!(
                    !stderr_color_enabled(),
                    "without a TTY and without force-color, color should be disabled"
                );
                FORCE_COLOR_FLAG.store(false, Ordering::SeqCst);
            },
        );
    }
}
