use anyhow::{Context, Result};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::fetch;
use crate::manifest::{Manifest, ParsedChecksum};

// ── public types ──────────────────────────────────────────────────────────────

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PruneSummary {
    pub blobs_removed: usize,
    pub bytes_reclaimed: u64,
    pub quarantine_entries_cleared: usize,
}

// ── internal plan ─────────────────────────────────────────────────────────────

#[derive(Debug, Default)]
struct PrunePlan {
    blob_paths: Vec<PathBuf>,
    quarantine_paths: Vec<PathBuf>,
    summary: PruneSummary,
}

// ── public API ────────────────────────────────────────────────────────────────

/// Remove cache blobs not referenced by `manifest`, clear stale quarantine
/// entries, and prune orphaned oci-index entries.
///
/// When `dry_run` is `true` the plan is computed but nothing is deleted;
/// the returned [`PruneSummary`] reports what *would* be removed.
pub fn prune_cache(cache_dir: &Path, manifest: &Manifest, dry_run: bool) -> Result<PruneSummary> {
    let referenced = referenced_blob_hexes(manifest, cache_dir)?;
    let oci_index_keys = oci_referenced_index_keys(manifest);
    let plan = compute_prune_plan(cache_dir, &referenced, &oci_index_keys)?;
    apply_prune_plan(plan, dry_run)
}

/// Format a [`PruneSummary`] as a human-readable string.
///
/// The output format matches the `shasset prune` CLI exactly, including the
/// `"dry run: ..."` prefix when `dry_run` is `true`.
pub fn format_prune_summary(summary: &PruneSummary, dry_run: bool) -> String {
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

// ── private helpers ───────────────────────────────────────────────────────────

fn oci_referenced_index_keys(manifest: &Manifest) -> HashSet<String> {
    manifest
        .assets
        .values()
        .filter_map(|asset| {
            let uri = asset.expanded_uri();
            let manifest_hex = fetch::oci_manifest_hex_from_asset(asset, &uri)?;
            let platform_slug = fetch::oci_platform_slug_from_asset(asset).ok()?;
            Some(format!("{manifest_hex}.{platform_slug}"))
        })
        .collect()
}

fn referenced_blob_hexes(manifest: &Manifest, cache_dir: &Path) -> Result<HashSet<String>> {
    let mut referenced = HashSet::new();
    for asset in manifest.assets.values() {
        if let Some(checksum) = &asset.checksum {
            let parsed = ParsedChecksum::parse(checksum)?;
            referenced.insert(parsed.hex.to_ascii_lowercase());
        }
        // For OCI assets, look up the assembled-tar sha256 from the oci-index
        let uri = asset.expanded_uri();
        if uri.starts_with("oci://") {
            if let Some(tar_hex) = fetch::oci_index_tar_hex_from_cache(cache_dir, &uri, asset) {
                referenced.insert(tar_hex);
            }
        }
    }
    Ok(referenced)
}

fn compute_prune_plan(
    cache_dir: &Path,
    referenced: &HashSet<String>,
    oci_index_keys: &HashSet<String>,
) -> Result<PrunePlan> {
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

    // Scan oci-index dir for orphaned entries
    let oci_index_dir = cache_dir.join("oci-index");
    if oci_index_dir.exists() {
        for entry in std::fs::read_dir(&oci_index_dir)
            .with_context(|| format!("cannot read oci-index dir: {}", oci_index_dir.display()))?
        {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
            if !oci_index_keys.contains(&name) {
                plan.summary.quarantine_entries_cleared += 1; // reuse field for simplicity
                plan.quarantine_paths.push(entry.path());
            }
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

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{Asset, Manifest};
    use std::collections::BTreeMap;
    use tempfile::TempDir;

    fn asset_with_checksum(hex: &str) -> Asset {
        Asset {
            uri: "https://example.com/v1/tool".to_string(),
            version: "1".to_string(),
            checksum: Some(format!("sha256:{hex}")),
            digest: None,
            filename: Some("tool.bin".to_string()),
            auth: None,
            platform: None,
            archive: false,
            labels: Default::default(),
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
                    digest: None,
                    filename: None,
                    auth: None,
                    platform: None,
                    archive: false,
                    labels: Default::default(),
                },
            )]),
        };

        let summary = prune_cache(cache.path(), &manifest, false).unwrap();
        assert_eq!(summary.blobs_removed, 1);
        assert!(!blob_path.exists());
    }
}
