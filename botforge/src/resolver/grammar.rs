//! Pure grammar for botforge `@` reference strings.
//!
//! This module is intentionally I/O-free: no `shasset`, no `std::fs` imports.
//! The grammar is owned here so the constraint can be enforced at the module
//! boundary — `grammar` never needs anything that would force an I/O dep.

use anyhow::{bail, Result};
use serde::Deserialize;
use std::path::{Component, Path, PathBuf};

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

#[cfg(test)]
mod tests {
    use super::*;

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
}
