use anyhow::Result;
use std::collections::BTreeMap;

use super::parser::ExprNode;
use super::value::{EvaluatedSpan, EvaluatedValue};
use super::TypedInputMap;

/// Evaluate a `${{ expr }}` span (the text between `${{` and `}}`).
///
/// Returns a typed `EvaluatedValue` wrapped in `EvaluatedSpan::Value`, or
/// `EvaluatedSpan::Deferred` when the expression references a namespace whose values
/// are not yet available in this substitution pass.
///
/// # Deferred semantics
/// The two-pass substitution (inputs → args) relies on the whole original placeholder
/// `${{ … }}` being preserved verbatim when a reference is deferred. See `EvaluatedSpan`.
pub(super) fn evaluate_expression_span(
    expr: &str,
    active_namespace: &str,
    typed_inputs: &TypedInputMap,
    args: &BTreeMap<String, String>,
) -> Result<EvaluatedSpan> {
    let parsed = super::parser::Parser::parse(expr)?;
    evaluate_node(&parsed, active_namespace, typed_inputs, args)
}

fn evaluate_node(
    node: &ExprNode,
    active_namespace: &str,
    typed_inputs: &TypedInputMap,
    args: &BTreeMap<String, String>,
) -> Result<EvaluatedSpan> {
    match node {
        ExprNode::String(text) => Ok(EvaluatedSpan::Value(EvaluatedValue::String(text.clone()))),
        ExprNode::Number(number) => Ok(EvaluatedSpan::Value(EvaluatedValue::Number(*number))),
        ExprNode::Bool(flag) => Ok(EvaluatedSpan::Value(EvaluatedValue::Bool(*flag))),

        ExprNode::Reference { namespace, name } => {
            if namespace == "inputs" {
                if active_namespace == "inputs" {
                    // The TypedInputMap now carries fully typed EvaluatedValues resolved
                    // at the boundary; no re-parsing needed (resolve_typed_input removed).
                    return Ok(EvaluatedSpan::Value(
                        typed_inputs
                            .get(name)
                            .cloned()
                            .unwrap_or(EvaluatedValue::Empty),
                    ));
                }
                if is_deferred_namespace(active_namespace, "inputs") {
                    return Ok(EvaluatedSpan::Deferred);
                }
            }
            if namespace == "args" {
                if active_namespace == "args" {
                    // `args.*` has no type declaration; always treat as String.
                    return Ok(EvaluatedSpan::Value(
                        args.get(name)
                            .map(|v| EvaluatedValue::String(v.clone()))
                            .unwrap_or(EvaluatedValue::Empty),
                    ));
                }
                if is_deferred_namespace(active_namespace, "args") {
                    return Ok(EvaluatedSpan::Deferred);
                }
            }
            anyhow::bail!(
                "unknown namespace '{}' in reference '{}.{}'",
                namespace,
                namespace,
                name
            )
        }

        // `steps.<id>.outputs.<name>` values only exist after the referenced
        // `uses:` step has executed; always deferred during the load-time
        // (inputs/args) passes and resolved lazily at execution time.
        ExprNode::StepOutputReference { .. } => Ok(EvaluatedSpan::Deferred),
        // `outputs.<name>` values only exist at fragment execution time.
        ExprNode::FragmentOutputReference { .. } => Ok(EvaluatedSpan::Deferred),

        ExprNode::FunctionCall { name, arg } => {
            evaluate_function_call(name, arg, active_namespace, typed_inputs, args)
        }

        ExprNode::Not(expr) => match evaluate_node(expr, active_namespace, typed_inputs, args)? {
            EvaluatedSpan::Deferred => Ok(EvaluatedSpan::Deferred),
            EvaluatedSpan::Value(value) => {
                Ok(EvaluatedSpan::Value(EvaluatedValue::Bool(!value.truthy())))
            }
        },

        ExprNode::Equal(lhs, rhs) => {
            evaluate_equality(lhs, rhs, active_namespace, typed_inputs, args, true)
        }
        ExprNode::NotEqual(lhs, rhs) => {
            evaluate_equality(lhs, rhs, active_namespace, typed_inputs, args, false)
        }

        ExprNode::And(lhs, rhs) => {
            let lhs_value = match evaluate_node(lhs, active_namespace, typed_inputs, args)? {
                EvaluatedSpan::Deferred => return Ok(EvaluatedSpan::Deferred),
                EvaluatedSpan::Value(value) => value,
            };
            if !lhs_value.truthy() {
                return Ok(EvaluatedSpan::Value(lhs_value));
            }
            evaluate_node(rhs, active_namespace, typed_inputs, args)
        }

        ExprNode::Or(lhs, rhs) => {
            let lhs_value = match evaluate_node(lhs, active_namespace, typed_inputs, args)? {
                EvaluatedSpan::Deferred => return Ok(EvaluatedSpan::Deferred),
                EvaluatedSpan::Value(value) => value,
            };
            if lhs_value.truthy() {
                return Ok(EvaluatedSpan::Value(lhs_value));
            }
            evaluate_node(rhs, active_namespace, typed_inputs, args)
        }
    }
}

