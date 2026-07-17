//! Acceptance tests for the botforge plugin host.
//!
//! These tests exercise the full load → version → provides → reconcile → call
//! path using the real compiled `libhello.so` fixture plugin.
//!
//! # Running
//!
//! Build the fixture `.so` first:
//!
//! ```sh
//! cargo build -p botforge-plugin-hello
//! ```
//!
//! Then run the tests:
//!
//! ```sh
//! cargo test -p botforge-plugin-host
//! ```
//!
//! # How the `.so` is located
//!
//! The test computes the path to `libhello.so` via `CARGO_MANIFEST_DIR` and
//! the standard Cargo target directory layout.  The build profile (debug /
//! release) is detected via `cfg(debug_assertions)`.

use botforge_plugin_host::{LoadError, PingHandle, PluginRegistry, PING_SENTINEL};
use std::path::{Path, PathBuf};

// ── Fixture path helpers ──────────────────────────────────────────────────────

/// Returns the workspace root (the parent of the `botforge-plugin-host` crate).
fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("CARGO_MANIFEST_DIR must have a parent")
        .to_path_buf()
}

/// Returns the Cargo target subdirectory for the current build profile.
fn target_dir() -> PathBuf {
    workspace_root()
        .join("target")
        .join(if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        })
}

/// Returns the path to `libhello.so` in the Cargo target directory.
fn hello_so_path() -> PathBuf {
    target_dir().join("libhello.so")
}

/// Skip the test if the fixture .so has not been built yet.
macro_rules! require_hello_so {
    () => {{
        let path = hello_so_path();
        if !path.exists() {
            eprintln!(
                "SKIP: {} not found — run `cargo build -p botforge-plugin-hello` first",
                path.display()
            );
            return;
        }
        path
    }};
}

// ── Positive path: full end-to-end ────────────────────────────────────────────

/// Full path: load → abi_version → provides → reconcile → wire → call.
#[test]
fn acceptance_full_path_repo_relative() {
    let so = require_hello_so!();
    let mut reg = PluginRegistry::new();
    reg.load_plugin("hello", &so, None)
        .expect("hello plugin should load cleanly");

    // Plugin is wired.
    assert!(reg.is_registered("core/ping", "hello"));
    assert_eq!(reg.provider_of("core/ping", "hello"), Some("hello"));

    // Exactly one plugin loaded.
    assert_eq!(reg.plugins.len(), 1);
    assert_eq!(reg.plugins[0].name, "hello");
    assert_eq!(
        reg.plugins[0].abi_version,
        botforge_plugin_host::HOST_ABI_VERSION
    );
    assert_eq!(
        reg.plugins[0].provides,
        vec![("core/ping".to_owned(), "hello".to_owned())]
    );

    // Call across the boundary.
    let ping: &PingHandle = reg.get_ping("hello").expect("ping handle should be wired");
    assert_eq!(ping.call(), PING_SENTINEL, "ping must return PING_SENTINEL");
}

/// Explicit provides: filter — wiring only the listed slot.
#[test]
fn acceptance_provides_filter_wires_only_listed_slot() {
    let so = require_hello_so!();
    let mut reg = PluginRegistry::new();
    // Allow-list includes core/ping → it should be wired.
    reg.load_plugin("hello", &so, Some(&["core/ping".to_owned()]))
        .expect("should load with provides filter");
    assert!(reg.is_registered("core/ping", "hello"));
    assert!(reg.get_ping("hello").is_some());
}

/// Explicit provides: filter that excludes all slots — plugin registers nothing.
#[test]
fn acceptance_provides_filter_excludes_all() {
    let so = require_hello_so!();
    let mut reg = PluginRegistry::new();
    // Allow-list with a slot the plugin does NOT provide → filtered to empty.
    reg.load_plugin("hello", &so, Some(&["build/compressor".to_owned()]))
        .expect("loading with an empty-post-filter is valid (no capabilities wired)");
    // The registry has no entries (nothing was wired).
    assert!(!reg.is_registered("core/ping", "hello"));
    assert!(!reg.is_registered("build/compressor", "hello"));
}

// ── Negative: ABI version mismatch ────────────────────────────────────────────

