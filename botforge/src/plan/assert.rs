//! Declarative `assert:` phase — run after all `steps:` in a `type: test` document.
//!
//! Currently implements the `assert.files:`, `assert.users:`, and `assert.groups:`
//! sub-keys.  File entries are probed via a single batched SSH script; user and
//! group entries are probed via `getent passwd` / `getent group` (for names and
//! attributes) and `id -nG <user>` (for group membership).

use anyhow::Result;
use std::collections::BTreeMap;
use std::time::Duration;

use crate::ssh::{ssh_capture_stdout, SshOptions};
use crate::util::shell_single_quote;

use super::config::{AssertBlock, AssertFile, AssertGroup, AssertUser};
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
// Users assert
// ---------------------------------------------------------------------------

/// Run the `assert.users:` phase.
///
/// Probes via `getent passwd` for existence and shell; `id -nG <user>` for
/// group membership.  Pattern keys (containing `*`, `?`, or `[`) are matched
/// against every user in `/etc/passwd` (via `getent passwd`).
///
/// `installer_username` is the ephemeral botforge installer account for this
/// run.  It is **excluded** from pattern-based negative assertions so that
/// `botforge-*: { exists: false }` does not spuriously fail against botforge's
/// own installer user.  Exact-name assertions are never filtered.
pub(crate) fn run_assert_users(
    ssh: &SshOptions,
    assert_block: &AssertBlock,
    installer_username: Option<&str>,
) -> Result<()> {
    if assert_block.users.is_empty() {
        return Ok(());
    }

    // Separate exact-name entries from pattern entries.
    type UserEntries<'a> = Vec<(&'a String, &'a AssertUser)>;
    let (exact_entries, pattern_entries): (UserEntries<'_>, UserEntries<'_>) = assert_block
        .users
        .iter()
        .partition(|(k, _)| !is_glob_pattern(k));

    // Build the probe script.
    // For exact names: one `getent passwd <name>` line each.
    // For patterns and membership checks: enumerate all users once via `getent passwd`.
    // We always fetch the full passwd if there are pattern entries or membership checks.
    let need_all_users = !pattern_entries.is_empty()
        || exact_entries
            .iter()
            .any(|(_, e)| e.exists && !e.groups.is_empty());

    let mut probe_parts: Vec<String> = Vec::new();

    // Emit per-exact-name probes (existence + shell) as "name:shell" or "absent".
    for (name, _) in &exact_entries {
        let q = shell_single_quote(name);
        probe_parts.push(format!(
            concat!(
                "_u=$(getent passwd {q} 2>/dev/null || true)\n",
                "if [ -n \"$_u\" ]; then\n",
                "  _shell=$(echo \"$_u\" | cut -d: -f7)\n",
                "  printf 'present:%s\\n' \"$_shell\"\n",
                "else\n",
                "  printf 'absent\\n'\n",
                "fi\n",
            ),
            q = q
        ));
    }

    // Emit group-membership probes for exact users that need them.
    for (name, expectation) in &exact_entries {
        if expectation.exists && !expectation.groups.is_empty() {
            let q = shell_single_quote(name);
            probe_parts.push(format!(
                "_groups=$(id -nG {q} 2>/dev/null || true)\nprintf '%s\\n' \"$_groups\"\n",
                q = q
            ));
        }
    }

    // Emit full passwd dump for pattern matching.
    if need_all_users && !pattern_entries.is_empty() {
        probe_parts
            .push("getent passwd 2>/dev/null || true\nprintf '__END_PASSWD__\\n'\n".to_string());
    }

    let script = format!("set -e\n{}", probe_parts.join(""));
    let probe_output = ssh_capture_stdout(
        ssh,
        &script,
        ASSERT_TRANSPORT_RETRIES,
        ASSERT_TRANSPORT_RETRY_DELAY,
        ASSERT_CONNECT_TIMEOUT,
    )
    .map_err(|e| anyhow::anyhow!("assert.users: SSH probe failed: {e:#}"))?;

    let mut lines = probe_output.lines();
    let mut any_failed = false;

    // Evaluate exact-name entries.
    for (name, expectation) in &exact_entries {
        let raw_line = lines.next().unwrap_or("absent");
        let failures = check_one_user(name, expectation, raw_line, None);
        let ok = failures.is_empty();
        print_phase_status("assert", &format!("user {name}"), ok);
        if !ok {
            for msg in &failures {
                eprintln!("         {msg}");
            }
            any_failed = true;
        }

        // Consume the groups line if we emitted one.
        if expectation.exists && !expectation.groups.is_empty() {
            let groups_line = lines.next().unwrap_or("");
            let group_failures = check_user_groups(name, expectation, groups_line);
            if !group_failures.is_empty() {
                print_phase_status("assert", &format!("user {name} groups"), false);
                for msg in &group_failures {
                    eprintln!("         {msg}");
                }
                any_failed = true;
            } else if expectation.exists {
                // Only print a pass line if the existence check also passed.
                let existence_ok = failures.is_empty();
                if existence_ok {
                    print_phase_status("assert", &format!("user {name} groups"), true);
                }
            }
        }
    }

    // Evaluate pattern entries using the full passwd dump.
    if !pattern_entries.is_empty() {
        // Collect remaining lines up to __END_PASSWD__ sentinel.
        let mut all_usernames: Vec<String> = Vec::new();
        for line in lines.by_ref() {
            if line == "__END_PASSWD__" {
                break;
            }
            // getent passwd lines: name:password:uid:gid:gecos:home:shell
            if let Some(uname) = line.split(':').next() {
                if !uname.is_empty() {
                    all_usernames.push(uname.to_string());
                }
            }
        }

        for (pattern, expectation) in &pattern_entries {
            let glob_pat = glob::Pattern::new(pattern)
                .unwrap_or_else(|_| glob::Pattern::new("__no_match__").unwrap());

            if !expectation.exists {
                // Negative pattern: no matching user (except the installer) may exist.
                let matched: Vec<&str> = all_usernames
                    .iter()
                    .filter(|u| glob_pat.matches(u) && Some(u.as_str()) != installer_username)
                    .map(String::as_str)
                    .collect();

                if matched.is_empty() {
                    let label = match installer_username {
                        Some(inst) => format!(
                            r#"no user matching "{pattern}" (excluding installer "{inst}")"#
                        ),
                        None => format!(r#"no user matching "{pattern}""#),
                    };
                    print_phase_status("assert", &format!("users: {label}"), true);
                } else {
                    for found in matched {
                        let label = match installer_username {
                            Some(inst) => format!(
                                r#"expected no user matching "{pattern}" (excluding installer "{inst}"), but found "{found}""#
                            ),
                            None => format!(
                                r#"expected no user matching "{pattern}", but found "{found}""#
                            ),
                        };
                        eprintln!("         {label}");
                        print_phase_status("assert", &format!("users: {label}"), false);
                        any_failed = true;
                    }
                }
            } else {
                // Positive pattern: at least one matching user must exist.
                let matched: Vec<&str> = all_usernames
                    .iter()
                    .filter(|u| glob_pat.matches(u))
                    .map(String::as_str)
                    .collect();
                if matched.is_empty() {
                    let label = format!(r#"expected a user matching "{pattern}", but none found"#);
                    eprintln!("         {label}");
                    print_phase_status("assert", &format!("users: {label}"), false);
                    any_failed = true;
                } else {
                    print_phase_status(
                        "assert",
                        &format!(r#"users: at least one user matching "{pattern}""#),
                        true,
                    );
                }
            }
        }
    }

    if any_failed {
        anyhow::bail!("one or more assert.users: checks failed");
    }
    Ok(())
}

/// Check a single user's probe line against its expectation (existence + shell).
/// Returns a list of failure messages (empty = pass).
fn check_one_user(
    name: &str,
    expectation: &AssertUser,
    raw_line: &str,
    _installer_username: Option<&str>,
) -> Vec<String> {
    let mut failures = Vec::new();

    if raw_line == "absent" {
        if expectation.exists {
            failures.push(format!("user {name}: expected present, got absent"));
        }
        return failures;
    }

    // User is present.
    if !expectation.exists {
        failures.push(format!("user {name}: expected absent, but present"));
        return failures;
    }

    // Parse "present:<shell>"
    let Some(rest) = raw_line.strip_prefix("present:") else {
        failures.push(format!(
            "assert.users: unexpected probe output for '{name}': {raw_line}"
        ));
        return failures;
    };
    let actual_shell = rest;

    if let Some(ref expected_shell) = expectation.shell {
        if actual_shell != expected_shell {
            failures.push(format!(
                "user {name}: shell expected {expected_shell}, got {actual_shell}"
            ));
        }
    }

    failures
}

/// Check a user's group membership line against its expected groups.
/// Returns a list of failure messages (empty = pass).
fn check_user_groups(name: &str, expectation: &AssertUser, groups_line: &str) -> Vec<String> {
    let mut failures = Vec::new();
    let actual_groups: Vec<&str> = groups_line.split_whitespace().collect();
    for required_group in &expectation.groups {
        if !actual_groups.contains(&required_group.as_str()) {
            failures.push(format!(
                r#"user {name}: missing required group "{required_group}""#
            ));
        }
    }
    failures
}

// ---------------------------------------------------------------------------
// Groups assert
// ---------------------------------------------------------------------------

/// Run the `assert.groups:` phase.
///
/// Probes via `getent group` for existence.  Pattern keys are matched against
/// all group names from `getent group`.
pub(crate) fn run_assert_groups(ssh: &SshOptions, assert_block: &AssertBlock) -> Result<()> {
    if assert_block.groups.is_empty() {
        return Ok(());
    }

    type GroupEntries<'a> = Vec<(&'a String, &'a AssertGroup)>;
    let (exact_entries, pattern_entries): (GroupEntries<'_>, GroupEntries<'_>) = assert_block
        .groups
        .iter()
        .partition(|(k, _)| !is_glob_pattern(k));

    let mut probe_parts: Vec<String> = Vec::new();

    // Per-exact-name group probes.
    for (name, _) in &exact_entries {
        let q = shell_single_quote(name);
        probe_parts.push(format!(
            concat!(
                "_g=$(getent group {q} 2>/dev/null || true)\n",
                "if [ -n \"$_g\" ]; then printf 'present\\n'; else printf 'absent\\n'; fi\n",
            ),
            q = q
        ));
    }

    // Full group dump for pattern matching.
    if !pattern_entries.is_empty() {
        probe_parts
            .push("getent group 2>/dev/null || true\nprintf '__END_GROUP__\\n'\n".to_string());
    }

    let script = format!("set -e\n{}", probe_parts.join(""));
    let probe_output = ssh_capture_stdout(
        ssh,
        &script,
        ASSERT_TRANSPORT_RETRIES,
        ASSERT_TRANSPORT_RETRY_DELAY,
        ASSERT_CONNECT_TIMEOUT,
    )
    .map_err(|e| anyhow::anyhow!("assert.groups: SSH probe failed: {e:#}"))?;

    let mut lines = probe_output.lines();
    let mut any_failed = false;

    // Evaluate exact-name entries.
    for (name, expectation) in &exact_entries {
        let raw_line = lines.next().unwrap_or("absent");
        let is_present = raw_line == "present";
        let ok = is_present == expectation.exists;
        let description = if ok {
            if expectation.exists {
                format!("group {name}: present")
            } else {
                format!("group {name}: absent as expected")
            }
        } else if expectation.exists {
            format!("group {name}: expected present, got absent")
        } else {
            format!("group {name}: expected absent, but present")
        };
        print_phase_status("assert", &format!("group {name}"), ok);
        if !ok {
            eprintln!("         {description}");
            any_failed = true;
        }
    }

    // Evaluate pattern entries.
    if !pattern_entries.is_empty() {
        let mut all_groups: Vec<String> = Vec::new();
        for line in lines.by_ref() {
            if line == "__END_GROUP__" {
                break;
            }
            // getent group lines: name:password:gid:members
            if let Some(gname) = line.split(':').next() {
                if !gname.is_empty() {
                    all_groups.push(gname.to_string());
                }
            }
        }

        for (pattern, expectation) in &pattern_entries {
            let glob_pat = glob::Pattern::new(pattern)
                .unwrap_or_else(|_| glob::Pattern::new("__no_match__").unwrap());

            if !expectation.exists {
                let matched: Vec<&str> = all_groups
                    .iter()
                    .filter(|g| glob_pat.matches(g))
                    .map(String::as_str)
                    .collect();
                if matched.is_empty() {
                    print_phase_status(
                        "assert",
                        &format!(r#"groups: no group matching "{pattern}""#),
                        true,
                    );
                } else {
                    for found in matched {
                        let label = format!(
                            r#"expected no group matching "{pattern}", but found "{found}""#
                        );
                        eprintln!("         {label}");
                        print_phase_status("assert", &format!("groups: {label}"), false);
                        any_failed = true;
                    }
                }
            } else {
                let matched: Vec<&str> = all_groups
                    .iter()
                    .filter(|g| glob_pat.matches(g))
                    .map(String::as_str)
                    .collect();
                if matched.is_empty() {
                    let label = format!(r#"expected a group matching "{pattern}", but none found"#);
                    eprintln!("         {label}");
                    print_phase_status("assert", &format!("groups: {label}"), false);
                    any_failed = true;
                } else {
                    print_phase_status(
                        "assert",
                        &format!(r#"groups: at least one group matching "{pattern}""#),
                        true,
                    );
                }
            }
        }
    }

    if any_failed {
        anyhow::bail!("one or more assert.groups: checks failed");
    }
    Ok(())
}

/// Returns `true` if the string contains any glob metacharacters (`*`, `?`, `[`).
fn is_glob_pattern(s: &str) -> bool {
    s.contains('*') || s.contains('?') || s.contains('[')
}

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

    // ---------------------------------------------------------------------------
    // check_one_user tests
    // ---------------------------------------------------------------------------

    use super::{check_one_user, check_user_groups, is_glob_pattern};
    use crate::plan::config::AssertUser;

    fn user_expectation(exists: bool, shell: Option<&str>, groups: Vec<&str>) -> AssertUser {
        AssertUser {
            exists,
            shell: shell.map(str::to_string),
            groups: groups.into_iter().map(str::to_string).collect(),
        }
    }

    #[test]
    fn test_check_one_user_exists_true_absent_fails() {
        let exp = user_expectation(true, None, vec![]);
        let failures = check_one_user("bot", &exp, "absent", None);
        assert!(
            !failures.is_empty(),
            "should fail when exists:true but absent"
        );
        assert!(failures[0].contains("absent"), "{:?}", failures);
    }

    #[test]
    fn test_check_one_user_exists_false_absent_passes() {
        let exp = user_expectation(false, None, vec![]);
        let failures = check_one_user("mallory", &exp, "absent", None);
        assert!(failures.is_empty(), "should pass: {:?}", failures);
    }

    #[test]
    fn test_check_one_user_exists_false_present_fails() {
        let exp = user_expectation(false, None, vec![]);
        let failures = check_one_user("mallory", &exp, "present:/bin/bash", None);
        assert!(!failures.is_empty());
        assert!(failures[0].contains("expected absent"), "{:?}", failures);
    }

    #[test]
    fn test_check_one_user_correct_shell_passes() {
        let exp = user_expectation(true, Some("/bin/bash"), vec![]);
        let failures = check_one_user("bot", &exp, "present:/bin/bash", None);
        assert!(failures.is_empty(), "should pass: {:?}", failures);
    }

    #[test]
    fn test_check_one_user_wrong_shell_fails() {
        let exp = user_expectation(true, Some("/bin/bash"), vec![]);
        let failures = check_one_user("bot", &exp, "present:/usr/sbin/nologin", None);
        assert!(!failures.is_empty());
        assert!(failures[0].contains("shell"), "{:?}", failures);
        assert!(failures[0].contains("/bin/bash"), "{:?}", failures);
        assert!(failures[0].contains("/usr/sbin/nologin"), "{:?}", failures);
    }

    #[test]
    fn test_check_one_user_no_shell_check_passes_any_shell() {
        let exp = user_expectation(true, None, vec![]);
        let failures = check_one_user("bot", &exp, "present:/usr/sbin/nologin", None);
        assert!(
            failures.is_empty(),
            "should pass when no shell check: {:?}",
            failures
        );
    }

    // ---------------------------------------------------------------------------
    // check_user_groups tests
    // ---------------------------------------------------------------------------

    #[test]
    fn test_check_user_groups_all_present_passes() {
        let exp = user_expectation(true, None, vec!["bot", "docker"]);
        let failures = check_user_groups("bot", &exp, "bot docker sudo");
        assert!(failures.is_empty(), "should pass: {:?}", failures);
    }

    #[test]
    fn test_check_user_groups_missing_group_fails() {
        let exp = user_expectation(true, None, vec!["bot", "docker"]);
        let failures = check_user_groups("bot", &exp, "bot sudo");
        assert!(!failures.is_empty());
        assert!(failures[0].contains("docker"), "{:?}", failures);
        assert!(
            failures[0].contains("missing required group"),
            "{:?}",
            failures
        );
    }

    #[test]
    fn test_check_user_groups_empty_required_passes() {
        let exp = user_expectation(true, None, vec![]);
        let failures = check_user_groups("bot", &exp, "bot sudo docker");
        assert!(
            failures.is_empty(),
            "no required groups = always pass: {:?}",
            failures
        );
    }

    // ---------------------------------------------------------------------------
    // is_glob_pattern tests
    // ---------------------------------------------------------------------------

    #[test]
    fn test_is_glob_pattern_exact_name_is_false() {
        assert!(!is_glob_pattern("bot"));
        assert!(!is_glob_pattern("mallory"));
        assert!(!is_glob_pattern("docker"));
    }

    #[test]
    fn test_is_glob_pattern_star_is_true() {
        assert!(is_glob_pattern("botforge-*"));
        assert!(is_glob_pattern("*"));
    }

    #[test]
    fn test_is_glob_pattern_question_mark_is_true() {
        assert!(is_glob_pattern("bot?"));
    }

    #[test]
    fn test_is_glob_pattern_bracket_is_true() {
        assert!(is_glob_pattern("bot[0-9]"));
    }
}
