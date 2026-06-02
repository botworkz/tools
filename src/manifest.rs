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
    pub url: String,
    pub version: String,
    /// `sha256:<64-hex>` — required for fetch/verify unless being computed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checksum: Option<String>,
    /// Forced output filename; when absent the URL basename is used.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filename: Option<String>,
    /// `${ENV_VAR}` template resolved at runtime; NEVER written back resolved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<String>,
}

impl Asset {
    /// Expand `${version}` inside `url` (and `filename` if present).
    pub fn expanded_url(&self) -> String {
        self.url.replace("${version}", &self.version)
    }

    /// Derive the output filename: manifest `filename` (version-expanded) if set,
    /// else basename of the expanded URL.
    pub fn output_filename(&self) -> Result<String> {
        if let Some(tpl) = &self.filename {
            return Ok(tpl.replace("${version}", &self.version));
        }
        let url = self.expanded_url();
        let path = url
            .split('?')
            .next()
            .unwrap_or(&url)
            .split('#')
            .next()
            .unwrap_or(&url);
        let basename = path
            .rsplit('/')
            .next()
            .filter(|s| !s.is_empty())
            .with_context(|| format!("cannot derive output filename from URL: {url}"))?;
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
    std::fs::write(path, content)
        .with_context(|| format!("cannot write manifest: {}", path.display()))?;
    Ok(())
}
