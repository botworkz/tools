use anyhow::{bail, Context, Result};
use bollard::container::{
    AttachContainerOptions, Config, CreateContainerOptions, RemoveContainerOptions,
    StartContainerOptions, WaitContainerOptions,
};
use bollard::errors::Error as DockerError;
use bollard::models::{DeviceMapping, HostConfig};
use bollard::Docker;
use clap::Args;
use futures_util::stream::StreamExt;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::util::{repo_relative_path, resolve_under_root, run_command};

#[derive(Args, Debug)]
pub(crate) struct PackArgs {
    /// VM checkout root containing images/ (default: current directory).
    #[arg(long)]
    repo_root: Option<PathBuf>,
    /// Compress the qcow2 output with qemu-img convert -c.
    #[arg(long)]
    compress: bool,
    /// SSH private key path (default: <repo-root>/build/packer_ssh_key).
    #[arg(long)]
    key: Option<PathBuf>,
    /// Packer tools container image reference.
    #[arg(
        long,
        env = "BOTFORGE_PACKER_IMAGE",
        default_value = "botwork/packer-tools:local"
    )]
    packer_image: String,
}

/// Run the simplified v1 Packer flow in a Docker container.
///
/// This intentionally does not build or stage dependencies/images; callers must
/// arrange that beforehand. KVM is required.
pub(crate) fn cmd_pack(args: PackArgs) -> Result<()> {
    if !Path::new("/dev/kvm").exists() {
        bail!("botforge pack requires KVM: /dev/kvm not found");
    }

    let repo_root = std::fs::canonicalize(
        args.repo_root
            .unwrap_or(std::env::current_dir().context("failed to determine current directory")?),
    )
    .context("failed to resolve repo root")?;
    if !repo_root.is_dir() {
        bail!("repo root is not a directory: {}", repo_root.display());
    }

    let build_dir = repo_root.join("build");
    let build_rel = repo_relative_path(&repo_root, &build_dir)?;
    if build_rel != "build" {
        bail!(
            "refusing to use non-standard build directory under repo root: {}",
            build_dir.display()
        );
    }
    std::fs::create_dir_all(&build_dir)
        .with_context(|| format!("cannot create build dir: {}", build_dir.display()))?;

    let build_output_dir = build_dir.join("output");
    if build_output_dir.exists() {
        let build_output_real = std::fs::canonicalize(&build_output_dir).with_context(|| {
            format!(
                "cannot resolve build output directory: {}",
                build_output_dir.display()
            )
        })?;
        let build_output_rel = repo_relative_path(&repo_root, &build_output_real)?;
        if build_output_rel != "build/output" {
            bail!(
                "refusing to remove non-standard build output path: {}",
                build_output_dir.display()
            );
        }
        std::fs::remove_dir_all(&build_output_dir).with_context(|| {
            format!(
                "cannot remove prior build output directory: {}",
                build_output_dir.display()
            )
        })?;
    }

    let default_key = build_dir.join("packer_ssh_key");
    let key_path = resolve_under_root(&repo_root, args.key.clone().unwrap_or(default_key.clone()));
    let uses_default_key = key_path == default_key;
    if uses_default_key && !key_path.exists() {
        println!("generating ephemeral SSH key at {}", key_path.display());
        run_command(
            "ssh-keygen",
            &[
                "-t".into(),
                "ed25519".into(),
                "-N".into(),
                "".into(),
                "-f".into(),
                key_path.display().to_string(),
            ],
            &[],
            "failed to generate default packer SSH key",
        )?;
    }

    let public_key_path = PathBuf::from(format!("{}.pub", key_path.display()));
    if !key_path.is_file() {
        bail!("SSH private key not found: {}", key_path.display());
    }
    if !public_key_path.is_file() {
        bail!("SSH public key not found: {}", public_key_path.display());
    }
    let public_key = std::fs::read_to_string(&public_key_path)
        .with_context(|| format!("cannot read SSH public key: {}", public_key_path.display()))?
        .trim()
        .to_string();
    let key_real = std::fs::canonicalize(&key_path)
        .with_context(|| format!("cannot resolve SSH private key: {}", key_path.display()))?;
    let _ = repo_relative_path(&repo_root, &key_real)?;
    let rel_key_path = repo_relative_path(&repo_root, &key_path)?;

    let host_uid = std::env::var("HOST_UID").unwrap_or(run_capture("id", &["-u"])?);
    let host_gid = std::env::var("HOST_GID").unwrap_or(run_capture("id", &["-g"])?);
    let host_kvm_gid = resolve_host_kvm_gid(
        std::env::var("HOST_KVM_GID").ok(),
        getent_group_kvm_output(),
    );

    let runtime = tokio::runtime::Runtime::new().context("failed to create tokio runtime")?;
    let run_config = DockerRunConfig {
        packer_image: &args.packer_image,
        repo_root: &repo_root,
        host_uid: &host_uid,
        host_gid: &host_gid,
        host_kvm_gid: &host_kvm_gid,
    };
    runtime.block_on(run_pack_containers(&run_config, &rel_key_path, &public_key))?;

    if args.compress {
        let source = build_output_dir.join("debian-13-botwork.qcow2");
        if !source.is_file() {
            bail!(
                "qcow2 source image not found for compression: {}",
                source.display()
            );
        }
        let target = build_dir.join("debian-13-botwork-compressed.qcow2");
        let rel_source = repo_relative_path(&repo_root, &source)?;
        let rel_target = repo_relative_path(&repo_root, &target)?;
        println!("compressing qcow2 to {}", target.display());
        runtime.block_on(run_step(
            &docker_client()?,
            "qcow2 compression",
            &run_config,
            compress_command(&rel_source, &rel_target),
        ))?;
    }

    println!("pack complete");
    Ok(())
}

