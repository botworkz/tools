#![forbid(unsafe_code)]

use anyhow::{anyhow, bail, Context, Result};
#[allow(deprecated)]
use bollard::container::{
    Config, CreateContainerOptions, LogsOptions, StartContainerOptions, WaitContainerOptions,
};
use bollard::errors::Error as BollardError;
#[allow(deprecated)]
use bollard::image::CreateImageOptions;
use bollard::models::{DeviceMapping, HostConfig};
use bollard::Docker;
use futures_util::stream::TryStreamExt;
use nix::unistd::{Gid, Group, Uid};
use std::fs::OpenOptions;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

const MARKER_FILE: &str = "botforge.yaml";
const VERSION_FILE: &str = ".botforgeversion";
const KVM_DEVICE: &str = "/dev/kvm";
const DEFAULT_IMAGE: &str = "ghcr.io/botworkz/tools/botforge";

#[tokio::main]
async fn main() {
    match run().await {
        Ok(code) => std::process::exit(code as i32),
        Err(err) => {
            eprintln!("error: {err:#}");
            std::process::exit(1);
        }
    }
}

async fn run() -> Result<i64> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let verbose = env_truthy("BOTSPAWN_VERBOSE");

    let cwd = std::env::current_dir().context("failed to determine current directory")?;
    let repo_root = discover_repo_root(&cwd)?;
    let image = resolve_image(&repo_root)?;
    let host_ids = resolve_host_ids()?;

    eprintln!("botspawn: checking docker and kvm access...");
    let docker = preflight_docker().await?;
    preflight_kvm()?;

    ensure_image_present(&docker, &image, verbose).await?;

    eprintln!("botspawn: launching botforge from {image}");
    run_container(&docker, &repo_root, &image, &host_ids, &args).await
}

#[derive(Debug, Clone, Copy)]
struct HostIds {
    uid: u32,
    gid: u32,
    kvm_gid: u32,
}

fn discover_repo_root(start: &Path) -> Result<PathBuf> {
    let mut dir = start;
    loop {
        if dir.join(MARKER_FILE).is_file() {
            return std::fs::canonicalize(dir)
                .with_context(|| format!("cannot canonicalize repo root: {}", dir.display()));
        }

        match dir.parent() {
            Some(parent) => dir = parent,
            None => break,
        }
    }

    bail!(
        "not inside a botforge workspace: could not find '{MARKER_FILE}' in the current directory or any parent"
    );
}

fn resolve_image(repo_root: &Path) -> Result<String> {
    let path = repo_root.join(VERSION_FILE);
    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("cannot read {}", path.display()))?;
    let raw = raw.trim();
    if raw.is_empty() {
        bail!("{} is empty", path.display());
    }

    Ok(pin_digest_image(raw))
}

fn pin_digest_image(value: &str) -> String {
    let trimmed = value.trim();
    if let Some((left, digest)) = trimmed.split_once("@sha256:") {
        let digest = digest.trim();
        if digest.is_empty() {
            return trimmed.to_string();
        }
        let repo = strip_tag(left.trim());
        return format!("{repo}@sha256:{digest}");
    }

    if trimmed.is_empty() {
        DEFAULT_IMAGE.to_string()
    } else {
        trimmed.to_string()
    }
}

fn strip_tag(image: &str) -> &str {
    match image.rsplit_once(':') {
        Some((left, right)) if !right.contains('/') => left,
        _ => image,
    }
}

fn env_truthy(key: &str) -> bool {
    matches!(
        std::env::var(key).ok().as_deref(),
        Some("1") | Some("true") | Some("TRUE") | Some("yes") | Some("YES")
    )
}

