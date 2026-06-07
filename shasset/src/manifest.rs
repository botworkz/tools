use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

/// Top-level manifest (`shasset.yaml`).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Manifest {
    #[serde(default)]
    pub settings: Settings,
    #[serde(default)]
    pub assets: BTreeMap<String, Asset>,
}

/// Global settings; all fields are optional in the file and have defaults.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default = "default_concurrency")]
    pub concurrency: usize,
    #[serde(default = "default_retries")]
    pub retries: u32,
    #[serde(default)]
    pub backoff: Backoff,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            concurrency: default_concurrency(),
            retries: default_retries(),
            backoff: Backoff::default(),
        }
    }
}

fn default_concurrency() -> usize {
    4
}

fn default_retries() -> u32 {
    3
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Backoff {
    #[serde(default = "default_base_ms")]
    pub base_ms: u64,
    #[serde(default = "default_max_ms")]
    pub max_ms: u64,
    #[serde(default = "default_factor")]
    pub factor: u64,
}

impl Default for Backoff {
    fn default() -> Self {
        Self {
            base_ms: default_base_ms(),
            max_ms: default_max_ms(),
            factor: default_factor(),
        }
    }
}

fn default_base_ms() -> u64 {
    500
}
fn default_max_ms() -> u64 {
    8000
}
fn default_factor() -> u64 {
    2
}

/// A single named asset entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Asset {
    pub uri: String,
    #[serde(default)]
    pub version: String,
    /// `sha256:<64-hex>` — required for fetch/verify unless being computed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checksum: Option<String>,
    /// `sha256:<64-hex>` — OCI manifest digest. Required for `oci://` URIs
    /// (unless using the legacy `oci://<registry>/<repo>@sha256:<hex>` form).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub digest: Option<String>,
    /// Forced output filename; when absent the URI basename is used.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filename: Option<String>,
    /// `${ENV_VAR}` template resolved at runtime; NEVER written back resolved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<String>,
    /// Platform selector for OCI image indices. Defaults to `linux/amd64`.
    /// Form: `os/arch` or `os/arch/variant`. Ignored for non-OCI URIs and
    /// for OCI URIs that resolve to a single-platform manifest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform: Option<String>,
}

impl Asset {
    /// Expand `${version}` inside `uri` (and `filename` if present).
    pub fn expanded_uri(&self) -> String {
        self.uri.replace("${version}", &self.version)
    }

    /// Derive the output filename: manifest `filename` (version-expanded) if set,
    /// else basename of the expanded URI.
    pub fn output_filename(&self) -> Result<String> {
        if let Some(tpl) = &self.filename {
            return Ok(tpl.replace("${version}", &self.version));
        }
        let uri = self.expanded_uri();
        let path = uri
            .split('?')
            .next()
            .unwrap_or(&uri)
            .split('#')
            .next()
            .unwrap_or(&uri);
        let basename = path
            .rsplit('/')
            .next()
            .filter(|s| !s.is_empty())
            .with_context(|| format!("cannot derive output filename from URI: {uri}"))?;
        Ok(basename.to_string())
    }

    /// Resolve `${ENV_VAR}` inside `auth`; error if the variable is unset.
    pub fn resolved_auth(&self) -> Result<Option<String>> {
        let Some(tpl) = &self.auth else {
            return Ok(None);
        };
        let resolved = interpolate_env(tpl)?;
        Ok(Some(resolved))
    }

    /// Parse and validate the checksum field.
    pub fn parsed_checksum(&self) -> Result<Option<ParsedChecksum>> {
        let Some(raw) = &self.checksum else {
            return Ok(None);
        };
        Ok(Some(ParsedChecksum::parse(raw)?))
    }

    /// Parse and validate the `digest:` field (OCI manifest digest).
    pub fn parsed_digest(&self) -> Result<Option<ParsedChecksum>> {
        let Some(raw) = &self.digest else {
            return Ok(None);
        };
        Ok(Some(ParsedChecksum::parse(raw)?))
    }

    /// Resolve the OCI manifest digest hex from either the structured `digest:` field
    /// or the legacy `oci://...@sha256:<hex>` URI suffix.
    ///
    /// - Prefers `self.digest` when set.
    /// - Falls back to the `@sha256:<hex>` suffix in `self.expanded_uri()`.
    /// - If **both** are present they must match exactly; returns `Err` if they differ.
    /// - Returns `Ok(None)` when neither is present.
    pub fn oci_digest_hex(&self) -> Result<Option<String>> {
        let parsed_d = self.parsed_digest()?;
        let uri = self.expanded_uri();
        let uri_hex = oci_uri_manifest_hex(&uri);

        match (parsed_d, uri_hex) {
            (Some(d), Some(u)) => {
                let field_hex = d.hex.to_ascii_lowercase();
                if field_hex != u {
                    bail!(
                        "OCI digest mismatch: 'digest' field says sha256:{}, URI suffix says sha256:{}",
                        field_hex,
                        u
                    );
                }
                Ok(Some(field_hex))
            }
            (Some(d), None) => Ok(Some(d.hex.to_ascii_lowercase())),
            (None, Some(u)) => Ok(Some(u)),
            (None, None) => Ok(None),
        }
    }

