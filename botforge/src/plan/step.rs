use anyhow::Result;
use serde::de::{self, Deserializer};
use serde::Deserialize;
use serde_yaml::Value;

/// Where a test step executes: inside the guest (SSH) or on the harness host (local).
#[derive(Debug, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub(crate) enum StepTarget {
    /// Run via SSH inside the guest VM.
    #[default]
    Guest,
    /// Run locally in the botforge container (harness), reaching the guest only via forwarded
    /// `ports:`. This is the botforge container / harness where botforge itself runs — not the
    /// CI runner host.
    Host,
}

/// Per-stream matchers for an `expect:` block.
///
/// `contains` is an AND over its list: **all** strings must appear in the output.
/// `not_contains` is an AND-of-negations: **none** of the strings may appear.
#[derive(Debug, Deserialize, Default, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct StdioExpect {
    /// All strings in this list must appear in the stream output (AND).
    #[serde(default)]
    pub(crate) contains: Vec<String>,
    /// No string in this list may appear in the stream output (AND-of-negations).
    #[serde(default)]
    pub(crate) not_contains: Vec<String>,
}

/// Declarative outcome assertions for a `type: test` or `type: build` run step.
///
/// When present, the step captures stdout/stderr and validates exit/output expectations
/// after execution; any mismatch aborts the run.
#[derive(Debug, Deserialize, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct ExpectBlock {
    /// Expected exit code.  Defaults to `0` when `expect:` is present but `exit:` is absent.
    #[serde(default)]
    pub(crate) exit: Option<i32>,
    /// Matchers applied to the step's stdout.
    #[serde(default)]
    pub(crate) stdout: Option<StdioExpect>,
    /// Matchers applied to the step's stderr.
    #[serde(default)]
    pub(crate) stderr: Option<StdioExpect>,
}

