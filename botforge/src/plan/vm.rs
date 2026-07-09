use anyhow::{Context, Result};
use glob::MatchOptions;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::ssh::{
    journalctl_command, scp_with_retry, ssh_capture_stdout, ssh_command_args, ssh_with_retry,
    wait_for_ssh, SshOptions,
};
use crate::util::{resolve_under_root, unique_suffix};

use super::config::{src_has_glob_metacharacters, TestConfig, TestIsoBootstrap};
use super::log::{
    join_output_forwarders, print_step_status, print_step_title, spawn_output_forwarder,
    step_log_path, StepLogWriter, StepOutputStream,
};
use super::step::{resolve_shell, ArchiveStep, RunStep, StepTarget, TestStep, TopLevelUpload};

const TEST_SSH_READY_TIMEOUT: Duration = Duration::from_secs(300);
const TEST_TRANSPORT_RETRIES: usize = 10;
const TEST_TRANSPORT_RETRY_DELAY: Duration = Duration::from_secs(2);
const TEST_STABLE_SSH_ATTEMPTS: usize = 5;
const TEST_STABLE_SSH_REQUIRED: usize = 2;
type ArchiveExecutor<'a> = dyn FnMut(usize, &ArchiveStep) -> Result<()> + 'a;

pub(crate) struct StepFlowPlan<'a> {
    pub(crate) top_level_uploads: &'a [TopLevelUpload],
    pub(crate) steps: &'a [TestStep],
    pub(crate) bootstraps: &'a [TestIsoBootstrap],
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
) -> Result<()> {
    run_step_flow(
        repo_root,
        StepFlowPlan {
            top_level_uploads: &config.uploads,
            steps: &config.steps,
            bootstraps,
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

    if !plan.top_level_uploads.is_empty() {
        ensure_overall_budget(overall_deadline, timeouts.overall_timeout)?;
        stage_top_level_uploads(repo_root, plan.top_level_uploads, ssh)?;
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
        print_step_status(step_idx, step.display_name(), step.display_id(), step_result.is_ok());
        step_result?;
    }
    Ok(overall_deadline)
}

/// Options controlling how `install_file_to_guest` places a file in the guest.
struct InstallOpts<'a> {
    /// File permission mode (3–4 octal digits). Defaults to `"0644"`.
    mode: Option<&'a str>,
    /// Owner (user name or numeric uid). Defaults to `"root"`.
    owner: Option<&'a str>,
    /// Group (group name or numeric gid). Defaults to `"root"`.
    group: Option<&'a str>,
    /// If `false`, fail with an error when `dest` already exists. Defaults to `true`.
    overwrite: Option<bool>,
    /// If `true` (default), create intermediate destination directories (`install -D`).
    parents: Option<bool>,
}

/// Stage a local file to the guest using `sudo install`, applying the given `opts`.
///
/// Uses `sudo install` so it can set mode, owner, and group atomically.  The temporary file
/// is always cleaned up after the install attempt.
fn install_file_to_guest(
    ssh: &SshOptions,
    local_blob: &Path,
    dest: &str,
    opts: &InstallOpts<'_>,
    temp_label: &str,
) -> Result<()> {
    let dest_q = shell_single_quote(dest);
    let suffix = unique_suffix();
    let remote_tmp = format!("/tmp/botforge-upload-{temp_label}-{suffix}");
    let remote_tmp_q = shell_single_quote(&remote_tmp);

    scp_with_retry(
        ssh,
        local_blob,
        &remote_tmp,
        TEST_TRANSPORT_RETRIES,
        TEST_TRANSPORT_RETRY_DELAY,
    )
    .with_context(|| format!("failed to scp file to guest for install to '{dest}'"))?;

    // If overwrite is disabled, precheck that dest does not exist.
    let overwrite = opts.overwrite.unwrap_or(true);
    if !overwrite {
        let precheck_result = ssh_with_retry(
            ssh,
            &format!("sudo test ! -e {dest_q}"),
            1,
            Duration::from_secs(0),
            Duration::from_secs(30),
        );
        if precheck_result.is_err() {
            let _ = ssh_with_retry(
                ssh,
                &format!("rm -f {remote_tmp_q}"),
                1,
                Duration::from_secs(0),
                Duration::from_secs(10),
            );
            anyhow::bail!("upload dest '{dest}' already exists and overwrite is false");
        }
    }

    let mode = opts.mode.unwrap_or("0644");
    let owner = opts.owner.unwrap_or("root");
    let group = opts.group.unwrap_or("root");
    let parents = opts.parents.unwrap_or(true);
    let owner_q = shell_single_quote(owner);
    let group_q = shell_single_quote(group);

    let install_cmd = if parents {
        format!("sudo install -D -m {mode} -o {owner_q} -g {group_q} {remote_tmp_q} {dest_q}")
    } else {
        format!("sudo install -m {mode} -o {owner_q} -g {group_q} {remote_tmp_q} {dest_q}")
    };

    let install_result = ssh_with_retry(
        ssh,
        &install_cmd,
        1,
        Duration::from_secs(0),
        Duration::from_secs(30),
    )
    .with_context(|| format!("failed to install file to '{dest}' in guest"));

    // Best-effort cleanup of the temp file regardless of install success/failure.
    let _ = ssh_with_retry(
        ssh,
        &format!("rm -f {remote_tmp_q}"),
        1,
        Duration::from_secs(0),
        Duration::from_secs(10),
    );

    install_result
}

fn stage_top_level_uploads(
    repo_root: &Path,
    uploads: &[TopLevelUpload],
    ssh: &SshOptions,
) -> Result<()> {
    for (upload_idx, upload) in uploads.iter().enumerate() {
        let mappings = resolve_top_level_upload_mappings(repo_root, upload)?;
        let opts = InstallOpts {
            mode: upload.mode.as_deref(),
            owner: upload.owner.as_deref(),
            group: upload.group.as_deref(),
            overwrite: upload.overwrite,
            parents: upload.parents,
        };
        for (mapping_idx, mapping) in mappings.iter().enumerate() {
            install_file_to_guest(
                ssh,
                &mapping.local_path,
                &mapping.guest_dest,
                &opts,
                &format!("{upload_idx}-{mapping_idx}"),
            )
            .with_context(|| {
                format!(
                    "top-level upload '{}' failed while staging '{}' to '{}'",
                    upload.src,
                    mapping.local_path.display(),
                    mapping.guest_dest
                )
            })?;
            println!(
                "top-level upload {} staged {} -> {}",
                upload_idx + 1,
                mapping.local_path.display(),
                mapping.guest_dest
            );
        }
    }
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct UploadMapping {
    pub(crate) local_path: PathBuf,
    pub(crate) guest_dest: String,
}

pub(crate) fn resolve_top_level_upload_mappings(
    repo_root: &Path,
    upload: &TopLevelUpload,
) -> Result<Vec<UploadMapping>> {
    let src = upload.src.trim();
    let dest = upload.dest.trim();
    validate_top_level_upload_src_for_runtime(src)?;
    if !src_has_glob_metacharacters(src) {
        let local_path = resolve_under_root(repo_root, PathBuf::from(src));
        if !local_path.is_file() {
            anyhow::bail!(
                "upload src '{}' does not resolve to a regular file under {}",
                src,
                repo_root.display()
            );
        }
        let guest_dest = if dest.ends_with('/') {
            let basename = local_path.file_name().with_context(|| {
                format!("upload src '{}' has no file name", local_path.display())
            })?;
            Path::new(dest)
                .join(basename)
                .to_string_lossy()
                .into_owned()
        } else {
            dest.to_string()
        };
        return Ok(vec![UploadMapping {
            local_path,
            guest_dest,
        }]);
    }

    let fixed_prefix = fixed_glob_prefix(src);
    let fixed_prefix_root = repo_root.join(&fixed_prefix);
    let pattern = repo_root.join(src).to_string_lossy().into_owned();
    let match_options = MatchOptions {
        case_sensitive: true,
        require_literal_separator: false,
        require_literal_leading_dot: false,
    };
    let mut mappings = Vec::new();
    for entry in glob::glob_with(&pattern, match_options)
        .with_context(|| format!("invalid upload src glob '{}'", src))?
    {
        let local_path = entry.with_context(|| {
            format!(
                "failed while expanding upload src glob '{}' under {}",
                src,
                repo_root.display()
            )
        })?;
        if !local_path.is_file() {
            continue;
        }
        let relative = local_path
            .strip_prefix(&fixed_prefix_root)
            .with_context(|| {
                format!(
                    "upload src glob '{}' produced '{}' outside fixed prefix '{}'",
                    src,
                    local_path.display(),
                    fixed_prefix_root.display()
                )
            })?;
        let guest_dest = Path::new(dest)
            .join(relative)
            .to_string_lossy()
            .into_owned();
        mappings.push(UploadMapping {
            local_path,
            guest_dest,
        });
    }

    if mappings.is_empty() {
        anyhow::bail!(
            "no files matched upload src glob '{}' under {}",
            src,
            repo_root.display()
        );
    }

    Ok(mappings)
}

fn fixed_glob_prefix(src: &str) -> PathBuf {
    let mut prefix = PathBuf::new();
    for component in Path::new(src).components() {
        let std::path::Component::Normal(part) = component else {
            break;
        };
        if src_has_glob_metacharacters(&part.to_string_lossy()) {
            break;
        }
        prefix.push(part);
    }
    prefix
}

fn validate_top_level_upload_src_for_runtime(src: &str) -> Result<()> {
    let path = Path::new(src);
    if path.as_os_str().is_empty() {
        anyhow::bail!("upload src must not be empty");
    }
    if path.is_absolute() {
        anyhow::bail!("upload src must be repo-relative, got: {}", path.display());
    }
    for component in path.components() {
        match component {
            std::path::Component::Normal(_) => {}
            _ => anyhow::bail!(
                "upload src must contain no '.' or '..' segments: {}",
                path.display()
            ),
        }
    }
    Ok(())
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
                let ssh_cmd =
                    build_guest_ssh_cmd(&template, &remote_script, step.sudo == Some(true));
                run_ssh_step_with_step_log(
                    &step.name,
                    ssh_command_args(context.ssh, &ssh_cmd, Duration::from_secs(300).as_secs()),
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

fn run_ssh_step_with_step_log(
    name: &str,
    args: Vec<String>,
    log_path: &Path,
    budget: StepExecutionBudget,
    retries: usize,
    retry_delay: Duration,
) -> Result<()> {
    let logger = Arc::new(StepLogWriter::create(log_path)?);
    let mut attempts = 0usize;
    loop {
        let mut command = Command::new("ssh");
        command.args(&args);
        let (mut child, forwarders) =
            spawn_logged_child(&mut command, Arc::clone(&logger), "failed to execute ssh")?;

        // Poll with a bounded deadline; kill and surface a timeout error if
        // the step does not exit within its own timeout or the overall run budget.
        let step_deadline = Instant::now() + budget.step_timeout;
        let wait_result: Result<WaitResult> = loop {
            match child.try_wait() {
                Ok(Some(status)) => break Ok(WaitResult::Exit(status)),
                Ok(None) => {
                    if let Some(timeout_result) =
                        timeout_cause(step_deadline, budget.overall_deadline)
                    {
                        let _ = child.kill();
                        let _ = child.wait();
                        break Ok(timeout_result);
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
            WaitResult::StepTimeout => {
                anyhow::bail!(
                    "guest step '{}' timed out after {}s",
                    name,
                    budget.step_timeout.as_secs()
                );
            }
            WaitResult::OverallTimeout => {
                return Err(overall_timeout_error(budget.overall_timeout))
            }
            WaitResult::Exit(status) => status,
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
                    Some(WaitResult::Exit(_)) => unreachable!(),
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
    Exit(std::process::ExitStatus),
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
        resolve_step_timeout, resolve_top_level_upload_mappings, run_host_step, shell_single_quote,
        HostStepFiles, StepExecutionBudget, UploadMapping,
    };
    use crate::plan::step::{resolve_shell, RunStep, StepTarget, TopLevelUpload};
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
    fn test_resolve_top_level_upload_mappings_preserves_glob_relative_paths() {
        let repo = tempfile::tempdir().unwrap();
        let ecds = repo.path().join("images/botspace/envoy/ecds");
        std::fs::create_dir_all(&ecds).unwrap();
        let file = ecds.join("ext_authz.yaml");
        std::fs::write(&file, "kind: envoy\n").unwrap();

        let mappings = resolve_top_level_upload_mappings(
            repo.path(),
            &TopLevelUpload {
                src: "images/botspace/envoy/**/*.yaml".to_string(),
                dest: "/tmp/bake-staging/envoy/".to_string(),
                ..Default::default()
            },
        )
        .unwrap();

        assert_eq!(
            mappings,
            vec![UploadMapping {
                local_path: file,
                guest_dest: "/tmp/bake-staging/envoy/ecds/ext_authz.yaml".to_string(),
            }]
        );
    }

    #[test]
    fn test_resolve_top_level_upload_mappings_preserves_flat_glob_matches() {
        let repo = tempfile::tempdir().unwrap();
        let payload = repo.path().join("build/images/payload");
        std::fs::create_dir_all(&payload).unwrap();
        let file = payload.join("mcp-fs.tar");
        std::fs::write(&file, "tarball").unwrap();

        let mappings = resolve_top_level_upload_mappings(
            repo.path(),
            &TopLevelUpload {
                src: "build/images/payload/*.tar".to_string(),
                dest: "/usr/share/botwork/images/".to_string(),
                ..Default::default()
            },
        )
        .unwrap();

        assert_eq!(
            mappings,
            vec![UploadMapping {
                local_path: file,
                guest_dest: "/usr/share/botwork/images/mcp-fs.tar".to_string(),
            }]
        );
    }

    #[test]
    fn test_resolve_top_level_upload_mappings_literal_dest_directory_uses_basename() {
        let repo = tempfile::tempdir().unwrap();
        let local = repo.path().join("scripts/setup.sh");
        std::fs::create_dir_all(local.parent().unwrap()).unwrap();
        std::fs::write(&local, "#!/bin/sh\n").unwrap();

        let mappings = resolve_top_level_upload_mappings(
            repo.path(),
            &TopLevelUpload {
                src: "scripts/setup.sh".to_string(),
                dest: "/tmp/staging/".to_string(),
                ..Default::default()
            },
        )
        .unwrap();

        assert_eq!(
            mappings,
            vec![UploadMapping {
                local_path: local,
                guest_dest: "/tmp/staging/setup.sh".to_string(),
            }]
        );
    }

    #[test]
    fn test_resolve_top_level_upload_mappings_zero_match_is_error() {
        let repo = tempfile::tempdir().unwrap();
        let err = resolve_top_level_upload_mappings(
            repo.path(),
            &TopLevelUpload {
                src: "images/**/*.yaml".to_string(),
                dest: "/tmp/staging/".to_string(),
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("no files matched"));
    }

    #[test]
    fn test_resolve_top_level_upload_mappings_rejects_traversal() {
        let repo = tempfile::tempdir().unwrap();
        let err = resolve_top_level_upload_mappings(
            repo.path(),
            &TopLevelUpload {
                src: "images/../secret.txt".to_string(),
                dest: "/tmp/staging/".to_string(),
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains(".."));
    }

    #[test]
    fn test_resolve_top_level_upload_mappings_skips_directories_and_requires_files() {
        let repo = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(repo.path().join("images/botspace/envoy/ecds")).unwrap();
        let err = resolve_top_level_upload_mappings(
            repo.path(),
            &TopLevelUpload {
                src: "images/botspace/envoy/**".to_string(),
                dest: "/tmp/staging/".to_string(),
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("no files matched"));
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
