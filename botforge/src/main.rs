use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use serde::Deserialize;
use shasset::fetch::{fetch_asset, FetchParams, MaterializeMode};
use shasset::manifest::{load, Asset};
use std::ffi::OsString;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Component, Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

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
    /// Launch a VM with qemu (KVM-only).
    Run(RunArgs),
    /// Boot and validate a packed VM from a test config.
    Test(TestArgs),
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
    /// Source directory tree to include in the ISO (required in plain mode; ignored in seed mode).
    #[arg(long)]
    src: Option<PathBuf>,
    /// Output ISO file path.
    #[arg(long, required = true)]
    out: PathBuf,
    /// ISO volume ID.
    #[arg(long, default_value = "BOTFORGE")]
    volume_id: String,
    /// Inject this SSH public key into generated cloud-init user-data.
    #[arg(long)]
    ssh_public_key: Option<String>,
    /// Read SSH public key from this file and inject into generated cloud-init user-data.
    #[arg(long)]
    ssh_public_key_file: Option<PathBuf>,
    /// Optional cloud-init user-data template; replaces REPLACE_WITH_SSH_PUBLIC_KEY.
    #[arg(long)]
    user_data_template: Option<PathBuf>,
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

#[derive(clap::Args, Debug)]
struct RunArgs {
    /// Base qcow2 image path.
    #[arg(long, required = true)]
    base_image: PathBuf,
    /// Overlay qcow2 image path to create.
    #[arg(long, required = true)]
    overlay_image: PathBuf,
    /// NoCloud seed ISO path.
    #[arg(long, required = true)]
    seed_iso: PathBuf,
    /// Optional payload ISO path.
    #[arg(long)]
    payload_iso: Option<PathBuf>,
    /// Host SSH forward port to guest 22.
    #[arg(long, default_value_t = 2222)]
    ssh_port: u16,
    /// Run qemu in the foreground.
    #[arg(long)]
    foreground: bool,
}

#[derive(clap::Args, Debug)]
struct TestArgs {
    /// Path to test.yaml config.
    #[arg(long = "test-config", required = true)]
    test_config: PathBuf,
    /// Base qcow2 image path.
    #[arg(long, required = true)]
    base_image: PathBuf,
    /// SSH private key path for guest access.
    #[arg(long, required = true)]
    ssh_key: PathBuf,
    /// SSH host forwarded port.
    #[arg(long, default_value_t = 2222)]
    ssh_port: u16,
    /// SSH host.
    #[arg(long, default_value = "127.0.0.1")]
    ssh_host: String,
    /// SSH user.
    #[arg(long, default_value = "bot")]
    ssh_user: String,
    /// Repo root for resolving relative test paths (default: current dir).
    #[arg(long)]
    repo_root: Option<PathBuf>,
    /// Leave VM running and preserve overlay on exit.
    #[arg(long)]
    keep_running: bool,
}

#[derive(Debug, Deserialize, Default)]
struct TestConfig {
    #[serde(default)]
    isos: Vec<PathBuf>,
    #[serde(default)]
    steps: Vec<TestStep>,
    #[serde(default)]
    diagnostics_units: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct TestStep {
    name: String,
    #[serde(default)]
    uploads: Vec<TestUpload>,
    run: String,
}

#[derive(Debug, Deserialize)]
struct TestUpload {
    src: PathBuf,
    dest: String,
}

const USER_DATA_PLACEHOLDER: &str = "REPLACE_WITH_SSH_PUBLIC_KEY";
const TEST_SSH_READY_TIMEOUT: Duration = Duration::from_secs(300);
const TEST_CLOUD_INIT_TIMEOUT: Duration = Duration::from_secs(300);
const TEST_TRANSPORT_RETRIES: usize = 10;
const TEST_TRANSPORT_RETRY_DELAY: Duration = Duration::from_secs(2);
const TEST_STABLE_SSH_ATTEMPTS: usize = 5;
const TEST_STABLE_SSH_REQUIRED: usize = 2;

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
        Commands::Run(args) => cmd_run(args),
        Commands::Test(args) => cmd_test(args),
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
        let expanded_uri = asset.expanded_uri();
        if expanded_uri.starts_with("oci://") {
            stage_oci_asset(name, asset, &args.out)
                .with_context(|| format!("failed to stage oci asset '{name}'"))?;
            continue;
        }

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

#[derive(Debug, Clone, PartialEq, Eq)]
struct OciRef {
    ref_with_digest: String,
    digest: String,
}

fn parse_oci_uri(expanded_uri: &str) -> Result<OciRef> {
    let ref_with_digest = expanded_uri
        .strip_prefix("oci://")
        .with_context(|| format!("uri must use oci:// scheme: {expanded_uri}"))?;
    if ref_with_digest.is_empty() {
        bail!("oci uri is missing image reference: {expanded_uri}");
    }
    let (image_ref, digest) = ref_with_digest
        .rsplit_once('@')
        .with_context(|| format!("oci uri must include digest @sha256:<64-hex>: {expanded_uri}"))?;
    if image_ref.is_empty() {
        bail!("oci uri is missing image reference before digest: {expanded_uri}");
    }
    validate_sha256_digest(digest)?;
    Ok(OciRef {
        ref_with_digest: ref_with_digest.to_string(),
        digest: digest.to_string(),
    })
}

fn validate_sha256_digest(digest: &str) -> Result<()> {
    let hex = digest
        .strip_prefix("sha256:")
        .with_context(|| format!("digest must start with sha256:, got: {digest}"))?;
    if hex.len() != 64 {
        bail!(
            "sha256 digest must have 64 hex chars, got {}: {digest}",
            hex.len()
        );
    }
    if !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        bail!("sha256 digest contains non-hex characters: {digest}");
    }
    Ok(())
}

