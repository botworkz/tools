//! Acceptance tests for the botforge plugin host.
//!
//! These tests exercise the full load → version → provides → reconcile → call
//! path using the real compiled `libhello.so`, `libpigz.so`, and
//! `libgithub.so` fixture plugins.
//!
//! # Running
//!
//! Build the fixture `.so` files first:
//!
//! ```sh
//! cargo build -p botforge-plugin-hello
//! cargo build -p botforge-plugin-hello-badabi
//! cargo build -p botforge-plugin-pigz
//! cargo build -p botforge-plugin-github
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

/// Returns the path to `libpigz.so` if it has been built, else `None`.
fn pigz_so_path() -> Option<PathBuf> {
    find_fixture_so("libpigz.so")
}

/// Returns the path to `libgithub.so` if it has been built, else `None`.
fn github_so_path() -> Option<PathBuf> {
    find_fixture_so("libgithub.so")
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

// ── pigz build/compressor acceptance ─────────────────────────────────────────

/// Full pigz path: load → abi_version → provides → reconcile → wire → round-trip.
///
/// Verifies:
/// - `libpigz.so` loads cleanly and is wired under `build/compressor` / `pigz`.
/// - `get_compressor("pigz")` returns a valid `CompressorHandle`.
/// - `compress(data) → decompress(compressed) == data` (round-trip correctness).
/// - The compressed output is a valid gzip stream (starts with the gzip magic bytes).
#[test]
fn pigz_acceptance_full_path_and_round_trip() {
    let so = require_fixture_so!(pigz_so_path(), "cargo build -p botforge-plugin-pigz");
    let mut reg = PluginRegistry::new();
    reg.load_plugin("pigz", &so, None)
        .expect("pigz plugin should load cleanly");

    // Plugin is wired under the correct slot and name.
    assert!(reg.is_registered("build/compressor", "pigz"));
    assert_eq!(reg.provider_of("build/compressor", "pigz"), Some("pigz"));

    // Exactly one plugin loaded.
    assert_eq!(reg.plugins.len(), 1);
    assert_eq!(reg.plugins[0].name, "pigz");
    assert_eq!(reg.plugins[0].abi_version, HOST_ABI_VERSION);
    assert_eq!(
        reg.plugins[0].provides,
        vec![("build/compressor".to_owned(), "pigz".to_owned())]
    );

    // Retrieve the compressor handle.
    let handle = reg
        .get_compressor("pigz")
        .expect("build/compressor handle must be wired");

    // ── Round-trip test ───────────────────────────────────────────────────────
    // Use a realistic artifact-sized input (~1 MiB) to exercise parallel gzip.
    let original: Vec<u8> = (0u32..262144).flat_map(|i| i.to_le_bytes()).collect();

    let compressed = handle
        .compress(&original, "")
        .expect("compress must succeed");

    // The compressed output must be non-empty and smaller (or at least valid).
    assert!(
        !compressed.is_empty(),
        "compressed output must not be empty"
    );

    // Verify gzip magic bytes (0x1F 0x8B).
    assert_eq!(
        &compressed[..2],
        &[0x1F, 0x8B],
        "compressed output must start with gzip magic"
    );

    let decompressed = handle
        .decompress(&compressed, "")
        .expect("decompress must succeed");

    assert_eq!(
        decompressed, original,
        "round-trip must be byte-for-byte identical"
    );
}

/// Pigz compressor with opts (`-1 -p1`) — fast compression, single thread.
#[test]
fn pigz_acceptance_with_opts() {
    let so = require_fixture_so!(pigz_so_path(), "cargo build -p botforge-plugin-pigz");
    let mut reg = PluginRegistry::new();
    reg.load_plugin("pigz", &so, None)
        .expect("pigz plugin should load");
    let handle = reg.get_compressor("pigz").expect("compressor handle");

    let data = b"hello from the pigz opts test, repeated many times"
        .iter()
        .copied()
        .cycle()
        .take(4096)
        .collect::<Vec<u8>>();

    let compressed = handle
        .compress(&data, "-1 -p1")
        .expect("compress with opts");
    assert_eq!(&compressed[..2], &[0x1F, 0x8B], "must be gzip");

    let decompressed = handle.decompress(&compressed, "").expect("decompress");
    assert_eq!(decompressed, data, "round-trip with opts");
}

/// Pigz compressor names appear in `PluginRegistry::compressor_names()`.
#[test]
fn pigz_appears_in_compressor_names() {
    let so = require_fixture_so!(pigz_so_path(), "cargo build -p botforge-plugin-pigz");
    let mut reg = PluginRegistry::new();
    reg.load_plugin("pigz", &so, None).expect("load");
    let names = reg.compressor_names();
    assert!(
        names.contains(&"pigz"),
        "pigz must appear in compressor_names(): {names:?}"
    );
}

/// Loading pigz alongside hello must not collide — different slots.
#[test]
fn pigz_and_hello_coexist_no_collision() {
    let hello_so = require_fixture_so!(hello_so_path(), "cargo build -p botforge-plugin-hello");
    let pigz_so = require_fixture_so!(pigz_so_path(), "cargo build -p botforge-plugin-pigz");
    let mut reg = PluginRegistry::new();
    reg.load_plugin("hello", &hello_so, None)
        .expect("hello should load");
    reg.load_plugin("pigz", &pigz_so, None)
        .expect("pigz should load alongside hello (different slots)");
    assert!(reg.is_registered("core/ping", "hello"));
    assert!(reg.is_registered("build/compressor", "pigz"));
    assert_eq!(reg.plugins.len(), 2);
}

/// A `build/compressor` named `pigz` blocks a second `build/compressor` named
/// `pigz` (same slot+name collision).
#[test]
fn pigz_collision_same_slot_name() {
    let so = require_fixture_so!(pigz_so_path(), "cargo build -p botforge-plugin-pigz");
    let mut reg = PluginRegistry::new();
    reg.load_plugin("pigz", &so, None)
        .expect("first pigz should load");
    let err = reg
        .load_plugin("pigz-2", &so, None)
        .expect_err("second pigz under same slot+name must collide");
    match &err {
        LoadError::CapabilityCollision {
            slot,
            name,
            existing_provider,
            new_provider,
        } => {
            assert_eq!(slot, "build/compressor");
            assert_eq!(name, "pigz");
            assert_eq!(existing_provider, "pigz");
            assert_eq!(new_provider, "pigz-2");
        }
        other => panic!("expected CapabilityCollision, got: {other}"),
    }
    assert_eq!(reg.plugins.len(), 1, "second plugin must not be wired");
}

// ── github publish/github acceptance ─────────────────────────────────────────

/// Minimal mock HTTP server for the GitHub Releases REST API.
///
/// Handles exactly two request patterns:
/// 1. `POST /repos/{owner}/{repo}/releases` → 201 with create-release JSON
/// 2. `POST /{any_upload_path}` → 201 with upload-asset JSON
///
/// All other requests get a 500 error response.
///
/// The server runs in a background thread and is stopped by dropping the
/// returned `tiny_http::Server` (which closes the listening socket).
struct MockGithubServer {
    /// Base URL of the mock server (e.g. `http://127.0.0.1:PORT`).
    pub base_url: String,
    /// Recorded requests for assertion.
    pub requests: std::sync::Arc<std::sync::Mutex<Vec<ReceivedRequest>>>,
    // keep the server alive
    _server: std::sync::Arc<tiny_http::Server>,
}

struct ReceivedRequest {
    pub method: String,
    pub url: String,
    pub body: Vec<u8>,
    pub content_type: Option<String>,
}

impl MockGithubServer {
    fn start() -> Self {
        let server = tiny_http::Server::http("127.0.0.1:0").expect("bind mock server");
        let addr = server.server_addr().to_ip().expect("server addr");
        let base_url = format!("http://127.0.0.1:{}", addr.port());
        let requests: std::sync::Arc<std::sync::Mutex<Vec<ReceivedRequest>>> =
            std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));

        let server = std::sync::Arc::new(server);
        let server_clone = std::sync::Arc::clone(&server);
        let requests_clone = std::sync::Arc::clone(&requests);
        let base_url_clone = base_url.clone();

        std::thread::spawn(move || {
            // Handle requests until the server is dropped.
            while let Ok(req) = server_clone.recv() {
                let method = req.method().to_string();
                let url = req.url().to_string();
                let content_type = req
                    .headers()
                    .iter()
                    .find(|h| h.field.equiv("content-type"))
                    .map(|h| h.value.to_string());

                let mut body = Vec::new();
                let mut req = req;
                let _ = std::io::Read::read_to_end(req.as_reader(), &mut body);

                requests_clone.lock().unwrap().push(ReceivedRequest {
                    method: method.clone(),
                    url: url.clone(),
                    body: body.clone(),
                    content_type,
                });

                // Determine the right response based on the request path.
                let (status, response_body) =
                    if method == "POST" && url.contains("/releases") && !url.contains("/assets") {
                        // create-release endpoint
                        let release_url =
                            format!("{base_url_clone}/repos/owner/repo/releases/tag/v1.0.0");
                        let upload_url = format!(
                            "{base_url_clone}/repos/owner/repo/releases/1/assets{{?name,label}}"
                        );
                        let json = format!(
                            r#"{{"id":1,"html_url":"{release_url}","upload_url":"{upload_url}"}}"#
                        );
                        (201u16, json)
                    } else if method == "POST" {
                        // asset-upload endpoint (any path with POST)
                        let json = r#"{"id":1,"name":"vm.qcow2","size":14}"#.to_string();
                        (201u16, json)
                    } else {
                        (500u16, r#"{"error":"unexpected request"}"#.to_string())
                    };

                let response = tiny_http::Response::from_string(response_body)
                    .with_status_code(status)
                    .with_header(
                        "Content-Type: application/json"
                            .parse::<tiny_http::Header>()
                            .unwrap(),
                    );
                let _ = req.respond(response);
            }
        });

        MockGithubServer {
            base_url,
            requests,
            _server: server,
        }
    }
}

