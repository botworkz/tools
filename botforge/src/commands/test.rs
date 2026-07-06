use anyhow::{Context, Result};
use clap::Args;
use serde::Deserialize;
use std::collections::HashSet;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::Child;
use std::time::{Duration, Instant};

use crate::iso::{build_iso, detect_iso_tool, render_user_data, write_seed_files};
use crate::qemu::{create_overlay_image, qemu_run_args, require_kvm, spawn_qemu_with_log};
use crate::ssh::{
    journalctl_command, require_stable_ssh, scp_with_retry, ssh_with_retry, wait_for_ssh,
    SshOptions,
};
use crate::util::{create_temp_dir, ensure_command, resolve_under_root, unique_suffix};

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
    ports: Vec<u16>,
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

/// Where a test step executes: inside the guest (SSH) or on the harness host (local).
#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum StepTarget {
    /// Run via SSH inside the guest VM.
    Guest,
    /// Run locally in the botforge container (harness), reaching the guest only via forwarded
    /// `ports:`. This is the botforge container / harness where botforge itself runs — not the
    /// CI runner host.
    Host,
}

#[derive(Debug, Deserialize)]
struct TestStep {
    /// Where this step executes. Required; must be `guest` or `host`.
    #[serde(rename = "on")]
    target: StepTarget,
    name: String,
    /// Files to scp into the guest before running. Only valid on `on: guest` steps.
    #[serde(default)]
    uploads: Vec<TestUpload>,
    run: String,
    /// Interpreter used to execute `run:`. Mirrors GitHub Actions `shell:` semantics.
    ///
    /// Named shells: `bash` (default), `sh`, `python`.
    /// Custom template: any string containing `{0}`, e.g. `python3 -u {0}`.
    /// When absent, defaults to `bash --noprofile --norc -e -o pipefail {0}` with
    /// automatic `sh -e {0}` fallback if bash is not available.
    #[serde(default)]
    shell: Option<String>,
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
    validate_test_steps(&test_config.steps, &test_config.ports)?;
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

    for (step_idx, step) in config.steps.iter().enumerate() {
        match step.target {
            StepTarget::Guest => {
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

                let suffix = unique_suffix();
                let local_script = std::env::temp_dir()
                    .join(format!("botforge-step-{step_idx}-{suffix}.sh"));
                std::fs::write(&local_script, step.run.as_bytes()).with_context(|| {
                    format!("test step '{}': failed to write script file", step.name)
                })?;
                let remote_script =
                    format!("/tmp/botforge-step-{step_idx}-{suffix}.sh");

                let template = resolve_shell(step.shell.as_deref())
                    .expect("shell already validated at config load");

                let scp_result = scp_with_retry(
                    ssh,
                    &local_script,
                    &remote_script,
                    TEST_TRANSPORT_RETRIES,
                    TEST_TRANSPORT_RETRY_DELAY,
                )
                .with_context(|| {
                    format!("test step '{}' script upload failed", step.name)
                });

                let step_result = if scp_result.is_ok() {
                    let ssh_cmd = template
                        .iter()
                        .map(|a| {
                            if a == "{0}" {
                                shell_single_quote(&remote_script)
                            } else {
                                a.clone()
                            }
                        })
                        .collect::<Vec<_>>()
                        .join(" ");
                    ssh_with_retry(
                        ssh,
                        &ssh_cmd,
                        TEST_TRANSPORT_RETRIES,
                        TEST_TRANSPORT_RETRY_DELAY,
                        Duration::from_secs(300),
                    )
                    .with_context(|| format!("test step '{}' command failed", step.name))
                } else {
                    scp_result
                };

                // Best-effort cleanup: remote script, then local temp file.
                let _ = ssh_with_retry(
                    ssh,
                    &format!("rm -f {}", shell_single_quote(&remote_script)),
                    1,
                    Duration::from_secs(0),
                    Duration::from_secs(10),
                );
                let _ = std::fs::remove_file(&local_script);

                step_result?;
            }
            StepTarget::Host => {
                let template = resolve_shell(step.shell.as_deref())
                    .expect("shell already validated at config load");
                run_host_step(
                    &step.name,
                    &step.run,
                    repo_root,
                    Duration::from_secs(300),
                    &template,
                )
                .with_context(|| format!("test step '{}' command failed", step.name))?;
            }
        }
    }
    Ok(())
}

