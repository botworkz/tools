use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use shasset::fetch::{fetch_asset, FetchParams, MaterializeMode};
use shasset::manifest::{load, Asset};
use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

#[derive(Parser, Debug)]
#[command(
    name = "botforge",
    about = "Build-time tooling for botworkz VM artifacts",
    long_about = "botforge is a build-time companion tool for preparing dependencies and VM build artifacts."
)]
struct Cli {
    /// Path to shasset manifest.
    #[arg(long, short = 'c', default_value = "shasset.yaml", global = true)]
    config: PathBuf,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Fetch and stage one or all assets from shasset.yaml into a flat output directory.
    Deps(DepsArgs),
    /// Build an ISO image from a source directory.
    Iso(IsoArgs),
    /// Run the KVM-only Packer build flow inside docker compose.
    Pack(PackArgs),
}

#[derive(clap::Args, Debug)]
struct DepsArgs {
    /// Asset name to fetch (all assets if omitted).
    name: Option<String>,
    /// Flat output directory; each asset is materialized to `<out>/<asset-filename>`.
    #[arg(long, required = true)]
    out: PathBuf,
    /// Cache directory (default: `~/.cache/shasset`).
    #[arg(long)]
    cache_dir: Option<PathBuf>,
    /// Skip re-verifying cache blobs before use.
    #[arg(long)]
    no_reverify: bool,
    /// Set the executable bit (0o755) on each staged file (Unix only).
    #[arg(long)]
    executable: bool,
}

#[derive(clap::Args, Debug)]
struct IsoArgs {
    /// Source directory tree to include in the ISO.
    #[arg(long, required = true)]
    src: PathBuf,
    /// Output ISO file path.
    #[arg(long, required = true)]
    out: PathBuf,
    /// ISO volume ID.
    #[arg(long, default_value = "BOTFORGE")]
    volume_id: String,
}

#[derive(clap::Args, Debug)]
struct PackArgs {
    /// VM checkout root containing compose.yaml and images/ (default: current directory).
    #[arg(long)]
    repo_root: Option<PathBuf>,
    /// Compress the qcow2 output with qemu-img convert -c.
    #[arg(long)]
    compress: bool,
    /// SSH private key path (default: <repo-root>/build/packer_ssh_key).
    #[arg(long)]
    key: Option<PathBuf>,
    /// Docker compose service to run packer/qemu-img in.
    #[arg(long, default_value = "tools-kvm")]
    compose_service: String,
    /// Docker compose file path (default: <repo-root>/compose.yaml).
    #[arg(long)]
    compose_file: Option<PathBuf>,
}

fn main() {
    if let Err(e) = run() {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Commands::Deps(args) => cmd_deps(&cli.config, args),
        Commands::Iso(args) => cmd_iso(args),
        Commands::Pack(args) => cmd_pack(args),
    }
}

fn cmd_deps(config: &Path, args: DepsArgs) -> Result<()> {
    let manifest = load(config)?;
    let cache_dir = args.cache_dir.unwrap_or_else(default_cache_dir);

    let targets: Vec<(&str, &Asset)> = if let Some(ref name) = args.name {
        let asset = manifest
            .assets
            .get(name.as_str())
            .with_context(|| format!("asset '{name}' not found"))?;
        vec![(name.as_str(), asset)]
    } else {
        manifest
            .assets
            .iter()
            .map(|(name, asset)| (name.as_str(), asset))
            .collect()
    };

    if targets.is_empty() {
        println!("no assets to fetch");
        return Ok(());
    }

    std::fs::create_dir_all(&args.out)
        .with_context(|| format!("cannot create output dir: {}", args.out.display()))?;

    for (name, asset) in targets {
        let fetched = fetch_asset(FetchParams {
            name,
            asset,
            out_dir: None,
            cache_dir: &cache_dir,
            retries: manifest.settings.retries,
            backoff: &manifest.settings.backoff,
            compute_checksum: false,
            no_reverify: args.no_reverify,
            materialize_mode: MaterializeMode::Copy,
            transport: None,
        })
        .with_context(|| format!("failed to fetch asset '{name}'"))?;

        let filename = asset
            .output_filename()
            .with_context(|| format!("asset '{name}': cannot determine output filename"))?;
        let out_path = materialize_flat(&fetched.blob_path, &args.out, &filename, args.executable)
            .with_context(|| format!("failed to stage asset '{name}'"))?;
        println!("fetched '{}' → {}", name, out_path.display());
    }

    Ok(())
}

