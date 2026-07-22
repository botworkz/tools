use anyhow::{bail, Context, Result};
use clap::{ArgGroup, Args};
use shasset::manifest::Manifest;
use std::path::{Path, PathBuf};

use crate::commands::build::CpusArg;
use crate::iso::{generate_installer_username, prepare_seed_image, render_user_data};
use crate::qemu::{
    create_overlay_image, qemu_run_args, require_kvm, spawn_qemu_attached, spawn_qemu_with_log,
};
use crate::resolver::{AssetKind, Reference, ResolveFileContext, ResolveSpec};
use crate::signal;
use crate::ssh::{SshOptions, TemporarySshKeypair};
use crate::util::{create_temp_dir, ensure_command, resolve_under_root};
use crate::workspace::{
    discover_context, load_inline_manifest, load_plugin_entries, registry::load_committed_registry,
};

use crate::config::{
    load_test_config, validate_test_ports, validate_test_steps, TestIso, TestIsoBootstrap,
};
use crate::plan::{
    cleanup_test, collect_test_diagnostics, init_force_color, print_log_tail, run_test_flow,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TestCompletion {
    Passed,
    Failed,
    Interrupted,
}

fn classify_test_completion(run_result_ok: bool, interrupted: bool) -> TestCompletion {
    if interrupted {
        TestCompletion::Interrupted
    } else if run_result_ok {
        TestCompletion::Passed
    } else {
        TestCompletion::Failed
    }
}

#[derive(Args, Debug)]
#[command(group(ArgGroup::new("target").required(true).args(["name", "test_config"])))]
pub(crate) struct TestArgs {
    /// Name of the test to run, resolved via workspace discovery.
    /// Mutually exclusive with --test-config.
    #[arg(value_name = "NAME")]
    name: Option<String>,
    /// Path to test.yaml config (explicit override).
    /// Mutually exclusive with NAME.
    #[arg(long = "test-config")]
    test_config: Option<PathBuf>,
    /// Base qcow2 image path or `@…` reference. Overrides `image:` in the test config.
    #[arg(long)]
    base_image: Option<PathBuf>,
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
    /// Workspace context root. When provided, must contain a botforge marker. When
    /// omitted, botforge walks up from the current directory to find one.
    #[arg(long)]
    context: Option<PathBuf>,
    /// Leave VM running and preserve overlay on exit.
    #[arg(long)]
    keep_running: bool,
    /// Guest RAM in MiB. Controls the runner VM only; does not affect the output image.
    #[arg(long, default_value_t = 4096)]
    memory: u32,
    /// Number of vCPUs for the runner VM, or 'auto' to use all available host CPUs.
    /// Controls the runner VM only; does not affect the output image.
    #[arg(long, default_value = "4")]
    cpus: CpusArg,
    /// Force ANSI color output even when stderr is not a TTY.
    /// When set, color is always emitted regardless of TTY detection and wins over
    /// the `NO_COLOR` environment variable.  Alternatively, set `FORCE_COLOR=1` or
    /// `CLICOLOR_FORCE=1` in the environment (useful in CI without threading the
    /// flag everywhere).
    #[arg(long)]
    pub(crate) color: bool,
    /// Attach an interactive serial console to the VM.
    ///
    /// Adds `-serial mon:stdio` to the QEMU argv and inherits stdin/stdout/stderr
    /// so the operator can interact with the guest serial console directly.
    /// Use `Ctrl-A c` to toggle between the serial console and the QEMU monitor.
    ///
    /// If stdin is not a TTY (e.g. under CI), falls back to the default
    /// non-interactive background mode with a warning.
    ///
    /// Note: in attach mode the VM log file is not written (stdout/stderr are
    /// inherited rather than redirected), so failure diagnostics via `print_log_tail`
    /// will show an empty log.  Use non-attach mode for automated CI runs.
    #[arg(long)]
    pub(crate) attach: bool,
}

pub(crate) fn cmd_test(args: TestArgs) -> Result<()> {
    // Initialize force-color from the --color flag before any logging output.
    init_force_color(args.color);
    require_kvm()?;
    ensure_command("qemu-system-x86_64")?;
    ensure_command("qemu-img")?;

    let context = discover_context(args.context.as_deref())?;
    let manifest = load_inline_manifest(&context)?;

    // Resolve the test config path: either from an explicit --test-config flag
    // or by looking up the named test in the committed registry.
    let test_config_path = match (args.name, args.test_config) {
        (Some(name), None) => {
            let registry = load_committed_registry(&context)?;
            registry.test(&name)?.clone()
        }
        (None, Some(tc)) => resolve_under_root(&context, tc),
        _ => bail!("exactly one of NAME or --test-config must be provided"),
    };

    let test_config = load_test_config(&context, &test_config_path)?;
    let base_image = resolve_test_base_image(
        &context,
        &manifest,
        args.base_image,
        test_config.image.as_ref(),
    )?;
    validate_test_steps(&test_config.steps, &test_config.ports)?;
    let build_dir = context.join("build");
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
            let key_path = resolve_under_root(&context, key);
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
            test_config.cloud_init.as_ref(),
        )
    } else {
        render_user_data(
            None,
            ssh_public_key.trim(),
            None,
            test_config.cloud_init.as_ref(),
        )
    };
    prepare_seed_image(&seed_dir, &seed_iso, &user_data)?;

    create_overlay_image(&base_image, &overlay_image)?;

    let mut extra_isos = Vec::new();
    let mut bootstraps = Vec::new();
    for iso in &test_config.isos {
        match iso {
            TestIso::Attach(path) => {
                extra_isos.push(resolve_test_iso_path(&context, &manifest, path)?);
            }
            TestIso::Bootstrap {
                path,
                label,
                mount,
                bootstrap,
            } => {
                extra_isos.push(resolve_test_iso_path(&context, &manifest, path)?);
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
        args.memory,
        args.cpus.resolve(),
    );

    // Arm the interrupt handler BEFORE spawning QEMU so that Ctrl-C
    // (SIGINT) is caught by botforge from the very first moment the VM
    // exists.  On the --attach path the handler is deliberately NOT armed
    // (Ctrl-C is forwarded to the guest).
    let _interrupt_guard = if args.attach {
        None
    } else {
        Some(signal::arm_interrupts()?)
    };
    let (mut vm_child, _terminal_guard) = if args.attach {
        let (child, guard) = spawn_qemu_attached(&qemu_args, &vm_log)?;
        (Some(child), guard)
    } else {
        (Some(spawn_qemu_with_log(&qemu_args, &vm_log)?), None)
    };
    // Capture the installer username before it is moved into ssh_options.
    // This is only meaningful when botforge owns the installer (_kept_keypair is Some).
    let installer_username: Option<String> = if _kept_keypair.is_some() {
        Some(ssh_user_string.clone())
    } else {
        None
    };
    let ssh_options = SshOptions {
        host: args.ssh_host.clone(),
        port: args.ssh_port,
        user: ssh_user_string,
        key: ssh_key_path,
    };

    // Load plugins from the workspace marker so that plugin-provided assert
    // verbs (e.g. assert/docker) are available during the assert phase.
    let plugin_entries = load_plugin_entries(&context)?;
    let mut plugin_registry = botforge_plugin_host::PluginRegistry::new();
    for entry in &plugin_entries {
        let provides_filter: Option<Vec<String>> = entry.provides.clone();
        plugin_registry
            .load_plugin(&entry.name, &entry.src, provides_filter.as_deref())
            .with_context(|| format!("failed to load plugin '{}'", entry.name))?;
    }

    let test_result = run_test_flow(
        &context,
        &test_config,
        &ssh_options,
        &bootstraps,
        &manifest,
        None,
        installer_username.as_deref(),
        &plugin_registry,
    );
    match classify_test_completion(test_result.is_ok(), signal::is_interrupted()) {
        TestCompletion::Interrupted => {
            eprintln!("interrupted — shutting down VM…");
            if !args.keep_running {
                cleanup_test(&mut vm_child, &overlay_image);
            }
            Err(anyhow::anyhow!("test interrupted"))
        }
        TestCompletion::Failed => {
            let err = test_result.expect_err("classified failed must have error");
            eprintln!("test failed: {err:#}");
            collect_test_diagnostics(&ssh_options, &test_config.diagnostics_units);
            print_log_tail(&vm_log, 200);
            if !args.keep_running {
                cleanup_test(&mut vm_child, &overlay_image);
            }
            Err(err)
        }
        TestCompletion::Passed => {
            if !args.keep_running {
                cleanup_test(&mut vm_child, &overlay_image);
            }
            println!("test passed");
            Ok(())
        }
    }
}

fn resolve_test_base_image(
    context: &Path,
    manifest: &Manifest,
    base_image_arg: Option<PathBuf>,
    config_image: Option<&Reference>,
) -> Result<PathBuf> {
    let base_image_spec = ResolveSpec {
        deny_kinds: vec![AssetKind::OciImage],
        ..Default::default()
    };
    let resolve_context = ResolveFileContext {
        context,
        manifest,
        cache_dir_override: None,
    };

    if let Some(base_image) = base_image_arg {
        if let Some(raw) = base_image.to_str().filter(|raw| raw.starts_with('@')) {
            let reference = Reference::parse(raw)
                .with_context(|| format!("invalid --base-image reference: {raw:?}"))?;
            return reference.resolve_one_validated(&resolve_context, &base_image_spec);
        }
        return Ok(resolve_under_root(context, base_image));
    }

    let image = config_image.context(
        "no base image provided: set `image:` in the test config or pass `--base-image`",
    )?;
    image.resolve_one_validated(&resolve_context, &base_image_spec)
}

fn resolve_test_iso_path(context: &Path, manifest: &Manifest, path: &Path) -> Result<PathBuf> {
    if let Some(raw) = path.to_str().filter(|raw| raw.starts_with('@')) {
        let iso_spec = ResolveSpec {
            deny_kinds: vec![AssetKind::OciImage],
            ..Default::default()
        };
        let resolve_context = ResolveFileContext {
            context,
            manifest,
            cache_dir_override: None,
        };
        let reference = Reference::parse(raw)
            .with_context(|| format!("invalid iso path reference: {raw:?}"))?;
        return reference.resolve_one_validated(&resolve_context, &iso_spec);
    }
    Ok(resolve_under_root(context, path.to_path_buf()))
}

#[cfg(test)]
mod tests {
    use super::{
        classify_test_completion, resolve_test_base_image, resolve_test_iso_path, TestCompletion,
    };
    use crate::cli::{Cli, Commands};
    use crate::resolver::Reference;
    use clap::Parser;
    use shasset::manifest::Manifest;
    use std::path::PathBuf;
    use tempfile::TempDir;

    #[test]
    fn classify_completion_prefers_interrupt_over_success() {
        assert_eq!(
            classify_test_completion(true, true),
            TestCompletion::Interrupted
        );
    }

    #[test]
    fn classify_completion_prefers_interrupt_over_failure() {
        assert_eq!(
            classify_test_completion(false, true),
            TestCompletion::Interrupted
        );
    }

    #[test]
    fn test_test_cli_no_context_parses_ok() {
        // --context is now optional; the CLI parses fine without it.
        // Runtime context discovery is tested in workspace.rs.
        let result = Cli::try_parse_from([
            "botforge",
            "test",
            "--test-config",
            "test.yaml",
            "--base-image",
            "base.qcow2",
        ]);
        assert!(
            result.is_ok(),
            "CLI should parse without --context: {}",
            result.unwrap_err()
        );
    }

    #[test]
    fn test_test_cli_requires_name_or_test_config() {
        // Neither NAME nor --test-config provided: should error.
        let err = Cli::try_parse_from(["botforge", "test"]).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);
    }

    #[test]
    fn test_test_cli_name_parses_positional() {
        let result = Cli::try_parse_from(["botforge", "test", "my-test"]);
        assert!(
            result.is_ok(),
            "test should parse with positional NAME: {result:?}"
        );
        if let Ok(cli) = result {
            if let Commands::Test(args) = cli.command {
                assert_eq!(args.name, Some("my-test".to_string()));
                assert!(args.test_config.is_none());
            }
        }
    }

    #[test]
    fn test_test_cli_name_and_test_config_mutually_exclusive() {
        let err =
            Cli::try_parse_from(["botforge", "test", "my-test", "--test-config", "test.yaml"])
                .unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    #[test]
    fn test_test_cli_context_flag_accepted() {
        // --context is accepted as an optional override.
        let result = Cli::try_parse_from([
            "botforge",
            "test",
            "--test-config",
            "test.yaml",
            "--base-image",
            "base.qcow2",
            "--context",
            "/some/dir",
        ]);
        assert!(
            result.is_ok(),
            "CLI should parse with --context: {}",
            result.unwrap_err()
        );
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
        ]);
        // Should parse without a clap error (ssh-user and ssh-key are both optional).
        assert!(
            result.is_ok(),
            "CLI should parse without ssh-user/ssh-key: {}",
            result.unwrap_err()
        );
    }

    #[test]
    fn test_test_cli_memory_and_cpus_flags_accepted() {
        let result = Cli::try_parse_from([
            "botforge",
            "test",
            "--test-config",
            "test.yaml",
            "--base-image",
            "base.qcow2",
            "--memory",
            "8192",
            "--cpus",
            "auto",
        ]);
        assert!(
            result.is_ok(),
            "CLI should accept --memory and --cpus auto: {}",
            result.unwrap_err()
        );
    }

    #[test]
    fn test_test_cli_accepts_config_image_without_base_image_flag() {
        let result = Cli::try_parse_from(["botforge", "test", "--test-config", "test.yaml"]);
        assert!(
            result.is_ok(),
            "CLI should allow test-config image without --base-image: {}",
            result.unwrap_err()
        );
    }

    #[test]
    fn test_test_cli_shows_memory_and_cpus_in_help() {
        let help = Cli::try_parse_from(["botforge", "test", "--help"])
            .unwrap_err()
            .to_string();
        assert!(
            help.contains("--memory"),
            "--memory missing from test help: {help}"
        );
        assert!(
            help.contains("--cpus"),
            "--cpus missing from test help: {help}"
        );
    }

    #[test]
    fn test_resolve_test_base_image_uses_cli_override_before_config_image() {
        let repo = TempDir::new().unwrap();
        let manifest = Manifest::default();
        let cli_image = repo.path().join("cli.qcow2");
        let config_image = repo.path().join("config.qcow2");
        std::fs::write(&cli_image, "cli").unwrap();
        std::fs::write(&config_image, "config").unwrap();

        let resolved = resolve_test_base_image(
            repo.path(),
            &manifest,
            Some(cli_image.clone()),
            Some(&Reference::Repo {
                path: Some(PathBuf::from("config.qcow2")),
            }),
        )
        .unwrap();

        assert_eq!(resolved, cli_image);
    }

    #[test]
    fn test_resolve_test_base_image_uses_config_image_when_cli_absent() {
        let repo = TempDir::new().unwrap();
        let manifest = Manifest::default();
        let artifact = repo.path().join("build/artifact/base.qcow2");
        std::fs::create_dir_all(artifact.parent().unwrap()).unwrap();
        std::fs::write(&artifact, "base").unwrap();

        let resolved = resolve_test_base_image(
            repo.path(),
            &manifest,
            None,
            Some(&Reference::Repo {
                path: Some(PathBuf::from("build/artifact/base.qcow2")),
            }),
        )
        .unwrap();

        assert_eq!(resolved, artifact);
    }

    #[test]
    fn test_resolve_test_base_image_requires_cli_or_config_image() {
        let repo = TempDir::new().unwrap();
        let manifest = Manifest::default();
        let err = resolve_test_base_image(repo.path(), &manifest, None, None).unwrap_err();
        assert!(
            format!("{err:#}").contains("no base image provided"),
            "missing base image should be rejected: {err:#}"
        );
    }

    // ── resolve_test_iso_path tests ───────────────────────────────────────────

    #[test]
    fn test_resolve_iso_path_at_context_root_ref_resolves_to_absolute_path() {
        let repo = TempDir::new().unwrap();
        let manifest = Manifest::default();
        let iso = repo.path().join("build/foo.iso");
        std::fs::create_dir_all(iso.parent().unwrap()).unwrap();
        std::fs::write(&iso, "").unwrap();

        let resolved = resolve_test_iso_path(
            repo.path(),
            &manifest,
            std::path::Path::new("@://build/foo.iso"),
        )
        .unwrap();

        assert_eq!(resolved, iso);
    }

    #[test]
    fn test_resolve_iso_path_plain_relative_resolves_under_root() {
        let repo = TempDir::new().unwrap();
        let manifest = Manifest::default();

        // Does not require the file to exist — regression guard for plain paths.
        let resolved = resolve_test_iso_path(
            repo.path(),
            &manifest,
            std::path::Path::new("build/foo.iso"),
        )
        .unwrap();

        assert_eq!(resolved, repo.path().join("build/foo.iso"));
    }

    #[test]
    fn test_resolve_iso_path_absolute_path_returned_unchanged() {
        let repo = TempDir::new().unwrap();
        let manifest = Manifest::default();
        let abs = std::path::PathBuf::from("/tmp/some.iso");

        let resolved = resolve_test_iso_path(repo.path(), &manifest, &abs).unwrap();

        assert_eq!(resolved, abs);
    }

    #[test]
    fn test_resolve_iso_path_at_ref_rejects_oci_asset() {
        let repo = TempDir::new().unwrap();
        let manifest: Manifest = serde_yaml::from_str(
            "settings:\n  retries: 0\nassets:\n  my-image:\n    uri: oci://ghcr.io/example/img@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n    version: \"1\"\n",
        )
        .unwrap();

        let err = resolve_test_iso_path(repo.path(), &manifest, std::path::Path::new("@my-image"))
            .unwrap_err();

        let msg = format!("{err:#}");
        assert!(
            msg.contains("oci://") || msg.contains("OCI") || msg.contains("my-image"),
            "deny_kinds should reject oci:// assets: {msg}"
        );
    }

    #[test]
    fn test_resolve_test_base_image_uses_inline_assets_manifest() {
        let repo = TempDir::new().unwrap();
        let manifest: Manifest = serde_yaml::from_str(
            "settings:\n  retries: 0\nassets:\n  base:\n    uri: https://example.com/base.qcow2\n    version: \"1\"\n    checksum: sha256:34cb20b33d115697e75baf0d12172c7c3b42a5f04b047c64f38d0aa2b57c988f\n    filename: base.qcow2\n",
        )
        .unwrap();
        let cache = repo.path().join(".cache");
        let body = b"fake-qcow2-content";
        let checksum_dir = cache.join("blobs/sha256");
        std::fs::create_dir_all(&checksum_dir).unwrap();
        std::fs::write(
            checksum_dir.join("34cb20b33d115697e75baf0d12172c7c3b42a5f04b047c64f38d0aa2b57c988f"),
            body,
        )
        .unwrap();
        let base_images = cache.join("base-images");
        std::fs::create_dir_all(&base_images).unwrap();
        std::fs::write(base_images.join("base.qcow2"), body).unwrap();

        let resolved = Reference::parse("@base")
            .unwrap()
            .resolve_one_validated(
                &crate::resolver::ResolveFileContext {
                    context: repo.path(),
                    manifest: &manifest,
                    cache_dir_override: Some(cache.as_path()),
                },
                &crate::resolver::ResolveSpec {
                    deny_kinds: vec![crate::resolver::AssetKind::OciImage],
                    ..Default::default()
                },
            )
            .unwrap();

        assert_eq!(resolved, base_images.join("base.qcow2"));
    }
}