fn resolve_host_ids() -> Result<HostIds> {
    let uid = std::env::var("HOST_UID")
        .ok()
        .map(|v| parse_u32_env("HOST_UID", &v))
        .transpose()?
        .unwrap_or_else(|| Uid::current().as_raw());

    let gid = std::env::var("HOST_GID")
        .ok()
        .map(|v| parse_u32_env("HOST_GID", &v))
        .transpose()?
        .unwrap_or_else(|| Gid::current().as_raw());

    let kvm_gid = std::env::var("HOST_KVM_GID")
        .ok()
        .map(|v| parse_u32_env("HOST_KVM_GID", &v))
        .transpose()?
        .map(Ok)
        .unwrap_or_else(|| {
            Group::from_name("kvm")?
                .map(|group| group.gid.as_raw())
                .ok_or_else(|| anyhow!("could not resolve gid for 'kvm' group; set HOST_KVM_GID"))
        })?;

    Ok(HostIds { uid, gid, kvm_gid })
}

fn parse_u32_env(name: &str, raw: &str) -> Result<u32> {
    raw.parse::<u32>()
        .with_context(|| format!("{name} must be a positive integer, got '{raw}'"))
}

async fn preflight_docker() -> Result<Docker> {
    let docker = Docker::connect_with_local_defaults()
        .context("failed to connect to docker through local defaults")?;

    docker
        .ping()
        .await
        .map_err(|err| anyhow!(classify_docker_ping_error(&err)))?;

    Ok(docker)
}

fn classify_docker_ping_error(err: &BollardError) -> String {
    let msg = err.to_string();
    if msg.contains("Permission denied") || msg.contains("os error 13") {
        "docker socket permission denied. Add your user to the docker group or fix socket ACLs."
            .to_string()
    } else if msg.contains("Connection refused")
        || msg.contains("No such file or directory")
        || msg.contains("failed to connect")
    {
        "docker daemon is not reachable. Start docker and ensure /var/run/docker.sock is available."
            .to_string()
    } else {
        format!("docker daemon preflight failed: {msg}")
    }
}

fn preflight_kvm() -> Result<()> {
    let dev = Path::new(KVM_DEVICE);
    if !dev.exists() {
        bail!(
            "{KVM_DEVICE} does not exist. Enable hardware virtualization/KVM on this host before running botforge."
        );
    }

    match OpenOptions::new().read(true).write(true).open(dev) {
        Ok(_) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::PermissionDenied => bail!(
            "{KVM_DEVICE} is not readable/writable by your user. Add your user to the 'kvm' group and re-login."
        ),
        Err(err) => Err(anyhow!("failed to open {KVM_DEVICE}: {err}")),
    }
}

#[allow(deprecated)]
async fn ensure_image_present(docker: &Docker, image: &str, verbose: bool) -> Result<()> {
    if docker.inspect_image(image).await.is_ok() {
        return Ok(());
    }

    eprintln!("botspawn: pulling botforge image...");
    #[allow(deprecated)]
    let options = Some(CreateImageOptions {
        from_image: image.to_string(),
        ..Default::default()
    });

    let mut stream = docker.create_image(options, None, None);
    while let Some(info) = stream.try_next().await? {
        if verbose {
            if let Some(status) = info.status {
                match (info.id, info.progress) {
                    (Some(id), Some(progress)) => eprintln!("[{id}] {status} {progress}"),
                    (Some(id), None) => eprintln!("[{id}] {status}"),
                    (None, Some(progress)) => eprintln!("{status} {progress}"),
                    (None, None) => eprintln!("{status}"),
                }
            }
        }
    }

    eprintln!("botspawn: image pull complete");
    Ok(())
}

fn passthrough_env(key: &str) -> String {
    format!("{key}={}", std::env::var(key).unwrap_or_default())
}

