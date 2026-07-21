//! Pigz (parallel gzip) `build/compressor` plugin for the botforge plugin system.
//!
//! This cdylib provides the `build/compressor` capability under the name `pigz`,
//! implementing whole-artifact gzip compression using the `gzp` crate for
//! multi-threaded compression and `flate2` for decompression.
//!
//! # Capability
//!
//! - Slot: `build/compressor`
//! - Name: `pigz`
//!
//! # ABI exports
//!
//! - `abi_version() -> u32` — returns [`botforge_plugin_host::HOST_ABI_VERSION`]
//! - `plugin_provides_count() -> u32` — returns `1`
//! - `plugin_provides_slot(index: u32) -> *const c_char` — `"build/compressor\0"` at index 0
//! - `plugin_provides_name(index: u32) -> *const c_char` — `"pigz\0"` at index 0
//! - `plugin_build_compress(...)` — gzip-compress a byte buffer (parallel via `gzp`)
//! - `plugin_build_decompress(...)` — gunzip-decompress a byte buffer (via `flate2`)
//! - `plugin_build_compressor_free(ptr)` — free a buffer allocated by this plugin
//!
//! # Options string
//!
//! The `opts` parameter accepts a whitespace-delimited string of flags.
//! Unrecognized flags are silently ignored for forward-compatibility.
//!
//! Supported flags:
//! - `-N` where N ∈ 1–9: gzip compression level (default: 6)
//! - `-pN` where N is a positive integer: number of parallel threads (default: all CPUs)
//!
//! Examples:
//! - `"-6 -p4"` — level 6, 4 threads
//! - `"-9"` — best compression, all CPUs
//! - `""` — defaults (level 6, all CPUs)
//!
//! # Memory-ownership contract
//!
//! The buffer returned via `*out_ptr` on a successful compress/decompress call
//! uses a **length-prefixed allocation**:
//!
//! ```text
//! [ 8 bytes: usize length (native endian) | N bytes: output data ]
//! ^                                         ^
//! allocation start (aligned to usize)       *out_ptr (returned to host)
//! ```
//!
//! `*out_ptr` points to the data region (after the header).  The host reads
//! the data via `*out_ptr`/`*out_len` and then calls
//! `plugin_build_compressor_free(*out_ptr)`, which walks back by
//! `size_of::<usize>()` to recover the header, reads the stored length, and
//! deallocates the entire allocation.
//!
//! Calling `plugin_build_compressor_free(NULL)` is safe (no-op).
//!
//! # unsafe policy
//!
//! This crate contains `unsafe` blocks solely for `extern "C"` FFI exports
//! and the length-prefixed allocator helpers.  The same workspace policy that
//! exempts `botforge-plugin-host` applies here: `#![forbid(unsafe_code)]`
//! is intentionally absent.

use std::alloc::{alloc, dealloc, Layout};
use std::ffi::c_char;
use std::io::Read;

use botforge_plugin_host::HOST_ABI_VERSION;
use flate2::read::GzDecoder;
use gzp::deflate::Gzip;
use gzp::ZBuilder;

// Static NUL-terminated capability strings.
static SLOT_BUILD_COMPRESSOR: &[u8] = b"build/compressor\0";
static NAME_PIGZ: &[u8] = b"pigz\0";

// ── Options parsing ───────────────────────────────────────────────────────────

/// Parsed compression options.
struct PigzOpts {
    /// Gzip compression level: 1 (fastest) to 9 (best).  Default 6.
    level: u32,
    /// Number of parallel compression threads.  0 = use all available CPUs.
    threads: usize,
}

impl Default for PigzOpts {
    fn default() -> Self {
        Self {
            level: 6,
            threads: 0,
        }
    }
}

/// Parse a pigz options string.
///
/// Supported tokens (whitespace-separated):
/// - `-N` where N ∈ 1–9: compression level
/// - `-pN` where N is a positive integer: number of threads
///
/// Unknown tokens are silently skipped (forward-compat).
fn parse_opts(opts: &str) -> PigzOpts {
    let mut parsed = PigzOpts::default();
    for token in opts.split_whitespace() {
        if let Some(after_p) = token.strip_prefix("-p") {
            if let Ok(n) = after_p.parse::<usize>() {
                if n > 0 {
                    parsed.threads = n;
                }
            }
        } else if let Some(after_dash) = token.strip_prefix('-') {
            if let Ok(level) = after_dash.parse::<u32>() {
                if (1..=9).contains(&level) {
                    parsed.level = level;
                }
            }
        }
    }
    parsed
}