fn local_tag_for(asset_key: &str) -> String {
    format!("botwork/{asset_key}:local")
}

fn image_tarball_name(asset_key: &str, filename: Option<&str>) -> Result<String> {
    let name = filename
        .map(ToString::to_string)
        .unwrap_or_else(|| format!("{asset_key}.tar"));
    validate_flat_filename(&name)?;
    Ok(name)
}

fn docker_pull_args(ref_with_digest: &str) -> Vec<String> {
    vec!["pull".into(), ref_with_digest.into()]
}

fn docker_tag_args(ref_with_digest: &str, local_tag: &str) -> Vec<String> {
    vec!["tag".into(), ref_with_digest.into(), local_tag.into()]
}

fn docker_save_args(local_tag: &str, out_tarball_path: &Path) -> Vec<String> {
    vec![
        "save".into(),
        local_tag.into(),
        "-o".into(),
        out_tarball_path.display().to_string(),
    ]
}

/// Stage an `oci://` asset by pulling from a registry, tagging locally, then saving
/// an image tarball into the flat deps output directory.
///
/// This is intentionally registry-only for v1. A future `dev-pack` flow can provide
/// sibling/earthly image resolution while preserving the local-tag + tarball contract.
fn stage_oci_asset(asset_key: &str, asset: &Asset, out_dir: &Path) -> Result<PathBuf> {
    if !command_exists("docker") {
        bail!("asset '{asset_key}' uses oci:// but 'docker' is not available on PATH");
    }

    let expanded_uri = asset.expanded_uri();
    let oci = parse_oci_uri(&expanded_uri)
        .with_context(|| format!("asset '{asset_key}' has invalid oci uri"))?;
    if asset.checksum.is_some() {
        eprintln!("warning: asset '{asset_key}' has checksum but oci:// ignores checksum");
    }

    let expanded_filename =
        if asset.filename.is_some() {
            Some(asset.output_filename().with_context(|| {
                format!("asset '{asset_key}': cannot determine output filename")
            })?)
        } else {
            None
        };
    let tarball_name = image_tarball_name(asset_key, expanded_filename.as_deref())?;
    std::fs::create_dir_all(out_dir)
        .with_context(|| format!("cannot create output dir: {}", out_dir.display()))?;

    let final_path = out_dir.join(&tarball_name);
    let tmp_path = out_dir.join(format!(
        ".{}-{}.tmp",
        tarball_name,
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));

    let local_tag = local_tag_for(asset_key);
    run_command(
        "docker",
        &docker_pull_args(&oci.ref_with_digest),
        &[],
        &format!(
            "asset '{asset_key}': docker pull failed for {}",
            oci.ref_with_digest
        ),
    )?;
    run_command(
        "docker",
        &docker_tag_args(&oci.ref_with_digest, &local_tag),
        &[],
        &format!("asset '{asset_key}': docker tag failed"),
    )?;
    if let Err(err) = run_command(
        "docker",
        &docker_save_args(&local_tag, &tmp_path),
        &[],
        &format!("asset '{asset_key}': docker save failed"),
    ) {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(err);
    }

    if final_path.exists() {
        std::fs::remove_file(&final_path)
            .with_context(|| format!("cannot replace output file: {}", final_path.display()))?;
    }
    std::fs::rename(&tmp_path, &final_path).with_context(|| {
        format!(
            "cannot atomically materialize output from {} to {}",
            tmp_path.display(),
            final_path.display()
        )
    })?;

    println!(
        "pulled '{}' {} → {} (tag {})",
        asset_key,
        oci.ref_with_digest,
        final_path.display(),
        local_tag
    );
    Ok(final_path)
}

