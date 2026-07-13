use anyhow::{Context, Result};
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::resolver::ResolveFileContext;
use crate::ssh::{
    journalctl_command, scp_with_retry, ssh_capture_stdout, ssh_exec_logged, ssh_with_retry,
    wait_for_ssh, SshExecOutcome, SshOptions,
};
use crate::util::unique_suffix;

use super::config::{TestConfig, TestIsoBootstrap};
use super::files::{stage_files, FileEntry};
use super::log::{
    join_output_forwarders, print_step_status, print_step_title, spawn_output_forwarder,
    step_log_path, StepLogWriter, StepOutputStream,
};
use super::step::{resolve_shell, ArchiveStep, RunStep, StepTarget, TestStep};

const TEST_SSH_READY_TIMEOUT: Duration = Duration::from_secs(300);
const TEST_TRANSPORT_RETRIES: usize = 10;
const TEST_TRANSPORT_RETRY_DELAY: Duration = Duration::from_secs(2);
const TEST_STABLE_SSH_ATTEMPTS: usize = 5;
const TEST_STABLE_SSH_REQUIRED: usize = 2;
type ArchiveExecutor<'a> = dyn FnMut(usize, &ArchiveStep) -> Result<()> + 'a;

pub(crate) struct StepFlowPlan<'a> {
    pub(crate) files: &'a [FileEntry],
    pub(crate) steps: &'a [TestStep],
    pub(crate) bootstraps: &'a [TestIsoBootstrap],
    /// Shasset manifest path, forwarded to the resolver for `@`-reference srcs in files.
    pub(crate) manifest_path: &'a Path,
    /// Optional cache directory override, forwarded to the resolver.
    pub(crate) cache_dir_override: Option<&'a Path>,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct StepTimeoutPolicy {
    pub(crate) overall_timeout: Duration,
    pub(crate) default_step_timeout: Duration,
    pub(crate) cloud_init_timeout: Duration,
}

#[derive(Clone, Copy, Debug)]
struct StepExecutionBudget {
    step_timeout: Duration,
    overall_deadline: Instant,
    overall_timeout: Duration,
}

struct RunStepContext<'a> {
    repo_root: &'a Path,
    ssh: &'a SshOptions,
    step_log_dir: &'a Path,
    overall_deadline: Instant,
    overall_timeout: Duration,
    default_step_timeout: Duration,
}

// ---------------------------------------------------------------------------
// VM runtime (formerly run.rs)
// ---------------------------------------------------------------------------

pub(crate) fn run_test_flow(
    repo_root: &Path,
    config: &TestConfig,
    ssh: &SshOptions,
    bootstraps: &[TestIsoBootstrap],
    manifest_path: &Path,
    cache_dir_override: Option<&Path>,
) -> Result<()> {
    run_step_flow(
        repo_root,
        StepFlowPlan {
            files: &config.files,
            steps: &config.steps,
            bootstraps,
            manifest_path,
            cache_dir_override,
        },
        ssh,
        StepTimeoutPolicy {
            overall_timeout: Duration::from_secs(config.timeout),
            default_step_timeout: Duration::from_secs(config.step_timeout),
            cloud_init_timeout: Duration::from_secs(config.cloud_init_timeout),
        },
        None,
    )
    .map(|_| ())
}