/// Full github path: load → abi_version → provides → reconcile → wire →
/// call against a mock HTTP server → assert create-release + upload-asset.
///
/// This test never touches real github.com.
#[test]
fn github_acceptance_full_path_against_mock() {
    let so = require_fixture_so!(github_so_path(), "cargo build -p botforge-plugin-github");

    let mut reg = PluginRegistry::new();
    reg.load_plugin("github", &so, None)
        .expect("github plugin should load cleanly");

    // Plugin is wired under the correct slot and name.
    assert!(reg.is_registered("publish/github", "github"));
    assert_eq!(reg.provider_of("publish/github", "github"), Some("github"));

    // Exactly one plugin loaded.
    assert_eq!(reg.plugins.len(), 1);
    assert_eq!(reg.plugins[0].name, "github");
    assert_eq!(reg.plugins[0].abi_version, HOST_ABI_VERSION);
    assert_eq!(
        reg.plugins[0].provides,
        vec![("publish/github".to_owned(), "github".to_owned())]
    );

    // Start the mock HTTP server (no real GitHub access).
    let mock = MockGithubServer::start();

    // Create a temporary asset file to upload.
    let tmpdir = tempfile::tempdir().expect("temp dir");
    let asset_path = tmpdir.path().join("vm.qcow2");
    std::fs::write(&asset_path, b"fake-qcow2-data").expect("write temp asset");

    // Retrieve the publisher handle.
    let publisher = reg
        .get_publisher("github")
        .expect("publish/github handle must be wired");

    let request = botforge_plugin_host::PublishRequest {
        repo: "owner/repo",
        tag: "v1.0.0",
        title: Some("Release v1.0.0"),
        description: Some("Test release from botforge acceptance test."),
        asset_paths: &[asset_path.as_path()],
        api_base_url: &mock.base_url,
        secrets: &[("token", "dummy-test-token")],
    };

    let outcome = publisher
        .publish(&request)
        .expect("publish must succeed against mock");

    // The release URL must match what the mock returned.
    assert!(
        outcome.release_url.contains("v1.0.0"),
        "release URL should contain the tag: {}",
        outcome.release_url
    );
    assert!(
        outcome.release_url.starts_with(&mock.base_url),
        "release URL should point to mock server: {}",
        outcome.release_url
    );

    // Assert the mock received exactly 2 requests: create-release + upload-asset.
    let reqs = mock.requests.lock().unwrap();
    assert_eq!(
        reqs.len(),
        2,
        "mock should have received exactly 2 requests (create-release + upload-asset), \
         got: {}\nRequests: {:?}",
        reqs.len(),
        reqs.iter()
            .map(|r| format!("{} {}", r.method, r.url))
            .collect::<Vec<_>>()
    );

    // First request: POST to the releases endpoint.
    let create_req = &reqs[0];
    assert_eq!(create_req.method, "POST", "first request must be POST");
    assert!(
        create_req.url.contains("/releases"),
        "first request must target the releases endpoint: {}",
        create_req.url
    );

    // The create-release body must be valid JSON with the expected fields.
    let body: serde_json::Value =
        serde_json::from_slice(&create_req.body).expect("create-release body must be JSON");
    assert_eq!(
        body["tag_name"].as_str(),
        Some("v1.0.0"),
        "tag_name must match"
    );
    assert_eq!(
        body["name"].as_str(),
        Some("Release v1.0.0"),
        "name must match"
    );

    // Second request: POST to the asset-upload endpoint.
    let upload_req = &reqs[1];
    assert_eq!(upload_req.method, "POST", "second request must be POST");
    assert!(
        upload_req.url.contains("name=vm.qcow2"),
        "upload URL must include asset filename: {}",
        upload_req.url
    );
    assert_eq!(
        upload_req.body, b"fake-qcow2-data",
        "upload body must be the asset content"
    );
    assert_eq!(
        upload_req.content_type.as_deref(),
        Some("application/octet-stream"),
        "upload Content-Type must be application/octet-stream"
    );
}

