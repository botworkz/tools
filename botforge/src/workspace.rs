use anyhow::{bail, Context, Result};
use serde::Deserialize;
use shasset::manifest::{Asset, Manifest, Settings};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const MARKER: &str = "botforge.yaml";

/// Parsed workspace context: the resolved root directory and the shasset manifest
/// sourced from the `assets:` section of `botforge.yaml`.
///
/// When `botforge.yaml` has no `assets:` section the manifest is empty
/// (`Manifest::default()`).  That is valid — it simply means no shasset assets
/// are available in this workspace.
#[derive(Debug)]
pub(crate) struct WorkspaceContext {
    pub(crate) root: PathBuf,
    pub(crate) manifest: Manifest,
}

/// Discover the botforge workspace context root.
///
/// - When `explicit` is `None`: walk up from the current working directory looking
///   for a file named `botforge.yaml`.  The first directory (cwd or any ancestor)
///   that contains the marker is returned as the canonicalized context root.  If the
///   walk reaches the filesystem root without finding one, a hard error is returned.
/// - When `explicit` is `Some(dir)`: `dir` **must** contain a `botforge.yaml`.
///   If it does, the canonicalized `dir` is returned.  If not, a hard error is
///   returned.  The walk-up is **not** applied to an explicit path.
///
/// Returns a [`WorkspaceContext`] containing both the root path and the shasset
/// [`Manifest`] parsed from the `assets:` section of `botforge.yaml` (empty if
/// the section is absent).
pub(crate) fn discover_workspace(explicit: Option<&Path>) -> Result<WorkspaceContext> {
    let cwd = std::env::current_dir().context("failed to determine current directory")?;
    discover_workspace_from(explicit, &cwd)
}

fn discover_workspace_from(explicit: Option<&Path>, start_dir: &Path) -> Result<WorkspaceContext> {
    if let Some(dir) = explicit {
        let canonical = std::fs::canonicalize(dir)
            .with_context(|| format!("--context '{}': cannot resolve directory", dir.display()))?;
        let marker = canonical.join(MARKER);
        if marker.is_file() {
            let manifest = load_botforge_yaml(&marker)?;
            return Ok(WorkspaceContext {
                root: canonical,
                manifest,
            });
        }
        bail!(
            "--context '{}': no botforge.yaml found in that directory",
            dir.display()
        );
    }

    // Walk up from the provided start directory.
    let mut dir: &Path = start_dir;
    loop {
        let marker = dir.join(MARKER);
        if marker.is_file() {
            let manifest = load_botforge_yaml(&marker)?;
            let canonical = std::fs::canonicalize(dir)
                .with_context(|| format!("cannot canonicalize context root: {}", dir.display()))?;
            return Ok(WorkspaceContext {
                root: canonical,
                manifest,
            });
        }
        match dir.parent() {
            Some(parent) => dir = parent,
            None => break,
        }
    }

    bail!(
        "not inside a botforge workspace: no botforge.yaml found in the current directory or any parent"
    );
}

/// Raw deserialization target for `botforge.yaml`.
///
/// Only the `assets:` section is extracted here; other top-level keys (e.g.
/// the `config:` block added by B3) are unknown and silently ignored via
/// `serde_yaml`'s default behaviour.
#[derive(Debug, Deserialize, Default)]
struct BotforgeYaml {
    /// The shasset asset entries, keyed by asset name.
    ///
    /// Absent in the file → empty map (no assets).  Present → handed to the
    /// shasset library as a [`Manifest`] with default settings.
    #[serde(default)]
    assets: BTreeMap<String, Asset>,
}

/// Parse `botforge.yaml` and extract the shasset [`Manifest`] from the
/// optional `assets:` section.
///
/// Returns `Manifest::default()` (empty assets, default settings) when the
/// `assets:` key is absent.  An invalid YAML file is a hard error.
fn load_botforge_yaml(path: &Path) -> Result<Manifest> {
    let contents =
        std::fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))?;
    let bf: BotforgeYaml = serde_yaml::from_str(&contents)
        .with_context(|| format!("invalid YAML in {}", path.display()))?;
    Ok(Manifest {
        settings: Settings::default(),
        assets: bf.assets,
    })
}

#[cfg(test)]
mod tests {
    use super::{discover_workspace, discover_workspace_from};
    use std::path::Path;
    use tempfile::TempDir;

    fn write_marker(dir: &Path) {
        std::fs::write(dir.join("botforge.yaml"), "").unwrap();
    }

    // ── discover_workspace(None) ──────────────────────────────────────────────

    #[test]
    fn discover_context_none_walks_up_to_ancestor_with_marker() {
        let root = TempDir::new().unwrap();
        write_marker(root.path());
        let subdir = root.path().join("a/b/c");
        std::fs::create_dir_all(&subdir).unwrap();

        let ws = discover_workspace_from(None, &subdir).unwrap();
        assert_eq!(ws.root, root.path().canonicalize().unwrap());
    }