fn cmd_iso(args: IsoArgs) -> Result<()> {
    let ssh_public_key = read_ssh_public_key(args.ssh_public_key, args.ssh_public_key_file)?;
    if let Some(key) = ssh_public_key {
        let template_content = args
            .user_data_template
            .as_ref()
            .map(|path| {
                std::fs::read_to_string(path)
                    .with_context(|| format!("cannot read user-data template: {}", path.display()))
            })
            .transpose()?;
        let temp_dir = create_temp_dir("botforge-seed")?;
        let user_data = render_user_data(template_content.as_deref(), &key, None);
        write_seed_files(&temp_dir, &user_data)?;
        build_iso(&temp_dir, &args.out, &args.volume_id)?;
        std::fs::remove_dir_all(&temp_dir)
            .with_context(|| format!("cannot remove temp seed dir: {}", temp_dir.display()))?;
    } else {
        let src = args.src.ok_or_else(|| {
            anyhow::anyhow!(
                "--src is required when no SSH key flag (--ssh-public-key or --ssh-public-key-file) is provided"
            )
        })?;
        if !src.is_dir() {
            bail!("source directory does not exist: {}", src.display());
        }
        build_iso(&src, &args.out, &args.volume_id)?;
    }

    println!("built ISO at {}", args.out.display());
    Ok(())
}

fn read_ssh_public_key(
    ssh_public_key: Option<String>,
    ssh_public_key_file: Option<PathBuf>,
) -> Result<Option<String>> {
    match (ssh_public_key, ssh_public_key_file) {
        (Some(_), Some(_)) => {
            bail!("provide only one of --ssh-public-key or --ssh-public-key-file")
        }
        (Some(key), None) => Ok(Some(key.trim().to_string())),
        (None, Some(path)) => {
            let key = std::fs::read_to_string(&path)
                .with_context(|| format!("cannot read SSH public key file: {}", path.display()))?;
            Ok(Some(key.trim().to_string()))
        }
        (None, None) => Ok(None),
    }
}

fn render_user_data(
    template: Option<&str>,
    ssh_public_key: &str,
    ssh_user: Option<&str>,
) -> String {
    if let Some(template) = template {
        return template.replace(USER_DATA_PLACEHOLDER, ssh_public_key);
    }
    if let Some(user) = ssh_user {
        return format!(
            "#cloud-config\nusers:\n  - default\n  - name: {user}\n    ssh_authorized_keys:\n      - {ssh_public_key}\n"
        );
    }
    format!("#cloud-config\nssh_authorized_keys:\n  - {ssh_public_key}\n")
}

fn write_seed_files(seed_dir: &Path, user_data: &str) -> Result<()> {
    std::fs::create_dir_all(seed_dir)
        .with_context(|| format!("cannot create seed dir: {}", seed_dir.display()))?;
    std::fs::write(
        seed_dir.join("meta-data"),
        "instance-id: iid-local01\nlocal-hostname: botforge\n",
    )
    .with_context(|| format!("cannot write seed meta-data in {}", seed_dir.display()))?;
    std::fs::write(seed_dir.join("user-data"), user_data)
        .with_context(|| format!("cannot write seed user-data in {}", seed_dir.display()))?;
    Ok(())
}

fn build_iso(src_dir: &Path, out: &Path, volume_id: &str) -> Result<()> {
    if !src_dir.is_dir() {
        bail!("source directory does not exist: {}", src_dir.display());
    }
    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("cannot create output dir: {}", parent.display()))?;
    }

    let tool = detect_iso_tool()?;
    let file_name = out
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("out.iso");
    let tmp_out = out
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(format!(".{file_name}.{}.tmp", unique_suffix()));

    let args = iso_args(tool, src_dir, &tmp_out, volume_id)?;
    if let Err(err) = run_command(tool, &args, &[], &format!("{tool} failed")) {
        let _ = std::fs::remove_file(&tmp_out);
        return Err(err);
    }

    if out.exists() {
        std::fs::remove_file(out)
            .with_context(|| format!("cannot replace output file: {}", out.display()))?;
    }
    std::fs::rename(&tmp_out, out).with_context(|| {
        format!(
            "cannot atomically materialize output from {} to {}",
            tmp_out.display(),
            out.display()
        )
    })?;
    Ok(())
}

