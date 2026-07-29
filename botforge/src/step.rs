use anyhow::{Context, Result};
use serde::de::{self, Deserializer};
use serde::Deserialize;
use serde_yaml::Value;

use crate::config::{yaml_scalar_to_string, yaml_scalar_truthiness};

/// The declared type of a step output.
///
/// This is a **closed** set — no other type is valid.  An unknown type is a
/// config error at parse time (serde will return an error).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum OutputType {
    String,
    Number,
    Bool,
}

impl std::fmt::Display for OutputType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OutputType::String => write!(f, "string"),
            OutputType::Number => write!(f, "number"),
            OutputType::Bool => write!(f, "bool"),
        }
    }
}

/// A single output declaration on a run step.
///
/// Outputs are declared on the step, typed with `type:` (closed enum:
/// `string`, `number`, `bool`), and optionally `required: true` to enforce
/// that the step actually emits a non-empty value.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct OutputDecl {
    /// The output name.  Must be unique within a step's `outputs:` list.
    #[serde(deserialize_with = "deserialize_scalar_as_string")]
    pub(crate) name: String,
    /// The declared type.  One of `string`, `number`, `bool`.
    #[serde(rename = "type")]
    pub(crate) output_type: OutputType,
    /// When `true`, the step must emit a non-empty value for this output or
    /// the step fails at runtime.  Defaults to `false`.
    #[serde(default)]
    pub(crate) required: bool,
}

/// The coerced value of a captured output.
///
/// There is exactly **one** universal empty: `Null`.  It is used for both
/// "not emitted" and "emitted as empty string".  `Null` is *not* a per-type
/// zero (no `0`, no `false`, no `""`).
///
/// Projection behaviour of `Null`:
/// - Into a string context (interpolation / `string`-typed field): renders as `""`.
/// - Into a typed input: provides no value at all (treated as absent).
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum OutputValue {
    Null,
    String(String),
    Number(f64),
    Bool(bool),
}

impl OutputValue {
    /// Project the value into a string context, returning an owned `String`.
    ///
    /// `Null` → `""`.  All other variants → their string representation.
    ///
    /// Stage 2 is capture-only; this method is reserved for the downstream
    /// resolution stage (Stage 4+) and is intentionally unused until then.
    #[allow(dead_code)]
    pub(crate) fn to_string_context(&self) -> String {
        match self {
            OutputValue::Null => String::new(),
            OutputValue::String(s) => s.clone(),
            OutputValue::Number(n) => {
                if n.fract() == 0.0 && n.abs() < 1e15 {
                    format!("{}", *n as i64)
                } else {
                    format!("{n}")
                }
            }
            OutputValue::Bool(b) => b.to_string(),
        }
    }
}

/// A captured and coerced output value, as stored on the executed step node.
///
/// Stage 2 is capture-only; these fields are reserved for the downstream
/// resolution stage (Stage 4+) and are intentionally unused until then.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub(crate) struct CapturedOutput {
    /// The output name (matches a declaration in `RunStep.outputs`).
    pub(crate) name: String,
    /// The declared type (copied from the declaration for uniform source-agnostic access).
    pub(crate) declared_type: OutputType,
    /// The coerced value.
    pub(crate) value: OutputValue,
}

/// Coerce a raw emitted string to the declared output type.
///
/// An empty raw string is **always** `Null`, regardless of declared type.
/// Type only matters when there is an actual (non-empty) value to coerce.
/// Coercion failure is a **hard failure** — there is no opt-in leniency.
pub(crate) fn coerce_output_value(
    name: &str,
    raw: &str,
    declared_type: OutputType,
) -> Result<OutputValue> {
    if raw.is_empty() {
        return Ok(OutputValue::Null);
    }
    match declared_type {
        OutputType::String => Ok(OutputValue::String(raw.to_string())),
        OutputType::Number => raw.parse::<f64>().map(OutputValue::Number).map_err(|_| {
            anyhow::anyhow!(
                "output '{}': value {:?} cannot be coerced to number",
                name,
                raw
            )
        }),
        OutputType::Bool => match raw {
            "true" => Ok(OutputValue::Bool(true)),
            "false" => Ok(OutputValue::Bool(false)),
            other => anyhow::bail!(
                "output '{}': value {:?} cannot be coerced to bool (expected \"true\" or \"false\")",
                name,
                other
            ),
        },
    }
}