/// Strict / type-aware equality.
///
/// Two values are equal only when they are the **same type and same value**:
/// - `String(a) == String(b)` iff `a == b`
/// - `Number(a) == Number(b)` iff `a == b`
/// - `Bool(a) == Bool(b)` iff `a == b`
/// - `Empty == Empty` → true
/// - Cross-type comparisons (`Number(0) == Bool(false)`, `Bool(false) == String("")`, etc.)
///   → **false** (not equal).
///
/// This means `0`, `false`, and `""` are all mutually unequal.
fn evaluate_equality(
    lhs: &ExprNode,
    rhs: &ExprNode,
    active_namespace: &str,
    typed_inputs: &TypedInputMap,
    args: &BTreeMap<String, String>,
    equals: bool,
) -> Result<EvaluatedSpan> {
    let lhs = match evaluate_node(lhs, active_namespace, typed_inputs, args)? {
        EvaluatedSpan::Deferred => return Ok(EvaluatedSpan::Deferred),
        EvaluatedSpan::Value(value) => value,
    };
    let rhs = match evaluate_node(rhs, active_namespace, typed_inputs, args)? {
        EvaluatedSpan::Deferred => return Ok(EvaluatedSpan::Deferred),
        EvaluatedSpan::Value(value) => value,
    };
    let same = match (&lhs, &rhs) {
        (EvaluatedValue::String(a), EvaluatedValue::String(b)) => a == b,
        (EvaluatedValue::Number(a), EvaluatedValue::Number(b)) => a == b,
        (EvaluatedValue::Bool(a), EvaluatedValue::Bool(b)) => a == b,
        (EvaluatedValue::Empty, EvaluatedValue::Empty) => true,
        // Cross-type comparisons are always unequal.
        _ => false,
    };
    Ok(EvaluatedSpan::Value(EvaluatedValue::Bool(if equals {
        same
    } else {
        !same
    })))
}

/// Dispatch a function call to a known built-in function.
///
/// Currently supported: `to_json`, `from_json` (scalars only).
/// Unknown function names are hard errors.
fn evaluate_function_call(
    name: &str,
    arg: &ExprNode,
    active_namespace: &str,
    typed_inputs: &TypedInputMap,
    args: &BTreeMap<String, String>,
) -> Result<EvaluatedSpan> {
    match name {
        "to_json" => {
            let val = match evaluate_node(arg, active_namespace, typed_inputs, args)? {
                EvaluatedSpan::Deferred => return Ok(EvaluatedSpan::Deferred),
                EvaluatedSpan::Value(v) => v,
            };
            Ok(EvaluatedSpan::Value(EvaluatedValue::String(
                val.to_json_string(),
            )))
        }
        "from_json" => {
            let val = match evaluate_node(arg, active_namespace, typed_inputs, args)? {
                EvaluatedSpan::Deferred => return Ok(EvaluatedSpan::Deferred),
                EvaluatedSpan::Value(v) => v,
            };
            let json_str = match val {
                EvaluatedValue::String(s) => s,
                other => anyhow::bail!("from_json() requires a string argument, got {:?}", other),
            };
            Ok(EvaluatedSpan::Value(parse_from_json(&json_str)?))
        }
        unknown => anyhow::bail!(
            "unknown function '{}'; supported functions: to_json, from_json",
            unknown
        ),
    }
}

/// Parse a JSON string into a typed `EvaluatedValue` (scalars only).
///
/// - `"false"` / `"true"` → `Bool`
/// - numbers → `Number`
/// - `"\"string\""` → `String`
/// - `"null"` → `Empty`
/// - arrays / objects → hard error (not yet implemented; leave extension point intact)
fn parse_from_json(s: &str) -> Result<EvaluatedValue> {
    let json: serde_json::Value =
        serde_json::from_str(s).map_err(|e| anyhow::anyhow!("from_json() invalid JSON: {}", e))?;
    match json {
        serde_json::Value::Bool(b) => Ok(EvaluatedValue::Bool(b)),
        serde_json::Value::Number(n) => Ok(EvaluatedValue::Number(n.as_f64().unwrap_or(0.0))),
        serde_json::Value::String(s) => Ok(EvaluatedValue::String(s)),
        serde_json::Value::Null => Ok(EvaluatedValue::Empty),
        serde_json::Value::Array(_) | serde_json::Value::Object(_) => {
            anyhow::bail!("from_json() does not support arrays or objects (not yet implemented)")
        }
    }
}

/// Whether a reference to `namespace` should be deferred during the `active_namespace` pass.
///
/// The two-pass substitution (inputs first, then args) means each pass only resolves
/// its own namespace. A reference to the "other" namespace is deferred so that the
/// whole `${{ … }}` placeholder is preserved for the next pass.
fn is_deferred_namespace(active_namespace: &str, namespace: &str) -> bool {
    active_namespace != namespace
        && matches!(
            (active_namespace, namespace),
            ("inputs", "args") | ("args", "inputs")
        )
}
