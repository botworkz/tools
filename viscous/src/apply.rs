//! Apply a [`Plan`] to disk.
//!
//! The plan is the only thing that does I/O against the destination. It
//! refuses to write into a non-empty directory unless the caller opts in,
//! and never deletes files it didn't put there.

use crate::error::{Error, Result};
use crate::plan::{Action, Plan};
use std::path::Path;

/// Conflict-resolution mode for an already-populated destination.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DestPolicy {
    /// Refuse to write into a non-empty directory.
    #[default]
    RequireEmpty,
    /// Write into an existing directory, refusing to overwrite individual files.
    Merge,
    /// Write into an existing directory, overwriting any colliding files.
    Overwrite,
}

/// Apply `plan` to disk under `plan.dest_root`. Returns the list of written
/// destinations (relative to dest_root, in execution order, skips omitted).
pub fn apply(plan: &Plan, policy: DestPolicy) -> Result<Vec<std::path::PathBuf>> {
    let root = &plan.dest_root;
    prepare_dest(root, policy)?;

    let mut written = Vec::new();
    for op in &plan.ops {
        match op.action {
            Action::Skip => continue,
            Action::Create | Action::Overwrite | Action::Append => {
                let abs = root.join(&op.dest);

                if matches!(op.action, Action::Create)
                    && matches!(policy, DestPolicy::Merge)
                    && abs.exists()
                {
                    return Err(Error::Conflict {
                        dest: op.dest.clone(),
                        new_step: op.step,
                        existing_step: usize::MAX,
                        action: "create",
                        policy: "merge".into(),
                    });
                }

                if let Some(parent) = abs.parent() {
                    std::fs::create_dir_all(parent).map_err(|e| Error::Io {
                        path: parent.to_path_buf(),
                        source: e,
                    })?;
                }
                std::fs::write(&abs, &op.bytes).map_err(|e| Error::Io {
                    path: abs.clone(),
                    source: e,
                })?;
                written.push(op.dest.clone());
            }
        }
    }
    Ok(written)
}

fn prepare_dest(root: &Path, policy: DestPolicy) -> Result<()> {
    if root.exists() {
        if root.is_file() {
            return Err(Error::DestIsFile(root.to_path_buf()));
        }
        if matches!(policy, DestPolicy::RequireEmpty) {
            let mut entries = std::fs::read_dir(root).map_err(|e| Error::Io {
                path: root.to_path_buf(),
                source: e,
            })?;
            if entries.next().is_some() {
                return Err(Error::DestNotEmpty(root.to_path_buf()));
            }
        }
    } else {
        std::fs::create_dir_all(root).map_err(|e| Error::Io {
            path: root.to_path_buf(),
            source: e,
        })?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::{fingerprint, Op, Origin, Plan};
    use std::path::PathBuf;

    fn op(dest: &str, body: &str, step: usize, action: Action) -> Op {
        let bytes = body.as_bytes().to_vec();
        let (size, sha) = fingerprint(&bytes);
        Op {
            step,
            action,
            dest: PathBuf::from(dest),
            overrides_step: None,
            origin: Origin::Static {
                source: PathBuf::from("(test)"),
            },
            size,
            sha256: sha,
            bytes,
        }
    }

    #[test]
    fn writes_files_in_order_with_subdirs() {
        let dest = tempfile::tempdir().unwrap();
        let plan = Plan {
            template_name: "t".into(),
            template_version: "".into(),
            dest_root: dest.path().to_path_buf(),
            ops: vec![
                op("a/b/c.txt", "hello\n", 0, Action::Create),
                op("top.txt", "top\n", 0, Action::Create),
            ],
            final_files: 2,
            collisions_resolved: 0,
            vars_used: serde_json::json!({}),
        };
        let written = apply(&plan, DestPolicy::RequireEmpty).unwrap();
        assert_eq!(written.len(), 2);
        assert_eq!(
            std::fs::read_to_string(dest.path().join("a/b/c.txt")).unwrap(),
            "hello\n"
        );
        assert_eq!(
            std::fs::read_to_string(dest.path().join("top.txt")).unwrap(),
            "top\n"
        );
    }

    #[test]
    fn refuses_nonempty_dest_by_default() {
        let dest = tempfile::tempdir().unwrap();
        std::fs::write(dest.path().join("intruder"), "x").unwrap();
        let plan = Plan {
            template_name: "t".into(),
            template_version: "".into(),
            dest_root: dest.path().to_path_buf(),
            ops: vec![op("a.txt", "x", 0, Action::Create)],
            final_files: 1,
            collisions_resolved: 0,
            vars_used: serde_json::json!({}),
        };
        let err = apply(&plan, DestPolicy::RequireEmpty).unwrap_err();
        assert!(matches!(err, Error::DestNotEmpty(_)));
    }

    #[test]
    fn merge_mode_refuses_overwrite_of_existing_files() {
        let dest = tempfile::tempdir().unwrap();
        std::fs::write(dest.path().join("a.txt"), "old").unwrap();
        let plan = Plan {
            template_name: "t".into(),
            template_version: "".into(),
            dest_root: dest.path().to_path_buf(),
            ops: vec![op("a.txt", "new", 0, Action::Create)],
            final_files: 1,
            collisions_resolved: 0,
            vars_used: serde_json::json!({}),
        };
        let err = apply(&plan, DestPolicy::Merge).unwrap_err();
        assert!(matches!(err, Error::Conflict { .. }));
    }

    #[test]
    fn overwrite_mode_clobbers() {
        let dest = tempfile::tempdir().unwrap();
        std::fs::write(dest.path().join("a.txt"), "old").unwrap();
        let plan = Plan {
            template_name: "t".into(),
            template_version: "".into(),
            dest_root: dest.path().to_path_buf(),
            ops: vec![op("a.txt", "new", 0, Action::Create)],
            final_files: 1,
            collisions_resolved: 0,
            vars_used: serde_json::json!({}),
        };
        apply(&plan, DestPolicy::Overwrite).unwrap();
        assert_eq!(
            std::fs::read_to_string(dest.path().join("a.txt")).unwrap(),
            "new"
        );
    }
}
