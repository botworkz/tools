use anyhow::{bail, Context, Result};
use std::io::Read;
use std::path::{Path, PathBuf};

use crate::util::{command_exists, run_command, unique_suffix};

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
) -> String {
    if let Some(template) = template {
        return template.replace(USER_DATA_PLACEHOLDER, ssh_public_key);
    }
    if let Some(user) = ssh_user {
        // The named user is always a botforge-owned ephemeral installer: it needs
        // passwordless sudo to run provisioner steps and cloud-init waits, a
        // login shell for SSH execution, and key-only (locked password) access.
        return format!(
            "#cloud-config\nusers:\n  - default\n  - name: {user}\n    shell: /bin/bash\n    lock_passwd: true\n    sudo: 'ALL=(ALL) NOPASSWD:ALL'\n    ssh_authorized_keys:\n      - {ssh_public_key}\n"
        );
    }
    format!("#cloud-config\nssh_authorized_keys:\n  - {ssh_public_key}\n")
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
    use super::{generate_installer_username, iso_args, render_user_data};
    use std::path::Path;

    #[test]
    fn render_user_data_replaces_placeholder() {
        let template = "#cloud-config\nssh_authorized_keys:\n  - REPLACE_WITH_SSH_PUBLIC_KEY\n";
        let rendered = render_user_data(Some(template), "ssh-ed25519 AAAA test", None);
        assert!(rendered.contains("ssh-ed25519 AAAA test"));
        assert!(!rendered.contains("REPLACE_WITH_SSH_PUBLIC_KEY"));
    }

    #[test]
    fn render_user_data_no_user_emits_top_level_key() {
        let rendered = render_user_data(None, "ssh-ed25519 AAAA nouser", None);
        assert!(rendered.contains("ssh_authorized_keys:"));
        assert!(rendered.contains("ssh-ed25519 AAAA nouser"));
        assert!(!rendered.contains("users:"));
        assert!(!rendered.contains("sudo:"));
    }

    #[test]
    fn render_user_data_installer_has_sudo_key_and_lock_passwd() {
        let rendered =
            render_user_data(None, "ssh-ed25519 AAAA installer", Some("botforge-abc123"));
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