fn cmd_iso(args: IsoArgs) -> Result<()> {
    if !args.src.is_dir() {
        bail!("source directory does not exist: {}", args.src.display());
    }

    if let Some(parent) = args.out.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("cannot create output dir: {}", parent.display()))?;
    }

    let tool = detect_iso_tool()?;
    let mut command = Command::new(tool);
    if tool == "xorriso" {
        command.arg("-as").arg("mkisofs");
    }
    command
        .arg("-r")
        .arg("-J")
        .arg("-V")
        .arg(&args.volume_id)
        .arg("-o")
        .arg(&args.out)
        .arg(&args.src);

    let output = command
        .output()
        .with_context(|| format!("failed to execute {tool}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("{tool} failed: {}", stderr.trim());
    }

    println!("built ISO at {}", args.out.display());
    Ok(())
}

/// Run the simplified v1 Packer flow in docker compose.
///
/// This intentionally does not build or stage dependencies/images; callers must
/// arrange that beforehand. KVM is required.
fn cmd_pack(args: PackArgs) -> Result<()> {
    if !Path::new("/dev/kvm").exists() {
        bail!("botforge pack requires KVM: /dev/kvm not found");
    }
    if !command_exists("docker") {
        bail!("'docker' is not available on PATH");
    }

    let repo_root = std::fs::canonicalize(
        args.repo_root
            .unwrap_or(std::env::current_dir().context("failed to determine current directory")?),
    )
    .context("failed to resolve repo root")?;
    if !repo_root.is_dir() {
        bail!("repo root is not a directory: {}", repo_root.display());
    }

    let compose_file = resolve_under_root(
        &repo_root,
        args.compose_file
            .unwrap_or_else(|| PathBuf::from("compose.yaml")),
    );
    let build_dir = repo_root.join("build");
    if build_dir.exists() {
        let build_dir_real = std::fs::canonicalize(&build_dir)
            .with_context(|| format!("cannot resolve build dir: {}", build_dir.display()))?;
        let build_rel = repo_relative_path(&repo_root, &build_dir_real)?;
        if build_rel != "build" {
            bail!(
                "refusing to use non-standard build directory under repo root: {}",
                build_dir.display()
            );
        }
    }
    let build_rel = repo_relative_path(&repo_root, &build_dir)?;
    if build_rel != "build" {
        bail!(
            "refusing to use non-standard build directory under repo root: {}",
            build_dir.display()
        );
    }
    std::fs::create_dir_all(&build_dir)
        .with_context(|| format!("cannot create build dir: {}", build_dir.display()))?;

    let build_output_dir = build_dir.join("output");
    if build_output_dir.exists() {
        let build_output_real = std::fs::canonicalize(&build_output_dir).with_context(|| {
            format!(
                "cannot resolve build output directory: {}",
                build_output_dir.display()
            )
        })?;
        let build_output_rel = repo_relative_path(&repo_root, &build_output_real)?;
        if build_output_rel != "build/output" {
            bail!(
                "refusing to remove non-standard build output path: {}",
                build_output_dir.display()
            );
        }
        std::fs::remove_dir_all(&build_output_dir).with_context(|| {
            format!(
                "cannot remove prior build output directory: {}",
                build_output_dir.display()
            )
        })?;
    }

    let default_key = build_dir.join("packer_ssh_key");
    let key_path = resolve_under_root(&repo_root, args.key.clone().unwrap_or(default_key.clone()));
    let uses_default_key = key_path == default_key;
    if uses_default_key && !key_path.exists() {
        println!("generating ephemeral SSH key at {}", key_path.display());
        run_command(
            "ssh-keygen",
            &[
                "-t".into(),
                "ed25519".into(),
                "-N".into(),
                "".into(),
                "-f".into(),
                key_path.display().to_string(),
            ],
            &[],
            "failed to generate default packer SSH key",
        )?;
    }

    let public_key_path = PathBuf::from(format!("{}.pub", key_path.display()));
    if !key_path.is_file() {
        bail!("SSH private key not found: {}", key_path.display());
    }
    if !public_key_path.is_file() {
        bail!("SSH public key not found: {}", public_key_path.display());
    }
    let public_key = std::fs::read_to_string(&public_key_path)
        .with_context(|| format!("cannot read SSH public key: {}", public_key_path.display()))?
        .trim()
        .to_string();
    let key_real = std::fs::canonicalize(&key_path)
        .with_context(|| format!("cannot resolve SSH private key: {}", key_path.display()))?;
    let _ = repo_relative_path(&repo_root, &key_real)?;
    let rel_key_path = repo_relative_path(&repo_root, &key_path)?;

    let host_uid = std::env::var("HOST_UID").unwrap_or(run_capture("id", &["-u"])?);
    let host_gid = std::env::var("HOST_GID").unwrap_or(run_capture("id", &["-g"])?);
    let host_kvm_gid = resolve_host_kvm_gid(
        std::env::var("HOST_KVM_GID").ok(),
        getent_group_kvm_output(),
    );
    let env_pairs = [
        ("HOST_UID", host_uid.as_str()),
        ("HOST_GID", host_gid.as_str()),
        ("HOST_KVM_GID", host_kvm_gid.as_str()),
    ];

    let compose_base = compose_base_args(&repo_root, &compose_file);

    println!(
        "running packer init in compose service {}",
        args.compose_service
    );
    run_command(
        "docker",
        &packer_init_args(&compose_base, &args.compose_service),
        &env_pairs,
        "packer init failed",
    )?;

    println!(
        "running packer build in compose service {}",
        args.compose_service
    );
    run_command(
        "docker",
        &packer_build_args(
            &compose_base,
            &args.compose_service,
            &rel_key_path,
            &public_key,
        ),
        &env_pairs,
        "packer build failed",
    )?;

    if args.compress {
        let source = build_output_dir.join("debian-13-botwork.qcow2");
        if !source.is_file() {
            bail!(
                "qcow2 source image not found for compression: {}",
                source.display()
            );
        }
        let target = build_dir.join("debian-13-botwork-compressed.qcow2");
        let rel_source = repo_relative_path(&repo_root, &source)?;
        let rel_target = repo_relative_path(&repo_root, &target)?;
        println!("compressing qcow2 to {}", target.display());
        run_command(
            "docker",
            &compress_args(
                &compose_base,
                &args.compose_service,
                &rel_source,
                &rel_target,
            ),
            &env_pairs,
            "qcow2 compression failed",
        )?;
    }

    println!("pack complete");
    Ok(())
}