/// `publish/github` collision test — same slot+name must collide.
#[test]
fn github_collision_same_slot_name() {
    let so = require_fixture_so!(github_so_path(), "cargo build -p botforge-plugin-github");
    let mut reg = PluginRegistry::new();
    reg.load_plugin("github", &so, None)
        .expect("first github should load");
    let err = reg
        .load_plugin("github-2", &so, None)
        .expect_err("second github under same slot+name must collide");
    match &err {
        botforge_plugin_host::LoadError::CapabilityCollision {
            slot,
            name,
            existing_provider,
            new_provider,
        } => {
            assert_eq!(slot, "publish/github");
            assert_eq!(name, "github");
            assert_eq!(existing_provider, "github");
            assert_eq!(new_provider, "github-2");
        }
        other => panic!("expected CapabilityCollision, got: {other}"),
    }
    assert_eq!(reg.plugins.len(), 1, "second plugin must not be wired");
}

/// `publish/github` and `build/compressor` can coexist — different slots.
#[test]
fn github_and_pigz_coexist_no_collision() {
    let github_so = require_fixture_so!(github_so_path(), "cargo build -p botforge-plugin-github");
    let pigz_so = require_fixture_so!(pigz_so_path(), "cargo build -p botforge-plugin-pigz");
    let mut reg = PluginRegistry::new();
    reg.load_plugin("github", &github_so, None)
        .expect("github should load");
    reg.load_plugin("pigz", &pigz_so, None)
        .expect("pigz should load alongside github (different slots)");
    assert!(reg.is_registered("publish/github", "github"));
    assert!(reg.is_registered("build/compressor", "pigz"));
    assert_eq!(reg.plugins.len(), 2);
}

