#![forbid(unsafe_code)]
mod assert;
mod cli;
mod commands;
mod compress;
mod config;
mod iso;
mod plan;
mod qemu;
mod resolver;
mod signal;
mod ssh;
mod step;
mod util;
mod workspace;

use anyhow::Result;
use clap::Parser;

use crate::cli::{Cli, Commands};

fn main() {
    if let Err(e) = run() {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Commands::Build(args) => commands::build::cmd_build(args),
        Commands::Config { context, sub } => commands::config::cmd_config(context, sub),
        Commands::Deps(args) => commands::deps::cmd_deps(args),
        Commands::Iso(args) => commands::iso::cmd_iso(args),
        Commands::Payload(args) => commands::payload::cmd_payload(args),
        Commands::Publish(args) => commands::publish::cmd_publish(args),
        Commands::Run(args) => commands::run::cmd_run(args),
        Commands::Test(args) => commands::test::cmd_test(args),
    }
}
