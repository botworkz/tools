use anyhow::{Context, Result};
use serde::{de, Deserialize};
use serde_yaml::Value;
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use crate::qemu::PortSpec;
use crate::resolver::Reference;
use crate::util::resolve_under_root;

mod cloud_init;
mod expressions;

#[cfg(test)]
mod tests;

use self::cloud_init::{
    merge_cloud_init_mappings, validate_cloud_init_fragment, validate_cloud_init_schema_fragment,
};
use self::expressions::{
    expand_raw_step, extract_fragment_input_declarations, resolve_fragment_inputs,
    substitute_inputs_in_value,
};
use crate::assert::{validate_assert_block, AssertBlock};
use crate::plan::files::FileEntry;
use crate::step::{deserialize_optional_positive_seconds, resolve_shell, StepTarget, TestStep};

/// Maximum number of active `uses:` includes on the call stack at any one time.
/// Includes the root document, which is always on the stack; so this limits nesting
/// to `MAX_INCLUDE_DEPTH - 1` fragment levels below the root.
pub(super) const MAX_INCLUDE_DEPTH: usize = 32;

/// The kind of a botforge YAML document, specified by the required `type:` field.
///
/// Every document must carry exactly one `type:` discriminator.  The loader
/// dispatches on it to enforce command-boundary separation and per-kind presence
/// rules.
#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Eq)]
enum DocumentType {
    /// An entrypoint document consumed directly by `botforge test`.
    #[serde(rename = "botforge/test")]
    Test,
    /// An entrypoint document consumed directly by `botforge build`.
    #[serde(rename = "botforge/build")]
    Build,
    /// A reusable document spliced in via `uses:`.  May not carry
    /// entrypoint-only sections (`ports:`, `isos:`, `diagnostics_units:`,
    /// `disk_size:`, `memsize:`, `smp:`).
    #[serde(rename = "botforge/fragment")]
    Fragment,
    /// An entrypoint document consumed directly by `botforge publish`.
    #[serde(rename = "botforge/publish")]
    Publish,
}

impl DocumentType {
    fn as_str(self) -> &'static str {
        match self {
            DocumentType::Test => "botforge/test",
            DocumentType::Build => "botforge/build",
            DocumentType::Fragment => "botforge/fragment",
            DocumentType::Publish => "botforge/publish",
        }
    }

    /// Returns `true` if this kind is the expected entrypoint for `botforge test`.
    fn is_test_entrypoint(self) -> bool {
        matches!(self, DocumentType::Test)
    }

    /// Returns `true` if this kind is the expected entrypoint for `botforge build`.
    fn is_build_entrypoint(self) -> bool {
        matches!(self, DocumentType::Build)
    }

    /// Returns `true` if this kind can be consumed via a `uses:` reference.
    fn is_consumable_fragment(self) -> bool {
        matches!(self, DocumentType::Fragment)
    }

    /// Returns `true` if this kind is the expected entrypoint for `botforge publish`.
    fn is_publish_entrypoint(self) -> bool {
        matches!(self, DocumentType::Publish)
    }
}

#[derive(Debug, Deserialize, Default)]
pub(crate) struct TestConfig {
    #[serde(default)]
    #[allow(dead_code)]
    pub(crate) name: String,
    #[serde(default)]
    pub(crate) image: Option<Reference>,
    #[serde(default)]
    pub(crate) isos: Vec<TestIso>,
    #[serde(default)]
    pub(crate) ports: Vec<PortSpec>,
    #[serde(default)]
    pub(crate) steps: Vec<TestStep>,
    #[serde(default, alias = "files")]
    pub(crate) files: Vec<FileEntry>,
    #[serde(default)]
    pub(crate) diagnostics_units: Vec<String>,
    #[serde(
        default = "default_test_step_timeout",
        deserialize_with = "deserialize_positive_seconds"
    )]
    pub(crate) step_timeout: u64,
    #[serde(
        default = "default_test_timeout",
        deserialize_with = "deserialize_positive_seconds"
    )]
    pub(crate) timeout: u64,
    #[serde(skip, default = "default_test_cloud_init_timeout")]
    pub(crate) cloud_init_timeout: u64,
    /// Merged cloud-config fragment for the runner VM seed.  Accumulated from
    /// the root document and any `uses:` fragment includes.
    pub(crate) cloud_init: Option<serde_yaml::Mapping>,
    /// Declarative assertions checked as a pre-steps phase (after boot/SSH/cloud-init,
    /// before the first `steps:` entry).
    #[serde(default)]
    pub(crate) assert: Option<AssertBlock>,
}

/// Raw deserialization target for a top-level `botforge test` document.
/// The `type:` field is required; parsing fails with a descriptive error when it
/// is absent or carries an unrecognised value.
#[derive(Debug, Deserialize)]
struct RawTestDocument {
    #[serde(rename = "type")]
    doc_type: DocumentType,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    image: Option<String>,
    #[serde(default)]
    isos: Vec<TestIso>,
    #[serde(default)]
    ports: Vec<PortSpec>,
    #[serde(default)]
    steps: Vec<RawTestStep>,
    #[serde(default, alias = "files")]
    files: Vec<FileEntry>,
    #[serde(default)]
    diagnostics_units: Vec<String>,
    #[serde(
        default = "default_test_step_timeout",
        deserialize_with = "deserialize_positive_seconds"
    )]
    step_timeout: u64,
    #[serde(
        default = "default_test_timeout",
        deserialize_with = "deserialize_positive_seconds"
    )]
    timeout: u64,
    /// Optional cloud-config fragment to merge into the runner VM seed.
    #[serde(default)]
    cloud_init: Option<serde_yaml::Mapping>,
    /// Declarative assertions to run as a pre-steps phase (before `steps:`).
    #[serde(default)]
    assert: Option<AssertBlock>,
}

