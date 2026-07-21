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
mod ssh;
mod step;
mod util;
mod workspace;

use clap::Parser;

use crate::cli::{Cli, Commands};

fn main() {
    let (result, final_outcome_command) = run();
    if let Err(e) = &result {
        eprintln!("error: {e:#}");
    }
    if let Some(command) = final_outcome_command {
        crate::plan::print_final_outcome(command, result.is_ok());
    }
    if result.is_err() {
        std::process::exit(1);
    }
}

fn run() -> (anyhow::Result<()>, Option<&'static str>) {
    let cli = Cli::parse();
    match cli.command {
        Commands::Build(args) => (commands::build::cmd_build(args), Some("build")),
        Commands::Config { context, sub } => (commands::config::cmd_config(context, sub), None),
        Commands::Deps(args) => (commands::deps::cmd_deps(args), None),
        Commands::Iso(args) => (commands::iso::cmd_iso(args), None),
        Commands::Payload(args) => (commands::payload::cmd_payload(args), None),
        Commands::Publish(args) => (commands::publish::cmd_publish(args), None),
        Commands::Run(args) => (commands::run::cmd_run(args), None),
        Commands::Test(args) => (commands::test::cmd_test(args), Some("test")),
    }
}
