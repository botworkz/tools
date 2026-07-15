//! Committed registry I/O — B4.
//!
//! Reads and writes the `build:` / `test:` registry blocks in `botforge.yaml`.
//! Each block is a map from spec **name** to a single-key map `{ spec: <relative-path> }`.
//!
//! Example:
//! ```yaml
//! build:
//!   foo:
//!     spec: specs/foo.yaml
//! test:
//!   foo:
//!     spec: specs/foo-test.yaml
//! ```
//!
//! Paths are stored relative to the context root and are validated to ensure they
//! do not escape via `..` components.  On load, relative paths are resolved to
//! absolute paths anchored at the context root.

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

use super::marker_path;

// ─── raw deserialization types ────────────────────────────────────────────────

/// A single registry entry as stored in `botforge.yaml`.
#[derive(Debug, Deserialize, Clone)]
pub(crate) struct RawRegistryEntry {
    pub(crate) spec: String,
}

/// The `botforge.yaml` root document — only the registry blocks are consumed here.
#[derive(Debug, Deserialize, Default)]
struct RawWorkspaceRegistryDoc {
    #[serde(default)]
    pub(crate) build: BTreeMap<String, RawRegistryEntry>,
    #[serde(default)]
    pub(crate) test: BTreeMap<String, RawRegistryEntry>,
}

// ─── CommittedRegistry ───────────────────────────────────────────────────────

/// The in-memory representation of the committed `build:` / `test:` registry.
///
/// Paths are **absolute** (resolved relative to the context root at load time).
#[derive(Debug, Default)]
pub(crate) struct CommittedRegistry {
    /// Maps spec name → absolute path for `type: botforge/build` entries.
    pub(crate) builds: BTreeMap<String, PathBuf>,
    /// Maps spec name → absolute path for `type: botforge/test` entries.
    pub(crate) tests: BTreeMap<String, PathBuf>,
}

impl CommittedRegistry {
    /// Resolve a build spec by name.
    ///
    /// Returns the absolute path of the spec file, or an error naming the registry
    /// and hinting at `botforge config sync`.
    pub(crate) fn build(&self, name: &str) -> Result<&PathBuf> {
        self.builds.get(name).ok_or_else(|| {
            anyhow::anyhow!(
                "no build named '{}' in registry (run 'botforge config sync')",
                name
            )
        })
    }

    /// Resolve a test spec by name.
    ///
    /// Returns the absolute path of the spec file, or an error naming the registry
    /// and hinting at `botforge config sync`.
    pub(crate) fn test(&self, name: &str) -> Result<&PathBuf> {
        self.tests.get(name).ok_or_else(|| {
            anyhow::anyhow!(
                "no test named '{}' in registry (run 'botforge config sync')",
                name
            )
        })
    }
}

// ─── path validation ──────────────────────────────────────────────────────────

/// Validate that a registry `spec:` path is safe: must be relative, must not
/// contain `..` components, and must not be absolute.
pub(crate) fn validate_spec_path(spec: &str) -> Result<()> {
    let p = Path::new(spec);
    if p.is_absolute() {
        bail!("spec path must be relative, not absolute: {spec:?}");
    }
    for component in p.components() {
        if component == Component::ParentDir {
            bail!("spec path must not contain '..' components: {spec:?}");
        }
    }
    if spec.trim().is_empty() {
        bail!("spec path must not be empty");
    }
    Ok(())
}

// ─── load ─────────────────────────────────────────────────────────────────────

/// Load the committed registry from `<context_root>/botforge.yaml`.
///
/// Returns a `CommittedRegistry` with absolute spec paths.  If `botforge.yaml`
/// has no `build:` or `test:` blocks, the respective maps are empty (not an error).
pub(crate) fn load_committed_registry(context_root: &Path) -> Result<CommittedRegistry> {
    let marker_path = marker_path(context_root)?;
    let contents = std::fs::read_to_string(&marker_path)
        .with_context(|| format!("cannot read {}", marker_path.display()))?;

    if contents.trim().is_empty() {
        return Ok(CommittedRegistry::default());
    }

    let doc: RawWorkspaceRegistryDoc = serde_yaml::from_str(&contents)
        .with_context(|| format!("invalid YAML in {}", marker_path.display()))?;

    let mut reg = CommittedRegistry::default();

    for (name, entry) in doc.build {
        validate_spec_path(&entry.spec).with_context(|| {
            format!(
                "invalid spec path for build entry '{name}' in {}",
                marker_path.display()
            )
        })?;
        let abs = context_root.join(&entry.spec);
        reg.builds.insert(name, abs);
    }

    for (name, entry) in doc.test {
        validate_spec_path(&entry.spec).with_context(|| {
            format!(
                "invalid spec path for test entry '{name}' in {}",
                marker_path.display()
            )
        })?;
        let abs = context_root.join(&entry.spec);
        reg.tests.insert(name, abs);
    }

    Ok(reg)
}

