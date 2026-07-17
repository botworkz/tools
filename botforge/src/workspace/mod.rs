pub(crate) mod discover;
pub(crate) mod registry;

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use shasset::manifest::{Asset, Manifest, Settings};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::util::resolve_under_root;

pub(crate) const MARKER_NAMES: [&str; 5] = [
    "botforge.yaml",
    "botforge.yml",
    ".botforge.yaml",
    ".botforge.yml",
    "BOTFORGE",
];

const MARKER_DISPLAY: &str = "botforge.yaml, botforge.yml, .botforge.yaml, .botforge.yml, BOTFORGE";

// ── Plugin config ────────────────────────────────────────────────────────────

/// Raw YAML representation of a single plugin entry in the workspace marker.
///
/// ```yaml
/// plugins:
///   - name: hello
///     src: ./plugins/libhello.so       # repo-relative
///     provides:                        # OPTIONAL allow-list
///       - core/ping
/// ```
///
/// `deny_unknown_fields` ensures typos in entry keys are caught at parse time.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPluginEntry {
    /// Unique plugin instance name within this workspace.
    name: String,
    /// Path to the `.so` file.  Absolute paths are used as-is;
    /// relative paths are resolved against the context root.
    src: String,
    /// Optional capability allow-list.  When present, only listed slots
    /// are wired; when absent, all slots the plugin declares are wired.
    #[serde(default)]
    provides: Option<Vec<String>>,
}

/// A validated, fully-resolved plugin config entry ready for loading.
///
/// Created by [`load_plugin_entries`].  Consumers pass these to
/// `botforge_plugin_host::PluginRegistry::load_plugin` to open the `.so` and
/// wire capabilities.
#[derive(Debug, Clone)]
pub(crate) struct PluginEntry {
    /// Unique plugin instance name.
    pub(crate) name: String,
    /// Resolved absolute path to the `.so` file.
    pub(crate) src: PathBuf,
    /// Optional capability allow-list (absent ⇒ implicit-all).
    pub(crate) provides: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, Default)]
struct RawWorkspaceManifestDoc {
    #[serde(default)]
    settings: Settings,
    #[serde(default)]
    assets: BTreeMap<String, Asset>,
    /// Optional plugin list.  Parsed but NOT automatically loaded at
    /// workspace discovery time — callers that need plugins call
    /// [`load_plugin_entries`] explicitly.
    #[serde(default)]
    plugins: Vec<RawPluginEntry>,
}

pub(crate) fn is_marker_name(name: &str) -> bool {
    MARKER_NAMES.contains(&name)
}

pub(crate) fn find_marker_path(dir: &Path) -> Option<PathBuf> {
    MARKER_NAMES
        .iter()
        .map(|name| dir.join(name))
        .find(|path| path.is_file())
}

pub(crate) fn marker_path(dir: &Path) -> Result<PathBuf> {
    find_marker_path(dir).ok_or_else(|| {
        anyhow::anyhow!(
            "no botforge marker found in '{}'; expected one of: {MARKER_DISPLAY}",
            dir.display()
        )
    })
}

/// Discover the botforge workspace context root.
///
/// - When `explicit` is `None`: walk up from the current working directory looking
///   for a botforge marker file. The first directory (cwd or any ancestor)
///   that contains the marker is returned as the canonicalized context root.  If the
///   walk reaches the filesystem root without finding one, a hard error is returned.
/// - When `explicit` is `Some(dir)`: `dir` **must** contain a botforge marker.
///   If it does, the canonicalized `dir` is returned.  If not, a hard error is
///   returned.  The walk-up is **not** applied to an explicit path.
pub(crate) fn discover_context(explicit: Option<&Path>) -> Result<PathBuf> {
    let cwd = std::env::current_dir().context("failed to determine current directory")?;
    discover_context_from(explicit, &cwd)
}

fn discover_context_from(explicit: Option<&Path>, start_dir: &Path) -> Result<PathBuf> {
    if let Some(dir) = explicit {
        let canonical = std::fs::canonicalize(dir)
            .with_context(|| format!("--context '{}': cannot resolve directory", dir.display()))?;
        if let Some(marker) = find_marker_path(&canonical) {
            load_botforge_yaml(&marker)?;
            return Ok(canonical);
        }
        bail!(
            "--context '{}': no botforge marker found in that directory (expected one of: {MARKER_DISPLAY})",
            dir.display()
        );
    }

    // Walk up from the provided start directory.
    let mut dir: &Path = start_dir;
    loop {
        if let Some(marker) = find_marker_path(dir) {
            load_botforge_yaml(&marker)?;
            let canonical = std::fs::canonicalize(dir)
                .with_context(|| format!("cannot canonicalize context root: {}", dir.display()))?;
            return Ok(canonical);
        }
        match dir.parent() {
            Some(parent) => dir = parent,
            None => break,
        }
    }

    bail!(
        "not inside a botforge workspace: no marker found in the current directory or any parent (expected one of: {MARKER_DISPLAY})"
    );
}

