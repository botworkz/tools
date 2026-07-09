use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::io::Read;
use std::path::{Path, PathBuf};

use crate::util::{command_exists, run_command, unique_suffix};

/// A single entry in a cloud-init `bootcmd:` list.
///
/// Cloud-init supports two forms for each entry:
/// - A plain string, which it passes to `sh -c`.
/// - A sequence of strings (argv/exec form), which cloud-init passes directly
///   to `execvp`.  This is useful for `cloud-init-per` invocations such as
///   `[ cloud-init-per, once, mask-stack, sh, -c, "systemctl mask …" ]`.
///
/// `#[serde(untagged)]` lets serde pick the right variant by structure: a YAML
/// scalar becomes [`BootcmdEntry::Shell`] and a YAML sequence becomes
/// [`BootcmdEntry::Exec`].
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

pub(crate) fn render_user_data(
    template: Option<&str>,
    ssh_public_key: &str,
    ssh_user: Option<&str>,
    bootcmd: &[BootcmdEntry],
) -> String {
    if let Some(template) = template {
        return template.replace(USER_DATA_PLACEHOLDER, ssh_public_key);
    }
    let base = if let Some(user) = ssh_user {
        // The named user is always a botforge-owned ephemeral installer: it needs
        // passwordless sudo to run provisioner steps and cloud-init waits, a
        // login shell for SSH execution, and key-only (locked password) access.
        format!(
            "#cloud-config\nusers:\n  - default\n  - name: {user}\n    shell: /bin/bash\n    lock_passwd: true\n    sudo: 'ALL=(ALL) NOPASSWD:ALL'\n    ssh_authorized_keys:\n      - {ssh_public_key}\n"
        )
    } else {
        format!("#cloud-config\nssh_authorized_keys:\n  - {ssh_public_key}\n")
    };
    if bootcmd.is_empty() {
        return base;
    }
    let bootcmd_block = render_bootcmd_block(bootcmd);
    format!("{base}{bootcmd_block}")
}

/// Serialize a non-empty `bootcmd` entry list into a `bootcmd:` YAML block
/// suitable for appending to a `#cloud-config` document.
///
/// Uses `serde_yaml` so that string values are escaped/quoted correctly and
/// the output is guaranteed to round-trip through a standard YAML parser.
fn render_bootcmd_block(entries: &[BootcmdEntry]) -> String {
    debug_assert!(!entries.is_empty(), "caller must check non-empty");
    // A single-key map {"bootcmd": entries}. serde_yaml serialises a mapping
    // without a leading `---` document marker, so the result appends cleanly to
    // the existing cloud-config string. Serialising a map of
    // &str -> &[BootcmdEntry] (each a String or Vec<String>) is infallible,
    // hence the expect().
    let map = std::collections::BTreeMap::from([("bootcmd", entries)]);
    serde_yaml::to_string(&map).expect("bootcmd mapping is always serializable")
}

pub(crate) fn write_seed_files(seed_dir: &Path, user_data: &str) -> Result<()> {
    std::fs::create_dir_all(seed_dir)
        .with_context(|| format!("cannot create seed dir: {}", seed_dir.display()))?;
    std::fs::write(
        seed_dir.join("meta-data"),
        "instance-id: iid-local01\nlocal-hostname: botforge\n",
    )
    .with_context(|| format!("cannot write seed meta-data in {}", seed_dir.display()))?;
    std::fs::write(seed_dir.join("user-data"), user_data)
        .with_context(|| format!("cannot write seed user-data in {}", seed_dir.display()))?;
    Ok(())
}

pub(crate) fn detect_iso_tool() -> Result<&'static str> {
    if command_exists("xorriso") {
        return Ok("xorriso");
    }
    if command_exists("genisoimage") {
        return Ok("genisoimage");
    }
    bail!("neither 'xorriso' nor 'genisoimage' is available on PATH")
}

