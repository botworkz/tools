use anyhow::{Context, Result};
use clap::Args;
use serde::{Deserialize, Serialize};
use serde_yaml::Value;
use std::collections::{BTreeMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, IsTerminal, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

use crate::iso::{build_iso, detect_iso_tool, render_user_data, write_seed_files};
use crate::qemu::{
    create_overlay_image, qemu_run_args, require_kvm, spawn_qemu_with_log, PortSpec,
};
use crate::ssh::{
    journalctl_command, require_stable_ssh, scp_with_retry, ssh_capture_stdout, ssh_command_args,
    ssh_with_retry, wait_for_ssh, SshOptions,
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
    /// Repo root for resolving relative test paths and `uses:` step includes.
    #[arg(long, required = true)]
    repo_root: PathBuf,
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

#[derive(Debug, Deserialize, Default)]
struct RawTestConfig {
    #[serde(default)]
    isos: Vec<TestIso>,
    #[serde(default)]
    ports: Vec<PortSpec>,
    #[serde(default)]
    steps: Vec<RawTestStep>,
    #[serde(default)]
    diagnostics_units: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum RawTestStep {
    Step(TestStep),
    Include(TestStepInclude),
}

#[derive(Debug, Deserialize)]
struct TestStepInclude {
    uses: String,
    #[serde(default)]
    inputs: BTreeMap<String, String>,
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

    let repo_root = std::fs::canonicalize(args.repo_root).context("failed to resolve repo root")?;
    let test_config_path = resolve_under_root(&repo_root, args.test_config);
    let base_image = resolve_under_root(&repo_root, args.base_image);
    let ssh_key = resolve_under_root(&repo_root, args.ssh_key);
    let ssh_pub = PathBuf::from(format!("{}.pub", ssh_key.display()));

    let test_config = load_test_config(&repo_root, &test_config_path)?;
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
    let step_log_dir = repo_root.join("build").join("logs");
    std::fs::create_dir_all(&step_log_dir).with_context(|| {
        format!(
            "cannot create test step log dir: {}",
            step_log_dir.display()
        )
    })?;
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

    // Shared ordered env map threaded across all steps (both guest and host).
    let mut accumulated_env: Vec<(String, String)> = Vec::new();

    for (step_idx, step) in config.steps.iter().enumerate() {
        let step_log_path = step_log_path(&step_log_dir, step_idx, &step.name);
        // The file is created by StepLogWriter::create inside each step runner;
        // no pre-creation needed here (the directory was already created above).
        print_step_title(step_idx, &step.name);
        let step_result = match step.target {
            StepTarget::Guest => (|| -> Result<()> {
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
                let local_script =
                    std::env::temp_dir().join(format!("botforge-step-{step_idx}-{suffix}.sh"));
                let remote_script = format!("/tmp/botforge-step-{step_idx}-{suffix}.sh");
                let remote_env_path = format!("/tmp/botforge-env-{step_idx}-{suffix}");

                // Prepend env preamble (exports + BOTFORGE_ENV setup) to the script body.
                let preamble = build_guest_env_preamble(&accumulated_env, &remote_env_path);
                let script_content = format!("{preamble}{}", step.run);
                std::fs::write(&local_script, script_content.as_bytes()).with_context(|| {
                    format!("test step '{}': failed to write script file", step.name)
                })?;

                let template = resolve_shell(step.shell.as_deref())
                    .expect("shell already validated at config load");

                let scp_result = scp_with_retry(
                    ssh,
                    &local_script,
                    &remote_script,
                    TEST_TRANSPORT_RETRIES,
                    TEST_TRANSPORT_RETRY_DELAY,
                )
                .with_context(|| format!("test step '{}' script upload failed", step.name));

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
                    run_ssh_step_with_step_log(
                        ssh_command_args(ssh, &ssh_cmd, Duration::from_secs(300).as_secs()),
                        &step_log_path,
                        TEST_TRANSPORT_RETRIES,
                        TEST_TRANSPORT_RETRY_DELAY,
                    )
                    .with_context(|| format!("test step '{}' command failed", step.name))
                } else {
                    scp_result
                };

                // On success, read back the remote env file and merge into accumulated env.
                if step_result.is_ok() {
                    if let Ok(env_contents) = ssh_capture_stdout(
                        ssh,
                        &format!("cat {}", shell_single_quote(&remote_env_path)),
                        1,
                        Duration::from_secs(0),
                        Duration::from_secs(10),
                    ) {
                        if let Ok(new_entries) = parse_env_file(&env_contents) {
                            env_merge(&mut accumulated_env, new_entries);
                        }
                    }
                }

                // Best-effort cleanup: remote env file, remote script, then local temp file.
                let _ = ssh_with_retry(
                    ssh,
                    &format!(
                        "rm -f {} {}",
                        shell_single_quote(&remote_env_path),
                        shell_single_quote(&remote_script)
                    ),
                    1,
                    Duration::from_secs(0),
                    Duration::from_secs(10),
                );
                let _ = std::fs::remove_file(&local_script);

                step_result
            })(),
            StepTarget::Host => {
                let template = resolve_shell(step.shell.as_deref())
                    .expect("shell already validated at config load");
                let suffix = unique_suffix();
                let env_file = std::env::temp_dir().join(format!("botforge-host-env-{suffix}"));
                let step_result = run_host_step(
                    &step.name,
                    &step.run,
                    repo_root,
                    Duration::from_secs(300),
                    &template,
                    &accumulated_env,
                    HostStepFiles {
                        env_file: &env_file,
                        log_path: &step_log_path,
                    },
                )
                .with_context(|| format!("test step '{}' command failed", step.name));

                // On success, parse the local env file and merge into accumulated env.
                if step_result.is_ok() {
                    if let Ok(contents) = std::fs::read_to_string(&env_file) {
                        if let Ok(new_entries) = parse_env_file(&contents) {
                            env_merge(&mut accumulated_env, new_entries);
                        }
                    }
                }

                // Best-effort cleanup of the local env file.
                let _ = std::fs::remove_file(&env_file);

                step_result
            }
        };
        print_step_status(step_idx, &step.name, step_result.is_ok());
        step_result?;
    }
    Ok(())
}

fn load_test_config(repo_root: &Path, path: &Path) -> Result<TestConfig> {
    let yaml = std::fs::read_to_string(path)
        .with_context(|| format!("cannot read test config: {}", path.display()))?;
    let raw: RawTestConfig = serde_yaml::from_str(&yaml)
        .with_context(|| format!("invalid test config: {}", path.display()))?;
    let mut include_stack = Vec::new();
    Ok(TestConfig {
        isos: raw.isos,
        ports: raw.ports,
        steps: expand_test_steps(repo_root, path, raw.steps, &mut include_stack)?,
        diagnostics_units: raw.diagnostics_units,
    })
}

fn expand_test_steps(
    repo_root: &Path,
    current_file: &Path,
    steps: Vec<RawTestStep>,
    include_stack: &mut Vec<PathBuf>,
) -> Result<Vec<TestStep>> {
    let mut expanded = Vec::new();
    for step in steps {
        match step {
            RawTestStep::Step(step) => expanded.push(step),
            RawTestStep::Include(include) => {
                let include_path =
                    resolve_uses_path(repo_root, &include.uses).with_context(|| {
                        format!("invalid test step include in {}", current_file.display())
                    })?;
                if include_stack.contains(&include_path) {
                    let mut chain: Vec<String> = include_stack
                        .iter()
                        .map(|path| path.display().to_string())
                        .collect();
                    chain.push(include_path.display().to_string());
                    anyhow::bail!("cyclic test step include detected: {}", chain.join(" -> "));
                }
                include_stack.push(include_path.clone());
                let nested =
                    load_test_steps_fragment(&include_path, &include.inputs).and_then(|steps| {
                        expand_test_steps(repo_root, &include_path, steps, include_stack)
                    });
                include_stack.pop();
                expanded.extend(nested?);
            }
        }
    }
    Ok(expanded)
}

fn load_test_steps_fragment(
    path: &Path,
    inputs: &BTreeMap<String, String>,
) -> Result<Vec<RawTestStep>> {
    let yaml = std::fs::read_to_string(path)
        .with_context(|| format!("cannot read test step include: {}", path.display()))?;
    let mut value: Value = serde_yaml::from_str(&yaml)
        .with_context(|| format!("invalid test step include: {}", path.display()))?;
    substitute_inputs_in_value(&mut value, inputs)
        .with_context(|| format!("invalid test step include: {}", path.display()))?;
    serde_yaml::from_value(value)
        .with_context(|| format!("invalid test step include: {}", path.display()))
}

fn resolve_uses_path(repo_root: &Path, uses: &str) -> Result<PathBuf> {
    let (scheme, raw_path) = uses
        .split_once("://")
        .ok_or_else(|| anyhow::anyhow!("invalid uses value '{uses}': expected @://<path>"))?;
    match scheme {
        "@" => {
            let path = PathBuf::from(raw_path);
            validate_uses_repo_path(&path)?;
            Ok(resolve_under_root(repo_root, path))
        }
        other => anyhow::bail!(
            "unsupported uses scheme '{other}' in '{uses}'; only @://<path> is supported"
        ),
    }
}

fn validate_uses_repo_path(path: &Path) -> Result<()> {
    if path.as_os_str().is_empty() {
        anyhow::bail!("uses path must not be empty");
    }
    if path.is_absolute() {
        anyhow::bail!("uses path must be repo-relative, got: {}", path.display());
    }
    for component in path.components() {
        match component {
            Component::Normal(_) => {}
            _ => anyhow::bail!(
                "uses path must contain no '.' or '..' segments: {}",
                path.display()
            ),
        }
    }
    Ok(())
}

fn substitute_inputs_in_value(value: &mut Value, inputs: &BTreeMap<String, String>) -> Result<()> {
    match value {
        Value::String(text) => {
            *text = substitute_inputs_in_string(text, inputs)?;
        }
        Value::Sequence(items) => {
            for item in items {
                substitute_inputs_in_value(item, inputs)?;
            }
        }
        Value::Mapping(entries) => {
            for (_, value) in entries.iter_mut() {
                substitute_inputs_in_value(value, inputs)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn substitute_inputs_in_string(text: &str, inputs: &BTreeMap<String, String>) -> Result<String> {
    let mut rendered = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("${{") {
        rendered.push_str(&rest[..start]);
        let after_open = &rest[start + 3..];
        let end = after_open
            .find("}}")
            .ok_or_else(|| anyhow::anyhow!("unterminated input expression in '{text}'"))?;
        let expr = after_open[..end].trim();
        let name = expr.strip_prefix("inputs.").ok_or_else(|| {
            anyhow::anyhow!(
                "unsupported expression '${{{{{expr}}}}}'; only ${{{{ inputs.NAME }}}} is supported"
            )
        })?;
        if name.is_empty()
            || name
                .chars()
                .any(|ch| !(ch.is_ascii_alphanumeric() || ch == '_' || ch == '-'))
        {
            anyhow::bail!("invalid input name '{name}' in '{text}'");
        }
        let value = inputs
            .get(name)
            .ok_or_else(|| anyhow::anyhow!("missing required input '{name}'"))?;
        rendered.push_str(value);
        rest = &after_open[end + 2..];
    }
    rendered.push_str(rest);
    Ok(rendered)
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

fn validate_test_steps(steps: &[TestStep], ports: &[PortSpec]) -> Result<()> {
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

#[derive(Clone, Copy)]
enum StepOutputStream {
    Stdout,
    Stderr,
}

impl StepOutputStream {
    fn as_str(self) -> &'static str {
        match self {
            Self::Stdout => "stdout",
            Self::Stderr => "stderr",
        }
    }
}

#[derive(Serialize)]
struct StepLogRecord<'a> {
    ts: String,
    stream: &'a str,
    line: String,
}

struct StepLogWriter {
    inner: Mutex<BufWriter<File>>,
}

impl StepLogWriter {
    fn create(path: &Path) -> Result<Self> {
        let file = File::create(path)
            .with_context(|| format!("failed to create step log file: {}", path.display()))?;
        Ok(Self {
            inner: Mutex::new(BufWriter::new(file)),
        })
    }

    fn log_line(&self, stream: StepOutputStream, line: &[u8]) -> Result<()> {
        // Compute the timestamp and lossy-convert the line *before* acquiring
        // the lock so formatting latency is not serialised across both threads.
        let ts = step_log_timestamp()?;
        let line_str = String::from_utf8_lossy(line).into_owned();
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| anyhow::anyhow!("step log writer mutex poisoned"))?;
        serde_json::to_writer(
            &mut *inner,
            &StepLogRecord {
                ts,
                stream: stream.as_str(),
                line: line_str,
            },
        )
        .context("failed to serialize step log record")?;
        inner
            .write_all(b"\n")
            .context("failed to write step log newline")?;
        // Flush after each line for crash-safety: if the step is killed, all
        // prior lines are still visible in the JSONL file.
        inner.flush().context("failed to flush step log")?;
        Ok(())
    }
}

fn step_log_timestamp() -> Result<String> {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .context("failed to format step log timestamp")
}

fn sanitize_step_log_name(name: &str) -> String {
    name.chars()
        .map(|c| match c {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '.' | '_' | '-' => c,
            _ => '_',
        })
        .collect()
}

fn step_log_path(log_dir: &Path, step_idx: usize, step_name: &str) -> PathBuf {
    log_dir.join(format!(
        "step-{step_idx}-{}.log",
        sanitize_step_log_name(step_name)
    ))
}

fn stderr_color_enabled() -> bool {
    std::io::stderr().is_terminal() && std::env::var_os("NO_COLOR").is_none()
}

fn step_title_line(step_idx: usize, name: &str, color: bool) -> String {
    if color {
        format!("🤖 ({step_idx}) \x1b[1m{name}\x1b[0m")
    } else {
        format!("🤖 ({step_idx}) {name}")
    }
}

fn step_status_marker(step_idx: usize, name: &str, success: bool, color: bool) -> String {
    if color {
        if success {
            format!("\x1b[32m✓\x1b[0m ({step_idx}) \x1b[2m{name}\x1b[0m")
        } else {
            format!("\x1b[31m✗\x1b[0m ({step_idx}) {name}")
        }
    } else {
        let tick = if success { '✓' } else { '✗' };
        format!("{tick} ({step_idx}) {name}")
    }
}

fn print_step_title(step_idx: usize, step_name: &str) {
    eprintln!("{}", step_title_line(step_idx, step_name, stderr_color_enabled()));
}

fn print_step_status(step_idx: usize, step_name: &str, success: bool) {
    eprintln!(
        "{}",
        step_status_marker(step_idx, step_name, success, stderr_color_enabled())
    );
}

fn spawn_output_forwarder<R: Read + Send + 'static>(
    reader: R,
    stream: StepOutputStream,
    logger: Arc<StepLogWriter>,
) -> JoinHandle<Result<()>> {
    std::thread::spawn(move || match stream {
        StepOutputStream::Stdout => {
            let stdout = std::io::stdout();
            stream_child_output(reader, stdout.lock(), stream, &logger)
        }
        StepOutputStream::Stderr => {
            let stderr = std::io::stderr();
            stream_child_output(reader, stderr.lock(), stream, &logger)
        }
    })
}

/// Write all bytes of `buf` to `writer`, retrying on non-blocking back-pressure.
///
/// Unlike the stdlib `write_all`, this handles:
/// - Partial writes (advances past the written bytes and continues).
/// - `WouldBlock` / `EAGAIN` (fd not ready): sleeps a short backoff and retries
///   the **same remaining bytes** — no data is dropped or reordered.
/// - `Interrupted` / `EINTR`: retries immediately.
/// - Any other error: returned to the caller.
///
/// This is used for the live-console tee path, where botforge's own inherited
/// stdout/stderr fd may be in non-blocking mode (common under PTYs, process
/// supervisors, and some container runtimes).
fn write_all_resilient<W: Write>(writer: &mut W, mut buf: &[u8]) -> std::io::Result<()> {
    use std::io::ErrorKind;
    let mut backoff = Duration::from_millis(1);
    while !buf.is_empty() {
        match writer.write(buf) {
            Ok(0) => {
                return Err(std::io::Error::new(
                    ErrorKind::WriteZero,
                    "write returned zero bytes",
                ));
            }
            Ok(n) => {
                buf = &buf[n..];
                // Reset backoff after making forward progress.
                backoff = Duration::from_millis(1);
            }
            Err(e) if e.kind() == ErrorKind::Interrupted => {
                // EINTR: retry immediately with the same slice.
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock => {
                // EAGAIN: fd not ready. Sleep briefly and retry the same bytes.
                std::thread::sleep(backoff);
                backoff = (backoff * 2).min(Duration::from_millis(10));
            }
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

fn stream_child_output<R: Read, W: Write>(
    mut reader: R,
    mut writer: W,
    stream: StepOutputStream,
    logger: &StepLogWriter,
) -> Result<()> {
    let mut chunk = [0u8; 8192];
    let mut pending = Vec::new();
    loop {
        let bytes_read = reader
            .read(&mut chunk)
            .context("failed to read child process output")?;
        if bytes_read == 0 {
            break;
        }
        let slice = &chunk[..bytes_read];
        // Use the resilient writer so a non-blocking console fd (EAGAIN / os
        // error 11) causes a brief back-off retry instead of a fatal error.
        write_all_resilient(&mut writer, slice)
            .context("failed to forward child process output")?;
        // No per-chunk flush on the console side: the terminal/pipe receives
        // bytes as written; flushing every chunk adds syscall pressure without
        // correctness benefit. The JSONL log is flushed per-line below.
        pending.extend_from_slice(slice);
        while let Some(pos) = pending.iter().position(|&b| b == b'\n') {
            let mut line = pending.drain(..=pos).collect::<Vec<_>>();
            line.pop();
            logger.log_line(stream, &line)?;
        }
    }
    if !pending.is_empty() {
        logger.log_line(stream, &pending)?;
    }
    Ok(())
}

fn join_output_forwarders(handles: Vec<JoinHandle<Result<()>>>) -> Result<()> {
    for handle in handles {
        handle
            .join()
            .map_err(|_| anyhow::anyhow!("step output forwarder panicked"))??;
    }
    Ok(())
}

fn spawn_logged_child(
    command: &mut Command,
    logger: Arc<StepLogWriter>,
    spawn_context: &str,
) -> Result<(Child, Vec<JoinHandle<Result<()>>>)> {
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = command.spawn().with_context(|| spawn_context.to_string())?;
    let stdout = child
        .stdout
        .take()
        .context("failed to capture child stdout for step logging")?;
    let stderr = child
        .stderr
        .take()
        .context("failed to capture child stderr for step logging")?;
    Ok((
        child,
        vec![
            spawn_output_forwarder(stdout, StepOutputStream::Stdout, Arc::clone(&logger)),
            spawn_output_forwarder(stderr, StepOutputStream::Stderr, logger),
        ],
    ))
}

fn run_ssh_step_with_step_log(
    args: Vec<String>,
    log_path: &Path,
    retries: usize,
    retry_delay: Duration,
) -> Result<()> {
    // 300s execution timeout per attempt, matching the host-step ceiling.
    const SSH_STEP_TIMEOUT: Duration = Duration::from_secs(300);
    let logger = Arc::new(StepLogWriter::create(log_path)?);
    let mut attempts = 0usize;
    loop {
        let mut command = Command::new("ssh");
        command.args(&args);
        let (mut child, forwarders) =
            spawn_logged_child(&mut command, Arc::clone(&logger), "failed to execute ssh")?;

        // Poll with a bounded deadline; kill and surface a timeout error if
        // the step does not exit within SSH_STEP_TIMEOUT.
        let deadline = Instant::now() + SSH_STEP_TIMEOUT;
        let wait_result: Result<Option<std::process::ExitStatus>> = loop {
            match child.try_wait() {
                Ok(Some(status)) => break Ok(Some(status)),
                Ok(None) => {
                    if Instant::now() >= deadline {
                        let _ = child.kill();
                        let _ = child.wait();
                        break Ok(None); // None signals timeout
                    }
                    std::thread::sleep(Duration::from_millis(200));
                }
                Err(e) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    // Convert to anyhow and break; forwarders joined below.
                    break Err(anyhow::Error::new(e).context("failed to wait for ssh"));
                }
            }
        };

        // Always join forwarders so threads and pipes are cleaned up on every
        // exit path. Forwarder errors are suppressed — the step result wins
        // (see Bug 3 fix; WouldBlock is no longer an error after Bug 1 fix).
        let _ = join_output_forwarders(forwarders);

        let status = match wait_result? {
            None => {
                anyhow::bail!("ssh step timed out after {}s", SSH_STEP_TIMEOUT.as_secs());
            }
            Some(s) => s,
        };

        if status.success() {
            return Ok(());
        }
        attempts += 1;
        // Non-255 exit or retries exhausted → hard failure. Retry only on 255
        // (SSH transport error) to match retry_transport_cmd semantics.
        if status.code() != Some(255) || attempts >= retries {
            anyhow::bail!("ssh command failed (exit status: {status})");
        }
        std::thread::sleep(retry_delay);
    }
}

#[derive(Clone, Copy)]
struct HostStepFiles<'a> {
    env_file: &'a Path,
    log_path: &'a Path,
}

/// Run a step locally in the botforge container (harness) with a plain execution timeout.
/// `run` is written to a temp file and executed via `template` (argv with `{0}` slot).
/// The working directory is `repo_root`. Inherits the current process environment, with
/// `accumulated_env` injected (overriding inherited values) and `BOTFORGE_ENV` pointing at
/// `env_file` so the step can write new key-value pairs for later steps to consume.
fn run_host_step(
    name: &str,
    run: &str,
    repo_root: &Path,
    timeout: Duration,
    template: &[String],
    accumulated_env: &[(String, String)],
    files: HostStepFiles<'_>,
) -> Result<()> {
    // Create/truncate the env file so `>>` always works inside the step.
    std::fs::write(files.env_file, b"")
        .with_context(|| format!("failed to create env file for host step '{name}'"))?;

    let script = std::env::temp_dir().join(format!("botforge-host-step-{}.sh", unique_suffix()));
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

    let logger = Arc::new(StepLogWriter::create(files.log_path)?);
    let mut command = Command::new(&argv[0]);
    command
        .args(&argv[1..])
        .current_dir(repo_root)
        .env("BOTFORGE_ENV", files.env_file)
        .envs(
            accumulated_env
                .iter()
                .map(|(k, v)| (k.as_str(), v.as_str())),
        );
    let (mut child, forwarders) = spawn_logged_child(
        &mut command,
        logger,
        &format!("failed to spawn host step '{name}'"),
    )?;

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

    // Join forwarders unconditionally — threads and pipes must be cleaned up
    // on every exit path (success, failure, timeout). Forwarder errors are
    // suppressed here: step_result is the source of truth for the step outcome.
    // A forwarder/tee error must NOT override a real step failure, and must not
    // by itself fail an otherwise-successful step (WouldBlock is no longer an
    // error after Bug 1 fix).
    let _ = join_output_forwarders(forwarders);

    // Best-effort cleanup of temp script — always runs regardless of outcome.
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

/// Parse a `$GITHUB_ENV`-style env file, returning key-value pairs in insertion order.
///
/// Supported formats:
/// - **Single-line:** `KEY=value` — key is everything before the first `=`; value is
///   everything after (may contain `=`; no trimming).
/// - **Heredoc / multiline:** `KEY<<DELIMITER` followed by lines of value text ending
///   at a line equal to `DELIMITER`. The value is the lines joined with `\n` (no
///   trailing newline added). The delimiter line itself is consumed and not included.
///
/// Blank lines and lines that match neither format are skipped.
pub(crate) fn parse_env_file(contents: &str) -> Result<Vec<(String, String)>> {
    let mut result: Vec<(String, String)> = Vec::new();
    let lines: Vec<&str> = contents.lines().collect();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        if line.is_empty() {
            i += 1;
            continue;
        }
        // Check for heredoc: KEY<<DELIMITER (no '=' before '<<')
        let eq_pos = line.find('=');
        let heredoc_pos = line.find("<<");
        if let Some(hpos) = heredoc_pos {
            let before_heredoc = &line[..hpos];
            if !before_heredoc.is_empty()
                && eq_pos.is_none_or(|ep| ep > hpos)
                && !before_heredoc.contains('=')
            {
                let key = before_heredoc;
                let delimiter = &line[hpos + 2..];
                if !delimiter.is_empty() {
                    i += 1;
                    let mut value_lines: Vec<&str> = Vec::new();
                    while i < lines.len() && lines[i] != delimiter {
                        value_lines.push(lines[i]);
                        i += 1;
                    }
                    if i >= lines.len() {
                        anyhow::bail!("unterminated heredoc for key '{key}'");
                    }
                    i += 1; // consume the delimiter line
                    result.push((key.to_string(), value_lines.join("\n")));
                    continue;
                }
            }
        }
        // Single-line: KEY=value
        if let Some(eq) = eq_pos {
            let key = &line[..eq];
            let value = &line[eq + 1..];
            if !key.is_empty() {
                result.push((key.to_string(), value.to_string()));
            }
        }
        i += 1;
    }
    Ok(result)
}

/// Merge `new_entries` into `accumulated`, updating existing keys in-place and
/// appending new keys at the end (insertion-ordered, last-write-wins).
fn env_merge(accumulated: &mut Vec<(String, String)>, new_entries: Vec<(String, String)>) {
    for (key, value) in new_entries {
        if let Some(entry) = accumulated.iter_mut().find(|(k, _)| k == &key) {
            entry.1 = value;
        } else {
            accumulated.push((key, value));
        }
    }
}

/// Build the shell preamble that exports all accumulated env vars and sets up
/// `BOTFORGE_ENV` pointing at `remote_env_path` for a guest step script.
fn build_guest_env_preamble(accumulated_env: &[(String, String)], remote_env_path: &str) -> String {
    let mut lines: Vec<String> = Vec::new();
    for (key, value) in accumulated_env {
        lines.push(format!("export {}={}", key, shell_single_quote(value)));
    }
    lines.push(format!(
        "export BOTFORGE_ENV={}",
        shell_single_quote(remote_env_path)
    ));
    lines.push(": > \"$BOTFORGE_ENV\"".to_string());
    let mut preamble = lines.join("\n");
    preamble.push('\n');
    preamble
}

#[cfg(test)]
mod tests {
    use super::{
        build_guest_env_preamble, default_bootstrap_path, env_merge, load_test_config,
        parse_env_file, resolve_shell, run_host_step, shell_single_quote, step_log_path,
        step_status_marker, step_title_line, validate_test_ports, validate_test_steps,
        write_all_resilient, HostStepFiles, StepTarget, TestConfig, TestIso, TestStep, TestUpload,
    };
    use crate::cli::Cli;
    use crate::qemu::PortSpec;
    use crate::util::unique_suffix;
    use clap::Parser;
    use serde::Deserialize;
    use std::path::{Path, PathBuf};
    use std::time::Duration;
    use tempfile::TempDir;

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
        assert!(
            serde_yaml::from_str::<TestConfig>("ports:\n  - \"0.0.0.0:notanumber\"\n").is_err()
        );
        assert!(serde_yaml::from_str::<TestConfig>("ports:\n  - \"0.0.0.0:99999\"\n").is_err());
    }

    #[test]
    fn test_config_ports_validation_rejects_invalid_and_duplicate_values() {
        assert!(validate_test_ports(&[loopback(0)], 2222).is_err());
        assert!(validate_test_ports(&[loopback(2222)], 2222).is_err());
        assert!(validate_test_ports(&[loopback(22)], 2222).is_err());
        assert!(validate_test_ports(&[loopback(80), loopback(80)], 2222).is_err());
        // duplicate port number regardless of address
        assert!(validate_test_ports(
            &[
                loopback(80),
                PortSpec {
                    addr: "0.0.0.0".into(),
                    port: 80
                }
            ],
            2222
        )
        .is_err());
    }

    #[test]
    fn test_config_ports_validation_accepts_distinct_ports() {
        assert!(validate_test_ports(
            &[
                loopback(80),
                PortSpec {
                    addr: "0.0.0.0".into(),
                    port: 9901
                }
            ],
            2222
        )
        .is_ok());
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
        assert_eq!(
            config.steps[0].uploads[0].src,
            PathBuf::from("local/file.sh")
        );
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

    #[test]
    fn test_load_test_config_expands_uses_steps_with_inputs() {
        let repo = TempDir::new().unwrap();
        std::fs::create_dir_all(repo.path().join("shared")).unwrap();
        std::fs::write(
            repo.path().join("shared/narrative.yaml"),
            r#"
- on: guest
  name: "narrative-${{ inputs.target }}"
  shell: ${{ inputs.shell }}
  uploads:
    - src: scripts/${{ inputs.target }}.sh
      dest: /tmp/${{ inputs.target }}.sh
  run: |
    echo "${USER}"
    bash /tmp/${{ inputs.target }}.sh
"#,
        )
        .unwrap();
        std::fs::write(
            repo.path().join("test.yaml"),
            r#"
steps:
  - uses: "@://shared/narrative.yaml"
    inputs:
      target: edge
      shell: bash
"#,
        )
        .unwrap();

        let config = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap();

        assert_eq!(config.steps.len(), 1);
        assert_eq!(config.steps[0].name, "narrative-edge");
        assert_eq!(config.steps[0].shell.as_deref(), Some("bash"));
        assert_eq!(
            config.steps[0].uploads[0].src,
            PathBuf::from("scripts/edge.sh")
        );
        assert_eq!(config.steps[0].uploads[0].dest, "/tmp/edge.sh");
        assert!(config.steps[0].run.contains(r#"echo "${USER}""#));
        assert!(config.steps[0].run.contains("bash /tmp/edge.sh"));
    }

    #[test]
    fn test_load_test_config_rejects_unsupported_uses_scheme() {
        let repo = TempDir::new().unwrap();
        std::fs::write(
            repo.path().join("test.yaml"),
            r#"
steps:
  - uses: "file://shared/narrative.yaml"
"#,
        )
        .unwrap();

        let err = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap_err();
        assert!(format!("{err:#}").contains("unsupported uses scheme 'file'"));
    }

    #[test]
    fn test_load_test_config_rejects_missing_include_input() {
        let repo = TempDir::new().unwrap();
        std::fs::create_dir_all(repo.path().join("shared")).unwrap();
        std::fs::write(
            repo.path().join("shared/narrative.yaml"),
            r#"
- on: guest
  name: "${{ inputs.target }}"
  run: "echo ok"
"#,
        )
        .unwrap();
        std::fs::write(
            repo.path().join("test.yaml"),
            r#"
steps:
  - uses: "@://shared/narrative.yaml"
"#,
        )
        .unwrap();

        let err = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap_err();
        assert!(format!("{err:#}").contains("missing required input 'target'"));
    }

    #[test]
    fn test_load_test_config_rejects_parent_segments_in_uses_path() {
        let repo = TempDir::new().unwrap();
        std::fs::write(
            repo.path().join("test.yaml"),
            r#"
steps:
  - uses: "@://shared/../narrative.yaml"
"#,
        )
        .unwrap();

        let err = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap_err();
        assert!(format!("{err:#}").contains("must contain no '.' or '..' segments"));
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
        assert!(validate_test_steps(&steps, &[loopback(80)]).is_ok());
    }

    #[test]
    fn test_validate_steps_accepts_host_without_uploads() {
        let steps = vec![make_step(StepTarget::Host, "s", false)];
        assert!(validate_test_steps(&steps, &[loopback(80)]).is_ok());
    }

    #[test]
    fn test_validate_steps_rejects_uploads_on_host_step() {
        let steps = vec![make_step(StepTarget::Host, "bad", true)];
        let err = validate_test_steps(&steps, &[loopback(80)]).unwrap_err();
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

    #[test]
    fn test_test_cli_requires_repo_root() {
        let err = Cli::try_parse_from([
            "botforge",
            "test",
            "--test-config",
            "test.yaml",
            "--base-image",
            "base.qcow2",
            "--ssh-key",
            "id_ed25519",
        ])
        .unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);
        assert!(err.to_string().contains("--repo-root"));
    }

    // --- shell resolver ---

    #[test]
    fn test_resolve_shell_absent_returns_bash_template() {
        let tmpl = resolve_shell(None).unwrap();
        assert_eq!(
            tmpl,
            vec![
                "bash",
                "--noprofile",
                "--norc",
                "-e",
                "-o",
                "pipefail",
                "{0}"
            ]
        );
    }

    #[test]
    fn test_resolve_shell_bash_returns_bash_template() {
        let tmpl = resolve_shell(Some("bash")).unwrap();
        assert_eq!(
            tmpl,
            vec![
                "bash",
                "--noprofile",
                "--norc",
                "-e",
                "-o",
                "pipefail",
                "{0}"
            ]
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

    fn tmp_env_file() -> std::path::PathBuf {
        std::env::temp_dir().join(format!("botforge-test-env-{}.env", unique_suffix()))
    }

    fn tmp_step_log(dir: &Path) -> PathBuf {
        dir.join(format!("step-{}.log", unique_suffix()))
    }

    #[derive(Debug, Deserialize)]
    struct LoggedStepLine {
        ts: String,
        stream: String,
        line: String,
    }

    fn read_step_log(path: &Path) -> Vec<LoggedStepLine> {
        std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    #[test]
    fn test_host_step_sh_false_fails() {
        let dir = tempfile::tempdir().unwrap();
        let tmpl = resolve_shell(Some("sh")).unwrap();
        let env_file = tmp_env_file();
        let log_file = tmp_step_log(dir.path());
        let err = run_host_step(
            "fail-step",
            "false",
            dir.path(),
            Duration::from_secs(10),
            &tmpl,
            &[],
            HostStepFiles {
                env_file: &env_file,
                log_path: &log_file,
            },
        )
        .unwrap_err();
        let _ = std::fs::remove_file(&env_file);
        assert!(
            err.to_string().contains("fail-step"),
            "error should mention step name: {err}"
        );
    }

    #[test]
    fn test_host_step_exit_nonzero_fails() {
        let dir = tempfile::tempdir().unwrap();
        let tmpl = resolve_shell(Some("sh")).unwrap();
        let env_file = tmp_env_file();
        let log_file = tmp_step_log(dir.path());
        let err = run_host_step(
            "bad",
            "exit 3",
            dir.path(),
            Duration::from_secs(10),
            &tmpl,
            &[],
            HostStepFiles {
                env_file: &env_file,
                log_path: &log_file,
            },
        )
        .unwrap_err();
        let _ = std::fs::remove_file(&env_file);
        assert!(
            err.to_string().contains("bad"),
            "error should mention step name: {err}"
        );
    }

    #[test]
    fn test_host_step_default_set_e_fails_on_mid_script_error() {
        // Under the default bash template (-e -o pipefail), a non-final failing
        // command must fail the step even though the last command would succeed.
        let dir = tempfile::tempdir().unwrap();
        let tmpl = resolve_shell(None).unwrap();
        let env_file = tmp_env_file();
        let log_file = tmp_step_log(dir.path());
        let err = run_host_step(
            "mid-fail",
            "false\necho ok\n",
            dir.path(),
            Duration::from_secs(10),
            &tmpl,
            &[],
            HostStepFiles {
                env_file: &env_file,
                log_path: &log_file,
            },
        )
        .unwrap_err();
        let _ = std::fs::remove_file(&env_file);
        assert!(
            err.to_string().contains("mid-fail"),
            "error should mention step name: {err}"
        );
    }

    #[test]
    fn test_host_step_success() {
        let dir = tempfile::tempdir().unwrap();
        let tmpl = resolve_shell(None).unwrap();
        let env_file = tmp_env_file();
        let log_file = tmp_step_log(dir.path());
        let result = run_host_step(
            "ok",
            "true",
            dir.path(),
            Duration::from_secs(10),
            &tmpl,
            &[],
            HostStepFiles {
                env_file: &env_file,
                log_path: &log_file,
            },
        );
        let _ = std::fs::remove_file(&env_file);
        assert!(result.is_ok());
    }

    #[test]
    fn test_host_step_injects_accumulated_env() {
        let dir = tempfile::tempdir().unwrap();
        let tmpl = resolve_shell(None).unwrap();
        let env_file = tmp_env_file();
        let log_file = tmp_step_log(dir.path());
        let accumulated = vec![
            ("MY_VAR".to_string(), "hello world".to_string()),
            ("ANOTHER".to_string(), "42".to_string()),
        ];
        let result = run_host_step(
            "env-check",
            r#"test "$MY_VAR" = "hello world" && test "$ANOTHER" = "42""#,
            dir.path(),
            Duration::from_secs(10),
            &tmpl,
            &accumulated,
            HostStepFiles {
                env_file: &env_file,
                log_path: &log_file,
            },
        );
        let _ = std::fs::remove_file(&env_file);
        assert!(
            result.is_ok(),
            "accumulated env vars should be visible in host step: {result:?}"
        );
    }

    #[test]
    fn test_host_step_botforge_env_is_set_and_writable() {
        let dir = tempfile::tempdir().unwrap();
        let tmpl = resolve_shell(None).unwrap();
        let env_file = tmp_env_file();
        let log_file = tmp_step_log(dir.path());
        let result = run_host_step(
            "write-env",
            r#"echo "WRITTEN=yes" >> "$BOTFORGE_ENV""#,
            dir.path(),
            Duration::from_secs(10),
            &tmpl,
            &[],
            HostStepFiles {
                env_file: &env_file,
                log_path: &log_file,
            },
        );
        let contents = std::fs::read_to_string(&env_file).unwrap_or_default();
        let _ = std::fs::remove_file(&env_file);
        assert!(result.is_ok(), "step should succeed: {result:?}");
        assert!(
            contents.contains("WRITTEN=yes"),
            "env file should contain written value, got: {contents:?}"
        );
    }

    #[test]
    fn test_host_step_writes_jsonl_log() {
        let dir = tempfile::tempdir().unwrap();
        let tmpl = resolve_shell(Some("sh")).unwrap();
        let env_file = tmp_env_file();
        let log_file = tmp_step_log(dir.path());
        let result = run_host_step(
            "log-lines",
            "printf 'alpha\\n'; printf 'beta\\n' >&2; printf 'omega'",
            dir.path(),
            Duration::from_secs(10),
            &tmpl,
            &[],
            HostStepFiles {
                env_file: &env_file,
                log_path: &log_file,
            },
        );
        let _ = std::fs::remove_file(&env_file);
        assert!(result.is_ok(), "step should succeed: {result:?}");

        let logged = read_step_log(&log_file);
        assert_eq!(logged.len(), 3, "expected stdout/stderr lines in log");
        assert!(
            logged.iter().all(|entry| !entry.ts.is_empty()),
            "all log entries should have timestamps: {logged:?}"
        );
        assert!(
            logged
                .iter()
                .any(|entry| entry.stream == "stdout" && entry.line == "alpha"),
            "stdout line should be captured: {logged:?}"
        );
        assert!(
            logged
                .iter()
                .any(|entry| entry.stream == "stderr" && entry.line == "beta"),
            "stderr line should be captured: {logged:?}"
        );
        assert!(
            logged
                .iter()
                .any(|entry| entry.stream == "stdout" && entry.line == "omega"),
            "trailing partial line should be captured: {logged:?}"
        );
    }

    #[test]
    fn test_step_log_path_sanitizes_name() {
        let log_dir = PathBuf::from("/tmp/botforge-step-logs");
        let path = step_log_path(&log_dir, 7, "name with/slash\tand*chars");
        assert_eq!(path, log_dir.join("step-7-name_with_slash_and_chars.log"));
    }

    #[test]
    fn test_step_status_marker_formats_result() {
        assert_eq!(
            step_status_marker(4, "mcp-smoke", false, false),
            "✗ (4) mcp-smoke"
        );
        assert_eq!(
            step_status_marker(4, "mcp-smoke", true, false),
            "✓ (4) mcp-smoke"
        );
        let success_color = step_status_marker(4, "mcp-smoke", true, true);
        assert!(
            success_color.contains("\x1b[32m"),
            "success color should contain green: {success_color:?}"
        );
        assert!(
            success_color.contains('✓'),
            "success color should contain tick: {success_color:?}"
        );
        assert!(
            success_color.contains("\x1b[2m"),
            "success color should dim name: {success_color:?}"
        );
        assert!(
            success_color.contains("\x1b[0m"),
            "success color should reset: {success_color:?}"
        );
        let failure_color = step_status_marker(4, "mcp-smoke", false, true);
        assert!(
            failure_color.contains("\x1b[31m"),
            "failure color should contain red: {failure_color:?}"
        );
        assert!(
            failure_color.contains('✗'),
            "failure color should contain cross: {failure_color:?}"
        );
        assert!(
            !failure_color.contains("\x1b[2m"),
            "failure color should NOT dim name: {failure_color:?}"
        );
    }

    #[test]
    fn test_step_title_line_formats() {
        assert_eq!(
            step_title_line(4, "mcp-smoke", false),
            "🤖 (4) mcp-smoke"
        );
        let colored = step_title_line(4, "mcp-smoke", true);
        assert!(
            colored.contains("🤖 (4) "),
            "colored title should contain robot prefix: {colored:?}"
        );
        assert!(
            colored.contains("\x1b[1m"),
            "colored title should contain bold code: {colored:?}"
        );
        assert!(
            colored.contains("mcp-smoke"),
            "colored title should contain name: {colored:?}"
        );
        assert!(
            colored.contains("\x1b[0m"),
            "colored title should contain reset: {colored:?}"
        );
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

    // --- parse_env_file ---

    #[test]
    fn test_parse_env_file_empty() {
        let result = parse_env_file("").unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn test_parse_env_file_single_line() {
        let result = parse_env_file("FOO=bar\n").unwrap();
        assert_eq!(result, vec![("FOO".to_string(), "bar".to_string())]);
    }

    #[test]
    fn test_parse_env_file_value_contains_equals() {
        let result = parse_env_file("URL=https://example.com/a=1&b=2\n").unwrap();
        assert_eq!(
            result,
            vec![("URL".to_string(), "https://example.com/a=1&b=2".to_string())]
        );
    }

    #[test]
    fn test_parse_env_file_value_preserves_interior_spaces() {
        let result = parse_env_file("MSG=  hello world  \n").unwrap();
        assert_eq!(
            result,
            vec![("MSG".to_string(), "  hello world  ".to_string())]
        );
    }

    #[test]
    fn test_parse_env_file_multiple_single_line() {
        let result = parse_env_file("A=1\nB=2\nC=3\n").unwrap();
        assert_eq!(
            result,
            vec![
                ("A".to_string(), "1".to_string()),
                ("B".to_string(), "2".to_string()),
                ("C".to_string(), "3".to_string()),
            ]
        );
    }

    #[test]
    fn test_parse_env_file_skips_blank_lines() {
        let result = parse_env_file("\nA=1\n\nB=2\n\n").unwrap();
        assert_eq!(
            result,
            vec![
                ("A".to_string(), "1".to_string()),
                ("B".to_string(), "2".to_string()),
            ]
        );
    }

    #[test]
    fn test_parse_env_file_heredoc_basic() {
        let input = "BODY<<EOF\nline1\nline2\nEOF\n";
        let result = parse_env_file(input).unwrap();
        assert_eq!(
            result,
            vec![("BODY".to_string(), "line1\nline2".to_string())]
        );
    }

    #[test]
    fn test_parse_env_file_heredoc_custom_delimiter() {
        let input = "MSG<<ENDOFMSG\nhello\nworld\nENDOFMSG\n";
        let result = parse_env_file(input).unwrap();
        assert_eq!(
            result,
            vec![("MSG".to_string(), "hello\nworld".to_string())]
        );
    }

    #[test]
    fn test_parse_env_file_heredoc_single_line_value() {
        let input = "KEY<<DELIM\nonly line\nDELIM\n";
        let result = parse_env_file(input).unwrap();
        assert_eq!(result, vec![("KEY".to_string(), "only line".to_string())]);
    }

    #[test]
    fn test_parse_env_file_heredoc_empty_value() {
        let input = "KEY<<DELIM\nDELIM\n";
        let result = parse_env_file(input).unwrap();
        assert_eq!(result, vec![("KEY".to_string(), "".to_string())]);
    }

    #[test]
    fn test_parse_env_file_mixed_single_and_heredoc() {
        let input = "A=1\nBODY<<EOF\nline1\nline2\nEOF\nZ=last\n";
        let result = parse_env_file(input).unwrap();
        assert_eq!(
            result,
            vec![
                ("A".to_string(), "1".to_string()),
                ("BODY".to_string(), "line1\nline2".to_string()),
                ("Z".to_string(), "last".to_string()),
            ]
        );
    }

    #[test]
    fn test_parse_env_file_unterminated_heredoc_is_error() {
        let input = "KEY<<DELIM\nsome line\n";
        assert!(parse_env_file(input).is_err());
    }

    #[test]
    fn test_parse_env_file_value_with_heredoc_markers_in_single_line() {
        // Single-line value that contains "<<" should be treated as single-line
        // because there's an "=" before the "<<".
        let input = "KEY=value<<not-heredoc\n";
        let result = parse_env_file(input).unwrap();
        assert_eq!(
            result,
            vec![("KEY".to_string(), "value<<not-heredoc".to_string())]
        );
    }

    // --- env_merge ---

    #[test]
    fn test_env_merge_appends_new_keys() {
        let mut acc: Vec<(String, String)> = vec![("A".to_string(), "1".to_string())];
        env_merge(&mut acc, vec![("B".to_string(), "2".to_string())]);
        assert_eq!(
            acc,
            vec![
                ("A".to_string(), "1".to_string()),
                ("B".to_string(), "2".to_string()),
            ]
        );
    }

    #[test]
    fn test_env_merge_updates_existing_key_in_place() {
        let mut acc: Vec<(String, String)> = vec![
            ("A".to_string(), "old".to_string()),
            ("B".to_string(), "keep".to_string()),
        ];
        env_merge(&mut acc, vec![("A".to_string(), "new".to_string())]);
        // A's value updated; B preserved; order unchanged
        assert_eq!(
            acc,
            vec![
                ("A".to_string(), "new".to_string()),
                ("B".to_string(), "keep".to_string()),
            ]
        );
    }

    #[test]
    fn test_env_merge_empty_new_entries_is_noop() {
        let mut acc: Vec<(String, String)> = vec![("A".to_string(), "1".to_string())];
        env_merge(&mut acc, vec![]);
        assert_eq!(acc, vec![("A".to_string(), "1".to_string())]);
    }

    #[test]
    fn test_env_merge_last_write_wins_for_duplicate_new_keys() {
        let mut acc: Vec<(String, String)> = vec![];
        env_merge(
            &mut acc,
            vec![
                ("X".to_string(), "first".to_string()),
                ("X".to_string(), "second".to_string()),
            ],
        );
        // Second write overwrites first; X appears once
        assert_eq!(acc, vec![("X".to_string(), "second".to_string())]);
    }

    // --- build_guest_env_preamble ---

    #[test]
    fn test_build_guest_env_preamble_empty_env() {
        let preamble = build_guest_env_preamble(&[], "/tmp/botforge-env-1");
        assert!(preamble.contains("export BOTFORGE_ENV='/tmp/botforge-env-1'"));
        assert!(preamble.contains(": > \"$BOTFORGE_ENV\""));
        // No extra export lines when accumulated env is empty
        assert!(!preamble.contains("export A="));
    }

    #[test]
    fn test_build_guest_env_preamble_exports_accumulated_vars() {
        let acc = vec![
            ("FOO".to_string(), "bar".to_string()),
            ("MSG".to_string(), "hello world".to_string()),
        ];
        let preamble = build_guest_env_preamble(&acc, "/tmp/env");
        assert!(
            preamble.contains("export FOO='bar'"),
            "preamble: {preamble}"
        );
        assert!(
            preamble.contains("export MSG='hello world'"),
            "preamble: {preamble}"
        );
        assert!(preamble.contains("export BOTFORGE_ENV="));
        assert!(preamble.contains(": > \"$BOTFORGE_ENV\""));
    }

    #[test]
    fn test_build_guest_env_preamble_quotes_special_chars() {
        let acc = vec![("VAL".to_string(), "it's a value".to_string())];
        let preamble = build_guest_env_preamble(&acc, "/tmp/env");
        // shell_single_quote escapes embedded single quotes
        let expected = format!("export VAL={}", shell_single_quote("it's a value"));
        assert!(preamble.contains(&expected), "preamble: {preamble}");
    }

    // --- write_all_resilient ---

    /// Verify that write_all_resilient successfully forwards all bytes to a
    /// non-blocking Unix-domain socket, handling EAGAIN (WouldBlock) without
    /// dropping or reordering data.
    #[cfg(unix)]
    #[test]
    fn test_write_all_resilient_nonblocking_fd() {
        use std::io::Read;
        use std::os::unix::net::UnixStream;

        let (mut writer_end, mut reader_end) = UnixStream::pair().unwrap();
        // Make the write end non-blocking so writes may return WouldBlock.
        writer_end.set_nonblocking(true).unwrap();

        // 512 KiB — well above the socket buffer (typically 128–212 KiB).
        let data: Vec<u8> = (0u8..=255u8).cycle().take(512 * 1024).collect();
        let data_for_check = data.clone();

        // Drain slowly in a background thread to force repeated WouldBlock.
        let reader_handle = std::thread::spawn(move || {
            let mut received = Vec::new();
            let mut buf = [0u8; 4096];
            loop {
                match reader_end.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => received.extend_from_slice(&buf[..n]),
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    Err(e) => panic!("reader error: {e}"),
                }
            }
            received
        });

        // Must not error; must not drop or reorder bytes.
        write_all_resilient(&mut writer_end, &data_for_check).unwrap();
        // Drop the write end so the reader sees EOF and terminates.
        drop(writer_end);

        let received = reader_handle.join().unwrap();
        assert_eq!(
            received, data_for_check,
            "write_all_resilient must forward all bytes without loss or reordering"
        );
    }

    // --- host step timeout ---

    #[test]
    fn test_host_step_timeout_kills_and_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let tmpl = resolve_shell(Some("sh")).unwrap();
        let env_file = tmp_env_file();
        let log_file = tmp_step_log(dir.path());
        let start = std::time::Instant::now();
        // Use `exec` so the shell replaces itself with sleep; the child we hold
        // is then the sleep process itself, so child.kill() kills it directly
        // and the pipes close immediately without a lingering grandchild.
        let err = run_host_step(
            "slow-step",
            "exec sleep 5",
            dir.path(),
            Duration::from_millis(400),
            &tmpl,
            &[],
            HostStepFiles {
                env_file: &env_file,
                log_path: &log_file,
            },
        )
        .unwrap_err();
        let elapsed = start.elapsed();
        let _ = std::fs::remove_file(&env_file);
        // Should have killed quickly — well under the 5-second sleep.
        assert!(
            elapsed < Duration::from_secs(3),
            "host step timeout should kill quickly; took {elapsed:?}"
        );
        let msg = err.to_string();
        assert!(
            msg.contains("slow-step"),
            "timeout error should mention the step name: {msg}"
        );
        assert!(
            msg.contains("timed out"),
            "timeout error should mention 'timed out': {msg}"
        );
    }
}
