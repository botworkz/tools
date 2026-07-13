//! Declarative `assert:` phase — run after all `steps:` in a `type: test` document.
//!
//! Currently implements the `assert.files:` sub-key only.  Each entry is probed via
//! a single SSH round-trip (one batched shell script per call to [`run_assert_files`]).

use anyhow::Result;
use std::collections::BTreeMap;
use std::time::Duration;

use crate::ssh::{ssh_capture_stdout, SshOptions};
use crate::util::shell_single_quote;

use super::config::{AssertBlock, AssertFile};
use super::log::print_phase_status;

const ASSERT_TRANSPORT_RETRIES: usize = 3;
const ASSERT_TRANSPORT_RETRY_DELAY: Duration = Duration::from_secs(2);
const ASSERT_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

// ---------------------------------------------------------------------------
// Public entry-point
// ---------------------------------------------------------------------------

/// Run the `assert.files:` phase, probing each expected path via a single batched
/// SSH script and comparing the results against the declared expectations.
///
/// Prints a `✓` or `✗` status line per path, then returns an error if any
/// assertion failed.
pub(crate) fn run_assert_files(ssh: &SshOptions, assert_block: &AssertBlock) -> Result<()> {
    if assert_block.files.is_empty() {
        return Ok(());
    }

    // BTreeMap iteration order is sorted by key, so the output lines are
    // deterministically matched to paths by index.
    let paths: Vec<&str> = assert_block.files.keys().map(String::as_str).collect();
    let probe_output = run_file_probe(ssh, &paths)
        .map_err(|e| anyhow::anyhow!("assert.files: SSH probe failed: {e:#}"))?;

    evaluate_probe_results(&assert_block.files, &probe_output)
}

// ---------------------------------------------------------------------------
// Probe
// ---------------------------------------------------------------------------

/// Build and execute the batched SSH probe script.
///
/// The script outputs exactly one line per path in the same order as `paths`.
/// Each line is either `absent` or `present:<filetype>:<owner>:<group>:<mode>`.
fn run_file_probe(ssh: &SshOptions, paths: &[&str]) -> Result<String> {
    let mut script = String::from("set -e\n");
    for path in paths {
        let q = shell_single_quote(path);
        // Use `[ -L ]` first so broken symlinks count as present.
        // `stat -c '%U %G %a'` follows symlinks on Linux (no -L flag needed for owner/group/mode).
        script.push_str(&format!(
            concat!(
                "if [ -L {q} ] || [ -e {q} ]; then\n",
                "  if [ -L {q} ]; then _ft=symlink\n",
                "  elif [ -f {q} ]; then _ft=file\n",
                "  elif [ -d {q} ]; then _ft=directory\n",
                "  else _ft=other\n",
                "  fi\n",
                "  _owner=$(stat -c '%U' {q})\n",
                "  _group=$(stat -c '%G' {q})\n",
                "  _mode=$(stat -c '%a' {q})\n",
                "  printf 'present:%s:%s:%s:%s\\n' \"$_ft\" \"$_owner\" \"$_group\" \"$_mode\"\n",
                "else\n",
                "  printf 'absent\\n'\n",
                "fi\n",
            ),
            q = q
        ));
    }
    ssh_capture_stdout(
        ssh,
        &script,
        ASSERT_TRANSPORT_RETRIES,
        ASSERT_TRANSPORT_RETRY_DELAY,
        ASSERT_CONNECT_TIMEOUT,
    )
}

// ---------------------------------------------------------------------------
// Result evaluation
// ---------------------------------------------------------------------------

/// Compare each probe output line against its expectation and report results.
fn evaluate_probe_results(files: &BTreeMap<String, AssertFile>, probe_output: &str) -> Result<()> {
    let mut lines = probe_output.lines();
    let mut any_failed = false;

    for (path, expectation) in files {
        let raw_line = lines.next().unwrap_or("absent");
        let failures = check_one_path(path, expectation, raw_line);
        let ok = failures.is_empty();
        print_phase_status("assert", path, ok);
        if !ok {
            for msg in &failures {
                eprintln!("         {msg}");
            }
            any_failed = true;
        }
    }

    if any_failed {
        anyhow::bail!("one or more assert.files: checks failed");
    }
    Ok(())
}

