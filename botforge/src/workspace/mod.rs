pub(crate) mod discover;
pub(crate) mod registry;

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use shasset::manifest::{Asset, Manifest, Settings};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub(crate) const MARKER_NAMES: [&str; 5] = [
    "botforge.yaml",
    "botforge.yml",
    ".botforge.yaml",
    ".botforge.yml",
    "BOTFORGE",
];

const MARKER_DISPLAY: &str = "botforge.yaml, botforge.yml, .botforge.yaml, .botforge.yml, BOTFORGE";

#[derive(Debug, Deserialize, Default)]
struct RawWorkspaceManifestDoc {
    #[serde(default)]
    settings: Settings,
    #[serde(default)]
    assets: BTreeMap<String, Asset>,
}

pub(crate) fn is_marker_name(name: &str) -> bool {
    MARKER_NAMES.contains(&name)
}

pub(crate) fn find_marker_path(dir: &Path) -> Option<PathBuf> {
    MARKER_NAMES.iter().map(|name| dir.join(name)).find(|path| path.is_file())
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
    let contents =
        std::fs::read_to_string(&marker).with_context(|| format!("cannot read {}", marker.display()))?;
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

#[cfg(test)]
mod tests {
    use super::{discover_context, discover_context_from, find_marker_path, load_inline_manifest};
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
        assert_eq!(marker.file_name().and_then(|name| name.to_str()), Some("botforge.yml"));
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
}