fn default_disk_size() -> String {
    "10G".to_string()
}

fn default_test_step_timeout() -> u64 {
    300
}

fn default_build_step_timeout() -> u64 {
    1800
}

fn default_test_timeout() -> u64 {
    1800
}

fn default_build_timeout() -> u64 {
    7200
}

fn default_test_cloud_init_timeout() -> u64 {
    300
}

fn default_build_cloud_init_timeout() -> u64 {
    600
}

fn parse_config_image(raw: &str) -> Result<Reference> {
    Reference::parse(raw)
}

use crate::compress::CompressConfig;

/// Resolved configuration for a `botforge build` run.
#[derive(Debug)]
pub(crate) struct BuildConfig {
    #[allow(dead_code)]
    pub(crate) name: String,
    /// Parsed `image:` reference naming the source qcow2 to boot from.
    pub(crate) image: Reference,
    /// Declared artifact filename (no directories). The output directory is derived from
    /// the build spec path under the repo root.
    pub(crate) output: String,
    pub(crate) disk_size: String,
    pub(crate) steps: Vec<TestStep>,
    pub(crate) files: Vec<FileEntry>,
    pub(crate) step_timeout: u64,
    pub(crate) timeout: u64,
    pub(crate) cloud_init_timeout: u64,
    /// Optional cloud-config fragment to merge into the runner VM seed.
    /// Accumulated from the root document and any `uses:` fragment includes.
    pub(crate) cloud_init: Option<serde_yaml::Mapping>,
    /// Optional output compression.  `None` (or `Some { enabled: false }`) ⇒
    /// plain atomic rename, byte-identical to existing behaviour.
    pub(crate) compress: Option<CompressConfig>,
}

/// Raw deserialization target for a top-level `botforge build` document.
/// The `type:` field is required and must be `build`.
#[derive(Debug, Deserialize)]
struct RawBuildDocument {
    #[serde(rename = "type")]
    doc_type: DocumentType,
    #[serde(default)]
    name: Option<String>,
    /// Raw `image:` reference (e.g. `@debian-base`). Required.
    #[serde(rename = "image", default)]
    image: Option<String>,
    #[serde(default)]
    output: Option<String>,
    #[serde(default = "default_disk_size")]
    disk_size: String,
    #[serde(default)]
    steps: Vec<RawTestStep>,
    #[serde(default, alias = "files")]
    files: Vec<FileEntry>,
    #[serde(
        default = "default_build_step_timeout",
        deserialize_with = "deserialize_positive_seconds"
    )]
    step_timeout: u64,
    #[serde(
        default = "default_build_timeout",
        deserialize_with = "deserialize_positive_seconds"
    )]
    timeout: u64,
    /// Optional cloud-config fragment to merge into the runner VM seed.
    /// The value is an arbitrary cloud-config mapping; unknown keys are passed
    /// through to cloud-init.
    #[serde(default)]
    cloud_init: Option<serde_yaml::Mapping>,
    /// Optional output-compression config.  Absent ⇒ plain atomic rename.
    #[serde(default)]
    compress: Option<CompressConfig>,
}

#[derive(Debug, Deserialize)]
struct RawTestStepFragment {
    #[serde(default)]
    steps: Vec<RawTestStep>,
    #[serde(default, alias = "files")]
    files: Vec<FileEntry>,
    /// Optional cloud-config fragment contributed by this `type: botforge/fragment` document.
    /// Deep-merged with the parent's cloud_init under the same precedence rules.
    #[serde(default)]
    cloud_init: Option<serde_yaml::Mapping>,
}

fn deserialize_positive_seconds<'de, D>(deserializer: D) -> std::result::Result<u64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    deserialize_optional_positive_seconds(deserializer)?
        .ok_or_else(|| serde::de::Error::custom("expected a positive integer number of seconds"))
}

#[derive(Debug)]
enum RawTestStep {
    Step(Value),
    Include(TestStepInclude),
}

