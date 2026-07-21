//! Wrong-ABI hello fixture for plugin-host acceptance tests.
//!
//! This cdylib mirrors `botforge-plugin-hello` except that `abi_version()`
//! intentionally returns a mismatched value so the real loader path can assert
//! `LoadError::AbiVersionMismatch`.

use std::ffi::c_char;

use botforge_plugin_host::{HOST_ABI_VERSION, PING_SENTINEL};

static SLOT_CORE_PING: &[u8] = b"core/ping\0";
static NAME_HELLO: &[u8] = b"hello\0";

#[no_mangle]
pub extern "C" fn abi_version() -> u32 {
    HOST_ABI_VERSION + 100
}

#[no_mangle]
pub extern "C" fn plugin_provides_count() -> u32 {
    1
}

#[no_mangle]
pub extern "C" fn plugin_provides_slot(index: u32) -> *const c_char {
    match index {
        // SAFETY: `SLOT_CORE_PING` is a valid NUL-terminated static C string.
        0 => SLOT_CORE_PING.as_ptr().cast(),
        _ => std::ptr::null(),
    }
}

#[no_mangle]
pub extern "C" fn plugin_provides_name(index: u32) -> *const c_char {
    match index {
        // SAFETY: `NAME_HELLO` is a valid NUL-terminated static C string.
        0 => NAME_HELLO.as_ptr().cast(),
        _ => std::ptr::null(),
    }
}

#[no_mangle]
pub extern "C" fn plugin_core_ping() -> u32 {
    PING_SENTINEL
}
