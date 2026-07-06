use anyhow::{Context, Result};
use clap::Args;
use serde::Deserialize;
use std::collections::HashSet;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::Child;
use std::time::Duration;

use crate::iso::{build_iso, detect_iso_tool, render_user_data, write_seed_files};
use crate::qemu::{create_overlay_image, qemu_run_args, require_kvm, spawn_qemu_with_log, PortSpec};
use crate::ssh::{
    journalctl_command, require_stable_ssh, scp_with_retry, ssh_with_retry, wait_for_ssh,
    SshOptions,
};
use crate::util::{create_temp_dir, ensure_command, resolve_under_root};

const TEST_SSH_READY_TIMEOUT: Duration = Duration::from_secs(300);
const TEST_CLOUD_INIT_TIMEOUT: Duration = Duration::from_secs(300);
const TEST_TRANSPORT_RETRIES: usize = 10;
const TEST_TRANSPORT_RETRY_DELAY: Duration = Duration::from_secs(2);
const TEST_STABLE_SSH_ATTEMPTS: usize = 5;
const TEST_STABLE_SSH_REQUIRED: usize = 2;

#[derive(Args, Debug)]
pub(crate) struct TestArgs {
    /// Path to test.yaml config.
    #[arg(long = "test-config", required = true)]
    test_config: PathBuf,
    /// Base qcow2 image path.
    #[arg(long, required = true)]
    base_image: PathBuf,
    /// SSH private key path for guest access.
    #[arg(long, required = true)]
    ssh_key: PathBuf,
    /// SSH host forwarded port.
    #[arg(long, default_value_t = 2222)]
    ssh_port: u16,
    /// SSH host.
    #[arg(long, default_value = "127.0.0.1")]
    ssh_host: String,
    /// SSH user.
    #[arg(long, default_value = "bot")]
    ssh_user: String,
    /// Repo root for resolving relative test paths (default: current dir).
    #[arg(long)]
    repo_root: Option<PathBuf>,
    /// Leave VM running and preserve overlay on exit.
    #[arg(long)]
    keep_running: bool,
}

