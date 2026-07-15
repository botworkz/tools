//! `botforge config` command group — B4.
//!
//! Subcommands:
//! - [`sync`](sync::SyncArgs) — reconcile the `build:`/`test:` registry in
//!   `botforge.yaml` with the on-disk spec files.

pub(crate) mod sync;

use anyhow::Result;
use clap::Subcommand;

#[derive(Subcommand, Debug)]
pub(crate) enum ConfigCommands {
    /// Reconcile the build/test registry in botforge.yaml with on-disk spec files.
    ///
    /// Default (no flags): dry-run diff to stdout showing what the registry would
    /// be regenerated to based on discovered spec files.  Nothing is written.
    /// Pass --write to actually update botforge.yaml.
    ///
    /// --check  : compare committed registry vs discovered specs; exit non-zero on
    ///            any drift (CI mode).  Writes nothing.
    ///
    /// --out    : outward reconciliation (config → files).  Rewrites each spec
    ///            file's `name:` field to match its registry key.  Warns about
    ///            files that would be deleted by --out --delete.
    ///
    /// --out --delete : as --out, plus delete files the config no longer references.
    ///
    /// --delete (without --out) : error.
    Sync(sync::SyncArgs),
}

pub(crate) fn cmd_config(context: Option<std::path::PathBuf>, sub: ConfigCommands) -> Result<()> {
    match sub {
        ConfigCommands::Sync(args) => sync::cmd_sync(context, args),
    }
}
