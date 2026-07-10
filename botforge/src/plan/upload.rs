use anyhow::{Context, Result};
use glob::MatchOptions;
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::ssh::{scp_with_retry, ssh_with_retry, SshOptions};
use crate::util::{resolve_under_root, shell_single_quote, unique_suffix};

use super::config::src_has_glob_metacharacters;

const TRANSPORT_RETRIES: usize = 10;
const TRANSPORT_RETRY_DELAY: Duration = Duration::from_secs(2);

#[derive(Debug, Deserialize, Clone, PartialEq, Eq, Default)]
#[serde(deny_unknown_fields)]
pub(crate) struct TopLevelUpload {
    pub(crate) src: String,
    pub(crate) dest: String,
    /// File permission mode (3–4 octal digits). Defaults to `"0644"` at install time.
    #[serde(default)]
    pub(crate) mode: Option<String>,
    /// Owner (user name or numeric uid) to pass to `install -o`. Defaults to `root`.
    #[serde(default)]
    pub(crate) owner: Option<String>,
    /// Group (group name or numeric gid) to pass to `install -g`. Defaults to `root`.
    #[serde(default)]
    pub(crate) group: Option<String>,
    /// When `false`, the install fails with a hard error if `dest` already exists.
    /// Defaults to `true` (overwrite is allowed).
    #[serde(default)]
    pub(crate) overwrite: Option<bool>,
    /// When `true` (default), create intermediate destination directories (`install -D`).
    /// When `false`, the parent directory must already exist.
    #[serde(default)]
    pub(crate) parents: Option<bool>,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct UploadMapping {
    pub(crate) local_path: PathBuf,
    pub(crate) guest_dest: String,
}

/// Options controlling how `install_file_to_guest` places a file in the guest.
struct InstallOpts<'a> {
    /// File permission mode (3–4 octal digits). Defaults to `"0644"`.
    mode: Option<&'a str>,
    /// Owner (user name or numeric uid). Defaults to `"root"`.
    owner: Option<&'a str>,
    /// Group (group name or numeric gid). Defaults to `"root"`.
    group: Option<&'a str>,
    /// If `false`, fail with an error when `dest` already exists. Defaults to `true`.
    overwrite: Option<bool>,
    /// If `true` (default), create intermediate destination directories (`install -D`).
    parents: Option<bool>,
}

pub(crate) fn resolve_top_level_upload_mappings(
    repo_root: &Path,
    upload: &TopLevelUpload,
) -> Result<Vec<UploadMapping>> {
    let src = upload.src.trim();
    let dest = upload.dest.trim();
    validate_top_level_upload_src_for_runtime(src)?;
    if !src_has_glob_metacharacters(src) {
        let local_path = resolve_under_root(repo_root, PathBuf::from(src));
        if !local_path.is_file() {
            anyhow::bail!(
                "upload src '{}' does not resolve to a regular file under {}",
                src,
                repo_root.display()
            );
        }
        let guest_dest = if dest.ends_with('/') {
            let basename = local_path.file_name().with_context(|| {
                format!("upload src '{}' has no file name", local_path.display())
            })?;
            Path::new(dest)
                .join(basename)
                .to_string_lossy()
                .into_owned()
        } else {
            dest.to_string()
        };
        return Ok(vec![UploadMapping {
            local_path,
            guest_dest,
        }]);
    }

    let fixed_prefix = fixed_glob_prefix(src);
    let fixed_prefix_root = repo_root.join(&fixed_prefix);
    let pattern = repo_root.join(src).to_string_lossy().into_owned();
    let match_options = MatchOptions {
        case_sensitive: true,
        require_literal_separator: false,
        require_literal_leading_dot: false,
    };
    let mut mappings = Vec::new();
    for entry in glob::glob_with(&pattern, match_options)
        .with_context(|| format!("invalid upload src glob '{}'", src))?
    {
        let local_path = entry.with_context(|| {
            format!(
                "failed while expanding upload src glob '{}' under {}",
                src,
                repo_root.display()
            )
        })?;
        if !local_path.is_file() {
            continue;
        }
        let relative = local_path
            .strip_prefix(&fixed_prefix_root)
            .with_context(|| {
                format!(
                    "upload src glob '{}' produced '{}' outside fixed prefix '{}'",
                    src,
                    local_path.display(),
                    fixed_prefix_root.display()
                )
            })?;
        let guest_dest = Path::new(dest)
            .join(relative)
            .to_string_lossy()
            .into_owned();
        mappings.push(UploadMapping {
            local_path,
            guest_dest,
        });
    }

    if mappings.is_empty() {
        anyhow::bail!(
            "no files matched upload src glob '{}' under {}",
            src,
            repo_root.display()
        );
    }

    Ok(mappings)
}