#[derive(Debug, Deserialize, Default)]
struct TestConfig {
    #[serde(default)]
    isos: Vec<TestIso>,
    #[serde(default)]
    ports: Vec<PortSpec>,
    #[serde(default)]
    steps: Vec<TestStep>,
    #[serde(default)]
    diagnostics_units: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum TestIso {
    Attach(PathBuf),
    Bootstrap {
        path: PathBuf,
        label: String,
        mount: PathBuf,
        #[serde(default = "default_bootstrap_path")]
        bootstrap: PathBuf,
    },
}

#[derive(Debug)]
struct TestIsoBootstrap {
    label: String,
    mount: PathBuf,
    bootstrap: PathBuf,
}

fn default_bootstrap_path() -> PathBuf {
    PathBuf::from("bootstrap.sh")
}

#[derive(Debug, Deserialize)]
struct TestStep {
    name: String,
    #[serde(default)]
    uploads: Vec<TestUpload>,
    run: String,
}

#[derive(Debug, Deserialize)]
struct TestUpload {
    src: PathBuf,
    dest: String,
}

pub(crate) fn cmd_test(args: TestArgs) -> Result<()> {
    require_kvm()?;
    ensure_command("qemu-system-x86_64")?;
    ensure_command("qemu-img")?;
    ensure_command("ssh")?;
    ensure_command("scp")?;
    detect_iso_tool()?;

    let repo_root = std::fs::canonicalize(
        args.repo_root
            .unwrap_or(std::env::current_dir().context("failed to determine current directory")?),
    )
    .context("failed to resolve repo root")?;
    let test_config_path = resolve_under_root(&repo_root, args.test_config);
    let base_image = resolve_under_root(&repo_root, args.base_image);
    let ssh_key = resolve_under_root(&repo_root, args.ssh_key);
    let ssh_pub = PathBuf::from(format!("{}.pub", ssh_key.display()));

    let test_config = load_test_config(&test_config_path)?;
    let build_dir = repo_root.join("build");
    std::fs::create_dir_all(&build_dir)
        .with_context(|| format!("cannot create build dir: {}", build_dir.display()))?;
    let overlay_image = build_dir.join("test-overlay.qcow2");
    let seed_iso = build_dir.join("test-seed.iso");
    let vm_log = build_dir.join("test-vm.log");
    let seed_dir = create_temp_dir("botforge-test-seed")?;

    let ssh_public_key = std::fs::read_to_string(&ssh_pub)
        .with_context(|| format!("cannot read SSH public key: {}", ssh_pub.display()))?;
    let user_data = render_user_data(None, ssh_public_key.trim(), Some(args.ssh_user.as_str()));
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
        user: args.ssh_user.clone(),
        key: ssh_key.clone(),
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

fn run_test_flow(
    repo_root: &Path,
    config: &TestConfig,
    ssh: &SshOptions,
    bootstraps: &[TestIsoBootstrap],
) -> Result<()> {
    wait_for_ssh(ssh, TEST_SSH_READY_TIMEOUT)?;
    ssh_with_retry(
        ssh,
        "sudo cloud-init status --wait",
        TEST_TRANSPORT_RETRIES,
        TEST_TRANSPORT_RETRY_DELAY,
        TEST_CLOUD_INIT_TIMEOUT,
    )?;
    require_stable_ssh(ssh, TEST_STABLE_SSH_ATTEMPTS, TEST_STABLE_SSH_REQUIRED)?;

    for bootstrap in bootstraps {
        let mount = shell_single_quote(&bootstrap.mount.display().to_string());
        let label = shell_single_quote(&bootstrap.label);
        let mount_cmd = format!("sudo mkdir -p {mount} && sudo mount -L {label} -o ro {mount}");
        ssh_with_retry(
            ssh,
            &mount_cmd,
            TEST_TRANSPORT_RETRIES,
            TEST_TRANSPORT_RETRY_DELAY,
            TEST_CLOUD_INIT_TIMEOUT,
        )
        .with_context(|| format!("iso bootstrap mount failed for label {}", bootstrap.label))?;

        let script_path = bootstrap.mount.join(&bootstrap.bootstrap);
        let run_cmd = format!(
            "sudo bash {}",
            shell_single_quote(&script_path.display().to_string())
        );
        ssh_with_retry(
            ssh,
            &run_cmd,
            TEST_TRANSPORT_RETRIES,
            TEST_TRANSPORT_RETRY_DELAY,
            TEST_CLOUD_INIT_TIMEOUT,
        )
        .with_context(|| format!("iso bootstrap script failed for label {}", bootstrap.label))?;
    }

    for step in &config.steps {
        for upload in &step.uploads {
            let src = resolve_under_root(repo_root, upload.src.clone());
            scp_with_retry(
                ssh,
                &src,
                &upload.dest,
                TEST_TRANSPORT_RETRIES,
                TEST_TRANSPORT_RETRY_DELAY,
            )
            .with_context(|| format!("test step '{}' upload failed", step.name))?;
        }
        ssh_with_retry(
            ssh,
            &step.run,
            TEST_TRANSPORT_RETRIES,
            TEST_TRANSPORT_RETRY_DELAY,
            Duration::from_secs(300),
        )
        .with_context(|| format!("test step '{}' command failed", step.name))?;
    }
    Ok(())
}

fn load_test_config(path: &Path) -> Result<TestConfig> {
    let yaml = std::fs::read_to_string(path)
        .with_context(|| format!("cannot read test config: {}", path.display()))?;
    serde_yaml::from_str(&yaml).with_context(|| format!("invalid test config: {}", path.display()))
}

fn validate_test_ports(ports: &[PortSpec], ssh_port: u16) -> Result<()> {
    let mut seen = HashSet::new();
    for spec in ports {
        if spec.port == 0 {
            anyhow::bail!("invalid test config port 0: ports must be in 1..=65535");
        }
        if spec.port == ssh_port {
            anyhow::bail!(
                "invalid test config port {}: duplicates configured ssh port",
                spec.port
            );
        }
        if spec.port == 22 {
            anyhow::bail!("invalid test config port 22: guest ssh is forwarded automatically");
        }
        if !seen.insert(spec.port) {
            anyhow::bail!(
                "invalid test config port {}: duplicate port numbers are not allowed in `ports` \
                 (binds on different addresses may still conflict at QEMU startup)",
                spec.port
            );
        }
    }
    Ok(())
}

fn collect_test_diagnostics(ssh: &SshOptions, units: &[String]) {
    let _ = ssh_with_retry(
        ssh,
        "systemctl --failed",
        1,
        Duration::from_secs(0),
        Duration::from_secs(10),
    );
    let _ = ssh_with_retry(
        ssh,
        &journalctl_command(units),
        1,
        Duration::from_secs(0),
        Duration::from_secs(10),
    );
    let _ = ssh_with_retry(
        ssh,
        "cloud-init status --long",
        1,
        Duration::from_secs(0),
        Duration::from_secs(10),
    );
}

fn print_log_tail(path: &Path, line_count: usize) {
    let Ok(file) = File::open(path) else {
        return;
    };
    let lines: Vec<String> = BufReader::new(file)
        .lines()
        .map_while(|line| line.ok())
        .collect();
    let start = lines.len().saturating_sub(line_count);
    for line in &lines[start..] {
        eprintln!("{line}");
    }
}

fn cleanup_test(vm_child: &mut Option<Child>, overlay_image: &Path) {
    if let Some(child) = vm_child.as_mut() {
        let _ = child.kill();
        let _ = child.wait();
    }
    *vm_child = None;
    let _ = std::fs::remove_file(overlay_image);
}

fn shell_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

#[cfg(test)]
mod tests {
    use super::{default_bootstrap_path, validate_test_ports, TestConfig, TestIso};
    use crate::qemu::PortSpec;
    use std::path::PathBuf;

    fn loopback(port: u16) -> PortSpec {
        PortSpec {
            addr: "127.0.0.1".into(),
            port,
        }
    }

    #[test]
    fn test_config_isos_parses_legacy_and_bootstrap_shapes() {
        let config: TestConfig = serde_yaml::from_str(
            r#"
isos:
  - some/legacy.iso
  - path: some/payload.iso
    label: botwork-payload
    mount: /mnt/botwork-payload
"#,
        )
        .unwrap();

        assert_eq!(config.isos.len(), 2);
        match &config.isos[0] {
            TestIso::Attach(path) => assert_eq!(path, &PathBuf::from("some/legacy.iso")),
            TestIso::Bootstrap { .. } => panic!("expected legacy iso entry"),
        }
        match &config.isos[1] {
            TestIso::Bootstrap {
                path,
                label,
                mount,
                bootstrap,
            } => {
                assert_eq!(path, &PathBuf::from("some/payload.iso"));
                assert_eq!(label, "botwork-payload");
                assert_eq!(mount, &PathBuf::from("/mnt/botwork-payload"));
                assert_eq!(bootstrap, &default_bootstrap_path());
            }
            TestIso::Attach(_) => panic!("expected bootstrap iso entry"),
        }
    }

    #[test]
    fn test_config_isos_parses_bootstrap_override() {
        let config: TestConfig = serde_yaml::from_str(
            r#"
isos:
  - path: other.iso
    label: lbl
    mount: /mnt/other
    bootstrap: custom-init.sh
"#,
        )
        .unwrap();

        match &config.isos[0] {
            TestIso::Bootstrap { bootstrap, .. } => {
                assert_eq!(bootstrap, &PathBuf::from("custom-init.sh"))
            }
            TestIso::Attach(_) => panic!("expected bootstrap iso entry"),
        }
    }

    #[test]
    fn test_config_isos_parses_empty_list() {
        let config: TestConfig = serde_yaml::from_str("isos: []\n").unwrap();
        assert!(config.isos.is_empty());
    }

    #[test]
    fn test_config_ports_integer_parses_to_loopback() {
        let config: TestConfig = serde_yaml::from_str(
            r#"
ports:
  - 80
"#,
        )
        .unwrap();
        assert_eq!(config.ports.len(), 1);
        assert_eq!(config.ports[0], loopback(80));
    }

    #[test]
    fn test_config_ports_string_parses_to_custom_addr() {
        let config: TestConfig = serde_yaml::from_str(
            r#"
ports:
  - "0.0.0.0:9901"
"#,
        )
        .unwrap();
        assert_eq!(config.ports.len(), 1);
        assert_eq!(
            config.ports[0],
            PortSpec {
                addr: "0.0.0.0".into(),
                port: 9901
            }
        );
    }

    #[test]
    fn test_config_ports_explicit_loopback_string() {
        let config: TestConfig = serde_yaml::from_str(
            r#"
ports:
  - "127.0.0.1:80"
"#,
        )
        .unwrap();
        assert_eq!(config.ports[0], loopback(80));
    }

    #[test]
    fn test_config_ports_mixed_int_and_string() {
        let config: TestConfig = serde_yaml::from_str(
            r#"
ports:
  - 80
  - "0.0.0.0:9901"
"#,
        )
        .unwrap();
        assert_eq!(config.ports.len(), 2);
        assert_eq!(config.ports[0], loopback(80));
        assert_eq!(
            config.ports[1],
            PortSpec {
                addr: "0.0.0.0".into(),
                port: 9901
            }
        );
    }

    #[test]
    fn test_config_ports_default_is_empty() {
        let config: TestConfig = serde_yaml::from_str("steps: []\n").unwrap();
        assert!(config.ports.is_empty());
    }

    #[test]
    fn test_config_ports_malformed_string_rejected() {
        assert!(serde_yaml::from_str::<TestConfig>("ports:\n  - \"noport\"\n").is_err());
        assert!(serde_yaml::from_str::<TestConfig>("ports:\n  - \":80\"\n").is_err());
        assert!(serde_yaml::from_str::<TestConfig>("ports:\n  - \"0.0.0.0:notanumber\"\n").is_err());
        assert!(serde_yaml::from_str::<TestConfig>("ports:\n  - \"0.0.0.0:99999\"\n").is_err());
    }

    #[test]
    fn test_config_ports_validation_rejects_invalid_and_duplicate_values() {
        assert!(validate_test_ports(&[loopback(0)], 2222).is_err());
        assert!(validate_test_ports(&[loopback(2222)], 2222).is_err());
        assert!(validate_test_ports(&[loopback(22)], 2222).is_err());
        assert!(validate_test_ports(&[loopback(80), loopback(80)], 2222).is_err());
        // duplicate port number regardless of address
        assert!(
            validate_test_ports(
                &[
                    loopback(80),
                    PortSpec {
                        addr: "0.0.0.0".into(),
                        port: 80
                    }
                ],
                2222
            )
            .is_err()
        );
    }

    #[test]
    fn test_config_ports_validation_accepts_distinct_ports() {
        assert!(
            validate_test_ports(
                &[
                    loopback(80),
                    PortSpec {
                        addr: "0.0.0.0".into(),
                        port: 9901
                    }
                ],
                2222
            )
            .is_ok()
        );
    }
}
