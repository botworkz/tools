//! The execution plan: an ordered list of file operations the engine intends
//! to perform.
//!
//! A plan is **inert** — building one never touches the filesystem. Applying
//! one does. This split is what makes `dry_run` honest and tests trivial:
//! snapshot a plan, compare it byte-for-byte to expectations.

use crate::spec::OnConflict;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// What the engine will do to `dest`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    /// Write a new file (no prior step touched this dest).
    Create,
    /// Replace an earlier step's file at this dest.
    Overwrite,
    /// Append `\n` + new content to an earlier step's file.
    Append,
    /// Earlier step wrote this dest; this step is skipped due to `on_conflict: skip`.
    Skip,
}

impl Action {
    pub fn as_str(self) -> &'static str {
        match self {
            Action::Create => "create",
            Action::Overwrite => "overwrite",
            Action::Append => "append",
            Action::Skip => "skip",
        }
    }
}

/// What produced this op — useful for diagnostics and dry-run output.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Origin {
    /// One file walked from the static template tree.
    Static { source: PathBuf },
    /// One emission from a `generate:` step.
    Generate {
        /// Index into `spec.generate`.
        index: usize,
        /// The `template:` field of the step (relative to template root).
        template: PathBuf,
        /// For for_each steps, the JSON value of the bound item.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        for_each_item: Option<serde_json::Value>,
    },
}

/// One concrete file operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Op {
    /// Step index in execution order (0 = static tree, 1.. = each `generate:` entry).
    pub step: usize,
    pub action: Action,
    pub dest: PathBuf,
    /// `Some(idx)` when this op deliberately overrides an earlier op.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overrides_step: Option<usize>,
    pub origin: Origin,
    /// Size of the would-be-written bytes (final size after append for append ops).
    pub size: u64,
    /// SHA-256 of the would-be-written bytes (final content for append ops).
    pub sha256: String,
    /// Bytes to write. Excluded from JSON serialisation by default to keep
    /// dry-run output readable; the CLI/MCP can ask for it explicitly.
    #[serde(skip)]
    pub bytes: Vec<u8>,
}

/// The result of planning.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Plan {
    pub template_name: String,
    pub template_version: String,
    pub dest_root: PathBuf,
    pub ops: Vec<Op>,
    /// Number of "create" ops in the final plan (i.e. distinct files that will exist).
    pub final_files: usize,
    /// How many times an override resolved a conflict.
    pub collisions_resolved: usize,
    /// Variables actually used to render, post-defaults and post-derived.
    pub vars_used: serde_json::Value,
}

impl Plan {
    /// Index ops by destination path, mapping each dest to the *most recent*
    /// op writing it. Useful for "what's the final state of this file?"
    pub fn dest_index(&self) -> BTreeMap<PathBuf, &Op> {
        let mut idx = BTreeMap::new();
        for op in &self.ops {
            if matches!(op.action, Action::Skip) {
                continue;
            }
            idx.insert(op.dest.clone(), op);
        }
        idx
    }
}

/// Helper: compute (size, sha256) for a byte slice.
pub fn fingerprint(bytes: &[u8]) -> (u64, String) {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    (bytes.len() as u64, hex::encode(digest))
}

/// Tracks earlier ops by dest during planning so the next step can detect
/// conflicts and apply the chosen [`OnConflict`] policy.
#[derive(Default)]
pub(crate) struct Ledger {
    /// dest -> (step, op index in the plan's ops vec)
    by_dest: BTreeMap<PathBuf, LedgerEntry>,
}

#[derive(Clone, Copy)]
pub(crate) struct LedgerEntry {
    pub step: usize,
    pub op_index: usize,
}

impl Ledger {
    pub fn record(&mut self, dest: PathBuf, step: usize, op_index: usize) {
        self.by_dest.insert(dest, LedgerEntry { step, op_index });
    }

    pub fn get(&self, dest: &std::path::Path) -> Option<LedgerEntry> {
        self.by_dest.get(dest).copied()
    }
}

/// Decide what action to take when `dest` already exists in the ledger.
///
/// Returns `(action, overrides_step)` or an `Err` matching the policy.
pub(crate) fn resolve_conflict(
    dest: &std::path::Path,
    step: usize,
    existing: Option<LedgerEntry>,
    policy: OnConflict,
) -> crate::error::Result<Option<(Action, Option<usize>)>> {
    match (existing, policy) {
        (None, OnConflict::Overwrite | OnConflict::Append) => {
            Err(crate::error::Error::NothingToOverride {
                dest: dest.to_path_buf(),
                step,
            })
        }
        (None, _) => Ok(Some((Action::Create, None))),
        (Some(e), OnConflict::Error) => Err(crate::error::Error::Conflict {
            dest: dest.to_path_buf(),
            new_step: step,
            existing_step: e.step,
            action: "create",
            policy: policy.as_str().to_string(),
        }),
        (Some(e), OnConflict::Overwrite) => Ok(Some((Action::Overwrite, Some(e.step)))),
        (Some(_), OnConflict::Skip) => Ok(Some((Action::Skip, None))),
        (Some(e), OnConflict::Append) => Ok(Some((Action::Append, Some(e.step)))),
        (Some(e), OnConflict::Upsert) => Ok(Some((Action::Overwrite, Some(e.step)))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Error;
    use std::path::Path;

    #[test]
    fn no_prior_create_is_create() {
        let (action, overrides) = resolve_conflict(Path::new("a"), 1, None, OnConflict::Error)
            .unwrap()
            .unwrap();
        assert_eq!(action, Action::Create);
        assert!(overrides.is_none());
    }

    #[test]
    fn no_prior_overwrite_errors() {
        let err = resolve_conflict(Path::new("a"), 1, None, OnConflict::Overwrite).unwrap_err();
        assert!(matches!(err, Error::NothingToOverride { .. }));
    }

    #[test]
    fn no_prior_upsert_is_create() {
        let (action, overrides) = resolve_conflict(Path::new("a"), 1, None, OnConflict::Upsert)
            .unwrap()
            .unwrap();
        assert_eq!(action, Action::Create);
        assert!(overrides.is_none());
    }

    #[test]
    fn prior_with_error_errors() {
        let entry = LedgerEntry {
            step: 0,
            op_index: 0,
        };
        let err = resolve_conflict(Path::new("a"), 1, Some(entry), OnConflict::Error).unwrap_err();
        assert!(matches!(err, Error::Conflict { .. }));
    }

    #[test]
    fn prior_with_overwrite_overrides() {
        let entry = LedgerEntry {
            step: 0,
            op_index: 0,
        };
        let (action, overrides) =
            resolve_conflict(Path::new("a"), 1, Some(entry), OnConflict::Overwrite)
                .unwrap()
                .unwrap();
        assert_eq!(action, Action::Overwrite);
        assert_eq!(overrides, Some(0));
    }

    #[test]
    fn prior_with_skip_skips() {
        let entry = LedgerEntry {
            step: 0,
            op_index: 0,
        };
        let (action, overrides) =
            resolve_conflict(Path::new("a"), 1, Some(entry), OnConflict::Skip)
                .unwrap()
                .unwrap();
        assert_eq!(action, Action::Skip);
        assert!(overrides.is_none());
    }
}
