//! Botforge reference resolver — single owner of the `@` reference grammar.
//!
//! Every asset/path reference in botforge YAML documents uses the same grammar:
//!
//! ```text
//! <ref>   ::= "@" <root> ("://" <path>)?
//! <root>  ::= ""          -- this repo (checked-in source)
//!           | "artifact"  -- reserved; repo artifact directory
//!           | <name>      -- a pinned shasset asset name (everything else)
//! <path>  ::= <non-empty repo-relative path, optionally containing glob metacharacters>
//! ```
//!
//! The three roots differ by **resolution strategy**, not by what files they name:
//!
//! | Reference              | Root     | Strategy                                             |
//! |------------------------|----------|------------------------------------------------------|
//! | `@`                    | Repo     | repo root directory                                  |
//! | `@://path`             | Repo     | repo root + path (single file)                      |
//! | `@://<glob>`           | Repo     | glob expansion under repo root (multi-file)          |
//! | `@foo`                 | Asset    | manifest lookup → fetch + verify → blob             |
//! | `@foo://path`          | Asset    | fetch → traverse into archive (deferred)            |
//! | `@artifact`            | Artifact | `build/artifact/` directory                         |
//! | `@artifact://path`     | Artifact | `build/artifact/` + path, single file               |
//! | `@artifact://<glob>`   | Artifact | glob expansion under `build/artifact/` (multi-file) |
//!
//! Glob metacharacters (`*`, `**`, `?`, `[…]`) are only meaningful in the
//! `://path` segment of `@` (repo) and `@artifact` references.  Glob paths
//! are legal wherever literal paths are; resolution (not parsing) decides
//! whether to do a single-file or multi-file lookup.

use anyhow::{bail, Context, Result};
use glob::MatchOptions;
use serde::Deserialize;
use shasset::fetch::{fetch_asset, FetchParams, MaterializeMode, Transport};
use shasset::manifest::load;
use std::path::{Component, Path, PathBuf};

use crate::util::{default_cache_dir, materialize_flat};

pub(crate) const ARTIFACT_DIR: &str = "build/artifact";

/// Context required to resolve a parsed [`Reference`] to a concrete local file or files.
pub(crate) struct ResolveFileContext<'a> {
    pub(crate) repo_root: &'a Path,
    pub(crate) manifest_path: &'a Path,
    pub(crate) cache_dir_override: Option<&'a Path>,
}

/// A single file produced by [`Reference::resolve_to_files`].
///
/// Both paths are absolute for `local_path` and are relative for
/// `relative_path`.
///
/// For glob expansions the `relative_path` is the path relative to the
/// pattern's fixed literal prefix (directory components before the first
/// metacharacter).  For single-file references it is the file's base name.
/// This relative path is what callers use to reconstruct a destination tree
/// when staging files to a guest.
#[allow(dead_code)]
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ResolvedFile {
    /// Absolute local path on the host.
    pub(crate) local_path: PathBuf,
    /// Path relative to the glob's fixed-literal prefix (or the base name for
    /// non-glob references).
    pub(crate) relative_path: PathBuf,
}

// ── Reference ────────────────────────────────────────────────────────────────

/// A parsed botforge reference string.
///
/// Parsing is pure (no I/O).
#[derive(Debug, PartialEq, Clone)]
pub(crate) enum Reference {
    /// `@` or `@://path` — a path inside this repo's checked-in source tree.
    Repo { path: Option<PathBuf> },

    /// `@<name>` or `@<name>://path` — a pinned shasset asset.
    ///
    /// The `path` form requires the asset to carry `archive: true` in the
    /// shasset manifest; resolution errors out if that marker is absent.
    Asset { name: String, path: Option<PathBuf> },

    /// `@artifact` or `@artifact://path` — the repo's build artifact directory.
    ///
    /// This root is position-addressed, not content-addressed.  Resolution
    /// performs an existence check only; it never fetches or verifies.
    ///
    /// `@artifact` is intentionally used as both:
    /// - the deterministic output target for `botforge build`, and
    /// - an input reference to prebuilt artifacts staged into `build/artifact/`.
    Artifact { path: Option<PathBuf> },
}

impl<'de> Deserialize<'de> for Reference {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        Reference::parse(&raw).map_err(serde::de::Error::custom)
    }
}

