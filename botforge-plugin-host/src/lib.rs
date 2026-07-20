//! Plugin host for the botforge `.so` plugin system.
//!
//! # Trust boundary (load-bearing invariant)
//!
//! **The plugin knows nothing about the host environment.**  A plugin has no
//! ambient access to environment variables, process secrets, or any host state
//! unless the host explicitly hands something across the ABI.  The host is the
//! sole broker of capabilities and credentials: anything a plugin needs is passed
//! to it through the ABI by the host.  The `core/ping` handshake capability (this
//! PR's only capability) is deliberately auth-free to uphold this invariant.
//!
//! # Config-driven discovery (no autoload)
//!
//! Nothing loads unless the plugin is explicitly listed in the botforge config
//! under the `plugins:` key.  A `.so` present on disk but absent from config is
//! never loaded.
//!
//! ## Path resolution (two roots)
//!
//! 1. **Repo-relative** — a bare or `./`-prefixed path is resolved against the
//!    botforge context root (consistent with how other botforge paths work).
//! 2. **Absolute / system dir** — absolute paths are used as-is.  The canonical
//!    home for container-shipped plugins is `/usr/share/botforge/plugins/`.
//!
//! # ABI version handshake
//!
//! Every plugin must export `extern "C" fn abi_version() -> u32`.  The host calls
//! it and does a **hard exact match** against [`HOST_ABI_VERSION`].  A mismatch
//! produces a [`LoadError::AbiVersionMismatch`] naming both versions.
//!
//! Range-based or negotiated ABI versions are explicitly deferred; exact-match
//! only for v0.
//!
//! # `provides:` semantics
//!
//! The plugin self-declares which capability slots it provides via the
//! `plugin_provides_count` / `plugin_provides_slot` / `plugin_provides_name` ABI
//! exports.  The config `provides:` list (when present) acts as an **allow-list**
//! that constrains what the host actually wires; when absent the host wires all
//! capabilities the plugin declares.
//!
//! A future `strict_mode` config knob (NOT in this PR) may make `provides:`
//! mandatory for untrusted sources.
//!
//! # `(slot, name)` collision and reconciliation
//!
//! The registry is keyed by `(slot, name)` where `slot` is a namespaced
//! `<domain>/<capability>` string (e.g. `core/ping`) and `name` is the capability
//! name the plugin registers under.
//!
//! Collision rule: a `(slot, name)` that is already wired (by a built-in or a
//! previously-loaded plugin) **blocks** the new plugin from loading.  The full
//! collision check runs as a **code-free reconciliation pass** — no plugin
//! capability logic ever runs — before any capability is wired.  A collision
//! produces a [`LoadError::CapabilityCollision`] naming the slot, name, and both
//! providers.  Only when the plugin's *entire* provided set reconciles cleanly is
//! anything wired.
//!
//! Built-ins are modeled as pre-registered entries, so "a plugin cannot redefine a
//! built-in" falls out of the same `(slot, name)` check for free.
//!
//! The SAME name in a DIFFERENT slot is NOT a collision.
//!
//! # `core/ping` handshake seam
//!
//! `core/ping` is a lightweight host-level diagnostic/handshake capability, not
//! a general-purpose plugin feature. Its sole purpose is to prove the full path
//! end to end:
//!
//! > load → `abi_version()` hard-match → read `provides` → reconcile/wire →
//! > call across boundary → get the correct sentinel back
//!
//! The "must return 42" contract exists only for this self-test seam. It takes
//! no host-environment access (trust boundary upheld), and persists as the
//! loader's permanent handshake check.
//!
//! ## Plugin ABI contract for `core/ping`
//!
//! ```c
//! // Called by the host to execute a ping.  Must return PING_SENTINEL (42u32).
//! uint32_t plugin_core_ping(void);
//! ```
//!
//! # `build/compressor` capability
//!
//! `build/compressor` exposes whole-artifact / stream compression via a plugin.
//! This is **distinct** from the qcow2-internal cluster codec (`zstd`/`zlib`):
//! a `build/compressor` plugin receives and returns raw byte buffers and is
//! suitable for compressing complete artifact files (e.g. gzip-compressing an
//! output `.qcow2`), not for qcow2-cluster-level encoding.
//!
//! ## Plugin ABI contract for `build/compressor`
//!
//! ### Memory-ownership contract
//!
//! On success (return value `0`):
//! - The plugin allocates the output buffer and writes its address to `*out_ptr`.
//! - The plugin writes the byte count of the output to `*out_len`.
//! - The **host** is responsible for freeing the buffer by calling
//!   `plugin_build_compressor_free(*out_ptr)` exactly once after use.
//! - `*out_ptr` is guaranteed non-null and `*out_len > 0` on success.
//!
//! On error (return value non-zero):
//! - `*out_ptr` and `*out_len` are **undefined** — the host must not read or
//!   free them.
//!
//! Empty input (`in_len == 0`) is valid; the plugin may return an empty or
//! format-only output (e.g. an empty gzip stream).
//!
//! `opts` may be `NULL`, which the plugin must treat identically to `""`.
//!
//! ```c
//! // Compress `in_len` bytes at `in_ptr` with options from NUL-terminated
//! // `opts` (may be NULL ≡ empty string).
//! // Returns 0 on success; non-zero error code on failure.
//! // On success: *out_ptr points to the allocated output, *out_len is its byte
//! // count.  The caller MUST call plugin_build_compressor_free(*out_ptr) after
//! // use.  On failure: *out_ptr and *out_len are undefined.
//! int32_t plugin_build_compress(
//!     const uint8_t *in_ptr, size_t  in_len,
//!     const char    *opts,
//!     uint8_t      **out_ptr, size_t *out_len
//! );
//!
//! // Decompress `in_len` bytes at `in_ptr`.  Same ABI and memory-ownership
//! // contract as plugin_build_compress.
//! int32_t plugin_build_decompress(
//!     const uint8_t *in_ptr, size_t  in_len,
//!     const char    *opts,
//!     uint8_t      **out_ptr, size_t *out_len
//! );
//!
//! // Free a buffer previously returned by plugin_build_compress or
//! // plugin_build_decompress.  Must be called exactly once per successful
//! // call.  Calling with NULL is safe (no-op).
//! void plugin_build_compressor_free(uint8_t *ptr);
//! ```
//!
//! # `publish/github` capability
//!
//! `publish/github` publishes build artifacts to a GitHub Releases endpoint.
//! The host hands all inputs explicitly across the ABI boundary (trust
//! boundary upheld): asset file paths, release metadata, the target repo,
//! the **API base URL**, and a **named-secrets map** (resolved by the host).
//! The plugin never reads any ambient host environment.
//!
//! ## Plugin ABI contract for `publish/github` (ABI v4)
//!
//! ### Memory-ownership contract
//!
//! On success (return value `0`):
//! - The plugin allocates a NUL-terminated release URL string and writes its
//!   address to `*out_url`.
//! - The **host** is responsible for freeing the string by calling
//!   `plugin_publish_github_free(*out_url)` exactly once after use.
//! - `*out_url` is guaranteed non-null on success.
//! - `*out_error` is **undefined** on success — the host must not read or free
//!   it.
//!
//! On error (return value non-zero):
//! - The plugin allocates a NUL-terminated error-message string and writes its
//!   address to `*out_error`.
//! - The **host** is responsible for freeing the string by calling
//!   `plugin_publish_github_free(*out_error)` exactly once after use.
//! - `*out_url` is **undefined** on error — the host must not read or free it.
//!
//! `title` may be `NULL`, which the plugin treats as equivalent to `tag`.
//! `description` may be `NULL`, which the plugin treats as equivalent to `""`.
//! `asset_paths` may be `NULL` when `asset_count == 0` (no assets).
//! `secret_keys`/`secret_values` may be `NULL` when `secret_count == 0`.
//!
//! ```c
//! // Publish a GitHub Release.
//! // Returns 0 on success; non-zero on error.
//! // On success: *out_url points to a plugin-allocated NUL-terminated release
//! // URL string.  The caller MUST call plugin_publish_github_free(*out_url)
//! // after use.  On failure: *out_error is set to a plugin-allocated error
//! // message; the caller MUST call plugin_publish_github_free(*out_error).
//! int32_t plugin_publish_github(
//!     const char        *repo,          // "owner/repo", non-null
//!     const char        *tag,           // release tag, non-null
//!     const char        *title,         // release title (NULL ≡ tag)
//!     const char        *description,   // release body (NULL ≡ "")
//!     const char *const *asset_paths,   // array of NUL-terminated file paths
//!     uint32_t           asset_count,   // length of asset_paths (0 is valid)
//!     const char        *api_base_url,  // e.g. "https://api.github.com", non-null
//!     const char *const *secret_keys,   // array of secret name strings
//!     const char *const *secret_values, // array of secret value strings (parallel)
//!     uint32_t           secret_count,  // length of both secret arrays
//!     char             **out_url,       // set to release URL on success
//!     char             **out_error      // set to error message on failure
//! );
//!
//! // Free a string previously returned (via *out_url or *out_error) by
//! // plugin_publish_github.  Calling with NULL is safe (no-op).  Must be
//! // called exactly once per non-null pointer returned by the plugin.
//! void plugin_publish_github_free(char *ptr);
//! ```
//!
//! # ABI version history
//!
//! | Version | Change |
//! |---------|--------|
//! | 1 | Initial: `core/ping` capability only. |
//! | 2 | Added `build/compressor` capability (`plugin_build_compress`, |
//! |   | `plugin_build_decompress`, `plugin_build_compressor_free`). |
//! | 3 | Added `publish/github` capability (`plugin_publish_github`, |
//! |   | `plugin_publish_github_free`). |
//! | 4 | Extended `publish/github`: named-secrets array replaces `token`; |
//! |   | `out_error` out-parameter surfaces legible failure messages. |
//! | 5 | Added `assert/<name>` capability slot family (`plugin_assert_build_probe`, |
//! |   | `plugin_assert_evaluate`, `plugin_assert_free`). Fixed generic symbol names |
//! |   | so future assert plugins need no host changes. |
//! | 6 | Extended `assert/<name>`: added `out_error *char**` parameter to |
//! |   | `plugin_assert_build_probe` and `plugin_assert_evaluate` so config-parse |
//! |   | failures surface the real serde message rather than a bare error code. |
//!
//! # Safety policy
//!
//! This crate is the **sole sanctioned location** in the botforge workspace where
//! `unsafe` code is permitted.  All other workspace members declare
//! `#![forbid(unsafe_code)]`; this crate intentionally omits that attribute.
//!
//! Every `unsafe` block is accompanied by a `// SAFETY:` comment explaining the
//! invariant that makes it sound.