fn fixed_glob_prefix(src: &str) -> PathBuf {
    let mut prefix = PathBuf::new();
    for component in Path::new(src).components() {
        let std::path::Component::Normal(part) = component else {
            break;
        };
        if src_has_glob_metacharacters(&part.to_string_lossy()) {
            break;
        }
        prefix.push(part);
    }
    prefix
}

fn validate_top_level_upload_src_for_runtime(src: &str) -> Result<()> {
    let path = Path::new(src);
    if path.as_os_str().is_empty() {
        anyhow::bail!("upload src must not be empty");
    }
    if path.is_absolute() {
        anyhow::bail!("upload src must be repo-relative, got: {}", path.display());
    }
    for component in path.components() {
        match component {
            std::path::Component::Normal(_) => {}
            _ => anyhow::bail!(
                "upload src must contain no '.' or '..' segments: {}",
                path.display()
            ),
        }
    }
    Ok(())
}

/// Stage a local file to the guest using `sudo install`, applying the given `opts`.
///
/// Uses `sudo install` so it can set mode, owner, and group atomically.  The temporary file
/// is always cleaned up after the install attempt.
fn install_file_to_guest(
    ssh: &SshOptions,
    local_blob: &Path,
    dest: &str,
    opts: &InstallOpts<'_>,
    temp_label: &str,
) -> Result<()> {
    let dest_q = shell_single_quote(dest);
    let suffix = unique_suffix();
    let remote_tmp = format!("/tmp/botforge-upload-{temp_label}-{suffix}");
    let remote_tmp_q = shell_single_quote(&remote_tmp);

    scp_with_retry(
        ssh,
        local_blob,
        &remote_tmp,
        TRANSPORT_RETRIES,
        TRANSPORT_RETRY_DELAY,
    )
    .with_context(|| format!("failed to scp file to guest for install to '{dest}'"))?;

    // If overwrite is disabled, precheck that dest does not exist.
    let overwrite = opts.overwrite.unwrap_or(true);
    if !overwrite {
        let precheck_result = ssh_with_retry(
            ssh,
            &format!("sudo test ! -e {dest_q}"),
            1,
            Duration::from_secs(0),
            Duration::from_secs(30),
        );
        if precheck_result.is_err() {
            let _ = ssh_with_retry(
                ssh,
                &format!("rm -f {remote_tmp_q}"),
                1,
                Duration::from_secs(0),
                Duration::from_secs(10),
            );
            anyhow::bail!("upload dest '{dest}' already exists and overwrite is false");
        }
    }

    let mode = opts.mode.unwrap_or("0644");
    let owner = opts.owner.unwrap_or("root");
    let group = opts.group.unwrap_or("root");
    let parents = opts.parents.unwrap_or(true);
    let owner_q = shell_single_quote(owner);
    let group_q = shell_single_quote(group);

    let install_cmd = if parents {
        format!("sudo install -D -m {mode} -o {owner_q} -g {group_q} {remote_tmp_q} {dest_q}")
    } else {
        format!("sudo install -m {mode} -o {owner_q} -g {group_q} {remote_tmp_q} {dest_q}")
    };

    let install_result = ssh_with_retry(
        ssh,
        &install_cmd,
        1,
        Duration::from_secs(0),
        Duration::from_secs(30),
    )
    .with_context(|| format!("failed to install file to '{dest}' in guest"));

    // Best-effort cleanup of the temp file regardless of install success/failure.
    let _ = ssh_with_retry(
        ssh,
        &format!("rm -f {remote_tmp_q}"),
        1,
        Duration::from_secs(0),
        Duration::from_secs(10),
    );

    install_result
}

