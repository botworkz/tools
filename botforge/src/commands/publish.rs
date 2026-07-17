//! `botforge publish` — copy/push build artifacts to one or more targets.
//!
//! Supported targets:
//! - **`fs`**: copy resolved artifact(s) to a local filesystem directory.
//! - **`s3`**: upload resolved artifact(s) to an S3 (or S3-compatible) URL
//!   using a native Rust S3 client (`object_store`).  Credentials and region
//!   are read from the environment (`AWS_ACCESS_KEY_ID`,
//!   `AWS_SECRET_ACCESS_KEY`, `AWS_SESSION_TOKEN`, `AWS_REGION` /
//!   `AWS_DEFAULT_REGION`).  When `AWS_ENDPOINT_URL` is set, that endpoint
//!   is used (path-style, with `allow_http` when the scheme is plain `http`)
//!   so MinIO and Contabo Object Storage work out of the box.
//! - **`github`**: publish artifact(s) as assets on a GitHub Release (create
//!   or reuse), via the `publish/github` plugin capability.  Requires the
//!   `publish/github` plugin to be declared in the workspace marker.
//!
//! ## GitHub publish secrets contract
//!
//! Auth secrets are declared in the publish plan's `secrets:` map as
//! `${VAR}` templates.  The host resolves each template via `interpolate_env`
//! at publish time and passes the resolved values to the plugin across the ABI
//! boundary.  **Resolved values are never stored, logged, or echoed.**  Any
//! config display or dry-run output shows the template strings only.
//!
//! Example:
//!
//! ```yaml
//! publish:
//!   github:
//!     - src: "@artifact://images/vm.qcow2"
//!       repo: my-org/my-repo
//!       tag: v1.0.0
//!       secrets:
//!         token: ${GITHUB_TOKEN}
//! ```
//!
//! - `GITHUB_API_URL`: base URL of the GitHub-compatible REST API.
//!   Defaults to `https://api.github.com` when not set.  Override to
//!   point at a mock server in CI.  This is non-secret config, not a
//!   `secrets:` entry.
//!
//! ## Ambient-env hygiene for plugin invocations
//!
//! Before each plugin invocation, the host trims the process environment to an
//! explicit allowlist of benign, operationally-necessary variables (PATH,
//! HOME, TLS/cert vars, proxy vars, etc.) and restores it afterward.  This is
//! **hygiene / defence-in-depth for trusted in-process plugins, NOT an
//! enforced security boundary.**
//!
//! The plugin already has full process access via its `.so` address space
//! (the same posture Envoy takes for dynamic modules); this env trimming only
//! deters *casual / accidental* ambient env reads.  It is racy in
//! multi-threaded contexts; acceptable for this single-threaded publish path.
//! Do not mistake this for a sandbox.
//!
//! ## Schema contract
//!
//! `steps:` is an **ordered, sequential, fail-fast prepare phase** that runs
//! BEFORE any targets.  Steps use plain shell only — no `@://` in step bodies,
//! no `${{ }}` expressions, no input machinery.  cwd is the repo/context root
//! in the container.  This is where versioning paths, rewriting changelogs,
//! computing checksums, and staging/renaming artifacts belong.
//!
//! Publish target directives live under the top-level `publish:` map.  Each
//! target kind within that map (`fs`, `s3`, `github`) is a **list of
//! instances** — multiple destinations of the same kind are expressed as
//! multiple list entries.
//! Publish targets are **unordered** and MAY run in parallel; plans MUST NOT
//! assume any ordering within a kind's list or across kinds.  The current
//! implementation runs them serially, but the iteration order is an
//! implementation detail that plans must not depend on.
//!
//! ## Plan shape
//!
//! ```yaml
//! type: botforge/publish
//! name: my-release
//!
//! steps:
//!   - name: stamp version
//!     run: echo "$(cat VERSION)" > build/artifact/VERSION.txt
//!
//! publish:
//!   fs:
//!     - src: "@artifact://images/my-vm.qcow2"
//!       dest: /mnt/nas/releases/
//!
//!   s3:
//!     - src: "@artifact://images/my-vm.qcow2"
//!       dest: s3://bucket-a/releases/
//!
//!   github:
//!     - src: "@artifact://images/my-vm.qcow2"
//!       repo: my-org/my-repo
//!       tag: v1.0.0
//!       title: "Release v1.0.0"
//!       description: "Release notes here."
//!       secrets:
//!         token: ${GITHUB_TOKEN}
//! ```

use anyhow::{bail, Context, Result};
use clap::{ArgGroup, Args};
use shasset::manifest::{interpolate_env, Manifest};
use std::path::{Path, PathBuf};

use crate::config::{load_publish_config, FsTarget, GithubTarget, PublishConfig, S3Target};
use crate::plan::run_local_steps;
use crate::resolver::{Reference, ResolveFileContext, ResolvedFile};
use crate::step::TestStep;
use crate::util::resolve_under_root;
use crate::workspace::{
    discover_context, load_inline_manifest, load_plugin_entries, registry::load_committed_registry,
};

#[derive(Args, Debug)]
#[command(group(ArgGroup::new("target").required(true).args(["name", "spec"])))]
pub(crate) struct PublishArgs {
    /// Name of the publish plan to run, resolved via workspace registry.
    /// Mutually exclusive with --spec.
    #[arg(value_name = "NAME")]
    name: Option<String>,
    /// Path to a publish plan YAML file (explicit override).
    /// Mutually exclusive with NAME.
    #[arg(long)]
    spec: Option<PathBuf>,
    /// Workspace context root.  When provided, must contain a botforge marker.
    /// When omitted, botforge walks up from the current directory to find one.
    #[arg(long)]
    context: Option<PathBuf>,
    /// Dry-run mode: resolve sources and validate the plan but do not write
    /// or upload any files.
    #[arg(long)]
    dry_run: bool,
}

pub(crate) fn cmd_publish(args: PublishArgs) -> Result<()> {
    let context = discover_context(args.context.as_deref())?;
    let manifest = load_inline_manifest(&context)?;

    let spec_path = match (args.name, args.spec) {
        (Some(name), None) => {
            let registry = load_committed_registry(&context)?;
            registry.publish(&name)?.clone()
        }
        (None, Some(spec)) => resolve_under_root(&context, spec),
        _ => bail!("exactly one of NAME or --spec must be provided"),
    };

    let config = load_publish_config(&spec_path)?;

    run_publish(&context, &manifest, &config, args.dry_run)
}