use std::collections::HashMap;
use std::ffi::CStr;
use std::path::{Path, PathBuf};

use libloading::{Library, Symbol};
use thiserror::Error;

// ── ABI version ──────────────────────────────────────────────────────────────

/// Monotone integer ABI version the host expects every plugin to report.
///
/// A plugin whose `abi_version()` export returns any value other than this
/// constant is rejected with [`LoadError::AbiVersionMismatch`].  Increment
/// this constant (and rebuild all plugins) whenever the plugin ABI changes in
/// a backwards-incompatible way.
///
/// Version 3 adds the `publish/github` capability slot.
/// Version 4 extends `publish/github`: named-secrets array replaces the single
/// `token` parameter; an `out_error` out-parameter surfaces legible error
/// messages on failure.
/// Version 5 adds the `assert/<name>` capability slot family.
/// Version 6 extends `assert/<name>`: `out_error *char**` on both
/// `plugin_assert_build_probe` and `plugin_assert_evaluate` so config-parse
/// failures surface the real serde message.
pub const HOST_ABI_VERSION: u32 = 6;

/// Sentinel value returned by a correct `core/ping` implementation.
///
/// The host asserts that `plugin_core_ping()` returns exactly this value.
pub const PING_SENTINEL: u32 = 42;

// ── Errors ───────────────────────────────────────────────────────────────────

/// Structured errors for plugin loading and capability wiring.
#[derive(Debug, Error)]
pub enum LoadError {
    /// The `.so` file was not found at the given path.
    #[error("plugin file not found: {path}")]
    FileNotFound { path: PathBuf },

    /// `libloading`/`dlopen` failed to open the library.
    #[error("failed to open plugin {path}: {source}")]
    DlopenFailed {
        path: PathBuf,
        #[source]
        source: libloading::Error,
    },

    /// A required symbol could not be resolved in the library.
    #[error("plugin {plugin} is missing required symbol '{symbol}'")]
    MissingSymbol {
        plugin: String,
        symbol: &'static str,
    },

    /// The plugin's `abi_version()` return value does not match the host.
    #[error(
        "ABI version mismatch for plugin '{plugin}': \
         plugin reports {plugin_version}, host requires {host_version}"
    )]
    AbiVersionMismatch {
        plugin: String,
        plugin_version: u32,
        host_version: u32,
    },

    /// The plugin's capability-enumeration ABI returned an invalid C string.
    #[error("plugin '{plugin}' returned invalid UTF-8 in capability {index}: {source}")]
    CapabilityEnumerationFailed {
        plugin: String,
        index: u32,
        #[source]
        source: std::str::Utf8Error,
    },

    /// A `(slot, name)` pair exported by the plugin is already registered.
    ///
    /// Nothing from the offending plugin is wired.
    #[error(
        "capability collision: slot '{slot}' name '{name}' \
         is already registered by '{existing_provider}', \
         cannot also register it for '{new_provider}'"
    )]
    CapabilityCollision {
        slot: String,
        name: String,
        existing_provider: String,
        new_provider: String,
    },

    /// A provided capability slot is unknown to the host.
    #[error(
        "plugin '{plugin}' declares unknown capability slot '{slot}'; \
         config provides: filter may be wrong"
    )]
    UnknownCapabilitySlot { plugin: String, slot: String },

    /// A `build/compressor` compress or decompress operation returned a
    /// non-zero error code from the plugin.
    #[error("plugin '{plugin}' build/compressor operation failed with error code {code}")]
    CompressorError { plugin: String, code: i32 },

    /// The plugin returned an output length that exceeds `usize::MAX` on this
    /// platform (should not happen in practice; guards against pathological
    /// plugins on 32-bit hosts).
    #[error(
        "plugin '{plugin}' build/compressor returned output length {out_len} \
         which overflows usize"
    )]
    CompressorOutputOverflow { plugin: String, out_len: usize },

    /// A `publish/github` publish operation returned a non-zero error code
    /// from the plugin.  `message` is the human-readable error string that the
    /// plugin wrote to `out_error` (empty when the plugin returned no message).
    #[error("plugin '{plugin}' publish/github operation failed (code {code}): {message}")]
    PublisherError {
        plugin: String,
        code: i32,
        message: String,
    },

    /// The plugin returned an invalid UTF-8 release URL.
    #[error("plugin '{plugin}' publish/github returned a non-UTF-8 release URL")]
    PublisherInvalidUrl { plugin: String },

    /// A `assert/<name>` build_probe or evaluate operation returned a non-zero error.
    /// `message` is the human-readable error string that the plugin wrote to
    /// `out_error` (empty when the plugin returned no message).
    #[error("plugin '{plugin}' assert/{assert_name} operation failed (code {code}): {message}")]
    AssertProviderError {
        plugin: String,
        assert_name: String,
        code: i32,
        message: String,
    },

    /// The plugin returned invalid UTF-8 from an assert operation.
    #[error("plugin '{plugin}' assert/{assert_name} returned non-UTF-8 output")]
    AssertProviderInvalidUtf8 { plugin: String, assert_name: String },
}

// ── Capability handles ────────────────────────────────────────────────────────

