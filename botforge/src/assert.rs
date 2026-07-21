//! Declarative `assert:` phase — run before `steps:` in a `type: botforge/test` document
//! (after boot/SSH/cloud-init, on the fresh-boot image state).
//!
//! Currently implements the `assert.files:`, `assert.users:`, `assert.groups:`,
//! `assert.packages:`, and `assert.services:` sub-keys.  File entries are probed
//! via a single batched SSH script; user and group entries are probed via
//! `getent passwd` / `getent group` (for names and attributes) and `id -nG <user>`
//! (for group membership); package entries are probed via a single `dpkg-query -W`
//! dump; service entries are probed via `systemctl is-enabled` / `systemctl is-active`.

use anyhow::Result;
use serde::Deserialize;
use serde_yaml::Value;
use std::collections::BTreeMap;
use std::fmt;
use std::time::Duration;

use crate::ssh::{ssh_capture_stdout, SshOptions};
use crate::util::shell_single_quote;

use crate::config::{validate_mode_string, validate_owner_group_string};
use crate::plan::log::print_phase_status;

pub(crate) mod registry;

// ---------------------------------------------------------------------------
// assert: block types  (schema / parse section)
// ---------------------------------------------------------------------------

/// Expected file type for an `assert.files:` entry.
#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum AssertFileType {
    File,
    Directory,
    Symlink,
}

impl AssertFileType {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::File => "file",
            Self::Directory => "directory",
            Self::Symlink => "symlink",
        }
    }
}

impl fmt::Display for AssertFileType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

fn default_assert_exists() -> bool {
    true
}

/// A single file-existence/attribute expectation inside `assert.files:`.
///
/// When `exists: false`, all other attribute fields must be absent (rejected
/// at config-load time).
#[derive(Debug, Deserialize, Clone, PartialEq, Eq)]
pub(crate) struct AssertFile {
    /// When `false`, the path must not exist on the guest.  Defaults to `true`.
    #[serde(default = "default_assert_exists")]
    pub(crate) exists: bool,
    /// Selects the permission-check baseline for this entry.
    ///
    /// When omitted or `true` (default), a secure credset baseline is applied:
    /// `owner: root`, `group: root`, and a filetype-aware default mode
    /// (`0644` for regular files, `0755` for directories, skipped for symlinks).
    /// Any explicit `owner`/`group`/`mode` fields overlay the baseline per field.
    ///
    /// When `false`, only the permission fields the user explicitly wrote are
    /// checked; anything omitted is skipped entirely.
    ///
    /// Must be omitted when `exists: false` (a non-existent file has no perms).
    /// Only meaningful when `exists: true`.
    #[serde(rename = "default-permissions", default)]
    pub(crate) default_permissions: Option<bool>,
    /// Expected file type (`file`, `directory`, or `symlink`).
    /// Only meaningful when `exists: true`.
    #[serde(default)]
    pub(crate) filetype: Option<AssertFileType>,
    /// Expected owning user name or numeric uid.
    /// Only meaningful when `exists: true`.
    #[serde(default)]
    pub(crate) owner: Option<String>,
    /// Expected owning group name or numeric gid.
    /// Only meaningful when `exists: true`.
    #[serde(default)]
    pub(crate) group: Option<String>,
    /// Expected permission mode (3–4 octal digits, e.g. `"0755"`).
    /// Only meaningful when `exists: true`.
    #[serde(default)]
    pub(crate) mode: Option<String>,
}

/// A single user expectation inside `assert.users:`.
///
/// When `exists: false`, the `shell` and `groups` fields must be absent
/// (rejected at config-load time).  The key may be an exact name **or** a
/// glob pattern (e.g. `botforge-*`); pattern negatives enumerate
/// `getent passwd` output and match against each user name.
#[derive(Debug, Deserialize, Clone, PartialEq, Eq)]
pub(crate) struct AssertUser {
    /// When `false`, the user must not exist on the guest.  Defaults to `true`.
    #[serde(default = "default_assert_exists")]
    pub(crate) exists: bool,
    /// Expected login shell (e.g. `/bin/bash`).
    /// Only meaningful when `exists: true`.
    #[serde(default)]
    pub(crate) shell: Option<String>,
    /// All listed groups must be present in the user's supplementary groups
    /// (checked via `id -nG <user>`).  Only meaningful when `exists: true`.
    #[serde(default)]
    pub(crate) groups: Vec<String>,
}

/// A single group expectation inside `assert.groups:`.
///
/// When `exists: false`, no other attribute fields are supported.
/// The key may be an exact name **or** a glob pattern.
#[derive(Debug, Deserialize, Clone, PartialEq, Eq)]
pub(crate) struct AssertGroup {
    /// When `false`, the group must not exist on the guest.  Defaults to `true`.
    #[serde(default = "default_assert_exists")]
    pub(crate) exists: bool,
}

/// A single package expectation inside `assert.packages:`.
///
/// The only supported attribute is `installed:`.  Unknown attributes are
/// rejected at config-load time via `#[serde(deny_unknown_fields)]`.
/// The key may be an exact package name **or** a glob pattern (e.g. `*-dev`).
#[derive(Debug, Deserialize, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct AssertPackage {
    /// When `true`, the package must be installed (`install ok installed` via
    /// `dpkg-query`).  When `false`, the package must not be installed.
    /// Defaults to `true`.
    #[serde(default = "default_assert_exists")]
    pub(crate) installed: bool,
}

/// A single service expectation inside `assert.services:`.
#[derive(Debug, Deserialize, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct AssertService {
    /// When `false`, the unit must NOT exist on the guest. Defaults to `true`.
    /// When `false`, `enabled`/`active`/`environment` must be omitted (rejected at load time).
    #[serde(default = "default_assert_exists")]
    pub(crate) exists: bool,
    /// When `true`, `systemctl is-enabled` must report `enabled`.
    /// When `false`, it must not. Defaults to `true`. Only meaningful when `exists: true`.
    #[serde(default = "default_assert_exists")]
    pub(crate) enabled: bool,
    /// When `true`, `systemctl is-active` must report `active`.
    /// When `false`, it must not. Defaults to `true`. Only meaningful when `exists: true`.
    #[serde(default = "default_assert_exists")]
    pub(crate) active: bool,
    /// Optional substring assertions on the unit's `Environment=` settings
    /// (from `systemctl show -p Environment <unit>`).
    /// Only meaningful when `exists: true`.
    #[serde(default)]
    pub(crate) environment: Option<ServiceEnvironmentExpect>,
}

/// Substring expectations on a systemd unit's `Environment=` line.
///
/// `contains` — every listed substring must appear in the environment output.
/// `not_contains` — none of the listed substrings may appear.
/// Substring matching, not regex (consistent with `expect.stdout`).
#[derive(Debug, Deserialize, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct ServiceEnvironmentExpect {
    #[serde(default)]
    pub(crate) contains: Vec<String>,
    #[serde(default)]
    pub(crate) not_contains: Vec<String>,
}

/// Validated `assert:` block from a `type: botforge/test` document.
#[derive(Debug, Deserialize, Clone, Default, PartialEq, Eq)]
pub(crate) struct AssertBlock {
    /// Map of absolute guest path → file expectation.
    #[serde(default)]
    pub(crate) files: BTreeMap<String, AssertFile>,
    /// Map of user name (or glob pattern) → user expectation.
    #[serde(default)]
    pub(crate) users: BTreeMap<String, AssertUser>,
    /// Map of group name (or glob pattern) → group expectation.
    #[serde(default)]
    pub(crate) groups: BTreeMap<String, AssertGroup>,
    /// Map of package name (or glob pattern) → package expectation.
    #[serde(default)]
    pub(crate) packages: BTreeMap<String, AssertPackage>,
    /// Map of service name → service expectation.
    #[serde(default)]
    pub(crate) services: BTreeMap<String, AssertService>,
    /// Plugin-provided assert subtrees, keyed by verb name.
    /// These are retained as raw YAML and dispatched to plugin providers at run time.
    #[serde(skip)]
    pub(crate) plugin_asserts: BTreeMap<String, serde_yaml::Value>,
}

pub(crate) fn parse_assert_block(raw_block: &Value) -> Result<AssertBlock> {
    let Some(mapping) = raw_block.as_mapping() else {
        anyhow::bail!("'assert' must be a mapping");
    };

    let registry = registry::built_in_assert_registry();
    let mut block = AssertBlock::default();

    for (raw_key, raw_value) in mapping {
        let Some(verb) = raw_key.as_str() else {
            anyhow::bail!("assert verb keys must be strings");
        };
        if let Some(kind) = registry.get(verb) {
            kind.parse_into(raw_value, &mut block)?;
        } else {
            // Non-built-in verb: retain the raw subtree.
            // Validity requires a loaded plugin providing assert/<verb>;
            // the run/validate phase checks this.
            block
                .plugin_asserts
                .insert(verb.to_owned(), raw_value.clone());
        }
    }

    Ok(block)
}

// ---------------------------------------------------------------------------
// Config-load-time validation
// ---------------------------------------------------------------------------

pub(crate) fn validate_assert_block(block: &AssertBlock) -> Result<()> {
    let registry = registry::built_in_assert_registry();
    for kind in registry.iter() {
        kind.validate(block)?;
    }
    Ok(())
}

fn validate_assert_file_entry(guest_path: &str, expectation: &AssertFile) -> Result<()> {
    if !guest_path.starts_with('/') {
        anyhow::bail!(
            "assert.files: path '{guest_path}' must be an absolute guest path (must start with '/')"
        );
    }
    if !expectation.exists {
        // When exists: false, attribute fields are meaningless — reject them.
        if expectation.filetype.is_some()
            || expectation.owner.is_some()
            || expectation.group.is_some()
            || expectation.mode.is_some()
            || expectation.default_permissions.is_some()
        {
            anyhow::bail!(
                "assert.files: path '{guest_path}': attribute fields \
                 (filetype/owner/group/mode/default-permissions) must not be set when `exists: false`"
            );
        }
        return Ok(());
    }
    if let Some(ref mode) = expectation.mode {
        validate_mode_string(mode, guest_path, "assert")?;
    }
    if let Some(ref owner) = expectation.owner {
        validate_owner_group_string(owner, "owner", guest_path, "assert")?;
    }
    if let Some(ref group) = expectation.group {
        validate_owner_group_string(group, "group", guest_path, "assert")?;
    }
    Ok(())
}