fn iso_args(tool: &str, src_dir: &Path, out: &Path, volume_id: &str) -> Result<Vec<String>> {
    let mut args = match tool {
        "xorriso" => vec!["-as".into(), "mkisofs".into()],
        "genisoimage" => Vec::new(),
        _ => bail!("unsupported iso tool '{tool}'"),
    };
    args.extend([
        "-r".into(),
        "-J".into(),
        "-V".into(),
        volume_id.into(),
        "-o".into(),
        out.display().to_string(),
        src_dir.display().to_string(),
    ]);
    Ok(args)
}

fn create_temp_dir(prefix: &str) -> Result<PathBuf> {
    let base = std::env::temp_dir();
    let path = base.join(format!("{prefix}-{}", unique_suffix()));
    std::fs::create_dir_all(&path)
        .with_context(|| format!("cannot create temp dir: {}", path.display()))?;
    Ok(path)
}

fn unique_suffix() -> String {
    format!(
        "{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    )
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

fn cmd_run(args: RunArgs) -> Result<()> {
    require_kvm()?;
    ensure_command("qemu-system-x86_64")?;
    ensure_command("qemu-img")?;

    let base_image = normalize_path(&args.base_image);
    let overlay_image = normalize_path(&args.overlay_image);
    let seed_iso = normalize_path(&args.seed_iso);
    let payload_isos: Vec<PathBuf> = args
        .payload_iso
        .as_ref()
        .map(|path| vec![normalize_path(path)])
        .unwrap_or_default();

    create_overlay_image(&base_image, &overlay_image)?;
    let mut qemu_args = qemu_run_args(
        &overlay_image,
        &seed_iso,
        &payload_isos,
        args.ssh_port,
    );
    if !args.foreground {
        qemu_args.push("-daemonize".into());
    }

    run_command("qemu-system-x86_64", &qemu_args, &[], "qemu launch failed")?;
    Ok(())
}

fn cmd_test(args: TestArgs) -> Result<()> {
    require_kvm()?;
    ensure_command("qemu-system-x86_64")?;
    ensure_command("qemu-img")?;
    ensure_command("ssh")?;
    ensure_command("scp")?;
    detect_iso_tool()?;

    let repo_root = std::fs::canonicalize(
        args.repo_root
            .unwrap_or(std::env::current_dir().context("failed to determine current directory")?),
    )
    .context("failed to resolve repo root")?;
    let test_config_path = resolve_under_root(&repo_root, args.test_config);
    let base_image = resolve_under_root(&repo_root, args.base_image);
    let ssh_key = resolve_under_root(&repo_root, args.ssh_key);
    let ssh_pub = PathBuf::from(format!("{}.pub", ssh_key.display()));

    let test_config = load_test_config(&test_config_path)?;
    let build_dir = repo_root.join("build");
    std::fs::create_dir_all(&build_dir)
        .with_context(|| format!("cannot create build dir: {}", build_dir.display()))?;
    let overlay_image = build_dir.join("test-overlay.qcow2");
    let seed_iso = build_dir.join("test-seed.iso");
    let vm_log = build_dir.join("test-vm.log");
    let seed_dir = create_temp_dir("botforge-test-seed")?;

    let ssh_public_key = std::fs::read_to_string(&ssh_pub)
        .with_context(|| format!("cannot read SSH public key: {}", ssh_pub.display()))?;
    let user_data = render_user_data(None, ssh_public_key.trim(), Some(args.ssh_user.as_str()));
    write_seed_files(&seed_dir, &user_data)?;
    build_iso(&seed_dir, &seed_iso, "cidata")?;
    std::fs::remove_dir_all(&seed_dir)
        .with_context(|| format!("cannot remove temp seed dir: {}", seed_dir.display()))?;

    create_overlay_image(&base_image, &overlay_image)?;

    let mut extra_isos = Vec::new();
    for iso in &test_config.isos {
        extra_isos.push(resolve_under_root(&repo_root, iso.clone()));
    }
    let qemu_args = qemu_run_args(
        &overlay_image,
        &seed_iso,
        &extra_isos,
        args.ssh_port,
    );

    let mut vm_child = Some(spawn_qemu_with_log(&qemu_args, &vm_log)?);
    let ssh_options = SshOptions {
        host: args.ssh_host.clone(),
        port: args.ssh_port,
        user: args.ssh_user.clone(),
        key: ssh_key.clone(),
    };

    let test_result = run_test_flow(&repo_root, &test_config, &ssh_options);
    if let Err(err) = test_result {
        eprintln!("test failed: {err:#}");
        collect_test_diagnostics(&ssh_options, &test_config.diagnostics_units);
        print_log_tail(&vm_log, 200);
        if !args.keep_running {
            cleanup_test(&mut vm_child, &overlay_image);
        }
        return Err(err);
    }

    if !args.keep_running {
        cleanup_test(&mut vm_child, &overlay_image);
    }
    println!("test passed");
    Ok(())
}

#[derive(Clone)]
struct SshOptions {
    host: String,
    port: u16,
    user: String,
    key: PathBuf,
}

fn run_test_flow(repo_root: &Path, config: &TestConfig, ssh: &SshOptions) -> Result<()> {
    wait_for_ssh(ssh, TEST_SSH_READY_TIMEOUT)?;
    ssh_with_retry(
        ssh,
        "sudo cloud-init status --wait",
        TEST_TRANSPORT_RETRIES,
        TEST_TRANSPORT_RETRY_DELAY,
        TEST_CLOUD_INIT_TIMEOUT,
    )?;
    require_stable_ssh(ssh, TEST_STABLE_SSH_ATTEMPTS, TEST_STABLE_SSH_REQUIRED)?;

    for step in &config.steps {
        for upload in &step.uploads {
            let src = resolve_under_root(repo_root, upload.src.clone());
            scp_with_retry(
                ssh,
                &src,
                &upload.dest,
                TEST_TRANSPORT_RETRIES,
                TEST_TRANSPORT_RETRY_DELAY,
            )
            .with_context(|| format!("test step '{}' upload failed", step.name))?;
        }
        ssh_with_retry(
            ssh,
            &step.run,
            TEST_TRANSPORT_RETRIES,
            TEST_TRANSPORT_RETRY_DELAY,
            Duration::from_secs(300),
        )
        .with_context(|| format!("test step '{}' command failed", step.name))?;
    }
    Ok(())
}

fn load_test_config(path: &Path) -> Result<TestConfig> {
    let yaml = std::fs::read_to_string(path)
        .with_context(|| format!("cannot read test config: {}", path.display()))?;
    serde_yaml::from_str(&yaml).with_context(|| format!("invalid test config: {}", path.display()))
}

fn require_kvm() -> Result<()> {
    if !Path::new("/dev/kvm").exists() {
        bail!("KVM is required: /dev/kvm not found");
    }
    Ok(())
}

fn ensure_command(program: &str) -> Result<()> {
    if !command_exists(program) {
        bail!("'{program}' is not available on PATH");
    }
    Ok(())
}

fn create_overlay_image(base_image: &Path, overlay_image: &Path) -> Result<()> {
    if !base_image.is_file() {
        bail!("base image not found: {}", base_image.display());
    }
    if let Some(parent) = overlay_image.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("cannot create overlay dir: {}", parent.display()))?;
    }
    let args = qemu_img_create_args(base_image, overlay_image);
    run_command("qemu-img", &args, &[], "qemu-img create overlay failed")
}