/// A callable handle to a wired `core/ping` capability.
///
/// # Safety
///
/// The function pointer is valid for as long as the originating
/// [`LoadedPlugin`] (and hence its [`Library`]) stays alive.  Callers must not
/// use a `PingHandle` after the plugin has been dropped.
pub struct PingHandle {
    /// Raw function pointer resolved from the plugin.
    ///
    /// SAFETY argument: see [`LoadedPlugin::load`] — the pointer is obtained
    /// via `libloading::Symbol::into_raw` after a successful `dlsym`, and the
    /// symbol remains valid for the lifetime of the owning `Library`.
    func: unsafe extern "C" fn() -> u32,
}

impl PingHandle {
    /// Call the plugin's `core/ping` entrypoint and return the result.
    pub fn call(&self) -> u32 {
        // SAFETY: The function pointer was obtained from a successfully
        // dlopen-ed library and the symbol was verified to exist.  The
        // calling convention is `extern "C"` on both sides.  The library
        // must stay live; see struct-level safety note.
        unsafe { (self.func)() }
    }
}

/// A callable handle to a wired `build/compressor` capability.
///
/// Exposes safe `compress` and `decompress` methods that call across the C
/// ABI boundary, manage the plugin-allocated output buffer, and return an
/// owned `Vec<u8>`.
///
/// # Memory ownership
///
/// Internally, `compress` / `decompress` call the plugin's FFI functions and
/// immediately copy the output into a Rust `Vec<u8>`, then call
/// `plugin_build_compressor_free` to release the plugin-allocated buffer.
/// The caller receives a purely Rust-owned `Vec<u8>` with no further
/// lifetime coupling to the plugin.
///
/// # Safety
///
/// All function pointers are valid for as long as the originating
/// [`LoadedPlugin`] (and hence its [`Library`]) stays alive.  Callers must
/// not use a `CompressorHandle` after the plugin has been dropped.
pub struct CompressorHandle {
    /// Plugin name (for error messages).
    plugin_name: String,
    /// `plugin_build_compress` function pointer.
    ///
    /// SAFETY: see [`LoadedPlugin::load`].
    compress_fn: unsafe extern "C" fn(
        *const u8,
        usize,
        *const std::os::raw::c_char,
        *mut *mut u8,
        *mut usize,
    ) -> i32,
    /// `plugin_build_decompress` function pointer.
    ///
    /// SAFETY: see [`LoadedPlugin::load`].
    decompress_fn: unsafe extern "C" fn(
        *const u8,
        usize,
        *const std::os::raw::c_char,
        *mut *mut u8,
        *mut usize,
    ) -> i32,
    /// `plugin_build_compressor_free` function pointer.
    ///
    /// SAFETY: see [`LoadedPlugin::load`].
    free_fn: unsafe extern "C" fn(*mut u8),
}

impl CompressorHandle {
    /// Compress `data` using the plugin, passing `opts` as the options string.
    ///
    /// `opts` may be empty; `None` is treated identically to `""`.
    ///
    /// Returns a newly-allocated `Vec<u8>` containing the compressed output.
    ///
    /// # Errors
    ///
    /// Returns [`LoadError::CompressorError`] if the plugin reports a non-zero
    /// error code, or [`LoadError::CompressorOutputOverflow`] on pathological
    /// output-length values.
    pub fn compress(&self, data: &[u8], opts: &str) -> Result<Vec<u8>, LoadError> {
        self.call_compressor_fn(self.compress_fn, data, opts)
    }

    /// Decompress `data` using the plugin, passing `opts` as the options
    /// string.
    ///
    /// Returns a newly-allocated `Vec<u8>` containing the decompressed output.
    ///
    /// # Errors
    ///
    /// Same as [`compress`](Self::compress).
    pub fn decompress(&self, data: &[u8], opts: &str) -> Result<Vec<u8>, LoadError> {
        self.call_compressor_fn(self.decompress_fn, data, opts)
    }

    /// Shared body for `compress` and `decompress`.
    fn call_compressor_fn(
        &self,
        func: unsafe extern "C" fn(
            *const u8,
            usize,
            *const std::os::raw::c_char,
            *mut *mut u8,
            *mut usize,
        ) -> i32,
        data: &[u8],
        opts: &str,
    ) -> Result<Vec<u8>, LoadError> {
        // Build a NUL-terminated opts string for the C boundary.
        let opts_cstring = std::ffi::CString::new(opts).unwrap_or_default();

        let mut out_ptr: *mut u8 = std::ptr::null_mut();
        let mut out_len: usize = 0;

        // SAFETY: We call the plugin's compress/decompress function pointer,
        // which was obtained from a live `Library` via `dlsym`.  The in-pointer
        // and length come from a valid Rust slice.  `opts_cstring` is
        // NUL-terminated.  `out_ptr` and `out_len` are valid stack addresses.
        // The plugin contract guarantees that on success the returned pointer
        // is a valid allocation of `out_len` bytes.  On failure the out-
        // parameters are undefined and we do not read them.
        let rc = unsafe {
            func(
                data.as_ptr(),
                data.len(),
                opts_cstring.as_ptr(),
                &mut out_ptr,
                &mut out_len,
            )
        };

        if rc != 0 {
            return Err(LoadError::CompressorError {
                plugin: self.plugin_name.clone(),
                code: rc,
            });
        }

        // Guard: the plugin returned a suspiciously large length on a 32-bit
        // host.  On 64-bit (the common case) `usize` is 8 bytes and this
        // check is a no-op tautology.
        #[allow(clippy::useless_conversion)]
        let len = usize::try_from(out_len).map_err(|_| LoadError::CompressorOutputOverflow {
            plugin: self.plugin_name.clone(),
            out_len,
        })?;

        // SAFETY: The plugin returned rc == 0 so `out_ptr` points to a
        // valid buffer of `len` bytes.  We copy the bytes into a Rust
        // Vec before freeing.
        let output = unsafe { std::slice::from_raw_parts(out_ptr, len) }.to_vec();

        // SAFETY: `out_ptr` was allocated by the plugin and must be freed
        // via `plugin_build_compressor_free`.  We call it exactly once here.
        unsafe { (self.free_fn)(out_ptr) };

        Ok(output)
    }
}

/// Request data for a `publish/github` call, passed by the host to
/// [`PublisherHandle::publish`].
pub struct PublishRequest<'a> {
    /// GitHub repository in `owner/repo` form.
    pub repo: &'a str,
    /// Release tag name (e.g. `v1.0.0`).
    pub tag: &'a str,
    /// Release title.  `None` → plugin uses the tag name.
    pub title: Option<&'a str>,
    /// Release description / body text.  `None` → plugin uses empty string.
    pub description: Option<&'a str>,
    /// Paths to local asset files to upload to the release.
    pub asset_paths: &'a [&'a std::path::Path],
    /// Base URL of the GitHub-compatible REST API
    /// (e.g. `"https://api.github.com"` or `"http://mock:8080"`).
    pub api_base_url: &'a str,
    /// Named resolved secrets to pass across the ABI boundary.
    ///
    /// Each entry is `(name, resolved_value)`.  The host resolves any
    /// `${VAR}` templates at publish time and hands the live values here;
    /// they are never stored beyond the duration of this call.
    ///
    /// The `publish/github` plugin reads the entry named `"token"` as its
    /// bearer-auth credential.  Additional entries support future multi-secret
    /// plugins without ABI changes.
    pub secrets: &'a [(&'a str, &'a str)],
}

/// Outcome of a successful `publish/github` call.
#[derive(Debug)]
pub struct PublishOutcome {
    /// The web URL of the published release
    /// (e.g. `"https://github.com/owner/repo/releases/tag/v1.0.0"`).
    pub release_url: String,
}

