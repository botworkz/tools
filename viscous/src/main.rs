#![forbid(unsafe_code)]
//! `viscous` CLI — thin wrapper around the library.
//!
//! Three verbs match the lib API:
//!   - `describe TEMPLATE`     → emit the parsed spec as JSON.
//!   - `plan TEMPLATE DEST`    → build a plan and emit it as JSON; never writes.
//!   - `generate TEMPLATE DEST`→ build a plan and apply it.

use anyhow::{anyhow, Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(
    name = "viscous",
    about = "Opinionated, agent-friendly directory template generator",
    long_about = "viscous renders a directory template (containing __template__.yaml) \
into a destination directory, with declarative for_each / when / conflict semantics.\n\n\
Run `viscous <COMMAND> --help` for details on each command."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Parse a template's __template__.yaml and print its schema as JSON.
    Describe {
        /// Path to the template directory.
        template: PathBuf,
    },

    /// Build a plan and print it as JSON. Does not touch the destination.
    Plan {
        /// Path to the template directory.
        template: PathBuf,
        /// Where the plan would write to (used to root the dest paths).
        dest: PathBuf,
        /// Path to a JSON or YAML file of input vars.
        #[arg(long, short = 'v')]
        vars: Option<PathBuf>,
        /// Inline `--set key=value` overrides, applied after `--vars`.
        #[arg(long = "set", value_name = "KEY=VALUE")]
        sets: Vec<String>,
        /// Include file bodies (base64) in the plan output. Off by default
        /// because bodies dwarf the rest of the manifest.
        #[arg(long)]
        with_bodies: bool,
    },

    /// Build a plan and apply it to the destination directory.
    Generate {
        template: PathBuf,
        dest: PathBuf,
        #[arg(long, short = 'v')]
        vars: Option<PathBuf>,
        #[arg(long = "set", value_name = "KEY=VALUE")]
        sets: Vec<String>,
        /// How to behave when the destination directory already contains files.
        #[arg(long, value_enum, default_value_t = DestPolicyArg::RequireEmpty)]
        policy: DestPolicyArg,
    },
}

#[derive(Copy, Clone, Debug, ValueEnum)]
enum DestPolicyArg {
    RequireEmpty,
    Merge,
    Overwrite,
}

impl From<DestPolicyArg> for viscous::DestPolicy {
    fn from(p: DestPolicyArg) -> Self {
        match p {
            DestPolicyArg::RequireEmpty => viscous::DestPolicy::RequireEmpty,
            DestPolicyArg::Merge => viscous::DestPolicy::Merge,
            DestPolicyArg::Overwrite => viscous::DestPolicy::Overwrite,
        }
    }
}

fn main() {
    if let Err(e) = run() {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Describe { template } => {
            let spec = viscous::describe(&template)?;
            let json = serde_json::to_string_pretty(&spec)?;
            println!("{json}");
        }
        Command::Plan {
            template,
            dest,
            vars,
            sets,
            with_bodies,
        } => {
            let merged = load_vars(vars.as_deref(), &sets)?;
            let spec = viscous::Spec::load_from_dir(&template)?;
            let plan = viscous::build_plan(&template, &spec, &merged, &dest)?;
            let json = if with_bodies {
                serde_json::to_string_pretty(&PlanWithBodies::from(&plan))?
            } else {
                serde_json::to_string_pretty(&plan)?
            };
            println!("{json}");
        }
        Command::Generate {
            template,
            dest,
            vars,
            sets,
            policy,
        } => {
            let merged = load_vars(vars.as_deref(), &sets)?;
            let spec = viscous::Spec::load_from_dir(&template)?;
            let plan = viscous::build_plan(&template, &spec, &merged, &dest)?;
            let written = viscous::apply(&plan, policy.into())?;
            let summary = serde_json::json!({
                "template": plan.template_name,
                "dest_root": plan.dest_root,
                "files_written": written.len(),
                "collisions_resolved": plan.collisions_resolved,
                "final_files": plan.final_files,
                "paths": written,
            });
            println!("{}", serde_json::to_string_pretty(&summary)?);
        }
    }
    Ok(())
}

fn load_vars(path: Option<&std::path::Path>, sets: &[String]) -> Result<serde_json::Value> {
    let mut value = if let Some(p) = path {
        let body = std::fs::read_to_string(p)
            .with_context(|| format!("cannot read vars file: {}", p.display()))?;
        if p.extension().and_then(|s| s.to_str()) == Some("yaml")
            || p.extension().and_then(|s| s.to_str()) == Some("yml")
        {
            let v: serde_yaml::Value = serde_yaml::from_str(&body)
                .with_context(|| format!("invalid YAML in {}", p.display()))?;
            serde_json::to_value(v)?
        } else {
            serde_json::from_str(&body)
                .with_context(|| format!("invalid JSON in {}", p.display()))?
        }
    } else {
        serde_json::Value::Object(serde_json::Map::new())
    };

    for raw in sets {
        let (k, v) = raw
            .split_once('=')
            .ok_or_else(|| anyhow!("invalid --set '{raw}': expected key=value"))?;
        let parsed: serde_json::Value = match serde_json::from_str(v) {
            Ok(j) => j,
            Err(_) => serde_json::Value::String(v.to_string()),
        };
        let obj = value
            .as_object_mut()
            .ok_or_else(|| anyhow!("vars must be a JSON object"))?;
        obj.insert(k.to_string(), parsed);
    }
    Ok(value)
}

/// Wrapper that serialises [`viscous::Op`] with base64-encoded bodies.
#[derive(serde::Serialize)]
struct PlanWithBodies<'a> {
    #[serde(flatten)]
    inner: &'a viscous::Plan,
    bodies: Vec<BodyEntry>,
}