impl Reference {
    /// Parse a raw reference string into a typed [`Reference`].
    ///
    /// # Parsing rules (all hard/parse-time errors)
    ///
    /// - Must start with `@`; bare names without `@` are rejected.
    /// - `artifact` is a reserved root keyword; every other non-empty token after
    ///   `@` is treated as a shasset asset name.
    /// - Any `://path` segment must be repo-relative: non-empty, not absolute,
    ///   and free of `.` / `..` components.
    /// - Bare `@` and bare `@artifact` are valid and resolve to their root
    ///   **directories** (symmetric with the `://path` traversal form).
    pub(crate) fn parse(raw: &str) -> Result<Self> {
        let rest = raw.strip_prefix('@').ok_or_else(|| {
            anyhow::anyhow!("reference must start with `@`; bare names are not supported: {raw:?}")
        })?;

        if let Some((root_token, path_str)) = rest.split_once("://") {
            // Traversal form: @<root>://<path>
            let path = PathBuf::from(path_str);
            validate_ref_path(&path)?;
            match root_token {
                "" => Ok(Reference::Repo { path: Some(path) }),
                "artifact" => Ok(Reference::Artifact { path: Some(path) }),
                name => Ok(Reference::Asset {
                    name: name.to_string(),
                    path: Some(path),
                }),
            }
        } else {
            // Simple form: @<root>
            match rest {
                "" => Ok(Reference::Repo { path: None }),
                "artifact" => Ok(Reference::Artifact { path: None }),
                name => Ok(Reference::Asset {
                    name: name.to_string(),
                    path: None,
                }),
            }
        }
    }

    /// Resolve this parsed reference to a concrete local file path ready for consumption.
    pub(crate) fn resolve_to_file(&self, context: &ResolveFileContext<'_>) -> Result<PathBuf> {
        self.resolve_to_file_with_transport(context, || None)
    }

    /// Resolve this reference to a local file path for use as a **base image** (qcow2 slot).
    ///
    /// Identical to [`resolve_to_file`] for all reference kinds except bare asset references
    /// (`@<name>`): for those, this helper additionally enforces the base-image contract that
    /// the asset must **not** be an `oci://` image.  `oci://` assets are valid upload sources
    /// but cannot be booted directly as a qcow2 base image.
    ///
    /// Both `botforge build` (the `image:` slot) and `botforge test` (the `--base-image`/
    /// `image:` slot) use this helper so the contract lives in exactly one place.
    pub(crate) fn resolve_base_image_to_file(
        &self,
        context: &ResolveFileContext<'_>,
    ) -> Result<PathBuf> {
        if let Reference::Asset { name, path: None } = self {
            let manifest = load(context.manifest_path).with_context(|| {
                format!(
                    "cannot load shasset manifest: {}",
                    context.manifest_path.display()
                )
            })?;
            if let Some(asset) = manifest.assets.get(name) {
                let uri = asset.expanded_uri();
                if uri.starts_with("oci://") {
                    bail!(
                        "base image asset '{name}' is an oci:// image; \
                         image must resolve to a qcow2 file asset"
                    );
                }
            }
        }
        self.resolve_to_file(context)
    }

