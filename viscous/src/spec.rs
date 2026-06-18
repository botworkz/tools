//! Parse and validate `__template__.yaml`.
//!
//! The spec is the user-facing contract: it declares what variables a template
//! consumes, what files it emits beyond the static tree, and how conflicts are
//! resolved. It is **schema-only** — it never touches the filesystem.

use crate::error::{Error, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

/// Name of the spec file living at the root of every template directory.
pub const SPEC_FILENAME: &str = "__template__.yaml";

/// Name of the generator-templates directory; excluded from the static walker.
pub const GENERATORS_DIRNAME: &str = "__templates__";

/// Parsed `__template__.yaml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Spec {
    pub name: String,

    #[serde(default)]
    pub description: String,

    #[serde(default)]
    pub version: String,

    /// Variable schema, keyed by name. Order is preserved by [`serde_yaml`] when
    /// using a `BTreeMap` -> deterministic; the original yaml-insertion order
    /// would be nicer but isn't worth a custom map for v1.
    #[serde(default)]
    pub vars: BTreeMap<String, VarSpec>,

    /// Derived vars are evaluated as liquid expressions against the resolved
    /// `vars` map, in declared order, and added to the var dict before any
    /// file is rendered.
    #[serde(default)]
    pub derived: BTreeMap<String, String>,

    /// Declarative generator steps. Processed in yaml list order.
    #[serde(default)]
    pub generate: Vec<GenerateStep>,

    /// File globs (rooted at the template dir) whose **contents** are copied
    /// verbatim rather than rendered as liquid. Filename templating still
    /// applies. Useful for files that legitimately contain `{{` or `{%`.
    #[serde(default)]
    pub verbatim: Vec<String>,

    /// Additional path globs (rooted at the template dir) excluded from the
    /// static tree walk. `__template__.yaml` and `__templates__/` are always
    /// excluded.
    #[serde(default)]
    pub ignore: Vec<String>,
}

/// One variable's schema entry.
///
/// The `type` discriminator is required; everything else has defaults so the
/// minimal entry is `{type: string}`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VarSpec {
    #[serde(rename = "type")]
    pub ty: VarType,

    #[serde(default)]
    pub required: bool,

    #[serde(default)]
    pub description: String,

    /// Default value (must match the declared type; validated at spec-load
    /// time only for the scalar types — array/object defaults are validated
    /// when applied).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<serde_json::Value>,

    /// For `type: array` — schema of each item. Defaults to `VarType::String`
    /// when omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub items: Option<Box<VarSpec>>,

    /// For `type: object` — schema of each field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<BTreeMap<String, VarSpec>>,
}

/// Variable types supported by the spec.
///
/// Kept deliberately small. `int` and `float` collapse to JSON Number on the
/// wire; the schema distinguishes them so descriptions are accurate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VarType {
    String,
    Bool,
    Int,
    Float,
    Array,
    Object,
}

impl VarType {
    pub fn as_str(self) -> &'static str {
        match self {
            VarType::String => "string",
            VarType::Bool => "bool",
            VarType::Int => "int",
            VarType::Float => "float",
            VarType::Array => "array",
            VarType::Object => "object",
        }
    }
}

/// One declarative entry under `generate:`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GenerateStep {
    /// Path to a template file, relative to the template root. Conventionally
    /// lives under `__templates__/`, but isn't required to.
    pub template: String,

    /// Destination path, relative to the destination root. Liquid-rendered
    /// against the current variable scope (top-level vars + `as`-bound item
    /// when inside `for_each`).
    pub dest: String,

    /// Iterate over the named array variable; one rendered file per item.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub for_each: Option<String>,

    /// Loop variable name; only meaningful with `for_each`. Defaults to "item".
    #[serde(
        default = "default_loop_var",
        skip_serializing_if = "is_default_loop_var"
    )]
    pub r#as: String,

    /// Liquid boolean expression; step is skipped when it renders falsy.
    /// Example: `use_tailwind`, `kind == "ssr"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when: Option<String>,

    #[serde(default)]
    pub on_conflict: OnConflict,
}

