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
//! # ABI version history
//!
//! | Version | Change |
//! |---------|--------|
//! | 1 | Initial: `core/ping` capability only. |
//! | 2 | Added `build/compressor` capability (`plugin_build_compress`, |
//! |   | `plugin_build_decompress`, `plugin_build_compressor_free`). |
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
/// Version 2 adds the `build/compressor` capability slot.
pub const HOST_ABI_VERSION: u32 = 2;

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
}