// ── Length-prefixed allocator ─────────────────────────────────────────────────

/// Header size placed before the data in every plugin-allocated output buffer.
const HEADER_SIZE: usize = std::mem::size_of::<usize>();

/// Alignment for plugin-allocated output buffers.
const HEADER_ALIGN: usize = std::mem::align_of::<usize>();

/// Allocate a length-prefixed buffer, write `data` into it, and return a
/// pointer to the data region (after the header) along with the data length.
///
/// Layout: `[ usize: data_len | data_len bytes of data ]`
///
/// The returned pointer must be freed by [`free_pigz_buf`].
///
/// Returns `None` if the total allocation size overflows or the allocator
/// returns null.
fn alloc_pigz_buf(data: &[u8]) -> Option<(*mut u8, usize)> {
    let data_len = data.len();
    let total = HEADER_SIZE.checked_add(data_len)?;
    // SAFETY: HEADER_ALIGN is a power of two and total > 0.
    let layout = unsafe { Layout::from_size_align_unchecked(total, HEADER_ALIGN) };
    // SAFETY: layout is valid.
    let header_ptr = unsafe { alloc(layout) };
    if header_ptr.is_null() {
        return None;
    }
    // SAFETY: header_ptr is valid for HEADER_SIZE bytes (at minimum).
    unsafe {
        std::ptr::write_unaligned(header_ptr.cast::<usize>(), data_len);
    }
    // SAFETY: header_ptr is valid for total bytes; HEADER_SIZE < total.
    let data_ptr = unsafe { header_ptr.add(HEADER_SIZE) };
    // SAFETY: data_ptr is valid for data_len bytes; data slice is valid.
    unsafe {
        std::ptr::copy_nonoverlapping(data.as_ptr(), data_ptr, data_len);
    }
    Some((data_ptr, data_len))
}

/// Free a buffer previously allocated by [`alloc_pigz_buf`].
///
/// # Safety
///
/// - `data_ptr` must be the data pointer returned by `alloc_pigz_buf` (i.e.,
///   `HEADER_SIZE` bytes after the allocation start).
/// - Must be called exactly once per allocation.
unsafe fn free_pigz_buf(data_ptr: *mut u8) {
    // Walk back to the header to recover data_len, then compute the full layout.
    // SAFETY: data_ptr was returned by alloc_pigz_buf, so header_ptr is valid.
    let header_ptr = unsafe { data_ptr.sub(HEADER_SIZE) };
    let data_len = unsafe { std::ptr::read_unaligned(header_ptr.cast::<usize>()) };
    let total = HEADER_SIZE + data_len;
    // SAFETY: HEADER_ALIGN is a power of two and total > 0.
    let layout = unsafe { Layout::from_size_align_unchecked(total, HEADER_ALIGN) };
    // SAFETY: header_ptr was allocated with this layout.
    unsafe { dealloc(header_ptr, layout) };
}

// ── Compression helpers ───────────────────────────────────────────────────────

/// Gzip-compress `data` using `gzp` (parallel).
fn do_compress(data: &[u8], opts: &str) -> Result<Vec<u8>, ()> {
    let o = parse_opts(opts);
    let num_threads = if o.threads == 0 {
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
    } else {
        o.threads
    };
    let level = flate2::Compression::new(o.level);
    let mut writer = ZBuilder::<Gzip, _>::new()
        .num_threads(num_threads)
        .compression_level(level)
        .from_writer(Vec::new());
    std::io::copy(&mut std::io::Cursor::new(data), &mut writer).map_err(|_| ())?;
    writer.finish().map_err(|_| ())
}

/// Gunzip-decompress `data` using `flate2`.
fn do_decompress(data: &[u8], _opts: &str) -> Result<Vec<u8>, ()> {
    let mut decoder = GzDecoder::new(data);
    let mut out = Vec::new();
    decoder.read_to_end(&mut out).map_err(|_| ())?;
    Ok(out)
}

// ── FFI helpers ───────────────────────────────────────────────────────────────

/// Convert a `*const c_char` opts pointer to a `&str`, treating NULL as `""`.
///
/// # Safety
///
/// `ptr` must be null or a valid NUL-terminated C string for the duration of
/// this call.
unsafe fn opts_from_ptr<'a>(ptr: *const c_char) -> &'a str {
    if ptr.is_null() {
        return "";
    }
    // SAFETY: Caller guarantees ptr is a valid NUL-terminated C string.
    unsafe { std::ffi::CStr::from_ptr(ptr) }
        .to_str()
        .unwrap_or("")
}