fn run_publish(
    context: &Path,
    manifest: &Manifest,
    config: &PublishConfig,
    dry_run: bool,
) -> Result<()> {
    let resolve_ctx = ResolveFileContext {
        context,
        manifest,
        cache_dir_override: None,
    };

    // ── prepare phase (steps) ─────────────────────────────────────────────────
    // Steps run sequentially and fail-fast BEFORE any target executes.
    // In dry-run, steps are not executed (they may have side effects); instead,
    // each step name is logged so the user can see what would run.
    if !config.steps.is_empty() {
        if dry_run {
            for (i, step) in config.steps.iter().enumerate() {
                if let TestStep::Run(run) = step {
                    eprintln!("[dry-run] publish step {}: {}", i + 1, run.name);
                }
            }
        } else {
            run_local_steps(context, &config.steps)
                .context("publish: prepare phase (steps) failed")?;
        }
    }

    // ── Load plugins (once, before any plugin-backed target) ─────────────────
    // Plugins are only needed for github targets; they are loaded unconditionally
    // here so that capability resolution errors surface before any target runs.
    let plugin_registry = if !config.github.is_empty() {
        let mut registry = botforge_plugin_host::PluginRegistry::new();
        let entries = load_plugin_entries(context)
            .context("publish: failed to load plugin entries from workspace marker")?;
        for entry in &entries {
            let provides_filter: Option<Vec<String>> = entry.provides.clone();
            registry
                .load_plugin(&entry.name, &entry.src, provides_filter.as_deref())
                .with_context(|| format!("publish: failed to load plugin '{}'", entry.name))?;
        }
        Some(registry)
    } else {
        None
    };

    // Publish targets are unordered; iteration order is an implementation
    // detail that plans must not depend on.  Future work may run these in
    // parallel.

    // ── fs targets ────────────────────────────────────────────────────────────
    for (i, fs) in config.fs.iter().enumerate() {
        run_fs_target(fs, &resolve_ctx, dry_run)
            .with_context(|| format!("publish: fs[{i}] target failed"))?;
    }

    // ── s3 targets ────────────────────────────────────────────────────────────
    for (i, s3) in config.s3.iter().enumerate() {
        run_s3_target(s3, &resolve_ctx, dry_run)
            .with_context(|| format!("publish: s3[{i}] target failed"))?;
    }

    // ── github targets ────────────────────────────────────────────────────────
    if !config.github.is_empty() {
        let registry = plugin_registry
            .as_ref()
            .expect("plugin registry must be Some when github targets are present");

        // GITHUB_API_URL is non-secret config (a base URL, not a credential);
        // it can remain env-driven.
        let api_base_url = std::env::var("GITHUB_API_URL")
            .unwrap_or_else(|_| "https://api.github.com".to_string());

        for (i, gh) in config.github.iter().enumerate() {
            run_github_target(gh, &resolve_ctx, registry, &api_base_url, dry_run)
                .with_context(|| format!("publish: github[{i}] target failed"))?;
        }
    }

    Ok(())
}

// ─── fs target ────────────────────────────────────────────────────────────────

fn run_fs_target(
    target: &FsTarget,
    resolve_ctx: &ResolveFileContext<'_>,
    dry_run: bool,
) -> Result<()> {
    let reference = Reference::parse(&target.src)
        .with_context(|| format!("invalid fs.src reference '{}'", target.src))?;
    let files = reference
        .resolve_to_files(resolve_ctx)
        .with_context(|| format!("fs: cannot resolve source '{}'", target.src))?;
    if files.is_empty() {
        bail!("fs: source '{}' resolved to no files", target.src);
    }

    let dest = PathBuf::from(&target.dest);

    for file in &files {
        copy_to_fs(file, &dest, dry_run)?;
    }

    Ok(())
}

fn copy_to_fs(file: &ResolvedFile, dest: &Path, dry_run: bool) -> Result<()> {
    let dest_path = dest.join(&file.relative_path);

    if dry_run {
        eprintln!(
            "[dry-run] fs: would copy {} → {}",
            file.local_path.display(),
            dest_path.display()
        );
        return Ok(());
    }

    if let Some(parent) = dest_path.parent() {
        std::fs::create_dir_all(parent).with_context(|| {
            format!(
                "fs: cannot create destination directory '{}'",
                parent.display()
            )
        })?;
    }

    std::fs::copy(&file.local_path, &dest_path).with_context(|| {
        format!(
            "fs: cannot copy '{}' to '{}'",
            file.local_path.display(),
            dest_path.display()
        )
    })?;

    eprintln!(
        "publish: fs: {} → {}",
        file.local_path.display(),
        dest_path.display()
    );

    Ok(())
}

// ─── s3 target ────────────────────────────────────────────────────────────────

/// Parse `s3://bucket/key-prefix` into `(bucket, key_prefix)`.
///
/// The destination is validated at load time to start with `s3://`, so this
/// function only needs to split the remainder.  The key prefix may be empty.
fn parse_s3_dest(dest_prefix: &str) -> (&str, &str) {
    let rest = dest_prefix
        .strip_prefix("s3://")
        .expect("s3 dest already validated to start with s3://");
    match rest.split_once('/') {
        Some((bucket, key_prefix)) => (bucket, key_prefix),
        None => (rest, ""),
    }
}

fn run_s3_target(
    target: &S3Target,
    resolve_ctx: &ResolveFileContext<'_>,
    dry_run: bool,
) -> Result<()> {
    let reference = Reference::parse(&target.src)
        .with_context(|| format!("invalid s3.src reference '{}'", target.src))?;
    let files = reference
        .resolve_to_files(resolve_ctx)
        .with_context(|| format!("s3: cannot resolve source '{}'", target.src))?;
    if files.is_empty() {
        bail!("s3: source '{}' resolved to no files", target.src);
    }

    let dest_prefix = target.dest.trim_end_matches('/');

    for file in &files {
        upload_to_s3(file, dest_prefix, dry_run)?;
    }

    Ok(())
}

fn upload_to_s3(file: &ResolvedFile, dest_prefix: &str, dry_run: bool) -> Result<()> {
    let relative = file.relative_path.to_str().ok_or_else(|| {
        anyhow::anyhow!(
            "s3: relative path '{}' is not valid UTF-8",
            file.relative_path.display()
        )
    })?;
    let s3_dest = format!("{dest_prefix}/{relative}");

    if dry_run {
        eprintln!(
            "[dry-run] s3: would upload {} → {}",
            file.local_path.display(),
            s3_dest
        );
        return Ok(());
    }

    let (bucket, key_prefix) = parse_s3_dest(dest_prefix);
    let key = if key_prefix.is_empty() {
        relative.to_string()
    } else {
        format!("{key_prefix}/{relative}")
    };

    let endpoint = std::env::var("AWS_ENDPOINT_URL").ok();
    let endpoint_ref = endpoint.as_deref();

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("s3: failed to build tokio runtime for upload")?;

    rt.block_on(upload_to_s3_async(
        &file.local_path,
        bucket,
        &key,
        endpoint_ref,
    ))
    .with_context(|| {
        format!(
            "s3: upload failed: {} → {s3_dest}",
            file.local_path.display()
        )
    })?;

    eprintln!("publish: s3: {} → {}", file.local_path.display(), s3_dest);

    Ok(())
}

