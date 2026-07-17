//! Hello-world fixture plugin for botforge plugin system acceptance tests.
//!
//! This cdylib provides the `core/ping` capability for use in the plugin
//! host acceptance tests.  It is NOT a production plugin; it exists solely
//! to exercise the full load → version → provides → reconcile → call path.
//!
//! # ABI exports
//!
//! - `abi_version() -> u32` — returns [`botforge_plugin_host::HOST_ABI_VERSION`]
//! - `plugin_provides_count() -> u32` — returns `1`
//! - `plugin_provides_slot(index: u32) -> *const c_char` — `"core/ping\0"`
//! - `plugin_provides_name(index: u32) -> *const c_char` — `"hello\0"`
//! - `plugin_core_ping() -> u32` — returns [`botforge_plugin_host::PING_SENTINEL`]
//!
//! # unsafe policy
//!
//! This crate contains `unsafe` blocks solely for the `extern "C"` FFI
//! exports.  The same workspace policy that exempts `botforge-plugin-host`
//! applies here: the `#![forbid(unsafe_code)]` attribute is intentionally
//! absent.

use std::ffi::c_char;

// Re-export host constants so the fixture is always in sync.
use botforge_plugin_host::{HOST_ABI_VERSION, PING_SENTINEL};

// Static NUL-terminated capability strings.
// SAFETY: These are `'static` byte slices ending in `\0`; casting to
// `*const c_char` yields a valid C string for the lifetime of the process.
static SLOT_CORE_PING: &[u8] = b"core/ping\0";
static NAME_HELLO: &[u8] = b"hello\0";

/// Returns the ABI version this plugin was built against.
///
/// The host hard-matches this value against [`HOST_ABI_VERSION`].
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
/// Only index `0` is valid; other indices are out of range.
///
/// # Safety
///
/// The host must only call this with `index < plugin_provides_count()`.
/// The returned pointer is `'static`; the host must NOT free it.
#[no_mangle]
pub extern "C" fn plugin_provides_slot(index: u32) -> *const c_char {
    match index {
        // SAFETY: `SLOT_CORE_PING` is a `'static` NUL-terminated byte slice.
        // Casting it to `*const c_char` yields a valid C string.
        0 => SLOT_CORE_PING.as_ptr().cast(),
        _ => std::ptr::null(),
    }
}

/// Returns the capability name for the capability at `index`.
///
/// Only index `0` is valid; other indices are out of range.
///
/// # Safety
///
/// The host must only call this with `index < plugin_provides_count()`.
/// The returned pointer is `'static`; the host must NOT free it.
#[no_mangle]
pub extern "C" fn plugin_provides_name(index: u32) -> *const c_char {
    match index {
        // SAFETY: `NAME_HELLO` is a `'static` NUL-terminated byte slice.
        0 => NAME_HELLO.as_ptr().cast(),
        _ => std::ptr::null(),
    }
}

/// The `core/ping` entrypoint.  Returns [`PING_SENTINEL`] (42).
///
/// The host calls this to verify end-to-end plugin loading is functional.
#[no_mangle]
pub extern "C" fn plugin_core_ping() -> u32 {
    PING_SENTINEL
}
