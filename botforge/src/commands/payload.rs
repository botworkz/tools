use anyhow::{bail, Context, Result};
use clap::Args;
use serde::Deserialize;
use std::path::{Component, Path, PathBuf};

use crate::iso::build_iso;
use crate::util::{create_temp_dir, normalize_path, validate_flat_filename};

#[derive(Args, Debug)]
pub(crate) struct PayloadArgs {
    /// Output payload ISO file path.
    #[arg(long, required = true)]
    out: PathBuf,
    /// Optional staging directory to populate before ISO build.
    #[arg(long)]
    staging_dir: Option<PathBuf>,
    /// ISO volume ID.
    #[arg(long, default_value = "botwork-payload")]
    volume_id: String,
}

#[derive(Debug, Deserialize, Default)]
struct PayloadConfig {
    #[serde(default)]
    images: Vec<PayloadImage>,
    #[serde(default)]
    files: Vec<PayloadFile>,
    #[serde(default)]
    services: PayloadServices,
}

#[derive(Debug, Deserialize)]
struct PayloadImage {
    source: PathBuf,
    filename: Option<String>,
}

#[derive(Debug, Deserialize)]
struct PayloadFile {
    source: PathBuf,
    staging_path: PathBuf,
    install_path: PathBuf,
    #[serde(default = "default_payload_file_mode")]
    mode: String,
}

#[derive(Debug, Deserialize, Default)]
struct PayloadServices {
    #[serde(default)]
    enable: Vec<String>,
    #[serde(default)]
    restart: Vec<String>,
}

fn default_payload_file_mode() -> String {
    "0644".to_string()
}

pub(crate) fn cmd_payload(config_path: &Path, args: PayloadArgs) -> Result<()> {
    let payload = load_payload_config(config_path)?;
    let config_dir = config_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));

    let (staging_dir, remove_staging) = if let Some(dir) = args.staging_dir {
        (normalize_path(&dir), false)
    } else {
        (create_temp_dir("botforge-payload")?, true)
    };

    let result = (|| -> Result<()> {
        std::fs::create_dir_all(&staging_dir)
            .with_context(|| format!("cannot create staging dir: {}", staging_dir.display()))?;
        stage_payload_tree(&payload, &config_dir, &staging_dir)?;
        write_payload_bootstrap_script(&payload, &staging_dir)?;
        build_iso(&staging_dir, &args.out, &args.volume_id)?;
        Ok(())
    })();

    if remove_staging {
        let _ = std::fs::remove_dir_all(&staging_dir);
    }
    result?;

    println!(
        "built payload ISO at {} (volume id: {})",
        args.out.display(),
        args.volume_id
    );
    Ok(())
}

fn load_payload_config(path: &Path) -> Result<PayloadConfig> {
    let yaml = std::fs::read_to_string(path)
        .with_context(|| format!("cannot read payload config: {}", path.display()))?;
    serde_yaml::from_str(&yaml)
        .with_context(|| format!("invalid payload config: {}", path.display()))
}

fn stage_payload_tree(
    payload: &PayloadConfig,
    config_dir: &Path,
    staging_dir: &Path,
) -> Result<()> {
    let image_dir = staging_dir.join("images");
    std::fs::create_dir_all(&image_dir)
        .with_context(|| format!("cannot create image staging dir: {}", image_dir.display()))?;

    for image in &payload.images {
        let source = resolve_config_relative_path(config_dir, &image.source);
        if !source.is_file() {
            bail!("payload image source is not a file: {}", source.display());
        }
        let filename = if let Some(name) = &image.filename {
            name.clone()
        } else {
            source
                .file_name()
                .and_then(|name| name.to_str())
                .map(str::to_string)
                .with_context(|| {
                    format!(
                        "payload image source has no file name for default tarball naming: {}",
                        source.display()
                    )
                })?
        };
        validate_flat_filename(&filename)?;
        copy_file(&source, &image_dir.join(filename))?;
    }

    for file in &payload.files {
        validate_mode_string(&file.mode)?;
        validate_payload_install_path(&file.install_path)?;

        let source = resolve_config_relative_path(config_dir, &file.source);
        if !source.is_file() {
            bail!("payload overlay source is not a file: {}", source.display());
        }
        let relative_stage_path = validate_relative_staging_path(&file.staging_path)?;
        let staged_dest = staging_dir.join(relative_stage_path);
        copy_file(&source, &staged_dest)?;
    }

    Ok(())
}