// ─── save ─────────────────────────────────────────────────────────────────────

/// Rewrite the `build:` and `test:` registry blocks in `<context_root>/botforge.yaml`,
/// preserving all other keys (e.g. `config:`, `assets:`) unchanged.
///
/// `builds` and `tests` map spec names → **absolute** paths.  Paths are stored
/// as relative-to-context-root in the YAML.
pub(crate) fn save_registry(
    context_root: &Path,
    builds: &BTreeMap<String, PathBuf>,
    tests: &BTreeMap<String, PathBuf>,
) -> Result<()> {
    let marker_path = marker_path(context_root)?;

    // Read the existing file so we can merge (preserving other keys).
    let existing = std::fs::read_to_string(&marker_path)
        .with_context(|| format!("cannot read {}", marker_path.display()))?;

    let mut doc: serde_yaml::Value = if existing.trim().is_empty() {
        serde_yaml::Value::Mapping(serde_yaml::Mapping::new())
    } else {
        serde_yaml::from_str(&existing)
            .with_context(|| format!("invalid YAML in {}", marker_path.display()))?
    };

    let map = doc
        .as_mapping_mut()
        .ok_or_else(|| anyhow::anyhow!("{} is not a YAML mapping", marker_path.display()))?;

    // Build the new `build:` block.
    let build_block = registry_map_to_yaml(context_root, builds)?;
    // Build the new `test:` block.
    let test_block = registry_map_to_yaml(context_root, tests)?;

    // Insert or overwrite — we always write both keys so the file is complete.
    map.insert(serde_yaml::Value::String("build".to_string()), build_block);
    map.insert(serde_yaml::Value::String("test".to_string()), test_block);

    let yaml_str =
        serde_yaml::to_string(&doc).context("failed to serialize updated botforge.yaml")?;

    std::fs::write(&marker_path, yaml_str)
        .with_context(|| format!("cannot write {}", marker_path.display()))?;

    Ok(())
}

/// Convert a `BTreeMap<String, PathBuf>` (absolute paths) into a YAML value
/// of the form `{ <name>: { spec: <relative-path> } }`.
fn registry_map_to_yaml(
    context_root: &Path,
    map: &BTreeMap<String, PathBuf>,
) -> Result<serde_yaml::Value> {
    let mut out = serde_yaml::Mapping::new();
    for (name, abs_path) in map {
        let rel = abs_path.strip_prefix(context_root).with_context(|| {
            format!(
                "spec path '{}' is outside context root '{}'",
                abs_path.display(),
                context_root.display()
            )
        })?;
        // Convert to forward-slash string (portable YAML).
        let rel_str = rel
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("spec path is not valid UTF-8: {}", abs_path.display()))?
            .to_string();

        let mut entry = serde_yaml::Mapping::new();
        entry.insert(
            serde_yaml::Value::String("spec".to_string()),
            serde_yaml::Value::String(rel_str),
        );
        out.insert(
            serde_yaml::Value::String(name.clone()),
            serde_yaml::Value::Mapping(entry),
        );
    }
    Ok(serde_yaml::Value::Mapping(out))
}