fn qemu_img_create_args(base_image: &Path, overlay_image: &Path) -> Vec<String> {
    vec![
        "create".into(),
        "-f".into(),
        "qcow2".into(),
        "-F".into(),
        "qcow2".into(),
        "-b".into(),
        base_image.display().to_string(),
        overlay_image.display().to_string(),
    ]
}

fn qemu_run_args(
    overlay_image: &Path,
    seed_iso: &Path,
    extra_isos: &[PathBuf],
    ssh_port: u16,
) -> Vec<String> {
    let mut args = vec![
        "-accel".into(),
        "kvm".into(),
        "-m".into(),
        "2048".into(),
        "-smp".into(),
        "2".into(),
        "-cpu".into(),
        "host".into(),
        "-drive".into(),
        format!("file={},if=virtio,format=qcow2", overlay_image.display()),
        "-drive".into(),
        format!("file={},media=cdrom,readonly=on", seed_iso.display()),
    ];
    for iso in extra_isos {
        args.push("-drive".into());
        args.push(format!("file={},media=cdrom,readonly=on", iso.display()));
    }
    args.extend([
        "-netdev".into(),
        format!("user,id=net0,hostfwd=tcp:127.0.0.1:{ssh_port}-:22"),
        "-device".into(),
        "virtio-net-pci,netdev=net0".into(),
        "-nographic".into(),
    ]);
    args
}

fn spawn_qemu_with_log(args: &[String], log_path: &Path) -> Result<Child> {
    let log = File::create(log_path)
        .with_context(|| format!("cannot create VM log file: {}", log_path.display()))?;
    let log_err = log
        .try_clone()
        .with_context(|| format!("cannot clone VM log file handle: {}", log_path.display()))?;
    Command::new("qemu-system-x86_64")
        .args(args)
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err))
        .spawn()
        .context("failed to launch qemu in background")
}