async fn upload_to_s3_async(
    local_path: &Path,
    bucket: &str,
    key: &str,
    endpoint: Option<&str>,
) -> Result<()> {
    use object_store::aws::AmazonS3Builder;
    use object_store::path::Path as ObjPath;
    use object_store::{ObjectStoreExt, WriteMultipart};
    use tokio::io::AsyncReadExt;

    let mut builder = AmazonS3Builder::from_env().with_bucket_name(bucket);

    if let Some(ep) = endpoint {
        builder = builder
            .with_endpoint(ep)
            .with_virtual_hosted_style_request(false);
        if ep.starts_with("http://") {
            builder = builder.with_allow_http(true);
        }
    }

    let store = builder
        .build()
        .with_context(|| format!("s3: failed to build S3 client for bucket '{bucket}'"))?;

    let obj_path =
        ObjPath::parse(key).with_context(|| format!("s3: invalid object key '{key}'"))?;

    // Stream the file to S3 via multipart upload so large qcow2 images do not
    // need to be loaded fully into memory.  WriteMultipart manages part
    // boundaries and concurrency transparently.
    let upload = store
        .put_multipart(&obj_path)
        .await
        .with_context(|| format!("s3: failed to initiate upload to '{bucket}/{key}'"))?;

    let mut writer = WriteMultipart::new(upload);

    let mut src = tokio::fs::File::open(local_path)
        .await
        .with_context(|| format!("s3: cannot open source file '{}'", local_path.display()))?;

    // 8 MiB read buffer — well above the S3 minimum part size (5 MiB).
    let mut buf = vec![0u8; 8 * 1024 * 1024];
    loop {
        let n = src
            .read(&mut buf)
            .await
            .with_context(|| format!("s3: read error on '{}'", local_path.display()))?;
        if n == 0 {
            break;
        }
        writer.write(&buf[..n]);
        // Limit in-flight part concurrency to avoid unbounded memory growth.
        writer
            .wait_for_capacity(4)
            .await
            .with_context(|| format!("s3: upload error for '{bucket}/{key}'"))?;
    }

    writer
        .finish()
        .await
        .with_context(|| format!("s3: failed to complete upload to '{bucket}/{key}'"))?;

    Ok(())
}

// ─── github target ────────────────────────────────────────────────────────────

/// Allowlist of environment variable names passed through to plugin
/// invocations.  Variables not in this list are removed from the process
/// environment for the duration of the plugin call and restored afterward.
///
/// **This is hygiene / defence-in-depth for trusted in-process plugins, NOT
/// an enforced security boundary.**  A dlopen'd `.so` shares the address
/// space and retains full process capabilities (filesystem, network, libc,
/// `/proc/self/environ`).  This list deters *accidental* ambient env reads
/// only — consistent with the Envoy dynamic-modules trust model.  Do not
/// mistake this for a sandbox.
const PLUGIN_ENV_ALLOWLIST: &[&str] = &[
    "PATH",
    "HOME",
    "TMPDIR",
    "TMP",
    "TEMP",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
    "SSL_CERT_PATH",
    "CURL_CA_BUNDLE",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "NO_PROXY",
    "http_proxy",
    "https_proxy",
    "no_proxy",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "TERM",
    "USER",
    "LOGNAME",
    "XDG_RUNTIME_DIR",
    "RUST_LOG",
];

/// Execute `f` with the process environment trimmed to [`PLUGIN_ENV_ALLOWLIST`].
///
/// Variables not in the allowlist are removed before calling `f` and
/// restored (to their original values) afterward.  The restoration is
/// best-effort; if `f` panics, env vars already removed will not be restored.
///
/// **Hygiene / deterrent only** — see [`PLUGIN_ENV_ALLOWLIST`] doc.
fn with_trimmed_env<F, R>(f: F) -> R
where
    F: FnOnce() -> R,
{
    // Capture all current env vars not in the allowlist.
    let removed: Vec<(String, String)> = std::env::vars()
        .filter(|(k, _)| !PLUGIN_ENV_ALLOWLIST.contains(&k.as_str()))
        .collect();

    // Remove them from the current process environment.
    for (k, _) in &removed {
        std::env::remove_var(k);
    }

    let result = f();

    // Restore all removed vars.
    for (k, v) in removed {
        std::env::set_var(k, v);
    }

    result
}

fn run_github_target(
    target: &GithubTarget,
    resolve_ctx: &ResolveFileContext<'_>,
    registry: &botforge_plugin_host::PluginRegistry,
    api_base_url: &str,
    dry_run: bool,
) -> Result<()> {
    let reference = Reference::parse(&target.src)
        .with_context(|| format!("invalid github.src reference '{}'", target.src))?;
    let files = reference
        .resolve_to_files(resolve_ctx)
        .with_context(|| format!("github: cannot resolve source '{}'", target.src))?;
    if files.is_empty() {
        bail!("github: source '{}' resolved to no files", target.src);
    }

    if dry_run {
        // In dry-run mode, do not resolve secrets (no network calls needed).
        // Show only the template strings — never resolved values.
        for file in &files {
            eprintln!(
                "[dry-run] github: would upload {} → {}/{}/releases/tag/{}",
                file.local_path.display(),
                api_base_url,
                target.repo,
                target.tag,
            );
        }
        return Ok(());
    }

    // Resolve secrets from the target's `secrets:` map via `interpolate_env`.
    // Each value is a `${VAR}` template; the resolved values are live stack
    // allocations that never leave this scope.
    let resolved_secrets: Vec<(String, String)> = target
        .secrets
        .iter()
        .map(|(name, tpl)| {
            let value = interpolate_env(tpl).with_context(|| {
                format!("github: failed to resolve secret '{name}' from template '{tpl}'")
            })?;
            Ok((name.clone(), value))
        })
        .collect::<Result<_>>()?;

    let secrets_refs: Vec<(&str, &str)> = resolved_secrets
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();

    // Look up the publish/github capability from the registry.
    let publisher = registry.get_publisher("github").ok_or_else(|| {
        anyhow::anyhow!(
            "publish: github target requires the 'publish/github' plugin capability \
             (name 'github') — add the botforge-plugin-github plugin to the \
             workspace marker's 'plugins:' list"
        )
    })?;

    let asset_paths: Vec<&std::path::Path> = files.iter().map(|f| f.local_path.as_path()).collect();

    let request = botforge_plugin_host::PublishRequest {
        repo: &target.repo,
        tag: &target.tag,
        title: target.title.as_deref(),
        description: target.description.as_deref(),
        asset_paths: &asset_paths,
        api_base_url,
        secrets: &secrets_refs,
    };

    // Invoke the plugin with a trimmed environment (hygiene / deterrent; see
    // PLUGIN_ENV_ALLOWLIST for the rationale and the trust-model caveat).
    let outcome = with_trimmed_env(|| publisher.publish(&request))
        .with_context(|| format!("github: publish to {}/{} failed", target.repo, target.tag))?;

    eprintln!(
        "publish: github: released {} → {}",
        target.tag, outcome.release_url
    );

    Ok(())
}

