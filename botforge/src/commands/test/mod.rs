use anyhow::{bail, Context, Result};
use clap::Args;
use std::path::PathBuf;

use crate::iso::{
    build_iso, detect_iso_tool, generate_installer_username, render_user_data, write_seed_files,
};
use crate::qemu::{create_overlay_image, qemu_run_args, require_kvm, spawn_qemu_with_log};
use crate::ssh::{SshOptions, TemporarySshKeypair};
use crate::util::{create_temp_dir, ensure_command, resolve_under_root};

use crate::plan::{
    cleanup_test, collect_test_diagnostics, load_test_config, print_log_tail, run_test_flow,
    validate_test_ports, validate_test_steps, TestIso, TestIsoBootstrap,
};

#[derive(Args, Debug)]
pub(crate) struct TestArgs {
    /// Path to test.yaml config.
    #[arg(long = "test-config", required = true)]
    test_config: PathBuf,
    /// Base qcow2 image path.
    #[arg(long, required = true)]
    base_image: PathBuf,
    /// SSH private key path for guest access. Required when --ssh-user is provided;
    /// ignored when --ssh-user is omitted (botforge generates an ephemeral keypair).
    #[arg(long)]
    ssh_key: Option<PathBuf>,
    /// SSH host forwarded port.
    #[arg(long, default_value_t = 2222)]
    ssh_port: u16,
    /// SSH host.
    #[arg(long, default_value = "127.0.0.1")]
    ssh_host: String,
    /// SSH user. When omitted (the default), botforge generates a private ephemeral installer
    /// account (`botforge-<suffix>`) with passwordless sudo and seeds it via cloud-init.
    /// When explicitly provided together with --ssh-key, botforge connects as that existing
    /// user without creating it (the caller is responsible for the account and its key).
    #[arg(long)]
    ssh_user: Option<String>,
    /// Repo root for resolving relative test paths and `uses:` step includes.
    #[arg(long, required = true)]
    repo_root: PathBuf,
    /// Leave VM running and preserve overlay on exit.
    #[arg(long)]
    keep_running: bool,
}