impl<'de> Deserialize<'de> for RawTestStep {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = Value::deserialize(deserializer)?;
        if let Value::Mapping(mapping) = &value {
            if mapping.contains_key(Value::String("uses".to_string())) {
                return serde_yaml::from_value::<TestStepInclude>(value)
                    .map(Self::Include)
                    .map_err(de::Error::custom);
            }
        }
        Ok(Self::Step(value))
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TestStepInclude {
    uses: String,
    #[serde(default)]
    with: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub(crate) enum TestIso {
    Attach(PathBuf),
    Bootstrap {
        path: PathBuf,
        label: String,
        mount: PathBuf,
        #[serde(default = "default_bootstrap_path")]
        bootstrap: PathBuf,
    },
}

pub(crate) struct TestIsoBootstrap {
    pub(crate) label: String,
    pub(crate) mount: PathBuf,
    pub(crate) bootstrap: PathBuf,
}

pub(super) fn default_bootstrap_path() -> PathBuf {
    PathBuf::from("bootstrap.sh")
}

pub(crate) fn load_test_config(repo_root: &Path, path: &Path) -> Result<TestConfig> {
    let yaml = std::fs::read_to_string(path)
        .with_context(|| format!("cannot read test config: {}", path.display()))?;
    let value: Value = serde_yaml::from_str(&yaml)
        .with_context(|| format!("invalid test config: {}", path.display()))?;
    check_no_build_sections_in_test_doc(path, &value)?;
    check_no_top_level_bootcmd(path, &value)?;
    let raw: RawTestDocument = serde_yaml::from_value(value)
        .with_context(|| format!("invalid test config: {}", path.display()))?;
    if !raw.doc_type.is_test_entrypoint() {
        anyhow::bail!(
            "botforge test requires a 'type: botforge/test' document, got 'type: {}'",
            raw.doc_type.as_str()
        );
    }
    let name = validate_entrypoint_name(raw.name, path, DocumentType::Test)?;
    let root_cloud_init = raw.cloud_init.clone();
    // Seed the stack with the root document so that a fragment including the root
    // is caught by the cycle check (A → B → A).
    let mut include_stack = vec![path.to_path_buf()];
    let mut cloud_init_acc = raw.cloud_init;
    let mut files_acc = Vec::new();
    let config = TestConfig {
        name,
        image: match raw.image {
            None => None,
            Some(s) if s.trim().is_empty() => anyhow::bail!(
                "'image' in a 'type: botforge/test' document ({}) must not be blank",
                path.display()
            ),
            Some(s) => Some(parse_config_image(&s).with_context(|| {
                format!("invalid 'image' value in test config ({})", path.display())
            })?),
        },
        isos: raw.isos,
        ports: raw.ports,
        steps: expand_test_steps(
            repo_root,
            path,
            raw.steps,
            &mut include_stack,
            &mut cloud_init_acc,
            &mut files_acc,
        )?,
        files: {
            let mut files = raw.files;
            files.extend(files_acc);
            let files = dedupe_identical_files(files);
            validate_top_level_files("test", &files)
                .with_context(|| format!("invalid test config: {}", path.display()))?;
            files
        },
        diagnostics_units: raw.diagnostics_units,
        step_timeout: raw.step_timeout,
        timeout: raw.timeout,
        cloud_init_timeout: default_test_cloud_init_timeout(),
        cloud_init: cloud_init_acc,
        assert: {
            if let Some(ref block) = raw.assert {
                validate_assert_block(block)
                    .with_context(|| format!("invalid test config: {}", path.display()))?;
            }
            raw.assert
        },
    };
    run_semantic_validators(SemanticValidationTarget::Test {
        path,
        root_cloud_init: root_cloud_init.as_ref(),
        steps: &config.steps,
        ports: &config.ports,
        files: &config.files,
    })?;
    Ok(config)
}

pub(crate) fn load_build_config(repo_root: &Path, path: &Path) -> Result<BuildConfig> {
    let yaml = std::fs::read_to_string(path)
        .with_context(|| format!("cannot read build config: {}", path.display()))?;
    let value: Value = serde_yaml::from_str(&yaml)
        .with_context(|| format!("invalid build config: {}", path.display()))?;
    check_no_test_entrypoint_sections_in_build_doc(path, &value)?;
    check_no_top_level_bootcmd(path, &value)?;
    let raw: RawBuildDocument = serde_yaml::from_value(value)
        .with_context(|| format!("invalid build config: {}", path.display()))?;
    if !raw.doc_type.is_build_entrypoint() {
        anyhow::bail!(
            "botforge build requires a 'type: botforge/build' document, got 'type: {}'",
            raw.doc_type.as_str()
        );
    }
    let name = validate_entrypoint_name(raw.name, path, DocumentType::Build)?;
    let image = match raw.image {
        None => anyhow::bail!(
            "'image' is required in a 'type: botforge/build' document ({}): \
             set it to an `@…` reference, e.g. `image: \"@debian-base\"`",
            path.display()
        ),
        Some(s) if s.trim().is_empty() => anyhow::bail!(
            "'image' is required in a 'type: botforge/build' document ({}): \
             set it to an `@…` reference, e.g. `image: \"@debian-base\"`",
            path.display()
        ),
        Some(s) => parse_config_image(&s).with_context(|| {
            format!("invalid 'image' value in build config ({})", path.display())
        })?,
    };
    let output = match raw.output {
        None => anyhow::bail!(
            "'output' is required in a 'type: botforge/build' document ({}): \
             set it to a bare artifact filename, e.g. `output: \"image.qcow2\"`",
            path.display()
        ),
        Some(s) if s.trim().is_empty() => anyhow::bail!(
            "'output' is required in a 'type: botforge/build' document ({}): \
             set it to a bare artifact filename, e.g. `output: \"image.qcow2\"`",
            path.display()
        ),
        Some(s) => s,
    };
    let root_cloud_init = raw.cloud_init.clone();
    let mut include_stack = vec![path.to_path_buf()];
    let mut cloud_init_acc = raw.cloud_init;
    let mut files_acc = Vec::new();
    let config = BuildConfig {
        name,
        image,
        output,
        disk_size: raw.disk_size,
        steps: expand_test_steps(
            repo_root,
            path,
            raw.steps,
            &mut include_stack,
            &mut cloud_init_acc,
            &mut files_acc,
        )?,
        files: {
            let mut files = raw.files;
            files.extend(files_acc);
            let files = dedupe_identical_files(files);
            validate_top_level_files("build", &files)
                .with_context(|| format!("invalid build config: {}", path.display()))?;
            files
        },
        step_timeout: raw.step_timeout,
        timeout: raw.timeout,
        cloud_init_timeout: default_build_cloud_init_timeout(),
        cloud_init: cloud_init_acc,
        compress: raw.compress,
    };
    run_semantic_validators(SemanticValidationTarget::Build {
        path,
        output: &config.output,
        root_cloud_init: root_cloud_init.as_ref(),
        steps: &config.steps,
        files: &config.files,
    })?;
    Ok(config)
}

// ─── publish config ───────────────────────────────────────────────────────────

/// Filesystem target block in a `type: botforge/publish` document.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawFsTarget {
    /// Source: an `@`-notation reference, e.g. `@artifact://images/vm.qcow2`.
    src: String,
    /// Destination directory on the local filesystem.  Created if absent.
    dest: String,
}

/// S3 target block in a `type: botforge/publish` document.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawS3Target {
    /// Source: an `@`-notation reference, e.g. `@artifact://images/vm.qcow2`.
    src: String,
    /// S3 destination URL, e.g. `s3://my-bucket/releases/`.
    /// Credentials are read from the environment
    /// (`AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, `AWS_DEFAULT_REGION`,
    /// and optionally `AWS_ENDPOINT_URL` for S3-compatible services).
    dest: String,
}

