use anyhow::{anyhow, bail, Context, Result};
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
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

#[derive(Debug)]
pub enum FetchError {
    Retryable(anyhow::Error),
    Permanent(anyhow::Error),
}

impl FetchError {
    pub fn retryable(error: impl Into<anyhow::Error>) -> Self {
        Self::Retryable(error.into())
    }

    pub fn permanent(error: impl Into<anyhow::Error>) -> Self {
        Self::Permanent(error.into())
    }

    pub fn is_retryable(&self) -> bool {
        matches!(self, Self::Retryable(_))
    }

    fn into_anyhow(self) -> anyhow::Error {
        match self {
            Self::Retryable(error) | Self::Permanent(error) => error,
        }
    }
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Retryable(error) | Self::Permanent(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for FetchError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Retryable(error) | Self::Permanent(error) => error.source(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MaterializeMode {
    Copy,
    Symlink,
}

/// Download `asset` into the local content-addressed cache, verify checksum,
/// and optionally materialize to `out_dir/<name>/<filename>`.
pub struct FetchParams<'a> {
    pub name: &'a str,
    pub asset: &'a Asset,
    pub out_dir: Option<&'a Path>,
    pub cache_dir: &'a Path,
    pub retries: u32,
    pub backoff: &'a Backoff,
    pub compute_checksum: bool,
    pub no_reverify: bool,
    pub materialize_mode: MaterializeMode,
    /// Injected transport for testing; `None` → use real reqwest.
    pub transport: Option<Box<dyn Transport>>,
}

pub struct DownloadResponse {
    pub body: Box<dyn Read + Send>,
    pub content_length: Option<u64>,
}

/// Response type for OCI HTTP requests where 401+challenge is meaningful.
pub struct OciHttpResponse {
    pub status: u16,
    pub www_authenticate: Option<String>,
    pub body: Box<dyn Read + Send>,
    pub content_length: Option<u64>,
}

/// Parsed components of an `oci://` URI.
#[derive(Debug, Clone)]
pub struct OciRef {
    pub registry: String,
    pub repo: String,
    pub digest: String,     // "sha256:<64-hex>"
    pub digest_hex: String, // just the <64-hex> part
}

/// Minimal transport abstraction so tests can inject a mock.
pub trait Transport: Send + Sync {
    /// Fetch `uri` (with optional auth token) and return a streaming body reader.
    fn get(
        &self,
        uri: &str,
        auth: Option<&str>,
        accept: Option<&str>,
    ) -> std::result::Result<DownloadResponse, FetchError>;

    fn get_json(
        &self,
        uri: &str,
        auth: Option<&str>,
    ) -> std::result::Result<serde_json::Value, FetchError> {
        let mut response = self.get(uri, auth, Some("application/vnd.github+json"))?;
        let mut body = String::new();
        response.body.read_to_string(&mut body).map_err(|error| {
            FetchError::retryable(
                anyhow::Error::new(error)
                    .context(format!("failed to read response body from {uri}")),
            )
        })?;
        serde_json::from_str::<serde_json::Value>(&body).map_err(|error| {
            FetchError::permanent(
                anyhow::Error::new(error)
                    .context(format!("failed to parse JSON response from {uri}")),
            )
        })
    }

    /// Fetch `uri` for OCI use, surfacing the HTTP status code and `WWW-Authenticate` header
    /// even on 401 responses so callers can perform token exchange.
    ///
    /// The default implementation wraps `get`; `ReqwestTransport` overrides this to inspect
    /// response headers on 401. Mock transports in tests can also override it.
    fn get_oci(
        &self,
        uri: &str,
        auth: Option<&str>,
        accept: Option<&str>,
    ) -> std::result::Result<OciHttpResponse, FetchError> {
        // Default: just use get() and assume 200 success with no challenge.
        let resp = self.get(uri, auth, accept)?;
        Ok(OciHttpResponse {
            status: 200,
            www_authenticate: None,
            body: resp.body,
            content_length: resp.content_length,
        })
    }

    /// Fetch `uri` using HTTP Basic auth (`Authorization: Basic <base64(user:password)>`).
    ///
    /// Used for OCI token-exchange endpoints, which require Basic auth per the docker token
    /// spec. The default implementation falls back to `get` with a `user:password` string so
    /// that mock transports in tests keep compiling without change; `ReqwestTransport` overrides
    /// this to send a correctly-encoded Basic header.
    fn get_with_basic_auth(
        &self,
        uri: &str,
        user: &str,
        password: &str,
        accept: Option<&str>,
    ) -> std::result::Result<DownloadResponse, FetchError> {
        let combined = format!("{user}:{password}");
        self.get(uri, Some(&combined), accept)
    }
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
    fn get(
        &self,
        uri: &str,
        auth: Option<&str>,
        accept: Option<&str>,
    ) -> std::result::Result<DownloadResponse, FetchError> {
        let mut req = self.client.get(uri).header("User-Agent", "shasset");
        if let Some(token) = auth {
            let header_val = ["Bearer ", token].concat();
            req = req.header("Authorization", header_val);
        }
        if let Some(accept_header) = accept {
            req = req.header("Accept", accept_header);
        }
        let resp = req.send().map_err(|error| {
            FetchError::retryable(
                anyhow::Error::new(error).context(format!("HTTP request failed: {uri}")),
            )
        })?;
        let status = resp.status();
        if !status.is_success() {
            let code = status.as_u16();
            return Err(http_status_error(code, uri));
        }
        let content_length = resp.content_length();
        Ok(DownloadResponse {
            body: Box::new(resp),
            content_length,
        })
    }

    fn get_with_basic_auth(
        &self,
        uri: &str,
        user: &str,
        password: &str,
        accept: Option<&str>,
    ) -> std::result::Result<DownloadResponse, FetchError> {
        let mut req = self
            .client
            .get(uri)
            .header("User-Agent", "shasset")
            .basic_auth(user, Some(password));
        if let Some(accept_header) = accept {
            req = req.header("Accept", accept_header);
        }
        let resp = req.send().map_err(|error| {
            FetchError::retryable(
                anyhow::Error::new(error).context(format!("HTTP request failed: {uri}")),
            )
        })?;
        let status = resp.status();
        if !status.is_success() {
            let code = status.as_u16();
            return Err(http_status_error(code, uri));
        }
        let content_length = resp.content_length();
        Ok(DownloadResponse {
            body: Box::new(resp),
            content_length,
        })
    }

    fn get_oci(
        &self,
        uri: &str,
        auth: Option<&str>,
        accept: Option<&str>,
    ) -> std::result::Result<OciHttpResponse, FetchError> {
        let mut req = self.client.get(uri).header("User-Agent", "shasset");
        if let Some(token) = auth {
            let header_val = ["Bearer ", token].concat();
            req = req.header("Authorization", header_val);
        }
        if let Some(a) = accept {
            req = req.header("Accept", a);
        }
        let resp = req.send().map_err(|e| {
            FetchError::retryable(
                anyhow::Error::new(e).context(format!("HTTP request failed: {uri}")),
            )
        })?;
        let status = resp.status().as_u16();
        let www_authenticate = resp
            .headers()
            .get("www-authenticate")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        if status == 200 || status == 401 {
            let content_length = resp.content_length();
            Ok(OciHttpResponse {
                status,
                www_authenticate,
                body: Box::new(resp),
                content_length,
            })
        } else {
            Err(http_status_error(status, uri))
        }
    }
}

#[derive(Debug)]
pub struct FetchResult {
    pub path: Option<PathBuf>,
    pub blob_path: PathBuf,
    /// The SHA-256 hex of the downloaded content.
    pub computed_sha256: String,
}

pub fn fetch_asset(params: FetchParams<'_>) -> Result<FetchResult> {
    let FetchParams {
        name,
        asset,
        out_dir,
        cache_dir,
        retries,
        backoff,
        compute_checksum,
        no_reverify,
        materialize_mode,
        transport,
    } = params;

    let uri = asset.expanded_uri();
    let auth = asset.resolved_auth()?;

    let is_oci = uri.starts_with("oci://");
    if !compute_checksum && asset.checksum.is_none() && !is_oci {
        bail!(
            "asset '{name}' has no checksum; refusing to fetch without verification. \
             Use `shasset add --compute` to populate it first, or set `checksum` in the manifest."
        );
    }

    let expected = asset.parsed_checksum()?;

    let filename = asset
        .output_filename()
        .with_context(|| format!("asset '{name}': cannot determine output filename"))?;

    ensure_cache_layout(cache_dir)?;

    if let Some(expected_cs) = &expected {
        let blob_path = cache_blob_path(cache_dir, &expected_cs.hex);
        if blob_path.exists() {
            if no_reverify || verify_on_disk(name, &blob_path, &expected_cs.hex).is_ok() {
                let out_path = if compute_checksum {
                    None
                } else {
                    let out_root = out_dir.with_context(|| {
                        format!("asset '{name}': missing output dir for fetch materialization")
                    })?;
                    Some(materialize_blob(
                        name,
                        &filename,
                        out_root,
                        &blob_path,
                        materialize_mode,
                    )?)
                };

                return Ok(FetchResult {
                    path: out_path,
                    blob_path,
                    computed_sha256: expected_cs.hex.clone(),
                });
            }

            // Cache blob exists but failed verification.
            let _ = std::fs::remove_file(&blob_path);
        }
    }

    // For OCI assets: check the oci-index cache.
    if is_oci {
        if let Some(cached_hex) = oci_index_tar_hex_from_cache(cache_dir, &uri, asset) {
            let blob_path = cache_blob_path(cache_dir, &cached_hex);
            if blob_path.exists() {
                if no_reverify || verify_on_disk(name, &blob_path, &cached_hex).is_ok() {
                    let out_path = if compute_checksum {
                        None
                    } else {
                        let out_root = out_dir.with_context(|| {
                            format!("asset '{name}': missing output dir for fetch materialization")
                        })?;
                        Some(materialize_blob(
                            name,
                            &filename,
                            out_root,
                            &blob_path,
                            materialize_mode,
                        )?)
                    };
                    return Ok(FetchResult {
                        path: out_path,
                        blob_path,
                        computed_sha256: cached_hex,
                    });
                }
                // Cache blob failed verification; remove it and also clean the oci-index entry
                let _ = std::fs::remove_file(&blob_path);
                let _ = std::fs::remove_file(
                    oci_index_path_for_uri(cache_dir, &uri, asset).unwrap_or_default(),
                );
            }
        }
    }

    // Retry loop for cache misses or invalidated cache blobs.
    let mut attempt = 0u32;
    let downloaded = loop {
        attempt += 1;
        let transport_ref: &dyn Transport = match &transport {
            Some(t) => t.as_ref(),
            None => &ReqwestTransport::default(),
        };

        match download_via_scheme(transport_ref, cache_dir, name, asset, &uri, auth.as_deref()) {
            Ok(result) => break result,
            Err(e) => {
                let retryable = e.is_retryable();
                let error = e.into_anyhow();
                if !retryable || attempt > retries {
                    return Err(error.context(format!("fetch failed for asset '{name}'")));
                }
                let wait = compute_backoff(backoff, attempt);
                eprintln!(
                    "[shasset] warning: transient error for '{name}' (attempt {attempt}/{retries}): {error}; retrying in {wait}ms"
                );
                std::thread::sleep(Duration::from_millis(wait));
            }
        }
    };

    if downloaded.bytes_written == 0 {
        let _ = std::fs::remove_file(&downloaded.quarantine_path);
        bail!("asset '{name}': downloaded zero bytes — refusing to write empty file");
    }

    if let Some(content_length) = downloaded.content_length {
        if downloaded.bytes_written != content_length {
            let _ = std::fs::remove_file(&downloaded.quarantine_path);
            bail!(
                "asset '{name}': truncated download — expected {content_length} bytes, got {}",
                downloaded.bytes_written
            );
        }
    }

    if let Some(expected_cs) = &expected {
        if downloaded.computed_sha256 != expected_cs.hex {
            let _ = std::fs::remove_file(&downloaded.quarantine_path);
            bail!(
                "checksum mismatch for asset '{name}': expected sha256:{}, got sha256:{}",
                expected_cs.hex,
                downloaded.computed_sha256
            );
        }
    }

    let blob_path = cache_blob_path(cache_dir, &downloaded.computed_sha256);
    promote_to_cache_blob(&downloaded.quarantine_path, &blob_path)?;

    let out_path = if compute_checksum {
        None
    } else {
        let out_root = out_dir.with_context(|| {
            format!("asset '{name}': missing output dir for fetch materialization")
        })?;
        Some(materialize_blob(
            name,
            &filename,
            out_root,
            &blob_path,
            materialize_mode,
        )?)
    };

    Ok(FetchResult {
        path: out_path,
        blob_path,
        computed_sha256: downloaded.computed_sha256,
    })
}

fn ensure_cache_layout(cache_dir: &Path) -> Result<()> {
    std::fs::create_dir_all(cache_dir.join("blobs/sha256"))
        .with_context(|| format!("cannot create cache dir: {}", cache_dir.display()))?;
    std::fs::create_dir_all(cache_dir.join("quarantine"))
        .with_context(|| format!("cannot create quarantine dir: {}", cache_dir.display()))?;
    std::fs::create_dir_all(cache_dir.join("oci-index"))
        .with_context(|| format!("cannot create oci-index dir: {}", cache_dir.display()))?;
    Ok(())
}

fn cache_blob_path(cache_dir: &Path, hex: &str) -> PathBuf {
    cache_dir.join("blobs").join("sha256").join(hex)
}

#[derive(Debug)]
struct DownloadedFile {
    quarantine_path: PathBuf,
    computed_sha256: String,
    bytes_written: u64,
    content_length: Option<u64>,
}

fn download_via_scheme(
    transport: &dyn Transport,
    cache_dir: &Path,
    name: &str,
    asset: &Asset,
    uri: &str,
    auth: Option<&str>,
) -> std::result::Result<DownloadedFile, FetchError> {
    let parsed = reqwest::Url::parse(uri).map_err(|error| {
        FetchError::permanent(anyhow::Error::new(error).context(format!("invalid uri: {uri}")))
    })?;
    match parsed.scheme() {
        "http" | "https" => try_download(transport, cache_dir, uri, auth, None),
        "github-release" => try_download_github_release(transport, cache_dir, uri, auth),
        "oci" => try_download_oci(transport, cache_dir, name, asset, uri, auth),
        other => Err(FetchError::permanent(anyhow!(
            "unsupported uri scheme '{other}' in asset uri: {uri}"
        ))),
    }
}

#[derive(Debug)]
struct GithubReleaseRef {
    owner: String,
    repo: String,
    tag: String,
    asset_name: String,
}

fn parse_github_release_uri(uri: &str) -> std::result::Result<GithubReleaseRef, FetchError> {
    let parsed = reqwest::Url::parse(uri).map_err(|error| {
        FetchError::permanent(
            anyhow::Error::new(error).context(format!("invalid github-release uri: {uri}")),
        )
    })?;
    if parsed.scheme() != "github-release" {
        return Err(FetchError::permanent(anyhow!(
            "invalid github-release uri scheme in: {uri}"
        )));
    }
    let owner = parsed.host_str().filter(|s| !s.is_empty()).ok_or_else(|| {
        FetchError::permanent(anyhow!("github-release uri is missing owner: {uri}"))
    })?;
    let segments: Vec<_> = parsed
        .path_segments()
        .map(|parts| parts.filter(|s| !s.is_empty()).collect())
        .unwrap_or_default();
    if segments.len() < 3 {
        return Err(FetchError::permanent(anyhow!(
            "github-release uri must be github-release://<owner>/<repo>/<tag>/<asset-name>: {uri}"
        )));
    }
    let repo = segments[0].to_string();
    let asset_name = segments[segments.len() - 1].to_string();
    let tag = segments[1..segments.len() - 1].join("/");
    if asset_name.is_empty() {
        return Err(FetchError::permanent(anyhow!(
            "github-release uri is missing asset name: {uri}"
        )));
    }
    Ok(GithubReleaseRef {
        owner: owner.to_string(),
        repo,
        tag,
        asset_name,
    })
}

fn try_download_github_release(
    transport: &dyn Transport,
    cache_dir: &Path,
    uri: &str,
    auth: Option<&str>,
) -> std::result::Result<DownloadedFile, FetchError> {
    let reference = parse_github_release_uri(uri)?;
    let release_uri = format!(
        "https://api.github.com/repos/{}/{}/releases/tags/{}",
        reference.owner, reference.repo, reference.tag
    );
    let release = transport.get_json(&release_uri, auth)?;
    let assets = release
        .get("assets")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| {
            FetchError::permanent(anyhow!(
                "GitHub release response did not include an assets array for {release_uri}"
            ))
        })?;

    let asset_id = assets
        .iter()
        .find_map(|asset| {
            let name = asset.get("name")?.as_str()?;
            if name == reference.asset_name {
                asset.get("id")?.as_u64()
            } else {
                None
            }
        })
        .ok_or_else(|| {
            FetchError::permanent(anyhow!(
                "asset '{}' not found in GitHub release {} for repo {}/{}",
                reference.asset_name,
                reference.tag,
                reference.owner,
                reference.repo
            ))
        })?;

    let asset_uri = format!(
        "https://api.github.com/repos/{}/{}/releases/assets/{asset_id}",
        reference.owner, reference.repo
    );
    try_download(
        transport,
        cache_dir,
        &asset_uri,
        auth,
        Some("application/octet-stream"),
    )
}

fn try_download(
    transport: &dyn Transport,
    cache_dir: &Path,
    uri: &str,
    auth: Option<&str>,
    accept: Option<&str>,
) -> std::result::Result<DownloadedFile, FetchError> {
    let mut response = transport.get(uri, auth, accept)?;

    let quarantine_path = cache_dir.join("quarantine").join(format!(
        "download-{}-{}.part",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));

    let mut file = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&quarantine_path)
        .map_err(|error| {
            FetchError::permanent(anyhow::Error::new(error).context(format!(
                "cannot create quarantine file for download: {}",
                quarantine_path.display()
            )))
        })?;