/// Shared boot→wait-for-SSH→wait-for-cloud-init→run-steps spine used by both
/// `botforge test` and `botforge build`.  `bootstraps` is empty for build runs.
pub(crate) fn run_step_flow(
    repo_root: &Path,
    plan: StepFlowPlan<'_>,
    ssh: &SshOptions,
    timeouts: StepTimeoutPolicy,
    mut archive_executor: Option<&mut ArchiveExecutor<'_>>,
) -> Result<Instant> {
    let overall_deadline = Instant::now() + timeouts.overall_timeout;
    let step_log_dir = repo_root.join("build").join("logs");
    std::fs::create_dir_all(&step_log_dir).with_context(|| {
        format!(
            "cannot create test step log dir: {}",
            step_log_dir.display()
        )
    })?;
    ensure_overall_budget(overall_deadline, timeouts.overall_timeout)?;
    wait_for_ssh(
        ssh,
        remaining_budget(overall_deadline).min(TEST_SSH_READY_TIMEOUT),
    )?;
    ensure_overall_budget(overall_deadline, timeouts.overall_timeout)?;
    ssh_with_retry(
        ssh,
        "sudo cloud-init status --wait",
        TEST_TRANSPORT_RETRIES,
        TEST_TRANSPORT_RETRY_DELAY,
        remaining_budget(overall_deadline).min(timeouts.cloud_init_timeout),
    )?;
    require_stable_ssh_with_deadline(
        ssh,
        TEST_STABLE_SSH_ATTEMPTS,
        TEST_STABLE_SSH_REQUIRED,
        overall_deadline,
        timeouts.overall_timeout,
    )?;

    for bootstrap in plan.bootstraps {
        ensure_overall_budget(overall_deadline, timeouts.overall_timeout)?;
        let mount = shell_single_quote(&bootstrap.mount.display().to_string());
        let label = shell_single_quote(&bootstrap.label);
        let mount_cmd = format!("sudo mkdir -p {mount} && sudo mount -L {label} -o ro {mount}");
        ssh_with_retry(
            ssh,
            &mount_cmd,
            TEST_TRANSPORT_RETRIES,
            TEST_TRANSPORT_RETRY_DELAY,
            remaining_budget(overall_deadline).min(timeouts.cloud_init_timeout),
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
            remaining_budget(overall_deadline).min(timeouts.cloud_init_timeout),
        )
        .with_context(|| format!("iso bootstrap script failed for label {}", bootstrap.label))?;
    }

    if !plan.files.is_empty() {
        ensure_overall_budget(overall_deadline, timeouts.overall_timeout)?;
        let resolve_context = ResolveFileContext {
            repo_root,
            manifest_path: plan.manifest_path,
            cache_dir_override: plan.cache_dir_override,
        };
        stage_files(plan.files, &resolve_context, ssh)?;
    }

    // Shared ordered env map threaded across all steps (both guest and host).
    let mut accumulated_env: Vec<(String, String)> = Vec::new();
    let run_context = RunStepContext {
        repo_root,
        ssh,
        step_log_dir: &step_log_dir,
        overall_deadline,
        overall_timeout: timeouts.overall_timeout,
        default_step_timeout: timeouts.default_step_timeout,
    };

    for (step_idx, step) in plan.steps.iter().enumerate() {
        ensure_overall_budget(overall_deadline, timeouts.overall_timeout)?;
        // The file is created by StepLogWriter::create inside each step runner;
        // no pre-creation needed here (the directory was already created above).
        print_step_title(step_idx, step.display_name(), step.display_id());
        let step_result = match step {
            TestStep::Run(step) => run_run_step(&run_context, step_idx, step, &mut accumulated_env),
            TestStep::Archive(step) => {
                if let Some(executor) = archive_executor.as_mut() {
                    executor(step_idx, step)
                } else {
                    let archive_name = step
                        .archive
                        .name
                        .as_deref()
                        .unwrap_or(step.archive.src.as_str());
                    anyhow::bail!(
                        "step {} ('{}') is an `archive` step, but archive execution is not enabled for this command",
                        step_idx + 1,
                        archive_name
                    );
                }
            }
        };
        print_step_status(
            step_idx,
            step.display_name(),
            step.display_id(),
            step_result.is_ok(),
        );
        step_result?;
    }
    Ok(overall_deadline)
}

