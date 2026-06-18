//! Validate input vars against a [`Spec`] and produce a liquid [`Object`].
//!
//! Inputs come in as `serde_json::Value` (which is exactly what an MCP tool
//! call hands us). Defaults from the spec are merged in. Missing required vars
//! become structured errors. The result is a liquid `Object` ready for
//! rendering.

use crate::error::{Error, Result};
use crate::spec::{Spec, VarSpec, VarType};
use serde_json::Value;
use std::collections::BTreeMap;

/// Validate `input` against `spec.vars`, applying defaults.
///
/// Returns a JSON object containing exactly the keys declared in the spec
/// (defaults applied where needed). Keys in `input` that aren't in the spec
/// are silently dropped — the spec is the contract; extras are noise.
pub fn resolve(spec: &Spec, input: &Value) -> Result<serde_json::Map<String, Value>> {
    let input_obj = match input {
        Value::Object(m) => m.clone(),
        Value::Null => serde_json::Map::new(),
        other => {
            return Err(Error::VarValidation {
                name: "<root>".into(),
                message: format!("expected an object, got {}", json_type(other)),
            });
        }
    };

    let mut out: serde_json::Map<String, Value> = serde_json::Map::new();
    for (name, vspec) in &spec.vars {
        let provided = input_obj.get(name);
        let resolved = resolve_one(name, vspec, provided)?;
        out.insert(name.clone(), resolved);
    }
    Ok(out)
}

fn resolve_one(name: &str, vspec: &VarSpec, provided: Option<&Value>) -> Result<Value> {
    if let Some(v) = provided {
        validate(name, vspec, v)?;
        return Ok(v.clone());
    }
    if let Some(default) = &vspec.default {
        validate(name, vspec, default)?;
        return Ok(default.clone());
    }
    if vspec.required {
        return Err(Error::RequiredVarMissing(name.to_string()));
    }
    // Optional, no default — synthesize a zero value of the right type so
    // templates can safely reference it without {% if %} dances.
    Ok(zero_value(vspec))
}

fn zero_value(vspec: &VarSpec) -> Value {
    match vspec.ty {
        VarType::String => Value::String(String::new()),
        VarType::Bool => Value::Bool(false),
        VarType::Int => Value::Number(0.into()),
        VarType::Float => {
            // serde_json::Number::from_f64(0.0) is Some(_)
            Value::Number(serde_json::Number::from_f64(0.0).expect("0.0 is finite"))
        }
        VarType::Array => Value::Array(Vec::new()),
        VarType::Object => Value::Object(serde_json::Map::new()),
    }
}

/// Validate a value against a single var schema.
pub fn validate(name: &str, vspec: &VarSpec, v: &Value) -> Result<()> {
    match (vspec.ty, v) {
        (VarType::String, Value::String(_)) => Ok(()),
        (VarType::Bool, Value::Bool(_)) => Ok(()),
        (VarType::Int, Value::Number(n)) if n.is_i64() || n.is_u64() => Ok(()),
        (VarType::Float, Value::Number(_)) => Ok(()),
        (VarType::Array, Value::Array(items)) => {
            let item_schema = vspec
                .items
                .as_deref()
                .cloned()
                .unwrap_or_else(default_items_schema);
            for (i, item) in items.iter().enumerate() {
                validate(&format!("{name}[{i}]"), &item_schema, item)?;
            }
            Ok(())
        }
        (VarType::Object, Value::Object(fields)) => {
            if let Some(schema) = &vspec.schema {
                for (fname, fspec) in schema {
                    let path = format!("{name}.{fname}");
                    let provided = fields.get(fname);
                    if provided.is_none() {
                        if fspec.required && fspec.default.is_none() {
                            return Err(Error::RequiredVarMissing(path));
                        }
                        continue;
                    }
                    validate(&path, fspec, provided.unwrap())?;
                }
            }
            Ok(())
        }
        (expected, got) => Err(Error::VarTypeMismatch {
            name: name.to_string(),
            expected: expected.as_str().to_string(),
            got: json_type(got).to_string(),
        }),
    }
}

fn default_items_schema() -> VarSpec {
    VarSpec {
        ty: VarType::String,
        required: false,
        description: String::new(),
        default: None,
        items: None,
        schema: None,
    }
}

/// Fill defaults inside objects of an already-resolved var map. Used after
/// `resolve` for nested object schemas, where missing inner fields with
/// declared defaults should be materialised.
pub fn fill_object_defaults(spec: &Spec, vars: &mut serde_json::Map<String, Value>) {
    for (name, vspec) in &spec.vars {
        if let Some(v) = vars.get_mut(name) {
            fill_in(vspec, v);
        }
    }
}