/// Check a single path's probe line against its expectation.
/// Returns a list of failure messages (empty = pass).
fn check_one_path(path: &str, expectation: &AssertFile, raw_line: &str) -> Vec<String> {
    let mut failures = Vec::new();

    if raw_line == "absent" {
        if expectation.exists {
            failures.push("exists: expected present, got absent".to_string());
        }
        // If exists: false and path is absent → pass; no attribute checks.
        return failures;
    }

    // Path is present.
    if !expectation.exists {
        failures.push("exists: expected absent, but path exists".to_string());
        return failures;
    }

    // Parse "present:<ft>:<owner>:<group>:<mode>"
    let Some(rest) = raw_line.strip_prefix("present:") else {
        failures.push(format!(
            "assert.files: unexpected probe output for '{path}': {raw_line}"
        ));
        return failures;
    };

    // Split into at most 4 parts (mode may be absent if stat failed).
    let parts: Vec<&str> = rest.splitn(4, ':').collect();
    let (actual_ft, actual_owner, actual_group, actual_mode) = match parts.as_slice() {
        [ft, owner, group, mode] => (*ft, *owner, *group, *mode),
        _ => {
            failures.push(format!(
                "assert.files: malformed probe output for '{path}': {raw_line}"
            ));
            return failures;
        }
    };

    // Normalize mode to 4-digit octal (e.g. "755" → "0755").
    let actual_mode_normalized = normalize_mode(actual_mode);

    if let Some(expected_ft) = expectation.filetype {
        if actual_ft != expected_ft.as_str() {
            failures.push(format!(
                "filetype: expected {}, got {actual_ft}",
                expected_ft.as_str()
            ));
        }
    }
    if let Some(ref expected_owner) = expectation.owner {
        if actual_owner != expected_owner {
            failures.push(format!(
                "owner: expected {expected_owner}, got {actual_owner}"
            ));
        }
    }
    if let Some(ref expected_group) = expectation.group {
        if actual_group != expected_group {
            failures.push(format!(
                "group: expected {expected_group}, got {actual_group}"
            ));
        }
    }
    if let Some(ref expected_mode) = expectation.mode {
        let expected_normalized = normalize_mode(expected_mode);
        if actual_mode_normalized != expected_normalized {
            failures.push(format!(
                "mode: expected {expected_normalized}, got {actual_mode_normalized}"
            ));
        }
    }

    failures
}