    /// Resolve the requested OCI platform, defaulting to "linux/amd64".
    /// Returns (os, arch, variant) where variant is None if not specified.
    pub fn resolved_platform(&self) -> Result<(String, String, Option<String>)> {
        let raw = self.platform.as_deref().unwrap_or("linux/amd64");
        let parts: Vec<&str> = raw.split('/').collect();
        match parts.as_slice() {
            [os, arch] if !os.is_empty() && !arch.is_empty() => {
                Ok((os.to_string(), arch.to_string(), None))
            }
            [os, arch, variant] if !os.is_empty() && !arch.is_empty() && !variant.is_empty() => {
                Ok((os.to_string(), arch.to_string(), Some(variant.to_string())))
            }
            _ => bail!("invalid platform '{raw}': expected 'os/arch' or 'os/arch/variant'"),
        }
    }
}

/// Extract the manifest digest hex from an `oci://` URI, if valid.
/// Private helper — avoids a circular dependency with fetch.rs.
fn oci_uri_manifest_hex(uri: &str) -> Option<String> {
    let rest = uri.strip_prefix("oci://")?;
    let (_, digest) = rest.rsplit_once('@')?;
    let hex = digest.strip_prefix("sha256:")?;
    if hex.len() == 64 && hex.chars().all(|c| c.is_ascii_hexdigit()) {
        Some(hex.to_ascii_lowercase())
    } else {
        None
    }
}

/// A parsed checksum value.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct ParsedChecksum {
    pub algorithm: String,
    pub hex: String,
}

impl ParsedChecksum {
    pub fn parse(s: &str) -> Result<Self> {
        let (algo, hex) = s
            .split_once(':')
            .with_context(|| format!("checksum must be <algorithm>:<hex>, got: {s}"))?;
        match algo {
            "sha256" => {
                if hex.len() != 64 {
                    bail!(
                        "sha256 checksum must be 64 hex chars, got {}: {s}",
                        hex.len()
                    );
                }
                if !hex.chars().all(|c| c.is_ascii_hexdigit()) {
                    bail!("sha256 checksum contains non-hex characters: {s}");
                }
            }
            other => bail!("unsupported checksum algorithm '{other}'; only sha256 is supported"),
        }
        Ok(Self {
            algorithm: algo.to_string(),
            hex: hex.to_string(),
        })
    }
}

/// Replace `${VAR}` with the value of environment variable `VAR`.
/// Errors if any referenced variable is unset.
pub fn interpolate_env(s: &str) -> Result<String> {
    let mut result = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(start) = rest.find("${") {
        let prefix = &rest[..start];
        result.push_str(prefix);
        rest = &rest[start + 2..];
        let end = rest
            .find('}')
            .with_context(|| format!("unterminated '${{' in: {s}"))?;
        let var_name = &rest[..end];
        let value = std::env::var(var_name).with_context(|| {
            format!("environment variable '{var_name}' is unset (referenced in auth field)")
        })?;
        result.push_str(&value);
        rest = &rest[end + 1..];
    }
    result.push_str(rest);
    Ok(result)
}

// ── I/O helpers ──────────────────────────────────────────────────────────────

pub fn load(path: &Path) -> Result<Manifest> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("cannot read manifest: {}", path.display()))?;
    let manifest: Manifest = serde_yaml::from_str(&content)
        .with_context(|| format!("invalid manifest YAML: {}", path.display()))?;
    Ok(manifest)
}