/// Capture and coerce all declared outputs for a step after execution.
///
/// `out_contents` is the contents of the BF_OUT file written by the step.
/// The file format is identical to BF_ENV (`KEY=VALUE` or heredoc).
///
/// - Undeclared emissions are silently ignored.
/// - Coercion failures are hard errors.
/// - `required: true` outputs that are null (not emitted or emitted as empty) fail.
pub(crate) fn capture_step_outputs(
    step_name: &str,
    declarations: &[OutputDecl],
    out_contents: &str,
) -> Result<Vec<CapturedOutput>> {
    if declarations.is_empty() {
        return Ok(Vec::new());
    }

    let emitted = parse_out_file(out_contents)?;

    let mut captured: Vec<CapturedOutput> = Vec::with_capacity(declarations.len());

    for decl in declarations {
        let raw = emitted
            .iter()
            .find(|(k, _)| k == &decl.name)
            .map(|(_, v)| v.as_str())
            .unwrap_or("");

        let value = coerce_output_value(&decl.name, raw, decl.output_type)
            .with_context(|| format!("step '{}' output coercion failed", step_name))?;

        if decl.required && matches!(value, OutputValue::Null) {
            anyhow::bail!(
                "step '{}': required output '{}' was not emitted or was empty",
                step_name,
                decl.name
            );
        }

        captured.push(CapturedOutput {
            name: decl.name.clone(),
            declared_type: decl.output_type,
            value,
        });
    }

    Ok(captured)
}

/// Parse a BF_OUT-format string (`KEY=VALUE` / heredoc), returning key-value pairs.
///
/// This is the same format as BF_ENV; blank lines and lines matching neither
/// format are skipped.
fn parse_out_file(contents: &str) -> Result<Vec<(String, String)>> {
    let mut result: Vec<(String, String)> = Vec::new();
    let lines: Vec<&str> = contents.lines().collect();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        if line.is_empty() {
            i += 1;
            continue;
        }
        let eq_pos = line.find('=');
        let heredoc_pos = line.find("<<");
        if let Some(hpos) = heredoc_pos {
            let before_heredoc = &line[..hpos];
            if !before_heredoc.is_empty()
                && eq_pos.is_none_or(|ep| ep > hpos)
                && !before_heredoc.contains('=')
            {
                let key = before_heredoc;
                let delimiter = &line[hpos + 2..];
                if !delimiter.is_empty() {
                    i += 1;
                    let mut value_lines: Vec<&str> = Vec::new();
                    while i < lines.len() && lines[i] != delimiter {
                        value_lines.push(lines[i]);
                        i += 1;
                    }
                    if i >= lines.len() {
                        anyhow::bail!("unterminated heredoc for key '{key}'");
                    }
                    i += 1;
                    result.push((key.to_string(), value_lines.join("\n")));
                    continue;
                }
            }
        }
        if let Some(eq) = eq_pos {
            let key = &line[..eq];
            let value = &line[eq + 1..];
            if !key.is_empty() {
                result.push((key.to_string(), value.to_string()));
            }
        }
        i += 1;
    }
    Ok(result)
}

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
    /// Typed output declarations for this step.
    ///
    /// When the step runs, `BF_OUT` is set to a writable file; the step body
    /// emits `NAME=value` lines to it.  After execution the executor reads the
    /// file, coerces each emission to the declared type, enforces `required`,
    /// and stores the results in `captured_outputs`.
    ///
    /// On `uses:` steps the declarations come from the fragment's `outputs:`
    /// block instead (Stage 4+).  This field is empty for such steps.
    #[serde(default)]
    pub(crate) outputs: Vec<OutputDecl>,
    /// Captured and coerced output values, populated after this step executes.
    ///
    /// `None` before the step runs; `Some(vec)` after.  Written by the step
    /// executor; the slot is the substrate a later deferred-resolution pass
    /// (Stage 4) will read from.  Not part of the YAML schema.
    #[serde(skip)]
    pub(crate) captured_outputs: std::cell::RefCell<Option<Vec<CapturedOutput>>>,
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

/// A scoped fragment invocation produced by a `uses:` entry.
///
/// The fragment's inner steps execute in their own env scope: they inherit the
/// caller's accumulated env, but any env mutations made inside the invocation are
/// contained within that scope and do not leak back to the caller or to later
/// sibling steps.  `files:` and `cloud_init:` contributions from the fragment are
/// accumulated globally at load time before this step is produced.
#[derive(Debug)]
pub(crate) struct InvokeStep {
    /// The `uses:` value as written; used for display in step titles and logs.
    pub(crate) uses: String,
    /// The fragment's pre-resolved, pre-expanded step list.  May itself contain
    /// further `Invoke` steps (recursive fragments).
    pub(crate) steps: Vec<TestStep>,
}

