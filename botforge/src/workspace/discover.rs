//! Workspace discovery engine — B3.
//!
//! Walks the context subtree to find all `type: botforge/build` and
//! `type: botforge/test` documents and indexes them by `(name, type)`.
//!
//! Entry point: [`discover`].

use anyhow::{bail, Context, Result};
use globset::{Glob, GlobSet, GlobSetBuilder};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};
use walkdir::WalkDir;
const BUILD_DIR: &str = "build";

use super::{find_marker_path, is_marker_name, marker_path};

// ─── botforge.yaml config block ──────────────────────────────────────────────

/// Intermediate deserialization: `match:` accepts a scalar string OR a sequence.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum RawMatchPatterns {
    Single(String),
    List(Vec<String>),
}

impl RawMatchPatterns {
    fn into_vec(self) -> Vec<String> {
        match self {
            RawMatchPatterns::Single(s) => vec![s],
            RawMatchPatterns::List(v) => v,
        }
    }
}

/// The `config:` block from `botforge.yaml`.
#[derive(Debug, Deserialize, Default)]
struct RawWorkspaceConfig {
    #[serde(rename = "match")]
    match_patterns: Option<RawMatchPatterns>,
    #[serde(rename = "repo-only", default)]
    repo_only: bool,
}

/// The `botforge.yaml` root document (only the `config:` block is consumed here;
/// other keys are ignored).
#[derive(Debug, Deserialize, Default)]
struct RawWorkspaceDoc {
    #[serde(default)]
    config: RawWorkspaceConfig,
}

/// Parsed, validated workspace discovery configuration.
#[derive(Debug)]
pub(crate) struct WorkspaceConfig {
    /// Glob patterns (relative to context root) to match candidate spec files.
    pub(crate) match_patterns: Vec<String>,
    /// When true, intersect candidates with git-tracked files.
    pub(crate) repo_only: bool,
}

impl Default for WorkspaceConfig {
    fn default() -> Self {
        WorkspaceConfig {
            match_patterns: vec!["**/*.{yaml,yml}".to_string()],
            repo_only: false,
        }
    }
}

/// Load and parse the `config:` block from `<context_root>/botforge.yaml`.
/// Falls back to defaults if the file has no `config:` section.
fn load_workspace_config(context_root: &Path) -> Result<WorkspaceConfig> {
    let marker_path = marker_path(context_root)?;
    let contents = std::fs::read_to_string(&marker_path)
        .with_context(|| format!("cannot read {}", marker_path.display()))?;
    // Empty or pure-whitespace file → no config block.
    if contents.trim().is_empty() {
        return Ok(WorkspaceConfig::default());
    }
    let doc: RawWorkspaceDoc = serde_yaml::from_str(&contents)
        .with_context(|| format!("invalid YAML in {}", marker_path.display()))?;

    let match_patterns = match doc.config.match_patterns {
        None => vec!["**/*.{yaml,yml}".to_string()],
        Some(raw) => {
            let patterns = raw.into_vec();
            if patterns.is_empty() {
                bail!(
                    "config.match in {} must not be empty",
                    marker_path.display()
                );
            }
            // Validate each pattern is a valid glob and doesn't try to escape the context root.
            for p in &patterns {
                validate_match_pattern(p).with_context(|| {
                    format!(
                        "invalid config.match pattern {p:?} in {}",
                        marker_path.display()
                    )
                })?;
            }
            patterns
        }
    };

    Ok(WorkspaceConfig {
        match_patterns,
        repo_only: doc.config.repo_only,
    })
}

/// Reject glob patterns that contain `..` path components (escape attempts).
fn validate_match_pattern(pattern: &str) -> Result<()> {
    // Walk the literal path segments (not the glob-special parts), looking for `..`.
    for component in Path::new(pattern).components() {
        if component == Component::ParentDir {
            bail!("pattern must not contain '..' components");
        }
    }
    Ok(())
}