/// A callable handle to a wired `publish/github` capability.
///
/// Exposes a safe [`publish`](PublisherHandle::publish) method that calls
/// across the C ABI boundary, manages the plugin-allocated URL string, and
/// returns an owned [`PublishOutcome`].
///
/// # Memory ownership
///
/// Internally, `publish` calls the plugin's `plugin_publish_github` FFI
/// function.  On success the plugin allocates a NUL-terminated release URL
/// string and writes it to `*out_url`; on failure it allocates a NUL-terminated
/// error message and writes it to `*out_error`.  `publish` copies the
/// relevant string into a Rust value, then calls `plugin_publish_github_free`
/// to release the plugin-owned allocation.  The caller receives a purely
/// Rust-owned value with no lifetime coupling to the plugin.
///
/// # Safety
///
/// All function pointers are valid for as long as the originating
/// [`LoadedPlugin`] (and hence its [`Library`]) stays alive.  Callers must
/// not use a `PublisherHandle` after the plugin has been dropped.
pub struct PublisherHandle {
    /// Plugin name (for error messages).
    plugin_name: String,
    /// `plugin_publish_github` function pointer (ABI v4).
    ///
    /// SAFETY: see [`LoadedPlugin::load`].
    #[allow(clippy::type_complexity)]
    publish_fn: unsafe extern "C" fn(
        *const std::os::raw::c_char,        // repo
        *const std::os::raw::c_char,        // tag
        *const std::os::raw::c_char,        // title (may be null)
        *const std::os::raw::c_char,        // description (may be null)
        *const *const std::os::raw::c_char, // asset_paths
        u32,                                // asset_count
        *const std::os::raw::c_char,        // api_base_url
        *const *const std::os::raw::c_char, // secret_keys
        *const *const std::os::raw::c_char, // secret_values
        u32,                                // secret_count
        *mut *mut std::os::raw::c_char,     // out_url  (success)
        *mut *mut std::os::raw::c_char,     // out_error (failure)
    ) -> i32,
    /// `plugin_publish_github_free` function pointer.
    ///
    /// Frees both URL strings (success path) and error strings (failure path).
    ///
    /// SAFETY: see [`LoadedPlugin::load`].
    free_fn: unsafe extern "C" fn(*mut std::os::raw::c_char),
}

impl PublisherHandle {
    /// Publish a GitHub Release using the plugin.
    ///
    /// Converts `request` to C-compatible types, calls the plugin's
    /// `plugin_publish_github` entrypoint, copies the returned release URL
    /// into a [`PublishOutcome`] (or the error message into a
    /// [`LoadError::PublisherError`]), and frees the plugin-allocated string.
    ///
    /// # Errors
    ///
    /// Returns [`LoadError::PublisherError`] if the plugin reports a non-zero
    /// error code (with the plugin-supplied error message), or
    /// [`LoadError::PublisherInvalidUrl`] if the returned URL is not valid
    /// UTF-8.
    pub fn publish(&self, request: &PublishRequest<'_>) -> Result<PublishOutcome, LoadError> {
        use std::ffi::CString;

        // Build NUL-terminated C strings for all input parameters.
        let repo_c = CString::new(request.repo).unwrap_or_default();
        let tag_c = CString::new(request.tag).unwrap_or_default();
        let title_c: Option<CString> = request.title.map(|t| CString::new(t).unwrap_or_default());
        let description_c: Option<CString> = request
            .description
            .map(|d| CString::new(d).unwrap_or_default());
        let api_base_url_c = CString::new(request.api_base_url).unwrap_or_default();

        // Build array of C string pointers for asset paths.
        // Keep `CString` values alive in a Vec so the pointers remain valid.
        let asset_cstrings: Vec<CString> = request
            .asset_paths
            .iter()
            .map(|p| CString::new(p.to_string_lossy().as_ref()).unwrap_or_default())
            .collect();
        let asset_ptrs: Vec<*const std::os::raw::c_char> =
            asset_cstrings.iter().map(|cs| cs.as_ptr()).collect();

        // Build parallel arrays for named secrets.
        // Keep `CString` values alive until after the FFI call.
        let secret_key_cstrings: Vec<CString> = request
            .secrets
            .iter()
            .map(|(k, _)| CString::new(*k).unwrap_or_default())
            .collect();
        let secret_val_cstrings: Vec<CString> = request
            .secrets
            .iter()
            .map(|(_, v)| CString::new(*v).unwrap_or_default())
            .collect();
        let secret_key_ptrs: Vec<*const std::os::raw::c_char> =
            secret_key_cstrings.iter().map(|cs| cs.as_ptr()).collect();
        let secret_val_ptrs: Vec<*const std::os::raw::c_char> =
            secret_val_cstrings.iter().map(|cs| cs.as_ptr()).collect();

        let mut out_url: *mut std::os::raw::c_char = std::ptr::null_mut();
        let mut out_error: *mut std::os::raw::c_char = std::ptr::null_mut();

        // SAFETY: We call the plugin's publish function pointer, which was
        // obtained from a live `Library` via `dlsym`.  All input C strings are
        // NUL-terminated and valid for the duration of this call (kept alive
        // by the local CString variables above).  `out_url` and `out_error`
        // are valid stack addresses.
        // On success (rc == 0): the plugin writes a non-null, plugin-allocated
        // NUL-terminated string to `*out_url`; `*out_error` is undefined.
        // On failure (rc != 0): the plugin writes a NUL-terminated error
        // message to `*out_error`; `*out_url` is undefined.
        let rc = unsafe {
            (self.publish_fn)(
                repo_c.as_ptr(),
                tag_c.as_ptr(),
                title_c.as_ref().map_or(std::ptr::null(), |c| c.as_ptr()),
                description_c
                    .as_ref()
                    .map_or(std::ptr::null(), |c| c.as_ptr()),
                if asset_ptrs.is_empty() {
                    std::ptr::null()
                } else {
                    asset_ptrs.as_ptr()
                },
                asset_ptrs.len() as u32,
                api_base_url_c.as_ptr(),
                if secret_key_ptrs.is_empty() {
                    std::ptr::null()
                } else {
                    secret_key_ptrs.as_ptr()
                },
                if secret_val_ptrs.is_empty() {
                    std::ptr::null()
                } else {
                    secret_val_ptrs.as_ptr()
                },
                secret_key_ptrs.len() as u32,
                &mut out_url,
                &mut out_error,
            )
        };

        if rc != 0 {
            // Read the error message the plugin wrote to *out_error, then free
            // the plugin-allocated string.  If the plugin wrote NULL (should
            // not happen but defensive), use an empty message.
            let message = if out_error.is_null() {
                String::new()
            } else {
                // SAFETY: rc != 0 so the plugin guarantees `out_error` is a
                // valid, non-null, NUL-terminated C string.
                let s = unsafe { CStr::from_ptr(out_error) }
                    .to_str()
                    .unwrap_or("")
                    .to_owned();
                // SAFETY: `out_error` was allocated by the plugin and must be
                // freed via `plugin_publish_github_free` exactly once.
                unsafe { (self.free_fn)(out_error) };
                s
            };
            return Err(LoadError::PublisherError {
                plugin: self.plugin_name.clone(),
                code: rc,
                message,
            });
        }

        // SAFETY: rc == 0, so the plugin guarantees `out_url` is a valid,
        // non-null, NUL-terminated C string.  We copy it into a Rust String
        // before freeing.
        let release_url = unsafe { CStr::from_ptr(out_url) }
            .to_str()
            .map(str::to_owned)
            .map_err(|_| LoadError::PublisherInvalidUrl {
                plugin: self.plugin_name.clone(),
            })?;

        // SAFETY: `out_url` was allocated by the plugin via `plugin_publish_github`.
        // We call `plugin_publish_github_free` exactly once here.
        unsafe { (self.free_fn)(out_url) };

        Ok(PublishOutcome { release_url })
    }
}

/// Read a plugin-allocated NUL-terminated C string from `ptr` (which may be
/// null) into a Rust `String`, then free it via `free_fn`.
///
/// Returns an empty `String` when `ptr` is null.
///
/// # Safety
///
/// This function is safe to call in error paths: when `ptr` is non-null it must
/// be a valid, plugin-allocated NUL-terminated UTF-8 string; `free_fn` must be
/// the same `plugin_assert_free` that allocated it.
fn read_and_free_plugin_cstring(
    ptr: *mut std::os::raw::c_char,
    free_fn: unsafe extern "C" fn(*mut std::os::raw::c_char),
) -> String {
    if ptr.is_null() {
        return String::new();
    }
    // SAFETY: ptr is non-null and the plugin guarantees it is a valid
    // NUL-terminated UTF-8 string on the error path.
    let s = unsafe { std::ffi::CStr::from_ptr(ptr) }
        .to_str()
        .unwrap_or("")
        .to_owned();
    // SAFETY: ptr was allocated by the plugin and must be freed via free_fn.
    unsafe { free_fn(ptr) };
    s
}

