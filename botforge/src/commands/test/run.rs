use anyhow::{Context, Result};
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::ssh::{
    journalctl_command, require_stable_ssh, scp_with_retry, ssh_capture_stdout, ssh_command_args,
    ssh_with_retry, wait_for_ssh, SshOptions,
};
use crate::util::{resolve_under_root, unique_suffix};

use super::config::{resolve_shell, StepTarget, TestConfig, TestIsoBootstrap};
use super::log::{
    join_output_forwarders, print_step_status, print_step_title, spawn_output_forwarder,
    step_log_path, StepLogWriter, StepOutputStream,
};

const TEST_SSH_READY_TIMEOUT: Duration = Duration::from_secs(300);
const TEST_CLOUD_INIT_TIMEOUT: Duration = Duration::from_secs(300);
const TEST_TRANSPORT_RETRIES: usize = 10;
const TEST_TRANSPORT_RETRY_DELAY: Duration = Duration::from_secs(2);
const TEST_STABLE_SSH_ATTEMPTS: usize = 5;
const TEST_STABLE_SSH_REQUIRED: usize = 2;

pub(super) fn run_test_flow(
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

pub(super) fn collect_test_diagnostics(ssh: &SshOptions, units: &[String]) {
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

pub(super) fn print_log_tail(path: &Path, line_count: usize) {
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

pub(super) fn cleanup_test(vm_child: &mut Option<Child>, overlay_image: &Path) {
    if let Some(child) = vm_child.as_mut() {
        let _ = child.kill();
        let _ = child.wait();
    }
    *vm_child = None;
    let _ = std::fs::remove_file(overlay_image);
}

#[cfg(test)]
mod tests {
    use super::{
        build_guest_env_preamble, env_merge, parse_env_file, run_host_step, shell_single_quote,
        HostStepFiles,
    };
    use crate::commands::test::config::resolve_shell;
    use crate::util::unique_suffix;
    use serde::Deserialize;
    use std::path::{Path, PathBuf};
    use std::time::Duration;

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