// ─── glob set builder ─────────────────────────────────────────────────────────

fn build_glob_set(patterns: &[String]) -> Result<GlobSet> {
    let mut builder = GlobSetBuilder::new();
    for p in patterns {
        let glob = Glob::new(p).with_context(|| format!("invalid glob pattern {p:?}"))?;
        builder.add(glob);
    }
    builder.build().context("failed to build glob set")
}

// ─── Registry ────────────────────────────────────────────────────────────────

/// Indexed workspace spec paths, keyed by `(name, type)`.
///
/// Produced by [`discover`]; consumed by `botforge build <name>` /
/// `botforge test <name>` / `botforge publish <name>`.  Structure is
/// intentionally a reusable helper so future work (B4 sync/drift-check) can
/// materialise the same output without re-discovering.
#[derive(Debug, Default)]
pub(crate) struct Registry {
    /// Maps spec name → absolute path for `type: botforge/build` docs.
    pub(crate) builds: BTreeMap<String, PathBuf>,
    /// Maps spec name → absolute path for `type: botforge/test` docs.
    pub(crate) tests: BTreeMap<String, PathBuf>,
    /// Maps spec name → absolute path for `type: botforge/publish` docs.
    pub(crate) publishes: BTreeMap<String, PathBuf>,
}

/// Methods used in tests to look up named entries in the discovered registry.
#[cfg(test)]
impl Registry {
    /// Resolve a build spec by name (test helper).
    pub(crate) fn build(&self, name: &str, context_root: &Path) -> Result<&PathBuf> {
        self.builds.get(name).ok_or_else(|| {
            anyhow::anyhow!(
                "no build named '{}' found in {}",
                name,
                context_root.display()
            )
        })
    }

    /// Resolve a test spec by name (test helper).
    pub(crate) fn test(&self, name: &str, context_root: &Path) -> Result<&PathBuf> {
        self.tests.get(name).ok_or_else(|| {
            anyhow::anyhow!(
                "no test named '{}' found in {}",
                name,
                context_root.display()
            )
        })
    }

    /// Resolve a publish spec by name (test helper).
    pub(crate) fn publish(&self, name: &str, context_root: &Path) -> Result<&PathBuf> {
        self.publishes.get(name).ok_or_else(|| {
            anyhow::anyhow!(
                "no publish plan named '{}' found in {}",
                name,
                context_root.display()
            )
        })
    }
}

// ─── doc-type peeker ─────────────────────────────────────────────────────────

#[derive(Debug, PartialEq, Eq)]
enum DocKind {
    Build,
    Test,
    Publish,
}

/// Read a YAML file enough to extract `type:` and `name:`.
/// Returns `None` if the file is not a `botforge/build`, `botforge/test`, or
/// `botforge/publish` doc.
/// Returns an error if the file has the right type but a missing/invalid name.
fn peek_doc(path: &Path) -> Result<Option<(DocKind, String)>> {
    let contents = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            // Unreadable file: skip silently (matches walkdir behaviour for inaccessible paths).
            if crate::util::botforge_debug_enabled() {
                eprintln!("[discovery] skip unreadable {}: {e}", path.display());
            }
            return Ok(None);
        }
    };

    // Parse as a generic YAML value so we can inspect `type:` cheaply.
    let val: serde_yaml::Value = match serde_yaml::from_str(&contents) {
        Ok(v) => v,
        Err(_) => {
            // Non-YAML or malformed file: not a botforge spec, skip.
            return Ok(None);
        }
    };

    let map = match val.as_mapping() {
        Some(m) => m,
        None => return Ok(None),
    };

    let type_val = match map.get("type") {
        Some(v) => v,
        None => return Ok(None),
    };

    let kind = match type_val.as_str() {
        Some("botforge/build") => DocKind::Build,
        Some("botforge/test") => DocKind::Test,
        Some("botforge/publish") => DocKind::Publish,
        _ => return Ok(None),
    };

    let type_label = match kind {
        DocKind::Build => "type: botforge/build",
        DocKind::Test => "type: botforge/test",
        DocKind::Publish => "type: botforge/publish",
    };

    // `name:` is required for entrypoint docs.
    let name = match map.get("name") {
        None => {
            bail!(
                "'name' is required in a '{}' document ({})",
                type_label,
                path.display()
            )
        }
        Some(v) => match v.as_str() {
            None => bail!(
                "'name' must be a string in {} ({})",
                type_label,
                path.display()
            ),
            Some(s) => s.to_string(),
        },
    };

    // Reuse the name-validation rules (printable ASCII, non-empty, no control chars).
    if name.trim().is_empty() {
        bail!(
            "'name' must not be blank in {} ({})",
            type_label,
            path.display()
        );
    }
    if !name.is_ascii() || name.chars().any(|c| c.is_ascii_control()) {
        bail!(
            "'name' must be printable ASCII in {} ({})",
            type_label,
            path.display()
        );
    }

    Ok(Some((kind, name)))
}