#[derive(Debug)]
pub(crate) enum TestStep {
    Run(RunStep),
    Archive(ArchiveStep),
    /// A scoped fragment invocation (`uses:` entry).
    Invoke(InvokeStep),
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
            Self::Invoke(step) => &step.uses,
        }
    }

    pub(crate) fn display_id(&self) -> Option<&str> {
        match self {
            Self::Run(step) => step.id.as_deref(),
            Self::Archive(_) | Self::Invoke(_) => None,
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

    // --- Stage 2: OutputType parse-time validation ---

    mod output_type {
        use super::super::{OutputDecl, OutputType, RunStep};

        #[test]
        fn test_output_type_string_parses() {
            let yaml = "name: my-output\ntype: string\n";
            let decl: OutputDecl = serde_yaml::from_str(yaml).unwrap();
            assert_eq!(decl.output_type, OutputType::String);
        }

        #[test]
        fn test_output_type_number_parses() {
            let yaml = "name: count\ntype: number\n";
            let decl: OutputDecl = serde_yaml::from_str(yaml).unwrap();
            assert_eq!(decl.output_type, OutputType::Number);
        }

        #[test]
        fn test_output_type_bool_parses() {
            let yaml = "name: flag\ntype: bool\n";
            let decl: OutputDecl = serde_yaml::from_str(yaml).unwrap();
            assert_eq!(decl.output_type, OutputType::Bool);
        }

        #[test]
        fn test_output_type_unknown_is_config_error() {
            let yaml = "name: x\ntype: integer\n";
            let err = serde_yaml::from_str::<OutputDecl>(yaml).unwrap_err();
            let msg = err.to_string();
            assert!(
                msg.contains("integer")
                    || msg.contains("unknown variant")
                    || msg.contains("expected one of"),
                "error should mention the bad type: {msg}"
            );
        }

        #[test]
        fn test_output_type_object_is_config_error() {
            let yaml = "name: x\ntype: object\n";
            let err = serde_yaml::from_str::<OutputDecl>(yaml).unwrap_err();
            assert!(
                !err.to_string().is_empty(),
                "unknown type 'object' should fail"
            );
        }

        #[test]
        fn test_output_required_defaults_to_false() {
            let yaml = "name: x\ntype: string\n";
            let decl: OutputDecl = serde_yaml::from_str(yaml).unwrap();
            assert!(!decl.required, "required defaults to false");
        }

        #[test]
        fn test_output_required_true_parses() {
            let yaml = "name: x\ntype: string\nrequired: true\n";
            let decl: OutputDecl = serde_yaml::from_str(yaml).unwrap();
            assert!(decl.required);
        }

        #[test]
        fn test_output_unknown_field_is_error() {
            let yaml = "name: x\ntype: string\nfoo: bar\n";
            let err = serde_yaml::from_str::<OutputDecl>(yaml).unwrap_err();
            assert!(
                err.to_string().contains("unknown field") || err.to_string().contains("foo"),
                "unknown field should be rejected: {err}"
            );
        }

        #[test]
        fn test_run_step_parses_outputs_list() {
            let yaml = "name: s\nrun: echo ok\noutputs:\n  - name: result\n    type: string\n";
            let step: RunStep = serde_yaml::from_str(yaml).unwrap();
            assert_eq!(step.outputs.len(), 1);
            assert_eq!(step.outputs[0].name, "result");
            assert_eq!(step.outputs[0].output_type, OutputType::String);
        }

        #[test]
        fn test_run_step_outputs_absent_defaults_to_empty() {
            let yaml = "name: s\nrun: echo ok\n";
            let step: RunStep = serde_yaml::from_str(yaml).unwrap();
            assert!(step.outputs.is_empty());
        }

        #[test]
        fn test_run_step_outputs_display_name() {
            assert_eq!(OutputType::String.to_string(), "string");
            assert_eq!(OutputType::Number.to_string(), "number");
            assert_eq!(OutputType::Bool.to_string(), "bool");
        }
    }

    // --- Stage 2: coerce_output_value ---

    mod coercion {
        use super::super::{coerce_output_value, OutputType, OutputValue};

        // --- string coercion ---
        #[test]
        fn test_coerce_string_returns_string_value() {
            let v = coerce_output_value("x", "hello", OutputType::String).unwrap();
            assert_eq!(v, OutputValue::String("hello".to_string()));
        }

        #[test]
        fn test_coerce_string_empty_is_null() {
            let v = coerce_output_value("x", "", OutputType::String).unwrap();
            assert_eq!(v, OutputValue::Null);
        }

        // --- number coercion ---
        #[test]
        fn test_coerce_number_integer_string() {
            let v = coerce_output_value("n", "42", OutputType::Number).unwrap();
            assert_eq!(v, OutputValue::Number(42.0));
        }

        #[test]
        fn test_coerce_number_float_string() {
            let v = coerce_output_value("n", "3.14", OutputType::Number).unwrap();
            assert!(matches!(v, OutputValue::Number(f) if (f - 3.14).abs() < 1e-9));
        }

        #[test]
        fn test_coerce_number_negative() {
            let v = coerce_output_value("n", "-7", OutputType::Number).unwrap();
            assert_eq!(v, OutputValue::Number(-7.0));
        }

        #[test]
        fn test_coerce_number_empty_is_null() {
            let v = coerce_output_value("n", "", OutputType::Number).unwrap();
            assert_eq!(v, OutputValue::Null);
        }

        #[test]
        fn test_coerce_number_non_numeric_is_hard_fail() {
            let err = coerce_output_value("n", "hello", OutputType::Number).unwrap_err();
            let msg = err.to_string();
            assert!(
                msg.contains("cannot be coerced to number") || msg.contains("number"),
                "error should mention coercion failure: {msg}"
            );
        }

        // --- bool coercion ---
        #[test]
        fn test_coerce_bool_true_string() {
            let v = coerce_output_value("b", "true", OutputType::Bool).unwrap();
            assert_eq!(v, OutputValue::Bool(true));
        }

        #[test]
        fn test_coerce_bool_false_string() {
            let v = coerce_output_value("b", "false", OutputType::Bool).unwrap();
            assert_eq!(v, OutputValue::Bool(false));
        }

        #[test]
        fn test_coerce_bool_empty_is_null() {
            let v = coerce_output_value("b", "", OutputType::Bool).unwrap();
            assert_eq!(v, OutputValue::Null);
        }

        #[test]
        fn test_coerce_bool_invalid_is_hard_fail() {
            let err = coerce_output_value("b", "yes", OutputType::Bool).unwrap_err();
            let msg = err.to_string();
            assert!(
                msg.contains("cannot be coerced to bool") || msg.contains("bool"),
                "error should mention coercion failure: {msg}"
            );
        }

        #[test]
        fn test_coerce_bool_1_is_hard_fail() {
            // Only "true"/"false" are accepted for bool; "1"/"0" are not.
            let err = coerce_output_value("b", "1", OutputType::Bool).unwrap_err();
            assert!(!err.to_string().is_empty(), "1 is not a valid bool string");
        }

        #[test]
        fn test_coerce_bool_true_is_case_sensitive() {
            // "True" (capital T) is not valid — only lowercase "true".
            let err = coerce_output_value("b", "True", OutputType::Bool).unwrap_err();
            assert!(
                !err.to_string().is_empty(),
                "True is not a valid bool string"
            );
        }
    }

    // --- Stage 2: null model projection ---

    mod null_model {
        use super::super::OutputValue;

        #[test]
        fn test_null_to_string_context_is_empty_string() {
            assert_eq!(OutputValue::Null.to_string_context(), "");
        }

        #[test]
        fn test_string_to_string_context() {
            let v = OutputValue::String("hello".into());
            assert_eq!(v.to_string_context(), "hello");
        }

        #[test]
        fn test_number_integer_to_string_context() {
            let v = OutputValue::Number(42.0);
            assert_eq!(v.to_string_context(), "42");
        }

        #[test]
        fn test_number_float_to_string_context() {
            let v = OutputValue::Number(3.5);
            assert_eq!(v.to_string_context(), "3.5");
        }

        #[test]
        fn test_bool_true_to_string_context() {
            let v = OutputValue::Bool(true);
            assert_eq!(v.to_string_context(), "true");
        }

        #[test]
        fn test_bool_false_to_string_context() {
            let v = OutputValue::Bool(false);
            assert_eq!(v.to_string_context(), "false");
        }

        #[test]
        fn test_null_is_not_zero_for_number() {
            // Null is the universal empty — not 0.0 for numbers.
            assert_ne!(OutputValue::Null, OutputValue::Number(0.0));
        }

        #[test]
        fn test_null_is_not_false_for_bool() {
            assert_ne!(OutputValue::Null, OutputValue::Bool(false));
        }

        #[test]
        fn test_null_is_not_empty_string() {
            // Null and String("") are different values; they only *project* the same way.
            assert_ne!(OutputValue::Null, OutputValue::String(String::new()));
        }
    }

    // --- Stage 2: capture_step_outputs ---

    mod capture {
        use super::super::{capture_step_outputs, OutputDecl, OutputType, OutputValue};

        fn decl(name: &str, t: OutputType, required: bool) -> OutputDecl {
            OutputDecl {
                name: name.to_string(),
                output_type: t,
                required,
            }
        }

        #[test]
        fn test_capture_empty_declarations_returns_empty() {
            let captured = capture_step_outputs("step", &[], "FOO=bar\n").unwrap();
            assert!(captured.is_empty());
        }

        #[test]
        fn test_capture_string_output() {
            let decls = vec![decl("result", OutputType::String, false)];
            let captured = capture_step_outputs("step", &decls, "result=hello\n").unwrap();
            assert_eq!(captured.len(), 1);
            assert_eq!(captured[0].name, "result");
            assert_eq!(captured[0].value, OutputValue::String("hello".to_string()));
        }

        #[test]
        fn test_capture_number_output() {
            let decls = vec![decl("count", OutputType::Number, false)];
            let captured = capture_step_outputs("step", &decls, "count=99\n").unwrap();
            assert_eq!(captured[0].value, OutputValue::Number(99.0));
        }

        #[test]
        fn test_capture_bool_output_true() {
            let decls = vec![decl("ok", OutputType::Bool, false)];
            let captured = capture_step_outputs("step", &decls, "ok=true\n").unwrap();
            assert_eq!(captured[0].value, OutputValue::Bool(true));
        }

        #[test]
        fn test_capture_not_emitted_becomes_null() {
            let decls = vec![decl("missing", OutputType::String, false)];
            let captured = capture_step_outputs("step", &decls, "").unwrap();
            assert_eq!(captured[0].value, OutputValue::Null);
        }

        #[test]
        fn test_capture_emitted_as_empty_becomes_null() {
            let decls = vec![decl("empty", OutputType::Number, false)];
            let captured = capture_step_outputs("step", &decls, "empty=\n").unwrap();
            assert_eq!(captured[0].value, OutputValue::Null);
        }

        #[test]
        fn test_capture_required_not_emitted_fails() {
            let decls = vec![decl("must_have", OutputType::String, true)];
            let err = capture_step_outputs("step", &decls, "").unwrap_err();
            let msg = err.to_string();
            assert!(
                msg.contains("required") && msg.contains("must_have"),
                "error should mention required output: {msg}"
            );
        }

        #[test]
        fn test_capture_required_emitted_empty_fails() {
            let decls = vec![decl("must_have", OutputType::String, true)];
            let err = capture_step_outputs("step", &decls, "must_have=\n").unwrap_err();
            assert!(err.to_string().contains("required"));
        }

        #[test]
        fn test_capture_required_emitted_non_empty_succeeds() {
            let decls = vec![decl("must_have", OutputType::String, true)];
            let captured = capture_step_outputs("step", &decls, "must_have=present\n").unwrap();
            assert_eq!(
                captured[0].value,
                OutputValue::String("present".to_string())
            );
        }

        #[test]
        fn test_capture_coercion_failure_is_hard_fail() {
            let decls = vec![decl("n", OutputType::Number, false)];
            let err = capture_step_outputs("step", &decls, "n=not-a-number\n").unwrap_err();
            let msg = err.to_string();
            assert!(
                msg.contains("coercion") || msg.contains("number"),
                "error should describe coercion failure: {msg}"
            );
        }

        #[test]
        fn test_capture_undeclared_emissions_are_ignored() {
            let decls = vec![decl("declared", OutputType::String, false)];
            let captured =
                capture_step_outputs("step", &decls, "declared=yes\nundeclared=ignored\n").unwrap();
            assert_eq!(captured.len(), 1);
            assert_eq!(captured[0].name, "declared");
        }

        #[test]
        fn test_capture_declared_type_stored_on_result() {
            let decls = vec![decl("count", OutputType::Number, false)];
            let captured = capture_step_outputs("step", &decls, "count=5\n").unwrap();
            assert_eq!(captured[0].declared_type, OutputType::Number);
        }

        #[test]
        fn test_capture_multiple_outputs() {
            let decls = vec![
                decl("name", OutputType::String, false),
                decl("count", OutputType::Number, false),
                decl("ready", OutputType::Bool, false),
            ];
            let contents = "name=alice\ncount=7\nready=true\n";
            let captured = capture_step_outputs("step", &decls, contents).unwrap();
            assert_eq!(captured.len(), 3);
            assert_eq!(captured[0].value, OutputValue::String("alice".to_string()));
            assert_eq!(captured[1].value, OutputValue::Number(7.0));
            assert_eq!(captured[2].value, OutputValue::Bool(true));
        }
    }
}
