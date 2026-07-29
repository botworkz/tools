use anyhow::{Context, Result};
use serde_yaml::Value;
use shasset::manifest::Manifest;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::resolver::ResolveFileContext;
use crate::signal;
use crate::ssh::{
    journalctl_command, scp_with_retry, ssh_capture_stdout, ssh_exec_logged, ssh_with_retry,
    wait_for_ssh, SshExecOutcome, SshOptions,
};
use crate::util::unique_suffix;

use super::files::{stage_files, FileEntry};
use super::log::{
    join_output_forwarders, print_step_skipped, print_step_status, print_step_title,
    spawn_capturing_forwarder, spawn_output_forwarder, step_log_path, StepLogWriter,
    StepOutputStream,
};
use crate::assert::{registry::built_in_assert_registry, AssertBlock};
use crate::config::{
    resolve_deferred_condition, resolve_deferred_refs_in_string, EvaluatedValue, TestConfig,
    TestIsoBootstrap,
};
use crate::step::{
    capture_step_outputs, coerce_output_value, resolve_shell, ArchiveStep, CapturedOutput,
    ExpectBlock, FragmentOutputDecl, InvokeStep, OutputValue, RunStep, StdioExpect, StepCondition,
    StepTarget, TestStep,
};

const TEST_SSH_READY_TIMEOUT: Duration = Duration::from_secs(300);
const TEST_TRANSPORT_RETRIES: usize = 10;
const TEST_TRANSPORT_RETRY_DELAY: Duration = Duration::from_secs(2);
const TEST_STABLE_SSH_ATTEMPTS: usize = 5;
const TEST_STABLE_SSH_REQUIRED: usize = 2;
type ArchiveExecutor<'a> = dyn FnMut(&str, &ArchiveStep) -> Result<()> + 'a;
type PreStepsHook<'a> = dyn Fn(&SshOptions) -> Result<()> + 'a;

pub(crate) struct StepFlowPlan<'a> {
    pub(crate) files: &'a [FileEntry],
    pub(crate) steps: &'a [TestStep],
    pub(crate) bootstraps: &'a [TestIsoBootstrap],
    /// Parsed inline asset manifest, forwarded to the resolver for `@` references.
    pub(crate) manifest: &'a Manifest,
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
    context: &'a Path,
    ssh: &'a SshOptions,
    step_log_dir: &'a Path,
    overall_deadline: Instant,
    overall_timeout: Duration,
    default_step_timeout: Duration,
}

// ---------------------------------------------------------------------------
// VM runtime (formerly run.rs)
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
pub(crate) fn run_test_flow(
    context: &Path,
    config: &TestConfig,
    ssh: &SshOptions,
    bootstraps: &[TestIsoBootstrap],
    manifest: &Manifest,
    cache_dir_override: Option<&Path>,
    installer_username: Option<&str>,
    plugin_registry: &botforge_plugin_host::PluginRegistry,
    vm_child: Option<&mut Child>,
) -> Result<()> {
    // Run the declarative `assert:` block as a pre-steps phase (after boot /
    // SSH / cloud-init, but before the first `steps:` entry) so that
    // assertions validate the image as built rather than the post-steps state.
    let pre_steps: Option<Box<PreStepsHook<'_>>> = config.assert.as_ref().map(|assert_block| {
        Box::new(move |ssh: &SshOptions| {
            run_assert_phase(ssh, assert_block, installer_username, plugin_registry)
        }) as Box<PreStepsHook<'_>>
    });

    run_step_flow(
        context,
        StepFlowPlan {
            files: &config.files,
            steps: &config.steps,
            bootstraps,
            manifest,
            cache_dir_override,
        },
        ssh,
        StepTimeoutPolicy {
            overall_timeout: Duration::from_secs(config.timeout),
            default_step_timeout: Duration::from_secs(config.step_timeout),
            cloud_init_timeout: Duration::from_secs(config.cloud_init_timeout),
        },
        None,
        pre_steps.as_deref(),
        vm_child,
    )
    .map(|_| ())
}

/// Execute the `assert:` block as a pre-steps phase (after boot/SSH/cloud-init,
/// before any `steps:` entry).
///
/// `installer_username` is the ephemeral botforge installer account for this
/// run (e.g. `botforge-abc123`).  Both the installer user and its same-named
/// primary group are excluded from pattern-based assertions (positive and
/// negative) so that `botforge-*: { exists: false }` does not spuriously fail
/// and `botforge-*: { exists: true }` cannot be satisfied by the installer
/// identity alone.
fn run_assert_phase(
    ssh: &SshOptions,
    assert_block: &AssertBlock,
    installer_username: Option<&str>,
    plugin_registry: &botforge_plugin_host::PluginRegistry,
) -> Result<()> {
    let registry = built_in_assert_registry();
    for kind in registry.iter() {
        if kind.is_empty(assert_block) {
            continue;
        }
        kind.run(ssh, assert_block, installer_username)?;
    }

    // Dispatch plugin-provided assert verbs.
    for (verb, raw_value) in &assert_block.plugin_asserts {
        let handle = plugin_registry.get_assert(verb).ok_or_else(|| {
            let builtins = built_in_assert_registry().known_verbs().join(", ");
            let plugin_names = plugin_registry.assert_names().join(", ");
            let plugin_part = if plugin_names.is_empty() {
                "(no plugin-provided assert verbs loaded)".to_owned()
            } else {
                format!("plugin verbs: {plugin_names}")
            };
            anyhow::anyhow!(
                "no assert provider for verb '{verb}' \
                 (built-in verbs: {builtins}; {plugin_part})"
            )
        })?;
        let config_json = serde_json::to_string(raw_value).map_err(|e| {
            anyhow::anyhow!("assert.{verb}: failed to serialize config to JSON: {e}")
        })?;
        let script = handle
            .build_probe(&config_json)
            .map_err(|e| anyhow::anyhow!("assert.{verb}: build_probe failed: {e}"))?;
        let probe_stdout = crate::assert::run_privileged_probe(ssh, &script, "__PLUGIN_ASSERT__")
            .map_err(|e| anyhow::anyhow!("assert.{verb}: probe failed: {e:#}"))?;
        let results_json = handle
            .evaluate(&config_json, &probe_stdout)
            .map_err(|e| anyhow::anyhow!("assert.{verb}: evaluate failed: {e}"))?;
        run_plugin_assert_checks(verb, &results_json)?;
    }

    Ok(())
}

/// Parse results JSON from a plugin assert provider and render per-check status.
///
/// Results JSON contract:
/// ```json
/// { "checks": [ { "label": "...", "ok": true, "message": null } ] }
/// ```
fn run_plugin_assert_checks(verb: &str, results_json: &str) -> Result<()> {
    use serde::Deserialize;
    #[derive(Deserialize)]
    struct CheckResult {
        label: String,
        ok: bool,
        message: Option<String>,
    }
    #[derive(Deserialize)]
    struct Results {
        checks: Vec<CheckResult>,
    }

    let results: Results = serde_json::from_str(results_json)
        .map_err(|e| anyhow::anyhow!("assert.{verb}: invalid results JSON: {e}"))?;

    let mut any_failed = false;
    for check in &results.checks {
        crate::plan::log::print_phase_status("assert", &check.label, check.ok, None);
        if !check.ok {
            if let Some(ref msg) = check.message {
                eprintln!("         {msg}");
            }
            any_failed = true;
        }
    }

    if any_failed {
        anyhow::bail!("one or more assert.{verb} checks failed");
    }
    Ok(())
}

/// Shared boot→wait-for-SSH→wait-for-cloud-init→run-steps spine used by both
/// `botforge test` and `botforge build`.  `bootstraps` is empty for build runs.
///
/// `pre_steps` is an optional callback invoked after cloud-init/bootstraps/files
/// staging but **before** the first step executes.  `botforge test` uses this to
/// run the declarative `assert:` phase on the fresh-boot image state.
pub(crate) fn run_step_flow(
    context: &Path,
    plan: StepFlowPlan<'_>,
    ssh: &SshOptions,
    timeouts: StepTimeoutPolicy,
    mut archive_executor: Option<&mut ArchiveExecutor<'_>>,
    pre_steps: Option<&PreStepsHook<'_>>,
    vm_child: Option<&mut Child>,
) -> Result<Instant> {
    let overall_deadline = Instant::now() + timeouts.overall_timeout;
    let step_log_dir = context.join("build").join("logs");
    std::fs::create_dir_all(&step_log_dir).with_context(|| {
        format!(
            "cannot create test step log dir: {}",
            step_log_dir.display()
        )
    })?;
    ensure_overall_budget(overall_deadline, timeouts.overall_timeout)?;
    crate::plan::print_phase("vm", "Waiting for SSH");
    let wait_for_ssh_started = Instant::now();
    let wait_for_ssh_result = wait_for_ssh(
        ssh,
        remaining_budget(overall_deadline).min(TEST_SSH_READY_TIMEOUT),
        vm_child,
    );
    crate::plan::print_phase_status(
        "vm",
        "Waiting for SSH",
        wait_for_ssh_result.is_ok(),
        Some(wait_for_ssh_started.elapsed()),
    );
    wait_for_ssh_result?;
    ensure_overall_budget(overall_deadline, timeouts.overall_timeout)?;
    crate::plan::print_phase("vm", "Waiting for cloud-init");
    let cloud_init_started = Instant::now();
    let cloud_init_result = ssh_with_retry(
        ssh,
        "sudo cloud-init status --wait",
        TEST_TRANSPORT_RETRIES,
        TEST_TRANSPORT_RETRY_DELAY,
        remaining_budget(overall_deadline).min(timeouts.cloud_init_timeout),
    );
    crate::plan::print_phase_status(
        "vm",
        "Waiting for cloud-init",
        cloud_init_result.is_ok(),
        Some(cloud_init_started.elapsed()),
    );
    cloud_init_result?;
    crate::plan::print_phase("vm", "Waiting for stable SSH");
    let stable_ssh_started = Instant::now();
    let stable_ssh_result = require_stable_ssh_with_deadline(
        ssh,
        TEST_STABLE_SSH_ATTEMPTS,
        TEST_STABLE_SSH_REQUIRED,
        overall_deadline,
        timeouts.overall_timeout,
    );
    crate::plan::print_phase_status(
        "vm",
        "Waiting for stable SSH",
        stable_ssh_result.is_ok(),
        Some(stable_ssh_started.elapsed()),
    );
    stable_ssh_result?;

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
            context,
            manifest: plan.manifest,
            cache_dir_override: plan.cache_dir_override,
        };
        stage_files(plan.files, &resolve_context, ssh)?;
    }

    // Shared ordered env map threaded across all steps (both guest and host).
    let mut accumulated_env: Vec<(String, String)> = Vec::new();
    let run_context = RunStepContext {
        context,
        ssh,
        step_log_dir: &step_log_dir,
        overall_deadline,
        overall_timeout: timeouts.overall_timeout,
        default_step_timeout: timeouts.default_step_timeout,
    };

    // Run the optional pre-steps hook (e.g. the declarative `assert:` phase
    // for `botforge test`) before any step mutates the guest.
    if let Some(hook) = pre_steps {
        hook(ssh)?;
    }

    run_steps_inner(
        &run_context,
        plan.steps,
        &[],
        None,
        &mut accumulated_env,
        &mut archive_executor,
        &std::collections::BTreeMap::new(),
    )?;
    Ok(overall_deadline)
}

/// Build the human-readable hierarchical step index string.
///
/// - Root-level step 0 → `"0"`, step 3 → `"3"`.
/// - Inner step 2 of an invocation at root index 3 → `"3.2"`.
fn build_step_display(parent_indices: &[usize], step_idx: usize) -> String {
    if parent_indices.is_empty() {
        step_idx.to_string()
    } else {
        let prefix = parent_indices
            .iter()
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join(".");
        format!("{prefix}.{step_idx}")
    }
}