async fn run_pack_containers(
    run_config: &DockerRunConfig<'_>,
    rel_key_path: &str,
    public_key: &str,
) -> Result<()> {
    let docker = docker_client()?;

    println!(
        "running packer init in container image {}",
        run_config.packer_image
    );
    run_step(&docker, "packer init", run_config, packer_init_command()).await?;

    println!(
        "running packer build in container image {}",
        run_config.packer_image
    );
    run_step(
        &docker,
        "packer build",
        run_config,
        packer_build_command(rel_key_path, public_key),
    )
    .await?;

    Ok(())
}

fn docker_client() -> Result<Docker> {
    Docker::connect_with_defaults().with_context(|| {
        "failed to connect to Docker daemon (check DOCKER_HOST or local Docker socket)"
    })
}

struct DockerRunConfig<'a> {
    packer_image: &'a str,
    repo_root: &'a Path,
    host_uid: &'a str,
    host_gid: &'a str,
    host_kvm_gid: &'a str,
}

async fn run_step(
    docker: &Docker,
    step_name: &str,
    run_config: &DockerRunConfig<'_>,
    command: Vec<String>,
) -> Result<()> {
    let repo_root_string = run_config.repo_root.display().to_string();
    // botforge may itself be running in a container against the host daemon (DooD). In that
    // setup Docker interprets bind sources on the host, so this must stay the host repo path.
    let binds = vec![format!("{repo_root_string}:/workspace")];

    let create_config: Config<String> = Config {
        image: Some(run_config.packer_image.to_string()),
        cmd: Some(command),
        env: Some(vec![
            "BOTWORK_IN_DOCKER=1".to_string(),
            "PACKER_PLUGIN_PATH=/workspace/build/packer-plugins".to_string(),
            "PACKER_CACHE_DIR=/workspace/build/packer-cache".to_string(),
        ]),
        working_dir: Some("/workspace".to_string()),
        user: Some(format!("{}:{}", run_config.host_uid, run_config.host_gid)),
        attach_stdout: Some(true),
        attach_stderr: Some(true),
        tty: Some(false),
        host_config: Some(HostConfig {
            binds: Some(binds),
            init: Some(true),
            devices: Some(vec![DeviceMapping {
                path_on_host: Some("/dev/kvm".to_string()),
                path_in_container: Some("/dev/kvm".to_string()),
                cgroup_permissions: Some("rwm".to_string()),
            }]),
            group_add: Some(vec![run_config.host_kvm_gid.to_string()]),
            ..Default::default()
        }),
        ..Default::default()
    };

    let create_result = match docker
        .create_container(None::<CreateContainerOptions<String>>, create_config)
        .await
    {
        Ok(result) => result,
        Err(DockerError::DockerResponseServerError {
            status_code: 404, ..
        }) => {
            bail!(
                "packer image '{}' is not available on the target Docker daemon; load/build it first",
                run_config.packer_image
            );
        }
        Err(err) => {
            return Err(err).with_context(|| format!("failed to create container for {step_name}"));
        }
    };

    let container_id = create_result.id;
    let run_result = run_existing_container(docker, &container_id, step_name).await;
    let remove_result = docker
        .remove_container(
            &container_id,
            Some(RemoveContainerOptions {
                force: true,
                ..Default::default()
            }),
        )
        .await;

    if let Err(err) = run_result {
        if let Err(remove_err) = remove_result {
            eprintln!("warning: failed to remove container {container_id}: {remove_err}");
        }
        return Err(err);
    }

    remove_result.with_context(|| format!("failed to remove container after {step_name}"))?;
    Ok(())
}