/// `get_publisher` returns None when no publish/github plugin is loaded.
#[test]
fn get_publisher_returns_none_without_plugin() {
    let reg = PluginRegistry::new();
    assert!(reg.get_publisher("github").is_none());
}

// ── Error-path: mock HTTP errors surface as legible messages ─────────────────

/// A mock server that always returns a fixed status code with a JSON body.
struct FixedStatusMockServer {
    pub base_url: String,
    _server: std::sync::Arc<tiny_http::Server>,
}

impl FixedStatusMockServer {
    fn start(status: u16, body: &'static str) -> Self {
        let server = tiny_http::Server::http("127.0.0.1:0").expect("bind mock server");
        let addr = server.server_addr().to_ip().expect("server addr");
        let base_url = format!("http://127.0.0.1:{}", addr.port());
        let server = std::sync::Arc::new(server);
        let server_clone = std::sync::Arc::clone(&server);

        std::thread::spawn(move || {
            while let Ok(req) = server_clone.recv() {
                let response = tiny_http::Response::from_string(body)
                    .with_status_code(status)
                    .with_header(
                        "Content-Type: application/json"
                            .parse::<tiny_http::Header>()
                            .unwrap(),
                    );
                let _ = req.respond(response);
            }
        });

        FixedStatusMockServer {
            base_url,
            _server: server,
        }
    }
}

