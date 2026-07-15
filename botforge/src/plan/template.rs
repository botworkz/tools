use anyhow::{Context, Result};
use serde::Deserialize;
use serde_yaml::Value;
use std::collections::BTreeMap;
use std::path::Path;

use super::step::TestStep;

const DEFAULT_SENTINEL: &str = "__default__";

// ---------------------------------------------------------------------------
// Fragment input declaration types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(super) enum InputType {
    String,
    Number,
    Boolean,
}

#[derive(Debug, Deserialize)]
pub(super) struct InputDeclaration {
    #[serde(rename = "type")]
    pub(super) input_type: InputType,
    #[serde(default)]
    pub(super) required: bool,
    pub(super) default: Option<String>,
}

// ---------------------------------------------------------------------------
// `for:` expansion helpers
// ---------------------------------------------------------------------------

pub(super) fn expand_raw_step(step: Value) -> Result<Vec<TestStep>> {
    let mut mapping = match step {
        Value::Mapping(mapping) => mapping,
        _ => anyhow::bail!("step entry must be a mapping"),
    };

    // Extract step name for richer error context (best-effort; substituted values
    // used for for:-expanded steps).
    let name_key = Value::String("name".to_string());
    let step_name_template = mapping
        .get(&name_key)
        .and_then(|v| v.as_str())
        .unwrap_or("<unnamed>")
        .to_string();

    let for_key = Value::String("for".to_string());
    let Some(items_value) = mapping.remove(&for_key) else {
        let parsed: TestStep = serde_yaml::from_value(Value::Mapping(mapping))
            .with_context(|| format!("step '{step_name_template}': invalid step definition"))?;
        return Ok(vec![parsed]);
    };

    let body = Value::Mapping(mapping);
    let items = match items_value {
        Value::Sequence(items) => items,
        _ => anyhow::bail!("step `for:` must be a sequence"),
    };

    let mut expanded = Vec::new();
    for item in items {
        let args = resolve_for_args(&item)?;
        let mut concrete = body.clone();
        substitute_args_in_value(&mut concrete, &args)?;
        // Use the post-substitution name for the error context.
        let concrete_name = if let Value::Mapping(ref m) = concrete {
            m.get(&name_key)
                .and_then(|v| v.as_str())
                .unwrap_or(&step_name_template)
                .to_string()
        } else {
            step_name_template.clone()
        };
        expanded.push(
            serde_yaml::from_value::<TestStep>(concrete)
                .with_context(|| format!("step '{concrete_name}': invalid step definition"))?,
        );
    }
    Ok(expanded)
}

fn resolve_for_args(item: &Value) -> Result<BTreeMap<String, String>> {
    let mut args = BTreeMap::new();
    match item {
        Value::Sequence(values) => {
            for (index, value) in values.iter().enumerate() {
                args.insert(index.to_string(), scalar_value_to_string(value)?);
            }
        }
        Value::Mapping(entries) => {
            for (key, value) in entries {
                let name = match key {
                    Value::String(name) => name.clone(),
                    _ => anyhow::bail!("step `for:` mapping keys must be strings"),
                };
                args.insert(name, scalar_value_to_string(value)?);
            }
        }
        _ => {
            args.insert("0".to_string(), scalar_value_to_string(item)?);
        }
    }
    Ok(args)
}

fn scalar_value_to_string(value: &Value) -> Result<String> {
    match value {
        Value::String(text) => Ok(text.clone()),
        Value::Bool(flag) => Ok(flag.to_string()),
        Value::Number(number) => Ok(number.to_string()),
        Value::Null => Ok("null".to_string()),
        _ => anyhow::bail!("step `for:` values must be scalars, sequences, or mappings"),
    }
}

// ---------------------------------------------------------------------------
// Fragment input declaration extraction and resolution
// ---------------------------------------------------------------------------

pub(super) fn extract_fragment_input_declarations(
    value: &Value,
) -> Result<BTreeMap<String, InputDeclaration>> {
    let mapping = match value {
        Value::Mapping(m) => m,
        _ => return Ok(BTreeMap::new()),
    };
    let inputs_key = Value::String("inputs".to_string());
    match mapping.get(&inputs_key) {
        None => Ok(BTreeMap::new()),
        Some(inputs_value) => {
            let declarations: BTreeMap<String, InputDeclaration> =
                serde_yaml::from_value(inputs_value.clone())
                    .context("invalid inputs: declaration")?;
            Ok(declarations)
        }
    }
}