/// Recursive inner step executor.
///
/// `parent_indices` is the chain of ancestor step indices leading to this call
/// (empty for the root level).  Each `Invoke` step appends its own index to
/// produce the hierarchical display for its children.
///
/// `accumulated_env` is the env accumulator for the *current* scope.  When
/// entering an `Invoke` step the caller clones this env to form the child
/// scope; mutations inside the invocation are discarded (not written back).
fn run_steps_inner(
    context: &RunStepContext<'_>,
    steps: &[TestStep],
    parent_indices: &[usize],
    fragment_output_decls: Option<&[FragmentOutputDecl]>,
    accumulated_env: &mut Vec<(String, String)>,
    archive_executor: &mut Option<&mut ArchiveExecutor<'_>>,
    // Step-output values resolved at the outer invocation boundary (from deferred_with).
    // These cover `${{ steps.X.outputs.Y }}` refs that were injected into inner steps
    // via input substitution from the calling scope.
    inherited_step_refs: &std::collections::BTreeMap<(String, String), EvaluatedValue>,
) -> Result<()> {
    for (step_idx, step) in steps.iter().enumerate() {
        ensure_overall_budget(context.overall_deadline, context.overall_timeout)?;

        let display = build_step_display(parent_indices, step_idx);

        match step {
            TestStep::Invoke(invoke) => {
                // Print the invocation boundary as its own step entry.
                print_step_title(&display, invoke.uses.as_str(), None);
                let invoke_started = Instant::now();

                // The invocation runs with its own env scope: it inherits the
                // caller's accumulated env but mutations are contained within.
                let mut child_env = accumulated_env.clone();

                // Build the child prefix: append this step's index.
                let mut child_prefix = parent_indices.to_vec();
                child_prefix.push(step_idx);

                // Resolve deferred_with values against prior siblings in the current scope.
                // These become the inherited step refs for the child invocation.
                let deferred_with_result =
                    resolve_invoke_deferred_with(invoke, steps, step_idx, inherited_step_refs);
                let child_inherited = match deferred_with_result {
                    Ok(map) => map,
                    Err(e) => {
                        print_step_status(
                            &display,
                            invoke.uses.as_str(),
                            None,
                            false,
                            Some(invoke_started.elapsed()),
                        );
                        return Err(e);
                    }
                };

                let invoke_result = run_steps_inner(
                    context,
                    &invoke.steps,
                    &child_prefix,
                    Some(&invoke.output_decls),
                    &mut child_env,
                    archive_executor,
                    &child_inherited,
                );
                // child_env is dropped here — env mutations do not leak back out.

                // On success, resolve the fragment's declared outputs at the boundary.
                let invoke_result = invoke_result.and_then(|()| resolve_invoke_outputs(invoke));

                print_step_status(
                    &display,
                    invoke.uses.as_str(),
                    None,
                    invoke_result.is_ok(),
                    Some(invoke_started.elapsed()),
                );
                invoke_result?;
            }

            TestStep::Run(run) => {
                // Evaluate the `if:` condition before doing any work.
                // For Deferred conditions, resolve against the current scope at runtime.
                let should_run = match &run.condition {
                    StepCondition::Always => true,
                    StepCondition::Resolved(b) => *b,
                    StepCondition::Deferred(expr) => {
                        let prior_steps = &steps[..step_idx];
                        resolve_deferred_condition(
                            expr,
                            &mut |step_id: &str, output_name: &str| {
                                let key = (step_id.to_string(), output_name.to_string());
                                if let Some(v) = inherited_step_refs.get(&key) {
                                    return Ok(v.clone());
                                }
                                resolve_step_output_reference(
                                    run,
                                    steps,
                                    prior_steps,
                                    step_id,
                                    output_name,
                                )
                            },
                            &mut |output_name: &str| {
                                resolve_fragment_output_reference(
                                    run,
                                    steps,
                                    prior_steps,
                                    fragment_output_decls,
                                    output_name,
                                )
                            },
                        )
                        .unwrap_or(false)
                    }
                };
                if !should_run {
                    print_step_skipped(&display, step.display_name(), step.display_id());
                    continue;
                }

                // The file is created by StepLogWriter::create inside each step runner;
                // no pre-creation needed here (the directory was already created above).
                print_step_title(&display, step.display_name(), step.display_id());
                let step_started = Instant::now();
                // Lazy resolution: substitute `${{ steps.<id>.outputs.<name> }}`
                // / `${{ outputs.<name> }}` references in the run body against
                // already-executed siblings in this scope (backward-only, hard
                // error otherwise).
                let step_result = resolve_run_step_output_refs(
                    run,
                    steps,
                    step_idx,
                    fragment_output_decls,
                    inherited_step_refs,
                )
                .and_then(|run_body| {
                    run_run_step(context, &display, run, &run_body, accumulated_env)
                });
                print_step_status(
                    &display,
                    step.display_name(),
                    step.display_id(),
                    step_result.is_ok(),
                    Some(step_started.elapsed()),
                );
                step_result?;
            }

            TestStep::Archive(archive_step) => {
                print_step_title(&display, step.display_name(), step.display_id());
                let step_started = Instant::now();
                let step_result = if let Some(executor) = archive_executor.as_mut() {
                    executor(&display, archive_step)
                } else {
                    let archive_name = archive_step
                        .archive
                        .name
                        .as_deref()
                        .unwrap_or(archive_step.archive.src.as_str());
                    anyhow::bail!(
                        "step {} ('{}') is an `archive` step, but archive execution is not enabled for this command",
                        display,
                        archive_name
                    )
                };
                print_step_status(
                    &display,
                    step.display_name(),
                    step.display_id(),
                    step_result.is_ok(),
                    Some(step_started.elapsed()),
                );
                step_result?;
            }
        }
    }
    Ok(())
}