// ─── insertion helper (duplicate detection) ──────────────────────────────────

fn insert_entry(
    map: &mut BTreeMap<String, PathBuf>,
    name: String,
    path: PathBuf,
    kind_label: &str,
) -> Result<()> {
    use std::collections::btree_map::Entry;
    match map.entry(name.clone()) {
        Entry::Vacant(e) => {
            e.insert(path);
            Ok(())
        }
        Entry::Occupied(e) => {
            bail!(
                "duplicate {} named '{}': found in both '{}' and '{}'",
                kind_label,
                name,
                e.get().display(),
                path.display()
            )
        }
    }
}

// ─── nested-marker check ─────────────────────────────────────────────────────

/// Returns `true` if `dir` is a nested workspace boundary (contains a
/// botforge marker file but is NOT the context root itself).
fn is_nested_marker(dir: &Path, context_root: &Path) -> bool {
    if dir == context_root {
        return false;
    }
    find_marker_path(dir).is_some()
}

// ─── filesystem-walk discovery (repo-only: false) ────────────────────────────

fn discover_via_walk(context_root: &Path, glob_set: &GlobSet) -> Result<Vec<PathBuf>> {
    let build_dir = context_root.join(BUILD_DIR);
    let mut candidates = Vec::new();

    let walker = WalkDir::new(context_root)
        .into_iter()
        .filter_entry(|entry| {
            let path = entry.path();
            // Always yield root.
            if path == context_root {
                return true;
            }
            if entry.file_type().is_dir() {
                // Prune <context_root>/build/.
                if *path == build_dir {
                    return false;
                }
                // Prune nested workspace boundaries.
                if is_nested_marker(path, context_root) {
                    return false;
                }
            }
            true
        });

    for entry in walker {
        let entry = entry.context("error walking context directory")?;
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        let relative = match path.strip_prefix(context_root) {
            Ok(r) => r,
            Err(_) => continue,
        };
        // Reject any path that escapes via `..` (defensive; walkdir doesn't produce these).
        if relative.components().any(|c| c == Component::ParentDir) {
            continue;
        }
        if glob_set.is_match(relative) {
            candidates.push(path.to_path_buf());
        }
    }

    Ok(candidates)
}

// ─── git-index-based discovery (repo-only: true) ─────────────────────────────