#[derive(serde::Serialize)]
struct BodyEntry {
    dest: PathBuf,
    /// Base64 (standard alphabet, no padding stripped) of the bytes.
    body_base64: String,
}

impl<'a> From<&'a viscous::Plan> for PlanWithBodies<'a> {
    fn from(plan: &'a viscous::Plan) -> Self {
        let bodies = plan
            .ops
            .iter()
            .map(|op| BodyEntry {
                dest: op.dest.clone(),
                body_base64: base64_encode(&op.bytes),
            })
            .collect();
        Self {
            inner: plan,
            bodies,
        }
    }
}

/// Minimal RFC 4648 base64 encoder; avoids pulling in another crate.
fn base64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    let mut chunks = bytes.chunks_exact(3);
    for chunk in chunks.by_ref() {
        let n = (u32::from(chunk[0]) << 16) | (u32::from(chunk[1]) << 8) | u32::from(chunk[2]);
        out.push(ALPHABET[((n >> 18) & 0x3F) as usize] as char);
        out.push(ALPHABET[((n >> 12) & 0x3F) as usize] as char);
        out.push(ALPHABET[((n >> 6) & 0x3F) as usize] as char);
        out.push(ALPHABET[(n & 0x3F) as usize] as char);
    }
    let rem = chunks.remainder();
    match rem.len() {
        0 => {}
        1 => {
            let n = u32::from(rem[0]) << 16;
            out.push(ALPHABET[((n >> 18) & 0x3F) as usize] as char);
            out.push(ALPHABET[((n >> 12) & 0x3F) as usize] as char);
            out.push('=');
            out.push('=');
        }
        2 => {
            let n = (u32::from(rem[0]) << 16) | (u32::from(rem[1]) << 8);
            out.push(ALPHABET[((n >> 18) & 0x3F) as usize] as char);
            out.push(ALPHABET[((n >> 12) & 0x3F) as usize] as char);
            out.push(ALPHABET[((n >> 6) & 0x3F) as usize] as char);
            out.push('=');
        }
        _ => unreachable!(),
    }
    out
}

#[cfg(test)]
mod tests {
    use super::base64_encode;

    #[test]
    fn base64_known_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }
}
