//! Botforge reference resolver — single owner of the `@` reference grammar.
//!
//! Every asset/path reference in botforge YAML documents uses the same grammar:
//!
//! ```text
//! <ref>   ::= "@" <root> ("://" <path>)?
//! <root>  ::= ""          -- this repo (checked-in source)
//!           | "output"    -- reserved; repo build output directory
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
//! | `@output`        | Output | `build/output/` directory                 |
//! | `@output://path` | Output | `build/output/` + path, existence check   |

use anyhow::{bail, Context, Result};
use shasset::fetch::{fetch_asset, FetchParams, MaterializeMode};
use shasset::manifest::load;
use std::path::{Component, Path, PathBuf};

use crate::util::{default_cache_dir, resolve_under_root};

/// The blessed build output directory, relative to the repo root.
///
/// Both CI (via a prior job / hydrate step) and local development (by running
/// the previous build stage) must populate this directory out-of-band before
/// any `@output://…` reference can resolve.
pub(crate) const OUTPUT_DIR: &str = "build/output";

// ── Reference ────────────────────────────────────────────────────────────────

/// A parsed botforge reference string.
///
/// Parsing is pure (no I/O).  Use [`Resolver::resolve`] to turn a `Reference`
/// into a filesystem path.
#[derive(Debug, PartialEq, Clone)]
pub(crate) enum Reference {
    /// `@` or `@://path` — a path inside this repo's checked-in source tree.
    Repo { path: Option<PathBuf> },

    /// `@<name>` or `@<name>://path` — a pinned shasset asset.
    ///
    /// The `path` form requires the asset to carry `archive: true` in the
    /// shasset manifest; resolution errors out if that marker is absent.
    Asset { name: String, path: Option<PathBuf> },

    /// `@output` or `@output://path` — the repo's build output directory.
    ///
    /// This root is position-addressed, not content-addressed.  Resolution
    /// performs an existence check only; it never fetches or verifies.
    Output { path: Option<PathBuf> },
}