fn detect_iso_tool() -> Result<&'static str> {
    if command_exists("xorriso") {
        return Ok("xorriso");
    }
    if command_exists("genisoimage") {
        return Ok("genisoimage");
    }
    bail!("neither 'xorriso' nor 'genisoimage' is available on PATH")
}

fn command_exists(program: &str) -> bool {
    Command::new(program)
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn resolve_under_root(repo_root: &Path, path: PathBuf) -> PathBuf {
    if path.is_absolute() {
        normalize_path(&path)
    } else {
        normalize_path(&repo_root.join(path))
    }
}

fn normalize_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                let _ = normalized.pop();
            }
            Component::Normal(part) => normalized.push(part),
        }
    }
    normalized
}

fn repo_relative_path(repo_root: &Path, path: &Path) -> Result<String> {
    let repo_root = normalize_path(repo_root);
    let path = normalize_path(path);
    let relative = path.strip_prefix(&repo_root).with_context(|| {
        format!(
            "path '{}' is outside repo root '{}'",
            path.display(),
            repo_root.display()
        )
    })?;
    let rendered = if relative.as_os_str().is_empty() {
        ".".to_string()
    } else {
        relative.to_string_lossy().replace('\\', "/")
    };
    Ok(rendered)
}

fn resolve_host_kvm_gid(env_value: Option<String>, getent_output: Option<String>) -> String {
    if let Some(value) = env_value.filter(|value| !value.trim().is_empty()) {
        return value;
    }

    if let Some(line) = getent_output {
        if let Some(gid) = line
            .lines()
            .find_map(|entry| entry.split(':').nth(2))
            .map(str::trim)
            .filter(|gid| !gid.is_empty())
        {
            return gid.to_string();
        }
    }

    "993".to_string()
}

fn compose_base_args(repo_root: &Path, compose_file: &Path) -> Vec<String> {
    vec![
        "compose".into(),
        "--project-directory".into(),
        repo_root.display().to_string(),
        "-f".into(),
        compose_file.display().to_string(),
    ]
}

fn packer_init_args(base_args: &[String], service: &str) -> Vec<String> {
    let mut args = base_args.to_vec();
    args.extend([
        "run".into(),
        "--rm".into(),
        service.into(),
        "packer".into(),
        "init".into(),
        "images/".into(),
    ]);
    args
}

