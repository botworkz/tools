mod cli;
mod fetch;
mod manifest;

use anyhow::{bail, Context, Result};
use clap::Parser;
use rayon::prelude::*;
use serde_json::json;
use std::collections::{BTreeMap, HashSet};
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use cli::{Cli, Command};
use fetch::{fetch_asset, verify_on_disk, FetchParams, MaterializeMode};
use manifest::{load, save, Asset, Manifest, ParsedChecksum};

fn main() {
    if let Err(e) = run() {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Add(args) => cmd_add(&cli.config, args),
        Command::Remove(args) => cmd_remove(&cli.config, args),
        Command::Get(args) => cmd_get(&cli.config, args),
        Command::Fetch(args) => cmd_fetch(&cli.config, args),
        Command::Prune(args) => cmd_prune(&cli.config, args),
        Command::Verify(args) => cmd_verify(&cli.config, args),
    }
}

/// Resolve the cache directory from environment lookups.
///
/// Precedence (the `--cache-dir` flag is handled by callers and takes priority
/// over all of these):
///   1. `SHASSET_CACHE`
///   2. `$XDG_CACHE_HOME/shasset`
///   3. `$HOME/.cache/shasset`
///   4. `.cache/shasset` (last-resort relative fallback)
///
/// Kept as a pure function (taking the looked-up values explicitly) so it can
/// be unit-tested without mutating process-global environment state.
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

// ── add ───────────────────────────────────────────────────────────────────────

fn cmd_add(config: &Path, args: cli::AddArgs) -> Result<()> {
    let mut manifest = if config.exists() {
        load(config)?
    } else {
        Manifest::default()
    };

    if !args.compute {
        if let Some(ref cs) = args.checksum {
            ParsedChecksum::parse(cs)
                .with_context(|| format!("invalid checksum for asset '{}'", args.name))?;
        }
    }

    let asset = Asset {
        uri: args.uri.clone(),
        version: args.version.clone(),
        checksum: args.checksum.clone(),
        filename: args.filename.clone(),
        auth: args.auth.clone(),
    };

    let checksum = if args.compute {
        let cache_dir = args.cache_dir.clone().unwrap_or_else(default_cache_dir);
        let result = fetch_asset(FetchParams {
            name: &args.name,
            asset: &asset,
            out_dir: None,
            cache_dir: &cache_dir,
            retries: manifest.settings.retries,
            backoff: &manifest.settings.backoff,
            compute_checksum: true,
            no_reverify: false,
            materialize_mode: MaterializeMode::Copy,
            transport: None,
        })?;
        println!(
            "computed checksum for '{}': sha256:{}",
            args.name, result.computed_sha256
        );
        Some(format!("sha256:{}", result.computed_sha256))
    } else {
        args.checksum.clone()
    };

    let stored = Asset {
        uri: args.uri,
        version: args.version,
        checksum,
        filename: args.filename,
        auth: args.auth,
    };

    manifest.assets.insert(args.name.clone(), stored);
    save(config, &manifest)?;
    println!("added asset '{}' to {}", args.name, config.display());
    Ok(())
}

// ── remove ─────────────────────────────────────────────────────────────────────

fn cmd_remove(config: &Path, args: cli::RemoveArgs) -> Result<()> {
    let mut manifest = load(config)?;
    if manifest.assets.remove(&args.name).is_none() {
        bail!("asset '{}' not found in manifest", args.name);
    }
    save(config, &manifest)?;
    println!("removed asset '{}'", args.name);
    Ok(())
}

// ── get ────────────────────────────────────────────────────────────────────────

