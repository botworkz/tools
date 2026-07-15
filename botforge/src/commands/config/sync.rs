//! `botforge config sync` — B4.
//!
//! Reconciles the `build:`/`test:` registry in `botforge.yaml` with on-disk spec files.
//!
//! # Modes
//!
//! | invocation              | direction | effect                                              |
//! |-------------------------|-----------|-----------------------------------------------------|
//! | `sync`                  | inward    | dry-run diff (files → config)                       |
//! | `sync --write`          | inward    | regenerate registry in botforge.yaml                |
//! | `sync --check`          | none      | any drift → non-zero exit; writes nothing           |
//! | `sync --out`            | outward   | rewrite spec files' `name:`; warn about unsynced    |
//! | `sync --out --delete`   | outward   | name rewrites + delete files config dropped         |
//! | `sync --delete`         | —         | **error**: `--delete` requires `--out`              |

use anyhow::{bail, Context, Result};
use clap::Args;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};

use crate::workspace::{
    discover::discover,
    discover_context,
    registry::{load_committed_registry, save_registry},
};

const BUILD_DIR: &str = "build";
const MARKER_YAML: &str = "botforge.yaml";
const MARKER_YML: &str = "botforge.yml";

#[derive(Args, Debug)]
pub(crate) struct SyncArgs {
    /// Workspace context root. When omitted, botforge walks up from cwd.
    #[arg(long)]
    pub(crate) context: Option<PathBuf>,

    /// Write the regenerated registry to botforge.yaml (inward mode only).
    /// Without this flag, inward sync is a dry-run diff to stdout.
    #[arg(long)]
    pub(crate) write: bool,

    /// CI mode: compare committed registry vs discovered specs.
    /// Exits non-zero on any drift.  Writes nothing.
    #[arg(long)]
    pub(crate) check: bool,

    /// Outward reconciliation: rewrite each spec file's `name:` to match the
    /// registry key.  Warns about files the config no longer references.
    #[arg(long)]
    pub(crate) out: bool,

    /// Delete spec files the config no longer references (requires --out).
    #[arg(long)]
    pub(crate) delete: bool,
}

pub(crate) fn cmd_sync(context_override: Option<PathBuf>, args: SyncArgs) -> Result<()> {
    // --delete without --out is a hard error.
    if args.delete && !args.out {
        bail!("--delete requires --out");
    }

    // Merge context from two sources: explicit subcommand --context wins, then
    // config-group-level context (passed in from the caller).
    let context_path = args.context.as_deref().or(context_override.as_deref());
    let context = discover_context(context_path)?;

    if args.check {
        return do_check(&context);
    }

    if args.out {
        return do_out(&context, args.delete);
    }

    // Default: inward (files → config).
    do_inward(&context, args.write)
}

// ─── inward: files → config ───────────────────────────────────────────────────

fn do_inward(context: &Path, write: bool) -> Result<()> {
    let discovered = discover(context)?;
    let committed = load_committed_registry(context)?;

    // Compute the diff.
    let diff = compute_diff(
        &committed.builds,
        &discovered.builds,
        &committed.tests,
        &discovered.tests,
        context,
    );

    if diff.is_empty() {
        println!("registry is up to date — no changes needed");
        return Ok(());
    }

    // Print diff to stdout.
    print_diff(&diff);

    if write {
        save_registry(context, &discovered.builds, &discovered.tests)
            .context("failed to update registry in botforge.yaml")?;
        println!("registry updated in botforge.yaml");
    } else {
        println!("\nrun 'botforge config sync --write' to apply these changes");
    }

    Ok(())
}

// ─── check: detect drift ─────────────────────────────────────────────────────

fn do_check(context: &Path) -> Result<()> {
    let discovered = discover(context)?;
    let committed = load_committed_registry(context)?;

    let drift = compute_drift(
        &committed.builds,
        &discovered.builds,
        &committed.tests,
        &discovered.tests,
        context,
    );

    if drift.is_empty() {
        println!("registry is in sync — no drift detected");
        return Ok(());
    }

    eprintln!("drift detected:");
    for item in &drift {
        eprintln!("  {item}");
    }
    bail!("registry is out of sync ({} issue(s) found)", drift.len());
}