    let mut hasher = Sha256::new();
    let mut total = 0u64;
    let mut buf = [0u8; 64 * 1024];

    loop {
        let n = response.body.read(&mut buf).map_err(|error| {
            FetchError::retryable(
                anyhow::Error::new(error)
                    .context(format!("failed to read response body from {uri}")),
            )
        })?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n]).map_err(|error| {
            FetchError::permanent(anyhow::Error::new(error).context(format!(
                "cannot write quarantine file for download: {}",
                quarantine_path.display()
            )))
        })?;
        hasher.update(&buf[..n]);
        total = total.saturating_add(n as u64);
    }
    file.flush().map_err(|error| {
        FetchError::permanent(anyhow::Error::new(error).context(format!(
            "cannot flush quarantine file for download: {}",
            quarantine_path.display()
        )))
    })?;

    Ok(DownloadedFile {
        quarantine_path,
        computed_sha256: hex::encode(hasher.finalize()),
        bytes_written: total,
        content_length: response.content_length,
    })
}

fn http_status_error(status: u16, url: &str) -> FetchError {
    let class = classify_status(status);
    let error = match class {
        RetryClass::Retry => anyhow!("HTTP {status} fetching {url}"),
        RetryClass::NoRetry => anyhow!("HTTP {status} (non-retryable) fetching {url}"),
    };
    match class {
        RetryClass::Retry => FetchError::retryable(error),
        RetryClass::NoRetry => FetchError::permanent(error),
    }
}

fn promote_to_cache_blob(quarantine_path: &Path, blob_path: &Path) -> Result<()> {
    if let Some(parent) = blob_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("cannot create cache blob dir: {}", parent.display()))?;
    }

    if blob_path.exists() {
        let _ = std::fs::remove_file(quarantine_path);
        return Ok(());
    }

    match std::fs::rename(quarantine_path, blob_path) {
        Ok(()) => Ok(()),
        Err(e) => {
            if blob_path.exists() {
                let _ = std::fs::remove_file(quarantine_path);
                Ok(())
            } else {
                Err(e).with_context(|| {
                    format!(
                        "cannot promote cache blob from {} to {}",
                        quarantine_path.display(),
                        blob_path.display()
                    )
                })
            }
        }
    }
}

fn materialize_blob(
    name: &str,
    filename: &str,
    out_dir: &Path,
    blob_path: &Path,
    materialize_mode: MaterializeMode,
) -> Result<PathBuf> {
    let asset_dir = out_dir.join(name);
    std::fs::create_dir_all(&asset_dir)
        .with_context(|| format!("cannot create output dir: {}", asset_dir.display()))?;
    let out_path = asset_dir.join(filename);

    if out_path.exists() {
        std::fs::remove_file(&out_path)
            .with_context(|| format!("cannot replace output file: {}", out_path.display()))?;
    }

    match materialize_mode {
        MaterializeMode::Copy => {
            let tmp_out = asset_dir.join(format!(
                ".{}-{}.tmp",
                filename,
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos()
            ));
            std::fs::copy(blob_path, &tmp_out).with_context(|| {
                format!(
                    "cannot copy cache blob {} to {}",
                    blob_path.display(),
                    tmp_out.display()
                )
            })?;
            std::fs::rename(&tmp_out, &out_path).with_context(|| {
                format!(
                    "cannot move materialized file from {} to {}",
                    tmp_out.display(),
                    out_path.display()
                )
            })?;
        }
        MaterializeMode::Symlink => {
            create_symlink(blob_path, &out_path).with_context(|| {
                format!(
                    "cannot create symlink from {} to {}",
                    out_path.display(),
                    blob_path.display()
                )
            })?;
        }
    }

    Ok(out_path)
}

#[cfg(unix)]
fn create_symlink(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(src, dst)
}

#[cfg(windows)]
fn create_symlink(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::os::windows::fs::symlink_file(src, dst)
}

/// Verify an already-downloaded asset on disk.
pub fn verify_on_disk(name: &str, path: &Path, expected_hex: &str) -> Result<()> {
    if !path.exists() {
        bail!("asset '{name}': file not found at {}", path.display());
    }
    let mut f =
        std::fs::File::open(path).with_context(|| format!("cannot open {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut total = 0u64;
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = f
            .read(&mut buf)
            .with_context(|| format!("cannot read {}", path.display()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        total = total.saturating_add(n as u64);
    }
    if total == 0 {
        bail!("asset '{name}': file is empty: {}", path.display());
    }
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

fn compute_backoff(backoff: &Backoff, attempt: u32) -> u64 {
    let exp = backoff.factor.pow(attempt.saturating_sub(1));
    let ms = backoff.base_ms.saturating_mul(exp);
    ms.min(backoff.max_ms)
}

// ── OCI helpers ──────────────────────────────────────────────────────────────

/// Parse an `oci://` URI into its components.
///
/// URI form: `oci://<registry>/<repo>[:<tag>]@sha256:<64-hex>`
fn parse_oci_uri(uri: &str) -> std::result::Result<OciRef, FetchError> {
    let rest = uri
        .strip_prefix("oci://")
        .ok_or_else(|| FetchError::permanent(anyhow!("URI must use oci:// scheme: {uri}")))?;
    if rest.is_empty() {
        return Err(FetchError::permanent(anyhow!("oci:// URI is empty: {uri}")));
    }
    let (image_part, digest) = rest.rsplit_once('@').ok_or_else(|| {
        FetchError::permanent(anyhow!(
            "oci:// URI must include digest @sha256:<64-hex>: {uri}"
        ))
    })?;
    if image_part.is_empty() {
        return Err(FetchError::permanent(anyhow!(
            "oci:// URI is missing image reference before '@': {uri}"
        )));
    }
    let digest_hex = validate_oci_sha256_digest(digest)
        .map_err(|e| FetchError::permanent(anyhow!("invalid digest in oci:// URI '{uri}': {e}")))?;
    let (registry, repo) = image_part.split_once('/').ok_or_else(|| {
        FetchError::permanent(anyhow!(
            "oci:// URI must have '<registry>/<repo>' after 'oci://': {uri}"
        ))
    })?;
    if registry.is_empty() || repo.is_empty() {
        return Err(FetchError::permanent(anyhow!(
            "oci:// URI has empty registry or repo: {uri}"
        )));
    }
    Ok(OciRef {
        registry: registry.to_string(),
        repo: repo.to_string(),
        digest: digest.to_string(),
        digest_hex,
    })
}

fn validate_oci_sha256_digest(digest: &str) -> Result<String> {
    let hex = digest
        .strip_prefix("sha256:")
        .with_context(|| format!("digest must start with 'sha256:', got: {digest}"))?;
    if hex.len() != 64 {
        bail!(
            "sha256 digest must be 64 hex chars, got {}: {digest}",
            hex.len()
        );
    }
    if !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        bail!("sha256 digest contains non-hex characters: {digest}");
    }
    Ok(hex.to_string())
}

/// Extract the manifest digest hex from an `oci://` URI, if valid.
pub fn oci_manifest_hex_from_uri(uri: &str) -> Option<String> {
    let rest = uri.strip_prefix("oci://")?;
    let (_, digest) = rest.rsplit_once('@')?;
    let hex = digest.strip_prefix("sha256:")?;
    if hex.len() == 64 && hex.chars().all(|c| c.is_ascii_hexdigit()) {
        Some(hex.to_ascii_lowercase())
    } else {
        None
    }
}

/// Extract the manifest digest hex for an OCI asset, trying `asset.digest` first
/// and falling back to the legacy `@sha256:<hex>` URI suffix.
pub fn oci_manifest_hex_from_asset(asset: &Asset, uri: &str) -> Option<String> {
    if let Some(ref d) = asset.digest {
        if let Some(hex) = d.strip_prefix("sha256:") {
            if hex.len() == 64 && hex.chars().all(|c| c.is_ascii_hexdigit()) {
                return Some(hex.to_ascii_lowercase());
            }
        }
    }
    oci_manifest_hex_from_uri(uri)
}

/// Path for the oci-index entry mapping a manifest digest to the assembled-tar sha256.
pub fn oci_index_path(cache_dir: &Path, manifest_hex: &str, platform_slug: &str) -> PathBuf {
    cache_dir
        .join("oci-index")
        .join(format!("{manifest_hex}.{platform_slug}"))
}

/// Read the assembled-tar sha256 hex from an oci-index entry (if present and valid).
pub fn oci_index_tar_hex_from_cache(cache_dir: &Path, uri: &str, asset: &Asset) -> Option<String> {
    let manifest_hex = oci_manifest_hex_from_asset(asset, uri)?;
    let platform_slug = oci_platform_slug_from_asset(asset).ok()?;
    let path = oci_index_path(cache_dir, &manifest_hex, &platform_slug);
    std::fs::read_to_string(&path)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| s.len() == 64 && s.chars().all(|c| c.is_ascii_hexdigit()))
}

/// Enumerate valid oci-index entry keys (`<manifest_hex>.<platform_slug>`).
pub fn oci_index_manifest_hexes_in_cache(cache_dir: &Path) -> Vec<String> {
    let dir = cache_dir.join("oci-index");
    std::fs::read_dir(&dir)
        .ok()
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_ascii_lowercase();
            let (manifest_hex, _platform) = name.split_once('.')?;
            if manifest_hex.len() == 64
                && manifest_hex.chars().all(|c| c.is_ascii_hexdigit())
                && !name.ends_with('.')
            {
                Some(name)
            } else {
                None
            }
        })
        .collect()
}

