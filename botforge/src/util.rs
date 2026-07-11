use anyhow::{bail, Context, Result};
use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};

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
/// On success returns `Ok(())`.  On failure the captured output is included in the returned
/// error so failures remain debuggable.
///
/// When `BOTFORGE_DEBUG=1` the child process inherits stdout/stderr (identical to
/// [`run_command`]) so the raw tool output is visible for troubleshooting.
///
/// The child's stdin is always redirected from `/dev/null`.  Tools such as `xorriso`
/// enter an interactive dialog mode when their stdout is not a terminal; with an
/// inherited stdin (an open pipe under CI, with no controlling TTY) they can block
/// waiting for input, hanging the build.  Capturing callers here never send input, so
/// nulling stdin is always correct and prevents the hang.
pub(crate) fn run_command_capture(
    program: &str,
    args: &[String],
    envs: &[(&str, &str)],
    failure_context: &str,
) -> Result<()> {
    if botforge_debug_enabled() {
        return run_command(program, args, envs, failure_context);
    }
    let output = Command::new(program)
        .args(args)
        .envs(envs.iter().copied())
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("failed to execute {program}"))?;
    if !output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let mut msg = format!("{failure_context} (exit status: {})", output.status);
        if !stdout.trim().is_empty() {
            msg.push_str("\nstdout:\n");
            msg.push_str(&stdout);
        }
        if !stderr.trim().is_empty() {
            msg.push_str("\nstderr:\n");
            msg.push_str(&stderr);
        }
        bail!("{msg}");
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

pub(crate) fn resolve_under_root(repo_root: &Path, path: PathBuf) -> PathBuf {
    if path.is_absolute() {
        normalize_path(&path)
    } else {
        normalize_path(&repo_root.join(path))
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
    use super::{format_bytes_human, materialize_flat, run_command_capture};
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
    fn run_command_capture_nulls_stdin_and_does_not_block() {
        // A tool that reads stdin (`cat` with no args) must not hang waiting for
        // input: stdin is redirected from /dev/null, so it sees immediate EOF and
        // exits 0. Without the explicit null stdin this would block on an inherited
        // stdin, reproducing the seed-ISO (xorriso) hang seen under CI.
        let result = run_command_capture("cat", &[], &[], "cat failed");
        assert!(
            result.is_ok(),
            "cat with null stdin should exit immediately, got: {result:?}"
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
}
