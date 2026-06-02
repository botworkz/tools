use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::path::Path;
use std::time::Duration;

use crate::manifest::{Asset, Backoff};

/// How to classify an HTTP status code for retry purposes.
#[derive(Debug, PartialEq)]
pub enum RetryClass {
    /// Transient — retry with backoff.
    Retry,
    /// Permanent — fail immediately.
    NoRetry,
}

pub fn classify_status(status: u16) -> RetryClass {
    match status {
        429 | 500..=599 => RetryClass::Retry,
        _ => RetryClass::NoRetry,
    }
}

/// Download `asset` into `out_dir/<name>/`, verify checksum, return the output path.
///
/// `out_dir` is the `--out` value from the CLI.
/// The `name` is the asset's registry key.
///
/// If `compute_checksum` is true, skip checksum verification and return the
/// computed hex so the caller can store it.
pub struct FetchParams<'a> {
    pub name: &'a str,
    pub asset: &'a Asset,
    pub out_dir: &'a Path,
    pub retries: u32,
    pub backoff: &'a Backoff,
    pub compute_checksum: bool,
    /// Injected transport for testing; `None` → use real reqwest.
    pub transport: Option<Box<dyn Transport>>,
}

/// Minimal transport abstraction so tests can inject a mock.
pub trait Transport: Send + Sync {
    /// Fetch `url` (with optional `****** and return the raw bytes.
    fn get(&self, url: &str, auth: Option<&str>) -> Result<Vec<u8>>;
}

/// Real reqwest-based transport.
pub struct ReqwestTransport {
    pub client: reqwest::blocking::Client,
}

impl Default for ReqwestTransport {
    fn default() -> Self {
        Self {
            client: reqwest::blocking::Client::builder()
                .timeout(Duration::from_secs(60))
                .build()
                .expect("failed to build HTTP client"),
        }
    }
}

impl Transport for ReqwestTransport {
    fn get(&self, url: &str, auth: Option<&str>) -> Result<Vec<u8>> {
        let mut req = self.client.get(url);
        if let Some(token) = auth {
            let header_val = ["Bearer ", token].concat();
            req = req.header("Authorization", header_val);
        }
        let resp = req
            .send()
            .with_context(|| format!("HTTP request failed: {url}"))?;
        let status = resp.status();
        if !status.is_success() {
            let code = status.as_u16();
            if classify_status(code) == RetryClass::NoRetry {
                bail!("HTTP {code} (non-retryable) fetching {url}");
            }
            bail!("HTTP {code} fetching {url}");
        }
        let bytes = resp
            .bytes()
            .with_context(|| format!("failed to read response body from {url}"))?;
        Ok(bytes.to_vec())
    }
}

#[derive(Debug)]
pub struct FetchResult {
    pub path: std::path::PathBuf,
    /// The SHA-256 hex of the downloaded content.
    pub computed_sha256: String,
}

pub fn fetch_asset(params: FetchParams<'_>) -> Result<FetchResult> {
    let FetchParams {
        name,
        asset,
        out_dir,
        retries,
        backoff,
        compute_checksum,
        transport,
    } = params;

    let url = asset.expanded_url();
    let auth = asset.resolved_auth()?;

    if !compute_checksum && asset.checksum.is_none() {
        bail!(
            "asset '{name}' has no checksum; refusing to fetch without verification. \
             Use `shasset add --compute` to populate it first, or set `checksum` in the manifest."
        );
    }

    let expected = asset.parsed_checksum()?;

    let filename = asset
        .output_filename()
        .with_context(|| format!("asset '{name}': cannot determine output filename"))?;

    let asset_dir = out_dir.join(name);
    std::fs::create_dir_all(&asset_dir)
        .with_context(|| format!("cannot create output dir: {}", asset_dir.display()))?;
    let out_path = asset_dir.join(&filename);

    // Retry loop.
    let mut attempt = 0u32;
    let bytes = loop {
        attempt += 1;
        let transport_ref: &dyn Transport = match &transport {
            Some(t) => t.as_ref(),
            None => &ReqwestTransport::default(),
        };
        match try_download(transport_ref, &url, auth.as_deref()) {
            Ok(b) => break b,
            Err(e) => {
                // Determine if this error is retryable.
                let msg = e.to_string();
                let retryable = is_retryable_error(&msg);
                if !retryable || attempt > retries {
                    return Err(e.context(format!("fetch failed for asset '{name}'")));
                }
                let wait = compute_backoff(backoff, attempt);
                eprintln!(
                    "[shasset] warning: transient error for '{name}' (attempt {attempt}/{retries}): {e}; retrying in {wait}ms"
                );
                std::thread::sleep(Duration::from_millis(wait));
            }
        }
    };

    if bytes.is_empty() {
        bail!("asset '{name}': downloaded zero bytes — refusing to write empty file");
    }

    // Compute SHA-256.
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    let computed_hex = hex::encode(hasher.finalize());

    // Verify if we have an expected checksum.
    if let Some(expected_cs) = &expected {
        if computed_hex != expected_cs.hex {
            bail!(
                "checksum mismatch for asset '{name}': expected sha256:{}, got sha256:{}",
                expected_cs.hex,
                computed_hex
            );
        }
    }

    // Write to disk.
    let mut f = std::fs::File::create(&out_path)
        .with_context(|| format!("cannot create output file: {}", out_path.display()))?;
    f.write_all(&bytes)
        .with_context(|| format!("cannot write output file: {}", out_path.display()))?;

    Ok(FetchResult {
        path: out_path,
        computed_sha256: computed_hex,
    })
}