pub fn save(path: &Path, manifest: &Manifest) -> Result<()> {
    let content =
        serde_yaml::to_string(manifest).context("failed to serialise manifest to YAML")?;
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)
        .with_context(|| format!("cannot create manifest dir: {}", parent.display()))?;
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("shasset.yaml");
    let tmp_path = parent.join(format!(
        ".{file_name}.tmp-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    std::fs::write(&tmp_path, content)
        .with_context(|| format!("cannot write temp manifest: {}", tmp_path.display()))?;
    std::fs::rename(&tmp_path, path).with_context(|| {
        format!(
            "cannot atomically replace manifest {} from {}",
            path.display(),
            tmp_path.display()
        )
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{Asset, Manifest};

    #[test]
    fn version_defaults_empty_for_versionless_asset() {
        let digest = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let manifest: Manifest = serde_yaml::from_str(&format!(
            "assets:\n  image:\n    uri: oci://ghcr.io/botworkz/session-broker@sha256:{digest}\n    filename: session-broker.tar\n"
        ))
        .unwrap();
        let asset = &manifest.assets["image"];
        assert_eq!(asset.version, "");
        assert_eq!(
            asset.expanded_uri(),
            format!("oci://ghcr.io/botworkz/session-broker@sha256:{digest}")
        );
        assert_eq!(asset.output_filename().unwrap(), "session-broker.tar");
    }

    #[test]
    fn resolved_platform_parses_and_defaults() {
        let default_asset = Asset {
            uri: "oci://ghcr.io/botworkz/svc@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                .to_string(),
            version: String::new(),
            checksum: None,
            digest: None,
            filename: Some("svc.tar".to_string()),
            auth: None,
            platform: None,
        };
        assert_eq!(
            default_asset.resolved_platform().unwrap(),
            ("linux".to_string(), "amd64".to_string(), None)
        );

        let explicit = Asset {
            platform: Some("linux/amd64".to_string()),
            ..default_asset.clone()
        };
        assert_eq!(
            explicit.resolved_platform().unwrap(),
            ("linux".to_string(), "amd64".to_string(), None)
        );

        let with_variant = Asset {
            platform: Some("linux/arm/v7".to_string()),
            ..default_asset
        };
        assert_eq!(
            with_variant.resolved_platform().unwrap(),
            (
                "linux".to_string(),
                "arm".to_string(),
                Some("v7".to_string())
            )
        );
    }

    #[test]
    fn resolved_platform_rejects_invalid_shapes() {
        for raw in ["linux", "linux/", "/amd64", ""] {
            let asset = Asset {
                uri: "oci://ghcr.io/botworkz/svc@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                    .to_string(),
                version: String::new(),
                checksum: None,
                digest: None,
                filename: Some("svc.tar".to_string()),
                auth: None,
                platform: Some(raw.to_string()),
            };
            let err = asset.resolved_platform().unwrap_err();
            assert!(
                err.to_string().contains("invalid platform"),
                "unexpected error for '{raw}': {err:#}"
            );
        }
    }

    // Shared hex used in oci_digest_hex tests — 64 valid hex chars.
    const HEX_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const HEX_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    fn make_oci_asset(uri: &str, digest: Option<&str>) -> Asset {
        Asset {
            uri: uri.to_string(),
            version: String::new(),
            checksum: None,
            digest: digest.map(str::to_string),
            filename: Some("svc.tar".to_string()),
            auth: None,
            platform: None,
        }
    }

    #[test]
    fn oci_digest_hex_from_field_only() {
        let asset = make_oci_asset(
            "oci://ghcr.io/botworkz/svc",
            Some(&format!("sha256:{HEX_A}")),
        );
        assert_eq!(asset.oci_digest_hex().unwrap(), Some(HEX_A.to_string()));
    }

    #[test]
    fn oci_digest_hex_from_uri_only() {
        let asset = make_oci_asset(&format!("oci://ghcr.io/botworkz/svc@sha256:{HEX_A}"), None);
        assert_eq!(asset.oci_digest_hex().unwrap(), Some(HEX_A.to_string()));
    }

    #[test]
    fn oci_digest_hex_both_matching() {
        let asset = make_oci_asset(
            &format!("oci://ghcr.io/botworkz/svc@sha256:{HEX_A}"),
            Some(&format!("sha256:{HEX_A}")),
        );
        assert_eq!(asset.oci_digest_hex().unwrap(), Some(HEX_A.to_string()));
    }

    #[test]
    fn oci_digest_hex_both_mismatching_errors() {
        let asset = make_oci_asset(
            &format!("oci://ghcr.io/botworkz/svc@sha256:{HEX_A}"),
            Some(&format!("sha256:{HEX_B}")),
        );
        let err = asset.oci_digest_hex().unwrap_err();
        assert!(
            err.to_string().contains("OCI digest mismatch"),
            "expected mismatch error, got: {err:#}"
        );
        assert!(
            err.to_string().contains(HEX_A) && err.to_string().contains(HEX_B),
            "error should name both values: {err:#}"
        );
    }

    #[test]
    fn oci_digest_hex_neither_returns_none() {
        let asset = make_oci_asset("oci://ghcr.io/botworkz/svc", None);
        assert_eq!(asset.oci_digest_hex().unwrap(), None);
    }
}