impl Reference {
    /// Parse a raw reference string into a typed [`Reference`].
    ///
    /// # Parsing rules (all hard/parse-time errors)
    ///
    /// - Must start with `@`; bare names without `@` are rejected.
    /// - `output` is a reserved root keyword; every other non-empty token after
    ///   `@` is treated as a shasset asset name.
    /// - Any `://path` segment must be repo-relative: non-empty, not absolute,
    ///   and free of `.` / `..` components.
    /// - Bare `@` and bare `@output` are valid and resolve to their root
    ///   **directories** (symmetric with the `://path` traversal form).
    pub(crate) fn parse(raw: &str) -> Result<Self> {
        let rest = raw.strip_prefix('@').ok_or_else(|| {
            anyhow::anyhow!(
                "reference must start with `@`; bare names are not supported: {raw:?}"
            )
        })?;

        if let Some((root_token, path_str)) = rest.split_once("://") {
            // Traversal form: @<root>://<path>
            let path = PathBuf::from(path_str);
            validate_ref_path(&path)?;
            match root_token {
                "" => Ok(Reference::Repo { path: Some(path) }),
                "output" => Ok(Reference::Output { path: Some(path) }),
                name => Ok(Reference::Asset {
                    name: name.to_string(),
                    path: Some(path),
                }),
            }
        } else {
            // Simple form: @<root>
            match rest {
                "" => Ok(Reference::Repo { path: None }),
                "output" => Ok(Reference::Output { path: None }),
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

// ── Resolved ─────────────────────────────────────────────────────────────────

/// The result of resolving a [`Reference`] to a filesystem location.
///
/// Callers that care about the type (e.g. `image:` requires a file) should
/// match on this enum rather than calling `path()` directly.
#[derive(Debug, PartialEq, Clone)]
pub(crate) enum Resolved {
    /// A single file.
    File(PathBuf),
    /// A directory tree.
    Dir(PathBuf),
}

impl Resolved {
    /// Return the inner path regardless of whether it's a file or directory.
    #[allow(dead_code)]
    pub(crate) fn path(&self) -> &Path {
        match self {
            Resolved::File(p) | Resolved::Dir(p) => p,
        }
    }

    /// Consume `self` and return the inner path.
    pub(crate) fn into_path(self) -> PathBuf {
        match self {
            Resolved::File(p) | Resolved::Dir(p) => p,
        }
    }
}

// ── Resolver ─────────────────────────────────────────────────────────────────

/// Resolution context for botforge references.
///
/// Holds the three coordinates needed for all resolution strategies:
/// - **`repo_root`** — used for `Repo` references and as the base for `Output`.
/// - **`manifest_path`** — shasset manifest file; used for `Asset` references.
/// - **`cache_dir`** — shasset blob cache; used for `Asset` fetch + verify.
pub(crate) struct Resolver<'a> {
    repo_root: &'a Path,
    manifest_path: &'a Path,
    cache_dir: PathBuf,
}

impl<'a> Resolver<'a> {
    /// Create a new resolver.
    ///
    /// `cache_dir` defaults to [`default_cache_dir`] when `None` is passed.
    pub(crate) fn new(
        repo_root: &'a Path,
        manifest_path: &'a Path,
        cache_dir: Option<PathBuf>,
    ) -> Self {
        Self {
            repo_root,
            manifest_path,
            cache_dir: cache_dir.unwrap_or_else(default_cache_dir),
        }
    }

    /// Resolve a parsed [`Reference`] to a filesystem path.
    ///
    /// # Resolution strategies
    ///
    /// - **`Repo`** — joins `path` under `repo_root`; file vs directory is
    ///   determined by the target's existence on disk.  Bare `@` resolves to
    ///   the repo root directory.
    ///
    /// - **`Asset`** — looks up `name` in the shasset manifest, fetches +
    ///   verifies the blob via [`fetch_asset`], and returns
    ///   `Resolved::File(blob_path)`.  The traversal form (`@foo://path`)
    ///   requires the asset to carry `archive: true`; resolution is not yet
    ///   implemented and returns an error.
    ///
    /// - **`Output`** — joins `path` under `build/output/` and performs an
    ///   **existence check only** (no fetch, no verify).  Missing output →
    ///   clear error telling the operator to run the previous build stage.
    ///   Bare `@output` resolves to the output directory itself.
    pub(crate) fn resolve(&self, reference: &Reference) -> Result<Resolved> {
        match reference {
            Reference::Repo { path: None } => Ok(Resolved::Dir(self.repo_root.to_path_buf())),

            Reference::Repo { path: Some(p) } => {
                let full = resolve_under_root(self.repo_root, p.clone());
                Ok(if full.is_dir() {
                    Resolved::Dir(full)
                } else {
                    Resolved::File(full)
                })
            }

            Reference::Asset { name, path: None } => self.resolve_asset(name),

            Reference::Asset {
                name,
                path: Some(_),
            } => {
                // Grammar is fixed; traversal resolution is typed-but-deferred.
                bail!(
                    "asset traversal (`@{name}://…`) is not yet implemented; \
                     use `@{name}` to reference the asset directly"
                )
            }

            Reference::Output { path: None } => {
                let output_dir = self.repo_root.join(OUTPUT_DIR);
                Ok(Resolved::Dir(output_dir))
            }

            Reference::Output { path: Some(p) } => {
                let output_root = self.repo_root.join(OUTPUT_DIR);
                let full = resolve_under_root(&output_root, p.clone());
                if !full.exists() {
                    bail!(
                        "`@output://{}` does not exist; \
                         run the previous build stage to populate the output directory \
                         (`{}`)",
                        p.display(),
                        output_root.display()
                    );
                }
                Ok(if full.is_dir() {
                    Resolved::Dir(full)
                } else {
                    Resolved::File(full)
                })
            }
        }
    }

    fn resolve_asset(&self, name: &str) -> Result<Resolved> {
        let manifest = load(self.manifest_path).with_context(|| {
            format!(
                "cannot load shasset manifest: {}",
                self.manifest_path.display()
            )
        })?;
        let asset = manifest.assets.get(name).with_context(|| {
            format!(
                "asset '{name}' not found in manifest {}",
                self.manifest_path.display()
            )
        })?;
        let fetched = fetch_asset(FetchParams {
            name,
            asset,
            out_dir: None,
            cache_dir: &self.cache_dir,
            retries: manifest.settings.retries,
            backoff: &manifest.settings.backoff,
            compute_checksum: true,
            no_reverify: false,
            materialize_mode: MaterializeMode::Copy,
            transport: None,
        })
        .with_context(|| format!("failed to fetch asset '{name}'"))?;
        Ok(Resolved::File(fetched.blob_path))
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::{Reference, Resolved, Resolver, OUTPUT_DIR};
    use std::path::{Path, PathBuf};
    use tempfile::TempDir;

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
    fn parse_output_bare() {
        assert_eq!(
            Reference::parse("@output").unwrap(),
            Reference::Output { path: None }
        );
    }

    #[test]
    fn parse_output_with_path() {
        assert_eq!(
            Reference::parse("@output://images/vm.qcow2").unwrap(),
            Reference::Output {
                path: Some(PathBuf::from("images/vm.qcow2"))
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
        for raw in &["@://", "@foo://", "@output://"] {
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

    // ── Resolver::resolve ─────────────────────────────────────────────────────

    #[test]
    fn resolve_repo_root_returns_dir() {
        let tmp = TempDir::new().unwrap();
        let resolver = Resolver::new(tmp.path(), Path::new("/dev/null"), None);
        let result = resolver.resolve(&Reference::Repo { path: None }).unwrap();
        assert_eq!(result, Resolved::Dir(tmp.path().to_path_buf()));
    }

    #[test]
    fn resolve_repo_path_to_file() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("hello.txt"), b"hi").unwrap();
        let resolver = Resolver::new(tmp.path(), Path::new("/dev/null"), None);
        let result = resolver
            .resolve(&Reference::Repo {
                path: Some(PathBuf::from("hello.txt")),
            })
            .unwrap();
        assert_eq!(result, Resolved::File(tmp.path().join("hello.txt")));
    }

    #[test]
    fn resolve_repo_path_to_dir() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir(tmp.path().join("subdir")).unwrap();
        let resolver = Resolver::new(tmp.path(), Path::new("/dev/null"), None);
        let result = resolver
            .resolve(&Reference::Repo {
                path: Some(PathBuf::from("subdir")),
            })
            .unwrap();
        assert_eq!(result, Resolved::Dir(tmp.path().join("subdir")));
    }

    #[test]
    fn resolve_output_bare_returns_output_dir() {
        let tmp = TempDir::new().unwrap();
        let resolver = Resolver::new(tmp.path(), Path::new("/dev/null"), None);
        let result = resolver.resolve(&Reference::Output { path: None }).unwrap();
        let expected = tmp.path().join(OUTPUT_DIR);
        assert_eq!(result, Resolved::Dir(expected));
    }

    #[test]
    fn resolve_output_path_missing_errors() {
        let tmp = TempDir::new().unwrap();
        let resolver = Resolver::new(tmp.path(), Path::new("/dev/null"), None);
        let err = resolver
            .resolve(&Reference::Output {
                path: Some(PathBuf::from("vm.qcow2")),
            })
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("does not exist") || msg.contains("run the previous build stage"),
            "unexpected error: {msg}"
        );
    }

    #[test]
    fn resolve_output_path_existing_file_succeeds() {
        let tmp = TempDir::new().unwrap();
        let output_dir = tmp.path().join(OUTPUT_DIR);
        std::fs::create_dir_all(&output_dir).unwrap();
        std::fs::write(output_dir.join("vm.qcow2"), b"fake").unwrap();

        let resolver = Resolver::new(tmp.path(), Path::new("/dev/null"), None);
        let result = resolver
            .resolve(&Reference::Output {
                path: Some(PathBuf::from("vm.qcow2")),
            })
            .unwrap();
        assert_eq!(result, Resolved::File(output_dir.join("vm.qcow2")));
    }

    #[test]
    fn resolve_asset_traversal_errors_not_yet_implemented() {
        let tmp = TempDir::new().unwrap();
        let resolver = Resolver::new(tmp.path(), Path::new("/dev/null"), None);
        let err = resolver
            .resolve(&Reference::Asset {
                name: "my-tool".to_string(),
                path: Some(PathBuf::from("bin/tool")),
            })
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("not yet implemented"),
            "unexpected error: {msg}"
        );
    }
}