/// A callable handle to a wired `assert/<name>` capability.
///
/// Exposes safe [`build_probe`](AssertProviderHandle::build_probe) and
/// [`evaluate`](AssertProviderHandle::evaluate) methods that call across the
/// C ABI boundary and return owned Rust strings.
///
/// # Memory ownership
///
/// Both methods call the plugin's FFI function, copy the returned C string
/// into a Rust `String`, then call `free_fn` exactly once.
///
/// # Safety
///
/// All function pointers are valid for as long as the originating
/// [`LoadedPlugin`] stays alive.
pub struct AssertProviderHandle {
    plugin_name: String,
    assert_name: String,
    build_probe_fn: unsafe extern "C" fn(
        *const std::os::raw::c_char,
        *mut *mut std::os::raw::c_char,
        *mut *mut std::os::raw::c_char,
    ) -> i32,
    evaluate_fn: unsafe extern "C" fn(
        *const std::os::raw::c_char,
        *const std::os::raw::c_char,
        *mut *mut std::os::raw::c_char,
        *mut *mut std::os::raw::c_char,
    ) -> i32,
    free_fn: unsafe extern "C" fn(*mut std::os::raw::c_char),
}

impl AssertProviderHandle {
    /// Build the guest probe shell script from `config_json`.
    ///
    /// Returns the script string on success, or a [`LoadError`] if the plugin
    /// returns a non-zero code or non-UTF-8 output.
    pub fn build_probe(&self, config_json: &str) -> Result<String, LoadError> {
        use std::ffi::{CStr, CString};
        let config_c = CString::new(config_json).unwrap_or_default();
        let mut out_ptr: *mut std::os::raw::c_char = std::ptr::null_mut();
        let mut out_error: *mut std::os::raw::c_char = std::ptr::null_mut();
        // SAFETY: build_probe_fn was obtained from a live Library via dlsym.
        // config_c is NUL-terminated. out_ptr and out_error are valid stack addresses.
        // On success the plugin writes a non-null NUL-terminated string to *out_ptr.
        // On failure the plugin writes a NUL-terminated error message to *out_error.
        let rc = unsafe { (self.build_probe_fn)(config_c.as_ptr(), &mut out_ptr, &mut out_error) };
        if rc != 0 {
            let message = read_and_free_plugin_cstring(out_error, self.free_fn);
            return Err(LoadError::AssertProviderError {
                plugin: self.plugin_name.clone(),
                assert_name: self.assert_name.clone(),
                code: rc,
                message,
            });
        }
        // SAFETY: rc == 0 so plugin guarantees out_ptr is valid NUL-terminated.
        let s = unsafe { CStr::from_ptr(out_ptr) }
            .to_str()
            .map(str::to_owned)
            .map_err(|_| LoadError::AssertProviderInvalidUtf8 {
                plugin: self.plugin_name.clone(),
                assert_name: self.assert_name.clone(),
            })?;
        // SAFETY: out_ptr was allocated by plugin, freed exactly once.
        unsafe { (self.free_fn)(out_ptr) };
        Ok(s)
    }

    /// Evaluate captured probe stdout against `config_json`.
    ///
    /// Returns the results JSON string on success.
    pub fn evaluate(&self, config_json: &str, probe_stdout: &str) -> Result<String, LoadError> {
        use std::ffi::{CStr, CString};
        let config_c = CString::new(config_json).unwrap_or_default();
        let stdout_c = CString::new(probe_stdout).unwrap_or_default();
        let mut out_ptr: *mut std::os::raw::c_char = std::ptr::null_mut();
        let mut out_error: *mut std::os::raw::c_char = std::ptr::null_mut();
        // SAFETY: evaluate_fn was obtained from a live Library via dlsym.
        // config_c and stdout_c are NUL-terminated. out_ptr and out_error are valid
        // stack addresses. On failure the plugin writes a message to *out_error.
        let rc = unsafe {
            (self.evaluate_fn)(
                config_c.as_ptr(),
                stdout_c.as_ptr(),
                &mut out_ptr,
                &mut out_error,
            )
        };
        if rc != 0 {
            let message = read_and_free_plugin_cstring(out_error, self.free_fn);
            return Err(LoadError::AssertProviderError {
                plugin: self.plugin_name.clone(),
                assert_name: self.assert_name.clone(),
                code: rc,
                message,
            });
        }
        // SAFETY: rc == 0 so plugin guarantees out_ptr is valid NUL-terminated.
        let s = unsafe { CStr::from_ptr(out_ptr) }
            .to_str()
            .map(str::to_owned)
            .map_err(|_| LoadError::AssertProviderInvalidUtf8 {
                plugin: self.plugin_name.clone(),
                assert_name: self.assert_name.clone(),
            })?;
        // SAFETY: out_ptr was allocated by plugin, freed exactly once.
        unsafe { (self.free_fn)(out_ptr) };
        Ok(s)
    }
}

// ── Loaded plugin ─────────────────────────────────────────────────────────────

/// A plugin that has been successfully opened and version-checked.
///
/// `LoadedPlugin` holds the open [`Library`] handle; dropping it closes
/// the `.so`.  All capability handles derived from this library are invalid
/// after the drop, so keep `LoadedPlugin` alive as long as capability handles
/// are in use.
pub struct LoadedPlugin {
    /// Human-readable name from the config entry.
    pub name: String,
    /// ABI version read from the plugin (already matched against
    /// [`HOST_ABI_VERSION`]).
    pub abi_version: u32,
    /// Capability `(slot, name)` pairs self-declared by the plugin
    /// (possibly filtered by the config `provides:` allow-list).
    pub provides: Vec<(String, String)>,
    /// `core/ping` handle, present if the plugin wired that capability.
    pub ping: Option<PingHandle>,
    /// `build/compressor` handle, present if the plugin wired that capability.
    pub compressor: Option<CompressorHandle>,
    /// `publish/github` handle, present if the plugin wired that capability.
    pub publisher: Option<PublisherHandle>,
    /// `assert/<name>` handles, one per registered assert capability.
    pub assert_providers: Vec<(String, AssertProviderHandle)>,
    /// The open library handle.  Must stay alive as long as any capability
    /// handles derived from it are in use.
    _lib: Library,
}

impl std::fmt::Debug for LoadedPlugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoadedPlugin")
            .field("name", &self.name)
            .field("abi_version", &self.abi_version)
            .field("provides", &self.provides)
            .field("ping", &self.ping.is_some())
            .field("compressor", &self.compressor.is_some())
            .field("publisher", &self.publisher.is_some())
            .field(
                "assert_providers",
                &self
                    .assert_providers
                    .iter()
                    .map(|(n, _)| n.as_str())
                    .collect::<Vec<_>>(),
            )
            .finish_non_exhaustive()
    }
}

