use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::resolver::{Reference, ResolveFileContext};
use crate::ssh::{scp_with_retry, ssh_with_retry, SshOptions};
use crate::util::{shell_single_quote, unique_suffix};

const TRANSPORT_RETRIES: usize = 10;
const TRANSPORT_RETRY_DELAY: Duration = Duration::from_secs(2);

#[derive(Debug, Deserialize, Clone, PartialEq, Eq, Default)]
#[serde(deny_unknown_fields)]
pub(crate) struct FileEntry {
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
pub(crate) struct FileMapping {
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

pub(crate) fn resolve_file_mappings(
    file: &FileEntry,
    context: &ResolveFileContext<'_>,
) -> Result<Vec<FileMapping>> {
    let src = file.src.trim();
    let dest = file.dest.trim();
    let reference = Reference::parse(src)
        .with_context(|| format!("file src '{src}' is not a valid @-reference"))?;
    let resolved_files = reference
        .resolve_to_files(context)
        .with_context(|| format!("failed to resolve file src '{src}'"))?;
    let mut mappings = Vec::new();
    for rf in resolved_files {
        let guest_dest = if dest.ends_with('/') {
            Path::new(dest)
                .join(&rf.relative_path)
                .to_string_lossy()
                .into_owned()
        } else {
            dest.to_string()
        };
        mappings.push(FileMapping {
            local_path: rf.local_path,
            guest_dest,
        });
    }
    Ok(mappings)
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
            anyhow::bail!("file dest '{dest}' already exists and overwrite is false");
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

pub(crate) fn stage_files(
    files: &[FileEntry],
    context: &ResolveFileContext<'_>,
    ssh: &SshOptions,
) -> Result<()> {
    for (file_idx, file) in files.iter().enumerate() {
        let mappings = resolve_file_mappings(file, context)?;
        let opts = InstallOpts {
            mode: file.mode.as_deref(),
            owner: file.owner.as_deref(),
            group: file.group.as_deref(),
            overwrite: file.overwrite,
            parents: file.parents,
        };
        for (mapping_idx, mapping) in mappings.iter().enumerate() {
            install_file_to_guest(
                ssh,
                &mapping.local_path,
                &mapping.guest_dest,
                &opts,
                &format!("{file_idx}-{mapping_idx}"),
            )
            .with_context(|| {
                format!(
                    "file entry '{}' failed while staging '{}' to '{}'",
                    file.src,
                    mapping.local_path.display(),
                    mapping.guest_dest
                )
            })?;
            println!(
                "file {} staged {} -> {}",
                file_idx + 1,
                mapping.local_path.display(),
                mapping.guest_dest
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{resolve_file_mappings, FileEntry, FileMapping};
    use crate::resolver::ResolveFileContext;
    use tempfile::TempDir;

    fn make_context<'a>(
        repo: &'a TempDir,
        manifest: &'a std::path::Path,
    ) -> ResolveFileContext<'a> {
        ResolveFileContext {
            context: repo.path(),
            manifest_path: manifest,
            cache_dir_override: None,
        }
    }

    #[test]
    fn test_resolve_file_mappings_repo_glob_preserves_relative_paths() {
        let repo = TempDir::new().unwrap();
        let manifest = repo.path().join("shasset.yaml");
        let ecds = repo.path().join("images/botspace/envoy/ecds");
        std::fs::create_dir_all(&ecds).unwrap();
        let file = ecds.join("ext_authz.yaml");
        std::fs::write(&file, "kind: envoy\n").unwrap();

        let ctx = make_context(&repo, &manifest);
        let mappings = resolve_file_mappings(
            &FileEntry {
                src: "@://images/botspace/envoy/**/*.yaml".to_string(),
                dest: "/tmp/bake-staging/envoy/".to_string(),
                ..Default::default()
            },
            &ctx,
        )
        .unwrap();

        assert_eq!(
            mappings,
            vec![FileMapping {
                local_path: file.canonicalize().unwrap(),
                guest_dest: "/tmp/bake-staging/envoy/ecds/ext_authz.yaml".to_string(),
            }]
        );
    }

    #[test]
    fn test_resolve_file_mappings_artifact_glob_preserves_flat_matches() {
        let repo = TempDir::new().unwrap();
        let manifest = repo.path().join("shasset.yaml");
        let artifact_dir = repo.path().join("build/artifact/payload");
        std::fs::create_dir_all(&artifact_dir).unwrap();
        let file = artifact_dir.join("mcp-fs.tar");
        std::fs::write(&file, "tarball").unwrap();

        let ctx = make_context(&repo, &manifest);
        let mappings = resolve_file_mappings(
            &FileEntry {
                src: "@artifact://payload/*.tar".to_string(),
                dest: "/usr/share/botwork/images/".to_string(),
                ..Default::default()
            },
            &ctx,
        )
        .unwrap();

        assert_eq!(
            mappings,
            vec![FileMapping {
                local_path: file.canonicalize().unwrap(),
                guest_dest: "/usr/share/botwork/images/mcp-fs.tar".to_string(),
            }]
        );
    }

    #[test]
    fn test_resolve_file_mappings_repo_literal_dest_uses_relative_path() {
        let repo = TempDir::new().unwrap();
        let manifest = repo.path().join("shasset.yaml");
        let local = repo.path().join("scripts/setup.sh");
        std::fs::create_dir_all(local.parent().unwrap()).unwrap();
        std::fs::write(&local, "#!/bin/sh\n").unwrap();

        let ctx = make_context(&repo, &manifest);
        let mappings = resolve_file_mappings(
            &FileEntry {
                src: "@://scripts/setup.sh".to_string(),
                dest: "/tmp/staging/setup.sh".to_string(),
                ..Default::default()
            },
            &ctx,
        )
        .unwrap();

        assert_eq!(
            mappings,
            vec![FileMapping {
                local_path: local.canonicalize().unwrap(),
                guest_dest: "/tmp/staging/setup.sh".to_string(),
            }]
        );
    }

    #[test]
    fn test_resolve_file_mappings_repo_literal_dir_dest_appends_basename() {
        let repo = TempDir::new().unwrap();
        let manifest = repo.path().join("shasset.yaml");
        let local = repo.path().join("scripts/setup.sh");
        std::fs::create_dir_all(local.parent().unwrap()).unwrap();
        std::fs::write(&local, "#!/bin/sh\n").unwrap();

        let ctx = make_context(&repo, &manifest);
        let mappings = resolve_file_mappings(
            &FileEntry {
                src: "@://scripts/setup.sh".to_string(),
                dest: "/tmp/staging/".to_string(),
                ..Default::default()
            },
            &ctx,
        )
        .unwrap();

        assert_eq!(
            mappings,
            vec![FileMapping {
                local_path: local.canonicalize().unwrap(),
                guest_dest: "/tmp/staging/setup.sh".to_string(),
            }]
        );
    }

    #[test]
    fn test_resolve_file_mappings_zero_match_is_error() {
        let repo = TempDir::new().unwrap();
        let manifest = repo.path().join("shasset.yaml");
        let ctx = make_context(&repo, &manifest);
        let err = resolve_file_mappings(
            &FileEntry {
                src: "@://images/**/*.yaml".to_string(),
                dest: "/tmp/staging/".to_string(),
                ..Default::default()
            },
            &ctx,
        )
        .unwrap_err();
        assert!(
            format!("{err:#}").contains("zero")
                || format!("{err:#}").contains("no files")
                || format!("{err:#}").contains("matched"),
            "error should mention zero matches: {err:#}"
        );
    }

    #[test]
    fn test_resolve_file_mappings_rejects_bare_path() {
        let repo = TempDir::new().unwrap();
        let manifest = repo.path().join("shasset.yaml");
        let ctx = make_context(&repo, &manifest);
        let err = resolve_file_mappings(
            &FileEntry {
                src: "images/botspace/envoy/**/*.yaml".to_string(),
                dest: "/tmp/staging/".to_string(),
                ..Default::default()
            },
            &ctx,
        )
        .unwrap_err();
        assert!(
            format!("{err:#}").contains("@") || format!("{err:#}").contains("reference"),
            "error should mention @-reference: {err:#}"
        );
    }
}