pub(crate) fn build_iso(src_dir: &Path, out: &Path, volume_id: &str) -> Result<()> {
    if !src_dir.is_dir() {
        bail!("source directory does not exist: {}", src_dir.display());
    }
    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("cannot create output dir: {}", parent.display()))?;
    }

    let tool = detect_iso_tool()?;
    let file_name = out
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("out.iso");
    let tmp_out = out
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(format!(".{file_name}.{}.tmp", unique_suffix()));

    let args = iso_args(tool, src_dir, &tmp_out, volume_id)?;
    if let Err(err) = run_command(tool, &args, &[], &format!("{tool} failed")) {
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

pub(crate) fn iso_args(
    tool: &str,
    src_dir: &Path,
    out: &Path,
    volume_id: &str,
) -> Result<Vec<String>> {
    let mut args = match tool {
        "xorriso" => vec!["-as".into(), "mkisofs".into()],
        "genisoimage" => Vec::new(),
        _ => bail!("unsupported iso tool '{tool}'"),
    };
    args.extend([
        "-r".into(),
        "-J".into(),
        "-V".into(),
        volume_id.into(),
        "-o".into(),
        out.display().to_string(),
        src_dir.display().to_string(),
    ]);
    Ok(args)
}

#[cfg(test)]
mod tests {
    use super::{generate_installer_username, iso_args, render_user_data, BootcmdEntry};
    use std::path::Path;

    #[test]
    fn render_user_data_replaces_placeholder() {
        let template = "#cloud-config\nssh_authorized_keys:\n  - REPLACE_WITH_SSH_PUBLIC_KEY\n";
        let rendered = render_user_data(Some(template), "ssh-ed25519 AAAA test", None, &[]);
        assert!(rendered.contains("ssh-ed25519 AAAA test"));
        assert!(!rendered.contains("REPLACE_WITH_SSH_PUBLIC_KEY"));
    }

    #[test]
    fn render_user_data_no_user_emits_top_level_key() {
        let rendered = render_user_data(None, "ssh-ed25519 AAAA nouser", None, &[]);
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
            &[],
        );
        // Must include the installer user entry
        assert!(
            rendered.contains("name: botforge-abc123"),
            "missing user name: {rendered}"
        );
        // Must grant passwordless sudo (installer must be able to sudo without a terminal)
        assert!(
            rendered.contains("sudo: 'ALL=(ALL) NOPASSWD:ALL'"),
            "missing sudo grant: {rendered}"
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
    // bootcmd tests
    // -----------------------------------------------------------------

    #[test]
    fn render_user_data_without_bootcmd_is_identical_to_baseline() {
        // Empty bootcmd slice must produce byte-for-byte the same output as no
        // bootcmd at all (regression guard: adding the parameter must not change
        // existing behaviour).
        let with_empty = render_user_data(None, "ssh-ed25519 AAAA key", Some("botforge-abc"), &[]);
        let baseline = "#cloud-config\nusers:\n  - default\n  - name: botforge-abc\n    shell: /bin/bash\n    lock_passwd: true\n    sudo: 'ALL=(ALL) NOPASSWD:ALL'\n    ssh_authorized_keys:\n      - ssh-ed25519 AAAA key\n";
        assert_eq!(
            with_empty, baseline,
            "empty bootcmd must not alter user-data"
        );
    }

    #[test]
    fn render_user_data_no_bootcmd_key_when_entries_absent() {
        // Verify the no-user variant also produces no `bootcmd:` key when absent.
        let rendered = render_user_data(None, "ssh-ed25519 AAAA key", None, &[]);
        assert!(
            !rendered.contains("bootcmd"),
            "no bootcmd: key expected when entries are empty: {rendered}"
        );
    }

    #[test]
    fn render_user_data_single_string_bootcmd_entry() {
        let entries = vec![BootcmdEntry::Shell("echo hello world".to_string())];
        let rendered =
            render_user_data(None, "ssh-ed25519 AAAA key", Some("botforge-abc"), &entries);

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
        let rendered =
            render_user_data(None, "ssh-ed25519 AAAA key", Some("botforge-abc"), &entries);

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
        let rendered = render_user_data(None, "ssh-ed25519 AAAA key", None, &entries);
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
        // ssh_authorized_keys AND append a valid bootcmd: block. This mirrors the
        // `else` arm of cmd_build's user_data construction (build.rs), which was
        // otherwise only exercised with an empty bootcmd slice.
        let entries = vec![BootcmdEntry::Shell(
            "systemctl mask botwork-api.service".to_string(),
        )];
        let rendered = render_user_data(None, "ssh-ed25519 AAAA nouser", None, &entries);

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
        // order they were supplied (only "single" and "mixed" were covered before).
        let entries = vec![
            BootcmdEntry::Shell("echo one".to_string()),
            BootcmdEntry::Shell("echo two".to_string()),
            BootcmdEntry::Shell("echo three".to_string()),
        ];
        let rendered =
            render_user_data(None, "ssh-ed25519 AAAA key", Some("botforge-abc"), &entries);
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
        // Guards the `render_bootcmd_block` invariant that BootcmdEntry always
        // serialises cleanly (the `.expect()` there): serialize each variant and
        // parse it back, asserting the value survives.
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
        // End-to-end seam between BuildConfig.bootcmd and the rendered seed:
        // cmd_build renders user-data via render_user_data(..., &build_config.bootcmd)
        // in two arms — botforge-owned (installer user) and caller-supplied
        // (ssh_user: None). A `Vec<BootcmdEntry>` (as BuildConfig would hold) must
        // land as a valid `bootcmd:` block in BOTH, alongside the identity content.
        let bootcmd = vec![
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

        // botforge-owned arm: installer user + bootcmd both present.
        let owned = render_user_data(None, "ssh-ed25519 AAAA k", Some("botforge-xyz"), &bootcmd);
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
        let supplied = render_user_data(None, "ssh-ed25519 AAAA k", None, &bootcmd);
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
    fn iso_args_xorriso_match_expected_argv() {
        let args = iso_args(
            "xorriso",
            Path::new("/tmp/src"),
            Path::new("/tmp/out.iso"),
            "cidata",
        )
        .unwrap();
        assert_eq!(
            args,
            vec![
                "-as",
                "mkisofs",
                "-r",
                "-J",
                "-V",
                "cidata",
                "-o",
                "/tmp/out.iso",
                "/tmp/src"
            ]
        );
    }

    #[test]
    fn iso_args_genisoimage_match_expected_argv() {
        let args = iso_args(
            "genisoimage",
            Path::new("/tmp/src"),
            Path::new("/tmp/out.iso"),
            "BOTFORGE",
        )
        .unwrap();
        assert_eq!(
            args,
            vec![
                "-r",
                "-J",
                "-V",
                "BOTFORGE",
                "-o",
                "/tmp/out.iso",
                "/tmp/src"
            ]
        );
    }
}
