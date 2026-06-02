use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(
    name = "shasset",
    about = "Generic verified-asset downloader and registry manager",
    long_about = "shasset maintains a declarative manifest of named assets (each with a URL, \
version, and mandatory checksum) and downloads + verifies those assets.\n\n\
Run `shasset <COMMAND> --help` for details on each command."
)]
pub struct Cli {
    /// Path to the manifest file.
    #[arg(long, short = 'c', default_value = "shasset.yaml", global = true)]
    pub config: PathBuf,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Add or update an asset in the manifest.
    Add(AddArgs),
    /// Remove an asset from the manifest.
    Remove(RemoveArgs),
    /// Show one or all assets.
    Get(GetArgs),
    /// Download one or all assets, verifying checksums.
    Fetch(FetchArgs),
    /// Prune unreferenced cache blobs and stale quarantine entries.
    Prune(PruneArgs),
    /// Verify on-disk files against manifest checksums (no network).
    Verify(VerifyArgs),
}

// ── add ──────────────────────────────────────────────────────────────────────

#[derive(clap::Args, Debug)]
pub struct AddArgs {
    /// Name of the asset.
    pub name: String,
    /// Download URL (may contain `${version}`).
    #[arg(long)]
    pub url: String,
    /// Version string (used to expand `${version}` in url/filename).
    #[arg(long)]
    pub version: String,
    /// Checksum in `sha256:<hex>` format. Mutually exclusive with `--compute`.
    #[arg(long, conflicts_with = "compute")]
    pub checksum: Option<String>,
    /// Download the asset, compute its sha256 checksum, and store it.
    #[arg(long, conflicts_with = "checksum")]
    pub compute: bool,
    /// Forced output filename. When absent the URL basename is used.
    #[arg(long)]
    pub filename: Option<String>,
    /// Auth template, e.g. `${GH_TOKEN}`. Stored as-is; resolved at fetch time.
    #[arg(long)]
    pub auth: Option<String>,
    /// Cache directory (default: `~/.cache/shasset`).
    #[arg(long)]
    pub cache_dir: Option<PathBuf>,
}

// ── remove ───────────────────────────────────────────────────────────────────

#[derive(clap::Args, Debug)]
pub struct RemoveArgs {
    /// Name of the asset to remove.
    pub name: String,
}

// ── get ──────────────────────────────────────────────────────────────────────

#[derive(clap::Args, Debug)]
pub struct GetArgs {
    /// Asset name to show (all assets if omitted).
    pub name: Option<String>,
    /// Output as JSON.
    #[arg(long)]
    pub json: bool,
}

// ── fetch ─────────────────────────────────────────────────────────────────────

#[derive(clap::Args, Debug)]
pub struct FetchArgs {
    /// Asset name to fetch (all assets if omitted).
    pub name: Option<String>,
    /// Output directory; required. Each asset is written to `<out>/<name>/<filename>`.
    #[arg(long, required = true)]
    pub out: PathBuf,
    /// Cache directory (default: `~/.cache/shasset`).
    #[arg(long)]
    pub cache_dir: Option<PathBuf>,
    /// Override concurrency (parallel downloads).
    #[arg(long)]
    pub concurrency: Option<usize>,
    /// Materialize fetched files as symlinks into the cache instead of copying.
    #[arg(long)]
    pub link: bool,
    /// Skip re-verifying cache blobs before use.
    #[arg(long)]
    pub no_reverify: bool,
}

// ── prune ─────────────────────────────────────────────────────────────────────

#[derive(clap::Args, Debug)]
pub struct PruneArgs {
    /// Cache directory (default: `~/.cache/shasset`).
    #[arg(long)]
    pub cache_dir: Option<PathBuf>,
    /// Show what would be removed without deleting anything.
    #[arg(long)]
    pub dry_run: bool,
}

// ── verify ────────────────────────────────────────────────────────────────────

#[derive(clap::Args, Debug)]
pub struct VerifyArgs {
    /// Asset name to verify (all assets if omitted).
    pub name: Option<String>,
    /// Directory to verify against; `<out>/<name>/<filename>`.
    #[arg(long, required = true)]
    pub out: PathBuf,
    /// Output as JSON.
    #[arg(long)]
    pub json: bool,
}
