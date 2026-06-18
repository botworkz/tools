//! Top-level planner: combine spec + vars + walker + generators into a [`Plan`].
//!
//! Order, exactly as spec'd in the design doc:
//!   1. Vars resolved, then derived vars computed and merged into scope.
//!   2. Static tree walked depth-first, alphabetical at each level.
//!   3. `generate:` entries processed in yaml list order; within each
//!      `for_each`, items processed in array order.

use crate::engine;
use crate::error::{Error, Result};
use crate::plan::{fingerprint, resolve_conflict, Action, Ledger, Op, Origin, Plan};
use crate::spec::{GenerateStep, Spec};
use crate::vars;
use crate::walker;
use liquid::Parser;
use serde_json::Value;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// Build an execution plan for `template_dir` with the given input vars.
pub fn build_plan(
    template_dir: &Path,
    spec: &Spec,
    input_vars: &Value,
    dest_root: &Path,
) -> Result<Plan> {
    if !template_dir.is_dir() {
        return Err(Error::TemplateRootNotDir(template_dir.to_path_buf()));
    }

    let parser = engine::parser()?;

    // ── Step 1: resolve user vars, then derived vars.
    let mut resolved = vars::resolve(spec, input_vars)?;
    vars::fill_object_defaults(spec, &mut resolved);

    let mut liquid_obj = vars::to_liquid(&resolved)?;
    let mut all_vars_json = serde_json::Map::new();
    all_vars_json.extend(resolved.clone());

    // Derived vars: evaluated in declaration order, each can refer to earlier ones.
    // BTreeMap iteration order is alphabetical; that's deterministic, even if it
    // means derived-var ordering is by key rather than spec-declaration order.
    for (key, expr) in &spec.derived {
        let rendered = match engine::render_expr(&parser, expr, &liquid_obj) {
            Ok(s) => s,
            Err(Error::LiquidRender { source, .. }) | Err(Error::LiquidParse { source, .. }) => {
                return Err(Error::DerivedVarRender {
                    name: key.clone(),
                    source,
                });
            }
            Err(other) => return Err(other),
        };
        let value = Value::String(rendered);
        all_vars_json.insert(key.clone(), value.clone());
        vars::insert_liquid_value(&mut liquid_obj, key, &value)?;
    }

    // ── Step 2: static tree.
    let mut ops: Vec<Op> = Vec::new();
    let mut ledger = Ledger::default();
    let mut collisions = 0usize;

    let static_entries = walker::walk(template_dir, spec, &parser, &liquid_obj)?;
    for entry in static_entries {
        let (size, sha) = fingerprint(&entry.bytes);
        let op_idx = ops.len();
        let op = Op {
            step: 0,
            action: Action::Create,
            dest: entry.dest.clone(),
            overrides_step: None,
            origin: Origin::Static {
                source: entry.source,
            },
            size,
            sha256: sha,
            bytes: entry.bytes,
        };
        ledger.record(entry.dest, 0, op_idx);
        ops.push(op);
    }

    // ── Step 3: generators.
    for (i, step) in spec.generate.iter().enumerate() {
        let step_num = i + 1; // 0 reserved for static tree
        run_generator(
            template_dir,
            &parser,
            &liquid_obj,
            step,
            step_num,
            i,
            &mut ops,
            &mut ledger,
            &mut collisions,
        )?;
    }

    let final_files = ops
        .iter()
        .filter(|o| !matches!(o.action, Action::Skip))
        .map(|o| o.dest.clone())
        .collect::<BTreeSet<_>>()
        .len();

    Ok(Plan {
        template_name: spec.name.clone(),
        template_version: spec.version.clone(),
        dest_root: dest_root.to_path_buf(),
        ops,
        final_files,
        collisions_resolved: collisions,
        vars_used: Value::Object(all_vars_json),
    })
}

