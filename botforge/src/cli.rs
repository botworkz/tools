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
    #[command(subcommand)]
    pub(crate) command: Commands,
}

#[derive(Subcommand, Debug)]
pub(crate) enum Commands {
    /// Build a qcow2 image by booting the source image and provisioning it via plan steps.
    Build(commands::build::BuildArgs),
    /// Manage workspace configuration: registry sync, drift detection.
    Config {
        /// Workspace context root. When provided, must contain a botforge marker. When
        /// omitted, botforge walks up from the current directory to find one.
        #[arg(long, global = true)]
        context: Option<PathBuf>,
        #[command(subcommand)]
        sub: commands::config::ConfigCommands,
    },
    /// Fetch and stage one or all assets from the workspace marker's inline assets block.
    Deps(commands::deps::DepsArgs),
    /// Build an ISO image from a source directory.
    Iso(commands::iso::IsoArgs),
    /// Build a payload ISO from a spec-driven staging plan.
    Payload(commands::payload::PayloadArgs),
    /// Publish build artifacts to one or more targets (fs, s3).
    Publish(commands::publish::PublishArgs),
    /// Launch a VM with qemu (KVM-only).
    Run(commands::run::RunArgs),
    /// Boot and validate a packed VM from a test config.
    Test(commands::test::TestArgs),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_rejects_removed_short_config_flag_before_subcommand() {
        let err = Cli::try_parse_from(["botforge", "-c", "whatever", "deps", "--out", "out"])
            .unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::UnknownArgument);
        assert!(
            err.to_string().contains("-c"),
            "error should mention -c: {err}"
        );
    }

    #[test]
    fn cli_rejects_removed_long_config_flag_before_subcommand() {
        let err = Cli::try_parse_from(["botforge", "--config", "whatever", "deps", "--out", "out"])
            .unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::UnknownArgument);
        assert!(
            err.to_string().contains("--config"),
            "error should mention --config: {err}"
        );
    }

    #[test]
    fn cli_rejects_removed_config_flag_after_subcommand() {
        let err = Cli::try_parse_from(["botforge", "deps", "-c", "whatever", "--out", "out"])
            .unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::UnknownArgument);
        assert!(
            err.to_string().contains("-c"),
            "error should mention -c: {err}"
        );
    }

    #[test]
    fn removed_config_flag_not_shown_in_top_level_help() {
        let err = Cli::try_parse_from(["botforge", "--help"]).unwrap_err();
        let help = err.to_string();
        assert!(
            !help.contains("--config") && !help.contains("-c,"),
            "top-level help must not advertise removed -c/--config: {help}"
        );
    }

    // --- --color and --attach flag parsing tests ---

    #[test]
    fn build_accepts_color_flag() {
        // --color must be accepted by `botforge build`.
        // We pass a fake NAME and rely on the parser not reaching the filesystem.
        let result = Cli::try_parse_from(["botforge", "build", "my-build", "--color"]);
        match result {
            Ok(cli) => match cli.command {
                Commands::Build(args) => {
                    assert!(args.color, "--color should be true");
                }
                _ => panic!("expected Build command"),
            },
            // Parsing succeeds; any downstream error (missing file etc.) is irrelevant here.
            Err(e) => panic!("build --color parse failed: {e}"),
        }
    }

    #[test]
    fn build_accepts_attach_flag() {
        let result = Cli::try_parse_from(["botforge", "build", "my-build", "--attach"]);
        match result {
            Ok(cli) => match cli.command {
                Commands::Build(args) => {
                    assert!(args.attach, "--attach should be true");
                }
                _ => panic!("expected Build command"),
            },
            Err(e) => panic!("build --attach parse failed: {e}"),
        }
    }

    #[test]
    fn test_accepts_color_flag() {
        let result = Cli::try_parse_from(["botforge", "test", "my-test", "--color"]);
        match result {
            Ok(cli) => match cli.command {
                Commands::Test(args) => {
                    assert!(args.color, "--color should be true");
                }
                _ => panic!("expected Test command"),
            },
            Err(e) => panic!("test --color parse failed: {e}"),
        }
    }

    #[test]
    fn test_accepts_attach_flag() {
        let result = Cli::try_parse_from(["botforge", "test", "my-test", "--attach"]);
        match result {
            Ok(cli) => match cli.command {
                Commands::Test(args) => {
                    assert!(args.attach, "--attach should be true");
                }
                _ => panic!("expected Test command"),
            },
            Err(e) => panic!("test --attach parse failed: {e}"),
        }
    }

    #[test]
    fn build_color_defaults_to_false() {
        let result = Cli::try_parse_from(["botforge", "build", "my-build"]);
        match result {
            Ok(cli) => match cli.command {
                Commands::Build(args) => {
                    assert!(!args.color, "--color should default to false");
                    assert!(!args.attach, "--attach should default to false");
                }
                _ => panic!("expected Build command"),
            },
            Err(e) => panic!("build parse failed: {e}"),
        }
    }

    #[test]
    fn test_color_defaults_to_false() {
        let result = Cli::try_parse_from(["botforge", "test", "my-test"]);
        match result {
            Ok(cli) => match cli.command {
                Commands::Test(args) => {
                    assert!(!args.color, "--color should default to false");
                    assert!(!args.attach, "--attach should default to false");
                }
                _ => panic!("expected Test command"),
            },
            Err(e) => panic!("test parse failed: {e}"),
        }
    }

    #[test]
    fn build_help_mentions_color_and_attach() {
        let err = Cli::try_parse_from(["botforge", "build", "--help"]).unwrap_err();
        let help = err.to_string();
        assert!(
            help.contains("--color"),
            "build --help must mention --color: {help}"
        );
        assert!(
            help.contains("--attach"),
            "build --help must mention --attach: {help}"
        );
    }

    #[test]
    fn test_help_mentions_color_and_attach() {
        let err = Cli::try_parse_from(["botforge", "test", "--help"]).unwrap_err();
        let help = err.to_string();
        assert!(
            help.contains("--color"),
            "test --help must mention --color: {help}"
        );
        assert!(
            help.contains("--attach"),
            "test --help must mention --attach: {help}"
        );
    }
}
