//! `botforge publish` — copy/push build artifacts to one or more targets.
//!
//! Supported targets (built-in, not plugins):
//! - **`fs`**: copy resolved artifact(s) to a local filesystem directory.
//! - **`s3`**: upload resolved artifact(s) to an S3 (or S3-compatible) URL
//!   using the `aws` CLI.  Credentials are read from the environment
//!   (`AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, `AWS_DEFAULT_REGION`,
//!   and optionally `AWS_ENDPOINT_URL` for S3-compatible services such as
//!   Contabo Object Storage).
//!
//! ## Schema contract
//!
//! Each target kind (`fs`, `s3`) is a **list of instances** — multiple
//! destinations of the same kind are expressed as multiple list entries.
//! Publish targets are **unordered** and MAY run in parallel; plans MUST NOT
//! assume any ordering within a kind's list or across kinds.  The current
//! implementation runs them serially, but the iteration order is an
//! implementation detail that plans must not depend on.
//! All ordered / pre-publish work (path mangling, versioning, checksums, etc.)
//! is deferred to a future `steps:` prepare phase (not yet implemented).
//!
//! ## Plan shape
//!
//! ```yaml
//! type: botforge/publish
//! name: my-release
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
use crate::resolver::{Reference, ResolveFileContext, ResolvedFile};
use crate::util::{ensure_command, resolve_under_root, run_command};
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

fn run_s3_target(
    target: &S3Target,
    resolve_ctx: &ResolveFileContext<'_>,
    dry_run: bool,
) -> Result<()> {
    if !dry_run {
        ensure_command("aws").context(
            "publish: s3 target requires the AWS CLI ('aws'); \
             install it or set PATH to include it",
        )?;
    }

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

    let local = file
        .local_path
        .to_str()
        .ok_or_else(|| {
            anyhow::anyhow!(
                "s3: local path '{}' is not valid UTF-8",
                file.local_path.display()
            )
        })?
        .to_string();

    run_command(
        "aws",
        &["s3".to_string(), "cp".to_string(), local, s3_dest.clone()],
        &[],
        &format!("s3: failed to upload to '{s3_dest}'"),
    )?;

    eprintln!("publish: s3: {} → {}", file.local_path.display(), s3_dest);

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
    fn s3_target_dry_run_does_not_invoke_aws() {
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

        // dry_run=true: should succeed without calling the `aws` CLI.
        run_s3_target(&target, &resolve_ctx, true).unwrap();
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
}
