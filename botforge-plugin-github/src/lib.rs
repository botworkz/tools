//! GitHub Releases `publish/github` plugin for the botforge plugin system.
//!
//! This cdylib provides the `publish/github` capability under the name `github`,
//! implementing artifact publication to GitHub Releases using the GitHub REST
//! API.
//!
//! # Capability
//!
//! - Slot: `publish/github`
//! - Name: `github`
//!
//! # ABI exports (v4)
//!
//! - `abi_version() -> u32` — returns [`botforge_plugin_host::HOST_ABI_VERSION`]
//! - `plugin_provides_count() -> u32` — returns `1`
//! - `plugin_provides_slot(index: u32) -> *const c_char` — `"publish/github\0"` at index 0
//! - `plugin_provides_name(index: u32) -> *const c_char` — `"github\0"` at index 0
//! - `plugin_publish_github(...)` — create/reuse a GitHub Release and upload assets
//! - `plugin_publish_github_free(ptr)` — free a URL or error string returned by
//!   `plugin_publish_github`
//!
//! # Trust boundary
//!
//! The plugin receives NO ambient host environment.  All credentials and
//! configuration are passed explicitly across the ABI by the host:
//! - `api_base_url` — the GitHub-compatible API endpoint.
//! - Named secrets (`secret_keys`/`secret_values`) — the host resolves any
//!   `${VAR}` templates at publish time and passes the live values here.  The
//!   plugin reads the entry named `"token"` as its bearer-auth credential.
//!
//! The plugin MUST honor `api_base_url` for every request.  It MUST NOT
//! hardcode `api.github.com` or any other host.  It MUST NOT read environment
//! variables for auth — all secrets arrive via the ABI.
//!
//! # Behavior
//!
//! `plugin_publish_github` performs the following steps:
//!
//! 1. **Create release**: `POST {api_base_url}/repos/{repo}/releases` with
//!    `{ tag_name, name, body, draft: false, prerelease: false }`.  If a
//!    release for the tag already exists the API returns 422; a subsequent
//!    `GET` retrieves the existing release.
//! 2. **Extract upload URL**: read `upload_url` from the create-release
//!    response and strip the URI-template suffix (`{?name,label}`).
//! 3. **Upload assets**: for each path in `asset_paths`, POST the file
//!    contents to `{upload_url}?name={filename}` with
//!    `Content-Type: application/octet-stream`.
//! 4. **Return release URL**: write the `html_url` from the create-release
//!    response to `*out_url` as a plugin-allocated NUL-terminated string.
//!
//! # Memory-ownership contract
//!
//! On success (return value `0`):
//! - The URL written to `*out_url` is allocated by this plugin via
//!   [`std::ffi::CString::into_raw`].
//! - The host MUST call [`plugin_publish_github_free`] on `*out_url` exactly
//!   once after use.
//! - `*out_error` is **undefined** — the host must not read or free it.
//!
//! On error (return value non-zero):
//! - An error message is written to `*out_error`, also allocated via
//!   [`std::ffi::CString::into_raw`].
//! - The host MUST call [`plugin_publish_github_free`] on `*out_error` exactly
//!   once after use.
//! - `*out_url` is **undefined** — the host must not read or free it.
//!
//! Calling `plugin_publish_github_free(NULL)` is safe (no-op).
//!
//! # unsafe policy
//!
//! This crate contains `unsafe` blocks solely for `extern "C"` FFI exports
//! and CString raw pointer operations.  The same workspace policy that
//! exempts `botforge-plugin-host` applies here: `#![forbid(unsafe_code)]`
//! is intentionally absent.

use std::ffi::{c_char, CStr, CString};
use std::path::Path;

use botforge_plugin_host::HOST_ABI_VERSION;
use serde::{Deserialize, Serialize};

// ── Static capability declarations ────────────────────────────────────────────

static SLOT_PUBLISH_GITHUB: &[u8] = b"publish/github\0";
static NAME_GITHUB: &[u8] = b"github\0";

// ── API request/response types ────────────────────────────────────────────────

#[derive(Serialize)]
struct CreateReleaseRequest<'a> {
    tag_name: &'a str,
    name: &'a str,
    body: &'a str,
    draft: bool,
    prerelease: bool,
}

#[derive(Deserialize)]
struct CreateReleaseResponse {
    html_url: String,
    upload_url: String,
}

// ── GitHub API helpers ────────────────────────────────────────────────────────