/// Normalize a mode string to always be 4 octal digits.
/// "755" → "0755", "0755" → "0755", "644" → "0644".
fn normalize_mode(mode: &str) -> String {
    match mode.len() {
        3 => format!("0{mode}"),
        4 => mode.to_string(),
        _ => mode.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::{check_one_path, normalize_mode};
    use crate::plan::config::{AssertFile, AssertFileType};

    fn file_expectation(
        exists: bool,
        filetype: Option<AssertFileType>,
        owner: Option<&str>,
        group: Option<&str>,
        mode: Option<&str>,
    ) -> AssertFile {
        AssertFile {
            exists,
            filetype,
            owner: owner.map(str::to_string),
            group: group.map(str::to_string),
            mode: mode.map(str::to_string),
        }
    }

    #[test]
    fn test_normalize_mode_pads_3_digits() {
        assert_eq!(normalize_mode("755"), "0755");
        assert_eq!(normalize_mode("644"), "0644");
    }

    #[test]
    fn test_normalize_mode_keeps_4_digits() {
        assert_eq!(normalize_mode("0755"), "0755");
        assert_eq!(normalize_mode("0644"), "0644");
    }

    #[test]
    fn test_check_one_path_exists_true_absent_line_fails() {
        let exp = file_expectation(true, None, None, None, None);
        let failures = check_one_path("/usr/bin/foo", &exp, "absent");
        assert!(
            !failures.is_empty(),
            "should fail when exists:true but absent"
        );
        assert!(failures[0].contains("absent"), "message: {:?}", failures);
    }

    #[test]
    fn test_check_one_path_exists_false_absent_line_passes() {
        let exp = file_expectation(false, None, None, None, None);
        let failures = check_one_path("/some/path", &exp, "absent");
        assert!(failures.is_empty(), "should pass: {:?}", failures);
    }

    #[test]
    fn test_check_one_path_exists_false_present_fails() {
        let exp = file_expectation(false, None, None, None, None);
        let line = "present:file:root:root:755";
        let failures = check_one_path("/some/path", &exp, line);
        assert!(!failures.is_empty());
        assert!(failures[0].contains("expected absent"), "{:?}", failures);
    }

    #[test]
    fn test_check_one_path_correct_attributes_pass() {
        let exp = file_expectation(
            true,
            Some(AssertFileType::File),
            Some("root"),
            Some("root"),
            Some("0755"),
        );
        let line = "present:file:root:root:755";
        let failures = check_one_path("/usr/bin/foo", &exp, line);
        assert!(failures.is_empty(), "should pass: {:?}", failures);
    }

    #[test]
    fn test_check_one_path_wrong_filetype_fails() {
        let exp = file_expectation(true, Some(AssertFileType::File), None, None, None);
        let line = "present:directory:root:root:755";
        let failures = check_one_path("/some/dir", &exp, line);
        assert!(!failures.is_empty());
        assert!(failures[0].contains("filetype"), "{:?}", failures);
    }

    #[test]
    fn test_check_one_path_wrong_owner_fails() {
        let exp = file_expectation(true, None, Some("root"), None, None);
        let line = "present:file:nobody:root:755";
        let failures = check_one_path("/some/file", &exp, line);
        assert!(!failures.is_empty());
        assert!(failures[0].contains("owner"), "{:?}", failures);
    }

    #[test]
    fn test_check_one_path_wrong_mode_fails() {
        let exp = file_expectation(true, None, None, None, Some("0755"));
        let line = "present:file:root:root:644";
        let failures = check_one_path("/some/file", &exp, line);
        assert!(!failures.is_empty());
        assert!(failures[0].contains("mode"), "{:?}", failures);
    }

    #[test]
    fn test_check_one_path_no_attributes_present_passes() {
        // When no attributes are specified (just exists: true), any present file passes.
        let exp = file_expectation(true, None, None, None, None);
        let line = "present:file:someuser:somegroup:644";
        let failures = check_one_path("/some/file", &exp, line);
        assert!(failures.is_empty(), "should pass: {:?}", failures);
    }

    #[test]
    fn test_check_one_path_wrong_group_fails() {
        let exp = file_expectation(true, None, None, Some("root"), None);
        let line = "present:file:root:nobody:755";
        let failures = check_one_path("/some/file", &exp, line);
        assert!(!failures.is_empty());
        assert!(failures[0].contains("group"), "{:?}", failures);
    }

    #[test]
    fn test_check_one_path_symlink_filetype() {
        let exp = file_expectation(true, Some(AssertFileType::Symlink), None, None, None);
        let line = "present:symlink:root:root:777";
        let failures = check_one_path("/some/link", &exp, line);
        assert!(failures.is_empty(), "should pass: {:?}", failures);
    }

    #[test]
    fn test_check_one_path_directory_filetype() {
        let exp = file_expectation(true, Some(AssertFileType::Directory), None, None, None);
        let line = "present:directory:root:root:755";
        let failures = check_one_path("/some/dir", &exp, line);
        assert!(failures.is_empty(), "should pass: {:?}", failures);
    }

    #[test]
    fn test_check_one_path_multiple_failures_reported() {
        let exp = file_expectation(
            true,
            Some(AssertFileType::File),
            Some("root"),
            Some("root"),
            Some("0755"),
        );
        let line = "present:directory:nobody:nobody:644";
        let failures = check_one_path("/some/path", &exp, line);
        assert_eq!(
            failures.len(),
            4,
            "all four attributes should fail: {:?}",
            failures
        );
    }
}