fn fill_in(vspec: &VarSpec, v: &mut Value) {
    match (vspec.ty, v) {
        (VarType::Array, Value::Array(items)) => {
            if let Some(item_schema) = vspec.items.as_deref() {
                for item in items.iter_mut() {
                    fill_in(item_schema, item);
                }
            }
        }
        (VarType::Object, Value::Object(fields)) => {
            if let Some(schema) = &vspec.schema {
                for (fname, fspec) in schema {
                    if !fields.contains_key(fname) {
                        if let Some(default) = &fspec.default {
                            fields.insert(fname.clone(), default.clone());
                        }
                    }
                    if let Some(inner) = fields.get_mut(fname) {
                        fill_in(fspec, inner);
                    }
                }
            }
        }
        _ => {}
    }
}

fn json_type(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// Convert a resolved JSON var map to a liquid [`Object`] for rendering.
pub fn to_liquid(map: &serde_json::Map<String, Value>) -> Result<liquid::Object> {
    // liquid 0.26 exposes `to_object` via the model module.
    let value = Value::Object(map.clone());
    let obj = liquid_core::to_object(&value).map_err(|e| Error::VarValidation {
        name: "<root>".to_string(),
        message: format!("failed to convert vars to liquid object: {e}"),
    })?;
    Ok(obj)
}

/// Insert a single key into a liquid object as a plain JSON value.
pub fn insert_liquid_value(obj: &mut liquid::Object, key: &str, value: &Value) -> Result<()> {
    let v = liquid_core::to_value(value).map_err(|e| Error::VarValidation {
        name: key.to_string(),
        message: format!("failed to convert value to liquid: {e}"),
    })?;
    obj.insert(key.to_string().into(), v);
    Ok(())
}

/// Drop the [`BTreeMap`] insertion order: a stable view that callers can iterate.
pub fn ordered_var_names(spec: &Spec) -> Vec<String> {
    let names: BTreeMap<&String, ()> = spec.vars.keys().map(|k| (k, ())).collect();
    names.keys().map(|s| s.to_string()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn spec_from(yaml: &str) -> Spec {
        serde_yaml::from_str(yaml).unwrap()
    }

    #[test]
    fn applies_defaults_for_missing_optional() {
        let spec = spec_from(
            r#"
name: t
vars:
  greeting: { type: string, default: "hi" }
"#,
        );
        let resolved = resolve(&spec, &json!({})).unwrap();
        assert_eq!(resolved["greeting"], json!("hi"));
    }

    #[test]
    fn errors_on_missing_required() {
        let spec = spec_from(
            r#"
name: t
vars:
  who: { type: string, required: true }
"#,
        );
        let err = resolve(&spec, &json!({})).unwrap_err();
        assert!(matches!(err, Error::RequiredVarMissing(s) if s == "who"));
    }

    #[test]
    fn synthesizes_zero_for_optional_without_default() {
        let spec = spec_from(
            r#"
name: t
vars:
  flag:  { type: bool }
  count: { type: int }
  list:  { type: array, items: { type: string } }
  obj:   { type: object }
"#,
        );
        let r = resolve(&spec, &json!({})).unwrap();
        assert_eq!(r["flag"], json!(false));
        assert_eq!(r["count"], json!(0));
        assert_eq!(r["list"], json!([]));
        assert_eq!(r["obj"], json!({}));
    }

    #[test]
    fn validates_array_item_types() {
        let spec = spec_from(
            r#"
name: t
vars:
  xs: { type: array, items: { type: int } }
"#,
        );
        let err = resolve(&spec, &json!({"xs": [1, "two", 3]})).unwrap_err();
        assert!(matches!(err, Error::VarTypeMismatch { name, .. } if name == "xs[1]"));
    }

    #[test]
    fn validates_nested_object_fields() {
        let spec = spec_from(
            r#"
name: t
vars:
  components:
    type: array
    items:
      type: object
      schema:
        name: { type: string, required: true }
        props: { type: array, items: { type: string }, default: [] }
"#,
        );
        // Required nested field missing.
        let err = resolve(&spec, &json!({"components": [{"props": ["x"]}]})).unwrap_err();
        assert!(matches!(err, Error::RequiredVarMissing(p) if p == "components[0].name"));

        // Valid; props default applied via fill_object_defaults.
        let mut r = resolve(&spec, &json!({"components": [{"name": "Button"}]})).unwrap();
        fill_object_defaults(&spec, &mut r);
        assert_eq!(r["components"][0]["name"], json!("Button"));
        assert_eq!(r["components"][0]["props"], json!([]));
    }

    #[test]
    fn drops_extra_input_keys() {
        let spec = spec_from(
            r#"
name: t
vars:
  declared: { type: string, default: "x" }
"#,
        );
        let r = resolve(&spec, &json!({"declared": "y", "extra": 42})).unwrap();
        assert!(!r.contains_key("extra"));
        assert_eq!(r["declared"], json!("y"));
    }
}