async fn run_existing_container(
    docker: &Docker,
    container_id: &str,
    step_name: &str,
) -> Result<()> {
    let mut attached = docker
        .attach_container(
            container_id,
            Some(AttachContainerOptions::<String> {
                stdout: Some(true),
                stderr: Some(true),
                stream: Some(true),
                logs: Some(true),
                ..Default::default()
            }),
        )
        .await
        .with_context(|| format!("failed to attach container output for {step_name}"))?;

    docker
        .start_container(container_id, None::<StartContainerOptions<String>>)
        .await
        .with_context(|| format!("failed to start container for {step_name}"))?;

    while let Some(frame) = attached.output.next().await {
        let frame = frame.with_context(|| format!("failed to stream output for {step_name}"))?;
        match frame {
            bollard::container::LogOutput::StdOut { message }
            | bollard::container::LogOutput::Console { message } => {
                std::io::stdout()
                    .write_all(&message)
                    .with_context(|| format!("failed to write stdout for {step_name}"))?;
                std::io::stdout()
                    .flush()
                    .with_context(|| format!("failed to flush stdout for {step_name}"))?;
            }
            bollard::container::LogOutput::StdErr { message } => {
                std::io::stderr()
                    .write_all(&message)
                    .with_context(|| format!("failed to write stderr for {step_name}"))?;
                std::io::stderr()
                    .flush()
                    .with_context(|| format!("failed to flush stderr for {step_name}"))?;
            }
            bollard::container::LogOutput::StdIn { .. } => {}
        }
    }

    let mut wait_stream = docker.wait_container(container_id, None::<WaitContainerOptions<String>>);
    let wait_result = wait_stream
        .next()
        .await
        .transpose()
        .with_context(|| format!("failed while waiting for container in {step_name}"))?
        .context("docker wait stream ended unexpectedly")?;

    if wait_result.status_code != 0 {
        bail!(
            "{step_name} failed (container exit status: {})",
            wait_result.status_code
        );
    }

    Ok(())
}

fn resolve_host_kvm_gid(env_value: Option<String>, getent_output: Option<String>) -> String {
    if let Some(value) = env_value.filter(|value| !value.trim().is_empty()) {
        return value;
    }

    if let Some(line) = getent_output {
        if let Some(gid) = line
            .lines()
            .find_map(|entry| entry.split(':').nth(2))
            .map(str::trim)
            .filter(|gid| !gid.is_empty())
        {
            return gid.to_string();
        }
    }

    "993".to_string()
}

fn packer_init_command() -> Vec<String> {
    vec!["packer".into(), "init".into(), "images/".into()]
}

fn packer_build_command(rel_key_path: &str, public_key: &str) -> Vec<String> {
    vec![
        "packer".into(),
        "build".into(),
        "-var".into(),
        "accelerator=kvm".into(),
        "-var".into(),
        format!("ssh_private_key_file={rel_key_path}"),
        "-var".into(),
        format!("ssh_public_key={public_key}"),
        "images/".into(),
    ]
}

fn compress_command(rel_source: &str, rel_target: &str) -> Vec<String> {
    vec![
        "qemu-img".into(),
        "convert".into(),
        "-O".into(),
        "qcow2".into(),
        "-c".into(),
        rel_source.into(),
        rel_target.into(),
    ]
}

fn run_capture(program: &str, args: &[&str]) -> Result<String> {
    let output = Command::new(program)
        .args(args)
        .output()
        .with_context(|| format!("failed to execute {program}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("{program} failed: {}", stderr.trim());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn getent_group_kvm_output() -> Option<String> {
    Command::new("getent")
        .args(["group", "kvm"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::{packer_build_command, resolve_host_kvm_gid};

    #[test]
    fn resolve_host_kvm_gid_prefers_env_override() {
        let gid = resolve_host_kvm_gid(Some("1234".into()), Some("kvm:x:55:".into()));
        assert_eq!(gid, "1234");
    }

    #[test]
    fn resolve_host_kvm_gid_parses_getent_output() {
        let gid = resolve_host_kvm_gid(None, Some("kvm:x:77:qemu".into()));
        assert_eq!(gid, "77");
    }

    #[test]
    fn resolve_host_kvm_gid_falls_back_to_default() {
        let gid = resolve_host_kvm_gid(None, None);
        assert_eq!(gid, "993");
    }

    #[test]
    fn packer_build_command_match_expected_argv() {
        let args = packer_build_command("build/packer_ssh_key", "ssh-ed25519 AAAA example");
        assert_eq!(
            args,
            vec![
                "packer",
                "build",
                "-var",
                "accelerator=kvm",
                "-var",
                "ssh_private_key_file=build/packer_ssh_key",
                "-var",
                "ssh_public_key=ssh-ed25519 AAAA example",
                "images/",
            ]
        );
    }
}