fn load_test_config(path: &Path) -> Result<TestConfig> {
    let yaml = std::fs::read_to_string(path)
        .with_context(|| format!("cannot read test config: {}", path.display()))?;
    serde_yaml::from_str(&yaml).with_context(|| format!("invalid test config: {}", path.display()))
}

fn validate_test_steps(steps: &[TestStep], ports: &[u16]) -> Result<()> {
    for step in steps {
        resolve_shell(step.shell.as_deref())
            .with_context(|| format!("test step '{}': invalid `shell:` value", step.name))?;
        if step.target == StepTarget::Host && !step.uploads.is_empty() {
            anyhow::bail!(
                "test step '{}': `uploads` is not valid on an `on: host` step; \
                 files are already local in the harness",
                step.name
            );
        }
    }
    let has_host_step = steps.iter().any(|s| s.target == StepTarget::Host);
    if has_host_step && ports.is_empty() {
        anyhow::bail!(
            "test config has `on: host` steps but no `ports:` are declared; \
             a host step reaches the guest only via forwarded ports"
        );
    }
    Ok(())
}

/// Run a step locally in the botforge container (harness) with a plain execution timeout.
/// `run` is written to a temp file and executed via `template` (argv with `{0}` slot).
/// The working directory is `repo_root`. Inherits the current process environment.
fn run_host_step(
    name: &str,
    run: &str,
    repo_root: &Path,
    timeout: Duration,
    template: &[String],
) -> Result<()> {
    let script =
        std::env::temp_dir().join(format!("botforge-host-step-{}.sh", unique_suffix()));
    std::fs::write(&script, run.as_bytes())
        .with_context(|| format!("failed to write script file for host step '{name}'"))?;

    let argv: Vec<String> = template
        .iter()
        .map(|a| {
            if a == "{0}" {
                script.to_string_lossy().into_owned()
            } else {
                a.clone()
            }
        })
        .collect();

    let mut child = std::process::Command::new(&argv[0])
        .args(&argv[1..])
        .current_dir(repo_root)
        .spawn()
        .with_context(|| format!("failed to spawn host step '{name}'"))?;

    let deadline = Instant::now() + timeout;
    let step_result = loop {
        match child
            .try_wait()
            .with_context(|| format!("failed to wait for host step '{name}'"))?
        {
            Some(status) => {
                if status.success() {
                    break Ok(());
                }
                break Err(anyhow::anyhow!(
                    "host step '{name}' exited with status: {status}"
                ));
            }
            None => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    break Err(anyhow::anyhow!(
                        "host step '{name}' timed out after {}s",
                        timeout.as_secs()
                    ));
                }
                std::thread::sleep(Duration::from_millis(200));
            }
        }
    };

    // Best-effort cleanup of temp script.
    let _ = std::fs::remove_file(&script);

    step_result
}

/// Resolve a step's `shell:` value into an argv template with a `{0}` slot.
///
/// Named shells (`bash`, `sh`, `python`) map to fixed GHA-compatible templates.
/// Custom templates must contain `{0}` as a placeholder for the script file path.
/// `None` (absent) returns the default `bash` template.
///
/// Returns `Err` for: unknown single-token named shell, or a custom multi-token
/// shell string that does not contain `{0}`.
fn resolve_shell(shell: Option<&str>) -> Result<Vec<String>> {
    match shell {
        None | Some("bash") => Ok(vec![
            "bash".to_string(),
            "--noprofile".to_string(),
            "--norc".to_string(),
            "-e".to_string(),
            "-o".to_string(),
            "pipefail".to_string(),
            "{0}".to_string(),
        ]),
        Some("sh") => Ok(vec!["sh".to_string(), "-e".to_string(), "{0}".to_string()]),
        Some("python") => Ok(vec!["python3".to_string(), "{0}".to_string()]),
        Some(custom) => {
            if custom.contains("{0}") {
                Ok(custom.split_whitespace().map(str::to_string).collect())
            } else if custom.split_whitespace().count() <= 1 {
                anyhow::bail!(
                    "unknown named shell '{}'; supported named shells: bash, sh, python. \
                     For a custom interpreter use the '{{0}}' placeholder form, \
                     e.g. '{} {{0}}'",
                    custom,
                    custom
                )
            } else {
                anyhow::bail!(
                    "custom shell '{}' does not contain the '{{0}}' placeholder; \
                     '{{0}}' must appear in the shell template to indicate where \
                     the script file path is substituted",
                    custom
                )
            }
        }
    }
}