fn run_run_step(
    context: &RunStepContext<'_>,
    step_idx: usize,
    step: &RunStep,
    accumulated_env: &mut Vec<(String, String)>,
) -> Result<()> {
    let step_log_path = step_log_path(context.step_log_dir, step_idx, &step.name);
    let step_timeout = resolve_step_timeout(step.timeout, context.default_step_timeout);
    let step_budget = StepExecutionBudget {
        step_timeout,
        overall_deadline: context.overall_deadline,
        overall_timeout: context.overall_timeout,
    };

    match step.target {
        StepTarget::Guest => (|| -> Result<()> {
            let suffix = unique_suffix();
            let local_script =
                std::env::temp_dir().join(format!("botforge-step-{step_idx}-{suffix}.sh"));
            let remote_script = format!("/tmp/botforge-step-{step_idx}-{suffix}.sh");
            let remote_env_path = format!("/tmp/botforge-env-{step_idx}-{suffix}");

            // Prepend env preamble (exports + BOTFORGE_ENV setup) to the script body.
            let preamble = build_guest_env_preamble(accumulated_env, &remote_env_path);
            let script_content = format!("{preamble}{}", step.run);
            std::fs::write(&local_script, script_content.as_bytes()).with_context(|| {
                format!("test step '{}': failed to write script file", step.name)
            })?;

            let template = resolve_shell(step.shell.as_deref())
                .expect("shell already validated at config load");

            let scp_result = scp_with_retry(
                context.ssh,
                &local_script,
                &remote_script,
                TEST_TRANSPORT_RETRIES,
                TEST_TRANSPORT_RETRY_DELAY,
            )
            .with_context(|| format!("test step '{}' script upload failed", step.name));

            let step_result = if scp_result.is_ok() {
                let ssh_cmd = build_guest_ssh_cmd(&template, &remote_script, step.sudo_enabled());
                run_ssh_step_with_step_log(
                    &step.name,
                    context.ssh,
                    &ssh_cmd,
                    Duration::from_secs(300),
                    &step_log_path,
                    step_budget,
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
                    context.ssh,
                    &format!("cat {}", shell_single_quote(&remote_env_path)),
                    1,
                    Duration::from_secs(0),
                    Duration::from_secs(10),
                ) {
                    if let Ok(new_entries) = parse_env_file(&env_contents) {
                        env_merge(accumulated_env, new_entries);
                    }
                }
            }

            // Best-effort cleanup: remote env file, remote script, then local temp file.
            let _ = ssh_with_retry(
                context.ssh,
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
                context.repo_root,
                step_budget,
                &template,
                accumulated_env,
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
                        env_merge(accumulated_env, new_entries);
                    }
                }
            }

            // Best-effort cleanup of the local env file.
            let _ = std::fs::remove_file(&env_file);

            step_result
        }
    }?;
    Ok(())
}

fn resolve_step_timeout(step_timeout: Option<u64>, default_step_timeout: Duration) -> Duration {
    Duration::from_secs(step_timeout.unwrap_or(default_step_timeout.as_secs()))
}

fn overall_timeout_error(overall_timeout: Duration) -> anyhow::Error {
    anyhow::anyhow!("overall run timed out after {}s", overall_timeout.as_secs())
}

fn ensure_overall_budget(overall_deadline: Instant, overall_timeout: Duration) -> Result<()> {
    if Instant::now() >= overall_deadline {
        return Err(overall_timeout_error(overall_timeout));
    }
    Ok(())
}

fn remaining_budget(overall_deadline: Instant) -> Duration {
    overall_deadline.saturating_duration_since(Instant::now())
}

fn require_stable_ssh_with_deadline(
    ssh: &SshOptions,
    attempts: usize,
    required_consecutive: usize,
    overall_deadline: Instant,
    overall_timeout: Duration,
) -> Result<()> {
    let mut consecutive = 0usize;
    for attempt_idx in 0..attempts {
        ensure_overall_budget(overall_deadline, overall_timeout)?;
        if ssh_with_retry(
            ssh,
            "true",
            1,
            Duration::from_secs(0),
            remaining_budget(overall_deadline).min(Duration::from_secs(10)),
        )
        .is_ok()
        {
            consecutive += 1;
            if consecutive >= required_consecutive {
                return Ok(());
            }
        } else {
            consecutive = 0;
        }
        if attempt_idx + 1 < attempts {
            let sleep_for = remaining_budget(overall_deadline).min(Duration::from_secs(2));
            if sleep_for.is_zero() {
                return Err(overall_timeout_error(overall_timeout));
            }
            std::thread::sleep(sleep_for);
        }
    }
    anyhow::bail!("SSH was not stable enough after {attempts} probes")
}