    fn resolve_to_file_with_transport<F>(
        &self,
        context: &ResolveFileContext<'_>,
        mut transport_factory: F,
    ) -> Result<PathBuf>
    where
        F: FnMut() -> Option<Box<dyn Transport>>,
    {
        // Pre-canonicalize the repo root once so containment checks in
        // resolve_existing_file can detect symlink escapes at resolve time.
        let canonical_root = context.repo_root.canonicalize().with_context(|| {
            format!(
                "cannot canonicalize repo root: {}",
                context.repo_root.display()
            )
        })?;

        match self {
            Reference::Asset { name, path: None } => {
                let manifest = load(context.manifest_path).with_context(|| {
                    format!(
                        "cannot load shasset manifest: {}",
                        context.manifest_path.display()
                    )
                })?;
                let cache_dir = context
                    .cache_dir_override
                    .map(Path::to_path_buf)
                    .unwrap_or_else(default_cache_dir);

                let asset = manifest.assets.get(name).with_context(|| {
                    format!(
                        "asset '{name}' not found in manifest {}",
                        context.manifest_path.display()
                    )
                })?;

                if asset.checksum.is_none() {
                    eprintln!(
                        "warning: image asset '{name}' has no checksum; \
                         integrity will not be verified"
                    );
                }

                let fetched = fetch_asset(FetchParams {
                    name,
                    asset,
                    out_dir: None,
                    cache_dir: &cache_dir,
                    retries: manifest.settings.retries,
                    backoff: &manifest.settings.backoff,
                    compute_checksum: true,
                    no_reverify: false,
                    materialize_mode: MaterializeMode::Copy,
                    transport: transport_factory(),
                })
                .with_context(|| format!("failed to fetch image asset '{name}'"))?;

                let filename = asset.output_filename().with_context(|| {
                    format!("image asset '{name}': cannot determine output filename")
                })?;
                let out_dir = cache_dir.join("base-images");
                materialize_flat(&fetched.blob_path, &out_dir, &filename, false)
                    .with_context(|| format!("failed to stage image asset '{name}'"))
            }
            Reference::Repo { path: Some(path) } => {
                resolve_existing_file(context.repo_root.join(path), "repo image reference", &canonical_root)
            }
            Reference::Artifact { path: Some(path) } => resolve_existing_file(
                context.repo_root.join(ARTIFACT_DIR).join(path),
                "artifact image reference",
                &canonical_root,
            ),
            Reference::Asset { name, path: Some(path) } => bail!(
                "image reference '@{name}://{}' requires archive traversal, which is not yet supported",
                path.display()
            ),
            Reference::Repo { path: None } => bail!(
                "image reference '@' resolves to the repository root directory; a base image must resolve to a file"
            ),
            Reference::Artifact { path: None } => bail!(
                "image reference '@artifact' resolves to the artifact root directory; a base image must resolve to a file"
            ),
        }
    }
}

#[allow(dead_code)]
impl Reference {
    /// Resolve this reference to a collection of local file paths.
    ///
    /// This is the multi-file counterpart to [`resolve_to_file`].
    ///
    /// For `@://<glob>` and `@artifact://<glob>` references whose path
    /// contains glob metacharacters (`*`, `**`, `?`, `[…]`), every matching
    /// regular file is returned together with a [`ResolvedFile::relative_path`]
    /// computed by stripping the pattern's fixed literal prefix, preserving
    /// the sub-tree structure underneath that prefix.
    ///
    /// For single-file references (no glob metacharacters, or bare `@name`
    /// asset blobs) a one-element [`Vec`] is returned so call sites can use
    /// this API uniformly.
    ///
    /// # Errors
    ///
    /// - Zero glob matches → hard error.
    /// - Only regular files are included; directories silently skipped.
    /// - `@<name>://<glob>` (shasset archive traversal) is not yet supported.
    /// - Bare `@` and `@artifact` (no path) are errors.
    pub(crate) fn resolve_to_files(
        &self,
        context: &ResolveFileContext<'_>,
    ) -> Result<Vec<ResolvedFile>> {
        self.resolve_to_files_with_transport(context, || None)
    }

    fn resolve_to_files_with_transport<F>(
        &self,
        context: &ResolveFileContext<'_>,
        transport_factory: F,
    ) -> Result<Vec<ResolvedFile>>
    where
        F: FnMut() -> Option<Box<dyn Transport>>,
    {
        match self {
            // Single-blob shasset asset: delegate to the existing single-file
            // resolver and wrap the result.
            Reference::Asset {
                name: _,
                path: None,
            } => {
                let local_path = self.resolve_to_file_with_transport(context, transport_factory)?;
                let relative_path = local_path
                    .file_name()
                    .map(PathBuf::from)
                    .unwrap_or_else(|| local_path.clone());
                Ok(vec![ResolvedFile {
                    local_path,
                    relative_path,
                }])
            }
            Reference::Asset {
                name,
                path: Some(path),
            } => bail!(
                "reference '@{name}://{}' requires archive traversal, \
                 which is not yet supported",
                path.display()
            ),
            Reference::Repo { path: None } => bail!(
                "reference '@' resolves to the repository root directory; \
                 a path or glob is required to expand to files"
            ),
            Reference::Artifact { path: None } => bail!(
                "reference '@artifact' resolves to the artifact root directory; \
                 a path or glob is required to expand to files"
            ),
            Reference::Repo { path: Some(path) } => {
                let canonical_root = context.repo_root.canonicalize().with_context(|| {
                    format!(
                        "cannot canonicalize repo root: {}",
                        context.repo_root.display()
                    )
                })?;
                resolve_ref_path_to_files(
                    context.repo_root,
                    path,
                    "repo reference",
                    &canonical_root,
                )
            }
            Reference::Artifact { path: Some(path) } => {
                let canonical_root = context.repo_root.canonicalize().with_context(|| {
                    format!(
                        "cannot canonicalize repo root: {}",
                        context.repo_root.display()
                    )
                })?;
                let artifact_root = context.repo_root.join(ARTIFACT_DIR);
                resolve_ref_path_to_files(
                    &artifact_root,
                    path,
                    "artifact reference",
                    &canonical_root,
                )
            }
        }
    }
}

