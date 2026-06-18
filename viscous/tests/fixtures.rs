//! End-to-end integration tests driven by on-disk fixtures.
//!
//! Each subdirectory of `tests/fixtures/` is one scenario:
//!
//! ```text
//! fixtures/<scenario>/
//!   template/         # input template (contains __template__.yaml)
//!   vars.yaml         # input vars
//!   expected/         # snapshot of the destination tree after `generate`
//! ```
//!
//! The runner walks both `expected/` and the actual generated tree and
//! compares them byte-for-byte. Adding a scenario = adding a directory.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

fn fixtures_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

fn list_files(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut out = BTreeMap::new();
    if !root.exists() {
        return out;
    }
    for entry in walkdir::WalkDir::new(root).sort_by_file_name() {
        let entry = entry.unwrap();
        if !entry.file_type().is_file() {
            continue;
        }
        let rel = entry.path().strip_prefix(root).unwrap().to_path_buf();
        let bytes = std::fs::read(entry.path()).unwrap();
        out.insert(rel, bytes);
    }
    out
}

fn run_fixture(name: &str) {
    let root = fixtures_root().join(name);
    let template = root.join("template");
    let expected = root.join("expected");
    let vars_path = root.join("vars.yaml");

    assert!(
        template.is_dir(),
        "fixture {name}: template/ missing at {}",
        template.display()
    );
    assert!(
        expected.is_dir(),
        "fixture {name}: expected/ missing at {}",
        expected.display()
    );

    let vars: serde_json::Value = if vars_path.is_file() {
        let raw = std::fs::read_to_string(&vars_path).unwrap();
        let y: serde_yaml::Value = serde_yaml::from_str(&raw).unwrap();
        serde_json::to_value(y).unwrap()
    } else {
        serde_json::json!({})
    };

    let dest = tempfile::tempdir().unwrap();
    let plan = viscous::generate(
        &template,
        &vars,
        dest.path(),
        viscous::DestPolicy::RequireEmpty,
    )
    .unwrap_or_else(|e| panic!("fixture {name}: generate failed: {e:#}"));

    let got = list_files(dest.path());
    let want = list_files(&expected);

    // Compare keys first for clearer diagnostics.
    let got_keys: Vec<_> = got.keys().collect();
    let want_keys: Vec<_> = want.keys().collect();
    assert_eq!(
        got_keys, want_keys,
        "fixture {name}: file-set differs\n  got:    {got_keys:?}\n  wanted: {want_keys:?}\n  plan summary: final_files={}, collisions_resolved={}",
        plan.final_files, plan.collisions_resolved
    );

    for (path, want_bytes) in &want {
        let got_bytes = got.get(path).unwrap();
        assert_eq!(
            String::from_utf8_lossy(got_bytes),
            String::from_utf8_lossy(want_bytes),
            "fixture {name}: contents differ at {path:?}"
        );
    }
}

#[test]
fn fixture_leptos_webview() {
    run_fixture("leptos_webview");
}

#[test]
fn fixture_minimal_static() {
    run_fixture("minimal_static");
}

#[test]
fn fixture_overrides_and_appends() {
    run_fixture("overrides_and_appends");
}