/// Raw deserialization target for a top-level `botforge publish` document.
///
/// `deny_unknown_fields` ensures that unrecognised target blocks (e.g.
/// `github:`, typo'd `s3x:`) produce a clear parse-time error.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPublishDocument {
    #[serde(rename = "type")]
    doc_type: DocumentType,
    #[serde(default)]
    name: Option<String>,
    /// Zero or more filesystem targets.
    #[serde(default)]
    fs: Vec<RawFsTarget>,
    /// Zero or more S3 targets.
    #[serde(default)]
    s3: Vec<RawS3Target>,
}

/// Filesystem publish target.
#[derive(Debug)]
pub(crate) struct FsTarget {
    /// Resolved `@`-reference string (as written in the YAML).
    pub(crate) src: String,
    /// Local destination directory (a plain filesystem path; not `@`-resolved).
    pub(crate) dest: String,
}

/// S3 publish target.
#[derive(Debug)]
pub(crate) struct S3Target {
    /// Resolved `@`-reference string (as written in the YAML).
    pub(crate) src: String,
    /// S3 destination URL (`s3://bucket/prefix`).
    pub(crate) dest: String,
}

/// Validated publish plan loaded from a `type: botforge/publish` document.
///
/// ## Schema contract
///
/// - Each target kind (`fs`, `s3`) is a **list of instances**.  Multiple
///   destinations of the same kind are expressed as multiple list entries.
/// - Publish targets are **unordered** and MAY run in parallel; plans MUST NOT
///   assume any ordering within a kind's list or across kinds.  The current
///   implementation runs them serially, but the iteration order is an
///   implementation detail that plans must not depend on.
/// - All ordered / pre-publish work (path mangling, versioning, checksums, etc.)
///   is deferred to a future `steps:` prepare phase (not yet implemented).
#[derive(Debug)]
pub(crate) struct PublishConfig {
    #[allow(dead_code)]
    pub(crate) name: String,
    /// Filesystem targets (may be empty).
    pub(crate) fs: Vec<FsTarget>,
    /// S3 targets (may be empty; credentials from environment).
    pub(crate) s3: Vec<S3Target>,
}

/// Load and validate a `type: botforge/publish` document from `path`.
pub(crate) fn load_publish_config(path: &Path) -> Result<PublishConfig> {
    let yaml = std::fs::read_to_string(path)
        .with_context(|| format!("cannot read publish config: {}", path.display()))?;
    let raw: RawPublishDocument = serde_yaml::from_str(&yaml)
        .with_context(|| format!("invalid publish config: {}", path.display()))?;
    if !raw.doc_type.is_publish_entrypoint() {
        anyhow::bail!(
            "botforge publish requires a 'type: botforge/publish' document, got 'type: {}'",
            raw.doc_type.as_str()
        );
    }
    let name = validate_entrypoint_name(raw.name, path, DocumentType::Publish)?;

    let fs: Vec<FsTarget> = raw
        .fs
        .into_iter()
        .enumerate()
        .map(|(i, t)| {
            validate_publish_src(&t.src, path, &format!("fs[{i}]"))?;
            Ok(FsTarget {
                src: t.src,
                dest: t.dest,
            })
        })
        .collect::<Result<_>>()?;

    let s3: Vec<S3Target> = raw
        .s3
        .into_iter()
        .enumerate()
        .map(|(i, t)| {
            validate_publish_src(&t.src, path, &format!("s3[{i}]"))?;
            validate_s3_dest(&t.dest, path)?;
            Ok(S3Target {
                src: t.src,
                dest: t.dest,
            })
        })
        .collect::<Result<_>>()?;

    if fs.is_empty() && s3.is_empty() {
        anyhow::bail!(
            "publish plan '{}' ({}) has no targets; \
             add at least one 'fs' or 's3' entry",
            name,
            path.display()
        );
    }

    Ok(PublishConfig { name, fs, s3 })
}

/// Assert that a publish `src` value is an `@`-reference.
fn validate_publish_src(src: &str, path: &Path, target: &str) -> Result<()> {
    if !src.starts_with('@') {
        anyhow::bail!(
            "publish {target}.src must be an @-reference (e.g. @artifact://...), \
             got '{src}' in {}",
            path.display()
        );
    }
    crate::resolver::Reference::parse(src).with_context(|| {
        format!(
            "invalid {target}.src reference '{src}' in {}",
            path.display()
        )
    })?;
    Ok(())
}