fn ssh_command_args(
    ssh: &SshOptions,
    remote_command: &str,
    connect_timeout_secs: u64,
) -> Vec<String> {
    vec![
        "-o".into(),
        "StrictHostKeyChecking=no".into(),
        "-o".into(),
        "UserKnownHostsFile=/dev/null".into(),
        "-o".into(),
        format!("ConnectTimeout={connect_timeout_secs}"),
        "-i".into(),
        ssh.key.display().to_string(),
        "-p".into(),
        ssh.port.to_string(),
        format!("{}@{}", ssh.user, ssh.host),
        remote_command.into(),
    ]
}

fn scp_command_args(ssh: &SshOptions, src: &Path, dest: &str) -> Vec<String> {
    vec![
        "-o".into(),
        "StrictHostKeyChecking=no".into(),
        "-o".into(),
        "UserKnownHostsFile=/dev/null".into(),
        "-i".into(),
        ssh.key.display().to_string(),
        "-P".into(),
        ssh.port.to_string(),
        src.display().to_string(),
        format!("{}@{}:{dest}", ssh.user, ssh.host),
    ]
}

fn journalctl_command(units: &[String]) -> String {
    if units.is_empty() {
        return "sudo journalctl --no-pager -n 200".into();
    }
    let mut parts = vec!["sudo journalctl".to_string()];
    for unit in units {
        parts.push(format!("-u {unit}"));
    }
    parts.push("--no-pager -n 200".to_string());
    parts.join(" ")
}

fn wait_for_ssh(ssh: &SshOptions, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        if ssh_with_retry(
            ssh,
            "true",
            1,
            Duration::from_secs(0),
            Duration::from_secs(10),
        )
        .is_ok()
        {
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!("timed out waiting for SSH");
        }
        std::thread::sleep(Duration::from_secs(2));
    }
}

fn require_stable_ssh(
    ssh: &SshOptions,
    attempts: usize,
    required_consecutive: usize,
) -> Result<()> {
    let mut consecutive = 0usize;
    for _ in 0..attempts {
        if ssh_with_retry(
            ssh,
            "true",
            1,
            Duration::from_secs(0),
            Duration::from_secs(10),
        )
        .is_ok()
        {
            consecutive += 1;
            if consecutive >= required_consecutive {
                return Ok(());
            }
        } else {
            consecutive = 0;
        }
        std::thread::sleep(Duration::from_secs(2));
    }
    bail!("SSH was not stable enough after {attempts} probes")
}

fn ssh_with_retry(
    ssh: &SshOptions,
    remote_command: &str,
    retries: usize,
    retry_delay: Duration,
    connect_timeout: Duration,
) -> Result<()> {
    let args = ssh_command_args(ssh, remote_command, connect_timeout.as_secs());
    retry_transport_cmd("ssh", &args, retries, retry_delay, "ssh command failed")
}

fn scp_with_retry(
    ssh: &SshOptions,
    src: &Path,
    dest: &str,
    retries: usize,
    retry_delay: Duration,
) -> Result<()> {
    let args = scp_command_args(ssh, src, dest);
    retry_transport_cmd("scp", &args, retries, retry_delay, "scp command failed")
}

fn retry_transport_cmd(
    program: &str,
    args: &[String],
    retries: usize,
    retry_delay: Duration,
    failure_context: &str,
) -> Result<()> {
    let mut attempts = 0usize;
    loop {
        let status = Command::new(program)
            .args(args)
            .status()
            .with_context(|| format!("failed to execute {program}"))?;
        if status.success() {
            return Ok(());
        }
        attempts += 1;
        if status.code() != Some(255) || attempts >= retries {
            bail!("{failure_context} (exit status: {status})");
        }
        std::thread::sleep(retry_delay);
    }
}

fn collect_test_diagnostics(ssh: &SshOptions, units: &[String]) {
    let _ = ssh_with_retry(
        ssh,
        "systemctl --failed",
        1,
        Duration::from_secs(0),
        Duration::from_secs(10),
    );
    let _ = ssh_with_retry(
        ssh,
        &journalctl_command(units),
        1,
        Duration::from_secs(0),
        Duration::from_secs(10),
    );
    let _ = ssh_with_retry(
        ssh,
        "cloud-init status --long",
        1,
        Duration::from_secs(0),
        Duration::from_secs(10),
    );
}

fn print_log_tail(path: &Path, line_count: usize) {
    let Ok(file) = File::open(path) else {
        return;
    };
    let lines: Vec<String> = BufReader::new(file)
        .lines()
        .map_while(|line| line.ok())
        .collect();
    let start = lines.len().saturating_sub(line_count);
    for line in &lines[start..] {
        eprintln!("{line}");
    }
}

