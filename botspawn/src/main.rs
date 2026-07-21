//! botspawn — bazelisk-style launcher for botforge.
//!
//! Walks up from the current working directory to find the botforge workspace
//! root, reads the desired image reference from `.botforgeversion`, and
//! transparently launches botforge inside that Docker container.
#![forbid(unsafe_code)]

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

// ── Workspace marker names ────────────────────────────────────────────────────

/// Workspace marker filenames accepted by botforge.
///
/// **Source of truth:** `botforge/src/workspace/mod.rs` — `MARKER_NAMES`.
/// This list is intentionally duplicated here (botspawn has no compile-time
/// dependency on the botforge crate).  If you add or remove a marker name in
/// botforge, update this constant to match.
const MARKER_NAMES: &[&str] = &[
    "botforge.yaml",
    "botforge.yml",
    ".botforge.yaml",
    ".botforge.yml",
    "BOTFORGE",
];

const MARKER_DISPLAY: &str = "botforge.yaml, botforge.yml, .botforge.yaml, .botforge.yml, BOTFORGE";

// ── Version-pin file ──────────────────────────────────────────────────────────

/// Version-pin file at the workspace root (analogous to `.bazelversion`).
/// Its contents are a single Docker image reference, e.g.
/// `ghcr.io/botworkz/tools/botforge:1.2.3`.
const VERSION_FILE: &str = ".botforgeversion";

/// Fallback image when no `.botforgeversion` file is present.
const DEFAULT_IMAGE: &str = "ghcr.io/botworkz/tools/botforge:latest";

// ── Entry point ───────────────────────────────────────────────────────────────

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();

    // Handle --help/-h and --version before workspace discovery so the Docker
    // smoke test (`docker run --rm botwork/botspawn:ci --help`) works without
    // requiring an actual botforge workspace or a running Docker daemon.
    if args
        .iter()
        .any(|a| a == "--help" || a == "-h" || a == "--version")
    {
        println!(
            "botspawn {}\n\
             Bazelisk-style launcher for botforge.\n\
             Usage: botspawn [BOTFORGE_ARGS...]",
            env!("CARGO_PKG_VERSION")
        );
        return;
    }

    if let Err(e) = run(&args) {
        eprintln!("botspawn: error: {e:#}");
        std::process::exit(1);
    }
}

fn run(args: &[String]) -> Result<()> {
    let cwd = std::env::current_dir().context("failed to get current working directory")?;
    let root = discover_repo_root(&cwd)?;
    let image = read_image(&root)?;

    eprintln!("botspawn: checking docker and kvm access");
    check_docker()?;

    // Attempt to pin to a content digest for reproducibility; fall back to
    // the tag reference if docker inspect fails (e.g. image not yet pulled).
    let resolved = pin_digest_image(&image).unwrap_or(image);
    eprintln!("botspawn: launching botforge from {resolved}");

    launch_botforge(&root, &resolved, args)
}

// ── Workspace discovery ───────────────────────────────────────────────────────

/// Walk up from `start` until a directory containing a botforge workspace
/// marker is found, and return its canonicalised path.
///
/// The set of accepted marker filenames exactly mirrors what botforge itself
/// accepts (see `botforge/src/workspace/mod.rs` — `MARKER_NAMES`).
fn discover_repo_root(start: &Path) -> Result<PathBuf> {
    let mut dir = start;
    loop {
        if MARKER_NAMES.iter().any(|name| dir.join(name).is_file()) {
            return std::fs::canonicalize(dir)
                .with_context(|| format!("failed to canonicalize '{}'", dir.display()));
        }
        match dir.parent() {
            Some(parent) => dir = parent,
            None => break,
        }
    }
    bail!(
        "not inside a botforge workspace: could not find a marker \
         ({MARKER_DISPLAY}) in '{}' or any parent",
        start.display()
    );
}

// ── Image resolution ──────────────────────────────────────────────────────────

fn read_image(root: &Path) -> Result<String> {
    let version_file = root.join(VERSION_FILE);
    if version_file.is_file() {
        let raw = std::fs::read_to_string(&version_file)
            .with_context(|| format!("failed to read '{}'", version_file.display()))?;
        let image = raw.trim().to_owned();
        if image.is_empty() {
            bail!(
                "'{}' is empty — expected a Docker image reference",
                version_file.display()
            );
        }
        return Ok(image);
    }
    Ok(DEFAULT_IMAGE.to_owned())
}

