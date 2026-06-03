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
}

#[derive(clap::Args, Debug)]
struct DepsArgs {
    /// Asset name to fetch (all assets if omitted).
    name: Option<String>,
    /// Flat output directory; each asset is materialized to `<out>/<asset-name>`.
    #[arg(long, required = true)]
    out: PathBuf,
    /// Cache directory (default: `~/.cache/shasset`).
    #[arg(long)]
    cache_dir: Option<PathBuf>,
    /// Skip re-verifying cache blobs before use.
    #[arg(long)]
    no_reverify: bool,
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

        let out_path = materialize_flat(&fetched.blob_path, &args.out, name, false)
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
        bail!("asset name must be a flat filename, got: {filename}");
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
    use super::materialize_flat;
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
}