// ─── tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    const MARKER_YAML: &str = "botforge.yaml";

    fn write_marker(dir: &Path, content: &str) {
        fs::write(dir.join(MARKER_YAML), content).unwrap();
    }

    fn write_named_marker(dir: &Path, name: &str, content: &str) {
        fs::write(dir.join(name), content).unwrap();
    }

    // ── validate_spec_path ────────────────────────────────────────────────────

    #[test]
    fn validate_spec_path_accepts_simple_relative() {
        assert!(validate_spec_path("specs/foo.yaml").is_ok());
    }

    #[test]
    fn validate_spec_path_accepts_nested_relative() {
        assert!(validate_spec_path("a/b/c/foo.yaml").is_ok());
    }

    #[test]
    fn validate_spec_path_rejects_absolute() {
        let err = validate_spec_path("/etc/foo.yaml").unwrap_err();
        assert!(format!("{err:#}").contains("absolute"), "{err:#}");
    }

    #[test]
    fn validate_spec_path_rejects_dotdot() {
        let err = validate_spec_path("../escape.yaml").unwrap_err();
        assert!(format!("{err:#}").contains(".."), "{err:#}");
    }

    #[test]
    fn validate_spec_path_rejects_embedded_dotdot() {
        let err = validate_spec_path("specs/../../etc/foo.yaml").unwrap_err();
        assert!(format!("{err:#}").contains(".."), "{err:#}");
    }

    #[test]
    fn validate_spec_path_rejects_empty() {
        let err = validate_spec_path("").unwrap_err();
        assert!(format!("{err:#}").contains("empty"), "{err:#}");
    }

    // ── load_committed_registry ───────────────────────────────────────────────

    #[test]
    fn load_empty_marker_returns_empty_registry() {
        let root = TempDir::new().unwrap();
        write_marker(root.path(), "");
        let reg = load_committed_registry(root.path()).unwrap();
        assert!(reg.builds.is_empty());
        assert!(reg.tests.is_empty());
    }

    #[test]
    fn load_registry_with_build_and_test() {
        let root = TempDir::new().unwrap();
        write_marker(
            root.path(),
            "build:\n  foo:\n    spec: specs/foo.yaml\ntest:\n  bar:\n    spec: specs/bar-test.yaml\n",
        );
        let reg = load_committed_registry(root.path()).unwrap();
        assert_eq!(reg.builds.len(), 1);
        assert_eq!(reg.tests.len(), 1);
        assert_eq!(reg.builds["foo"], root.path().join("specs/foo.yaml"));
        assert_eq!(reg.tests["bar"], root.path().join("specs/bar-test.yaml"));
    }

    #[test]
    fn load_registry_from_alternate_marker_name() {
        let root = TempDir::new().unwrap();
        write_named_marker(
            root.path(),
            ".botforge.yml",
            "build:\n  foo:\n    spec: specs/foo.yaml\n",
        );
        let reg = load_committed_registry(root.path()).unwrap();
        assert_eq!(reg.builds["foo"], root.path().join("specs/foo.yaml"));
    }

    #[test]
    fn load_registry_rejects_dotdot_spec_path() {
        let root = TempDir::new().unwrap();
        write_marker(root.path(), "build:\n  evil:\n    spec: ../../etc/passwd\n");
        let err = load_committed_registry(root.path()).unwrap_err();
        assert!(format!("{err:#}").contains(".."), "{err:#}");
    }

    #[test]
    fn load_registry_build_and_test_may_share_name() {
        let root = TempDir::new().unwrap();
        write_marker(
            root.path(),
            "build:\n  foo:\n    spec: specs/foo.yaml\ntest:\n  foo:\n    spec: specs/foo-test.yaml\n",
        );
        let reg = load_committed_registry(root.path()).unwrap();
        assert!(reg.build("foo").is_ok());
        assert!(reg.test("foo").is_ok());
    }

    #[test]
    fn committed_registry_build_not_found_error() {
        let root = TempDir::new().unwrap();
        write_marker(root.path(), "");
        let reg = load_committed_registry(root.path()).unwrap();
        let err = reg.build("missing").unwrap_err();
        assert!(
            format!("{err:#}").contains("no build named 'missing'"),
            "{err:#}"
        );
        assert!(
            format!("{err:#}").contains("botforge config sync"),
            "{err:#}"
        );
    }

    #[test]
    fn committed_registry_test_not_found_error() {
        let root = TempDir::new().unwrap();
        write_marker(root.path(), "");
        let reg = load_committed_registry(root.path()).unwrap();
        let err = reg.test("missing").unwrap_err();
        assert!(
            format!("{err:#}").contains("no test named 'missing'"),
            "{err:#}"
        );
        assert!(
            format!("{err:#}").contains("botforge config sync"),
            "{err:#}"
        );
    }

    // ── save_registry ─────────────────────────────────────────────────────────

    #[test]
    fn save_registry_round_trips() {
        let root = TempDir::new().unwrap();
        write_marker(root.path(), "");

        let mut builds = BTreeMap::new();
        builds.insert("foo".to_string(), root.path().join("specs/foo.yaml"));
        let mut tests = BTreeMap::new();
        tests.insert("bar".to_string(), root.path().join("specs/bar-test.yaml"));

        save_registry(root.path(), &builds, &tests).unwrap();

        let reg = load_committed_registry(root.path()).unwrap();
        assert_eq!(reg.builds["foo"], root.path().join("specs/foo.yaml"));
        assert_eq!(reg.tests["bar"], root.path().join("specs/bar-test.yaml"));
    }

    #[test]
    fn save_registry_preserves_config_block() {
        let root = TempDir::new().unwrap();
        write_named_marker(root.path(), "BOTFORGE", "config:\n  repo-only: true\n");

        save_registry(root.path(), &BTreeMap::new(), &BTreeMap::new()).unwrap();

        let contents = fs::read_to_string(root.path().join("BOTFORGE")).unwrap();
        assert!(
            contents.contains("repo-only") || contents.contains("repo_only"),
            "config block should be preserved: {contents}"
        );
    }

    #[test]
    fn save_registry_overwrites_existing_registry() {
        let root = TempDir::new().unwrap();
        write_marker(root.path(), "build:\n  old:\n    spec: old.yaml\n");

        let mut builds = BTreeMap::new();
        builds.insert("new".to_string(), root.path().join("new.yaml"));

        save_registry(root.path(), &builds, &BTreeMap::new()).unwrap();

        let reg = load_committed_registry(root.path()).unwrap();
        assert!(reg.builds.contains_key("new"));
        assert!(!reg.builds.contains_key("old"));
    }
}