#[allow(clippy::too_many_arguments)]
fn run_ssh_step_with_step_log(
    name: &str,
    ssh: &SshOptions,
    remote_command: &str,
    connect_timeout: Duration,
    log_path: &Path,
    budget: StepExecutionBudget,
    retries: usize,
    retry_delay: Duration,
) -> Result<()> {
    let logger = Arc::new(StepLogWriter::create(log_path)?);
    let mut attempts = 0usize;
    loop {
        // Per-attempt output buffers for line-oriented log writing.
        let mut pending_out: Vec<u8> = Vec::new();
        let mut pending_err: Vec<u8> = Vec::new();

        let logger_ref = Arc::clone(&logger);
        let mut on_output = |is_stderr: bool, data: &[u8]| {
            use std::io::Write;
            if is_stderr {
                let _ = std::io::stderr().write_all(data);
                pending_err.extend_from_slice(data);
                flush_log_lines(&logger_ref, &mut pending_err, StepOutputStream::Stderr);
            } else {
                let _ = std::io::stdout().write_all(data);
                pending_out.extend_from_slice(data);
                flush_log_lines(&logger_ref, &mut pending_out, StepOutputStream::Stdout);
            }
        };

        let outcome = ssh_exec_logged(
            ssh,
            remote_command,
            connect_timeout,
            budget.step_timeout,
            budget.overall_deadline,
            &mut on_output,
        );

        // Flush any partial (no-newline) buffered lines to the log.
        if !pending_out.is_empty() {
            let _ = logger_ref.log_line(StepOutputStream::Stdout, &pending_out);
        }
        if !pending_err.is_empty() {
            let _ = logger_ref.log_line(StepOutputStream::Stderr, &pending_err);
        }

        match outcome {
            SshExecOutcome::Success => return Ok(()),
            SshExecOutcome::StepTimeout => {
                anyhow::bail!(
                    "guest step '{}' timed out after {}s",
                    name,
                    budget.step_timeout.as_secs()
                );
            }
            SshExecOutcome::OverallTimeout => {
                return Err(overall_timeout_error(budget.overall_timeout));
            }
            SshExecOutcome::RemoteFailure(code) => {
                // Remote command ran and exited non-zero — fail fast, no retry.
                anyhow::bail!("ssh command failed (exit status: {code})");
            }
            SshExecOutcome::TransportError(e) => {
                attempts += 1;
                // Retry only on transport errors (equivalent to the old exit-code-255 gate).
                if attempts >= retries {
                    anyhow::bail!("ssh command failed (transport error, retries exhausted): {e:#}");
                }
                std::thread::sleep(retry_delay);
            }
        }
    }
}

