//! Declarative validation interface for botforge resolver consumers.
//!
//! Consumers declare what they accept via [`ResolveSpec`]; the resolver
//! enforces the spec in a single code path.  This keeps consumer-specific
//! policy out of the core resolution logic and prevents ad-hoc `if` branches
//! scattered across call sites.

use anyhow::{bail, Result};
use std::collections::BTreeSet;

use super::ResolvedFile;

// ── Arity ─────────────────────────────────────────────────────────────────────

/// How many resolved files a consumer expects from a single reference.
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) enum Arity {
    /// Exactly one file must resolve; more or fewer is an error.
    ExactlyOne,
    /// At least one file must resolve; zero is an error.
    AtLeastOne,
    /// Any number of files (including zero) is accepted.
    #[default]
    Any,
}

// ── AssetKind ─────────────────────────────────────────────────────────────────

/// The kind of a shasset asset, derived from its URI scheme.
///
/// `AssetKind` is intentionally separate from filename extension: an
/// `oci://` asset materialises as a `.tar` archive, so an extension check
/// cannot distinguish it from a plain file tarball.  Consumers that need to
/// reject OCI images (e.g. the base-image qcow2 slot) should deny
/// [`AssetKind::OciImage`] rather than require a specific extension.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AssetKind {
    /// An OCI container image (`oci://` URI).
    OciImage,
    /// A plain file asset (any non-`oci://` URI).
    File,
}

impl AssetKind {
    /// Derive the asset kind from the expanded URI string.
    pub(crate) fn from_uri(uri: &str) -> Self {
        if uri.starts_with("oci://") {
            AssetKind::OciImage
        } else {
            AssetKind::File
        }
    }

    fn display_name(&self) -> &'static str {
        match self {
            AssetKind::OciImage => "oci:// image",
            AssetKind::File => "file asset",
        }
    }
}

// ── ResolveSpec ───────────────────────────────────────────────────────────────

/// Declarative validation spec for resolver consumers.
///
/// Consumers construct a `ResolveSpec` and pass it to
/// [`Reference::resolve_validated`] or [`Reference::resolve_one_validated`].
/// The resolver enforces all predicates in one place.
///
/// # Example — base-image contract
///
/// ```rust,ignore
/// let source = image_ref.resolve_one_validated(ctx, &ResolveSpec {
///     arity: Arity::ExactlyOne,
///     deny_kinds: vec![AssetKind::OciImage],
///     ..Default::default()
/// })?;
/// ```
#[derive(Debug, Default)]
pub(crate) struct ResolveSpec {
    /// How many files must be returned.
    pub arity: Arity,
    /// If `Some`, every resolved file's extension must appear in this list.
    /// The comparison is case-sensitive and uses the file's final extension
    /// component only.
    ///
    /// Note: extension checks are inappropriate for distinguishing OCI image
    /// assets from plain files — use [`deny_kinds`](Self::deny_kinds) instead.
    pub extensions: Option<&'static [&'static str]>,
    /// Asset kinds that are forbidden for this consumer.  Only applied to
    /// [`Reference::Asset`] variants; Repo and Artifact references are always
    /// treated as plain files.
    pub deny_kinds: Vec<AssetKind>,
    /// Labels that every resolved asset must carry.  Only applied to
    /// [`Reference::Asset`] variants.
    pub require_labels: Vec<String>,
    /// Labels that prevent resolution.  If an asset carries any of these
    /// labels, resolution fails.  Only applied to [`Reference::Asset`] variants.
    pub deny_labels: Vec<String>,
}

// ── Enforcement helpers ───────────────────────────────────────────────────────

/// Assert that `files.len()` satisfies `spec.arity`.
pub(crate) fn check_arity(files: &[ResolvedFile], spec: &ResolveSpec) -> Result<()> {
    match spec.arity {
        Arity::ExactlyOne => {
            if files.len() != 1 {
                bail!("expected exactly one resolved file, got {}", files.len());
            }
        }
        Arity::AtLeastOne => {
            if files.is_empty() {
                bail!("expected at least one resolved file, got none");
            }
        }
        Arity::Any => {}
    }
    Ok(())
}