fn default_loop_var() -> String {
    "item".to_string()
}

fn is_default_loop_var(s: &str) -> bool {
    s == "item"
}

/// How a generator step reacts when its `dest` was already written by an
/// earlier step (including the implicit static-tree step at position 0).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum OnConflict {
    /// Hard error. The safe default — surfaces accidental clobbers.
    #[default]
    Error,

    /// Replace the earlier file's contents. The earlier step must have
    /// produced the file (otherwise [`Error::NothingToOverride`]).
    Overwrite,

    /// Skip emission if the dest already exists in the plan.
    Skip,

    /// Append `\n` + new content to the existing file.
    /// The earlier step must have produced the file.
    Append,

    /// Like `overwrite` but does not require an earlier file to exist; behaves
    /// like a normal create when there's no prior write.
    Upsert,
}

impl OnConflict {
    pub fn as_str(self) -> &'static str {
        match self {
            OnConflict::Error => "error",
            OnConflict::Overwrite => "overwrite",
            OnConflict::Skip => "skip",
            OnConflict::Append => "append",
            OnConflict::Upsert => "upsert",
        }
    }
}

impl Spec {
    /// Load and parse `__template__.yaml` from a template directory.
    pub fn load_from_dir(dir: &Path) -> Result<Self> {
        let path = dir.join(SPEC_FILENAME);
        if !path.is_file() {
            return Err(Error::SpecMissing(path));
        }
        let raw = std::fs::read_to_string(&path).map_err(|e| Error::Io {
            path: path.clone(),
            source: e,
        })?;
        let spec: Spec = serde_yaml::from_str(&raw).map_err(|source| Error::SpecInvalid {
            path: path.clone(),
            source,
        })?;
        Ok(spec)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_minimal_spec() {
        let s: Spec = serde_yaml::from_str("name: foo\n").unwrap();
        assert_eq!(s.name, "foo");
        assert!(s.vars.is_empty());
        assert!(s.generate.is_empty());
    }

    #[test]
    fn parses_full_spec() {
        let yaml = r#"
name: leptos-webview
description: minimal CSR app
version: 0.1.0

vars:
  project_name:
    type: string
    required: true
  routes:
    type: array
    items:
      type: string
    default: ["/"]
  components:
    type: array
    items:
      type: object
      schema:
        name:
          type: string
          required: true
        props:
          type: array
          items: { type: string }
          default: []
    default: []
  use_tailwind:
    type: bool
    default: false

derived:
  crate_name: "{{ project_name | replace: '-', '_' }}"

generate:
  - template: __templates__/component.rs.liquid
    for_each: components
    as: component
    dest: "src/components/{{ component.name }}.rs"
  - template: __templates__/route.rs.liquid
    for_each: routes
    dest: "src/routes/{{ item | replace: '/', '_' }}.rs"
  - template: __templates__/tailwind.config.js.liquid
    when: use_tailwind
    dest: tailwind.config.js

verbatim:
  - .github/workflows/*.yml

ignore:
  - "*.bak"
"#;
        let s: Spec = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(s.name, "leptos-webview");
        assert_eq!(s.vars.len(), 4);
        assert_eq!(s.derived.len(), 1);
        assert_eq!(s.generate.len(), 3);
        assert_eq!(s.generate[0].r#as, "component");
        assert_eq!(s.generate[1].r#as, "item"); // default
        assert_eq!(s.generate[2].on_conflict, OnConflict::Error);
    }

    #[test]
    fn rejects_unknown_fields_in_generate_step() {
        let yaml = r#"
name: t
generate:
  - template: a
    dest: b
    bogus_field: true
"#;
        let err = serde_yaml::from_str::<Spec>(yaml).unwrap_err();
        assert!(err.to_string().contains("bogus_field"), "{err}");
    }
}
