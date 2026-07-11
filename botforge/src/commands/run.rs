use anyhow::Result;
use clap::Args;
use std::path::PathBuf;

use crate::commands::build::CpusArg;
use crate::qemu::{create_overlay_image, qemu_run_args, require_kvm};
use crate::util::{ensure_command, normalize_path, run_command};

#[derive(Args, Debug)]
pub(crate) struct RunArgs {
    /// Base qcow2 image path.
    #[arg(long, required = true)]
    base_image: PathBuf,
    /// Overlay qcow2 image path to create.
    #[arg(long, required = true)]
    overlay_image: PathBuf,
    /// NoCloud seed ISO path.
    #[arg(long, required = true)]
    seed_iso: PathBuf,
    /// Optional payload ISO path.
    #[arg(long)]
    payload_iso: Option<PathBuf>,
    /// Host SSH forward port to guest 22.
    #[arg(long, default_value_t = 2222)]
    ssh_port: u16,
    /// Run qemu in the foreground.
    #[arg(long)]
    foreground: bool,
    /// Guest RAM in MiB. Controls the runner VM only; does not affect the output image.
    #[arg(long, default_value_t = 4096)]
    memory: u32,
    /// Number of vCPUs for the runner VM, or 'auto' to use all available host CPUs.
    /// Controls the runner VM only; does not affect the output image.
    #[arg(long, default_value = "4")]
    cpus: CpusArg,
}

pub(crate) fn cmd_run(args: RunArgs) -> Result<()> {
    require_kvm()?;
    ensure_command("qemu-system-x86_64")?;
    ensure_command("qemu-img")?;

    let base_image = normalize_path(&args.base_image);
    let overlay_image = normalize_path(&args.overlay_image);
    let seed_iso = normalize_path(&args.seed_iso);
    let payload_isos: Vec<PathBuf> = args
        .payload_iso
        .as_ref()
        .map(|path| vec![normalize_path(path)])
        .unwrap_or_default();

    create_overlay_image(&base_image, &overlay_image)?;
    let mut qemu_args = qemu_run_args(
        &overlay_image,
        &seed_iso,
        &payload_isos,
        args.ssh_port,
        &[],
        args.memory,
        args.cpus.resolve(),
    );
    if !args.foreground {
        qemu_args.push("-daemonize".into());
    }

    run_command("qemu-system-x86_64", &qemu_args, &[], "qemu launch failed")?;
    Ok(())
}
