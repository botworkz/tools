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
//! cargo build -p botforge-plugin-hello-badabi
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
//! The test resolves the Cargo target directory by honoring
//! `CARGO_TARGET_DIR` (falling back to `<workspace>/target`) and then probing
//! the standard build-profile subdirectories (`debug` and `release`).  It
//! returns the first candidate that actually contains the requested fixture
//! `.so`.  `cargo tarpaulin` instruments binaries under `target/debug` (the
//! same location as plain `cargo test`), so no special tarpaulin subdir is
//! needed.

use botforge_plugin_host::{
    LoadError, PingHandle, PluginRegistry, HOST_ABI_VERSION, PING_SENTINEL,
};
use std::path::{Path, PathBuf};

// ── Fixture path helpers ──────────────────────────────────────────────────────

/// Returns the workspace root (the parent of the `botforge-plugin-host` crate).
fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("CARGO_MANIFEST_DIR must have a parent")
        .to_path_buf()
}

/// Returns the base Cargo target directory, honoring `CARGO_TARGET_DIR`.
fn target_base() -> PathBuf {
    match std::env::var_os("CARGO_TARGET_DIR") {
        Some(dir) => PathBuf::from(dir),
        None => workspace_root().join("target"),
    }
}

/// Candidate build-profile subdirectories where a fixture `.so` might live.
///
/// Different tooling uses different profile dirs:
/// - `cargo test` (debug) → `debug`
/// - `cargo test --release` → `release`
/// - `cargo tarpaulin` instruments binaries under `target/debug` (same as
///   `cargo test`; the tarpaulin profile subdir does not exist).
///
/// We probe both standard profiles and pick the first that contains the
/// artifact.
fn profile_candidates() -> Vec<PathBuf> {
    let base = target_base();
    ["debug", "release"].iter().map(|p| base.join(p)).collect()
}

/// Locate a fixture `.so` by filename across the candidate profile dirs.
///
/// Returns the first existing path, or `None` if the fixture was not built
/// in any known location.
fn find_fixture_so(filename: &str) -> Option<PathBuf> {
    profile_candidates()
        .into_iter()
        .map(|dir| dir.join(filename))
        .find(|p| p.exists())
}

/// Returns the path to `libhello.so` if it has been built, else `None`.
fn hello_so_path() -> Option<PathBuf> {
    find_fixture_so("libhello.so")
}

/// Returns the path to `libhello_badabi.so` if it has been built, else `None`.
fn hello_badabi_so_path() -> Option<PathBuf> {
    find_fixture_so("libhello_badabi.so")
}

/// Require a fixture `.so` to exist before continuing.
///
/// Hard-fails (panics) with an actionable message if the fixture has not been
/// built in any known profile directory.  This is intentional: these are
/// acceptance tests and a missing fixture must not silently pass.
macro_rules! require_fixture_so {
    ($lookup:expr, $hint:expr) => {{
        match $lookup {
            Some(path) => path,
            None => panic!(
                "fixture .so not found in any known target profile dir — run `{}` first",
                $hint
            ),
        }
    }};
}

// ── Positive path: full end-to-end ────────────────────────────────────────────

/// Full path: load → abi_version → provides → reconcile → wire → call.
#[test]
fn acceptance_full_path_repo_relative() {
    let so = require_fixture_so!(hello_so_path(), "cargo build -p botforge-plugin-hello");
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
    let so = require_fixture_so!(hello_so_path(), "cargo build -p botforge-plugin-hello");
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
    let so = require_fixture_so!(hello_so_path(), "cargo build -p botforge-plugin-hello");
    let mut reg = PluginRegistry::new();
    // Allow-list with a slot the plugin does NOT provide → filtered to empty.
    reg.load_plugin("hello", &so, Some(&["build/compressor".to_owned()]))
        .expect("loading with an empty-post-filter is valid (no capabilities wired)");
    // The registry has no entries (nothing was wired).
    assert!(!reg.is_registered("core/ping", "hello"));
    assert!(!reg.is_registered("build/compressor", "hello"));
}

// ── Negative: ABI version mismatch ────────────────────────────────────────────

/// A real `.so` exporting the wrong `abi_version()` must be rejected by the
/// loader with a structured mismatch error and no wiring side effects.
#[test]
fn abi_mismatch_real_so_is_rejected() {
    let so = require_fixture_so!(
        hello_badabi_so_path(),
        "cargo build -p botforge-plugin-hello-badabi"
    );
    let mut reg = PluginRegistry::new();
    let err = reg
        .load_plugin("hello-badabi", &so, None)
        .expect_err("wrong-ABI fixture must be rejected");
    match err {
        LoadError::AbiVersionMismatch {
            plugin_version,
            host_version,
            ..
        } => {
            assert_eq!(host_version, HOST_ABI_VERSION);
            assert_eq!(plugin_version, HOST_ABI_VERSION + 100);
        }
        other => panic!("expected AbiVersionMismatch, got: {other}"),
    }
    assert!(reg.plugins.is_empty(), "bad-ABI plugin must not be wired");
    assert!(!reg.is_registered("core/ping", "hello"));
}

/// Structured mismatch errors should still render actionable host/plugin versions.
#[test]
fn abi_mismatch_error_display_is_structured() {
    let err = LoadError::AbiVersionMismatch {
        plugin: "bad-plugin".to_owned(),
        plugin_version: 99,
        host_version: HOST_ABI_VERSION,
    };
    let msg = err.to_string();
    assert!(msg.contains("bad-plugin"), "must name the plugin: {msg}");
    assert!(msg.contains("99"), "must show plugin version: {msg}");
    assert!(
        msg.contains(&HOST_ABI_VERSION.to_string()),
        "must show host version: {msg}"
    );
    assert!(matches!(err, LoadError::AbiVersionMismatch { .. }));
}

// ── Negative: (slot, name) collision ─────────────────────────────────────────

/// Loading the same plugin twice must produce a collision error on the second
/// load, naming both providers.
#[test]
fn collision_same_plugin_loaded_twice() {
    let so = require_fixture_so!(hello_so_path(), "cargo build -p botforge-plugin-hello");
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
    let so = require_fixture_so!(hello_so_path(), "cargo build -p botforge-plugin-hello");
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

// ── No autoload: .so present but not in config ────────────────────────��───────

/// A `.so` on disk that is NOT explicitly loaded via load_plugin must never
/// appear in the registry.  This is enforced by the architecture (the registry
/// only gets entries from explicit `load_plugin` calls) — this test documents
/// the invariant.
///
/// This test does NOT require the fixture to exist: it asserts the registry is
/// empty regardless, so it must not use the hard-fail `require_fixture_so!`.
#[test]
fn no_autoload_so_on_disk_not_in_registry_unless_explicitly_loaded() {
    // Even if the .so exists, an empty registry has no entry for it.
    let reg = PluginRegistry::new();
    assert!(!reg.is_registered("core/ping", "hello"));
    assert!(reg.plugins.is_empty());
    // If the fixture happens to be built, it is STILL not in the registry —
    // only explicit load_plugin adds entries.
    if hello_so_path().is_some() {
        assert!(!reg.is_registered("core/ping", "hello"));
    }
}
