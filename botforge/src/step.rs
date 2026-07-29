use anyhow::Result;
use serde::de::{self, Deserializer};
use serde::Deserialize;
use serde_yaml::Value;
use std::collections::BTreeMap;

use crate::config::{yaml_scalar_to_string, yaml_scalar_truthiness};

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

/// Declarative outcome assertions for a `type: botforge/test` or `type: botforge/build` run step.
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

// ---------------------------------------------------------------------------
// Step output declaration types
// ---------------------------------------------------------------------------

/// Closed set of types an output may declare.
///
/// Unlike GitHub Actions (where every output is an untyped string), botforge outputs are
/// typed. Exactly these three types are permitted — any other declared type is a config
/// error at parse time.
#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum OutputType {
    String,
    Number,
    Bool,
}

/// Declaration of a single named output for a step.
///
/// Stored on the step node (in [`RunStep::outputs`]) as parsed from the `outputs:` map.
/// The declared type is a hard requirement: every output must have a type.
#[derive(Debug, Deserialize, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct OutputDeclaration {
    /// Declared type of this output. Must be one of `string`, `number`, `bool`.
    #[serde(rename = "type")]
    pub(crate) output_type: OutputType,
    /// Whether the output must be present (non-empty) after the step executes.
    /// `true` → step fails if the output is absent or empty.
    /// `false` (default) → absence/empty is acceptable; the output becomes null.
    #[serde(default)]
    pub(crate) required: bool,
}

/// A coerced output value captured from a step execution.
///
/// Represents the universal non-null case. The absent/null state is represented by
/// `Option::None` at the capture site — there is exactly one null, not per-type zeros.
/// Type only matters when a value is actually present; null has its own projection rules:
/// - into a string context: renders as `""`
/// - into a typed input: arrives as absent (not zero/false/"")
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum CapturedOutputValue {
    String(std::string::String),
    Number(f64),
    Bool(bool),
}

impl CapturedOutputValue {
    /// Project this value **into a string context** (interpolation or `string`-typed field).
    ///
    /// All concrete types render faithfully. Callers handling `Option<CapturedOutputValue>`
    /// must return `""` for `None` before calling this — null maps to `""` in string context.
    // Stage 2: capture-only; used by tests and will be called by Stage 4 interpolation.
    #[allow(dead_code)]
    pub(crate) fn to_string_projection(&self) -> std::string::String {
        match self {
            Self::String(s) => s.clone(),
            Self::Number(n) => format_output_number(*n),
            Self::Bool(b) => (if *b { "true" } else { "false" }).to_string(),
        }
    }
}

/// Format a float output value: whole numbers without a decimal point.
// Stage 2: only used by to_string_projection, itself guarded with allow(dead_code).
#[allow(dead_code)]
fn format_output_number(n: f64) -> String {
    if n.fract() == 0.0 && n.abs() < 1e15 {
        format!("{}", n as i64)
    } else {
        format!("{}", n)
    }
}

