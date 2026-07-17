//! `botforge publish` — copy/push build artifacts to one or more targets.
//!
//! Supported targets (built-in, not plugins):
//! - **`fs`**: copy resolved artifact(s) to a local filesystem directory.
//! - **`s3`**: upload resolved artifact(s) to an S3 (or S3-compatible) URL
//!   using a native Rust S3 client (`object_store`).  Credentials and region
//!   are read from the environment (`AWS_ACCESS_KEY_ID`,
//!   `AWS_SECRET_ACCESS_KEY`, `AWS_SESSION_TOKEN`, `AWS_REGION` /
//!   `AWS_DEFAULT_REGION`).  When `AWS_ENDPOINT_URL` is set, that endpoint
//!   is used (path-style, with `allow_http` when the scheme is plain `http`)
//!   so MinIO and Contabo Object Storage work out of the box.
//!
//! ## Schema contract
//!
//! `steps:` is an **ordered, sequential, fail-fast prepare phase** that runs
//! BEFORE any targets.  Steps use plain shell only — no `@://` in step bodies,
//! no `${{ }}` expressions, no input machinery.  cwd is the repo/context root
//! in the container.  This is where versioning paths, rewriting changelogs,
//! computing checksums, and staging/renaming artifacts belong.
//!
//! Each target kind (`fs`, `s3`) is a **list of instances** — multiple
//! destinations of the same kind are expressed as multiple list entries.
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
//! fs:
//!   - src: "@artifact://images/my-vm.qcow2"
//!     dest: /mnt/nas/releases/
//!   - src: "@artifact://images/my-vm.qcow2"
//!     dest: /mnt/mirror/releases/
//!
//! s3:
//!   - src: "@artifact://images/my-vm.qcow2"
//!     dest: s3://bucket-a/releases/
//!   - src: "@artifact://images/my-vm.qcow2"
//!     dest: s3://bucket-b/releases/
//! ```

use anyhow::{bail, Context, Result};
use clap::{ArgGroup, Args};
use shasset::manifest::Manifest;
use std::path::{Path, PathBuf};

use crate::config::{load_publish_config, FsTarget, PublishConfig, S3Target};
use crate::plan::run_local_steps;
use crate::resolver::{Reference, ResolveFileContext, ResolvedFile};
use crate::step::TestStep;
use crate::util::resolve_under_root;
use crate::workspace::{discover_context, load_inline_manifest, registry::load_committed_registry};

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
    fn publish_config_unknown_top_level_key_is_error() {
        let yaml = r#"
type: botforge/publish
name: my-release
github:
  - src: "@artifact://images/vm.qcow2"
    dest: https://github.com/foo/bar/releases/
"#;
        let (_dir, path) = write_temp_publish(yaml);
        let err = crate::config::load_publish_config(&path).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("unknown field") || msg.contains("invalid publish config"),
            "expected parse-time unknown-field error, got: {msg}"
        );
    }

    #[test]
    fn publish_config_typo_top_level_key_is_error() {
        let yaml = r#"
type: botforge/publish
name: my-release
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
}
