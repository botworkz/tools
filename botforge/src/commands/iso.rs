use anyhow::{bail, Context, Result};
use clap::Args;
use std::path::PathBuf;

use crate::iso::{build_iso, read_ssh_public_key, render_user_data, write_seed_files};
use crate::util::create_temp_dir;

#[derive(Args, Debug)]
pub(crate) struct IsoArgs {
    /// Source directory tree to include in the ISO (required in plain mode; ignored in seed mode).
    #[arg(long)]
    src: Option<PathBuf>,
    /// Output ISO file path.
    #[arg(long, required = true)]
    out: PathBuf,
    /// ISO volume ID.
    #[arg(long, default_value = "BOTFORGE")]
    volume_id: String,
    /// Inject this SSH public key into generated cloud-init user-data.
    #[arg(long)]
    ssh_public_key: Option<String>,
    /// Read SSH public key from this file and inject into generated cloud-init user-data.
    #[arg(long)]
    ssh_public_key_file: Option<PathBuf>,
    /// Optional cloud-init user-data template; replaces REPLACE_WITH_SSH_PUBLIC_KEY.
    #[arg(long)]
    user_data_template: Option<PathBuf>,
}

pub(crate) fn cmd_iso(args: IsoArgs) -> Result<()> {
    let ssh_public_key = read_ssh_public_key(args.ssh_public_key, args.ssh_public_key_file)?;
    if let Some(key) = ssh_public_key {
        let template_content = args
            .user_data_template
            .as_ref()
            .map(|path| {
                std::fs::read_to_string(path)
                    .with_context(|| format!("cannot read user-data template: {}", path.display()))
            })
            .transpose()?;
        let temp_dir = create_temp_dir("botforge-seed")?;
        let user_data = render_user_data(template_content.as_deref(), &key, None, &[]);
        write_seed_files(&temp_dir, &user_data)?;
        build_iso(&temp_dir, &args.out, &args.volume_id)?;
        std::fs::remove_dir_all(&temp_dir)
            .with_context(|| format!("cannot remove temp seed dir: {}", temp_dir.display()))?;
    } else {
        let src = args.src.ok_or_else(|| {
            anyhow::anyhow!(
                "--src is required when no SSH key flag (--ssh-public-key or --ssh-public-key-file) is provided"
            )
        })?;
        if !src.is_dir() {
            bail!("source directory does not exist: {}", src.display());
        }
        build_iso(&src, &args.out, &args.volume_id)?;
    }

    println!("built ISO at {}", args.out.display());
    Ok(())
}