pub(crate) fn stage_top_level_uploads(
    repo_root: &Path,
    uploads: &[TopLevelUpload],
    ssh: &SshOptions,
) -> Result<()> {
    for (upload_idx, upload) in uploads.iter().enumerate() {
        let mappings = resolve_top_level_upload_mappings(repo_root, upload)?;
        let opts = InstallOpts {
            mode: upload.mode.as_deref(),
            owner: upload.owner.as_deref(),
            group: upload.group.as_deref(),
            overwrite: upload.overwrite,
            parents: upload.parents,
        };
        for (mapping_idx, mapping) in mappings.iter().enumerate() {
            install_file_to_guest(
                ssh,
                &mapping.local_path,
                &mapping.guest_dest,
                &opts,
                &format!("{upload_idx}-{mapping_idx}"),
            )
            .with_context(|| {
                format!(
                    "top-level upload '{}' failed while staging '{}' to '{}'",
                    upload.src,
                    mapping.local_path.display(),
                    mapping.guest_dest
                )
            })?;
            println!(
                "top-level upload {} staged {} -> {}",
                upload_idx + 1,
                mapping.local_path.display(),
                mapping.guest_dest
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{resolve_top_level_upload_mappings, TopLevelUpload, UploadMapping};

    #[test]
    fn test_resolve_top_level_upload_mappings_preserves_glob_relative_paths() {
        let repo = tempfile::tempdir().unwrap();
        let ecds = repo.path().join("images/botspace/envoy/ecds");
        std::fs::create_dir_all(&ecds).unwrap();
        let file = ecds.join("ext_authz.yaml");
        std::fs::write(&file, "kind: envoy\n").unwrap();

        let mappings = resolve_top_level_upload_mappings(
            repo.path(),
            &TopLevelUpload {
                src: "images/botspace/envoy/**/*.yaml".to_string(),
                dest: "/tmp/bake-staging/envoy/".to_string(),
                ..Default::default()
            },
        )
        .unwrap();

        assert_eq!(
            mappings,
            vec![UploadMapping {
                local_path: file,
                guest_dest: "/tmp/bake-staging/envoy/ecds/ext_authz.yaml".to_string(),
            }]
        );
    }

    #[test]
    fn test_resolve_top_level_upload_mappings_preserves_flat_glob_matches() {
        let repo = tempfile::tempdir().unwrap();
        let payload = repo.path().join("build/images/payload");
        std::fs::create_dir_all(&payload).unwrap();
        let file = payload.join("mcp-fs.tar");
        std::fs::write(&file, "tarball").unwrap();

        let mappings = resolve_top_level_upload_mappings(
            repo.path(),
            &TopLevelUpload {
                src: "build/images/payload/*.tar".to_string(),
                dest: "/usr/share/botwork/images/".to_string(),
                ..Default::default()
            },
        )
        .unwrap();

        assert_eq!(
            mappings,
            vec![UploadMapping {
                local_path: file,
                guest_dest: "/usr/share/botwork/images/mcp-fs.tar".to_string(),
            }]
        );
    }

    #[test]
    fn test_resolve_top_level_upload_mappings_literal_dest_directory_uses_basename() {
        let repo = tempfile::tempdir().unwrap();
        let local = repo.path().join("scripts/setup.sh");
        std::fs::create_dir_all(local.parent().unwrap()).unwrap();
        std::fs::write(&local, "#!/bin/sh\n").unwrap();

        let mappings = resolve_top_level_upload_mappings(
            repo.path(),
            &TopLevelUpload {
                src: "scripts/setup.sh".to_string(),
                dest: "/tmp/staging/".to_string(),
                ..Default::default()
            },
        )
        .unwrap();

        assert_eq!(
            mappings,
            vec![UploadMapping {
                local_path: local,
                guest_dest: "/tmp/staging/setup.sh".to_string(),
            }]
        );
    }

    #[test]
    fn test_resolve_top_level_upload_mappings_zero_match_is_error() {
        let repo = tempfile::tempdir().unwrap();
        let err = resolve_top_level_upload_mappings(
            repo.path(),
            &TopLevelUpload {
                src: "images/**/*.yaml".to_string(),
                dest: "/tmp/staging/".to_string(),
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("no files matched"));
    }

    #[test]
    fn test_resolve_top_level_upload_mappings_rejects_traversal() {
        let repo = tempfile::tempdir().unwrap();
        let err = resolve_top_level_upload_mappings(
            repo.path(),
            &TopLevelUpload {
                src: "images/../secret.txt".to_string(),
                dest: "/tmp/staging/".to_string(),
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains(".."));
    }

    #[test]
    fn test_resolve_top_level_upload_mappings_skips_directories_and_requires_files() {
        let repo = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(repo.path().join("images/botspace/envoy/ecds")).unwrap();
        let err = resolve_top_level_upload_mappings(
            repo.path(),
            &TopLevelUpload {
                src: "images/botspace/envoy/**".to_string(),
                dest: "/tmp/staging/".to_string(),
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("no files matched"));
    }
}