pub(crate) fn cmd_test(args: TestArgs) -> Result<()> {
    require_kvm()?;
    ensure_command("qemu-system-x86_64")?;
    ensure_command("qemu-img")?;
    detect_iso_tool()?;

    let repo_root = std::fs::canonicalize(args.repo_root).context("failed to resolve repo root")?;
    let test_config_path = resolve_under_root(&repo_root, args.test_config);
    let base_image = resolve_under_root(&repo_root, args.base_image);

    let test_config = load_test_config(&repo_root, &test_config_path)?;
    validate_test_steps(&test_config.steps, &test_config.ports)?;
    let build_dir = repo_root.join("build");
    std::fs::create_dir_all(&build_dir)
        .with_context(|| format!("cannot create build dir: {}", build_dir.display()))?;
    let overlay_image = build_dir.join("test-overlay.qcow2");
    let seed_iso = build_dir.join("test-seed.iso");
    let vm_log = build_dir.join("test-vm.log");
    let seed_dir = create_temp_dir("botforge-test-seed")?;

    // -------------------------------------------------------------------------
    // Determine installer identity: ephemeral (botforge-owned) or caller-supplied.
    //
    // When --ssh-user is omitted, botforge generates an ephemeral installer account
    // (botforge-<suffix>) with passwordless sudo, generates a matching ephemeral
    // keypair, and seeds the installer via cloud-init.  The test overlay is
    // discarded on exit, so installer teardown is not required for test (though
    // the seed/connect-as-installer change is shared with build for consistency).
    //
    // When --ssh-user is provided, --ssh-key must also be present; botforge
    // connects as that existing user without creating it in cloud-init.
    // -------------------------------------------------------------------------
    let (ssh_public_key, ssh_user_string, ssh_key_path, _kept_keypair): (
        String,
        String,
        PathBuf,
        Option<TemporarySshKeypair>,
    ) = match (args.ssh_user, args.ssh_key) {
        (Some(user), Some(key)) => {
            // Caller-supplied identity: user exists in the image with the given key.
            let key_path = resolve_under_root(&repo_root, key);
            let pub_path = PathBuf::from(format!("{}.pub", key_path.display()));
            let pub_key = std::fs::read_to_string(&pub_path)
                .with_context(|| format!("cannot read SSH public key: {}", pub_path.display()))?;
            (pub_key, user, key_path, None)
        }
        (None, None) => {
            // Ephemeral installer: generate per-run keypair and installer account.
            let kp = TemporarySshKeypair::generate("botforge-test-ssh")?;
            let pub_key = std::fs::read_to_string(kp.public_key()).with_context(|| {
                format!(
                    "cannot read ephemeral SSH public key: {}",
                    kp.public_key().display()
                )
            })?;
            let key_path = kp.private_key().to_path_buf();
            let user = generate_installer_username();
            (pub_key, user, key_path, Some(kp))
        }
        _ => {
            bail!("--ssh-key and --ssh-user must both be provided together or both omitted");
        }
    };

    // When botforge owns the installer, seed it with sudo so the harness can run
    // provisioner steps. When the caller supplied an explicit user, don't create
    // it in cloud-init (caller owns the account and its key).
    let user_data = if _kept_keypair.is_some() {
        render_user_data(
            None,
            ssh_public_key.trim(),
            Some(ssh_user_string.as_str()),
            &[],
        )
    } else {
        render_user_data(None, ssh_public_key.trim(), None, &[])
    };
    write_seed_files(&seed_dir, &user_data)?;
    build_iso(&seed_dir, &seed_iso, "cidata")?;
    std::fs::remove_dir_all(&seed_dir)
        .with_context(|| format!("cannot remove temp seed dir: {}", seed_dir.display()))?;

    create_overlay_image(&base_image, &overlay_image)?;

    let mut extra_isos = Vec::new();
    let mut bootstraps = Vec::new();
    for iso in &test_config.isos {
        match iso {
            TestIso::Attach(path) => {
                extra_isos.push(resolve_under_root(&repo_root, path.clone()));
            }
            TestIso::Bootstrap {
                path,
                label,
                mount,
                bootstrap,
            } => {
                extra_isos.push(resolve_under_root(&repo_root, path.clone()));
                bootstraps.push(TestIsoBootstrap {
                    label: label.clone(),
                    mount: mount.clone(),
                    bootstrap: bootstrap.clone(),
                });
            }
        }
    }
    validate_test_ports(&test_config.ports, args.ssh_port)?;
    let qemu_args = qemu_run_args(
        &overlay_image,
        &seed_iso,
        &extra_isos,
        args.ssh_port,
        &test_config.ports,
    );

    let mut vm_child = Some(spawn_qemu_with_log(&qemu_args, &vm_log)?);
    let ssh_options = SshOptions {
        host: args.ssh_host.clone(),
        port: args.ssh_port,
        user: ssh_user_string,
        key: ssh_key_path,
    };

    let test_result = run_test_flow(&repo_root, &test_config, &ssh_options, &bootstraps);
    if let Err(err) = test_result {
        eprintln!("test failed: {err:#}");
        collect_test_diagnostics(&ssh_options, &test_config.diagnostics_units);
        print_log_tail(&vm_log, 200);
        if !args.keep_running {
            cleanup_test(&mut vm_child, &overlay_image);
        }
        return Err(err);
    }

    if !args.keep_running {
        cleanup_test(&mut vm_child, &overlay_image);
    }
    println!("test passed");
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::cli::Cli;
    use clap::Parser;

    #[test]
    fn test_test_cli_requires_repo_root() {
        let err = Cli::try_parse_from([
            "botforge",
            "test",
            "--test-config",
            "test.yaml",
            "--base-image",
            "base.qcow2",
        ])
        .unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);
        assert!(err.to_string().contains("--repo-root"));
    }

    #[test]
    fn test_test_cli_ssh_user_and_key_both_optional() {
        // Both --ssh-user and --ssh-key are optional at the clap level (consistency
        // between them is validated at runtime). Verify all required clap args can
        // be satisfied without either flag (ephemeral installer mode).
        let result = Cli::try_parse_from([
            "botforge",
            "test",
            "--test-config",
            "test.yaml",
            "--base-image",
            "base.qcow2",
            "--repo-root",
            "/repo",
        ]);
        // Should parse without a clap error (ssh-user and ssh-key are both optional).
        assert!(
            result.is_ok(),
            "CLI should parse without ssh-user/ssh-key: {}",
            result.unwrap_err()
        );
    }
}
