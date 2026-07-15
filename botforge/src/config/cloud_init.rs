use anyhow::{Context, Result};
use serde_yaml::Value;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::util::{command_exists, create_temp_dir};

/// Validate a `cloud_init:` mapping at config-load time.
///
/// Two classes of violation are hard-rejected:
///
/// **Ingress protection** — `cloud_init:` must not name host sources.  A
/// `write_files:` entry with a `source:` field would instruct cloud-init to pull
/// content from a URL or local path; that is an ingress vector outside the
/// shasset-only host→build boundary and is therefore rejected.  Inline
/// `content:` fields (which carry values, not paths) are allowed.
///
/// **Harness protection** — settings that would lock botforge out of the runner
/// VM are rejected.  Currently this guards against `ssh_pwauth: false`, which
/// (when combined with certain sshd configurations) could break key-based login.
pub(crate) fn validate_cloud_init_fragment(
    cloud_init: &serde_yaml::Mapping,
    path: &Path,
) -> Result<()> {
    // Ingress guard: write_files entries must not have a source: field.
    let write_files_key = Value::String("write_files".to_string());
    if let Some(Value::Sequence(entries)) = cloud_init.get(&write_files_key) {
        for entry in entries {
            if let Value::Mapping(entry_map) = entry {
                if entry_map.contains_key(Value::String("source".to_string())) {
                    anyhow::bail!(
                        "cloud_init.write_files: 'source:' is not allowed in {} \
                         (ingress guard: use 'files:' for host→guest file transfer; \
                         inline 'content:' is allowed)",
                        path.display()
                    );
                }
            }
        }
    }
    // Harness guard: reject ssh_pwauth: false which can break key-based login
    // in combination with certain sshd configurations.
    let ssh_pwauth_key = Value::String("ssh_pwauth".to_string());
    if let Some(Value::Bool(false)) = cloud_init.get(&ssh_pwauth_key) {
        anyhow::bail!(
            "cloud_init.ssh_pwauth: false is not allowed in {} \
             (harness guard: setting ssh_pwauth false may break botforge's key-based \
             SSH access to the runner VM)",
            path.display()
        );
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CloudInitSchemaMode {
    Off,
    Warn,
    Strict,
}

impl CloudInitSchemaMode {
    fn from_env() -> Self {
        let raw = std::env::var("BOTFORGE_CLOUD_INIT_SCHEMA")
            .unwrap_or_else(|_| "warn".to_string())
            .trim()
            .to_ascii_lowercase();
        match raw.as_str() {
            "off" => Self::Off,
            "warn" => Self::Warn,
            "strict" => Self::Strict,
            _ => Self::Warn,
        }
    }
}

enum CloudInitSchemaCheck {
    Pass,
    MissingBinary,
    InvocationFailed(String),
    Invalid(String),
}

pub(crate) fn validate_cloud_init_schema_fragment(
    cloud_init: &serde_yaml::Mapping,
    path: &Path,
) -> Result<()> {
    let mode = cloud_init_schema_mode();
    if matches!(mode, CloudInitSchemaMode::Off) {
        return Ok(());
    }

    let rendered = render_cloud_init_schema_document(cloud_init)
        .with_context(|| format!("failed to render cloud_init document in {}", path.display()))?;
    match run_cloud_init_schema_check(&rendered) {
        CloudInitSchemaCheck::Pass | CloudInitSchemaCheck::MissingBinary => Ok(()),
        CloudInitSchemaCheck::InvocationFailed(details) => {
            emit_cloud_init_schema_warning(&format!(
                "cloud-init schema pre-validation skipped for {}: {}",
                path.display(),
                details
            ));
            Ok(())
        }
        CloudInitSchemaCheck::Invalid(details) => match mode {
            CloudInitSchemaMode::Warn => {
                emit_cloud_init_schema_warning(&format!(
                    "cloud-init schema pre-validation reported issues for {}:\n{}",
                    path.display(),
                    details
                ));
                Ok(())
            }
            CloudInitSchemaMode::Strict => anyhow::bail!(
                "cloud-init schema pre-validation failed for {}:\n{}",
                path.display(),
                details
            ),
            CloudInitSchemaMode::Off => Ok(()),
        },
    }
}

fn render_cloud_init_schema_document(cloud_init: &serde_yaml::Mapping) -> Result<String> {
    // Deliberately validate the user-provided fragment (with `#cloud-config` header)
    // rather than botforge's merged installer user-data. This keeps pre-validation
    // focused on user-authored keys while preserving authoritative runtime merge
    // behavior in `iso::render_user_data`.
    let yaml = serde_yaml::to_string(&Value::Mapping(cloud_init.clone()))
        .context("failed to serialize cloud_init fragment as YAML")?;
    Ok(format!("#cloud-config\n{yaml}"))
}

fn cloud_init_schema_mode() -> CloudInitSchemaMode {
    #[cfg(test)]
    {
        if let Some(mode) = cloud_init_schema_mode_override() {
            return mode;
        }
    }
    CloudInitSchemaMode::from_env()
}

fn run_cloud_init_schema_check(document: &str) -> CloudInitSchemaCheck {
    #[cfg(test)]
    {
        if let Some(result) = run_cloud_init_schema_check_override(document) {
            return result;
        }
    }
    let Some(cloud_init_bin) = locate_cloud_init_binary() else {
        return CloudInitSchemaCheck::MissingBinary;
    };
    match run_cloud_init_schema_via_stdin(&cloud_init_bin, document) {
        Ok(CloudInitSchemaCheck::Pass) => CloudInitSchemaCheck::Pass,
        Ok(_) => match run_cloud_init_schema_via_temp_file(&cloud_init_bin, document) {
            Ok(result) => result,
            Err(err) => CloudInitSchemaCheck::InvocationFailed(err),
        },
        Err(err) => CloudInitSchemaCheck::InvocationFailed(err),
    }
}

fn locate_cloud_init_binary() -> Option<PathBuf> {
    if command_exists("cloud-init") {
        Some(PathBuf::from("cloud-init"))
    } else {
        None
    }
}

fn run_cloud_init_schema_via_stdin(
    cloud_init_bin: &Path,
    document: &str,
) -> std::result::Result<CloudInitSchemaCheck, String> {
    let mut child = Command::new(cloud_init_bin)
        .arg("schema")
        .arg("--config-file")
        .arg("-")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| format!("failed to execute {}: {err}", cloud_init_bin.display()))?;
    child
        .stdin
        .as_mut()
        .ok_or_else(|| "failed to open stdin for cloud-init schema".to_string())?
        .write_all(document.as_bytes())
        .map_err(|err| format!("failed to write cloud-init schema stdin: {err}"))?;
    let output = child
        .wait_with_output()
        .map_err(|err| format!("failed to wait for cloud-init schema: {err}"))?;
    Ok(parse_cloud_init_schema_output(output))
}

fn run_cloud_init_schema_via_temp_file(
    cloud_init_bin: &Path,
    document: &str,
) -> std::result::Result<CloudInitSchemaCheck, String> {
    let temp_dir = create_temp_dir("botforge-cloud-init-schema")
        .map_err(|err| format!("failed to create temp dir for cloud-init schema: {err:#}"))?;
    let config_path = temp_dir.join("cloud-init.yaml");
    std::fs::write(&config_path, document).map_err(|err| {
        format!(
            "failed to write temp cloud-init config {}: {err}",
            config_path.display()
        )
    })?;
    let output = Command::new(cloud_init_bin)
        .arg("schema")
        .arg("--config-file")
        .arg(&config_path)
        .output()
        .map_err(|err| format!("failed to execute {}: {err}", cloud_init_bin.display()));
    let cleanup_result = std::fs::remove_dir_all(&temp_dir);
    if let Err(err) = cleanup_result {
        emit_cloud_init_schema_warning(&format!(
            "failed to remove temp dir {} after cloud-init schema pre-validation: {}",
            temp_dir.display(),
            err
        ));
    }
    output.map(parse_cloud_init_schema_output)
}

fn parse_cloud_init_schema_output(output: std::process::Output) -> CloudInitSchemaCheck {
    if output.status.success() {
        return CloudInitSchemaCheck::Pass;
    }
    CloudInitSchemaCheck::Invalid(format_cloud_init_schema_message(&output))
}

fn format_cloud_init_schema_message(output: &std::process::Output) -> String {
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    match (stdout.is_empty(), stderr.is_empty()) {
        (true, true) => format!("cloud-init schema exited with {}", output.status),
        (true, false) => stderr,
        (false, true) => stdout,
        (false, false) => format!("{stderr}\n{stdout}"),
    }
}

fn emit_cloud_init_schema_warning(message: &str) {
    #[cfg(test)]
    {
        if capture_cloud_init_schema_warning_for_test(message) {
            return;
        }
    }
    eprintln!("warning: {message}");
}

/// Deep-merge two cloud-config mappings under botforge's merge semantics:
///
/// - **Sequences**: base first, then overlay (botforge-first concatenation).
/// - **Mappings**: recurse.
/// - **Scalars**: overlay wins.
pub(crate) fn merge_cloud_init_mappings(
    base: serde_yaml::Mapping,
    overlay: serde_yaml::Mapping,
) -> serde_yaml::Mapping {
    let mut result = base;
    for (key, overlay_val) in overlay {
        match result.get_mut(&key) {
            None => {
                result.insert(key, overlay_val);
            }
            Some(base_val) => match (base_val, overlay_val) {
                (Value::Sequence(base_seq), Value::Sequence(overlay_seq)) => {
                    base_seq.extend(overlay_seq);
                }
                (Value::Mapping(base_map), Value::Mapping(overlay_map)) => {
                    *base_map = merge_cloud_init_mappings(base_map.clone(), overlay_map);
                }
                (base_val, overlay_val) => {
                    *base_val = overlay_val;
                }
            },
        }
    }
    result
}

#[cfg(test)]
type CloudInitSchemaCheckFn = fn(&str) -> CloudInitSchemaCheck;

#[cfg(test)]
thread_local! {
    static CLOUD_INIT_SCHEMA_MODE_OVERRIDE: std::cell::RefCell<Option<CloudInitSchemaMode>> =
        const { std::cell::RefCell::new(None) };
    static CLOUD_INIT_SCHEMA_CHECK_OVERRIDE: std::cell::RefCell<Option<CloudInitSchemaCheckFn>> =
        const { std::cell::RefCell::new(None) };
    static CLOUD_INIT_SCHEMA_WARNINGS: std::cell::RefCell<Option<Vec<String>>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
fn cloud_init_schema_mode_override() -> Option<CloudInitSchemaMode> {
    CLOUD_INIT_SCHEMA_MODE_OVERRIDE.with(|slot| *slot.borrow())
}

#[cfg(test)]
fn run_cloud_init_schema_check_override(document: &str) -> Option<CloudInitSchemaCheck> {
    CLOUD_INIT_SCHEMA_CHECK_OVERRIDE
        .with(|slot| slot.borrow().as_ref().copied())
        .map(|check| check(document))
}

#[cfg(test)]
fn capture_cloud_init_schema_warning_for_test(message: &str) -> bool {
    CLOUD_INIT_SCHEMA_WARNINGS.with(|slot| {
        let mut slot = slot.borrow_mut();
        if let Some(ref mut warnings) = *slot {
            warnings.push(message.to_string());
            true
        } else {
            false
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{load_build_config, load_test_config};
    use tempfile::TempDir;

    fn write_build_config(repo: &TempDir, name: &str, content: &str) {
        std::fs::write(repo.path().join(name), content).unwrap();
    }

    fn write_test_config(repo: &TempDir, name: &str, content: &str) {
        std::fs::write(repo.path().join(name), content).unwrap();
    }

    fn with_cloud_init_schema_mode<T>(mode: CloudInitSchemaMode, f: impl FnOnce() -> T) -> T {
        CLOUD_INIT_SCHEMA_MODE_OVERRIDE.with(|slot| *slot.borrow_mut() = Some(mode));
        let result = f();
        CLOUD_INIT_SCHEMA_MODE_OVERRIDE.with(|slot| *slot.borrow_mut() = None);
        result
    }

    fn with_cloud_init_schema_check<T>(
        check: fn(&str) -> CloudInitSchemaCheck,
        f: impl FnOnce() -> T,
    ) -> T {
        CLOUD_INIT_SCHEMA_CHECK_OVERRIDE.with(|slot| *slot.borrow_mut() = Some(check));
        let result = f();
        CLOUD_INIT_SCHEMA_CHECK_OVERRIDE.with(|slot| *slot.borrow_mut() = None);
        result
    }

    fn with_warning_capture<T>(f: impl FnOnce() -> T) -> (T, Vec<String>) {
        CLOUD_INIT_SCHEMA_WARNINGS.with(|slot| *slot.borrow_mut() = Some(Vec::new()));
        let result = f();
        let warnings = CLOUD_INIT_SCHEMA_WARNINGS
            .with(|slot| slot.borrow_mut().take())
            .unwrap_or_default();
        (result, warnings)
    }

    fn schema_pass(_: &str) -> CloudInitSchemaCheck {
        CloudInitSchemaCheck::Pass
    }

    fn schema_missing(_: &str) -> CloudInitSchemaCheck {
        CloudInitSchemaCheck::MissingBinary
    }

    fn schema_invalid(_: &str) -> CloudInitSchemaCheck {
        CloudInitSchemaCheck::Invalid("invalid cloud-config key".to_string())
    }

    // -----------------------------------------------------------------
    // cloud_init field tests (replaced bootcmd)
    // -----------------------------------------------------------------

    #[test]
    fn test_load_build_config_cloud_init_absent_is_none() {
        let repo = TempDir::new().unwrap();
        write_build_config(
        &repo,
        "build.yaml",
        "type: botforge/build\nname: build\nimage: \"@base\"\noutput: \"out.qcow2\"\nsteps: []\n",
    );
        let config = load_build_config(repo.path(), &repo.path().join("build.yaml")).unwrap();
        assert!(
            config.cloud_init.is_none(),
            "absent cloud_init must deserialize as None"
        );
    }

    #[test]
    fn test_load_build_config_cloud_init_bootcmd_string_entries() {
        let repo = TempDir::new().unwrap();
        write_build_config(
            &repo,
            "build.yaml",
            r#"
type: botforge/build
name: build
image: "@base"
output: "out.qcow2"
steps: []
cloud_init:
  bootcmd:
    - echo hello
    - echo world
"#,
        );
        let config = load_build_config(repo.path(), &repo.path().join("build.yaml")).unwrap();
        let ci = config.cloud_init.expect("cloud_init must be Some");
        let bootcmd = ci
            .get(serde_yaml::Value::String("bootcmd".to_string()))
            .expect("bootcmd must be present in cloud_init");
        let entries = bootcmd.as_sequence().expect("bootcmd must be a sequence");
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].as_str(), Some("echo hello"));
        assert_eq!(entries[1].as_str(), Some("echo world"));
    }

    #[test]
    fn test_load_build_config_cloud_init_bootcmd_exec_entry() {
        let repo = TempDir::new().unwrap();
        write_build_config(
            &repo,
            "build.yaml",
            r#"
type: botforge/build
name: build
image: "@base"
output: "out.qcow2"
steps: []
cloud_init:
  bootcmd:
    - [ cloud-init-per, once, mask-stack, sh, -c, "systemctl mask a.service" ]
"#,
        );
        let config = load_build_config(repo.path(), &repo.path().join("build.yaml")).unwrap();
        let ci = config.cloud_init.expect("cloud_init must be Some");
        let bootcmd = ci
            .get(serde_yaml::Value::String("bootcmd".to_string()))
            .expect("bootcmd must be present");
        let entries = bootcmd.as_sequence().expect("bootcmd must be a sequence");
        assert_eq!(entries.len(), 1);
        let exec = entries[0]
            .as_sequence()
            .expect("first entry must be a sequence");
        assert_eq!(exec[0].as_str(), Some("cloud-init-per"));
        assert_eq!(exec[5].as_str(), Some("systemctl mask a.service"));
    }

    #[test]
    fn test_load_build_config_top_level_bootcmd_rejected_with_migration_error() {
        // top-level bootcmd: must produce a clear migration error.
        let repo = TempDir::new().unwrap();
        write_build_config(
            &repo,
            "build.yaml",
            r#"
type: botforge/build
name: build
image: "@base"
output: "out.qcow2"
steps: []
bootcmd:
  - echo hello
"#,
        );
        let err = load_build_config(repo.path(), &repo.path().join("build.yaml")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("cloud_init"),
            "migration error must mention cloud_init: {msg}"
        );
        assert!(
            msg.contains("bootcmd"),
            "migration error must mention bootcmd: {msg}"
        );
    }

    #[test]
    fn test_load_test_config_top_level_bootcmd_rejected_with_migration_error() {
        // top-level bootcmd: must be rejected in test docs too.
        let repo = TempDir::new().unwrap();
        write_test_config(
            &repo,
            "test.yaml",
            r#"
type: botforge/test
name: test
steps: []
bootcmd:
  - echo hello
"#,
        );
        let err = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("cloud_init"),
            "migration error must mention cloud_init: {msg}"
        );
    }

    #[test]
    fn test_load_build_config_cloud_init_packages_accepted() {
        let repo = TempDir::new().unwrap();
        write_build_config(
            &repo,
            "build.yaml",
            r#"
type: botforge/build
name: build
image: "@base"
output: "out.qcow2"
steps: []
cloud_init:
  packages:
    - curl
    - git
"#,
        );
        let config = load_build_config(repo.path(), &repo.path().join("build.yaml")).unwrap();
        let ci = config.cloud_init.expect("cloud_init must be Some");
        let pkgs = ci
            .get(serde_yaml::Value::String("packages".to_string()))
            .expect("packages must be present")
            .as_sequence()
            .expect("packages must be a sequence");
        assert_eq!(pkgs.len(), 2);
        assert_eq!(pkgs[0].as_str(), Some("curl"));
        assert_eq!(pkgs[1].as_str(), Some("git"));
    }

    #[test]
    fn test_load_test_config_cloud_init_mounts_accepted() {
        // type: botforge/test also accepts cloud_init: (motivating tmpfs-on-test example).
        let repo = TempDir::new().unwrap();
        write_test_config(
            &repo,
            "test.yaml",
            r#"
type: botforge/test
name: test
steps: []
cloud_init:
  mounts:
    - [tmpfs, /var/cache/apt, tmpfs, "size=512M", "0", "0"]
"#,
        );
        let config = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap();
        let ci = config.cloud_init.expect("cloud_init must be Some");
        let mounts = ci
            .get(serde_yaml::Value::String("mounts".to_string()))
            .expect("mounts must be present")
            .as_sequence()
            .expect("mounts must be a sequence");
        assert_eq!(mounts.len(), 1);
    }

    #[test]
    fn test_cloud_init_write_files_source_rejected_ingress_guard() {
        // write_files with source: must be rejected (ingress guard).
        let repo = TempDir::new().unwrap();
        write_build_config(
            &repo,
            "build.yaml",
            r#"
type: botforge/build
name: build
image: "@base"
output: "out.qcow2"
steps: []
cloud_init:
  write_files:
    - path: /etc/myapp.conf
      source: file:///etc/host.conf
"#,
        );
        let err = load_build_config(repo.path(), &repo.path().join("build.yaml")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("source"),
            "ingress guard error must mention source: {msg}"
        );
        assert!(
            msg.contains("write_files"),
            "ingress guard error must mention write_files: {msg}"
        );
    }

    #[test]
    fn test_cloud_init_write_files_inline_content_allowed() {
        // write_files with content: is allowed (inline value, not host-path ingress).
        let repo = TempDir::new().unwrap();
        write_build_config(
            &repo,
            "build.yaml",
            r#"
type: botforge/build
name: build
image: "@base"
output: "out.qcow2"
steps: []
cloud_init:
  write_files:
    - path: /etc/myapp.conf
      content: "key=value\n"
"#,
        );
        let config = load_build_config(repo.path(), &repo.path().join("build.yaml")).unwrap();
        assert!(config.cloud_init.is_some(), "cloud_init must be accepted");
    }

    #[test]
    fn test_cloud_init_ssh_pwauth_false_rejected_harness_guard() {
        // ssh_pwauth: false must be rejected (harness guard).
        let repo = TempDir::new().unwrap();
        write_build_config(
            &repo,
            "build.yaml",
            r#"
type: botforge/build
name: build
image: "@base"
output: "out.qcow2"
steps: []
cloud_init:
  ssh_pwauth: false
"#,
        );
        let err = load_build_config(repo.path(), &repo.path().join("build.yaml")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("ssh_pwauth"),
            "harness guard error must mention ssh_pwauth: {msg}"
        );
    }

    #[test]
    fn test_cloud_init_schema_missing_binary_is_skipped() {
        let repo = TempDir::new().unwrap();
        write_build_config(
            &repo,
            "build.yaml",
            r#"
type: botforge/build
name: build
image: "@base"
output: "out.qcow2"
steps: []
cloud_init:
  users:
    - name: app
"#,
        );
        let (result, warnings) = with_warning_capture(|| {
            with_cloud_init_schema_mode(CloudInitSchemaMode::Warn, || {
                with_cloud_init_schema_check(schema_missing, || {
                    load_build_config(repo.path(), &repo.path().join("build.yaml"))
                })
            })
        });
        let config = result.expect("missing cloud-init binary should not fail config load");
        assert!(config.cloud_init.is_some());
        assert!(
            warnings.is_empty(),
            "missing cloud-init should be skipped without warnings: {warnings:?}"
        );
    }

    #[test]
    fn test_cloud_init_schema_invalid_warn_mode_emits_warning() {
        let repo = TempDir::new().unwrap();
        write_build_config(
            &repo,
            "build.yaml",
            r#"
type: botforge/build
name: build
image: "@base"
output: "out.qcow2"
steps: []
cloud_init:
  user: typo
"#,
        );
        let (result, warnings) = with_warning_capture(|| {
            with_cloud_init_schema_mode(CloudInitSchemaMode::Warn, || {
                with_cloud_init_schema_check(schema_invalid, || {
                    load_build_config(repo.path(), &repo.path().join("build.yaml"))
                })
            })
        });
        assert!(
            result.is_ok(),
            "warn mode should not fail cloud-init schema violations"
        );
        assert_eq!(warnings.len(), 1, "warn mode must emit one warning");
        assert!(
            warnings[0].contains("invalid cloud-config key"),
            "warning must include validator message: {}",
            warnings[0]
        );
    }

    #[test]
    fn test_cloud_init_schema_invalid_strict_mode_is_hard_error() {
        let repo = TempDir::new().unwrap();
        write_test_config(
            &repo,
            "test.yaml",
            r#"
type: botforge/test
name: test
steps: []
cloud_init:
  user: typo
"#,
        );
        let err = with_cloud_init_schema_mode(CloudInitSchemaMode::Strict, || {
            with_cloud_init_schema_check(schema_invalid, || {
                load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap_err()
            })
        });
        let msg = format!("{err:#}");
        assert!(
            msg.contains("cloud-init schema pre-validation failed"),
            "strict mode must hard-fail schema violations: {msg}"
        );
    }

    #[test]
    fn test_cloud_init_schema_valid_fragment_passes_in_all_modes() {
        for mode in [
            CloudInitSchemaMode::Off,
            CloudInitSchemaMode::Warn,
            CloudInitSchemaMode::Strict,
        ] {
            let repo = TempDir::new().unwrap();
            write_test_config(
                &repo,
                "test.yaml",
                r#"
type: botforge/test
name: test
steps: []
cloud_init:
  users:
    - name: app
"#,
            );
            let (result, warnings) = with_warning_capture(|| {
                with_cloud_init_schema_mode(mode, || {
                    with_cloud_init_schema_check(schema_pass, || {
                        load_test_config(repo.path(), &repo.path().join("test.yaml"))
                    })
                })
            });
            assert!(
                result.is_ok(),
                "valid cloud_init should pass in mode {mode:?}"
            );
            assert!(
                warnings.is_empty(),
                "valid cloud_init should not warn in mode {mode:?}: {warnings:?}"
            );
        }
    }

    #[test]
    fn test_cloud_init_guards_still_hard_fail_in_strict_mode() {
        let repo = TempDir::new().unwrap();
        write_build_config(
            &repo,
            "build.yaml",
            r#"
type: botforge/build
name: build
image: "@base"
output: "out.qcow2"
steps: []
cloud_init:
  ssh_pwauth: false
"#,
        );
        let err = with_cloud_init_schema_mode(CloudInitSchemaMode::Strict, || {
            with_cloud_init_schema_check(schema_pass, || {
                load_build_config(repo.path(), &repo.path().join("build.yaml")).unwrap_err()
            })
        });
        let msg = format!("{err:#}");
        assert!(
            msg.contains("ssh_pwauth"),
            "harness guard must still hard-fail independent of schema mode: {msg}"
        );
    }

    #[test]
    fn test_load_build_config_still_rejects_invalid_files_via_pipeline() {
        let repo = TempDir::new().unwrap();
        write_build_config(
            &repo,
            "build.yaml",
            r#"
type: botforge/build
name: build
image: "@base"
output: "out.qcow2"
steps: []
files:
  - src: "asset.txt"
    dest: relative/path
"#,
        );
        let err = load_build_config(repo.path(), &repo.path().join("build.yaml")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("files"),
            "invalid files must still be rejected by loader pipeline: {msg}"
        );
    }

    #[test]
    fn test_load_test_config_still_rejects_invalid_steps_via_pipeline() {
        let repo = TempDir::new().unwrap();
        write_test_config(
            &repo,
            "test.yaml",
            r#"
type: botforge/test
name: test
steps:
  - on: host
    name: host-step
    run: echo hi
"#,
        );
        let err = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("ports"),
            "invalid host step without ports must be rejected by loader pipeline: {msg}"
        );
    }
}