// ─── outward: config → files ─────────────────────────────────────────────────

fn do_out(context: &Path, delete: bool) -> Result<()> {
    let committed = load_committed_registry(context)?;

    // Validate: no two entries may share the same spec path.
    let mut all_spec_paths: BTreeMap<PathBuf, String> = BTreeMap::new();
    for (name, path) in committed.builds.iter().chain(committed.tests.iter()) {
        if let Some(prev_name) = all_spec_paths.insert(path.clone(), name.clone()) {
            bail!(
                "registry error: build/test entries '{}' and '{}' both point at '{}' \
                 — each spec file must have exactly one registry entry",
                prev_name,
                name,
                path.display()
            );
        }
    }

    // Validate: every spec path in the committed registry must exist on disk.
    for (name, path) in committed.builds.iter().chain(committed.tests.iter()) {
        if !path.is_file() {
            bail!(
                "spec file for entry '{}' does not exist: '{}'\n\
                 (--out conforms existing files; it does not create missing ones)",
                name,
                path.display()
            );
        }
    }

    // Perform name rewrites.
    let mut rewrites = 0usize;
    for (name, path) in committed.builds.iter().chain(committed.tests.iter()) {
        if rewrite_spec_name(path, name)? {
            rewrites += 1;
        }
    }

    if rewrites > 0 {
        println!("{rewrites} spec file(s) updated");
    } else {
        println!("all spec files are already in sync");
    }

    // Detect files that are discoverable but not referenced by the registry.
    // These are the files --out would warn about (and --out --delete would remove).
    let discovered = discover(context)?;

    let unreferenced: Vec<PathBuf> = collect_unreferenced(context, &committed, &discovered);

    if delete {
        // Delete unreferenced files.
        if unreferenced.is_empty() {
            println!("no unreferenced files to delete");
        } else {
            for path in &unreferenced {
                // Safety check: never cross build/ or nested workspace boundaries.
                if is_inside_build_dir(context, path) || is_nested_workspace(context, path) {
                    eprintln!(
                        "warning: skipping '{}' — inside build/ dir or nested workspace",
                        path.display()
                    );
                    continue;
                }
                std::fs::remove_file(path)
                    .with_context(|| format!("cannot delete '{}'", path.display()))?;
                println!("deleted: {}", path.display());
            }
        }
    } else if !unreferenced.is_empty() {
        // Warn about unsynced remainder.
        eprintln!(
            "warning: repo not fully in sync — {} file(s) are no longer referenced by the \
             config and were NOT deleted:",
            unreferenced.len()
        );
        for path in &unreferenced {
            let rel = path.strip_prefix(context).unwrap_or(path.as_path());
            eprintln!("  {}", rel.display());
        }
        eprintln!("run 'botforge config sync --out --delete' to remove them");
    }

    Ok(())
}

// ─── helpers ──────────────────────────────────────────────────────────────────

/// Collect spec files that are discoverable on disk but not referenced by the
/// committed registry (i.e. the set that `--out --delete` would remove).
fn collect_unreferenced(
    _context: &Path,
    committed: &crate::workspace::registry::CommittedRegistry,
    discovered: &crate::workspace::discover::Registry,
) -> Vec<PathBuf> {
    let registered_paths: BTreeSet<&PathBuf> = committed
        .builds
        .values()
        .chain(committed.tests.values())
        .collect();

    let mut unreferenced = Vec::new();

    for path in discovered.builds.values().chain(discovered.tests.values()) {
        if !registered_paths.contains(path) {
            unreferenced.push(path.clone());
        }
    }

    unreferenced.sort();
    unreferenced.dedup();
    unreferenced
}

/// Returns `true` if `path` is inside `<context_root>/build/`.
fn is_inside_build_dir(context: &Path, path: &Path) -> bool {
    path.starts_with(context.join(BUILD_DIR))
}