/// Verify an already-downloaded asset on disk.
pub fn verify_on_disk(name: &str, path: &Path, expected_hex: &str) -> Result<()> {
    if !path.exists() {
        bail!("asset '{name}': file not found at {}", path.display());
    }
    let mut f =
        std::fs::File::open(path).with_context(|| format!("cannot open {}", path.display()))?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf)
        .with_context(|| format!("cannot read {}", path.display()))?;
    if buf.is_empty() {
        bail!("asset '{name}': file is empty: {}", path.display());
    }
    let mut hasher = Sha256::new();
    hasher.update(&buf);
    let computed = hex::encode(hasher.finalize());
    if computed != expected_hex {
        bail!(
            "checksum mismatch for asset '{name}': expected sha256:{}, got sha256:{}",
            expected_hex,
            computed
        );
    }
    Ok(())
}

fn try_download(transport: &dyn Transport, url: &str, auth: Option<&str>) -> Result<Vec<u8>> {
    transport.get(url, auth)
}

fn is_retryable_error(msg: &str) -> bool {
    // HTTP status codes surfaced in the error message.
    for code in [429u16, 500, 502, 503, 504] {
        if msg.contains(&format!("HTTP {code}")) {
            return true;
        }
    }
    // Connection-level errors.
    if msg.contains("connection") || msg.contains("timeout") || msg.contains("connect") {
        return true;
    }
    false
}

fn compute_backoff(backoff: &Backoff, attempt: u32) -> u64 {
    let exp = backoff.factor.pow(attempt.saturating_sub(1));
    let ms = backoff.base_ms.saturating_mul(exp);
    ms.min(backoff.max_ms)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_status_retry() {
        assert_eq!(classify_status(429), RetryClass::Retry);
        assert_eq!(classify_status(500), RetryClass::Retry);
        assert_eq!(classify_status(503), RetryClass::Retry);
    }

    #[test]
    fn classify_status_no_retry() {
        assert_eq!(classify_status(200), RetryClass::NoRetry);
        assert_eq!(classify_status(404), RetryClass::NoRetry);
        assert_eq!(classify_status(401), RetryClass::NoRetry);
        assert_eq!(classify_status(403), RetryClass::NoRetry);
    }

    #[test]
    fn backoff_capped_at_max() {
        let b = Backoff {
            base_ms: 500,
            max_ms: 8000,
            factor: 2,
        };
        // attempt=1: 500*2^0=500
        assert_eq!(compute_backoff(&b, 1), 500);
        // attempt=2: 500*2^1=1000
        assert_eq!(compute_backoff(&b, 2), 1000);
        // attempt=5: 500*2^4=8000
        assert_eq!(compute_backoff(&b, 5), 8000);
        // attempt=6: 500*2^5=16000 → capped at 8000
        assert_eq!(compute_backoff(&b, 6), 8000);
    }

    #[test]
    fn is_retryable_matches_http_codes() {
        assert!(is_retryable_error("HTTP 429 fetching http://x"));
        assert!(is_retryable_error("HTTP 503 fetching http://x"));
        assert!(!is_retryable_error(
            "HTTP 404 (non-retryable) fetching http://x"
        ));
        assert!(!is_retryable_error("HTTP 401 fetching http://x"));
    }
}