/// Assert that a publish `s3.dest` value starts with `s3://`.
fn validate_s3_dest(dest: &str, path: &Path) -> Result<()> {
    if !dest.starts_with("s3://") {
        anyhow::bail!(
            "publish s3.dest must be an S3 URL starting with 's3://', \
             got '{dest}' in {}",
            path.display()
        );
    }
    Ok(())
}

fn validate_entrypoint_name(
    raw_name: Option<String>,
    path: &Path,
    doc_type: DocumentType,
) -> Result<String> {
    let name = match raw_name {
        None => anyhow::bail!(
            "'name' is required in a 'type: {}' document ({})",
            doc_type.as_str(),
            path.display()
        ),
        Some(name) => name,
    };
    if name.trim().is_empty() {
        anyhow::bail!(
            "'name' is required in a 'type: {}' document ({})",
            doc_type.as_str(),
            path.display()
        );
    }
    if !name.is_ascii() || name.chars().any(|c| c.is_ascii_control()) {
        anyhow::bail!(
            "'name' in a 'type: {}' document ({}) must be printable ASCII",
            doc_type.as_str(),
            path.display()
        );
    }
    // NOTE: uniqueness is intentionally deferred; it will be enforced per-type
    // during the later discovery/registry work.
    Ok(name)
}

fn validate_build_output_filename(output: &str) -> Result<&str> {
    use std::path::{Component, Path};

    let path = Path::new(output);
    if path.is_absolute() {
        anyhow::bail!("output filename must be a bare filename, got absolute path");
    }
    let mut saw_normal = false;
    for component in path.components() {
        match component {
            Component::Normal(_) => saw_normal = true,
            Component::CurDir
            | Component::ParentDir
            | Component::RootDir
            | Component::Prefix(_) => {
                anyhow::bail!(
                    "output filename must be a bare filename with no path segments, got '{}'",
                    output
                );
            }
        }
    }
    if !saw_normal || path.file_name().is_none() || output.contains(std::path::MAIN_SEPARATOR) {
        anyhow::bail!(
            "output filename must be a bare filename with no path segments, got '{}'",
            output
        );
    }
    if output.contains('/') || output.contains('\\') {
        anyhow::bail!(
            "output filename must be a bare filename with no path segments, got '{}'",
            output
        );
    }
    Ok(output)
}

enum SemanticValidationTarget<'a> {
    Build {
        path: &'a Path,
        output: &'a str,
        root_cloud_init: Option<&'a serde_yaml::Mapping>,
        steps: &'a [TestStep],
        files: &'a [FileEntry],
    },
    Test {
        path: &'a Path,
        root_cloud_init: Option<&'a serde_yaml::Mapping>,
        steps: &'a [TestStep],
        ports: &'a [PortSpec],
        files: &'a [FileEntry],
    },
}

fn run_semantic_validators(target: SemanticValidationTarget<'_>) -> Result<()> {
    match target {
        SemanticValidationTarget::Build {
            path,
            output,
            root_cloud_init,
            steps,
            files,
        } => {
            validate_build_output_filename(output).with_context(|| {
                format!(
                    "invalid 'output' value in build config ({})",
                    path.display()
                )
            })?;
            if let Some(ci) = root_cloud_init {
                validate_cloud_init_fragment(ci, path)?;
                validate_cloud_init_schema_fragment(ci, path)?;
            }
            validate_top_level_files("build", files)
                .with_context(|| format!("invalid build config: {}", path.display()))?;
            validate_build_steps(steps)
        }
        SemanticValidationTarget::Test {
            path,
            root_cloud_init,
            steps,
            ports,
            files,
        } => {
            if let Some(ci) = root_cloud_init {
                validate_cloud_init_fragment(ci, path)?;
                validate_cloud_init_schema_fragment(ci, path)?;
            }
            validate_top_level_files("test", files)
                .with_context(|| format!("invalid test config: {}", path.display()))?;
            validate_test_steps(steps, ports)
        }
    }
}

fn expand_test_steps(
    repo_root: &Path,
    current_file: &Path,
    steps: Vec<RawTestStep>,
    include_stack: &mut Vec<PathBuf>,
    cloud_init_acc: &mut Option<serde_yaml::Mapping>,
    files_acc: &mut Vec<FileEntry>,
) -> Result<Vec<TestStep>> {
    let mut expanded = Vec::new();
    for step in steps {
        match step {
            RawTestStep::Step(step) => expanded.extend(
                expand_raw_step(step)
                    .with_context(|| format!("invalid step in {}", current_file.display()))?,
            ),
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
                if include_stack.len() >= MAX_INCLUDE_DEPTH {
                    let mut chain: Vec<String> = include_stack
                        .iter()
                        .map(|path| path.display().to_string())
                        .collect();
                    chain.push(include_path.display().to_string());
                    anyhow::bail!(
                        "test step include depth limit ({}) exceeded: {}",
                        MAX_INCLUDE_DEPTH,
                        chain.join(" -> ")
                    );
                }
                include_stack.push(include_path.clone());
                let result = load_test_steps_fragment(&include_path, &include.uses, &include.with)
                    .and_then(|(steps, ci, files)| {
                        files_acc.extend(files);
                        let mut ci_acc = ci;
                        let expanded_steps = expand_test_steps(
                            repo_root,
                            &include_path,
                            steps,
                            include_stack,
                            &mut ci_acc,
                            files_acc,
                        )?;
                        Ok((expanded_steps, ci_acc))
                    });
                include_stack.pop();
                let (fragment_steps, fragment_cloud_init) = result?;
                expanded.extend(fragment_steps);
                // Merge fragment's cloud_init into accumulator.
                if let Some(fci) = fragment_cloud_init {
                    *cloud_init_acc = Some(match cloud_init_acc.take() {
                        None => fci,
                        Some(base) => merge_cloud_init_mappings(base, fci),
                    });
                }
            }
        }
    }
    Ok(expanded)
}