impl ExpectBlock {
    /// The expected exit code; defaults to `0` when `exit:` is absent.
    pub(crate) fn expected_exit(&self) -> i32 {
        self.exit.unwrap_or(0)
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RunStep {
    /// Where this step executes. Optional; defaults to `guest`.
    #[serde(rename = "on", default)]
    pub(crate) target: StepTarget,
    pub(crate) name: String,
    pub(crate) run: String,
    #[serde(default, deserialize_with = "deserialize_optional_positive_seconds")]
    pub(crate) timeout: Option<u64>,
    /// Interpreter used to execute `run:`. Mirrors GitHub Actions `shell:` semantics.
    ///
    /// Named shells: `bash` (default), `sh`, `python`.
    /// Custom template: any string containing `{0}`, e.g. `python3 -u {0}`.
    /// When absent, defaults to `bash --noprofile --norc -e -o pipefail {0}` with
    /// automatic `sh -e {0}` fallback if bash is not available.
    #[serde(default)]
    pub(crate) shell: Option<String>,
    /// When `true`, run the step's interpreter under `sudo -E` so the entire
    /// `run:` body executes as root. Only valid on `on: guest` steps (host steps
    /// run in the botforge container as the harness user). Defaults to `true`.
    #[serde(default)]
    pub(crate) sudo: Option<bool>,
    /// Optional identifier for the step. When set, it is shown in the step's
    /// title/status line as `(<index>/<id>)`. Purely a display label today — it is
    /// not required to be unique and is not addressable by other steps.
    #[serde(default)]
    pub(crate) id: Option<String>,
    /// Declarative outcome assertions for test/build run steps.
    #[serde(default)]
    pub(crate) expect: Option<ExpectBlock>,
}

impl RunStep {
    pub(crate) fn sudo_enabled(&self) -> bool {
        self.sudo.unwrap_or(true)
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ArchiveStepSpec {
    pub(crate) src: String,
    #[serde(default)]
    pub(crate) into: Option<String>,
    #[serde(default)]
    pub(crate) name: Option<String>,
    /// Guest destination path. Only valid when the step's `on:` is `guest`.
    #[serde(default)]
    pub(crate) dest: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ArchiveStep {
    pub(crate) archive: ArchiveStepSpec,
    #[serde(rename = "on", default)]
    pub(crate) target: Option<StepTarget>,
    #[serde(default)]
    pub(crate) run: Option<String>,
    #[serde(default, deserialize_with = "deserialize_optional_positive_seconds")]
    pub(crate) timeout: Option<u64>,
    #[serde(default)]
    pub(crate) shell: Option<String>,
}

#[derive(Debug)]
pub(crate) enum TestStep {
    Run(RunStep),
    Archive(ArchiveStep),
}

impl TestStep {
    pub(crate) fn display_name(&self) -> &str {
        match self {
            Self::Run(step) => &step.name,
            Self::Archive(step) => step
                .archive
                .name
                .as_deref()
                .unwrap_or(step.archive.src.as_str()),
        }
    }

    pub(crate) fn display_id(&self) -> Option<&str> {
        match self {
            Self::Run(step) => step.id.as_deref(),
            Self::Archive(_) => None,
        }
    }
}

impl<'de> Deserialize<'de> for TestStep {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = Value::deserialize(deserializer)?;
        if let Value::Mapping(mapping) = &value {
            if mapping.contains_key(Value::String("archive".to_string())) {
                return serde_yaml::from_value::<ArchiveStep>(value)
                    .map(Self::Archive)
                    .map_err(de::Error::custom);
            }
        }
        serde_yaml::from_value::<RunStep>(value)
            .map(Self::Run)
            .map_err(de::Error::custom)
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum SecondsValue {
    Integer(i64),
    String(String),
}

fn parse_positive_seconds(value: SecondsValue) -> std::result::Result<u64, String> {
    let parsed = match value {
        SecondsValue::Integer(value) => value,
        SecondsValue::String(value) => value
            .trim()
            .parse::<i64>()
            .map_err(|_| format!("expected a positive integer number of seconds, got '{value}'"))?,
    };
    if parsed <= 0 {
        return Err(format!(
            "expected a positive integer number of seconds, got {parsed}"
        ));
    }
    Ok(parsed as u64)
}

pub(crate) fn deserialize_optional_positive_seconds<'de, D>(
    deserializer: D,
) -> std::result::Result<Option<u64>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<SecondsValue>::deserialize(deserializer)?;
    value
        .map(parse_positive_seconds)
        .transpose()
        .map_err(de::Error::custom)
}

/// Resolve a step's `shell:` value into an argv template with a `{0}` slot.
///
/// Named shells (`bash`, `sh`, `python`) map to fixed GHA-compatible templates.
/// Custom templates must contain `{0}` as a placeholder for the script file path.
/// `None` (absent) returns the default `bash` template.
///
/// Returns `Err` for: unknown single-token named shell, or a custom multi-token
/// shell string that does not contain `{0}`.
pub(crate) fn resolve_shell(shell: Option<&str>) -> Result<Vec<String>> {
    match shell {
        None | Some("bash") => Ok(vec![
            "bash".to_string(),
            "--noprofile".to_string(),
            "--norc".to_string(),
            "-e".to_string(),
            "-o".to_string(),
            "pipefail".to_string(),
            "{0}".to_string(),
        ]),
        Some("sh") => Ok(vec!["sh".to_string(), "-e".to_string(), "{0}".to_string()]),
        Some("python") => Ok(vec!["python3".to_string(), "{0}".to_string()]),
        Some(custom) => {
            if custom.contains("{0}") {
                Ok(custom.split_whitespace().map(str::to_string).collect())
            } else if custom.split_whitespace().count() <= 1 {
                anyhow::bail!(
                    "unknown named shell '{}'; supported named shells: bash, sh, python. \
                     For a custom interpreter use the '{{0}}' placeholder form, \
                     e.g. '{} {{0}}'",
                    custom,
                    custom
                )
            } else {
                anyhow::bail!(
                    "custom shell '{}' does not contain the '{{0}}' placeholder; \
                     '{{0}}' must appear in the shell template to indicate where \
                     the script file path is substituted",
                    custom
                )
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{resolve_shell, RunStep, StepTarget};

    #[test]
    fn test_resolve_shell_absent_returns_bash_template() {
        let tmpl = resolve_shell(None).unwrap();
        assert_eq!(
            tmpl,
            vec![
                "bash",
                "--noprofile",
                "--norc",
                "-e",
                "-o",
                "pipefail",
                "{0}"
            ]
        );
    }

    #[test]
    fn test_resolve_shell_bash_returns_bash_template() {
        let tmpl = resolve_shell(Some("bash")).unwrap();
        assert_eq!(
            tmpl,
            vec![
                "bash",
                "--noprofile",
                "--norc",
                "-e",
                "-o",
                "pipefail",
                "{0}"
            ]
        );
    }

    #[test]
    fn test_resolve_shell_sh_returns_sh_template() {
        let tmpl = resolve_shell(Some("sh")).unwrap();
        assert_eq!(tmpl, vec!["sh", "-e", "{0}"]);
    }

    #[test]
    fn test_resolve_shell_python_returns_python3_template() {
        let tmpl = resolve_shell(Some("python")).unwrap();
        assert_eq!(tmpl, vec!["python3", "{0}"]);
    }

    #[test]
    fn test_resolve_shell_custom_with_placeholder_is_split() {
        let tmpl = resolve_shell(Some("python3 -u {0}")).unwrap();
        assert_eq!(tmpl, vec!["python3", "-u", "{0}"]);
    }

    #[test]
    fn test_resolve_shell_custom_without_placeholder_is_error() {
        let err = resolve_shell(Some("python3 -u")).unwrap_err();
        assert!(
            err.to_string().contains("{0}"),
            "error should mention '{{0}}': {err}"
        );
    }

    #[test]
    fn test_resolve_shell_unknown_named_shell_is_error() {
        let err = resolve_shell(Some("fish")).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("fish"),
            "error should mention the shell name: {msg}"
        );
        assert!(
            msg.contains("bash") && msg.contains("sh") && msg.contains("python"),
            "error should list supported shells: {msg}"
        );
    }

    #[test]
    fn test_apply_shell_template_substitutes_placeholder() {
        let tmpl = resolve_shell(None).unwrap();
        let argv: Vec<String> = tmpl
            .iter()
            .map(|a| {
                if a == "{0}" {
                    "/tmp/my-script.sh".to_string()
                } else {
                    a.clone()
                }
            })
            .collect();
        assert_eq!(
            argv,
            vec![
                "bash",
                "--noprofile",
                "--norc",
                "-e",
                "-o",
                "pipefail",
                "/tmp/my-script.sh"
            ]
        );
    }

    #[test]
    fn test_apply_sh_template_substitutes_placeholder() {
        let tmpl = resolve_shell(Some("sh")).unwrap();
        let argv: Vec<String> = tmpl
            .iter()
            .map(|a| {
                if a == "{0}" {
                    "/tmp/step.sh".to_string()
                } else {
                    a.clone()
                }
            })
            .collect();
        assert_eq!(argv, vec!["sh", "-e", "/tmp/step.sh"]);
    }

    #[test]
    fn test_run_step_parses_sudo_true() {
        let step: RunStep = serde_yaml::from_str(
            r#"
on: guest
name: root-step
sudo: true
run: echo ok
"#,
        )
        .unwrap();
        assert_eq!(step.sudo, Some(true));
    }

    #[test]
    fn test_run_step_parses_sudo_false() {
        let step: RunStep = serde_yaml::from_str(
            r#"
on: guest
name: unprivileged-step
sudo: false
run: echo ok
"#,
        )
        .unwrap();
        assert_eq!(step.sudo, Some(false));
    }

    #[test]
    fn test_run_step_parses_without_sudo_defaults_to_none() {
        let step: RunStep = serde_yaml::from_str(
            r#"
on: guest
name: no-sudo
run: echo ok
"#,
        )
        .unwrap();
        assert_eq!(step.sudo, None);
    }

    #[test]
    fn test_run_step_parses_without_on_defaults_to_guest() {
        let step: RunStep = serde_yaml::from_str(
            r#"
name: no-target
run: echo ok
"#,
        )
        .unwrap();
        assert_eq!(step.target, StepTarget::Guest);
    }

    #[test]
    fn test_run_step_sudo_enabled_uses_default_true() {
        let omitted: RunStep = serde_yaml::from_str(
            r#"
name: sudo-default
run: echo ok
"#,
        )
        .unwrap();
        assert!(omitted.sudo_enabled());

        let explicit_false: RunStep = serde_yaml::from_str(
            r#"
name: sudo-false
sudo: false
run: echo ok
"#,
        )
        .unwrap();
        assert!(!explicit_false.sudo_enabled());

        let explicit_true: RunStep = serde_yaml::from_str(
            r#"
name: sudo-true
sudo: true
run: echo ok
"#,
        )
        .unwrap();
        assert!(explicit_true.sudo_enabled());
    }

    #[test]
    fn test_run_step_unknown_field_still_fails() {
        let err = serde_yaml::from_str::<RunStep>(
            r#"
on: guest
name: bad-step
run: echo ok
surprise: nope
"#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("unknown field"));
    }

    // --- expect: block ---

    #[test]
    fn test_expect_block_parses_full() {
        let step: RunStep = serde_yaml::from_str(
            r#"
name: check-service
run: systemctl is-enabled foo.service
expect:
  exit: 0
  stdout:
    contains: [enabled]
    not_contains: [masked, disabled]
  stderr:
    not_contains: [error]
"#,
        )
        .unwrap();
        let expect = step.expect.expect("expect should be Some");
        assert_eq!(expect.exit, Some(0));
        let stdout = expect.stdout.expect("stdout should be Some");
        assert_eq!(stdout.contains, vec!["enabled"]);
        assert_eq!(stdout.not_contains, vec!["masked", "disabled"]);
        let stderr = expect.stderr.expect("stderr should be Some");
        assert!(stderr.contains.is_empty());
        assert_eq!(stderr.not_contains, vec!["error"]);
    }

    #[test]
    fn test_expect_block_absent_yields_none() {
        let step: RunStep = serde_yaml::from_str(
            r#"
name: no-expect
run: echo ok
"#,
        )
        .unwrap();
        assert!(step.expect.is_none());
    }

    #[test]
    fn test_expect_block_exit_only() {
        let step: RunStep = serde_yaml::from_str(
            r#"
name: must-fail
run: exit 2
expect:
  exit: 2
"#,
        )
        .unwrap();
        let expect = step.expect.expect("expect should be Some");
        assert_eq!(expect.exit, Some(2));
        assert!(expect.stdout.is_none());
        assert!(expect.stderr.is_none());
        assert_eq!(expect.expected_exit(), 2);
    }

    #[test]
    fn test_expect_block_default_exit_is_zero() {
        let step: RunStep = serde_yaml::from_str(
            r#"
name: default-exit
run: echo hi
expect:
  stdout:
    contains: [hi]
"#,
        )
        .unwrap();
        let expect = step.expect.expect("expect should be Some");
        assert_eq!(expect.exit, None);
        assert_eq!(expect.expected_exit(), 0);
    }

    #[test]
    fn test_expect_block_unknown_field_is_rejected() {
        let err = serde_yaml::from_str::<RunStep>(
            r#"
name: bad
run: echo ok
expect:
  exit: 0
  unknown_key: true
"#,
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("unknown field"),
            "should reject unknown field in expect block: {err}"
        );
    }

    #[test]
    fn test_stdio_expect_unknown_field_is_rejected() {
        let err = serde_yaml::from_str::<RunStep>(
            r#"
name: bad
run: echo ok
expect:
  stdout:
    contains: [ok]
    unexpected: true
"#,
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("unknown field"),
            "should reject unknown field in stdout expect block: {err}"
        );
    }
}