fn write_payload_bootstrap_script(payload: &PayloadConfig, staging_dir: &Path) -> Result<()> {
    let bootstrap = render_payload_bootstrap(payload)?;
    let path = staging_dir.join("bootstrap.sh");
    std::fs::write(&path, bootstrap)
        .with_context(|| format!("cannot write payload bootstrap script: {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&path)
            .with_context(|| format!("cannot stat payload bootstrap script: {}", path.display()))?
            .permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).with_context(|| {
            format!(
                "cannot set executable mode on payload bootstrap script: {}",
                path.display()
            )
        })?;
    }
    Ok(())
}

fn render_payload_bootstrap(payload: &PayloadConfig) -> Result<String> {
    let mut script = String::from(
        "#!/usr/bin/env bash\nset -euo pipefail\n\nPAYLOAD_MOUNT=/mnt/botwork-payload\n\nshopt -s nullglob\nfor image_tar in \"$PAYLOAD_MOUNT\"/images/*.tar; do\n  docker load -i \"$image_tar\"\ndone\nshopt -u nullglob\n",
    );

    for file in &payload.files {
        validate_mode_string(&file.mode)?;
        let relative_stage_path = validate_relative_staging_path(&file.staging_path)?;
        validate_payload_install_path(&file.install_path)?;
        let payload_source = Path::new("/mnt/botwork-payload").join(relative_stage_path);
        script.push_str(&format!(
            "install -D -m {} {} {}\n",
            file.mode,
            shell_single_quote(&payload_source.display().to_string()),
            shell_single_quote(&file.install_path.display().to_string())
        ));
    }

    if !payload.services.enable.is_empty() || !payload.services.restart.is_empty() {
        script.push_str("systemctl daemon-reload\n");
    }
    for service in &payload.services.enable {
        script.push_str(&format!(
            "systemctl enable {}\n",
            shell_single_quote(service)
        ));
    }
    for service in &payload.services.restart {
        script.push_str(&format!(
            "systemctl restart {}\n",
            shell_single_quote(service)
        ));
    }

    Ok(script)
}

fn validate_relative_staging_path(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        bail!(
            "payload staging_path must be relative, got: {}",
            path.display()
        );
    }
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => normalized.push(part),
            _ => bail!(
                "payload staging_path must not contain '.' or '..' segments: {}",
                path.display()
            ),
        }
    }
    if normalized.as_os_str().is_empty() {
        bail!("payload staging_path must not be empty");
    }
    Ok(normalized)
}

fn validate_payload_install_path(path: &Path) -> Result<()> {
    if !path.is_absolute() {
        bail!(
            "payload install_path must be absolute in guest filesystem: {}",
            path.display()
        );
    }
    if path
        .components()
        .any(|component| matches!(component, Component::CurDir | Component::ParentDir))
    {
        bail!(
            "payload install_path must not contain '.' or '..' segments: {}",
            path.display()
        );
    }
    Ok(())
}

fn resolve_config_relative_path(config_dir: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        normalize_path(path)
    } else {
        normalize_path(&config_dir.join(path))
    }
}

fn validate_mode_string(mode: &str) -> Result<()> {
    if mode.len() < 3 || mode.len() > 4 || !mode.chars().all(|ch| ('0'..='7').contains(&ch)) {
        bail!("payload file mode must be 3-4 octal digits, got: {mode}");
    }
    Ok(())
}

fn copy_file(source: &Path, destination: &Path) -> Result<()> {
    if let Some(parent) = destination.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("cannot create destination dir: {}", parent.display()))?;
    }
    std::fs::copy(source, destination).with_context(|| {
        format!(
            "cannot copy payload file from {} to {}",
            source.display(),
            destination.display()
        )
    })?;
    Ok(())
}

