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

    // Retry loop for cache misses or invalidated cache blobs.
    let mut attempt = 0u32;
    let downloaded = loop {
        attempt += 1;
        let transport_ref: &dyn Transport = match &transport {
            Some(t) => t.as_ref(),
            None => &ReqwestTransport::default(),
        };

        match download_via_scheme(transport_ref, cache_dir, &uri, auth.as_deref()) {
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
    uri: &str,
    auth: Option<&str>,
) -> std::result::Result<DownloadedFile, FetchError> {
    let parsed = reqwest::Url::parse(uri).map_err(|error| {
        FetchError::permanent(anyhow::Error::new(error).context(format!("invalid uri: {uri}")))
    })?;
    match parsed.scheme() {
        "http" | "https" => try_download(transport, cache_dir, uri, auth, None),
        "github-release" => try_download_github_release(transport, cache_dir, uri, auth),
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
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
            filename: Some("tool.bin".to_string()),
            auth: None,
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
            filename: Some("tool.bin".to_string()),
            auth: None,
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
            filename: None,
            auth: Some("${SHASSET_TEST_TOKEN}".to_string()),
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
            filename: None,
            auth: Some("${SHASSET_TEST_TOKEN}".to_string()),
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
            filename: None,
            auth: None,
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

        let err =
            download_via_scheme(&transport, cache.path(), "ftp://example.com/x", None).unwrap_err();

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
}
