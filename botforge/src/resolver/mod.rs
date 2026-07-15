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
//!
//! ## Module layout
//!
//! | Module       | Responsibility                                     |
//! |--------------|---------------------------------------------------|
//! | `grammar`    | `Reference` enum, `parse`, `validate_ref_path` — pure, I/O-free |
//! | `paths`      | `resolve_existing_file`, glob helpers, symlink-escape guard |
//! | `fetch`      | Asset arm: manifest lookup → fetch_asset → materialize |
//! | `validate`   | `ResolveSpec`, `Arity`, `AssetKind`, enforcement helpers |
//! | `mod` (this) | Public API: `resolve_to_file`, `resolve_to_files`, `resolve_validated` |

mod fetch;
mod grammar;
mod paths;
pub(crate) mod validate;

pub(crate) use grammar::Reference;
pub(crate) use validate::{Arity, AssetKind, ResolveSpec};

use anyhow::{bail, Context, Result};
use shasset::fetch::Transport;
use shasset::manifest::Manifest;
use std::path::{Path, PathBuf};

pub(crate) const ARTIFACT_DIR: &str = "build/artifact";

/// Context required to resolve a parsed [`Reference`] to a concrete local file or files.
pub(crate) struct ResolveFileContext<'a> {
    pub(crate) context: &'a Path,
    pub(crate) manifest: &'a Manifest,
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

// ── Public resolution API ─────────────────────────────────────────────────────

impl Reference {
    /// Resolve this parsed reference to a concrete local file path ready for consumption.
    #[allow(dead_code)]
    pub(crate) fn resolve_to_file(&self, context: &ResolveFileContext<'_>) -> Result<PathBuf> {
        let spec = ResolveSpec::default();
        let files = self.resolve_validated_with_transport(context, &spec, || None)?;
        // Default spec has Arity::Any; extract the single file ourselves.
        // For single-file references (Asset, Repo+path, Artifact+path) there
        // will be exactly one entry.  For directory roots the inner impl
        // already bails, so we never reach here with 0 or >1 entries from
        // those arms.
        Ok(files
            .into_iter()
            .next()
            .map(|f| f.local_path)
            .unwrap_or_else(|| context.context.to_path_buf()))
    }

    /// Resolve this reference to a collection of local file paths.
    ///
    /// This is the multi-file counterpart to [`resolve_to_file`].
    ///
    /// For `@://<glob>` and `@artifact://<glob>` references whose path
    /// contains glob metacharacters, every matching regular file is returned
    /// together with a [`ResolvedFile::relative_path`] computed by stripping
    /// the pattern's fixed literal prefix.
    ///
    /// For single-file references a one-element [`Vec`] is returned so call
    /// sites can use this API uniformly.
    pub(crate) fn resolve_to_files(
        &self,
        context: &ResolveFileContext<'_>,
    ) -> Result<Vec<ResolvedFile>> {
        let spec = ResolveSpec::default();
        self.resolve_validated_with_transport(context, &spec, || None)
    }

    /// Resolve this reference and enforce the declarative [`ResolveSpec`].
    ///
    /// All consumer-specific policy (arity, extension, asset kind, labels) is
    /// declared in `spec` and enforced here in a single code path.
    ///
    /// Returns all resolved files as a `Vec<ResolvedFile>`.  Arity checking
    /// is applied after resolution; use [`resolve_one_validated`] for the
    /// common `ExactlyOne` case.
    #[allow(dead_code)]
    pub(crate) fn resolve_validated(
        &self,
        context: &ResolveFileContext<'_>,
        spec: &ResolveSpec,
    ) -> Result<Vec<ResolvedFile>> {
        self.resolve_validated_with_transport(context, spec, || None)
    }

    /// Convenience wrapper around [`resolve_validated`] for consumers that
    /// expect exactly one resolved file.
    ///
    /// Enforces `Arity::ExactlyOne` regardless of `spec.arity` and returns
    /// the resolved path directly, making call sites cleaner.
    pub(crate) fn resolve_one_validated(
        &self,
        context: &ResolveFileContext<'_>,
        spec: &ResolveSpec,
    ) -> Result<PathBuf> {
        let one_spec = ResolveSpec {
            arity: Arity::ExactlyOne,
            extensions: spec.extensions,
            deny_kinds: spec.deny_kinds.clone(),
            require_labels: spec.require_labels.clone(),
            deny_labels: spec.deny_labels.clone(),
        };
        let mut files = self.resolve_validated_with_transport(context, &one_spec, || None)?;
        // Arity::ExactlyOne was already enforced; safe to unwrap.
        Ok(files.remove(0).local_path)
    }

    // ── Internal implementation ────────────────────────────────────────────

    pub(crate) fn resolve_validated_with_transport<F>(
        &self,
        context: &ResolveFileContext<'_>,
        spec: &ResolveSpec,
        mut transport_factory: F,
    ) -> Result<Vec<ResolvedFile>>
    where
        F: FnMut() -> Option<Box<dyn Transport>>,
    {
        use validate::{check_arity, check_extensions};

        let files = match self {
            // ── Bare shasset asset → fetch + pre-fetch spec validation ────────
            Reference::Asset { name, path: None } => {
                let local_path =
                    fetch::fetch_asset_blob(name, context, spec, &mut transport_factory)?;
                let relative_path = local_path
                    .file_name()
                    .map(PathBuf::from)
                    .unwrap_or_else(|| local_path.clone());
                vec![ResolvedFile {
                    local_path,
                    relative_path,
                }]
            }

            // ── Asset traversal (archive) — not yet supported ─────────────────
            Reference::Asset {
                name,
                path: Some(path),
            } => bail!(
                "reference '@{name}://{}' requires archive traversal, \
                 which is not yet supported",
                path.display()
            ),

            // ── Directory roots → hard errors ─────────────────────────────────
            Reference::Repo { path: None } => bail!(
                "reference '@' resolves to the context root directory; \
                 a path or glob is required"
            ),
            Reference::Artifact { path: None } => bail!(
                "reference '@artifact' resolves to the artifact root directory; \
                 a path or glob is required"
            ),

            // ── Repo path / glob ──────────────────────────────────────────────
            Reference::Repo { path: Some(path) } => {
                let canonical_root = canonicalize_root(context.context)?;
                paths::resolve_ref_path_to_files(
                    context.context,
                    path,
                    "repo reference",
                    &canonical_root,
                )?
            }

            // ── Artifact path / glob ──────────────────────────────────────────
            Reference::Artifact { path: Some(path) } => {
                let canonical_root = canonicalize_root(context.context)?;
                let artifact_root = context.context.join(ARTIFACT_DIR);
                paths::resolve_ref_path_to_files(
                    &artifact_root,
                    path,
                    "artifact reference",
                    &canonical_root,
                )?
            }
        };

        // Post-resolution spec checks (extension, arity).
        check_extensions(&files, spec)?;
        check_arity(&files, spec)?;

        Ok(files)
    }
}

fn canonicalize_root(context: &Path) -> Result<PathBuf> {
    context
        .canonicalize()
        .with_context(|| format!("cannot canonicalize context root: {}", context.display()))
}