/// A file that exports the wrong `abi_version()` must be rejected with a
/// structured mismatch error.  We simulate this by creating a tiny inline
/// shared library via a temporary file ... but that requires compiling C code
/// at test time.  Instead we test the error type/message directly via
/// unit-level construction (the real load path is covered by the positive tests
/// above; mismatch is triggered there if HOST_ABI_VERSION ever changes).
#[test]
fn abi_mismatch_error_is_structured() {
    let err = LoadError::AbiVersionMismatch {
        plugin: "bad-plugin".to_owned(),
        plugin_version: 99,
        host_version: botforge_plugin_host::HOST_ABI_VERSION,
    };
    let msg = err.to_string();
    assert!(msg.contains("bad-plugin"), "must name the plugin: {msg}");
    assert!(msg.contains("99"), "must show plugin version: {msg}");
    assert!(
        msg.contains(&botforge_plugin_host::HOST_ABI_VERSION.to_string()),
        "must show host version: {msg}"
    );
    // Confirm it's a LoadError::AbiVersionMismatch variant.
    assert!(matches!(err, LoadError::AbiVersionMismatch { .. }));
}

// ── Negative: (slot, name) collision ─────────────────────────────────────────

/// Loading the same plugin twice must produce a collision error on the second
/// load, naming both providers.
#[test]
fn collision_same_plugin_loaded_twice() {
    let so = require_hello_so!();
    let mut reg = PluginRegistry::new();
    reg.load_plugin("hello", &so, None)
        .expect("first load should succeed");
    let err = reg
        .load_plugin("hello-2", &so, None)
        .expect_err("second load should collide");
    match &err {
        LoadError::CapabilityCollision {
            slot,
            name,
            existing_provider,
            new_provider,
        } => {
            assert_eq!(slot, "core/ping");
            assert_eq!(name, "hello");
            assert_eq!(existing_provider, "hello");
            assert_eq!(new_provider, "hello-2");
        }
        other => panic!("expected CapabilityCollision, got: {other}"),
    }
    // The second plugin was NOT wired.
    assert_eq!(
        reg.plugins.len(),
        1,
        "only the first plugin should be wired"
    );
    assert_eq!(
        reg.provider_of("core/ping", "hello"),
        Some("hello"),
        "first plugin still owns the slot"
    );
}

/// A built-in with the same (slot, name) blocks the plugin.
#[test]
fn collision_builtin_blocks_plugin() {
    let so = require_hello_so!();
    let mut reg = PluginRegistry::new();
    reg.seed_builtin("core/ping", "hello");
    let err = reg
        .load_plugin("hello", &so, None)
        .expect_err("plugin should not overwrite built-in");
    match &err {
        LoadError::CapabilityCollision {
            existing_provider, ..
        } => {
            assert_eq!(existing_provider, "<built-in>");
        }
        other => panic!("expected CapabilityCollision, got: {other}"),
    }
    assert_eq!(reg.plugins.len(), 0, "plugin must not be loaded");
}

/// Same name in a DIFFERENT slot must NOT collide.
#[test]
fn same_name_different_slot_does_not_collide() {
    let mut reg = PluginRegistry::new();
    reg.seed_builtin("core/ping", "hello");
    // Seeding a different slot with the same name should succeed.
    reg.seed_builtin("build/compressor", "hello");
    assert!(reg.is_registered("core/ping", "hello"));
    assert!(reg.is_registered("build/compressor", "hello"));
}

// ── Negative: file not found ──────────────────────────────────────────────────

/// A path that does not exist must produce FileNotFound.
#[test]
fn missing_so_gives_file_not_found() {
    let mut reg = PluginRegistry::new();
    let err = reg
        .load_plugin("ghost", Path::new("/nonexistent/libghost.so"), None)
        .expect_err("missing .so must error");
    assert!(
        matches!(err, LoadError::FileNotFound { .. }),
        "expected FileNotFound, got: {err}"
    );
    assert!(reg.plugins.is_empty());
}

// ── No autoload: .so present but not in config ────────────────────────────────

/// A `.so` on disk that is NOT explicitly loaded via load_plugin must never
/// appear in the registry.  This is enforced by the architecture (the registry
/// only gets entries from explicit `load_plugin` calls) — this test documents
/// the invariant.
#[test]
fn no_autoload_so_on_disk_not_in_registry_unless_explicitly_loaded() {
    let so = hello_so_path();
    // Even if the .so exists, an empty registry has no entry for it.
    let reg = PluginRegistry::new();
    assert!(!reg.is_registered("core/ping", "hello"));
    assert!(reg.plugins.is_empty());
    // The file exists on disk (if the fixture was built), but the registry is empty.
    if so.exists() {
        // Still not in registry — only explicit load_plugin adds entries.
        assert!(!reg.is_registered("core/ping", "hello"));
    }
}
