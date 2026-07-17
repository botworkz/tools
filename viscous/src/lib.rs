#![forbid(unsafe_code)]
//! `viscous` — opinionated, agent-friendly directory template generator.
//!
//! The library exposes three pure layers:
//!   - [`spec`] — parse and validate `__template__.yaml`.
//!   - [`vars`] — validate user input against a spec, apply defaults.
//!   - [`planner`] — combine spec + vars + filesystem walk into a [`Plan`].
//!
//! The [`apply`] module is the only I/O sink: it materialises a plan to disk.
//!
//! # Two trees, one mechanism
//!
//! A template is a directory containing `__template__.yaml`. Files live in
//! one of two trees:
//!
//! - **Static tree** — every file outside `__templates__/` is walked
//!   alphabetically, has filename + body rendered through liquid (unless
//!   marked verbatim), and is emitted 1:1.
//! - **Generator tree** — files under `__templates__/` are only emitted when
//!   referenced by a `generate:` entry. Generators support `for_each` (1→N),
//!   `when` (1→0|1), and per-entry `on_conflict` policy.
//!
//! Both trees feed the same [`Plan`]; the static tree is the implicit step 0,
//! generator entries are steps 1..N in yaml list order.

pub mod apply;
pub mod engine;
pub mod error;
mod glob;
pub mod plan;
pub mod planner;
pub mod spec;
pub mod vars;
pub mod walker;

pub use apply::{apply, DestPolicy};
pub use error::{Error, Result};
pub use plan::{Action, Op, Origin, Plan};
pub use planner::build_plan;
pub use spec::{OnConflict, Spec, VarSpec, VarType, GENERATORS_DIRNAME, SPEC_FILENAME};

use std::path::Path;

/// Load the spec for the template at `template_dir`.
pub fn describe(template_dir: &Path) -> Result<Spec> {
    Spec::load_from_dir(template_dir)
}

/// One-shot helper: build a plan and apply it under `dest_root`.
///
/// Equivalent to `build_plan(...)` followed by `apply(...)`; callers that
/// want dry-run or pre-flight inspection should use the two-step form.
pub fn generate(
    template_dir: &Path,
    input_vars: &serde_json::Value,
    dest_root: &Path,
    policy: DestPolicy,
) -> Result<Plan> {
    let spec = Spec::load_from_dir(template_dir)?;
    let plan = build_plan(template_dir, &spec, input_vars, dest_root)?;
    apply(&plan, policy)?;
    Ok(plan)
}