fn packer_build_args(
    base_args: &[String],
    service: &str,
    rel_key_path: &str,
    public_key: &str,
) -> Vec<String> {
    let mut args = base_args.to_vec();
    args.extend([
        "run".into(),
        "--rm".into(),
        service.into(),
        "packer".into(),
        "build".into(),
        "-var".into(),
        "accelerator=kvm".into(),
        "-var".into(),
        format!("ssh_private_key_file={rel_key_path}"),
        "-var".into(),
        format!("ssh_public_key={public_key}"),
        "images/".into(),
    ]);
    args
}

fn compress_args(
    base_args: &[String],
    service: &str,
    rel_source: &str,
    rel_target: &str,
) -> Vec<String> {
    let mut args = base_args.to_vec();
    args.extend([
        "run".into(),
        "--rm".into(),
        service.into(),
        "qemu-img".into(),
        "convert".into(),
        "-O".into(),
        "qcow2".into(),
        "-c".into(),
        rel_source.into(),
        rel_target.into(),
    ]);
    args
}

fn run_capture(program: &str, args: &[&str]) -> Result<String> {
    let output = Command::new(program)
        .args(args)
        .output()
        .with_context(|| format!("failed to execute {program}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("{program} failed: {}", stderr.trim());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn getent_group_kvm_output() -> Option<String> {
    Command::new("getent")
        .args(["group", "kvm"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
}

fn run_command(
    program: &str,
    args: &[String],
    envs: &[(&str, &str)],
    failure_context: &str,
) -> Result<()> {
    let status = Command::new(program)
        .args(args)
        .envs(envs.iter().copied())
        .status()
        .with_context(|| format!("failed to execute {program}"))?;
    if !status.success() {
        bail!("{failure_context} (exit status: {status})");
    }
    Ok(())
}

fn resolve_cache_dir(
    shasset_cache: Option<OsString>,
    xdg_cache_home: Option<OsString>,
    home: Option<OsString>,
) -> PathBuf {
    if let Some(dir) = shasset_cache.filter(|s| !s.is_empty()) {
        return PathBuf::from(dir);
    }
    if let Some(xdg) = xdg_cache_home.filter(|s| !s.is_empty()) {
        return PathBuf::from(xdg).join("shasset");
    }
    if let Some(home) = home.filter(|s| !s.is_empty()) {
        return PathBuf::from(home).join(".cache").join("shasset");
    }
    PathBuf::from(".cache").join("shasset")
}

fn default_cache_dir() -> PathBuf {
    resolve_cache_dir(
        std::env::var_os("SHASSET_CACHE"),
        std::env::var_os("XDG_CACHE_HOME"),
        std::env::var_os("HOME"),
    )
}

fn materialize_flat(
    blob_path: &Path,
    out_dir: &Path,
    filename: &str,
    executable: bool,
) -> Result<PathBuf> {
    let file_path = Path::new(filename);
    let components: Vec<Component<'_>> = file_path.components().collect();
    if components.len() != 1 || !matches!(components[0], Component::Normal(_)) {
        bail!("asset filename must be a flat filename, got: {filename}");
    }

    std::fs::create_dir_all(out_dir)
        .with_context(|| format!("cannot create output dir: {}", out_dir.display()))?;
    let out_path = out_dir.join(filename);
    let tmp_out = out_dir.join(format!(
        ".{}-{}.tmp",
        filename,
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));

    std::fs::copy(blob_path, &tmp_out).with_context(|| {
        format!(
            "cannot materialize cached blob from {} to {}",
            blob_path.display(),
            tmp_out.display()
        )
    })?;

    #[cfg(unix)]
    if executable {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&tmp_out)
            .with_context(|| format!("cannot stat temp output: {}", tmp_out.display()))?
            .permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&tmp_out, perms)
            .with_context(|| format!("cannot set executable mode on {}", tmp_out.display()))?;
    }

    #[cfg(not(unix))]
    let _ = executable;

    if out_path.exists() {
        std::fs::remove_file(&out_path)
            .with_context(|| format!("cannot replace output file: {}", out_path.display()))?;
    }

    std::fs::rename(&tmp_out, &out_path).with_context(|| {
        format!(
            "cannot atomically materialize output from {} to {}",
            tmp_out.display(),
            out_path.display()
        )
    })?;

    Ok(out_path)
}

#[cfg(test)]
mod tests {
    use super::{materialize_flat, packer_build_args, repo_relative_path, resolve_host_kvm_gid};
    use std::path::Path;
    use tempfile::TempDir;

    #[test]
    fn materialize_flat_writes_expected_path() {
        let tmp = TempDir::new().unwrap();
        let blob = tmp.path().join("blob");
        let out = tmp.path().join("out");
        std::fs::write(&blob, b"hello").unwrap();

        let path = materialize_flat(&blob, &out, "tool.bin", false).unwrap();
        assert_eq!(path, out.join("tool.bin"));
        assert_eq!(std::fs::read(&path).unwrap(), b"hello");
        assert!(Path::new(&out.join("tool.bin")).is_file());
    }

    #[test]
    fn materialize_flat_replaces_existing_file() {
        let tmp = TempDir::new().unwrap();
        let blob = tmp.path().join("blob");
        let out = tmp.path().join("out");
        std::fs::create_dir_all(&out).unwrap();
        std::fs::write(&blob, b"new-bytes").unwrap();
        std::fs::write(out.join("asset"), b"old-bytes").unwrap();

        materialize_flat(&blob, &out, "asset", false).unwrap();
        assert_eq!(std::fs::read(out.join("asset")).unwrap(), b"new-bytes");
    }

    #[test]
    fn materialize_flat_rejects_non_flat_name() {
        let tmp = TempDir::new().unwrap();
        let blob = tmp.path().join("blob");
        let out = tmp.path().join("out");
        std::fs::write(&blob, b"hello").unwrap();

        assert!(materialize_flat(&blob, &out, "nested/asset", false).is_err());
        assert!(materialize_flat(&blob, &out, "../asset", false).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn materialize_flat_sets_executable_mode() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new().unwrap();
        let blob = tmp.path().join("blob");
        let out = tmp.path().join("out");
        std::fs::write(&blob, b"hello").unwrap();

        let path = materialize_flat(&blob, &out, "tool", true).unwrap();
        let mode = std::fs::metadata(path).unwrap().permissions().mode();
        assert_eq!(mode & 0o111, 0o111);
    }

    #[test]
    fn repo_relative_path_returns_relative_path_for_inside_path() {
        let relative = repo_relative_path(
            Path::new("/repo/root"),
            Path::new("/repo/root/build/packer_ssh_key"),
        )
        .unwrap();
        assert_eq!(relative, "build/packer_ssh_key");
    }

    #[test]
    fn repo_relative_path_normalizes_dots_and_parents() {
        let relative = repo_relative_path(
            Path::new("/repo/root"),
            Path::new("/repo/root/build/./nested/../packer_ssh_key"),
        )
        .unwrap();
        assert_eq!(relative, "build/packer_ssh_key");
    }

    #[test]
    fn repo_relative_path_rejects_outside_path() {
        let err =
            repo_relative_path(Path::new("/repo/root"), Path::new("/repo/other/key")).unwrap_err();
        assert!(err.to_string().contains("outside repo root"));
    }

    #[test]
    fn resolve_host_kvm_gid_prefers_env_override() {
        let gid = resolve_host_kvm_gid(Some("1234".into()), Some("kvm:x:55:".into()));
        assert_eq!(gid, "1234");
    }

    #[test]
    fn resolve_host_kvm_gid_parses_getent_output() {
        let gid = resolve_host_kvm_gid(None, Some("kvm:x:77:qemu".into()));
        assert_eq!(gid, "77");
    }

    #[test]
    fn resolve_host_kvm_gid_falls_back_to_default() {
        let gid = resolve_host_kvm_gid(None, None);
        assert_eq!(gid, "993");
    }

    #[test]
    fn packer_build_args_match_expected_argv() {
        let args = packer_build_args(
            &[
                "compose".into(),
                "--project-directory".into(),
                "/repo/root".into(),
                "-f".into(),
                "/repo/root/compose.yaml".into(),
            ],
            "tools-kvm",
            "build/packer_ssh_key",
            "ssh-ed25519 AAAA example",
        );
        assert_eq!(
            args,
            vec![
                "compose",
                "--project-directory",
                "/repo/root",
                "-f",
                "/repo/root/compose.yaml",
                "run",
                "--rm",
                "tools-kvm",
                "packer",
                "build",
                "-var",
                "accelerator=kvm",
                "-var",
                "ssh_private_key_file=build/packer_ssh_key",
                "-var",
                "ssh_public_key=ssh-ed25519 AAAA example",
                "images/",
            ]
        );
    }
}