#[allow(deprecated)]
async fn run_container(
    docker: &Docker,
    repo_root: &Path,
    image: &str,
    host_ids: &HostIds,
    args: &[String],
) -> Result<i64> {
    let root = repo_root
        .to_str()
        .ok_or_else(|| anyhow!("repo root is not valid utf-8: {}", repo_root.display()))?
        .to_string();

    let env = vec![
        "HOME=/tmp".to_string(),
        "XDG_CACHE_HOME=/tmp/.cache".to_string(),
        passthrough_env("SHASSET_CACHE"),
        format!("HOST_UID={}", host_ids.uid),
        format!("HOST_GID={}", host_ids.gid),
        format!("HOST_KVM_GID={}", host_ids.kvm_gid),
        "LIBGUESTFS_BACKEND=direct".to_string(),
        passthrough_env("AWS_ACCESS_KEY_ID"),
        passthrough_env("AWS_SECRET_ACCESS_KEY"),
        passthrough_env("AWS_DEFAULT_REGION"),
        passthrough_env("AWS_ENDPOINT_URL"),
    ];

    #[allow(deprecated)]
    let config = Config::<String> {
        image: Some(image.to_string()),
        user: Some(format!("{}:{}", host_ids.uid, host_ids.gid)),
        working_dir: Some(root.clone()),
        env: Some(env),
        cmd: if args.is_empty() {
            None
        } else {
            Some(args.to_vec())
        },
        host_config: Some(HostConfig {
            auto_remove: Some(true),
            binds: Some(vec![format!("{root}:{root}")]),
            group_add: Some(vec![host_ids.kvm_gid.to_string()]),
            devices: Some(vec![DeviceMapping {
                path_on_host: Some(KVM_DEVICE.to_string()),
                path_in_container: Some(KVM_DEVICE.to_string()),
                cgroup_permissions: Some("rwm".to_string()),
            }]),
            ..Default::default()
        }),
        ..Default::default()
    };

    let response = docker
        .create_container(None::<CreateContainerOptions<String>>, config)
        .await
        .context("failed to create botforge container")?;

    docker
        .start_container(&response.id, None::<StartContainerOptions<String>>)
        .await
        .context("failed to start botforge container")?;

    let mut logs = docker.logs(
        &response.id,
        Some(LogsOptions::<String> {
            follow: true,
            stdout: true,
            stderr: true,
            ..Default::default()
        }),
    );

    while let Some(output) = logs.try_next().await? {
        match output {
            bollard::container::LogOutput::StdOut { message }
            | bollard::container::LogOutput::Console { message } => {
                io::stdout().write_all(&message)?;
                io::stdout().flush()?;
            }
            bollard::container::LogOutput::StdErr { message } => {
                io::stderr().write_all(&message)?;
                io::stderr().flush()?;
            }
            _ => {}
        }
    }

    let mut wait = docker.wait_container(&response.id, None::<WaitContainerOptions<String>>);
    let status = wait
        .try_next()
        .await?
        .ok_or_else(|| anyhow!("docker wait returned no result"))?;

    Ok(status.status_code)
}

#[cfg(test)]
mod tests {
    use super::{discover_repo_root, pin_digest_image};
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn discover_repo_root_walks_up_to_botforge_yaml() {
        let root = TempDir::new().unwrap();
        fs::write(root.path().join("botforge.yaml"), "").unwrap();
        let nested = root.path().join("a/b/c");
        fs::create_dir_all(&nested).unwrap();

        let found = discover_repo_root(&nested).unwrap();
        assert_eq!(found, root.path().canonicalize().unwrap());
    }

    #[test]
    fn discover_repo_root_requires_marker() {
        let root = TempDir::new().unwrap();
        let err = discover_repo_root(root.path()).unwrap_err();
        assert!(
            format!("{err:#}").contains("not inside a botforge workspace"),
            "unexpected error: {err:#}"
        );
    }

    #[test]
    fn pin_digest_image_removes_tag_when_digest_present() {
        let image = pin_digest_image("ghcr.io/botworkz/tools/botforge:v1.2.3@sha256:abcd");
        assert_eq!(image, "ghcr.io/botworkz/tools/botforge@sha256:abcd");
    }

    #[test]
    fn pin_digest_image_keeps_tag_only_form() {
        let image = pin_digest_image("ghcr.io/botworkz/tools/botforge:v1.2.3");
        assert_eq!(image, "ghcr.io/botworkz/tools/botforge:v1.2.3");
    }

    #[test]
    fn pin_digest_image_preserves_registry_port() {
        let image = pin_digest_image("localhost:5000/botforge:v1@sha256:beef");
        assert_eq!(image, "localhost:5000/botforge@sha256:beef");
    }
}
