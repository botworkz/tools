use anyhow::{Context, Result};
use clap::Args;
use shasset::fetch::{fetch_asset, FetchParams, MaterializeMode, Transport};
use shasset::manifest::{load, Asset};
use std::path::{Path, PathBuf};

use crate::util::{default_cache_dir, materialize_flat, validate_flat_filename};

#[derive(Args, Debug)]
pub(crate) struct DepsArgs {
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

pub(crate) fn cmd_deps(config: &Path, args: DepsArgs) -> Result<()> {
    cmd_deps_with_transport(config, args, || None)
}

fn cmd_deps_with_transport<F>(config: &Path, args: DepsArgs, mut transport_factory: F) -> Result<()>
where
    F: FnMut() -> Option<Box<dyn Transport>>,
{
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
        if asset.expanded_uri().starts_with("oci://") && asset.checksum.is_some() {
            eprintln!("warning: asset '{name}' has checksum but oci:// ignores checksum");
        }

        let fetched = fetch_asset(FetchParams {
            name,
            asset,
            out_dir: None,
            cache_dir: &cache_dir,
            retries: manifest.settings.retries,
            backoff: &manifest.settings.backoff,
            compute_checksum: true,
            no_reverify: args.no_reverify,
            materialize_mode: MaterializeMode::Copy,
            transport: transport_factory(),
        })
        .with_context(|| format!("failed to fetch asset '{name}'"))?;

        let filename = oci_or_default_filename(name, asset)
            .with_context(|| format!("asset '{name}': cannot determine output filename"))?;
        let out_path = materialize_flat(&fetched.blob_path, &args.out, &filename, args.executable)
            .with_context(|| format!("failed to stage asset '{name}'"))?;
        println!("fetched '{}' → {}", name, out_path.display());
    }

    Ok(())
}

/// Determine the flat output filename for an asset.
///
/// For `oci://` assets, defaults to `<asset_key>.tar` when `filename` is unset.
/// For all other schemes, delegates to `asset.output_filename()`.
fn oci_or_default_filename(asset_key: &str, asset: &Asset) -> Result<String> {
    let uri = asset.expanded_uri();
    if uri.starts_with("oci://") {
        let name = if let Some(filename) = &asset.filename {
            let expanded = filename.replace("${version}", &asset.version);
            validate_flat_filename(&expanded)?;
            expanded
        } else {
            format!("{asset_key}.tar")
        };
        Ok(name)
    } else {
        asset
            .output_filename()
            .with_context(|| format!("asset '{asset_key}': cannot determine output filename"))
    }
}

#[cfg(test)]
mod tests {
    use super::{cmd_deps_with_transport, oci_or_default_filename, DepsArgs};
    use shasset::fetch::{DownloadResponse, FetchError, Transport};
    use shasset::manifest::Asset;
    use std::io::Cursor;
    use tempfile::TempDir;

    struct MockTransport {
        expected_uri: String,
        body: Vec<u8>,
    }

    impl Transport for MockTransport {
        fn get(
            &self,
            uri: &str,
            _auth: Option<&str>,
            accept: Option<&str>,
        ) -> std::result::Result<DownloadResponse, FetchError> {
            assert_eq!(uri, self.expected_uri);
            assert!(accept.is_none());
            Ok(DownloadResponse {
                body: Box::new(Cursor::new(self.body.clone())),
                content_length: Some(self.body.len() as u64),
            })
        }
    }

    #[test]
    fn deps_fetches_https_asset_to_flat_output_with_executable_mode() {
        let tmp = TempDir::new().unwrap();
        let manifest = tmp.path().join("shasset.yaml");
        let out = tmp.path().join("out");
        let cache = tmp.path().join("cache");
        let body = b"hello world".to_vec();
        let uri = "https://example.com/v1/launcher".to_string();
        let checksum = "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9";
        std::fs::write(
            &manifest,
            format!(
                "settings:\n  retries: 0\nassets:\n  botwork-launcher:\n    uri: {uri}\n    version: \"1\"\n    checksum: sha256:{checksum}\n    filename: launcher\n"
            ),
        )
        .unwrap();

        let mut transport = Some(Box::new(MockTransport {
            expected_uri: uri,
            body: body.clone(),
        }) as Box<dyn Transport>);

        cmd_deps_with_transport(
            &manifest,
            DepsArgs {
                name: None,
                out: out.clone(),
                cache_dir: Some(cache.clone()),
                no_reverify: false,
                executable: true,
            },
            || transport.take(),
        )
        .unwrap();

        let staged = out.join("launcher");
        assert_eq!(std::fs::read(&staged).unwrap(), body);
        assert!(!out.join("botwork-launcher").exists());
        assert!(cache.join("blobs").join("sha256").join(checksum).exists());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&staged).unwrap().permissions().mode() & 0o777,
                0o755
            );
        }
    }

    #[test]
    fn deps_fails_on_https_checksum_mismatch() {
        let tmp = TempDir::new().unwrap();
        let manifest = tmp.path().join("shasset.yaml");
        let out = tmp.path().join("out");
        let cache = tmp.path().join("cache");
        let uri = "https://example.com/v1/launcher".to_string();
        std::fs::write(
            &manifest,
            format!(
                "settings:\n  retries: 0\nassets:\n  botwork-launcher:\n    uri: {uri}\n    version: \"1\"\n    checksum: sha256:{}\n    filename: launcher\n",
                "0".repeat(64)
            ),
        )
        .unwrap();

        let mut transport = Some(Box::new(MockTransport {
            expected_uri: uri,
            body: b"hello world".to_vec(),
        }) as Box<dyn Transport>);

        let err = cmd_deps_with_transport(
            &manifest,
            DepsArgs {
                name: None,
                out: out.clone(),
                cache_dir: Some(cache),
                no_reverify: false,
                executable: false,
            },
            || transport.take(),
        )
        .unwrap_err();

        assert!(format!("{err:#}").contains("checksum mismatch"));
        assert!(!out.join("launcher").exists());
    }

    #[test]
    fn oci_or_default_filename_defaults_to_key_tar() {
        let asset = Asset {
            uri: "oci://ghcr.io/botworkz/svc@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
            version: String::new(),
            checksum: None,
            digest: None,
            filename: None,
            auth: None,
            platform: None,
        };
        assert_eq!(
            oci_or_default_filename("session-broker", &asset).unwrap(),
            "session-broker.tar"
        );
    }

    #[test]
    fn oci_or_default_filename_uses_manifest_filename() {
        let asset = Asset {
            uri: "oci://ghcr.io/botworkz/svc@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
            version: String::new(),
            checksum: None,
            digest: None,
            filename: Some("broker.tar".to_string()),
            auth: None,
            platform: None,
        };
        assert_eq!(
            oci_or_default_filename("session-broker", &asset).unwrap(),
            "broker.tar"
        );
    }

    #[test]
    fn oci_or_default_filename_rejects_non_flat_name() {
        let asset = Asset {
            uri: "oci://ghcr.io/botworkz/svc@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
            version: String::new(),
            checksum: None,
            digest: None,
            filename: Some("nested/broker.tar".to_string()),
            auth: None,
            platform: None,
        };
        assert!(oci_or_default_filename("session-broker", &asset).is_err());
    }
}