impl LoadedPlugin {
    /// Open a plugin `.so`, verify the ABI version, enumerate capabilities,
    /// and apply the optional config `provides:` filter.
    ///
    /// # Errors
    ///
    /// Returns a [`LoadError`] if:
    /// - the file does not exist,
    /// - `dlopen` fails,
    /// - `abi_version` is missing or mismatches,
    /// - the capability-enumeration symbols are missing or return bad data,
    /// - a capability the plugin declares is unknown to the host.
    pub fn load(
        plugin_name: &str,
        path: &Path,
        config_provides: Option<&[String]>,
    ) -> Result<Self, LoadError> {
        if !path.exists() {
            return Err(LoadError::FileNotFound {
                path: path.to_owned(),
            });
        }

        // SAFETY: `dlopen` is inherently unsafe.  We accept the risk here
        // as the sole designated location for FFI loading in this workspace.
        // The path has already been verified to exist above.
        let lib = unsafe {
            Library::new(path).map_err(|e| LoadError::DlopenFailed {
                path: path.to_owned(),
                source: e,
            })?
        };

        // ── ABI version handshake ─────────────────────────────────────────
        let plugin_ver: u32 = {
            // SAFETY: We are looking up the symbol `abi_version` which is
            // expected to be an `extern "C" fn() -> u32`.  If the plugin
            // exports a symbol with this name but a different signature, the
            // call below is UB.  We document this as part of the plugin ABI
            // contract: plugins MUST export exactly this signature.
            let sym: Symbol<unsafe extern "C" fn() -> u32> = unsafe {
                lib.get(b"abi_version\0")
                    .map_err(|_| LoadError::MissingSymbol {
                        plugin: plugin_name.to_owned(),
                        symbol: "abi_version",
                    })?
            };
            // SAFETY: same as above; calling the function pointer.
            unsafe { sym() }
        };

        if plugin_ver != HOST_ABI_VERSION {
            return Err(LoadError::AbiVersionMismatch {
                plugin: plugin_name.to_owned(),
                plugin_version: plugin_ver,
                host_version: HOST_ABI_VERSION,
            });
        }

        // ── Capability enumeration ────────────────────────────────────────
        //
        // The plugin exports three functions for capability enumeration:
        //
        //   extern "C" fn plugin_provides_count() -> u32
        //     Returns the total number of (slot, name) pairs this plugin
        //     provides.  Indices are 0-based; must be stable across calls.
        //
        //   extern "C" fn plugin_provides_slot(index: u32) -> *const c_char
        //     Returns a NUL-terminated UTF-8 slot string for the given index.
        //     The memory is static and owned by the plugin; the host must NOT
        //     free it.
        //
        //   extern "C" fn plugin_provides_name(index: u32) -> *const c_char
        //     Returns a NUL-terminated UTF-8 capability name string for the
        //     given index.  Same ownership rules.
        //
        // Memory ownership: all returned pointers are `'static` string
        // literals inside the plugin `.so` binary.  They are valid for the
        // lifetime of the open `Library` and must never be freed by the host.

        // SAFETY: Looking up the symbol with the exact documented signature.
        let count_sym: Symbol<unsafe extern "C" fn() -> u32> = unsafe {
            lib.get(b"plugin_provides_count\0")
                .map_err(|_| LoadError::MissingSymbol {
                    plugin: plugin_name.to_owned(),
                    symbol: "plugin_provides_count",
                })?
        };
        // SAFETY: Calling the function pointer obtained via dlsym.
        let count = unsafe { count_sym() };

        // SAFETY: Looking up the symbol with the exact documented signature.
        let slot_sym: Symbol<unsafe extern "C" fn(u32) -> *const std::os::raw::c_char> = unsafe {
            lib.get(b"plugin_provides_slot\0")
                .map_err(|_| LoadError::MissingSymbol {
                    plugin: plugin_name.to_owned(),
                    symbol: "plugin_provides_slot",
                })?
        };

        // SAFETY: Looking up the symbol with the exact documented signature.
        let name_sym: Symbol<unsafe extern "C" fn(u32) -> *const std::os::raw::c_char> = unsafe {
            lib.get(b"plugin_provides_name\0")
                .map_err(|_| LoadError::MissingSymbol {
                    plugin: plugin_name.to_owned(),
                    symbol: "plugin_provides_name",
                })?
        };

        let mut all_provides: Vec<(String, String)> = Vec::with_capacity(count as usize);
        for i in 0..count {
            // SAFETY: Calling a function pointer obtained via dlsym with an
            // in-range index.  The returned pointer is a `'static` C string
            // inside the plugin binary — valid as long as the library is open.
            let slot_ptr = unsafe { slot_sym(i) };
            let name_ptr = unsafe { name_sym(i) };

            // SAFETY: The plugin contract requires both pointers to be valid
            // NUL-terminated UTF-8 strings, non-null and `'static`.  If a
            // malformed plugin violates this, behaviour is undefined; we accept
            // this as inherent to the FFI trust boundary.
            let slot = unsafe { CStr::from_ptr(slot_ptr) }
                .to_str()
                .map_err(|e| LoadError::CapabilityEnumerationFailed {
                    plugin: plugin_name.to_owned(),
                    index: i,
                    source: e,
                })?
                .to_owned();
            let cap_name = unsafe { CStr::from_ptr(name_ptr) }
                .to_str()
                .map_err(|e| LoadError::CapabilityEnumerationFailed {
                    plugin: plugin_name.to_owned(),
                    index: i,
                    source: e,
                })?
                .to_owned();

            all_provides.push((slot, cap_name));
        }

        // ── Apply config provides: filter ─────────────────────────────────
        let provides: Vec<(String, String)> = if let Some(allow_list) = config_provides {
            all_provides
                .into_iter()
                .filter(|(slot, _)| allow_list.iter().any(|a| a == slot))
                .collect()
        } else {
            // absent ⇒ implicit-all (v0 behaviour)
            all_provides
        };

        // ── Resolve capability handles ────────────────────────────────────
        let mut ping: Option<PingHandle> = None;
        let mut compressor: Option<CompressorHandle> = None;
        let mut publisher: Option<PublisherHandle> = None;
        let mut assert_providers: Vec<(String, AssertProviderHandle)> = Vec::new();

        for (slot, _cap_name) in &provides {
            match slot.as_str() {
                "core/ping" => {
                    // SAFETY: The symbol `plugin_core_ping` must be an
                    // `extern "C" fn() -> u32` per the documented `core/ping`
                    // ABI contract.  We use `Symbol::into_raw` to detach the
                    // lifetime from `lib` and store the raw pointer; the
                    // pointer remains valid for the lifetime of `lib` (stored
                    // in `_lib` on the returned `LoadedPlugin`).
                    let sym: Symbol<unsafe extern "C" fn() -> u32> = unsafe {
                        lib.get(b"plugin_core_ping\0")
                            .map_err(|_| LoadError::MissingSymbol {
                                plugin: plugin_name.to_owned(),
                                symbol: "plugin_core_ping",
                            })?
                    };
                    // SAFETY: `into_raw` detaches the symbol from the `Symbol`
                    // wrapper's lifetime borrow on `lib`.  We guarantee that
                    // `_lib` outlives any use of `func` because both are owned
                    // by the same `LoadedPlugin`.
                    let func = unsafe { sym.into_raw() };
                    ping = Some(PingHandle { func: *func });
                }
                "build/compressor" => {
                    // Resolve all three compressor symbols.
                    //
                    // SAFETY: Each symbol is looked up with the exact signature
                    // documented in the module-level `build/compressor` ABI
                    // contract.  `Symbol::into_raw` detaches the lifetime from
                    // `lib`; the raw pointers remain valid as long as `_lib`
                    // (stored on the returned `LoadedPlugin`) is alive.

                    type CompressFn = unsafe extern "C" fn(
                        *const u8,
                        usize,
                        *const std::os::raw::c_char,
                        *mut *mut u8,
                        *mut usize,
                    ) -> i32;

                    let compress_sym: Symbol<CompressFn> = unsafe {
                        lib.get(b"plugin_build_compress\0").map_err(|_| {
                            LoadError::MissingSymbol {
                                plugin: plugin_name.to_owned(),
                                symbol: "plugin_build_compress",
                            }
                        })?
                    };
                    let compress_fn = *unsafe { compress_sym.into_raw() };

                    let decompress_sym: Symbol<CompressFn> = unsafe {
                        lib.get(b"plugin_build_decompress\0").map_err(|_| {
                            LoadError::MissingSymbol {
                                plugin: plugin_name.to_owned(),
                                symbol: "plugin_build_decompress",
                            }
                        })?
                    };
                    let decompress_fn = *unsafe { decompress_sym.into_raw() };

                    type FreeFn = unsafe extern "C" fn(*mut u8);
                    let free_sym: Symbol<FreeFn> = unsafe {
                        lib.get(b"plugin_build_compressor_free\0").map_err(|_| {
                            LoadError::MissingSymbol {
                                plugin: plugin_name.to_owned(),
                                symbol: "plugin_build_compressor_free",
                            }
                        })?
                    };
                    let free_fn = *unsafe { free_sym.into_raw() };

                    compressor = Some(CompressorHandle {
                        plugin_name: plugin_name.to_owned(),
                        compress_fn,
                        decompress_fn,
                        free_fn,
                    });
                }
                "publish/github" => {
                    // Resolve both publisher symbols.
                    //
                    // SAFETY: Each symbol is looked up with the exact signature
                    // documented in the module-level `publish/github` ABI v4
                    // contract.  `Symbol::into_raw` detaches the lifetime from
                    // `lib`; the raw pointers remain valid as long as `_lib`
                    // (stored on the returned `LoadedPlugin`) is alive.

                    #[allow(clippy::type_complexity)]
                    type PublishFn = unsafe extern "C" fn(
                        *const std::os::raw::c_char,        // repo
                        *const std::os::raw::c_char,        // tag
                        *const std::os::raw::c_char,        // title
                        *const std::os::raw::c_char,        // description
                        *const *const std::os::raw::c_char, // asset_paths
                        u32,                                // asset_count
                        *const std::os::raw::c_char,        // api_base_url
                        *const *const std::os::raw::c_char, // secret_keys
                        *const *const std::os::raw::c_char, // secret_values
                        u32,                                // secret_count
                        *mut *mut std::os::raw::c_char,     // out_url
                        *mut *mut std::os::raw::c_char,     // out_error
                    ) -> i32;

                    let publish_sym: Symbol<PublishFn> = unsafe {
                        lib.get(b"plugin_publish_github\0").map_err(|_| {
                            LoadError::MissingSymbol {
                                plugin: plugin_name.to_owned(),
                                symbol: "plugin_publish_github",
                            }
                        })?
                    };
                    let publish_fn = *unsafe { publish_sym.into_raw() };

                    type PublishFreeFn = unsafe extern "C" fn(*mut std::os::raw::c_char);
                    let free_sym: Symbol<PublishFreeFn> = unsafe {
                        lib.get(b"plugin_publish_github_free\0").map_err(|_| {
                            LoadError::MissingSymbol {
                                plugin: plugin_name.to_owned(),
                                symbol: "plugin_publish_github_free",
                            }
                        })?
                    };
                    let free_fn = *unsafe { free_sym.into_raw() };

                    publisher = Some(PublisherHandle {
                        plugin_name: plugin_name.to_owned(),
                        publish_fn,
                        free_fn,
                    });
                }
                slot if slot.starts_with("assert/") => {
                    let assert_name = slot.strip_prefix("assert/").unwrap_or("").to_owned();

                    type BuildProbeFn = unsafe extern "C" fn(
                        *const std::os::raw::c_char,
                        *mut *mut std::os::raw::c_char,
                        *mut *mut std::os::raw::c_char,
                    ) -> i32;
                    type EvaluateFn = unsafe extern "C" fn(
                        *const std::os::raw::c_char,
                        *const std::os::raw::c_char,
                        *mut *mut std::os::raw::c_char,
                        *mut *mut std::os::raw::c_char,
                    ) -> i32;
                    type AssertFreeFn = unsafe extern "C" fn(*mut std::os::raw::c_char);

                    let build_probe_sym: Symbol<BuildProbeFn> = unsafe {
                        lib.get(b"plugin_assert_build_probe\0").map_err(|_| {
                            LoadError::MissingSymbol {
                                plugin: plugin_name.to_owned(),
                                symbol: "plugin_assert_build_probe",
                            }
                        })?
                    };
                    let build_probe_fn = *unsafe { build_probe_sym.into_raw() };

                    let evaluate_sym: Symbol<EvaluateFn> = unsafe {
                        lib.get(b"plugin_assert_evaluate\0").map_err(|_| {
                            LoadError::MissingSymbol {
                                plugin: plugin_name.to_owned(),
                                symbol: "plugin_assert_evaluate",
                            }
                        })?
                    };
                    let evaluate_fn = *unsafe { evaluate_sym.into_raw() };

                    let free_sym: Symbol<AssertFreeFn> = unsafe {
                        lib.get(b"plugin_assert_free\0")
                            .map_err(|_| LoadError::MissingSymbol {
                                plugin: plugin_name.to_owned(),
                                symbol: "plugin_assert_free",
                            })?
                    };
                    let free_fn = *unsafe { free_sym.into_raw() };

                    assert_providers.push((
                        assert_name.clone(),
                        AssertProviderHandle {
                            plugin_name: plugin_name.to_owned(),
                            assert_name,
                            build_probe_fn,
                            evaluate_fn,
                            free_fn,
                        },
                    ));
                }
                other => {
                    return Err(LoadError::UnknownCapabilitySlot {
                        plugin: plugin_name.to_owned(),
                        slot: other.to_owned(),
                    });
                }
            }
        }

        Ok(LoadedPlugin {
            name: plugin_name.to_owned(),
            abi_version: plugin_ver,
            provides,
            ping,
            compressor,
            publisher,
            assert_providers,
            _lib: lib,
        })
    }
}