#[allow(clippy::too_many_arguments)]
fn run_generator(
    template_dir: &Path,
    parser: &Parser,
    base_vars: &liquid::Object,
    step: &GenerateStep,
    step_num: usize,
    step_index_in_spec: usize,
    ops: &mut Vec<Op>,
    ledger: &mut Ledger,
    collisions: &mut usize,
) -> Result<()> {
    let template_path = template_dir.join(&step.template);
    if !template_path.is_file() {
        return Err(Error::GeneratorTemplateMissing(template_path));
    }
    let template_src = std::fs::read_to_string(&template_path).map_err(|e| Error::Io {
        path: template_path.clone(),
        source: e,
    })?;

    // `when:` evaluation when not iterating.
    let items: Vec<(Option<Value>, liquid::Object)> = if let Some(var_name) = &step.for_each {
        let v = base_vars
            .get(var_name.as_str())
            .ok_or_else(|| Error::ForEachUndefined {
                index: step_index_in_spec,
                var: var_name.clone(),
            })?;
        // Pull the array via JSON to avoid liquid's awkward ValueView/Array bridging.
        let as_json: Value = liquid_to_json(v).map_err(|e| Error::VarValidation {
            name: var_name.clone(),
            message: format!("for_each var not serialisable: {e}"),
        })?;
        let arr = as_json
            .as_array()
            .cloned()
            .ok_or_else(|| Error::ForEachNotArray {
                index: step_index_in_spec,
                var: var_name.clone(),
                got: json_type(&as_json).to_string(),
            })?;
        arr.into_iter()
            .map(|item| {
                let mut scope = base_vars.clone();
                let val = liquid_core::to_value(&item).expect("json -> liquid");
                scope.insert(step.r#as.clone().into(), val);
                (Some(item), scope)
            })
            .collect()
    } else {
        vec![(None, base_vars.clone())]
    };

    let mut seen_within_step: BTreeSet<PathBuf> = BTreeSet::new();

    for (item_value, scope) in items {
        if let Some(expr) = &step.when {
            let rendered = engine::render_expr(parser, &wrap_when(expr), &scope)?;
            if !is_truthy(&rendered) {
                continue;
            }
        }

        let dest_str = engine::render_expr(parser, &step.dest, &scope)?;
        let dest = PathBuf::from(dest_str);

        if !seen_within_step.insert(dest.clone()) {
            return Err(Error::WithinGeneratorCollision {
                index: step_index_in_spec,
                dest,
            });
        }

        let rendered_body = engine::render(parser, &template_src, &scope, &template_path)?;
        let bytes = rendered_body.into_bytes();

        let existing = ledger.get(&dest);
        let outcome = resolve_conflict(&dest, step_num, existing, step.on_conflict)?;
        let Some((action, overrides_step)) = outcome else {
            continue;
        };

        // For append, combine with the prior bytes (final fingerprint reflects
        // the on-disk result post-apply).
        let (final_bytes, size, sha) = if matches!(action, Action::Append) {
            let prior_op = existing
                .map(|e| &ops[e.op_index])
                .expect("append needs prior");
            let mut combined = prior_op.bytes.clone();
            if !combined.is_empty() && !combined.ends_with(b"\n") {
                combined.push(b'\n');
            }
            combined.extend_from_slice(&bytes);
            let (sz, sh) = fingerprint(&combined);
            (combined, sz, sh)
        } else if matches!(action, Action::Skip) {
            // No content change; record the would-be bytes for diagnostics.
            let (sz, sh) = fingerprint(&bytes);
            (bytes, sz, sh)
        } else {
            let (sz, sh) = fingerprint(&bytes);
            (bytes, sz, sh)
        };

        if matches!(action, Action::Overwrite | Action::Append) {
            *collisions += 1;
        }

        let op_idx = ops.len();
        let op = Op {
            step: step_num,
            action,
            dest: dest.clone(),
            overrides_step,
            origin: Origin::Generate {
                index: step_index_in_spec,
                template: PathBuf::from(&step.template),
                for_each_item: item_value,
            },
            size,
            sha256: sha,
            bytes: final_bytes,
        };

        // Skip ops don't update the ledger — the earlier op still owns the dest.
        if !matches!(action, Action::Skip) {
            ledger.record(dest, step_num, op_idx);
        }
        ops.push(op);
    }

    Ok(())
}

/// Wrap a `when:` expression so it renders to "true"/"false". The user writes
/// `use_tailwind` or `kind == "ssr"`; we turn it into `{% if … %}true{% else %}false{% endif %}`.
fn wrap_when(expr: &str) -> String {
    format!("{{% if {expr} %}}true{{% else %}}false{{% endif %}}")
}

fn is_truthy(rendered: &str) -> bool {
    matches!(rendered.trim(), "true")
}

/// Round-trip a liquid value through JSON so we can inspect array shape.
fn liquid_to_json(v: &liquid_core::Value) -> std::result::Result<Value, serde_json::Error> {
    let s = serde_json::to_string(v)?;
    serde_json::from_str(&s)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::Spec;
    use serde_json::json;
    use std::fs;

    fn write(p: &Path, body: &str) {
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(p, body).unwrap();
    }

    fn minimal_template(spec_yaml: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("__template__.yaml"), spec_yaml);
        dir
    }

    #[test]
    fn static_only_plan() {
        let dir = minimal_template("name: t\n");
        write(&dir.path().join("Cargo.toml"), "[package]\nname=\"x\"\n");
        write(&dir.path().join("src/main.rs"), "fn main() {}\n");

        let spec: Spec = serde_yaml::from_str("name: t\n").unwrap();
        let dest = tempfile::tempdir().unwrap();
        let plan = build_plan(dir.path(), &spec, &json!({}), dest.path()).unwrap();
        assert_eq!(plan.ops.len(), 2);
        assert!(plan.ops.iter().all(|o| o.step == 0));
        // Alphabetical at each level: Cargo.toml comes before src/.
        assert_eq!(plan.ops[0].dest, PathBuf::from("Cargo.toml"));
        assert_eq!(plan.ops[1].dest, PathBuf::from("src/main.rs"));
    }

    #[test]
    fn for_each_expands_items() {
        let spec_yaml = r#"
name: t
vars:
  components:
    type: array
    items:
      type: object
      schema:
        name: { type: string, required: true }
generate:
  - template: __templates__/component.rs.liquid
    for_each: components
    as: comp
    dest: "src/components/{{ comp.name | snake_case }}.rs"
"#;
        let dir = minimal_template(spec_yaml);
        write(
            &dir.path().join("__templates__/component.rs.liquid"),
            "pub struct {{ comp.name }};\n",
        );

        let spec: Spec = serde_yaml::from_str(spec_yaml).unwrap();
        let dest = tempfile::tempdir().unwrap();
        let plan = build_plan(
            dir.path(),
            &spec,
            &json!({"components": [{"name": "Button"}, {"name": "InputBox"}]}),
            dest.path(),
        )
        .unwrap();
        let gen_ops: Vec<_> = plan.ops.iter().filter(|o| o.step == 1).collect();
        assert_eq!(gen_ops.len(), 2);
        assert_eq!(gen_ops[0].dest, PathBuf::from("src/components/button.rs"));
        assert_eq!(
            gen_ops[1].dest,
            PathBuf::from("src/components/input_box.rs")
        );
        assert_eq!(gen_ops[0].bytes, b"pub struct Button;\n");
    }

    #[test]
    fn when_skips_step() {
        let spec_yaml = r#"
name: t
vars:
  use_tailwind: { type: bool, default: false }
generate:
  - template: __templates__/tw.liquid
    when: use_tailwind
    dest: tailwind.config.js
"#;
        let dir = minimal_template(spec_yaml);
        write(
            &dir.path().join("__templates__/tw.liquid"),
            "module.exports = {};\n",
        );

        let spec: Spec = serde_yaml::from_str(spec_yaml).unwrap();
        let dest = tempfile::tempdir().unwrap();

        let off = build_plan(dir.path(), &spec, &json!({}), dest.path()).unwrap();
        assert!(off
            .ops
            .iter()
            .all(|o| o.dest.as_path() != Path::new("tailwind.config.js")));

        let on = build_plan(
            dir.path(),
            &spec,
            &json!({"use_tailwind": true}),
            dest.path(),
        )
        .unwrap();
        assert!(on
            .ops
            .iter()
            .any(|o| o.dest.as_path() == Path::new("tailwind.config.js")));
    }

    #[test]
    fn conflict_default_errors() {
        let spec_yaml = r#"
name: t
generate:
  - template: __templates__/a.liquid
    dest: foo.txt
  - template: __templates__/b.liquid
    dest: foo.txt
"#;
        let dir = minimal_template(spec_yaml);
        write(&dir.path().join("__templates__/a.liquid"), "a\n");
        write(&dir.path().join("__templates__/b.liquid"), "b\n");

        let spec: Spec = serde_yaml::from_str(spec_yaml).unwrap();
        let dest = tempfile::tempdir().unwrap();
        let err = build_plan(dir.path(), &spec, &json!({}), dest.path()).unwrap_err();
        assert!(matches!(err, Error::Conflict { .. }));
    }

    #[test]
    fn conflict_overwrite_records_override() {
        let spec_yaml = r#"
name: t
generate:
  - template: __templates__/a.liquid
    dest: foo.txt
  - template: __templates__/b.liquid
    dest: foo.txt
    on_conflict: overwrite
"#;
        let dir = minimal_template(spec_yaml);
        write(&dir.path().join("__templates__/a.liquid"), "a\n");
        write(&dir.path().join("__templates__/b.liquid"), "b\n");

        let spec: Spec = serde_yaml::from_str(spec_yaml).unwrap();
        let dest = tempfile::tempdir().unwrap();
        let plan = build_plan(dir.path(), &spec, &json!({}), dest.path()).unwrap();
        assert_eq!(plan.collisions_resolved, 1);
        let final_op = plan
            .dest_index()
            .get(Path::new("foo.txt"))
            .copied()
            .cloned()
            .unwrap();
        assert_eq!(final_op.action, Action::Overwrite);
        assert_eq!(final_op.overrides_step, Some(1));
        assert_eq!(final_op.bytes, b"b\n");
    }

    #[test]
    fn append_combines_bytes_with_newline() {
        let spec_yaml = r#"
name: t
generate:
  - template: __templates__/base.liquid
    dest: notes.txt
  - template: __templates__/more.liquid
    dest: notes.txt
    on_conflict: append
"#;
        let dir = minimal_template(spec_yaml);
        write(&dir.path().join("__templates__/base.liquid"), "first");
        write(&dir.path().join("__templates__/more.liquid"), "second\n");

        let spec: Spec = serde_yaml::from_str(spec_yaml).unwrap();
        let dest = tempfile::tempdir().unwrap();
        let plan = build_plan(dir.path(), &spec, &json!({}), dest.path()).unwrap();

        let final_op = plan
            .dest_index()
            .get(Path::new("notes.txt"))
            .copied()
            .cloned()
            .unwrap();
        assert_eq!(final_op.action, Action::Append);
        assert_eq!(final_op.bytes, b"first\nsecond\n");
    }

    #[test]
    fn within_generator_collision_errors() {
        let spec_yaml = r#"
name: t
vars:
  components:
    type: array
    items:
      type: object
      schema:
        name: { type: string, required: true }
generate:
  - template: __templates__/c.liquid
    for_each: components
    as: c
    dest: "src/{{ c.name | snake_case }}.rs"
"#;
        let dir = minimal_template(spec_yaml);
        write(&dir.path().join("__templates__/c.liquid"), "x\n");

        let spec: Spec = serde_yaml::from_str(spec_yaml).unwrap();
        let dest = tempfile::tempdir().unwrap();
        let err = build_plan(
            dir.path(),
            &spec,
            &json!({"components": [{"name": "User"}, {"name": "user"}]}),
            dest.path(),
        )
        .unwrap_err();
        assert!(matches!(err, Error::WithinGeneratorCollision { .. }));
    }

    #[test]
    fn derived_vars_are_in_scope() {
        let spec_yaml = r#"
name: t
vars:
  project_name: { type: string, required: true }
derived:
  crate_name: "{{ project_name | snake_case }}"
generate:
  - template: __templates__/cargo.liquid
    dest: Cargo.toml
"#;
        let dir = minimal_template(spec_yaml);
        write(
            &dir.path().join("__templates__/cargo.liquid"),
            "name=\"{{ crate_name }}\"\n",
        );

        let spec: Spec = serde_yaml::from_str(spec_yaml).unwrap();
        let dest = tempfile::tempdir().unwrap();
        let plan = build_plan(
            dir.path(),
            &spec,
            &json!({"project_name": "Bells-Whistles"}),
            dest.path(),
        )
        .unwrap();
        let op = plan
            .ops
            .iter()
            .find(|o| o.dest.as_path() == Path::new("Cargo.toml"))
            .unwrap();
        assert_eq!(op.bytes, b"name=\"bells_whistles\"\n");
    }
}
