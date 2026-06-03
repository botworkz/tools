use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};

use crate::util::{command_exists, run_command, unique_suffix};

const USER_DATA_PLACEHOLDER: &str = "REPLACE_WITH_SSH_PUBLIC_KEY";

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
        return format!(
            "#cloud-config\nusers:\n  - default\n  - name: {user}\n    ssh_authorized_keys:\n      - {ssh_public_key}\n"
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
    use super::{iso_args, render_user_data};
    use std::path::Path;

    #[test]
    fn render_user_data_replaces_placeholder() {
        let template = "#cloud-config\nssh_authorized_keys:\n  - REPLACE_WITH_SSH_PUBLIC_KEY\n";
        let rendered = render_user_data(Some(template), "ssh-ed25519 AAAA test", None);
        assert!(rendered.contains("ssh-ed25519 AAAA test"));
        assert!(!rendered.contains("REPLACE_WITH_SSH_PUBLIC_KEY"));
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