fn validate_assert_user_entry(name_or_pattern: &str, expectation: &AssertUser) -> Result<()> {
    if !expectation.exists {
        // When exists: false, attribute fields are meaningless — reject them.
        if expectation.shell.is_some() || !expectation.groups.is_empty() {
            anyhow::bail!(
                "assert.users: entry '{name_or_pattern}': attribute fields \
                 (shell/groups) must not be set when `exists: false`"
            );
        }
    }
    Ok(())
}

fn validate_assert_group_entry(_name_or_pattern: &str, _expectation: &AssertGroup) -> Result<()> {
    // Currently no additional validation beyond deserialization for groups.
    Ok(())
}

fn validate_assert_package_entry(
    _name_or_pattern: &str,
    _expectation: &AssertPackage,
) -> Result<()> {
    // Unknown attributes are already rejected at deserialization time via
    // `#[serde(deny_unknown_fields)]` on `AssertPackage`.  No further
    // validation is required in v1.
    Ok(())
}

fn validate_assert_service_entry(name: &str, expectation: &AssertService) -> Result<()> {
    // Parse-time validation catches explicit `enabled` / `active` keys when
    // `exists: false` (including explicit `true`). Keep this invariant check
    // for structural consistency with other assert entry validators.
    if !expectation.exists && (!expectation.enabled || !expectation.active) {
        anyhow::bail!(
            "assert.services: entry '{name}': enabled/active must not be set when `exists: false`"
        );
    }
    if !expectation.exists && expectation.environment.is_some() {
        anyhow::bail!(
            "assert.services: entry '{name}': environment must not be set when `exists: false`"
        );
    }
    Ok(())
}

const ASSERT_TRANSPORT_RETRIES: usize = 3;
const ASSERT_TRANSPORT_RETRY_DELAY: Duration = Duration::from_secs(2);
const ASSERT_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const FILE_PROBE_HEREDOC_DELIM: &str = "__BOTFORGE_FILE_PROBE__";
const USERS_PROBE_HEREDOC_DELIM: &str = "__BOTFORGE_USERS_PROBE__";
const GROUPS_PROBE_HEREDOC_DELIM: &str = "__BOTFORGE_GROUPS_PROBE__";

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
    let probe_output = run_file_probe(ssh, &paths).map_err(|e| {
        anyhow::anyhow!(
            "assert.files: privileged probe failed: sudo -n not available or failed ({e:#})"
        )
    })?;

    evaluate_probe_results(&assert_block.files, &probe_output)
}

// ---------------------------------------------------------------------------
// Probe
// ---------------------------------------------------------------------------

/// Build the inner probe script body for `paths`.
///
/// Returns a `set -e` shell script that outputs exactly one line per path in
/// the same order as `paths`.  Each line is either `absent` or
/// `present:<filetype>:<owner>:<group>:<mode>`.
///
/// This function is pure (no I/O) and is extracted so it can be unit-tested.
fn build_file_probe_script(paths: &[&str]) -> String {
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
    script
}

/// Build and execute the batched SSH probe script under `sudo -n sh`.
///
/// Running privileged is required so that root-only paths (e.g.
/// `/etc/sudoers.d/*`, SSH host keys, `/root/*`) can be stat-ed.  The botforge
/// installer user has NOPASSWD sudo on build/test VMs.
///
/// The script outputs exactly one line per path in the same order as `paths`.
/// Each line is either `absent` or `present:<filetype>:<owner>:<group>:<mode>`.
///
/// If `sudo -n` is unavailable or not configured for passwordless access the
/// remote command will exit non-zero, causing an error to propagate rather than
/// silently reporting every path as `absent`.
fn build_privileged_probe_script(inner_script: &str, heredoc_delim: &str) -> String {
    format!("sudo -n sh <<'{heredoc_delim}'\n{inner_script}{heredoc_delim}\n")
}

pub(crate) fn run_privileged_probe(
    ssh: &SshOptions,
    inner_script: &str,
    heredoc_delim: &str,
) -> Result<String> {
    let script = build_privileged_probe_script(inner_script, heredoc_delim);
    ssh_capture_stdout(
        ssh,
        &script,
        ASSERT_TRANSPORT_RETRIES,
        ASSERT_TRANSPORT_RETRY_DELAY,
        ASSERT_CONNECT_TIMEOUT,
    )
}