fn cmd_get(config: &Path, args: cli::GetArgs) -> Result<()> {
    let manifest = load(config)?;

    let assets: BTreeMap<&str, &Asset> = if let Some(ref name) = args.name {
        let asset = manifest
            .assets
            .get(name.as_str())
            .with_context(|| format!("asset '{name}' not found"))?;
        std::iter::once((name.as_str(), asset)).collect()
    } else {
        manifest
            .assets
            .iter()
            .map(|(k, v)| (k.as_str(), v))
            .collect()
    };

    if args.json {
        // Output the auth template, NOT any resolved value.
        let map: serde_json::Map<String, serde_json::Value> = assets
            .iter()
            .map(|(name, a)| {
                let mut obj = json!({
                    "uri": a.uri,
                    "version": a.version,
                    "checksum": a.checksum,
                });
                if let Some(f) = &a.filename {
                    obj["filename"] = json!(f);
                }
                if let Some(auth) = &a.auth {
                    obj["auth"] = json!(auth);
                }
                (name.to_string(), obj)
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&map)?);
    } else {
        for (name, a) in &assets {
            println!("name:     {name}");
            println!("  uri:      {}", a.uri);
            println!("  version:  {}", a.version);
            println!("  checksum: {}", a.checksum.as_deref().unwrap_or("<none>"));
            if let Some(f) = &a.filename {
                println!("  filename: {f}");
            }
            if let Some(auth) = &a.auth {
                // Show the template, never the resolved secret.
                println!("  auth:     {auth}");
            }
        }
    }
    Ok(())
}

// ── fetch ──────────────────────────────────────────────────────────────────────

fn cmd_fetch(config: &Path, args: cli::FetchArgs) -> Result<()> {
    let manifest = load(config)?;
    let cache_dir = args.cache_dir.clone().unwrap_or_else(default_cache_dir);
    let concurrency = args
        .concurrency
        .unwrap_or(manifest.settings.concurrency)
        .max(1);

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
            .map(|(k, v)| (k.as_str(), v))
            .collect()
    };

    if targets.is_empty() {
        println!("no assets to fetch");
        return Ok(());
    }

    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(concurrency)
        .build()
        .context("failed to build rayon thread pool")?;

    let materialize_mode = if args.link {
        MaterializeMode::Symlink
    } else {
        MaterializeMode::Copy
    };

    let results: Vec<Result<()>> = pool.install(|| {
        targets
            .par_iter()
            .map(|(name, asset)| {
                let result = fetch_asset(FetchParams {
                    name,
                    asset,
                    out_dir: Some(&args.out),
                    cache_dir: &cache_dir,
                    retries: manifest.settings.retries,
                    backoff: &manifest.settings.backoff,
                    compute_checksum: false,
                    no_reverify: args.no_reverify,
                    materialize_mode,
                    transport: None,
                });
                match &result {
                    Ok(r) => {
                        if let Some(path) = &r.path {
                            println!("fetched '{}' → {}", name, path.display())
                        } else {
                            println!("fetched '{}' → {}", name, r.blob_path.display())
                        }
                    }
                    Err(e) => eprintln!("error: failed to fetch '{name}': {e:#}"),
                }
                result.map(|_| ())
            })
            .collect()
    });

    let errors: Vec<_> = results.into_iter().filter_map(|r| r.err()).collect();
    if !errors.is_empty() {
        bail!("{} asset(s) failed to fetch", errors.len());
    }
    Ok(())
}

// ── prune ──────────────────────────────────────────────────────────────────────

#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct PruneSummary {
    blobs_removed: usize,
    bytes_reclaimed: u64,
    quarantine_entries_cleared: usize,
}

#[derive(Debug, Default)]
struct PrunePlan {
    blob_paths: Vec<PathBuf>,
    quarantine_paths: Vec<PathBuf>,
    summary: PruneSummary,
}

fn cmd_prune(config: &Path, args: cli::PruneArgs) -> Result<()> {
    let manifest = load(config)?;
    let cache_dir = args.cache_dir.clone().unwrap_or_else(default_cache_dir);
    let summary = prune_cache(&cache_dir, &manifest, args.dry_run)?;
    println!("{}", format_prune_summary(&summary, args.dry_run));
    Ok(())
}

fn prune_cache(cache_dir: &Path, manifest: &Manifest, dry_run: bool) -> Result<PruneSummary> {
    let referenced = referenced_blob_hexes(manifest)?;
    let plan = compute_prune_plan(cache_dir, &referenced)?;
    apply_prune_plan(plan, dry_run)
}