    #[test]
    fn discover_context_none_finds_marker_in_cwd() {
        let root = TempDir::new().unwrap();
        write_marker(root.path());
        let ws = discover_workspace_from(None, root.path()).unwrap();
        assert_eq!(ws.root, root.path().canonicalize().unwrap());
    }

    #[test]
    fn discover_context_none_errors_when_no_marker_found() {
        // Use a temp directory with no marker anywhere in its ancestor chain.
        let root = TempDir::new().unwrap();
        let err = discover_workspace_from(None, root.path()).unwrap_err();
        assert!(
            format!("{err:#}").contains("not inside a botforge workspace"),
            "expected 'not inside a botforge workspace' error, got: {err:#}"
        );
    }

    // ── discover_workspace(Some(dir)) ────────────────────────────────────────

    #[test]
    fn discover_context_explicit_with_marker_returns_canonical() {
        let root = TempDir::new().unwrap();
        write_marker(root.path());
        let ws = discover_workspace(Some(root.path())).unwrap();
        assert_eq!(ws.root, root.path().canonicalize().unwrap());
    }

    #[test]
    fn discover_context_explicit_without_marker_errors() {
        let root = TempDir::new().unwrap();
        let err = discover_workspace(Some(root.path())).unwrap_err();
        assert!(
            format!("{err:#}").contains("no botforge.yaml found in that directory"),
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
        let err = discover_workspace(Some(&subdir)).unwrap_err();
        assert!(
            format!("{err:#}").contains("no botforge.yaml found in that directory"),
            "explicit context must not walk up: {err:#}"
        );
    }

    // ── botforge.yaml contents ────────────────────────────────────────────────

    #[test]
    fn discover_context_empty_marker_is_valid() {
        let root = TempDir::new().unwrap();
        std::fs::write(root.path().join("botforge.yaml"), "").unwrap();
        let ws = discover_workspace(Some(root.path())).unwrap();
        assert_eq!(ws.root, root.path().canonicalize().unwrap());
        assert!(
            ws.manifest.assets.is_empty(),
            "empty marker yields empty manifest"
        );
    }

    #[test]
    fn discover_context_empty_map_marker_is_valid() {
        let root = TempDir::new().unwrap();
        std::fs::write(root.path().join("botforge.yaml"), "{}").unwrap();
        let ws = discover_workspace(Some(root.path())).unwrap();
        assert_eq!(ws.root, root.path().canonicalize().unwrap());
    }

    #[test]
    fn discover_context_invalid_yaml_marker_errors() {
        let root = TempDir::new().unwrap();
        std::fs::write(root.path().join("botforge.yaml"), ": invalid: [yaml").unwrap();
        let err = discover_workspace(Some(root.path())).unwrap_err();
        assert!(
            format!("{err:#}").contains("invalid YAML"),
            "expected invalid YAML error, got: {err:#}"
        );
    }

    // ── assets: section ───────────────────────────────────────────────────────

    #[test]
    fn assets_section_parsed_into_manifest() {
        let root = TempDir::new().unwrap();
        std::fs::write(
            root.path().join("botforge.yaml"),
            "assets:\n  my-tool:\n    uri: https://example.com/v1/tool\n    version: \"1.0\"\n",
        )
        .unwrap();
        let ws = discover_workspace(Some(root.path())).unwrap();
        assert!(
            ws.manifest.assets.contains_key("my-tool"),
            "assets section should be parsed into manifest"
        );
        assert_eq!(
            ws.manifest.assets["my-tool"].uri,
            "https://example.com/v1/tool"
        );
    }

    #[test]
    fn absent_assets_section_yields_empty_manifest() {
        let root = TempDir::new().unwrap();
        std::fs::write(root.path().join("botforge.yaml"), "{}").unwrap();
        let ws = discover_workspace(Some(root.path())).unwrap();
        assert!(
            ws.manifest.assets.is_empty(),
            "absent assets: should yield empty manifest, not an error"
        );
    }

    #[test]
    fn stray_shasset_yaml_is_ignored() {
        // A shasset.yaml next to botforge.yaml must NOT affect the workspace manifest.
        let root = TempDir::new().unwrap();
        std::fs::write(root.path().join("botforge.yaml"), "{}").unwrap();
        std::fs::write(
            root.path().join("shasset.yaml"),
            "assets:\n  stray-tool:\n    uri: https://example.com/stray\n    version: \"1\"\n",
        )
        .unwrap();
        let ws = discover_workspace(Some(root.path())).unwrap();
        assert!(
            ws.manifest.assets.is_empty(),
            "stray shasset.yaml must be ignored; manifest should be empty: {:?}",
            ws.manifest.assets.keys().collect::<Vec<_>>()
        );
    }
}