/// Helper: load the github plugin, publish against a mock, return the error.
fn github_publish_expect_err(mock_base_url: &str) -> botforge_plugin_host::LoadError {
    let so = require_fixture_so!(github_so_path(), "cargo build -p botforge-plugin-github");
    let mut reg = PluginRegistry::new();
    reg.load_plugin("github", &so, None)
        .expect("github plugin should load cleanly");

    let tmpdir = tempfile::tempdir().expect("temp dir");
    let asset_path = tmpdir.path().join("asset.bin");
    std::fs::write(&asset_path, b"test-data").expect("write temp asset");

    let publisher = reg.get_publisher("github").expect("publisher handle");
    let request = botforge_plugin_host::PublishRequest {
        repo: "owner/repo",
        tag: "v1.0.0",
        title: None,
        description: None,
        asset_paths: &[asset_path.as_path()],
        api_base_url: mock_base_url,
        secrets: &[("token", "dummy-token")],
    };

    publisher
        .publish(&request)
        .expect_err("publish against error mock must fail")
}

/// A 401 response must surface as `PublisherError` with a legible message.
#[test]
fn github_error_401_surfaces_legible_message() {
    let mock = FixedStatusMockServer::start(
        401,
        r#"{"message":"Bad credentials","documentation_url":"https://docs.github.com"}"#,
    );

    let err = github_publish_expect_err(&mock.base_url);
    match &err {
        LoadError::PublisherError { code, message, .. } => {
            assert_eq!(*code, -1, "error code must be -1");
            assert!(
                message.contains("401")
                    || message.contains("Unauthorized")
                    || message.contains("credentials"),
                "error message must mention 401 or auth failure: {message:?}"
            );
        }
        other => panic!("expected PublisherError, got: {other}"),
    }
}

/// A 500 response must surface as `PublisherError` with a legible message.
#[test]
fn github_error_500_surfaces_legible_message() {
    let mock = FixedStatusMockServer::start(500, r#"{"message":"Internal Server Error"}"#);

    let err = github_publish_expect_err(&mock.base_url);
    match &err {
        LoadError::PublisherError { code, message, .. } => {
            assert_eq!(*code, -1, "error code must be -1");
            assert!(
                message.contains("500") || message.contains("Server"),
                "error message must mention 500: {message:?}"
            );
        }
        other => panic!("expected PublisherError, got: {other}"),
    }
}

/// Missing 'token' in secrets must surface a clear error message.
#[test]
fn github_missing_token_in_secrets_surfaces_legible_message() {
    let so = require_fixture_so!(github_so_path(), "cargo build -p botforge-plugin-github");
    let mut reg = PluginRegistry::new();
    reg.load_plugin("github", &so, None)
        .expect("github plugin should load cleanly");

    let tmpdir = tempfile::tempdir().expect("temp dir");
    let asset_path = tmpdir.path().join("asset.bin");
    std::fs::write(&asset_path, b"test-data").expect("write temp asset");

    let publisher = reg.get_publisher("github").expect("publisher handle");
    let request = botforge_plugin_host::PublishRequest {
        repo: "owner/repo",
        tag: "v1.0.0",
        title: None,
        description: None,
        asset_paths: &[asset_path.as_path()],
        api_base_url: "http://127.0.0.1:1",
        // Deliberately omit 'token'.
        secrets: &[("other_key", "other_value")],
    };

    let err = publisher
        .publish(&request)
        .expect_err("missing token must fail");

    match &err {
        LoadError::PublisherError { code, message, .. } => {
            assert_eq!(*code, -1, "error code must be -1");
            assert!(
                message.contains("token") || message.contains("secret"),
                "error must mention missing token: {message:?}"
            );
        }
        other => panic!("expected PublisherError, got: {other}"),
    }
}

/// `PublisherError` display string must include the message.
#[test]
fn publisher_error_display_includes_message() {
    let err = LoadError::PublisherError {
        plugin: "github".to_owned(),
        code: -1,
        message: "GitHub API 401: Bad credentials".to_owned(),
    };
    let s = err.to_string();
    assert!(s.contains("github"), "must name plugin: {s}");
    assert!(s.contains("401"), "must contain message content: {s}");
    assert!(s.contains("-1"), "must contain code: {s}");
}
