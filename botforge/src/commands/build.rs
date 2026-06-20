use anyhow::{bail, Context, Result};
use clap::Args;
use serde::Deserialize;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use crate::util::{create_temp_dir, ensure_command, normalize_path, resolve_under_root};

const DEFAULT_DISK_SIZE: &str = "10G";
const DEFAULT_MEMSIZE: u32 = 4096;
const DEFAULT_SMP: u32 = 4;

#[derive(Args, Debug)]
pub(crate) struct BuildArgs {
    /// Path to the build spec YAML (default: `build.yaml` next to --source).
    #[arg(long, required = true)]
    spec: PathBuf,
    /// Source qcow2 to chroot into. Read-only; copied to <output>.partial
    /// before any modification.
    #[arg(long, required = true)]
    source: PathBuf,
    /// Output qcow2 path. Materialized atomically from <output>.partial on
    /// success.
    #[arg(long, required = true)]
    output: PathBuf,
    /// Repo root for resolving relative spec/source/output/step/context
    /// paths (default: current dir).
    #[arg(long)]
    repo_root: Option<PathBuf>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct BuildSpec {
    #[serde(default)]
    disk_size: Option<String>,
    #[serde(default)]
    memsize: Option<u32>,
    #[serde(default)]
    smp: Option<u32>,
    #[serde(default)]
    context: Option<ContextSpec>,
    // singleton_map_recursive lets BuildStep variants render as the
    // pleasant `- run: foo` YAML form rather than serde_yaml's default
    // `!Run foo` tagged form.
    #[serde(default, with = "serde_yaml::with::singleton_map_recursive")]
    steps: Vec<BuildStep>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ContextSpec {
    /// Absolute guest path where the tarball is extracted before steps run.
    dest: PathBuf,
    /// Host paths to bundle into the guest context tarball. Each entry is
    /// either a bare string (then `dest` inside the context = basename of
    /// the host path) or a mapping with explicit `src:` / `dest:`.
    paths: Vec<ContextPath>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum ContextPath {
    Bare(PathBuf),
    Mapped {
        src: PathBuf,
        #[serde(default)]
        dest: Option<PathBuf>,
    },
}

impl ContextPath {
    fn src(&self) -> &Path {
        match self {
            ContextPath::Bare(p) => p.as_path(),
            ContextPath::Mapped { src, .. } => src.as_path(),
        }
    }

    /// Returns the relative path inside the context tarball where the
    /// source should be placed. Defaults to the basename of `src` when
    /// not explicitly mapped.
    fn dest(&self) -> Result<PathBuf> {
        let explicit = match self {
            ContextPath::Bare(_) => None,
            ContextPath::Mapped { dest, .. } => dest.clone(),
        };
        if let Some(dest) = explicit {
            validate_relative_path(&dest)?;
            return Ok(normalize_path(&dest));
        }
        let basename = self
            .src()
            .file_name()
            .with_context(|| {
                format!(
                    "context path '{}' has no basename; provide an explicit `dest:`",
                    self.src().display()
                )
            })?
            .to_owned();
        Ok(PathBuf::from(basename))
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
enum BuildStep {
    /// Copy a host script into the guest, chmod +x, and run it as root.
    Run(PathBuf),
    /// Run a shell command in the guest (no host file involved).
    RunCommand(String),
    /// Upload a single host file to an explicit guest path (preserves
    /// the destination path exactly, unlike `copy_in`).
    Upload(UploadSpec),
    /// Recursively copy a host file or directory into a guest *directory*
    /// (preserves the source basename inside the destination).
    CopyIn(CopyInSpec),
    /// Create a directory in the guest (parents created).
    Mkdir(PathBuf),
    /// Truncate a guest file to zero bytes (creates it if missing).
    Truncate(PathBuf),
    /// Delete a guest file or directory.
    Delete(PathBuf),
    /// Write a literal string to a guest file path.
    Write(WriteSpec),
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct UploadSpec {
    src: PathBuf,
    dest: PathBuf,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CopyInSpec {
    src: PathBuf,
    dest: PathBuf,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WriteSpec {
    path: PathBuf,
    content: String,
}

pub(crate) fn cmd_build(args: BuildArgs) -> Result<()> {
    ensure_command("virt-customize")?;
    ensure_command("tar")?;

    let repo_root = std::fs::canonicalize(
        args.repo_root
            .unwrap_or(std::env::current_dir().context("failed to determine current directory")?),
    )
    .context("failed to resolve repo root")?;

    let spec_path = resolve_under_root(&repo_root, args.spec.clone());
    let spec = load_spec(&spec_path)?;
    let source = resolve_under_root(&repo_root, args.source.clone());
    let output = resolve_under_root(&repo_root, args.output.clone());

    if !source.is_file() {
        bail!("source qcow2 not found: {}", source.display());
    }
    if let Some(parent) = output.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("cannot create output dir: {}", parent.display()))?;
    }

    let partial = partial_output_path(&output);
    // A stale .partial from a previous failed run would confuse the atomic
    // rename on success; clear it up front.
    if partial.exists() {
        std::fs::remove_file(&partial).with_context(|| {
            format!("cannot remove stale partial output: {}", partial.display())
        })?;
    }

    copy_qcow2(&source, &partial)?;
    resize_qcow2(
        &partial,
        spec.disk_size.as_deref().unwrap_or(DEFAULT_DISK_SIZE),
    )?;

    let staging_dir = create_temp_dir("botforge-build")?;
    let result = (|| -> Result<()> {
        let mut virt_args = Vec::<String>::new();
        virt_args.extend(["-a".into(), partial.display().to_string()]);
        virt_args.extend([
            "--memsize".into(),
            spec.memsize.unwrap_or(DEFAULT_MEMSIZE).to_string(),
        ]);
        virt_args.extend(["--smp".into(), spec.smp.unwrap_or(DEFAULT_SMP).to_string()]);

        // Context staging runs before any declared step so consumers can
        // reference uploaded paths immediately.
        let mut context_tar: Option<PathBuf> = None;
        if let Some(context) = &spec.context {
            validate_guest_absolute_path(&context.dest)?;
            let tar = build_context_tarball(&repo_root, &staging_dir, context)?;
            let guest_tar = context.dest.join("ctx.tar");
            let guest_tar_str = guest_tar.display().to_string();
            virt_args.extend([
                "--mkdir".into(),
                context.dest.display().to_string(),
                "--upload".into(),
                format!("{}:{}", tar.display(), guest_tar_str),
                "--run-command".into(),
                format!(
                    "tar -C {dest} -xf {tar} && rm -f {tar}",
                    dest = shell_single_quote(&context.dest.display().to_string()),
                    tar = shell_single_quote(&guest_tar_str),
                ),
            ]);
            context_tar = Some(tar);
        }

        for step in &spec.steps {
            extend_virt_args_for_step(&repo_root, step, &mut virt_args)?;
        }

        run_virt_customize(&virt_args)?;
        drop(context_tar);
        Ok(())
    })();

    // Always clean up the staging dir before propagating errors. The .partial
    // is left behind on failure for post-mortem; cleared at the top of the
    // next invocation.
    let _ = std::fs::remove_dir_all(&staging_dir);
    result?;

    std::fs::rename(&partial, &output).with_context(|| {
        format!(
            "cannot atomically materialize output from {} to {}",
            partial.display(),
            output.display()
        )
    })?;

    println!("built image at {}", output.display());
    Ok(())
}

fn load_spec(path: &Path) -> Result<BuildSpec> {
    let yaml = std::fs::read_to_string(path)
        .with_context(|| format!("cannot read build spec: {}", path.display()))?;
    serde_yaml::from_str(&yaml).with_context(|| format!("invalid build spec: {}", path.display()))
}

fn copy_qcow2(source: &Path, partial: &Path) -> Result<()> {
    // Prefer `cp --reflink=auto` for instant CoW on btrfs/xfs; fall back
    // to a regular copy when reflink isn't available.
    let status = Command::new("cp")
        .arg("--reflink=auto")
        .arg(source)
        .arg(partial)
        .status()
        .context("failed to execute cp")?;
    if status.success() {
        return Ok(());
    }
    std::fs::copy(source, partial).with_context(|| {
        format!(
            "cannot copy source qcow2 from {} to {}",
            source.display(),
            partial.display()
        )
    })?;
    Ok(())
}

fn resize_qcow2(disk: &Path, size: &str) -> Result<()> {
    let new_size =
        crate::qcow2::parse_size(size).with_context(|| format!("invalid disk_size '{}'", size))?;
    crate::qcow2::grow_qcow2_virtual_size(disk, new_size)
}

fn run_virt_customize(args: &[String]) -> Result<()> {
    let status = Command::new("virt-customize")
        .args(args)
        .status()
        .context("failed to execute virt-customize")?;
    if !status.success() {
        bail!("virt-customize failed (exit status: {status})");
    }
    Ok(())
}

fn build_context_tarball(
    repo_root: &Path,
    staging_dir: &Path,
    context: &ContextSpec,
) -> Result<PathBuf> {
    let layout_dir = staging_dir.join("ctx");
    std::fs::create_dir_all(&layout_dir)
        .with_context(|| format!("cannot create context layout dir: {}", layout_dir.display()))?;

    for entry in &context.paths {
        let src = resolve_under_root(repo_root, entry.src().to_path_buf());
        if !src.exists() {
            bail!("context path does not exist: {}", src.display());
        }
        let dest_rel = entry.dest()?;
        let dest = layout_dir.join(&dest_rel);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent).with_context(|| {
                format!("cannot create context staging dir: {}", parent.display())
            })?;
        }
        // Use `cp -a` to preserve perms, symlinks, dir trees in one shot.
        // The std lib has no recursive copy; shelling out keeps this honest.
        let status = Command::new("cp")
            .arg("-a")
            .arg(&src)
            .arg(&dest)
            .status()
            .context("failed to execute cp -a for context staging")?;
        if !status.success() {
            bail!(
                "cp -a {} {} failed (exit status: {status})",
                src.display(),
                dest.display()
            );
        }
    }

    let tar_path = staging_dir.join("ctx.tar");
    let status = Command::new("tar")
        .arg("-C")
        .arg(&layout_dir)
        .arg("-cf")
        .arg(&tar_path)
        .arg(".")
        .status()
        .context("failed to execute tar")?;
    if !status.success() {
        bail!("tar failed to package context (exit status: {status})");
    }
    Ok(tar_path)
}

fn extend_virt_args_for_step(
    repo_root: &Path,
    step: &BuildStep,
    out: &mut Vec<String>,
) -> Result<()> {
    match step {
        BuildStep::Run(script) => {
            let resolved = resolve_under_root(repo_root, script.clone());
            if !resolved.is_file() {
                bail!("step run script not found: {}", resolved.display());
            }
            out.push("--run".into());
            out.push(resolved.display().to_string());
        }
        BuildStep::RunCommand(cmd) => {
            out.push("--run-command".into());
            out.push(cmd.clone());
        }
        BuildStep::Upload(spec) => {
            let src = resolve_under_root(repo_root, spec.src.clone());
            if !src.is_file() {
                bail!("step upload source is not a file: {}", src.display());
            }
            validate_guest_absolute_path(&spec.dest)?;
            out.push("--upload".into());
            out.push(format!("{}:{}", src.display(), spec.dest.display()));
        }
        BuildStep::CopyIn(spec) => {
            let src = resolve_under_root(repo_root, spec.src.clone());
            if !src.exists() {
                bail!("step copy_in source does not exist: {}", src.display());
            }
            validate_guest_absolute_path(&spec.dest)?;
            out.push("--copy-in".into());
            out.push(format!("{}:{}", src.display(), spec.dest.display()));
        }
        BuildStep::Mkdir(path) => {
            validate_guest_absolute_path(path)?;
            out.push("--mkdir".into());
            out.push(path.display().to_string());
        }
        BuildStep::Truncate(path) => {
            validate_guest_absolute_path(path)?;
            out.push("--truncate".into());
            out.push(path.display().to_string());
        }
        BuildStep::Delete(path) => {
            validate_guest_absolute_path(path)?;
            out.push("--delete".into());
            out.push(path.display().to_string());
        }
        BuildStep::Write(spec) => {
            validate_guest_absolute_path(&spec.path)?;
            out.push("--write".into());
            out.push(format!("{}:{}", spec.path.display(), spec.content));
        }
    }
    Ok(())
}

fn validate_guest_absolute_path(path: &Path) -> Result<()> {
    if !path.is_absolute() {
        bail!("guest path must be absolute, got: {}", path.display());
    }
    if path
        .components()
        .any(|c| matches!(c, Component::CurDir | Component::ParentDir))
    {
        bail!(
            "guest path must not contain '.' or '..' segments: {}",
            path.display()
        );
    }
    // Path::components silently normalizes a leading `/./` away (it folds
    // CurDir after RootDir), so re-scan the raw string to catch the form
    // `/./foo`, which is just as suspect as `/foo/./bar`.
    let raw = path.to_string_lossy();
    if raw.contains("/./") || raw.contains("/../") || raw.ends_with("/.") || raw.ends_with("/..") {
        bail!(
            "guest path must not contain '.' or '..' segments: {}",
            path.display()
        );
    }
    Ok(())
}

fn validate_relative_path(path: &Path) -> Result<()> {
    if path.is_absolute() {
        bail!("path must be relative, got: {}", path.display());
    }
    for component in path.components() {
        match component {
            Component::Normal(_) => {}
            _ => bail!(
                "path must contain no '.' or '..' segments: {}",
                path.display()
            ),
        }
    }
    Ok(())
}

fn partial_output_path(output: &Path) -> PathBuf {
    let mut name = output
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    name.push(".partial");
    output.with_file_name(name)
}

fn shell_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

#[cfg(test)]
mod tests {
    use super::{
        extend_virt_args_for_step, partial_output_path, shell_single_quote,
        validate_guest_absolute_path, validate_relative_path, BuildSpec, BuildStep, ContextPath,
        UploadSpec,
    };
    use std::path::{Path, PathBuf};
    use tempfile::TempDir;

    #[test]
    fn spec_round_trips_full_example() {
        let yaml = r#"
disk_size: 12G
memsize: 2048
smp: 2
context:
  dest: /tmp/botwork-build-context
  paths:
    - images/botwork/payload/envoy
    - { src: build/images/baked, dest: images }
steps:
  - run: images/_shared/provisioners/00-base.sh
  - run_command: "true"
  - upload: { src: build/foo, dest: /tmp/foo }
  - copy_in: { src: images/botwork/payload/systemd, dest: /etc/systemd/system }
  - mkdir: /tmp/botwork-build-context
  - truncate: /etc/machine-id
  - delete: /var/lib/dbus/machine-id
  - write: { path: /etc/marker, content: "hello\n" }
"#;
        let spec: BuildSpec = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(spec.disk_size.as_deref(), Some("12G"));
        assert_eq!(spec.memsize, Some(2048));
        assert_eq!(spec.smp, Some(2));
        let ctx = spec.context.as_ref().unwrap();
        assert_eq!(ctx.dest, PathBuf::from("/tmp/botwork-build-context"));
        assert_eq!(ctx.paths.len(), 2);
        match &ctx.paths[0] {
            ContextPath::Bare(p) => {
                assert_eq!(p, &PathBuf::from("images/botwork/payload/envoy"))
            }
            ContextPath::Mapped { .. } => panic!("expected bare context path"),
        }
        match &ctx.paths[1] {
            ContextPath::Mapped { src, dest } => {
                assert_eq!(src, &PathBuf::from("build/images/baked"));
                assert_eq!(dest, &Some(PathBuf::from("images")));
            }
            ContextPath::Bare(_) => panic!("expected mapped context path"),
        }
        assert_eq!(spec.steps.len(), 8);
        assert!(matches!(spec.steps[0], BuildStep::Run(_)));
        assert!(matches!(spec.steps[1], BuildStep::RunCommand(_)));
        assert!(matches!(spec.steps[2], BuildStep::Upload(_)));
        assert!(matches!(spec.steps[3], BuildStep::CopyIn(_)));
        assert!(matches!(spec.steps[4], BuildStep::Mkdir(_)));
        assert!(matches!(spec.steps[5], BuildStep::Truncate(_)));
        assert!(matches!(spec.steps[6], BuildStep::Delete(_)));
        assert!(matches!(spec.steps[7], BuildStep::Write(_)));
    }

    #[test]
    fn context_path_dest_defaults_to_basename() {
        let bare = ContextPath::Bare(PathBuf::from("a/b/payload"));
        assert_eq!(bare.dest().unwrap(), PathBuf::from("payload"));
    }

    #[test]
    fn context_path_dest_uses_explicit_value_when_set() {
        let mapped = ContextPath::Mapped {
            src: PathBuf::from("build/images/baked"),
            dest: Some(PathBuf::from("images")),
        };
        assert_eq!(mapped.dest().unwrap(), PathBuf::from("images"));
    }

    #[test]
    fn context_path_dest_rejects_absolute_or_parent_segments() {
        let bad_abs = ContextPath::Mapped {
            src: PathBuf::from("anything"),
            dest: Some(PathBuf::from("/etc")),
        };
        assert!(bad_abs.dest().is_err());
        let bad_dotdot = ContextPath::Mapped {
            src: PathBuf::from("anything"),
            dest: Some(PathBuf::from("../escape")),
        };
        assert!(bad_dotdot.dest().is_err());
    }

    #[test]
    fn extend_virt_args_for_run_step_resolves_relative_paths() {
        let tmp = TempDir::new().unwrap();
        let script = tmp.path().join("provisioner.sh");
        std::fs::write(&script, "#!/bin/sh\ntrue\n").unwrap();
        let step = BuildStep::Run(PathBuf::from("provisioner.sh"));
        let mut out = Vec::new();
        extend_virt_args_for_step(tmp.path(), &step, &mut out).unwrap();
        assert_eq!(out[0], "--run");
        assert_eq!(out[1], script.display().to_string());
    }

    #[test]
    fn extend_virt_args_for_run_step_rejects_missing_script() {
        let tmp = TempDir::new().unwrap();
        let step = BuildStep::Run(PathBuf::from("nope.sh"));
        let mut out = Vec::new();
        assert!(extend_virt_args_for_step(tmp.path(), &step, &mut out).is_err());
    }

    #[test]
    fn extend_virt_args_for_upload_joins_src_dest_with_colon() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("payload.bin");
        std::fs::write(&src, b"x").unwrap();
        let step = BuildStep::Upload(UploadSpec {
            src: PathBuf::from("payload.bin"),
            dest: PathBuf::from("/opt/payload.bin"),
        });
        let mut out = Vec::new();
        extend_virt_args_for_step(tmp.path(), &step, &mut out).unwrap();
        assert_eq!(out[0], "--upload");
        assert_eq!(out[1], format!("{}:/opt/payload.bin", src.display()));
    }

    #[test]
    fn extend_virt_args_for_upload_rejects_non_absolute_dest() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("payload.bin");
        std::fs::write(&src, b"x").unwrap();
        let step = BuildStep::Upload(UploadSpec {
            src: PathBuf::from("payload.bin"),
            dest: PathBuf::from("relative/dest"),
        });
        let mut out = Vec::new();
        assert!(extend_virt_args_for_step(tmp.path(), &step, &mut out).is_err());
    }

    #[test]
    fn extend_virt_args_for_mkdir_truncate_delete_match_expected_argv() {
        let tmp = TempDir::new().unwrap();
        let mut out = Vec::new();
        extend_virt_args_for_step(
            tmp.path(),
            &BuildStep::Mkdir(PathBuf::from("/var/lib/botwork")),
            &mut out,
        )
        .unwrap();
        extend_virt_args_for_step(
            tmp.path(),
            &BuildStep::Truncate(PathBuf::from("/etc/machine-id")),
            &mut out,
        )
        .unwrap();
        extend_virt_args_for_step(
            tmp.path(),
            &BuildStep::Delete(PathBuf::from("/var/lib/dbus/machine-id")),
            &mut out,
        )
        .unwrap();
        assert_eq!(
            out,
            vec![
                "--mkdir",
                "/var/lib/botwork",
                "--truncate",
                "/etc/machine-id",
                "--delete",
                "/var/lib/dbus/machine-id"
            ]
        );
    }

    #[test]
    fn extend_virt_args_for_run_command_passes_command_through() {
        let mut out = Vec::new();
        extend_virt_args_for_step(
            Path::new("/repo"),
            &BuildStep::RunCommand("echo hi".to_string()),
            &mut out,
        )
        .unwrap();
        assert_eq!(out, vec!["--run-command", "echo hi"]);
    }

    #[test]
    fn validate_guest_absolute_path_accepts_clean_absolute() {
        assert!(validate_guest_absolute_path(Path::new("/etc/machine-id")).is_ok());
    }

    #[test]
    fn validate_guest_absolute_path_rejects_relative_or_traversal() {
        assert!(validate_guest_absolute_path(Path::new("etc/machine-id")).is_err());
        assert!(validate_guest_absolute_path(Path::new("/etc/../machine-id")).is_err());
        assert!(validate_guest_absolute_path(Path::new("/./machine-id")).is_err());
    }

    #[test]
    fn validate_relative_path_matches_expected_shape() {
        assert!(validate_relative_path(Path::new("a/b")).is_ok());
        assert!(validate_relative_path(Path::new("/abs")).is_err());
        assert!(validate_relative_path(Path::new("a/../b")).is_err());
        assert!(validate_relative_path(Path::new("./a")).is_err());
    }

    #[test]
    fn partial_output_path_appends_partial_suffix() {
        assert_eq!(
            partial_output_path(Path::new("/build/out.qcow2")),
            PathBuf::from("/build/out.qcow2.partial")
        );
    }

    #[test]
    fn shell_single_quote_escapes_embedded_quotes() {
        assert_eq!(shell_single_quote("a'b"), "'a'\"'\"'b'");
    }

    #[test]
    fn spec_rejects_unknown_top_level_fields() {
        let yaml = r#"
disk_size: 10G
boguous_field: true
steps: []
"#;
        let err = serde_yaml::from_str::<BuildSpec>(yaml).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("boguous_field") || msg.contains("unknown field"),
            "unexpected error: {msg}"
        );
    }
}