fn validate_test_ports(ports: &[u16], ssh_port: u16) -> Result<()> {
    let mut seen = HashSet::new();
    for port in ports {
        if *port == 0 {
            anyhow::bail!("invalid test config port 0: ports must be in 1..=65535");
        }
        if *port == ssh_port {
            anyhow::bail!("invalid test config port {port}: duplicates configured ssh port");
        }
        if *port == 22 {
            anyhow::bail!("invalid test config port 22: guest ssh is forwarded automatically");
        }
        if !seen.insert(*port) {
            anyhow::bail!(
                "invalid test config port {port}: duplicate ports are not allowed in `ports`"
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
    use super::{
        default_bootstrap_path, resolve_shell, run_host_step, validate_test_ports,
        validate_test_steps, StepTarget, TestConfig, TestIso, TestStep, TestUpload,
    };
    use std::path::PathBuf;
    use std::time::Duration;

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
    fn test_config_ports_parse_and_default() {
        let with_ports: TestConfig = serde_yaml::from_str(
            r#"
ports:
  - 80
"#,
        )
        .unwrap();
        assert_eq!(with_ports.ports, vec![80]);

        let without_ports: TestConfig = serde_yaml::from_str("steps: []\n").unwrap();
        assert!(without_ports.ports.is_empty());
    }

    #[test]
    fn test_config_ports_validation_rejects_invalid_and_duplicate_values() {
        assert!(validate_test_ports(&[0], 2222).is_err());
        assert!(validate_test_ports(&[2222], 2222).is_err());
        assert!(validate_test_ports(&[22], 2222).is_err());
        assert!(validate_test_ports(&[80, 80], 2222).is_err());
    }

    // --- step deserialization ---

    #[test]
    fn test_step_parses_guest_step() {
        let config: TestConfig = serde_yaml::from_str(
            r#"
steps:
  - on: guest
    name: goss
    run: goss -g /path/goss.yaml validate
"#,
        )
        .unwrap();

        assert_eq!(config.steps.len(), 1);
        assert_eq!(config.steps[0].target, StepTarget::Guest);
        assert_eq!(config.steps[0].name, "goss");
        assert_eq!(config.steps[0].run, "goss -g /path/goss.yaml validate");
        assert!(config.steps[0].uploads.is_empty());
    }

    #[test]
    fn test_step_parses_host_step() {
        let config: TestConfig = serde_yaml::from_str(
            r#"
ports:
  - 80
steps:
  - on: host
    name: vm-narrative
    run: bash smoke/vm-narrative.sh 127.0.0.1
"#,
        )
        .unwrap();

        assert_eq!(config.steps.len(), 1);
        assert_eq!(config.steps[0].target, StepTarget::Host);
        assert_eq!(config.steps[0].name, "vm-narrative");
        assert_eq!(config.steps[0].run, "bash smoke/vm-narrative.sh 127.0.0.1");
    }

    #[test]
    fn test_step_parses_guest_step_with_uploads() {
        let config: TestConfig = serde_yaml::from_str(
            r#"
steps:
  - on: guest
    name: upload-and-run
    uploads:
      - src: local/file.sh
        dest: /tmp/file.sh
    run: bash /tmp/file.sh
"#,
        )
        .unwrap();

        assert_eq!(config.steps[0].uploads.len(), 1);
        assert_eq!(config.steps[0].uploads[0].src, PathBuf::from("local/file.sh"));
        assert_eq!(config.steps[0].uploads[0].dest, "/tmp/file.sh");
    }

    #[test]
    fn test_step_parses_interleaved_guest_and_host_steps_in_order() {
        let config: TestConfig = serde_yaml::from_str(
            r#"
ports:
  - 80
steps:
  - on: guest
    name: goss
    run: goss -g /path/goss.yaml validate
  - on: guest
    name: flip-spigot
    run: sudo cp /etc/envoy/rds/active.ingress.yaml /etc/envoy/rds/active.yaml
  - on: host
    name: vm-narrative
    run: bash smoke/vm-narrative.sh 127.0.0.1
  - on: guest
    name: flip-spigot-back
    run: sudo cp /etc/envoy/rds/active.holding.yaml /etc/envoy/rds/active.yaml
"#,
        )
        .unwrap();

        assert_eq!(config.steps.len(), 4);
        assert_eq!(config.steps[0].target, StepTarget::Guest);
        assert_eq!(config.steps[1].target, StepTarget::Guest);
        assert_eq!(config.steps[2].target, StepTarget::Host);
        assert_eq!(config.steps[3].target, StepTarget::Guest);
    }

    #[test]
    fn test_step_rejects_missing_on_field() {
        let result: Result<TestConfig, _> = serde_yaml::from_str(
            r#"
steps:
  - name: no-on-field
    run: echo hello
"#,
        );
        assert!(result.is_err());
    }

    #[test]
    fn test_step_rejects_invalid_on_value() {
        let result: Result<TestConfig, _> = serde_yaml::from_str(
            r#"
steps:
  - on: invalid
    name: bad-step
    run: echo hello
"#,
        );
        assert!(result.is_err());
    }

    // --- step validation ---

    fn make_step(target: StepTarget, name: &str, with_uploads: bool) -> TestStep {
        TestStep {
            target,
            name: name.to_string(),
            run: "echo ok".to_string(),
            shell: None,
            uploads: if with_uploads {
                vec![TestUpload {
                    src: PathBuf::from("src/file"),
                    dest: "/tmp/file".to_string(),
                }]
            } else {
                vec![]
            },
        }
    }

    #[test]
    fn test_validate_steps_accepts_guest_with_uploads() {
        let steps = vec![make_step(StepTarget::Guest, "s", true)];
        assert!(validate_test_steps(&steps, &[80]).is_ok());
    }

    #[test]
    fn test_validate_steps_accepts_host_without_uploads() {
        let steps = vec![make_step(StepTarget::Host, "s", false)];
        assert!(validate_test_steps(&steps, &[80]).is_ok());
    }

    #[test]
    fn test_validate_steps_rejects_uploads_on_host_step() {
        let steps = vec![make_step(StepTarget::Host, "bad", true)];
        let err = validate_test_steps(&steps, &[80]).unwrap_err();
        assert!(
            err.to_string().contains("uploads"),
            "error should mention 'uploads': {err}"
        );
        assert!(
            err.to_string().contains("bad"),
            "error should mention step name: {err}"
        );
    }

    #[test]
    fn test_validate_steps_rejects_host_step_without_ports() {
        let steps = vec![make_step(StepTarget::Host, "edge", false)];
        let err = validate_test_steps(&steps, &[]).unwrap_err();
        assert!(
            err.to_string().contains("ports"),
            "error should mention 'ports': {err}"
        );
    }

    #[test]
    fn test_validate_steps_accepts_empty_steps_without_ports() {
        assert!(validate_test_steps(&[], &[]).is_ok());
    }

    #[test]
    fn test_validate_steps_accepts_guest_only_without_ports() {
        let steps = vec![make_step(StepTarget::Guest, "s", false)];
        assert!(validate_test_steps(&steps, &[]).is_ok());
    }

    // --- shell resolver ---

    #[test]
    fn test_resolve_shell_absent_returns_bash_template() {
        let tmpl = resolve_shell(None).unwrap();
        assert_eq!(
            tmpl,
            vec!["bash", "--noprofile", "--norc", "-e", "-o", "pipefail", "{0}"]
        );
    }

    #[test]
    fn test_resolve_shell_bash_returns_bash_template() {
        let tmpl = resolve_shell(Some("bash")).unwrap();
        assert_eq!(
            tmpl,
            vec!["bash", "--noprofile", "--norc", "-e", "-o", "pipefail", "{0}"]
        );
    }

    #[test]
    fn test_resolve_shell_sh_returns_sh_template() {
        let tmpl = resolve_shell(Some("sh")).unwrap();
        assert_eq!(tmpl, vec!["sh", "-e", "{0}"]);
    }

    #[test]
    fn test_resolve_shell_python_returns_python3_template() {
        let tmpl = resolve_shell(Some("python")).unwrap();
        assert_eq!(tmpl, vec!["python3", "{0}"]);
    }

    #[test]
    fn test_resolve_shell_custom_with_placeholder_is_split() {
        let tmpl = resolve_shell(Some("python3 -u {0}")).unwrap();
        assert_eq!(tmpl, vec!["python3", "-u", "{0}"]);
    }

    #[test]
    fn test_resolve_shell_custom_without_placeholder_is_error() {
        let err = resolve_shell(Some("python3 -u")).unwrap_err();
        assert!(
            err.to_string().contains("{0}"),
            "error should mention '{{0}}': {err}"
        );
    }

    #[test]
    fn test_resolve_shell_unknown_named_shell_is_error() {
        let err = resolve_shell(Some("fish")).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("fish"),
            "error should mention the shell name: {msg}"
        );
        assert!(
            msg.contains("bash") && msg.contains("sh") && msg.contains("python"),
            "error should list supported shells: {msg}"
        );
    }

    // --- shell deserialization ---

    #[test]
    fn test_step_parses_shell_python() {
        let config: TestConfig = serde_yaml::from_str(
            r#"
steps:
  - on: guest
    name: py-step
    shell: python
    run: print("hello")
"#,
        )
        .unwrap();
        assert_eq!(config.steps[0].shell.as_deref(), Some("python"));
    }

    #[test]
    fn test_step_parses_without_shell_defaults_to_none() {
        let config: TestConfig = serde_yaml::from_str(
            r#"
steps:
  - on: guest
    name: no-shell
    run: echo hello
"#,
        )
        .unwrap();
        assert!(config.steps[0].shell.is_none());
    }

    // --- {0} substitution ---

    #[test]
    fn test_apply_shell_template_substitutes_placeholder() {
        let tmpl = resolve_shell(None).unwrap();
        let argv: Vec<String> = tmpl
            .iter()
            .map(|a| {
                if a == "{0}" {
                    "/tmp/my-script.sh".to_string()
                } else {
                    a.clone()
                }
            })
            .collect();
        assert_eq!(
            argv,
            vec![
                "bash",
                "--noprofile",
                "--norc",
                "-e",
                "-o",
                "pipefail",
                "/tmp/my-script.sh"
            ]
        );
    }

    #[test]
    fn test_apply_sh_template_substitutes_placeholder() {
        let tmpl = resolve_shell(Some("sh")).unwrap();
        let argv: Vec<String> = tmpl
            .iter()
            .map(|a| {
                if a == "{0}" {
                    "/tmp/step.sh".to_string()
                } else {
                    a.clone()
                }
            })
            .collect();
        assert_eq!(argv, vec!["sh", "-e", "/tmp/step.sh"]);
    }

    // --- host step execution ---

    #[test]
    fn test_host_step_sh_false_fails() {
        let dir = tempfile::tempdir().unwrap();
        let tmpl = resolve_shell(Some("sh")).unwrap();
        let err =
            run_host_step("fail-step", "false", dir.path(), Duration::from_secs(10), &tmpl)
                .unwrap_err();
        assert!(
            err.to_string().contains("fail-step"),
            "error should mention step name: {err}"
        );
    }

    #[test]
    fn test_host_step_exit_nonzero_fails() {
        let dir = tempfile::tempdir().unwrap();
        let tmpl = resolve_shell(Some("sh")).unwrap();
        let err =
            run_host_step("bad", "exit 3", dir.path(), Duration::from_secs(10), &tmpl)
                .unwrap_err();
        assert!(err.to_string().contains("bad"), "error should mention step name: {err}");
    }

    #[test]
    fn test_host_step_default_set_e_fails_on_mid_script_error() {
        // Under the default bash template (-e -o pipefail), a non-final failing
        // command must fail the step even though the last command would succeed.
        let dir = tempfile::tempdir().unwrap();
        let tmpl = resolve_shell(None).unwrap();
        let err = run_host_step(
            "mid-fail",
            "false\necho ok\n",
            dir.path(),
            Duration::from_secs(10),
            &tmpl,
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("mid-fail"),
            "error should mention step name: {err}"
        );
    }

    #[test]
    fn test_host_step_success() {
        let dir = tempfile::tempdir().unwrap();
        let tmpl = resolve_shell(None).unwrap();
        assert!(run_host_step("ok", "true", dir.path(), Duration::from_secs(10), &tmpl).is_ok());
    }

    #[test]
    fn test_validate_steps_rejects_bad_shell() {
        let mut step = make_step(StepTarget::Guest, "bad-shell", false);
        step.shell = Some("fish".to_string());
        let err = validate_test_steps(&[step], &[]).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("fish"),
            "error should mention shell name: {msg}"
        );
    }
}