fn run_run_step(
    context: &RunStepContext<'_>,
    step_display: &str,
    step: &RunStep,
    run_body: &str,
    accumulated_env: &mut Vec<(String, String)>,
) -> Result<()> {
    let step_log_path = step_log_path(context.step_log_dir, step_display, &step.name);
    let step_timeout = resolve_step_timeout(step.timeout, context.default_step_timeout);
    let step_budget = StepExecutionBudget {
        step_timeout,
        overall_deadline: context.overall_deadline,
        overall_timeout: context.overall_timeout,
    };

    match step.target {
        StepTarget::Guest => (|| -> Result<()> {
            let suffix = unique_suffix();
            // Use the display index (e.g. "3" or "3.2") in temp-file names; replace
            // '.' with '-' so the path component is always filesystem-safe.
            let safe_display = step_display.replace('.', "-");
            let local_script =
                std::env::temp_dir().join(format!("botforge-step-{safe_display}-{suffix}.sh"));
            let remote_script = format!("/tmp/botforge-step-{safe_display}-{suffix}.sh");
            let remote_env_path = format!("/tmp/botforge-env-{safe_display}-{suffix}");
            let remote_out_path = format!("/tmp/botforge-out-{safe_display}-{suffix}");

            // Write only the user's script body — no interpreter-specific preamble.
            std::fs::write(&local_script, run_body.as_bytes()).with_context(|| {
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

            let step_result: Result<()> = if let Some(expect) = &step.expect {
                // Capturing path: surface stdout/stderr and exit code for assertion checking.
                // Propagate the scp error early if upload failed.
                scp_result?;
                // Initialize the remote env and out files (world-writable for sudo compat).
                let _ = ssh_with_retry(
                    context.ssh,
                    &guest_files_init_cmd(&remote_env_path, &remote_out_path),
                    1,
                    Duration::from_secs(0),
                    Duration::from_secs(10),
                );
                let ssh_cmd = build_guest_ssh_cmd(
                    &template,
                    &remote_script,
                    step.sudo_enabled(),
                    accumulated_env,
                    &remote_env_path,
                    &remote_out_path,
                );
                let (capture, actual_exit) = run_ssh_step_capturing(
                    &step.name,
                    context.ssh,
                    &ssh_cmd,
                    Duration::from_secs(300),
                    &step_log_path,
                    step_budget,
                    TEST_TRANSPORT_RETRIES,
                    TEST_TRANSPORT_RETRY_DELAY,
                )
                .with_context(|| format!("test step '{}' command failed", step.name))?;

                // Merge env (best-effort; the command ran regardless of exit code).
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

                let expect_result = check_expect_block(&step.name, expect, &capture, actual_exit);

                // Capture declared outputs on success.
                if expect_result.is_ok() {
                    capture_declared_step_outputs(step, || {
                        ssh_capture_stdout(
                            context.ssh,
                            &format!("cat {}", shell_single_quote(&remote_out_path)),
                            1,
                            Duration::from_secs(0),
                            Duration::from_secs(10),
                        )
                    })?;
                }

                expect_result
            } else {
                // Standard path: exit 0 required, no stdout/stderr capture.
                let result = if scp_result.is_ok() {
                    // Initialize the remote env and out files (world-writable for sudo compat).
                    let _ = ssh_with_retry(
                        context.ssh,
                        &guest_files_init_cmd(&remote_env_path, &remote_out_path),
                        1,
                        Duration::from_secs(0),
                        Duration::from_secs(10),
                    );
                    let ssh_cmd = build_guest_ssh_cmd(
                        &template,
                        &remote_script,
                        step.sudo_enabled(),
                        accumulated_env,
                        &remote_env_path,
                        &remote_out_path,
                    );
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
                if result.is_ok() {
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

                    // Capture declared outputs on success.
                    capture_declared_step_outputs(step, || {
                        ssh_capture_stdout(
                            context.ssh,
                            &format!("cat {}", shell_single_quote(&remote_out_path)),
                            1,
                            Duration::from_secs(0),
                            Duration::from_secs(10),
                        )
                    })?;
                }

                result
            };

            // Best-effort cleanup: remote out file, env file, script, local temp file.
            let _ = ssh_with_retry(
                context.ssh,
                &format!(
                    "rm -f {} {} {}",
                    shell_single_quote(&remote_out_path),
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
            let out_file = std::env::temp_dir().join(format!("botforge-host-out-{suffix}"));

            let step_result: Result<()> = if let Some(expect) = &step.expect {
                // Capturing path: surface stdout/stderr and exit code for assertion checking.
                let (capture, actual_exit) = run_host_step_capturing(
                    &step.name,
                    run_body,
                    context.context,
                    step_budget,
                    &template,
                    accumulated_env,
                    HostStepFiles {
                        env_file: &env_file,
                        out_file: &out_file,
                        log_path: &step_log_path,
                    },
                )
                .with_context(|| format!("test step '{}' command failed", step.name))?;

                // Merge env (best-effort; the command ran regardless of exit code).
                if let Ok(contents) = std::fs::read_to_string(&env_file) {
                    if let Ok(new_entries) = parse_env_file(&contents) {
                        env_merge(accumulated_env, new_entries);
                    }
                }

                let expect_result = check_expect_block(&step.name, expect, &capture, actual_exit);

                // Capture declared outputs on success.
                if expect_result.is_ok() && !step.outputs.is_empty() {
                    let out_contents = std::fs::read_to_string(&out_file).unwrap_or_default();
                    let captured = capture_step_outputs(&step.name, &step.outputs, &out_contents)?;
                    *step.captured_outputs.borrow_mut() = Some(captured);
                }

                let _ = std::fs::remove_file(&env_file);
                let _ = std::fs::remove_file(&out_file);
                expect_result
            } else {
                // Standard path: exit 0 required, no output capture.
                let result = run_host_step(
                    &step.name,
                    run_body,
                    context.context,
                    step_budget,
                    &template,
                    accumulated_env,
                    HostStepFiles {
                        env_file: &env_file,
                        out_file: &out_file,
                        log_path: &step_log_path,
                    },
                )
                .with_context(|| format!("test step '{}' command failed", step.name));

                // On success, parse the local env file and merge into accumulated env.
                if result.is_ok() {
                    if let Ok(contents) = std::fs::read_to_string(&env_file) {
                        if let Ok(new_entries) = parse_env_file(&contents) {
                            env_merge(accumulated_env, new_entries);
                        }
                    }

                    // Capture declared outputs on success.
                    if !step.outputs.is_empty() {
                        let out_contents = std::fs::read_to_string(&out_file).unwrap_or_default();
                        let captured =
                            capture_step_outputs(&step.name, &step.outputs, &out_contents)?;
                        *step.captured_outputs.borrow_mut() = Some(captured);
                    }
                }

                let _ = std::fs::remove_file(&env_file);
                let _ = std::fs::remove_file(&out_file);
                result
            };

            step_result
        }
    }?;
    Ok(())
}

fn capture_declared_step_outputs<F>(step: &RunStep, read_out: F) -> Result<()>
where
    F: FnOnce() -> Result<String>,
{
    if step.outputs.is_empty() {
        return Ok(());
    }

    let out_contents = read_out().with_context(|| {
        format!(
            "step '{}': failed to read $BF_OUT to capture declared outputs",
            step.name
        )
    })?;
    let captured = capture_step_outputs(&step.name, &step.outputs, &out_contents)?;
    *step.captured_outputs.borrow_mut() = Some(captured);
    Ok(())
}

/// Resolve deferred runtime output references in a run step's `run:` body against
/// the current scope's already-executed siblings.
///
/// Runtime references are lazy/backward-only:
/// - `steps.<id>.outputs.<name>` resolves against earlier sibling run/invoke steps.
/// - `outputs.<name>` (fragment-self) resolves only inside fragment scopes and maps
///   to the fragment's declared output contract (`value:` expression, `default`,
///   `required`) at the current execution point.
///
/// `inherited_step_refs` carries step-output values that were resolved at the outer
/// invocation boundary (from `deferred_with`) and injected into this scope via input
/// substitution — they take precedence over local scope lookup.
///
/// Expression evaluation lives solely in config/expressions; do not parse `${{ }}` here.
fn resolve_run_step_output_refs(
    step: &RunStep,
    scope_steps: &[TestStep],
    step_idx: usize,
    fragment_output_decls: Option<&[FragmentOutputDecl]>,
    inherited_step_refs: &std::collections::BTreeMap<(String, String), EvaluatedValue>,
) -> Result<String> {
    let prior_steps = &scope_steps[..step_idx];
    resolve_deferred_refs_in_string(
        &step.run,
        &mut |step_id: &str, output_name: &str| {
            // Check inherited refs first (outer-scope with: substitution).
            let key = (step_id.to_string(), output_name.to_string());
            if let Some(value) = inherited_step_refs.get(&key) {
                return Ok(value.clone());
            }
            // Then check local scope (backward-only).
            resolve_step_output_reference(step, scope_steps, prior_steps, step_id, output_name)
        },
        &mut |output_name: &str| {
            resolve_fragment_output_reference(
                step,
                scope_steps,
                prior_steps,
                fragment_output_decls,
                output_name,
            )
        },
    )
}

/// Convert a resolved [`OutputValue`] to an [`EvaluatedValue`] for use in the expression
/// engine at runtime (e.g. string interpolation, typed field resolution).
fn output_value_to_evaluated(v: OutputValue) -> EvaluatedValue {
    match v {
        OutputValue::String(s) => EvaluatedValue::String(s),
        OutputValue::Number(n) => EvaluatedValue::Number(n),
        OutputValue::Bool(b) => EvaluatedValue::Bool(b),
        OutputValue::Null => EvaluatedValue::Empty,
    }
}

enum ScopedStepById<'a> {
    Run(&'a RunStep),
    Invoke(&'a InvokeStep),
}

fn find_step_by_id<'a>(steps: &'a [TestStep], step_id: &str) -> Option<ScopedStepById<'a>> {
    for step in steps {
        match step {
            TestStep::Run(run) if run.id.as_deref() == Some(step_id) => {
                return Some(ScopedStepById::Run(run));
            }
            TestStep::Invoke(invoke) if invoke.id.as_deref() == Some(step_id) => {
                return Some(ScopedStepById::Invoke(invoke));
            }
            _ => {}
        }
    }
    None
}

/// Resolve a `uses:` step's deferred `with:` values at the invocation boundary,
/// against already-executed siblings in the current scope (backward-only; a
/// forward or unknown reference is a hard error).
///
/// Runs BEFORE the child invocation executes. The resolved `${{ steps.X.outputs.Y }}`
/// values are returned as the child's inherited step refs, so inner steps that
/// received the deferred ref via load-time input substitution resolve to the real
/// captured value at runtime.
///
/// Expression evaluation lives solely in config/expressions; do not parse `${{ }}` here.
fn resolve_invoke_deferred_with(
    invoke: &InvokeStep,
    scope_steps: &[TestStep],
    step_idx: usize,
    inherited_step_refs: &std::collections::BTreeMap<(String, String), EvaluatedValue>,
) -> Result<std::collections::BTreeMap<(String, String), EvaluatedValue>> {
    let prior_steps = &scope_steps[..step_idx];
    let mut child_inherited: std::collections::BTreeMap<(String, String), EvaluatedValue> =
        std::collections::BTreeMap::new();
    invoke
        .deferred_with
        .values()
        .filter_map(|v| {
            if let Value::String(s) = v {
                Some(s.as_str())
            } else {
                None
            }
        })
        .try_for_each(|s| {
            resolve_deferred_refs_in_string(
                s,
                &mut |step_id: &str, output_name: &str| {
                    // Check inherited (outer-scope) refs first.
                    let key = (step_id.to_string(), output_name.to_string());
                    if let Some(v) = inherited_step_refs.get(&key) {
                        child_inherited.insert(key, v.clone());
                        return Ok(v.clone());
                    }
                    // Then look up in the current scope's prior steps.
                    let dummy = make_boundary_consumer(&invoke.uses);
                    resolve_step_output_reference(
                        &dummy,
                        scope_steps,
                        prior_steps,
                        step_id,
                        output_name,
                    )
                    .inspect(|v| {
                        child_inherited
                            .insert((step_id.to_string(), output_name.to_string()), v.clone());
                    })
                },
                &mut |output_name: &str| {
                    anyhow::bail!(
                        "'{}': outputs.* not available in outer-scope 'with:' (output: '{}')",
                        invoke.uses,
                        output_name
                    )
                },
            )
            .map(|_| ())
        })?;
    Ok(child_inherited)
}

/// Create a dummy `RunStep` consumer for error-message context at the invocation boundary.
/// Used when resolving `deferred_with` values outside an actual run step.
fn make_boundary_consumer(uses_name: &str) -> RunStep {
    RunStep {
        name: format!("<invoke boundary: {}>", uses_name),
        run: String::new(),
        id: None,
        condition: StepCondition::Always,
        target: Default::default(),
        timeout: None,
        shell: None,
        sudo: None,
        outputs: std::collections::BTreeMap::new(),
        expect: None,
        captured_outputs: Default::default(),
    }
}

fn resolve_step_output_reference(
    consumer: &RunStep,
    scope_steps: &[TestStep],
    prior_steps: &[TestStep],
    step_id: &str,
    output_name: &str,
) -> Result<EvaluatedValue> {
    if let Some(prior) = find_step_by_id(prior_steps, step_id) {
        return resolve_captured_output_value(consumer, step_id, output_name, prior)
            .map(output_value_to_evaluated);
    }

    if find_step_by_id(scope_steps, step_id).is_some() {
        anyhow::bail!(
            "step '{}': cannot resolve ${{{{ steps.{}.outputs.{} }}}} — step '{}' exists \
             in the current scope but has not run yet",
            consumer.name,
            step_id,
            output_name,
            step_id
        );
    }

    anyhow::bail!(
        "step '{}': cannot resolve ${{{{ steps.{}.outputs.{} }}}} — no step with id '{}' \
         exists in the current scope",
        consumer.name,
        step_id,
        output_name,
        step_id
    );
}

fn resolve_captured_output_value(
    consumer: &RunStep,
    step_id: &str,
    output_name: &str,
    step: ScopedStepById<'_>,
) -> Result<OutputValue> {
    let (kind, maybe_outputs) = match step {
        ScopedStepById::Run(run) => ("run step", run.captured_outputs.borrow().clone()),
        ScopedStepById::Invoke(invoke) => (
            "fragment invocation",
            invoke.captured_outputs.borrow().clone(),
        ),
    };
    let Some(outputs) = maybe_outputs else {
        anyhow::bail!(
            "step '{}': cannot resolve ${{{{ steps.{}.outputs.{} }}}} — {} '{}' has no captured outputs",
            consumer.name,
            step_id,
            output_name,
            kind,
            step_id
        );
    };
    let Some(output) = outputs.iter().find(|o| o.name == output_name) else {
        anyhow::bail!(
            "step '{}': cannot resolve ${{{{ steps.{}.outputs.{} }}}} — {} '{}' does not declare output '{}'",
            consumer.name,
            step_id,
            output_name,
            kind,
            step_id,
            output_name
        );
    };
    Ok(output.value.clone())
}

fn resolve_fragment_output_reference(
    consumer: &RunStep,
    scope_steps: &[TestStep],
    prior_steps: &[TestStep],
    fragment_output_decls: Option<&[FragmentOutputDecl]>,
    output_name: &str,
) -> Result<EvaluatedValue> {
    let Some(decls) = fragment_output_decls else {
        anyhow::bail!(
            "step '{}': cannot resolve ${{{{ outputs.{} }}}} — outputs.* is only available inside a fragment scope",
            consumer.name,
            output_name
        );
    };
    let Some(decl) = decls.iter().find(|decl| decl.name == output_name) else {
        anyhow::bail!(
            "step '{}': cannot resolve ${{{{ outputs.{} }}}} — fragment does not declare output '{}'",
            consumer.name,
            output_name,
            output_name
        );
    };

    // Resolve the declared `value:` expression against the fragment scope at the
    // current execution point (backward-only), then coerce to the declared type.
    let effective = resolve_fragment_output_decl_value(decl, &mut |step_id, name| {
        resolve_step_output_reference(consumer, scope_steps, prior_steps, step_id, name)
    })
    .with_context(|| {
        format!(
            "step '{}': cannot resolve ${{{{ outputs.{} }}}}",
            consumer.name, output_name
        )
    })?;
    Ok(output_value_to_evaluated(effective))
}

/// Resolve a fragment output declaration's `value:` expression and apply the
/// declared-type contract.
///
/// The `value:` expression is resolved through the expression engine via
/// `resolve_step` (a caller-supplied `steps.<id>.outputs.<name>` lookup closure);
/// nested `outputs.*` references inside a `value:` are a hard error.  The resolved
/// string is then coerced and validated against the declared `type:` — a value that
/// does not satisfy the type is a hard error naming the fragment output — and the
/// `default`/`required` rules are applied to the resolved value.
///
/// Expression evaluation lives solely in config/expressions; do not parse `${{ }}` here.
fn resolve_fragment_output_decl_value(
    decl: &FragmentOutputDecl,
    resolve_step: &mut dyn FnMut(&str, &str) -> Result<EvaluatedValue>,
) -> Result<OutputValue> {
    let resolved = resolve_deferred_refs_in_string(&decl.value, resolve_step, &mut |name| {
        anyhow::bail!(
            "fragment output '{}': 'value:' cannot reference ${{{{ outputs.{} }}}} — \
             only steps.<id>.outputs.<name> references are available",
            decl.name,
            name
        )
    })?;

    // Coerce + validate the resolved value against the declared type (validity,
    // not matching): a resolved value that does not satisfy the declared type is
    // a hard error at the boundary, naming the fragment output.
    let value =
        coerce_output_value(&decl.name, &resolved, decl.output_type).with_context(|| {
            format!(
                "fragment output '{}': resolved value does not satisfy declared type '{}'",
                decl.name, decl.output_type
            )
        })?;

    // Apply `default:` when the resolved value is Null, then enforce `required`.
    let effective = if !matches!(value, OutputValue::Null) {
        value
    } else if let Some(default) = &decl.default {
        default.clone()
    } else {
        OutputValue::Null
    };
    if decl.required && matches!(effective, OutputValue::Null) {
        anyhow::bail!(
            "fragment output '{}': value resolved to null and output is required",
            decl.name
        );
    }
    Ok(effective)
}

/// Resolve a fragment's declared outputs onto the `InvokeStep` node.
///
/// Must be called after all inner steps of the invoke have run successfully.
/// For each declared output, resolves its `value:` expression through the expression
/// engine against the fragment's executed inner steps (backward: all inner steps
/// have run at the boundary), coerces + validates the resolved value against the
/// declared `type:` (a hard error at the boundary when it does not satisfy the type),
/// applies the declared `default` when the resolved value is `Null`, enforces
/// `required`, and stores the resulting [`Vec<CapturedOutput>`] in
/// `invoke.captured_outputs`.
///
/// This makes each fragment output **observable at the `Invoke` boundary**: the caller
/// can inspect `invoke.captured_outputs` after execution and reference the values via
/// `${{ steps.<id>.outputs.<name> }}`.
fn resolve_invoke_outputs(invoke: &InvokeStep) -> Result<()> {
    if invoke.output_decls.is_empty() {
        return Ok(());
    }

    let mut captured: Vec<CapturedOutput> = Vec::with_capacity(invoke.output_decls.len());
    let consumer = make_boundary_consumer(&invoke.uses);

    for decl in &invoke.output_decls {
        // All inner steps have executed at the boundary; the whole inner step list
        // is the "prior" scope for backward resolution.
        let effective_value = resolve_fragment_output_decl_value(decl, &mut |step_id, name| {
            resolve_step_output_reference(&consumer, &invoke.steps, &invoke.steps, step_id, name)
        })
        .with_context(|| {
            format!(
                "fragment '{}': failed to resolve output '{}'",
                invoke.uses, decl.name
            )
        })?;

        captured.push(CapturedOutput {
            name: decl.name.clone(),
            declared_type: decl.output_type,
            value: effective_value,
        });
    }

    *invoke.captured_outputs.borrow_mut() = Some(captured);
    Ok(())
}

/// Captured stdout and stderr from a step execution, available for `expect:` matching.
struct StepCapture {
    stdout: String,
    stderr: String,
}

/// Like `run_ssh_step_with_step_log` but also captures stdout and stderr for post-execution
/// expectation matching.  Returns `(capture, exit_code)` instead of `Result<()>`:
/// - transport errors still propagate as `Err`
/// - `RemoteFailure(code)` returns `Ok((capture, code))` so the caller decides pass/fail
/// - `Success` returns `Ok((capture, 0))`
#[allow(clippy::too_many_arguments)]
fn run_ssh_step_capturing(
    name: &str,
    ssh: &SshOptions,
    remote_command: &str,
    connect_timeout: Duration,
    log_path: &Path,
    budget: StepExecutionBudget,
    retries: usize,
    retry_delay: Duration,
) -> Result<(StepCapture, i32)> {
    let logger = Arc::new(StepLogWriter::create(log_path)?);
    let mut attempts = 0usize;
    loop {
        signal::poll_interrupt()?;
        let mut pending_out: Vec<u8> = Vec::new();
        let mut pending_err: Vec<u8> = Vec::new();
        let mut cap_out: Vec<u8> = Vec::new();
        let mut cap_err: Vec<u8> = Vec::new();

        let logger_ref = Arc::clone(&logger);
        let mut on_output = |is_stderr: bool, data: &[u8]| {
            use std::io::Write;
            if is_stderr {
                let _ = std::io::stderr().write_all(data);
                pending_err.extend_from_slice(data);
                cap_err.extend_from_slice(data);
                flush_log_lines(&logger_ref, &mut pending_err, StepOutputStream::Stderr);
            } else {
                let _ = std::io::stdout().write_all(data);
                pending_out.extend_from_slice(data);
                cap_out.extend_from_slice(data);
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

        let capture = StepCapture {
            stdout: String::from_utf8_lossy(&cap_out).into_owned(),
            stderr: String::from_utf8_lossy(&cap_err).into_owned(),
        };

        match outcome {
            SshExecOutcome::Success => return Ok((capture, 0)),
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
                // Return the exit code to the caller for expectation checking.
                return Ok((capture, code as i32));
            }
            SshExecOutcome::TransportError(e) => {
                attempts += 1;
                if attempts >= retries {
                    anyhow::bail!("ssh command failed (transport error, retries exhausted): {e:#}");
                }
                signal::poll_interrupt()?;
                std::thread::sleep(retry_delay);
            }
        }
    }
}

/// Like `run_host_step` but also captures stdout and stderr for post-execution expectation
/// matching.  Returns `(capture, exit_code)` instead of `Result<()>`:
/// - timeout and spawn errors still propagate as `Err`
/// - any exit code (including non-zero) is returned in the `Ok` tuple
fn run_host_step_capturing(
    name: &str,
    run: &str,
    context: &Path,
    budget: StepExecutionBudget,
    template: &[String],
    accumulated_env: &[(String, String)],
    files: HostStepFiles<'_>,
) -> Result<(StepCapture, i32)> {
    // Create/truncate the env file so `>>` always works inside the step, and make
    // it world-writable so that both root and the non-root ephemeral identity can
    // append via $BF_ENV regardless of which one the step runs as.
    std::fs::write(files.env_file, b"")
        .with_context(|| format!("failed to create env file for host step '{name}'"))?;
    // Create/truncate the output file so `>>` always works via $BF_OUT.
    std::fs::write(files.out_file, b"")
        .with_context(|| format!("failed to create out file for host step '{name}'"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(files.env_file)
            .with_context(|| format!("failed to stat env file for host step '{name}'"))?
            .permissions();
        perms.set_mode(0o666);
        std::fs::set_permissions(files.env_file, perms)
            .with_context(|| format!("failed to chmod env file for host step '{name}'"))?;
        let mut out_perms = std::fs::metadata(files.out_file)
            .with_context(|| format!("failed to stat out file for host step '{name}'"))?
            .permissions();
        out_perms.set_mode(0o666);
        std::fs::set_permissions(files.out_file, out_perms)
            .with_context(|| format!("failed to chmod out file for host step '{name}'"))?;
    }

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
        .current_dir(context)
        .env("BF_ENV", files.env_file)
        .env("BF_OUT", files.out_file)
        .envs(
            accumulated_env
                .iter()
                .map(|(k, v)| (k.as_str(), v.as_str())),
        )
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = command
        .spawn()
        .with_context(|| format!("failed to spawn host step '{name}'"))?;
    let child_stdout = child
        .stdout
        .take()
        .context("failed to capture child stdout for step logging")?;
    let child_stderr = child
        .stderr
        .take()
        .context("failed to capture child stderr for step logging")?;

    let (stdout_handle, stdout_cap) =
        spawn_capturing_forwarder(child_stdout, StepOutputStream::Stdout, Arc::clone(&logger));
    let (stderr_handle, stderr_cap) =
        spawn_capturing_forwarder(child_stderr, StepOutputStream::Stderr, logger);

    let step_deadline = Instant::now() + budget.step_timeout;
    let exit_result: Result<i32> = loop {
        signal::poll_interrupt()?;
        match child
            .try_wait()
            .with_context(|| format!("failed to wait for host step '{name}'"))?
        {
            Some(status) => {
                break Ok(status.code().unwrap_or(-1));
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
                signal::poll_interrupt()?;
                std::thread::sleep(Duration::from_millis(200));
            }
        }
    };

    // Join forwarders to ensure all output is captured before reading the buffers.
    // Forwarder errors are suppressed: exit_result is the source of truth.
    let _ = stdout_handle.join();
    let _ = stderr_handle.join();

    // Best-effort cleanup of temp script.
    let _ = std::fs::remove_file(&script);

    let exit_code = exit_result?;
    let stdout = String::from_utf8_lossy(
        &stdout_cap
            .lock()
            .map_err(|_| anyhow::anyhow!("stdout capture mutex poisoned"))?,
    )
    .into_owned();
    let stderr = String::from_utf8_lossy(
        &stderr_cap
            .lock()
            .map_err(|_| anyhow::anyhow!("stderr capture mutex poisoned"))?,
    )
    .into_owned();
    Ok((StepCapture { stdout, stderr }, exit_code))
}

/// Evaluate an `expect:` block against the captured step output and actual exit code.
///
/// Prints a per-failure diagnostic line to stderr before returning `Err`, so the
/// caller's `print_step_status` ✗ line is preceded by the specific failure reason.
fn check_expect_block(
    step_name: &str,
    expect: &ExpectBlock,
    capture: &StepCapture,
    actual_exit: i32,
) -> Result<()> {
    let mut failures: Vec<String> = Vec::new();

    let expected_exit = expect.expected_exit();
    if actual_exit != expected_exit {
        failures.push(format!("exit: expected {expected_exit}, got {actual_exit}"));
    }

    if let Some(stdout_exp) = &expect.stdout {
        check_stdio_expect(&capture.stdout, "stdout", stdout_exp, &mut failures);
    }
    if let Some(stderr_exp) = &expect.stderr {
        check_stdio_expect(&capture.stderr, "stderr", stderr_exp, &mut failures);
    }

    if failures.is_empty() {
        return Ok(());
    }

    for failure in &failures {
        eprintln!("  {failure}");
    }
    anyhow::bail!(
        "step '{}' failed outcome assertions: {}",
        step_name,
        failures.join("; ")
    )
}

fn check_stdio_expect(
    output: &str,
    stream: &str,
    expect: &StdioExpect,
    failures: &mut Vec<String>,
) {
    for needle in &expect.contains {
        if !output.contains(needle.as_str()) {
            failures.push(format!(
                "{stream}: expected to contain {:?}, not found",
                needle
            ));
        }
    }
    for needle in &expect.not_contains {
        if output.contains(needle.as_str()) {
            failures.push(format!(
                "{stream}: expected NOT to contain {:?}, but it was present",
                needle
            ));
        }
    }
}

fn resolve_step_timeout(step_timeout: Option<u64>, default_step_timeout: Duration) -> Duration {
    Duration::from_secs(step_timeout.unwrap_or(default_step_timeout.as_secs()))
}

fn overall_timeout_error(overall_timeout: Duration) -> anyhow::Error {
    anyhow::anyhow!("overall run timed out after {}s", overall_timeout.as_secs())
}

fn ensure_overall_budget(overall_deadline: Instant, overall_timeout: Duration) -> Result<()> {
    signal::poll_interrupt()?;
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
        signal::poll_interrupt()?;
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
                signal::poll_interrupt()?;
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
    out_file: &'a Path,
    log_path: &'a Path,
}

/// Run a step locally in the botforge container (harness) with a plain execution timeout.
/// `run` is written to a temp file and executed via `template` (argv with `{0}` slot).
/// The working directory is `context`. Inherits the current process environment, with
/// `accumulated_env` injected (overriding inherited values) and `BF_ENV` pointing at
/// `env_file` so the step can write new key-value pairs for later steps to consume.
/// `out_file` captures emitted `NAME=value` pairs for typed output coercion.
fn run_host_step(
    name: &str,
    run: &str,
    context: &Path,
    budget: StepExecutionBudget,
    template: &[String],
    accumulated_env: &[(String, String)],
    files: HostStepFiles<'_>,
) -> Result<()> {
    // Create/truncate the env file so `>>` always works inside the step, and make
    // it world-writable so that both root and the non-root ephemeral identity can
    // append via $BF_ENV regardless of which one the step runs as.
    std::fs::write(files.env_file, b"")
        .with_context(|| format!("failed to create env file for host step '{name}'"))?;
    // Create/truncate the output file so `>>` always works via $BF_OUT.
    std::fs::write(files.out_file, b"")
        .with_context(|| format!("failed to create out file for host step '{name}'"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(files.env_file)
            .with_context(|| format!("failed to stat env file for host step '{name}'"))?
            .permissions();
        perms.set_mode(0o666);
        std::fs::set_permissions(files.env_file, perms)
            .with_context(|| format!("failed to chmod env file for host step '{name}'"))?;
        let mut out_perms = std::fs::metadata(files.out_file)
            .with_context(|| format!("failed to stat out file for host step '{name}'"))?
            .permissions();
        out_perms.set_mode(0o666);
        std::fs::set_permissions(files.out_file, out_perms)
            .with_context(|| format!("failed to chmod out file for host step '{name}'"))?;
    }

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
        .current_dir(context)
        .env("BF_ENV", files.env_file)
        .env("BF_OUT", files.out_file)
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
        signal::poll_interrupt()?;
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
                signal::poll_interrupt()?;
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

/// Initialise the remote BF_ENV and BF_OUT files in a single SSH command.
///
/// Both files are created/truncated and made world-writable so that both the
/// SSH user and root (sudo) can append to them.
fn guest_files_init_cmd(remote_env_path: &str, remote_out_path: &str) -> String {
    let env_path = shell_single_quote(remote_env_path);
    let out_path = shell_single_quote(remote_out_path);
    let payload = format!(
        "umask 000; : > {env_path}; chmod 0666 {env_path}; : > {out_path}; chmod 0666 {out_path}"
    );
    format!("sudo sh -c {}", shell_single_quote(&payload))
}

fn build_guest_ssh_cmd(
    template: &[String],
    remote_script: &str,
    sudo: bool,
    accumulated_env: &[(String, String)],
    remote_env_path: &str,
    remote_out_path: &str,
) -> String {
    let interpreter_cmd = template
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

    // Inject accumulated env vars, BF_ENV, and BF_OUT via a POSIX `env` prefix so
    // that the interpreter receives them as genuine environment variables.
    let mut env_parts: Vec<String> = accumulated_env
        .iter()
        .map(|(k, v)| format!("{}={}", k, shell_single_quote(v)))
        .collect();
    env_parts.push(format!("BF_ENV={}", shell_single_quote(remote_env_path)));
    env_parts.push(format!("BF_OUT={}", shell_single_quote(remote_out_path)));
    let env_prefix = format!("env {}", env_parts.join(" "));

    if sudo {
        format!("sudo -E {env_prefix} {interpreter_cmd}")
    } else {
        format!("{env_prefix} {interpreter_cmd}")
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

/// Execute an ordered, sequential, fail-fast prepare-phase for `botforge publish`.
///
/// Steps run **locally** in the container with `cwd` at `context` (the repo/context
/// root) — the same execution context as `on: host` steps in build/test plans.
/// Only `RunStep` steps are supported; `ArchiveStep` produces a hard error.
///
/// - Shell selection, `shell:` / default bash template, reused as-is.
/// - `for:` expansion is performed at load time by `expand_raw_step`; by the time
///   steps reach this function they are already fully expanded.
/// - `if:` condition checking: a step with `if: false` is skipped (logged).
/// - `expect:` assertion handling: supported; exit/stdout/stderr assertions apply.
/// - `BF_ENV` env accumulation: each step may append `KEY=VALUE` pairs; later
///   steps see earlier steps' exported vars (same mechanics as host steps in build/test).
/// - Fail-fast: a non-zero exit (or failed assertion) aborts the whole phase immediately.
pub(crate) fn run_local_steps(context: &Path, steps: &[TestStep]) -> Result<()> {
    if steps.is_empty() {
        return Ok(());
    }

    let step_log_dir = context.join("build").join("logs");
    std::fs::create_dir_all(&step_log_dir).with_context(|| {
        format!(
            "cannot create publish step log dir: {}",
            step_log_dir.display()
        )
    })?;

    // No overall wall-clock timeout for publish steps; each step has a generous
    // per-step budget.  Using a far-future deadline avoids code paths that check
    // `overall_deadline` from firing spuriously.  24 hours is well beyond any
    // reasonable publish step and avoids Duration arithmetic overflow.
    let overall_timeout = Duration::from_secs(86_400);
    let overall_deadline = Instant::now() + overall_timeout;
    let default_step_timeout = Duration::from_secs(300);

    let mut accumulated_env: Vec<(String, String)> = Vec::new();

    for (step_idx, step) in steps.iter().enumerate() {
        let display = step_idx.to_string();
        match step {
            TestStep::Run(run) => {
                if !run.condition_enabled() {
                    print_step_skipped(&display, step.display_name(), step.display_id());
                    continue;
                }

                print_step_title(&display, step.display_name(), step.display_id());
                let step_started = Instant::now();

                let log_path = step_log_path(&step_log_dir, &display, &run.name);
                let step_timeout = resolve_step_timeout(run.timeout, default_step_timeout);
                let budget = StepExecutionBudget {
                    step_timeout,
                    overall_deadline,
                    overall_timeout,
                };

                let template = resolve_shell(run.shell.as_deref())
                    .expect("shell already validated at config load");
                let suffix = unique_suffix();
                let env_file = std::env::temp_dir().join(format!("botforge-publish-env-{suffix}"));
                let out_file = std::env::temp_dir().join(format!("botforge-publish-out-{suffix}"));

                let step_result: Result<()> = resolve_run_step_output_refs(
                    run,
                    steps,
                    step_idx,
                    None,
                    &std::collections::BTreeMap::new(),
                )
                .and_then(|run_body| {
                    if let Some(expect) = &run.expect {
                        let (capture, actual_exit) = run_host_step_capturing(
                            &run.name,
                            &run_body,
                            context,
                            budget,
                            &template,
                            &accumulated_env,
                            HostStepFiles {
                                env_file: &env_file,
                                out_file: &out_file,
                                log_path: &log_path,
                            },
                        )
                        .with_context(|| format!("publish step '{}' command failed", run.name))?;

                        if let Ok(contents) = std::fs::read_to_string(&env_file) {
                            if let Ok(new_entries) = parse_env_file(&contents) {
                                env_merge(&mut accumulated_env, new_entries);
                            }
                        }
                        let _ = std::fs::remove_file(&env_file);
                        let _ = std::fs::remove_file(&out_file);
                        check_expect_block(&run.name, expect, &capture, actual_exit)
                    } else {
                        let result = run_host_step(
                            &run.name,
                            &run_body,
                            context,
                            budget,
                            &template,
                            &accumulated_env,
                            HostStepFiles {
                                env_file: &env_file,
                                out_file: &out_file,
                                log_path: &log_path,
                            },
                        )
                        .with_context(|| format!("publish step '{}' command failed", run.name));

                        if result.is_ok() {
                            if let Ok(contents) = std::fs::read_to_string(&env_file) {
                                if let Ok(new_entries) = parse_env_file(&contents) {
                                    env_merge(&mut accumulated_env, new_entries);
                                }
                            }
                        }
                        let _ = std::fs::remove_file(&env_file);
                        let _ = std::fs::remove_file(&out_file);
                        result
                    }
                });

                print_step_status(
                    &display,
                    step.display_name(),
                    step.display_id(),
                    step_result.is_ok(),
                    Some(step_started.elapsed()),
                );
                step_result?;
            }
            TestStep::Archive(_) | TestStep::Invoke(_) => {
                let name = step.display_name();
                anyhow::bail!(
                    "publish step {} ('{}'): only run steps are supported in the \
                     publish prepare phase",
                    display,
                    name
                );
            }
        }
    }

    Ok(())
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
    crate::plan::print_phase("vm", "Stopping vm");
    let mut stopped_cleanly = true;
    if let Some(child) = vm_child.as_mut() {
        // TODO(#509): Prefer graceful test VM shutdown before forced termination.
        if child.kill().is_err() {
            stopped_cleanly = false;
        }
        if child.wait().is_err() {
            stopped_cleanly = false;
        }
        signal::kill_child(child);
    }
    *vm_child = None;
    let _ = std::fs::remove_file(overlay_image);
    crate::plan::print_phase_status("vm", "Stopping vm", stopped_cleanly, None);
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
    crate::plan::print_phase("vm", "Stopping vm");
    if signal::is_interrupted() {
        if let Some(child) = vm_child.as_mut() {
            signal::kill_child(child);
        }
        *vm_child = None;
        preserve_failed_build_disk(partial, failed_partial)?;
        crate::plan::print_phase_status("vm", "Stopping vm", false, None);
        anyhow::bail!(
            "build interrupted; tainted partial disk left at {} for post-mortem",
            failed_partial.display()
        );
    }

    if Instant::now() >= overall_deadline {
        if let Some(child) = vm_child.as_mut() {
            signal::kill_child(child);
        }
        *vm_child = None;
        preserve_failed_build_disk(partial, failed_partial)?;
        crate::plan::print_phase_status("vm", "Stopping vm", false, None);
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
    let mut interrupted = false;
    let clean_exit = if let Some(child) = vm_child.as_mut() {
        let deadline = Instant::now() + BUILD_POWEROFF_TIMEOUT;
        loop {
            if signal::is_interrupted() {
                signal::kill_child(child);
                interrupted = true;
                break false;
            }
            match child.try_wait() {
                Ok(Some(status)) => {
                    break status.success();
                }
                Ok(None) => {
                    if Instant::now() >= overall_deadline {
                        signal::kill_child(child);
                        timed_out_overall = true;
                        break false;
                    }
                    if Instant::now() >= deadline {
                        signal::kill_child(child);
                        break false; // timeout
                    }
                    std::thread::sleep(Duration::from_millis(500));
                }
                Err(e) => {
                    signal::kill_child(child);
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
        crate::plan::print_phase_status("vm", "Stopping vm", false, None);
        Err(overall_timeout_error(overall_timeout))
    } else if interrupted {
        preserve_failed_build_disk(partial, failed_partial)?;
        crate::plan::print_phase_status("vm", "Stopping vm", false, None);
        anyhow::bail!(
            "build interrupted; tainted partial disk left at {} for post-mortem",
            failed_partial.display()
        )
    } else if clean_exit {
        crate::plan::print_phase_status("vm", "Stopping vm", true, None);
        Ok(())
    } else {
        preserve_failed_build_disk(partial, failed_partial)?;
        crate::plan::print_phase_status("vm", "Stopping vm", false, None);
        anyhow::bail!(
            "build VM did not shut down cleanly; \
             partial disk left at {} for post-mortem",
            failed_partial.display()
        )
    }
}

/// Test-only re-export of [`resolve_invoke_outputs`] so that tests in sibling modules
/// (e.g. `config/tests`) can drive the boundary resolution logic directly without
/// needing a live VM.
#[cfg(test)]
pub(crate) fn resolve_invoke_outputs_for_test(invoke: &InvokeStep) -> anyhow::Result<()> {
    resolve_invoke_outputs(invoke)
}

#[cfg(test)]
mod tests {
    use super::{
        build_guest_ssh_cmd, capture_declared_step_outputs, env_merge, guest_files_init_cmd,
        parse_env_file, resolve_invoke_deferred_with, resolve_run_step_output_refs,
        resolve_step_timeout, run_host_step, shell_single_quote, HostStepFiles,
        StepExecutionBudget,
    };
    use crate::config::EvaluatedValue;
    use crate::step::{
        resolve_shell, CapturedOutput, FragmentOutputDecl, InvokeStep, OutputDecl, OutputType,
        OutputValue, RunStep, StepCondition, StepTarget, TestStep,
    };
    use crate::util::unique_suffix;
    use serde::Deserialize;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    fn tmp_env_file() -> std::path::PathBuf {
        std::env::temp_dir().join(format!("botforge-test-env-{}.env", unique_suffix()))
    }

    fn tmp_out_file() -> std::path::PathBuf {
        std::env::temp_dir().join(format!("botforge-test-out-{}.out", unique_suffix()))
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
        let out_file = tmp_out_file();
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
                out_file: &out_file,
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
        let out_file = tmp_out_file();
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
                out_file: &out_file,
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
        let out_file = tmp_out_file();
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
                out_file: &out_file,
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
        let out_file = tmp_out_file();
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
                out_file: &out_file,
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
        let out_file = tmp_out_file();
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
                out_file: &out_file,
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
        let out_file = tmp_out_file();
        let log_file = tmp_step_log(dir.path());
        let result = run_host_step(
            "write-env",
            r#"echo "WRITTEN=yes" >> "$BF_ENV""#,
            dir.path(),
            test_budget(),
            &tmpl,
            &[],
            HostStepFiles {
                env_file: &env_file,
                out_file: &out_file,
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
    #[cfg(unix)]
    fn test_host_step_env_file_is_world_writable() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let tmpl = resolve_shell(None).unwrap();
        let env_file = tmp_env_file();
        let out_file = tmp_out_file();
        let log_file = tmp_step_log(dir.path());
        let result = run_host_step(
            "mode-check",
            "true",
            dir.path(),
            test_budget(),
            &tmpl,
            &[],
            HostStepFiles {
                env_file: &env_file,
                out_file: &out_file,
                log_path: &log_file,
            },
        );
        let mode = std::fs::metadata(&env_file)
            .map(|m| m.permissions().mode())
            .unwrap_or(0);
        let _ = std::fs::remove_file(&env_file);
        assert!(result.is_ok(), "step should succeed: {result:?}");
        assert_eq!(
            mode & 0o777,
            0o666,
            "env file should be world-writable (0666), got mode {mode:#o}"
        );
    }

    #[test]
    #[cfg(unix)]
    fn test_host_step_capturing_env_file_is_world_writable() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let tmpl = resolve_shell(Some("sh")).unwrap();
        let env_file = tmp_env_file();
        let out_file = tmp_out_file();
        let log_file = tmp_step_log(dir.path());
        let result = super::run_host_step_capturing(
            "mode-check-capturing",
            "true",
            dir.path(),
            test_budget(),
            &tmpl,
            &[],
            HostStepFiles {
                env_file: &env_file,
                out_file: &out_file,
                log_path: &log_file,
            },
        );
        let mode = std::fs::metadata(&env_file)
            .map(|m| m.permissions().mode())
            .unwrap_or(0);
        let _ = std::fs::remove_file(&env_file);
        assert!(result.is_ok(), "step should succeed");
        assert_eq!(
            mode & 0o777,
            0o666,
            "capturing env file should be world-writable (0666), got mode {mode:#o}"
        );
    }

    #[test]
    fn test_host_step_writes_jsonl_log() {
        let dir = tempfile::tempdir().unwrap();
        let tmpl = resolve_shell(Some("sh")).unwrap();
        let env_file = tmp_env_file();
        let out_file = tmp_out_file();
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
                out_file: &out_file,
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

    // --- build_guest_ssh_cmd env injection ---

    #[test]
    fn test_guest_files_init_cmd_uses_sudo_umask_and_quoted_paths() {
        let env_path = "/tmp/botforge env 1";
        let out_path = "/tmp/botforge-out-1";
        let cmd = guest_files_init_cmd(env_path, out_path);
        assert!(
            cmd.starts_with("sudo sh -c "),
            "expected sudo sh -c prefix: {cmd}"
        );
        assert!(cmd.contains("umask 000"), "expected umask 000: {cmd}");
        assert!(cmd.contains("chmod 0666"), "expected chmod 0666: {cmd}");
        let quoted_env = shell_single_quote(env_path);
        let quoted_out = shell_single_quote(out_path);
        assert!(
            cmd.contains(&quoted_env),
            "expected env path {quoted_env} in command: {cmd}"
        );
        assert!(
            cmd.contains(&quoted_out),
            "expected out path {quoted_out} in command: {cmd}"
        );
    }

    #[test]
    fn test_build_guest_ssh_cmd_prefixes_sudo_for_guest_root_step() {
        let tmpl = resolve_shell(None).unwrap();
        let cmd = build_guest_ssh_cmd(
            &tmpl,
            "/tmp/botforge-step.sh",
            true,
            &[],
            "/tmp/botforge-env-1",
            "/tmp/botforge-out-1",
        );
        assert_eq!(
            cmd,
            "sudo -E env BF_ENV='/tmp/botforge-env-1' BF_OUT='/tmp/botforge-out-1' bash --noprofile --norc -e -o pipefail '/tmp/botforge-step.sh'"
        );
    }

    #[test]
    fn test_build_guest_ssh_cmd_without_sudo_injects_env_prefix() {
        let tmpl = resolve_shell(Some("sh")).unwrap();
        let cmd = build_guest_ssh_cmd(
            &tmpl,
            "/tmp/botforge-step.sh",
            false,
            &[],
            "/tmp/botforge-env-1",
            "/tmp/botforge-out-1",
        );
        assert_eq!(
            cmd,
            "env BF_ENV='/tmp/botforge-env-1' BF_OUT='/tmp/botforge-out-1' sh -e '/tmp/botforge-step.sh'"
        );
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
        let cmd = build_guest_ssh_cmd(
            &tmpl,
            "/tmp/botforge-step.sh",
            step.sudo_enabled(),
            &[],
            "/tmp/botforge-env-1",
            "/tmp/botforge-out-1",
        );
        assert_eq!(
            cmd,
            "sudo -E env BF_ENV='/tmp/botforge-env-1' BF_OUT='/tmp/botforge-out-1' bash --noprofile --norc -e -o pipefail '/tmp/botforge-step.sh'"
        );
    }

    #[test]
    fn test_build_guest_ssh_cmd_injects_accumulated_env_via_posix_env() {
        let tmpl = resolve_shell(None).unwrap();
        let acc = vec![
            ("FOO".to_string(), "bar".to_string()),
            ("MSG".to_string(), "hello world".to_string()),
        ];
        let cmd = build_guest_ssh_cmd(
            &tmpl,
            "/tmp/botforge-step.sh",
            true,
            &acc,
            "/tmp/env",
            "/tmp/out",
        );
        assert!(
            cmd.starts_with("sudo -E env "),
            "expected sudo -E env prefix: {cmd}"
        );
        assert!(cmd.contains("FOO='bar'"), "expected FOO: {cmd}");
        assert!(cmd.contains("MSG='hello world'"), "expected MSG: {cmd}");
        assert!(cmd.contains("BF_ENV='/tmp/env'"), "expected BF_ENV: {cmd}");
        assert!(cmd.contains("BF_OUT='/tmp/out'"), "expected BF_OUT: {cmd}");
        // No bash-syntax export statements in the command
        assert!(
            !cmd.contains("export "),
            "must not contain bash export: {cmd}"
        );
    }

    #[test]
    fn test_build_guest_ssh_cmd_quotes_special_chars_in_env() {
        let tmpl = resolve_shell(Some("sh")).unwrap();
        let acc = vec![("VAL".to_string(), "it's a value".to_string())];
        let cmd = build_guest_ssh_cmd(&tmpl, "/tmp/s.sh", false, &acc, "/tmp/env", "/tmp/out");
        let expected_val = format!("VAL={}", shell_single_quote("it's a value"));
        assert!(cmd.contains(&expected_val), "cmd: {cmd}");
    }

    #[test]
    fn test_build_guest_ssh_cmd_empty_env_includes_only_botforge_env() {
        let tmpl = resolve_shell(None).unwrap();
        let cmd = build_guest_ssh_cmd(
            &tmpl,
            "/tmp/botforge-step.sh",
            true,
            &[],
            "/tmp/botforge-env-1",
            "/tmp/botforge-out-1",
        );
        assert!(cmd.contains("BF_ENV='/tmp/botforge-env-1'"), "cmd: {cmd}");
        assert!(cmd.contains("BF_OUT='/tmp/botforge-out-1'"), "cmd: {cmd}");
        // No extra env var assignments beyond BF_ENV/BF_OUT
        assert!(!cmd.contains("FOO="), "unexpected extra env var: {cmd}");
    }

    #[test]
    fn test_build_guest_ssh_cmd_python_interpreter_no_bash_syntax() {
        let tmpl = resolve_shell(Some("python")).unwrap();
        let acc = vec![("MYVAR".to_string(), "myval".to_string())];
        let cmd = build_guest_ssh_cmd(&tmpl, "/tmp/script.py", false, &acc, "/tmp/env", "/tmp/out");
        // No bash-specific syntax in the SSH command
        assert!(!cmd.contains("export "), "must not have bash export: {cmd}");
        assert!(
            !cmd.contains(": > "),
            "must not have bash redirect init: {cmd}"
        );
        // POSIX env prefix carries the variable
        assert!(cmd.contains("env "), "must have env prefix: {cmd}");
        assert!(cmd.contains("MYVAR='myval'"), "must pass MYVAR: {cmd}");
        assert!(
            cmd.contains("python3 '/tmp/script.py'"),
            "must invoke python3: {cmd}"
        );
    }

    // --- host step timeout ---

    #[test]
    fn test_host_step_timeout_kills_and_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let tmpl = resolve_shell(Some("sh")).unwrap();
        let env_file = tmp_env_file();
        let out_file = tmp_out_file();
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
                out_file: &out_file,
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
        let out_file = tmp_out_file();
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
                out_file: &out_file,
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
            expect: None,
            condition: StepCondition::Always,
            outputs: std::collections::BTreeMap::new(),
            captured_outputs: Default::default(),
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
            expect: None,
            condition: StepCondition::Always,
            outputs: std::collections::BTreeMap::new(),
            captured_outputs: Default::default(),
        };
        assert_eq!(
            resolve_step_timeout(step.timeout, Duration::from_secs(1800)),
            Duration::from_secs(1800)
        );
    }

    fn output_decl(output_type: OutputType, required: bool) -> OutputDecl {
        OutputDecl {
            output_type,
            required,
        }
    }

    fn host_run_step_with_outputs(outputs: Vec<(&str, OutputDecl)>) -> RunStep {
        RunStep {
            target: StepTarget::Host,
            name: "capture-step".to_string(),
            run: "echo ok".to_string(),
            timeout: None,
            shell: None,
            sudo: None,
            id: None,
            expect: None,
            condition: StepCondition::Always,
            outputs: outputs
                .into_iter()
                .map(|(name, decl)| (name.to_string(), decl))
                .collect(),
            captured_outputs: Default::default(),
        }
    }

    #[test]
    fn test_capture_declared_step_outputs_errors_when_read_fails() {
        let step =
            host_run_step_with_outputs(vec![("must_emit", output_decl(OutputType::String, true))]);

        let err = capture_declared_step_outputs(&step, || anyhow::bail!("ssh transport hiccup"))
            .unwrap_err();
        let msg = err.to_string();

        assert!(
            msg.contains("failed to read $BF_OUT") && msg.contains("capture-step"),
            "expected BF_OUT read context in error: {msg}"
        );
        assert!(step.captured_outputs.borrow().is_none());
    }

    #[test]
    fn test_capture_declared_step_outputs_skips_read_when_no_outputs_declared() {
        let step = host_run_step_with_outputs(vec![]);
        let mut read_called = false;

        capture_declared_step_outputs(&step, || {
            read_called = true;
            anyhow::bail!("should not read")
        })
        .unwrap();

        assert!(
            !read_called,
            "steps without outputs should not read $BF_OUT"
        );
        assert!(step.captured_outputs.borrow().is_none());
    }

    #[test]
    fn test_capture_declared_step_outputs_stores_captured_values() {
        let step =
            host_run_step_with_outputs(vec![("label", output_decl(OutputType::String, true))]);

        capture_declared_step_outputs(&step, || Ok("label=hello\n".to_string())).unwrap();

        let captured = step.captured_outputs.borrow();
        let captured = captured.as_ref().expect("captured outputs");
        assert_eq!(captured.len(), 1);
        assert_eq!(captured[0].value, OutputValue::String("hello".to_string()));
    }

    // --- resolve_run_step_output_refs (lazy steps.* / outputs.*) ---

    fn consumer_step(run: &str) -> RunStep {
        RunStep {
            target: StepTarget::Host,
            name: "consumer".to_string(),
            run: run.to_string(),
            timeout: None,
            shell: None,
            sudo: None,
            id: None,
            expect: None,
            condition: StepCondition::Always,
            outputs: std::collections::BTreeMap::new(),
            captured_outputs: Default::default(),
        }
    }

    fn executed_invoke(id: &str, outputs: Vec<CapturedOutput>) -> TestStep {
        TestStep::Invoke(InvokeStep {
            uses: "frag.yaml".to_string(),
            id: Some(id.to_string()),
            steps: vec![],
            output_decls: vec![],
            deferred_with: std::collections::BTreeMap::new(),
            captured_outputs: std::cell::RefCell::new(Some(outputs)),
        })
    }

    fn executed_run_with_outputs(id: &str, outputs: Vec<CapturedOutput>) -> TestStep {
        TestStep::Run(RunStep {
            target: StepTarget::Host,
            name: "producer".to_string(),
            run: "true".to_string(),
            timeout: None,
            shell: None,
            sudo: None,
            id: Some(id.to_string()),
            expect: None,
            condition: StepCondition::Always,
            outputs: std::collections::BTreeMap::new(),
            captured_outputs: std::cell::RefCell::new(Some(outputs)),
        })
    }

    fn captured(name: &str, declared_type: OutputType, value: OutputValue) -> CapturedOutput {
        CapturedOutput {
            name: name.to_string(),
            declared_type,
            value,
        }
    }

    #[test]
    fn test_resolve_run_refs_backward_invoke_ref_resolves() {
        let scope = vec![
            executed_invoke(
                "build",
                vec![
                    captured(
                        "version",
                        OutputType::String,
                        OutputValue::String("1.2.3".to_string()),
                    ),
                    captured("count", OutputType::Number, OutputValue::Number(7.0)),
                    captured("ready", OutputType::Bool, OutputValue::Bool(true)),
                ],
            ),
            TestStep::Run(consumer_step("echo noop")),
        ];
        let step = consumer_step(
            "echo ${{ steps.build.outputs.version }}/${{ steps.build.outputs.count }}/${{ steps.build.outputs.ready }}",
        );

        let resolved = resolve_run_step_output_refs(
            &step,
            &scope,
            1,
            None,
            &std::collections::BTreeMap::new(),
        )
        .unwrap();
        assert_eq!(resolved, "echo 1.2.3/7/true");
    }

    #[test]
    fn test_resolve_run_refs_null_projects_as_empty_string() {
        let scope = vec![
            executed_invoke(
                "build",
                vec![captured("maybe", OutputType::String, OutputValue::Null)],
            ),
            TestStep::Run(consumer_step("echo noop")),
        ];
        let step = consumer_step("echo [${{ steps.build.outputs.maybe }}]");

        let resolved = resolve_run_step_output_refs(
            &step,
            &scope,
            1,
            None,
            &std::collections::BTreeMap::new(),
        )
        .unwrap();
        assert_eq!(resolved, "echo []");
    }

    #[test]
    fn test_resolve_run_refs_no_refs_is_identity() {
        let step = consumer_step("echo plain");
        let scope = vec![TestStep::Run(consumer_step("echo noop"))];
        let resolved = resolve_run_step_output_refs(
            &step,
            &scope,
            0,
            None,
            &std::collections::BTreeMap::new(),
        )
        .unwrap();
        assert_eq!(resolved, "echo plain");
    }

    #[test]
    fn test_resolve_run_refs_unknown_id_is_error() {
        let step = consumer_step("echo ${{ steps.nope.outputs.version }}");
        let scope = vec![TestStep::Run(consumer_step(
            "echo ${{ steps.nope.outputs.version }}",
        ))];
        let err = resolve_run_step_output_refs(
            &step,
            &scope,
            0,
            None,
            &std::collections::BTreeMap::new(),
        )
        .unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("no step with id 'nope' exists in the current scope"),
            "unknown id must be a hard error: {msg}"
        );
    }

    #[test]
    fn test_resolve_run_refs_forward_reference_is_error() {
        let step = consumer_step("echo ${{ steps.later.outputs.version }}");
        let scope = vec![
            TestStep::Run(consumer_step("echo ${{ steps.later.outputs.version }}")),
            executed_invoke(
                "later",
                vec![captured(
                    "version",
                    OutputType::String,
                    OutputValue::String("1.2.3".to_string()),
                )],
            ),
        ];
        let err = resolve_run_step_output_refs(
            &step,
            &scope,
            0,
            None,
            &std::collections::BTreeMap::new(),
        )
        .unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("has not run yet"),
            "forward reference must be a hard error: {msg}"
        );
    }

    // --- Invoke-boundary `deferred_with` resolution ----------------------------
    // Mirrors the run-body resolution tests above: an executed sibling produces
    // captured outputs, and a later `uses:` step's deferred `with:` values resolve
    // against it at the invocation boundary (backward-only, hard error otherwise).

    fn invoke_with_deferred_with(with: &[(&str, &str)]) -> InvokeStep {
        InvokeStep {
            uses: "@://user-frag.yaml".to_string(),
            id: None,
            steps: vec![],
            output_decls: vec![],
            deferred_with: with
                .iter()
                .map(|(k, v)| (k.to_string(), serde_yaml::Value::String(v.to_string())))
                .collect(),
            captured_outputs: std::cell::RefCell::new(None),
        }
    }

    #[test]
    fn test_resolve_invoke_deferred_with_backward_sibling_resolves() {
        // The headline OTP scenario: an executed `uses:` sibling (id: admin)
        // produced a captured output; a later invoke consumes it via
        // `otp: ${{ steps.admin.outputs.testuser_otp }}` in its with: block.
        let invoke =
            invoke_with_deferred_with(&[("otp", "${{ steps.admin.outputs.testuser_otp }}")]);
        let scope = vec![
            executed_invoke(
                "admin",
                vec![captured(
                    "testuser_otp",
                    OutputType::String,
                    OutputValue::String("otp-12345".to_string()),
                )],
            ),
            TestStep::Invoke(invoke),
        ];
        let TestStep::Invoke(invoke) = &scope[1] else {
            unreachable!()
        };
        let resolved =
            resolve_invoke_deferred_with(invoke, &scope, 1, &std::collections::BTreeMap::new())
                .unwrap();
        assert_eq!(
            resolved.get(&("admin".to_string(), "testuser_otp".to_string())),
            Some(&EvaluatedValue::String("otp-12345".to_string())),
            "the child must inherit the real resolved value"
        );
    }

    #[test]
    fn test_resolve_invoke_deferred_with_run_step_producer_resolves() {
        let invoke = invoke_with_deferred_with(&[("count", "${{ steps.emit.outputs.count }}")]);
        let scope = vec![
            executed_run_with_outputs(
                "emit",
                vec![captured(
                    "count",
                    OutputType::Number,
                    OutputValue::Number(7.0),
                )],
            ),
            TestStep::Invoke(invoke),
        ];
        let TestStep::Invoke(invoke) = &scope[1] else {
            unreachable!()
        };
        let resolved =
            resolve_invoke_deferred_with(invoke, &scope, 1, &std::collections::BTreeMap::new())
                .unwrap();
        assert_eq!(
            resolved.get(&("emit".to_string(), "count".to_string())),
            Some(&EvaluatedValue::Number(7.0))
        );
    }

    #[test]
    fn test_resolve_invoke_deferred_with_forward_reference_is_error() {
        let invoke = invoke_with_deferred_with(&[("otp", "${{ steps.admin.outputs.otp }}")]);
        let scope = vec![
            TestStep::Invoke(invoke),
            executed_invoke(
                "admin",
                vec![captured(
                    "otp",
                    OutputType::String,
                    OutputValue::String("late".to_string()),
                )],
            ),
        ];
        let TestStep::Invoke(invoke) = &scope[0] else {
            unreachable!()
        };
        let err =
            resolve_invoke_deferred_with(invoke, &scope, 0, &std::collections::BTreeMap::new())
                .unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("has not run yet"),
            "forward reference in with: must be a hard error: {msg}"
        );
    }

    #[test]
    fn test_resolve_invoke_deferred_with_unknown_step_is_error() {
        let invoke = invoke_with_deferred_with(&[("otp", "${{ steps.nope.outputs.otp }}")]);
        let scope = vec![TestStep::Invoke(invoke)];
        let TestStep::Invoke(invoke) = &scope[0] else {
            unreachable!()
        };
        let err =
            resolve_invoke_deferred_with(invoke, &scope, 0, &std::collections::BTreeMap::new())
                .unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("no step with id"),
            "unknown reference in with: must be a hard error: {msg}"
        );
    }

    #[test]
    fn test_resolve_invoke_deferred_with_outputs_ref_is_error() {
        let invoke = invoke_with_deferred_with(&[("x", "${{ outputs.value }}")]);
        let scope = vec![TestStep::Invoke(invoke)];
        let TestStep::Invoke(invoke) = &scope[0] else {
            unreachable!()
        };
        let err =
            resolve_invoke_deferred_with(invoke, &scope, 0, &std::collections::BTreeMap::new())
                .unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("outputs.* not available"),
            "outputs.* in outer-scope with: must be a hard error: {msg}"
        );
    }

    #[test]
    fn test_resolve_invoke_deferred_with_inherited_ref_takes_precedence() {
        // An outer-scope ref already resolved at a previous invocation boundary
        // is forwarded to the child without a local-scope lookup.
        let invoke = invoke_with_deferred_with(&[("otp", "${{ steps.outer.outputs.otp }}")]);
        let scope = vec![TestStep::Invoke(invoke)];
        let TestStep::Invoke(invoke) = &scope[0] else {
            unreachable!()
        };
        let mut inherited = std::collections::BTreeMap::new();
        inherited.insert(
            ("outer".to_string(), "otp".to_string()),
            EvaluatedValue::String("from-outer".to_string()),
        );
        let resolved = resolve_invoke_deferred_with(invoke, &scope, 0, &inherited).unwrap();
        assert_eq!(
            resolved.get(&("outer".to_string(), "otp".to_string())),
            Some(&EvaluatedValue::String("from-outer".to_string()))
        );
    }

    #[test]
    fn test_resolve_invoke_deferred_with_empty_yields_empty_map() {
        let invoke = invoke_with_deferred_with(&[]);
        let scope = vec![TestStep::Invoke(invoke)];
        let TestStep::Invoke(invoke) = &scope[0] else {
            unreachable!()
        };
        let resolved =
            resolve_invoke_deferred_with(invoke, &scope, 0, &std::collections::BTreeMap::new())
                .unwrap();
        assert!(resolved.is_empty());
    }

    #[test]
    fn test_resolve_run_refs_run_step_id_resolves() {
        let scope = vec![
            executed_run_with_outputs(
                "build",
                vec![captured(
                    "version",
                    OutputType::String,
                    OutputValue::String("2.0.0".to_string()),
                )],
            ),
            TestStep::Run(consumer_step("echo noop")),
        ];
        let step = consumer_step("echo ${{ steps.build.outputs.version }}");
        let resolved = resolve_run_step_output_refs(
            &step,
            &scope,
            1,
            None,
            &std::collections::BTreeMap::new(),
        )
        .unwrap();
        assert_eq!(resolved, "echo 2.0.0");
    }

    #[test]
    fn test_resolve_run_refs_secret_output_uses_real_value() {
        let scope = vec![
            executed_run_with_outputs(
                "build",
                vec![captured(
                    "token",
                    OutputType::Secret,
                    OutputValue::String("super-secret-token".to_string()),
                )],
            ),
            TestStep::Run(consumer_step("echo noop")),
        ];
        let step = consumer_step("echo ${{ steps.build.outputs.token }}");
        let resolved = resolve_run_step_output_refs(
            &step,
            &scope,
            1,
            None,
            &std::collections::BTreeMap::new(),
        )
        .unwrap();
        assert_eq!(resolved, "echo super-secret-token");
    }

    // --- Stage 5: secret masking invariant regression tests ---

    #[test]
    fn test_captured_output_display_value_secret_masks() {
        // CapturedOutput::display_value() must mask secrets at display sinks.
        let c = captured(
            "token",
            OutputType::Secret,
            OutputValue::String("super-secret-token".to_string()),
        );
        assert_eq!(
            c.display_value(),
            "***",
            "display_value must return *** for a secret output"
        );
        // Use sink still gets the real value.
        assert_eq!(
            c.value.to_use_string(),
            "super-secret-token",
            "to_use_string must return the real value at use sinks"
        );
    }

    #[test]
    fn test_captured_output_display_value_non_secret_shows_real() {
        let c = captured(
            "version",
            OutputType::String,
            OutputValue::String("1.2.3".to_string()),
        );
        assert_eq!(c.display_value(), "1.2.3");
    }

    #[test]
    fn test_resolver_error_on_secret_output_does_not_leak_raw_value() {
        // Resolver errors must never include the raw secret value in their message.
        // The "no captured outputs" error path should only mention step/output names.
        let scope = vec![
            TestStep::Invoke(InvokeStep {
                uses: "frag.yaml".to_string(),
                id: Some("fetch".to_string()),
                steps: vec![],
                output_decls: vec![],
                deferred_with: std::collections::BTreeMap::new(),
                captured_outputs: std::cell::RefCell::new(None),
            }),
            TestStep::Run(consumer_step("echo noop")),
        ];
        let step = consumer_step("echo ${{ steps.fetch.outputs.token }}");
        let err = resolve_run_step_output_refs(
            &step,
            &scope,
            1,
            None,
            &std::collections::BTreeMap::new(),
        )
        .unwrap_err();
        let msg = format!("{err:#}");
        // Confirm the error is about missing outputs (not a value leak).
        assert!(
            msg.contains("has no captured outputs"),
            "resolver error should describe the problem, not the value: {msg}"
        );
        // Confirm no raw secret value appears (defensive — the value is not even set here).
        assert!(
            !msg.contains("super-secret"),
            "resolver error must not contain a raw secret value: {msg}"
        );
    }

    #[test]
    fn test_resolver_error_unknown_output_does_not_leak_raw_value() {
        // "does not declare output X" error must mention the name but not any raw secret.
        let secret_val = "raw-secret-xyz";
        let scope = vec![
            executed_run_with_outputs(
                "fetch",
                vec![captured(
                    "token",
                    OutputType::Secret,
                    OutputValue::String(secret_val.to_string()),
                )],
            ),
            TestStep::Run(consumer_step("echo noop")),
        ];
        let step = consumer_step("echo ${{ steps.fetch.outputs.nope }}");
        let err = resolve_run_step_output_refs(
            &step,
            &scope,
            1,
            None,
            &std::collections::BTreeMap::new(),
        )
        .unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("does not declare output 'nope'"),
            "error must describe the problem: {msg}"
        );
        assert!(
            !msg.contains(secret_val),
            "resolver error must not contain the raw secret value: {msg}"
        );
    }

    #[test]
    fn test_invoke_reexported_secret_output_is_secret_typed_and_masks_at_display() {
        // A secret inner output re-exported through a fragment boundary must:
        // - stay Secret-typed on invoke.captured_outputs
        // - mask via display_value()
        // - resolve real at the use sink

        let inner_run = executed_run_with_outputs(
            "fetcher",
            vec![captured(
                "token",
                OutputType::Secret,
                OutputValue::String("my-secret-value".to_string()),
            )],
        );

        let invoke = InvokeStep {
            uses: "frag.yaml".to_string(),
            id: Some("call".to_string()),
            steps: vec![inner_run],
            output_decls: vec![FragmentOutputDecl {
                name: "token".to_string(),
                output_type: OutputType::Secret,
                value: "${{ steps.fetcher.outputs.token }}".to_string(),
                required: true,
                default: None,
            }],
            deferred_with: std::collections::BTreeMap::new(),
            captured_outputs: std::cell::RefCell::new(None),
        };

        super::resolve_invoke_outputs_for_test(&invoke).unwrap();

        let guard = invoke.captured_outputs.borrow();
        let outputs = guard
            .as_ref()
            .expect("captured_outputs must be set after resolution");
        assert_eq!(outputs.len(), 1, "one output expected");
        let out = &outputs[0];

        // Must stay Secret-typed at the boundary.
        assert_eq!(
            out.declared_type,
            OutputType::Secret,
            "re-exported secret must remain Secret-typed at Invoke boundary"
        );
        // Display sink: must mask.
        assert_eq!(
            out.display_value(),
            "***",
            "re-exported secret must mask at display sinks"
        );
        // Use sink: must be real.
        assert_eq!(
            out.value.to_use_string(),
            "my-secret-value",
            "re-exported secret must resolve real at use sinks"
        );
    }

    #[test]
    fn test_invoke_reexported_secret_use_sink_resolves_real_value() {
        // After boundary resolution, referencing the invoke's secret output via
        // ${{ steps.call.outputs.token }} must yield the real value (use sink).

        let inner_run = executed_run_with_outputs(
            "fetcher",
            vec![captured(
                "token",
                OutputType::Secret,
                OutputValue::String("boundary-secret".to_string()),
            )],
        );

        let invoke = InvokeStep {
            uses: "frag.yaml".to_string(),
            id: Some("call".to_string()),
            steps: vec![inner_run],
            output_decls: vec![FragmentOutputDecl {
                name: "token".to_string(),
                output_type: OutputType::Secret,
                value: "${{ steps.fetcher.outputs.token }}".to_string(),
                required: true,
                default: None,
            }],
            deferred_with: std::collections::BTreeMap::new(),
            captured_outputs: std::cell::RefCell::new(None),
        };

        super::resolve_invoke_outputs_for_test(&invoke).unwrap();

        // Wire the resolved invoke into a scope and resolve a use-sink reference.
        let scope = vec![
            TestStep::Invoke(invoke),
            TestStep::Run(consumer_step("echo noop")),
        ];
        let step = consumer_step("echo ${{ steps.call.outputs.token }}");
        let resolved = resolve_run_step_output_refs(
            &step,
            &scope,
            1,
            None,
            &std::collections::BTreeMap::new(),
        )
        .unwrap();
        // Use sink must receive the real value.
        assert_eq!(
            resolved, "echo boundary-secret",
            "use sink reference to re-exported secret must resolve real"
        );
    }

    #[test]
    fn test_resolve_run_refs_unknown_output_name_is_error() {
        let scope = vec![
            executed_invoke(
                "build",
                vec![captured(
                    "version",
                    OutputType::String,
                    OutputValue::String("1.2.3".to_string()),
                )],
            ),
            TestStep::Run(consumer_step("echo noop")),
        ];
        let step = consumer_step("echo ${{ steps.build.outputs.nope }}");

        let err = resolve_run_step_output_refs(
            &step,
            &scope,
            1,
            None,
            &std::collections::BTreeMap::new(),
        )
        .unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("does not declare output 'nope'"),
            "unknown output name must be a hard error: {msg}"
        );
    }

    #[test]
    fn test_resolve_run_refs_invoke_without_outputs_is_error() {
        let scope = vec![
            TestStep::Invoke(InvokeStep {
                uses: "frag.yaml".to_string(),
                id: Some("build".to_string()),
                steps: vec![],
                output_decls: vec![],
                deferred_with: std::collections::BTreeMap::new(),
                captured_outputs: std::cell::RefCell::new(None),
            }),
            TestStep::Run(consumer_step("echo noop")),
        ];
        let step = consumer_step("echo ${{ steps.build.outputs.version }}");

        let err = resolve_run_step_output_refs(
            &step,
            &scope,
            1,
            None,
            &std::collections::BTreeMap::new(),
        )
        .unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("has no captured outputs"),
            "invoke without outputs must be a hard error: {msg}"
        );
    }

    #[test]
    fn test_resolve_run_refs_fragment_outputs_ref_resolves_with_default() {
        let scope = vec![
            executed_run_with_outputs(
                "emit",
                vec![captured(
                    "version",
                    OutputType::String,
                    OutputValue::String("1.2.3".to_string()),
                )],
            ),
            executed_run_with_outputs(
                "quiet",
                vec![captured("maybe", OutputType::String, OutputValue::Null)],
            ),
            TestStep::Run(consumer_step("echo noop")),
        ];
        let step = consumer_step("echo ${{ outputs.version }}-${{ outputs.fallback }}");
        let decls = vec![
            FragmentOutputDecl {
                name: "version".to_string(),
                output_type: OutputType::String,
                value: "${{ steps.emit.outputs.version }}".to_string(),
                required: true,
                default: None,
            },
            FragmentOutputDecl {
                name: "fallback".to_string(),
                output_type: OutputType::String,
                value: "${{ steps.quiet.outputs.maybe }}".to_string(),
                required: false,
                default: Some(OutputValue::String("defaulted".to_string())),
            },
        ];

        let resolved = resolve_run_step_output_refs(
            &step,
            &scope,
            2,
            Some(&decls),
            &std::collections::BTreeMap::new(),
        )
        .unwrap();
        assert_eq!(resolved, "echo 1.2.3-defaulted");
    }

    #[test]
    fn test_resolve_run_refs_fragment_outputs_ref_forward_is_error() {
        let scope = vec![
            TestStep::Run(consumer_step("echo noop")),
            executed_run_with_outputs(
                "emit",
                vec![captured(
                    "version",
                    OutputType::String,
                    OutputValue::String("1.2.3".to_string()),
                )],
            ),
        ];
        let step = consumer_step("echo ${{ outputs.version }}");
        let decls = vec![FragmentOutputDecl {
            name: "version".to_string(),
            output_type: OutputType::String,
            value: "${{ steps.emit.outputs.version }}".to_string(),
            required: true,
            default: None,
        }];

        let err = resolve_run_step_output_refs(
            &step,
            &scope,
            0,
            Some(&decls),
            &std::collections::BTreeMap::new(),
        )
        .unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("has not run yet"),
            "forward fragment output reference must fail: {msg}"
        );
    }

    // --- check_expect_block ---

    use super::{check_expect_block, check_stdio_expect, StepCapture};
    use crate::step::{ExpectBlock, StdioExpect};

    fn make_capture(stdout: &str, stderr: &str) -> StepCapture {
        StepCapture {
            stdout: stdout.to_string(),
            stderr: stderr.to_string(),
        }
    }

    fn make_expect(
        exit: Option<i32>,
        stdout: Option<StdioExpect>,
        stderr: Option<StdioExpect>,
    ) -> ExpectBlock {
        ExpectBlock {
            exit,
            stdout,
            stderr,
        }
    }

    #[test]
    fn test_check_expect_exit_code_matches_succeeds() {
        let expect = make_expect(Some(0), None, None);
        let cap = make_capture("", "");
        assert!(check_expect_block("step", &expect, &cap, 0).is_ok());
    }

    #[test]
    fn test_check_expect_exit_code_mismatch_fails() {
        let expect = make_expect(Some(0), None, None);
        let cap = make_capture("", "");
        let err = check_expect_block("step", &expect, &cap, 1).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("exit"), "should mention 'exit': {msg}");
        assert!(
            msg.contains("expected 0"),
            "should mention expected 0: {msg}"
        );
        assert!(msg.contains("got 1"), "should mention got 1: {msg}");
    }

    #[test]
    fn test_check_expect_nonzero_exit_matches_succeeds() {
        let expect = make_expect(Some(2), None, None);
        let cap = make_capture("", "");
        assert!(check_expect_block("must-exit-2", &expect, &cap, 2).is_ok());
    }

    #[test]
    fn test_check_expect_default_exit_zero() {
        // No `exit:` in expect block → defaults to 0
        let expect = make_expect(None, None, None);
        let cap = make_capture("", "");
        assert!(check_expect_block("step", &expect, &cap, 0).is_ok());
        let err = check_expect_block("step", &expect, &cap, 1).unwrap_err();
        assert!(err.to_string().contains("expected 0, got 1"));
    }

    #[test]
    fn test_check_expect_stdout_contains_present_succeeds() {
        let expect = make_expect(
            None,
            Some(StdioExpect {
                contains: vec!["hello".to_string(), "world".to_string()],
                not_contains: vec![],
            }),
            None,
        );
        let cap = make_capture("hello world\n", "");
        assert!(check_expect_block("step", &expect, &cap, 0).is_ok());
    }

    #[test]
    fn test_check_expect_stdout_contains_missing_fails() {
        let expect = make_expect(
            None,
            Some(StdioExpect {
                contains: vec!["missing".to_string()],
                not_contains: vec![],
            }),
            None,
        );
        let cap = make_capture("something else\n", "");
        let err = check_expect_block("step", &expect, &cap, 0).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("stdout"), "should mention stdout: {msg}");
        assert!(msg.contains("missing"), "should mention needle: {msg}");
        assert!(msg.contains("not found"), "should say not found: {msg}");
    }

    #[test]
    fn test_check_expect_stdout_not_contains_absent_succeeds() {
        let expect = make_expect(
            None,
            Some(StdioExpect {
                contains: vec![],
                not_contains: vec!["error".to_string()],
            }),
            None,
        );
        let cap = make_capture("all good\n", "");
        assert!(check_expect_block("step", &expect, &cap, 0).is_ok());
    }

    #[test]
    fn test_check_expect_stdout_not_contains_present_fails() {
        let expect = make_expect(
            None,
            Some(StdioExpect {
                contains: vec![],
                not_contains: vec!["denied".to_string()],
            }),
            None,
        );
        let cap = make_capture("permission denied\n", "");
        let err = check_expect_block("step", &expect, &cap, 0).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("stdout"), "should mention stdout: {msg}");
        assert!(msg.contains("denied"), "should mention needle: {msg}");
        assert!(
            msg.contains("NOT to contain"),
            "should say NOT to contain: {msg}"
        );
    }

    #[test]
    fn test_check_expect_stderr_matchers_work() {
        let expect = make_expect(
            None,
            None,
            Some(StdioExpect {
                contains: vec!["warn".to_string()],
                not_contains: vec!["error".to_string()],
            }),
        );
        // passes: stderr contains "warn" and does not contain "error"
        let cap = make_capture("", "warning: something\n");
        assert!(check_expect_block("step", &expect, &cap, 0).is_ok());
        // fails: stderr contains forbidden "error"
        let cap_bad = make_capture("", "error: bad\n");
        let err = check_expect_block("step", &expect, &cap_bad, 0).unwrap_err();
        assert!(err.to_string().contains("stderr"));
    }

    #[test]
    fn test_check_expect_multiple_failures_all_reported() {
        let expect = make_expect(
            Some(0),
            Some(StdioExpect {
                contains: vec!["expected1".to_string(), "expected2".to_string()],
                not_contains: vec!["forbidden".to_string()],
            }),
            None,
        );
        let cap = make_capture("forbidden text\n", "");
        let err = check_expect_block("step", &expect, &cap, 1).unwrap_err();
        let msg = err.to_string();
        // All three failures (wrong exit, missing expected1, missing expected2, forbidden present)
        // should appear in the error
        assert!(msg.contains("exit"), "should mention exit: {msg}");
        assert!(msg.contains("expected1"), "should mention expected1: {msg}");
        assert!(msg.contains("expected2"), "should mention expected2: {msg}");
        assert!(msg.contains("forbidden"), "should mention forbidden: {msg}");
    }

    #[test]
    fn test_check_stdio_expect_all_contains_must_match() {
        let expect = StdioExpect {
            contains: vec!["a".to_string(), "b".to_string(), "c".to_string()],
            not_contains: vec![],
        };
        let mut failures = Vec::new();
        check_stdio_expect("a b", "stdout", &expect, &mut failures);
        // "c" is missing
        assert_eq!(failures.len(), 1, "one failure expected: {failures:?}");
        assert!(
            failures[0].contains("c"),
            "failure should mention 'c': {:?}",
            failures[0]
        );
    }

    // --- run_host_step_capturing ---

    #[test]
    fn test_host_step_capturing_captures_stdout_and_stderr() {
        let dir = tempfile::tempdir().unwrap();
        let tmpl = resolve_shell(Some("sh")).unwrap();
        let env_file = tmp_env_file();
        let out_file = tmp_out_file();
        let log_file = tmp_step_log(dir.path());
        let (cap, code) = super::run_host_step_capturing(
            "capture-test",
            "printf 'out-line\\n'; printf 'err-line\\n' >&2",
            dir.path(),
            test_budget(),
            &tmpl,
            &[],
            HostStepFiles {
                env_file: &env_file,
                out_file: &out_file,
                log_path: &log_file,
            },
        )
        .unwrap();
        let _ = std::fs::remove_file(&env_file);
        assert_eq!(code, 0, "exit code should be 0");
        assert!(
            cap.stdout.contains("out-line"),
            "stdout should contain out-line: {:?}",
            cap.stdout
        );
        assert!(
            cap.stderr.contains("err-line"),
            "stderr should contain err-line: {:?}",
            cap.stderr
        );
    }

    #[test]
    fn test_host_step_capturing_returns_nonzero_exit_without_error() {
        let dir = tempfile::tempdir().unwrap();
        let tmpl = resolve_shell(Some("sh")).unwrap();
        let env_file = tmp_env_file();
        let out_file = tmp_out_file();
        let log_file = tmp_step_log(dir.path());
        let (cap, code) = super::run_host_step_capturing(
            "nonzero",
            "echo failing; exit 2",
            dir.path(),
            test_budget(),
            &tmpl,
            &[],
            HostStepFiles {
                env_file: &env_file,
                out_file: &out_file,
                log_path: &log_file,
            },
        )
        .unwrap();
        let _ = std::fs::remove_file(&env_file);
        assert_eq!(code, 2, "should return exit code 2, got {code}");
        assert!(
            cap.stdout.contains("failing"),
            "stdout should contain 'failing': {:?}",
            cap.stdout
        );
    }
}