// ─── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{Cli, Commands};
    use clap::Parser;
    use std::fs;
    use tempfile::TempDir;

    // ── CLI parsing ───────────────────────────────────────────────────────────

    #[test]
    fn publish_cli_name_parses_positional() {
        let result = Cli::try_parse_from(["botforge", "publish", "my-release"]);
        assert!(
            result.is_ok(),
            "publish should parse with positional NAME: {result:?}"
        );
        if let Ok(cli) = result {
            if let Commands::Publish(args) = cli.command {
                assert_eq!(args.name, Some("my-release".to_string()));
                assert!(args.spec.is_none());
            }
        }
    }

    #[test]
    fn publish_cli_spec_parses() {
        let result = Cli::try_parse_from(["botforge", "publish", "--spec", "publish.yaml"]);
        assert!(
            result.is_ok(),
            "publish should parse with --spec: {result:?}"
        );
        if let Ok(cli) = result {
            if let Commands::Publish(args) = cli.command {
                assert!(args.name.is_none());
                assert_eq!(args.spec, Some(PathBuf::from("publish.yaml")));
            }
        }
    }

    #[test]
    fn publish_cli_requires_name_or_spec() {
        let err = Cli::try_parse_from(["botforge", "publish"]).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);
    }

    #[test]
    fn publish_cli_name_and_spec_are_mutually_exclusive() {
        let err = Cli::try_parse_from([
            "botforge",
            "publish",
            "my-release",
            "--spec",
            "publish.yaml",
        ])
        .unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    #[test]
    fn publish_cli_dry_run_flag_parses() {
        let result =
            Cli::try_parse_from(["botforge", "publish", "--spec", "publish.yaml", "--dry-run"]);
        assert!(result.is_ok(), "dry-run flag should parse: {result:?}");
        if let Ok(cli) = result {
            if let Commands::Publish(args) = cli.command {
                assert!(args.dry_run);
            }
        }
    }

    // ── fs target execution ───────────────────────────────────────────────────

    #[test]
    fn fs_target_copies_file_to_dest() {
        let workspace = TempDir::new().unwrap();
        let context = workspace.path();

        // Write a marker so context discovery succeeds.
        fs::write(context.join("botforge.yaml"), "").unwrap();

        // Create a source file in the repo tree.
        let src_dir = context.join("artifacts");
        fs::create_dir_all(&src_dir).unwrap();
        fs::write(src_dir.join("vm.qcow2"), b"fake-qcow2-data").unwrap();

        // Destination directory.
        let dest_dir = TempDir::new().unwrap();

        let manifest = Manifest::default();
        let resolve_ctx = ResolveFileContext {
            context,
            manifest: &manifest,
            cache_dir_override: None,
        };

        let target = FsTarget {
            src: "@://artifacts/vm.qcow2".to_string(),
            dest: dest_dir.path().display().to_string(),
        };

        run_fs_target(&target, &resolve_ctx, false).unwrap();

        let copied = dest_dir.path().join("vm.qcow2");
        assert!(copied.exists(), "file should be copied to dest");
        assert_eq!(
            fs::read(&copied).unwrap(),
            b"fake-qcow2-data",
            "copied content should match source"
        );
    }

    #[test]
    fn fs_target_dry_run_does_not_copy() {
        let workspace = TempDir::new().unwrap();
        let context = workspace.path();
        fs::write(context.join("botforge.yaml"), "").unwrap();

        let src_dir = context.join("artifacts");
        fs::create_dir_all(&src_dir).unwrap();
        fs::write(src_dir.join("vm.qcow2"), b"data").unwrap();

        let dest_dir = TempDir::new().unwrap();

        let manifest = Manifest::default();
        let resolve_ctx = ResolveFileContext {
            context,
            manifest: &manifest,
            cache_dir_override: None,
        };

        let target = FsTarget {
            src: "@://artifacts/vm.qcow2".to_string(),
            dest: dest_dir.path().display().to_string(),
        };

        run_fs_target(&target, &resolve_ctx, true).unwrap();

        let would_be_dest = dest_dir.path().join("vm.qcow2");
        assert!(!would_be_dest.exists(), "dry-run must not create any files");
    }

    #[test]
    fn fs_target_creates_nested_dest_dirs() {
        let workspace = TempDir::new().unwrap();
        let context = workspace.path();
        fs::write(context.join("botforge.yaml"), "").unwrap();

        let src_dir = context.join("artifacts");
        fs::create_dir_all(&src_dir).unwrap();
        fs::write(src_dir.join("image.qcow2"), b"payload").unwrap();

        // Dest is a deeply nested directory that does not yet exist.
        let dest_dir = TempDir::new().unwrap();
        let nested_dest = dest_dir.path().join("a/b/c");

        let manifest = Manifest::default();
        let resolve_ctx = ResolveFileContext {
            context,
            manifest: &manifest,
            cache_dir_override: None,
        };

        let target = FsTarget {
            src: "@://artifacts/image.qcow2".to_string(),
            dest: nested_dest.display().to_string(),
        };

        run_fs_target(&target, &resolve_ctx, false).unwrap();

        let copied = nested_dest.join("image.qcow2");
        assert!(
            copied.exists(),
            "nested dest dirs should be created and file copied"
        );
    }

    #[test]
    fn fs_target_fails_on_missing_source() {
        let workspace = TempDir::new().unwrap();
        let context = workspace.path();
        fs::write(context.join("botforge.yaml"), "").unwrap();

        let dest_dir = TempDir::new().unwrap();
        let manifest = Manifest::default();
        let resolve_ctx = ResolveFileContext {
            context,
            manifest: &manifest,
            cache_dir_override: None,
        };

        let target = FsTarget {
            src: "@://nonexistent/file.qcow2".to_string(),
            dest: dest_dir.path().display().to_string(),
        };

        let err = run_fs_target(&target, &resolve_ctx, false).unwrap_err();
        assert!(
            format!("{err:#}").contains("fs:"),
            "error should mention 'fs:': {err:#}"
        );
    }

    // ── s3 target dry-run ─────────────────────────────────────────────────────

    #[test]
    fn s3_target_dry_run_does_not_upload() {
        let workspace = TempDir::new().unwrap();
        let context = workspace.path();
        fs::write(context.join("botforge.yaml"), "").unwrap();

        let src_dir = context.join("artifacts");
        fs::create_dir_all(&src_dir).unwrap();
        fs::write(src_dir.join("vm.qcow2"), b"data").unwrap();

        let manifest = Manifest::default();
        let resolve_ctx = ResolveFileContext {
            context,
            manifest: &manifest,
            cache_dir_override: None,
        };

        let target = S3Target {
            src: "@://artifacts/vm.qcow2".to_string(),
            dest: "s3://test-bucket/releases".to_string(),
        };

        // dry_run=true: must succeed without contacting S3 / building a client.
        run_s3_target(&target, &resolve_ctx, true).unwrap();
    }

    // ── parse_s3_dest ─────────────────────────────────────────────────────────

    #[test]
    fn parse_s3_dest_splits_bucket_and_prefix() {
        let (bucket, prefix) = parse_s3_dest("s3://my-bucket/releases");
        assert_eq!(bucket, "my-bucket");
        assert_eq!(prefix, "releases");
    }

    #[test]
    fn parse_s3_dest_nested_prefix() {
        let (bucket, prefix) = parse_s3_dest("s3://my-bucket/a/b/c");
        assert_eq!(bucket, "my-bucket");
        assert_eq!(prefix, "a/b/c");
    }

    #[test]
    fn parse_s3_dest_no_prefix() {
        let (bucket, prefix) = parse_s3_dest("s3://my-bucket");
        assert_eq!(bucket, "my-bucket");
        assert_eq!(prefix, "");
    }

    // ── publish config: list-valued schema ────────────────────────────────────

    fn write_temp_publish(content: &str) -> (TempDir, std::path::PathBuf) {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("publish.yaml");
        fs::write(&path, content).unwrap();
        (dir, path)
    }

    #[test]
    fn publish_config_single_fs_instance_parses() {
        let yaml = r#"
type: botforge/publish
name: my-release
publish:
  fs:
    - src: "@artifact://images/vm.qcow2"
      dest: /mnt/nas/releases/
"#;
        let (_dir, path) = write_temp_publish(yaml);
        let config = crate::config::load_publish_config(&path).unwrap();
        assert_eq!(config.fs.len(), 1);
        assert_eq!(config.fs[0].src, "@artifact://images/vm.qcow2");
        assert_eq!(config.fs[0].dest, "/mnt/nas/releases/");
        assert!(config.s3.is_empty());
    }

    #[test]
    fn publish_config_multiple_fs_instances_parse() {
        let yaml = r#"
type: botforge/publish
name: my-release
publish:
  fs:
    - src: "@artifact://images/vm.qcow2"
      dest: /mnt/nas/releases/
    - src: "@artifact://images/vm.qcow2"
      dest: /mnt/mirror/releases/
"#;
        let (_dir, path) = write_temp_publish(yaml);
        let config = crate::config::load_publish_config(&path).unwrap();
        assert_eq!(config.fs.len(), 2);
        assert_eq!(config.fs[0].dest, "/mnt/nas/releases/");
        assert_eq!(config.fs[1].dest, "/mnt/mirror/releases/");
        assert!(config.s3.is_empty());
    }

    #[test]
    fn publish_config_single_s3_instance_parses() {
        let yaml = r#"
type: botforge/publish
name: my-release
publish:
  s3:
    - src: "@artifact://images/vm.qcow2"
      dest: s3://bucket-a/releases/
"#;
        let (_dir, path) = write_temp_publish(yaml);
        let config = crate::config::load_publish_config(&path).unwrap();
        assert_eq!(config.s3.len(), 1);
        assert_eq!(config.s3[0].src, "@artifact://images/vm.qcow2");
        assert_eq!(config.s3[0].dest, "s3://bucket-a/releases/");
        assert!(config.fs.is_empty());
    }

    #[test]
    fn publish_config_multiple_s3_instances_parse() {
        let yaml = r#"
type: botforge/publish
name: my-release
publish:
  s3:
    - src: "@artifact://images/vm.qcow2"
      dest: s3://bucket-a/releases/
    - src: "@artifact://images/vm.qcow2"
      dest: s3://bucket-b/releases/
"#;
        let (_dir, path) = write_temp_publish(yaml);
        let config = crate::config::load_publish_config(&path).unwrap();
        assert_eq!(config.s3.len(), 2);
        assert_eq!(config.s3[0].dest, "s3://bucket-a/releases/");
        assert_eq!(config.s3[1].dest, "s3://bucket-b/releases/");
    }

    #[test]
    fn publish_config_mixed_fs_and_s3_parses() {
        let yaml = r#"
type: botforge/publish
name: mixed-release
publish:
  fs:
    - src: "@artifact://images/vm.qcow2"
      dest: /mnt/nas/releases/
    - src: "@artifact://images/vm.qcow2"
      dest: /mnt/mirror/releases/
  s3:
    - src: "@artifact://images/vm.qcow2"
      dest: s3://bucket-a/releases/
    - src: "@artifact://images/vm.qcow2"
      dest: s3://bucket-b/releases/
"#;
        let (_dir, path) = write_temp_publish(yaml);
        let config = crate::config::load_publish_config(&path).unwrap();
        assert_eq!(config.fs.len(), 2);
        assert_eq!(config.s3.len(), 2);
    }

    #[test]
    fn publish_config_github_target_parses() {
        let yaml = r#"
type: botforge/publish
name: my-release
publish:
  github:
    - src: "@artifact://images/vm.qcow2"
      repo: my-org/my-repo
      tag: v1.0.0
      title: "Release v1.0.0"
      description: "My release notes."
"#;
        let (_dir, path) = write_temp_publish(yaml);
        let config = crate::config::load_publish_config(&path).unwrap();
        assert_eq!(config.github.len(), 1);
        assert_eq!(config.github[0].repo, "my-org/my-repo");
        assert_eq!(config.github[0].tag, "v1.0.0");
        assert_eq!(config.github[0].title.as_deref(), Some("Release v1.0.0"));
        assert_eq!(
            config.github[0].description.as_deref(),
            Some("My release notes.")
        );
    }

    #[test]
    fn publish_config_github_target_title_and_description_optional() {
        let yaml = r#"
type: botforge/publish
name: my-release
publish:
  github:
    - src: "@artifact://images/vm.qcow2"
      repo: my-org/my-repo
      tag: v1.0.0
"#;
        let (_dir, path) = write_temp_publish(yaml);
        let config = crate::config::load_publish_config(&path).unwrap();
        assert_eq!(config.github.len(), 1);
        assert!(config.github[0].title.is_none());
        assert!(config.github[0].description.is_none());
    }

    #[test]
    fn publish_config_github_typo_key_is_error() {
        // 'githubx:' is a typo — deny_unknown_fields must catch it.
        let yaml = r#"
type: botforge/publish
name: my-release
publish:
  githubx:
    - src: "@artifact://images/vm.qcow2"
      repo: my-org/my-repo
      tag: v1.0.0
"#;
        let (_dir, path) = write_temp_publish(yaml);
        let err = crate::config::load_publish_config(&path).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("unknown field") || msg.contains("invalid publish config"),
            "expected parse-time unknown-field error for 'githubx:', got: {msg}"
        );
    }

    #[test]
    fn publish_config_typo_publish_target_key_is_error() {
        let yaml = r#"
type: botforge/publish
name: my-release
publish:
  s3x:
    - src: "@artifact://images/vm.qcow2"
      dest: s3://bucket-a/releases/
"#;
        let (_dir, path) = write_temp_publish(yaml);
        let err = crate::config::load_publish_config(&path).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("unknown field") || msg.contains("invalid publish config"),
            "expected parse-time unknown-field error for typo'd key, got: {msg}"
        );
    }

    #[test]
    fn publish_config_old_top_level_fs_is_error() {
        let yaml = r#"
type: botforge/publish
name: migrated-release
fs:
  - src: "@artifact://images/vm.qcow2"
    dest: /mnt/nas/releases/
"#;
        let (_dir, path) = write_temp_publish(yaml);
        let err = crate::config::load_publish_config(&path).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("unknown field") || msg.contains("fs"),
            "expected unknown top-level field error for old flat fs key, got: {msg}"
        );
    }

    #[test]
    fn publish_config_no_targets_is_error() {
        let yaml = r#"
type: botforge/publish
name: empty-release
"#;
        let (_dir, path) = write_temp_publish(yaml);
        let err = crate::config::load_publish_config(&path).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("no targets"),
            "expected 'no targets' error for empty publish plan, got: {msg}"
        );
    }

    #[test]
    fn publish_config_fs_src_must_be_at_reference() {
        let yaml = r#"
type: botforge/publish
name: my-release
publish:
  fs:
    - src: "plain/path/no-at"
      dest: /mnt/nas/releases/
"#;
        let (_dir, path) = write_temp_publish(yaml);
        let err = crate::config::load_publish_config(&path).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("@-reference") || msg.contains("fs[0].src"),
            "expected @-reference validation error, got: {msg}"
        );
    }

    #[test]
    fn publish_config_s3_src_must_be_at_reference() {
        let yaml = r#"
type: botforge/publish
name: my-release
publish:
  s3:
    - src: "plain/path/no-at"
      dest: s3://bucket-a/releases/
"#;
        let (_dir, path) = write_temp_publish(yaml);
        let err = crate::config::load_publish_config(&path).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("@-reference") || msg.contains("s3[0].src"),
            "expected @-reference validation error, got: {msg}"
        );
    }

    #[test]
    fn publish_config_s3_dest_must_have_s3_prefix() {
        let yaml = r#"
type: botforge/publish
name: my-release
publish:
  s3:
    - src: "@artifact://images/vm.qcow2"
      dest: https://bucket-a/releases/
"#;
        let (_dir, path) = write_temp_publish(yaml);
        let err = crate::config::load_publish_config(&path).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("s3://") || msg.contains("S3 URL"),
            "expected s3:// prefix validation error, got: {msg}"
        );
    }

    #[test]
    fn publish_config_s3_dest_validation_applies_to_every_list_element() {
        // First element valid, second invalid — validation must fire on the second.
        let yaml = r#"
type: botforge/publish
name: my-release
publish:
  s3:
    - src: "@artifact://images/vm.qcow2"
      dest: s3://bucket-a/releases/
    - src: "@artifact://images/vm.qcow2"
      dest: https://not-s3/releases/
"#;
        let (_dir, path) = write_temp_publish(yaml);
        let err = crate::config::load_publish_config(&path).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("s3://") || msg.contains("S3 URL"),
            "validation should fire on second list element, got: {msg}"
        );
    }

    #[test]
    fn publish_config_fs_src_validation_applies_to_every_list_element() {
        // Second element has a bad src — validation must fire on it.
        let yaml = r#"
type: botforge/publish
name: my-release
publish:
  fs:
    - src: "@artifact://images/vm.qcow2"
      dest: /mnt/nas/releases/
    - src: "plain/path"
      dest: /mnt/mirror/releases/
"#;
        let (_dir, path) = write_temp_publish(yaml);
        let err = crate::config::load_publish_config(&path).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("@-reference") || msg.contains("fs[1]"),
            "validation should fire on second fs list element, got: {msg}"
        );
    }

    // ── prepare-phase steps + run_publish tests ───────────────────────────────

    /// Build a minimal `PublishConfig` with `steps:` populated, and `fs` pointing at
    /// a source file in `context`.
    fn make_publish_config_with_steps(
        steps: Vec<crate::step::TestStep>,
        fs: Vec<FsTarget>,
    ) -> PublishConfig {
        PublishConfig {
            name: "test-publish".to_string(),
            steps,
            fs,
            s3: vec![],
            github: vec![],
        }
    }

    #[test]
    fn run_publish_steps_execute_before_targets() {
        // A step writes a sentinel file at the repo root; the fs target then
        // publishes that file.  The published content must reflect the step's write.
        let workspace = TempDir::new().unwrap();
        let context = workspace.path();
        fs::write(context.join("botforge.yaml"), "").unwrap();

        // Source file that the step will WRITE (does not exist yet at plan load time).
        let sentinel = context.join("sentinel.txt");

        // Destination directory for the fs target.
        let dest_dir = TempDir::new().unwrap();

        let manifest = Manifest::default();
        let _resolve_ctx = ResolveFileContext {
            context,
            manifest: &manifest,
            cache_dir_override: None,
        };

        // Step: write "step-was-here" into sentinel.txt at the repo root.
        let step = crate::step::TestStep::Run(crate::step::RunStep {
            name: "write sentinel".to_string(),
            run: format!("echo step-was-here > {}", sentinel.display()),
            target: crate::step::StepTarget::Guest, // ignored for local steps
            shell: None,
            timeout: None,
            sudo: None,
            id: None,
            expect: None,
            condition: None,
        });

        let config = make_publish_config_with_steps(
            vec![step],
            vec![FsTarget {
                src: "@://sentinel.txt".to_string(),
                dest: dest_dir.path().display().to_string(),
            }],
        );

        run_publish(context, &manifest, &config, false).unwrap();

        // The fs target should have published the file that the step created.
        let published = dest_dir.path().join("sentinel.txt");
        assert!(published.exists(), "sentinel.txt should be published");
        let content = fs::read_to_string(&published).unwrap();
        assert!(
            content.contains("step-was-here"),
            "published file should contain the step's output: {content:?}"
        );
    }

    #[test]
    fn run_publish_failing_step_aborts_before_targets() {
        let workspace = TempDir::new().unwrap();
        let context = workspace.path();
        fs::write(context.join("botforge.yaml"), "").unwrap();

        // Source file for the target (exists, so the target would succeed if reached).
        let src_dir = context.join("artifacts");
        fs::create_dir_all(&src_dir).unwrap();
        fs::write(src_dir.join("vm.qcow2"), b"data").unwrap();

        let dest_dir = TempDir::new().unwrap();
        let manifest = Manifest::default();

        let failing_step = crate::step::TestStep::Run(crate::step::RunStep {
            name: "fail-fast step".to_string(),
            run: "exit 42".to_string(),
            target: crate::step::StepTarget::Guest,
            shell: Some("sh".to_string()),
            timeout: None,
            sudo: None,
            id: None,
            expect: None,
            condition: None,
        });

        let config = make_publish_config_with_steps(
            vec![failing_step],
            vec![FsTarget {
                src: "@://artifacts/vm.qcow2".to_string(),
                dest: dest_dir.path().display().to_string(),
            }],
        );

        let err = run_publish(context, &manifest, &config, false).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("prepare phase") || msg.contains("step") || msg.contains("failed"),
            "failing step should abort publish with a clear error: {msg}"
        );

        // The fs target must NOT have run (the destination file must not exist).
        let would_be_dest = dest_dir.path().join("vm.qcow2");
        assert!(
            !would_be_dest.exists(),
            "target must not execute when a step fails"
        );
    }

    #[test]
    fn run_publish_dry_run_does_not_execute_steps() {
        let workspace = TempDir::new().unwrap();
        let context = workspace.path();
        fs::write(context.join("botforge.yaml"), "").unwrap();

        // The step tries to create this marker file; it must NOT be created in dry-run.
        let marker = context.join("dry-run-marker.txt");

        let src_dir = context.join("artifacts");
        fs::create_dir_all(&src_dir).unwrap();
        fs::write(src_dir.join("vm.qcow2"), b"data").unwrap();

        let dest_dir = TempDir::new().unwrap();
        let manifest = Manifest::default();

        let step = crate::step::TestStep::Run(crate::step::RunStep {
            name: "side-effect step".to_string(),
            run: format!("touch {}", marker.display()),
            target: crate::step::StepTarget::Guest,
            shell: None,
            timeout: None,
            sudo: None,
            id: None,
            expect: None,
            condition: None,
        });

        let config = make_publish_config_with_steps(
            vec![step],
            vec![FsTarget {
                src: "@://artifacts/vm.qcow2".to_string(),
                dest: dest_dir.path().display().to_string(),
            }],
        );

        run_publish(context, &manifest, &config, true).unwrap();

        assert!(
            !marker.exists(),
            "dry-run must not execute step side effects"
        );
    }

    #[test]
    fn run_local_steps_step_env_accumulates_across_steps() {
        let workspace = TempDir::new().unwrap();
        let context = workspace.path();

        // Step 1 writes a key to BOTFORGE_ENV; step 2 reads it.
        // If accumulation works, step 2 should see MY_KEY.
        let output_file = context.join("output.txt");
        let steps = vec![
            crate::step::TestStep::Run(crate::step::RunStep {
                name: "set env".to_string(),
                run: "echo MY_KEY=hello >> \"$BOTFORGE_ENV\"".to_string(),
                target: crate::step::StepTarget::Guest,
                shell: None,
                timeout: None,
                sudo: None,
                id: None,
                expect: None,
                condition: None,
            }),
            crate::step::TestStep::Run(crate::step::RunStep {
                name: "read env".to_string(),
                run: format!("echo \"$MY_KEY\" > {}", output_file.display()),
                target: crate::step::StepTarget::Guest,
                shell: None,
                timeout: None,
                sudo: None,
                id: None,
                expect: None,
                condition: None,
            }),
        ];

        crate::plan::run_local_steps(context, &steps).unwrap();

        let content = fs::read_to_string(&output_file).unwrap();
        assert!(
            content.contains("hello"),
            "step 2 should see MY_KEY exported by step 1, got: {content:?}"
        );
    }

    #[test]
    fn run_local_steps_cwd_is_context_root() {
        let workspace = TempDir::new().unwrap();
        let context = workspace.path();

        // Write a file in the repo root; the step should be able to read it
        // with a relative path (cwd = context root).
        fs::write(context.join("VERSION"), "1.2.3").unwrap();
        let output_file = context.join("version-check.txt");

        let steps = vec![crate::step::TestStep::Run(crate::step::RunStep {
            name: "read VERSION".to_string(),
            run: format!("cat VERSION > {}", output_file.display()),
            target: crate::step::StepTarget::Guest,
            shell: None,
            timeout: None,
            sudo: None,
            id: None,
            expect: None,
            condition: None,
        })];

        crate::plan::run_local_steps(context, &steps).unwrap();

        let content = fs::read_to_string(&output_file).unwrap();
        assert!(
            content.contains("1.2.3"),
            "step should read VERSION from cwd (context root): {content:?}"
        );
    }

    #[test]
    fn run_local_steps_nonzero_exit_fails_fast() {
        let workspace = TempDir::new().unwrap();
        let context = workspace.path();

        let marker = context.join("after-fail.txt");

        let steps = vec![
            crate::step::TestStep::Run(crate::step::RunStep {
                name: "failing step".to_string(),
                run: "exit 1".to_string(),
                target: crate::step::StepTarget::Guest,
                shell: Some("sh".to_string()),
                timeout: None,
                sudo: None,
                id: None,
                expect: None,
                condition: None,
            }),
            crate::step::TestStep::Run(crate::step::RunStep {
                name: "should not run".to_string(),
                run: format!("touch {}", marker.display()),
                target: crate::step::StepTarget::Guest,
                shell: None,
                timeout: None,
                sudo: None,
                id: None,
                expect: None,
                condition: None,
            }),
        ];

        let err = crate::plan::run_local_steps(context, &steps).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("step") || msg.contains("failed") || msg.contains("exit"),
            "failing step should produce a clear error: {msg}"
        );
        assert!(
            !marker.exists(),
            "second step must not run after first step fails"
        );
    }

    // ── github target ─────────────────────────────────────────────────────────

    #[test]
    fn github_target_dry_run_does_not_invoke_plugin() {
        let workspace = TempDir::new().unwrap();
        let context = workspace.path();
        fs::write(context.join("botforge.yaml"), "").unwrap();

        let src_dir = context.join("artifacts");
        fs::create_dir_all(&src_dir).unwrap();
        fs::write(src_dir.join("vm.qcow2"), b"fake-qcow2-data").unwrap();

        let manifest = Manifest::default();
        let resolve_ctx = ResolveFileContext {
            context,
            manifest: &manifest,
            cache_dir_override: None,
        };

        // An empty registry has no publisher wired — dry-run must not invoke it.
        let registry = botforge_plugin_host::PluginRegistry::new();

        let target = GithubTarget {
            src: "@://artifacts/vm.qcow2".to_string(),
            repo: "my-org/my-repo".to_string(),
            tag: "v1.0.0".to_string(),
            title: Some("Release v1.0.0".to_string()),
            description: None,
            secrets: std::collections::HashMap::new(),
        };

        // Dry-run must succeed without secrets resolved or plugin loaded.
        run_github_target(
            &target,
            &resolve_ctx,
            &registry,
            /* api_base_url = */ "https://api.github.com",
            /* dry_run = */ true,
        )
        .expect("dry-run github target must succeed without plugin or secrets");
    }

    #[test]
    fn github_target_missing_plugin_is_error() {
        let workspace = TempDir::new().unwrap();
        let context = workspace.path();
        fs::write(context.join("botforge.yaml"), "").unwrap();

        let src_dir = context.join("artifacts");
        fs::create_dir_all(&src_dir).unwrap();
        fs::write(src_dir.join("vm.qcow2"), b"data").unwrap();

        let manifest = Manifest::default();
        let resolve_ctx = ResolveFileContext {
            context,
            manifest: &manifest,
            cache_dir_override: None,
        };

        // Registry with no publish/github capability loaded.
        let registry = botforge_plugin_host::PluginRegistry::new();

        let target = GithubTarget {
            src: "@://artifacts/vm.qcow2".to_string(),
            repo: "my-org/my-repo".to_string(),
            tag: "v1.0.0".to_string(),
            title: None,
            description: None,
            // Provide a resolved token so the test reaches the plugin lookup.
            secrets: [("token".to_string(), "dummy-token".to_string())]
                .into_iter()
                .collect(),
        };

        let err = run_github_target(
            &target,
            &resolve_ctx,
            &registry,
            "https://api.github.com",
            /* dry_run = */ false,
        )
        .expect_err("missing plugin must produce an error");

        let msg = format!("{err:#}");
        assert!(
            msg.contains("publish/github") || msg.contains("plugin"),
            "error should mention the missing plugin capability: {msg}"
        );
    }

    #[test]
    fn publish_config_github_no_targets_only_github_counts() {
        // A plan with ONLY a github target must succeed the no-targets check.
        let yaml = r#"
type: botforge/publish
name: my-release
publish:
  github:
    - src: "@artifact://images/vm.qcow2"
      repo: my-org/my-repo
      tag: v1.0.0
"#;
        let (_dir, path) = write_temp_publish(yaml);
        let config = crate::config::load_publish_config(&path).unwrap();
        assert_eq!(config.github.len(), 1);
        assert!(config.fs.is_empty());
        assert!(config.s3.is_empty());
    }

    // ── github secrets config ─────────────────────────────────────────────────

    #[test]
    fn publish_config_github_secrets_parses() {
        // Secrets declared as ${VAR} templates must round-trip through config
        // as templates, not resolved values.
        let yaml = r#"
type: botforge/publish
name: my-release
publish:
  github:
    - src: "@artifact://images/vm.qcow2"
      repo: my-org/my-repo
      tag: v1.0.0
      secrets:
        token: ${GITHUB_TOKEN}
        extra: ${EXTRA_SECRET}
"#;
        let (_dir, path) = write_temp_publish(yaml);
        let config = crate::config::load_publish_config(&path).unwrap();
        assert_eq!(config.github.len(), 1);
        let gh = &config.github[0];
        assert_eq!(
            gh.secrets.get("token").map(String::as_str),
            Some("${GITHUB_TOKEN}"),
            "token secret template must be stored verbatim"
        );
        assert_eq!(
            gh.secrets.get("extra").map(String::as_str),
            Some("${EXTRA_SECRET}"),
            "extra secret template must be stored verbatim"
        );
        // The secrets map must never contain a resolved value at this stage.
        for (name, tpl) in &gh.secrets {
            assert!(
                tpl.starts_with("${") && tpl.ends_with('}'),
                "secret '{name}' must be a ${{VAR}} template at config time, got: {tpl:?}"
            );
        }
    }

    #[test]
    fn publish_config_github_empty_secrets_is_allowed() {
        // No `secrets:` block is allowed (omitted → empty map); resolution
        // errors will surface at publish time, not load time.
        let yaml = r#"
type: botforge/publish
name: my-release
publish:
  github:
    - src: "@artifact://images/vm.qcow2"
      repo: my-org/my-repo
      tag: v1.0.0
"#;
        let (_dir, path) = write_temp_publish(yaml);
        let config = crate::config::load_publish_config(&path).unwrap();
        assert!(
            config.github[0].secrets.is_empty(),
            "absent secrets block must produce an empty map"
        );
    }

    #[test]
    fn github_target_missing_token_secret_is_error() {
        // When no 'token' secret is provided and we are not in dry-run mode,
        // the publish invocation must fail with a clear error.  We use a
        // preloaded registry with the real plugin so the error comes from the
        // plugin's 'token not found' path.
        //
        // This test only runs when the plugin .so has been built.
        let workspace = TempDir::new().unwrap();
        let context = workspace.path();
        fs::write(context.join("botforge.yaml"), "").unwrap();

        let src_dir = context.join("artifacts");
        fs::create_dir_all(&src_dir).unwrap();
        fs::write(src_dir.join("vm.qcow2"), b"data").unwrap();

        let manifest = Manifest::default();
        let resolve_ctx = ResolveFileContext {
            context,
            manifest: &manifest,
            cache_dir_override: None,
        };

        // An empty secrets map means the plugin will not find "token".
        let target = GithubTarget {
            src: "@://artifacts/vm.qcow2".to_string(),
            repo: "my-org/my-repo".to_string(),
            tag: "v1.0.0".to_string(),
            title: None,
            description: None,
            secrets: std::collections::HashMap::new(),
        };

        // Use an empty registry — missing plugin error fires first, which is
        // also a legible failure (confirms the error propagation chain).
        let registry = botforge_plugin_host::PluginRegistry::new();
        let err = run_github_target(
            &target,
            &resolve_ctx,
            &registry,
            "https://api.github.com",
            false,
        )
        .unwrap_err();

        let msg = format!("{err:#}");
        assert!(
            msg.contains("plugin") || msg.contains("publish/github"),
            "should mention missing plugin or capability: {msg}"
        );
    }

    // ── env hygiene: with_trimmed_env ─────────────────────────────────────────

    #[test]
    fn with_trimmed_env_restores_removed_vars() {
        // Set a sentinel env var that is NOT in the allowlist.
        let key = "_BOTFORGE_TEST_TRIMMED_RESTORE";
        std::env::set_var(key, "original-value");

        // Inside with_trimmed_env the var should be absent.
        with_trimmed_env(|| {
            assert!(
                std::env::var(key).is_err(),
                "non-allowlist var should be absent inside with_trimmed_env"
            );
        });

        // After with_trimmed_env the var should be restored.
        assert_eq!(
            std::env::var(key).as_deref(),
            Ok("original-value"),
            "non-allowlist var must be restored after with_trimmed_env"
        );

        // Clean up.
        std::env::remove_var(key);
    }

    #[test]
    fn with_trimmed_env_allowlist_vars_remain() {
        // PATH (always in allowlist) must remain set inside the closure.
        let path_val = std::env::var("PATH").unwrap_or_default();
        if !path_val.is_empty() {
            with_trimmed_env(|| {
                assert!(
                    std::env::var("PATH").is_ok(),
                    "PATH must remain set inside with_trimmed_env"
                );
            });
        }
    }
}