/// Write complete newline-terminated lines from `pending` to the log.
/// Bytes before the first newline are left in `pending` for the next chunk.
fn flush_log_lines(logger: &Arc<StepLogWriter>, pending: &mut Vec<u8>, stream: StepOutputStream) {
    while let Some(pos) = pending.iter().position(|&b| b == b'\n') {
        let mut line: Vec<u8> = pending.drain(..=pos).collect();
        line.pop(); // remove the '\n'
        let _ = logger.log_line(stream, &line);
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
    budget: StepExecutionBudget,
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

    let step_deadline = Instant::now() + budget.step_timeout;
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
                match timeout_cause(step_deadline, budget.overall_deadline) {
                    Some(WaitResult::StepTimeout) => {
                        let _ = child.kill();
                        let _ = child.wait();
                        break Err(anyhow::anyhow!(
                            "host step '{name}' timed out after {}s",
                            budget.step_timeout.as_secs()
                        ));
                    }
                    Some(WaitResult::OverallTimeout) => {
                        let _ = child.kill();
                        let _ = child.wait();
                        break Err(overall_timeout_error(budget.overall_timeout));
                    }
                    None => {}
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WaitResult {
    StepTimeout,
    OverallTimeout,
}

fn timeout_cause(step_deadline: Instant, overall_deadline: Instant) -> Option<WaitResult> {
    let now = Instant::now();
    if step_deadline <= overall_deadline {
        if now >= step_deadline {
            Some(WaitResult::StepTimeout)
        } else if now >= overall_deadline {
            Some(WaitResult::OverallTimeout)
        } else {
            None
        }
    } else if now >= overall_deadline {
        Some(WaitResult::OverallTimeout)
    } else if now >= step_deadline {
        Some(WaitResult::StepTimeout)
    } else {
        None
    }
}

fn spawn_logged_child(
    command: &mut Command,
    logger: Arc<StepLogWriter>,
    spawn_context: &str,
) -> Result<(Child, Vec<std::thread::JoinHandle<Result<()>>>)> {
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

fn shell_single_quote(value: &str) -> String {
    crate::util::shell_single_quote(value)
}

fn build_guest_ssh_cmd(template: &[String], remote_script: &str, sudo: bool) -> String {
    let ssh_cmd = template
        .iter()
        .map(|arg| {
            if arg == "{0}" {
                shell_single_quote(remote_script)
            } else {
                arg.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" ");
    if sudo {
        format!("sudo -E {ssh_cmd}")
    } else {
        ssh_cmd
    }
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
fn parse_env_file(contents: &str) -> Result<Vec<(String, String)>> {
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

pub(crate) fn collect_test_diagnostics(ssh: &SshOptions, units: &[String]) {
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

pub(crate) fn print_log_tail(path: &Path, line_count: usize) {
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

pub(crate) fn cleanup_test(vm_child: &mut Option<Child>, overlay_image: &Path) {
    if let Some(child) = vm_child.as_mut() {
        let _ = child.kill();
        let _ = child.wait();
    }
    *vm_child = None;
    let _ = std::fs::remove_file(overlay_image);
}

pub(crate) fn preserve_failed_build_disk(partial: &Path, failed_partial: &Path) -> Result<()> {
    if failed_partial.exists() {
        std::fs::remove_file(failed_partial).with_context(|| {
            format!(
                "cannot replace previous failed build disk: {}",
                failed_partial.display()
            )
        })?;
    }
    std::fs::rename(partial, failed_partial).with_context(|| {
        format!(
            "cannot preserve failed build disk from {} to {}",
            partial.display(),
            failed_partial.display()
        )
    })?;
    Ok(())
}

const BUILD_POWEROFF_TIMEOUT: Duration = Duration::from_secs(120);

/// Issue a graceful poweroff over SSH, then poll for the qemu process to exit cleanly.
///
/// On a clean shutdown (`Ok(())`), the caller should atomically rename the partial disk
/// to the final output path. On failure (`Err(...)`), this preserves the tainted disk at
/// `<output>.partial.failed` for post-mortem; the caller must NOT rename it to the output path.
///
/// Only calls `child.kill()` if the 120 s timeout fires — killing a live-write qcow2
/// yields a non-fsck-clean image.
pub(crate) fn shutdown_build_vm(
    vm_child: &mut Option<Child>,
    partial: &Path,
    failed_partial: &Path,
    ssh: &SshOptions,
    request_poweroff: bool,
    overall_deadline: Instant,
    overall_timeout: Duration,
) -> Result<()> {
    if Instant::now() >= overall_deadline {
        if let Some(child) = vm_child.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
        *vm_child = None;
        preserve_failed_build_disk(partial, failed_partial)?;
        return Err(overall_timeout_error(overall_timeout));
    }

    if request_poweroff {
        // Best-effort graceful poweroff; ignore SSH errors (VM may be unresponsive).
        let _ = ssh_with_retry(
            ssh,
            "sudo systemctl poweroff",
            1,
            Duration::from_secs(0),
            Duration::from_secs(10),
        );
    }

    let mut timed_out_overall = false;
    let clean_exit = if let Some(child) = vm_child.as_mut() {
        let deadline = Instant::now() + BUILD_POWEROFF_TIMEOUT;
        loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    break status.success();
                }
                Ok(None) => {
                    if Instant::now() >= overall_deadline {
                        let _ = child.kill();
                        let _ = child.wait();
                        timed_out_overall = true;
                        break false;
                    }
                    if Instant::now() >= deadline {
                        let _ = child.kill();
                        let _ = child.wait();
                        break false; // timeout
                    }
                    std::thread::sleep(Duration::from_millis(500));
                }
                Err(e) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    eprintln!("warning: failed to poll build VM status: {e}");
                    break false;
                }
            }
        }
    } else {
        false
    };
    *vm_child = None;

    if timed_out_overall {
        preserve_failed_build_disk(partial, failed_partial)?;
        Err(overall_timeout_error(overall_timeout))
    } else if clean_exit {
        Ok(())
    } else {
        preserve_failed_build_disk(partial, failed_partial)?;
        anyhow::bail!(
            "build VM did not shut down cleanly; \
             partial disk left at {} for post-mortem",
            failed_partial.display()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::{
        build_guest_env_preamble, build_guest_ssh_cmd, env_merge, parse_env_file,
        resolve_step_timeout, run_host_step, shell_single_quote, HostStepFiles,
        StepExecutionBudget,
    };
    use crate::plan::step::{resolve_shell, RunStep, StepTarget};
    use crate::util::unique_suffix;
    use serde::Deserialize;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

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

    fn test_budget() -> StepExecutionBudget {
        let overall_timeout = Duration::from_secs(30);
        StepExecutionBudget {
            step_timeout: Duration::from_secs(10),
            overall_deadline: Instant::now() + overall_timeout,
            overall_timeout,
        }
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
            test_budget(),
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
            test_budget(),
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
            test_budget(),
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
            test_budget(),
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
            test_budget(),
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
            test_budget(),
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
            test_budget(),
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

    #[test]
    fn test_build_guest_ssh_cmd_prefixes_sudo_for_guest_root_step() {
        let tmpl = resolve_shell(None).unwrap();
        let cmd = build_guest_ssh_cmd(&tmpl, "/tmp/botforge-step.sh", true);
        assert_eq!(
            cmd,
            "sudo -E bash --noprofile --norc -e -o pipefail '/tmp/botforge-step.sh'"
        );
    }

    #[test]
    fn test_build_guest_ssh_cmd_without_sudo_matches_previous_command() {
        let tmpl = resolve_shell(Some("sh")).unwrap();
        let cmd = build_guest_ssh_cmd(&tmpl, "/tmp/botforge-step.sh", false);
        assert_eq!(cmd, "sh -e '/tmp/botforge-step.sh'");
    }

    #[test]
    fn test_guest_step_omitted_sudo_defaults_to_sudo_prefix() {
        let step: RunStep = serde_yaml::from_str(
            r#"
name: default-root
run: echo ok
"#,
        )
        .unwrap();
        let tmpl = resolve_shell(None).unwrap();
        let cmd = build_guest_ssh_cmd(&tmpl, "/tmp/botforge-step.sh", step.sudo_enabled());
        assert_eq!(
            cmd,
            "sudo -E bash --noprofile --norc -e -o pipefail '/tmp/botforge-step.sh'"
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
            StepExecutionBudget {
                step_timeout: Duration::from_millis(400),
                ..test_budget()
            },
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

    #[test]
    fn test_host_step_overall_timeout_returns_distinct_error() {
        let dir = tempfile::tempdir().unwrap();
        let tmpl = resolve_shell(Some("sh")).unwrap();
        let env_file = tmp_env_file();
        let log_file = tmp_step_log(dir.path());
        let overall_timeout = Duration::from_secs(1);
        let err = run_host_step(
            "slow-overall",
            "exec sleep 5",
            dir.path(),
            StepExecutionBudget {
                step_timeout: Duration::from_secs(5),
                overall_deadline: Instant::now() + overall_timeout,
                overall_timeout,
            },
            &tmpl,
            &[],
            HostStepFiles {
                env_file: &env_file,
                log_path: &log_file,
            },
        )
        .unwrap_err();
        let _ = std::fs::remove_file(&env_file);
        let msg = err.to_string();
        assert!(
            msg.contains("overall run timed out after 1s"),
            "overall timeout error should be distinct: {msg}"
        );
        assert!(
            !msg.contains("slow-overall"),
            "overall timeout should not be reported as a step timeout: {msg}"
        );
    }

    #[test]
    fn test_resolve_step_timeout_prefers_per_step_override() {
        let step = RunStep {
            target: StepTarget::Host,
            name: "timeout-step".to_string(),
            run: "echo ok".to_string(),
            timeout: Some(45),
            shell: None,
            sudo: None,
            id: None,
        };
        assert_eq!(
            resolve_step_timeout(step.timeout, Duration::from_secs(300)),
            Duration::from_secs(45)
        );
    }

    #[test]
    fn test_resolve_step_timeout_falls_back_to_document_default() {
        let step = RunStep {
            target: StepTarget::Host,
            name: "timeout-step".to_string(),
            run: "echo ok".to_string(),
            timeout: None,
            shell: None,
            sudo: None,
            id: None,
        };
        assert_eq!(
            resolve_step_timeout(step.timeout, Duration::from_secs(1800)),
            Duration::from_secs(1800)
        );
    }
}