fn load_test_steps_fragment(
    path: &Path,
    uses: &str,
    with: &BTreeMap<String, String>,
) -> Result<(
    Vec<RawTestStep>,
    Option<serde_yaml::Mapping>,
    Vec<FileEntry>,
)> {
    let yaml = std::fs::read_to_string(path)
        .with_context(|| format!("cannot read test step include: {}", path.display()))?;
    let mut value: Value = serde_yaml::from_str(&yaml)
        .with_context(|| format!("invalid test step include: {}", path.display()))?;
    if value.is_sequence() {
        return Err(anyhow::anyhow!(
            "test step include must be a mapping with a 'steps:' key"
        ))
        .with_context(|| format!("invalid test step include: {}", path.display()));
    }
    // Enforce `type: botforge/fragment` — entrypoint documents must not be used as fragments.
    check_fragment_document_type(uses, &value)?;
    // Entrypoint-only sections are not valid in fragment documents.
    check_no_entrypoint_sections_in_fragment(path, &value)?;
    let declarations = extract_fragment_input_declarations(&value)
        .with_context(|| format!("invalid test step include: {}", path.display()))?;
    let resolved = resolve_fragment_inputs(path, &declarations, with)
        .with_context(|| format!("invalid test step include: {}", path.display()))?;
    substitute_inputs_in_value(&mut value, &resolved)
        .with_context(|| format!("invalid test step include: {}", path.display()))?;
    let fragment: RawTestStepFragment = serde_yaml::from_value(value)
        .with_context(|| format!("invalid test step include: {}", path.display()))?;
    if let Some(ref ci) = fragment.cloud_init {
        validate_cloud_init_fragment(ci, path)?;
    }
    Ok((fragment.steps, fragment.cloud_init, fragment.files))
}

/// Verify that a `uses:` target is a `type: botforge/fragment` document.
///
/// A missing `type:` field or a non-fragment kind (e.g. `type: botforge/test`)
/// is a hard
/// load-time error.  The `uses` string (the original `@://...` value) is used in
/// the error message so the caller can pinpoint the offending include.
fn check_fragment_document_type(uses: &str, value: &Value) -> Result<()> {
    let mapping = match value {
        Value::Mapping(m) => m,
        _ => return Ok(()),
    };
    let type_key = Value::String("type".to_string());
    match mapping.get(&type_key) {
        None => anyhow::bail!("{} is missing required 'type:' field", uses),
        Some(Value::String(t)) => {
            // Deserialize the type string as a DocumentType so that the generic
            // `is_consumable_fragment` predicate drives the decision — adding a new
            // entrypoint kind (`build`) later does not require touching this check.
            match serde_yaml::from_str::<DocumentType>(t) {
                Ok(doc_type) if doc_type.is_consumable_fragment() => Ok(()),
                Ok(doc_type) => anyhow::bail!(
                    "{} is not a consumable fragment (type: {})",
                    uses,
                    doc_type.as_str()
                ),
                Err(_) => anyhow::bail!("{} is not a consumable fragment (type: {})", uses, t),
            }
        }
        Some(_) => anyhow::bail!("{}: 'type:' field must be a string", uses),
    }
}

/// Reject entrypoint-only sections (`ports:`, `isos:`, `diagnostics_units:`,
/// `disk_size:`, `memsize:`, `smp:`, `step_timeout:`, `timeout:`, `image:`,
/// `output:`, `compress:`, `name:`) inside a `type: botforge/fragment` document.
/// Serde would silently ignore them; this turns a misplaced key into an explicit
/// load-time error.
fn check_no_entrypoint_sections_in_fragment(path: &Path, value: &Value) -> Result<()> {
    let mapping = match value {
        Value::Mapping(m) => m,
        _ => return Ok(()),
    };
    for section in &[
        "ports",
        "isos",
        "diagnostics_units",
        "disk_size",
        "memsize",
        "smp",
        "step_timeout",
        "timeout",
        "image",
        "output",
        "compress",
        "name",
    ] {
        if mapping.contains_key(Value::String(section.to_string())) {
            anyhow::bail!(
                "{}: is not valid in a 'type: botforge/fragment' document ({})",
                section,
                path.display()
            );
        }
    }
    Ok(())
}

/// Reject build-only sections (`disk_size:`, `memsize:`, `smp:`,
/// `output:`, `compress:`) inside a
/// `type: botforge/test` document.  Serde would silently ignore them; this turns a
/// misplaced key into an explicit load-time error.
fn check_no_build_sections_in_test_doc(path: &Path, value: &Value) -> Result<()> {
    let mapping = match value {
        Value::Mapping(m) => m,
        _ => return Ok(()),
    };
    for section in &["disk_size", "memsize", "smp", "output", "compress"] {
        if mapping.contains_key(Value::String(section.to_string())) {
            anyhow::bail!(
                "{}: is not valid in a 'type: botforge/test' document ({})",
                section,
                path.display()
            );
        }
    }
    Ok(())
}