/// Assert that every file's extension is in `spec.extensions`.
pub(crate) fn check_extensions(files: &[ResolvedFile], spec: &ResolveSpec) -> Result<()> {
    let Some(exts) = spec.extensions else {
        return Ok(());
    };
    for file in files {
        let ext = file
            .local_path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("");
        if !exts.contains(&ext) {
            bail!(
                "resolved file '{}' has extension '.{}'; expected one of: {}",
                file.local_path.display(),
                ext,
                exts.join(", ")
            );
        }
    }
    Ok(())
}

/// Assert that an asset's kind is not in `spec.deny_kinds`.
///
/// Called before fetching so we never pull bytes for a rejected asset.
pub(crate) fn check_asset_kind(name: &str, uri: &str, spec: &ResolveSpec) -> Result<()> {
    let kind = AssetKind::from_uri(uri);
    for denied in &spec.deny_kinds {
        if &kind == denied {
            bail!(
                "asset '{name}' is a {}; this consumer does not accept {} assets",
                denied.display_name(),
                denied.display_name()
            );
        }
    }
    Ok(())
}

/// Assert that an asset's label set satisfies `spec.require_labels` and
/// does not intersect `spec.deny_labels`.
///
/// Called before fetching so we never pull bytes for a rejected asset.
pub(crate) fn check_asset_labels(
    name: &str,
    labels: &BTreeSet<String>,
    spec: &ResolveSpec,
) -> Result<()> {
    for req in &spec.require_labels {
        if !labels.contains(req) {
            bail!("asset '{name}' is missing required label '{req}'");
        }
    }
    for denied in &spec.deny_labels {
        if labels.contains(denied) {
            bail!("asset '{name}' carries denied label '{denied}'");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn rf(name: &str) -> ResolvedFile {
        ResolvedFile {
            local_path: PathBuf::from(name),
            relative_path: PathBuf::from(name),
        }
    }

    // ── Arity ─────────────────────────────────────────────────────────────────

    #[test]
    fn arity_exactly_one_accepts_one() {
        let spec = ResolveSpec {
            arity: Arity::ExactlyOne,
            ..Default::default()
        };
        assert!(check_arity(&[rf("a.qcow2")], &spec).is_ok());
    }

    #[test]
    fn arity_exactly_one_rejects_zero() {
        let spec = ResolveSpec {
            arity: Arity::ExactlyOne,
            ..Default::default()
        };
        let err = check_arity(&[], &spec).unwrap_err();
        assert!(err.to_string().contains("exactly one"), "{err:#}");
    }

    #[test]
    fn arity_exactly_one_rejects_two() {
        let spec = ResolveSpec {
            arity: Arity::ExactlyOne,
            ..Default::default()
        };
        let err = check_arity(&[rf("a"), rf("b")], &spec).unwrap_err();
        assert!(err.to_string().contains("exactly one"), "{err:#}");
    }

    #[test]
    fn arity_at_least_one_accepts_many() {
        let spec = ResolveSpec {
            arity: Arity::AtLeastOne,
            ..Default::default()
        };
        assert!(check_arity(&[rf("a"), rf("b")], &spec).is_ok());
    }

    #[test]
    fn arity_at_least_one_rejects_zero() {
        let spec = ResolveSpec {
            arity: Arity::AtLeastOne,
            ..Default::default()
        };
        let err = check_arity(&[], &spec).unwrap_err();
        assert!(err.to_string().contains("at least one"), "{err:#}");
    }

    #[test]
    fn arity_any_accepts_zero() {
        let spec = ResolveSpec {
            arity: Arity::Any,
            ..Default::default()
        };
        assert!(check_arity(&[], &spec).is_ok());
    }

    // ── Extension ─────────────────────────────────────────────────────────────

    #[test]
    fn extensions_none_accepts_anything() {
        let spec = ResolveSpec {
            extensions: None,
            ..Default::default()
        };
        assert!(check_extensions(&[rf("foo.tar")], &spec).is_ok());
    }

    #[test]
    fn extensions_some_accepts_matching() {
        let spec = ResolveSpec {
            extensions: Some(&["qcow2"]),
            ..Default::default()
        };
        assert!(check_extensions(&[rf("base.qcow2")], &spec).is_ok());
    }

    #[test]
    fn extensions_some_rejects_non_matching() {
        let spec = ResolveSpec {
            extensions: Some(&["qcow2"]),
            ..Default::default()
        };
        let err = check_extensions(&[rf("broker.tar")], &spec).unwrap_err();
        assert!(err.to_string().contains("qcow2"), "{err:#}");
    }

    // ── AssetKind ─────────────────────────────────────────────────────────────

    #[test]
    fn asset_kind_oci_uri_yields_oci_image() {
        assert_eq!(
            AssetKind::from_uri("oci://ghcr.io/botworkz/broker@sha256:aa"),
            AssetKind::OciImage
        );
    }

    #[test]
    fn asset_kind_https_uri_yields_file() {
        assert_eq!(
            AssetKind::from_uri("https://example.com/base.qcow2"),
            AssetKind::File
        );
    }

    #[test]
    fn check_asset_kind_deny_oci_rejects_oci_asset() {
        let spec = ResolveSpec {
            deny_kinds: vec![AssetKind::OciImage],
            ..Default::default()
        };
        let err = check_asset_kind("broker", "oci://ghcr.io/botworkz/broker@sha256:aa", &spec)
            .unwrap_err();
        assert!(err.to_string().contains("broker"), "{err:#}");
        assert!(err.to_string().contains("oci://"), "{err:#}");
    }

    #[test]
    fn check_asset_kind_deny_oci_accepts_file_asset() {
        let spec = ResolveSpec {
            deny_kinds: vec![AssetKind::OciImage],
            ..Default::default()
        };
        assert!(check_asset_kind("debian", "https://example.com/debian.qcow2", &spec).is_ok());
    }

    // ── Labels ────────────────────────────────────────────────────────────────

    #[test]
    fn labels_require_satisfied() {
        let spec = ResolveSpec {
            require_labels: vec!["production".to_string()],
            ..Default::default()
        };
        let labels: BTreeSet<_> = ["production", "verified"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert!(check_asset_labels("asset", &labels, &spec).is_ok());
    }

    #[test]
    fn labels_require_missing_is_error() {
        let spec = ResolveSpec {
            require_labels: vec!["production".to_string()],
            ..Default::default()
        };
        let labels = BTreeSet::new();
        let err = check_asset_labels("my-asset", &labels, &spec).unwrap_err();
        assert!(err.to_string().contains("production"), "{err:#}");
        assert!(err.to_string().contains("my-asset"), "{err:#}");
    }

    #[test]
    fn labels_deny_absent_passes() {
        let spec = ResolveSpec {
            deny_labels: vec!["internal".to_string()],
            ..Default::default()
        };
        let labels: BTreeSet<_> = ["production"].iter().map(|s| s.to_string()).collect();
        assert!(check_asset_labels("asset", &labels, &spec).is_ok());
    }

    #[test]
    fn labels_deny_present_is_error() {
        let spec = ResolveSpec {
            deny_labels: vec!["internal".to_string()],
            ..Default::default()
        };
        let labels: BTreeSet<_> = ["internal"].iter().map(|s| s.to_string()).collect();
        let err = check_asset_labels("secret-asset", &labels, &spec).unwrap_err();
        assert!(err.to_string().contains("internal"), "{err:#}");
        assert!(err.to_string().contains("secret-asset"), "{err:#}");
    }
}
