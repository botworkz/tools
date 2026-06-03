use anyhow::{bail, Context, Result};
use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};

pub(crate) fn command_exists(program: &str) -> bool {
    Command::new(program)
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok()
}

pub(crate) fn ensure_command(program: &str) -> Result<()> {
    if !command_exists(program) {
        bail!("'{program}' is not available on PATH");
    }
    Ok(())
}

pub(crate) fn run_command(
    program: &str,
    args: &[String],
    envs: &[(&str, &str)],
    failure_context: &str,
) -> Result<()> {
    let status = Command::new(program)
        .args(args)
        .envs(envs.iter().copied())
        .status()
        .with_context(|| format!("failed to execute {program}"))?;
    if !status.success() {
        bail!("{failure_context} (exit status: {status})");
    }
    Ok(())
}

pub(crate) fn resolve_under_root(repo_root: &Path, path: PathBuf) -> PathBuf {
    if path.is_absolute() {
        normalize_path(&path)
    } else {
        normalize_path(&repo_root.join(path))
    }
}

pub(crate) fn normalize_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                let _ = normalized.pop();
            }
            Component::Normal(part) => normalized.push(part),
        }
    }
    normalized
}

pub(crate) fn repo_relative_path(repo_root: &Path, path: &Path) -> Result<String> {
    let repo_root = normalize_path(repo_root);
    let path = normalize_path(path);
    let relative = path.strip_prefix(&repo_root).with_context(|| {
        format!(
            "path '{}' is outside repo root '{}'",
            path.display(),
            repo_root.display()
        )
    })?;
    let rendered = if relative.as_os_str().is_empty() {
        ".".to_string()
    } else {
        relative.to_string_lossy().replace('\\', "/")
    };
    Ok(rendered)
}

pub(crate) fn resolve_cache_dir(
    shasset_cache: Option<OsString>,
    xdg_cache_home: Option<OsString>,
    home: Option<OsString>,
) -> PathBuf {
    if let Some(dir) = shasset_cache.filter(|s| !s.is_empty()) {
        return PathBuf::from(dir);
    }
    if let Some(xdg) = xdg_cache_home.filter(|s| !s.is_empty()) {
        return PathBuf::from(xdg).join("shasset");
    }
    if let Some(home) = home.filter(|s| !s.is_empty()) {
        return PathBuf::from(home).join(".cache").join("shasset");
    }
    PathBuf::from(".cache").join("shasset")
}

pub(crate) fn default_cache_dir() -> PathBuf {
    resolve_cache_dir(
        std::env::var_os("SHASSET_CACHE"),
        std::env::var_os("XDG_CACHE_HOME"),
        std::env::var_os("HOME"),
    )
}

pub(crate) fn materialize_flat(
    blob_path: &Path,
    out_dir: &Path,
    filename: &str,
    executable: bool,
) -> Result<PathBuf> {
    validate_flat_filename(filename)?;

    std::fs::create_dir_all(out_dir)
        .with_context(|| format!("cannot create output dir: {}", out_dir.display()))?;
    let out_path = out_dir.join(filename);
    let tmp_out = out_dir.join(format!(
        ".{}-{}.tmp",
        filename,
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));

    std::fs::copy(blob_path, &tmp_out).with_context(|| {
        format!(
            "cannot materialize cached blob from {} to {}",
            blob_path.display(),
            tmp_out.display()
        )
    })?;

    #[cfg(unix)]
    if executable {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&tmp_out)
            .with_context(|| format!("cannot stat temp output: {}", tmp_out.display()))?
            .permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&tmp_out, perms)
            .with_context(|| format!("cannot set executable mode on {}", tmp_out.display()))?;
    }

    #[cfg(not(unix))]
    let _ = executable;

    if out_path.exists() {
        std::fs::remove_file(&out_path)
            .with_context(|| format!("cannot replace output file: {}", out_path.display()))?;
    }

    std::fs::rename(&tmp_out, &out_path).with_context(|| {
        format!(
            "cannot atomically materialize output from {} to {}",
            tmp_out.display(),
            out_path.display()
        )
    })?;

    Ok(out_path)
}