/// Coerce a raw emitted string to its declared output type.
///
/// The raw value is the text from the `$BOTFORGE_OUTPUT` file entry (with leading/trailing
/// whitespace stripped). Coercion failure is a hard error — there is no opt-in escape hatch.
///
/// Callers must handle the absent/null case (empty or not emitted) as `None` **before**
/// calling this function; it is only invoked when a non-empty raw value is present.
pub(crate) fn coerce_output(
    raw: &str,
    output_type: OutputType,
    output_name: &str,
) -> Result<CapturedOutputValue> {
    match output_type {
        OutputType::String => Ok(CapturedOutputValue::String(raw.to_string())),
        OutputType::Number => {
            let f: f64 = raw.trim().parse().map_err(|_| {
                anyhow::anyhow!(
                    "output '{}': cannot coerce {:?} to number",
                    output_name,
                    raw
                )
            })?;
            if !f.is_finite() {
                anyhow::bail!(
                    "output '{}': number must be finite, got {:?}",
                    output_name,
                    raw
                );
            }
            Ok(CapturedOutputValue::Number(f))
        }
        OutputType::Bool => match raw.trim() {
            "true" | "1" => Ok(CapturedOutputValue::Bool(true)),
            "false" | "0" => Ok(CapturedOutputValue::Bool(false)),
            _ => anyhow::bail!(
                "output '{}': cannot coerce {:?} to bool (expected true/false/1/0)",
                output_name,
                raw
            ),
        },
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RunStep {
    /// Where this step executes. Optional; defaults to `guest`.
    #[serde(rename = "on", default)]
    pub(crate) target: StepTarget,
    #[serde(deserialize_with = "deserialize_scalar_as_string")]
    pub(crate) name: String,
    #[serde(deserialize_with = "deserialize_scalar_as_string")]
    pub(crate) run: String,
    #[serde(default, deserialize_with = "deserialize_optional_positive_seconds")]
    pub(crate) timeout: Option<u64>,
    /// Interpreter used to execute `run:`. Mirrors GitHub Actions `shell:` semantics.
    ///
    /// Named shells: `bash` (default), `sh`, `python`.
    /// Custom template: any string containing `{0}`, e.g. `python3 -u {0}`.
    /// When absent, defaults to `bash --noprofile --norc -e -o pipefail {0}` with
    /// automatic `sh -e {0}` fallback if bash is not available.
    #[serde(default, deserialize_with = "deserialize_optional_scalar_as_string")]
    pub(crate) shell: Option<String>,
    /// When `true`, run the step's interpreter under `sudo -E` so the entire
    /// `run:` body executes as root. Only valid on `on: guest` steps (host steps
    /// run in the botforge container as the harness user). Defaults to `true`.
    #[serde(default)]
    pub(crate) sudo: Option<bool>,
    /// Optional identifier for the step. When set, it is shown in the step's
    /// title/status line as `(<index>/<id>)`. Purely a display label today — it is
    /// not required to be unique and is not addressable by other steps.
    #[serde(default, deserialize_with = "deserialize_optional_scalar_as_string")]
    pub(crate) id: Option<String>,
    /// Declarative outcome assertions for test/build run steps.
    #[serde(default)]
    pub(crate) expect: Option<ExpectBlock>,
    /// Optional runtime condition. When `false`, the step is skipped: it does not
    /// run, does not fail the plan, and is reported with a distinct skipped marker.
    /// When `true` (or absent), the step runs as normal.
    ///
    /// Truthiness uses the expression engine rules:
    /// - falsy: empty string, boolean `false`, number `0`
    /// - truthy: everything else
    ///
    /// Expression syntax (`${{ ... }}`) is evaluated before deserialization; this
    /// field receives the resulting scalar and applies the same truthiness coercion.
    #[serde(
        rename = "if",
        default,
        deserialize_with = "deserialize_step_condition"
    )]
    pub(crate) condition: Option<bool>,
    /// Named outputs this step may emit via `$BOTFORGE_OUTPUT`.
    ///
    /// Each entry declares the output's type (`string`, `number`, `bool`) and whether
    /// it is `required`. The closed type set is enforced at parse time: an unknown type
    /// is a config error. The declared type is stored here on the step node.
    #[serde(default)]
    pub(crate) outputs: BTreeMap<String, OutputDeclaration>,
}

impl RunStep {
    pub(crate) fn sudo_enabled(&self) -> bool {
        self.sudo.unwrap_or(true)
    }