/// Create (or look up) a GitHub Release and return `(html_url, upload_url)`.
///
/// Uses `POST /repos/{repo}/releases`.  On HTTP 422 (Unprocessable Entity)
/// the release already exists; a subsequent `GET` retrieves the existing one.
///
/// Returns `Err(message)` with a human-readable description of the failure.
fn create_or_get_release(
    client: &reqwest::blocking::Client,
    api_base_url: &str,
    repo: &str,
    tag: &str,
    title: &str,
    description: &str,
) -> Result<(String, String), String> {
    let url = format!("{api_base_url}/repos/{repo}/releases");
    let body = CreateReleaseRequest {
        tag_name: tag,
        name: title,
        body: description,
        draft: false,
        prerelease: false,
    };

    let response = client
        .post(&url)
        .json(&body)
        .send()
        .map_err(|e| format!("connection error creating release: {e}"))?;

    let status = response.status();

    if status.is_success() {
        let resp: CreateReleaseResponse = response
            .json()
            .map_err(|e| format!("JSON parse error in create-release response: {e}"))?;
        return Ok((resp.html_url, resp.upload_url));
    }

    // 422 = release already exists; retrieve it via GET.
    if status.as_u16() == 422 {
        let get_url = format!("{api_base_url}/repos/{repo}/releases/tags/{tag}");
        let get_resp = client
            .get(&get_url)
            .send()
            .map_err(|e| format!("connection error fetching existing release: {e}"))?;
        let get_status = get_resp.status();
        if get_status.is_success() {
            let resp: CreateReleaseResponse = get_resp
                .json()
                .map_err(|e| format!("JSON parse error in get-release response: {e}"))?;
            return Ok((resp.html_url, resp.upload_url));
        }
        let body_text = get_resp.text().unwrap_or_default();
        return Err(format!(
            "GitHub API {get_status} fetching existing release for tag '{tag}': {body_text}"
        ));
    }

    // Any other non-success status.
    let body_text = response.text().unwrap_or_default();
    Err(format!(
        "GitHub API {status} creating release for tag '{tag}': {body_text}"
    ))
}

/// Upload a single file as a release asset.
///
/// `raw_upload_url` is the `upload_url` field from the create-release
/// response (may contain a URI-template suffix like `{?name,label}`).
/// The suffix is stripped before adding `?name={filename}`.
///
/// Returns `Err(message)` on failure.
fn upload_asset(
    client: &reqwest::blocking::Client,
    raw_upload_url: &str,
    file_path: &Path,
    filename: &str,
) -> Result<(), String> {
    // Strip the URI-template suffix (e.g. `{?name,label}`) if present.
    let base = raw_upload_url
        .split('{')
        .next()
        .unwrap_or(raw_upload_url)
        .trim_end_matches('?');
    let upload_url = format!("{base}?name={filename}");

    let data = std::fs::read(file_path)
        .map_err(|e| format!("IO error reading asset '{}': {e}", file_path.display()))?;
    let response = client
        .post(&upload_url)
        .header("Content-Type", "application/octet-stream")
        .body(data)
        .send()
        .map_err(|e| format!("connection error uploading asset '{filename}': {e}"))?;

    let status = response.status();
    if status.is_success() {
        Ok(())
    } else {
        let body_text = response.text().unwrap_or_default();
        Err(format!(
            "GitHub API {status} uploading asset '{filename}': {body_text}"
        ))
    }
}

/// Core publish logic: create release, upload assets, return release URL.
///
/// `secrets` is a slice of `(name, value)` pairs; the entry named `"token"` is
/// used as the bearer-auth credential.
///
/// Returns `Err(message)` with a human-readable error description on failure.
fn do_publish(
    repo: &str,
    tag: &str,
    title: &str,
    description: &str,
    asset_paths: &[&Path],
    api_base_url: &str,
    secrets: &[(&str, &str)],
) -> Result<String, String> {
    // Look up the bearer-auth token from the secrets map.
    let token = secrets
        .iter()
        .find_map(|(k, v)| if *k == "token" { Some(*v) } else { None })
        .ok_or_else(|| "'token' not found in secrets — add 'secrets: token: ${GITHUB_TOKEN}' to the github target".to_string())?;

    // Build an HTTP client with default bearer-auth and GitHub API headers.
    let mut headers = reqwest::header::HeaderMap::new();
    let auth_header_value = ["Bearer ", token].concat();
    let auth_value = reqwest::header::HeaderValue::from_str(&auth_header_value)
        .map_err(|e| format!("invalid token (header encoding): {e}"))?;
    headers.insert(reqwest::header::AUTHORIZATION, auth_value);
    headers.insert(
        reqwest::header::ACCEPT,
        reqwest::header::HeaderValue::from_static("application/vnd.github.v3+json"),
    );
    headers.insert(
        reqwest::header::USER_AGENT,
        reqwest::header::HeaderValue::from_static("botforge-plugin-github"),
    );

    let client = reqwest::blocking::Client::builder()
        .default_headers(headers)
        .build()
        .map_err(|e| format!("failed to build HTTP client: {e}"))?;

    let (html_url, upload_url) =
        create_or_get_release(&client, api_base_url, repo, tag, title, description)?;

    for path in asset_paths {
        let filename = path
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| format!("asset path '{}' has no valid filename", path.display()))?;
        upload_asset(&client, &upload_url, path, filename)?;
    }

    Ok(html_url)
}

