use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};

const MARKER: &str = "botforge.yaml";

/// Discover the botforge workspace context root.
///
/// - When `explicit` is `None`: walk up from the current working directory looking
///   for a file named `botforge.yaml`.  The first directory (cwd or any ancestor)
///   that contains the marker is returned as the canonicalized context root.  If the
///   walk reaches the filesystem root without finding one, a hard error is returned.
/// - When `explicit` is `Some(dir)`: `dir` **must** contain a `botforge.yaml`.
///   If it does, the canonicalized `dir` is returned.  If not, a hard error is
///   returned.  The walk-up is **not** applied to an explicit path.
pub(crate) fn discover_context(explicit: Option<&Path>) -> Result<PathBuf> {
    if let Some(dir) = explicit {
        let canonical = std::fs::canonicalize(dir)
            .with_context(|| format!("--context '{}': cannot resolve directory", dir.display()))?;
        if canonical.join(MARKER).is_file() {
            load_botforge_yaml(&canonical.join(MARKER))?;
            return Ok(canonical);
        }
        bail!(
            "--context '{}': no botforge.yaml found in that directory",
            dir.display()
        );
    }

    // Walk up from cwd.
    let cwd = std::env::current_dir().context("failed to determine current directory")?;
    let mut dir: &Path = &cwd;
    loop {
        if dir.join(MARKER).is_file() {
            load_botforge_yaml(&dir.join(MARKER))?;
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
        "not inside a botforge workspace: no botforge.yaml found in the current directory or any parent"
    );
}

/// Load and validate `botforge.yaml`.  The file must exist and must parse as valid
/// YAML.  An empty file or `{}` is fine — the marker is presence-only for now.
fn load_botforge_yaml(path: &Path) -> Result<()> {
    let contents =
        std::fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))?;
    let _: serde_yaml::Value = serde_yaml::from_str(&contents)
        .with_context(|| format!("invalid YAML in {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::discover_context;
    use std::path::Path;
    use tempfile::TempDir;

    fn write_marker(dir: &Path) {
        std::fs::write(dir.join("botforge.yaml"), "").unwrap();
    }

    // ── discover_context(None) ────────────────────────────────────────────────

    #[test]
    fn discover_context_none_walks_up_to_ancestor_with_marker() {
        let root = TempDir::new().unwrap();
        write_marker(root.path());
        let subdir = root.path().join("a/b/c");
        std::fs::create_dir_all(&subdir).unwrap();

        // Temporarily change cwd to the subdir.
        let _guard = CwdGuard::set(&subdir);
        let result = discover_context(None).unwrap();
        assert_eq!(result, root.path().canonicalize().unwrap());
    }

    #[test]
    fn discover_context_none_finds_marker_in_cwd() {
        let root = TempDir::new().unwrap();
        write_marker(root.path());
        let _guard = CwdGuard::set(root.path());
        let result = discover_context(None).unwrap();
        assert_eq!(result, root.path().canonicalize().unwrap());
    }

    #[test]
    fn discover_context_none_errors_when_no_marker_found() {
        // Use a directory that definitely has no botforge.yaml anywhere in the
        // real ancestor chain — we create an isolated tmp tree with no marker.
        // We can't guarantee the real root has no marker, but we can test the
        // error message by setting cwd to a path with no marker upward.
        //
        // Create a temp dir, do NOT write marker, use it as cwd.
        // The walk will reach the fs root without finding a marker.
        // (In CI the real root almost certainly has no botforge.yaml.)
        let root = TempDir::new().unwrap();
        let _guard = CwdGuard::set(root.path());
        let err = discover_context(None).unwrap_err();
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
        let err = discover_context(Some(&subdir)).unwrap_err();
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

    // ── Helpers ───────────────────────────────────────────────────────────────

    /// RAII guard that sets cwd and restores it on drop.
    /// Tests that change cwd must be single-threaded (they are, per test binary).
    struct CwdGuard {
        original: std::path::PathBuf,
    }

    impl CwdGuard {
        fn set(path: &Path) -> Self {
            let original = std::env::current_dir().unwrap();
            std::env::set_current_dir(path).unwrap();
            CwdGuard { original }
        }
    }

    impl Drop for CwdGuard {
        fn drop(&mut self) {
            let _ = std::env::set_current_dir(&self.original);
        }
    }
}