pub fn oci_platform_slug_from_asset(asset: &Asset) -> Result<String> {
    let (os, arch, variant) = asset.resolved_platform()?;
    Ok(match variant {
        Some(variant) => format!("{os}-{arch}-{variant}"),
        None => format!("{os}-{arch}"),
    })
}

fn oci_index_path_for_uri(cache_dir: &Path, uri: &str, asset: &Asset) -> Option<PathBuf> {
    let manifest_hex = oci_manifest_hex_from_asset(asset, uri)?;
    let platform_slug = oci_platform_slug_from_asset(asset).ok()?;
    Some(oci_index_path(cache_dir, &manifest_hex, &platform_slug))
}

/// Build an [`OciRef`] for `asset` from its URI and resolved digest.
///
/// Accepts both the new form (`oci://<registry>/<repo>` + `asset.digest`) and the
/// legacy form (`oci://<registry>/<repo>@sha256:<hex>`). If both are present they
/// must agree (validated by [`Asset::oci_digest_hex`]). Fails fast if no digest
/// can be resolved.
fn resolve_oci_ref(
    name: &str,
    asset: &Asset,
    uri: &str,
) -> std::result::Result<OciRef, FetchError> {
    // Resolve the effective digest hex (prefers `asset.digest`, falls back to URI suffix,
    // errors if both present and mismatched, errors if neither present).
    let digest_hex = asset
        .oci_digest_hex()
        .map_err(FetchError::permanent)?
        .ok_or_else(|| {
            FetchError::permanent(anyhow!(
                "asset '{name}': OCI asset has no digest pin; add a \
                 'digest: sha256:<64-hex>' field, or use the legacy \
                 oci://<registry>/<repo>@sha256:<hex> URI form"
            ))
        })?;

    // For the legacy URI form (`oci://…@sha256:…`) delegate full URI parsing to
    // `parse_oci_uri` so its strict validation still runs.  For the new clean-URI
    // form we extract registry/repo directly.
    if uri.contains('@') {
        let parsed = parse_oci_uri(uri)?;
        Ok(OciRef {
            registry: parsed.registry,
            repo: parsed.repo,
            digest: format!("sha256:{digest_hex}"),
            digest_hex,
        })
    } else {
        let rest = uri
            .strip_prefix("oci://")
            .ok_or_else(|| FetchError::permanent(anyhow!("URI must use oci:// scheme: {uri}")))?;
        if rest.is_empty() {
            return Err(FetchError::permanent(anyhow!("oci:// URI is empty: {uri}")));
        }
        let (registry, repo) = rest.split_once('/').ok_or_else(|| {
            FetchError::permanent(anyhow!(
                "oci:// URI must have '<registry>/<repo>' after 'oci://': {uri}"
            ))
        })?;
        if registry.is_empty() || repo.is_empty() {
            return Err(FetchError::permanent(anyhow!(
                "oci:// URI has empty registry or repo: {uri}"
            )));
        }
        Ok(OciRef {
            registry: registry.to_string(),
            repo: repo.to_string(),
            digest: format!("sha256:{digest_hex}"),
            digest_hex,
        })
    }
}

/// Parse a `WWW-Authenticate: ******"...",service="...",scope="..."` header.
struct BearerChallenge {
    realm: String,
    service: Option<String>,
    scope: Option<String>,
}

fn parse_bearer_challenge(header: &str) -> Option<BearerChallenge> {
    let rest = header.strip_prefix("Bearer ")?;
    let mut realm: Option<String> = None;
    let mut service: Option<String> = None;
    let mut scope: Option<String> = None;

    let mut s = rest;
    while !s.is_empty() {
        s = s.trim_start_matches(|c: char| c == ',' || c.is_whitespace());
        if s.is_empty() {
            break;
        }
        let eq_pos = s.find('=')?;
        let key = s[..eq_pos].trim().to_ascii_lowercase();
        s = &s[eq_pos + 1..];
        let value = if s.starts_with('"') {
            s = &s[1..];
            let end = s.find('"')?;
            let v = s[..end].to_string();
            s = &s[end + 1..];
            v
        } else {
            let end = s
                .find(|c: char| c == ',' || c.is_whitespace())
                .unwrap_or(s.len());
            let v = s[..end].to_string();
            s = &s[end..];
            v
        };
        match key.as_str() {
            "realm" => realm = Some(value),
            "service" => service = Some(value),
            "scope" => scope = Some(value),
            _ => {}
        }
    }
    Some(BearerChallenge {
        realm: realm?,
        service,
        scope,
    })
}

/// Fetch bytes from an OCI registry URL, performing token exchange on 401.
fn fetch_oci_bytes(
    transport: &dyn Transport,
    url: &str,
    auth: Option<&str>,
    accept: Option<&str>,
) -> std::result::Result<Vec<u8>, FetchError> {
    // Always start OCI fetches unauthenticated and follow the 401 challenge flow.
    // `auth` here is used for token-endpoint credentials (HTTP Basic), not as a
    // pre-issued bearer token for manifest/blob requests.
    let resp = transport.get_oci(url, None, accept)?;
    if resp.status == 200 {
        let mut buf = Vec::new();
        let mut body = resp.body;
        body.read_to_end(&mut buf).map_err(|e| {
            FetchError::retryable(
                anyhow::Error::new(e)
                    .context(format!("failed to read OCI response body from {url}")),
            )
        })?;
        return Ok(buf);
    }

    if resp.status == 401 {
        if let Some(www_auth) = &resp.www_authenticate {
            if let Some(challenge) = parse_bearer_challenge(www_auth) {
                // Build token exchange URL
                let token_url = build_oci_token_url(&challenge)
                    .map_err(|e| FetchError::permanent(e.context("invalid OCI token realm URL")))?;

                // Resolve credentials for the token endpoint:
                //
                // - Some(("user", "pass"))      -> auth: "user:pass" in shasset.yaml
                // - Some(("x-access-token", t)) -> bare-token auth, no colon (GHCR / GitHub PAT)
                // - None                        -> anonymous (no auth field)
                //
                // x-access-token is the GitHub convention for "I only have a token, treat it as a
                // password". GHCR, Harbor, and generic docker-distribution registries all accept
                // this shape via HTTP Basic on the token endpoint. For ECR/AWS or other registries
                // that require a specific username, use auth: "AWS:${TOKEN}" in shasset.yaml.
                let creds: Option<(String, String)> = auth.map(|a| match a.split_once(':') {
                    Some((user, pass)) => (user.to_string(), pass.to_string()),
                    None => ("x-access-token".to_string(), a.to_string()),
                });

                // Fetch token using HTTP Basic auth (required by the OCI/docker token spec)
                let token_resp_result = match &creds {
                    Some((user, password)) => transport.get_with_basic_auth(
                        &token_url,
                        user,
                        password,
                        Some("application/json"),
                    ),
                    None => transport.get(&token_url, None, Some("application/json")),
                };
                let mut token_resp = token_resp_result?;
                let mut token_body = String::new();
                token_resp
                    .body
                    .read_to_string(&mut token_body)
                    .map_err(|e| {
                        FetchError::retryable(
                            anyhow::Error::new(e)
                                .context(format!("failed to read token response from {token_url}")),
                        )
                    })?;
                let token_json: serde_json::Value =
                    serde_json::from_str(&token_body).map_err(|e| {
                        FetchError::permanent(
                            anyhow::Error::new(e).context(format!(
                                "invalid JSON in token response from {token_url}"
                            )),
                        )
                    })?;
                let token = token_json
                    .get("token")
                    .or_else(|| token_json.get("access_token"))
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| {
                        FetchError::permanent(anyhow!(
                            "token exchange response from {token_url} missing 'token' field"
                        ))
                    })?
                    .to_string();

                // Retry with the exchanged token
                let resp2 = transport.get_oci(url, Some(&token), accept)?;
                if resp2.status == 200 {
                    let mut buf = Vec::new();
                    let mut body = resp2.body;
                    body.read_to_end(&mut buf).map_err(|e| {
                        FetchError::retryable(
                            anyhow::Error::new(e)
                                .context(format!("failed to read OCI response body from {url}")),
                        )
                    })?;
                    return Ok(buf);
                }
                return Err(http_status_error(resp2.status, url));
            }
        }
        return Err(http_status_error(401, url));
    }

    Err(http_status_error(resp.status, url))
}

fn build_oci_token_url(challenge: &BearerChallenge) -> Result<String> {
    let mut url = reqwest::Url::parse(&challenge.realm)
        .with_context(|| format!("invalid OCI token realm: {}", challenge.realm))?;
    {
        let mut q = url.query_pairs_mut();
        if let Some(service) = &challenge.service {
            q.append_pair("service", service);
        }
        if let Some(scope) = &challenge.scope {
            q.append_pair("scope", scope);
        }
    }
    Ok(url.to_string())
}

fn verify_digest(data: &[u8], expected_hex: &str) -> std::result::Result<(), FetchError> {
    let actual = hex::encode(sha2::Sha256::digest(data));
    if actual != expected_hex {
        return Err(FetchError::permanent(anyhow!(
            "digest mismatch: expected sha256:{expected_hex}, got sha256:{actual}"
        )));
    }
    Ok(())
}

const MANIFEST_ACCEPT: &str =
    "application/vnd.oci.image.manifest.v1+json,application/vnd.docker.distribution.manifest.v2+json";

