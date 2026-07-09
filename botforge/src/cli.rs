use clap::{Parser, Subcommand};
use std::path::PathBuf;

use crate::commands;

#[derive(Parser, Debug)]
#[command(
    name = "botforge",
    about = "Build-time tooling for botworkz VM artifacts",
    long_about = "botforge is a build-time companion tool for preparing dependencies and VM build artifacts."
)]
pub(crate) struct Cli {
    /// Path to command config file (shasset manifest for `deps`, payload config for `payload`).
    #[arg(long, short = 'c', default_value = "shasset.yaml", global = true)]
    pub(crate) config: PathBuf,

    #[command(subcommand)]
    pub(crate) command: Commands,
}

#[derive(Subcommand, Debug)]
pub(crate) enum Commands {
    /// Build a qcow2 image by booting the source image and provisioning it via plan steps.
    Build(commands::build::BuildArgs),
    /// Fetch and stage one or all assets from shasset.yaml into a flat output directory.
    Deps(commands::deps::DepsArgs),
    /// Build an ISO image from a source directory.
    Iso(commands::iso::IsoArgs),
    /// Build a payload ISO from a config-driven staging plan.
    Payload(commands::payload::PayloadArgs),
    /// Launch a VM with qemu (KVM-only).
    Run(commands::run::RunArgs),
    /// Boot and validate a packed VM from a test config.
    Test(commands::test::TestArgs),
}
