//! Path/filesystem helpers for resolving botforge references.
//!
//! Handles single-file existence + symlink-escape checks and glob expansion
//! under a repo or artifact root.  No shasset imports; all logic is pure
//! filesystem I/O.

use anyhow::{bail, Context, Result};
use glob::MatchOptions;
use std::path::{Component, Path, PathBuf};

use super::ResolvedFile;

/// Verify that `path` exists as a regular file inside `canonical_root`.
///
/// Resolves symlinks and asserts the result does not escape the root.
pub(super) fn resolve_existing_file(
    path: PathBuf,
    label: &str,
    canonical_root: &Path,
) -> Result<PathBuf> {
    if !path.exists() {
        bail!("{label} not found: {}", path.display());
    }
    if !path.is_file() {
        bail!("{label} must resolve to a file: {}", path.display());
    }
    // Resolve any symlinks and assert the result stays inside the repo root.
    // This is the complement to the parse-time dot/dotdot check: it catches
    // a symlink placed inside `build/artifact` (or the repo tree) that points
    // outside the root.
    let canonical = path
        .canonicalize()
        .with_context(|| format!("failed to canonicalize {label}: {}", path.display()))?;
    if !canonical.starts_with(canonical_root) {
        bail!(
            "{label} escapes repository root via symlink: \
             resolved path '{}' is outside root '{}'",
            canonical.display(),
            canonical_root.display()
        );
    }
    Ok(canonical)
}

/// Returns `true` if `s` contains any glob metacharacter (`*`, `?`, `[`).
pub(super) fn has_glob_metacharacters(s: &str) -> bool {
    s.contains('*') || s.contains('?') || s.contains('[')
}

/// Returns the fixed literal prefix of a glob pattern: all leading path
/// components that contain no metacharacters.
///
/// For `images/botspace/**/*.yaml` this yields `images/botspace`.
/// For `**/*.yaml` or a pattern that starts with a wildcard this yields `""`.
/// For a fully-literal path like `images/foo.yaml` this yields
/// `images/foo.yaml` (the entire path).
pub(super) fn glob_fixed_prefix(pattern: &str) -> PathBuf {
    let mut prefix = PathBuf::new();
    for component in Path::new(pattern).components() {
        let Component::Normal(part) = component else {
            break;
        };
        if has_glob_metacharacters(&part.to_string_lossy()) {
            break;
        }
        prefix.push(part);
    }
    prefix
}

/// Resolve `pattern` (relative to `root`) to a set of [`ResolvedFile`] entries.
///
/// When `pattern` contains glob metacharacters the function expands the glob
/// and returns every regular file matched, with a relative path computed by
/// stripping `root.join(fixed_literal_prefix(pattern))`.
///
/// When `pattern` is a fully-literal path the function verifies the path
/// exists as a regular file (existence check + symlink-escape guard) and
/// returns a single entry whose `relative_path` is the file's base name.
///
/// `canonical_root` must be the pre-canonicalized repo root used for
/// symlink-escape containment checks.
pub(super) fn resolve_ref_path_to_files(
    root: &Path,
    pattern: &Path,
    label: &str,
    canonical_root: &Path,
) -> Result<Vec<ResolvedFile>> {
    let pattern_str = pattern.to_string_lossy();

    if has_glob_metacharacters(&pattern_str) {
        let fixed_prefix = glob_fixed_prefix(&pattern_str);
        let fixed_prefix_root = root.join(&fixed_prefix);
        let full_pattern = root.join(pattern).to_string_lossy().into_owned();

        let match_options = MatchOptions {
            case_sensitive: true,
            require_literal_separator: false,
            require_literal_leading_dot: false,
        };

        let mut files = Vec::new();
        for entry in glob::glob_with(&full_pattern, match_options)
            .with_context(|| format!("invalid {label} glob '{pattern_str}'"))?
        {
            let local_path = entry.with_context(|| {
                format!(
                    "failed while expanding {label} glob '{pattern_str}' under {}",
                    root.display()
                )
            })?;
            // Only stage regular files; silently skip directories and other
            // special file types.
            if !local_path.is_file() {
                continue;
            }
            // Resolve symlinks and assert the result stays inside the repo root.
            let canonical = local_path.canonicalize().with_context(|| {
                format!(
                    "failed to canonicalize {label} glob match: {}",
                    local_path.display()
                )
            })?;
            if !canonical.starts_with(canonical_root) {
                bail!(
                    "{label} glob match escapes repository root via symlink: \
                     resolved path '{}' is outside root '{}'",
                    canonical.display(),
                    canonical_root.display()
                );
            }
            let relative_path = local_path
                .strip_prefix(&fixed_prefix_root)
                .with_context(|| {
                    format!(
                        "{label} glob '{pattern_str}' produced '{}' outside \
                         fixed prefix '{}'",
                        local_path.display(),
                        fixed_prefix_root.display()
                    )
                })?
                .to_path_buf();
            files.push(ResolvedFile {
                local_path: canonical,
                relative_path,
            });
        }

        if files.is_empty() {
            bail!(
                "no files matched {label} glob '{pattern_str}' under {}",
                root.display()
            );
        }

        Ok(files)
    } else {
        // Fully-literal path: single-file resolution with existence + symlink check.
        let full_path = root.join(pattern);
        let local_path = resolve_existing_file(full_path, label, canonical_root)?;
        let relative_path = local_path
            .file_name()
            .map(PathBuf::from)
            .unwrap_or_else(|| local_path.clone());
        Ok(vec![ResolvedFile {
            local_path,
            relative_path,
        }])
    }
}