// ── Plugin registry ───────────────────────────────────────────────────────────

/// The capability registry: a map from `(slot, name)` to the provider name.
///
/// Built-ins are pre-seeded before any plugins are loaded; subsequent loads
/// go through the same `(slot, name)` collision check.
#[derive(Default)]
pub struct PluginRegistry {
    /// `(slot, name)` → provider name.
    entries: HashMap<(String, String), String>,
    /// Successfully loaded plugins (in load order).
    pub plugins: Vec<LoadedPlugin>,
}

impl PluginRegistry {
    /// Create an empty registry with no pre-seeded built-ins.
    pub fn new() -> Self {
        Self::default()
    }

    /// Pre-seed the registry with a built-in `(slot, name)` entry.
    ///
    /// Built-ins cannot be overwritten by plugins; any plugin that tries
    /// produces a [`LoadError::CapabilityCollision`].
    pub fn seed_builtin(&mut self, slot: impl Into<String>, name: impl Into<String>) {
        let s = slot.into();
        let n = name.into();
        self.entries.insert((s, n), "<built-in>".to_owned());
    }

    /// Load a plugin from `path`, run the code-free reconciliation pass,
    /// and wire its capabilities into the registry.
    ///
    /// # Reconciliation pass
    ///
    /// 1. Open the `.so` and read its declared `(slot, name)` set.
    /// 2. Compute the intended registrations (all or config-filtered).
    /// 3. Check **every** `(slot, name)` against the merged registry in a
    ///    **single pass, no plugin logic invoked**.
    /// 4. On any collision: return [`LoadError::CapabilityCollision`]; the
    ///    plugin's `LoadedPlugin` is dropped (`.so` is closed), nothing is
    ///    wired.
    /// 5. Only on full clean pass: register all `(slot, name)` pairs and push
    ///    the plugin into [`Self::plugins`].
    pub fn load_plugin(
        &mut self,
        plugin_name: &str,
        path: &Path,
        config_provides: Option<&[String]>,
    ) -> Result<(), LoadError> {
        let loaded = LoadedPlugin::load(plugin_name, path, config_provides)?;

        // ── Code-free reconciliation pass ─────────────────────────────────
        // Check ALL intended registrations before wiring any of them.
        for (slot, name) in &loaded.provides {
            let key = (slot.clone(), name.clone());
            if let Some(existing) = self.entries.get(&key) {
                return Err(LoadError::CapabilityCollision {
                    slot: slot.clone(),
                    name: name.clone(),
                    existing_provider: existing.clone(),
                    new_provider: plugin_name.to_owned(),
                });
            }
        }

        // Full clean pass — wire everything.
        for (slot, name) in &loaded.provides {
            self.entries
                .insert((slot.clone(), name.clone()), plugin_name.to_owned());
        }
        self.plugins.push(loaded);
        Ok(())
    }

