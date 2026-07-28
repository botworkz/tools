use anyhow::{Context, Result};
use serde::Deserialize;
use serde_yaml::Value;
use std::collections::BTreeMap;
use std::path::Path;

use crate::step::TestStep;

mod eval;
mod lexer;
mod parser;
mod value;

use eval::evaluate_expression_span;
use value::{EvaluatedSpan, EvaluatedValue};

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
    /// Native-typed YAML default value for this input. Must match the declared `input_type`:
    /// `boolean` inputs require a YAML bool, `number` inputs a YAML number, `string` a string.
    /// A string default for a `boolean`/`number` input is a hard load-time error (R2).
    pub(super) default: Option<Value>,
}

/// Map of resolved input values, carrying a typed [`EvaluatedValue`] per input ready for the
/// expression evaluator. Values are validated against their declared [`InputType`] once at
/// resolution time; no late re-parsing occurs at evaluation time.
pub(super) type TypedInputMap = BTreeMap<std::string::String, EvaluatedValue>;

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
        let mut concrete = Value::Mapping(mapping);
        // Even without `for:`, run the expression pass so `${{ ... }}` literals
        // (including boolean gating expressions) are evaluated consistently.
        substitute_args_in_value(&mut concrete, &BTreeMap::new())?;
        // Invariant A: after the final (args) pass, no ${{ }} may remain.
        check_no_residual_expressions(&concrete)
            .with_context(|| format!("step '{step_name_template}'"))?;
        let parsed: TestStep = serde_yaml::from_value(concrete)
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
        // Invariant A: after the final (args) pass, no ${{ }} may remain.
        check_no_residual_expressions(&concrete)
            .with_context(|| format!("step '{step_name_template}'"))?;
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