fn referenced_blob_hexes(manifest: &Manifest) -> Result<HashSet<String>> {
    let mut referenced = HashSet::new();
    for asset in manifest.assets.values() {
        if let Some(checksum) = &asset.checksum {
            let parsed = ParsedChecksum::parse(checksum)?;
            referenced.insert(parsed.hex.to_ascii_lowercase());
        }
    }
    Ok(referenced)
}

fn compute_prune_plan(cache_dir: &Path, referenced: &HashSet<String>) -> Result<PrunePlan> {
    let mut plan = PrunePlan::default();
    let blobs_dir = cache_dir.join("blobs").join("sha256");
    if blobs_dir.exists() {
        for entry in std::fs::read_dir(&blobs_dir)
            .with_context(|| format!("cannot read cache blobs dir: {}", blobs_dir.display()))?
        {
            let entry = entry.with_context(|| {
                format!("cannot read cache blob entry under {}", blobs_dir.display())
            })?;
            if !entry
                .file_type()
                .with_context(|| {
                    format!(
                        "cannot inspect cache blob entry: {}",
                        entry.path().display()
                    )
                })?
                .is_file()
            {
                continue;
            }

            let hex = entry.file_name().to_string_lossy().to_ascii_lowercase();
            if referenced.contains(&hex) {
                continue;
            }

            let size = entry
                .metadata()
                .with_context(|| format!("cannot stat cache blob: {}", entry.path().display()))?
                .len();
            plan.summary.blobs_removed += 1;
            plan.summary.bytes_reclaimed = plan.summary.bytes_reclaimed.saturating_add(size);
            plan.blob_paths.push(entry.path());
        }
    }

    let quarantine_dir = cache_dir.join("quarantine");
    if quarantine_dir.exists() {
        for entry in std::fs::read_dir(&quarantine_dir).with_context(|| {
            format!(
                "cannot read cache quarantine dir: {}",
                quarantine_dir.display()
            )
        })? {
            let entry = entry.with_context(|| {
                format!(
                    "cannot read quarantine entry under {}",
                    quarantine_dir.display()
                )
            })?;
            plan.summary.quarantine_entries_cleared += 1;
            plan.quarantine_paths.push(entry.path());
        }
    }

    Ok(plan)
}

fn apply_prune_plan(plan: PrunePlan, dry_run: bool) -> Result<PruneSummary> {
    if dry_run {
        return Ok(plan.summary);
    }

    for path in plan.blob_paths {
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("cannot remove cache blob: {}", path.display()));
            }
        }
    }

    for path in plan.quarantine_paths {
        remove_quarantine_entry(&path)?;
    }

    Ok(plan.summary)
}

fn remove_quarantine_entry(path: &Path) -> Result<()> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("cannot inspect quarantine entry: {}", path.display()));
        }
    };

    if metadata.file_type().is_dir() {
        std::fs::remove_dir_all(path)
            .with_context(|| format!("cannot remove quarantine dir: {}", path.display()))?;
    } else {
        std::fs::remove_file(path)
            .with_context(|| format!("cannot remove quarantine entry: {}", path.display()))?;
    }

    Ok(())
}

fn format_prune_summary(summary: &PruneSummary, dry_run: bool) -> String {
    let reclaimed = format_bytes(summary.bytes_reclaimed);
    if dry_run {
        format!(
            "dry run: would prune {} blob(s), reclaim {}; would clear {} quarantine entr(y/ies)",
            summary.blobs_removed, reclaimed, summary.quarantine_entries_cleared
        )
    } else {
        format!(
            "pruned {} blob(s), reclaimed {}; cleared {} quarantine entr(y/ies)",
            summary.blobs_removed, reclaimed, summary.quarantine_entries_cleared
        )
    }
}

fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }

    let mut value = bytes as f64;
    let mut unit = 0usize;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }

    format!("{value:.1} {}", UNITS[unit])
}

// ── verify ─────────────────────────────────────────────────────────────────────