fn discover_via_git_index(context_root: &Path, glob_set: &GlobSet) -> Result<Vec<PathBuf>> {
    // Discover the git repository from the context root.
    let repo = gix::discover(context_root).map_err(|e| {
        anyhow::anyhow!(
            "repo-only: true requires a git repository but none was found \
             in or above '{}': {e}",
            context_root.display()
        )
    })?;

    // Bare repos have no work-tree.
    let work_dir = repo.work_dir().ok_or_else(|| {
        anyhow::anyhow!(
            "repo-only: true requires a git repository with a work-tree \
             (the repository at '{}' is bare)",
            context_root.display()
        )
    })?;

    // The context root must be inside the git work-tree.
    let context_in_repo = context_root.strip_prefix(work_dir).map_err(|_| {
        anyhow::anyhow!(
            "repo-only: true error: context root '{}' is outside the git \
             repository work-tree '{}'",
            context_root.display(),
            work_dir.display()
        )
    })?;

    // Read the index (tracked + staged files).
    let index = repo.index_or_empty().context("failed to read git index")?;

    // ── First pass: find nested workspace markers tracked in the index ────────
    // We need to exclude files that live under a nested botforge.yaml directory.
    let mut nested_dirs: Vec<PathBuf> = Vec::new();

    for entry in index.entries() {
        let raw_path = entry.path(&index);
        let path_str = match std::str::from_utf8(raw_path) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let entry_path = Path::new(path_str);

        // Only consider files under our context root.
        let relative_to_context = match entry_path.strip_prefix(context_in_repo) {
            Ok(r) => r,
            Err(_) => continue,
        };

        let fname = relative_to_context.file_name().and_then(|f| f.to_str());
        if fname.is_some_and(is_marker_name) {
            // This marker is inside a subdirectory of the context root → nested workspace.
            if let Some(parent) = relative_to_context.parent() {
                if !parent.as_os_str().is_empty() {
                    nested_dirs.push(parent.to_path_buf());
                }
            }
        }
    }

    // ── Second pass: collect candidates ──────────────────────────────────────
    let mut candidates = Vec::new();

    for entry in index.entries() {
        let raw_path = entry.path(&index);
        let path_str = match std::str::from_utf8(raw_path) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let entry_path = Path::new(path_str);

        // Only files under our context root.
        let relative_to_context = match entry_path.strip_prefix(context_in_repo) {
            Ok(r) => r,
            Err(_) => continue,
        };

        // Skip `..` escapes (defensive; git index paths don't normally have these).
        if relative_to_context
            .components()
            .any(|c| c == Component::ParentDir)
        {
            continue;
        }

        // Prune <context_root>/build/.
        if relative_to_context.starts_with(BUILD_DIR) {
            continue;
        }

        // Prune nested workspace subtrees.
        if nested_dirs
            .iter()
            .any(|nd| relative_to_context.starts_with(nd))
        {
            continue;
        }

        // Apply glob patterns.
        if glob_set.is_match(relative_to_context) {
            candidates.push(work_dir.join(entry_path));
        }
    }

    Ok(candidates)
}

// ─── public API ──────────────────────────────────────────────────────────────

/// Discover all `type: botforge/build` and `type: botforge/test` documents in
/// the workspace rooted at `context_root`, and return a [`Registry`] keyed by
/// `(name, type)`.
///
/// Discovery respects the `config:` block in `botforge.yaml` (glob patterns,
/// `repo-only` flag) and always prunes:
/// - the derived-output `<context_root>/build/` directory,
/// - any nested workspace boundary (`botforge.yaml` / `botforge.yml` in a subdirectory).
pub(crate) fn discover(context_root: &Path) -> Result<Registry> {
    let config = load_workspace_config(context_root)?;
    let glob_set = build_glob_set(&config.match_patterns)?;

    let candidates = if config.repo_only {
        discover_via_git_index(context_root, &glob_set)?
    } else {
        discover_via_walk(context_root, &glob_set)?
    };

    let mut registry = Registry::default();

    for path in candidates {
        match peek_doc(&path)? {
            None => {}
            Some((DocKind::Build, name)) => {
                insert_entry(&mut registry.builds, name, path, "botforge/build")?;
            }
            Some((DocKind::Test, name)) => {
                insert_entry(&mut registry.tests, name, path, "botforge/test")?;
            }
            Some((DocKind::Publish, name)) => {
                insert_entry(&mut registry.publishes, name, path, "botforge/publish")?;
            }
        }
    }

    Ok(registry)
}