pub(crate) fn validate_flat_filename(filename: &str) -> Result<()> {
    let file_path = Path::new(filename);
    let components: Vec<Component<'_>> = file_path.components().collect();
    if components.len() != 1 || !matches!(components[0], Component::Normal(_)) {
        bail!("asset filename must be a flat filename, got: {filename}");
    }
    Ok(())
}

pub(crate) fn create_temp_dir(prefix: &str) -> Result<PathBuf> {
    let base = std::env::temp_dir();
    let path = base.join(format!("{prefix}-{}", unique_suffix()));
    std::fs::create_dir_all(&path)
        .with_context(|| format!("cannot create temp dir: {}", path.display()))?;
    Ok(path)
}

pub(crate) fn unique_suffix() -> String {
    format!(
        "{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    )
}

#[cfg(test)]
mod tests {
    use super::{materialize_flat, repo_relative_path};
    use std::path::Path;
    use tempfile::TempDir;

    #[test]
    fn materialize_flat_writes_expected_path() {
        let tmp = TempDir::new().unwrap();
        let blob = tmp.path().join("blob");
        let out = tmp.path().join("out");
        std::fs::write(&blob, b"hello").unwrap();

        let path = materialize_flat(&blob, &out, "tool.bin", false).unwrap();
        assert_eq!(path, out.join("tool.bin"));
        assert_eq!(std::fs::read(&path).unwrap(), b"hello");
        assert!(Path::new(&out.join("tool.bin")).is_file());
    }

    #[test]
    fn materialize_flat_replaces_existing_file() {
        let tmp = TempDir::new().unwrap();
        let blob = tmp.path().join("blob");
        let out = tmp.path().join("out");
        std::fs::create_dir_all(&out).unwrap();
        std::fs::write(&blob, b"new-bytes").unwrap();
        std::fs::write(out.join("asset"), b"old-bytes").unwrap();

        materialize_flat(&blob, &out, "asset", false).unwrap();
        assert_eq!(std::fs::read(out.join("asset")).unwrap(), b"new-bytes");
    }

    #[test]
    fn materialize_flat_rejects_non_flat_name() {
        let tmp = TempDir::new().unwrap();
        let blob = tmp.path().join("blob");
        let out = tmp.path().join("out");
        std::fs::write(&blob, b"hello").unwrap();

        assert!(materialize_flat(&blob, &out, "nested/asset", false).is_err());
        assert!(materialize_flat(&blob, &out, "../asset", false).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn materialize_flat_sets_executable_mode() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new().unwrap();
        let blob = tmp.path().join("blob");
        let out = tmp.path().join("out");
        std::fs::write(&blob, b"hello").unwrap();

        let path = materialize_flat(&blob, &out, "tool", true).unwrap();
        let mode = std::fs::metadata(path).unwrap().permissions().mode();
        assert_eq!(mode & 0o111, 0o111);
    }

    #[test]
    fn repo_relative_path_returns_relative_path_for_inside_path() {
        let relative = repo_relative_path(
            Path::new("/repo/root"),
            Path::new("/repo/root/build/packer_ssh_key"),
        )
        .unwrap();
        assert_eq!(relative, "build/packer_ssh_key");
    }

    #[test]
    fn repo_relative_path_normalizes_dots_and_parents() {
        let relative = repo_relative_path(
            Path::new("/repo/root"),
            Path::new("/repo/root/build/./nested/../packer_ssh_key"),
        )
        .unwrap();
        assert_eq!(relative, "build/packer_ssh_key");
    }

    #[test]
    fn repo_relative_path_rejects_outside_path() {
        let err =
            repo_relative_path(Path::new("/repo/root"), Path::new("/repo/other/key")).unwrap_err();
        assert!(err.to_string().contains("outside repo root"));
    }
}
