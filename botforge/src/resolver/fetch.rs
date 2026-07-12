//! Asset fetch arm: manifest lookup → pre-fetch validation → fetch_asset → materialize.
//!
//! This module owns the shasset-facing I/O for resolving `Reference::Asset`
//! variants.  Grammar parsing and path helpers live in sibling modules.

use anyhow::{Context, Result};
use shasset::fetch::{fetch_asset, FetchParams, MaterializeMode, Transport};
use shasset::manifest::load;
use std::path::{Path, PathBuf};

use crate::util::{default_cache_dir, materialize_flat};

use super::validate::{check_asset_kind, check_asset_labels, ResolveSpec};
use super::ResolveFileContext;

/// Resolve a bare shasset asset reference (`@<name>`) to a local file path.
///
/// Performs manifest lookup, pre-fetch validation (kind + labels from `spec`),
/// download + verification, and materialization into the cache.
///
/// The `transport_factory` closure is called at most once to inject a mock
/// transport for tests; pass `|| None` in production to use the default HTTP
/// transport.
pub(super) fn fetch_asset_blob<F>(
    name: &str,
    context: &ResolveFileContext<'_>,
    spec: &ResolveSpec,
    transport_factory: &mut F,
) -> Result<PathBuf>
where
    F: FnMut() -> Option<Box<dyn Transport>>,
{
    let manifest = load(context.manifest_path).with_context(|| {
        format!(
            "cannot load shasset manifest: {}",
            context.manifest_path.display()
        )
    })?;
    let cache_dir: PathBuf = context
        .cache_dir_override
        .map(Path::to_path_buf)
        .unwrap_or_else(default_cache_dir);

    let asset = manifest.assets.get(name).with_context(|| {
        format!(
            "asset '{name}' not found in manifest {}",
            context.manifest_path.display()
        )
    })?;

    // Pre-fetch validation: kind and labels (both read from the manifest, no I/O).
    let uri = asset.expanded_uri();
    check_asset_kind(name, &uri, spec)?;
    check_asset_labels(name, &asset.labels, spec)?;

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

    let filename = asset
        .output_filename()
        .with_context(|| format!("image asset '{name}': cannot determine output filename"))?;
    let out_dir = cache_dir.join("base-images");
    materialize_flat(&fetched.blob_path, &out_dir, &filename, false)
        .with_context(|| format!("failed to stage image asset '{name}'"))
}

#[cfg(test)]
mod tests {
    use super::super::ResolveFileContext;
    use super::*;
    use crate::resolver::validate::AssetKind;
    use crate::resolver::Reference;
    use shasset::fetch::{DownloadResponse, FetchError, Transport};
    use std::io::Cursor;
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

    // ── fetch tests ───────────────────────────────────────────────────────────

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

        let spec = ResolveSpec::default();
        let path = Reference::Asset {
            name: "debian-base".to_string(),
            path: None,
        }
        .resolve_validated_with_transport(
            &resolve_context(tmp.path(), &manifest, Some(&cache)),
            &spec,
            || transport.take(),
        )
        .unwrap();

        assert_eq!(path.len(), 1, "should resolve exactly one file");
        let resolved = &path[0];
        assert!(
            resolved.local_path.exists(),
            "materialized qcow2 should exist: {:?}",
            resolved.local_path
        );
        assert_eq!(std::fs::read(&resolved.local_path).unwrap(), body);
        assert_eq!(resolved.local_path.file_name().unwrap(), "debian-13.qcow2");
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
        .resolve_to_file(&resolve_context(tmp.path(), &manifest, None))
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
    fn resolve_validated_deny_oci_rejects_oci_asset() {
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
        .resolve_validated(
            &resolve_context(tmp.path(), &manifest, Some(tmp.path())),
            &ResolveSpec {
                deny_kinds: vec![AssetKind::OciImage],
                ..Default::default()
            },
        )
        .unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("oci://") || msg.contains("OCI") || msg.contains("my-image"),
            "deny_kinds should reject oci:// assets: {msg}"
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
    fn resolve_validated_labels_deny_rejected() {
        let tmp = TempDir::new().unwrap();
        let manifest = tmp.path().join("shasset.yaml");
        std::fs::write(
            &manifest,
            "settings:\n  retries: 0\nassets:\n  secret-img:\n    uri: https://example.com/img.qcow2\n    version: \"1\"\n    labels:\n      - internal\n",
        )
        .unwrap();
        let err = Reference::Asset {
            name: "secret-img".to_string(),
            path: None,
        }
        .resolve_validated(
            &resolve_context(tmp.path(), &manifest, Some(tmp.path())),
            &ResolveSpec {
                deny_labels: vec!["internal".to_string()],
                ..Default::default()
            },
        )
        .unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("internal") && msg.contains("secret-img"),
            "deny_labels should reject asset with matching label: {msg}"
        );
    }

    #[test]
    fn resolve_validated_labels_require_missing_rejected() {
        let tmp = TempDir::new().unwrap();
        let manifest = tmp.path().join("shasset.yaml");
        std::fs::write(
            &manifest,
            "settings:\n  retries: 0\nassets:\n  my-img:\n    uri: https://example.com/img.qcow2\n    version: \"1\"\n",
        )
        .unwrap();
        let err = Reference::Asset {
            name: "my-img".to_string(),
            path: None,
        }
        .resolve_validated(
            &resolve_context(tmp.path(), &manifest, Some(tmp.path())),
            &ResolveSpec {
                require_labels: vec!["production".to_string()],
                ..Default::default()
            },
        )
        .unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("production") && msg.contains("my-img"),
            "require_labels should reject asset missing a required label: {msg}"
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
            format!("{repo_err:#}").contains("path or glob is required"),
            "repo root should be rejected: {repo_err:#}"
        );

        let artifact_err = Reference::Artifact { path: None }
            .resolve_to_file(&resolve_context(tmp.path(), &manifest, None))
            .unwrap_err();
        assert!(
            format!("{artifact_err:#}").contains("path or glob is required"),
            "artifact root should be rejected: {artifact_err:#}"
        );
    }

    #[test]
    fn resolve_to_file_rejects_unsupported_asset_traversal() {
        let tmp = TempDir::new().unwrap();
        let manifest = tmp.path().join("shasset.yaml");
        let err = Reference::Asset {
            name: "tool".to_string(),
            path: Some(std::path::PathBuf::from("bin/tool")),
        }
        .resolve_to_file(&resolve_context(tmp.path(), &manifest, None))
        .unwrap_err();
        assert!(
            format!("{err:#}").contains("not yet supported"),
            "asset traversal should remain unsupported: {err:#}"
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
            path: Some(std::path::PathBuf::from("build/artifact/base.qcow2")),
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
            path: Some(std::path::PathBuf::from("missing.qcow2")),
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
        use super::super::ARTIFACT_DIR;
        let tmp = TempDir::new().unwrap();
        let manifest = tmp.path().join("shasset.yaml");
        let file = tmp.path().join(ARTIFACT_DIR).join("foo.qcow2");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, "qcow2").unwrap();

        let path = Reference::Artifact {
            path: Some(std::path::PathBuf::from("foo.qcow2")),
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
            path: Some(std::path::PathBuf::from("foo.qcow2")),
        }
        .resolve_to_file(&resolve_context(tmp.path(), &manifest, None))
        .unwrap_err();
        assert!(
            format!("{err:#}").contains("not found"),
            "missing artifact file should be rejected: {err:#}"
        );
    }
}