/// Reject test-entrypoint-only sections (`ports:`, `isos:`, `diagnostics_units:`)
/// and runner-resource keys (`memsize:`, `smp:`) inside a
/// `type: botforge/build` document.
/// Serde would silently ignore them; this turns a misplaced key into an explicit
/// load-time error.
fn check_no_test_entrypoint_sections_in_build_doc(path: &Path, value: &Value) -> Result<()> {
    let mapping = match value {
        Value::Mapping(m) => m,
        _ => return Ok(()),
    };
    for section in &["ports", "isos", "diagnostics_units", "memsize", "smp"] {
        if mapping.contains_key(Value::String(section.to_string())) {
            anyhow::bail!(
                "{}: is not valid in a 'type: botforge/build' document ({})",
                section,
                path.display()
            );
        }
    }
    Ok(())
}

/// Hard-reject a top-level `bootcmd:` key and emit a clear migration error
/// pointing at `cloud_init.bootcmd`.
///
/// `bootcmd:` was removed as a top-level key; callers must migrate:
///
/// ```yaml
/// # before
/// bootcmd:
///   - echo hello
///
/// # after
/// cloud_init:
///   bootcmd:
///     - echo hello
/// ```
fn check_no_top_level_bootcmd(path: &Path, value: &Value) -> Result<()> {
    let mapping = match value {
        Value::Mapping(m) => m,
        _ => return Ok(()),
    };
    if mapping.contains_key(Value::String("bootcmd".to_string())) {
        anyhow::bail!(
            "top-level 'bootcmd:' is no longer supported ({}): \
             migrate to 'cloud_init:\\n  bootcmd:' instead",
            path.display()
        );
    }
    Ok(())
}

fn resolve_uses_path(repo_root: &Path, uses: &str) -> Result<PathBuf> {
    use crate::resolver::Reference;

    // Preserve backward-compatible error messages for schemes like `file://`
    // that look plausible but are not the `@` reference grammar.
    if !uses.starts_with('@') {
        if let Some((scheme, _)) = uses.split_once("://") {
            anyhow::bail!(
                "unsupported uses scheme '{scheme}' in '{uses}'; only @://<path> is supported"
            );
        }
        anyhow::bail!("invalid uses value '{uses}': expected @://<path>");
    }

    // Parse as a Reference; path-validation errors (e.g. `..` segments, absolute
    // paths) propagate via context so the caller sees the original diagnostic.
    let reference =
        Reference::parse(uses).with_context(|| format!("invalid uses value '{uses}'"))?;

    match reference {
        Reference::Repo { path: Some(p) } => Ok(resolve_under_root(repo_root, p)),
        Reference::Repo { path: None } => {
            anyhow::bail!("invalid uses value '{uses}': expected @://<path>")
        }
        _ => anyhow::bail!("unsupported uses scheme in '{uses}'; only @://<path> is supported"),
    }
}

fn validate_top_level_files(kind: &str, files: &[FileEntry]) -> Result<()> {
    for file in files {
        validate_top_level_file(kind, file)?;
    }
    Ok(())
}

fn dedupe_identical_files(files: Vec<FileEntry>) -> Vec<FileEntry> {
    let mut deduped = Vec::with_capacity(files.len());
    for file in files {
        if !deduped.iter().any(|existing| existing == &file) {
            deduped.push(file);
        }
    }
    deduped
}

/// Validate a `mode` string: must be 3–4 octal digits (same rule as `payload.rs`).
pub(crate) fn validate_mode_string(mode: &str, src: &str, kind: &str) -> Result<()> {
    if mode.len() < 3 || mode.len() > 4 || !mode.chars().all(|ch| ('0'..='7').contains(&ch)) {
        anyhow::bail!("{kind} files entry '{src}': `mode` must be 3–4 octal digits, got '{mode}'");
    }
    Ok(())
}

/// Validate an `owner` or `group` string: non-empty, no whitespace, no `/`, no shell metacharacters.
pub(crate) fn validate_owner_group_string(
    value: &str,
    field: &str,
    src: &str,
    kind: &str,
) -> Result<()> {
    if value.trim().is_empty() {
        anyhow::bail!("{kind} files entry '{src}': `{field}` must be non-empty");
    }
    for ch in value.chars() {
        if ch.is_whitespace()
            || matches!(
                ch,
                '/' | '\'' | '"' | '`' | '$' | ';' | '&' | '|' | '<' | '>'
            )
        {
            anyhow::bail!(
                "{kind} files entry '{src}': `{field}` contains invalid character '{ch}'; \
                 must not contain whitespace, '/', or shell metacharacters"
            );
        }
    }
    Ok(())
}