/// Load and validate a botforge marker file. The file must exist and must parse as
/// valid YAML. An empty file or `{}` is fine.
fn load_botforge_yaml(path: &Path) -> Result<()> {
    let contents =
        std::fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))?;
    let _: serde_yaml::Value = serde_yaml::from_str(&contents)
        .with_context(|| format!("invalid YAML in {}", path.display()))?;
    Ok(())
}

pub(crate) fn load_inline_manifest(context_root: &Path) -> Result<Manifest> {
    let marker = marker_path(context_root)?;
    let contents = std::fs::read_to_string(&marker)
        .with_context(|| format!("cannot read {}", marker.display()))?;
    if contents.trim().is_empty() {
        return Ok(Manifest::default());
    }
    let doc: RawWorkspaceManifestDoc = serde_yaml::from_str(&contents)
        .with_context(|| format!("invalid YAML in {}", marker.display()))?;
    Ok(Manifest {
        settings: doc.settings,
        assets: doc.assets,
    })
}

/// Load and validate plugin entries from the workspace marker.
///
/// Returns an ordered list of validated [`PluginEntry`] values with resolved
/// `src` paths.
///
/// # Validation
///
/// - Duplicate `name` values across the `plugins:` list are an error.
/// - `src` is resolved via the two-root scheme:
///   - Absolute paths are used exactly as provided in config (no normalization
///     or `..` cleanup by this function).
///   - Relative paths are resolved against `context_root`.
/// - `name` must be non-empty.
/// - `provides:` entries are passed through as-is (slot format validation
///   happens in `botforge-plugin-host` at load time).
///
/// # No autoload
///
/// This function parses config only; it does **not** open or load any `.so`
/// files.  Callers that want to load plugins pass the returned entries to
/// `botforge_plugin_host::PluginRegistry::load_plugin`.
pub(crate) fn load_plugin_entries(context_root: &Path) -> Result<Vec<PluginEntry>> {
    let marker = marker_path(context_root)?;
    let contents = std::fs::read_to_string(&marker)
        .with_context(|| format!("cannot read {}", marker.display()))?;
    if contents.trim().is_empty() {
        return Ok(Vec::new());
    }
    let doc: RawWorkspaceManifestDoc = serde_yaml::from_str(&contents)
        .with_context(|| format!("invalid plugin config in {}", marker.display()))?;

    // Validate: duplicate names are an error.
    let mut seen_names = std::collections::HashSet::new();
    let mut entries = Vec::with_capacity(doc.plugins.len());
    for raw in doc.plugins {
        if raw.name.is_empty() {
            anyhow::bail!("plugin entry in {} has an empty 'name'", marker.display());
        }
        if !seen_names.insert(raw.name.clone()) {
            anyhow::bail!(
                "duplicate plugin name '{}' in {}",
                raw.name,
                marker.display()
            );
        }
        let src = PathBuf::from(&raw.src);
        let src = if src.is_absolute() {
            src
        } else {
            resolve_under_root(context_root, src)
        };
        entries.push(PluginEntry {
            name: raw.name,
            src,
            provides: raw.provides,
        });
    }
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::{
        discover_context, discover_context_from, find_marker_path, load_inline_manifest,
        load_plugin_entries,
    };
    use std::path::Path;
    use tempfile::TempDir;

    fn write_marker(dir: &Path) {
        std::fs::write(dir.join("botforge.yaml"), "").unwrap();
    }

    fn write_named_marker(dir: &Path, name: &str, contents: &str) {
        std::fs::write(dir.join(name), contents).unwrap();
    }

    // ── discover_context(None) ────────────────────────────────────────────────

    #[test]
    fn discover_context_none_walks_up_to_ancestor_with_marker() {
        let root = TempDir::new().unwrap();
        write_marker(root.path());
        let subdir = root.path().join("a/b/c");
        std::fs::create_dir_all(&subdir).unwrap();

        let result = discover_context_from(None, &subdir).unwrap();
        assert_eq!(result, root.path().canonicalize().unwrap());
    }

    #[test]
    fn discover_context_none_finds_marker_in_cwd() {
        let root = TempDir::new().unwrap();
        write_marker(root.path());
        let result = discover_context_from(None, root.path()).unwrap();
        assert_eq!(result, root.path().canonicalize().unwrap());
    }

    #[test]
    fn discover_context_none_errors_when_no_marker_found() {
        // Use a temp directory with no marker anywhere in its ancestor chain.
        let root = TempDir::new().unwrap();
        let err = discover_context_from(None, root.path()).unwrap_err();
        assert!(
            format!("{err:#}").contains("not inside a botforge workspace"),
            "expected 'not inside a botforge workspace' error, got: {err:#}"
        );
    }

    // ── discover_context(Some(dir)) ───────────────────────────────────────────

    #[test]
    fn discover_context_explicit_with_marker_returns_canonical() {
        let root = TempDir::new().unwrap();
        write_marker(root.path());
        let result = discover_context(Some(root.path())).unwrap();
        assert_eq!(result, root.path().canonicalize().unwrap());
    }

    #[test]
    fn discover_context_explicit_without_marker_errors() {
        let root = TempDir::new().unwrap();
        let err = discover_context(Some(root.path())).unwrap_err();
        assert!(
            format!("{err:#}").contains("no botforge marker found in that directory"),
            "expected marker-absent error, got: {err:#}"
        );
    }

    #[test]
    fn discover_context_explicit_does_not_walk_up() {
        // Marker is only in the parent, not in the dir itself — explicit must fail.
        let root = TempDir::new().unwrap();
        write_marker(root.path());
        let subdir = root.path().join("sub");
        std::fs::create_dir_all(&subdir).unwrap();
        let err = discover_context(Some(&subdir)).unwrap_err();
        assert!(
            format!("{err:#}").contains("no botforge marker found in that directory"),
            "explicit context must not walk up: {err:#}"
        );
    }

    // ── botforge.yaml contents ────────────────────────────────────────────────

    #[test]
    fn discover_context_empty_marker_is_valid() {
        let root = TempDir::new().unwrap();
        std::fs::write(root.path().join("botforge.yaml"), "").unwrap();
        let result = discover_context(Some(root.path())).unwrap();
        assert_eq!(result, root.path().canonicalize().unwrap());
    }

    #[test]
    fn discover_context_empty_map_marker_is_valid() {
        let root = TempDir::new().unwrap();
        std::fs::write(root.path().join("botforge.yaml"), "{}").unwrap();
        let result = discover_context(Some(root.path())).unwrap();
        assert_eq!(result, root.path().canonicalize().unwrap());
    }

    #[test]
    fn discover_context_invalid_yaml_marker_errors() {
        let root = TempDir::new().unwrap();
        std::fs::write(root.path().join("botforge.yaml"), ": invalid: [yaml").unwrap();
        let err = discover_context(Some(root.path())).unwrap_err();
        assert!(
            format!("{err:#}").contains("invalid YAML"),
            "expected invalid YAML error, got: {err:#}"
        );
    }

    #[test]
    fn discover_context_accepts_all_marker_names() {
        for marker in [
            "botforge.yaml",
            "botforge.yml",
            ".botforge.yaml",
            ".botforge.yml",
            "BOTFORGE",
        ] {
            let root = TempDir::new().unwrap();
            write_named_marker(root.path(), marker, "");
            let result = discover_context(Some(root.path())).unwrap();
            assert_eq!(
                result,
                root.path().canonicalize().unwrap(),
                "marker {marker} should be accepted"
            );
        }
    }

    #[test]
    fn find_marker_path_prefers_existing_priority_order() {
        let root = TempDir::new().unwrap();
        write_named_marker(root.path(), ".botforge.yaml", "");
        write_named_marker(root.path(), "botforge.yml", "");
        let marker = find_marker_path(root.path()).unwrap();
        assert_eq!(
            marker.file_name().and_then(|name| name.to_str()),
            Some("botforge.yml")
        );
    }

    #[test]
    fn load_inline_manifest_reads_assets_and_settings_from_marker() {
        let root = TempDir::new().unwrap();
        write_named_marker(
            root.path(),
            ".botforge.yaml",
            "settings:\n  retries: 9\nassets:\n  base:\n    uri: https://example.com/base.qcow2\n    version: \"1\"\n",
        );
        let manifest = load_inline_manifest(root.path()).unwrap();
        assert_eq!(manifest.settings.retries, 9);
        assert_eq!(manifest.assets.len(), 1);
        assert!(manifest.assets.contains_key("base"));
    }

    #[test]
    fn load_inline_manifest_defaults_when_assets_block_is_absent() {
        let root = TempDir::new().unwrap();
        write_named_marker(root.path(), "BOTFORGE", "build: {}\n");
        let manifest = load_inline_manifest(root.path()).unwrap();
        assert!(manifest.assets.is_empty());
        assert_eq!(manifest.settings.retries, 3);
        assert_eq!(manifest.settings.concurrency, 4);
        assert_eq!(manifest.settings.backoff.base_ms, 500);
    }

    // ── plugins: config tests ─────────────────────────────────────────────────

    #[test]
    fn load_plugin_entries_empty_when_no_plugins_key() {
        let root = TempDir::new().unwrap();
        write_named_marker(root.path(), "botforge.yaml", "");
        let entries = load_plugin_entries(root.path()).unwrap();
        assert!(entries.is_empty());
    }

    #[test]
    fn load_plugin_entries_parses_absolute_src() {
        let root = TempDir::new().unwrap();
        write_named_marker(
            root.path(),
            "botforge.yaml",
            "plugins:\n  - name: hello\n    src: /usr/share/botforge/plugins/libhello.so\n",
        );
        let entries = load_plugin_entries(root.path()).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "hello");
        assert_eq!(
            entries[0].src,
            std::path::Path::new("/usr/share/botforge/plugins/libhello.so")
        );
        assert!(entries[0].provides.is_none());
    }

    #[test]
    fn load_plugin_entries_keeps_absolute_src_exactly_as_written() {
        let root = TempDir::new().unwrap();
        let raw_src = "/usr/share/botforge/plugins/../plugins/libhello.so";
        write_named_marker(
            root.path(),
            "botforge.yaml",
            &format!("plugins:\n  - name: hello\n    src: {raw_src}\n"),
        );
        let entries = load_plugin_entries(root.path()).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].src, std::path::Path::new(raw_src));
    }

    #[test]
    fn load_plugin_entries_resolves_relative_src_against_context_root() {
        let root = TempDir::new().unwrap();
        write_named_marker(
            root.path(),
            "botforge.yaml",
            "plugins:\n  - name: hello\n    src: ./plugins/libhello.so\n",
        );
        let entries = load_plugin_entries(root.path()).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].src, root.path().join("plugins/libhello.so"));
    }

    #[test]
    fn load_plugin_entries_parses_provides() {
        let root = TempDir::new().unwrap();
        write_named_marker(
            root.path(),
            "botforge.yaml",
            "plugins:\n  - name: hello\n    src: /tmp/libhello.so\n    provides:\n      - core/ping\n",
        );
        let entries = load_plugin_entries(root.path()).unwrap();
        assert_eq!(entries[0].provides, Some(vec!["core/ping".to_owned()]));
    }

    #[test]
    fn load_plugin_entries_duplicate_name_is_error() {
        let root = TempDir::new().unwrap();
        write_named_marker(
            root.path(),
            "botforge.yaml",
            "plugins:\n  - name: hello\n    src: /tmp/a.so\n  - name: hello\n    src: /tmp/b.so\n",
        );
        let err = load_plugin_entries(root.path()).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("duplicate") && msg.contains("hello"),
            "expected duplicate-name error: {msg}"
        );
    }

    #[test]
    fn load_plugin_entries_empty_name_is_error() {
        let root = TempDir::new().unwrap();
        write_named_marker(
            root.path(),
            "botforge.yaml",
            "plugins:\n  - name: \"\"\n    src: /tmp/a.so\n",
        );
        let err = load_plugin_entries(root.path()).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("empty"), "expected empty-name error: {msg}");
    }

    #[test]
    fn load_plugin_entries_unknown_field_is_error() {
        let root = TempDir::new().unwrap();
        write_named_marker(
            root.path(),
            "botforge.yaml",
            "plugins:\n  - name: hello\n    src: /tmp/a.so\n    unknown_field: oops\n",
        );
        let err = load_plugin_entries(root.path()).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("unknown_field") || msg.contains("unknown field"),
            "expected unknown-field error: {msg}"
        );
    }
}