/// Returns `true` if `path` is inside a nested workspace (a subdirectory with
/// its own `botforge.yaml` or `botforge.yml`).
fn is_nested_workspace(context: &Path, path: &Path) -> bool {
    if let Ok(rel) = path.strip_prefix(context) {
        let mut check = PathBuf::new();
        for component in rel.components() {
            if component == Component::Normal(std::ffi::OsStr::new("")) {
                continue;
            }
            check.push(component);
            let candidate = context.join(&check);
            if candidate != *context
                && (candidate.join(MARKER_YAML).is_file() || candidate.join(MARKER_YML).is_file())
            {
                return true;
            }
        }
    }
    false
}

/// Rewrite the `name:` field in a spec YAML file to `new_name`.
///
/// Returns `true` if the file was modified, `false` if it already had the
/// correct name.
fn rewrite_spec_name(path: &Path, new_name: &str) -> Result<bool> {
    let contents = std::fs::read_to_string(path)
        .with_context(|| format!("cannot read spec file: {}", path.display()))?;

    let mut doc: serde_yaml::Value = serde_yaml::from_str(&contents)
        .with_context(|| format!("invalid YAML in spec file: {}", path.display()))?;

    let map = doc
        .as_mapping_mut()
        .ok_or_else(|| anyhow::anyhow!("spec file is not a YAML mapping: {}", path.display()))?;

    let current_name = map.get("name").and_then(|v| v.as_str()).map(str::to_string);

    if current_name.as_deref() == Some(new_name) {
        return Ok(false); // Already correct — no write needed.
    }

    map.insert(
        serde_yaml::Value::String("name".to_string()),
        serde_yaml::Value::String(new_name.to_string()),
    );

    let updated = serde_yaml::to_string(&doc)
        .with_context(|| format!("failed to serialize spec file: {}", path.display()))?;

    std::fs::write(path, updated)
        .with_context(|| format!("cannot write spec file: {}", path.display()))?;

    Ok(true)
}

// ─── diff / drift computation ─────────────────────────────────────────────────

#[derive(Debug, PartialEq)]
enum DiffItem {
    /// A build/test entry is in the discovered set but not the registry.
    Added {
        kind: &'static str,
        name: String,
        path: String,
    },
    /// A build/test entry is in the registry but not the discovered set.
    Removed {
        kind: &'static str,
        name: String,
        path: String,
    },
    /// Same name exists in both but the spec path differs.
    Changed {
        kind: &'static str,
        name: String,
        from: String,
        to: String,
    },
}

impl std::fmt::Display for DiffItem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DiffItem::Added { kind, name, path } => {
                write!(f, "+ {kind} '{name}' → {path}")
            }
            DiffItem::Removed { kind, name, path } => {
                write!(f, "- {kind} '{name}' (was {path})")
            }
            DiffItem::Changed {
                kind,
                name,
                from,
                to,
            } => {
                write!(f, "~ {kind} '{name}': {from} → {to}")
            }
        }
    }
}

fn diff_map(
    kind: &'static str,
    committed: &BTreeMap<String, PathBuf>,
    discovered: &BTreeMap<String, PathBuf>,
    context: &Path,
) -> Vec<DiffItem> {
    let mut items = Vec::new();

    for (name, disc_path) in discovered {
        let rel = disc_path
            .strip_prefix(context)
            .unwrap_or(disc_path.as_path());
        match committed.get(name) {
            None => items.push(DiffItem::Added {
                kind,
                name: name.clone(),
                path: rel.display().to_string(),
            }),
            Some(comm_path) if comm_path != disc_path => {
                let comm_rel = comm_path
                    .strip_prefix(context)
                    .unwrap_or(comm_path.as_path());
                items.push(DiffItem::Changed {
                    kind,
                    name: name.clone(),
                    from: comm_rel.display().to_string(),
                    to: rel.display().to_string(),
                });
            }
            Some(_) => {}
        }
    }

    for (name, comm_path) in committed {
        if !discovered.contains_key(name) {
            let rel = comm_path
                .strip_prefix(context)
                .unwrap_or(comm_path.as_path());
            items.push(DiffItem::Removed {
                kind,
                name: name.clone(),
                path: rel.display().to_string(),
            });
        }
    }

    items
}

