use anyhow::{bail, Context, Result};
use hadris_iso::joliet::JolietLevel;
use hadris_iso::read::PathSeparator;
use hadris_iso::rrip::RripOptions;
use hadris_iso::write::options::{BaseIsoLevel, CreationFeatures, FormatOptions};
use hadris_iso::write::{InputFiles, IsoImageWriter};
#[cfg(test)]
use serde::{Deserialize, Serialize};
use serde_yaml::Value;
use std::io::Read;
use std::io::{Seek, Write};
use std::path::{Path, PathBuf};

use crate::util::unique_suffix;

/// A single entry in a cloud-init `bootcmd:` list.
///
/// Used internally for tests only.  Cloud-init supports two forms for each entry:
/// - A plain string, which it passes to `sh -c`.
/// - A sequence of strings (argv/exec form), which cloud-init passes directly
///   to `execvp`.  This is useful for `cloud-init-per` invocations such as
///   `[ cloud-init-per, once, mask-stack, sh, -c, "systemctl mask …" ]`.
///
/// `#[serde(untagged)]` lets serde pick the right variant by structure: a YAML
/// scalar becomes [`BootcmdEntry::Shell`] and a YAML sequence becomes
/// [`BootcmdEntry::Exec`].
#[cfg(test)]
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(untagged)]
pub(crate) enum BootcmdEntry {
    /// A shell string passed to `sh -c` by cloud-init.
    Shell(String),
    /// An argv list executed directly by cloud-init (exec form).
    Exec(Vec<String>),
}

const USER_DATA_PLACEHOLDER: &str = "REPLACE_WITH_SSH_PUBLIC_KEY";

/// Generate a per-run ephemeral installer username of the form `botforge-<20 hex chars>`.
///
/// The suffix is sourced from `/dev/urandom` (80 bits of entropy, effectively zero collision
/// probability) with a time+pid mix as fallback. The result is always a valid Linux username:
/// lowercase, starts with a letter, `[a-z0-9-]`, total length 29 (well within the 32-char limit).
pub(crate) fn generate_installer_username() -> String {
    let mut bytes = [0u8; 10]; // 10 bytes → 20 hex chars
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        let _ = f.read_exact(&mut bytes);
    } else {
        // Fallback: mix nanosecond timestamp and process ID
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let pid = std::process::id() as u128;
        let mix = nanos ^ (pid << 32) ^ (pid >> 32);
        for (i, b) in bytes.iter_mut().enumerate() {
            *b = ((mix >> (i * 8)) & 0xff) as u8;
        }
    }
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!("botforge-{hex}")
}

pub(crate) fn read_ssh_public_key(
    ssh_public_key: Option<String>,
    ssh_public_key_file: Option<PathBuf>,
) -> Result<Option<String>> {
    match (ssh_public_key, ssh_public_key_file) {
        (Some(_), Some(_)) => {
            bail!("provide only one of --ssh-public-key or --ssh-public-key-file")
        }
        (Some(key), None) => Ok(Some(key.trim().to_string())),
        (None, Some(path)) => {
            let key = std::fs::read_to_string(&path)
                .with_context(|| format!("cannot read SSH public key file: {}", path.display()))?;
            Ok(Some(key.trim().to_string()))
        }
        (None, None) => Ok(None),
    }
}

/// Render the cloud-init `#cloud-config` user-data for a runner VM seed.
///
/// When `template` is provided it replaces the [`USER_DATA_PLACEHOLDER`] with
/// `ssh_public_key` verbatim (legacy path, `cloud_init` is ignored).
///
/// Otherwise a base cloud-config mapping is built from the installer identity
/// and the optional `cloud_init` fragment is **deep-merged** into it:
///
/// - **`users:`** — botforge's installer entry always survives; user-supplied
///   `users:` entries are appended after the installer (installer-survives invariant).
/// - **List-valued keys** (`runcmd`, `bootcmd`, `write_files`, `packages`,
///   `mounts`, …) — botforge entries first, then user entries (concatenated).
/// - **Scalar / mapping keys** — the user fragment wins, except for keys that
///   would lock the harness out (those are validated at config load time by
///   [`crate::config::validate_cloud_init_fragment`] and rejected before
///   reaching this function).
pub(crate) fn render_user_data(
    template: Option<&str>,
    ssh_public_key: &str,
    ssh_user: Option<&str>,
    cloud_init: Option<&serde_yaml::Mapping>,
) -> String {
    if let Some(template) = template {
        return template.replace(USER_DATA_PLACEHOLDER, ssh_public_key);
    }
    // Build the botforge base cloud-config as a structured mapping.
    let mut base_map = serde_yaml::Mapping::new();
    if let Some(user) = ssh_user {
        // The named user is always a botforge-owned ephemeral installer: it needs
        // passwordless sudo to run provisioner steps and cloud-init waits, a
        // login shell for SSH execution, and key-only (locked password) access.
        let installer_entry: Value = serde_yaml::from_str(&format!(
            "name: {user}\nshell: /bin/bash\nlock_passwd: true\nsudo: 'ALL=(ALL) NOPASSWD:ALL'\nssh_authorized_keys:\n  - {ssh_public_key}\n"
        ))
        .expect("installer entry is always valid YAML");
        base_map.insert(
            Value::String("users".to_string()),
            Value::Sequence(vec![Value::String("default".to_string()), installer_entry]),
        );
    } else {
        base_map.insert(
            Value::String("ssh_authorized_keys".to_string()),
            Value::Sequence(vec![Value::String(ssh_public_key.to_string())]),
        );
    }

    // Deep-merge the user's cloud_init fragment into the base.
    let merged = if let Some(overlay) = cloud_init {
        deep_merge_cloud_config(base_map, overlay.clone())
    } else {
        base_map
    };

    let yaml = serde_yaml::to_string(&Value::Mapping(merged))
        .expect("merged cloud-config mapping is always serializable");
    format!("#cloud-config\n{yaml}")
}