fn validate_top_level_file(kind: &str, file: &FileEntry) -> Result<()> {
    let src = file.src.trim();
    let dest = file.dest.trim();

    if src.is_empty() {
        anyhow::bail!("{kind} files entry: `src` is required and must be non-empty");
    }
    if !src.starts_with('@') {
        anyhow::bail!(
            "{kind} files entry '{src}': `src` must be an `@`-reference \
             (e.g. `@foo`, `@://<glob>`, `@artifact://<glob>`); bare paths are not supported"
        );
    }
    // Validate the reference syntax at config-load time so bad references are caught early.
    Reference::parse(src)
        .with_context(|| format!("{kind} files entry '{src}': invalid `@`-reference in `src`"))?;
    if dest.is_empty() {
        anyhow::bail!(
            "{kind} files entry '{src}': `dest` is required and must be a non-empty absolute path"
        );
    }
    if !dest.starts_with('/') {
        anyhow::bail!(
            "{kind} files entry '{src}': `dest` must be an absolute guest path (got '{dest}')"
        );
    }
    if src_has_glob_metacharacters(src) && !dest.ends_with('/') {
        anyhow::bail!(
            "{kind} files entry '{src}': glob `src` requires `dest` to be a directory path ending with '/'"
        );
    }
    if let Some(mode) = &file.mode {
        validate_mode_string(mode, src, kind)?;
    }
    if let Some(owner) = &file.owner {
        validate_owner_group_string(owner, "owner", src, kind)?;
    }
    if let Some(group) = &file.group {
        validate_owner_group_string(group, "group", src, kind)?;
    }
    Ok(())
}

pub(crate) fn src_has_glob_metacharacters(src: &str) -> bool {
    src.contains('*') || src.contains('?') || src.contains('[')
}

pub(crate) fn validate_test_ports(ports: &[PortSpec], ssh_port: u16) -> Result<()> {
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

pub(crate) fn validate_test_steps(steps: &[TestStep], ports: &[PortSpec]) -> Result<()> {
    for step in steps {
        match step {
            TestStep::Run(step) => {
                if step.target == StepTarget::Host && step.sudo == Some(true) {
                    anyhow::bail!(
                        "test step '{}': 'sudo: true' is only supported on 'on: guest' steps",
                        step.name
                    );
                }
                resolve_shell(step.shell.as_deref()).with_context(|| {
                    format!("test step '{}': invalid `shell:` value", step.name)
                })?;
            }
            TestStep::Archive(step) => {
                anyhow::bail!(
                    "test step '{}': `archive` steps are only supported in `type: botforge/build` documents",
                    step.archive
                        .name
                        .as_deref()
                        .unwrap_or(step.archive.src.as_str())
                );
            }
        }
    }
    let has_host_step = steps
        .iter()
        .any(|step| matches!(step, TestStep::Run(run) if run.target == StepTarget::Host));
    if has_host_step && ports.is_empty() {
        anyhow::bail!(
            "test config has `on: host` steps but no `ports:` are declared; \
             a host step reaches the guest only via forwarded ports"
        );
    }
    Ok(())
}

pub(crate) fn validate_build_steps(steps: &[TestStep]) -> Result<()> {
    for step in steps {
        match step {
            TestStep::Run(step) => {
                if step.target == StepTarget::Host && step.sudo == Some(true) {
                    anyhow::bail!(
                        "build step '{}': 'sudo: true' is only supported on 'on: guest' steps",
                        step.name
                    );
                }
                resolve_shell(step.shell.as_deref()).with_context(|| {
                    format!("build step '{}': invalid `shell:` value", step.name)
                })?;
            }
            TestStep::Archive(step) => validate_archive_build_step(step)?,
        }
    }
    Ok(())
}

fn validate_archive_build_step(step: &crate::step::ArchiveStep) -> Result<()> {
    use crate::resolver::Reference;
    use crate::step::StepTarget;
    let name = step
        .archive
        .name
        .as_deref()
        .unwrap_or(step.archive.src.as_str());
    let src = step.archive.src.trim();
    if src.is_empty() {
        anyhow::bail!("build step '{name}': archive `src` is required and must be non-empty");
    }
    let reference = Reference::parse(src)
        .map_err(|_| anyhow::anyhow!("build step '{name}': archive `src` must start with '@'"))?;
    match reference {
        Reference::Asset { path: None, .. } => {} // valid
        _ => {
            if src.contains("://") {
                anyhow::bail!(
                    "build step '{name}': archive `src` does not support '@://' traversal"
                );
            } else {
                anyhow::bail!(
                    "build step '{name}': archive `src` must include a shasset name after '@'"
                );
            }
        }
    }
    if step.run.is_some() {
        anyhow::bail!("build step '{name}': `run` is not valid on an `archive` step");
    }
    if step.shell.is_some() {
        anyhow::bail!("build step '{name}': `shell` is not valid on an `archive` step");
    }
    if step.timeout.is_some() {
        anyhow::bail!("build step '{name}': `timeout` is not valid on an `archive` step");
    }

    match step.target.as_ref() {
        None | Some(StepTarget::Host) => {
            // Host mode (default): dest is not valid.
            if step.archive.dest.is_some() {
                anyhow::bail!(
                    "build step '{name}': `dest` is only valid on `on: guest` archive steps; \
                     omit `dest` for host-mode extraction into the build directory"
                );
            }
        }
        Some(StepTarget::Guest) => {
            // Guest mode: dest is required and must be an absolute path.
            match step.archive.dest.as_deref() {
                None | Some("") => {
                    anyhow::bail!(
                        "build step '{name}': `on: guest` archive step requires `dest` \
                         (an absolute guest path to extract into)"
                    );
                }
                Some(dest) if dest.trim().is_empty() => {
                    anyhow::bail!(
                        "build step '{name}': `on: guest` archive step requires `dest` \
                         (an absolute guest path to extract into)"
                    );
                }
                Some(dest) if !dest.starts_with('/') => {
                    anyhow::bail!(
                        "build step '{name}': archive `dest` must be an absolute path (got '{dest}')"
                    );
                }
                Some(_) => {}
            }
        }
    }

    Ok(())
}