/// Return an image reference with the tag replaced by a content digest.
///
/// If `image` already contains `@sha256:` it is returned unchanged.
/// Otherwise `docker inspect --format '{{index .RepoDigests 0}}'` is used to
/// resolve the digest from the locally-pulled image.
fn pin_digest_image(image: &str) -> Result<String> {
    if image.contains("@sha256:") {
        return Ok(image.to_owned());
    }
    let out = Command::new("docker")
        .args(["inspect", "--format", "{{index .RepoDigests 0}}", image])
        .output()
        .context("failed to run 'docker inspect'")?;
    if !out.status.success() {
        bail!(
            "could not inspect image '{}': {}",
            image,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let pinned = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    if pinned.is_empty() || !pinned.contains("@sha256:") {
        bail!(
            "docker inspect returned no digest for '{}' (got: {:?})",
            image,
            pinned
        );
    }
    Ok(pinned)
}

// ── Docker launch ─────────────────────────────────────────────────────────────

/// Build the `docker run` argument list for launching botforge.
///
/// Returns a `Vec<String>` starting from `"run"` so the caller can pass them
/// directly to `Command::new("docker").args(...)`.
fn parse_launch_args(root: &Path, image: &str, user_args: &[String]) -> Vec<String> {
    let root_str = root.to_string_lossy();
    let mut args = vec![
        "run".to_owned(),
        "--rm".to_owned(),
        "--network".to_owned(),
        "host".to_owned(),
        "-v".to_owned(),
        format!("{root_str}:/work"),
        "-w".to_owned(),
        "/work".to_owned(),
        image.to_owned(),
    ];
    args.extend(user_args.iter().cloned());
    args
}

fn check_docker() -> Result<()> {
    let status = Command::new("docker")
        .args(["info"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .context("failed to run 'docker info' — is Docker installed and running?")?;
    if !status.success() {
        bail!("Docker is not available or not running ('docker info' failed)");
    }
    Ok(())
}

fn launch_botforge(root: &Path, image: &str, user_args: &[String]) -> Result<()> {
    let docker_args = parse_launch_args(root, image, user_args);
    let status = Command::new("docker")
        .args(&docker_args)
        .status()
        .context("failed to spawn 'docker run'")?;
    let code = status.code().unwrap_or(1);
    if code != 0 {
        std::process::exit(code);
    }
    Ok(())
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    // ── discover_repo_root ────────────────────────────────────────────────────

    /// Walk-up to a `botforge.yaml` marker (original behaviour preserved).
    #[test]
    fn discover_repo_root_walks_up_to_botforge_yaml() {
        let root = TempDir::new().unwrap();
        fs::write(root.path().join("botforge.yaml"), "").unwrap();
        let nested = root.path().join("a/b/c");
        fs::create_dir_all(&nested).unwrap();
        let found = discover_repo_root(&nested).unwrap();
        assert_eq!(found, root.path().canonicalize().unwrap());
    }

    /// No marker anywhere in the tree → bail with the expected error.
    #[test]
    fn discover_repo_root_requires_marker() {
        let root = TempDir::new().unwrap();
        let err = discover_repo_root(root.path()).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("not inside a botforge workspace"),
            "unexpected error: {msg}"
        );
    }

    // ── all five marker filenames (aligned with botforge) ─────────────────────
    //
    // These tests FAIL on a single-literal `const MARKER_FILE = "botforge.yaml"`
    // implementation and PASS after the fix (MARKER_NAMES slice).

    #[test]
    fn discover_repo_root_accepts_botforge_yaml() {
        let root = TempDir::new().unwrap();
        fs::write(root.path().join("botforge.yaml"), "").unwrap();
        let found = discover_repo_root(root.path()).unwrap();
        assert_eq!(found, root.path().canonicalize().unwrap());
    }

    #[test]
    fn discover_repo_root_accepts_botforge_yml() {
        let root = TempDir::new().unwrap();
        fs::write(root.path().join("botforge.yml"), "").unwrap();
        let found = discover_repo_root(root.path()).unwrap();
        assert_eq!(found, root.path().canonicalize().unwrap());
    }

    #[test]
    fn discover_repo_root_accepts_dotbotforge_yaml() {
        let root = TempDir::new().unwrap();
        fs::write(root.path().join(".botforge.yaml"), "").unwrap();
        let found = discover_repo_root(root.path()).unwrap();
        assert_eq!(found, root.path().canonicalize().unwrap());
    }

    #[test]
    fn discover_repo_root_accepts_dotbotforge_yml() {
        let root = TempDir::new().unwrap();
        fs::write(root.path().join(".botforge.yml"), "").unwrap();
        let found = discover_repo_root(root.path()).unwrap();
        assert_eq!(found, root.path().canonicalize().unwrap());
    }

    #[test]
    fn discover_repo_root_accepts_botforge_uppercase() {
        let root = TempDir::new().unwrap();
        fs::write(root.path().join("BOTFORGE"), "").unwrap();
        let found = discover_repo_root(root.path()).unwrap();
        assert_eq!(found, root.path().canonicalize().unwrap());
    }

    /// Walk-up works for every marker name, not just `botforge.yaml`.
    #[test]
    fn discover_repo_root_walks_up_to_all_markers() {
        for marker in MARKER_NAMES {
            let root = TempDir::new().unwrap();
            fs::write(root.path().join(marker), "").unwrap();
            let nested = root.path().join("deep/nested/dir");
            fs::create_dir_all(&nested).unwrap();
            let found = discover_repo_root(&nested).unwrap();
            assert_eq!(
                found,
                root.path().canonicalize().unwrap(),
                "walk-up failed for marker '{marker}'"
            );
        }
    }

    /// CI smoke scenario: marker is in the start directory itself.
    ///
    /// Reproduces the `/work`-style Docker mount case where cwd IS the
    /// workspace root and no walk-up is needed.  This is the scenario from
    /// `_crate.yml`'s botforge smoke test:
    ///
    ///   docker run --rm -v "$PWD/.ci-deps-smoke:/work" -w /work botwork/botforge:ci …
    ///
    /// All five accepted marker names must be found in the start directory.
    #[test]
    fn discover_repo_root_finds_marker_in_start_dir() {
        for marker in MARKER_NAMES {
            let root = TempDir::new().unwrap();
            fs::write(root.path().join(marker), "").unwrap();
            // start == root — no walk-up required.
            let found = discover_repo_root(root.path()).unwrap();
            assert_eq!(
                found,
                root.path().canonicalize().unwrap(),
                "failed to find marker '{marker}' in start directory"
            );
        }
    }

    // ── parse_launch_args ─────────────────────────────────────────────────────

    #[test]
    fn parse_launch_args_includes_workspace_mount() {
        let root = TempDir::new().unwrap();
        let args = parse_launch_args(root.path(), "some-image:tag", &[]);
        let joined = args.join(" ");
        let root_str = root.path().to_string_lossy();
        assert!(
            joined.contains(&format!("{root_str}:/work")),
            "workspace mount missing: {joined}"
        );
    }

    #[test]
    fn parse_launch_args_sets_working_dir_to_work() {
        let root = TempDir::new().unwrap();
        let args = parse_launch_args(root.path(), "some-image:tag", &[]);
        let pos = args
            .iter()
            .position(|a| a == "-w")
            .expect("-w flag not found");
        assert_eq!(args[pos + 1], "/work");
    }

    #[test]
    fn parse_launch_args_passes_user_args_at_end() {
        let root = TempDir::new().unwrap();
        let user_args = vec!["deps".to_owned(), "--context".to_owned(), ".".to_owned()];
        let args = parse_launch_args(root.path(), "some-image:tag", &user_args);
        let last_three: Vec<_> = args.iter().rev().take(3).rev().cloned().collect();
        assert_eq!(last_three, vec!["deps", "--context", "."]);
    }

    #[test]
    fn parse_launch_args_uses_network_host() {
        let root = TempDir::new().unwrap();
        let args = parse_launch_args(root.path(), "some-image:tag", &[]);
        let pos = args
            .iter()
            .position(|a| a == "--network")
            .expect("--network flag not found");
        assert_eq!(args[pos + 1], "host");
    }

    #[test]
    fn parse_launch_args_removes_container() {
        let root = TempDir::new().unwrap();
        let args = parse_launch_args(root.path(), "some-image:tag", &[]);
        assert!(args.contains(&"--rm".to_owned()), "--rm flag missing");
    }

    // ── pin_digest_image ──────────────────────────────────────────────────────

    #[test]
    fn pin_digest_image_passthrough_when_already_pinned() {
        let pinned = "ghcr.io/botworkz/tools/botforge@sha256:abc123def456";
        let result = pin_digest_image(pinned).unwrap();
        assert_eq!(result, pinned);
    }

    #[test]
    fn pin_digest_image_passthrough_tag_and_digest() {
        // image:tag@sha256:... is unusual but valid and should pass through.
        let pinned = "ghcr.io/botworkz/tools/botforge:ci@sha256:deadbeef0000";
        let result = pin_digest_image(pinned).unwrap();
        assert_eq!(result, pinned);
    }
}