// ── FFI helpers ───────────────────────────────────────────────────────────────

/// Convert a `*const c_char` to a `&str`, treating NULL as `""`.
///
/// # Safety
///
/// `ptr` must be null or a valid NUL-terminated C string for the duration of
/// this call.
unsafe fn str_from_ptr<'a>(ptr: *const c_char) -> &'a str {
    if ptr.is_null() {
        return "";
    }
    // SAFETY: Caller guarantees ptr is a valid NUL-terminated C string.
    unsafe { CStr::from_ptr(ptr) }.to_str().unwrap_or("")
}

/// Allocate a `CString` from `s` and return its raw pointer, transferring
/// ownership to the caller.  Returns `NULL` on allocation failure (interior
/// NUL byte).
fn into_raw_cstring(s: String) -> *mut c_char {
    match CString::new(s) {
        Ok(cs) => cs.into_raw(),
        // If the string contains interior NULs (shouldn't happen for error
        // messages), fall back to a generic message.
        Err(_) => CString::new("(error message contains interior NUL byte)")
            .map(CString::into_raw)
            .unwrap_or(std::ptr::null_mut()),
    }
}

// ── ABI exports ───────────────────────────────────────────────────────────────

/// Returns the ABI version this plugin was built against.
#[no_mangle]
pub extern "C" fn abi_version() -> u32 {
    HOST_ABI_VERSION
}

/// Returns the number of `(slot, name)` pairs this plugin provides.
#[no_mangle]
pub extern "C" fn plugin_provides_count() -> u32 {
    1
}

/// Returns the slot string for the capability at `index`.
///
/// # Safety
///
/// The host must only call this with `index < plugin_provides_count()`.
/// The returned pointer is `'static`; the host must NOT free it.
#[no_mangle]
pub extern "C" fn plugin_provides_slot(index: u32) -> *const c_char {
    match index {
        // SAFETY: `SLOT_PUBLISH_GITHUB` is a `'static` NUL-terminated byte
        // slice.  Casting to `*const c_char` yields a valid C string.
        0 => SLOT_PUBLISH_GITHUB.as_ptr().cast(),
        _ => std::ptr::null(),
    }
}

/// Returns the capability name for the capability at `index`.
///
/// # Safety
///
/// The host must only call this with `index < plugin_provides_count()`.
/// The returned pointer is `'static`; the host must NOT free it.
#[no_mangle]
pub extern "C" fn plugin_provides_name(index: u32) -> *const c_char {
    match index {
        // SAFETY: `NAME_GITHUB` is a `'static` NUL-terminated byte slice.
        0 => NAME_GITHUB.as_ptr().cast(),
        _ => std::ptr::null(),
    }
}

