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
    /// Path to a config file passed explicitly by the caller.  The meaning varies
    /// by subcommand: for `deps` / `build` / `test` it is a shasset manifest;
    /// for `payload` it is the payload YAML.  When omitted, each subcommand
    /// applies its own sensible default.
    #[arg(long, short = 'c', global = true)]
    pub(crate) config: Option<PathBuf>,

    #[command(subcommand)]
    pub(crate) command: Commands,
}

#[derive(Subcommand, Debug)]
pub(crate) enum Commands {
    /// Build a qcow2 image by booting the source image and provisioning it via plan steps.
    Build(commands::build::BuildArgs),
    /// Fetch and stage one or all assets from the shasset manifest into a flat output directory.
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

#[cfg(test)]
mod tests {
    use super::*;

    // ------------------------------------------------------------------
    // -c / --config flag tests
    // ------------------------------------------------------------------

    #[test]
    fn config_absent_is_none() {
        // When -c is omitted the parsed value must be None, not a default path.
        let cli = Cli::try_parse_from(["botforge", "deps", "--out", "out"]).unwrap();
        assert!(
            cli.config.is_none(),
            "config must be None when -c is not supplied; got {:?}",
            cli.config
        );
    }

    #[test]
    fn config_short_flag_before_subcommand() {
        // -c <file> placed BEFORE the subcommand must parse successfully.
        let cli =
            Cli::try_parse_from(["botforge", "-c", "shasset-bad.yaml", "deps", "--out", "out"])
                .unwrap();
        assert_eq!(cli.config, Some(PathBuf::from("shasset-bad.yaml")));
    }

    #[test]
    fn config_long_flag_before_subcommand() {
        // --config <file> placed BEFORE the subcommand must parse successfully.
        let cli = Cli::try_parse_from([
            "botforge",
            "--config",
            "my.yaml",
            "deps",
            "--out",
            "out",
        ])
        .unwrap();
        assert_eq!(cli.config, Some(PathBuf::from("my.yaml")));
    }

    #[test]
    fn config_short_flag_after_subcommand() {
        // -c <file> placed AFTER the subcommand (global = true) must parse successfully.
        let cli =
            Cli::try_parse_from(["botforge", "deps", "-c", "shasset-bad.yaml", "--out", "out"])
                .unwrap();
        assert_eq!(cli.config, Some(PathBuf::from("shasset-bad.yaml")));
    }

    #[test]
    fn config_shown_in_top_level_help() {
        // --help output must advertise the -c, --config option.
        let err = Cli::try_parse_from(["botforge", "--help"]).unwrap_err();
        let help = err.to_string();
        assert!(
            help.contains("-c") && help.contains("--config"),
            "-c/--config must appear in top-level help: {help}"
        );
    }

    #[test]
    fn config_has_no_global_default() {
        // Absence of -c must never silently resolve to a hard-coded path.
        // (Regression guard: ensure default_value was not re-introduced.)
        let cli = Cli::try_parse_from(["botforge", "iso", "--src", "src", "--out", "out.iso"])
            .unwrap();
        assert!(
            cli.config.is_none(),
            "config must remain None for subcommands that do not use it: {:?}",
            cli.config
        );
    }
}