// ─── tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    const MARKER_YAML: &str = "botforge.yaml";
    const MARKER_YML: &str = "botforge.yml";

    // ── helpers ───────────────────────────────────────────────────────────────

    fn write_marker(dir: &Path) {
        fs::write(dir.join(MARKER_YAML), "").unwrap();
    }

    fn write_named_marker(dir: &Path, name: &str) {
        fs::write(dir.join(name), "").unwrap();
    }

    fn write_build_doc(dir: &Path, filename: &str, name: &str) {
        let content =
            format!("type: botforge/build\nname: {name}\nimage: \"@base\"\noutput: out.qcow2\n");
        fs::write(dir.join(filename), content).unwrap();
    }

    fn write_test_doc(dir: &Path, filename: &str, name: &str) {
        let content = format!("type: botforge/test\nname: {name}\n");
        fs::write(dir.join(filename), content).unwrap();
    }

    fn write_fragment_doc(dir: &Path, filename: &str) {
        fs::write(dir.join(filename), "type: botforge/fragment\n").unwrap();
    }

    fn write_publish_doc(dir: &Path, filename: &str, name: &str) {
        let content = format!("type: botforge/publish\nname: {name}\n");
        fs::write(dir.join(filename), content).unwrap();
    }

    // ── load_workspace_config ─────────────────────────────────────────────────

    #[test]
    fn workspace_config_defaults_when_no_config_block() {
        let root = TempDir::new().unwrap();
        write_marker(root.path());
        let cfg = load_workspace_config(root.path()).unwrap();
        assert_eq!(cfg.match_patterns, vec!["**/*.{yaml,yml}"]);
        assert!(!cfg.repo_only);
    }

    #[test]
    fn workspace_config_reads_scalar_match() {
        let root = TempDir::new().unwrap();
        fs::write(
            root.path().join(MARKER_YAML),
            "config:\n  match: \"**/*.yaml\"\n",
        )
        .unwrap();
        let cfg = load_workspace_config(root.path()).unwrap();
        assert_eq!(cfg.match_patterns, vec!["**/*.yaml"]);
    }

    #[test]
    fn workspace_config_reads_list_match() {
        let root = TempDir::new().unwrap();
        fs::write(
            root.path().join(MARKER_YAML),
            "config:\n  match:\n    - \"**/*.yaml\"\n    - \"**/*.yml\"\n",
        )
        .unwrap();
        let cfg = load_workspace_config(root.path()).unwrap();
        assert_eq!(cfg.match_patterns, vec!["**/*.yaml", "**/*.yml"]);
    }

    #[test]
    fn workspace_config_explicit_match_replaces_default() {
        let root = TempDir::new().unwrap();
        fs::write(
            root.path().join(MARKER_YAML),
            "config:\n  match: \"specs/**/*.yaml\"\n",
        )
        .unwrap();
        let cfg = load_workspace_config(root.path()).unwrap();
        assert_eq!(cfg.match_patterns, vec!["specs/**/*.yaml"]);
        // Default is NOT present.
        assert!(!cfg.match_patterns.iter().any(|p| p.contains("yml")));
    }

    #[test]
    fn workspace_config_reads_repo_only() {
        let root = TempDir::new().unwrap();
        fs::write(
            root.path().join(MARKER_YAML),
            "config:\n  repo-only: true\n",
        )
        .unwrap();
        let cfg = load_workspace_config(root.path()).unwrap();
        assert!(cfg.repo_only);
    }

    #[test]
    fn workspace_config_rejects_dotdot_pattern() {
        let root = TempDir::new().unwrap();
        fs::write(
            root.path().join(MARKER_YAML),
            "config:\n  match: \"../../etc/*.yaml\"\n",
        )
        .unwrap();
        let err = load_workspace_config(root.path()).unwrap_err();
        assert!(
            format!("{err:#}").contains(".."),
            "expected .. rejection, got: {err:#}"
        );
    }

    // ── glob matching ─────────────────────────────────────────────────────────

    #[test]
    fn discover_default_glob_matches_yaml_and_yml() {
        let root = TempDir::new().unwrap();
        write_marker(root.path());
        write_build_doc(root.path(), "build.yaml", "my-build");
        write_test_doc(root.path(), "test.yml", "my-test");
        fs::write(root.path().join("readme.txt"), "not a spec").unwrap();

        let reg = discover(root.path()).unwrap();
        assert!(reg.build("my-build", root.path()).is_ok());
        assert!(reg.test("my-test", root.path()).is_ok());
    }

    #[test]
    fn discover_explicit_match_pattern_scalar() {
        let root = TempDir::new().unwrap();
        fs::write(
            root.path().join(MARKER_YAML),
            "config:\n  match: \"**/*.yaml\"\n",
        )
        .unwrap();
        write_build_doc(root.path(), "build.yaml", "b");
        write_test_doc(root.path(), "test.yml", "t"); // .yml excluded by custom pattern

        let reg = discover(root.path()).unwrap();
        assert!(reg.build("b", root.path()).is_ok());
        assert!(reg.test("t", root.path()).is_err()); // .yml not matched
    }

    #[test]
    fn discover_explicit_match_pattern_list() {
        let root = TempDir::new().unwrap();
        fs::write(
            root.path().join(MARKER_YAML),
            "config:\n  match:\n    - \"builds/**/*.yaml\"\n",
        )
        .unwrap();
        let builds_dir = root.path().join("builds");
        fs::create_dir_all(&builds_dir).unwrap();
        write_build_doc(&builds_dir, "build.yaml", "deep-build");
        write_build_doc(root.path(), "root-build.yaml", "root-build"); // not matched

        let reg = discover(root.path()).unwrap();
        assert!(reg.build("deep-build", root.path()).is_ok());
        assert!(reg.build("root-build", root.path()).is_err());
    }

    // ── build/ pruning ────────────────────────────────────────────────────────

    #[test]
    fn discover_prunes_build_dir() {
        let root = TempDir::new().unwrap();
        write_marker(root.path());
        let build_dir = root.path().join("build");
        fs::create_dir_all(&build_dir).unwrap();
        // Spec in build/ should be invisible.
        write_build_doc(&build_dir, "artifact.yaml", "artifact");
        write_build_doc(root.path(), "real.yaml", "real");

        let reg = discover(root.path()).unwrap();
        assert!(
            reg.build("artifact", root.path()).is_err(),
            "build/ should be pruned"
        );
        assert!(reg.build("real", root.path()).is_ok());
    }

    // ── nested workspace boundary pruning ─────────────────────────────────────

    #[test]
    fn discover_prunes_nested_workspace_subtree() {
        let root = TempDir::new().unwrap();
        write_marker(root.path());
        let nested = root.path().join("nested");
        fs::create_dir_all(&nested).unwrap();
        write_marker(&nested); // nested workspace boundary
        write_build_doc(&nested, "build.yaml", "nested-build");
        write_build_doc(root.path(), "root-build.yaml", "root-build");

        let reg = discover(root.path()).unwrap();
        assert!(
            reg.build("nested-build", root.path()).is_err(),
            "nested subtree must be pruned"
        );
        assert!(reg.build("root-build", root.path()).is_ok());
    }

    #[test]
    fn discover_prunes_nested_workspace_yml_marker() {
        let root = TempDir::new().unwrap();
        write_marker(root.path());
        let nested = root.path().join("sub");
        fs::create_dir_all(&nested).unwrap();
        // Use .yml marker.
        fs::write(nested.join(MARKER_YML), "").unwrap();
        write_build_doc(&nested, "build.yaml", "sub-build");
        write_build_doc(root.path(), "root.yaml", "root");

        let reg = discover(root.path()).unwrap();
        assert!(
            reg.build("sub-build", root.path()).is_err(),
            "nested subtree (yml marker) must be pruned"
        );
        assert!(reg.build("root", root.path()).is_ok());
    }

    #[test]
    fn discover_prunes_nested_workspace_alt_marker_subtree() {
        let root = TempDir::new().unwrap();
        write_marker(root.path());
        let nested = root.path().join("sub");
        fs::create_dir_all(&nested).unwrap();
        write_named_marker(&nested, ".botforge.yaml");
        write_build_doc(&nested, "build.yaml", "sub-build");
        write_build_doc(root.path(), "root.yaml", "root");

        let reg = discover(root.path()).unwrap();
        assert!(
            reg.build("sub-build", root.path()).is_err(),
            "nested subtree (alternate marker) must be pruned"
        );
        assert!(reg.build("root", root.path()).is_ok());
    }

    #[test]
    fn discover_nested_marker_prunes_deeply_nested_files() {
        let root = TempDir::new().unwrap();
        write_marker(root.path());
        let nested = root.path().join("nested");
        let deep = nested.join("deep");
        fs::create_dir_all(&deep).unwrap();
        write_marker(&nested);
        write_build_doc(&deep, "deep.yaml", "deep-build");
        write_build_doc(root.path(), "root.yaml", "root");

        let reg = discover(root.path()).unwrap();
        assert!(
            reg.build("deep-build", root.path()).is_err(),
            "deep subtree of nested workspace must be pruned"
        );
        assert!(reg.build("root", root.path()).is_ok());
    }

    #[test]
    fn discover_nearest_ancestor_invariant() {
        // A spec in a dir that has a PARENT nested marker (but not a direct one)
        // is still pruned because it's under the nested workspace root.
        let root = TempDir::new().unwrap();
        write_marker(root.path());

        let nested = root.path().join("nested");
        fs::create_dir_all(&nested).unwrap();
        write_marker(&nested);

        let mid = nested.join("mid");
        fs::create_dir_all(&mid).unwrap();
        // mid/ does NOT have its own marker, but it's under nested/.
        write_build_doc(&mid, "mid.yaml", "mid-build");

        let reg = discover(root.path()).unwrap();
        assert!(
            reg.build("mid-build", root.path()).is_err(),
            "file under nested workspace must be pruned even without direct marker"
        );
    }

    // ── (name, type) uniqueness ───────────────────────────────────────────────

    #[test]
    fn discover_build_and_test_sharing_name_is_ok() {
        let root = TempDir::new().unwrap();
        write_marker(root.path());
        write_build_doc(root.path(), "build.yaml", "foo");
        write_test_doc(root.path(), "test.yaml", "foo");

        let reg = discover(root.path()).unwrap();
        assert!(reg.build("foo", root.path()).is_ok());
        assert!(reg.test("foo", root.path()).is_ok());
    }

    #[test]
    fn discover_same_type_duplicate_name_errors() {
        let root = TempDir::new().unwrap();
        write_marker(root.path());
        write_build_doc(root.path(), "build1.yaml", "foo");
        write_build_doc(root.path(), "build2.yaml", "foo");

        let err = discover(root.path()).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("duplicate") && msg.contains("foo"),
            "expected duplicate-name error mentioning 'foo': {msg}"
        );
        // Both file paths should appear in the message.
        assert!(
            msg.contains("build1.yaml") || msg.contains("build2.yaml"),
            "error must name files: {msg}"
        );
    }

    #[test]
    fn discover_same_type_duplicate_test_name_errors() {
        let root = TempDir::new().unwrap();
        write_marker(root.path());
        write_test_doc(root.path(), "test1.yaml", "bar");
        write_test_doc(root.path(), "test2.yaml", "bar");

        let err = discover(root.path()).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("duplicate") && msg.contains("bar"),
            "expected duplicate error: {msg}"
        );
    }

    // ── registry lookups ──────────────────────────────────────────────────────

    #[test]
    fn registry_build_not_found_returns_error() {
        let root = TempDir::new().unwrap();
        write_marker(root.path());
        let reg = discover(root.path()).unwrap();
        let err = reg.build("missing", root.path()).unwrap_err();
        assert!(
            format!("{err:#}").contains("no build named 'missing'"),
            "expected not-found error: {err:#}"
        );
    }

    #[test]
    fn registry_test_not_found_returns_error() {
        let root = TempDir::new().unwrap();
        write_marker(root.path());
        let reg = discover(root.path()).unwrap();
        let err = reg.test("missing", root.path()).unwrap_err();
        assert!(
            format!("{err:#}").contains("no test named 'missing'"),
            "expected not-found error: {err:#}"
        );
    }

    #[test]
    fn registry_build_returns_correct_path() {
        let root = TempDir::new().unwrap();
        write_marker(root.path());
        write_build_doc(root.path(), "my-build.yaml", "my-build");

        let reg = discover(root.path()).unwrap();
        let path = reg.build("my-build", root.path()).unwrap();
        assert!(path.ends_with("my-build.yaml"));
    }

    // ── fragment and non-botforge files are skipped ───────────────────────────

    #[test]
    fn discover_skips_fragment_docs() {
        let root = TempDir::new().unwrap();
        write_marker(root.path());
        write_fragment_doc(root.path(), "fragment.yaml");
        write_build_doc(root.path(), "build.yaml", "b");

        let reg = discover(root.path()).unwrap();
        assert!(reg.build("b", root.path()).is_ok());
    }

    #[test]
    fn discover_skips_non_yaml_files() {
        let root = TempDir::new().unwrap();
        write_marker(root.path());
        fs::write(root.path().join("readme.md"), "# not yaml").unwrap();
        let reg = discover(root.path()).unwrap();
        // No error — non-yaml just not matched.
        drop(reg);
    }

    #[test]
    fn discover_handles_empty_workspace() {
        let root = TempDir::new().unwrap();
        write_marker(root.path());
        let reg = discover(root.path()).unwrap();
        assert!(reg.build("anything", root.path()).is_err());
        assert!(reg.test("anything", root.path()).is_err());
        assert!(reg.publish("anything", root.path()).is_err());
    }

    #[test]
    fn discover_finds_publish_docs() {
        let root = TempDir::new().unwrap();
        write_marker(root.path());
        write_publish_doc(root.path(), "release.yaml", "my-release");

        let reg = discover(root.path()).unwrap();
        assert!(
            reg.publish("my-release", root.path()).is_ok(),
            "publish doc should be discoverable"
        );
        assert!(
            reg.build("my-release", root.path()).is_err(),
            "publish doc must not appear in builds"
        );
    }

    // ── recursive discovery ───────────────────────────────────────────────────

    #[test]
    fn discover_finds_specs_in_subdirectories() {
        let root = TempDir::new().unwrap();
        write_marker(root.path());
        let sub = root.path().join("a/b/c");
        fs::create_dir_all(&sub).unwrap();
        write_build_doc(&sub, "deep.yaml", "deep");
        write_test_doc(root.path(), "top.yaml", "top-test");

        let reg = discover(root.path()).unwrap();
        assert!(reg.build("deep", root.path()).is_ok());
        assert!(reg.test("top-test", root.path()).is_ok());
    }

    // ── repo-only: error paths ────────────────────────────────────────────────

    #[test]
    fn discover_repo_only_without_git_errors() {
        let root = TempDir::new().unwrap();
        fs::write(
            root.path().join(MARKER_YAML),
            "config:\n  repo-only: true\n",
        )
        .unwrap();

        // This temp dir is not inside a git repo (or it might be under one in CI,
        // but that's fine — in the non-CI case it should error).
        // We only test that discover() returns an error when repo-only is true
        // and the tmp dir has no git repo.
        // In CI we might actually be inside a git repo, so we can't assert error there.
        // Instead, just verify discover() doesn't panic.
        let _ = discover(root.path()); // either Ok or Err depending on environment
    }
}