pub(super) fn resolve_fragment_inputs(
    path: &Path,
    declarations: &BTreeMap<String, InputDeclaration>,
    with: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, String>> {
    // Declaration-time validation: required: true + default: together is a contradiction.
    for (name, decl) in declarations {
        if decl.required && decl.default.is_some() {
            anyhow::bail!(
                "input '{}' cannot set both 'required: true' and 'default'",
                name
            );
        }
    }

    // Caller must not pass keys that the fragment has not declared.
    for key in with.keys() {
        if !declarations.contains_key(key.as_str()) {
            anyhow::bail!(
                "unexpected input '{}' not declared by fragment {}",
                key,
                path.display()
            );
        }
    }

    let mut resolved = BTreeMap::new();

    for (name, decl) in declarations {
        let caller_value = with.get(name.as_str());

        // Resolution pipeline:
        //   omitted key or "__default__" sentinel → declared default (or unset if none).
        //   any other value (including "") → take literally.
        let effective: Option<String> = match caller_value.map(String::as_str) {
            None | Some(DEFAULT_SENTINEL) => decl.default.clone(),
            Some(v) => Some(v.to_string()),
        };

        // Type-validate the resolved value (sentinel is already gone at this point).
        if let Some(ref v) = effective {
            validate_input_type(name, decl.input_type, v)?;
        }

        // Required check: unset + required → error.
        if effective.is_none() && decl.required {
            anyhow::bail!("missing required input '{}'", name);
        }

        if let Some(v) = effective {
            resolved.insert(name.clone(), v);
        }
    }

    Ok(resolved)
}

fn validate_input_type(name: &str, input_type: InputType, value: &str) -> Result<()> {
    match input_type {
        InputType::String => Ok(()),
        InputType::Number => value
            .parse::<f64>()
            .map(|_| ())
            .map_err(|_| anyhow::anyhow!("input '{}' must be a number", name)),
        InputType::Boolean => match value.to_ascii_lowercase().as_str() {
            "true" | "false" => Ok(()),
            _ => anyhow::bail!("input '{}' must be a boolean", name),
        },
    }
}

// ---------------------------------------------------------------------------
// `${{ }}` substitution engine
// ---------------------------------------------------------------------------

pub(super) fn substitute_inputs_in_value(
    value: &mut Value,
    inputs: &BTreeMap<String, String>,
) -> Result<()> {
    substitute_namespace_in_value(value, "inputs", inputs)
}

fn substitute_args_in_value(value: &mut Value, args: &BTreeMap<String, String>) -> Result<()> {
    substitute_namespace_in_value(value, "args", args)
}

fn substitute_namespace_in_value(
    value: &mut Value,
    namespace: &str,
    values: &BTreeMap<String, String>,
) -> Result<()> {
    match value {
        Value::String(text) => {
            *text = substitute_namespace_in_string(text, namespace, values)?;
        }
        Value::Sequence(items) => {
            for item in items {
                substitute_namespace_in_value(item, namespace, values)?;
            }
        }
        Value::Mapping(entries) => {
            for (_, value) in entries.iter_mut() {
                substitute_namespace_in_value(value, namespace, values)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn substitute_namespace_in_string(
    text: &str,
    namespace: &str,
    values: &BTreeMap<String, String>,
) -> Result<String> {
    let mut rendered = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("${{") {
        rendered.push_str(&rest[..start]);
        let after_open = &rest[start + 3..];
        let end = after_open
            .find("}}")
            .ok_or_else(|| anyhow::anyhow!("unterminated input expression in '{text}'"))?;
        let expr = after_open[..end].trim();
        let prefix = format!("{namespace}.");
        let placeholder = &rest[start..start + 3 + end + 2];
        let Some(name) = expr.strip_prefix(&prefix) else {
            if is_deferred_namespace_expression(namespace, expr) {
                rendered.push_str(placeholder);
                rest = &after_open[end + 2..];
                continue;
            }
            return Err(anyhow::anyhow!(
                "unsupported expression '${{{{{expr}}}}}'; only ${{{{ {namespace}.NAME }}}} is supported"
            ));
        };
        if !is_valid_namespace_name(name) {
            anyhow::bail!("invalid input name '{name}' in '{text}'");
        }
        let value = values
            .get(name)
            .ok_or_else(|| anyhow::anyhow!("missing required input '{name}'"))?;
        rendered.push_str(value);
        rest = &after_open[end + 2..];
    }
    rendered.push_str(rest);
    Ok(rendered)
}

fn is_deferred_namespace_expression(active_namespace: &str, expr: &str) -> bool {
    let Some((namespace, name)) = expr.split_once('.') else {
        return false;
    };
    namespace != active_namespace
        && is_deferred_namespace(active_namespace, namespace)
        && is_valid_namespace_name(name)
}

fn is_deferred_namespace(active_namespace: &str, namespace: &str) -> bool {
    matches!(
        (active_namespace, namespace),
        ("inputs", "args") | ("args", "inputs")
    )
}

fn is_valid_namespace_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::path::Path;

    mod inputs {
        use super::*;

        // --- fragment input contract (resolve_fragment_inputs unit tests) ---

        fn decl(input_type: InputType, required: bool, default: Option<&str>) -> InputDeclaration {
            InputDeclaration {
                input_type,
                required,
                default: default.map(str::to_string),
            }
        }

        fn dummy_path() -> &'static Path {
            Path::new("fragment.yaml")
        }

        fn with_map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect()
        }

        fn decl_map(pairs: &[(&str, InputDeclaration)]) -> BTreeMap<String, InputDeclaration> {
            pairs
                .iter()
                .map(|(k, d)| {
                    (
                        k.to_string(),
                        InputDeclaration {
                            input_type: d.input_type,
                            required: d.required,
                            default: d.default.clone(),
                        },
                    )
                })
                .collect()
        }

        #[test]
        fn test_resolve_inputs_declared_default_applied_when_caller_omits_key() {
            let declarations = decl_map(&[("shell", decl(InputType::String, false, Some("bash")))]);
            let with = BTreeMap::new();
            let resolved = resolve_fragment_inputs(dummy_path(), &declarations, &with).unwrap();
            assert_eq!(resolved.get("shell").map(String::as_str), Some("bash"));
        }

        #[test]
        fn test_resolve_inputs_default_sentinel_resolves_to_declared_default() {
            let declarations = decl_map(&[("shell", decl(InputType::String, false, Some("bash")))]);
            let with = with_map(&[("shell", "__default__")]);
            let resolved = resolve_fragment_inputs(dummy_path(), &declarations, &with).unwrap();
            assert_eq!(resolved.get("shell").map(String::as_str), Some("bash"));
        }

        #[test]
        fn test_resolve_inputs_default_sentinel_with_no_declared_default_yields_unset() {
            let declarations = decl_map(&[("target", decl(InputType::String, false, None))]);
            let with = with_map(&[("target", "__default__")]);
            let resolved = resolve_fragment_inputs(dummy_path(), &declarations, &with).unwrap();
            assert!(
                !resolved.contains_key("target"),
                "unset input must not appear in resolved map"
            );
        }

        #[test]
        fn test_resolve_inputs_empty_string_yields_empty_not_default() {
            let declarations = decl_map(&[("shell", decl(InputType::String, false, Some("bash")))]);
            let with = with_map(&[("shell", "")]);
            let resolved = resolve_fragment_inputs(dummy_path(), &declarations, &with).unwrap();
            assert_eq!(
                resolved.get("shell").map(String::as_str),
                Some(""),
                "empty string must not be replaced by the declared default"
            );
        }

        #[test]
        fn test_resolve_inputs_empty_string_satisfies_required() {
            let declarations = decl_map(&[("target", decl(InputType::String, true, None))]);
            let with = with_map(&[("target", "")]);
            let result = resolve_fragment_inputs(dummy_path(), &declarations, &with);
            assert!(
                result.is_ok(),
                "empty string must satisfy required: {result:?}"
            );
            assert_eq!(result.unwrap().get("target").map(String::as_str), Some(""));
        }

        #[test]
        fn test_resolve_inputs_number_type_valid() {
            let declarations = decl_map(&[("count", decl(InputType::Number, false, None))]);
            let with = with_map(&[("count", "42")]);
            assert!(resolve_fragment_inputs(dummy_path(), &declarations, &with).is_ok());
        }

        #[test]
        fn test_resolve_inputs_number_type_valid_float() {
            let declarations = decl_map(&[("ratio", decl(InputType::Number, false, None))]);
            let with = with_map(&[("ratio", "3.14")]);
            assert!(resolve_fragment_inputs(dummy_path(), &declarations, &with).is_ok());
        }

        #[test]
        fn test_resolve_inputs_number_type_invalid() {
            let declarations = decl_map(&[("count", decl(InputType::Number, false, None))]);
            let with = with_map(&[("count", "not-a-number")]);
            let err = resolve_fragment_inputs(dummy_path(), &declarations, &with).unwrap_err();
            let msg = err.to_string();
            assert!(
                msg.contains("count") && msg.contains("number"),
                "error must name the input and type: {msg}"
            );
        }

        #[test]
        fn test_resolve_inputs_boolean_type_valid() {
            let declarations = decl_map(&[("flag", decl(InputType::Boolean, false, None))]);
            for v in &["true", "false", "True", "FALSE"] {
                let with = with_map(&[("flag", v)]);
                assert!(
                    resolve_fragment_inputs(dummy_path(), &declarations, &with).is_ok(),
                    "expected valid boolean for '{v}'"
                );
            }
        }

        #[test]
        fn test_resolve_inputs_boolean_type_invalid() {
            let declarations = decl_map(&[("flag", decl(InputType::Boolean, false, None))]);
            let with = with_map(&[("flag", "yes")]);
            let err = resolve_fragment_inputs(dummy_path(), &declarations, &with).unwrap_err();
            let msg = err.to_string();
            assert!(
                msg.contains("flag") && msg.contains("boolean"),
                "error must name the input and type: {msg}"
            );
        }

        #[test]
        fn test_resolve_inputs_default_sentinel_on_typed_input_is_not_parse_error() {
            // "__default__" on a number/boolean input resolves to the declared default,
            // not parsed as the type — so it must never produce a type error.
            let declarations = decl_map(&[("count", decl(InputType::Number, false, Some("10")))]);
            let with = with_map(&[("count", "__default__")]);
            let resolved = resolve_fragment_inputs(dummy_path(), &declarations, &with).unwrap();
            assert_eq!(resolved.get("count").map(String::as_str), Some("10"));
        }

        #[test]
        fn test_resolve_inputs_undeclared_with_key_errors() {
            let declarations = BTreeMap::new();
            let with = with_map(&[("unknown", "value")]);
            let err = resolve_fragment_inputs(dummy_path(), &declarations, &with).unwrap_err();
            let msg = err.to_string();
            assert!(
                msg.contains("unknown"),
                "error must name the undeclared key: {msg}"
            );
        }

        #[test]
        fn test_resolve_inputs_missing_required_when_omitted() {
            let declarations = decl_map(&[("target", decl(InputType::String, true, None))]);
            let with = BTreeMap::new();
            let err = resolve_fragment_inputs(dummy_path(), &declarations, &with).unwrap_err();
            let msg = err.to_string();
            assert!(
                msg.contains("missing required input 'target'"),
                "unexpected error: {msg}"
            );
        }

        #[test]
        fn test_resolve_inputs_declaration_required_and_default_contradiction() {
            let declarations = decl_map(&[("shell", decl(InputType::String, true, Some("bash")))]);
            let with = BTreeMap::new();
            let err = resolve_fragment_inputs(dummy_path(), &declarations, &with).unwrap_err();
            let msg = err.to_string();
            assert!(
                msg.contains("shell") && msg.contains("required") && msg.contains("default"),
                "error must describe the contradiction: {msg}"
            );
        }

        // --- `with:` at call site (integration tests via load_test_config) ---

        #[test]
        fn test_load_test_config_with_at_call_site_accepted() {
            use crate::plan::config::load_test_config;
            use crate::plan::step::{RunStep, TestStep};
            use tempfile::TempDir;

            fn run_ref(step: &TestStep) -> &RunStep {
                let TestStep::Run(step) = step else {
                    panic!("expected run step");
                };
                step
            }

            let repo = TempDir::new().unwrap();
            std::fs::create_dir_all(repo.path().join("shared")).unwrap();
            std::fs::write(
                repo.path().join("shared/frag.yaml"),
                r#"
type: botforge/fragment
inputs:
  msg:
    type: string
    required: true
steps:
  - on: guest
    name: "${{ inputs.msg }}"
    run: "echo ok"
"#,
            )
            .unwrap();
            std::fs::write(
                repo.path().join("test.yaml"),
                r#"
type: botforge/test
name: test
steps:
  - uses: "@://shared/frag.yaml"
    with:
      msg: hello
"#,
            )
            .unwrap();

            let config = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap();
            assert_eq!(config.steps.len(), 1);
            assert_eq!(run_ref(&config.steps[0]).name, "hello");
        }

        #[test]
        fn test_load_test_config_inputs_at_call_site_is_rejected() {
            use crate::plan::config::load_test_config;
            use tempfile::TempDir;

            let repo = TempDir::new().unwrap();
            std::fs::create_dir_all(repo.path().join("shared")).unwrap();
            std::fs::write(
                repo.path().join("shared/frag.yaml"),
                r#"
type: botforge/fragment
inputs:
  msg:
    type: string
    required: true
steps:
  - on: guest
    name: "${{ inputs.msg }}"
    run: "echo ok"
"#,
            )
            .unwrap();
            std::fs::write(
                repo.path().join("test.yaml"),
                r#"
type: botforge/test
name: test
steps:
  - uses: "@://shared/frag.yaml"
    inputs:
      msg: hello
"#,
            )
            .unwrap();

            // `inputs:` at the call site is not a recognized field; the step must fail to parse.
            assert!(
                load_test_config(repo.path(), &repo.path().join("test.yaml")).is_err(),
                "`inputs:` at call site must be rejected"
            );
        }

        #[test]
        fn test_load_test_config_declared_default_applied_via_fragment() {
            use crate::plan::config::load_test_config;
            use crate::plan::step::{RunStep, TestStep};
            use tempfile::TempDir;

            fn run_ref(step: &TestStep) -> &RunStep {
                let TestStep::Run(step) = step else {
                    panic!("expected run step");
                };
                step
            }

            let repo = TempDir::new().unwrap();
            std::fs::create_dir_all(repo.path().join("shared")).unwrap();
            std::fs::write(
                repo.path().join("shared/frag.yaml"),
                r#"
type: botforge/fragment
inputs:
  shell:
    type: string
    default: bash
steps:
  - on: guest
    name: step
    shell: ${{ inputs.shell }}
    run: "echo ok"
"#,
            )
            .unwrap();
            std::fs::write(
                repo.path().join("test.yaml"),
                r#"
type: botforge/test
name: test
steps:
  - uses: "@://shared/frag.yaml"
"#,
            )
            .unwrap();

            let config = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap();
            assert_eq!(run_ref(&config.steps[0]).shell.as_deref(), Some("bash"));
        }

        #[test]
        fn test_load_test_config_undeclared_with_key_errors() {
            use crate::plan::config::load_test_config;
            use tempfile::TempDir;

            let repo = TempDir::new().unwrap();
            std::fs::create_dir_all(repo.path().join("shared")).unwrap();
            std::fs::write(
                repo.path().join("shared/frag.yaml"),
                r#"
type: botforge/fragment
steps:
  - on: guest
    name: step
    run: "echo ok"
"#,
            )
            .unwrap();
            std::fs::write(
                repo.path().join("test.yaml"),
                r#"
type: botforge/test
name: test
steps:
  - uses: "@://shared/frag.yaml"
    with:
      undeclared: value
"#,
            )
            .unwrap();

            let err = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap_err();
            assert!(
                format!("{err:#}").contains("undeclared"),
                "error must mention the undeclared key: {err:#}"
            );
        }
    }
}
