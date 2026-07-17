//! Committed registry I/O — B4.
//!
//! Reads and writes the `plans:` registry block in `botforge.yaml`.
//! The block is a map from plan **name** to an object with optional scalar
//! `build:` and `test:` values (spec file paths, context-relative).
//!
//! Example:
//! ```yaml
//! plans:
//!   botwork:
//!     build: botwork/build.yaml
//!     test: botwork/test/test.yaml
//!   botwork-base:
//!     build: botwork-base/build.yaml
//! ```
//!
//! Paths are stored relative to the context root and are validated to ensure they
//! do not escape via `..` components.  On load, relative paths are resolved to
//! absolute paths anchored at the context root.

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};

use super::marker_path;

// ─── raw deserialization types ────────────────────────────────────────────────

/// A single plan entry as stored in `botforge.yaml`.
#[derive(Debug, Deserialize, Default)]
struct RawPlan {
    pub(crate) build: Option<String>,
    pub(crate) test: Option<String>,
    pub(crate) publish: Option<String>,
}

/// The `botforge.yaml` root document — only the registry block is consumed here.
#[derive(Debug, Deserialize, Default)]
struct RawWorkspaceRegistryDoc {
    #[serde(default)]
    pub(crate) plans: BTreeMap<String, RawPlan>,
}

// ─── CommittedRegistry ───────────────────────────────────────────────────────

/// The in-memory representation of the committed `plans:` registry.
///
/// Paths are **absolute** (resolved relative to the context root at load time).
#[derive(Debug, Default)]
pub(crate) struct CommittedRegistry {
    /// Maps plan name → absolute path for `type: botforge/build` entries.
    pub(crate) builds: BTreeMap<String, PathBuf>,
    /// Maps plan name → absolute path for `type: botforge/test` entries.
    pub(crate) tests: BTreeMap<String, PathBuf>,
    /// Maps plan name → absolute path for `type: botforge/publish` entries.
    pub(crate) publishes: BTreeMap<String, PathBuf>,
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

    /// Resolve a publish spec by name.
    ///
    /// Returns the absolute path of the spec file, or an error naming the registry
    /// and hinting at `botforge config sync`.
    pub(crate) fn publish(&self, name: &str) -> Result<&PathBuf> {
        self.publishes.get(name).ok_or_else(|| {
            anyhow::anyhow!(
                "no publish plan named '{}' in registry (run 'botforge config sync')",
                name
            )
        })
    }
}

// ─── path validation ─────────────────────────────────────────────────────────

/// Validate that a registry spec path is safe: must be relative, must not
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

// ─── load ────────────────────────────────────────────────────────────────────

/// Load the committed registry from `<context_root>/botforge.yaml`.
///
/// Returns a `CommittedRegistry` with absolute spec paths.  If `botforge.yaml`
/// has no `plans:` block, the respective maps are empty (not an error).
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

    for (name, plan) in doc.plans {
        if let Some(spec) = plan.build {
            validate_spec_path(&spec).with_context(|| {
                format!(
                    "invalid build spec path for plan '{name}' in {}",
                    marker_path.display()
                )
            })?;
            let abs = context_root.join(&spec);
            reg.builds.insert(name.clone(), abs);
        }
        if let Some(spec) = plan.test {
            validate_spec_path(&spec).with_context(|| {
                format!(
                    "invalid test spec path for plan '{name}' in {}",
                    marker_path.display()
                )
            })?;
            let abs = context_root.join(&spec);
            reg.tests.insert(name.clone(), abs);
        }
        if let Some(spec) = plan.publish {
            validate_spec_path(&spec).with_context(|| {
                format!(
                    "invalid publish spec path for plan '{name}' in {}",
                    marker_path.display()
                )
            })?;
            let abs = context_root.join(&spec);
            reg.publishes.insert(name, abs);
        }
    }

    Ok(reg)
}

// ─── save ────────────────────────────────────────────────────────────────────

/// Rewrite the `plans:` registry block in `<context_root>/botforge.yaml`,
/// preserving all other keys (e.g. `config:`, `assets:`) unchanged.
///
/// `builds`, `tests`, and `publishes` map plan names → **absolute** paths.
/// Paths are stored as relative-to-context-root in the YAML.
pub(crate) fn save_registry(
    context_root: &Path,
    builds: &BTreeMap<String, PathBuf>,
    tests: &BTreeMap<String, PathBuf>,
    publishes: &BTreeMap<String, PathBuf>,
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

    // Build the new `plans:` block.
    let plans_block = build_plans_yaml(context_root, builds, tests, publishes)?;

    // Insert or overwrite the `plans:` key.
    map.insert(serde_yaml::Value::String("plans".to_string()), plans_block);

    let yaml_str =
        serde_yaml::to_string(&doc).context("failed to serialize updated botforge.yaml")?;

    std::fs::write(&marker_path, yaml_str)
        .with_context(|| format!("cannot write {}", marker_path.display()))?;

    Ok(())
}

