use anyhow::Result;
use serde::de::{self, Deserializer};
use serde::Deserialize;
use serde_yaml::Value;

/// Where a test step executes: inside the guest (SSH) or on the harness host (local).
#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum StepTarget {
    /// Run via SSH inside the guest VM.
    Guest,
    /// Run locally in the botforge container (harness), reaching the guest only via forwarded
    /// `ports:`. This is the botforge container / harness where botforge itself runs — not the
    /// CI runner host.
    Host,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RunStep {
    /// Where this step executes. Required; must be `guest` or `host`.
    #[serde(rename = "on")]
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

#[derive(Debug, Deserialize, Clone, PartialEq, Eq, Default)]
#[serde(deny_unknown_fields)]
pub(crate) struct TopLevelUpload {
    pub(crate) src: String,
    pub(crate) dest: String,
    /// File permission mode (3–4 octal digits). Defaults to `"0644"` at install time.
    #[serde(default)]
    pub(crate) mode: Option<String>,
    /// Owner (user name or numeric uid) to pass to `install -o`. Defaults to `root`.
    #[serde(default)]
    pub(crate) owner: Option<String>,
    /// Group (group name or numeric gid) to pass to `install -g`. Defaults to `root`.
    #[serde(default)]
    pub(crate) group: Option<String>,
    /// When `false`, the install fails with a hard error if `dest` already exists.
    /// Defaults to `true` (overwrite is allowed).
    #[serde(default)]
    pub(crate) overwrite: Option<bool>,
    /// When `true` (default), create intermediate destination directories (`install -D`).
    /// When `false`, the parent directory must already exist.
    #[serde(default)]
    pub(crate) parents: Option<bool>,
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
    use super::resolve_shell;

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
}