/// Pull an OCI image by digest, assemble an OCI image-layout tar, and return a DownloadedFile.
///
/// The oci-index cache is checked by the caller (`fetch_asset`); this function always pulls.
fn try_download_oci(
    transport: &dyn Transport,
    cache_dir: &Path,
    name: &str,
    asset: &Asset,
    uri: &str,
    auth: Option<&str>,
) -> std::result::Result<DownloadedFile, FetchError> {
    let oci_ref = resolve_oci_ref(name, asset, uri)?;
    let asset_platform = asset.resolved_platform().map_err(|e| {
        FetchError::permanent(e.context(format!("asset '{name}': cannot resolve platform")))
    })?;
    let platform_slug = oci_platform_slug_from_asset(asset).map_err(FetchError::permanent)?;

    // 1. Pull manifest by digest
    let manifest_url = format!(
        "https://{}/v2/{}/manifests/{}",
        oci_ref.registry, oci_ref.repo, oci_ref.digest
    );
    let manifest_bytes = fetch_oci_bytes(transport, &manifest_url, auth, Some(MANIFEST_ACCEPT))?;

    // Self-verify manifest digest
    verify_digest(&manifest_bytes, &oci_ref.digest_hex)?;

    // Parse manifest
    let manifest: serde_json::Value = serde_json::from_slice(&manifest_bytes).map_err(|e| {
        FetchError::permanent(anyhow!("failed to parse OCI manifest for '{name}': {e}"))
    })?;

    let media_type = manifest
        .get("mediaType")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let (manifest, manifest_bytes, effective_digest_hex) = if is_image_index(media_type) {
        let child_digest =
            select_platform_child(&manifest, &asset_platform).ok_or_else(|| {
                let platform = match &asset_platform.2 {
                    Some(variant) => {
                        format!("{}/{}/{}", asset_platform.0, asset_platform.1, variant)
                    }
                    None => format!("{}/{}", asset_platform.0, asset_platform.1),
                };
                FetchError::permanent(anyhow!(
                    "oci asset '{name}': image index does not contain a child for platform '{platform}'"
                ))
            })?;
        let child_hex = child_digest
            .strip_prefix("sha256:")
            .ok_or_else(|| {
                FetchError::permanent(anyhow!(
                    "oci asset '{name}': index child digest is not sha256-prefixed: {child_digest}"
                ))
            })?
            .to_string();
        let child_url = format!(
            "https://{}/v2/{}/manifests/{}",
            oci_ref.registry, oci_ref.repo, child_digest
        );
        let child_bytes = fetch_oci_bytes(transport, &child_url, auth, Some(MANIFEST_ACCEPT))?;
        verify_digest(&child_bytes, &child_hex)?;
        let child_manifest: serde_json::Value =
            serde_json::from_slice(&child_bytes).map_err(|e| {
                FetchError::permanent(anyhow!(
                    "failed to parse child OCI manifest for '{name}': {e}"
                ))
            })?;
        let child_media = child_manifest
            .get("mediaType")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if is_image_index(child_media) {
            return Err(FetchError::permanent(anyhow!(
                "oci asset '{name}': nested image index not supported (child {child_digest} is also an index)"
            )));
        }
        (child_manifest, child_bytes, child_hex)
    } else {
        (manifest, manifest_bytes, oci_ref.digest_hex.clone())
    };

    // 2. Pull config blob
    let config = manifest.get("config").ok_or_else(|| {
        FetchError::permanent(anyhow!("OCI manifest for '{name}' missing 'config'"))
    })?;
    let config_digest = config
        .get("digest")
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            FetchError::permanent(anyhow!(
                "OCI config descriptor for '{name}' missing 'digest'"
            ))
        })?;
    let config_hex = config_digest
        .strip_prefix("sha256:")
        .ok_or_else(|| {
            FetchError::permanent(anyhow!(
                "OCI config digest for '{name}' must start with 'sha256:'"
            ))
        })?
        .to_string();

    let config_url = format!(
        "https://{}/v2/{}/blobs/{}",
        oci_ref.registry, oci_ref.repo, config_digest
    );
    let config_bytes = fetch_oci_bytes(transport, &config_url, auth, None)?;
    verify_digest(&config_bytes, &config_hex)?;

    // 3. Pull each layer blob
    let layers_json = manifest
        .get("layers")
        .and_then(|v| v.as_array())
        .ok_or_else(|| {
            FetchError::permanent(anyhow!("OCI manifest for '{name}' missing 'layers'"))
        })?;

    let mut layers: Vec<(String, Vec<u8>)> = Vec::new();
    for layer_desc in layers_json {
        let layer_digest = layer_desc
            .get("digest")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                FetchError::permanent(anyhow!(
                    "OCI layer descriptor for '{name}' missing 'digest'"
                ))
            })?;
        let layer_hex = layer_digest
            .strip_prefix("sha256:")
            .ok_or_else(|| {
                FetchError::permanent(anyhow!(
                    "OCI layer digest for '{name}' must start with 'sha256:'"
                ))
            })?
            .to_string();
        let layer_url = format!(
            "https://{}/v2/{}/blobs/{}",
            oci_ref.registry, oci_ref.repo, layer_digest
        );
        let layer_bytes = fetch_oci_bytes(transport, &layer_url, auth, None)?;
        verify_digest(&layer_bytes, &layer_hex)?;
        layers.push((layer_hex, layer_bytes));
    }

    // 4. Assemble OCI image-layout tar
    let tar_bytes = assemble_oci_archive(
        &effective_digest_hex,
        &manifest,
        &manifest_bytes,
        &config_hex,
        &config_bytes,
        &layers,
    )
    .map_err(|e| {
        FetchError::permanent(e.context(format!("failed to assemble OCI archive for '{name}'")))
    })?;

    let tar_hex = hex::encode(sha2::Sha256::digest(&tar_bytes));

    // 5. Write oci-index entry: manifest-digest → assembled-tar sha256
    let oci_index_dir = cache_dir.join("oci-index");
    std::fs::create_dir_all(&oci_index_dir).map_err(|e| {
        FetchError::permanent(anyhow::Error::new(e).context("cannot create oci-index directory"))
    })?;
    std::fs::write(
        oci_index_path(cache_dir, &oci_ref.digest_hex, &platform_slug),
        &tar_hex,
    )
    .map_err(|e| {
        FetchError::permanent(anyhow::Error::new(e).context("cannot write oci-index entry"))
    })?;

    // 6. Write assembled tar to quarantine
    let quarantine_path = cache_dir.join("quarantine").join(format!(
        "oci-{}-{}-{}.part",
        &effective_digest_hex[..16],
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    let tar_len = tar_bytes.len() as u64;
    std::fs::write(&quarantine_path, &tar_bytes).map_err(|e| {
        FetchError::permanent(anyhow::Error::new(e).context(format!(
            "cannot write OCI tar to quarantine: {}",
            quarantine_path.display()
        )))
    })?;

    Ok(DownloadedFile {
        quarantine_path,
        computed_sha256: tar_hex,
        bytes_written: tar_len,
        content_length: None,
    })
}

fn is_image_index(media_type: &str) -> bool {
    media_type.contains("image.index") || media_type.contains("manifest.list")
}

fn select_platform_child(
    index: &serde_json::Value,
    platform: &(String, String, Option<String>),
) -> Option<String> {
    let manifests = index.get("manifests")?.as_array()?;
    let (wanted_os, wanted_arch, wanted_variant) = platform;
    for entry in manifests {
        let plat = entry.get("platform")?;
        let os = plat.get("os")?.as_str()?;
        let arch = plat.get("architecture")?.as_str()?;
        if os != wanted_os || arch != wanted_arch {
            continue;
        }
        if let Some(want_var) = wanted_variant {
            let variant = plat.get("variant").and_then(|v| v.as_str()).unwrap_or("");
            if variant != want_var {
                continue;
            }
        }
        return entry.get("digest")?.as_str().map(str::to_string);
    }
    None
}

/// Assemble an OCI image-layout archive tarball.
///
/// Layout (entries sorted lexicographically for determinism):
/// ```text
/// blobs/sha256/<manifest-hex>     -- OCI manifest bytes
/// blobs/sha256/<config-hex>       -- OCI config blob bytes
/// blobs/sha256/<layer-hex>        -- OCI layer blob bytes
/// index.json                      -- OCI image index
/// oci-layout                      -- {"imageLayoutVersion":"1.0.0"}
/// ```
fn assemble_oci_archive(
    manifest_hex: &str,
    manifest: &serde_json::Value,
    manifest_bytes: &[u8],
    config_hex: &str,
    config_bytes: &[u8],
    layers: &[(String, Vec<u8>)],
) -> Result<Vec<u8>> {
    // Collect (path, data) pairs, then sort and write
    let mut entries: Vec<(String, Vec<u8>)> = Vec::new();

    let manifest_media_type = manifest
        .get("mediaType")
        .and_then(|v| v.as_str())
        .unwrap_or("application/vnd.oci.image.manifest.v1+json");

    let index_json = serde_json::to_vec(&serde_json::json!({
        "schemaVersion": 2,
        "manifests": [{
            "mediaType": manifest_media_type,
            "digest": format!("sha256:{manifest_hex}"),
            "size": manifest_bytes.len(),
        }]
    }))
    .context("failed to serialize OCI index.json")?;
    entries.push(("index.json".to_string(), index_json));

    let oci_layout = serde_json::to_vec(&serde_json::json!({
        "imageLayoutVersion": "1.0.0",
    }))
    .context("failed to serialize OCI layout file")?;
    entries.push(("oci-layout".to_string(), oci_layout));

    entries.push((
        format!("blobs/sha256/{manifest_hex}"),
        manifest_bytes.to_vec(),
    ));
    entries.push((format!("blobs/sha256/{config_hex}"), config_bytes.to_vec()));
    for (layer_hex, layer_bytes) in layers {
        entries.push((format!("blobs/sha256/{layer_hex}"), layer_bytes.clone()));
    }

    // Deterministic order
    entries.sort_by(|a, b| a.0.cmp(&b.0));

    // Write to in-memory tar
    let mut out = Vec::<u8>::new();
    {
        let mut ar = tar::Builder::new(&mut out);
        for (path, data) in &entries {
            let mut hdr = tar::Header::new_gnu();
            hdr.set_size(data.len() as u64);
            hdr.set_mode(0o644);
            hdr.set_mtime(0);
            hdr.set_uid(0);
            hdr.set_gid(0);
            ar.append_data(&mut hdr, path.as_str(), data.as_slice())
                .with_context(|| format!("failed to write tar entry '{path}'"))?;
        }
        ar.finish().context("failed to finalize OCI archive tar")?;
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{HashSet, VecDeque};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::sync::Mutex;
    use tempfile::TempDir;

    enum MockOutcome {
        Body {
            body: Vec<u8>,
            content_length: Option<u64>,
        },
        Error {
            retryable: bool,
            message: String,
        },
        HttpStatus(u16),
    }

    struct MockTransport {
        calls: Arc<AtomicUsize>,
        outcomes: Mutex<VecDeque<MockOutcome>>,
        expected_requests: Mutex<VecDeque<(String, Option<String>)>>,
    }

    impl MockTransport {
        fn new(body: Vec<u8>) -> (Self, Arc<AtomicUsize>) {
            Self::with_outcomes(vec![MockOutcome::Body {
                body,
                content_length: None,
            }])
        }

        fn with_content_length(body: Vec<u8>, content_length: u64) -> (Self, Arc<AtomicUsize>) {
            Self::with_outcomes(vec![MockOutcome::Body {
                body,
                content_length: Some(content_length),
            }])
        }

        fn with_outcomes(outcomes: Vec<MockOutcome>) -> (Self, Arc<AtomicUsize>) {
            let calls = Arc::new(AtomicUsize::new(0));
            (
                Self {
                    calls: calls.clone(),
                    outcomes: Mutex::new(VecDeque::from(outcomes)),
                    expected_requests: Mutex::new(VecDeque::new()),
                },
                calls,
            )
        }

        fn with_expectations(
            outcomes: Vec<MockOutcome>,
            expected_requests: Vec<(String, Option<String>)>,
        ) -> (Self, Arc<AtomicUsize>) {
            let (mut transport, calls) = Self::with_outcomes(outcomes);
            transport.expected_requests = Mutex::new(VecDeque::from(expected_requests));
            (transport, calls)
        }

        fn call_count(calls: &Arc<AtomicUsize>) -> usize {
            calls.load(Ordering::SeqCst)
        }
    }

    impl Transport for MockTransport {
        fn get(
            &self,
            uri: &str,
            _auth: Option<&str>,
            accept: Option<&str>,
        ) -> std::result::Result<DownloadResponse, FetchError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if let Some((expected_uri, expected_accept)) =
                self.expected_requests.lock().unwrap().pop_front()
            {
                assert_eq!(uri, expected_uri);
                assert_eq!(accept.map(str::to_string), expected_accept);
            }
            let outcome = self
                .outcomes
                .lock()
                .unwrap()
                .pop_front()
                .expect("mock transport exhausted");
            match outcome {
                MockOutcome::Body {
                    body,
                    content_length,
                } => Ok(DownloadResponse {
                    body: Box::new(std::io::Cursor::new(body)),
                    content_length,
                }),
                MockOutcome::Error { retryable, message } => {
                    let error = anyhow!(message);
                    if retryable {
                        Err(FetchError::retryable(error))
                    } else {
                        Err(FetchError::permanent(error))
                    }
                }
                MockOutcome::HttpStatus(status) => Err(http_status_error(status, "mock")),
            }
        }
    }

    fn sha256_hex(data: &[u8]) -> String {
        use sha2::{Digest, Sha256};
        hex::encode(Sha256::digest(data))
    }

    fn test_asset(hex: &str) -> Asset {
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
    fn cache_miss_downloads_and_creates_blob() {
        let data = b"hello world";
        let hex = sha256_hex(data);
        let asset = test_asset(&hex);
        let out = TempDir::new().unwrap();
        let cache = TempDir::new().unwrap();
        let (transport, calls) = MockTransport::new(data.to_vec());

        let result = fetch_asset(FetchParams {
            name: "mytool",
            asset: &asset,
            out_dir: Some(out.path()),
            cache_dir: cache.path(),
            retries: 0,
            backoff: &Backoff::default(),
            compute_checksum: false,
            no_reverify: false,
            materialize_mode: MaterializeMode::Copy,
            transport: Some(Box::new(transport)),
        })
        .unwrap();

        assert_eq!(MockTransport::call_count(&calls), 1);
        assert_eq!(
            result.blob_path,
            cache.path().join("blobs").join("sha256").join(&hex)
        );
        assert!(result.blob_path.exists());
        assert!(result.path.unwrap().exists());
    }

    #[test]
    fn cache_hit_skips_network_and_materializes() {
        let data = b"hello world";
        let hex = sha256_hex(data);
        let asset = test_asset(&hex);
        let out = TempDir::new().unwrap();
        let cache = TempDir::new().unwrap();
        let blob_path = cache.path().join("blobs").join("sha256").join(&hex);
        std::fs::create_dir_all(blob_path.parent().unwrap()).unwrap();
        std::fs::write(&blob_path, data).unwrap();
        let (transport, calls) = MockTransport::new(b"wrong".to_vec());

        let result = fetch_asset(FetchParams {
            name: "mytool",
            asset: &asset,
            out_dir: Some(out.path()),
            cache_dir: cache.path(),
            retries: 0,
            backoff: &Backoff::default(),
            compute_checksum: false,
            no_reverify: false,
            materialize_mode: MaterializeMode::Copy,
            transport: Some(Box::new(transport)),
        })
        .unwrap();

        assert_eq!(MockTransport::call_count(&calls), 0);
        assert_eq!(std::fs::read(result.path.unwrap()).unwrap(), data);
    }

    #[test]
    fn cache_corruption_reverify_redownloads_by_default() {
        let data = b"hello world";
        let hex = sha256_hex(data);
        let asset = test_asset(&hex);
        let out = TempDir::new().unwrap();
        let cache = TempDir::new().unwrap();
        let blob_path = cache.path().join("blobs").join("sha256").join(&hex);
        std::fs::create_dir_all(blob_path.parent().unwrap()).unwrap();
        std::fs::write(&blob_path, b"corrupt").unwrap();
        let (transport, calls) = MockTransport::new(data.to_vec());

        let result = fetch_asset(FetchParams {
            name: "mytool",
            asset: &asset,
            out_dir: Some(out.path()),
            cache_dir: cache.path(),
            retries: 0,
            backoff: &Backoff::default(),
            compute_checksum: false,
            no_reverify: false,
            materialize_mode: MaterializeMode::Copy,
            transport: Some(Box::new(transport)),
        })
        .unwrap();

        assert_eq!(MockTransport::call_count(&calls), 1);
        assert_eq!(std::fs::read(result.path.unwrap()).unwrap(), data);
        assert_eq!(std::fs::read(blob_path).unwrap(), data);
    }

    #[test]
    fn cache_corruption_no_reverify_uses_cached_blob() {
        let data = b"hello world";
        let hex = sha256_hex(data);
        let asset = test_asset(&hex);
        let out = TempDir::new().unwrap();
        let cache = TempDir::new().unwrap();
        let blob_path = cache.path().join("blobs").join("sha256").join(&hex);
        std::fs::create_dir_all(blob_path.parent().unwrap()).unwrap();
        std::fs::write(&blob_path, b"corrupt").unwrap();
        let (transport, calls) = MockTransport::new(data.to_vec());

        let result = fetch_asset(FetchParams {
            name: "mytool",
            asset: &asset,
            out_dir: Some(out.path()),
            cache_dir: cache.path(),
            retries: 0,
            backoff: &Backoff::default(),
            compute_checksum: false,
            no_reverify: true,
            materialize_mode: MaterializeMode::Copy,
            transport: Some(Box::new(transport)),
        })
        .unwrap();

        assert_eq!(MockTransport::call_count(&calls), 0);
        assert_eq!(std::fs::read(result.path.unwrap()).unwrap(), b"corrupt");
    }

    #[test]
    fn checksum_mismatch_cleans_quarantine_and_no_blob() {
        let data = b"hello world";
        let wrong_hex = "0".repeat(64);
        let asset = test_asset(&wrong_hex);
        let out = TempDir::new().unwrap();
        let cache = TempDir::new().unwrap();
        let (transport, _) = MockTransport::new(data.to_vec());

        let err = fetch_asset(FetchParams {
            name: "mytool",
            asset: &asset,
            out_dir: Some(out.path()),
            cache_dir: cache.path(),
            retries: 0,
            backoff: &Backoff::default(),
            compute_checksum: false,
            no_reverify: false,
            materialize_mode: MaterializeMode::Copy,
            transport: Some(Box::new(transport)),
        })
        .unwrap_err();

        assert!(err.to_string().contains("checksum mismatch"));
        assert!(!cache
            .path()
            .join("blobs")
            .join("sha256")
            .join(wrong_hex)
            .exists());
        assert!(std::fs::read_dir(cache.path().join("quarantine"))
            .unwrap()
            .next()
            .is_none());
    }

    #[test]
    fn zero_byte_download_errors_and_no_blob() {
        let data = Vec::new();
        let hex = sha256_hex(&data);
        let asset = test_asset(&hex);
        let out = TempDir::new().unwrap();
        let cache = TempDir::new().unwrap();
        let (transport, _) = MockTransport::new(data);

        let err = fetch_asset(FetchParams {
            name: "mytool",
            asset: &asset,
            out_dir: Some(out.path()),
            cache_dir: cache.path(),
            retries: 0,
            backoff: &Backoff::default(),
            compute_checksum: false,
            no_reverify: false,
            materialize_mode: MaterializeMode::Copy,
            transport: Some(Box::new(transport)),
        })
        .unwrap_err();

        assert!(err.to_string().contains("zero bytes"));
        assert!(!cache.path().join("blobs").join("sha256").join(hex).exists());
    }

    #[test]
    fn truncated_download_errors_and_no_blob() {
        let data = b"hello".to_vec();
        let hex = sha256_hex(&data);
        let asset = test_asset(&hex);
        let out = TempDir::new().unwrap();
        let cache = TempDir::new().unwrap();
        let (transport, _) = MockTransport::with_content_length(data, 100);

        let err = fetch_asset(FetchParams {
            name: "mytool",
            asset: &asset,
            out_dir: Some(out.path()),
            cache_dir: cache.path(),
            retries: 0,
            backoff: &Backoff::default(),
            compute_checksum: false,
            no_reverify: false,
            materialize_mode: MaterializeMode::Copy,
            transport: Some(Box::new(transport)),
        })
        .unwrap_err();

        assert!(err.to_string().contains("truncated"));
        assert!(!cache.path().join("blobs").join("sha256").join(hex).exists());
    }

    #[test]
    fn compute_mode_promotes_blob_without_output_file() {
        let data = b"hello world";
        let hex = sha256_hex(data);
        let asset = Asset {
            uri: "https://example.com/v1/tool".to_string(),
            version: "1".to_string(),
            checksum: None,
            digest: None,
            filename: Some("tool.bin".to_string()),
            auth: None,
            platform: None,
            archive: false,
            labels: Default::default(),
        };
        let cache = TempDir::new().unwrap();
        let (transport, calls) = MockTransport::new(data.to_vec());

        let result = fetch_asset(FetchParams {
            name: "mytool",
            asset: &asset,
            out_dir: None,
            cache_dir: cache.path(),
            retries: 0,
            backoff: &Backoff::default(),
            compute_checksum: true,
            no_reverify: false,
            materialize_mode: MaterializeMode::Copy,
            transport: Some(Box::new(transport)),
        })
        .unwrap();

        assert_eq!(MockTransport::call_count(&calls), 1);
        assert!(result.path.is_none());
        assert_eq!(result.computed_sha256, hex);
        assert!(cache.path().join("blobs").join("sha256").join(hex).exists());
    }

    #[test]
    fn symlink_materialization_points_at_blob() {
        let data = b"hello world";
        let hex = sha256_hex(data);
        let asset = test_asset(&hex);
        let out = TempDir::new().unwrap();
        let cache = TempDir::new().unwrap();
        let (transport, _) = MockTransport::new(data.to_vec());

        let result = fetch_asset(FetchParams {
            name: "mytool",
            asset: &asset,
            out_dir: Some(out.path()),
            cache_dir: cache.path(),
            retries: 0,
            backoff: &Backoff::default(),
            compute_checksum: false,
            no_reverify: false,
            materialize_mode: MaterializeMode::Symlink,
            transport: Some(Box::new(transport)),
        })
        .unwrap();

        let out_path = result.path.unwrap();
        let link_target = std::fs::read_link(&out_path).unwrap();
        assert_eq!(
            link_target,
            cache.path().join("blobs").join("sha256").join(hex)
        );
    }

    #[test]
    fn github_release_scheme_resolves_asset_then_downloads_bytes() {
        let data = b"hello from github release";
        let hex = sha256_hex(data);
        let asset = Asset {
            uri: "github-release://botworkz/tools/v1.2.3/tool.tar.gz".to_string(),
            version: "1".to_string(),
            checksum: Some(format!("sha256:{hex}")),
            digest: None,
            filename: None,
            auth: Some("${SHASSET_TEST_TOKEN}".to_string()),
            platform: None,
            archive: false,
            labels: Default::default(),
        };
        std::env::set_var("SHASSET_TEST_TOKEN", "token123");

        let out = TempDir::new().unwrap();
        let cache = TempDir::new().unwrap();

        let release_json = serde_json::json!({
            "assets": [
                {"id": 42u64, "name": "tool.tar.gz"},
                {"id": 99u64, "name": "other.tar.gz"}
            ]
        })
        .to_string()
        .into_bytes();

        let (transport, calls) = MockTransport::with_expectations(
            vec![
                MockOutcome::Body {
                    body: release_json,
                    content_length: None,
                },
                MockOutcome::Body {
                    body: data.to_vec(),
                    content_length: None,
                },
            ],
            vec![
                (
                    "https://api.github.com/repos/botworkz/tools/releases/tags/v1.2.3".to_string(),
                    Some("application/vnd.github+json".to_string()),
                ),
                (
                    "https://api.github.com/repos/botworkz/tools/releases/assets/42".to_string(),
                    Some("application/octet-stream".to_string()),
                ),
            ],
        );

        let result = fetch_asset(FetchParams {
            name: "mytool",
            asset: &asset,
            out_dir: Some(out.path()),
            cache_dir: cache.path(),
            retries: 0,
            backoff: &Backoff::default(),
            compute_checksum: false,
            no_reverify: false,
            materialize_mode: MaterializeMode::Copy,
            transport: Some(Box::new(transport)),
        })
        .unwrap();

        assert_eq!(MockTransport::call_count(&calls), 2);
        assert_eq!(std::fs::read(result.path.unwrap()).unwrap(), data);
    }

    #[test]
    fn parse_github_release_uri_allows_slashes_in_tag() {
        let slash_tag = parse_github_release_uri(
            "github-release://botworkz/botwork-extra/release/0.0.3/botwork-vault",
        )
        .unwrap();
        assert_eq!(slash_tag.owner, "botworkz");
        assert_eq!(slash_tag.repo, "botwork-extra");
        assert_eq!(slash_tag.tag, "release/0.0.3");
        assert_eq!(slash_tag.asset_name, "botwork-vault");

        let single_segment_tag =
            parse_github_release_uri("github-release://botworkz/tools/v1.2.3/tool.tar.gz").unwrap();
        assert_eq!(single_segment_tag.owner, "botworkz");
        assert_eq!(single_segment_tag.repo, "tools");
        assert_eq!(single_segment_tag.tag, "v1.2.3");
        assert_eq!(single_segment_tag.asset_name, "tool.tar.gz");
    }

    #[test]
    fn github_release_scheme_supports_slashes_in_tag() {
        let data = b"hello from github release with slash tag";
        let hex = sha256_hex(data);
        let asset = Asset {
            uri: "github-release://botworkz/botwork-extra/release/0.0.3/botwork-vault".to_string(),
            version: "1".to_string(),
            checksum: Some(format!("sha256:{hex}")),
            digest: None,
            filename: None,
            auth: Some("${SHASSET_TEST_TOKEN}".to_string()),
            platform: None,
            archive: false,
            labels: Default::default(),
        };
        std::env::set_var("SHASSET_TEST_TOKEN", "token123");

        let out = TempDir::new().unwrap();
        let cache = TempDir::new().unwrap();

        let release_json = serde_json::json!({
            "assets": [
                {"id": 7u64, "name": "botwork-vault"},
                {"id": 99u64, "name": "other.tar.gz"}
            ]
        })
        .to_string()
        .into_bytes();

        let (transport, calls) = MockTransport::with_expectations(
            vec![
                MockOutcome::Body {
                    body: release_json,
                    content_length: None,
                },
                MockOutcome::Body {
                    body: data.to_vec(),
                    content_length: None,
                },
            ],
            vec![
                (
                    "https://api.github.com/repos/botworkz/botwork-extra/releases/tags/release/0.0.3"
                        .to_string(),
                    Some("application/vnd.github+json".to_string()),
                ),
                (
                    "https://api.github.com/repos/botworkz/botwork-extra/releases/assets/7"
                        .to_string(),
                    Some("application/octet-stream".to_string()),
                ),
            ],
        );

        let result = fetch_asset(FetchParams {
            name: "mytool",
            asset: &asset,
            out_dir: Some(out.path()),
            cache_dir: cache.path(),
            retries: 0,
            backoff: &Backoff::default(),
            compute_checksum: false,
            no_reverify: false,
            materialize_mode: MaterializeMode::Copy,
            transport: Some(Box::new(transport)),
        })
        .unwrap();

        assert_eq!(MockTransport::call_count(&calls), 2);
        assert_eq!(result.computed_sha256, hex);
        assert_eq!(std::fs::read(result.path.unwrap()).unwrap(), data);
    }

    #[test]
    fn github_release_scheme_errors_when_asset_missing() {
        let asset = Asset {
            uri: "github-release://botworkz/tools/v1.2.3/missing.tar.gz".to_string(),
            version: "1".to_string(),
            checksum: Some(format!("sha256:{}", "a".repeat(64))),
            digest: None,
            filename: None,
            auth: None,
            platform: None,
            archive: false,
            labels: Default::default(),
        };
        let out = TempDir::new().unwrap();
        let cache = TempDir::new().unwrap();
        let release_json = serde_json::json!({
            "assets": [
                {"id": 42u64, "name": "tool.tar.gz"}
            ]
        })
        .to_string()
        .into_bytes();
        let (transport, calls) = MockTransport::new(release_json);

        let err = fetch_asset(FetchParams {
            name: "mytool",
            asset: &asset,
            out_dir: Some(out.path()),
            cache_dir: cache.path(),
            retries: 0,
            backoff: &Backoff::default(),
            compute_checksum: false,
            no_reverify: false,
            materialize_mode: MaterializeMode::Copy,
            transport: Some(Box::new(transport)),
        })
        .unwrap_err();

        assert_eq!(MockTransport::call_count(&calls), 1);
        assert!(format!("{err:#}").contains("asset 'missing.tar.gz' not found"));
    }

    #[test]
    fn unsupported_uri_scheme_is_permanent_error() {
        let cache = TempDir::new().unwrap();
        let (transport, calls) = MockTransport::new(b"ignored".to_vec());
        let asset = Asset {
            uri: "ftp://example.com/x".to_string(),
            version: String::new(),
            checksum: None,
            digest: None,
            filename: None,
            auth: None,
            platform: None,
            archive: false,
            labels: Default::default(),
        };

        let err = download_via_scheme(
            &transport,
            cache.path(),
            "test",
            &asset,
            "ftp://example.com/x",
            None,
        )
        .unwrap_err();

        assert_eq!(MockTransport::call_count(&calls), 0);
        assert!(format!("{err:#}").contains("unsupported uri scheme 'ftp'"));
    }

    #[test]
    fn parse_github_release_uri_requires_repo_tag_and_asset_segments() {
        let err = parse_github_release_uri("github-release://botworkz/botwork-extra").unwrap_err();

        assert!(format!("{err:#}").contains(
            "github-release uri must be github-release://<owner>/<repo>/<tag>/<asset-name>"
        ));
    }

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
    fn retryable_transport_error_retries_until_exhausted() {
        let tmp = TempDir::new().unwrap();
        let asset = test_asset(&"a".repeat(64));
        let (transport, calls) = MockTransport::with_outcomes(vec![
            MockOutcome::Error {
                retryable: true,
                message: "client error (Connect): dns error: Temporary failure in name resolution"
                    .to_string(),
            },
            MockOutcome::Error {
                retryable: true,
                message: "client error (Connect): dns error: Temporary failure in name resolution"
                    .to_string(),
            },
            MockOutcome::Error {
                retryable: true,
                message: "client error (Connect): dns error: Temporary failure in name resolution"
                    .to_string(),
            },
        ]);

        let err = fetch_asset(FetchParams {
            name: "mytool",
            asset: &asset,
            out_dir: Some(tmp.path()),
            cache_dir: tmp.path(),
            retries: 2,
            backoff: &Backoff {
                base_ms: 0,
                max_ms: 0,
                factor: 1,
            },
            compute_checksum: false,
            no_reverify: false,
            materialize_mode: MaterializeMode::Copy,
            transport: Some(Box::new(transport)),
        })
        .unwrap_err();

        assert_eq!(MockTransport::call_count(&calls), 3);
        assert!(format!("{err:#}").contains("Temporary failure in name resolution"));
    }

    #[test]
    fn retryable_transport_error_then_success_retries_once() {
        let data = b"hello world";
        let hex = sha256_hex(data);
        let asset = test_asset(&hex);
        let out = TempDir::new().unwrap();
        let cache = TempDir::new().unwrap();
        let (transport, calls) = MockTransport::with_outcomes(vec![
            MockOutcome::Error {
                retryable: true,
                message: "client error (Connect): dns error: Temporary failure in name resolution"
                    .to_string(),
            },
            MockOutcome::Body {
                body: data.to_vec(),
                content_length: None,
            },
        ]);

        let result = fetch_asset(FetchParams {
            name: "mytool",
            asset: &asset,
            out_dir: Some(out.path()),
            cache_dir: cache.path(),
            retries: 3,
            backoff: &Backoff {
                base_ms: 0,
                max_ms: 0,
                factor: 1,
            },
            compute_checksum: false,
            no_reverify: false,
            materialize_mode: MaterializeMode::Copy,
            transport: Some(Box::new(transport)),
        })
        .unwrap();

        assert_eq!(MockTransport::call_count(&calls), 2);
        assert_eq!(std::fs::read(result.path.unwrap()).unwrap(), data);
    }

    #[test]
    fn permanent_http_error_is_not_retried() {
        let tmp = TempDir::new().unwrap();
        let asset = test_asset(&"a".repeat(64));
        let (transport, calls) = MockTransport::with_outcomes(vec![MockOutcome::HttpStatus(404)]);

        let err = fetch_asset(FetchParams {
            name: "mytool",
            asset: &asset,
            out_dir: Some(tmp.path()),
            cache_dir: tmp.path(),
            retries: 3,
            backoff: &Backoff {
                base_ms: 0,
                max_ms: 0,
                factor: 1,
            },
            compute_checksum: false,
            no_reverify: false,
            materialize_mode: MaterializeMode::Copy,
            transport: Some(Box::new(transport)),
        })
        .unwrap_err();

        assert_eq!(MockTransport::call_count(&calls), 1);
        assert!(format!("{err:#}").contains("HTTP 404"));
    }

    // ── OCI tests ─────────────────────────────────────────────────────────────

    fn make_gzip(data: &[u8]) -> Vec<u8> {
        use flate2::{write::GzEncoder, Compression};
        use std::io::Write;
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(data).unwrap();
        encoder.finish().unwrap()
    }

    fn make_minimal_layer_tar() -> Vec<u8> {
        // A minimal tar with one file "hello"
        let mut out = Vec::new();
        {
            let mut ar = tar::Builder::new(&mut out);
            let data = b"hello";
            let mut hdr = tar::Header::new_gnu();
            hdr.set_size(data.len() as u64);
            hdr.set_mode(0o644);
            hdr.set_mtime(0);
            hdr.set_uid(0);
            hdr.set_gid(0);
            ar.append_data(&mut hdr, "hello.txt", data.as_slice())
                .unwrap();
            ar.finish().unwrap();
        }
        out
    }

    #[test]
    fn oci_uri_pulls_manifest_then_config_then_layers() {
        let layer_tar = make_minimal_layer_tar();
        let layer_gz = make_gzip(&layer_tar);
        let layer_gz_hex = sha256_hex(&layer_gz);

        let config_bytes = b"{\"architecture\":\"amd64\"}";
        let config_hex = sha256_hex(config_bytes);

        let manifest = serde_json::json!({
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "config": {
                "mediaType": "application/vnd.oci.image.config.v1+json",
                "digest": format!("sha256:{config_hex}"),
                "size": config_bytes.len()
            },
            "layers": [{
                "mediaType": "application/vnd.oci.image.layer.v1.tar+gzip",
                "digest": format!("sha256:{layer_gz_hex}"),
                "size": layer_gz.len()
            }]
        });
        let manifest_bytes = serde_json::to_vec(&manifest).unwrap();
        let manifest_hex = sha256_hex(&manifest_bytes);

        let asset = Asset {
            uri: format!("oci://ghcr.io/botworkz/svc@sha256:{manifest_hex}"),
            version: String::new(),
            checksum: None,
            digest: None,
            filename: Some("svc.tar".to_string()),
            auth: None,
            platform: None,
            archive: false,
            labels: Default::default(),
        };

        let cache = TempDir::new().unwrap();
        let (transport, calls) = MockTransport::with_expectations(
            vec![
                MockOutcome::Body {
                    body: manifest_bytes,
                    content_length: None,
                },
                MockOutcome::Body {
                    body: config_bytes.to_vec(),
                    content_length: None,
                },
                MockOutcome::Body {
                    body: layer_gz.clone(),
                    content_length: None,
                },
            ],
            vec![
                (
                    format!("https://ghcr.io/v2/botworkz/svc/manifests/sha256:{manifest_hex}"),
                    Some(MANIFEST_ACCEPT.to_string()),
                ),
                (
                    format!("https://ghcr.io/v2/botworkz/svc/blobs/sha256:{config_hex}"),
                    None,
                ),
                (
                    format!("https://ghcr.io/v2/botworkz/svc/blobs/sha256:{layer_gz_hex}"),
                    None,
                ),
            ],
        );

        let result = fetch_asset(FetchParams {
            name: "my-svc",
            asset: &asset,
            out_dir: None,
            cache_dir: cache.path(),
            retries: 0,
            backoff: &Backoff::default(),
            compute_checksum: true,
            no_reverify: false,
            materialize_mode: MaterializeMode::Copy,
            transport: Some(Box::new(transport)),
        })
        .unwrap();

        assert_eq!(MockTransport::call_count(&calls), 3);
        assert!(result.blob_path.exists());

        // Parse the resulting tar and verify OCI image-layout structure.
        let blob_bytes = std::fs::read(&result.blob_path).unwrap();
        let mut ar = tar::Archive::new(blob_bytes.as_slice());
        let mut seen_paths = HashSet::new();
        let mut index_digest: Option<String> = None;
        let mut image_layout_version: Option<String> = None;
        for entry in ar.entries().unwrap() {
            let mut entry = entry.unwrap();
            let path = entry.path().unwrap().to_string_lossy().to_string();
            seen_paths.insert(path.clone());
            if path == "index.json" {
                let index_json: serde_json::Value = serde_json::from_reader(&mut entry).unwrap();
                index_digest = index_json["manifests"][0]["digest"]
                    .as_str()
                    .map(ToString::to_string);
            } else if path == "oci-layout" {
                let layout_json: serde_json::Value = serde_json::from_reader(&mut entry).unwrap();
                image_layout_version = layout_json["imageLayoutVersion"]
                    .as_str()
                    .map(ToString::to_string);
            }
        }
        let expected_index_digest = format!("sha256:{manifest_hex}");
        assert_eq!(
            index_digest.as_deref(),
            Some(expected_index_digest.as_str())
        );
        assert_eq!(image_layout_version.as_deref(), Some("1.0.0"));
        assert!(
            seen_paths.iter().any(|p| p.starts_with("blobs/sha256/")),
            "no blobs/sha256/* entries found in assembled tar"
        );
        assert!(
            !seen_paths.contains("manifest.json"),
            "assembled tar should not contain top-level manifest.json"
        );
    }

    #[test]
    fn oci_uri_self_verifies_manifest_digest() {
        let config_bytes = b"{\"architecture\":\"amd64\"}";
        let config_hex = sha256_hex(config_bytes);

        let manifest = serde_json::json!({
            "config": {"digest": format!("sha256:{config_hex}"), "mediaType": "x"},
            "layers": []
        });
        let manifest_bytes = serde_json::to_vec(&manifest).unwrap();
        let real_hex = sha256_hex(&manifest_bytes);

        // Corrupt the manifest bytes
        let mut corrupt = manifest_bytes.clone();
        corrupt[0] = b'X';

        // We declare the real hex in the URI but serve corrupt bytes
        let asset = Asset {
            uri: format!("oci://ghcr.io/botworkz/svc@sha256:{real_hex}"),
            version: String::new(),
            checksum: None,
            digest: None,
            filename: Some("svc.tar".to_string()),
            auth: None,
            platform: None,
            archive: false,
            labels: Default::default(),
        };

        let cache = TempDir::new().unwrap();
        let (transport, _) = MockTransport::new(corrupt);

        let err = fetch_asset(FetchParams {
            name: "my-svc",
            asset: &asset,
            out_dir: None,
            cache_dir: cache.path(),
            retries: 0,
            backoff: &Backoff::default(),
            compute_checksum: true,
            no_reverify: false,
            materialize_mode: MaterializeMode::Copy,
            transport: Some(Box::new(transport)),
        })
        .unwrap_err();

        assert!(
            format!("{err:#}").contains("digest mismatch"),
            "error was: {err:#}"
        );
    }

    #[test]
    fn oci_uri_self_verifies_blob_digest() {
        let config_bytes = b"{\"architecture\":\"amd64\"}";
        let config_hex = sha256_hex(config_bytes);

        let layer_gz = make_gzip(b"some data");
        let layer_gz_hex = sha256_hex(&layer_gz);

        let manifest = serde_json::json!({
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "config": {
                "digest": format!("sha256:{config_hex}"),
                "mediaType": "application/vnd.oci.image.config.v1+json"
            },
            "layers": [{
                "mediaType": "application/vnd.oci.image.layer.v1.tar+gzip",
                "digest": format!("sha256:{layer_gz_hex}"),
                "size": layer_gz.len()
            }]
        });
        let manifest_bytes = serde_json::to_vec(&manifest).unwrap();
        let manifest_hex = sha256_hex(&manifest_bytes);

        let asset = Asset {
            uri: format!("oci://ghcr.io/botworkz/svc@sha256:{manifest_hex}"),
            version: String::new(),
            checksum: None,
            digest: None,
            filename: Some("svc.tar".to_string()),
            auth: None,
            platform: None,
            archive: false,
            labels: Default::default(),
        };

        let cache = TempDir::new().unwrap();
        // Serve correct manifest, correct config, but CORRUPT layer
        let (transport, _) = MockTransport::with_outcomes(vec![
            MockOutcome::Body {
                body: manifest_bytes,
                content_length: None,
            },
            MockOutcome::Body {
                body: config_bytes.to_vec(),
                content_length: None,
            },
            MockOutcome::Body {
                body: b"corrupt layer data".to_vec(),
                content_length: None,
            },
        ]);

        let err = fetch_asset(FetchParams {
            name: "my-svc",
            asset: &asset,
            out_dir: None,
            cache_dir: cache.path(),
            retries: 0,
            backoff: &Backoff::default(),
            compute_checksum: true,
            no_reverify: false,
            materialize_mode: MaterializeMode::Copy,
            transport: Some(Box::new(transport)),
        })
        .unwrap_err();

        assert!(
            format!("{err:#}").contains("digest mismatch"),
            "error was: {err:#}"
        );
    }

    #[test]
    fn oci_uri_image_index_selects_matching_platform() {
        let (child_manifest_bytes, config_bytes, layer_gz, child_manifest_hex) =
            make_oci_test_fixtures();
        let config_hex = sha256_hex(&config_bytes);
        let layer_gz_hex = sha256_hex(&layer_gz);
        let child_digest = format!("sha256:{child_manifest_hex}");

        let index = serde_json::json!({
            "mediaType": "application/vnd.oci.image.index.v1+json",
            "manifests": [
                {
                    "digest": "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                    "platform": {"os": "linux", "architecture": "arm64"}
                },
                {
                    "digest": child_digest,
                    "platform": {"os": "linux", "architecture": "amd64"}
                }
            ]
        });
        let index_bytes = serde_json::to_vec(&index).unwrap();
        let index_hex = sha256_hex(&index_bytes);

        let asset = Asset {
            uri: format!("oci://ghcr.io/botworkz/svc@sha256:{index_hex}"),
            version: String::new(),
            checksum: None,
            digest: None,
            filename: Some("svc.tar".to_string()),
            auth: None,
            platform: None,
            archive: false,
            labels: Default::default(),
        };

        let cache = TempDir::new().unwrap();
        let (transport, calls) = MockTransport::with_expectations(
            vec![
                MockOutcome::Body {
                    body: index_bytes,
                    content_length: None,
                },
                MockOutcome::Body {
                    body: child_manifest_bytes,
                    content_length: None,
                },
                MockOutcome::Body {
                    body: config_bytes,
                    content_length: None,
                },
                MockOutcome::Body {
                    body: layer_gz,
                    content_length: None,
                },
            ],
            vec![
                (
                    format!("https://ghcr.io/v2/botworkz/svc/manifests/sha256:{index_hex}"),
                    Some(MANIFEST_ACCEPT.to_string()),
                ),
                (
                    format!(
                        "https://ghcr.io/v2/botworkz/svc/manifests/sha256:{child_manifest_hex}"
                    ),
                    Some(MANIFEST_ACCEPT.to_string()),
                ),
                (
                    format!("https://ghcr.io/v2/botworkz/svc/blobs/sha256:{config_hex}"),
                    None,
                ),
                (
                    format!("https://ghcr.io/v2/botworkz/svc/blobs/sha256:{layer_gz_hex}"),
                    None,
                ),
            ],
        );

        let result = fetch_asset(FetchParams {
            name: "my-svc",
            asset: &asset,
            out_dir: None,
            cache_dir: cache.path(),
            retries: 0,
            backoff: &Backoff::default(),
            compute_checksum: true,
            no_reverify: false,
            materialize_mode: MaterializeMode::Copy,
            transport: Some(Box::new(transport)),
        })
        .unwrap();

        assert_eq!(MockTransport::call_count(&calls), 4);
        let index_entry = cache
            .path()
            .join("oci-index")
            .join(format!("{index_hex}.linux-amd64"));
        assert_eq!(
            std::fs::read_to_string(index_entry).unwrap().trim(),
            result.computed_sha256
        );
    }

    #[test]
    fn oci_uri_image_index_errors_when_platform_not_found() {
        let index = serde_json::json!({
            "mediaType": "application/vnd.oci.image.index.v1+json",
            "manifests": [
                {
                    "digest": "sha256:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc",
                    "platform": {"os": "linux", "architecture": "arm64"}
                }
            ]
        });
        let index_bytes = serde_json::to_vec(&index).unwrap();
        let index_hex = sha256_hex(&index_bytes);

        let asset = Asset {
            uri: format!("oci://ghcr.io/botworkz/svc@sha256:{index_hex}"),
            version: String::new(),
            checksum: None,
            digest: None,
            filename: Some("svc.tar".to_string()),
            auth: None,
            platform: None,
            archive: false,
            labels: Default::default(),
        };

        let cache = TempDir::new().unwrap();
        let (transport, calls) = MockTransport::new(index_bytes);

        let err = fetch_asset(FetchParams {
            name: "my-svc",
            asset: &asset,
            out_dir: None,
            cache_dir: cache.path(),
            retries: 0,
            backoff: &Backoff::default(),
            compute_checksum: true,
            no_reverify: false,
            materialize_mode: MaterializeMode::Copy,
            transport: Some(Box::new(transport)),
        })
        .unwrap_err();

        assert_eq!(MockTransport::call_count(&calls), 1);
        assert!(
            format!("{err:#}")
                .contains("image index does not contain a child for platform 'linux/amd64'"),
            "error was: {err:#}"
        );
    }

    #[test]
    fn oci_uri_nested_image_index_is_rejected() {
        let child_index = serde_json::json!({
            "mediaType": "application/vnd.docker.distribution.manifest.list.v2+json",
            "manifests": []
        });
        let child_index_bytes = serde_json::to_vec(&child_index).unwrap();
        let child_hex = sha256_hex(&child_index_bytes);

        let index = serde_json::json!({
            "mediaType": "application/vnd.oci.image.index.v1+json",
            "manifests": [
                {
                    "digest": format!("sha256:{child_hex}"),
                    "platform": {"os": "linux", "architecture": "amd64"}
                }
            ]
        });
        let index_bytes = serde_json::to_vec(&index).unwrap();
        let index_hex = sha256_hex(&index_bytes);

        let asset = Asset {
            uri: format!("oci://ghcr.io/botworkz/svc@sha256:{index_hex}"),
            version: String::new(),
            checksum: None,
            digest: None,
            filename: Some("svc.tar".to_string()),
            auth: None,
            platform: Some("linux/amd64".to_string()),
            archive: false,
            labels: Default::default(),
        };

        let cache = TempDir::new().unwrap();
        let (transport, calls) = MockTransport::with_expectations(
            vec![
                MockOutcome::Body {
                    body: index_bytes,
                    content_length: None,
                },
                MockOutcome::Body {
                    body: child_index_bytes,
                    content_length: None,
                },
            ],
            vec![
                (
                    format!("https://ghcr.io/v2/botworkz/svc/manifests/sha256:{index_hex}"),
                    Some(MANIFEST_ACCEPT.to_string()),
                ),
                (
                    format!("https://ghcr.io/v2/botworkz/svc/manifests/sha256:{child_hex}"),
                    Some(MANIFEST_ACCEPT.to_string()),
                ),
            ],
        );

        let err = fetch_asset(FetchParams {
            name: "my-svc",
            asset: &asset,
            out_dir: None,
            cache_dir: cache.path(),
            retries: 0,
            backoff: &Backoff::default(),
            compute_checksum: true,
            no_reverify: false,
            materialize_mode: MaterializeMode::Copy,
            transport: Some(Box::new(transport)),
        })
        .unwrap_err();

        assert_eq!(MockTransport::call_count(&calls), 2);
        assert!(
            format!("{err:#}").contains("nested image index not supported"),
            "error was: {err:#}"
        );
    }

    #[test]
    fn oci_uri_deterministic_tar_assembly() {
        let config_bytes = b"{\"architecture\":\"amd64\"}";
        let config_hex = sha256_hex(config_bytes);
        let layer_tar = make_minimal_layer_tar();
        let layer_gz = make_gzip(&layer_tar);
        let layer_gz_hex = sha256_hex(&layer_gz);

        let manifest = serde_json::json!({
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "config": {
                "digest": format!("sha256:{config_hex}"),
                "mediaType": "application/vnd.oci.image.config.v1+json"
            },
            "layers": [{
                "mediaType": "application/vnd.oci.image.layer.v1.tar+gzip",
                "digest": format!("sha256:{layer_gz_hex}"),
                "size": layer_gz.len()
            }]
        });
        let manifest_bytes = serde_json::to_vec(&manifest).unwrap();
        let manifest_hex = sha256_hex(&manifest_bytes);

        let assemble_fresh = || -> Vec<u8> {
            let asset = Asset {
                uri: format!("oci://ghcr.io/botworkz/svc@sha256:{manifest_hex}"),
                version: String::new(),
                checksum: None,
                digest: None,
                filename: Some("svc.tar".to_string()),
                auth: None,
                platform: None,
                archive: false,
                labels: Default::default(),
            };
            let cache = TempDir::new().unwrap();
            let (transport, _) = MockTransport::with_outcomes(vec![
                MockOutcome::Body {
                    body: manifest_bytes.clone(),
                    content_length: None,
                },
                MockOutcome::Body {
                    body: config_bytes.to_vec(),
                    content_length: None,
                },
                MockOutcome::Body {
                    body: layer_gz.clone(),
                    content_length: None,
                },
            ]);
            let result = fetch_asset(FetchParams {
                name: "my-svc",
                asset: &asset,
                out_dir: None,
                cache_dir: cache.path(),
                retries: 0,
                backoff: &Backoff::default(),
                compute_checksum: true,
                no_reverify: false,
                materialize_mode: MaterializeMode::Copy,
                transport: Some(Box::new(transport)),
            })
            .unwrap();
            std::fs::read(&result.blob_path).unwrap()
        };

        let run1 = assemble_fresh();
        let run2 = assemble_fresh();
        assert_eq!(run1, run2, "assembled tar is not deterministic");
    }

    /// New `digest:` field form produces the same manifest URL as the legacy URI-suffix form.
    #[test]
    fn oci_new_form_issues_same_manifest_url_as_legacy_form() {
        let layer_tar = make_minimal_layer_tar();
        let layer_gz = make_gzip(&layer_tar);
        let layer_gz_hex = sha256_hex(&layer_gz);
        let config_bytes = b"{\"architecture\":\"amd64\"}";
        let config_hex = sha256_hex(config_bytes);

        let manifest = serde_json::json!({
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "config": {
                "mediaType": "application/vnd.oci.image.config.v1+json",
                "digest": format!("sha256:{config_hex}"),
                "size": config_bytes.len()
            },
            "layers": [{
                "mediaType": "application/vnd.oci.image.layer.v1.tar+gzip",
                "digest": format!("sha256:{layer_gz_hex}"),
                "size": layer_gz.len()
            }]
        });
        let manifest_bytes = serde_json::to_vec(&manifest).unwrap();
        let manifest_hex = sha256_hex(&manifest_bytes);

        // New form: clean URI + structured digest field.
        let asset = Asset {
            uri: "oci://ghcr.io/botworkz/svc".to_string(),
            version: String::new(),
            checksum: None,
            digest: Some(format!("sha256:{manifest_hex}")),
            filename: Some("svc.tar".to_string()),
            auth: None,
            platform: None,
            archive: false,
            labels: Default::default(),
        };

        let cache = TempDir::new().unwrap();
        let (transport, calls) = MockTransport::with_expectations(
            vec![
                MockOutcome::Body {
                    body: manifest_bytes,
                    content_length: None,
                },
                MockOutcome::Body {
                    body: config_bytes.to_vec(),
                    content_length: None,
                },
                MockOutcome::Body {
                    body: layer_gz,
                    content_length: None,
                },
            ],
            vec![
                // Expect the SAME manifest URL as the legacy form would produce.
                (
                    format!("https://ghcr.io/v2/botworkz/svc/manifests/sha256:{manifest_hex}"),
                    Some(MANIFEST_ACCEPT.to_string()),
                ),
                (
                    format!("https://ghcr.io/v2/botworkz/svc/blobs/sha256:{config_hex}"),
                    None,
                ),
                (
                    format!("https://ghcr.io/v2/botworkz/svc/blobs/sha256:{layer_gz_hex}"),
                    None,
                ),
            ],
        );

        let result = fetch_asset(FetchParams {
            name: "my-svc",
            asset: &asset,
            out_dir: None,
            cache_dir: cache.path(),
            retries: 0,
            backoff: &Backoff::default(),
            compute_checksum: true,
            no_reverify: false,
            materialize_mode: MaterializeMode::Copy,
            transport: Some(Box::new(transport)),
        })
        .unwrap();

        assert_eq!(MockTransport::call_count(&calls), 3);
        assert!(result.blob_path.exists());
    }

    /// A cache entry populated by the legacy URI-suffix form must be a hit when
    /// re-fetching using the new `digest:` field form (same effective hex → same path).
    #[test]
    fn oci_cache_compat_new_form_hits_legacy_cache() {
        let config_bytes = b"{\"architecture\":\"amd64\"}";
        let config_hex = sha256_hex(config_bytes);
        let layer_tar = make_minimal_layer_tar();
        let layer_gz = make_gzip(&layer_tar);
        let layer_gz_hex = sha256_hex(&layer_gz);

        let manifest = serde_json::json!({
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "config": {
                "digest": format!("sha256:{config_hex}"),
                "mediaType": "application/vnd.oci.image.config.v1+json"
            },
            "layers": [{
                "mediaType": "application/vnd.oci.image.layer.v1.tar+gzip",
                "digest": format!("sha256:{layer_gz_hex}"),
                "size": layer_gz.len()
            }]
        });
        let manifest_bytes = serde_json::to_vec(&manifest).unwrap();
        let manifest_hex = sha256_hex(&manifest_bytes);

        let cache = TempDir::new().unwrap();

        // ── Step 1: populate cache via legacy URI-suffix form ──
        let legacy_asset = Asset {
            uri: format!("oci://ghcr.io/botworkz/svc@sha256:{manifest_hex}"),
            version: String::new(),
            checksum: None,
            digest: None,
            filename: Some("svc.tar".to_string()),
            auth: None,
            platform: None,
            archive: false,
            labels: Default::default(),
        };
        let (transport1, _) = MockTransport::with_outcomes(vec![
            MockOutcome::Body {
                body: manifest_bytes,
                content_length: None,
            },
            MockOutcome::Body {
                body: config_bytes.to_vec(),
                content_length: None,
            },
            MockOutcome::Body {
                body: layer_gz,
                content_length: None,
            },
        ]);
        let r1 = fetch_asset(FetchParams {
            name: "my-svc",
            asset: &legacy_asset,
            out_dir: None,
            cache_dir: cache.path(),
            retries: 0,
            backoff: &Backoff::default(),
            compute_checksum: true,
            no_reverify: false,
            materialize_mode: MaterializeMode::Copy,
            transport: Some(Box::new(transport1)),
        })
        .unwrap();
        let cached_tar_hex = r1.computed_sha256.clone();

        // ── Step 2: re-fetch using the new `digest:` field form ──
        let new_asset = Asset {
            uri: "oci://ghcr.io/botworkz/svc".to_string(),
            version: String::new(),
            checksum: None,
            digest: Some(format!("sha256:{manifest_hex}")),
            filename: Some("svc.tar".to_string()),
            auth: None,
            platform: None,
            archive: false,
            labels: Default::default(),
        };
        // Transport with NO outcomes — any real request would panic.
        let (transport2, calls2) = MockTransport::with_outcomes(vec![]);
        let r2 = fetch_asset(FetchParams {
            name: "my-svc",
            asset: &new_asset,
            out_dir: None,
            cache_dir: cache.path(),
            retries: 0,
            backoff: &Backoff::default(),
            compute_checksum: true,
            no_reverify: false,
            materialize_mode: MaterializeMode::Copy,
            transport: Some(Box::new(transport2)),
        })
        .unwrap();

        // Cache hit: no network calls and same tar hex.
        assert_eq!(
            MockTransport::call_count(&calls2),
            0,
            "new-form re-fetch must hit cache without any network call"
        );
        assert_eq!(
            r2.computed_sha256, cached_tar_hex,
            "cache hit must return the same tar sha256 as the original download"
        );
    }

    /// An OCI asset with neither a `@sha256:` URI suffix nor a `digest:` field
    /// must fail fast with a clear actionable error message.
    #[test]
    fn oci_no_digest_fails_fast_with_clear_message() {
        let asset = Asset {
            uri: "oci://ghcr.io/botworkz/svc".to_string(),
            version: String::new(),
            checksum: None,
            digest: None,
            filename: Some("svc.tar".to_string()),
            auth: None,
            platform: None,
            archive: false,
            labels: Default::default(),
        };

        let cache = TempDir::new().unwrap();
        let (transport, calls) = MockTransport::with_outcomes(vec![]);

        let err = fetch_asset(FetchParams {
            name: "my-svc",
            asset: &asset,
            out_dir: None,
            cache_dir: cache.path(),
            retries: 0,
            backoff: &Backoff::default(),
            compute_checksum: true,
            no_reverify: false,
            materialize_mode: MaterializeMode::Copy,
            transport: Some(Box::new(transport)),
        })
        .unwrap_err();

        assert_eq!(
            MockTransport::call_count(&calls),
            0,
            "no network call expected for missing-digest error"
        );
        let msg = format!("{err:#}");
        assert!(
            msg.contains("OCI asset has no digest pin"),
            "error should mention missing digest pin: {msg}"
        );
        assert!(
            msg.contains("digest: sha256:<64-hex>"),
            "error should suggest digest field: {msg}"
        );
    }

    // Helper: build a minimal OCI manifest + blobs for mock transport tests.
    fn make_oci_test_fixtures() -> (Vec<u8>, Vec<u8>, Vec<u8>, String) {
        let config_bytes: Vec<u8> = b"{\"architecture\":\"amd64\"}".to_vec();
        let config_hex = sha256_hex(&config_bytes);
        let layer_tar = make_minimal_layer_tar();
        let layer_gz = make_gzip(&layer_tar);
        let layer_gz_hex = sha256_hex(&layer_gz);
        let manifest = serde_json::json!({
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "config": {
                "digest": format!("sha256:{config_hex}"),
                "mediaType": "application/vnd.oci.image.config.v1+json"
            },
            "layers": [{
                "mediaType": "application/vnd.oci.image.layer.v1.tar+gzip",
                "digest": format!("sha256:{layer_gz_hex}"),
                "size": layer_gz.len()
            }]
        });
        let manifest_bytes = serde_json::to_vec(&manifest).unwrap();
        let manifest_hex = sha256_hex(&manifest_bytes);
        (manifest_bytes, config_bytes, layer_gz, manifest_hex)
    }

    const OCI_WWW_AUTH: &str = r#"Bearer realm="https://ghcr.io/token",service="ghcr.io",scope="repository:botworkz/svc:pull""#;

    type OciExchangeRun = (Vec<(String, String)>, Vec<Option<String>>, usize, bool);

    /// Run `fetch_asset` against an OCI mock that returns 401+challenge on the first manifest
    /// request, then succeeds after token exchange. Returns (basic_auth_calls, oci_auths,
    /// anonymous_get_calls, fetch_succeeded).
    fn run_oci_basic_auth_exchange(auth_value: Option<&str>) -> OciExchangeRun {
        use std::sync::Arc;

        let (manifest_bytes, config_bytes, layer_gz, manifest_hex) = make_oci_test_fixtures();
        let token_json = serde_json::json!({"token": "test-bearer-token"}).to_string();

        let basic_auth_calls = Arc::new(Mutex::new(Vec::<(String, String)>::new()));
        let oci_auths = Arc::new(Mutex::new(Vec::<Option<String>>::new()));
        let anon_get_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));

        type OciCallOutcome = (u16, Option<String>, Vec<u8>);

        struct InnerMock {
            basic_auth_calls: Arc<Mutex<Vec<(String, String)>>>,
            oci_auths: Arc<Mutex<Vec<Option<String>>>>,
            anon_get_count: Arc<std::sync::atomic::AtomicUsize>,
            get_calls: Mutex<VecDeque<Vec<u8>>>,
            basic_auth_responses: Mutex<VecDeque<Vec<u8>>>,
            oci_calls: Mutex<VecDeque<OciCallOutcome>>,
        }

        impl Transport for InnerMock {
            fn get(
                &self,
                _uri: &str,
                auth: Option<&str>,
                _accept: Option<&str>,
            ) -> std::result::Result<DownloadResponse, FetchError> {
                if auth.is_none() {
                    self.anon_get_count
                        .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                }
                let body = self
                    .get_calls
                    .lock()
                    .unwrap()
                    .pop_front()
                    .expect("mock get calls exhausted");
                Ok(DownloadResponse {
                    body: Box::new(std::io::Cursor::new(body)),
                    content_length: None,
                })
            }

            fn get_with_basic_auth(
                &self,
                _uri: &str,
                user: &str,
                password: &str,
                _accept: Option<&str>,
            ) -> std::result::Result<DownloadResponse, FetchError> {
                self.basic_auth_calls
                    .lock()
                    .unwrap()
                    .push((user.to_string(), password.to_string()));
                let body = self
                    .basic_auth_responses
                    .lock()
                    .unwrap()
                    .pop_front()
                    .expect("mock basic_auth calls exhausted");
                Ok(DownloadResponse {
                    body: Box::new(std::io::Cursor::new(body)),
                    content_length: None,
                })
            }

            fn get_oci(
                &self,
                _uri: &str,
                auth: Option<&str>,
                _accept: Option<&str>,
            ) -> std::result::Result<OciHttpResponse, FetchError> {
                self.oci_auths
                    .lock()
                    .unwrap()
                    .push(auth.map(str::to_string));
                let (status, www_auth, body) = self
                    .oci_calls
                    .lock()
                    .unwrap()
                    .pop_front()
                    .expect("mock get_oci calls exhausted");
                Ok(OciHttpResponse {
                    status,
                    www_authenticate: www_auth,
                    body: Box::new(std::io::Cursor::new(body)),
                    content_length: None,
                })
            }
        }

        let token_bytes = token_json.into_bytes();
        let transport = InnerMock {
            basic_auth_calls: Arc::clone(&basic_auth_calls),
            oci_auths: Arc::clone(&oci_auths),
            anon_get_count: Arc::clone(&anon_get_count),
            get_calls: Mutex::new(VecDeque::from(vec![token_bytes.clone()])),
            basic_auth_responses: Mutex::new(VecDeque::from(vec![token_bytes])),
            oci_calls: Mutex::new(VecDeque::from(vec![
                (401, Some(OCI_WWW_AUTH.to_string()), vec![]),
                (200, None, manifest_bytes),
                (200, None, config_bytes),
                (200, None, layer_gz),
            ])),
        };

        let asset = Asset {
            uri: format!("oci://ghcr.io/botworkz/svc@sha256:{manifest_hex}"),
            version: String::new(),
            checksum: None,
            digest: None,
            filename: Some("svc.tar".to_string()),
            auth: auth_value.map(str::to_string),
            platform: None,
            archive: false,
            labels: Default::default(),
        };

        let cache = TempDir::new().unwrap();
        let ok = fetch_asset(FetchParams {
            name: "my-svc",
            asset: &asset,
            out_dir: None,
            cache_dir: cache.path(),
            retries: 0,
            backoff: &Backoff::default(),
            compute_checksum: true,
            no_reverify: false,
            materialize_mode: MaterializeMode::Copy,
            transport: Some(Box::new(transport)),
        })
        .is_ok();

        let calls = basic_auth_calls.lock().unwrap().clone();
        let oci = oci_auths.lock().unwrap().clone();
        let anon = anon_get_count.load(std::sync::atomic::Ordering::SeqCst);
        (calls, oci, anon, ok)
    }

    /// Bug regression: a bare token (no colon) must be sent as Basic auth with
    /// username "x-access-token", not discarded or sent as an anonymous request.
    #[test]
    fn oci_token_exchange_bare_token_uses_x_access_token_basic_auth() {
        let (calls, _oci, _anon, ok) = run_oci_basic_auth_exchange(Some("ghs_test_token_no_colon"));
        assert!(ok, "fetch should succeed");
        assert_eq!(
            calls.len(),
            1,
            "expected exactly one basic-auth call to the token endpoint"
        );
        assert_eq!(
            calls[0],
            (
                "x-access-token".to_string(),
                "ghs_test_token_no_colon".to_string()
            ),
            "bare token must use x-access-token as the username"
        );
    }

    /// A `user:pass` auth value must be split on the first colon and sent as Basic auth.
    #[test]
    fn oci_token_exchange_user_colon_pass_splits_correctly() {
        let (calls, _oci, _anon, ok) = run_oci_basic_auth_exchange(Some("alice:s3cret"));
        assert!(ok, "fetch should succeed");
        assert_eq!(calls.len(), 1);
        assert_eq!(
            calls[0],
            ("alice".to_string(), "s3cret".to_string()),
            "user:pass must split on the first colon"
        );
    }

    /// Passwords that themselves contain colons must not be truncated (split_once, not splitn).
    #[test]
    fn oci_token_exchange_password_with_colons_preserved() {
        let (calls, _oci, _anon, ok) = run_oci_basic_auth_exchange(Some("alice:s3:cret"));
        assert!(ok, "fetch should succeed");
        assert_eq!(calls.len(), 1);
        assert_eq!(
            calls[0],
            ("alice".to_string(), "s3:cret".to_string()),
            "password portion must include everything after the first colon"
        );
    }

    /// When `auth` is None the token endpoint must be hit via `get` (anonymous), not via
    /// `get_with_basic_auth`.
    #[test]
    fn oci_token_exchange_no_auth_falls_back_to_anonymous() {
        let (calls, _oci, anon, ok) = run_oci_basic_auth_exchange(None);
        assert!(ok, "fetch should succeed");
        assert!(
            calls.is_empty(),
            "no basic-auth call expected for anonymous token exchange"
        );
        assert!(
            anon > 0,
            "anonymous token exchange should use get() not get_with_basic_auth()"
        );
    }

    #[test]
    fn oci_initial_request_is_unauthenticated_and_uses_exchanged_token_on_retry() {
        let (basic_auth_calls, oci_auths, _anon, ok) =
            run_oci_basic_auth_exchange(Some("phlax:ghs_test_token"));
        assert!(ok, "fetch should succeed");
        assert_eq!(
            oci_auths.len(),
            4,
            "expected manifest/config/layer OCI calls"
        );

        assert_eq!(
            oci_auths[0], None,
            "initial manifest request must be unauthenticated"
        );
        assert_eq!(oci_auths[1], Some("test-bearer-token".to_string()));
        assert_ne!(
            oci_auths[2],
            Some("phlax:ghs_test_token".to_string()),
            "config request must never send raw operator credentials as bearer auth"
        );
        assert_ne!(
            oci_auths[3],
            Some("phlax:ghs_test_token".to_string()),
            "layer request must never send raw operator credentials as bearer auth"
        );

        assert_eq!(basic_auth_calls.len(), 1);
        assert_eq!(
            basic_auth_calls[0],
            ("phlax".to_string(), "ghs_test_token".to_string())
        );
    }
}