fn cleanup_test(vm_child: &mut Option<Child>, overlay_image: &Path) {
    if let Some(child) = vm_child.as_mut() {
        let _ = child.kill();
        let _ = child.wait();
    }
    *vm_child = None;
    let _ = std::fs::remove_file(overlay_image);
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
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok()
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
    validate_flat_filename(filename)?;

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

fn validate_flat_filename(filename: &str) -> Result<()> {
    let file_path = Path::new(filename);
    let components: Vec<Component<'_>> = file_path.components().collect();
    if components.len() != 1 || !matches!(components[0], Component::Normal(_)) {
        bail!("asset filename must be a flat filename, got: {filename}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        docker_pull_args, docker_save_args, docker_tag_args, image_tarball_name, iso_args,
        journalctl_command, local_tag_for, materialize_flat, packer_build_args, parse_oci_uri,
        qemu_img_create_args, qemu_run_args, render_user_data, repo_relative_path,
        resolve_host_kvm_gid, scp_command_args, ssh_command_args, SshOptions,
    };
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

    #[test]
    fn parse_oci_uri_accepts_digest_reference() {
        let digest = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let parsed = parse_oci_uri(&format!("oci://ghcr.io/botworkz/svc@{digest}")).unwrap();
        assert_eq!(
            parsed.ref_with_digest,
            format!("ghcr.io/botworkz/svc@{digest}")
        );
        assert_eq!(parsed.digest, digest);
    }

    #[test]
    fn parse_oci_uri_accepts_tag_and_digest_reference() {
        let digest = "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        let parsed = parse_oci_uri(&format!("oci://ghcr.io/botworkz/svc:v1@{digest}")).unwrap();
        assert_eq!(
            parsed.ref_with_digest,
            format!("ghcr.io/botworkz/svc:v1@{digest}")
        );
        assert_eq!(parsed.digest, digest);
    }

    #[test]
    fn parse_oci_uri_rejects_missing_digest() {
        let err = parse_oci_uri("oci://ghcr.io/botworkz/svc:latest").unwrap_err();
        assert!(err
            .to_string()
            .contains("oci uri must include digest @sha256:<64-hex>"));
    }

    #[test]
    fn parse_oci_uri_rejects_non_oci_scheme() {
        let err = parse_oci_uri("https://example.com/tool.tar.gz").unwrap_err();
        assert!(err.to_string().contains("uri must use oci:// scheme"));
    }

    #[test]
    fn parse_oci_uri_rejects_invalid_digest_length() {
        let err = parse_oci_uri("oci://ghcr.io/botworkz/svc@sha256:deadbeef").unwrap_err();
        assert!(err
            .to_string()
            .contains("sha256 digest must have 64 hex chars"));
    }

    #[test]
    fn parse_oci_uri_rejects_invalid_digest_hex() {
        let err = parse_oci_uri(
            "oci://ghcr.io/botworkz/svc@sha256:zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz",
        )
        .unwrap_err();
        assert!(err
            .to_string()
            .contains("sha256 digest contains non-hex characters"));
    }

    #[test]
    fn local_tag_for_uses_asset_key() {
        assert_eq!(
            local_tag_for("session-broker"),
            "botwork/session-broker:local"
        );
    }

    #[test]
    fn image_tarball_name_defaults_to_key_tar() {
        assert_eq!(
            image_tarball_name("session-broker", None).unwrap(),
            "session-broker.tar"
        );
    }

    #[test]
    fn image_tarball_name_uses_filename_override() {
        assert_eq!(
            image_tarball_name("session-broker", Some("broker.tar")).unwrap(),
            "broker.tar"
        );
    }

    #[test]
    fn image_tarball_name_rejects_non_flat_name() {
        assert!(image_tarball_name("session-broker", Some("nested/broker.tar")).is_err());
    }

    #[test]
    fn docker_pull_args_match_expected_argv() {
        assert_eq!(
            docker_pull_args("ghcr.io/botworkz/svc@sha256:abc"),
            vec!["pull", "ghcr.io/botworkz/svc@sha256:abc"]
        );
    }

    #[test]
    fn docker_tag_args_match_expected_argv() {
        assert_eq!(
            docker_tag_args("ghcr.io/botworkz/svc@sha256:abc", "botwork/svc:local"),
            vec![
                "tag",
                "ghcr.io/botworkz/svc@sha256:abc",
                "botwork/svc:local"
            ]
        );
    }

    #[test]
    fn docker_save_args_match_expected_argv() {
        let out = Path::new("/tmp/out/svc.tar");
        assert_eq!(
            docker_save_args("botwork/svc:local", out),
            vec!["save", "botwork/svc:local", "-o", "/tmp/out/svc.tar"]
        );
    }

    #[test]
    fn render_user_data_replaces_placeholder() {
        let template = "#cloud-config\nssh_authorized_keys:\n  - REPLACE_WITH_SSH_PUBLIC_KEY\n";
        let rendered = render_user_data(Some(template), "ssh-ed25519 AAAA test", None);
        assert!(rendered.contains("ssh-ed25519 AAAA test"));
        assert!(!rendered.contains("REPLACE_WITH_SSH_PUBLIC_KEY"));
    }

    #[test]
    fn iso_args_xorriso_match_expected_argv() {
        let args = iso_args(
            "xorriso",
            Path::new("/tmp/src"),
            Path::new("/tmp/out.iso"),
            "cidata",
        )
        .unwrap();
        assert_eq!(
            args,
            vec![
                "-as",
                "mkisofs",
                "-r",
                "-J",
                "-V",
                "cidata",
                "-o",
                "/tmp/out.iso",
                "/tmp/src"
            ]
        );
    }

    #[test]
    fn iso_args_genisoimage_match_expected_argv() {
        let args = iso_args(
            "genisoimage",
            Path::new("/tmp/src"),
            Path::new("/tmp/out.iso"),
            "BOTFORGE",
        )
        .unwrap();
        assert_eq!(
            args,
            vec![
                "-r",
                "-J",
                "-V",
                "BOTFORGE",
                "-o",
                "/tmp/out.iso",
                "/tmp/src"
            ]
        );
    }

    #[test]
    fn qemu_img_create_args_match_expected_argv() {
        assert_eq!(
            qemu_img_create_args(Path::new("/base.qcow2"), Path::new("/overlay.qcow2")),
            vec![
                "create",
                "-f",
                "qcow2",
                "-F",
                "qcow2",
                "-b",
                "/base.qcow2",
                "/overlay.qcow2"
            ]
        );
    }

    #[test]
    fn qemu_run_args_match_expected_argv() {
        let args = qemu_run_args(
            Path::new("/overlay.qcow2"),
            Path::new("/seed.iso"),
            &[Path::new("/payload.iso").to_path_buf()],
            2222,
        );
        // base image must NOT appear in the argv
        assert!(
            !args.iter().any(|a| a.contains("/base.qcow2")),
            "base image must not appear in qemu args"
        );
        assert_eq!(
            args,
            vec![
                "-accel",
                "kvm",
                "-m",
                "2048",
                "-smp",
                "2",
                "-cpu",
                "host",
                "-drive",
                "file=/overlay.qcow2,if=virtio,format=qcow2",
                "-drive",
                "file=/seed.iso,media=cdrom,readonly=on",
                "-drive",
                "file=/payload.iso,media=cdrom,readonly=on",
                "-netdev",
                "user,id=net0,hostfwd=tcp:127.0.0.1:2222-:22",
                "-device",
                "virtio-net-pci,netdev=net0",
                "-nographic"
            ]
        );
    }

    #[test]
    fn ssh_command_args_match_expected_argv() {
        let ssh = SshOptions {
            host: "127.0.0.1".into(),
            port: 2222,
            user: "bot".into(),
            key: Path::new("/tmp/key").to_path_buf(),
        };
        assert_eq!(
            ssh_command_args(&ssh, "true", 10),
            vec![
                "-o",
                "StrictHostKeyChecking=no",
                "-o",
                "UserKnownHostsFile=/dev/null",
                "-o",
                "ConnectTimeout=10",
                "-i",
                "/tmp/key",
                "-p",
                "2222",
                "bot@127.0.0.1",
                "true"
            ]
        );
    }

    #[test]
    fn scp_command_args_match_expected_argv() {
        let ssh = SshOptions {
            host: "127.0.0.1".into(),
            port: 2222,
            user: "bot".into(),
            key: Path::new("/tmp/key").to_path_buf(),
        };
        assert_eq!(
            scp_command_args(&ssh, Path::new("/tmp/local"), "/tmp/remote"),
            vec![
                "-o",
                "StrictHostKeyChecking=no",
                "-o",
                "UserKnownHostsFile=/dev/null",
                "-i",
                "/tmp/key",
                "-P",
                "2222",
                "/tmp/local",
                "bot@127.0.0.1:/tmp/remote"
            ]
        );
    }

    #[test]
    fn journalctl_command_includes_units() {
        let cmd = journalctl_command(&["ssh".to_string(), "botwork-launcher".to_string()]);
        assert_eq!(
            cmd,
            "sudo journalctl -u ssh -u botwork-launcher --no-pager -n 200"
        );
    }
}