fn compute_diff(
    comm_builds: &BTreeMap<String, PathBuf>,
    disc_builds: &BTreeMap<String, PathBuf>,
    comm_tests: &BTreeMap<String, PathBuf>,
    disc_tests: &BTreeMap<String, PathBuf>,
    context: &Path,
) -> Vec<DiffItem> {
    let mut items = diff_map("build", comm_builds, disc_builds, context);
    items.extend(diff_map("test", comm_tests, disc_tests, context));
    items
}

fn print_diff(items: &[DiffItem]) {
    println!("registry diff (+ add, - remove, ~ change):");
    for item in items {
        println!("  {item}");
    }
}

// ─── drift computation (for --check) ─────────────────────────────────────────

/// Compute the full symmetric drift between committed and discovered registries.
///
/// Drift items are human-readable strings describing the discrepancy.
fn compute_drift(
    comm_builds: &BTreeMap<String, PathBuf>,
    disc_builds: &BTreeMap<String, PathBuf>,
    comm_tests: &BTreeMap<String, PathBuf>,
    disc_tests: &BTreeMap<String, PathBuf>,
    context: &Path,
) -> Vec<String> {
    let mut items = Vec::new();

    // --- builds ---
    for (name, disc_path) in disc_builds {
        let rel = disc_path
            .strip_prefix(context)
            .unwrap_or(disc_path.as_path());
        match comm_builds.get(name) {
            None => items.push(format!(
                "build '{}' is discoverable at '{}' but not in the registry",
                name,
                rel.display()
            )),
            Some(comm_path) if comm_path != disc_path => {
                let comm_rel = comm_path
                    .strip_prefix(context)
                    .unwrap_or(comm_path.as_path());
                items.push(format!(
                    "build '{}' is registered at '{}' but discovered at '{}'",
                    name,
                    comm_rel.display(),
                    rel.display()
                ));
            }
            Some(_) => {}
        }
    }
    for (name, comm_path) in comm_builds {
        if !disc_builds.contains_key(name) {
            let rel = comm_path
                .strip_prefix(context)
                .unwrap_or(comm_path.as_path());
            items.push(format!(
                "build '{}' is in the registry at '{}' but not discoverable on disk",
                name,
                rel.display()
            ));
        }
    }

    // --- tests ---
    for (name, disc_path) in disc_tests {
        let rel = disc_path
            .strip_prefix(context)
            .unwrap_or(disc_path.as_path());
        match comm_tests.get(name) {
            None => items.push(format!(
                "test '{}' is discoverable at '{}' but not in the registry",
                name,
                rel.display()
            )),
            Some(comm_path) if comm_path != disc_path => {
                let comm_rel = comm_path
                    .strip_prefix(context)
                    .unwrap_or(comm_path.as_path());
                items.push(format!(
                    "test '{}' is registered at '{}' but discovered at '{}'",
                    name,
                    comm_rel.display(),
                    rel.display()
                ));
            }
            Some(_) => {}
        }
    }
    for (name, comm_path) in comm_tests {
        if !disc_tests.contains_key(name) {
            let rel = comm_path
                .strip_prefix(context)
                .unwrap_or(comm_path.as_path());
            items.push(format!(
                "test '{}' is in the registry at '{}' but not discoverable on disk",
                name,
                rel.display()
            ));
        }
    }

    items
}