#[cfg(test)]
mod tests {
    use super::super::{ResolveFileContext, ResolvedFile, ARTIFACT_DIR};
    use crate::resolver::Reference;
    use std::path::PathBuf;
    use tempfile::TempDir;

    fn resolve_context<'a>(
        repo_root: &'a std::path::Path,
        manifest_path: &'a std::path::Path,
    ) -> ResolveFileContext<'a> {
        ResolveFileContext {
            repo_root,
            manifest_path,
            cache_dir_override: None,
        }
    }

    // ── Symlink-escape tests ──────────────────────────────────────────────────

    #[test]
    fn resolve_to_file_rejects_artifact_symlink_escaping_root() {
        let tmp = TempDir::new().unwrap();
        let manifest = tmp.path().join("shasset.yaml");
        // Create a target file outside the repo root.
        let outside = TempDir::new().unwrap();
        let target = outside.path().join("secret.qcow2");
        std::fs::write(&target, "not-in-repo").unwrap();
        // Plant a symlink inside build/artifact that points out of the root.
        let artifact_dir = tmp.path().join(ARTIFACT_DIR);
        std::fs::create_dir_all(&artifact_dir).unwrap();
        std::os::unix::fs::symlink(&target, artifact_dir.join("escape.qcow2")).unwrap();

        let err = Reference::Artifact {
            path: Some(PathBuf::from("escape.qcow2")),
        }
        .resolve_to_file(&resolve_context(tmp.path(), &manifest))
        .unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("escapes") || msg.contains("outside"),
            "symlink escaping artifact root should be rejected: {msg}"
        );
    }

    #[test]
    fn resolve_to_file_rejects_repo_symlink_escaping_root() {
        let tmp = TempDir::new().unwrap();
        let manifest = tmp.path().join("shasset.yaml");
        // Create a target file outside the repo root.
        let outside = TempDir::new().unwrap();
        let target = outside.path().join("secret.qcow2");
        std::fs::write(&target, "not-in-repo").unwrap();
        // Plant a symlink directly in the repo root that points outside.
        std::os::unix::fs::symlink(&target, tmp.path().join("escape.qcow2")).unwrap();

        let err = Reference::Repo {
            path: Some(PathBuf::from("escape.qcow2")),
        }
        .resolve_to_file(&resolve_context(tmp.path(), &manifest))
        .unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("escapes") || msg.contains("outside"),
            "symlink escaping repo root should be rejected: {msg}"
        );
    }

    // ── Glob expansion tests ──────────────────────────────────────────────────

    #[test]
    fn resolve_to_files_repo_literal_path_returns_single_entry() {
        let tmp = TempDir::new().unwrap();
        let manifest = tmp.path().join("shasset.yaml");
        let file = tmp.path().join("some/dir/file.yaml");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, "content").unwrap();

        let files = Reference::Repo {
            path: Some(PathBuf::from("some/dir/file.yaml")),
        }
        .resolve_to_files(&resolve_context(tmp.path(), &manifest))
        .unwrap();

        assert_eq!(files.len(), 1);
        assert_eq!(files[0].local_path, file);
        assert_eq!(files[0].relative_path, PathBuf::from("file.yaml"));
    }

    #[test]
    fn resolve_to_files_artifact_literal_path_returns_single_entry() {
        let tmp = TempDir::new().unwrap();
        let manifest = tmp.path().join("shasset.yaml");
        let file = tmp.path().join(ARTIFACT_DIR).join("foo.qcow2");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, "qcow2").unwrap();

        let files = Reference::Artifact {
            path: Some(PathBuf::from("foo.qcow2")),
        }
        .resolve_to_files(&resolve_context(tmp.path(), &manifest))
        .unwrap();

        assert_eq!(files.len(), 1);
        assert_eq!(files[0].local_path, file);
        assert_eq!(files[0].relative_path, PathBuf::from("foo.qcow2"));
    }

    #[test]
    fn resolve_to_files_repo_glob_returns_matching_files_with_relative_paths() {
        let tmp = TempDir::new().unwrap();
        let manifest = tmp.path().join("shasset.yaml");
        let ecds = tmp.path().join("images/botspace/envoy/ecds");
        std::fs::create_dir_all(&ecds).unwrap();
        let file = ecds.join("ext_authz.yaml");
        std::fs::write(&file, "kind: envoy\n").unwrap();

        let files = Reference::Repo {
            path: Some(PathBuf::from("images/botspace/envoy/**/*.yaml")),
        }
        .resolve_to_files(&resolve_context(tmp.path(), &manifest))
        .unwrap();

        assert_eq!(files.len(), 1);
        assert_eq!(files[0].local_path, file);
        assert_eq!(files[0].relative_path, PathBuf::from("ecds/ext_authz.yaml"));
    }

    #[test]
    fn resolve_to_files_artifact_glob_returns_matching_files_with_relative_paths() {
        let tmp = TempDir::new().unwrap();
        let manifest = tmp.path().join("shasset.yaml");
        let artifact_dir = tmp.path().join(ARTIFACT_DIR);
        let subdir = artifact_dir.join("images/payload");
        std::fs::create_dir_all(&subdir).unwrap();
        let file = subdir.join("mcp-fs.tar");
        std::fs::write(&file, "tarball").unwrap();

        let files = Reference::Artifact {
            path: Some(PathBuf::from("images/**/*.tar")),
        }
        .resolve_to_files(&resolve_context(tmp.path(), &manifest))
        .unwrap();

        assert_eq!(files.len(), 1);
        assert_eq!(files[0].local_path, file);
        assert_eq!(files[0].relative_path, PathBuf::from("payload/mcp-fs.tar"));
    }

    #[test]
    fn resolve_to_files_repo_double_star_glob_matches_entire_tree() {
        let tmp = TempDir::new().unwrap();
        let manifest = tmp.path().join("shasset.yaml");
        let a = tmp.path().join("a/b/c.txt");
        let d = tmp.path().join("a/d.txt");
        std::fs::create_dir_all(a.parent().unwrap()).unwrap();
        std::fs::write(&a, "c").unwrap();
        std::fs::write(&d, "d").unwrap();

        // Use `**/*` (not `**` alone) because the Rust glob crate requires a
        // trailing component after `**` to yield regular files.
        let mut files = Reference::Repo {
            path: Some(PathBuf::from("a/**/*")),
        }
        .resolve_to_files(&resolve_context(tmp.path(), &manifest))
        .unwrap();
        files.sort_by(|x, y| x.relative_path.cmp(&y.relative_path));

        assert_eq!(
            files,
            vec![
                ResolvedFile {
                    local_path: a,
                    relative_path: PathBuf::from("b/c.txt"),
                },
                ResolvedFile {
                    local_path: d,
                    relative_path: PathBuf::from("d.txt"),
                },
            ]
        );
    }

    #[test]
    fn resolve_to_files_glob_zero_matches_is_hard_error() {
        let tmp = TempDir::new().unwrap();
        let manifest = tmp.path().join("shasset.yaml");
        let err = Reference::Repo {
            path: Some(PathBuf::from("images/**/*.yaml")),
        }
        .resolve_to_files(&resolve_context(tmp.path(), &manifest))
        .unwrap_err();
        assert!(
            format!("{err:#}").contains("no files matched"),
            "zero glob matches should be a hard error: {err:#}"
        );
    }

    #[test]
    fn resolve_to_files_glob_skips_directories() {
        let tmp = TempDir::new().unwrap();
        let manifest = tmp.path().join("shasset.yaml");
        // Create a directory that matches the glob but no regular files.
        std::fs::create_dir_all(tmp.path().join("images/botspace/envoy/ecds")).unwrap();
        let err = Reference::Repo {
            path: Some(PathBuf::from("images/botspace/envoy/**")),
        }
        .resolve_to_files(&resolve_context(tmp.path(), &manifest))
        .unwrap_err();
        assert!(
            format!("{err:#}").contains("no files matched"),
            "glob matching only directories should be an error: {err:#}"
        );
    }

    #[test]
    fn resolve_to_files_bare_repo_root_is_error() {
        let tmp = TempDir::new().unwrap();
        let manifest = tmp.path().join("shasset.yaml");
        let err = Reference::Repo { path: None }
            .resolve_to_files(&resolve_context(tmp.path(), &manifest))
            .unwrap_err();
        assert!(
            format!("{err:#}").contains("path or glob is required"),
            "bare repo root should be rejected: {err:#}"
        );
    }

    #[test]
    fn resolve_to_files_bare_artifact_root_is_error() {
        let tmp = TempDir::new().unwrap();
        let manifest = tmp.path().join("shasset.yaml");
        let err = Reference::Artifact { path: None }
            .resolve_to_files(&resolve_context(tmp.path(), &manifest))
            .unwrap_err();
        assert!(
            format!("{err:#}").contains("path or glob is required"),
            "bare artifact root should be rejected: {err:#}"
        );
    }

    #[test]
    fn resolve_to_files_asset_traversal_remains_unsupported() {
        let tmp = TempDir::new().unwrap();
        let manifest = tmp.path().join("shasset.yaml");
        let err = Reference::Asset {
            name: "tool".to_string(),
            path: Some(PathBuf::from("bin/**")),
        }
        .resolve_to_files(&resolve_context(tmp.path(), &manifest))
        .unwrap_err();
        assert!(
            format!("{err:#}").contains("not yet supported"),
            "asset archive traversal should remain unsupported: {err:#}"
        );
    }

    #[test]
    fn resolve_to_files_glob_symlink_escape_is_rejected() {
        let tmp = TempDir::new().unwrap();
        let manifest = tmp.path().join("shasset.yaml");
        // Create a target file outside the repo root.
        let outside = TempDir::new().unwrap();
        let target = outside.path().join("secret.txt");
        std::fs::write(&target, "secret").unwrap();
        // Plant a symlink inside the repo that points outside.
        let images = tmp.path().join("images");
        std::fs::create_dir_all(&images).unwrap();
        std::os::unix::fs::symlink(&target, images.join("escape.txt")).unwrap();

        let err = Reference::Repo {
            path: Some(PathBuf::from("images/*.txt")),
        }
        .resolve_to_files(&resolve_context(tmp.path(), &manifest))
        .unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("escapes") || msg.contains("outside"),
            "symlink escaping root should be rejected: {msg}"
        );
    }
}
