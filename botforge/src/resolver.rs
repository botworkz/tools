//! Botforge reference resolver — single owner of the `@` reference grammar.
//!
//! Every asset/path reference in botforge YAML documents uses the same grammar:
//!
//! ```text
//! <ref>   ::= "@" <root> ("://" <path>)?
//! <root>  ::= ""          -- this repo (checked-in source)
//!           | "artifact"  -- reserved; repo artifact directory
//!           | <name>      -- a pinned shasset asset name (everything else)
//! <path>  ::= <non-empty repo-relative path with no "." or ".." segments>
//! ```
//!
//! The three roots differ by **resolution strategy**, not by what files they name:
//!
//! | Reference        | Root   | Strategy                                   |
//! |------------------|--------|--------------------------------------------|
//! | `@`              | Repo   | repo root directory                        |
//! | `@://path`       | Repo   | repo root + path (file or dir)             |
//! | `@foo`           | Asset  | manifest lookup → fetch + verify → blob   |
//! | `@foo://path`    | Asset  | fetch → traverse into archive (deferred)  |
//! | `@artifact`        | Artifact | `build/artifact/` directory                 |
//! | `@artifact://path` | Artifact | `build/artifact/` + path, existence check   |

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use shasset::fetch::{fetch_asset, FetchParams, MaterializeMode, Transport};
use shasset::manifest::load;
use std::path::{Component, Path, PathBuf};

use crate::util::{default_cache_dir, materialize_flat};

pub(crate) const ARTIFACT_DIR: &str = "build/artifact";

/// Context required to resolve a parsed [`Reference`] to a concrete local file.
pub(crate) struct ResolveFileContext<'a> {
    pub(crate) repo_root: &'a Path,
    pub(crate) manifest_path: &'a Path,
    pub(crate) cache_dir_override: Option<&'a Path>,
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

    fn resolve_to_file_with_transport<F>(
        &self,
        context: &ResolveFileContext<'_>,
        mut transport_factory: F,
    ) -> Result<PathBuf>
    where
        F: FnMut() -> Option<Box<dyn Transport>>,
    {
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

                let uri = asset.expanded_uri();
                if uri.starts_with("oci://") {
                    bail!(
                        "image asset '{name}' is an oci:// image; \
                         image must resolve to a qcow2 file asset"
                    );
                }
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
                resolve_existing_file(context.repo_root.join(path), "repo image reference")
            }
            Reference::Artifact { path: Some(path) } => resolve_existing_file(
                context.repo_root.join(ARTIFACT_DIR).join(path),
                "artifact image reference",
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

fn resolve_existing_file(path: PathBuf, label: &str) -> Result<PathBuf> {
    if !path.exists() {
        bail!("{label} not found: {}", path.display());
    }
    if !path.is_file() {
        bail!("{label} must resolve to a file: {}", path.display());
    }
    Ok(path)
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

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::{Reference, ResolveFileContext, ARTIFACT_DIR};
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
    fn resolve_to_file_fails_on_oci_asset() {
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
        .resolve_to_file(&resolve_context(tmp.path(), &manifest, Some(tmp.path())))
        .unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("oci://") || msg.contains("qcow2"),
            "error should mention oci or qcow2: {msg}"
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
}