/// Common body for `plugin_build_compress` and `plugin_build_decompress`.
///
/// Compresses/decompresses `input`, allocates a plugin-owned buffer, writes
/// the pointer and length to the out-params, and returns 0 on success or -1
/// on failure.
///
/// # Safety
///
/// `out_ptr` and `out_len` must be valid writeable pointers.
unsafe fn run_fn(result: Result<Vec<u8>, ()>, out_ptr: *mut *mut u8, out_len: *mut usize) -> i32 {
    match result {
        Ok(output) => {
            match alloc_pigz_buf(&output) {
                Some((data_ptr, data_len)) => {
                    // SAFETY: out_ptr and out_len are valid stack-allocated
                    // pointers supplied by the host per the ABI contract.
                    unsafe {
                        *out_ptr = data_ptr;
                        *out_len = data_len;
                    }
                    0
                }
                None => -1,
            }
        }
        Err(()) => -1,
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
        // SAFETY: `SLOT_BUILD_COMPRESSOR` is a `'static` NUL-terminated byte
        // slice. Casting to `*const c_char` yields a valid C string.
        0 => SLOT_BUILD_COMPRESSOR.as_ptr().cast(),
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
        // SAFETY: `NAME_PIGZ` is a `'static` NUL-terminated byte slice.
        0 => NAME_PIGZ.as_ptr().cast(),
        _ => std::ptr::null(),
    }
}

/// Gzip-compress `in_len` bytes at `in_ptr` using parallel gzip (`gzp`).
///
/// On success: returns 0, writes data pointer to `*out_ptr`, byte count to
/// `*out_len`.  The host MUST call [`plugin_build_compressor_free`] on
/// `*out_ptr` after use.
///
/// On error: returns `-1`; `*out_ptr` and `*out_len` are undefined.
///
/// # Safety
///
/// - `in_ptr` must be valid for reading `in_len` bytes.
/// - `opts` must be null or a valid NUL-terminated C string.
/// - `out_ptr` and `out_len` must be valid writeable pointers.
#[no_mangle]
pub unsafe extern "C" fn plugin_build_compress(
    in_ptr: *const u8,
    in_len: usize,
    opts: *const c_char,
    out_ptr: *mut *mut u8,
    out_len: *mut usize,
) -> i32 {
    // SAFETY: in_ptr valid for in_len bytes per contract.
    let input = unsafe { std::slice::from_raw_parts(in_ptr, in_len) };
    // SAFETY: opts is null or a valid NUL-terminated C string per contract.
    let opts_str = unsafe { opts_from_ptr(opts) };
    let result = do_compress(input, opts_str);
    // SAFETY: out_ptr and out_len are valid writeable pointers per contract.
    unsafe { run_fn(result, out_ptr, out_len) }
}

/// Gunzip-decompress `in_len` bytes at `in_ptr` using `flate2`.
///
/// Same ABI and memory-ownership contract as [`plugin_build_compress`].
///
/// # Safety
///
/// Same as [`plugin_build_compress`].
#[no_mangle]
pub unsafe extern "C" fn plugin_build_decompress(
    in_ptr: *const u8,
    in_len: usize,
    opts: *const c_char,
    out_ptr: *mut *mut u8,
    out_len: *mut usize,
) -> i32 {
    // SAFETY: in_ptr valid for in_len bytes per contract.
    let input = unsafe { std::slice::from_raw_parts(in_ptr, in_len) };
    // SAFETY: opts is null or a valid NUL-terminated C string per contract.
    let opts_str = unsafe { opts_from_ptr(opts) };
    let result = do_decompress(input, opts_str);
    // SAFETY: out_ptr and out_len are valid writeable pointers per contract.
    unsafe { run_fn(result, out_ptr, out_len) }
}

/// Free a buffer previously returned by [`plugin_build_compress`] or
/// [`plugin_build_decompress`].
///
/// Calling with `NULL` is safe (no-op).  Must be called exactly once per
/// successful compress/decompress invocation.
///
/// # Safety
///
/// - `ptr` must be `NULL` or a pointer previously returned (via `*out_ptr`) by
///   a successful `plugin_build_compress` / `plugin_build_decompress` call.
/// - Must not be called more than once for the same pointer.
#[no_mangle]
pub unsafe extern "C" fn plugin_build_compressor_free(ptr: *mut u8) {
    if ptr.is_null() {
        return;
    }
    // SAFETY: ptr is non-null and was returned by alloc_pigz_buf per the
    // contract.  free_pigz_buf recovers the header and deallocates the full
    // allocation exactly once.
    unsafe { free_pigz_buf(ptr) }
}
