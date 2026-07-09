mod cli;
mod commands;
mod iso;
mod plan;
mod qemu;
mod resolver;
mod ssh;
mod util;

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
        Commands::Build(args) => commands::build::cmd_build(&cli.config, args),
        Commands::Deps(args) => commands::deps::cmd_deps(&cli.config, args),
        Commands::Iso(args) => commands::iso::cmd_iso(args),
        Commands::Payload(args) => commands::payload::cmd_payload(&cli.config, args),
        Commands::Run(args) => commands::run::cmd_run(args),
        Commands::Test(args) => commands::test::cmd_test(args),
    }
}