/// Recursively deep-merge `overlay` into `base`, applying botforge merge semantics:
///
/// - **Sequences** (including `users:`, `runcmd:`, `bootcmd:`, `packages:`, …):
///   concatenate with `base` entries first ("botforge-first").  For `users:` this
///   guarantees the installer user is always present at the front.
/// - **Mappings**: recurse.
/// - **Scalars**: overlay value wins.
fn deep_merge_cloud_config(
    base: serde_yaml::Mapping,
    overlay: serde_yaml::Mapping,
) -> serde_yaml::Mapping {
    let mut result = base;
    for (key, overlay_val) in overlay {
        match result.get_mut(&key) {
            None => {
                result.insert(key, overlay_val);
            }
            Some(base_val) => match (base_val, overlay_val) {
                (Value::Sequence(base_seq), Value::Sequence(overlay_seq)) => {
                    // List: botforge-first concatenation.
                    base_seq.extend(overlay_seq);
                }
                (Value::Mapping(base_map), Value::Mapping(overlay_map)) => {
                    // Mapping: recurse.
                    *base_map = deep_merge_cloud_config(base_map.clone(), overlay_map);
                }
                (base_val, overlay_val) => {
                    // Scalar: overlay wins.
                    *base_val = overlay_val;
                }
            },
        }
    }
    result
}

pub(crate) fn write_seed_files(seed_dir: &Path, user_data: &str) -> Result<()> {
    std::fs::create_dir_all(seed_dir)
        .with_context(|| format!("cannot create seed dir: {}", seed_dir.display()))?;
    let instance_id = format!("iid-{}", unique_suffix());
    std::fs::write(
        seed_dir.join("meta-data"),
        format!("instance-id: {instance_id}\nlocal-hostname: botforge\n"),
    )
    .with_context(|| format!("cannot write seed meta-data in {}", seed_dir.display()))?;
    std::fs::write(seed_dir.join("user-data"), user_data)
        .with_context(|| format!("cannot write seed user-data in {}", seed_dir.display()))?;
    Ok(())
}

/// Prepare the cloud-init seed ISO: write seed files, build the ISO, then remove the
/// temporary seed directory.  Brackets the work with `(setup)` phase markers on stderr.
pub(crate) fn prepare_seed_image(seed_dir: &Path, seed_iso: &Path, user_data: &str) -> Result<()> {
    crate::plan::print_phase("setup", "Preparing environment (seed image)");
    write_seed_files(seed_dir, user_data)?;
    build_iso(seed_dir, seed_iso, "cidata")?;
    std::fs::remove_dir_all(seed_dir)
        .with_context(|| format!("cannot remove temp seed dir: {}", seed_dir.display()))?;
    crate::plan::print_phase_status("setup", "Preparing environment (seed image)", true, None);
    Ok(())
}

pub(crate) fn build_iso(src_dir: &Path, out: &Path, volume_id: &str) -> Result<()> {
    if !src_dir.is_dir() {
        bail!("source directory does not exist: {}", src_dir.display());
    }
    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("cannot create output dir: {}", parent.display()))?;
    }

    let file_name = out
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("out.iso");
    let tmp_out = out
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(format!(".{file_name}.{}.tmp", unique_suffix()));

    let result = build_iso_to_path(src_dir, &tmp_out, volume_id);
    if let Err(err) = result {
        let _ = std::fs::remove_file(&tmp_out);
        return Err(err);
    }

    if out.exists() {
        std::fs::remove_file(out)
            .with_context(|| format!("cannot replace output file: {}", out.display()))?;
    }
    std::fs::rename(&tmp_out, out).with_context(|| {
        format!(
            "cannot atomically materialize output from {} to {}",
            tmp_out.display(),
            out.display()
        )
    })?;
    Ok(())
}