    /// Returns `true` when the step should run, `false` when it should be skipped.
    /// Absent `if:` (stored as `None`) is treated as truthy — run as normal.
    pub(crate) fn condition_enabled(&self) -> bool {
        self.condition.unwrap_or(true)
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ArchiveStepSpec {
    #[serde(deserialize_with = "deserialize_scalar_as_string")]
    pub(crate) src: String,
    #[serde(default, deserialize_with = "deserialize_optional_scalar_as_string")]
    pub(crate) into: Option<String>,
    #[serde(default, deserialize_with = "deserialize_optional_scalar_as_string")]
    pub(crate) name: Option<String>,
    /// Guest destination path. Only valid when the step's `on:` is `guest`.
    #[serde(default, deserialize_with = "deserialize_optional_scalar_as_string")]
    pub(crate) dest: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ArchiveStep {
    pub(crate) archive: ArchiveStepSpec,
    #[serde(rename = "on", default)]
    pub(crate) target: Option<StepTarget>,
    #[serde(default, deserialize_with = "deserialize_optional_scalar_as_string")]
    pub(crate) run: Option<String>,
    #[serde(default, deserialize_with = "deserialize_optional_positive_seconds")]
    pub(crate) timeout: Option<u64>,
    #[serde(default, deserialize_with = "deserialize_optional_scalar_as_string")]
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
}

fn parse_positive_seconds(value: SecondsValue) -> std::result::Result<u64, String> {
    let SecondsValue::Integer(parsed) = value;
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

/// Deserialize the `if:` field into an `Option<bool>` using expression truthiness.
///
/// Delegates to `yaml_scalar_truthiness` (the single truthiness authority) so that
/// this function and `EvaluatedValue::truthy()` cannot drift out of agreement.
///
/// R6 — single truthiness authority: the PRIMARY path for expression-based `if:` conditions
/// is `substitute_if_condition_in_value` in the expression engine, which pre-evaluates the
/// expression and substitutes it back as a YAML `Bool`. This function then receives that
/// pre-evaluated `Bool` and falls through to the scalar case.
///
/// The remaining arms handle literal YAML scalars written directly (e.g. `if: false`,
/// `if: "false"`, `if: 0`). Their truthiness rules are consistent with `EvaluatedValue::truthy()`:
/// - `Bool(false)` / `Number(0)` / `String("")` → falsy
/// - non-empty strings (incl. `"false"`, `"0"`) → truthy (R6: non-empty string is ALWAYS truthy)
/// - `Bool(true)` / non-zero numbers → truthy
///
/// Do NOT add implicit string→typed coercion here. `if: "false"` (string) must run.
fn deserialize_step_condition<'de, D>(
    deserializer: D,
) -> std::result::Result<Option<bool>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Value::deserialize(deserializer)?;
    yaml_scalar_truthiness(&value).map_err(de::Error::custom)
}

/// Deserialize a required string field that may receive a typed YAML scalar (Number/Bool)
/// produced by a pure `${{ expr }}` substitution.
///
/// The scalar is coerced to its string representation using the same rules as
/// `EvaluatedValue::to_interpolated_string()` via `yaml_scalar_to_string`.
/// This is the single authority for "scalar coerced into a required string field".
fn deserialize_scalar_as_string<'de, D>(deserializer: D) -> std::result::Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Value::deserialize(deserializer)?;
    yaml_scalar_to_string(&value).map_err(de::Error::custom)
}

/// Deserialize an optional string field that may receive a typed YAML scalar (Number/Bool)
/// produced by a pure `${{ expr }}` substitution.
///
/// `null` yields `None`; all other scalars are coerced to `String` via `yaml_scalar_to_string`.
fn deserialize_optional_scalar_as_string<'de, D>(
    deserializer: D,
) -> std::result::Result<Option<String>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Value::deserialize(deserializer)?;
    match value {
        Value::Null => Ok(None),
        v => yaml_scalar_to_string(&v)
            .map(Some)
            .map_err(de::Error::custom),
    }
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

    // --- if: condition field ---

    #[test]
    fn test_if_absent_is_none() {
        let step: RunStep = serde_yaml::from_str("name: s\nrun: echo ok\n").unwrap();
        assert_eq!(step.condition, None);
        assert!(
            step.condition_enabled(),
            "absent if: should default to enabled"
        );
    }

    #[test]
    fn test_if_bool_true_is_some_true() {
        let step: RunStep = serde_yaml::from_str("name: s\nrun: echo ok\nif: true\n").unwrap();
        assert_eq!(step.condition, Some(true));
        assert!(step.condition_enabled());
    }

    #[test]
    fn test_if_bool_false_is_some_false() {
        let step: RunStep = serde_yaml::from_str("name: s\nrun: echo ok\nif: false\n").unwrap();
        assert_eq!(step.condition, Some(false));
        assert!(!step.condition_enabled());
    }

    #[test]
    fn test_if_string_truthy_literals() {
        for value in &["\"true\"", "\"1\"", "\"yes\"", "\"on\"", "\"anything\""] {
            let yaml = format!("name: s\nrun: echo ok\nif: {value}\n");
            let step: RunStep = serde_yaml::from_str(&yaml)
                .unwrap_or_else(|e| panic!("should parse if: {value} as truthy: {e}"));
            assert_eq!(
                step.condition,
                Some(true),
                "if: {value} should be Some(true)"
            );
            assert!(step.condition_enabled(), "if: {value} should be enabled");
        }
    }

    #[test]
    fn test_if_string_falsy_literals() {
        let value = "\"\"";
        let yaml = format!("name: s\nrun: echo ok\nif: {value}\n");
        let step: RunStep = serde_yaml::from_str(&yaml)
            .unwrap_or_else(|e| panic!("should parse if: {value} as falsy: {e}"));
        assert_eq!(
            step.condition,
            Some(false),
            "if: {value} should be Some(false)"
        );
        assert!(!step.condition_enabled(), "if: {value} should be disabled");
    }

    #[test]
    fn test_if_string_case_insensitive() {
        let truthy_cases = ["\"True\"", "\"TRUE\"", "\"YES\"", "\"ON\"", "\"1\""];
        for value in &truthy_cases {
            let yaml = format!("name: s\nrun: echo ok\nif: {value}\n");
            let step: RunStep = serde_yaml::from_str(&yaml)
                .unwrap_or_else(|e| panic!("should accept if: {value}: {e}"));
            assert_eq!(step.condition, Some(true), "if: {value} should be truthy");
        }
    }

    #[test]
    fn test_if_expression_placeholder_is_truthy_string_when_unresolved() {
        let step: RunStep =
            serde_yaml::from_str("name: s\nrun: echo ok\nif: \"${{ inputs.flag }}\"\n").unwrap();
        assert_eq!(step.condition, Some(true));
        assert!(step.condition_enabled());
    }

    #[test]
    fn test_if_number_truthiness() {
        let step: RunStep = serde_yaml::from_str("name: s\nrun: echo ok\nif: 2\n").unwrap();
        assert_eq!(step.condition, Some(true));
        assert!(step.condition_enabled());

        let step: RunStep = serde_yaml::from_str("name: s\nrun: echo ok\nif: 0\n").unwrap();
        assert_eq!(step.condition, Some(false));
        assert!(!step.condition_enabled());
    }

    #[test]
    fn test_if_deny_unknown_fields_still_holds() {
        let err =
            serde_yaml::from_str::<RunStep>("name: s\nrun: echo ok\nif: true\nsurprise: nope\n")
                .unwrap_err();
        assert!(
            err.to_string().contains("unknown field"),
            "deny_unknown_fields should still reject extra fields: {err}"
        );
    }

    // -------------------------------------------------------------------------
    // outputs: declaration parsing tests
    // -------------------------------------------------------------------------

    use super::{coerce_output, CapturedOutputValue, OutputDeclaration, OutputType};

    #[test]
    fn test_output_type_string_parses() {
        let decl: OutputDeclaration = serde_yaml::from_str("type: string\n").unwrap();
        assert_eq!(decl.output_type, OutputType::String);
        assert!(!decl.required);
    }

    #[test]
    fn test_output_type_number_parses() {
        let decl: OutputDeclaration = serde_yaml::from_str("type: number\n").unwrap();
        assert_eq!(decl.output_type, OutputType::Number);
    }

    #[test]
    fn test_output_type_bool_parses() {
        let decl: OutputDeclaration = serde_yaml::from_str("type: bool\n").unwrap();
        assert_eq!(decl.output_type, OutputType::Bool);
    }

    #[test]
    fn test_output_type_unknown_is_config_error() {
        // "object" is not in the closed set
        let err = serde_yaml::from_str::<OutputDeclaration>("type: object\n").unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("unknown variant") || msg.contains("object"),
            "unknown type should produce a config error: {msg}"
        );
    }

    #[test]
    fn test_output_type_array_is_config_error() {
        let err = serde_yaml::from_str::<OutputDeclaration>("type: array\n").unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("unknown variant") || msg.contains("array"),
            "array type should be rejected: {msg}"
        );
    }

    #[test]
    fn test_output_declaration_required_true() {
        let decl: OutputDeclaration =
            serde_yaml::from_str("type: string\nrequired: true\n").unwrap();
        assert!(decl.required);
    }

    #[test]
    fn test_output_declaration_required_defaults_false() {
        let decl: OutputDeclaration = serde_yaml::from_str("type: number\n").unwrap();
        assert!(!decl.required);
    }

    #[test]
    fn test_output_declaration_unknown_field_rejected() {
        let err =
            serde_yaml::from_str::<OutputDeclaration>("type: string\nunknown: nope\n").unwrap_err();
        assert!(
            err.to_string().contains("unknown field"),
            "deny_unknown_fields should apply to OutputDeclaration: {err}"
        );
    }

    #[test]
    fn test_run_step_parses_outputs_block() {
        let step: RunStep = serde_yaml::from_str(
            r#"
name: emit-step
run: echo "MY_VAL=42" >> "$BOTFORGE_OUTPUT"
outputs:
  my_val:
    type: number
    required: true
  tag:
    type: string
"#,
        )
        .unwrap();
        assert_eq!(step.outputs.len(), 2);
        let my_val = &step.outputs["my_val"];
        assert_eq!(my_val.output_type, OutputType::Number);
        assert!(my_val.required);
        let tag = &step.outputs["tag"];
        assert_eq!(tag.output_type, OutputType::String);
        assert!(!tag.required);
    }

    #[test]
    fn test_run_step_absent_outputs_is_empty() {
        let step: RunStep = serde_yaml::from_str("name: s\nrun: echo ok\n").unwrap();
        assert!(step.outputs.is_empty());
    }

    // -------------------------------------------------------------------------
    // coerce_output tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_coerce_string_identity() {
        let v = coerce_output("hello world", OutputType::String, "out").unwrap();
        assert_eq!(v, CapturedOutputValue::String("hello world".to_string()));
    }

    #[test]
    fn test_coerce_string_empty_not_called() {
        // Empty values are handled by callers as None, but coerce_output with empty string
        // for string type should still work (caller responsibility to treat empty as None)
        let v = coerce_output("", OutputType::String, "out").unwrap();
        assert_eq!(v, CapturedOutputValue::String("".to_string()));
    }

    #[test]
    fn test_coerce_number_integer() {
        let v = coerce_output("42", OutputType::Number, "count").unwrap();
        assert_eq!(v, CapturedOutputValue::Number(42.0));
    }

    #[test]
    fn test_coerce_number_float() {
        let v = coerce_output("3.14", OutputType::Number, "pi").unwrap();
        assert_eq!(v, CapturedOutputValue::Number(3.14));
    }

    #[test]
    fn test_coerce_number_negative() {
        let v = coerce_output("-7", OutputType::Number, "neg").unwrap();
        assert_eq!(v, CapturedOutputValue::Number(-7.0));
    }

    #[test]
    fn test_coerce_number_whitespace_trimmed() {
        let v = coerce_output("  99  ", OutputType::Number, "n").unwrap();
        assert_eq!(v, CapturedOutputValue::Number(99.0));
    }

    #[test]
    fn test_coerce_number_invalid_is_hard_fail() {
        let err = coerce_output("not-a-number", OutputType::Number, "bad").unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("bad") && msg.contains("not-a-number"),
            "error should identify output name and value: {msg}"
        );
    }

    #[test]
    fn test_coerce_number_infinity_is_hard_fail() {
        let err = coerce_output("inf", OutputType::Number, "inf_out").unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("finite"),
            "infinity should be rejected as non-finite: {msg}"
        );
    }

    #[test]
    fn test_coerce_bool_true_literal() {
        let v = coerce_output("true", OutputType::Bool, "flag").unwrap();
        assert_eq!(v, CapturedOutputValue::Bool(true));
    }

    #[test]
    fn test_coerce_bool_false_literal() {
        let v = coerce_output("false", OutputType::Bool, "flag").unwrap();
        assert_eq!(v, CapturedOutputValue::Bool(false));
    }

    #[test]
    fn test_coerce_bool_one_is_true() {
        let v = coerce_output("1", OutputType::Bool, "flag").unwrap();
        assert_eq!(v, CapturedOutputValue::Bool(true));
    }

    #[test]
    fn test_coerce_bool_zero_is_false() {
        let v = coerce_output("0", OutputType::Bool, "flag").unwrap();
        assert_eq!(v, CapturedOutputValue::Bool(false));
    }

    #[test]
    fn test_coerce_bool_invalid_is_hard_fail() {
        let err = coerce_output("yes", OutputType::Bool, "flag").unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("flag") && (msg.contains("bool") || msg.contains("true")),
            "bool coercion error should identify output and expected values: {msg}"
        );
    }

    #[test]
    fn test_coerce_bool_truecased_invalid_is_hard_fail() {
        // Bool coercion is case-sensitive: "True" is not "true"
        let err = coerce_output("True", OutputType::Bool, "flag").unwrap_err();
        assert!(
            err.to_string().contains("flag"),
            "should fail for 'True' (not lowercased): {err}"
        );
    }

    // -------------------------------------------------------------------------
    // CapturedOutputValue::to_string_projection tests (null model)
    // -------------------------------------------------------------------------

    #[test]
    fn test_string_projection_of_string_value() {
        let v = CapturedOutputValue::String("hello".to_string());
        assert_eq!(v.to_string_projection(), "hello");
    }

    #[test]
    fn test_string_projection_of_number_integer() {
        let v = CapturedOutputValue::Number(7.0);
        assert_eq!(v.to_string_projection(), "7");
    }

    #[test]
    fn test_string_projection_of_number_float() {
        let v = CapturedOutputValue::Number(1.5);
        assert_eq!(v.to_string_projection(), "1.5");
    }

    #[test]
    fn test_string_projection_of_bool_true() {
        let v = CapturedOutputValue::Bool(true);
        assert_eq!(v.to_string_projection(), "true");
    }

    #[test]
    fn test_string_projection_of_bool_false() {
        let v = CapturedOutputValue::Bool(false);
        assert_eq!(v.to_string_projection(), "false");
    }

    #[test]
    fn test_null_projects_to_empty_string_in_string_context() {
        // The universal null/absent is represented as Option::None.
        // Callers must render it as "" in string context.
        let opt_val: Option<CapturedOutputValue> = None;
        let rendered = opt_val
            .as_ref()
            .map(|v| v.to_string_projection())
            .unwrap_or_default();
        assert_eq!(
            rendered, "",
            "null must render as empty string in string context"
        );
    }
}