fn resolve_existing_file(path: PathBuf, label: &str, canonical_root: &Path) -> Result<PathBuf> {
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

/// Validate that a `://`-suffix path component is repo-relative.
///
/// Rules: non-empty, not absolute, no `.` or `..` segments.
pub(crate) fn validate_ref_path(path: &Path) -> Result<()> {
    if path.as_os_str().is_empty() {
        bail!("reference path must not be empty after `://`");
    }
    if path.is_absolute() {
        bail!(
            "reference path must be repo-relative, got: {}",
            path.display()
        );
    }
    for component in path.components() {
        match component {
            Component::Normal(_) => {}
            _ => bail!(
                "reference path must contain no '.' or '..' segments: {}",
                path.display()
            ),
        }
    }
    Ok(())
}

// ── Glob helpers ──────────────────────────────────────────────────────────────

/// Returns `true` if `s` contains any glob metacharacter (`*`, `?`, `[`).
#[allow(dead_code)]
fn has_glob_metacharacters(s: &str) -> bool {
    s.contains('*') || s.contains('?') || s.contains('[')
}

/// Returns the fixed literal prefix of a glob pattern: all leading path
/// components that contain no metacharacters.
///
/// For `images/botspace/**/*.yaml` this yields `images/botspace`.
/// For `**/*.yaml` or a pattern that starts with a wildcard this yields `""`.
/// For a fully-literal path like `images/foo.yaml` this yields
/// `images/foo.yaml` (the entire path).
#[allow(dead_code)]
fn glob_fixed_prefix(pattern: &str) -> PathBuf {
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
/// `canonical_root` must be the pre-canonicalized `root`'s repo root used
/// for symlink-escape containment checks.
#[allow(dead_code)]
fn resolve_ref_path_to_files(
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
    use super::{Reference, ResolveFileContext, ResolvedFile, ARTIFACT_DIR};
    use shasset::fetch::{DownloadResponse, FetchError, Transport};
    use std::io::Cursor;
    use std::path::PathBuf;
    use tempfile::TempDir;

    struct MockTransport {
        expected_uri: String,
        body: Vec<u8>,
    }

    impl Transport for MockTransport {
        fn get(
            &self,
            uri: &str,
            _auth: Option<&str>,
            accept: Option<&str>,
        ) -> std::result::Result<DownloadResponse, FetchError> {
            assert_eq!(uri, self.expected_uri);
            assert!(accept.is_none());
            Ok(DownloadResponse {
                body: Box::new(Cursor::new(self.body.clone())),
                content_length: Some(self.body.len() as u64),
            })
        }
    }

    fn resolve_context<'a>(
        repo_root: &'a std::path::Path,
        manifest_path: &'a std::path::Path,
        cache_dir: Option<&'a std::path::Path>,
    ) -> ResolveFileContext<'a> {
        ResolveFileContext {
            repo_root,
            manifest_path,
            cache_dir_override: cache_dir,
        }
    }

    // ── Reference::parse ──────────────────────────────────────────────────────

    #[test]
    fn parse_bare_at_is_repo_root() {
        assert_eq!(
            Reference::parse("@").unwrap(),
            Reference::Repo { path: None }
        );
    }

    #[test]
    fn parse_at_with_path_is_repo() {
        assert_eq!(
            Reference::parse("@://some/path").unwrap(),
            Reference::Repo {
                path: Some(PathBuf::from("some/path"))
            }
        );
    }

    #[test]
    fn parse_asset_simple() {
        assert_eq!(
            Reference::parse("@debian-base").unwrap(),
            Reference::Asset {
                name: "debian-base".to_string(),
                path: None
            }
        );
    }

    #[test]
    fn parse_asset_with_traversal_path() {
        assert_eq!(
            Reference::parse("@my-tool://bin/tool").unwrap(),
            Reference::Asset {
                name: "my-tool".to_string(),
                path: Some(PathBuf::from("bin/tool"))
            }
        );
    }

    #[test]
    fn parse_artifact_bare() {
        assert_eq!(
            Reference::parse("@artifact").unwrap(),
            Reference::Artifact { path: None }
        );
    }

    #[test]
    fn parse_artifact_with_path() {
        assert_eq!(
            Reference::parse("@artifact://images/vm.qcow2").unwrap(),
            Reference::Artifact {
                path: Some(PathBuf::from("images/vm.qcow2"))
            }
        );
    }

    #[test]
    fn parse_output_is_not_reserved_keyword_anymore() {
        assert_eq!(
            Reference::parse("@output").unwrap(),
            Reference::Asset {
                name: "output".to_string(),
                path: None
            }
        );
    }

    #[test]
    fn parse_bare_name_without_at_is_rejected() {
        let err = Reference::parse("debian-base").unwrap_err();
        assert!(
            err.to_string().contains("must start with `@`"),
            "unexpected error: {err:#}"
        );
    }

    #[test]
    fn parse_empty_string_is_rejected() {
        let err = Reference::parse("").unwrap_err();
        assert!(
            err.to_string().contains("must start with `@`"),
            "unexpected error: {err:#}"
        );
    }

    #[test]
    fn parse_empty_path_after_scheme_is_rejected() {
        for raw in &["@://", "@foo://", "@artifact://"] {
            let err = Reference::parse(raw).unwrap_err();
            assert!(
                err.to_string().contains("must not be empty"),
                "parse({raw:?}) should reject empty path, got: {err:#}"
            );
        }
    }

    #[test]
    fn parse_absolute_path_after_scheme_is_rejected() {
        let err = Reference::parse("@:///absolute").unwrap_err();
        assert!(
            err.to_string().contains("repo-relative"),
            "unexpected error: {err:#}"
        );
    }

    #[test]
    fn parse_dotdot_path_is_rejected() {
        for raw in &["@://shared/../secret", "@://.."] {
            let err = Reference::parse(raw).unwrap_err();
            assert!(
                err.to_string().contains("'.' or '..'"),
                "parse({raw:?}) should reject dotdot path, got: {err:#}"
            );
        }
    }

    #[test]
    fn parse_single_dot_path_is_rejected() {
        let err = Reference::parse("@://.").unwrap_err();
        assert!(
            err.to_string().contains("'.' or '..'"),
            "unexpected error: {err:#}"
        );
    }

    // ── Reference::resolve_to_file ────────────────────────────────────────────

    #[test]
    fn resolve_to_file_fetches_and_materializes_asset_default() {
        let tmp = TempDir::new().unwrap();
        let manifest = tmp.path().join("shasset.yaml");
        let cache = tmp.path().join("cache");
        let body = b"fake-qcow2-content".to_vec();
        let uri = "https://example.com/v1/base.qcow2".to_string();
        let checksum = "34cb20b33d115697e75baf0d12172c7c3b42a5f04b047c64f38d0aa2b57c988f";
        std::fs::write(
            &manifest,
            format!(
                "settings:\n  retries: 0\nassets:\n  debian-base:\n    uri: {uri}\n    version: \"13\"\n    checksum: sha256:{checksum}\n    filename: debian-13.qcow2\n"
            ),
        )
        .unwrap();

        let mut transport = Some(Box::new(MockTransport {
            expected_uri: uri,
            body: body.clone(),
        }) as Box<dyn Transport>);

        let path = Reference::Asset {
            name: "debian-base".to_string(),
            path: None,
        }
        .resolve_to_file_with_transport(
            &resolve_context(tmp.path(), &manifest, Some(&cache)),
            || transport.take(),
        )
        .unwrap();

        assert!(path.exists(), "materialized qcow2 should exist: {path:?}");
        assert_eq!(std::fs::read(&path).unwrap(), body);
        assert_eq!(path.file_name().unwrap(), "debian-13.qcow2");
    }

    #[test]
    fn resolve_to_file_fails_on_unknown_asset() {
        let tmp = TempDir::new().unwrap();
        let manifest = tmp.path().join("shasset.yaml");
        std::fs::write(
            &manifest,
            "settings:\n  retries: 0\nassets:\n  other-asset:\n    uri: https://example.com/img.qcow2\n    version: \"1\"\n",
        )
        .unwrap();
        let err = Reference::Asset {
            name: "nonexistent-key".to_string(),
            path: None,
        }
        .resolve_to_file(&resolve_context(tmp.path(), &manifest, Some(tmp.path())))
        .unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("nonexistent-key") && msg.contains("not found"),
            "error should name the missing key: {msg}"
        );
    }

    #[test]
    fn resolve_to_file_succeeds_on_oci_asset() {
        // The shared resolver no longer enforces the "must be qcow2" contract.
        // Verify that resolve_to_file for an oci:// asset does NOT fail with the
        // base-image/qcow2 contract error — it may fail for other reasons (e.g.
        // no network in tests) but that's a different failure path.
        let tmp = TempDir::new().unwrap();
        let manifest = tmp.path().join("shasset.yaml");
        std::fs::write(
            &manifest,
            "settings:\n  retries: 0\nassets:\n  session-broker:\n    uri: oci://ghcr.io/example/session-broker@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n    version: \"1\"\n",
        )
        .unwrap();
        let err = Reference::Asset {
            name: "session-broker".to_string(),
            path: None,
        }
        .resolve_to_file(&resolve_context(tmp.path(), &manifest, Some(tmp.path())))
        .unwrap_err();
        let msg = format!("{err:#}");
        // Must NOT be the base-image qcow2 contract error.
        assert!(
            !msg.contains("image must resolve to a qcow2 file asset"),
            "resolve_to_file must not enforce the base-image qcow2 contract: {msg}"
        );
    }

    #[test]
    fn resolve_base_image_to_file_rejects_oci_asset() {
        let tmp = TempDir::new().unwrap();
        let manifest = tmp.path().join("shasset.yaml");
        std::fs::write(
            &manifest,
            "settings:\n  retries: 0\nassets:\n  my-image:\n    uri: oci://ghcr.io/example/img@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n    version: \"1\"\n",
        )
        .unwrap();
        let err = Reference::Asset {
            name: "my-image".to_string(),
            path: None,
        }
        .resolve_base_image_to_file(&resolve_context(tmp.path(), &manifest, Some(tmp.path())))
        .unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("oci://") && msg.contains("qcow2"),
            "base-image helper should reject oci:// assets with oci+qcow2 message: {msg}"
        );
    }

    #[test]
    fn resolve_to_files_oci_asset_does_not_hit_qcow2_contract_error() {
        // An upload src like `@session-broker` (oci:// asset) must not fail with the
        // "image must resolve to a qcow2 file asset" message.  It may still fail (e.g.
        // fetch error) but not because of the base-image contract.
        let tmp = TempDir::new().unwrap();
        let manifest = tmp.path().join("shasset.yaml");
        std::fs::write(
            &manifest,
            "settings:\n  retries: 0\nassets:\n  session-broker:\n    uri: oci://ghcr.io/example/session-broker@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n    version: \"1\"\n",
        )
        .unwrap();
        let err = Reference::Asset {
            name: "session-broker".to_string(),
            path: None,
        }
        .resolve_to_files(&resolve_context(tmp.path(), &manifest, Some(tmp.path())))
        .unwrap_err();
        let msg = format!("{err:#}");
        // Must NOT be the base-image/qcow2 contract error.
        assert!(
            !msg.contains("image must resolve to a qcow2 file asset"),
            "resolve_to_files must not enforce the qcow2 base-image contract: {msg}"
        );
    }

    #[test]
    fn resolve_to_file_returns_repo_file() {
        let tmp = TempDir::new().unwrap();
        let manifest = tmp.path().join("shasset.yaml");
        let file = tmp.path().join("build/artifact/base.qcow2");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, "qcow2").unwrap();

        let path = Reference::Repo {
            path: Some(PathBuf::from("build/artifact/base.qcow2")),
        }
        .resolve_to_file(&resolve_context(tmp.path(), &manifest, None))
        .unwrap();

        assert_eq!(path, file);
    }

    #[test]
    fn resolve_to_file_rejects_missing_repo_file() {
        let tmp = TempDir::new().unwrap();
        let manifest = tmp.path().join("shasset.yaml");
        let err = Reference::Repo {
            path: Some(PathBuf::from("missing.qcow2")),
        }
        .resolve_to_file(&resolve_context(tmp.path(), &manifest, None))
        .unwrap_err();
        assert!(
            format!("{err:#}").contains("not found"),
            "missing repo file should be rejected: {err:#}"
        );
    }

    #[test]
    fn resolve_to_file_returns_artifact_file() {
        let tmp = TempDir::new().unwrap();
        let manifest = tmp.path().join("shasset.yaml");
        let file = tmp.path().join(ARTIFACT_DIR).join("foo.qcow2");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, "qcow2").unwrap();

        let path = Reference::Artifact {
            path: Some(PathBuf::from("foo.qcow2")),
        }
        .resolve_to_file(&resolve_context(tmp.path(), &manifest, None))
        .unwrap();

        assert_eq!(path, file);
    }

    #[test]
    fn resolve_to_file_rejects_missing_artifact_file() {
        let tmp = TempDir::new().unwrap();
        let manifest = tmp.path().join("shasset.yaml");
        let err = Reference::Artifact {
            path: Some(PathBuf::from("foo.qcow2")),
        }
        .resolve_to_file(&resolve_context(tmp.path(), &manifest, None))
        .unwrap_err();
        assert!(
            format!("{err:#}").contains("not found"),
            "missing artifact file should be rejected: {err:#}"
        );
    }

    #[test]
    fn resolve_to_file_rejects_directory_roots() {
        let tmp = TempDir::new().unwrap();
        let manifest = tmp.path().join("shasset.yaml");

        let repo_err = Reference::Repo { path: None }
            .resolve_to_file(&resolve_context(tmp.path(), &manifest, None))
            .unwrap_err();
        assert!(
            format!("{repo_err:#}").contains("must resolve to a file"),
            "repo root should be rejected: {repo_err:#}"
        );

        let artifact_err = Reference::Artifact { path: None }
            .resolve_to_file(&resolve_context(tmp.path(), &manifest, None))
            .unwrap_err();
        assert!(
            format!("{artifact_err:#}").contains("must resolve to a file"),
            "artifact root should be rejected: {artifact_err:#}"
        );
    }

    #[test]
    fn resolve_to_file_rejects_unsupported_asset_traversal() {
        let tmp = TempDir::new().unwrap();
        let manifest = tmp.path().join("shasset.yaml");
        let err = Reference::Asset {
            name: "tool".to_string(),
            path: Some(PathBuf::from("bin/tool")),
        }
        .resolve_to_file(&resolve_context(tmp.path(), &manifest, None))
        .unwrap_err();
        assert!(
            format!("{err:#}").contains("not yet supported"),
            "asset traversal should remain unsupported: {err:#}"
        );
    }

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
        .resolve_to_file(&resolve_context(tmp.path(), &manifest, None))
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
        .resolve_to_file(&resolve_context(tmp.path(), &manifest, None))
        .unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("escapes") || msg.contains("outside"),
            "symlink escaping repo root should be rejected: {msg}"
        );
    }

    // ── Reference::resolve_to_files ───────────────────────────────────────────

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
        .resolve_to_files(&resolve_context(tmp.path(), &manifest, None))
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
        .resolve_to_files(&resolve_context(tmp.path(), &manifest, None))
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
        .resolve_to_files(&resolve_context(tmp.path(), &manifest, None))
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
        .resolve_to_files(&resolve_context(tmp.path(), &manifest, None))
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
        .resolve_to_files(&resolve_context(tmp.path(), &manifest, None))
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
        .resolve_to_files(&resolve_context(tmp.path(), &manifest, None))
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
        .resolve_to_files(&resolve_context(tmp.path(), &manifest, None))
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
            .resolve_to_files(&resolve_context(tmp.path(), &manifest, None))
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
            .resolve_to_files(&resolve_context(tmp.path(), &manifest, None))
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
        .resolve_to_files(&resolve_context(tmp.path(), &manifest, None))
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
        .resolve_to_files(&resolve_context(tmp.path(), &manifest, None))
        .unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("escapes") || msg.contains("outside"),
            "symlink escaping root should be rejected: {msg}"
        );
    }
}