fn build_iso_to_path(src_dir: &Path, out: &Path, volume_id: &str) -> Result<()> {
    let input_files = InputFiles::from_fs(src_dir, PathSeparator::ForwardSlash)
        .with_context(|| format!("cannot read source directory: {}", src_dir.display()))?;

    let options = FormatOptions {
        volume_name: volume_id.to_string(),
        system_id: None,
        volume_set_id: None,
        publisher_id: None,
        preparer_id: None,
        application_id: None,
        sector_size: 2048,
        path_separator: PathSeparator::ForwardSlash,
        features: CreationFeatures {
            filenames: BaseIsoLevel::Level1 {
                supports_lowercase: false,
                supports_rrip: true,
            },
            long_filenames: false,
            joliet: Some(JolietLevel::Level3),
            rock_ridge: Some(RripOptions::default()),
            el_torito: None,
            hybrid_boot: None,
        },
        strict_charset: false,
    };

    let estimated = hadris_iso::write::estimator::estimate(&input_files, &options).minimum_bytes();

    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(out)
        .with_context(|| format!("cannot create output file: {}", out.display()))?;

    if estimated > 0 {
        file.seek(std::io::SeekFrom::Start(estimated as u64 - 1))
            .and_then(|_| file.write_all(&[0u8]))
            .and_then(|_| file.seek(std::io::SeekFrom::Start(0)))
            .with_context(|| format!("cannot pre-allocate output file: {}", out.display()))?;
    }

    IsoImageWriter::format_new(&mut file, input_files, options)
        .with_context(|| format!("ISO build failed writing to {}", out.display()))?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        build_iso, generate_installer_username, render_user_data, write_seed_files, BootcmdEntry,
    };
    use tempfile::TempDir;

    /// Build a `serde_yaml::Mapping` with a single `bootcmd:` key containing the
    /// supplied entries.  Convenience helper used by several tests that exercise
    /// the cloud_init parameter path.
    fn bootcmd_mapping(entries: &[BootcmdEntry]) -> serde_yaml::Mapping {
        let mut m = serde_yaml::Mapping::new();
        let seq: serde_yaml::Value = serde_yaml::to_value(entries).unwrap();
        m.insert(serde_yaml::Value::String("bootcmd".to_string()), seq);
        m
    }

    #[test]
    fn render_user_data_replaces_placeholder() {
        let template = "#cloud-config\nssh_authorized_keys:\n  - REPLACE_WITH_SSH_PUBLIC_KEY\n";
        let rendered = render_user_data(Some(template), "ssh-ed25519 AAAA test", None, None);
        assert!(rendered.contains("ssh-ed25519 AAAA test"));
        assert!(!rendered.contains("REPLACE_WITH_SSH_PUBLIC_KEY"));
    }

    #[test]
    fn render_user_data_no_user_emits_top_level_key() {
        let rendered = render_user_data(None, "ssh-ed25519 AAAA nouser", None, None);
        assert!(rendered.contains("ssh_authorized_keys:"));
        assert!(rendered.contains("ssh-ed25519 AAAA nouser"));
        assert!(!rendered.contains("users:"));
        assert!(!rendered.contains("sudo:"));
    }

    #[test]
    fn render_user_data_installer_has_sudo_key_and_lock_passwd() {
        let rendered = render_user_data(
            None,
            "ssh-ed25519 AAAA installer",
            Some("botforge-abc123"),
            None,
        );
        // Must include the installer user entry
        assert!(
            rendered.contains("name: botforge-abc123"),
            "missing user name: {rendered}"
        );
        // Must grant passwordless sudo — check via parsed YAML to be quoting-agnostic.
        let parsed: serde_yaml::Value =
            serde_yaml::from_str(&rendered).expect("output must be valid YAML");
        let users = parsed["users"]
            .as_sequence()
            .expect("users must be a sequence");
        let installer = users
            .iter()
            .find(|u| u["name"].as_str() == Some("botforge-abc123"))
            .expect("installer user must be present");
        assert_eq!(
            installer["sudo"].as_str(),
            Some("ALL=(ALL) NOPASSWD:ALL"),
            "installer must have passwordless sudo: {rendered}"
        );
        // Must carry the harness ephemeral SSH public key
        assert!(
            rendered.contains("ssh-ed25519 AAAA installer"),
            "missing ssh key: {rendered}"
        );
        // Must lock the password (key-only access)
        assert!(
            rendered.contains("lock_passwd: true"),
            "missing lock_passwd: {rendered}"
        );
        // Must preserve the default user
        assert!(
            rendered.contains("- default"),
            "missing default: {rendered}"
        );
    }

    // -----------------------------------------------------------------
    // cloud_init / bootcmd tests
    // Baseline test is a semantic YAML equality check: the structural builder
    // may reorder keys compared to the old format! string, so we parse both
    // sides and compare the YAML values rather than bytes.
    // -----------------------------------------------------------------

    #[test]
    fn render_user_data_without_cloud_init_matches_baseline_semantically() {
        // No cloud_init supplied must produce the same cloud-config content as the
        // old hard-coded format! string.  Parsed-YAML equality (not byte identity)
        // because key ordering may differ with a structural builder.
        let rendered = render_user_data(None, "ssh-ed25519 AAAA key", Some("botforge-abc"), None);
        let baseline = "#cloud-config\nusers:\n  - default\n  - name: botforge-abc\n    shell: /bin/bash\n    lock_passwd: true\n    sudo: 'ALL=(ALL) NOPASSWD:ALL'\n    ssh_authorized_keys:\n      - ssh-ed25519 AAAA key\n";
        let rendered_val: serde_yaml::Value =
            serde_yaml::from_str(&rendered).expect("rendered must be valid YAML");
        let baseline_val: serde_yaml::Value =
            serde_yaml::from_str(baseline).expect("baseline must be valid YAML");
        assert_eq!(
            rendered_val, baseline_val,
            "no-cloud_init output must be semantically identical to the baseline"
        );
    }

    #[test]
    fn render_user_data_no_bootcmd_key_when_cloud_init_absent() {
        // Verify the no-user variant also produces no `bootcmd:` key when cloud_init is None.
        let rendered = render_user_data(None, "ssh-ed25519 AAAA key", None, None);
        assert!(
            !rendered.contains("bootcmd"),
            "no bootcmd: key expected when cloud_init is absent: {rendered}"
        );
    }

    #[test]
    fn render_user_data_single_string_bootcmd_entry() {
        let entries = vec![BootcmdEntry::Shell("echo hello world".to_string())];
        let ci = bootcmd_mapping(&entries);
        let rendered = render_user_data(
            None,
            "ssh-ed25519 AAAA key",
            Some("botforge-abc"),
            Some(&ci),
        );

        // Must contain the bootcmd: key
        assert!(rendered.contains("bootcmd:"), "missing bootcmd: {rendered}");
        // Must contain the shell string
        assert!(
            rendered.contains("echo hello world"),
            "missing shell entry: {rendered}"
        );
        // Must still contain the installer user (botforge content preserved)
        assert!(
            rendered.contains("name: botforge-abc"),
            "installer user must be preserved: {rendered}"
        );
        // Output must parse as valid YAML and round-trip the entry
        let parsed: serde_yaml::Value =
            serde_yaml::from_str(&rendered).expect("output must be valid YAML");
        let bootcmd = parsed["bootcmd"]
            .as_sequence()
            .expect("bootcmd must be a sequence");
        assert_eq!(bootcmd.len(), 1);
        assert_eq!(bootcmd[0].as_str(), Some("echo hello world"));
    }

    #[test]
    fn render_user_data_mixed_bootcmd_entries() {
        let entries = vec![
            BootcmdEntry::Shell("echo first".to_string()),
            BootcmdEntry::Exec(vec![
                "cloud-init-per".to_string(),
                "once".to_string(),
                "mask-stack".to_string(),
                "sh".to_string(),
                "-c".to_string(),
                "systemctl mask a.service b.service".to_string(),
            ]),
        ];
        let ci = bootcmd_mapping(&entries);
        let rendered = render_user_data(
            None,
            "ssh-ed25519 AAAA key",
            Some("botforge-abc"),
            Some(&ci),
        );

        // Must parse as valid YAML
        let parsed: serde_yaml::Value =
            serde_yaml::from_str(&rendered).expect("output must be valid YAML");
        let bootcmd = parsed["bootcmd"]
            .as_sequence()
            .expect("bootcmd must be a sequence");
        assert_eq!(bootcmd.len(), 2);

        // First entry: plain string
        assert_eq!(
            bootcmd[0].as_str(),
            Some("echo first"),
            "first entry must be a plain string"
        );

        // Second entry: sequence (exec form)
        let exec = bootcmd[1]
            .as_sequence()
            .expect("second entry must be a sequence");
        assert_eq!(exec[0].as_str(), Some("cloud-init-per"));
        assert_eq!(exec[1].as_str(), Some("once"));
        assert_eq!(exec[5].as_str(), Some("systemctl mask a.service b.service"));

        // Installer user must still be present
        assert!(
            rendered.contains("name: botforge-abc"),
            "installer user must be preserved: {rendered}"
        );
    }

    #[test]
    fn render_user_data_bootcmd_with_special_chars_is_valid_yaml() {
        // Strings with colons, quotes, etc. must be properly escaped.
        let entries = vec![BootcmdEntry::Shell(
            "sh -c 'systemctl mask a.service: echo done'".to_string(),
        )];
        let ci = bootcmd_mapping(&entries);
        let rendered = render_user_data(None, "ssh-ed25519 AAAA key", None, Some(&ci));
        // Must parse without error and round-trip the entry
        let parsed: serde_yaml::Value = serde_yaml::from_str(&rendered)
            .expect("output must be valid YAML even with special chars");
        let bootcmd = parsed["bootcmd"]
            .as_sequence()
            .expect("bootcmd must be a sequence");
        assert_eq!(bootcmd.len(), 1);
        assert!(
            bootcmd[0].as_str().is_some(),
            "entry must round-trip as a string"
        );
    }

    #[test]
    fn render_user_data_bootcmd_merges_with_no_user_variant() {
        // The caller-supplied-user path (ssh_user: None) must keep the top-level
        // ssh_authorized_keys AND merge a valid bootcmd block.
        let entries = vec![BootcmdEntry::Shell(
            "systemctl mask botwork-api.service".to_string(),
        )];
        let ci = bootcmd_mapping(&entries);
        let rendered = render_user_data(None, "ssh-ed25519 AAAA nouser", None, Some(&ci));

        // Top-level SSH key must be preserved (no `users:` block in this variant).
        assert!(
            rendered.contains("ssh_authorized_keys:"),
            "top-level ssh_authorized_keys must be preserved: {rendered}"
        );
        assert!(
            rendered.contains("ssh-ed25519 AAAA nouser"),
            "ssh key must be preserved: {rendered}"
        );
        assert!(
            !rendered.contains("users:"),
            "no-user variant must not emit a users: block: {rendered}"
        );

        // The whole document must remain a single valid #cloud-config with both
        // the ssh key and the bootcmd entry.
        assert!(
            rendered.starts_with("#cloud-config\n"),
            "must be a single cloud-config doc: {rendered}"
        );
        let parsed: serde_yaml::Value =
            serde_yaml::from_str(&rendered).expect("output must be valid YAML");
        let bootcmd = parsed["bootcmd"]
            .as_sequence()
            .expect("bootcmd must be a sequence");
        assert_eq!(bootcmd.len(), 1);
        assert_eq!(
            bootcmd[0].as_str(),
            Some("systemctl mask botwork-api.service")
        );
        // ssh_authorized_keys must also survive in the parsed document.
        assert!(
            parsed["ssh_authorized_keys"].is_sequence(),
            "ssh_authorized_keys must remain a sequence in the merged doc: {rendered}"
        );
    }

    #[test]
    fn render_user_data_multiple_string_bootcmd_entries_preserve_order() {
        // Multiple shell entries must appear in the rendered sequence in the same
        // order they were supplied.
        let entries = vec![
            BootcmdEntry::Shell("echo one".to_string()),
            BootcmdEntry::Shell("echo two".to_string()),
            BootcmdEntry::Shell("echo three".to_string()),
        ];
        let ci = bootcmd_mapping(&entries);
        let rendered = render_user_data(
            None,
            "ssh-ed25519 AAAA key",
            Some("botforge-abc"),
            Some(&ci),
        );
        let parsed: serde_yaml::Value =
            serde_yaml::from_str(&rendered).expect("output must be valid YAML");
        let bootcmd = parsed["bootcmd"]
            .as_sequence()
            .expect("bootcmd must be a sequence");
        let order: Vec<Option<&str>> = bootcmd.iter().map(|v| v.as_str()).collect();
        assert_eq!(
            order,
            vec![Some("echo one"), Some("echo two"), Some("echo three")],
            "bootcmd entries must preserve their supplied order"
        );
    }

    #[test]
    fn bootcmd_entry_serialize_round_trips_both_forms() {
        let shell = BootcmdEntry::Shell("echo hi".to_string());
        let shell_yaml = serde_yaml::to_string(&shell).expect("shell entry must serialize");
        let shell_back: serde_yaml::Value =
            serde_yaml::from_str(&shell_yaml).expect("shell entry must reparse");
        assert_eq!(shell_back.as_str(), Some("echo hi"));

        let exec = BootcmdEntry::Exec(vec![
            "cloud-init-per".to_string(),
            "once".to_string(),
            "mask".to_string(),
        ]);
        let exec_yaml = serde_yaml::to_string(&exec).expect("exec entry must serialize");
        let exec_back: serde_yaml::Value =
            serde_yaml::from_str(&exec_yaml).expect("exec entry must reparse");
        let seq = exec_back
            .as_sequence()
            .expect("exec entry reparses as a sequence");
        let flat: Vec<Option<&str>> = seq.iter().map(|v| v.as_str()).collect();
        assert_eq!(
            flat,
            vec![Some("cloud-init-per"), Some("once"), Some("mask")]
        );
    }

    #[test]
    fn render_user_data_bootcmd_reaches_seed_for_both_identity_paths() {
        // cloud_init bootcmd must land as a valid `bootcmd:` block in BOTH
        // identity arms (installer user + caller-supplied).
        let entries = vec![
            BootcmdEntry::Shell("echo start".to_string()),
            BootcmdEntry::Exec(vec![
                "cloud-init-per".to_string(),
                "once".to_string(),
                "mask".to_string(),
                "sh".to_string(),
                "-c".to_string(),
                "systemctl mask botwork-api.service".to_string(),
            ]),
        ];
        let ci = bootcmd_mapping(&entries);

        // botforge-owned arm: installer user + bootcmd both present.
        let owned = render_user_data(None, "ssh-ed25519 AAAA k", Some("botforge-xyz"), Some(&ci));
        let owned_parsed: serde_yaml::Value =
            serde_yaml::from_str(&owned).expect("owned user-data must be valid YAML");
        assert!(
            owned.contains("name: botforge-xyz"),
            "installer user must be present in owned arm: {owned}"
        );
        assert_eq!(
            owned_parsed["bootcmd"]
                .as_sequence()
                .expect("owned bootcmd must be a sequence")
                .len(),
            2
        );

        // caller-supplied arm: top-level key + bootcmd both present.
        let supplied = render_user_data(None, "ssh-ed25519 AAAA k", None, Some(&ci));
        let supplied_parsed: serde_yaml::Value =
            serde_yaml::from_str(&supplied).expect("supplied user-data must be valid YAML");
        assert!(
            supplied_parsed["ssh_authorized_keys"].is_sequence(),
            "ssh_authorized_keys must be present in caller-supplied arm: {supplied}"
        );
        assert_eq!(
            supplied_parsed["bootcmd"]
                .as_sequence()
                .expect("supplied bootcmd must be a sequence")
                .len(),
            2
        );
    }

    #[test]
    fn bootcmd_entry_deserialize_shell_form() {
        let entry: BootcmdEntry = serde_yaml::from_str("echo hello").unwrap();
        assert_eq!(entry, BootcmdEntry::Shell("echo hello".to_string()));
    }

    #[test]
    fn bootcmd_entry_deserialize_exec_form() {
        let entry: BootcmdEntry =
            serde_yaml::from_str("- cloud-init-per\n- once\n- mask\n").unwrap();
        assert_eq!(
            entry,
            BootcmdEntry::Exec(vec![
                "cloud-init-per".to_string(),
                "once".to_string(),
                "mask".to_string()
            ])
        );
    }

    // -----------------------------------------------------------------
    // cloud_init deep-merge tests
    // -----------------------------------------------------------------

    #[test]
    fn render_user_data_cloud_init_users_appended_after_installer() {
        // User-supplied users must be appended AFTER the botforge installer; the
        // installer (name: botforge-test) must always appear first.
        let cloud_init_yaml =
            "users:\n  - name: alice\n    shell: /bin/bash\n    lock_passwd: true\n";
        let ci: serde_yaml::Mapping = serde_yaml::from_str(cloud_init_yaml).unwrap();
        let rendered = render_user_data(
            None,
            "ssh-ed25519 AAAA key",
            Some("botforge-test"),
            Some(&ci),
        );
        let parsed: serde_yaml::Value =
            serde_yaml::from_str(&rendered).expect("output must be valid YAML");
        let users = parsed["users"]
            .as_sequence()
            .expect("users must be a sequence");
        // Order: default, botforge-test, alice
        assert_eq!(
            users.len(),
            3,
            "expected 3 users: default + installer + alice"
        );
        assert_eq!(users[0].as_str(), Some("default"), "first must be default");
        assert_eq!(
            users[1]["name"].as_str(),
            Some("botforge-test"),
            "second must be installer"
        );
        assert_eq!(
            users[2]["name"].as_str(),
            Some("alice"),
            "third must be alice"
        );
    }

    #[test]
    fn render_user_data_cloud_init_packages_merged_botforge_first() {
        // packages: lists are concatenated with botforge entries first.
        // (Currently botforge emits no packages in its base, so user packages
        // appear as-is; the order invariant ensures future botforge packages
        // always precede user packages.)
        let cloud_init_yaml = "packages:\n  - curl\n  - git\n";
        let ci: serde_yaml::Mapping = serde_yaml::from_str(cloud_init_yaml).unwrap();
        let rendered = render_user_data(
            None,
            "ssh-ed25519 AAAA key",
            Some("botforge-abc"),
            Some(&ci),
        );
        let parsed: serde_yaml::Value =
            serde_yaml::from_str(&rendered).expect("output must be valid YAML");
        let pkgs = parsed["packages"]
            .as_sequence()
            .expect("packages must be a sequence");
        assert_eq!(pkgs.len(), 2);
        assert_eq!(pkgs[0].as_str(), Some("curl"));
        assert_eq!(pkgs[1].as_str(), Some("git"));
        // Installer user must still be present.
        assert!(
            rendered.contains("name: botforge-abc"),
            "installer must survive cloud_init merge: {rendered}"
        );
    }

    #[test]
    fn render_user_data_cloud_init_mounts_tmpfs_example() {
        // A motivating test-side example: tmpfs over /var/cache/apt and
        // /var/lib/apt/lists as a boot-time perf win (ephemeral on test).
        let cloud_init_yaml = "mounts:\n  - [tmpfs, /var/cache/apt, tmpfs, \"size=512M\", \"0\", \"0\"]\n  - [tmpfs, /var/lib/apt/lists, tmpfs, \"size=256M\", \"0\", \"0\"]\n";
        let ci: serde_yaml::Mapping = serde_yaml::from_str(cloud_init_yaml).unwrap();
        let rendered = render_user_data(
            None,
            "ssh-ed25519 AAAA key",
            Some("botforge-abc"),
            Some(&ci),
        );
        let parsed: serde_yaml::Value =
            serde_yaml::from_str(&rendered).expect("output must be valid YAML");
        let mounts = parsed["mounts"]
            .as_sequence()
            .expect("mounts must be a sequence");
        assert_eq!(mounts.len(), 2);
        assert_eq!(
            mounts[0]
                .as_sequence()
                .and_then(|s| s.get(1))
                .and_then(|v| v.as_str()),
            Some("/var/cache/apt")
        );
        assert_eq!(
            mounts[1]
                .as_sequence()
                .and_then(|s| s.get(1))
                .and_then(|v| v.as_str()),
            Some("/var/lib/apt/lists")
        );
        // Installer user must survive.
        assert!(
            rendered.contains("name: botforge-abc"),
            "installer must survive cloud_init merge: {rendered}"
        );
    }

    #[test]
    fn render_user_data_cloud_init_runcmd_appended() {
        // runcmd entries from cloud_init are appended after botforge's runcmd
        // entries (botforge-first). Botforge emits no runcmd in its base, so
        // user entries appear at the top of the list.
        let cloud_init_yaml = "runcmd:\n  - echo \"run after cloud-init\"\n";
        let ci: serde_yaml::Mapping = serde_yaml::from_str(cloud_init_yaml).unwrap();
        let rendered = render_user_data(None, "ssh-ed25519 AAAA key", None, Some(&ci));
        let parsed: serde_yaml::Value =
            serde_yaml::from_str(&rendered).expect("output must be valid YAML");
        let runcmd = parsed["runcmd"]
            .as_sequence()
            .expect("runcmd must be a sequence");
        assert_eq!(runcmd.len(), 1);
        assert_eq!(runcmd[0].as_str(), Some("echo \"run after cloud-init\""));
    }

    #[test]
    fn render_user_data_cloud_init_scalar_write_files_inline_content_allowed() {
        // write_files with inline content: is allowed (not an ingress path).
        let cloud_init_yaml =
            "write_files:\n  - path: /etc/myapp.conf\n    content: |\n      key=value\n";
        let ci: serde_yaml::Mapping = serde_yaml::from_str(cloud_init_yaml).unwrap();
        let rendered = render_user_data(None, "ssh-ed25519 AAAA key", None, Some(&ci));
        let parsed: serde_yaml::Value =
            serde_yaml::from_str(&rendered).expect("output must be valid YAML");
        let wf = parsed["write_files"]
            .as_sequence()
            .expect("write_files must be a sequence");
        assert_eq!(wf.len(), 1);
        assert_eq!(wf[0]["path"].as_str(), Some("/etc/myapp.conf"));
    }

    #[test]
    fn generate_installer_username_is_valid_linux_username() {
        let name = generate_installer_username();
        // Must start with "botforge-"
        assert!(
            name.starts_with("botforge-"),
            "username must start with botforge-: {name}"
        );
        // Must only contain [a-z0-9-]
        assert!(
            name.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'),
            "invalid chars in username: {name}"
        );
        // Must be within the Linux 32-char limit
        assert!(
            name.len() <= 32,
            "username exceeds 32-char Linux limit: {name} (len={})",
            name.len()
        );
        // suffix must be 20 hex chars (10 bytes)
        let suffix = &name["botforge-".len()..];
        assert_eq!(suffix.len(), 20, "suffix length must be 20: {suffix}");
        assert!(
            suffix.chars().all(|c| c.is_ascii_hexdigit()),
            "suffix must be hex: {suffix}"
        );
    }

    #[test]
    fn generate_installer_username_produces_unique_names() {
        // Two calls must produce different names (collision probability ~1/2^80)
        let a = generate_installer_username();
        let b = generate_installer_username();
        assert_ne!(a, b, "two calls produced identical installer usernames");
    }

    #[test]
    fn write_seed_files_produces_unique_instance_ids() {
        let first = TempDir::new().unwrap();
        let second = TempDir::new().unwrap();

        write_seed_files(first.path(), "#cloud-config\n").unwrap();
        write_seed_files(second.path(), "#cloud-config\n").unwrap();

        let first_meta = std::fs::read_to_string(first.path().join("meta-data")).unwrap();
        let second_meta = std::fs::read_to_string(second.path().join("meta-data")).unwrap();

        let first_instance_id = first_meta
            .lines()
            .find_map(|line| line.strip_prefix("instance-id: "))
            .unwrap();
        let second_instance_id = second_meta
            .lines()
            .find_map(|line| line.strip_prefix("instance-id: "))
            .unwrap();

        assert_ne!(
            first_instance_id, second_instance_id,
            "seed instance-id must be unique per write_seed_files call"
        );
        assert!(
            first_meta.contains("local-hostname: botforge"),
            "meta-data must preserve local-hostname"
        );
    }

    #[test]
    fn build_iso_cidata_joliet_label_is_lowercase() {
        // The cidata seed ISO must carry a Joliet volume label of exactly "cidata"
        // (case-sensitive) so cloud-init's NoCloud datasource can find it.
        let seed_dir = TempDir::new().unwrap();
        write_seed_files(seed_dir.path(), "#cloud-config\n").unwrap();

        let out_dir = TempDir::new().unwrap();
        let iso_path = out_dir.path().join("seed.iso");
        build_iso(seed_dir.path(), &iso_path, "cidata").unwrap();

        // Re-open the ISO and verify the Joliet SVD volume identifier is "cidata".
        let joliet_label = read_joliet_volume_label(&iso_path);
        assert_eq!(
            joliet_label.as_deref(),
            Some("cidata"),
            "Joliet volume label must be exactly 'cidata' (lowercase) for cloud-init"
        );
    }

    #[test]
    fn build_iso_seed_files_round_trip() {
        // Files written to the seed directory must be byte-identical when read back.
        let seed_dir = TempDir::new().unwrap();
        let user_data = "#cloud-config\nssh_authorized_keys:\n  - ssh-ed25519 AAAA test\n";
        write_seed_files(seed_dir.path(), user_data).unwrap();

        let out_dir = TempDir::new().unwrap();
        let iso_path = out_dir.path().join("seed.iso");
        build_iso(seed_dir.path(), &iso_path, "cidata").unwrap();

        // Read user-data and meta-data back from the ISO.
        let (ud, md) = read_iso_files(&iso_path, &["user-data", "meta-data"]);
        assert_eq!(
            String::from_utf8(ud).unwrap(),
            user_data,
            "user-data must round-trip byte-identically"
        );
        assert!(!md.is_empty(), "meta-data must be present in the ISO");
    }

    #[test]
    fn build_iso_custom_volume_label() {
        // Payload ISOs use a caller-supplied volume ID; verify it round-trips.
        let src_dir = TempDir::new().unwrap();
        std::fs::write(src_dir.path().join("payload.bin"), b"payload data").unwrap();
        std::fs::create_dir(src_dir.path().join("images")).unwrap();
        std::fs::write(src_dir.path().join("images").join("disk.img"), b"disk").unwrap();

        let out_dir = TempDir::new().unwrap();
        let iso_path = out_dir.path().join("payload.iso");
        build_iso(src_dir.path(), &iso_path, "botwork-payload").unwrap();

        let joliet_label = read_joliet_volume_label(&iso_path);
        assert_eq!(
            joliet_label.as_deref(),
            Some("botwork-payload"),
            "custom volume label must be preserved in Joliet SVD"
        );
    }

    /// Open the produced ISO and return the Joliet SVD volume identifier,
    /// decoded from UTF-16 BE.
    fn read_joliet_volume_label(iso_path: &std::path::Path) -> Option<String> {
        use hadris_iso::joliet::JolietLevel;
        use hadris_iso::read::IsoImage;
        use hadris_iso::volume::VolumeDescriptor;
        use std::io::Cursor;

        let data = std::fs::read(iso_path).expect("cannot read ISO");
        let image = IsoImage::open(Cursor::new(data)).expect("cannot open ISO");

        for vd in image.read_volume_descriptors() {
            if let Ok(VolumeDescriptor::Supplementary(svd)) = vd {
                // Check if this is a Joliet SVD (escape sequences match any Joliet level)
                if JolietLevel::from_escape_sequence(&svd.escape_sequences).is_some() {
                    // volume_identifier is a fixed 32-byte field stored as UTF-16 BE
                    let raw: &[u8] = svd.volume_identifier.as_bytes();
                    // Strip trailing UTF-16 BE spaces (0x00, 0x20)
                    let mut end = raw.len();
                    while end >= 2 && raw[end - 2] == 0x00 && raw[end - 1] == 0x20 {
                        end -= 2;
                    }
                    let trimmed = &raw[..end];
                    let chars: Vec<u16> = trimmed
                        .chunks_exact(2)
                        .map(|b| u16::from_be_bytes([b[0], b[1]]))
                        .collect();
                    return Some(String::from_utf16_lossy(&chars).to_string());
                }
            }
        }
        None
    }

    /// Open the ISO and return the raw byte contents of each named file
    /// (looked up case-insensitively via the best available directory tree).
    fn read_iso_files(iso_path: &std::path::Path, names: &[&str]) -> (Vec<u8>, Vec<u8>) {
        use hadris_iso::read::IsoImage;
        use std::io::Cursor;

        let data = std::fs::read(iso_path).expect("cannot read ISO");
        let image = IsoImage::open(Cursor::new(data)).expect("cannot open ISO");
        let root = image.root_dir();

        let mut result: Vec<Vec<u8>> = vec![Vec::new(); names.len()];
        for entry_result in root.iter(&image).entries() {
            let entry = entry_result.expect("dir entry read failed");
            if entry.is_special() {
                continue;
            }
            // Prefer RRIP display_name (lowercase) over raw ISO name (uppercased).
            let display = entry.display_name();
            let name_str = display.as_ref();
            // Strip version suffix (";1") which ISO 9660 may append.
            let name_clean = if let Some(pos) = name_str.rfind(';') {
                &name_str[..pos]
            } else {
                name_str
            };
            for (i, &want) in names.iter().enumerate() {
                if name_clean.eq_ignore_ascii_case(want) {
                    result[i] = image.read_file(&entry).expect("cannot read file");
                }
            }
        }
        (result.remove(0), result.remove(0))
    }
}