fn run_file_probe(ssh: &SshOptions, paths: &[&str]) -> Result<String> {
    let inner = build_file_probe_script(paths);
    run_privileged_probe(ssh, &inner, FILE_PROBE_HEREDOC_DELIM)
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
        print_phase_status("assert", path, ok, None);
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

    // Filetype check is always opt-in (explicit field only).
    if let Some(expected_ft) = expectation.filetype {
        if actual_ft != expected_ft.as_str() {
            failures.push(format!(
                "filetype: expected {}, got {actual_ft}",
                expected_ft.as_str()
            ));
        }
    }

    // Resolve the effective permission check set.
    //
    // `default_permissions` (None or Some(true)) → apply secure baseline:
    //   owner: root, group: root, mode: filetype-aware default (0644/0755/skip).
    //   Explicit owner/group/mode fields overlay the baseline per field.
    //
    // `default_permissions = Some(false)` → only fields the user wrote are checked.
    let use_defaults = expectation.default_permissions.unwrap_or(true);

    let effective_owner: Option<&str> = if expectation.owner.is_some() {
        expectation.owner.as_deref()
    } else if use_defaults {
        Some("root")
    } else {
        None
    };

    let effective_group: Option<&str> = if expectation.group.is_some() {
        expectation.group.as_deref()
    } else if use_defaults {
        Some("root")
    } else {
        None
    };

    // Filetype-aware default mode: 0644 for regular files, 0755 for
    // directories, None (skip) for symlinks.
    let effective_mode: Option<&str> = if expectation.mode.is_some() {
        expectation.mode.as_deref()
    } else if use_defaults {
        match actual_ft {
            "directory" => Some("0755"),
            "symlink" => None, // symlink permissions are meaningless
            _ => Some("0644"), // regular files and any other type
        }
    } else {
        None
    };

    if let Some(expected_owner) = effective_owner {
        if actual_owner != expected_owner {
            failures.push(format!(
                "owner: expected {expected_owner}, got {actual_owner}"
            ));
        }
    }
    if let Some(expected_group) = effective_group {
        if actual_group != expected_group {
            failures.push(format!(
                "group: expected {expected_group}, got {actual_group}"
            ));
        }
    }
    if let Some(expected_mode) = effective_mode {
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
                "if _u=$(getent passwd {q}); then\n",
                "  _shell=$(echo \"$_u\" | cut -d: -f7)\n",
                "  printf 'present:%s\\n' \"$_shell\"\n",
                "else\n",
                "  _rc=$?\n",
                "  if [ \"$_rc\" -eq 2 ]; then printf 'absent\\n'; else exit \"$_rc\"; fi\n",
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
                "_groups=$(id -nG {q})\nprintf '%s\\n' \"$_groups\"\n",
                q = q
            ));
        }
    }

    // Emit full passwd dump for pattern matching.
    if need_all_users && !pattern_entries.is_empty() {
        probe_parts.push("getent passwd\nprintf '__END_PASSWD__\\n'\n".to_string());
    }

    let script = format!("set -e\n{}", probe_parts.join(""));
    let probe_output =
        run_privileged_probe(ssh, &script, USERS_PROBE_HEREDOC_DELIM).map_err(|e| {
            anyhow::anyhow!(
                "assert.users: privileged probe failed: sudo -n not available or failed ({e:#})"
            )
        })?;

    let mut lines = probe_output.lines();
    let mut any_failed = false;

    // Evaluate exact-name entries.
    for (name, expectation) in &exact_entries {
        let raw_line = lines.next().unwrap_or("absent");
        let failures = check_one_user(name, expectation, raw_line, None);
        let ok = failures.is_empty();
        print_phase_status("assert", &format!("user {name}"), ok, None);
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
                print_phase_status("assert", &format!("user {name} groups"), false, None);
                for msg in &group_failures {
                    eprintln!("         {msg}");
                }
                any_failed = true;
            } else if expectation.exists {
                // Only print a pass line if the existence check also passed.
                let existence_ok = failures.is_empty();
                if existence_ok {
                    print_phase_status("assert", &format!("user {name} groups"), true, None);
                }
            }
        }
    }

    // Evaluate pattern entries using the full passwd dump.
    if !pattern_entries.is_empty() {
        // Collect remaining lines up to __END_PASSWD__ sentinel, filtering out
        // the installer identity so it is invisible to all pattern assertions
        // (positive and negative).
        let all_usernames = parse_names_from_dump(&mut lines, "__END_PASSWD__", installer_username);

        for (pattern, expectation) in &pattern_entries {
            let glob_pat = glob::Pattern::new(pattern)
                .unwrap_or_else(|_| glob::Pattern::new("__no_match__").unwrap());

            if !expectation.exists {
                // Negative pattern: no matching user may exist (installer already
                // excluded from the candidate list).
                let matched: Vec<&str> = all_usernames
                    .iter()
                    .filter(|u| glob_pat.matches(u))
                    .map(String::as_str)
                    .collect();

                if matched.is_empty() {
                    let label = match installer_username {
                        Some(inst) => format!(
                            r#"no user matching "{pattern}" (excluding installer "{inst}")"#
                        ),
                        None => format!(r#"no user matching "{pattern}""#),
                    };
                    print_phase_status("assert", &format!("users: {label}"), true, None);
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
                        print_phase_status("assert", &format!("users: {label}"), false, None);
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
                    print_phase_status("assert", &format!("users: {label}"), false, None);
                    any_failed = true;
                } else {
                    print_phase_status(
                        "assert",
                        &format!(r#"users: at least one user matching "{pattern}""#),
                        true,
                        None,
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
///
/// `installer_username` is the ephemeral botforge installer account for this
/// run.  The installer's same-named primary group is **excluded** from the
/// candidate set before any matching so that `botforge-*: { exists: false }`
/// does not spuriously fail, and so that `botforge-*: { exists: true }` cannot
/// be satisfied by the installer group alone.
pub(crate) fn run_assert_groups(
    ssh: &SshOptions,
    assert_block: &AssertBlock,
    installer_username: Option<&str>,
) -> Result<()> {
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
                "if _g=$(getent group {q}); then\n",
                "  printf 'present\\n'\n",
                "else\n",
                "  _rc=$?\n",
                "  if [ \"$_rc\" -eq 2 ]; then printf 'absent\\n'; else exit \"$_rc\"; fi\n",
                "fi\n",
            ),
            q = q
        ));
    }

    // Full group dump for pattern matching.
    if !pattern_entries.is_empty() {
        probe_parts.push("getent group\nprintf '__END_GROUP__\\n'\n".to_string());
    }

    let script = format!("set -e\n{}", probe_parts.join(""));
    let probe_output =
        run_privileged_probe(ssh, &script, GROUPS_PROBE_HEREDOC_DELIM).map_err(|e| {
            anyhow::anyhow!(
                "assert.groups: privileged probe failed: sudo -n not available or failed ({e:#})"
            )
        })?;

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
        print_phase_status("assert", &format!("group {name}"), ok, None);
        if !ok {
            eprintln!("         {description}");
            any_failed = true;
        }
    }

    // Evaluate pattern entries.
    if !pattern_entries.is_empty() {
        // Collect remaining lines up to __END_GROUP__ sentinel, filtering out
        // the installer identity (its same-named primary group) so it is
        // invisible to all pattern assertions (positive and negative).
        let all_groups = parse_names_from_dump(&mut lines, "__END_GROUP__", installer_username);

        for (pattern, expectation) in &pattern_entries {
            let glob_pat = glob::Pattern::new(pattern)
                .unwrap_or_else(|_| glob::Pattern::new("__no_match__").unwrap());

            if !expectation.exists {
                // Negative pattern: no matching group may exist (installer already
                // excluded from the candidate list).
                let matched: Vec<&str> = all_groups
                    .iter()
                    .filter(|g| glob_pat.matches(g))
                    .map(String::as_str)
                    .collect();
                if matched.is_empty() {
                    let label = match installer_username {
                        Some(inst) => format!(
                            r#"no group matching "{pattern}" (excluding installer "{inst}")"#
                        ),
                        None => format!(r#"no group matching "{pattern}""#),
                    };
                    print_phase_status("assert", &format!("groups: {label}"), true, None);
                } else {
                    for found in matched {
                        let label = match installer_username {
                            Some(inst) => format!(
                                r#"expected no group matching "{pattern}" (excluding installer "{inst}"), but found "{found}""#
                            ),
                            None => format!(
                                r#"expected no group matching "{pattern}", but found "{found}""#
                            ),
                        };
                        eprintln!("         {label}");
                        print_phase_status("assert", &format!("groups: {label}"), false, None);
                        any_failed = true;
                    }
                }
            } else {
                // Positive pattern: at least one matching group must exist (installer
                // excluded from candidates, so it cannot satisfy this assertion alone).
                let matched: Vec<&str> = all_groups
                    .iter()
                    .filter(|g| glob_pat.matches(g))
                    .map(String::as_str)
                    .collect();
                if matched.is_empty() {
                    let label = format!(r#"expected a group matching "{pattern}", but none found"#);
                    eprintln!("         {label}");
                    print_phase_status("assert", &format!("groups: {label}"), false, None);
                    any_failed = true;
                } else {
                    print_phase_status(
                        "assert",
                        &format!(r#"groups: at least one group matching "{pattern}""#),
                        true,
                        None,
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

/// Parse a `getent passwd` or `getent group` output dump up to a sentinel line,
/// extracting the first colon-separated field (name) from each line.
///
/// If `installer` is `Some(name)`, that exact name is **omitted** from the
/// returned list so it is invisible to all pattern assertions — positive and
/// negative, exact and glob — for both users and groups.  This is the single,
/// authoritative place where the ephemeral installer identity is filtered.
fn parse_names_from_dump<'a>(
    lines: &mut impl Iterator<Item = &'a str>,
    sentinel: &str,
    installer: Option<&str>,
) -> Vec<String> {
    let mut names = Vec::new();
    for line in lines {
        if line == sentinel {
            break;
        }
        if let Some(name) = line.split(':').next() {
            if !name.is_empty() && Some(name) != installer {
                names.push(name.to_string());
            }
        }
    }
    names
}

// ---------------------------------------------------------------------------
// Packages assert
// ---------------------------------------------------------------------------

/// Run the `assert.packages:` phase.
///
/// Probes via a single `dpkg-query -W` dump.  A package is considered
/// installed only when its status is `install ok installed`.  Pattern keys
/// (containing `*`, `?`, or `[`) are matched against all package names from
/// the dump.
///
/// - Exact positive (`installed: true`): the package must be installed.
/// - Exact negative (`installed: false`): the package must not be installed.
/// - Pattern positive (`installed: true`): at least one installed package must match.
/// - Pattern negative (`installed: false`): no installed package may match.
pub(crate) fn run_assert_packages(ssh: &SshOptions, assert_block: &AssertBlock) -> Result<()> {
    if assert_block.packages.is_empty() {
        return Ok(());
    }

    type PkgEntries<'a> = Vec<(&'a String, &'a AssertPackage)>;
    let (exact_entries, pattern_entries): (PkgEntries<'_>, PkgEntries<'_>) = assert_block
        .packages
        .iter()
        .partition(|(k, _)| !is_glob_pattern(k));

    // Single round-trip: dump all installed packages once.
    let need_all_packages = !pattern_entries.is_empty();
    let need_exact = !exact_entries.is_empty();

    let mut probe_parts: Vec<String> = Vec::new();

    // Per-exact-name probes: "present" or "absent".
    for (name, _) in &exact_entries {
        let q = shell_single_quote(name);
        probe_parts.push(format!(
            concat!(
                "_s=$(dpkg-query -W -f='${{Status}}' {q} 2>/dev/null || true)\n",
                "if [ \"$_s\" = 'install ok installed' ]; then\n",
                "  printf 'present\\n'\n",
                "else\n",
                "  printf 'absent\\n'\n",
                "fi\n",
            ),
            q = q
        ));
    }

    // Full dump for pattern matching — emitted once regardless of how many patterns.
    if need_all_packages {
        probe_parts.push(
            "dpkg-query -W -f='${Package} ${Status}\\n' 2>/dev/null || true\nprintf '__END_DPKG__\\n'\n"
                .to_string(),
        );
    }

    if !need_exact && !need_all_packages {
        return Ok(());
    }

    let script = format!("set -e\n{}", probe_parts.join(""));
    let probe_output = ssh_capture_stdout(
        ssh,
        &script,
        ASSERT_TRANSPORT_RETRIES,
        ASSERT_TRANSPORT_RETRY_DELAY,
        ASSERT_CONNECT_TIMEOUT,
    )
    .map_err(|e| anyhow::anyhow!("assert.packages: SSH probe failed: {e:#}"))?;

    let mut lines = probe_output.lines();
    let mut any_failed = false;

    // Evaluate exact-name entries.
    for (name, expectation) in &exact_entries {
        let raw_line = lines.next().unwrap_or("absent");
        let is_installed = raw_line == "present";
        let ok = is_installed == expectation.installed;
        print_phase_status("assert", &format!("package {name}"), ok, None);
        if !ok {
            let msg = if expectation.installed {
                format!("package {name}: expected installed, but not installed")
            } else {
                format!("package {name}: expected NOT installed, but present")
            };
            eprintln!("         {msg}");
            any_failed = true;
        }
    }

    // Evaluate pattern entries using the full dpkg-query dump.
    if !pattern_entries.is_empty() {
        // Collect installed package names up to the sentinel.
        let installed_pkgs = parse_dpkg_installed(&mut lines);

        for (pattern, expectation) in &pattern_entries {
            let glob_pat = glob::Pattern::new(pattern)
                .unwrap_or_else(|_| glob::Pattern::new("__no_match__").unwrap());

            if !expectation.installed {
                // Negative pattern: no installed package may match.
                let matched: Vec<&str> = installed_pkgs
                    .iter()
                    .filter(|p| glob_pat.matches(p))
                    .map(String::as_str)
                    .collect();
                if matched.is_empty() {
                    print_phase_status(
                        "assert",
                        &format!(r#"packages: expected no installed package matching "{pattern}""#),
                        true,
                        None,
                    );
                } else {
                    for found in matched {
                        let label = format!(
                            r#"packages: expected no installed package matching "{pattern}", but found "{found}""#
                        );
                        eprintln!("         {label}");
                        print_phase_status("assert", &label, false, None);
                        any_failed = true;
                    }
                }
            } else {
                // Positive pattern: at least one installed package must match.
                let matched: Vec<&str> = installed_pkgs
                    .iter()
                    .filter(|p| glob_pat.matches(p))
                    .map(String::as_str)
                    .collect();
                if matched.is_empty() {
                    let label = format!(
                        r#"packages: expected ≥1 installed matching "{pattern}", found none"#
                    );
                    eprintln!("         {label}");
                    print_phase_status("assert", &label, false, None);
                    any_failed = true;
                } else {
                    print_phase_status(
                        "assert",
                        &format!(r#"packages: at least one installed matching "{pattern}""#),
                        true,
                        None,
                    );
                }
            }
        }
    }

    if any_failed {
        anyhow::bail!("one or more assert.packages: checks failed");
    }
    Ok(())
}

/// Parse lines from `dpkg-query -W -f='${Package} ${Status}\n'` up to (but not
/// including) a sentinel line, returning the names of all installed packages
/// (status == `install ok installed`).
///
/// Exposed for unit testing.
fn parse_dpkg_installed<'a>(lines: &mut impl Iterator<Item = &'a str>) -> Vec<String> {
    let mut installed = Vec::new();
    for line in lines {
        if line == "__END_DPKG__" {
            break;
        }
        let mut parts = line.splitn(2, ' ');
        let pkg_name = parts.next().unwrap_or("").trim();
        let status = parts.next().unwrap_or("").trim();
        if !pkg_name.is_empty() && status == "install ok installed" {
            installed.push(pkg_name.to_string());
        }
    }
    installed
}

// ---------------------------------------------------------------------------
// Services assert
// ---------------------------------------------------------------------------

/// Run the `assert.services:` phase.
///
/// For each declared service, probes via `systemctl cat`, `systemctl is-enabled`,
/// `systemctl is-active`, and (when declared) `systemctl show -p Environment` in
/// a single batched SSH script.  Each service emits one state line
/// `<exists>:<enabled-state>:<active-state>` (e.g. `present:enabled:active`),
/// followed (when `environment:` is declared) by the `Environment=...` line and
/// a `__END_ENV_<name>__` sentinel.
pub(crate) fn run_assert_services(ssh: &SshOptions, assert_block: &AssertBlock) -> Result<()> {
    if assert_block.services.is_empty() {
        return Ok(());
    }

    let mut script = String::from("set -e\n");
    for (name, expectation) in &assert_block.services {
        let q = shell_single_quote(name);
        script.push_str(&format!(
            concat!(
                "if systemctl cat {q} >/dev/null 2>&1; then _x=present; else _x=absent; fi\n",
                "_e=$(systemctl is-enabled {q} 2>/dev/null || true)\n",
                "_a=$(systemctl is-active {q} 2>/dev/null || true)\n",
                "printf '%s:%s:%s\\n' \"$_x\" \"$_e\" \"$_a\"\n",
            ),
            q = q
        ));
        // Emit environment probe for services that declare environment: checks.
        if expectation.environment.is_some() {
            let sentinel = format!("__END_ENV_{}__", name);
            script.push_str(&format!(
                concat!(
                    "_env=$(systemctl show -p Environment {q} 2>/dev/null || true)\n",
                    "printf '%s\\n' \"$_env\"\n",
                    "printf '{sentinel}\\n'\n",
                ),
                q = q,
                sentinel = sentinel,
            ));
        }
    }

    let probe_output = ssh_capture_stdout(
        ssh,
        &script,
        ASSERT_TRANSPORT_RETRIES,
        ASSERT_TRANSPORT_RETRY_DELAY,
        ASSERT_CONNECT_TIMEOUT,
    )
    .map_err(|e| anyhow::anyhow!("assert.services: SSH probe failed: {e:#}"))?;

    let mut lines = probe_output.lines();
    let mut any_failed = false;

    for (name, expectation) in &assert_block.services {
        let raw_line = lines.next().unwrap_or("absent::");
        let result = check_one_service(name, expectation, raw_line);
        print_phase_status(
            "assert",
            &format!("service {name} exists"),
            result.exists_ok,
            None,
        );
        if let Some(ok) = result.enabled_ok {
            print_phase_status("assert", &format!("service {name} enabled"), ok, None);
        }
        if let Some(ok) = result.active_ok {
            print_phase_status("assert", &format!("service {name} active"), ok, None);
        }
        if !result.failures.is_empty() {
            for msg in &result.failures {
                eprintln!("         {msg}");
            }
            any_failed = true;
        }

        // Consume and evaluate environment output when declared.
        if let Some(env_expect) = &expectation.environment {
            let sentinel = format!("__END_ENV_{}__", name);
            // The probe emits: one `Environment=...` line + sentinel.
            let env_output = lines.next().unwrap_or("");
            loop {
                match lines.next() {
                    None => break,
                    Some(l) if l == sentinel => break,
                    Some(_) => {}
                }
            }
            let env_failures = check_service_environment(name, env_expect, env_output);
            for (label, ok, msg) in &env_failures {
                print_phase_status("assert", label, *ok, None);
                if !ok {
                    if let Some(m) = msg {
                        eprintln!("         {m}");
                    }
                    any_failed = true;
                }
            }
        }
    }

    if any_failed {
        anyhow::bail!("one or more assert.services: checks failed");
    }
    Ok(())
}

#[derive(Debug)]
struct ServiceCheckResult {
    exists_ok: bool,
    enabled_ok: Option<bool>,
    active_ok: Option<bool>,
    failures: Vec<String>,
}

fn check_one_service(
    name: &str,
    expectation: &AssertService,
    raw_line: &str,
) -> ServiceCheckResult {
    let mut parts = raw_line.splitn(3, ':');
    let exists_state = parts.next().unwrap_or("").trim();
    let enabled_state = parts.next().unwrap_or("").trim();
    let active_state = parts.next().unwrap_or("").trim();

    let mut result = ServiceCheckResult {
        exists_ok: false,
        enabled_ok: None,
        active_ok: None,
        failures: Vec::new(),
    };

    let is_present = match exists_state {
        "present" => true,
        "absent" => false,
        _ => {
            result.failures.push(format!(
                "assert.services: unexpected probe output for '{name}': {raw_line}"
            ));
            return result;
        }
    };

    if !expectation.exists {
        result.exists_ok = !is_present;
        if is_present {
            result.failures.push(format!(
                "service {name}: expected absent, but the unit exists"
            ));
        }
        return result;
    }

    if !is_present {
        result.failures.push(format!(
            "service {name}: expected present, but the unit does not exist"
        ));
        return result;
    }

    result.exists_ok = true;

    let actual_enabled = enabled_state == "enabled";
    let enabled_ok = actual_enabled == expectation.enabled;
    result.enabled_ok = Some(enabled_ok);
    if !enabled_ok {
        result.failures.push(format!(
            "service {name}: expected enabled={}, but systemctl is-enabled returned '{enabled_state}'",
            expectation.enabled
        ));
    }

    let actual_active = active_state == "active";
    let active_ok = actual_active == expectation.active;
    result.active_ok = Some(active_ok);
    if !active_ok {
        result.failures.push(format!(
            "service {name}: expected active={}, but systemctl is-active returned '{active_state}'",
            expectation.active
        ));
    }

    result
}

/// Evaluate `environment:` substring assertions against the `Environment=...` line
/// emitted by `systemctl show -p Environment`.
///
/// Returns a list of `(label, ok, Option<detail_message>)` tuples — one per
/// substring check — in `contains` order then `not_contains` order.
fn check_service_environment(
    name: &str,
    expect: &ServiceEnvironmentExpect,
    env_output: &str,
) -> Vec<(String, bool, Option<String>)> {
    let mut results = Vec::new();

    for substr in &expect.contains {
        let ok = env_output.contains(substr.as_str());
        let msg = if ok {
            None
        } else {
            Some(format!(
                "service {name}: environment does not contain {substr:?} (got: {env_output:?})"
            ))
        };
        results.push((
            format!("service {name} environment contains {substr:?}"),
            ok,
            msg,
        ));
    }

    for substr in &expect.not_contains {
        let ok = !env_output.contains(substr.as_str());
        let msg = if ok {
            None
        } else {
            Some(format!(
                "service {name}: environment must not contain {substr:?} (got: {env_output:?})"
            ))
        };
        results.push((
            format!("service {name} environment not_contains {substr:?}"),
            ok,
            msg,
        ));
    }

    results
}

#[cfg(test)]
mod tests {
    use super::{
        check_one_path, check_one_service, normalize_mode, AssertFile, AssertFileType,
        AssertService,
    };

    fn file_expectation(
        exists: bool,
        filetype: Option<AssertFileType>,
        owner: Option<&str>,
        group: Option<&str>,
        mode: Option<&str>,
    ) -> AssertFile {
        AssertFile {
            exists,
            default_permissions: None, // None → effective default true (baseline applies)
            filetype,
            owner: owner.map(str::to_string),
            group: group.map(str::to_string),
            mode: mode.map(str::to_string),
        }
    }

    /// Helper for entries where `default-permissions: false` (explicit-only checks).
    fn file_expectation_no_defaults(
        filetype: Option<AssertFileType>,
        owner: Option<&str>,
        group: Option<&str>,
        mode: Option<&str>,
    ) -> AssertFile {
        AssertFile {
            exists: true,
            default_permissions: Some(false),
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
        // With default-permissions: false and no explicit perm fields, only
        // existence is checked — any present file passes.
        let exp = file_expectation_no_defaults(None, None, None, None);
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
    // default-permissions tests
    // ---------------------------------------------------------------------------

    #[test]
    fn test_default_permissions_true_file_checks_root_root_0644() {
        // {} (empty entry, default-permissions = true) → root:root 0644 for a regular file.
        let exp = file_expectation(true, None, None, None, None);
        let line = "present:file:root:root:644";
        let failures = check_one_path("/etc/botwork/cfg.yaml", &exp, line);
        assert!(
            failures.is_empty(),
            "root:root:0644 should pass: {:?}",
            failures
        );
    }

    #[test]
    fn test_default_permissions_true_file_wrong_owner_fails() {
        // Default baseline fires even with no explicit owner field.
        let exp = file_expectation(true, None, None, None, None);
        let line = "present:file:www-data:root:644";
        let failures = check_one_path("/etc/botwork/cfg.yaml", &exp, line);
        assert!(
            !failures.is_empty(),
            "non-root owner should fail with defaults"
        );
        assert!(
            failures.iter().any(|m| m.contains("owner")),
            "owner failure expected: {:?}",
            failures
        );
    }

    #[test]
    fn test_default_permissions_true_file_wrong_mode_fails() {
        // Default baseline fires even with no explicit mode field.
        let exp = file_expectation(true, None, None, None, None);
        let line = "present:file:root:root:666";
        let failures = check_one_path("/etc/botwork/cfg.yaml", &exp, line);
        assert!(!failures.is_empty(), "wrong mode should fail with defaults");
        assert!(
            failures.iter().any(|m| m.contains("mode")),
            "mode failure expected: {:?}",
            failures
        );
    }

    #[test]
    fn test_default_permissions_true_directory_checks_root_root_0755() {
        // directory default → root:root 0755
        let exp = file_expectation(true, Some(AssertFileType::Directory), None, None, None);
        let line = "present:directory:root:root:755";
        let failures = check_one_path("/etc/botwork", &exp, line);
        assert!(
            failures.is_empty(),
            "root:root:0755 dir should pass: {:?}",
            failures
        );
    }

    #[test]
    fn test_default_permissions_true_directory_wrong_mode_fails() {
        // Directory baseline mode is 0755; 0644 should fail.
        let exp = file_expectation(true, Some(AssertFileType::Directory), None, None, None);
        let line = "present:directory:root:root:644";
        let failures = check_one_path("/etc/botwork", &exp, line);
        assert!(
            !failures.is_empty(),
            "0644 on dir should fail: {:?}",
            failures
        );
        assert!(
            failures.iter().any(|m| m.contains("mode")),
            "mode failure expected: {:?}",
            failures
        );
    }

    #[test]
    fn test_default_permissions_true_symlink_skips_mode() {
        // Symlink: owner/group checked via baseline, mode is skipped.
        let exp = file_expectation(true, Some(AssertFileType::Symlink), None, None, None);
        let line = "present:symlink:root:root:777";
        let failures = check_one_path("/usr/local/bin/tool", &exp, line);
        assert!(
            failures.is_empty(),
            "symlink with any mode should pass: {:?}",
            failures
        );
    }

    #[test]
    fn test_default_permissions_true_symlink_wrong_owner_still_fails() {
        // Even for symlinks, owner/group baseline still applies.
        let exp = file_expectation(true, Some(AssertFileType::Symlink), None, None, None);
        let line = "present:symlink:nobody:root:777";
        let failures = check_one_path("/usr/local/bin/tool", &exp, line);
        assert!(
            !failures.is_empty(),
            "wrong owner on symlink should fail: {:?}",
            failures
        );
    }

    #[test]
    fn test_default_permissions_true_explicit_mode_overlays_baseline() {
        // Explicit mode: "0440" overrides the file baseline "0644".
        let exp = file_expectation(true, None, None, None, Some("0440"));
        let line = "present:file:root:root:440";
        let failures = check_one_path("/etc/sudoers.d/90-botwork", &exp, line);
        assert!(
            failures.is_empty(),
            "root:root:0440 should pass: {:?}",
            failures
        );
    }

    #[test]
    fn test_default_permissions_true_explicit_owner_overlays_baseline() {
        // Explicit owner: "www-data" overrides baseline "root"; group still root.
        let exp = file_expectation(true, None, Some("www-data"), None, None);
        let line = "present:file:www-data:root:644";
        let failures = check_one_path("/var/www/index.html", &exp, line);
        assert!(
            failures.is_empty(),
            "www-data:root:0644 should pass: {:?}",
            failures
        );
    }

    #[test]
    fn test_default_permissions_false_no_perm_fields_passes_any_perms() {
        // default-permissions: false + no explicit perm fields → existence only.
        let exp = file_expectation_no_defaults(None, None, None, None);
        let line = "present:file:someuser:somegroup:666";
        let failures = check_one_path("/scratch/x", &exp, line);
        assert!(
            failures.is_empty(),
            "existence-only should pass: {:?}",
            failures
        );
    }

    #[test]
    fn test_default_permissions_false_with_mode_only_checks_mode() {
        // default-permissions: false, mode: "0600" → only mode checked.
        let exp = file_expectation_no_defaults(None, None, None, Some("0600"));
        let line = "present:file:anyuser:anygroup:600";
        let failures = check_one_path("/run/secret", &exp, line);
        assert!(
            failures.is_empty(),
            "mode-only check should pass: {:?}",
            failures
        );
    }

    #[test]
    fn test_default_permissions_false_with_mode_only_wrong_mode_fails() {
        let exp = file_expectation_no_defaults(None, None, None, Some("0600"));
        let line = "present:file:anyuser:anygroup:644";
        let failures = check_one_path("/run/secret", &exp, line);
        assert!(
            !failures.is_empty(),
            "wrong mode should fail: {:?}",
            failures
        );
        assert!(
            failures.iter().any(|m| m.contains("mode")),
            "mode failure expected: {:?}",
            failures
        );
        // Owner and group must NOT be checked.
        assert!(
            !failures
                .iter()
                .any(|m| m.contains("owner") || m.contains("group")),
            "owner/group must not be checked with default-permissions:false: {:?}",
            failures
        );
    }

    fn service_expectation(exists: bool, enabled: bool, active: bool) -> AssertService {
        AssertService {
            exists,
            enabled,
            active,
            environment: None,
        }
    }

    #[test]
    fn test_check_one_service_present_enabled_active_passes() {
        let exp = service_expectation(true, true, true);
        let result = check_one_service("ssh", &exp, "present:enabled:active");
        assert!(result.exists_ok);
        assert_eq!(result.enabled_ok, Some(true));
        assert_eq!(result.active_ok, Some(true));
        assert!(result.failures.is_empty(), "{result:?}");
    }

    #[test]
    fn test_check_one_service_absent_expected_absent_passes() {
        let exp = service_expectation(false, true, true);
        let result = check_one_service("obsolete", &exp, "absent:not-found:inactive");
        assert!(result.exists_ok);
        assert_eq!(result.enabled_ok, None);
        assert_eq!(result.active_ok, None);
        assert!(result.failures.is_empty(), "{result:?}");
    }

    #[test]
    fn test_check_one_service_absent_expected_present_fails() {
        let exp = service_expectation(true, true, true);
        let result = check_one_service("ssh", &exp, "absent:not-found:inactive");
        assert!(!result.exists_ok);
        assert_eq!(result.enabled_ok, None);
        assert_eq!(result.active_ok, None);
        assert_eq!(result.failures.len(), 1);
        assert!(
            result.failures[0].contains("expected present"),
            "{result:?}"
        );
    }

    #[test]
    fn test_check_one_service_present_but_disabled_fails_when_enabled_expected_true() {
        let exp = service_expectation(true, true, true);
        let result = check_one_service("ssh", &exp, "present:disabled:active");
        assert!(result.exists_ok);
        assert_eq!(result.enabled_ok, Some(false));
        assert_eq!(result.active_ok, Some(true));
        assert!(
            result.failures.iter().any(|m| m.contains("enabled=true")),
            "{result:?}"
        );
    }

    #[test]
    fn test_check_one_service_present_but_disabled_passes_when_enabled_expected_false() {
        let exp = service_expectation(true, false, true);
        let result = check_one_service("ssh", &exp, "present:disabled:active");
        assert!(result.exists_ok);
        assert_eq!(result.enabled_ok, Some(true));
        assert_eq!(result.active_ok, Some(true));
        assert!(result.failures.is_empty(), "{result:?}");
    }

    #[test]
    fn test_check_one_service_present_inactive_passes_when_active_expected_false() {
        let exp = service_expectation(true, true, false);
        let result = check_one_service("worker", &exp, "present:enabled:inactive");
        assert!(result.exists_ok);
        assert_eq!(result.enabled_ok, Some(true));
        assert_eq!(result.active_ok, Some(true));
        assert!(result.failures.is_empty(), "{result:?}");
    }

    // ---------------------------------------------------------------------------
    // check_one_user tests
    // ---------------------------------------------------------------------------

    use super::{check_one_user, check_user_groups, is_glob_pattern, AssertUser};

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

    // ---------------------------------------------------------------------------
    // parse_dpkg_installed tests
    // ---------------------------------------------------------------------------

    use super::parse_dpkg_installed;

    #[test]
    fn test_parse_dpkg_installed_returns_only_installed_packages() {
        let raw = "git install ok installed\ntelnet deinstall ok config-files\ncurl install ok installed\n__END_DPKG__\n";
        let pkgs = parse_dpkg_installed(&mut raw.lines());
        assert_eq!(pkgs, vec!["git", "curl"]);
    }

    #[test]
    fn test_parse_dpkg_installed_stops_at_sentinel() {
        let raw = "git install ok installed\n__END_DPKG__\nextra install ok installed\n";
        let pkgs = parse_dpkg_installed(&mut raw.lines());
        assert_eq!(pkgs, vec!["git"]);
    }

    #[test]
    fn test_parse_dpkg_installed_empty_output() {
        let raw = "__END_DPKG__\n";
        let pkgs = parse_dpkg_installed(&mut raw.lines());
        assert!(pkgs.is_empty());
    }

    #[test]
    fn test_parse_dpkg_installed_excludes_half_removed() {
        // config-remains / half-removed should not count as installed
        let raw = "libssl-dev remove ok half-removed\nbash install ok installed\n__END_DPKG__\n";
        let pkgs = parse_dpkg_installed(&mut raw.lines());
        assert_eq!(pkgs, vec!["bash"]);
    }

    #[test]
    fn test_parse_dpkg_installed_excludes_config_files() {
        let raw = "telnet deinstall ok config-files\n__END_DPKG__\n";
        let pkgs = parse_dpkg_installed(&mut raw.lines());
        assert!(pkgs.is_empty());
    }

    // ---------------------------------------------------------------------------
    // build_file_probe_script tests
    // ---------------------------------------------------------------------------

    use super::{
        build_file_probe_script, build_privileged_probe_script, FILE_PROBE_HEREDOC_DELIM,
        GROUPS_PROBE_HEREDOC_DELIM, USERS_PROBE_HEREDOC_DELIM,
    };

    #[test]
    fn test_build_file_probe_script_starts_with_set_e() {
        let script = build_file_probe_script(&["/etc/hosts"]);
        assert!(
            script.starts_with("set -e\n"),
            "probe script must start with 'set -e'"
        );
    }

    #[test]
    fn test_build_file_probe_script_contains_sudo_n_wrapper() {
        let inner = build_file_probe_script(&["/etc/sudoers.d/90-botwork"]);
        let wrapped = build_privileged_probe_script(&inner, FILE_PROBE_HEREDOC_DELIM);
        assert!(
            wrapped.starts_with("sudo -n sh"),
            "outer wrapper must invoke sudo -n sh"
        );
        assert!(
            wrapped.contains(FILE_PROBE_HEREDOC_DELIM),
            "heredoc delimiter must be present"
        );
        assert!(
            wrapped.contains("/etc/sudoers.d/90-botwork"),
            "path must appear in the script body"
        );
    }

    #[test]
    fn test_build_privileged_probe_script_supports_users_probe_wrapping() {
        let inner = "set -e\ngetent passwd\nprintf '__END_PASSWD__\\n'\n";
        let wrapped = build_privileged_probe_script(inner, USERS_PROBE_HEREDOC_DELIM);
        assert!(wrapped.starts_with("sudo -n sh"));
        assert!(wrapped.contains(USERS_PROBE_HEREDOC_DELIM));
        assert!(wrapped.contains("__END_PASSWD__"));
    }

    #[test]
    fn test_build_privileged_probe_script_supports_groups_probe_wrapping() {
        let inner = "set -e\ngetent group\nprintf '__END_GROUP__\\n'\n";
        let wrapped = build_privileged_probe_script(inner, GROUPS_PROBE_HEREDOC_DELIM);
        assert!(wrapped.starts_with("sudo -n sh"));
        assert!(wrapped.contains(GROUPS_PROBE_HEREDOC_DELIM));
        assert!(wrapped.contains("__END_GROUP__"));
    }

    #[test]
    fn test_build_file_probe_script_one_block_per_path() {
        let paths = ["/a", "/b", "/c"];
        let script = build_file_probe_script(&paths);
        // Each path produces exactly one `printf 'absent\n'` or `printf 'present:...`
        // block; count the number of `printf 'absent` occurrences as a proxy.
        let absent_count = script.matches("printf 'absent").count();
        assert_eq!(
            absent_count,
            paths.len(),
            "each path must have exactly one absent branch"
        );
    }

    #[test]
    fn test_build_file_probe_script_empty_paths() {
        let script = build_file_probe_script(&[]);
        assert_eq!(
            script, "set -e\n",
            "empty paths produces only set -e header"
        );
    }

    // ---------------------------------------------------------------------------
    // parse_names_from_dump tests
    // ---------------------------------------------------------------------------

    use super::parse_names_from_dump;

    #[test]
    fn test_parse_names_from_dump_basic_passwd() {
        let raw =
            "root:x:0:0::/root:/bin/bash\nbot:x:1000:1000::/home/bot:/bin/bash\n__END_PASSWD__\n";
        let names = parse_names_from_dump(&mut raw.lines(), "__END_PASSWD__", None);
        assert_eq!(names, vec!["root", "bot"]);
    }

    #[test]
    fn test_parse_names_from_dump_basic_group() {
        let raw = "root:x:0:\nbot:x:1000:bot\n__END_GROUP__\n";
        let names = parse_names_from_dump(&mut raw.lines(), "__END_GROUP__", None);
        assert_eq!(names, vec!["root", "bot"]);
    }

    #[test]
    fn test_parse_names_from_dump_filters_installer_user() {
        // The installer user must be removed from the candidate set.
        let raw = "root:x:0:0::/root:/bin/bash\nbotforge-abc123:x:999:999::/home/botforge-abc123:/usr/sbin/nologin\nbot:x:1000:1000::/home/bot:/bin/bash\n__END_PASSWD__\n";
        let names =
            parse_names_from_dump(&mut raw.lines(), "__END_PASSWD__", Some("botforge-abc123"));
        assert_eq!(names, vec!["root", "bot"]);
    }

    #[test]
    fn test_parse_names_from_dump_filters_installer_group() {
        // The installer's same-named primary group must be removed from the candidate set.
        let raw =
            "root:x:0:\nbotforge-abc123:x:999:botforge-abc123\nbot:x:1000:bot\n__END_GROUP__\n";
        let names =
            parse_names_from_dump(&mut raw.lines(), "__END_GROUP__", Some("botforge-abc123"));
        assert_eq!(names, vec!["root", "bot"]);
    }

    #[test]
    fn test_parse_names_from_dump_filters_only_exact_installer() {
        // A genuinely leaked name that is different from the installer must NOT be filtered.
        let raw = "root:x:0:0::/root:/bin/bash\nbotforge-evil:x:998:998::/:/nologin\nbotforge-abc123:x:999:999::/:/nologin\n__END_PASSWD__\n";
        let names =
            parse_names_from_dump(&mut raw.lines(), "__END_PASSWD__", Some("botforge-abc123"));
        assert_eq!(names, vec!["root", "botforge-evil"]);
    }

    #[test]
    fn test_parse_names_from_dump_stops_at_sentinel() {
        let raw = "foo:x:1:1::/:/nologin\n__END_PASSWD__\nbar:x:2:2::/:/nologin\n";
        let names = parse_names_from_dump(&mut raw.lines(), "__END_PASSWD__", None);
        assert_eq!(names, vec!["foo"]);
    }

    #[test]
    fn test_parse_names_from_dump_no_installer_keeps_all() {
        // When installer is None, no filtering occurs.
        let raw = "botforge-abc123:x:999:999::/:/nologin\nbot:x:1000:1000::/home/bot:/bin/bash\n__END_PASSWD__\n";
        let names = parse_names_from_dump(&mut raw.lines(), "__END_PASSWD__", None);
        assert_eq!(names, vec!["botforge-abc123", "bot"]);
    }

    #[test]
    fn test_assert_registry_resolves_builtin_verbs() {
        let registry = super::registry::built_in_assert_registry();
        for verb in ["files", "users", "groups", "packages", "services"] {
            assert!(
                registry.get(verb).is_some(),
                "expected assert registry to resolve built-in verb '{verb}'"
            );
        }
    }

    #[test]
    fn test_assert_registry_preserves_dispatch_order() {
        let registry = super::registry::built_in_assert_registry();
        let verbs: Vec<&str> = registry.iter().map(|kind| kind.verb()).collect();
        assert_eq!(
            verbs,
            vec!["files", "users", "groups", "packages", "services"]
        );
    }

    mod assert_block {
        use super::super::{check_service_environment, AssertFileType, ServiceEnvironmentExpect};
        use crate::config::load_test_config;
        use std::fs;
        use tempfile::TempDir;

        // --- assert: block ---

        #[test]
        fn test_load_test_config_assert_absent_is_none() {
            let repo = TempDir::new().unwrap();
            fs::write(
                repo.path().join("test.yaml"),
                "type: botforge/test\nname: test\nsteps: []\n",
            )
            .unwrap();
            let config = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap();
            assert!(config.assert.is_none(), "assert should default to None");
        }

        #[test]
        fn test_load_test_config_assert_files_parses_exists_true() {
            let repo = TempDir::new().unwrap();
            fs::write(
                repo.path().join("test.yaml"),
                r#"
type: botforge/test
name: test
steps: []
assert:
  files:
    /usr/local/bin/tool:
      exists: true
      filetype: file
      owner: root
      group: root
      mode: "0755"
"#,
            )
            .unwrap();
            let config = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap();
            let assert_block = config.assert.unwrap();
            let entry = assert_block.files.get("/usr/local/bin/tool").unwrap();
            assert!(entry.exists);
            assert_eq!(entry.filetype, Some(AssertFileType::File));
            assert_eq!(entry.owner.as_deref(), Some("root"));
            assert_eq!(entry.group.as_deref(), Some("root"));
            assert_eq!(entry.mode.as_deref(), Some("0755"));
        }

        #[test]
        fn test_load_test_config_assert_files_parses_exists_false() {
            let repo = TempDir::new().unwrap();
            fs::write(
                repo.path().join("test.yaml"),
                r#"
type: botforge/test
name: test
steps: []
assert:
  files:
    /tmp/should-be-gone:
      exists: false
"#,
            )
            .unwrap();
            let config = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap();
            let assert_block = config.assert.unwrap();
            let entry = assert_block.files.get("/tmp/should-be-gone").unwrap();
            assert!(!entry.exists);
            assert!(entry.filetype.is_none());
            assert!(entry.owner.is_none());
            assert!(entry.group.is_none());
            assert!(entry.mode.is_none());
        }

        #[test]
        fn test_load_test_config_assert_files_rejects_exists_false_with_attributes() {
            let repo = TempDir::new().unwrap();
            fs::write(
                repo.path().join("test.yaml"),
                r#"
type: botforge/test
name: test
steps: []
assert:
  files:
    /some/path:
      exists: false
      mode: "0755"
"#,
            )
            .unwrap();
            let err = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap_err();
            let msg = format!("{err:#}");
            assert!(
                msg.contains("exists: false") || msg.contains("attribute"),
                "error should mention exists:false and attributes: {msg}"
            );
        }

        #[test]
        fn test_load_test_config_assert_files_rejects_exists_false_with_default_permissions() {
            let repo = TempDir::new().unwrap();
            fs::write(
                repo.path().join("test.yaml"),
                r#"
type: botforge/test
name: test
steps: []
assert:
  files:
    /some/path:
      exists: false
      default-permissions: false
"#,
            )
            .unwrap();
            let err = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap_err();
            let msg = format!("{err:#}");
            assert!(
                msg.contains("exists: false")
                    || msg.contains("attribute")
                    || msg.contains("default-permissions"),
                "error should mention exists:false and default-permissions: {msg}"
            );
        }

        #[test]
        fn test_load_test_config_assert_files_default_permissions_false_parses() {
            let repo = TempDir::new().unwrap();
            fs::write(
                repo.path().join("test.yaml"),
                r#"
type: botforge/test
name: test
steps: []
assert:
  files:
    /scratch/x:
      default-permissions: false
    /run/secret:
      default-permissions: false
      mode: "0600"
"#,
            )
            .unwrap();
            let config = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap();
            let assert_block = config.assert.unwrap();
            let scratch = assert_block.files.get("/scratch/x").unwrap();
            assert!(scratch.exists);
            assert_eq!(scratch.default_permissions, Some(false));
            assert!(scratch.mode.is_none());
            let secret = assert_block.files.get("/run/secret").unwrap();
            assert!(secret.exists);
            assert_eq!(secret.default_permissions, Some(false));
            assert_eq!(secret.mode.as_deref(), Some("0600"));
        }

        #[test]
        fn test_load_test_config_assert_files_default_entry_has_default_permissions_none() {
            // An empty entry {} should have default_permissions = None (effective: true).
            let repo = TempDir::new().unwrap();
            fs::write(
                repo.path().join("test.yaml"),
                r#"
type: botforge/test
name: test
steps: []
assert:
  files:
    /etc/botwork/bootstrap.yaml: {}
"#,
            )
            .unwrap();
            let config = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap();
            let assert_block = config.assert.unwrap();
            let entry = assert_block
                .files
                .get("/etc/botwork/bootstrap.yaml")
                .unwrap();
            assert!(entry.exists);
            assert_eq!(entry.default_permissions, None); // None → effective true
            assert!(entry.owner.is_none());
            assert!(entry.group.is_none());
            assert!(entry.mode.is_none());
        }

        #[test]
        fn test_load_test_config_assert_files_rejects_relative_path() {
            let repo = TempDir::new().unwrap();
            fs::write(
                repo.path().join("test.yaml"),
                r#"
type: botforge/test
name: test
steps: []
assert:
  files:
    relative/path:
      exists: true
"#,
            )
            .unwrap();
            let err = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap_err();
            let msg = format!("{err:#}");
            assert!(
                msg.contains("absolute"),
                "error should mention absolute path: {msg}"
            );
        }

        #[test]
        fn test_load_test_config_assert_files_rejects_invalid_mode() {
            let repo = TempDir::new().unwrap();
            fs::write(
                repo.path().join("test.yaml"),
                r#"
type: botforge/test
name: test
steps: []
assert:
  files:
    /some/path:
      exists: true
      mode: "rwxr-xr-x"
"#,
            )
            .unwrap();
            let err = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap_err();
            let msg = format!("{err:#}");
            assert!(
                msg.contains("mode") || msg.contains("octal"),
                "error should mention mode: {msg}"
            );
        }

        #[test]
        fn test_load_test_config_assert_files_multiple_entries() {
            let repo = TempDir::new().unwrap();
            fs::write(
                repo.path().join("test.yaml"),
                r#"
type: botforge/test
name: test
steps: []
assert:
  files:
    /usr/bin/tool:
      exists: true
      filetype: file
    /var/data:
      exists: true
      filetype: directory
    /tmp/gone.tar:
      exists: false
"#,
            )
            .unwrap();
            let config = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap();
            let assert_block = config.assert.unwrap();
            assert_eq!(assert_block.files.len(), 3);
            assert_eq!(
                assert_block.files.get("/usr/bin/tool").unwrap().filetype,
                Some(AssertFileType::File)
            );
            assert_eq!(
                assert_block.files.get("/var/data").unwrap().filetype,
                Some(AssertFileType::Directory)
            );
            assert!(!assert_block.files.get("/tmp/gone.tar").unwrap().exists);
        }

        #[test]
        fn test_load_test_config_assert_users_basic() {
            let repo = TempDir::new().unwrap();
            fs::write(
                repo.path().join("test.yaml"),
                r#"
type: botforge/test
name: test
steps: []
assert:
  users:
    bot:
      exists: true
      shell: /bin/bash
      groups: [bot, docker]
    mallory:
      exists: false
"#,
            )
            .unwrap();
            let config = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap();
            let assert_block = config.assert.unwrap();
            assert_eq!(assert_block.users.len(), 2);
            let bot = assert_block.users.get("bot").unwrap();
            assert!(bot.exists);
            assert_eq!(bot.shell.as_deref(), Some("/bin/bash"));
            assert_eq!(bot.groups, vec!["bot", "docker"]);
            let mallory = assert_block.users.get("mallory").unwrap();
            assert!(!mallory.exists);
        }

        #[test]
        fn test_load_test_config_assert_users_pattern_negative() {
            let repo = TempDir::new().unwrap();
            fs::write(
                repo.path().join("test.yaml"),
                r#"
type: botforge/test
name: test
steps: []
assert:
  users:
    "botforge-*":
      exists: false
"#,
            )
            .unwrap();
            let config = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap();
            let assert_block = config.assert.unwrap();
            let pat = assert_block.users.get("botforge-*").unwrap();
            assert!(!pat.exists);
        }

        #[test]
        fn test_load_test_config_assert_users_rejects_attrs_with_exists_false() {
            let repo = TempDir::new().unwrap();
            fs::write(
                repo.path().join("test.yaml"),
                r#"
type: botforge/test
name: test
steps: []
assert:
  users:
    mallory:
      exists: false
      shell: /bin/bash
"#,
            )
            .unwrap();
            let err = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap_err();
            let msg = format!("{err:#}");
            assert!(
                msg.contains("shell") || msg.contains("exists: false"),
                "error should mention shell/exists: {msg}"
            );
        }

        #[test]
        fn test_load_test_config_assert_groups_basic() {
            let repo = TempDir::new().unwrap();
            fs::write(
                repo.path().join("test.yaml"),
                r#"
type: botforge/test
name: test
steps: []
assert:
  groups:
    docker:
      exists: true
    evilusers:
      exists: false
"#,
            )
            .unwrap();
            let config = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap();
            let assert_block = config.assert.unwrap();
            assert_eq!(assert_block.groups.len(), 2);
            assert!(assert_block.groups.get("docker").unwrap().exists);
            assert!(!assert_block.groups.get("evilusers").unwrap().exists);
        }

        #[test]
        fn test_load_test_config_assert_users_and_groups_combined() {
            let repo = TempDir::new().unwrap();
            fs::write(
                repo.path().join("test.yaml"),
                r#"
type: botforge/test
name: test
steps: []
assert:
  users:
    bot:
      exists: true
      shell: /bin/bash
  groups:
    docker:
      exists: true
"#,
            )
            .unwrap();
            let config = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap();
            let assert_block = config.assert.unwrap();
            assert_eq!(assert_block.users.len(), 1);
            assert_eq!(assert_block.groups.len(), 1);
        }

        #[test]
        fn test_load_test_config_assert_packages_basic() {
            let repo = TempDir::new().unwrap();
            fs::write(
                repo.path().join("test.yaml"),
                r#"
type: botforge/test
name: test
steps: []
assert:
  packages:
    git:
      installed: true
    telnet:
      installed: false
"#,
            )
            .unwrap();
            let config = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap();
            let assert_block = config.assert.unwrap();
            assert_eq!(assert_block.packages.len(), 2);
            assert!(assert_block.packages.get("git").unwrap().installed);
            assert!(!assert_block.packages.get("telnet").unwrap().installed);
        }

        #[test]
        fn test_load_test_config_assert_packages_pattern_negative() {
            let repo = TempDir::new().unwrap();
            fs::write(
                repo.path().join("test.yaml"),
                r#"
type: botforge/test
name: test
steps: []
assert:
  packages:
    "*-dev":
      installed: false
"#,
            )
            .unwrap();
            let config = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap();
            let assert_block = config.assert.unwrap();
            let pat = assert_block.packages.get("*-dev").unwrap();
            assert!(!pat.installed);
        }

        #[test]
        fn test_load_test_config_assert_packages_pattern_positive() {
            let repo = TempDir::new().unwrap();
            fs::write(
                repo.path().join("test.yaml"),
                r#"
type: botforge/test
name: test
steps: []
assert:
  packages:
    "linux-image-*":
      installed: true
"#,
            )
            .unwrap();
            let config = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap();
            let assert_block = config.assert.unwrap();
            let pat = assert_block.packages.get("linux-image-*").unwrap();
            assert!(pat.installed);
        }

        #[test]
        fn test_load_test_config_assert_packages_rejects_unknown_field() {
            let repo = TempDir::new().unwrap();
            fs::write(
                repo.path().join("test.yaml"),
                r#"
type: botforge/test
name: test
steps: []
assert:
  packages:
    git:
      installed: true
      version: "2.40"
"#,
            )
            .unwrap();
            let err = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap_err();
            let msg = format!("{err:#}");
            assert!(
                msg.contains("version") || msg.contains("unknown field"),
                "error should mention unknown field: {msg}"
            );
        }

        // ---------------------------------------------------------------------------
        // assert.services: tests
        // ---------------------------------------------------------------------------

        #[test]
        fn test_load_test_config_assert_services_basic() {
            let repo = TempDir::new().unwrap();
            fs::write(
                repo.path().join("test.yaml"),
                r#"
type: botforge/test
name: test
steps: []
assert:
  services:
    ssh:
      exists: true
      enabled: true
      active: true
    nginx:
      enabled: false
    botwork-api: {}
"#,
            )
            .unwrap();
            let config = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap();
            let assert_block = config.assert.unwrap();
            assert_eq!(assert_block.services.len(), 3);
            let ssh = assert_block.services.get("ssh").unwrap();
            assert!(ssh.exists);
            assert!(ssh.enabled);
            assert!(ssh.active);
            let nginx = assert_block.services.get("nginx").unwrap();
            assert!(nginx.exists);
            assert!(!nginx.enabled);
            assert!(nginx.active);
            let api = assert_block.services.get("botwork-api").unwrap();
            assert!(api.exists);
            assert!(api.enabled);
            assert!(api.active);
        }

        #[test]
        fn test_load_test_config_assert_services_partial_fields() {
            let repo = TempDir::new().unwrap();
            fs::write(
                repo.path().join("test.yaml"),
                r#"
type: botforge/test
name: test
steps: []
assert:
  services:
    cron:
      active: true
"#,
            )
            .unwrap();
            let config = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap();
            let assert_block = config.assert.unwrap();
            let cron = assert_block.services.get("cron").unwrap();
            assert!(cron.exists);
            assert!(cron.enabled);
            assert!(cron.active);
        }

        #[test]
        fn test_load_test_config_assert_services_exists_false() {
            let repo = TempDir::new().unwrap();
            fs::write(
                repo.path().join("test.yaml"),
                r#"
type: botforge/test
name: test
steps: []
assert:
  services:
    retired:
      exists: false
"#,
            )
            .unwrap();
            let config = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap();
            let assert_block = config.assert.unwrap();
            let retired = assert_block.services.get("retired").unwrap();
            assert!(!retired.exists);
            assert!(retired.enabled);
            assert!(retired.active);
        }

        #[test]
        fn test_load_test_config_assert_services_exists_false_rejects_enabled_or_active() {
            let repo = TempDir::new().unwrap();
            fs::write(
                repo.path().join("test.yaml"),
                r#"
type: botforge/test
name: test
steps: []
assert:
  services:
    retired:
      exists: false
      enabled: true
"#,
            )
            .unwrap();
            let err = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap_err();
            let msg = format!("{err:#}");
            assert!(
                msg.contains("exists: false"),
                "error should mention exists:false: {msg}"
            );
        }

        #[test]
        fn test_load_test_config_assert_services_bare_defaults_to_all_true() {
            let repo = TempDir::new().unwrap();
            fs::write(
                repo.path().join("test.yaml"),
                r#"
type: botforge/test
name: test
steps: []
assert:
  services:
    foo: {}
"#,
            )
            .unwrap();
            let config = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap();
            let assert_block = config.assert.unwrap();
            let foo = assert_block.services.get("foo").unwrap();
            assert!(foo.exists);
            assert!(foo.enabled);
            assert!(foo.active);
        }

        #[test]
        fn test_load_test_config_assert_services_environment_parses() {
            let repo = TempDir::new().unwrap();
            fs::write(
                repo.path().join("test.yaml"),
                r#"
type: botforge/test
name: test
steps: []
assert:
  services:
    botwork-launcher.service:
      environment:
        contains:
          - "BOTWORK_LAUNCHER_DEFAULT_NETWORK=botwork-plugin"
        not_contains:
          - "DEBUG=1"
"#,
            )
            .unwrap();
            let config = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap();
            let assert_block = config.assert.unwrap();
            let svc = assert_block
                .services
                .get("botwork-launcher.service")
                .unwrap();
            assert!(svc.exists);
            let env = svc.environment.as_ref().unwrap();
            assert_eq!(
                env.contains,
                vec!["BOTWORK_LAUNCHER_DEFAULT_NETWORK=botwork-plugin"]
            );
            assert_eq!(env.not_contains, vec!["DEBUG=1"]);
        }

        #[test]
        fn test_load_test_config_assert_services_exists_false_rejects_environment() {
            let repo = TempDir::new().unwrap();
            fs::write(
                repo.path().join("test.yaml"),
                r#"
type: botforge/test
name: test
steps: []
assert:
  services:
    retired:
      exists: false
      environment:
        contains: ["SOME=VAR"]
"#,
            )
            .unwrap();
            let err = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap_err();
            let msg = format!("{err:#}");
            assert!(
                msg.contains("exists: false"),
                "error should mention exists:false: {msg}"
            );
        }

        #[test]
        fn test_check_service_environment_contains_present_passes() {
            let expect = ServiceEnvironmentExpect {
                contains: vec!["BOTWORK_LAUNCHER_DEFAULT_NETWORK=botwork-plugin".to_string()],
                not_contains: vec![],
            };
            let output = "Environment=BOTWORK_LAUNCHER_DEFAULT_NETWORK=botwork-plugin FOO=bar";
            let results = check_service_environment("svc", &expect, output);
            assert_eq!(results.len(), 1);
            assert!(results[0].1, "contains match should pass: {:?}", results[0]);
        }

        #[test]
        fn test_check_service_environment_contains_missing_fails() {
            let expect = ServiceEnvironmentExpect {
                contains: vec!["BOTWORK_LAUNCHER_DEFAULT_NETWORK=botwork-plugin".to_string()],
                not_contains: vec![],
            };
            let output = "Environment=FOO=bar";
            let results = check_service_environment("svc", &expect, output);
            assert_eq!(results.len(), 1);
            assert!(!results[0].1, "contains miss should fail: {:?}", results[0]);
        }

        #[test]
        fn test_check_service_environment_not_contains_absent_passes() {
            let expect = ServiceEnvironmentExpect {
                contains: vec![],
                not_contains: vec!["DEBUG=1".to_string()],
            };
            let output = "Environment=BOTWORK_LAUNCHER_DEFAULT_NETWORK=botwork-plugin";
            let results = check_service_environment("svc", &expect, output);
            assert_eq!(results.len(), 1);
            assert!(
                results[0].1,
                "not_contains (absent) should pass: {:?}",
                results[0]
            );
        }

        #[test]
        fn test_check_service_environment_not_contains_present_fails() {
            let expect = ServiceEnvironmentExpect {
                contains: vec![],
                not_contains: vec!["DEBUG=1".to_string()],
            };
            let output = "Environment=DEBUG=1 BOTWORK_LAUNCHER_DEFAULT_NETWORK=botwork-plugin";
            let results = check_service_environment("svc", &expect, output);
            assert_eq!(results.len(), 1);
            assert!(
                !results[0].1,
                "not_contains (present) should fail: {:?}",
                results[0]
            );
        }

        #[test]
        fn test_check_service_environment_empty_lists_pass() {
            let expect = ServiceEnvironmentExpect {
                contains: vec![],
                not_contains: vec![],
            };
            let output = "Environment=ANYTHING=1";
            let results = check_service_environment("svc", &expect, output);
            assert!(results.is_empty(), "empty lists → no checks: {:?}", results);
        }

        #[test]
        fn test_check_service_environment_multiple_substrings() {
            let expect = ServiceEnvironmentExpect {
                contains: vec!["FOO=1".to_string(), "BAR=2".to_string()],
                not_contains: vec!["SECRET=x".to_string()],
            };
            let output = "Environment=FOO=1 BAR=2 BAZ=3";
            let results = check_service_environment("svc", &expect, output);
            assert_eq!(results.len(), 3);
            assert!(results[0].1, "FOO=1 should be found");
            assert!(results[1].1, "BAR=2 should be found");
            assert!(results[2].1, "SECRET=x should be absent");
        }

        #[test]
        fn test_check_service_environment_label_format() {
            let expect = ServiceEnvironmentExpect {
                contains: vec!["FOO=1".to_string()],
                not_contains: vec!["BAR=2".to_string()],
            };
            let results = check_service_environment("my.service", &expect, "Environment=FOO=1");
            assert_eq!(results.len(), 2);
            assert!(
                results[0].0.contains("contains"),
                "label should contain 'contains': {}",
                results[0].0
            );
            assert!(
                results[1].0.contains("not_contains"),
                "label should contain 'not_contains': {}",
                results[1].0
            );
        }

        #[test]
        fn test_load_test_config_assert_services_rejects_unknown_field() {
            let repo = TempDir::new().unwrap();
            fs::write(
                repo.path().join("test.yaml"),
                r#"
type: botforge/test
name: test
steps: []
assert:
  services:
    ssh:
      enabled: true
      version: "3.0"
"#,
            )
            .unwrap();
            let err = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap_err();
            let msg = format!("{err:#}");
            assert!(
                msg.contains("version") || msg.contains("unknown field"),
                "error should mention unknown field: {msg}"
            );
        }

        #[test]
        fn test_load_test_config_assert_unknown_verb_is_error() {
            // Since parse_assert_block now retains unknown verbs in plugin_asserts
            // (to support plugin-provided assert capabilities), parsing succeeds.
            // The unknown verb is stored and will error at run time if no plugin
            // provides assert/foo.
            let repo = TempDir::new().unwrap();
            fs::write(
                repo.path().join("test.yaml"),
                r#"
type: botforge/test
name: test
steps: []
assert:
  foo:
    bar:
      exists: true
"#,
            )
            .unwrap();
            let config = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap();
            let assert_block = config.assert.unwrap();
            // Unknown verb is retained in plugin_asserts, not in built-in fields.
            assert!(
                assert_block.plugin_asserts.contains_key("foo"),
                "unknown verb 'foo' should be retained in plugin_asserts"
            );
            assert!(
                assert_block.files.is_empty()
                    && assert_block.users.is_empty()
                    && assert_block.groups.is_empty()
                    && assert_block.packages.is_empty()
                    && assert_block.services.is_empty(),
                "built-in assert fields should be empty for unknown-verb-only block"
            );
        }
    }
}