fn resolve_for_args(item: &Value) -> Result<BTreeMap<std::string::String, std::string::String>> {
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

fn scalar_value_to_string(value: &Value) -> Result<std::string::String> {
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
) -> Result<BTreeMap<std::string::String, InputDeclaration>> {
    let mapping = match value {
        Value::Mapping(m) => m,
        _ => return Ok(BTreeMap::new()),
    };
    let inputs_key = Value::String("inputs".to_string());
    match mapping.get(&inputs_key) {
        None => Ok(BTreeMap::new()),
        Some(inputs_value) => {
            let declarations: BTreeMap<std::string::String, InputDeclaration> =
                serde_yaml::from_value(inputs_value.clone())
                    .context("invalid inputs: declaration")?;
            Ok(declarations)
        }
    }
}

/// Resolve declared input values from a `with:` call-site map, returning a
/// [`TypedInputMap`] that maps each input name to its fully typed [`EvaluatedValue`].
///
/// Enforces all input-boundary rules:
/// - R1: every input must have `default:` or `required: true`; neither is a hard error.
/// - R2: values must be native-typed (YAML bool for `boolean`, YAML number for `number`);
///   string values for `boolean`/`number` inputs are hard errors.
/// - R3: `number` values must be finite (`inf`/`nan` rejected).
/// - R4: the `__default__` sentinel string is honored for ALL input types, checked
///   BEFORE native type-validation so it is never rejected as a wrong type.
pub(super) fn resolve_fragment_inputs(
    path: &Path,
    declarations: &BTreeMap<std::string::String, InputDeclaration>,
    with: &BTreeMap<std::string::String, Value>,
) -> Result<TypedInputMap> {
    // Declaration-time validation.
    for (name, decl) in declarations {
        // R1: every input must have default: or required: true.
        if !decl.required && decl.default.is_none() {
            anyhow::bail!(
                "input '{}' must set 'required: true' or provide a 'default:'",
                name
            );
        }
        // Contradiction: required: true + default: together.
        if decl.required && decl.default.is_some() {
            anyhow::bail!(
                "input '{}' cannot set both 'required: true' and 'default'",
                name
            );
        }
        // R2/R3: eagerly validate the declared default value type; a bad default is
        // always a hard error even when a valid with: value is supplied.
        if let Some(ref default_val) = decl.default {
            yaml_value_to_evaluated(name, decl.input_type, default_val)
                .map_err(|e| anyhow::anyhow!("input '{}': invalid default value: {}", name, e))?;
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

    let mut resolved = TypedInputMap::new();

    for (name, decl) in declarations {
        let caller_value = with.get(name.as_str());

        // R4: check for the __default__ sentinel FIRST, before native type-validation.
        // `with: { flag: __default__ }` means "use declared default" for ALL input types
        // and must NOT be rejected as "string given to boolean/number".
        let is_sentinel = matches!(
            caller_value,
            Some(Value::String(s)) if s == DEFAULT_SENTINEL
        );

        // Resolution pipeline:
        //   omitted key or __default__ sentinel → use declared default.
        //   any other value → take literally with type validation.
        let effective: Option<EvaluatedValue> = if caller_value.is_none() || is_sentinel {
            // Use declared default (already type-validated in the declaration pass above).
            decl.default.as_ref().map(|v| {
                yaml_value_to_evaluated(name, decl.input_type, v)
                    .expect("default already validated in declaration pass")
            })
        } else if let Some(v) = caller_value {
            Some(
                yaml_value_to_evaluated(name, decl.input_type, v)
                    .map_err(|e| anyhow::anyhow!("input '{}' in 'with:' block: {}", name, e))?,
            )
        } else {
            None
        };

        // Required check: unset + required → error.
        if effective.is_none() && decl.required {
            anyhow::bail!("missing required input '{}'", name);
        }

        if let Some(ev) = effective {
            resolved.insert(name.clone(), ev);
        }
    }

    Ok(resolved)
}

/// Convert a native YAML `Value` to a typed [`EvaluatedValue`] per the declared [`InputType`].
///
/// Enforces native-types-only (R2):
/// - `boolean` inputs accept only YAML `Bool`; any string (including `"true"`/`"false"`)
///   is a hard error — use unquoted `true`/`false` in YAML.
/// - `number` inputs accept only YAML `Number` (finite); any string is a hard error.
/// - `string` inputs accept only YAML `String`.
///
/// The `__default__` sentinel must be stripped by the caller before invoking this function.
fn yaml_value_to_evaluated(
    name: &str,
    input_type: InputType,
    value: &Value,
) -> Result<EvaluatedValue> {
    match input_type {
        InputType::String => match value {
            Value::String(s) => Ok(EvaluatedValue::String(s.clone())),
            other => anyhow::bail!(
                "input '{}': expected a string value, got {}",
                name,
                yaml_type_name(other)
            ),
        },
        InputType::Boolean => match value {
            Value::Bool(b) => Ok(EvaluatedValue::Bool(*b)),
            other => anyhow::bail!(
                "input '{}': expected a native boolean value (true or false), got {}; \
                 use an unquoted YAML boolean (true/false), not a quoted string",
                name,
                yaml_type_name(other)
            ),
        },
        InputType::Number => match value {
            Value::Number(n) => {
                let f = n.as_f64().ok_or_else(|| {
                    anyhow::anyhow!("input '{}': number value is out of range", name)
                })?;
                validate_number(name, f)?;
                Ok(EvaluatedValue::Number(f))
            }
            other => anyhow::bail!(
                "input '{}': expected a native number value, got {}; \
                 use an unquoted YAML number, not a quoted string",
                name,
                yaml_type_name(other)
            ),
        },
    }
}

/// Validate a `f64` number: must be finite (rejects `inf`, `-inf`, `nan`).
///
/// Extension point (R3): range checks (`min`/`max`) can be added here without a rewrite.
/// Today only finiteness is checked; the name parameter is used for error context.
fn validate_number(name: &str, value: f64) -> Result<()> {
    if !value.is_finite() {
        anyhow::bail!("input '{}': number must be finite, got {}", name, value);
    }
    Ok(())
}

fn yaml_type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Sequence(_) => "sequence",
        Value::Mapping(_) => "mapping",
        Value::Tagged(_) => "tagged value",
    }
}

// ---------------------------------------------------------------------------
// `${{ }}` substitution engine
// ---------------------------------------------------------------------------

/// Substitute `${{ inputs.NAME }}` expressions in `value`, resolving each reference
/// using the declared type from `typed_inputs`.
///
/// After this pass, any `${{ args.NAME }}` references are preserved as-is (deferred)
/// for the subsequent [`substitute_args_in_value`] call.
pub(super) fn substitute_inputs_in_value(
    value: &mut Value,
    typed_inputs: &TypedInputMap,
) -> Result<()> {
    substitute_namespace_in_value(value, "inputs", typed_inputs, &BTreeMap::new(), false)
}

/// Substitute `${{ args.NAME }}` expressions in `value`.
///
/// `args.*` references have no type declaration and are always resolved as strings.
/// This is the final substitution pass; after it completes, every `${{ }}` span
/// must be resolved (no `Deferred` spans should remain).
pub(super) fn substitute_args_in_value(
    value: &mut Value,
    args: &BTreeMap<std::string::String, std::string::String>,
) -> Result<()> {
    substitute_namespace_in_value(value, "args", &TypedInputMap::new(), args, false)
}

/// Substitute expressions in `value`, with `preserve_typed` controlling how pure
/// `Number`/`Bool` results are emitted (R5).
///
/// - `preserve_typed = false` (default for all string-context fields): a pure expression
///   yielding `Number` or `Bool` is rendered as a YAML `String` via
///   `to_interpolated_string()`, so that string fields (`name:`, `run:`, `shell:`, etc.)
///   accept the result without a type-mismatch deserialisation error.
/// - `preserve_typed = true` (used for `timeout:` which needs a YAML integer): the typed
///   `to_yaml_value()` result is kept so that integer-expecting fields deserialise correctly.
///
/// The `if:` field is handled separately via [`substitute_if_condition_in_value`] and is
/// NOT affected by this parameter.
fn substitute_namespace_in_value(
    value: &mut Value,
    active_namespace: &str,
    typed_inputs: &TypedInputMap,
    args: &BTreeMap<std::string::String, std::string::String>,
    preserve_typed: bool,
) -> Result<()> {
    match value {
        Value::String(text) => {
            // For a single pure expression (`${{ expr }}` with no surrounding text):
            //   - `preserve_typed = true` (e.g. timeout:): emit the typed YAML scalar.
            //   - `preserve_typed = false` (all other string-context fields): coerce
            //     Number/Bool to a YAML String so string fields deserialise correctly (R5).
            // For mixed-content strings (surrounding text), always use string interpolation.
            let text_clone = text.clone();
            match try_substitute_pure_expression(&text_clone, active_namespace, typed_inputs, args)?
            {
                PureResult::Typed(ev) => {
                    *value = if preserve_typed {
                        ev.to_yaml_value()
                    } else {
                        // String-context fields: Number/Bool → String; String/Empty unchanged.
                        Value::String(ev.to_interpolated_string())
                    };
                }
                PureResult::Deferred => {
                    // Leave the placeholder intact for the next substitution pass.
                }
                PureResult::NotPure => {
                    *text = substitute_namespace_in_string(
                        &text_clone,
                        active_namespace,
                        typed_inputs,
                        args,
                    )?;
                }
            }
        }
        Value::Sequence(items) => {
            for item in items {
                substitute_namespace_in_value(
                    item,
                    active_namespace,
                    typed_inputs,
                    args,
                    preserve_typed,
                )?;
            }
        }
        Value::Mapping(entries) => {
            for (key, val) in entries.iter_mut() {
                match key.as_str() {
                    // `if:` requires typed Bool evaluation so that a boolean-false expression
                    // produces `Bool(false)` (falsy) rather than `String("false")` (truthy).
                    Some("if") => {
                        substitute_if_condition_in_value(
                            val,
                            active_namespace,
                            typed_inputs,
                            args,
                        )?;
                    }
                    // `timeout:` expects a YAML integer; keep the typed scalar so
                    // `SecondsValue::Integer` deserialises correctly.
                    Some("timeout") => {
                        substitute_namespace_in_value(
                            val,
                            active_namespace,
                            typed_inputs,
                            args,
                            true,
                        )?;
                    }
                    // All other fields are string-context: Number/Bool coerce to String (R5).
                    _ => {
                        substitute_namespace_in_value(
                            val,
                            active_namespace,
                            typed_inputs,
                            args,
                            false,
                        )?;
                    }
                }
            }
        }
        _ => {}
    }
    Ok(())
}

/// Result of attempting to evaluate a value as a pure single-expression.
enum PureResult {
    /// A single `${{ expr }}` was fully evaluated; use `EvaluatedValue::to_yaml_value()`.
    Typed(EvaluatedValue),
    /// A single `${{ expr }}` was encountered but references an inactive namespace.
    Deferred,
    /// Not a pure single expression (has surrounding text, or no expression at all).
    NotPure,
}

/// Check if `text` is a pure `${{ expr }}` (no surrounding text) and evaluate it.
fn try_substitute_pure_expression(
    text: &str,
    active_namespace: &str,
    typed_inputs: &TypedInputMap,
    args: &BTreeMap<std::string::String, std::string::String>,
) -> Result<PureResult> {
    let Some(expr) = extract_pure_expression(text) else {
        return Ok(PureResult::NotPure);
    };
    match evaluate_expression_span(expr, active_namespace, typed_inputs, args)? {
        EvaluatedSpan::Value(ev) => Ok(PureResult::Typed(ev)),
        EvaluatedSpan::Deferred => Ok(PureResult::Deferred),
    }
}

/// If the entire `text` is a single `${{ expr }}` with nothing outside it, return
/// the trimmed inner expression string. Otherwise return `None`.
fn extract_pure_expression(text: &str) -> Option<&str> {
    if let Some(rest) = text.strip_prefix("${{") {
        if let Some(end) = rest.find("}}") {
            if rest[end + 2..].is_empty() {
                return Some(rest[..end].trim());
            }
        }
    }
    None
}

/// Handle the `if:` field with typed evaluation.
///
/// When the value is a pure `${{ expr }}`:
/// - Evaluate to a typed `EvaluatedValue`
/// - Apply truthiness → store as `Bool(result)` so that `deserialize_step_condition`
///   receives a YAML bool, not a string.
///
/// For non-expression values (plain YAML bool/string/number) or mixed-content strings,
/// fall through to normal string substitution; `deserialize_step_condition` handles them.
fn substitute_if_condition_in_value(
    value: &mut Value,
    active_namespace: &str,
    typed_inputs: &TypedInputMap,
    args: &BTreeMap<std::string::String, std::string::String>,
) -> Result<()> {
    if let Value::String(text) = value {
        if let Some(expr) = extract_pure_expression(text) {
            match evaluate_expression_span(expr, active_namespace, typed_inputs, args)? {
                EvaluatedSpan::Value(ev) => {
                    *value = Value::Bool(ev.truthy());
                    return Ok(());
                }
                EvaluatedSpan::Deferred => {
                    // Leave placeholder intact.
                    return Ok(());
                }
            }
        }
        // Mixed-content `if:` value (has surrounding text): string interpolation.
        let text_clone = text.clone();
        *text = substitute_namespace_in_string(&text_clone, active_namespace, typed_inputs, args)?;
    }
    // Already a typed YAML value (bool/number/null); leave for deserialize_step_condition.
    Ok(())
}

/// Interpolate all `${{ expr }}` spans in `text` into a string.
///
/// This is the **string-interpolation path** (used when there is surrounding text).
/// Values are rendered via `EvaluatedValue::to_interpolated_string()` (faithful,
/// not truthiness-collapsed). Only `Empty` yields `""`.
fn substitute_namespace_in_string(
    text: &str,
    active_namespace: &str,
    typed_inputs: &TypedInputMap,
    args: &BTreeMap<std::string::String, std::string::String>,
) -> Result<std::string::String> {
    let mut rendered = std::string::String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("${{") {
        rendered.push_str(&rest[..start]);
        let after_open = &rest[start + 3..];
        let end = after_open
            .find("}}")
            .ok_or_else(|| anyhow::anyhow!("unterminated input expression in '{text}'"))?;
        let expr = after_open[..end].trim();
        let placeholder = &rest[start..start + 3 + end + 2];
        match evaluate_expression_span(expr, active_namespace, typed_inputs, args)
            .with_context(|| format!("while evaluating expression '${{{{{expr}}}}}' in '{text}'"))?
        {
            EvaluatedSpan::Value(value) => rendered.push_str(&value.to_interpolated_string()),
            EvaluatedSpan::Deferred => rendered.push_str(placeholder),
        }
        rest = &after_open[end + 2..];
    }
    rendered.push_str(rest);
    Ok(rendered)
}

/// Scan `value` recursively for any unresolved `${{ }}` expression.
///
/// # Invariant (A): No `${{ }}` may remain after the final substitution pass.
/// After the final (`args`) substitution pass every `${{ }}` placeholder must have
/// been evaluated to a concrete typed value. Any surviving placeholder is a hard
/// load-time error. Typical causes:
/// - An expression referencing an unknown namespace (usually caught earlier).
/// - A cross-namespace expression (`${{ inputs.x || args.y }}`) where the inputs
///   side was falsy and the args side couldn't be evaluated during the inputs pass
///   — the two-pass deferral means it can't be resolved.
/// - A top-level step (not from a fragment) that mistakenly uses `inputs.*` refs.
///
/// Callers MUST invoke this immediately after [`substitute_args_in_value`].
fn check_no_residual_expressions(value: &Value) -> Result<()> {
    match value {
        Value::String(text) => {
            if let Some(start) = text.find("${{") {
                let snippet = match text[start..].find("}}") {
                    Some(offset) => &text[start..start + offset + 2],
                    None => &text[start..],
                };
                anyhow::bail!(
                    "unresolved expression after final substitution pass: '{}'; \
                     check for unknown namespaces, typos, or cross-namespace \
                     expressions that cannot be evaluated in a single pass",
                    snippet
                );
            }
            Ok(())
        }
        Value::Sequence(items) => items.iter().try_for_each(check_no_residual_expressions),
        Value::Mapping(entries) => entries.values().try_for_each(check_no_residual_expressions),
        _ => Ok(()),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    // -----------------------------------------------------------------------
    // Test helpers
    // -----------------------------------------------------------------------

    fn map(pairs: &[(&str, &str)]) -> BTreeMap<std::string::String, std::string::String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    /// Build a `TypedInputMap` from `(key, string-repr, type)` triples.
    /// Converts the string representation to the appropriate `EvaluatedValue`.
    fn typed_map(pairs: &[(&str, &str, InputType)]) -> TypedInputMap {
        pairs
            .iter()
            .map(|(k, v, t)| {
                let ev = match t {
                    InputType::Boolean => EvaluatedValue::Bool(v.eq_ignore_ascii_case("true")),
                    InputType::Number => EvaluatedValue::Number(v.parse::<f64>().unwrap_or(0.0)),
                    InputType::String => EvaluatedValue::String(v.to_string()),
                };
                (k.to_string(), ev)
            })
            .collect()
    }

    /// Substitute inputs treating all as `String` type (for backward-compat tests).
    fn substitute_inputs(text: &str, pairs: &[(&str, &str)]) -> std::string::String {
        let tm: TypedInputMap = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), EvaluatedValue::String(v.to_string())))
            .collect();
        substitute_namespace_in_string(text, "inputs", &tm, &BTreeMap::new()).unwrap()
    }

    fn substitute_inputs_err(text: &str, pairs: &[(&str, &str)]) -> std::string::String {
        let tm: TypedInputMap = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), EvaluatedValue::String(v.to_string())))
            .collect();
        let err =
            substitute_namespace_in_string(text, "inputs", &tm, &BTreeMap::new()).unwrap_err();
        format!("{err:#}")
    }

    fn substitute_args(text: &str, pairs: &[(&str, &str)]) -> std::string::String {
        substitute_namespace_in_string(text, "args", &TypedInputMap::new(), &map(pairs)).unwrap()
    }

    // -----------------------------------------------------------------------
    // Fragment input resolution tests
    // -----------------------------------------------------------------------

    mod inputs {
        use super::*;

        /// Parse a YAML scalar string into a `serde_yaml::Value`.
        /// e.g. `yaml("42")` → `Value::Number(42)`, `yaml("true")` → `Value::Bool(true)`.
        fn yaml(s: &str) -> Value {
            serde_yaml::from_str(s).unwrap()
        }

        fn decl(input_type: InputType, required: bool, default: Option<Value>) -> InputDeclaration {
            InputDeclaration {
                input_type,
                required,
                default,
            }
        }

        fn dummy_path() -> &'static Path {
            Path::new("fragment.yaml")
        }

        /// Build a `with:` map with all `Value::String` values (for string inputs or sentinel).
        fn with_map(pairs: &[(&str, &str)]) -> BTreeMap<std::string::String, Value> {
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), Value::String(v.to_string())))
                .collect()
        }

        /// Build a `with:` map with explicit `Value`s (for native-typed inputs).
        fn with_val(pairs: &[(&str, Value)]) -> BTreeMap<std::string::String, Value> {
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.clone()))
                .collect()
        }

        fn decl_map(
            pairs: &[(&str, InputDeclaration)],
        ) -> BTreeMap<std::string::String, InputDeclaration> {
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
            let declarations =
                decl_map(&[("shell", decl(InputType::String, false, Some(yaml("bash"))))]);
            let with = with_map(&[]);
            let resolved = resolve_fragment_inputs(dummy_path(), &declarations, &with).unwrap();
            assert_eq!(
                resolved.get("shell"),
                Some(&EvaluatedValue::String("bash".to_string()))
            );
        }

        #[test]
        fn test_resolve_inputs_default_sentinel_resolves_to_declared_default() {
            let declarations =
                decl_map(&[("shell", decl(InputType::String, false, Some(yaml("bash"))))]);
            let with = with_map(&[("shell", "__default__")]);
            let resolved = resolve_fragment_inputs(dummy_path(), &declarations, &with).unwrap();
            assert_eq!(
                resolved.get("shell"),
                Some(&EvaluatedValue::String("bash".to_string()))
            );
        }

        /// R1: input with neither `default:` nor `required: true` is a hard load-time error.
        #[test]
        fn test_resolve_inputs_r1_neither_default_nor_required_is_error() {
            let declarations = decl_map(&[("target", decl(InputType::String, false, None))]);
            let with = with_map(&[]);
            let err = resolve_fragment_inputs(dummy_path(), &declarations, &with).unwrap_err();
            let msg = err.to_string();
            assert!(
                msg.contains("target") && (msg.contains("required") || msg.contains("default")),
                "R1 error must name the input and mention required/default: {msg}"
            );
        }

        /// R4: `__default__` sentinel on a `boolean` input uses the declared default without
        /// triggering a type error ("string given to boolean").
        #[test]
        fn test_resolve_inputs_default_sentinel_on_boolean_input_uses_default() {
            let declarations =
                decl_map(&[("flag", decl(InputType::Boolean, false, Some(yaml("false"))))]);
            // Passing the string "__default__" must use the declared Bool(false) default.
            let with = with_map(&[("flag", "__default__")]);
            let resolved = resolve_fragment_inputs(dummy_path(), &declarations, &with).unwrap();
            assert_eq!(
                resolved.get("flag"),
                Some(&EvaluatedValue::Bool(false)),
                "__default__ on boolean must use declared default, not fail type check"
            );
        }

        /// R4: `__default__` sentinel on a `number` input uses the declared default.
        #[test]
        fn test_resolve_inputs_default_sentinel_on_number_input_uses_default() {
            let declarations =
                decl_map(&[("count", decl(InputType::Number, false, Some(yaml("10"))))]);
            let with = with_map(&[("count", "__default__")]);
            let resolved = resolve_fragment_inputs(dummy_path(), &declarations, &with).unwrap();
            assert_eq!(
                resolved.get("count"),
                Some(&EvaluatedValue::Number(10.0)),
                "__default__ on number must use declared default"
            );
        }

        #[test]
        fn test_resolve_inputs_empty_string_yields_empty_not_default() {
            let declarations =
                decl_map(&[("shell", decl(InputType::String, false, Some(yaml("bash"))))]);
            let with = with_map(&[("shell", "")]);
            let resolved = resolve_fragment_inputs(dummy_path(), &declarations, &with).unwrap();
            assert_eq!(
                resolved.get("shell"),
                Some(&EvaluatedValue::String("".to_string())),
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
            assert_eq!(
                result.unwrap().get("target"),
                Some(&EvaluatedValue::String("".to_string()))
            );
        }

        /// R2: native YAML integer for `number` input → OK.
        #[test]
        fn test_resolve_inputs_number_type_valid_native_int() {
            let declarations = decl_map(&[("count", decl(InputType::Number, true, None))]);
            let with = with_val(&[("count", yaml("42"))]);
            let resolved = resolve_fragment_inputs(dummy_path(), &declarations, &with).unwrap();
            assert_eq!(resolved.get("count"), Some(&EvaluatedValue::Number(42.0)));
        }

        /// R2: native YAML float for `number` input → OK.
        #[test]
        fn test_resolve_inputs_number_type_valid_native_float() {
            let declarations = decl_map(&[("ratio", decl(InputType::Number, true, None))]);
            let with = with_val(&[("ratio", yaml("3.14"))]);
            let result = resolve_fragment_inputs(dummy_path(), &declarations, &with).unwrap();
            let v = result.get("ratio").unwrap();
            assert!(matches!(v, EvaluatedValue::Number(n) if (*n - 3.14).abs() < 1e-9));
        }

        /// R2: negative number and zero → OK.
        #[test]
        fn test_resolve_inputs_number_type_valid_negative_and_zero() {
            let declarations = decl_map(&[("n", decl(InputType::Number, true, None))]);
            for v in [yaml("-3"), yaml("-3.14"), yaml("0")] {
                let with = with_val(&[("n", v)]);
                assert!(
                    resolve_fragment_inputs(dummy_path(), &declarations, &with).is_ok(),
                    "negative / zero number must be valid"
                );
            }
        }

        /// R2: string `"42"` for a `number` input is a hard error.
        #[test]
        fn test_resolve_inputs_number_string_value_is_error() {
            let declarations = decl_map(&[("count", decl(InputType::Number, true, None))]);
            let with = with_map(&[("count", "42")]); // Value::String("42")
            let err = resolve_fragment_inputs(dummy_path(), &declarations, &with).unwrap_err();
            let msg = err.to_string();
            assert!(
                msg.contains("count") && msg.contains("number"),
                "string value for number input must be a hard error naming the input: {msg}"
            );
        }

        /// R2: string `"42"` as the declared `default:` for a `number` input is a hard error.
        #[test]
        fn test_resolve_inputs_number_string_default_is_error() {
            // default: "42" (a string) on a number input violates R2.
            let declarations = decl_map(&[(
                "count",
                decl(
                    InputType::Number,
                    false,
                    Some(Value::String("42".to_string())),
                ),
            )]);
            let with = with_map(&[]);
            let err = resolve_fragment_inputs(dummy_path(), &declarations, &with).unwrap_err();
            let msg = err.to_string();
            assert!(
                msg.contains("count") && msg.contains("number"),
                "string default for number input must be a hard error: {msg}"
            );
        }

        /// R3: non-finite number values (`inf`, `nan`) are hard errors.
        #[test]
        fn test_resolve_inputs_number_non_finite_is_error() {
            let declarations = decl_map(&[("n", decl(InputType::Number, true, None))]);
            // Directly call validate_number (the extension seam).
            let inf_err = validate_number("n", f64::INFINITY).unwrap_err();
            assert!(
                inf_err.to_string().contains("finite"),
                "inf must be rejected: {inf_err}"
            );
            let nan_err = validate_number("n", f64::NAN).unwrap_err();
            assert!(
                nan_err.to_string().contains("finite"),
                "nan must be rejected: {nan_err}"
            );
            let neg_inf_err = validate_number("n", f64::NEG_INFINITY).unwrap_err();
            assert!(
                neg_inf_err.to_string().contains("finite"),
                "-inf must be rejected: {neg_inf_err}"
            );
            // Negative and zero are valid.
            assert!(validate_number("n", -3.14).is_ok());
            assert!(validate_number("n", 0.0).is_ok());
            // via resolve_fragment_inputs: need a non-finite serde_yaml number.
            // serde_yaml parses ".inf" as a number (YAML float special).
            let inf_yaml: Value = serde_yaml::from_str(".inf").unwrap_or(Value::Null);
            if matches!(inf_yaml, Value::Number(_)) {
                let with = with_val(&[("n", inf_yaml)]);
                assert!(
                    resolve_fragment_inputs(dummy_path(), &declarations, &with).is_err(),
                    ".inf must be rejected via resolve_fragment_inputs"
                );
            }
        }

        /// R2: native YAML bool for `boolean` input → OK.
        #[test]
        fn test_resolve_inputs_boolean_type_valid_native() {
            let declarations = decl_map(&[("flag", decl(InputType::Boolean, true, None))]);
            for v in [yaml("true"), yaml("false")] {
                let with = with_val(&[("flag", v)]);
                assert!(
                    resolve_fragment_inputs(dummy_path(), &declarations, &with).is_ok(),
                    "native YAML bool must be valid"
                );
            }
        }

        /// R2: string `"false"` for a `boolean` input is a hard error.
        #[test]
        fn test_resolve_inputs_boolean_string_value_is_error() {
            let declarations = decl_map(&[("flag", decl(InputType::Boolean, true, None))]);
            for v in &["true", "false", "True", "FALSE"] {
                let with = with_map(&[("flag", v)]); // Value::String(...)
                let err = resolve_fragment_inputs(dummy_path(), &declarations, &with).unwrap_err();
                let msg = err.to_string();
                assert!(
                    msg.contains("flag") && msg.contains("boolean"),
                    "string value '{v}' for boolean input must be a hard error naming the input and type: {msg}"
                );
            }
        }

        /// R2: string `"false"` as the declared `default:` for a `boolean` input is a hard error.
        #[test]
        fn test_resolve_inputs_boolean_string_default_is_error() {
            let declarations = decl_map(&[(
                "flag",
                decl(
                    InputType::Boolean,
                    false,
                    Some(Value::String("false".to_string())),
                ),
            )]);
            let with = with_map(&[]);
            let err = resolve_fragment_inputs(dummy_path(), &declarations, &with).unwrap_err();
            let msg = err.to_string();
            assert!(
                msg.contains("flag") && msg.contains("boolean"),
                "string default for boolean input must be a hard error: {msg}"
            );
        }

        /// R2: non-bool string like `"yes"` on a `boolean` input is also a hard error.
        #[test]
        fn test_resolve_inputs_boolean_type_invalid() {
            let declarations = decl_map(&[("flag", decl(InputType::Boolean, true, None))]);
            let with = with_map(&[("flag", "yes")]); // Value::String("yes")
            let err = resolve_fragment_inputs(dummy_path(), &declarations, &with).unwrap_err();
            let msg = err.to_string();
            assert!(
                msg.contains("flag") && msg.contains("boolean"),
                "error must name the input and type: {msg}"
            );
        }

        /// R2: string `"not-a-number"` for a `number` input is a hard error naming the input.
        #[test]
        fn test_resolve_inputs_number_type_invalid() {
            let declarations = decl_map(&[("count", decl(InputType::Number, true, None))]);
            let with = with_map(&[("count", "not-a-number")]); // Value::String(...)
            let err = resolve_fragment_inputs(dummy_path(), &declarations, &with).unwrap_err();
            let msg = err.to_string();
            assert!(
                msg.contains("count") && msg.contains("number"),
                "error must name the input and type: {msg}"
            );
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
            let with = with_map(&[]);
            let err = resolve_fragment_inputs(dummy_path(), &declarations, &with).unwrap_err();
            let msg = err.to_string();
            assert!(
                msg.contains("missing required input 'target'"),
                "unexpected error: {msg}"
            );
        }

        #[test]
        fn test_resolve_inputs_declaration_required_and_default_contradiction() {
            let declarations =
                decl_map(&[("shell", decl(InputType::String, true, Some(yaml("bash"))))]);
            let with = with_map(&[]);
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
            use crate::config::load_test_config;
            use crate::step::{RunStep, TestStep};
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
            use crate::config::load_test_config;
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

            assert!(
                load_test_config(repo.path(), &repo.path().join("test.yaml")).is_err(),
                "`inputs:` at call site must be rejected"
            );
        }

        #[test]
        fn test_load_test_config_declared_default_applied_via_fragment() {
            use crate::config::load_test_config;
            use crate::step::{RunStep, TestStep};
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
            use crate::config::load_test_config;
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

    // -----------------------------------------------------------------------
    // Expression evaluation tests
    // -----------------------------------------------------------------------

    mod expressions {
        use super::*;

        #[test]
        fn test_back_compat_reference_and_splicing() {
            assert_eq!(
                substitute_inputs("${{ inputs.x }}", &[("x", "hello")]),
                "hello"
            );
            assert_eq!(substitute_inputs("${{ inputs.missing }}", &[]), "");
            assert_eq!(
                substitute_inputs("prefix ${{ inputs.x }} suffix", &[("x", "hello")]),
                "prefix hello suffix"
            );
            assert_eq!(
                substitute_inputs(
                    "a=${{ inputs.a }},b=${{ inputs.b }}",
                    &[("a", "x"), ("b", "y")]
                ),
                "a=x,b=y"
            );
        }

        // --- Problem 3: faithful interpolation ---

        #[test]
        fn test_interpolation_is_faithful_not_truthy_collapsed() {
            // Bool(false) and Number(0) must render faithfully, not as "".
            // Only Empty/undefined reference yields "".
            assert_eq!(substitute_inputs("${{ false }}", &[]), "false");
            assert_eq!(substitute_inputs("${{ 0 }}", &[]), "0");
            assert_eq!(substitute_inputs("${{ true }}", &[]), "true");
            assert_eq!(substitute_inputs("${{ '' }}", &[]), "");
            assert_eq!(substitute_inputs("${{ inputs.missing }}", &[]), "");
            assert_eq!(substitute_inputs("${{ 1.5 }}", &[]), "1.5");
            assert_eq!(substitute_inputs("${{ 'false' }}", &[]), "false");
            assert_eq!(substitute_inputs("${{ '0' }}", &[]), "0");
        }

        // --- Problem 3: truthiness ---

        #[test]
        fn test_truthiness_non_empty_string_is_truthy_including_0_and_false() {
            // Non-empty strings (including "0" and "false") are truthy.
            // Only the NUMBER 0 and BOOL false and EMPTY are falsy.
            assert_eq!(
                substitute_inputs("${{ '0' && 'yes' }}", &[]),
                "yes",
                "'0' string must be truthy"
            );
            assert_eq!(
                substitute_inputs("${{ 'false' && 'yes' }}", &[]),
                "yes",
                "'false' string must be truthy"
            );
            assert_eq!(
                substitute_inputs("${{ 0 && 'yes' }}", &[]),
                "0",
                "number 0 must be falsy"
            );
            assert_eq!(
                substitute_inputs("${{ false && 'yes' }}", &[]),
                "false",
                "bool false must be falsy"
            );
        }

        // --- Problem 2: strict type-aware equality ---

        #[test]
        fn test_equality_cross_type_is_always_false() {
            // 0, false, and "" are mutually unequal (cross-type comparison).
            assert_eq!(substitute_inputs("${{ 0 == '' }}", &[]), "false");
            assert_eq!(substitute_inputs("${{ false == '' }}", &[]), "false");
            assert_eq!(substitute_inputs("${{ 0 == false }}", &[]), "false");
            assert_eq!(substitute_inputs("${{ 1 == '1' }}", &[]), "false");
        }

        #[test]
        fn test_equality_same_type_same_value_is_true() {
            assert_eq!(substitute_inputs("${{ '' == '' }}", &[]), "true");
            assert_eq!(substitute_inputs("${{ 0 == 0 }}", &[]), "true");
            assert_eq!(substitute_inputs("${{ false == false }}", &[]), "true");
            assert_eq!(substitute_inputs("${{ true == true }}", &[]), "true");
            assert_eq!(substitute_inputs("${{ 'hello' == 'hello' }}", &[]), "true");
        }

        #[test]
        fn test_equality_and_inequality_expressions() {
            assert_eq!(
                substitute_inputs("${{ inputs.pov == 'admin' }}", &[("pov", "admin")]),
                "true"
            );
            assert_eq!(
                substitute_inputs("${{ inputs.pov == 'admin' }}", &[("pov", "user")]),
                "false"
            );
            assert_eq!(
                substitute_inputs("${{ inputs.pov != 'admin' }}", &[("pov", "user")]),
                "true"
            );
            assert_eq!(
                substitute_inputs("${{ inputs.n == '1' }}", &[("n", "1")]),
                "true"
            );
            assert_eq!(
                substitute_inputs(
                    "${{ inputs.a == inputs.b }}",
                    &[("a", "same"), ("b", "same")]
                ),
                "true"
            );
        }

        // --- Problem 1: typed boolean/number inputs ---

        #[test]
        fn test_typed_bool_input_false_is_falsy() {
            let tm = typed_map(&[("booly", "false", InputType::Boolean)]);
            // Bool(false) is falsy → && short-circuits, || picks right branch
            let result = substitute_namespace_in_string(
                "${{ inputs.booly && 'x' || 'y' }}",
                "inputs",
                &tm,
                &BTreeMap::new(),
            )
            .unwrap();
            assert_eq!(result, "y");
        }

        #[test]
        fn test_typed_bool_input_true_is_truthy() {
            let tm = typed_map(&[("booly", "true", InputType::Boolean)]);
            let result = substitute_namespace_in_string(
                "${{ inputs.booly && 'x' || 'y' }}",
                "inputs",
                &tm,
                &BTreeMap::new(),
            )
            .unwrap();
            assert_eq!(result, "x");
        }

        #[test]
        fn test_typed_string_false_value_is_truthy() {
            let tm = typed_map(&[("booly_string", "false", InputType::String)]);
            // String("false") is non-empty → truthy
            let result = substitute_namespace_in_string(
                "${{ inputs.booly_string && 'x' || 'y' }}",
                "inputs",
                &tm,
                &BTreeMap::new(),
            )
            .unwrap();
            assert_eq!(result, "x");
        }

        #[test]
        fn test_typed_number_input_zero_is_falsy() {
            let tm = typed_map(&[("n", "0", InputType::Number)]);
            let result = substitute_namespace_in_string(
                "${{ inputs.n && 'x' || 'y' }}",
                "inputs",
                &tm,
                &BTreeMap::new(),
            )
            .unwrap();
            assert_eq!(result, "y");
        }

        #[test]
        fn test_typed_bool_interpolation_renders_faithfully() {
            let tm = typed_map(&[("booly", "false", InputType::Boolean)]);
            // In a multi-span context, Bool(false) interpolates as "false" (not "").
            let result = substitute_namespace_in_string(
                "value=${{ inputs.booly }}",
                "inputs",
                &tm,
                &BTreeMap::new(),
            )
            .unwrap();
            assert_eq!(result, "value=false");
        }

        // --- Logical operators ---

        #[test]
        fn test_logical_operators_return_values_and_not() {
            assert_eq!(
                substitute_inputs("${{ inputs.a && 'x' }}", &[("a", "yes")]),
                "x"
            );
            assert_eq!(
                substitute_inputs("${{ inputs.a && 'x' }}", &[("a", "")]),
                ""
            );
            assert_eq!(
                substitute_inputs("${{ inputs.a || 'fallback' }}", &[("a", "yes")]),
                "yes"
            );
            assert_eq!(
                substitute_inputs("${{ inputs.a || 'fallback' }}", &[("a", "")]),
                "fallback"
            );
            // !truthy → Bool(false) → "false" (faithful interpolation)
            assert_eq!(
                substitute_inputs("${{ !inputs.a }}", &[("a", "x")]),
                "false"
            );
            // !falsy → Bool(true) → "true"
            assert_eq!(substitute_inputs("${{ !inputs.a }}", &[("a", "")]), "true");
        }

        #[test]
        fn test_ternary_substitute_and_precedence_and_parens() {
            assert_eq!(
                substitute_inputs("${{ inputs.cond && 'x' || 'y' }}", &[("cond", "yes")]),
                "x"
            );
            assert_eq!(
                substitute_inputs("${{ inputs.cond && 'x' || 'y' }}", &[("cond", "")]),
                "y"
            );
            assert_eq!(
                substitute_inputs(
                    "${{ inputs.a || inputs.b && inputs.c }}",
                    &[("a", ""), ("b", "b"), ("c", "c")]
                ),
                "c"
            );
            assert_eq!(
                substitute_inputs(
                    "${{ (inputs.foo == inputs.bar && inputs.bar == inputs.baz) && 'x' || 'y' }}",
                    &[("foo", "v"), ("bar", "v"), ("baz", "v")]
                ),
                "x"
            );
            assert_eq!(
                substitute_inputs(
                    "${{ !(inputs.a == 'x') && inputs.b }}",
                    &[("a", "y"), ("b", "z")]
                ),
                "z"
            );
        }

        // --- Deferred namespaces ---

        #[test]
        fn test_short_circuit_with_unresolved_other_namespace() {
            assert_eq!(
                substitute_inputs("${{ inputs.a || args.x }}", &[("a", "present")]),
                "present"
            );
            assert_eq!(
                substitute_inputs("${{ inputs.a || args.x }}", &[("a", "")]),
                "${{ inputs.a || args.x }}"
            );
            assert_eq!(
                substitute_args("${{ args.a && inputs.x }}", &[("a", "")]),
                ""
            );
        }

        // --- Syntax and namespace errors ---

        #[test]
        fn test_syntax_and_namespace_errors_are_hard_errors() {
            let err = substitute_inputs_err("${{ (inputs.a }}", &[("a", "x")]);
            assert!(
                err.contains("expected token") || err.contains("unterminated"),
                "unexpected message: {err}"
            );

            let err = substitute_inputs_err("${{ inputs.a && }}", &[("a", "x")]);
            assert!(
                err.contains("unexpected token"),
                "unexpected message: {err}"
            );

            let err = substitute_inputs_err("${{ env.PATH }}", &[]);
            assert!(
                err.contains("unknown namespace"),
                "unexpected message: {err}"
            );
        }

        // --- String escape sequences ---

        #[test]
        fn test_string_escape_sequences() {
            assert_eq!(
                substitute_inputs("${{ 'hello\\nworld' }}", &[]),
                "hello\nworld"
            );
            assert_eq!(substitute_inputs("${{ 'it\\'s' }}", &[]), "it's");
        }

        // --- to_json / from_json ---

        #[test]
        fn test_to_json_function() {
            // Bool false → JSON literal "false"
            assert_eq!(substitute_inputs("${{ to_json(false) }}", &[]), "false");
            // Bool true → "true"
            assert_eq!(substitute_inputs("${{ to_json(true) }}", &[]), "true");
            // Number 0 → "0"
            assert_eq!(substitute_inputs("${{ to_json(0) }}", &[]), "0");
            // Number 1.5 → "1.5"
            assert_eq!(substitute_inputs("${{ to_json(1.5) }}", &[]), "1.5");
            // String "false" → JSON-quoted: "\"false\""
            assert_eq!(
                substitute_inputs("${{ to_json('false') }}", &[]),
                "\"false\""
            );
            // String "x" → "\"x\""
            assert_eq!(substitute_inputs("${{ to_json('x') }}", &[]), "\"x\"");
        }

        #[test]
        fn test_from_json_function() {
            // "false" → Bool(false) → falsy → && picks right
            assert_eq!(
                substitute_inputs("${{ from_json('false') && 'yes' || 'no' }}", &[]),
                "no"
            );
            // "true" → Bool(true) → truthy
            assert_eq!(
                substitute_inputs("${{ from_json('true') && 'yes' || 'no' }}", &[]),
                "yes"
            );
            // "0" → Number(0) → falsy
            assert_eq!(
                substitute_inputs("${{ from_json('0') && 'yes' || 'no' }}", &[]),
                "no"
            );
            // "1" → Number(1) → truthy
            assert_eq!(
                substitute_inputs("${{ from_json('1') && 'yes' || 'no' }}", &[]),
                "yes"
            );
        }

        #[test]
        fn test_from_json_string_value() {
            // A JSON-quoted string "\"hello\"" → String("hello")
            assert_eq!(
                substitute_inputs("${{ from_json('\"hello\"') }}", &[]),
                "hello"
            );
        }

        #[test]
        fn test_from_json_null_is_empty_falsy() {
            assert_eq!(
                substitute_inputs("${{ from_json('null') && 'yes' || 'no' }}", &[]),
                "no"
            );
        }

        #[test]
        fn test_from_json_array_is_error() {
            let tm = TypedInputMap::new();
            let err = substitute_namespace_in_string(
                "${{ from_json('[1,2]') }}",
                "inputs",
                &tm,
                &BTreeMap::new(),
            )
            .unwrap_err();
            assert!(
                format!("{err:#}").contains("not yet implemented"),
                "array must produce a not-implemented error: {err:#}"
            );
        }

        #[test]
        fn test_unknown_function_is_error() {
            let err = substitute_inputs_err("${{ unknown_func('x') }}", &[]);
            assert!(
                err.contains("unknown function"),
                "unknown function must be a hard error: {err}"
            );
        }

        /// Canonical example: `from_json(inputs.booly_string)` reinterprets the string
        /// `"false"` as `Bool(false)` (falsy).
        #[test]
        fn test_from_json_booly_string_canonical() {
            let tm = typed_map(&[("booly_string", "false", InputType::String)]);
            // from_json("false") → Bool(false) → falsy → || picks right
            let result = substitute_namespace_in_string(
                "${{ from_json(inputs.booly_string) && 'x' || 'y' }}",
                "inputs",
                &tm,
                &BTreeMap::new(),
            )
            .unwrap();
            assert_eq!(result, "y");
        }

        // --- if: typed evaluation ---

        #[test]
        fn test_if_gate_expression_evaluates_via_truthiness() {
            use crate::config::load_test_config;
            use crate::step::TestStep;
            use tempfile::TempDir;

            let repo = TempDir::new().unwrap();
            std::fs::create_dir_all(repo.path().join("shared")).unwrap();
            std::fs::write(
                repo.path().join("shared/frag.yaml"),
                r#"
type: botforge/fragment
inputs:
  flag:
    type: string
    default: ""
  a:
    type: string
    default: ""
  b:
    type: string
    default: ""
steps:
  - on: guest
    name: run
    if: ${{ inputs.flag }}
    run: "echo ok"
  - on: guest
    name: and
    if: ${{ inputs.a == 'x' && inputs.b }}
    run: "echo ok"
"#,
            )
            .unwrap();

            std::fs::write(
                repo.path().join("test-enabled.yaml"),
                r#"
type: botforge/test
name: enabled
steps:
  - uses: "@://shared/frag.yaml"
    with:
      flag: "yes"
      a: "x"
      b: "1"
"#,
            )
            .unwrap();
            let enabled =
                load_test_config(repo.path(), &repo.path().join("test-enabled.yaml")).unwrap();
            let TestStep::Run(first) = &enabled.steps[0] else {
                panic!("expected run step");
            };
            let TestStep::Run(second) = &enabled.steps[1] else {
                panic!("expected run step");
            };
            assert!(first.condition_enabled());
            assert!(second.condition_enabled());

            std::fs::write(
                repo.path().join("test-disabled.yaml"),
                r#"
type: botforge/test
name: disabled
steps:
  - uses: "@://shared/frag.yaml"
    with:
      flag: ""
      a: "x"
      b: ""
"#,
            )
            .unwrap();
            let disabled =
                load_test_config(repo.path(), &repo.path().join("test-disabled.yaml")).unwrap();
            let TestStep::Run(first) = &disabled.steps[0] else {
                panic!("expected run step");
            };
            let TestStep::Run(second) = &disabled.steps[1] else {
                panic!("expected run step");
            };
            assert!(!first.condition_enabled());
            assert!(!second.condition_enabled());
        }

        /// Canonical example from the problem statement: boolean inputs must produce
        /// correctly typed values in expressions and in `if:` conditions.
        /// - `booly: false` (native YAML bool, declared boolean) → Bool(false) → falsy → skip
        /// - `booly_string: "false"` (quoted string, declared string) → truthy (non-empty) → run
        /// - `from_json(inputs.booly_string)` → Bool(false) → falsy → skip
        #[test]
        fn test_if_gate_typed_boolean_input() {
            use crate::config::load_test_config;
            use crate::step::TestStep;
            use tempfile::TempDir;

            let repo = TempDir::new().unwrap();
            std::fs::create_dir_all(repo.path().join("shared")).unwrap();
            std::fs::write(
                repo.path().join("shared/frag.yaml"),
                r#"
type: botforge/fragment
inputs:
  booly:
    type: boolean
    default: false
  booly_string:
    type: string
    default: "false"
steps:
  - on: guest
    name: bool-false-skip
    if: ${{ inputs.booly }}
    run: "echo bool-false"
  - on: guest
    name: string-false-run
    if: ${{ inputs.booly_string }}
    run: "echo string-false"
  - on: guest
    name: from-json-skip
    if: ${{ from_json(inputs.booly_string) }}
    run: "echo from-json"
"#,
            )
            .unwrap();

            // booly: false (native YAML bool) — R2: must NOT be quoted.
            // booly_string: "false" — valid string value.
            std::fs::write(
                repo.path().join("test.yaml"),
                r#"
type: botforge/test
name: test
steps:
  - uses: "@://shared/frag.yaml"
    with:
      booly: false
      booly_string: "false"
"#,
            )
            .unwrap();
            let config = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap();
            let TestStep::Run(bool_step) = &config.steps[0] else {
                panic!("expected run step");
            };
            let TestStep::Run(string_step) = &config.steps[1] else {
                panic!("expected run step");
            };
            let TestStep::Run(from_json_step) = &config.steps[2] else {
                panic!("expected run step");
            };

            assert!(
                !bool_step.condition_enabled(),
                "declared boolean false must be falsy in if:"
            );
            assert!(
                string_step.condition_enabled(),
                "string 'false' must be truthy in if:"
            );
            assert!(
                !from_json_step.condition_enabled(),
                "from_json('false') must be falsy in if:"
            );
        }

        #[test]
        fn test_if_gate_literal_expression_without_fragment_inputs() {
            use crate::config::load_test_config;
            use crate::step::TestStep;
            use tempfile::TempDir;

            let repo = TempDir::new().unwrap();
            std::fs::write(
                repo.path().join("test.yaml"),
                r#"
type: botforge/test
name: test
steps:
  - on: guest
    name: run
    if: ${{ false }}
    run: "echo ok"
  - on: guest
    name: run2
    if: ${{ true && 'x' }}
    run: "echo ok"
"#,
            )
            .unwrap();

            let config = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap();
            let TestStep::Run(first) = &config.steps[0] else {
                panic!("expected run step");
            };
            let TestStep::Run(second) = &config.steps[1] else {
                panic!("expected run step");
            };
            assert!(!first.condition_enabled());
            assert!(second.condition_enabled());
        }

        /// Canonical: `${{ inputs.booly }}` interpolated into surrounding text renders
        /// faithfully as "false", not "".
        #[test]
        fn test_canonical_bool_interpolation_in_string_context() {
            let tm = typed_map(&[("booly", "false", InputType::Boolean)]);
            let result = substitute_namespace_in_string(
                "This string has ${{ inputs.booly }} in it",
                "inputs",
                &tm,
                &BTreeMap::new(),
            )
            .unwrap();
            assert_eq!(result, "This string has false in it");
        }

        // --- Residual expression errors (invariant A) ---

        /// A top-level step (not from a fragment) that uses `inputs.*` references
        /// must be a hard error: the inputs pass is never run for top-level steps,
        /// so the placeholder survives the args pass and the residual check fires.
        #[test]
        fn test_residual_inputs_ref_in_top_level_step_is_error() {
            use crate::config::load_test_config;
            use tempfile::TempDir;

            let repo = TempDir::new().unwrap();
            std::fs::write(
                repo.path().join("test.yaml"),
                r#"
type: botforge/test
name: test
steps:
  - on: guest
    name: bad-step
    run: echo ${{ inputs.x }}
"#,
            )
            .unwrap();

            let err = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap_err();
            let msg = format!("{err:#}");
            assert!(
                msg.contains("unresolved expression"),
                "residual inputs.* in top-level step must be a hard error: {msg}"
            );
        }

        /// After both substitution passes, a surviving `${{ }}` placeholder is a
        /// hard error. A cross-namespace expression `${{ inputs.x || args.0 }}` where
        /// `inputs.x` is falsy cannot be resolved in either pass:
        /// - inputs pass: needs `args.0` → deferred
        /// - args pass: `inputs.x` ref → deferred again → residual!
        #[test]
        fn test_residual_cross_namespace_expression_is_error() {
            use crate::config::load_test_config;
            use tempfile::TempDir;

            let repo = TempDir::new().unwrap();
            std::fs::create_dir_all(repo.path().join("shared")).unwrap();
            std::fs::write(
                repo.path().join("shared/frag.yaml"),
                r#"
type: botforge/fragment
inputs:
  x:
    type: string
    default: ""
steps:
  - on: guest
    name: cross-ns
    run: echo ${{ inputs.x || args.0 }}
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
      x: ""
"#,
            )
            .unwrap();

            let err = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap_err();
            let msg = format!("{err:#}");
            assert!(
                msg.contains("unresolved expression"),
                "residual cross-namespace expression must be a hard error: {msg}"
            );
        }

        // --- Undefined ref: context-typed behavior (invariant C) ---

        /// `name: ${{ inputs.name_override }}` with `default: ""` and no `with:` value
        /// → uses default `""` → name is empty string (no error).
        #[test]
        fn test_undefined_ref_in_string_field_yields_empty_string() {
            use crate::config::load_test_config;
            use crate::step::TestStep;
            use tempfile::TempDir;

            let repo = TempDir::new().unwrap();
            std::fs::create_dir_all(repo.path().join("shared")).unwrap();
            std::fs::write(
                repo.path().join("shared/frag.yaml"),
                r#"
type: botforge/fragment
inputs:
  name_override:
    type: string
    default: ""
steps:
  - on: guest
    name: ${{ inputs.name_override }}
    run: echo ok
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
            let TestStep::Run(step) = &config.steps[0] else {
                panic!("expected run step");
            };
            assert_eq!(
                step.name, "",
                "undefined ref in string field must yield empty string"
            );
        }

        /// `if: ${{ inputs.gate }}` with `default: false` and no `with:` value
        /// → uses default `Bool(false)` → step SKIPS.
        #[test]
        fn test_undefined_ref_in_if_is_falsy() {
            use crate::config::load_test_config;
            use crate::step::TestStep;
            use tempfile::TempDir;

            let repo = TempDir::new().unwrap();
            std::fs::create_dir_all(repo.path().join("shared")).unwrap();
            std::fs::write(
                repo.path().join("shared/frag.yaml"),
                r#"
type: botforge/fragment
inputs:
  gate:
    type: boolean
    default: false
steps:
  - on: guest
    name: gated
    if: ${{ inputs.gate }}
    run: echo ok
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
            let TestStep::Run(step) = &config.steps[0] else {
                panic!("expected run step");
            };
            assert!(
                !step.condition_enabled(),
                "undefined ref in if: must be falsy (step skipped)"
            );
        }

        /// R5: `name: ${{ inputs.count }}` where `count` is a number input stringifies to `"5"`.
        ///
        /// Previously this was a load error because `to_yaml_value()` emitted a YAML integer
        /// that serde_yaml could not coerce into a `String` field. The R5 fix makes all
        /// non-typed fields (everything except `if:` and `timeout:`) stringify Number/Bool
        /// via `to_interpolated_string()`, so string-context fields just work.
        #[test]
        fn test_number_input_in_name_field_pure_expr_stringifies() {
            use crate::config::load_test_config;
            use crate::step::TestStep;
            use tempfile::TempDir;

            let repo = TempDir::new().unwrap();
            std::fs::create_dir_all(repo.path().join("shared")).unwrap();
            std::fs::write(
                repo.path().join("shared/frag.yaml"),
                r#"
type: botforge/fragment
inputs:
  count:
    type: number
    default: 5
steps:
  - on: guest
    name: ${{ inputs.count }}
    run: echo ok
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

            // R5: pure number expression in a string field must stringify to "5".
            let config = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap();
            let TestStep::Run(step) = &config.steps[0] else {
                panic!("expected run step");
            };
            assert_eq!(
                step.name, "5",
                "pure number expr in string field must coerce to string via R5"
            );
        }

        /// The interpolated form `name: prefix-${{ inputs.count }}` (surrounding text) always
        /// stringifies the Number correctly via `to_interpolated_string`.
        #[test]
        fn test_number_input_in_name_field_interpolated_works() {
            use crate::config::load_test_config;
            use crate::step::TestStep;
            use tempfile::TempDir;

            let repo = TempDir::new().unwrap();
            std::fs::create_dir_all(repo.path().join("shared")).unwrap();
            std::fs::write(
                repo.path().join("shared/frag.yaml"),
                r#"
type: botforge/fragment
inputs:
  count:
    type: number
    default: 5
steps:
  - on: guest
    name: step-${{ inputs.count }}
    run: echo ok
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
            let TestStep::Run(step) = &config.steps[0] else {
                panic!("expected run step");
            };
            assert_eq!(
                step.name, "step-5",
                "interpolated number in string field must coerce to string via to_interpolated_string"
            );
        }

        /// Short-circuit must not evaluate the dead branch: `false && inputs.undefined_ref`
        /// returns `Bool(false)` without ever touching `inputs.undefined_ref`. A regression
        /// that evaluates the dead branch would return an error or `Empty` instead of the
        /// lhs falsy value.
        #[test]
        fn test_short_circuit_dead_branch_skips_undefined_ref() {
            // false && inputs.undefined (dead branch, never evaluated) → Bool(false) = "false"
            assert_eq!(
                substitute_inputs("${{ false && inputs.undefined_key }}", &[]),
                "false",
                "dead branch of && must not be evaluated; lhs falsy value returned"
            );
            // empty_str && inputs.undefined (dead branch) → String("") = ""
            assert_eq!(
                substitute_inputs(
                    "${{ inputs.empty_str && inputs.undefined_key || 'ok' }}",
                    &[("empty_str", "")]
                ),
                "ok",
                "dead branch of && must not be evaluated; || then yields 'ok'"
            );
        }

        // --- R6: single truthiness authority ---
        // `deserialize_step_condition` must be consistent with `EvaluatedValue::truthy()`.

        /// R6: `if: false` (YAML bool) skips the step.
        #[test]
        fn test_r6_if_literal_false_skips() {
            use crate::config::load_test_config;
            use crate::step::TestStep;
            use tempfile::TempDir;

            let repo = TempDir::new().unwrap();
            std::fs::write(
                repo.path().join("test.yaml"),
                r#"
type: botforge/test
name: test
steps:
  - on: guest
    name: step
    if: false
    run: echo ok
"#,
            )
            .unwrap();

            let config = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap();
            let TestStep::Run(step) = &config.steps[0] else {
                panic!("expected run step");
            };
            assert!(!step.condition_enabled(), "if: false must skip the step");
        }

        /// R6: `if: "false"` (YAML string) runs the step — non-empty string is truthy.
        #[test]
        fn test_r6_if_string_false_runs() {
            use crate::config::load_test_config;
            use crate::step::TestStep;
            use tempfile::TempDir;

            let repo = TempDir::new().unwrap();
            std::fs::write(
                repo.path().join("test.yaml"),
                r#"
type: botforge/test
name: test
steps:
  - on: guest
    name: step
    if: "false"
    run: echo ok
"#,
            )
            .unwrap();

            let config = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap();
            let TestStep::Run(step) = &config.steps[0] else {
                panic!("expected run step");
            };
            assert!(
                step.condition_enabled(),
                "if: \"false\" (string) must run — non-empty string is truthy"
            );
        }

        /// R6: `if: true` (YAML bool) runs the step.
        #[test]
        fn test_r6_if_literal_true_runs() {
            use crate::config::load_test_config;
            use crate::step::TestStep;
            use tempfile::TempDir;

            let repo = TempDir::new().unwrap();
            std::fs::write(
                repo.path().join("test.yaml"),
                r#"
type: botforge/test
name: test
steps:
  - on: guest
    name: step
    if: true
    run: echo ok
"#,
            )
            .unwrap();

            let config = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap();
            let TestStep::Run(step) = &config.steps[0] else {
                panic!("expected run step");
            };
            assert!(step.condition_enabled(), "if: true must run the step");
        }

        /// R6: `if: 0` (YAML number) skips the step — number 0 is falsy.
        #[test]
        fn test_r6_if_zero_skips() {
            use crate::config::load_test_config;
            use crate::step::TestStep;
            use tempfile::TempDir;

            let repo = TempDir::new().unwrap();
            std::fs::write(
                repo.path().join("test.yaml"),
                r#"
type: botforge/test
name: test
steps:
  - on: guest
    name: step
    if: 0
    run: echo ok
"#,
            )
            .unwrap();

            let config = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap();
            let TestStep::Run(step) = &config.steps[0] else {
                panic!("expected run step");
            };
            assert!(
                !step.condition_enabled(),
                "if: 0 must skip the step — number 0 is falsy"
            );
        }

        /// R5 + timeout: `timeout: ${{ inputs.seconds }}` where seconds=42 must produce a valid
        /// u64 timeout (not a string error). The typed-field path for `timeout:` is preserved.
        #[test]
        fn test_r5_timeout_number_input_stays_typed() {
            use crate::config::load_test_config;
            use crate::step::TestStep;
            use tempfile::TempDir;

            let repo = TempDir::new().unwrap();
            std::fs::create_dir_all(repo.path().join("shared")).unwrap();
            std::fs::write(
                repo.path().join("shared/frag.yaml"),
                r#"
type: botforge/fragment
inputs:
  seconds:
    type: number
    default: 42
steps:
  - on: guest
    name: timed
    timeout: ${{ inputs.seconds }}
    run: echo ok
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
            let TestStep::Run(step) = &config.steps[0] else {
                panic!("expected run step");
            };
            assert_eq!(
                step.timeout,
                Some(42),
                "timeout: ${{number_input}} must be 42"
            );
        }
    }
}