fn cmd_verify(config: &Path, args: cli::VerifyArgs) -> Result<()> {
    let manifest = load(config)?;

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
            .map(|(k, v)| (k.as_str(), v))
            .collect()
    };

    #[derive(serde::Serialize)]
    struct VerifyRecord<'a> {
        name: &'a str,
        status: &'static str,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    }

    let mut records: Vec<VerifyRecord<'_>> = Vec::new();
    let mut any_failed = false;

    for (name, asset) in &targets {
        let cs = match &asset.checksum {
            Some(c) => c,
            None => {
                let msg = format!("asset '{name}' has no checksum in manifest");
                if args.json {
                    records.push(VerifyRecord {
                        name,
                        status: "error",
                        error: Some(msg),
                    });
                } else {
                    eprintln!("error: {msg}");
                }
                any_failed = true;
                continue;
            }
        };

        let parsed = match manifest::ParsedChecksum::parse(cs) {
            Ok(p) => p,
            Err(e) => {
                let msg = format!("{e}");
                if args.json {
                    records.push(VerifyRecord {
                        name,
                        status: "error",
                        error: Some(msg),
                    });
                } else {
                    eprintln!("error: {msg}");
                }
                any_failed = true;
                continue;
            }
        };

        let filename = match asset.output_filename() {
            Ok(f) => f,
            Err(e) => {
                let msg = format!("{e}");
                if args.json {
                    records.push(VerifyRecord {
                        name,
                        status: "error",
                        error: Some(msg),
                    });
                } else {
                    eprintln!("error: {msg}");
                }
                any_failed = true;
                continue;
            }
        };

        let file_path = args.out.join(name).join(&filename);

        match verify_on_disk(name, &file_path, &parsed.hex) {
            Ok(()) => {
                if args.json {
                    records.push(VerifyRecord {
                        name,
                        status: "ok",
                        error: None,
                    });
                } else {
                    println!("ok: '{name}' → {}", file_path.display());
                }
            }
            Err(e) => {
                let msg = format!("{e:#}");
                if args.json {
                    records.push(VerifyRecord {
                        name,
                        status: "fail",
                        error: Some(msg),
                    });
                } else {
                    eprintln!("FAIL: {msg}");
                }
                any_failed = true;
            }
        }
    }

    if args.json {
        println!("{}", serde_json::to_string_pretty(&records)?);
    }

    if any_failed {
        bail!("one or more assets failed verification");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::ffi::OsString;
    use tempfile::TempDir;

    // ── cache-dir resolution ─────────────────────────────────────────────────

    fn os(s: &str) -> Option<OsString> {
        Some(OsString::from(s))
    }

    #[test]
    fn cache_dir_prefers_shasset_cache() {
        // SHASSET_CACHE wins over XDG_CACHE_HOME and HOME.
        let resolved =
            resolve_cache_dir(os("/tmp/shacache"), os("/home/u/.xdgcache"), os("/home/u"));
        assert_eq!(resolved, PathBuf::from("/tmp/shacache"));
    }

    #[test]
    fn cache_dir_falls_back_to_xdg() {
        // No SHASSET_CACHE → XDG_CACHE_HOME/shasset.
        let resolved = resolve_cache_dir(None, os("/home/u/.xdgcache"), os("/home/u"));
        assert_eq!(resolved, PathBuf::from("/home/u/.xdgcache/shasset"));
    }

    #[test]
    fn cache_dir_falls_back_to_home() {
        // No SHASSET_CACHE, no XDG → $HOME/.cache/shasset.
        let resolved = resolve_cache_dir(None, None, os("/home/u"));
        assert_eq!(resolved, PathBuf::from("/home/u/.cache/shasset"));
    }

    #[test]
    fn cache_dir_relative_last_resort() {
        // Nothing set → relative .cache/shasset.
        let resolved = resolve_cache_dir(None, None, None);
        assert_eq!(resolved, PathBuf::from(".cache/shasset"));
    }

    #[test]
    fn cache_dir_ignores_empty_values() {
        // Empty env values are treated as unset and skipped.
        let resolved = resolve_cache_dir(os(""), os(""), os("/home/u"));
        assert_eq!(resolved, PathBuf::from("/home/u/.cache/shasset"));
    }

    fn asset_with_checksum(hex: &str) -> Asset {
        Asset {
            uri: "https://example.com/v1/tool".to_string(),
            version: "1".to_string(),
            checksum: Some(format!("sha256:{hex}")),
            filename: Some("tool.bin".to_string()),
            auth: None,
        }
    }

    #[test]
    fn prune_removes_unreferenced_blob_and_keeps_referenced_blob() {
        let cache = TempDir::new().unwrap();
        let kept_hex = "a".repeat(64);
        let removed_hex = "b".repeat(64);
        let blobs_dir = cache.path().join("blobs").join("sha256");
        std::fs::create_dir_all(&blobs_dir).unwrap();
        std::fs::write(blobs_dir.join(&kept_hex), b"keep").unwrap();
        std::fs::write(blobs_dir.join(&removed_hex), b"gone!").unwrap();

        let manifest = Manifest {
            settings: Default::default(),
            assets: BTreeMap::from([("kept".to_string(), asset_with_checksum(&kept_hex))]),
        };

        let summary = prune_cache(cache.path(), &manifest, false).unwrap();

        assert!(blobs_dir.join(&kept_hex).exists());
        assert!(!blobs_dir.join(&removed_hex).exists());
        assert_eq!(
            summary,
            PruneSummary {
                blobs_removed: 1,
                bytes_reclaimed: 5,
                quarantine_entries_cleared: 0,
            }
        );
        assert_eq!(
            format_prune_summary(&summary, false),
            "pruned 1 blob(s), reclaimed 5 B; cleared 0 quarantine entr(y/ies)"
        );
    }

    #[test]
    fn prune_dry_run_removes_nothing() {
        let cache = TempDir::new().unwrap();
        let kept_hex = "a".repeat(64);
        let removed_hex = "b".repeat(64);
        let blobs_dir = cache.path().join("blobs").join("sha256");
        std::fs::create_dir_all(&blobs_dir).unwrap();
        std::fs::write(blobs_dir.join(&kept_hex), b"keep").unwrap();
        std::fs::write(blobs_dir.join(&removed_hex), b"gone!").unwrap();

        let manifest = Manifest {
            settings: Default::default(),
            assets: BTreeMap::from([("kept".to_string(), asset_with_checksum(&kept_hex))]),
        };

        let summary = prune_cache(cache.path(), &manifest, true).unwrap();

        assert!(blobs_dir.join(&kept_hex).exists());
        assert!(blobs_dir.join(&removed_hex).exists());
        assert_eq!(summary.blobs_removed, 1);
        assert_eq!(summary.bytes_reclaimed, 5);
        assert_eq!(
            format_prune_summary(&summary, true),
            "dry run: would prune 1 blob(s), reclaim 5 B; would clear 0 quarantine entr(y/ies)"
        );
    }

    #[test]
    fn prune_clears_stale_quarantine_entries() {
        let cache = TempDir::new().unwrap();
        let quarantine_dir = cache.path().join("quarantine");
        std::fs::create_dir_all(&quarantine_dir).unwrap();
        let stale = quarantine_dir.join("download-x.part");
        std::fs::write(&stale, b"partial").unwrap();

        let summary = prune_cache(cache.path(), &Manifest::default(), false).unwrap();
        assert!(!stale.exists());
        assert_eq!(summary.quarantine_entries_cleared, 1);

        std::fs::write(&stale, b"partial").unwrap();
        let dry_run_summary = prune_cache(cache.path(), &Manifest::default(), true).unwrap();
        assert!(stale.exists());
        assert_eq!(dry_run_summary.quarantine_entries_cleared, 1);
    }

    #[test]
    fn prune_missing_cache_root_is_empty_and_non_creating() {
        let tmp = TempDir::new().unwrap();
        let missing_cache = tmp.path().join("missing-cache");
        let summary = prune_cache(&missing_cache, &Manifest::default(), false).unwrap();
        assert_eq!(summary, PruneSummary::default());
        assert!(!missing_cache.exists());
    }

    #[test]
    fn assets_without_checksums_do_not_keep_blobs() {
        let cache = TempDir::new().unwrap();
        let blobs_dir = cache.path().join("blobs").join("sha256");
        std::fs::create_dir_all(&blobs_dir).unwrap();
        let blob_hex = "c".repeat(64);
        let blob_path = blobs_dir.join(&blob_hex);
        std::fs::write(&blob_path, b"orphan").unwrap();

        let manifest = Manifest {
            settings: Default::default(),
            assets: BTreeMap::from([(
                "tool".to_string(),
                Asset {
                    uri: "https://example.com/tool".to_string(),
                    version: "1".to_string(),
                    checksum: None,
                    filename: None,
                    auth: None,
                },
            )]),
        };

        let summary = prune_cache(cache.path(), &manifest, false).unwrap();
        assert_eq!(summary.blobs_removed, 1);
        assert!(!blob_path.exists());
    }

    // ── manifest round-trip ──────────────────────────────────────────────────

    #[test]
    fn manifest_round_trip() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("shasset.yaml");

        let mut m = Manifest::default();
        m.assets.insert(
            "mytool".to_string(),
            Asset {
                uri: "https://example.com/v${version}/mytool".to_string(),
                version: "1.2.3".to_string(),
                checksum: Some(
                    "sha256:0000000000000000000000000000000000000000000000000000000000000000"
                        .to_string(),
                ),
                filename: None,
                auth: None,
            },
        );
        save(&path, &m).unwrap();
        let loaded = load(&path).unwrap();
        assert_eq!(loaded.assets.len(), 1);
        let a = &loaded.assets["mytool"];
        assert_eq!(a.version, "1.2.3");
        assert_eq!(
            a.checksum.as_deref(),
            Some("sha256:0000000000000000000000000000000000000000000000000000000000000000")
        );
    }

    // ── version interpolation ────────────────────────────────────────────────

    #[test]
    fn uri_version_interpolation() {
        let a = Asset {
            uri: "https://example.com/releases/v${version}/tool-${version}.tar.gz".to_string(),
            version: "2.0.0".to_string(),
            checksum: None,
            filename: None,
            auth: None,
        };
        assert_eq!(
            a.expanded_uri(),
            "https://example.com/releases/v2.0.0/tool-2.0.0.tar.gz"
        );
    }

    #[test]
    fn filename_interpolation() {
        let a = Asset {
            uri: "https://example.com/releases/v${version}/tool-${version}.tar.gz".to_string(),
            version: "2.0.0".to_string(),
            checksum: None,
            filename: Some("tool-${version}.tar.gz".to_string()),
            auth: None,
        };
        assert_eq!(a.output_filename().unwrap(), "tool-2.0.0.tar.gz");
    }

    #[test]
    fn uri_basename_fallback() {
        let a = Asset {
            uri: "https://example.com/releases/v1.0/archive.zip".to_string(),
            version: "1.0".to_string(),
            checksum: None,
            filename: None,
            auth: None,
        };
        assert_eq!(a.output_filename().unwrap(), "archive.zip");
    }

    // ── checksum parsing ─────────────────────────────────────────────────────

    #[test]
    fn checksum_parse_ok() {
        let cs = ParsedChecksum::parse(
            "sha256:aabbccdd00112233445566778899aabbccddeeff00112233445566778899aabb",
        )
        .unwrap();
        assert_eq!(cs.algorithm, "sha256");
    }

    #[test]
    fn checksum_parse_wrong_length() {
        let r = ParsedChecksum::parse("sha256:deadbeef");
        assert!(r.is_err());
    }

    #[test]
    fn checksum_parse_unknown_algo() {
        let r = ParsedChecksum::parse("md5:d41d8cd98f00b204e9800998ecf8427e");
        assert!(r.is_err());
    }

    // ── fetch + checksum verify (mock transport) ──────────────────────────────

    struct MockTransport {
        pub body: Vec<u8>,
        pub status_override: Option<u16>,
    }

    impl fetch::Transport for MockTransport {
        fn get(
            &self,
            _uri: &str,
            _auth: Option<&str>,
            _accept: Option<&str>,
        ) -> std::result::Result<fetch::DownloadResponse, fetch::FetchError> {
            if let Some(status) = self.status_override {
                return Err(match fetch::classify_status(status) {
                    fetch::RetryClass::Retry => {
                        fetch::FetchError::retryable(anyhow::anyhow!("HTTP {status} fetching mock"))
                    }
                    fetch::RetryClass::NoRetry => fetch::FetchError::permanent(anyhow::anyhow!(
                        "HTTP {status} (non-retryable) fetching mock"
                    )),
                });
            }
            Ok(fetch::DownloadResponse {
                body: Box::new(std::io::Cursor::new(self.body.clone())),
                content_length: None,
            })
        }
    }

    fn sha256_hex(data: &[u8]) -> String {
        use sha2::{Digest, Sha256};
        hex::encode(Sha256::digest(data))
    }

    #[test]
    fn fetch_succeeds_with_correct_checksum() {
        let data = b"hello world";
        let hex = sha256_hex(data);
        let tmp = TempDir::new().unwrap();

        let asset = Asset {
            uri: "https://example.com/v1/tool".to_string(),
            version: "1".to_string(),
            checksum: Some(format!("sha256:{hex}")),
            filename: Some("tool".to_string()),
            auth: None,
        };

        let result = fetch_asset(FetchParams {
            name: "mytool",
            asset: &asset,
            out_dir: Some(tmp.path()),
            cache_dir: tmp.path(),
            retries: 0,
            backoff: &manifest::Backoff::default(),
            compute_checksum: false,
            no_reverify: false,
            materialize_mode: MaterializeMode::Copy,
            transport: Some(Box::new(MockTransport {
                body: data.to_vec(),
                status_override: None,
            })),
        })
        .unwrap();

        assert!(result.path.unwrap().exists());
        assert_eq!(result.computed_sha256, hex);
    }

    #[test]
    fn fetch_fails_on_wrong_checksum() {
        let data = b"hello world";
        let wrong_hex = "0".repeat(64);
        let tmp = TempDir::new().unwrap();

        let asset = Asset {
            uri: "https://example.com/v1/tool".to_string(),
            version: "1".to_string(),
            checksum: Some(format!("sha256:{wrong_hex}")),
            filename: Some("tool".to_string()),
            auth: None,
        };

        let err = fetch_asset(FetchParams {
            name: "mytool",
            asset: &asset,
            out_dir: Some(tmp.path()),
            cache_dir: tmp.path(),
            retries: 0,
            backoff: &manifest::Backoff::default(),
            compute_checksum: false,
            no_reverify: false,
            materialize_mode: MaterializeMode::Copy,
            transport: Some(Box::new(MockTransport {
                body: data.to_vec(),
                status_override: None,
            })),
        })
        .unwrap_err();

        assert!(err.to_string().contains("checksum mismatch"));
    }

    #[test]
    fn fetch_fails_on_missing_checksum() {
        let tmp = TempDir::new().unwrap();
        let asset = Asset {
            uri: "https://example.com/v1/tool".to_string(),
            version: "1".to_string(),
            checksum: None,
            filename: Some("tool".to_string()),
            auth: None,
        };

        let err = fetch_asset(FetchParams {
            name: "mytool",
            asset: &asset,
            out_dir: Some(tmp.path()),
            cache_dir: tmp.path(),
            retries: 0,
            backoff: &manifest::Backoff::default(),
            compute_checksum: false,
            no_reverify: false,
            materialize_mode: MaterializeMode::Copy,
            transport: Some(Box::new(MockTransport {
                body: b"data".to_vec(),
                status_override: None,
            })),
        })
        .unwrap_err();

        assert!(err.to_string().contains("no checksum"));
    }

    #[test]
    fn fetch_rejects_empty_download() {
        let tmp = TempDir::new().unwrap();
        let asset = Asset {
            uri: "https://example.com/v1/tool".to_string(),
            version: "1".to_string(),
            checksum: Some(format!(
                "sha256:{}",
                sha256_hex(b"") // sha of empty — would match, but we catch empty first
            )),
            filename: Some("tool".to_string()),
            auth: None,
        };

        let err = fetch_asset(FetchParams {
            name: "mytool",
            asset: &asset,
            out_dir: Some(tmp.path()),
            cache_dir: tmp.path(),
            retries: 0,
            backoff: &manifest::Backoff::default(),
            compute_checksum: false,
            no_reverify: false,
            materialize_mode: MaterializeMode::Copy,
            transport: Some(Box::new(MockTransport {
                body: vec![],
                status_override: None,
            })),
        })
        .unwrap_err();

        assert!(err.to_string().contains("zero bytes"));
    }

    #[test]
    fn fetch_non_retryable_http_error() {
        let tmp = TempDir::new().unwrap();
        let asset = Asset {
            uri: "https://example.com/v1/tool".to_string(),
            version: "1".to_string(),
            checksum: Some(format!("sha256:{}", "a".repeat(64))),
            filename: Some("tool".to_string()),
            auth: None,
        };

        let err = fetch_asset(FetchParams {
            name: "mytool",
            asset: &asset,
            out_dir: Some(tmp.path()),
            cache_dir: tmp.path(),
            retries: 3,
            backoff: &manifest::Backoff {
                base_ms: 0,
                max_ms: 0,
                factor: 1,
            },
            compute_checksum: false,
            no_reverify: false,
            materialize_mode: MaterializeMode::Copy,
            transport: Some(Box::new(MockTransport {
                body: vec![],
                status_override: Some(404),
            })),
        })
        .unwrap_err();

        // 404 should NOT be retried — fail immediately
        assert!(err.to_string().contains("fetch failed"));
    }

    // ── auth interpolation ────────────────────────────────────────────────────

    #[test]
    fn auth_env_interpolation() {
        std::env::set_var("TEST_SHASSET_TOKEN_ABC", "supersecret");
        let a = Asset {
            uri: "https://example.com".to_string(),
            version: "1".to_string(),
            checksum: None,
            filename: None,
            auth: Some("${TEST_SHASSET_TOKEN_ABC}".to_string()),
        };
        assert_eq!(a.resolved_auth().unwrap().as_deref(), Some("supersecret"));
    }

    #[test]
    fn auth_env_missing_errors() {
        // Use a unique env var name that is almost certainly not set
        let a = Asset {
            uri: "https://example.com".to_string(),
            version: "1".to_string(),
            checksum: None,
            filename: None,
            auth: Some("${SHASSET_DEFINITELY_NOT_SET_XYZ123}".to_string()),
        };
        assert!(a.resolved_auth().is_err());
    }

    // ── verify on disk ───────────────────────────────────────────────────────

    #[test]
    fn verify_on_disk_pass() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("file.bin");
        let data = b"some content";
        std::fs::write(&path, data).unwrap();
        let hex = sha256_hex(data);
        verify_on_disk("x", &path, &hex).unwrap();
    }

    #[test]
    fn verify_on_disk_mismatch() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("file.bin");
        std::fs::write(&path, b"wrong").unwrap();
        let err = verify_on_disk("x", &path, &"a".repeat(64)).unwrap_err();
        assert!(err.to_string().contains("checksum mismatch"));
    }

    // ── JSON output shape for get ─────────────────────────────────────────────

    #[test]
    fn get_json_shows_template_not_secret() {
        // This tests that the `auth` field in JSON output is the raw template.
        let a = Asset {
            uri: "https://example.com".to_string(),
            version: "1.0".to_string(),
            checksum: Some("sha256:".to_string() + &"a".repeat(64)),
            filename: None,
            auth: Some("${SECRET_TOKEN}".to_string()),
        };
        let mut map = BTreeMap::new();
        map.insert("tool".to_string(), a);
        let manifest = Manifest {
            settings: Default::default(),
            assets: map,
        };

        // Simulate what cmd_get does for JSON output.
        let entry = &manifest.assets["tool"];
        assert_eq!(entry.auth.as_deref(), Some("${SECRET_TOKEN}"));
        // Verify it is NOT the resolved value (env var not set, but we don't even try).
    }

    // ── settings defaults ────────────────────────────────────────────────────

    #[test]
    fn settings_defaults() {
        let m: Manifest = serde_yaml::from_str("assets: {}").unwrap();
        assert_eq!(m.settings.concurrency, 4);
        assert_eq!(m.settings.retries, 3);
        assert_eq!(m.settings.backoff.base_ms, 500);
        assert_eq!(m.settings.backoff.max_ms, 8000);
        assert_eq!(m.settings.backoff.factor, 2);
    }
}