// ─── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn write_marker(dir: &Path, content: &str) {
        fs::write(dir.join(MARKER_YAML), content).unwrap();
    }

    fn write_build_doc(dir: &Path, filename: &str, name: &str) {
        let content =
            format!("type: botforge/build\nname: {name}\nimage: \"@base\"\noutput: out.qcow2\n");
        fs::write(dir.join(filename), content).unwrap();
    }

    #[allow(dead_code)]
    fn write_test_doc(dir: &Path, filename: &str, name: &str) {
        let content = format!("type: botforge/test\nname: {name}\n");
        fs::write(dir.join(filename), content).unwrap();
    }

    // ── --delete without --out ────────────────────────────────────────────────

    #[test]
    fn delete_without_out_errors() {
        let root = TempDir::new().unwrap();
        write_marker(root.path(), "");
        let args = SyncArgs {
            context: Some(root.path().to_path_buf()),
            write: false,
            check: false,
            out: false,
            delete: true,
        };
        let err = cmd_sync(None, args).unwrap_err();
        assert!(
            format!("{err:#}").contains("--delete requires --out"),
            "{err:#}"
        );
    }

    // ── inward dry-run ────────────────────────────────────────────────────────

    #[test]
    fn inward_dryrun_no_write() {
        let root = TempDir::new().unwrap();
        write_marker(root.path(), "");
        write_build_doc(root.path(), "foo.yaml", "foo");

        let args = SyncArgs {
            context: Some(root.path().to_path_buf()),
            write: false,
            check: false,
            out: false,
            delete: false,
        };
        cmd_sync(None, args).unwrap();

        // Marker should still be empty (dry-run, no write).
        let contents = fs::read_to_string(root.path().join(MARKER_YAML)).unwrap();
        assert!(
            !contents.contains("foo"),
            "dry-run should not write: {contents}"
        );
    }

    #[test]
    fn inward_write_updates_registry() {
        let root = TempDir::new().unwrap();
        write_marker(root.path(), "");
        write_build_doc(root.path(), "foo.yaml", "foo");

        let args = SyncArgs {
            context: Some(root.path().to_path_buf()),
            write: true,
            check: false,
            out: false,
            delete: false,
        };
        cmd_sync(None, args).unwrap();

        let committed = load_committed_registry(root.path()).unwrap();
        assert!(
            committed.builds.contains_key("foo"),
            "registry should contain 'foo'"
        );
    }

    // ── --check ───────────────────────────────────────────────────────────────

    #[test]
    fn check_clean_exits_zero() {
        let root = TempDir::new().unwrap();
        write_build_doc(root.path(), "foo.yaml", "foo");
        write_marker(root.path(), "plans:\n  foo:\n    build: foo.yaml\n");

        let args = SyncArgs {
            context: Some(root.path().to_path_buf()),
            write: false,
            check: true,
            out: false,
            delete: false,
        };
        assert!(cmd_sync(None, args).is_ok());
    }

    #[test]
    fn check_discovered_but_unregistered_fails() {
        let root = TempDir::new().unwrap();
        write_marker(root.path(), "");
        write_build_doc(root.path(), "foo.yaml", "foo");

        let args = SyncArgs {
            context: Some(root.path().to_path_buf()),
            write: false,
            check: true,
            out: false,
            delete: false,
        };
        let err = cmd_sync(None, args).unwrap_err();
        assert!(format!("{err:#}").contains("out of sync"), "{err:#}");
    }

    #[test]
    fn check_registered_but_missing_fails() {
        let root = TempDir::new().unwrap();
        write_marker(root.path(), "plans:\n  ghost:\n    build: ghost.yaml\n");
        // ghost.yaml not on disk → not discoverable → drift.
        let args = SyncArgs {
            context: Some(root.path().to_path_buf()),
            write: false,
            check: true,
            out: false,
            delete: false,
        };
        let err = cmd_sync(None, args).unwrap_err();
        assert!(format!("{err:#}").contains("out of sync"), "{err:#}");
    }

    // ── --out: name rewrite ───────────────────────────────────────────────────

    #[test]
    fn out_rewrites_spec_name() {
        let root = TempDir::new().unwrap();
        // Registry says `bar` → baz.yaml; baz.yaml currently has name: foo.
        let spec_path = root.path().join("baz.yaml");
        write_build_doc(root.path(), "baz.yaml", "foo");
        write_marker(root.path(), "plans:\n  bar:\n    build: baz.yaml\n");

        let args = SyncArgs {
            context: Some(root.path().to_path_buf()),
            write: false,
            check: false,
            out: true,
            delete: false,
        };
        cmd_sync(None, args).unwrap();

        let contents = fs::read_to_string(&spec_path).unwrap();
        assert!(
            contents.contains("name: bar"),
            "name should have been rewritten to 'bar': {contents}"
        );
    }

    #[test]
    fn out_no_rewrite_when_already_correct() {
        let root = TempDir::new().unwrap();
        write_build_doc(root.path(), "foo.yaml", "foo");
        write_marker(root.path(), "plans:\n  foo:\n    build: foo.yaml\n");

        let args = SyncArgs {
            context: Some(root.path().to_path_buf()),
            write: false,
            check: false,
            out: true,
            delete: false,
        };
        cmd_sync(None, args).unwrap(); // Should succeed without error.
    }

    #[test]
    fn out_errors_when_spec_file_missing() {
        let root = TempDir::new().unwrap();
        write_marker(root.path(), "plans:\n  missing:\n    build: missing.yaml\n");
        // missing.yaml does not exist on disk.
        let args = SyncArgs {
            context: Some(root.path().to_path_buf()),
            write: false,
            check: false,
            out: true,
            delete: false,
        };
        let err = cmd_sync(None, args).unwrap_err();
        assert!(format!("{err:#}").contains("does not exist"), "{err:#}");
    }

    #[test]
    fn out_errors_when_two_entries_share_spec_path() {
        let root = TempDir::new().unwrap();
        write_build_doc(root.path(), "shared.yaml", "x");
        write_marker(
            root.path(),
            "plans:\n  a:\n    build: shared.yaml\n  b:\n    build: shared.yaml\n",
        );
        let args = SyncArgs {
            context: Some(root.path().to_path_buf()),
            write: false,
            check: false,
            out: true,
            delete: false,
        };
        let err = cmd_sync(None, args).unwrap_err();
        assert!(format!("{err:#}").contains("both point at"), "{err:#}");
    }

    // ── --out: unsynced remainder warning ─────────────────────────────────────

    #[test]
    fn out_warns_about_unreferenced_files_but_does_not_delete() {
        let root = TempDir::new().unwrap();
        // Config references only "bar"; "old.yaml" is discoverable but not registered.
        write_build_doc(root.path(), "bar.yaml", "foo");
        write_build_doc(root.path(), "old.yaml", "old");
        write_marker(root.path(), "plans:\n  bar:\n    build: bar.yaml\n");

        let args = SyncArgs {
            context: Some(root.path().to_path_buf()),
            write: false,
            check: false,
            out: true,
            delete: false,
        };
        cmd_sync(None, args).unwrap(); // Must succeed (just warn).

        // old.yaml must still exist.
        assert!(
            root.path().join("old.yaml").exists(),
            "old.yaml should NOT have been deleted"
        );
    }

    // ── --out --delete ────────────────────────────────────────────────────────

    #[test]
    fn out_delete_removes_unreferenced_file() {
        let root = TempDir::new().unwrap();
        write_build_doc(root.path(), "kept.yaml", "kept");
        write_build_doc(root.path(), "dropped.yaml", "dropped");
        write_marker(root.path(), "plans:\n  kept:\n    build: kept.yaml\n");

        let args = SyncArgs {
            context: Some(root.path().to_path_buf()),
            write: false,
            check: false,
            out: true,
            delete: true,
        };
        cmd_sync(None, args).unwrap();

        assert!(
            root.path().join("kept.yaml").exists(),
            "kept.yaml should still exist"
        );
        assert!(
            !root.path().join("dropped.yaml").exists(),
            "dropped.yaml should have been deleted"
        );
    }

    #[test]
    fn out_delete_never_crosses_build_dir() {
        let root = TempDir::new().unwrap();
        let build_dir = root.path().join("build");
        fs::create_dir_all(&build_dir).unwrap();
        // Place a spec inside build/ — it should be pruned by discover and thus
        // never appear as unreferenced.
        write_build_doc(&build_dir, "artifact.yaml", "artifact");
        write_build_doc(root.path(), "real.yaml", "real");
        write_marker(root.path(), "plans:\n  real:\n    build: real.yaml\n");

        let args = SyncArgs {
            context: Some(root.path().to_path_buf()),
            write: false,
            check: false,
            out: true,
            delete: true,
        };
        cmd_sync(None, args).unwrap();

        // artifact.yaml is inside build/ so discover() never returns it;
        // it should be untouched.
        assert!(
            build_dir.join("artifact.yaml").exists(),
            "files inside build/ should never be touched"
        );
    }

    // ── rewrite_spec_name ─────────────────────────────────────────────────────

    #[test]
    fn rewrite_spec_name_updates_name_field() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("spec.yaml");
        fs::write(&path, "type: botforge/build\nname: old\nimage: \"@x\"\n").unwrap();
        let changed = rewrite_spec_name(&path, "new").unwrap();
        assert!(changed);
        let contents = fs::read_to_string(&path).unwrap();
        assert!(contents.contains("name: new"), "{contents}");
    }

    #[test]
    fn rewrite_spec_name_noop_when_already_correct() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("spec.yaml");
        fs::write(&path, "type: botforge/build\nname: foo\n").unwrap();
        let changed = rewrite_spec_name(&path, "foo").unwrap();
        assert!(!changed);
    }

    // ── compute_drift ─────────────────────────────────────────────────────────

    #[test]
    fn drift_detected_discovered_unregistered() {
        let context = TempDir::new().unwrap();
        let comm = BTreeMap::new();
        let mut disc = BTreeMap::new();
        disc.insert("foo".to_string(), context.path().join("foo.yaml"));
        let drift = compute_drift(
            &comm,
            &disc,
            &BTreeMap::new(),
            &BTreeMap::new(),
            context.path(),
        );
        assert_eq!(drift.len(), 1);
        assert!(drift[0].contains("discoverable"), "{}", drift[0]);
        assert!(drift[0].contains("not in the registry"), "{}", drift[0]);
    }

    #[test]
    fn drift_detected_registered_missing() {
        let context = TempDir::new().unwrap();
        let mut comm = BTreeMap::new();
        comm.insert("ghost".to_string(), context.path().join("ghost.yaml"));
        let disc = BTreeMap::new();
        let drift = compute_drift(
            &comm,
            &disc,
            &BTreeMap::new(),
            &BTreeMap::new(),
            context.path(),
        );
        assert_eq!(drift.len(), 1);
        assert!(drift[0].contains("not discoverable"), "{}", drift[0]);
    }

    #[test]
    fn drift_detected_path_mismatch() {
        let context = TempDir::new().unwrap();
        let mut comm = BTreeMap::new();
        comm.insert("foo".to_string(), context.path().join("a.yaml"));
        let mut disc = BTreeMap::new();
        disc.insert("foo".to_string(), context.path().join("b.yaml"));
        let drift = compute_drift(
            &comm,
            &disc,
            &BTreeMap::new(),
            &BTreeMap::new(),
            context.path(),
        );
        assert_eq!(drift.len(), 1);
        assert!(drift[0].contains("registered at"), "{}", drift[0]);
    }

    #[test]
    fn no_drift_when_exact_match() {
        let context = TempDir::new().unwrap();
        let mut comm = BTreeMap::new();
        comm.insert("foo".to_string(), context.path().join("foo.yaml"));
        let mut disc = BTreeMap::new();
        disc.insert("foo".to_string(), context.path().join("foo.yaml"));
        let drift = compute_drift(
            &comm,
            &disc,
            &BTreeMap::new(),
            &BTreeMap::new(),
            context.path(),
        );
        assert!(drift.is_empty(), "{drift:?}");
    }
}