fn shell_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

#[cfg(test)]
mod tests {
    use super::{
        render_payload_bootstrap, shell_single_quote, stage_payload_tree, validate_mode_string,
        validate_payload_install_path, validate_relative_staging_path, PayloadConfig, PayloadFile,
        PayloadImage, PayloadServices,
    };
    use std::path::Path;
    use tempfile::TempDir;

    #[test]
    fn validate_relative_staging_path_rejects_non_relative_values() {
        assert!(validate_relative_staging_path(Path::new("/etc/botwork/envoy.yaml")).is_err());
        assert!(validate_relative_staging_path(Path::new("../envoy.yaml")).is_err());
        assert!(validate_relative_staging_path(Path::new("./envoy.yaml")).is_err());
    }

    #[test]
    fn validate_payload_install_path_rejects_non_absolute_values() {
        assert!(validate_payload_install_path(Path::new("etc/botwork/envoy.yaml")).is_err());
        assert!(validate_payload_install_path(Path::new("/etc/../tmp/file")).is_err());
    }

    #[test]
    fn validate_mode_string_accepts_octal_only() {
        assert!(validate_mode_string("0644").is_ok());
        assert!(validate_mode_string("755").is_ok());
        assert!(validate_mode_string("08").is_err());
        assert!(validate_mode_string("abc").is_err());
    }

    #[test]
    fn render_payload_bootstrap_includes_expected_steps() {
        let payload = PayloadConfig {
            images: vec![PayloadImage {
                source: Path::new("/tmp/session-broker.tar").to_path_buf(),
                filename: None,
            }],
            files: vec![PayloadFile {
                source: Path::new("/tmp/listener.yaml").to_path_buf(),
                staging_path: Path::new("envoy/lds/listener.yaml").to_path_buf(),
                install_path: Path::new("/etc/botwork/envoy/lds/listener.yaml").to_path_buf(),
                mode: "0644".to_string(),
            }],
            services: PayloadServices {
                enable: vec!["botwork-auth-broker".to_string()],
                restart: vec!["botwork-envoy".to_string()],
            },
        };

        let script = render_payload_bootstrap(&payload).unwrap();
        assert!(script.contains("docker load -i \"$image_tar\""));
        assert!(script.contains(
            "install -D -m 0644 '/mnt/botwork-payload/envoy/lds/listener.yaml' '/etc/botwork/envoy/lds/listener.yaml'"
        ));
        assert!(script.contains("systemctl daemon-reload"));
        assert!(script.contains("systemctl enable 'botwork-auth-broker'"));
        assert!(script.contains("systemctl restart 'botwork-envoy'"));
    }

    #[test]
    fn stage_payload_tree_creates_expected_layout() {
        let temp = TempDir::new().unwrap();
        let config_dir = temp.path().join("config");
        let staging_dir = temp.path().join("staging");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(config_dir.join("session-broker.tar"), b"tar").unwrap();
        std::fs::write(config_dir.join("listener.yaml"), b"listener").unwrap();

        let payload = PayloadConfig {
            images: vec![PayloadImage {
                source: Path::new("session-broker.tar").to_path_buf(),
                filename: None,
            }],
            files: vec![PayloadFile {
                source: Path::new("listener.yaml").to_path_buf(),
                staging_path: Path::new("envoy/lds/listener.yaml").to_path_buf(),
                install_path: Path::new("/etc/botwork/envoy/lds/listener.yaml").to_path_buf(),
                mode: "0644".to_string(),
            }],
            services: PayloadServices::default(),
        };

        stage_payload_tree(&payload, &config_dir, &staging_dir).unwrap();

        assert_eq!(
            std::fs::read(staging_dir.join("images/session-broker.tar")).unwrap(),
            b"tar"
        );
        assert_eq!(
            std::fs::read(staging_dir.join("envoy/lds/listener.yaml")).unwrap(),
            b"listener"
        );
    }

    #[test]
    fn shell_single_quote_escapes_embedded_quotes() {
        assert_eq!(shell_single_quote("a'b"), "'a'\"'\"'b'");
    }
}