    /// Look up the `core/ping` handle for a named capability registration.
    ///
    /// Returns `None` if the plugin is not loaded or did not wire `core/ping`
    /// under `name`.
    pub fn get_ping(&self, name: &str) -> Option<&PingHandle> {
        // Find the plugin that registered (core/ping, name).
        let provider = self
            .entries
            .get(&("core/ping".to_owned(), name.to_owned()))?;
        self.plugins
            .iter()
            .find(|p| &p.name == provider)
            .and_then(|p| p.ping.as_ref())
    }

    /// Look up the `build/compressor` handle for a named capability
    /// registration.
    ///
    /// Returns `None` if no plugin is loaded that registered
    /// `build/compressor` under `name`.
    pub fn get_compressor(&self, name: &str) -> Option<&CompressorHandle> {
        let provider = self
            .entries
            .get(&("build/compressor".to_owned(), name.to_owned()))?;
        self.plugins
            .iter()
            .find(|p| &p.name == provider)
            .and_then(|p| p.compressor.as_ref())
    }

    /// Look up the `publish/github` handle for a named capability registration.
    ///
    /// Returns `None` if no plugin is loaded that registered
    /// `publish/github` under `name`.
    pub fn get_publisher(&self, name: &str) -> Option<&PublisherHandle> {
        let provider = self
            .entries
            .get(&("publish/github".to_owned(), name.to_owned()))?;
        self.plugins
            .iter()
            .find(|p| &p.name == provider)
            .and_then(|p| p.publisher.as_ref())
    }

    /// Look up the `assert/<name>` handle for a named assert capability.
    ///
    /// Returns `None` if no plugin is loaded that registered
    /// `assert/<name>` under `name`.
    pub fn get_assert(&self, name: &str) -> Option<&AssertProviderHandle> {
        let slot = format!("assert/{name}");
        let provider = self.entries.get(&(slot, name.to_owned()))?;
        self.plugins
            .iter()
            .find(|p| &p.name == provider)
            .and_then(|p| {
                p.assert_providers
                    .iter()
                    .find(|(n, _)| n == name)
                    .map(|(_, h)| h)
            })
    }

    /// Returns the names of all `assert/<name>` capabilities currently
    /// registered in this registry (from all loaded plugins).
    pub fn assert_names(&self) -> Vec<&str> {
        self.entries
            .iter()
            .filter_map(|((slot, name), _)| {
                if slot.starts_with("assert/") {
                    Some(name.as_str())
                } else {
                    None
                }
            })
            .collect()
    }

    /// Returns `true` if `(slot, name)` is already registered (by a built-in or
    /// a previously-loaded plugin).
    pub fn is_registered(&self, slot: &str, name: &str) -> bool {
        self.entries
            .contains_key(&(slot.to_owned(), name.to_owned()))
    }

    /// Returns the provider name for `(slot, name)`, if registered.
    pub fn provider_of(&self, slot: &str, name: &str) -> Option<&str> {
        self.entries
            .get(&(slot.to_owned(), name.to_owned()))
            .map(String::as_str)
    }

    /// Returns the names of all `build/compressor` capabilities currently
    /// registered in this registry (from all loaded plugins).
    pub fn compressor_names(&self) -> Vec<&str> {
        self.entries
            .iter()
            .filter_map(|((slot, name), _)| {
                if slot == "build/compressor" {
                    Some(name.as_str())
                } else {
                    None
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Unit tests that do NOT require a compiled .so ────────────────────

    #[test]
    fn builtin_blocks_plugin_same_slot_name() {
        let mut reg = PluginRegistry::new();
        reg.seed_builtin("core/ping", "hello");
        assert!(reg.is_registered("core/ping", "hello"));
        assert_eq!(reg.provider_of("core/ping", "hello"), Some("<built-in>"));
    }

    #[test]
    fn different_slot_same_name_is_not_collision() {
        let mut reg = PluginRegistry::new();
        reg.seed_builtin("core/ping", "hello");
        // A DIFFERENT slot with the SAME name is not a collision.
        assert!(!reg.is_registered("build/compressor", "hello"));
    }

    #[test]
    fn registry_empty_by_default() {
        let reg = PluginRegistry::new();
        assert!(!reg.is_registered("core/ping", "anything"));
        assert!(reg.plugins.is_empty());
    }

    #[test]
    fn file_not_found_error() {
        let err = LoadedPlugin::load("missing", Path::new("/nonexistent/libmissing.so"), None)
            .unwrap_err();
        assert!(
            matches!(err, LoadError::FileNotFound { .. }),
            "expected FileNotFound, got: {err}"
        );
    }

    #[test]
    fn load_error_messages_are_informative() {
        let err = LoadError::AbiVersionMismatch {
            plugin: "myplugin".to_owned(),
            plugin_version: 99,
            host_version: 1,
        };
        let msg = err.to_string();
        assert!(msg.contains("myplugin"), "should name the plugin: {msg}");
        assert!(msg.contains("99"), "should show plugin version: {msg}");
        assert!(msg.contains('1'), "should show host version: {msg}");

        let err2 = LoadError::CapabilityCollision {
            slot: "core/ping".to_owned(),
            name: "hello".to_owned(),
            existing_provider: "plugin-a".to_owned(),
            new_provider: "plugin-b".to_owned(),
        };
        let msg2 = err2.to_string();
        assert!(msg2.contains("core/ping"), "should name slot: {msg2}");
        assert!(msg2.contains("hello"), "should name cap name: {msg2}");
        assert!(msg2.contains("plugin-a"), "should name existing: {msg2}");
        assert!(msg2.contains("plugin-b"), "should name new: {msg2}");
    }

    #[test]
    fn compressor_error_variants_are_structured() {
        let err = LoadError::CompressorError {
            plugin: "pigz".to_owned(),
            code: -1,
        };
        let msg = err.to_string();
        assert!(msg.contains("pigz"), "should name plugin: {msg}");
        assert!(msg.contains("-1"), "should show error code: {msg}");

        let err2 = LoadError::CompressorOutputOverflow {
            plugin: "pigz".to_owned(),
            out_len: usize::MAX,
        };
        let msg2 = err2.to_string();
        assert!(msg2.contains("pigz"), "should name plugin: {msg2}");
        assert!(msg2.contains("overflow"), "should mention overflow: {msg2}");
    }

    #[test]
    fn get_compressor_returns_none_when_not_loaded() {
        let reg = PluginRegistry::new();
        assert!(reg.get_compressor("pigz").is_none());
    }

    #[test]
    fn compressor_names_empty_when_no_compressors() {
        let reg = PluginRegistry::new();
        assert!(reg.compressor_names().is_empty());
    }

    #[test]
    fn compressor_names_includes_seeded_builtin() {
        let mut reg = PluginRegistry::new();
        reg.seed_builtin("build/compressor", "pigz");
        let names = reg.compressor_names();
        assert!(names.contains(&"pigz"), "pigz should appear: {names:?}");
    }

    #[test]
    fn publisher_error_variants_are_structured() {
        let err = LoadError::PublisherError {
            plugin: "github".to_owned(),
            code: -1,
            message: "GitHub API 401: Bad credentials".to_owned(),
        };
        let msg = err.to_string();
        assert!(msg.contains("github"), "should name plugin: {msg}");
        assert!(msg.contains("-1"), "should show error code: {msg}");
        assert!(msg.contains("401"), "should include error message: {msg}");

        let err2 = LoadError::PublisherInvalidUrl {
            plugin: "github".to_owned(),
        };
        let msg2 = err2.to_string();
        assert!(msg2.contains("github"), "should name plugin: {msg2}");
    }

    #[test]
    fn get_publisher_returns_none_when_not_loaded() {
        let reg = PluginRegistry::new();
        assert!(reg.get_publisher("github").is_none());
    }

    #[test]
    fn publish_github_slot_is_not_compressor_slot() {
        let mut reg = PluginRegistry::new();
        reg.seed_builtin("publish/github", "github");
        // build/compressor is a different slot — not a collision
        assert!(!reg.is_registered("build/compressor", "github"));
        assert!(reg.is_registered("publish/github", "github"));
    }
}