/// Build a `plans:` YAML value from plan-name → absolute-path maps.
fn build_plans_yaml(
    context_root: &Path,
    builds: &BTreeMap<String, PathBuf>,
    tests: &BTreeMap<String, PathBuf>,
    publishes: &BTreeMap<String, PathBuf>,
) -> Result<serde_yaml::Value> {
    // Collect the union of all plan names in sorted order.
    let names: BTreeSet<&String> = builds
        .keys()
        .chain(tests.keys())
        .chain(publishes.keys())
        .collect();

    let mut plans = serde_yaml::Mapping::new();

    for name in names {
        let mut plan = serde_yaml::Mapping::new();

        if let Some(abs_path) = builds.get(name) {
            let rel_str = abs_to_rel_str(context_root, abs_path)?;
            plan.insert(
                serde_yaml::Value::String("build".to_string()),
                serde_yaml::Value::String(rel_str),
            );
        }
        if let Some(abs_path) = tests.get(name) {
            let rel_str = abs_to_rel_str(context_root, abs_path)?;
            plan.insert(
                serde_yaml::Value::String("test".to_string()),
                serde_yaml::Value::String(rel_str),
            );
        }
        if let Some(abs_path) = publishes.get(name) {
            let rel_str = abs_to_rel_str(context_root, abs_path)?;
            plan.insert(
                serde_yaml::Value::String("publish".to_string()),
                serde_yaml::Value::String(rel_str),
            );
        }

        plans.insert(
            serde_yaml::Value::String(name.clone()),
            serde_yaml::Value::Mapping(plan),
        );
    }

    Ok(serde_yaml::Value::Mapping(plans))
}

/// Convert an absolute path to a context-root-relative UTF-8 string.
fn abs_to_rel_str(context_root: &Path, abs_path: &Path) -> Result<String> {
    let rel = abs_path.strip_prefix(context_root).with_context(|| {
        format!(
            "spec path '{}' is outside context root '{}'",
            abs_path.display(),
            context_root.display()
        )
    })?;
    rel.to_str()
        .ok_or_else(|| anyhow::anyhow!("spec path is not valid UTF-8: {}", abs_path.display()))
        .map(str::to_string)
}

// ─── tests ─────────────────────────────────────────────────────────────────────

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
            "plans:\n  foo:\n    build: specs/foo.yaml\n  bar:\n    test: specs/bar-test.yaml\n",
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
            "plans:\n  foo:\n    build: specs/foo.yaml\n",
        );
        let reg = load_committed_registry(root.path()).unwrap();
        assert_eq!(reg.builds["foo"], root.path().join("specs/foo.yaml"));
    }

    #[test]
    fn load_registry_rejects_dotdot_spec_path() {
        let root = TempDir::new().unwrap();
        write_marker(
            root.path(),
            "plans:\n  evil:\n    build: ../../etc/passwd\n",
        );
        let err = load_committed_registry(root.path()).unwrap_err();
        assert!(format!("{err:#}").contains(".."), "{err:#}");
    }

    #[test]
    fn load_registry_build_and_test_may_share_name() {
        let root = TempDir::new().unwrap();
        write_marker(
            root.path(),
            "plans:\n  foo:\n    build: specs/foo.yaml\n    test: specs/foo-test.yaml\n",
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

        save_registry(root.path(), &builds, &tests, &BTreeMap::new()).unwrap();

        let reg = load_committed_registry(root.path()).unwrap();
        assert_eq!(reg.builds["foo"], root.path().join("specs/foo.yaml"));
        assert_eq!(reg.tests["bar"], root.path().join("specs/bar-test.yaml"));
    }

    #[test]
    fn save_registry_round_trips_with_publish() {
        let root = TempDir::new().unwrap();
        write_marker(root.path(), "");

        let mut publishes = BTreeMap::new();
        publishes.insert("my-release".to_string(), root.path().join("publish.yaml"));

        save_registry(root.path(), &BTreeMap::new(), &BTreeMap::new(), &publishes).unwrap();

        let reg = load_committed_registry(root.path()).unwrap();
        assert_eq!(
            reg.publishes["my-release"],
            root.path().join("publish.yaml")
        );
    }

    #[test]
    fn save_registry_preserves_config_block() {
        let root = TempDir::new().unwrap();
        write_named_marker(root.path(), "BOTFORGE", "config:\n  repo-only: true\n");

        save_registry(
            root.path(),
            &BTreeMap::new(),
            &BTreeMap::new(),
            &BTreeMap::new(),
        )
        .unwrap();

        let contents = fs::read_to_string(root.path().join("BOTFORGE")).unwrap();
        assert!(
            contents.contains("repo-only") || contents.contains("repo_only"),
            "config block should be preserved: {contents}"
        );
    }

    #[test]
    fn save_registry_overwrites_existing_registry() {
        let root = TempDir::new().unwrap();
        write_marker(root.path(), "plans:\n  old:\n    build: old.yaml\n");

        let mut builds = BTreeMap::new();
        builds.insert("new".to_string(), root.path().join("new.yaml"));

        save_registry(root.path(), &builds, &BTreeMap::new(), &BTreeMap::new()).unwrap();

        let reg = load_committed_registry(root.path()).unwrap();
        assert!(reg.builds.contains_key("new"));
        assert!(!reg.builds.contains_key("old"));
    }
}