/// Publish a GitHub Release.
///
/// Creates (or reuses) a release for `tag` in `repo`, uploads all files from
/// `asset_paths`, and writes the release's HTML URL to `*out_url` as a
/// plugin-allocated NUL-terminated string on success.
///
/// On success returns `0`; `*out_url` is a valid pointer freed via
/// [`plugin_publish_github_free`].  `*out_error` is **undefined** on success.
///
/// On failure returns `-1`; `*out_error` is a plugin-allocated NUL-terminated
/// error message freed via [`plugin_publish_github_free`].  `*out_url` is
/// **undefined** on failure.
///
/// # Safety
///
/// - `repo`, `tag`, `api_base_url`, `out_url`, and `out_error` must be
///   non-null.
/// - `title` and `description` may be null (treated as `""`/tag respectively).
/// - `asset_paths` may be null when `asset_count == 0`.
/// - `secret_keys`/`secret_values` may be null when `secret_count == 0`.
/// - All string pointers must be valid NUL-terminated C strings for the
///   duration of this call.
/// - `asset_paths[0..asset_count]` must each be valid NUL-terminated C
///   strings.
/// - `secret_keys[0..secret_count]` and `secret_values[0..secret_count]`
///   must each be valid NUL-terminated C strings.
/// - `out_url` and `out_error` must be valid writeable pointers.
#[no_mangle]
pub unsafe extern "C" fn plugin_publish_github(
    repo: *const c_char,
    tag: *const c_char,
    title: *const c_char,
    description: *const c_char,
    asset_paths: *const *const c_char,
    asset_count: u32,
    api_base_url: *const c_char,
    secret_keys: *const *const c_char,
    secret_values: *const *const c_char,
    secret_count: u32,
    out_url: *mut *mut c_char,
    out_error: *mut *mut c_char,
) -> i32 {
    // SAFETY: All string pointers are validated by the caller per the ABI
    // contract above.
    let repo_str = unsafe { str_from_ptr(repo) };
    let tag_str = unsafe { str_from_ptr(tag) };
    let title_str = unsafe { str_from_ptr(title) };
    let description_str = unsafe { str_from_ptr(description) };
    let api_base_url_str = unsafe { str_from_ptr(api_base_url) };

    // Use tag as title when title is empty.
    let effective_title = if title_str.is_empty() {
        tag_str
    } else {
        title_str
    };

    // Collect asset file paths from the C array.
    let mut paths: Vec<std::path::PathBuf> = Vec::with_capacity(asset_count as usize);
    for i in 0..asset_count as usize {
        // SAFETY: `asset_paths` is valid for `asset_count` elements per the
        // ABI contract.  Each element is a valid NUL-terminated C string.
        let ptr = unsafe { *asset_paths.add(i) };
        let s = unsafe { str_from_ptr(ptr) };
        paths.push(std::path::PathBuf::from(s));
    }
    let path_refs: Vec<&Path> = paths.iter().map(|p| p.as_path()).collect();

    // Collect named secrets from the parallel C arrays.
    let mut secrets_buf: Vec<(&str, &str)> = Vec::with_capacity(secret_count as usize);
    for i in 0..secret_count as usize {
        // SAFETY: Both arrays are valid for `secret_count` elements per the
        // ABI contract.  Each element is a valid NUL-terminated C string.
        let k_ptr = unsafe { *secret_keys.add(i) };
        let v_ptr = unsafe { *secret_values.add(i) };
        // SAFETY: pointers are valid NUL-terminated C strings per ABI contract.
        // Use str_from_ptr which handles interior NUL-ish edge cases gracefully.
        let k: &str = unsafe { str_from_ptr(k_ptr) };
        let v: &str = unsafe { str_from_ptr(v_ptr) };
        secrets_buf.push((k, v));
    }

    match do_publish(
        repo_str,
        tag_str,
        effective_title,
        description_str,
        &path_refs,
        api_base_url_str,
        &secrets_buf,
    ) {
        Ok(url) => {
            // SAFETY: `out_url` is a valid writeable pointer per the ABI
            // contract.  `into_raw_cstring` transfers ownership to the caller;
            // they must free it via `plugin_publish_github_free`.
            unsafe { *out_url = into_raw_cstring(url) };
            0
        }
        Err(message) => {
            // SAFETY: `out_error` is a valid writeable pointer per the ABI
            // contract.  `into_raw_cstring` transfers ownership to the caller;
            // they must free it via `plugin_publish_github_free`.
            unsafe { *out_error = into_raw_cstring(message) };
            -1
        }
    }
}

/// Free a string previously returned (via `*out_url` or `*out_error`) by
/// [`plugin_publish_github`].
///
/// Calling with `NULL` is safe (no-op).  Must be called exactly once per
/// non-null pointer returned by the plugin.
///
/// # Safety
///
/// - `ptr` must be `NULL` or a pointer previously returned by a
///   `plugin_publish_github` call (via `*out_url` on success, or `*out_error`
///   on failure).
/// - Must not be called more than once for the same pointer.
#[no_mangle]
pub unsafe extern "C" fn plugin_publish_github_free(ptr: *mut c_char) {
    if ptr.is_null() {
        return;
    }
    // SAFETY: `ptr` is non-null and was produced by `CString::into_raw` in
    // `into_raw_cstring`.  `CString::from_raw` reclaims ownership and drops
    // it, exactly once.
    unsafe { drop(CString::from_raw(ptr)) };
}
