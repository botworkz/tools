use anyhow::{bail, Context, Result};
use clap::Args;
use shasset::fetch::{fetch_asset, FetchParams, MaterializeMode, Transport};
use shasset::manifest::{load, Asset};
use std::path::{Path, PathBuf};

use crate::util::{
    command_exists, default_cache_dir, materialize_flat, run_command, validate_flat_filename,
};

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
            compute_checksum: true,
            no_reverify: args.no_reverify,
            materialize_mode: MaterializeMode::Copy,
            transport: transport_factory(),
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

fn oci_pull_args(ref_with_digest: &str) -> Vec<String> {
    vec!["pull".into(), ref_with_digest.into()]
}

fn oci_tag_args(ref_with_digest: &str, local_tag: &str) -> Vec<String> {
    vec!["tag".into(), ref_with_digest.into(), local_tag.into()]
}

fn oci_save_args(local_tag: &str, out_tarball_path: &Path) -> Vec<String> {
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
        &oci_pull_args(&oci.ref_with_digest),
        &[],
        &format!(
            "asset '{asset_key}': docker pull failed for {}",
            oci.ref_with_digest
        ),
    )?;
    run_command(
        "docker",
        &oci_tag_args(&oci.ref_with_digest, &local_tag),
        &[],
        &format!("asset '{asset_key}': docker tag failed"),
    )?;
    if let Err(err) = run_command(
        "docker",
        &oci_save_args(&local_tag, &tmp_path),
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

#[cfg(test)]
mod tests {
    use super::{
        cmd_deps_with_transport, image_tarball_name, local_tag_for, oci_pull_args, oci_save_args,
        oci_tag_args, parse_oci_uri, DepsArgs,
    };
    use shasset::fetch::{DownloadResponse, FetchError, Transport};
    use std::io::Cursor;
    use std::path::Path;
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
    fn oci_pull_args_match_expected_argv() {
        assert_eq!(
            oci_pull_args("ghcr.io/botworkz/svc@sha256:abc"),
            vec!["pull", "ghcr.io/botworkz/svc@sha256:abc"]
        );
    }

    #[test]
    fn oci_tag_args_match_expected_argv() {
        assert_eq!(
            oci_tag_args("ghcr.io/botworkz/svc@sha256:abc", "botwork/svc:local"),
            vec![
                "tag",
                "ghcr.io/botworkz/svc@sha256:abc",
                "botwork/svc:local"
            ]
        );
    }

    #[test]
    fn oci_save_args_match_expected_argv() {
        let out = Path::new("/tmp/out/svc.tar");
        assert_eq!(
            oci_save_args("botwork/svc:local", out),
            vec!["save", "botwork/svc:local", "-o", "/tmp/out/svc.tar"]
        );
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
}
