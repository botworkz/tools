use anyhow::{Context, Result};
use serde::{de, Deserialize};
use serde_yaml::Value;
use std::collections::{BTreeMap, HashSet};
use std::fmt;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::qemu::PortSpec;
use crate::resolver::Reference;
use crate::util::{command_exists, create_temp_dir, resolve_under_root};

use super::files::FileEntry;
use super::step::{deserialize_optional_positive_seconds, resolve_shell, StepTarget, TestStep};

const DEFAULT_SENTINEL: &str = "__default__";

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
#[serde(rename_all = "lowercase")]
enum DocumentType {
    /// An entrypoint document consumed directly by `botforge test`.
    Test,
    /// An entrypoint document consumed directly by `botforge build`.
    Build,
    /// A reusable document spliced in via `uses:`.  May not carry
    /// entrypoint-only sections (`ports:`, `isos:`, `diagnostics_units:`,
    /// `disk_size:`, `memsize:`, `smp:`).
    Fragment,
}

impl DocumentType {
    fn as_str(self) -> &'static str {
        match self {
            DocumentType::Test => "test",
            DocumentType::Build => "build",
            DocumentType::Fragment => "fragment",
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
}

#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(super) enum InputType {
    String,
    Number,
    Boolean,
}

#[derive(Debug, Deserialize)]
pub(super) struct InputDeclaration {
    #[serde(rename = "type")]
    pub(super) input_type: InputType,
    #[serde(default)]
    pub(super) required: bool,
    pub(super) default: Option<String>,
}

// ---------------------------------------------------------------------------
// assert: block types
// ---------------------------------------------------------------------------

/// Expected file type for an `assert.files:` entry.
#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum AssertFileType {
    File,
    Directory,
    Symlink,
}

impl AssertFileType {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::File => "file",
            Self::Directory => "directory",
            Self::Symlink => "symlink",
        }
    }
}

impl fmt::Display for AssertFileType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

fn default_assert_exists() -> bool {
    true
}

/// A single file-existence/attribute expectation inside `assert.files:`.
///
/// When `exists: false`, all other attribute fields must be absent (rejected
/// at config-load time).
#[derive(Debug, Deserialize, Clone, PartialEq, Eq)]
pub(crate) struct AssertFile {
    /// When `false`, the path must not exist on the guest.  Defaults to `true`.
    #[serde(default = "default_assert_exists")]
    pub(crate) exists: bool,
    /// Expected file type (`file`, `directory`, or `symlink`).
    /// Only meaningful when `exists: true`.
    #[serde(default)]
    pub(crate) filetype: Option<AssertFileType>,
    /// Expected owning user name or numeric uid.
    /// Only meaningful when `exists: true`.
    #[serde(default)]
    pub(crate) owner: Option<String>,
    /// Expected owning group name or numeric gid.
    /// Only meaningful when `exists: true`.
    #[serde(default)]
    pub(crate) group: Option<String>,
    /// Expected permission mode (3–4 octal digits, e.g. `"0755"`).
    /// Only meaningful when `exists: true`.
    #[serde(default)]
    pub(crate) mode: Option<String>,
}

/// A single user expectation inside `assert.users:`.
///
/// When `exists: false`, the `shell` and `groups` fields must be absent
/// (rejected at config-load time).  The key may be an exact name **or** a
/// glob pattern (e.g. `botforge-*`); pattern negatives enumerate
/// `getent passwd` output and match against each user name.
#[derive(Debug, Deserialize, Clone, PartialEq, Eq)]
pub(crate) struct AssertUser {
    /// When `false`, the user must not exist on the guest.  Defaults to `true`.
    #[serde(default = "default_assert_exists")]
    pub(crate) exists: bool,
    /// Expected login shell (e.g. `/bin/bash`).
    /// Only meaningful when `exists: true`.
    #[serde(default)]
    pub(crate) shell: Option<String>,
    /// All listed groups must be present in the user's supplementary groups
    /// (checked via `id -nG <user>`).  Only meaningful when `exists: true`.
    #[serde(default)]
    pub(crate) groups: Vec<String>,
}

/// A single group expectation inside `assert.groups:`.
///
/// When `exists: false`, no other attribute fields are supported.
/// The key may be an exact name **or** a glob pattern.
#[derive(Debug, Deserialize, Clone, PartialEq, Eq)]
pub(crate) struct AssertGroup {
    /// When `false`, the group must not exist on the guest.  Defaults to `true`.
    #[serde(default = "default_assert_exists")]
    pub(crate) exists: bool,
}

/// A single package expectation inside `assert.packages:`.
///
/// The only supported attribute is `installed:`.  Unknown attributes are
/// rejected at config-load time via `#[serde(deny_unknown_fields)]`.
/// The key may be an exact package name **or** a glob pattern (e.g. `*-dev`).
#[derive(Debug, Deserialize, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct AssertPackage {
    /// When `true`, the package must be installed (`install ok installed` via
    /// `dpkg-query`).  When `false`, the package must not be installed.
    /// Defaults to `true`.
    #[serde(default = "default_assert_exists")]
    pub(crate) installed: bool,
}

/// Validated `assert:` block from a `type: test` document.
#[derive(Debug, Deserialize, Clone, Default, PartialEq, Eq)]
pub(crate) struct AssertBlock {
    /// Map of absolute guest path → file expectation.
    #[serde(default)]
    pub(crate) files: BTreeMap<String, AssertFile>,
    /// Map of user name (or glob pattern) → user expectation.
    #[serde(default)]
    pub(crate) users: BTreeMap<String, AssertUser>,
    /// Map of group name (or glob pattern) → group expectation.
    #[serde(default)]
    pub(crate) groups: BTreeMap<String, AssertGroup>,
    /// Map of package name (or glob pattern) → package expectation.
    #[serde(default)]
    pub(crate) packages: BTreeMap<String, AssertPackage>,
}

#[derive(Debug, Deserialize, Default)]
pub(crate) struct TestConfig {
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
    /// Declarative assertions checked as an implicit final phase after `steps:`.
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
    /// Declarative assertions to run as an implicit final phase.
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

/// Output-compression options for `botforge build`.
///
/// Modelled as an optional map with a required `enabled:` field so it can
/// carry additional knobs without changing shape.  The struct is kept strict
/// (`deny_unknown_fields`) to catch typos at parse time.
#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub(crate) enum ReclaimMode {
    /// Default — no reclaim step before commit.
    #[default]
    None,
    /// Run in-guest `fstrim` as the last guest action before shutdown.
    Fstrim,
    /// Run host-side offline reclaim via qemu-nbd discard+fstrim after shutdown.
    Discard,
}

#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub(crate) enum CompressionType {
    #[default]
    Zstd,
    Zlib,
}

/// Output-compression options for `botforge build`.
///
/// `reclaim` is nested under `compress` because it is primarily used to make
/// qcow2 compression effective by reclaiming freed guest blocks before commit.
/// `reclaim` still runs even when `enabled: false` (plain rename) so users can
/// reclaim space without compression.
///
/// ```yaml
/// # default off — plain atomic rename (byte-identical to today)
/// # compress: absent
///
/// # on, qemu default cluster size
/// compress:
///   enabled: true
///   # compressor defaults to zstd
///
/// # on, explicit options via compressor_args map
/// compress:
///   enabled: true
///   compressor: zstd
///   compressor_args:
///     cluster_size: "1M"
///   compressor_opts: "-19 -T0"
///
/// # reclaim freed blocks before commit/compress
/// compress:
///   enabled: true
///   reclaim: fstrim
///
/// # explicit off (equivalent to omitting the block)
/// compress:
///   enabled: false
/// ```
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CompressConfig {
    /// Whether compression is enabled.  Required — a `compress:` block without
    /// `enabled:` is a hard parse error.
    pub(crate) enabled: bool,
    /// Compression algorithm passed as `-o compression_type=<val>` to
    /// the native qcow2 compression writer.
    ///
    /// Defaults to `zstd`, which requires qemu >= 5.1 only on consumers that
    /// open the produced qcow2.
    #[serde(default)]
    pub(crate) compressor: CompressionType,
    /// Optional qcow2-structural key=value options interpreted by botforge's
    /// native qcow2 writer. Keys are sorted (BTreeMap) so the stored config is
    /// deterministic.
    ///
    /// Example: `{cluster_size: "1M"}` changes the target qcow2 cluster size.
    #[serde(default)]
    pub(crate) compressor_args: std::collections::BTreeMap<String, String>,
    /// Optional raw codec options string passed to the selected in-process
    /// compressor implementation, which parses and validates it.
    #[serde(default)]
    pub(crate) compressor_opts: String,
    /// Optional reclaim mode that runs before commit/compress.
    ///
    /// Defaults to `none`. Runs even when `enabled: false`.
    #[serde(default)]
    pub(crate) reclaim: ReclaimMode,
}

/// Resolved configuration for a `botforge build` run.
#[derive(Debug)]
pub(crate) struct BuildConfig {
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
    /// Optional cloud-config fragment contributed by this `type: fragment` document.
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
    Step(TestStep),
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
        serde_yaml::from_value::<TestStep>(value)
            .map(Self::Step)
            .map_err(de::Error::custom)
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
            "botforge test requires a 'type: test' document, got 'type: {}'",
            raw.doc_type.as_str()
        );
    }
    let root_cloud_init = raw.cloud_init.clone();
    // Seed the stack with the root document so that a fragment including the root
    // is caught by the cycle check (A → B → A).
    let mut include_stack = vec![path.to_path_buf()];
    let mut cloud_init_acc = raw.cloud_init;
    let config = TestConfig {
        image: match raw.image {
            None => None,
            Some(s) if s.trim().is_empty() => anyhow::bail!(
                "'image' in a 'type: test' document ({}) must not be blank",
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
        )?,
        files: {
            validate_top_level_files("test", &raw.files)
                .with_context(|| format!("invalid test config: {}", path.display()))?;
            raw.files
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
            "botforge build requires a 'type: build' document, got 'type: {}'",
            raw.doc_type.as_str()
        );
    }
    let image = match raw.image {
        None => anyhow::bail!(
            "'image' is required in a 'type: build' document ({}): \
             set it to an `@…` reference, e.g. `image: \"@debian-base\"`",
            path.display()
        ),
        Some(s) if s.trim().is_empty() => anyhow::bail!(
            "'image' is required in a 'type: build' document ({}): \
             set it to an `@…` reference, e.g. `image: \"@debian-base\"`",
            path.display()
        ),
        Some(s) => parse_config_image(&s).with_context(|| {
            format!("invalid 'image' value in build config ({})", path.display())
        })?,
    };
    let output = match raw.output {
        None => anyhow::bail!(
            "'output' is required in a 'type: build' document ({}): \
             set it to a bare artifact filename, e.g. `output: \"image.qcow2\"`",
            path.display()
        ),
        Some(s) if s.trim().is_empty() => anyhow::bail!(
            "'output' is required in a 'type: build' document ({}): \
             set it to a bare artifact filename, e.g. `output: \"image.qcow2\"`",
            path.display()
        ),
        Some(s) => s,
    };
    let root_cloud_init = raw.cloud_init.clone();
    let mut include_stack = vec![path.to_path_buf()];
    let mut cloud_init_acc = raw.cloud_init;
    let config = BuildConfig {
        image,
        output,
        disk_size: raw.disk_size,
        steps: expand_test_steps(
            repo_root,
            path,
            raw.steps,
            &mut include_stack,
            &mut cloud_init_acc,
        )?,
        files: {
            validate_top_level_files("build", &raw.files)
                .with_context(|| format!("invalid build config: {}", path.display()))?;
            raw.files
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
                    .and_then(|(steps, ci)| {
                        let mut ci_acc = ci;
                        let expanded_steps = expand_test_steps(
                            repo_root,
                            &include_path,
                            steps,
                            include_stack,
                            &mut ci_acc,
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
) -> Result<(Vec<RawTestStep>, Option<serde_yaml::Mapping>)> {
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
    // Enforce `type: fragment` — entrypoint documents must not be used as fragments.
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
    Ok((fragment.steps, fragment.cloud_init))
}

/// Verify that a `uses:` target is a `type: fragment` document.
///
/// A missing `type:` field or a non-fragment kind (e.g. `type: test`) is a hard
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
/// `output:`, `compress:`) inside a `type: fragment` document.
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
        "files",
    ] {
        if mapping.contains_key(Value::String(section.to_string())) {
            anyhow::bail!(
                "{}: is not valid in a 'type: fragment' document ({})",
                section,
                path.display()
            );
        }
    }
    Ok(())
}

/// Reject build-only sections (`disk_size:`, `memsize:`, `smp:`,
/// `output:`, `compress:`) inside a
/// `type: test` document.  Serde would silently ignore them; this turns a
/// misplaced key into an explicit load-time error.
fn check_no_build_sections_in_test_doc(path: &Path, value: &Value) -> Result<()> {
    let mapping = match value {
        Value::Mapping(m) => m,
        _ => return Ok(()),
    };
    for section in &["disk_size", "memsize", "smp", "output", "compress"] {
        if mapping.contains_key(Value::String(section.to_string())) {
            anyhow::bail!(
                "{}: is not valid in a 'type: test' document ({})",
                section,
                path.display()
            );
        }
    }
    Ok(())
}

/// Reject test-entrypoint-only sections (`ports:`, `isos:`, `diagnostics_units:`)
/// and runner-resource keys (`memsize:`, `smp:`) inside a `type: build` document.
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
                "{}: is not valid in a 'type: build' document ({})",
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

/// Validate a `cloud_init:` mapping at config-load time.
///
/// Two classes of violation are hard-rejected:
///
/// **Ingress protection** — `cloud_init:` must not name host sources.  A
/// `write_files:` entry with a `source:` field would instruct cloud-init to pull
/// content from a URL or local path; that is an ingress vector outside the
/// shasset-only host→build boundary and is therefore rejected.  Inline
/// `content:` fields (which carry values, not paths) are allowed.
///
/// **Harness protection** — settings that would lock botforge out of the runner
/// VM are rejected.  Currently this guards against `ssh_pwauth: false`, which
/// (when combined with certain sshd configurations) could break key-based login.
pub(crate) fn validate_cloud_init_fragment(
    cloud_init: &serde_yaml::Mapping,
    path: &Path,
) -> Result<()> {
    // Ingress guard: write_files entries must not have a source: field.
    let write_files_key = Value::String("write_files".to_string());
    if let Some(Value::Sequence(entries)) = cloud_init.get(&write_files_key) {
        for entry in entries {
            if let Value::Mapping(entry_map) = entry {
                if entry_map.contains_key(Value::String("source".to_string())) {
                    anyhow::bail!(
                        "cloud_init.write_files: 'source:' is not allowed in {} \
                         (ingress guard: use 'files:' for host→guest file transfer; \
                         inline 'content:' is allowed)",
                        path.display()
                    );
                }
            }
        }
    }
    // Harness guard: reject ssh_pwauth: false which can break key-based login
    // in combination with certain sshd configurations.
    let ssh_pwauth_key = Value::String("ssh_pwauth".to_string());
    if let Some(Value::Bool(false)) = cloud_init.get(&ssh_pwauth_key) {
        anyhow::bail!(
            "cloud_init.ssh_pwauth: false is not allowed in {} \
             (harness guard: setting ssh_pwauth false may break botforge's key-based \
             SSH access to the runner VM)",
            path.display()
        );
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CloudInitSchemaMode {
    Off,
    Warn,
    Strict,
}

impl CloudInitSchemaMode {
    fn from_env() -> Self {
        let raw = std::env::var("BOTFORGE_CLOUD_INIT_SCHEMA")
            .unwrap_or_else(|_| "warn".to_string())
            .trim()
            .to_ascii_lowercase();
        match raw.as_str() {
            "off" => Self::Off,
            "warn" => Self::Warn,
            "strict" => Self::Strict,
            _ => Self::Warn,
        }
    }
}

enum CloudInitSchemaCheck {
    Pass,
    MissingBinary,
    InvocationFailed(String),
    Invalid(String),
}

fn validate_cloud_init_schema_fragment(
    cloud_init: &serde_yaml::Mapping,
    path: &Path,
) -> Result<()> {
    let mode = cloud_init_schema_mode();
    if matches!(mode, CloudInitSchemaMode::Off) {
        return Ok(());
    }

    let rendered = render_cloud_init_schema_document(cloud_init)
        .with_context(|| format!("failed to render cloud_init document in {}", path.display()))?;
    match run_cloud_init_schema_check(&rendered) {
        CloudInitSchemaCheck::Pass | CloudInitSchemaCheck::MissingBinary => Ok(()),
        CloudInitSchemaCheck::InvocationFailed(details) => {
            emit_cloud_init_schema_warning(&format!(
                "cloud-init schema pre-validation skipped for {}: {}",
                path.display(),
                details
            ));
            Ok(())
        }
        CloudInitSchemaCheck::Invalid(details) => match mode {
            CloudInitSchemaMode::Warn => {
                emit_cloud_init_schema_warning(&format!(
                    "cloud-init schema pre-validation reported issues for {}:\n{}",
                    path.display(),
                    details
                ));
                Ok(())
            }
            CloudInitSchemaMode::Strict => anyhow::bail!(
                "cloud-init schema pre-validation failed for {}:\n{}",
                path.display(),
                details
            ),
            CloudInitSchemaMode::Off => Ok(()),
        },
    }
}

fn render_cloud_init_schema_document(cloud_init: &serde_yaml::Mapping) -> Result<String> {
    // Deliberately validate the user-provided fragment (with `#cloud-config` header)
    // rather than botforge's merged installer user-data. This keeps pre-validation
    // focused on user-authored keys while preserving authoritative runtime merge
    // behavior in `iso::render_user_data`.
    let yaml = serde_yaml::to_string(&Value::Mapping(cloud_init.clone()))
        .context("failed to serialize cloud_init fragment as YAML")?;
    Ok(format!("#cloud-config\n{yaml}"))
}

fn cloud_init_schema_mode() -> CloudInitSchemaMode {
    #[cfg(test)]
    {
        if let Some(mode) = cloud_init_schema_mode_override() {
            return mode;
        }
    }
    CloudInitSchemaMode::from_env()
}

fn run_cloud_init_schema_check(document: &str) -> CloudInitSchemaCheck {
    #[cfg(test)]
    {
        if let Some(result) = run_cloud_init_schema_check_override(document) {
            return result;
        }
    }
    let Some(cloud_init_bin) = locate_cloud_init_binary() else {
        return CloudInitSchemaCheck::MissingBinary;
    };
    match run_cloud_init_schema_via_stdin(&cloud_init_bin, document) {
        Ok(CloudInitSchemaCheck::Pass) => CloudInitSchemaCheck::Pass,
        Ok(_) => match run_cloud_init_schema_via_temp_file(&cloud_init_bin, document) {
            Ok(result) => result,
            Err(err) => CloudInitSchemaCheck::InvocationFailed(err),
        },
        Err(err) => CloudInitSchemaCheck::InvocationFailed(err),
    }
}

fn locate_cloud_init_binary() -> Option<PathBuf> {
    if command_exists("cloud-init") {
        Some(PathBuf::from("cloud-init"))
    } else {
        None
    }
}

fn run_cloud_init_schema_via_stdin(
    cloud_init_bin: &Path,
    document: &str,
) -> std::result::Result<CloudInitSchemaCheck, String> {
    let mut child = Command::new(cloud_init_bin)
        .arg("schema")
        .arg("--config-file")
        .arg("-")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| format!("failed to execute {}: {err}", cloud_init_bin.display()))?;
    child
        .stdin
        .as_mut()
        .ok_or_else(|| "failed to open stdin for cloud-init schema".to_string())?
        .write_all(document.as_bytes())
        .map_err(|err| format!("failed to write cloud-init schema stdin: {err}"))?;
    let output = child
        .wait_with_output()
        .map_err(|err| format!("failed to wait for cloud-init schema: {err}"))?;
    Ok(parse_cloud_init_schema_output(output))
}

fn run_cloud_init_schema_via_temp_file(
    cloud_init_bin: &Path,
    document: &str,
) -> std::result::Result<CloudInitSchemaCheck, String> {
    let temp_dir = create_temp_dir("botforge-cloud-init-schema")
        .map_err(|err| format!("failed to create temp dir for cloud-init schema: {err:#}"))?;
    let config_path = temp_dir.join("cloud-init.yaml");
    std::fs::write(&config_path, document).map_err(|err| {
        format!(
            "failed to write temp cloud-init config {}: {err}",
            config_path.display()
        )
    })?;
    let output = Command::new(cloud_init_bin)
        .arg("schema")
        .arg("--config-file")
        .arg(&config_path)
        .output()
        .map_err(|err| format!("failed to execute {}: {err}", cloud_init_bin.display()));
    let cleanup_result = std::fs::remove_dir_all(&temp_dir);
    if let Err(err) = cleanup_result {
        emit_cloud_init_schema_warning(&format!(
            "failed to remove temp dir {} after cloud-init schema pre-validation: {}",
            temp_dir.display(),
            err
        ));
    }
    output.map(parse_cloud_init_schema_output)
}

fn parse_cloud_init_schema_output(output: std::process::Output) -> CloudInitSchemaCheck {
    if output.status.success() {
        return CloudInitSchemaCheck::Pass;
    }
    CloudInitSchemaCheck::Invalid(format_cloud_init_schema_message(&output))
}

fn format_cloud_init_schema_message(output: &std::process::Output) -> String {
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    match (stdout.is_empty(), stderr.is_empty()) {
        (true, true) => format!("cloud-init schema exited with {}", output.status),
        (true, false) => stderr,
        (false, true) => stdout,
        (false, false) => format!("{stderr}\n{stdout}"),
    }
}

fn emit_cloud_init_schema_warning(message: &str) {
    #[cfg(test)]
    {
        if capture_cloud_init_schema_warning_for_test(message) {
            return;
        }
    }
    eprintln!("warning: {message}");
}

/// Deep-merge two cloud-config mappings under botforge's merge semantics:
///
/// - **Sequences**: base first, then overlay (botforge-first concatenation).
/// - **Mappings**: recurse.
/// - **Scalars**: overlay wins.
pub(crate) fn merge_cloud_init_mappings(
    base: serde_yaml::Mapping,
    overlay: serde_yaml::Mapping,
) -> serde_yaml::Mapping {
    let mut result = base;
    for (key, overlay_val) in overlay {
        match result.get_mut(&key) {
            None => {
                result.insert(key, overlay_val);
            }
            Some(base_val) => match (base_val, overlay_val) {
                (Value::Sequence(base_seq), Value::Sequence(overlay_seq)) => {
                    base_seq.extend(overlay_seq);
                }
                (Value::Mapping(base_map), Value::Mapping(overlay_map)) => {
                    *base_map = merge_cloud_init_mappings(base_map.clone(), overlay_map);
                }
                (base_val, overlay_val) => {
                    *base_val = overlay_val;
                }
            },
        }
    }
    result
}

fn extract_fragment_input_declarations(
    value: &Value,
) -> Result<BTreeMap<String, InputDeclaration>> {
    let mapping = match value {
        Value::Mapping(m) => m,
        _ => return Ok(BTreeMap::new()),
    };
    let inputs_key = Value::String("inputs".to_string());
    match mapping.get(&inputs_key) {
        None => Ok(BTreeMap::new()),
        Some(inputs_value) => {
            let declarations: BTreeMap<String, InputDeclaration> =
                serde_yaml::from_value(inputs_value.clone())
                    .context("invalid inputs: declaration")?;
            Ok(declarations)
        }
    }
}

pub(super) fn resolve_fragment_inputs(
    path: &Path,
    declarations: &BTreeMap<String, InputDeclaration>,
    with: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, String>> {
    // Declaration-time validation: required: true + default: together is a contradiction.
    for (name, decl) in declarations {
        if decl.required && decl.default.is_some() {
            anyhow::bail!(
                "input '{}' cannot set both 'required: true' and 'default'",
                name
            );
        }
    }

    // Caller must not pass keys that the fragment has not declared.
    for key in with.keys() {
        if !declarations.contains_key(key.as_str()) {
            anyhow::bail!(
                "unexpected input '{}' not declared by fragment {}",
                key,
                path.display()
            );
        }
    }

    let mut resolved = BTreeMap::new();

    for (name, decl) in declarations {
        let caller_value = with.get(name.as_str());

        // Resolution pipeline:
        //   omitted key or "__default__" sentinel → declared default (or unset if none).
        //   any other value (including "") → take literally.
        let effective: Option<String> = match caller_value.map(String::as_str) {
            None | Some(DEFAULT_SENTINEL) => decl.default.clone(),
            Some(v) => Some(v.to_string()),
        };

        // Type-validate the resolved value (sentinel is already gone at this point).
        if let Some(ref v) = effective {
            validate_input_type(name, decl.input_type, v)?;
        }

        // Required check: unset + required → error.
        if effective.is_none() && decl.required {
            anyhow::bail!("missing required input '{}'", name);
        }

        if let Some(v) = effective {
            resolved.insert(name.clone(), v);
        }
    }

    Ok(resolved)
}

fn validate_input_type(name: &str, input_type: InputType, value: &str) -> Result<()> {
    match input_type {
        InputType::String => Ok(()),
        InputType::Number => value
            .parse::<f64>()
            .map(|_| ())
            .map_err(|_| anyhow::anyhow!("input '{}' must be a number", name)),
        InputType::Boolean => match value.to_ascii_lowercase().as_str() {
            "true" | "false" => Ok(()),
            _ => anyhow::bail!("input '{}' must be a boolean", name),
        },
    }
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

/// Validate a `mode` string: must be 3–4 octal digits (same rule as `payload.rs`).
fn validate_mode_string(mode: &str, src: &str, kind: &str) -> Result<()> {
    if mode.len() < 3 || mode.len() > 4 || !mode.chars().all(|ch| ('0'..='7').contains(&ch)) {
        anyhow::bail!("{kind} files entry '{src}': `mode` must be 3–4 octal digits, got '{mode}'");
    }
    Ok(())
}

/// Validate an `owner` or `group` string: non-empty, no whitespace, no `/`, no shell metacharacters.
fn validate_owner_group_string(value: &str, field: &str, src: &str, kind: &str) -> Result<()> {
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

fn validate_assert_block(block: &AssertBlock) -> Result<()> {
    for (guest_path, expectation) in &block.files {
        validate_assert_file_entry(guest_path, expectation)?;
    }
    for (name_or_pattern, expectation) in &block.users {
        validate_assert_user_entry(name_or_pattern, expectation)?;
    }
    for (name_or_pattern, expectation) in &block.groups {
        validate_assert_group_entry(name_or_pattern, expectation)?;
    }
    for (name_or_pattern, expectation) in &block.packages {
        validate_assert_package_entry(name_or_pattern, expectation)?;
    }
    Ok(())
}

fn validate_assert_file_entry(guest_path: &str, expectation: &AssertFile) -> Result<()> {
    if !guest_path.starts_with('/') {
        anyhow::bail!(
            "assert.files: path '{guest_path}' must be an absolute guest path (must start with '/')"
        );
    }
    if !expectation.exists {
        // When exists: false, attribute fields are meaningless — reject them.
        if expectation.filetype.is_some()
            || expectation.owner.is_some()
            || expectation.group.is_some()
            || expectation.mode.is_some()
        {
            anyhow::bail!(
                "assert.files: path '{guest_path}': attribute fields \
                 (filetype/owner/group/mode) must not be set when `exists: false`"
            );
        }
        return Ok(());
    }
    if let Some(ref mode) = expectation.mode {
        validate_mode_string(mode, guest_path, "assert")?;
    }
    if let Some(ref owner) = expectation.owner {
        validate_owner_group_string(owner, "owner", guest_path, "assert")?;
    }
    if let Some(ref group) = expectation.group {
        validate_owner_group_string(group, "group", guest_path, "assert")?;
    }
    Ok(())
}

pub(crate) fn src_has_glob_metacharacters(src: &str) -> bool {
    src.contains('*') || src.contains('?') || src.contains('[')
}

fn validate_assert_user_entry(name_or_pattern: &str, expectation: &AssertUser) -> Result<()> {
    if !expectation.exists {
        // When exists: false, attribute fields are meaningless — reject them.
        if expectation.shell.is_some() || !expectation.groups.is_empty() {
            anyhow::bail!(
                "assert.users: entry '{name_or_pattern}': attribute fields \
                 (shell/groups) must not be set when `exists: false`"
            );
        }
    }
    Ok(())
}

fn validate_assert_group_entry(_name_or_pattern: &str, _expectation: &AssertGroup) -> Result<()> {
    // Currently no additional validation beyond deserialization for groups.
    Ok(())
}

fn validate_assert_package_entry(
    _name_or_pattern: &str,
    _expectation: &AssertPackage,
) -> Result<()> {
    // Unknown attributes are already rejected at deserialization time via
    // `#[serde(deny_unknown_fields)]` on `AssertPackage`.  No further
    // validation is required in v1.
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
                    "test step '{}': `archive` steps are only supported in `type: build` documents",
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

fn validate_archive_build_step(step: &crate::plan::step::ArchiveStep) -> Result<()> {
    use crate::plan::step::StepTarget;
    use crate::resolver::Reference;
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

#[cfg(test)]
type CloudInitSchemaCheckFn = fn(&str) -> CloudInitSchemaCheck;

#[cfg(test)]
thread_local! {
    static CLOUD_INIT_SCHEMA_MODE_OVERRIDE: std::cell::RefCell<Option<CloudInitSchemaMode>> =
        const { std::cell::RefCell::new(None) };
    static CLOUD_INIT_SCHEMA_CHECK_OVERRIDE: std::cell::RefCell<Option<CloudInitSchemaCheckFn>> =
        const { std::cell::RefCell::new(None) };
    static CLOUD_INIT_SCHEMA_WARNINGS: std::cell::RefCell<Option<Vec<String>>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
fn cloud_init_schema_mode_override() -> Option<CloudInitSchemaMode> {
    CLOUD_INIT_SCHEMA_MODE_OVERRIDE.with(|slot| *slot.borrow())
}

#[cfg(test)]
fn run_cloud_init_schema_check_override(document: &str) -> Option<CloudInitSchemaCheck> {
    CLOUD_INIT_SCHEMA_CHECK_OVERRIDE
        .with(|slot| slot.borrow().as_ref().copied())
        .map(|check| check(document))
}

#[cfg(test)]
fn capture_cloud_init_schema_warning_for_test(message: &str) -> bool {
    CLOUD_INIT_SCHEMA_WARNINGS.with(|slot| {
        let mut slot = slot.borrow_mut();
        if let Some(ref mut warnings) = *slot {
            warnings.push(message.to_string());
            true
        } else {
            false
        }
    })
}

#[cfg(test)]
mod tests {
    use super::{
        default_bootstrap_path, load_build_config, load_test_config, resolve_fragment_inputs,
        validate_build_steps, validate_test_ports, validate_test_steps, AssertFileType,
        CloudInitSchemaCheck, CloudInitSchemaMode, CompressionType, InputDeclaration, InputType,
        ReclaimMode, TestConfig, TestIso, CLOUD_INIT_SCHEMA_CHECK_OVERRIDE,
        CLOUD_INIT_SCHEMA_MODE_OVERRIDE, CLOUD_INIT_SCHEMA_WARNINGS, MAX_INCLUDE_DEPTH,
    };
    use crate::plan::files::FileEntry;
    use crate::plan::step::{ArchiveStep, ArchiveStepSpec, RunStep, StepTarget, TestStep};
    use crate::qemu::PortSpec;
    use crate::resolver::Reference;
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};
    use tempfile::TempDir;

    fn loopback(port: u16) -> PortSpec {
        PortSpec {
            addr: "127.0.0.1".into(),
            port,
        }
    }

    fn with_cloud_init_schema_mode<T>(mode: CloudInitSchemaMode, f: impl FnOnce() -> T) -> T {
        CLOUD_INIT_SCHEMA_MODE_OVERRIDE.with(|slot| *slot.borrow_mut() = Some(mode));
        let result = f();
        CLOUD_INIT_SCHEMA_MODE_OVERRIDE.with(|slot| *slot.borrow_mut() = None);
        result
    }

    fn with_cloud_init_schema_check<T>(
        check: fn(&str) -> CloudInitSchemaCheck,
        f: impl FnOnce() -> T,
    ) -> T {
        CLOUD_INIT_SCHEMA_CHECK_OVERRIDE.with(|slot| *slot.borrow_mut() = Some(check));
        let result = f();
        CLOUD_INIT_SCHEMA_CHECK_OVERRIDE.with(|slot| *slot.borrow_mut() = None);
        result
    }

    fn with_warning_capture<T>(f: impl FnOnce() -> T) -> (T, Vec<String>) {
        CLOUD_INIT_SCHEMA_WARNINGS.with(|slot| *slot.borrow_mut() = Some(Vec::new()));
        let result = f();
        let warnings = CLOUD_INIT_SCHEMA_WARNINGS
            .with(|slot| slot.borrow_mut().take())
            .unwrap_or_default();
        (result, warnings)
    }

    fn schema_pass(_: &str) -> CloudInitSchemaCheck {
        CloudInitSchemaCheck::Pass
    }

    fn schema_missing(_: &str) -> CloudInitSchemaCheck {
        CloudInitSchemaCheck::MissingBinary
    }

    fn schema_invalid(_: &str) -> CloudInitSchemaCheck {
        CloudInitSchemaCheck::Invalid("invalid cloud-config key".to_string())
    }

    fn run_ref(step: &TestStep) -> &RunStep {
        let TestStep::Run(step) = step else {
            panic!("expected run step");
        };
        step
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
        assert_eq!(run_ref(&config.steps[0]).target, StepTarget::Guest);
        assert_eq!(run_ref(&config.steps[0]).name, "goss");
        assert_eq!(
            run_ref(&config.steps[0]).run,
            "goss -g /path/goss.yaml validate"
        );
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
        assert_eq!(run_ref(&config.steps[0]).target, StepTarget::Host);
        assert_eq!(run_ref(&config.steps[0]).name, "vm-narrative");
        assert_eq!(
            run_ref(&config.steps[0]).run,
            "bash smoke/vm-narrative.sh 127.0.0.1"
        );
    }

    #[test]
    fn test_step_parses_timeout_seconds() {
        let config: TestConfig = serde_yaml::from_str(
            r#"
steps:
  - on: guest
    name: long-step
    timeout: 900
    run: echo hello
"#,
        )
        .unwrap();

        assert_eq!(run_ref(&config.steps[0]).timeout, Some(900));
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
        assert_eq!(run_ref(&config.steps[0]).target, StepTarget::Guest);
        assert_eq!(run_ref(&config.steps[1]).target, StepTarget::Guest);
        assert_eq!(run_ref(&config.steps[2]).target, StepTarget::Host);
        assert_eq!(run_ref(&config.steps[3]).target, StepTarget::Guest);
    }

    #[test]
    fn test_step_parses_missing_on_field_as_guest() {
        let config: TestConfig = serde_yaml::from_str(
            r#"
steps:
  - name: no-on-field
    run: echo hello
"#,
        )
        .unwrap();
        assert_eq!(config.steps.len(), 1);
        assert_eq!(run_ref(&config.steps[0]).target, StepTarget::Guest);
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
type: fragment
inputs:
  target:
    type: string
    required: true
  shell:
    type: string
    default: bash
steps:
  - on: guest
    name: "narrative-${{ inputs.target }}"
    shell: ${{ inputs.shell }}
    run: |
      echo "${USER}"
      bash /tmp/${{ inputs.target }}.sh
"#,
        )
        .unwrap();
        std::fs::write(
            repo.path().join("test.yaml"),
            r#"
type: test
steps:
  - uses: "@://shared/narrative.yaml"
    with:
      target: edge
      shell: bash
"#,
        )
        .unwrap();

        let config = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap();

        assert_eq!(config.steps.len(), 1);
        assert_eq!(run_ref(&config.steps[0]).name, "narrative-edge");
        assert_eq!(run_ref(&config.steps[0]).shell.as_deref(), Some("bash"));
        assert!(run_ref(&config.steps[0]).run.contains(r#"echo "${USER}""#));
        assert!(run_ref(&config.steps[0]).run.contains("bash /tmp/edge.sh"));
    }

    #[test]
    fn test_load_test_config_preserves_fragment_sudo_via_uses() {
        let repo = TempDir::new().unwrap();
        std::fs::write(
            repo.path().join("frag.yaml"),
            r#"
type: fragment
steps:
  - on: guest
    name: frag-root-step
    sudo: true
    run: echo from-fragment
"#,
        )
        .unwrap();
        std::fs::write(
            repo.path().join("test.yaml"),
            r#"
type: test
steps:
  - uses: "@://frag.yaml"
"#,
        )
        .unwrap();

        let config = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap();

        assert_eq!(config.steps.len(), 1);
        assert_eq!(run_ref(&config.steps[0]).name, "frag-root-step");
        assert_eq!(run_ref(&config.steps[0]).sudo, Some(true));
    }

    #[test]
    fn test_load_test_config_rejects_unsupported_uses_scheme() {
        let repo = TempDir::new().unwrap();
        std::fs::write(
            repo.path().join("test.yaml"),
            r#"
type: test
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
type: fragment
inputs:
  target:
    type: string
    required: true
steps:
  - on: guest
    name: "${{ inputs.target }}"
    run: "echo ok"
"#,
        )
        .unwrap();
        std::fs::write(
            repo.path().join("test.yaml"),
            r#"
type: test
steps:
  - uses: "@://shared/narrative.yaml"
"#,
        )
        .unwrap();

        let err = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap_err();
        assert!(format!("{err:#}").contains("missing required input 'target'"));
    }

    #[test]
    fn test_load_test_config_rejects_bare_list_fragment() {
        let repo = TempDir::new().unwrap();
        std::fs::create_dir_all(repo.path().join("shared")).unwrap();
        std::fs::write(
            repo.path().join("shared/narrative.yaml"),
            r#"
- on: guest
  name: bare
  run: "echo ok"
"#,
        )
        .unwrap();
        std::fs::write(
            repo.path().join("test.yaml"),
            r#"
type: test
steps:
  - uses: "@://shared/narrative.yaml"
"#,
        )
        .unwrap();

        let err = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap_err();
        assert!(
            format!("{err:#}").contains("must be a mapping with a 'steps:' key"),
            "unexpected error: {err:#}"
        );
    }

    #[test]
    fn test_load_test_config_rejects_parent_segments_in_uses_path() {
        let repo = TempDir::new().unwrap();
        std::fs::write(
            repo.path().join("test.yaml"),
            r#"
type: test
steps:
  - uses: "@://shared/../narrative.yaml"
"#,
        )
        .unwrap();

        let err = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap_err();
        assert!(format!("{err:#}").contains("must contain no '.' or '..' segments"));
    }

    // --- step validation ---

    fn make_step(target: StepTarget, name: &str) -> TestStep {
        TestStep::Run(RunStep {
            target,
            name: name.to_string(),
            run: "echo ok".to_string(),
            timeout: None,
            shell: None,
            sudo: None,
            id: None,
        })
    }

    #[test]
    fn test_validate_steps_accepts_host_step() {
        let steps = vec![make_step(StepTarget::Host, "s")];
        assert!(validate_test_steps(&steps, &[loopback(80)]).is_ok());
    }

    #[test]
    fn test_validate_steps_rejects_host_step_without_ports() {
        let steps = vec![make_step(StepTarget::Host, "edge")];
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
        let steps = vec![make_step(StepTarget::Guest, "s")];
        assert!(validate_test_steps(&steps, &[]).is_ok());
    }

    #[test]
    fn test_validate_steps_rejects_host_step_with_sudo() {
        let mut step = make_step(StepTarget::Host, "host-root");
        let TestStep::Run(run) = &mut step else {
            panic!("expected run step");
        };
        run.sudo = Some(true);
        let err = validate_test_steps(&[step], &[loopback(80)]).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("host-root"),
            "error should mention step name: {msg}"
        );
        assert!(msg.contains("sudo"), "error should mention sudo: {msg}");
        assert!(msg.contains("guest"), "error should mention guest: {msg}");
    }

    #[test]
    fn test_validate_steps_accepts_host_step_with_explicit_sudo_false() {
        let mut step = make_step(StepTarget::Host, "host-unprivileged");
        let TestStep::Run(run) = &mut step else {
            panic!("expected run step");
        };
        run.sudo = Some(false);
        assert!(validate_test_steps(&[step], &[loopback(80)]).is_ok());
    }

    #[test]
    fn test_validate_steps_accepts_guest_step_with_sudo() {
        let mut step = make_step(StepTarget::Guest, "guest-root");
        let TestStep::Run(run) = &mut step else {
            panic!("expected run step");
        };
        run.sudo = Some(true);
        assert!(validate_test_steps(&[step], &[]).is_ok());
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
        assert_eq!(run_ref(&config.steps[0]).shell.as_deref(), Some("python"));
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
        assert!(run_ref(&config.steps[0]).shell.is_none());
    }

    #[test]
    fn test_validate_steps_rejects_bad_shell() {
        let mut step = make_step(StepTarget::Guest, "bad-shell");
        let TestStep::Run(run) = &mut step else {
            panic!("expected run step");
        };
        run.shell = Some("fish".to_string());
        let err = validate_test_steps(&[step], &[]).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("fish"),
            "error should mention shell name: {msg}"
        );
    }

    // --- id field deserialization ---

    #[test]
    fn test_step_parses_id_field() {
        let config: TestConfig = serde_yaml::from_str(
            r#"
steps:
  - on: guest
    name: my-step
    id: my-step
    run: echo hello
"#,
        )
        .unwrap();
        assert_eq!(run_ref(&config.steps[0]).id.as_deref(), Some("my-step"));
    }

    #[test]
    fn test_step_without_id_defaults_to_none() {
        let config: TestConfig = serde_yaml::from_str(
            r#"
steps:
  - on: guest
    name: no-id-step
    run: echo hello
"#,
        )
        .unwrap();
        assert!(run_ref(&config.steps[0]).id.is_none());
    }

    #[test]
    fn test_step_unknown_field_still_errors() {
        let err = serde_yaml::from_str::<TestConfig>(
            r#"
steps:
  - on: guest
    name: my-step
    run: echo hello
    bogus_field: not-allowed
"#,
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("bogus_field") || msg.contains("unknown field"),
            "error should mention the unknown field: {msg}"
        );
    }

    #[test]
    fn test_step_id_flows_through_uses_fragment() {
        let repo = TempDir::new().unwrap();
        std::fs::create_dir_all(repo.path().join("shared")).unwrap();
        std::fs::write(
            repo.path().join("shared/frag.yaml"),
            r#"
type: fragment
steps:
  - on: guest
    name: frag-step
    id: my-frag-id
    run: echo hello
"#,
        )
        .unwrap();
        std::fs::write(
            repo.path().join("test.yaml"),
            r#"
type: test
steps:
  - uses: "@://shared/frag.yaml"
"#,
        )
        .unwrap();

        let config = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap();

        assert_eq!(config.steps.len(), 1);
        assert_eq!(run_ref(&config.steps[0]).name, "frag-step");
        assert_eq!(
            run_ref(&config.steps[0]).id.as_deref(),
            Some("my-frag-id"),
            "id should be preserved through fragment splice"
        );
    }

    // --- fragment input contract (resolve_fragment_inputs unit tests) ---

    fn decl(input_type: InputType, required: bool, default: Option<&str>) -> InputDeclaration {
        InputDeclaration {
            input_type,
            required,
            default: default.map(str::to_string),
        }
    }

    fn dummy_path() -> &'static Path {
        Path::new("fragment.yaml")
    }

    fn with_map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn decl_map(pairs: &[(&str, InputDeclaration)]) -> BTreeMap<String, InputDeclaration> {
        pairs
            .iter()
            .map(|(k, d)| {
                (
                    k.to_string(),
                    InputDeclaration {
                        input_type: d.input_type,
                        required: d.required,
                        default: d.default.clone(),
                    },
                )
            })
            .collect()
    }

    #[test]
    fn test_resolve_inputs_declared_default_applied_when_caller_omits_key() {
        let declarations = decl_map(&[("shell", decl(InputType::String, false, Some("bash")))]);
        let with = BTreeMap::new();
        let resolved = resolve_fragment_inputs(dummy_path(), &declarations, &with).unwrap();
        assert_eq!(resolved.get("shell").map(String::as_str), Some("bash"));
    }

    #[test]
    fn test_resolve_inputs_default_sentinel_resolves_to_declared_default() {
        let declarations = decl_map(&[("shell", decl(InputType::String, false, Some("bash")))]);
        let with = with_map(&[("shell", "__default__")]);
        let resolved = resolve_fragment_inputs(dummy_path(), &declarations, &with).unwrap();
        assert_eq!(resolved.get("shell").map(String::as_str), Some("bash"));
    }

    #[test]
    fn test_resolve_inputs_default_sentinel_with_no_declared_default_yields_unset() {
        let declarations = decl_map(&[("target", decl(InputType::String, false, None))]);
        let with = with_map(&[("target", "__default__")]);
        let resolved = resolve_fragment_inputs(dummy_path(), &declarations, &with).unwrap();
        assert!(
            !resolved.contains_key("target"),
            "unset input must not appear in resolved map"
        );
    }

    #[test]
    fn test_resolve_inputs_empty_string_yields_empty_not_default() {
        let declarations = decl_map(&[("shell", decl(InputType::String, false, Some("bash")))]);
        let with = with_map(&[("shell", "")]);
        let resolved = resolve_fragment_inputs(dummy_path(), &declarations, &with).unwrap();
        assert_eq!(
            resolved.get("shell").map(String::as_str),
            Some(""),
            "empty string must not be replaced by the declared default"
        );
    }

    #[test]
    fn test_resolve_inputs_empty_string_satisfies_required() {
        let declarations = decl_map(&[("target", decl(InputType::String, true, None))]);
        let with = with_map(&[("target", "")]);
        let result = resolve_fragment_inputs(dummy_path(), &declarations, &with);
        assert!(
            result.is_ok(),
            "empty string must satisfy required: {result:?}"
        );
        assert_eq!(result.unwrap().get("target").map(String::as_str), Some(""));
    }

    #[test]
    fn test_resolve_inputs_number_type_valid() {
        let declarations = decl_map(&[("count", decl(InputType::Number, false, None))]);
        let with = with_map(&[("count", "42")]);
        assert!(resolve_fragment_inputs(dummy_path(), &declarations, &with).is_ok());
    }

    #[test]
    fn test_resolve_inputs_number_type_valid_float() {
        let declarations = decl_map(&[("ratio", decl(InputType::Number, false, None))]);
        let with = with_map(&[("ratio", "3.14")]);
        assert!(resolve_fragment_inputs(dummy_path(), &declarations, &with).is_ok());
    }

    #[test]
    fn test_resolve_inputs_number_type_invalid() {
        let declarations = decl_map(&[("count", decl(InputType::Number, false, None))]);
        let with = with_map(&[("count", "not-a-number")]);
        let err = resolve_fragment_inputs(dummy_path(), &declarations, &with).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("count") && msg.contains("number"),
            "error must name the input and type: {msg}"
        );
    }

    #[test]
    fn test_resolve_inputs_boolean_type_valid() {
        let declarations = decl_map(&[("flag", decl(InputType::Boolean, false, None))]);
        for v in &["true", "false", "True", "FALSE"] {
            let with = with_map(&[("flag", v)]);
            assert!(
                resolve_fragment_inputs(dummy_path(), &declarations, &with).is_ok(),
                "expected valid boolean for '{v}'"
            );
        }
    }

    #[test]
    fn test_resolve_inputs_boolean_type_invalid() {
        let declarations = decl_map(&[("flag", decl(InputType::Boolean, false, None))]);
        let with = with_map(&[("flag", "yes")]);
        let err = resolve_fragment_inputs(dummy_path(), &declarations, &with).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("flag") && msg.contains("boolean"),
            "error must name the input and type: {msg}"
        );
    }

    #[test]
    fn test_resolve_inputs_default_sentinel_on_typed_input_is_not_parse_error() {
        // "__default__" on a number/boolean input resolves to the declared default,
        // not parsed as the type — so it must never produce a type error.
        let declarations = decl_map(&[("count", decl(InputType::Number, false, Some("10")))]);
        let with = with_map(&[("count", "__default__")]);
        let resolved = resolve_fragment_inputs(dummy_path(), &declarations, &with).unwrap();
        assert_eq!(resolved.get("count").map(String::as_str), Some("10"));
    }

    #[test]
    fn test_resolve_inputs_undeclared_with_key_errors() {
        let declarations = BTreeMap::new();
        let with = with_map(&[("unknown", "value")]);
        let err = resolve_fragment_inputs(dummy_path(), &declarations, &with).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("unknown"),
            "error must name the undeclared key: {msg}"
        );
    }

    #[test]
    fn test_resolve_inputs_missing_required_when_omitted() {
        let declarations = decl_map(&[("target", decl(InputType::String, true, None))]);
        let with = BTreeMap::new();
        let err = resolve_fragment_inputs(dummy_path(), &declarations, &with).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("missing required input 'target'"),
            "unexpected error: {msg}"
        );
    }

    #[test]
    fn test_resolve_inputs_declaration_required_and_default_contradiction() {
        let declarations = decl_map(&[("shell", decl(InputType::String, true, Some("bash")))]);
        let with = BTreeMap::new();
        let err = resolve_fragment_inputs(dummy_path(), &declarations, &with).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("shell") && msg.contains("required") && msg.contains("default"),
            "error must describe the contradiction: {msg}"
        );
    }

    // --- `with:` at call site (integration tests via load_test_config) ---

    #[test]
    fn test_load_test_config_with_at_call_site_accepted() {
        let repo = TempDir::new().unwrap();
        std::fs::create_dir_all(repo.path().join("shared")).unwrap();
        std::fs::write(
            repo.path().join("shared/frag.yaml"),
            r#"
type: fragment
inputs:
  msg:
    type: string
    required: true
steps:
  - on: guest
    name: "${{ inputs.msg }}"
    run: "echo ok"
"#,
        )
        .unwrap();
        std::fs::write(
            repo.path().join("test.yaml"),
            r#"
type: test
steps:
  - uses: "@://shared/frag.yaml"
    with:
      msg: hello
"#,
        )
        .unwrap();

        let config = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap();
        assert_eq!(config.steps.len(), 1);
        assert_eq!(run_ref(&config.steps[0]).name, "hello");
    }

    #[test]
    fn test_load_test_config_inputs_at_call_site_is_rejected() {
        let repo = TempDir::new().unwrap();
        std::fs::create_dir_all(repo.path().join("shared")).unwrap();
        std::fs::write(
            repo.path().join("shared/frag.yaml"),
            r#"
type: fragment
inputs:
  msg:
    type: string
    required: true
steps:
  - on: guest
    name: "${{ inputs.msg }}"
    run: "echo ok"
"#,
        )
        .unwrap();
        std::fs::write(
            repo.path().join("test.yaml"),
            r#"
type: test
steps:
  - uses: "@://shared/frag.yaml"
    inputs:
      msg: hello
"#,
        )
        .unwrap();

        // `inputs:` at the call site is not a recognized field; the step must fail to parse.
        assert!(
            load_test_config(repo.path(), &repo.path().join("test.yaml")).is_err(),
            "`inputs:` at call site must be rejected"
        );
    }

    #[test]
    fn test_load_test_config_declared_default_applied_via_fragment() {
        let repo = TempDir::new().unwrap();
        std::fs::create_dir_all(repo.path().join("shared")).unwrap();
        std::fs::write(
            repo.path().join("shared/frag.yaml"),
            r#"
type: fragment
inputs:
  shell:
    type: string
    default: bash
steps:
  - on: guest
    name: step
    shell: ${{ inputs.shell }}
    run: "echo ok"
"#,
        )
        .unwrap();
        std::fs::write(
            repo.path().join("test.yaml"),
            r#"
type: test
steps:
  - uses: "@://shared/frag.yaml"
"#,
        )
        .unwrap();

        let config = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap();
        assert_eq!(run_ref(&config.steps[0]).shell.as_deref(), Some("bash"));
    }

    #[test]
    fn test_load_test_config_undeclared_with_key_errors() {
        let repo = TempDir::new().unwrap();
        std::fs::create_dir_all(repo.path().join("shared")).unwrap();
        std::fs::write(
            repo.path().join("shared/frag.yaml"),
            r#"
type: fragment
steps:
  - on: guest
    name: step
    run: "echo ok"
"#,
        )
        .unwrap();
        std::fs::write(
            repo.path().join("test.yaml"),
            r#"
type: test
steps:
  - uses: "@://shared/frag.yaml"
    with:
      undeclared: value
"#,
        )
        .unwrap();

        let err = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap_err();
        assert!(
            format!("{err:#}").contains("undeclared"),
            "error must mention the undeclared key: {err:#}"
        );
    }

    // --- type discriminator on root documents ---

    #[test]
    fn test_load_test_config_requires_type_field() {
        let repo = TempDir::new().unwrap();
        std::fs::write(
            repo.path().join("test.yaml"),
            r#"
steps: []
"#,
        )
        .unwrap();
        let err = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("type"), "error must mention 'type': {msg}");
    }

    #[test]
    fn test_load_test_config_rejects_unknown_type() {
        let repo = TempDir::new().unwrap();
        std::fs::write(
            repo.path().join("test.yaml"),
            r#"
type: unknown
steps: []
"#,
        )
        .unwrap();
        let err = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("unknown"),
            "error must mention the bad type value: {msg}"
        );
    }

    #[test]
    fn test_load_test_config_rejects_fragment_as_root() {
        let repo = TempDir::new().unwrap();
        std::fs::write(
            repo.path().join("test.yaml"),
            r#"
type: fragment
steps: []
"#,
        )
        .unwrap();
        let err = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("botforge test requires a 'type: test' document")
                && msg.contains("fragment"),
            "unexpected error: {msg}"
        );
    }

    // --- type discriminator on fragment documents ---

    #[test]
    fn test_load_test_config_uses_requires_type_field_on_fragment() {
        let repo = TempDir::new().unwrap();
        std::fs::write(
            repo.path().join("frag.yaml"),
            r#"
steps:
  - on: guest
    name: step
    run: "echo ok"
"#,
        )
        .unwrap();
        std::fs::write(
            repo.path().join("test.yaml"),
            r#"
type: test
steps:
  - uses: "@://frag.yaml"
"#,
        )
        .unwrap();
        let err = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("missing required 'type:' field") || msg.contains("type"),
            "error must mention missing 'type:': {msg}"
        );
    }

    #[test]
    fn test_load_test_config_uses_rejects_entrypoint_document_as_fragment() {
        let repo = TempDir::new().unwrap();
        std::fs::write(
            repo.path().join("frag.yaml"),
            r#"
type: test
steps:
  - on: guest
    name: step
    run: "echo ok"
"#,
        )
        .unwrap();
        std::fs::write(
            repo.path().join("test.yaml"),
            r#"
type: test
steps:
  - uses: "@://frag.yaml"
"#,
        )
        .unwrap();
        let err = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("not a consumable fragment") && msg.contains("test"),
            "unexpected error: {msg}"
        );
    }

    // --- per-kind presence validation ---

    #[test]
    fn test_fragment_with_ports_is_rejected() {
        let repo = TempDir::new().unwrap();
        std::fs::write(
            repo.path().join("frag.yaml"),
            r#"
type: fragment
ports:
  - 80
steps:
  - on: guest
    name: step
    run: "echo ok"
"#,
        )
        .unwrap();
        std::fs::write(
            repo.path().join("test.yaml"),
            r#"
type: test
steps:
  - uses: "@://frag.yaml"
"#,
        )
        .unwrap();
        let err = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("ports") && msg.contains("fragment"),
            "error must mention 'ports' and 'fragment': {msg}"
        );
    }

    #[test]
    fn test_fragment_with_isos_is_rejected() {
        let repo = TempDir::new().unwrap();
        std::fs::write(
            repo.path().join("frag.yaml"),
            r#"
type: fragment
isos:
  - some/payload.iso
steps:
  - on: guest
    name: step
    run: "echo ok"
"#,
        )
        .unwrap();
        std::fs::write(
            repo.path().join("test.yaml"),
            r#"
type: test
steps:
  - uses: "@://frag.yaml"
"#,
        )
        .unwrap();
        let err = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("isos") && msg.contains("fragment"),
            "error must mention 'isos' and 'fragment': {msg}"
        );
    }

    #[test]
    fn test_fragment_with_diagnostics_units_is_rejected() {
        let repo = TempDir::new().unwrap();
        std::fs::write(
            repo.path().join("frag.yaml"),
            r#"
type: fragment
diagnostics_units:
  - some-service.service
steps:
  - on: guest
    name: step
    run: "echo ok"
"#,
        )
        .unwrap();
        std::fs::write(
            repo.path().join("test.yaml"),
            r#"
type: test
steps:
  - uses: "@://frag.yaml"
"#,
        )
        .unwrap();
        let err = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("diagnostics_units") && msg.contains("fragment"),
            "error must mention 'diagnostics_units' and 'fragment': {msg}"
        );
    }

    #[test]
    fn test_type_test_with_all_entrypoint_sections_loads() {
        let repo = TempDir::new().unwrap();
        std::fs::write(
            repo.path().join("test.yaml"),
            r#"
type: test
isos:
  - some/payload.iso
ports:
  - 80
diagnostics_units:
  - myservice.service
steps:
  - on: guest
    name: basic
    run: "echo ok"
"#,
        )
        .unwrap();
        let config = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap();
        assert_eq!(config.isos.len(), 1);
        assert_eq!(config.ports.len(), 1);
        assert_eq!(config.diagnostics_units.len(), 1);
        assert_eq!(config.steps.len(), 1);
    }

    // --- recursion: cycle, re-entry, max depth ---

    #[test]
    fn test_load_test_config_cyclic_include_errors() {
        // root → frag_a → frag_b → frag_a  (cycle through two fragments)
        let repo = TempDir::new().unwrap();
        std::fs::write(
            repo.path().join("frag_a.yaml"),
            r#"
type: fragment
steps:
  - uses: "@://frag_b.yaml"
"#,
        )
        .unwrap();
        std::fs::write(
            repo.path().join("frag_b.yaml"),
            r#"
type: fragment
steps:
  - uses: "@://frag_a.yaml"
"#,
        )
        .unwrap();
        std::fs::write(
            repo.path().join("test.yaml"),
            r#"
type: test
steps:
  - uses: "@://frag_a.yaml"
"#,
        )
        .unwrap();
        let err = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("cyclic test step include detected"),
            "unexpected error: {msg}"
        );
    }

    #[test]
    fn test_load_test_config_root_includes_self_cycle_errors() {
        // root → root (direct self-cycle)
        let repo = TempDir::new().unwrap();
        std::fs::write(
            repo.path().join("test.yaml"),
            r#"
type: test
steps:
  - uses: "@://test.yaml"
"#,
        )
        .unwrap();
        let err = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap_err();
        let msg = format!("{err:#}");
        // The root is a type: test document, so the fragment type check fires first.
        // Either "cyclic" or "not a consumable fragment" is an acceptable error here —
        // both prevent the self-include.
        assert!(
            msg.contains("cyclic") || msg.contains("not a consumable fragment"),
            "unexpected error: {msg}"
        );
    }

    #[test]
    fn test_load_test_config_reentrant_include_succeeds_and_expands_twice() {
        // Including the same fragment from two independent steps (not a cycle).
        let repo = TempDir::new().unwrap();
        std::fs::write(
            repo.path().join("frag.yaml"),
            r#"
type: fragment
steps:
  - on: guest
    name: reused-step
    run: "echo ok"
"#,
        )
        .unwrap();
        std::fs::write(
            repo.path().join("test.yaml"),
            r#"
type: test
steps:
  - uses: "@://frag.yaml"
  - uses: "@://frag.yaml"
"#,
        )
        .unwrap();
        let config = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap();
        assert_eq!(
            config.steps.len(),
            2,
            "same fragment included twice must expand to two steps"
        );
        assert_eq!(run_ref(&config.steps[0]).name, "reused-step");
        assert_eq!(run_ref(&config.steps[1]).name, "reused-step");
    }

    #[test]
    fn test_load_test_config_max_depth_exceeded_errors() {
        // Create a chain of MAX_INCLUDE_DEPTH fragments deep, which should trigger the
        // depth-limit error.  With the root document seeded into the stack the limit is
        // MAX_INCLUDE_DEPTH total entries, meaning MAX_INCLUDE_DEPTH - 1 fragment levels
        // below the root.  We create exactly that many chain links plus one extra to
        // ensure the limit fires.
        let repo = TempDir::new().unwrap();
        let depth = MAX_INCLUDE_DEPTH; // 32
                                       // Each fragment 0..depth-2 includes the next one.
                                       // Fragment depth-1 is the one we try to include when the stack is full.
        for i in 0..(depth - 1) {
            let name = format!("frag{i:02}.yaml");
            let next = format!("frag{:02}.yaml", i + 1);
            std::fs::write(
                repo.path().join(&name),
                format!("type: fragment\nsteps:\n  - uses: \"@://{next}\"\n"),
            )
            .unwrap();
        }
        // The deepest fragment (depth-1) doesn't need to exist; the depth check fires
        // before loading it.  Write it anyway as a leaf so the test is self-contained.
        std::fs::write(
            repo.path().join(format!("frag{:02}.yaml", depth - 1)),
            "type: fragment\nsteps: []\n",
        )
        .unwrap();
        std::fs::write(
            repo.path().join("test.yaml"),
            "type: test\nsteps:\n  - uses: \"@://frag00.yaml\"\n",
        )
        .unwrap();
        let err = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("depth limit") && msg.contains(&depth.to_string()),
            "error must mention the depth limit: {msg}"
        );
    }

    // --- BuildConfig loading ---

    fn write_build_config(repo: &TempDir, name: &str, content: &str) {
        std::fs::write(repo.path().join(name), content).unwrap();
    }

    fn write_test_config(repo: &TempDir, name: &str, content: &str) {
        std::fs::write(repo.path().join(name), content).unwrap();
    }

    #[test]
    fn test_load_build_config_minimal() {
        let repo = TempDir::new().unwrap();
        write_build_config(
            &repo,
            "build.yaml",
            r#"
type: build
image: "@debian-base"
output: "built.qcow2"
steps:
  - on: guest
    name: provision
    run: echo hello
"#,
        );
        let config = load_build_config(repo.path(), &repo.path().join("build.yaml")).unwrap();
        assert_eq!(
            config.image,
            Reference::Asset {
                name: "debian-base".to_string(),
                path: None
            }
        );
        assert_eq!(config.output, "built.qcow2");
        assert_eq!(config.disk_size, "10G");
        assert_eq!(config.step_timeout, 1800);
        assert_eq!(config.timeout, 7200);
        assert_eq!(config.cloud_init_timeout, 600);
        assert_eq!(config.steps.len(), 1);
        assert_eq!(run_ref(&config.steps[0]).name, "provision");
    }

    #[test]
    fn test_load_build_config_overrides_defaults() {
        let repo = TempDir::new().unwrap();
        write_build_config(
            &repo,
            "build.yaml",
            r#"
type: build
image: "@my-base"
output: "out.qcow2"
disk_size: "20G"
step_timeout: 2400
timeout: 9600
steps: []
"#,
        );
        let config = load_build_config(repo.path(), &repo.path().join("build.yaml")).unwrap();
        assert_eq!(
            config.image,
            Reference::Asset {
                name: "my-base".to_string(),
                path: None
            }
        );
        assert_eq!(config.disk_size, "20G");
        assert_eq!(config.step_timeout, 2400);
        assert_eq!(config.timeout, 9600);
        assert!(config.steps.is_empty());
        assert!(
            config.cloud_init.is_none(),
            "cloud_init should default to None"
        );
    }

    #[test]
    fn test_load_build_config_accepts_repo_traversal_image() {
        let repo = TempDir::new().unwrap();
        write_build_config(
            &repo,
            "build.yaml",
            "type: build\nimage: >-\n  @://build/artifact/foo.qcow2\noutput: \"out.qcow2\"\nsteps: []\n",
        );
        let config = load_build_config(repo.path(), &repo.path().join("build.yaml")).unwrap();
        assert_eq!(
            config.image,
            Reference::Repo {
                path: Some(PathBuf::from("build/artifact/foo.qcow2"))
            }
        );
    }

    #[test]
    fn test_load_build_config_accepts_artifact_traversal_image() {
        let repo = TempDir::new().unwrap();
        write_build_config(
            &repo,
            "build.yaml",
            "type: build\nimage: \"@artifact://foo.qcow2\"\noutput: \"out.qcow2\"\nsteps: []\n",
        );
        let config = load_build_config(repo.path(), &repo.path().join("build.yaml")).unwrap();
        assert_eq!(
            config.image,
            Reference::Artifact {
                path: Some(PathBuf::from("foo.qcow2"))
            }
        );
    }

    #[test]
    fn test_load_build_config_rejects_memsize_section() {
        let repo = TempDir::new().unwrap();
        write_build_config(
            &repo,
            "build.yaml",
            "type: build\nimage: \"@base\"\nmemsize: 8192\nsteps: []\n",
        );
        let err = load_build_config(repo.path(), &repo.path().join("build.yaml")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("memsize") && msg.contains("type: build"),
            "error should mention memsize and document type: {msg}"
        );
    }

    #[test]
    fn test_load_build_config_rejects_smp_section() {
        let repo = TempDir::new().unwrap();
        write_build_config(
            &repo,
            "build.yaml",
            "type: build\nimage: \"@base\"\nsmp: 8\nsteps: []\n",
        );
        let err = load_build_config(repo.path(), &repo.path().join("build.yaml")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("smp") && msg.contains("type: build"),
            "error should mention smp and document type: {msg}"
        );
    }

    #[test]
    fn test_load_build_config_files_absent_is_empty() {
        let repo = TempDir::new().unwrap();
        write_build_config(
            &repo,
            "build.yaml",
            "type: build\nimage: \"@base\"\noutput: \"out.qcow2\"\nsteps: []\n",
        );
        let config = load_build_config(repo.path(), &repo.path().join("build.yaml")).unwrap();
        assert!(config.files.is_empty(), "files should default to empty");
    }

    #[test]
    fn test_load_build_config_parses_top_level_files() {
        let repo = TempDir::new().unwrap();
        write_build_config(
            &repo,
            "build.yaml",
            r#"
type: build
image: "@base"
output: "out.qcow2"
files:
  - src: "@://images/botspace/envoy/**/*.yaml"
    dest: /tmp/bake-staging/envoy/
  - src: "@artifact://build/images/payload/*.tar"
    dest: /usr/share/botwork/images/
steps: []
"#,
        );
        let config = load_build_config(repo.path(), &repo.path().join("build.yaml")).unwrap();
        assert_eq!(
            config.files,
            vec![
                FileEntry {
                    src: "@://images/botspace/envoy/**/*.yaml".to_string(),
                    dest: "/tmp/bake-staging/envoy/".to_string(),
                    ..Default::default()
                },
                FileEntry {
                    src: "@artifact://build/images/payload/*.tar".to_string(),
                    dest: "/usr/share/botwork/images/".to_string(),
                    ..Default::default()
                },
            ]
        );
    }

    #[test]
    fn test_load_build_config_rejects_top_level_file_bare_path_src() {
        let repo = TempDir::new().unwrap();
        write_build_config(
            &repo,
            "build.yaml",
            r#"
type: build
image: "@base"
output: "out.qcow2"
files:
  - src: payload/file.txt
    dest: /tmp/payload
steps: []
"#,
        );
        let err = load_build_config(repo.path(), &repo.path().join("build.yaml")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("@-reference") || msg.contains("`@`-reference"),
            "error should mention @-reference: {msg}"
        );
    }

    #[test]
    fn test_load_build_config_rejects_top_level_file_relative_dest() {
        let repo = TempDir::new().unwrap();
        write_build_config(
            &repo,
            "build.yaml",
            r#"
type: build
image: "@base"
output: "out.qcow2"
files:
  - src: "@://payload/file.txt"
    dest: relative/path
steps: []
"#,
        );
        let err = load_build_config(repo.path(), &repo.path().join("build.yaml")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("absolute"),
            "error should mention absolute dest: {msg}"
        );
    }

    #[test]
    fn test_load_build_config_rejects_top_level_file_src_invalid_ref() {
        let repo = TempDir::new().unwrap();
        write_build_config(
            &repo,
            "build.yaml",
            r#"
type: build
image: "@base"
output: "out.qcow2"
files:
  - src: "@://secret/../etc/passwd"
    dest: /tmp/secret.txt
steps: []
"#,
        );
        let err = load_build_config(repo.path(), &repo.path().join("build.yaml")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("..") || msg.contains("invalid"),
            "error should mention traversal or invalid ref: {msg}"
        );
    }

    #[test]
    fn test_load_build_config_rejects_top_level_file_glob_with_non_directory_dest() {
        let repo = TempDir::new().unwrap();
        write_build_config(
            &repo,
            "build.yaml",
            r#"
type: build
image: "@base"
output: "out.qcow2"
files:
  - src: "@artifact://payload/*.tar"
    dest: /tmp/payload.tar
steps: []
"#,
        );
        let err = load_build_config(repo.path(), &repo.path().join("build.yaml")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("ending with '/'"),
            "error should mention directory dest: {msg}"
        );
    }

    #[test]
    fn test_load_build_config_rejects_top_level_file_unknown_field() {
        let repo = TempDir::new().unwrap();
        write_build_config(
            &repo,
            "build.yaml",
            r#"
type: build
image: "@base"
output: "out.qcow2"
files:
  - src: "@://payload/file.txt"
    dest: /tmp/file.txt
    bogus: 1
steps: []
"#,
        );
        let err = load_build_config(repo.path(), &repo.path().join("build.yaml")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("bogus") || msg.contains("unknown field"));
    }

    #[test]
    fn test_load_build_config_parses_top_level_file_permission_fields() {
        let repo = TempDir::new().unwrap();
        write_build_config(
            &repo,
            "build.yaml",
            r#"
type: build
image: "@base"
output: "out.qcow2"
files:
  - src: "@payload"
    dest: /usr/local/bin/file
    mode: "0755"
    owner: root
    group: root
    overwrite: true
    parents: true
steps: []
"#,
        );
        let config = load_build_config(repo.path(), &repo.path().join("build.yaml")).unwrap();
        assert_eq!(config.files.len(), 1);
        let file = &config.files[0];
        assert_eq!(file.mode.as_deref(), Some("0755"));
        assert_eq!(file.owner.as_deref(), Some("root"));
        assert_eq!(file.group.as_deref(), Some("root"));
        assert_eq!(file.overwrite, Some(true));
        assert_eq!(file.parents, Some(true));
    }

    #[test]
    fn test_load_build_config_rejects_top_level_file_invalid_mode() {
        let repo = TempDir::new().unwrap();
        write_build_config(
            &repo,
            "build.yaml",
            r#"
type: build
image: "@base"
output: "out.qcow2"
files:
  - src: "@payload"
    dest: /tmp/file.txt
    mode: "abc"
steps: []
"#,
        );
        let err = load_build_config(repo.path(), &repo.path().join("build.yaml")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("mode") && msg.contains("octal"),
            "error should mention mode and octal: {msg}"
        );
    }

    #[test]
    fn test_load_build_config_rejects_top_level_file_owner_with_slash() {
        let repo = TempDir::new().unwrap();
        write_build_config(
            &repo,
            "build.yaml",
            r#"
type: build
image: "@base"
output: "out.qcow2"
files:
  - src: "@payload"
    dest: /tmp/file.txt
    owner: "root/admin"
steps: []
"#,
        );
        let err = load_build_config(repo.path(), &repo.path().join("build.yaml")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("owner") && msg.contains('/'),
            "error should mention owner and invalid char: {msg}"
        );
    }

    #[test]
    fn test_load_build_config_rejects_top_level_file_group_with_metachar() {
        let repo = TempDir::new().unwrap();
        write_build_config(
            &repo,
            "build.yaml",
            r#"
type: build
image: "@base"
output: "out.qcow2"
files:
  - src: "@payload"
    dest: /tmp/file.txt
    group: "adm;in"
steps: []
"#,
        );
        let err = load_build_config(repo.path(), &repo.path().join("build.yaml")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("group"), "error should mention group: {msg}");
    }

    // -----------------------------------------------------------------
    // cloud_init field tests (replaced bootcmd)
    // -----------------------------------------------------------------

    #[test]
    fn test_load_build_config_cloud_init_absent_is_none() {
        let repo = TempDir::new().unwrap();
        write_build_config(
            &repo,
            "build.yaml",
            "type: build\nimage: \"@base\"\noutput: \"out.qcow2\"\nsteps: []\n",
        );
        let config = load_build_config(repo.path(), &repo.path().join("build.yaml")).unwrap();
        assert!(
            config.cloud_init.is_none(),
            "absent cloud_init must deserialize as None"
        );
    }

    #[test]
    fn test_load_build_config_cloud_init_bootcmd_string_entries() {
        let repo = TempDir::new().unwrap();
        write_build_config(
            &repo,
            "build.yaml",
            r#"
type: build
image: "@base"
output: "out.qcow2"
steps: []
cloud_init:
  bootcmd:
    - echo hello
    - echo world
"#,
        );
        let config = load_build_config(repo.path(), &repo.path().join("build.yaml")).unwrap();
        let ci = config.cloud_init.expect("cloud_init must be Some");
        let bootcmd = ci
            .get(serde_yaml::Value::String("bootcmd".to_string()))
            .expect("bootcmd must be present in cloud_init");
        let entries = bootcmd.as_sequence().expect("bootcmd must be a sequence");
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].as_str(), Some("echo hello"));
        assert_eq!(entries[1].as_str(), Some("echo world"));
    }

    #[test]
    fn test_load_build_config_cloud_init_bootcmd_exec_entry() {
        let repo = TempDir::new().unwrap();
        write_build_config(
            &repo,
            "build.yaml",
            r#"
type: build
image: "@base"
output: "out.qcow2"
steps: []
cloud_init:
  bootcmd:
    - [ cloud-init-per, once, mask-stack, sh, -c, "systemctl mask a.service" ]
"#,
        );
        let config = load_build_config(repo.path(), &repo.path().join("build.yaml")).unwrap();
        let ci = config.cloud_init.expect("cloud_init must be Some");
        let bootcmd = ci
            .get(serde_yaml::Value::String("bootcmd".to_string()))
            .expect("bootcmd must be present");
        let entries = bootcmd.as_sequence().expect("bootcmd must be a sequence");
        assert_eq!(entries.len(), 1);
        let exec = entries[0]
            .as_sequence()
            .expect("first entry must be a sequence");
        assert_eq!(exec[0].as_str(), Some("cloud-init-per"));
        assert_eq!(exec[5].as_str(), Some("systemctl mask a.service"));
    }

    #[test]
    fn test_load_build_config_top_level_bootcmd_rejected_with_migration_error() {
        // top-level bootcmd: must produce a clear migration error.
        let repo = TempDir::new().unwrap();
        write_build_config(
            &repo,
            "build.yaml",
            r#"
type: build
image: "@base"
output: "out.qcow2"
steps: []
bootcmd:
  - echo hello
"#,
        );
        let err = load_build_config(repo.path(), &repo.path().join("build.yaml")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("cloud_init"),
            "migration error must mention cloud_init: {msg}"
        );
        assert!(
            msg.contains("bootcmd"),
            "migration error must mention bootcmd: {msg}"
        );
    }

    #[test]
    fn test_load_test_config_top_level_bootcmd_rejected_with_migration_error() {
        // top-level bootcmd: must be rejected in test docs too.
        let repo = TempDir::new().unwrap();
        write_test_config(
            &repo,
            "test.yaml",
            r#"
type: test
steps: []
bootcmd:
  - echo hello
"#,
        );
        let err = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("cloud_init"),
            "migration error must mention cloud_init: {msg}"
        );
    }

    #[test]
    fn test_load_build_config_cloud_init_packages_accepted() {
        let repo = TempDir::new().unwrap();
        write_build_config(
            &repo,
            "build.yaml",
            r#"
type: build
image: "@base"
output: "out.qcow2"
steps: []
cloud_init:
  packages:
    - curl
    - git
"#,
        );
        let config = load_build_config(repo.path(), &repo.path().join("build.yaml")).unwrap();
        let ci = config.cloud_init.expect("cloud_init must be Some");
        let pkgs = ci
            .get(serde_yaml::Value::String("packages".to_string()))
            .expect("packages must be present")
            .as_sequence()
            .expect("packages must be a sequence");
        assert_eq!(pkgs.len(), 2);
        assert_eq!(pkgs[0].as_str(), Some("curl"));
        assert_eq!(pkgs[1].as_str(), Some("git"));
    }

    #[test]
    fn test_load_test_config_cloud_init_mounts_accepted() {
        // type: test also accepts cloud_init: (motivating tmpfs-on-test example).
        let repo = TempDir::new().unwrap();
        write_test_config(
            &repo,
            "test.yaml",
            r#"
type: test
steps: []
cloud_init:
  mounts:
    - [tmpfs, /var/cache/apt, tmpfs, "size=512M", "0", "0"]
"#,
        );
        let config = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap();
        let ci = config.cloud_init.expect("cloud_init must be Some");
        let mounts = ci
            .get(serde_yaml::Value::String("mounts".to_string()))
            .expect("mounts must be present")
            .as_sequence()
            .expect("mounts must be a sequence");
        assert_eq!(mounts.len(), 1);
    }

    #[test]
    fn test_cloud_init_write_files_source_rejected_ingress_guard() {
        // write_files with source: must be rejected (ingress guard).
        let repo = TempDir::new().unwrap();
        write_build_config(
            &repo,
            "build.yaml",
            r#"
type: build
image: "@base"
output: "out.qcow2"
steps: []
cloud_init:
  write_files:
    - path: /etc/myapp.conf
      source: file:///etc/host.conf
"#,
        );
        let err = load_build_config(repo.path(), &repo.path().join("build.yaml")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("source"),
            "ingress guard error must mention source: {msg}"
        );
        assert!(
            msg.contains("write_files"),
            "ingress guard error must mention write_files: {msg}"
        );
    }

    #[test]
    fn test_cloud_init_write_files_inline_content_allowed() {
        // write_files with content: is allowed (inline value, not host-path ingress).
        let repo = TempDir::new().unwrap();
        write_build_config(
            &repo,
            "build.yaml",
            r#"
type: build
image: "@base"
output: "out.qcow2"
steps: []
cloud_init:
  write_files:
    - path: /etc/myapp.conf
      content: "key=value\n"
"#,
        );
        let config = load_build_config(repo.path(), &repo.path().join("build.yaml")).unwrap();
        assert!(config.cloud_init.is_some(), "cloud_init must be accepted");
    }

    #[test]
    fn test_cloud_init_ssh_pwauth_false_rejected_harness_guard() {
        // ssh_pwauth: false must be rejected (harness guard).
        let repo = TempDir::new().unwrap();
        write_build_config(
            &repo,
            "build.yaml",
            r#"
type: build
image: "@base"
output: "out.qcow2"
steps: []
cloud_init:
  ssh_pwauth: false
"#,
        );
        let err = load_build_config(repo.path(), &repo.path().join("build.yaml")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("ssh_pwauth"),
            "harness guard error must mention ssh_pwauth: {msg}"
        );
    }

    #[test]
    fn test_cloud_init_schema_missing_binary_is_skipped() {
        let repo = TempDir::new().unwrap();
        write_build_config(
            &repo,
            "build.yaml",
            r#"
type: build
image: "@base"
output: "out.qcow2"
steps: []
cloud_init:
  users:
    - name: app
"#,
        );
        let (result, warnings) = with_warning_capture(|| {
            with_cloud_init_schema_mode(CloudInitSchemaMode::Warn, || {
                with_cloud_init_schema_check(schema_missing, || {
                    load_build_config(repo.path(), &repo.path().join("build.yaml"))
                })
            })
        });
        let config = result.expect("missing cloud-init binary should not fail config load");
        assert!(config.cloud_init.is_some());
        assert!(
            warnings.is_empty(),
            "missing cloud-init should be skipped without warnings: {warnings:?}"
        );
    }

    #[test]
    fn test_cloud_init_schema_invalid_warn_mode_emits_warning() {
        let repo = TempDir::new().unwrap();
        write_build_config(
            &repo,
            "build.yaml",
            r#"
type: build
image: "@base"
output: "out.qcow2"
steps: []
cloud_init:
  user: typo
"#,
        );
        let (result, warnings) = with_warning_capture(|| {
            with_cloud_init_schema_mode(CloudInitSchemaMode::Warn, || {
                with_cloud_init_schema_check(schema_invalid, || {
                    load_build_config(repo.path(), &repo.path().join("build.yaml"))
                })
            })
        });
        assert!(
            result.is_ok(),
            "warn mode should not fail cloud-init schema violations"
        );
        assert_eq!(warnings.len(), 1, "warn mode must emit one warning");
        assert!(
            warnings[0].contains("invalid cloud-config key"),
            "warning must include validator message: {}",
            warnings[0]
        );
    }

    #[test]
    fn test_cloud_init_schema_invalid_strict_mode_is_hard_error() {
        let repo = TempDir::new().unwrap();
        write_test_config(
            &repo,
            "test.yaml",
            r#"
type: test
steps: []
cloud_init:
  user: typo
"#,
        );
        let err = with_cloud_init_schema_mode(CloudInitSchemaMode::Strict, || {
            with_cloud_init_schema_check(schema_invalid, || {
                load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap_err()
            })
        });
        let msg = format!("{err:#}");
        assert!(
            msg.contains("cloud-init schema pre-validation failed"),
            "strict mode must hard-fail schema violations: {msg}"
        );
    }

    #[test]
    fn test_cloud_init_schema_valid_fragment_passes_in_all_modes() {
        for mode in [
            CloudInitSchemaMode::Off,
            CloudInitSchemaMode::Warn,
            CloudInitSchemaMode::Strict,
        ] {
            let repo = TempDir::new().unwrap();
            write_test_config(
                &repo,
                "test.yaml",
                r#"
type: test
steps: []
cloud_init:
  users:
    - name: app
"#,
            );
            let (result, warnings) = with_warning_capture(|| {
                with_cloud_init_schema_mode(mode, || {
                    with_cloud_init_schema_check(schema_pass, || {
                        load_test_config(repo.path(), &repo.path().join("test.yaml"))
                    })
                })
            });
            assert!(
                result.is_ok(),
                "valid cloud_init should pass in mode {mode:?}"
            );
            assert!(
                warnings.is_empty(),
                "valid cloud_init should not warn in mode {mode:?}: {warnings:?}"
            );
        }
    }

    #[test]
    fn test_cloud_init_guards_still_hard_fail_in_strict_mode() {
        let repo = TempDir::new().unwrap();
        write_build_config(
            &repo,
            "build.yaml",
            r#"
type: build
image: "@base"
output: "out.qcow2"
steps: []
cloud_init:
  ssh_pwauth: false
"#,
        );
        let err = with_cloud_init_schema_mode(CloudInitSchemaMode::Strict, || {
            with_cloud_init_schema_check(schema_pass, || {
                load_build_config(repo.path(), &repo.path().join("build.yaml")).unwrap_err()
            })
        });
        let msg = format!("{err:#}");
        assert!(
            msg.contains("ssh_pwauth"),
            "harness guard must still hard-fail independent of schema mode: {msg}"
        );
    }

    #[test]
    fn test_load_build_config_still_rejects_invalid_files_via_pipeline() {
        let repo = TempDir::new().unwrap();
        write_build_config(
            &repo,
            "build.yaml",
            r#"
type: build
image: "@base"
output: "out.qcow2"
steps: []
files:
  - src: "asset.txt"
    dest: relative/path
"#,
        );
        let err = load_build_config(repo.path(), &repo.path().join("build.yaml")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("files"),
            "invalid files must still be rejected by loader pipeline: {msg}"
        );
    }

    #[test]
    fn test_load_test_config_still_rejects_invalid_steps_via_pipeline() {
        let repo = TempDir::new().unwrap();
        write_test_config(
            &repo,
            "test.yaml",
            r#"
type: test
steps:
  - on: host
    name: host-step
    run: echo hi
"#,
        );
        let err = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("ports"),
            "invalid host step without ports must be rejected by loader pipeline: {msg}"
        );
    }

    // -----------------------------------------------------------------
    // compress field tests
    // -----------------------------------------------------------------

    #[test]
    fn test_load_build_config_compress_absent_is_none() {
        let repo = TempDir::new().unwrap();
        write_build_config(
            &repo,
            "build.yaml",
            "type: build\nimage: \"@base\"\noutput: \"out.qcow2\"\nsteps: []\n",
        );
        let config = load_build_config(repo.path(), &repo.path().join("build.yaml")).unwrap();
        assert!(
            config.compress.is_none(),
            "absent compress must deserialize as None"
        );
    }

    #[test]
    fn test_load_build_config_compress_enabled_true_no_cluster_size() {
        let repo = TempDir::new().unwrap();
        write_build_config(
            &repo,
            "build.yaml",
            "type: build\nimage: \"@base\"\noutput: \"out.qcow2\"\nsteps: []\ncompress:\n  enabled: true\n",
        );
        let config = load_build_config(repo.path(), &repo.path().join("build.yaml")).unwrap();
        let compress = config.compress.expect("compress should be Some");
        assert!(compress.enabled, "enabled must be true");
        assert_eq!(
            compress.compressor,
            CompressionType::Zstd,
            "compressor must default to zstd"
        );
        assert!(
            compress.compressor_args.is_empty(),
            "compressor_args must default to empty"
        );
        assert!(
            compress.compressor_opts.is_empty(),
            "compressor_opts must default to empty"
        );
        assert_eq!(
            compress.reclaim,
            ReclaimMode::None,
            "reclaim must default to none"
        );
    }

    #[test]
    fn test_load_build_config_compress_enabled_true_with_cluster_size_in_args() {
        let repo = TempDir::new().unwrap();
        write_build_config(
            &repo,
            "build.yaml",
            "type: build\nimage: \"@base\"\noutput: \"out.qcow2\"\nsteps: []\ncompress:\n  enabled: true\n  compressor_args:\n    cluster_size: \"1M\"\n",
        );
        let config = load_build_config(repo.path(), &repo.path().join("build.yaml")).unwrap();
        let compress = config.compress.expect("compress should be Some");
        assert!(compress.enabled);
        assert_eq!(compress.compressor, CompressionType::Zstd);
        assert_eq!(
            compress
                .compressor_args
                .get("cluster_size")
                .map(String::as_str),
            Some("1M")
        );
        assert_eq!(compress.reclaim, ReclaimMode::None);
    }

    #[test]
    fn test_load_build_config_compress_explicit_compressor_zstd() {
        let repo = TempDir::new().unwrap();
        write_build_config(
            &repo,
            "build.yaml",
            "type: build\nimage: \"@base\"\noutput: \"out.qcow2\"\nsteps: []\ncompress:\n  enabled: true\n  compressor: zstd\n",
        );
        let config = load_build_config(repo.path(), &repo.path().join("build.yaml")).unwrap();
        let compress = config.compress.expect("compress should be Some");
        assert!(compress.enabled);
        assert_eq!(compress.compressor, CompressionType::Zstd);
    }

    #[test]
    fn test_load_build_config_compress_explicit_compressor_zlib() {
        let repo = TempDir::new().unwrap();
        write_build_config(
            &repo,
            "build.yaml",
            "type: build\nimage: \"@base\"\noutput: \"out.qcow2\"\nsteps: []\ncompress:\n  enabled: true\n  compressor: zlib\n",
        );
        let config = load_build_config(repo.path(), &repo.path().join("build.yaml")).unwrap();
        let compress = config.compress.expect("compress should be Some");
        assert!(compress.enabled);
        assert_eq!(compress.compressor, CompressionType::Zlib);
    }

    #[test]
    fn test_load_build_config_compress_explicit_compressor_args() {
        let repo = TempDir::new().unwrap();
        write_build_config(
            &repo,
            "build.yaml",
            "type: build\nimage: \"@base\"\noutput: \"out.qcow2\"\nsteps: []\ncompress:\n  enabled: true\n  compressor_args:\n    cluster_size: \"1M\"\n",
        );
        let config = load_build_config(repo.path(), &repo.path().join("build.yaml")).unwrap();
        let compress = config.compress.expect("compress should be Some");
        assert_eq!(
            compress
                .compressor_args
                .get("cluster_size")
                .map(String::as_str),
            Some("1M")
        );
        assert_eq!(compress.compressor_args.len(), 1);
    }

    #[test]
    fn test_load_build_config_compress_explicit_compressor_opts() {
        let repo = TempDir::new().unwrap();
        write_build_config(
            &repo,
            "build.yaml",
            "type: build\nimage: \"@base\"\noutput: \"out.qcow2\"\nsteps: []\ncompress:\n  enabled: true\n  compressor_opts: \"-19 -T0\"\n",
        );
        let config = load_build_config(repo.path(), &repo.path().join("build.yaml")).unwrap();
        let compress = config.compress.expect("compress should be Some");
        assert_eq!(compress.compressor_opts, "-19 -T0");
    }

    #[test]
    fn test_load_build_config_compress_enabled_false() {
        let repo = TempDir::new().unwrap();
        write_build_config(
            &repo,
            "build.yaml",
            "type: build\nimage: \"@base\"\noutput: \"out.qcow2\"\nsteps: []\ncompress:\n  enabled: false\n",
        );
        let config = load_build_config(repo.path(), &repo.path().join("build.yaml")).unwrap();
        let compress = config.compress.expect("compress should be Some");
        assert!(!compress.enabled, "enabled must be false");
        assert_eq!(compress.compressor, CompressionType::Zstd);
        assert_eq!(compress.reclaim, ReclaimMode::None);
    }

    #[test]
    fn test_load_build_config_compress_reclaim_fstrim() {
        let repo = TempDir::new().unwrap();
        write_build_config(
            &repo,
            "build.yaml",
            "type: build\nimage: \"@base\"\noutput: \"out.qcow2\"\nsteps: []\ncompress:\n  enabled: true\n  reclaim: fstrim\n",
        );
        let config = load_build_config(repo.path(), &repo.path().join("build.yaml")).unwrap();
        let compress = config.compress.expect("compress should be Some");
        assert!(compress.enabled);
        assert_eq!(compress.reclaim, ReclaimMode::Fstrim);
    }

    #[test]
    fn test_load_build_config_compress_reclaim_discard() {
        let repo = TempDir::new().unwrap();
        write_build_config(
            &repo,
            "build.yaml",
            "type: build\nimage: \"@base\"\noutput: \"out.qcow2\"\nsteps: []\ncompress:\n  enabled: true\n  reclaim: discard\n",
        );
        let config = load_build_config(repo.path(), &repo.path().join("build.yaml")).unwrap();
        let compress = config.compress.expect("compress should be Some");
        assert!(compress.enabled);
        assert_eq!(compress.reclaim, ReclaimMode::Discard);
    }

    #[test]
    fn test_load_build_config_compress_reclaim_none() {
        let repo = TempDir::new().unwrap();
        write_build_config(
            &repo,
            "build.yaml",
            "type: build\nimage: \"@base\"\noutput: \"out.qcow2\"\nsteps: []\ncompress:\n  enabled: true\n  reclaim: none\n",
        );
        let config = load_build_config(repo.path(), &repo.path().join("build.yaml")).unwrap();
        let compress = config.compress.expect("compress should be Some");
        assert!(compress.enabled);
        assert_eq!(compress.reclaim, ReclaimMode::None);
    }

    #[test]
    fn test_load_build_config_compress_reclaim_sparsify_is_unknown_variant() {
        let repo = TempDir::new().unwrap();
        write_build_config(
            &repo,
            "build.yaml",
            "type: build\nimage: \"@base\"\noutput: \"out.qcow2\"\nsteps: []\ncompress:\n  enabled: true\n  reclaim: sparsify\n",
        );
        let err = load_build_config(repo.path(), &repo.path().join("build.yaml")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("sparsify") || msg.contains("unknown variant"),
            "sparsify reclaim mode should now be rejected as unknown variant: {msg}"
        );
    }

    #[test]
    fn test_load_build_config_compress_enabled_false_reclaim_fstrim() {
        let repo = TempDir::new().unwrap();
        write_build_config(
            &repo,
            "build.yaml",
            "type: build\nimage: \"@base\"\noutput: \"out.qcow2\"\nsteps: []\ncompress:\n  enabled: false\n  reclaim: fstrim\n",
        );
        let config = load_build_config(repo.path(), &repo.path().join("build.yaml")).unwrap();
        let compress = config.compress.expect("compress should be Some");
        assert!(!compress.enabled);
        assert_eq!(compress.reclaim, ReclaimMode::Fstrim);
    }

    #[test]
    fn test_load_build_config_compress_missing_enabled_is_error() {
        let repo = TempDir::new().unwrap();
        write_build_config(
            &repo,
            "build.yaml",
            "type: build\nimage: \"@base\"\noutput: \"out.qcow2\"\nsteps: []\ncompress:\n  reclaim: fstrim\n",
        );
        let err = load_build_config(repo.path(), &repo.path().join("build.yaml")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("enabled") || msg.contains("missing"),
            "error should mention missing enabled field: {msg}"
        );
    }

    #[test]
    fn test_load_build_config_compress_reclaim_missing_enabled_is_error() {
        let repo = TempDir::new().unwrap();
        write_build_config(
            &repo,
            "build.yaml",
            "type: build\nimage: \"@base\"\noutput: \"out.qcow2\"\nsteps: []\ncompress:\n  reclaim: fstrim\n",
        );
        let err = load_build_config(repo.path(), &repo.path().join("build.yaml")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("enabled") || msg.contains("missing"),
            "error should mention missing enabled field: {msg}"
        );
    }

    #[test]
    fn test_load_build_config_compress_unknown_field_is_error() {
        let repo = TempDir::new().unwrap();
        write_build_config(
            &repo,
            "build.yaml",
            "type: build\nimage: \"@base\"\noutput: \"out.qcow2\"\nsteps: []\ncompress:\n  enabled: true\n  bogus: 1\n",
        );
        let err = load_build_config(repo.path(), &repo.path().join("build.yaml")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("bogus") || msg.contains("unknown"),
            "error should mention unknown field: {msg}"
        );
    }

    #[test]
    fn test_load_build_config_compress_reclaim_unknown_value_is_error() {
        let repo = TempDir::new().unwrap();
        write_build_config(
            &repo,
            "build.yaml",
            "type: build\nimage: \"@base\"\noutput: \"out.qcow2\"\nsteps: []\ncompress:\n  enabled: true\n  reclaim: bogus\n",
        );
        let err = load_build_config(repo.path(), &repo.path().join("build.yaml")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("bogus") || msg.contains("unknown variant"),
            "error should mention reclaim enum variant parse failure: {msg}"
        );
    }

    #[test]
    fn test_load_build_config_compress_compressor_unknown_value_is_error() {
        let repo = TempDir::new().unwrap();
        write_build_config(
            &repo,
            "build.yaml",
            "type: build\nimage: \"@base\"\noutput: \"out.qcow2\"\nsteps: []\ncompress:\n  enabled: true\n  compressor: bogus\n",
        );
        let err = load_build_config(repo.path(), &repo.path().join("build.yaml")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("bogus") || msg.contains("unknown variant"),
            "error should mention compressor enum variant parse failure: {msg}"
        );
    }

    #[test]
    fn test_load_build_config_compress_compression_type_key_is_unknown_field() {
        let repo = TempDir::new().unwrap();
        write_build_config(
            &repo,
            "build.yaml",
            "type: build\nimage: \"@base\"\noutput: \"out.qcow2\"\nsteps: []\ncompress:\n  enabled: true\n  compression_type: zstd\n",
        );
        let err = load_build_config(repo.path(), &repo.path().join("build.yaml")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("compression_type") || msg.contains("unknown field"),
            "error should mention the removed compression_type key: {msg}"
        );
    }

    #[test]
    fn test_load_build_config_compress_cluster_size_top_level_is_unknown_field() {
        let repo = TempDir::new().unwrap();
        write_build_config(
            &repo,
            "build.yaml",
            "type: build\nimage: \"@base\"\noutput: \"out.qcow2\"\nsteps: []\ncompress:\n  enabled: true\n  cluster_size: \"1M\"\n",
        );
        let err = load_build_config(repo.path(), &repo.path().join("build.yaml")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("cluster_size") || msg.contains("unknown field"),
            "cluster_size at top level should now be an unknown field error: {msg}"
        );
    }

    #[test]
    fn test_load_build_config_compress_reclaim_typo_key_is_error() {
        let repo = TempDir::new().unwrap();
        write_build_config(
            &repo,
            "build.yaml",
            "type: build\nimage: \"@base\"\noutput: \"out.qcow2\"\nsteps: []\ncompress:\n  enabled: true\n  recliam: fstrim\n",
        );
        let err = load_build_config(repo.path(), &repo.path().join("build.yaml")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("recliam") || msg.contains("unknown field"),
            "error should mention typo key in strict compress map: {msg}"
        );
    }

    #[test]
    fn test_load_test_config_rejects_compress_section() {
        let repo = TempDir::new().unwrap();
        std::fs::write(
            repo.path().join("test.yaml"),
            "type: test\ncompress:\n  enabled: true\nsteps: []\n",
        )
        .unwrap();
        let err = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("compress") && msg.contains("type: test"),
            "error should reject compress in test doc: {msg}"
        );
    }

    #[test]
    fn test_load_fragment_rejects_compress_section() {
        let repo = TempDir::new().unwrap();
        std::fs::write(
            repo.path().join("frag.yaml"),
            "type: fragment\ncompress:\n  enabled: true\nsteps: []\n",
        )
        .unwrap();
        std::fs::write(
            repo.path().join("test.yaml"),
            "type: test\nsteps:\n  - uses: \"@://frag.yaml\"\n",
        )
        .unwrap();
        let err = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("compress"),
            "error should reject compress in fragment doc: {msg}"
        );
    }

    #[test]
    fn test_load_fragment_rejects_files_section() {
        let repo = TempDir::new().unwrap();
        std::fs::write(
            repo.path().join("frag.yaml"),
            "type: fragment\nfiles:\n  - src: payload/file.txt\n    dest: /tmp/file.txt\nsteps: []\n",
        )
        .unwrap();
        std::fs::write(
            repo.path().join("test.yaml"),
            "type: test\nsteps:\n  - uses: \"@://frag.yaml\"\n",
        )
        .unwrap();
        let err = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("files"),
            "error should reject files in fragment doc: {msg}"
        );
    }

    #[test]
    fn test_load_test_config_files_absent_is_empty() {
        let repo = TempDir::new().unwrap();
        std::fs::write(repo.path().join("test.yaml"), "type: test\nsteps: []\n").unwrap();
        let config = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap();
        assert!(config.files.is_empty(), "files should default to empty");
    }

    #[test]
    fn test_load_test_config_parses_top_level_files() {
        let repo = TempDir::new().unwrap();
        std::fs::write(
            repo.path().join("test.yaml"),
            r#"
type: test
files:
  - src: "@://fixtures/envoy/**/*.yaml"
    dest: /tmp/envoy/
steps: []
"#,
        )
        .unwrap();
        let config = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap();
        assert_eq!(
            config.files,
            vec![FileEntry {
                src: "@://fixtures/envoy/**/*.yaml".to_string(),
                dest: "/tmp/envoy/".to_string(),
                ..Default::default()
            }]
        );
    }

    #[test]
    fn test_load_test_config_defaults_timeouts() {
        let repo = TempDir::new().unwrap();
        std::fs::write(repo.path().join("test.yaml"), "type: test\nsteps: []\n").unwrap();
        let config = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap();
        assert_eq!(config.step_timeout, 300);
        assert_eq!(config.timeout, 1800);
        assert_eq!(config.cloud_init_timeout, 300);
    }

    #[test]
    fn test_load_test_config_overrides_timeouts() {
        let repo = TempDir::new().unwrap();
        std::fs::write(
            repo.path().join("test.yaml"),
            "type: test\nstep_timeout: 600\ntimeout: 2400\nsteps: []\n",
        )
        .unwrap();
        let config = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap();
        assert_eq!(config.step_timeout, 600);
        assert_eq!(config.timeout, 2400);
        assert_eq!(config.cloud_init_timeout, 300);
    }

    #[test]
    fn test_load_build_config_rejects_wrong_type() {
        let repo = TempDir::new().unwrap();
        write_build_config(&repo, "build.yaml", "type: test\nsteps: []\n");
        let err = load_build_config(repo.path(), &repo.path().join("build.yaml")).unwrap_err();
        assert!(
            format!("{err:#}").contains("type: test"),
            "error should mention the actual type: {err:#}"
        );
    }

    #[test]
    fn test_load_build_config_rejects_fragment_type() {
        let repo = TempDir::new().unwrap();
        write_build_config(&repo, "build.yaml", "type: fragment\nsteps: []\n");
        let err = load_build_config(repo.path(), &repo.path().join("build.yaml")).unwrap_err();
        assert!(format!("{err:#}").contains("type: fragment"));
    }

    #[test]
    fn test_load_build_config_rejects_ports_section() {
        let repo = TempDir::new().unwrap();
        write_build_config(
            &repo,
            "build.yaml",
            "type: build\nports:\n  - 80\nsteps: []\n",
        );
        let err = load_build_config(repo.path(), &repo.path().join("build.yaml")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("ports") && msg.contains("type: build"),
            "error should mention the offending key and document type: {msg}"
        );
    }

    #[test]
    fn test_load_build_config_rejects_isos_section() {
        let repo = TempDir::new().unwrap();
        write_build_config(
            &repo,
            "build.yaml",
            "type: build\nisos:\n  - some.iso\nsteps: []\n",
        );
        let err = load_build_config(repo.path(), &repo.path().join("build.yaml")).unwrap_err();
        assert!(format!("{err:#}").contains("isos"));
    }

    #[test]
    fn test_load_build_config_rejects_diagnostics_units_section() {
        let repo = TempDir::new().unwrap();
        write_build_config(
            &repo,
            "build.yaml",
            "type: build\ndiagnostics_units:\n  - foo\nsteps: []\n",
        );
        let err = load_build_config(repo.path(), &repo.path().join("build.yaml")).unwrap_err();
        assert!(format!("{err:#}").contains("diagnostics_units"));
    }

    #[test]
    fn test_load_build_config_requires_image() {
        let repo = TempDir::new().unwrap();
        write_build_config(
            &repo,
            "build.yaml",
            "type: build\noutput: \"out.qcow2\"\nsteps: []\n",
        );
        let err = load_build_config(repo.path(), &repo.path().join("build.yaml")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("'image'") && msg.contains("required"),
            "error should mention missing image: {msg}"
        );
    }

    #[test]
    fn test_load_build_config_requires_output() {
        let repo = TempDir::new().unwrap();
        write_build_config(
            &repo,
            "build.yaml",
            "type: build\nimage: \"@base\"\nsteps: []\n",
        );
        let err = load_build_config(repo.path(), &repo.path().join("build.yaml")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("'output'") && msg.contains("required"),
            "error should mention missing output: {msg}"
        );
    }

    #[test]
    fn test_load_build_config_rejects_non_filename_output() {
        let repo = TempDir::new().unwrap();
        write_build_config(
            &repo,
            "build.yaml",
            "type: build\nimage: \"@base\"\noutput: \"foo/bar.qcow2\"\nsteps: []\n",
        );
        let err = load_build_config(repo.path(), &repo.path().join("build.yaml")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("bare filename"),
            "error should mention bare filename requirement: {msg}"
        );

        write_build_config(
            &repo,
            "build-dotdot.yaml",
            "type: build\nimage: \"@base\"\noutput: \"../bar.qcow2\"\nsteps: []\n",
        );
        let err =
            load_build_config(repo.path(), &repo.path().join("build-dotdot.yaml")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("bare filename"),
            "error should mention bare filename requirement for dotdot: {msg}"
        );
    }

    #[test]
    fn test_load_build_config_rejects_empty_image() {
        let repo = TempDir::new().unwrap();
        write_build_config(
            &repo,
            "build.yaml",
            "type: build\nimage: \"\"\noutput: \"out.qcow2\"\nsteps: []\n",
        );
        let err = load_build_config(repo.path(), &repo.path().join("build.yaml")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("'image'") && msg.contains("required"),
            "error should mention empty image: {msg}"
        );
    }

    #[test]
    fn test_fragment_rejects_image() {
        let repo = TempDir::new().unwrap();
        std::fs::write(
            repo.path().join("frag.yaml"),
            "type: fragment\nimage: \"@debian-base\"\nsteps: []\n",
        )
        .unwrap();
        std::fs::write(
            repo.path().join("test.yaml"),
            "type: test\nsteps:\n  - uses: \"@://frag.yaml\"\n",
        )
        .unwrap();
        let err = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("image"), "error should mention image: {msg}");
    }

    #[test]
    fn test_load_test_config_accepts_image_section() {
        let repo = TempDir::new().unwrap();
        std::fs::write(
            repo.path().join("test.yaml"),
            "type: test\nimage: \"@artifact://foo.qcow2\"\nsteps: []\n",
        )
        .unwrap();
        let config = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap();
        assert_eq!(
            config.image,
            Some(Reference::Artifact {
                path: Some(PathBuf::from("foo.qcow2"))
            })
        );
    }

    #[test]
    fn test_load_test_config_rejects_output_section() {
        let repo = TempDir::new().unwrap();
        std::fs::write(
            repo.path().join("test.yaml"),
            "type: test\noutput: \"out.qcow2\"\nsteps: []\n",
        )
        .unwrap();
        let err = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("output"), "error should mention output: {msg}");
    }

    #[test]
    fn test_fragment_rejects_output() {
        let repo = TempDir::new().unwrap();
        std::fs::write(
            repo.path().join("frag.yaml"),
            "type: fragment\noutput: \"out.qcow2\"\nsteps: []\n",
        )
        .unwrap();
        std::fs::write(
            repo.path().join("test.yaml"),
            "type: test\nsteps:\n  - uses: \"@://frag.yaml\"\n",
        )
        .unwrap();
        let err = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("output"), "error should mention output: {msg}");
    }

    #[test]
    fn test_load_test_config_rejects_disk_size_section() {
        let repo = TempDir::new().unwrap();
        std::fs::write(
            repo.path().join("test.yaml"),
            "type: test\ndisk_size: \"20G\"\nsteps: []\n",
        )
        .unwrap();
        let err = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("disk_size") && msg.contains("type: test"),
            "error should mention the offending key and document type: {msg}"
        );
    }

    #[test]
    fn test_load_test_config_rejects_memsize_section() {
        let repo = TempDir::new().unwrap();
        std::fs::write(
            repo.path().join("test.yaml"),
            "type: test\nmemsize: 8192\nsteps: []\n",
        )
        .unwrap();
        let err = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap_err();
        assert!(format!("{err:#}").contains("memsize"));
    }

    #[test]
    fn test_load_test_config_rejects_smp_section() {
        let repo = TempDir::new().unwrap();
        std::fs::write(
            repo.path().join("test.yaml"),
            "type: test\nsmp: 8\nsteps: []\n",
        )
        .unwrap();
        let err = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap_err();
        assert!(format!("{err:#}").contains("smp"));
    }

    #[test]
    fn test_fragment_rejects_disk_size() {
        let repo = TempDir::new().unwrap();
        std::fs::write(
            repo.path().join("frag.yaml"),
            "type: fragment\ndisk_size: \"20G\"\nsteps: []\n",
        )
        .unwrap();
        std::fs::write(
            repo.path().join("test.yaml"),
            "type: test\nsteps:\n  - uses: \"@://frag.yaml\"\n",
        )
        .unwrap();
        let err = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap_err();
        assert!(format!("{err:#}").contains("disk_size"));
    }

    #[test]
    fn test_fragment_rejects_memsize() {
        let repo = TempDir::new().unwrap();
        std::fs::write(
            repo.path().join("frag.yaml"),
            "type: fragment\nmemsize: 8192\nsteps: []\n",
        )
        .unwrap();
        std::fs::write(
            repo.path().join("test.yaml"),
            "type: test\nsteps:\n  - uses: \"@://frag.yaml\"\n",
        )
        .unwrap();
        let err = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap_err();
        assert!(format!("{err:#}").contains("memsize"));
    }

    #[test]
    fn test_fragment_rejects_smp() {
        let repo = TempDir::new().unwrap();
        std::fs::write(
            repo.path().join("frag.yaml"),
            "type: fragment\nsmp: 8\nsteps: []\n",
        )
        .unwrap();
        std::fs::write(
            repo.path().join("test.yaml"),
            "type: test\nsteps:\n  - uses: \"@://frag.yaml\"\n",
        )
        .unwrap();
        let err = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap_err();
        assert!(format!("{err:#}").contains("smp"));
    }

    #[test]
    fn test_fragment_rejects_step_timeout() {
        let repo = TempDir::new().unwrap();
        std::fs::write(
            repo.path().join("frag.yaml"),
            "type: fragment\nstep_timeout: 600\nsteps: []\n",
        )
        .unwrap();
        std::fs::write(
            repo.path().join("test.yaml"),
            "type: test\nsteps:\n  - uses: \"@://frag.yaml\"\n",
        )
        .unwrap();
        let err = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap_err();
        assert!(format!("{err:#}").contains("step_timeout"));
    }

    #[test]
    fn test_fragment_rejects_timeout() {
        let repo = TempDir::new().unwrap();
        std::fs::write(
            repo.path().join("frag.yaml"),
            "type: fragment\ntimeout: 600\nsteps: []\n",
        )
        .unwrap();
        std::fs::write(
            repo.path().join("test.yaml"),
            "type: test\nsteps:\n  - uses: \"@://frag.yaml\"\n",
        )
        .unwrap();
        let err = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap_err();
        assert!(format!("{err:#}").contains("timeout"));
    }

    #[test]
    fn test_build_config_accepts_fragment_via_uses() {
        let repo = TempDir::new().unwrap();
        std::fs::write(
            repo.path().join("frag.yaml"),
            r#"
type: fragment
steps:
  - on: guest
    name: frag-step
    timeout: 42
    run: echo from-fragment
"#,
        )
        .unwrap();
        write_build_config(
            &repo,
            "build.yaml",
            r#"
type: build
image: "@debian-base"
output: "out.qcow2"
steps:
  - uses: "@://frag.yaml"
"#,
        );
        let config = load_build_config(repo.path(), &repo.path().join("build.yaml")).unwrap();
        assert_eq!(config.steps.len(), 1);
        assert_eq!(run_ref(&config.steps[0]).name, "frag-step");
        assert_eq!(run_ref(&config.steps[0]).timeout, Some(42));
    }

    #[test]
    fn test_build_config_preserves_fragment_sudo_via_uses() {
        let repo = TempDir::new().unwrap();
        std::fs::write(
            repo.path().join("frag.yaml"),
            r#"
type: fragment
steps:
  - on: guest
    name: frag-step
    sudo: true
    run: echo from-fragment
"#,
        )
        .unwrap();
        write_build_config(
            &repo,
            "build.yaml",
            r#"
type: build
image: "@debian-base"
output: "out.qcow2"
steps:
  - uses: "@://frag.yaml"
"#,
        );
        let config = load_build_config(repo.path(), &repo.path().join("build.yaml")).unwrap();
        assert_eq!(config.steps.len(), 1);
        assert_eq!(run_ref(&config.steps[0]).name, "frag-step");
        assert_eq!(run_ref(&config.steps[0]).sudo, Some(true));
    }

    #[test]
    fn test_build_config_fragment_input_substitution_preserves_step_timeout() {
        let repo = TempDir::new().unwrap();
        std::fs::write(
            repo.path().join("frag.yaml"),
            r#"
type: fragment
inputs:
  seconds:
    type: number
    required: true
steps:
  - on: guest
    name: frag-step
    timeout: ${{ inputs.seconds }}
    run: echo from-fragment
"#,
        )
        .unwrap();
        write_build_config(
            &repo,
            "build.yaml",
            r#"
type: build
image: "@debian-base"
output: "out.qcow2"
steps:
  - uses: "@://frag.yaml"
    with:
      seconds: "75"
"#,
        );
        let config = load_build_config(repo.path(), &repo.path().join("build.yaml")).unwrap();
        assert_eq!(run_ref(&config.steps[0]).timeout, Some(75));
    }

    #[test]
    fn test_load_test_config_rejects_non_positive_document_timeouts() {
        let repo = TempDir::new().unwrap();
        for (name, content, needle) in [
            (
                "test-zero-step-timeout.yaml",
                "type: test\nstep_timeout: 0\nsteps: []\n",
                "positive integer",
            ),
            (
                "test-negative-timeout.yaml",
                "type: test\ntimeout: -1\nsteps: []\n",
                "positive integer",
            ),
        ] {
            std::fs::write(repo.path().join(name), content).unwrap();
            let err = load_test_config(repo.path(), &repo.path().join(name)).unwrap_err();
            assert!(
                format!("{err:#}").contains(needle),
                "error should mention invalid timeout value: {err:#}"
            );
        }
    }

    #[test]
    fn test_load_build_config_rejects_non_positive_timeouts() {
        let repo = TempDir::new().unwrap();
        for (name, content, needle) in [
            (
                "build-zero-step-timeout.yaml",
                "type: build\nstep_timeout: 0\nsteps: []\n",
                "positive integer",
            ),
            (
                "build-negative-step-timeout.yaml",
                "type: build\nsteps:\n  - on: host\n    name: slow\n    timeout: -5\n    run: echo ok\n",
                "positive integer",
            ),
        ] {
            write_build_config(&repo, name, content);
            let err = load_build_config(repo.path(), &repo.path().join(name)).unwrap_err();
            assert!(
                format!("{err:#}").contains(needle),
                "error should mention invalid timeout value: {err:#}"
            );
        }
    }

    #[test]
    fn test_build_config_cannot_be_used_as_fragment() {
        let repo = TempDir::new().unwrap();
        write_build_config(
            &repo,
            "build-base.yaml",
            "type: build\nsteps:\n  - on: guest\n    name: s\n    run: echo ok\n",
        );
        std::fs::write(
            repo.path().join("test.yaml"),
            "type: test\nsteps:\n  - uses: \"@://build-base.yaml\"\n",
        )
        .unwrap();
        let err = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("not a consumable fragment"),
            "error should reject build doc as fragment: {msg}"
        );
    }

    // --- validate_build_steps ---

    #[test]
    fn test_validate_build_steps_accepts_guest_step() {
        let steps = vec![make_step(StepTarget::Guest, "s")];
        assert!(validate_build_steps(&steps).is_ok());
    }

    #[test]
    fn test_validate_build_steps_accepts_host_step_without_ports() {
        // Unlike test, build does not require ports for host steps.
        let steps = vec![make_step(StepTarget::Host, "h")];
        assert!(validate_build_steps(&steps).is_ok());
    }

    #[test]
    fn test_validate_build_steps_rejects_host_step_with_sudo() {
        let mut step = make_step(StepTarget::Host, "host-root");
        let TestStep::Run(run) = &mut step else {
            panic!("expected run step");
        };
        run.sudo = Some(true);
        let err = validate_build_steps(&[step]).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("host-root"),
            "error should mention step name: {msg}"
        );
        assert!(msg.contains("sudo"), "error should mention sudo: {msg}");
        assert!(msg.contains("guest"), "error should mention guest: {msg}");
    }

    #[test]
    fn test_validate_build_steps_accepts_host_step_with_explicit_sudo_false() {
        let mut step = make_step(StepTarget::Host, "host-unprivileged");
        let TestStep::Run(run) = &mut step else {
            panic!("expected run step");
        };
        run.sudo = Some(false);
        assert!(validate_build_steps(&[step]).is_ok());
    }

    #[test]
    fn test_validate_build_steps_accepts_guest_step_with_sudo() {
        let mut step = make_step(StepTarget::Guest, "guest-root");
        let TestStep::Run(run) = &mut step else {
            panic!("expected run step");
        };
        run.sudo = Some(true);
        assert!(validate_build_steps(&[step]).is_ok());
    }

    #[test]
    fn test_validate_build_steps_rejects_bad_shell() {
        let mut step = make_step(StepTarget::Guest, "bad-shell");
        let TestStep::Run(run) = &mut step else {
            panic!("expected run step");
        };
        run.shell = Some("fish".to_string());
        let err = validate_build_steps(&[step]).unwrap_err();
        assert!(format!("{err:#}").contains("fish"));
    }

    #[test]
    fn test_build_step_deserialize_archive_shape() {
        let step: TestStep = serde_yaml::from_str(
            r#"
archive:
  src: "@some-tool"
  into: some-tool
  name: unpack-some-tool
"#,
        )
        .unwrap();
        let TestStep::Archive(archive) = step else {
            panic!("expected archive step");
        };
        assert_eq!(archive.archive.src, "@some-tool");
        assert_eq!(archive.archive.into.as_deref(), Some("some-tool"));
        assert_eq!(archive.archive.name.as_deref(), Some("unpack-some-tool"));
    }

    #[test]
    fn test_build_step_deserialize_run_shape_still_works() {
        let step: TestStep = serde_yaml::from_str(
            r#"
on: guest
name: run-it
run: echo ok
"#,
        )
        .unwrap();
        let TestStep::Run(step) = step else {
            panic!("expected run step");
        };
        assert_eq!(step.name, "run-it");
        assert_eq!(step.run, "echo ok");
    }

    #[test]
    fn test_validate_build_steps_accepts_archive_step() {
        let steps = vec![TestStep::Archive(ArchiveStep {
            archive: ArchiveStepSpec {
                src: "@some-tool".to_string(),
                into: Some("some-tool".to_string()),
                name: Some("unpack".to_string()),
                dest: None,
            },
            target: None,
            run: None,
            timeout: None,
            shell: None,
        })];
        assert!(validate_build_steps(&steps).is_ok());
    }

    #[test]
    fn test_validate_build_steps_rejects_archive_empty_src() {
        let steps = vec![TestStep::Archive(ArchiveStep {
            archive: ArchiveStepSpec {
                src: "   ".to_string(),
                into: None,
                name: Some("bad-archive".to_string()),
                dest: None,
            },
            target: None,
            run: None,
            timeout: None,
            shell: None,
        })];
        let err = validate_build_steps(&steps).unwrap_err();
        assert!(format!("{err:#}").contains("src"));
        assert!(format!("{err:#}").contains("bad-archive"));
    }

    #[test]
    fn test_validate_build_steps_rejects_archive_without_at_prefix() {
        let steps = vec![TestStep::Archive(ArchiveStep {
            archive: ArchiveStepSpec {
                src: "some-tool".to_string(),
                into: None,
                name: Some("bad-archive".to_string()),
                dest: None,
            },
            target: None,
            run: None,
            timeout: None,
            shell: None,
        })];
        let err = validate_build_steps(&steps).unwrap_err();
        assert!(format!("{err:#}").contains("must start with '@'"));
    }

    #[test]
    fn test_validate_build_steps_rejects_archive_with_forbidden_fields() {
        let steps = vec![TestStep::Archive(ArchiveStep {
            archive: ArchiveStepSpec {
                src: "@some-tool".to_string(),
                into: None,
                name: Some("bad-archive".to_string()),
                dest: None,
            },
            target: Some(StepTarget::Host),
            run: Some("echo hi".to_string()),
            timeout: Some(30),
            shell: Some("bash".to_string()),
        })];
        let err = validate_build_steps(&steps).unwrap_err();
        // run/shell/timeout are still forbidden regardless of on: host.
        assert!(
            format!("{err:#}").contains("run")
                || format!("{err:#}").contains("shell")
                || format!("{err:#}").contains("timeout"),
            "error should mention a forbidden field: {err:#}"
        );
    }

    #[test]
    fn test_validate_build_steps_rejects_archive_src_traversal_scheme() {
        let steps = vec![TestStep::Archive(ArchiveStep {
            archive: ArchiveStepSpec {
                src: "@://provider/asset".to_string(),
                into: None,
                name: Some("bad-archive".to_string()),
                dest: None,
            },
            target: None,
            run: None,
            timeout: None,
            shell: None,
        })];
        let err = validate_build_steps(&steps).unwrap_err();
        assert!(format!("{err:#}").contains("@://"));
    }

    #[test]
    fn test_validate_build_steps_accepts_explicit_on_host_archive_step() {
        // on: host is now a legal explicit spelling of the default.
        let steps = vec![TestStep::Archive(ArchiveStep {
            archive: ArchiveStepSpec {
                src: "@some-tool".to_string(),
                into: None,
                name: Some("fetch-tool".to_string()),
                dest: None,
            },
            target: Some(StepTarget::Host),
            run: None,
            timeout: None,
            shell: None,
        })];
        assert!(validate_build_steps(&steps).is_ok());
    }

    #[test]
    fn test_validate_build_steps_accepts_guest_archive_with_absolute_dest() {
        let steps = vec![TestStep::Archive(ArchiveStep {
            archive: ArchiveStepSpec {
                src: "@some-tool".to_string(),
                into: None,
                name: Some("install-tool".to_string()),
                dest: Some("/var/lib/foo".to_string()),
            },
            target: Some(StepTarget::Guest),
            run: None,
            timeout: None,
            shell: None,
        })];
        assert!(validate_build_steps(&steps).is_ok());
    }

    #[test]
    fn test_validate_build_steps_rejects_guest_archive_without_dest() {
        let steps = vec![TestStep::Archive(ArchiveStep {
            archive: ArchiveStepSpec {
                src: "@some-tool".to_string(),
                into: None,
                name: Some("bad-guest".to_string()),
                dest: None,
            },
            target: Some(StepTarget::Guest),
            run: None,
            timeout: None,
            shell: None,
        })];
        let err = validate_build_steps(&steps).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("dest"), "error should mention 'dest': {msg}");
        assert!(
            msg.contains("bad-guest"),
            "error should mention step name: {msg}"
        );
    }

    #[test]
    fn test_validate_build_steps_rejects_guest_archive_with_relative_dest() {
        let steps = vec![TestStep::Archive(ArchiveStep {
            archive: ArchiveStepSpec {
                src: "@some-tool".to_string(),
                into: None,
                name: Some("bad-dest".to_string()),
                dest: Some("relative/path".to_string()),
            },
            target: Some(StepTarget::Guest),
            run: None,
            timeout: None,
            shell: None,
        })];
        let err = validate_build_steps(&steps).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("absolute"),
            "error should mention absolute path: {msg}"
        );
    }

    #[test]
    fn test_validate_build_steps_rejects_host_archive_with_dest() {
        // dest is only valid on on: guest — reject it for on: host or omitted.
        for (label, target) in [("on: host", Some(StepTarget::Host)), ("on: omitted", None)] {
            let steps = vec![TestStep::Archive(ArchiveStep {
                archive: ArchiveStepSpec {
                    src: "@some-tool".to_string(),
                    into: None,
                    name: Some("bad-dest".to_string()),
                    dest: Some("/var/lib/foo".to_string()),
                },
                target,
                run: None,
                timeout: None,
                shell: None,
            })];
            let err = validate_build_steps(&steps).unwrap_err();
            let msg = format!("{err:#}");
            assert!(
                msg.contains("dest"),
                "error should mention 'dest' ({label}): {msg}"
            );
        }
    }

    #[test]
    fn test_build_step_deserialize_archive_guest_mode() {
        let step: TestStep = serde_yaml::from_str(
            r#"
on: guest
archive:
  src: "@some-tool"
  name: install-some-tool
  dest: /var/lib/foo
"#,
        )
        .unwrap();
        let TestStep::Archive(archive) = step else {
            panic!("expected archive step");
        };
        assert_eq!(archive.target, Some(StepTarget::Guest));
        assert_eq!(archive.archive.src, "@some-tool");
        assert_eq!(archive.archive.name.as_deref(), Some("install-some-tool"));
        assert_eq!(archive.archive.dest.as_deref(), Some("/var/lib/foo"));
    }

    #[test]
    fn test_build_step_deserialize_archive_host_mode_dest_absent() {
        // Host-mode archive (on: omitted) keeps dest absent.
        let step: TestStep = serde_yaml::from_str(
            r#"
archive:
  src: "@some-tool"
  into: some-tool
  name: unpack-some-tool
"#,
        )
        .unwrap();
        let TestStep::Archive(archive) = step else {
            panic!("expected archive step");
        };
        assert!(archive.target.is_none());
        assert!(archive.archive.dest.is_none());
    }

    #[test]
    fn test_load_build_config_rejects_archive_step_mixed_with_run_fields() {
        let repo = TempDir::new().unwrap();
        write_build_config(
            &repo,
            "build.yaml",
            r#"
type: build
base-image: @debian-base
steps:
  - archive:
      src: "@some-tool"
      name: bad-mixed
    on: host
    run: echo nope
"#,
        );
        let err = match load_build_config(repo.path(), &repo.path().join("build.yaml")) {
            Err(err) => err,
            Ok(config) => validate_build_steps(&config.steps)
                .expect_err("archive step mixed with run fields must be rejected"),
        };
        let msg = format!("{err:#}");
        assert!(
            msg.contains("on")
                || msg.contains("run")
                || msg.contains("archive")
                || msg.contains("unknown field"),
            "error should indicate archive/run field conflict: {msg}"
        );
    }

    // --- assert: block ---

    #[test]
    fn test_load_test_config_assert_absent_is_none() {
        let repo = TempDir::new().unwrap();
        std::fs::write(repo.path().join("test.yaml"), "type: test\nsteps: []\n").unwrap();
        let config = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap();
        assert!(config.assert.is_none(), "assert should default to None");
    }

    #[test]
    fn test_load_test_config_assert_files_parses_exists_true() {
        let repo = TempDir::new().unwrap();
        std::fs::write(
            repo.path().join("test.yaml"),
            r#"
type: test
steps: []
assert:
  files:
    /usr/local/bin/tool:
      exists: true
      filetype: file
      owner: root
      group: root
      mode: "0755"
"#,
        )
        .unwrap();
        let config = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap();
        let assert_block = config.assert.unwrap();
        let entry = assert_block.files.get("/usr/local/bin/tool").unwrap();
        assert!(entry.exists);
        assert_eq!(entry.filetype, Some(AssertFileType::File));
        assert_eq!(entry.owner.as_deref(), Some("root"));
        assert_eq!(entry.group.as_deref(), Some("root"));
        assert_eq!(entry.mode.as_deref(), Some("0755"));
    }

    #[test]
    fn test_load_test_config_assert_files_parses_exists_false() {
        let repo = TempDir::new().unwrap();
        std::fs::write(
            repo.path().join("test.yaml"),
            r#"
type: test
steps: []
assert:
  files:
    /tmp/should-be-gone:
      exists: false
"#,
        )
        .unwrap();
        let config = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap();
        let assert_block = config.assert.unwrap();
        let entry = assert_block.files.get("/tmp/should-be-gone").unwrap();
        assert!(!entry.exists);
        assert!(entry.filetype.is_none());
        assert!(entry.owner.is_none());
        assert!(entry.group.is_none());
        assert!(entry.mode.is_none());
    }

    #[test]
    fn test_load_test_config_assert_files_rejects_exists_false_with_attributes() {
        let repo = TempDir::new().unwrap();
        std::fs::write(
            repo.path().join("test.yaml"),
            r#"
type: test
steps: []
assert:
  files:
    /some/path:
      exists: false
      mode: "0755"
"#,
        )
        .unwrap();
        let err = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("exists: false") || msg.contains("attribute"),
            "error should mention exists:false and attributes: {msg}"
        );
    }

    #[test]
    fn test_load_test_config_assert_files_rejects_relative_path() {
        let repo = TempDir::new().unwrap();
        std::fs::write(
            repo.path().join("test.yaml"),
            r#"
type: test
steps: []
assert:
  files:
    relative/path:
      exists: true
"#,
        )
        .unwrap();
        let err = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("absolute"),
            "error should mention absolute path: {msg}"
        );
    }

    #[test]
    fn test_load_test_config_assert_files_rejects_invalid_mode() {
        let repo = TempDir::new().unwrap();
        std::fs::write(
            repo.path().join("test.yaml"),
            r#"
type: test
steps: []
assert:
  files:
    /some/path:
      exists: true
      mode: "rwxr-xr-x"
"#,
        )
        .unwrap();
        let err = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("mode") || msg.contains("octal"),
            "error should mention mode: {msg}"
        );
    }

    #[test]
    fn test_load_test_config_assert_files_multiple_entries() {
        let repo = TempDir::new().unwrap();
        std::fs::write(
            repo.path().join("test.yaml"),
            r#"
type: test
steps: []
assert:
  files:
    /usr/bin/tool:
      exists: true
      filetype: file
    /var/data:
      exists: true
      filetype: directory
    /tmp/gone.tar:
      exists: false
"#,
        )
        .unwrap();
        let config = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap();
        let assert_block = config.assert.unwrap();
        assert_eq!(assert_block.files.len(), 3);
        assert_eq!(
            assert_block.files.get("/usr/bin/tool").unwrap().filetype,
            Some(AssertFileType::File)
        );
        assert_eq!(
            assert_block.files.get("/var/data").unwrap().filetype,
            Some(AssertFileType::Directory)
        );
        assert!(!assert_block.files.get("/tmp/gone.tar").unwrap().exists);
    }

    #[test]
    fn test_load_test_config_assert_users_basic() {
        let repo = TempDir::new().unwrap();
        std::fs::write(
            repo.path().join("test.yaml"),
            r#"
type: test
steps: []
assert:
  users:
    bot:
      exists: true
      shell: /bin/bash
      groups: [bot, docker]
    mallory:
      exists: false
"#,
        )
        .unwrap();
        let config = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap();
        let assert_block = config.assert.unwrap();
        assert_eq!(assert_block.users.len(), 2);
        let bot = assert_block.users.get("bot").unwrap();
        assert!(bot.exists);
        assert_eq!(bot.shell.as_deref(), Some("/bin/bash"));
        assert_eq!(bot.groups, vec!["bot", "docker"]);
        let mallory = assert_block.users.get("mallory").unwrap();
        assert!(!mallory.exists);
    }

    #[test]
    fn test_load_test_config_assert_users_pattern_negative() {
        let repo = TempDir::new().unwrap();
        std::fs::write(
            repo.path().join("test.yaml"),
            r#"
type: test
steps: []
assert:
  users:
    "botforge-*":
      exists: false
"#,
        )
        .unwrap();
        let config = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap();
        let assert_block = config.assert.unwrap();
        let pat = assert_block.users.get("botforge-*").unwrap();
        assert!(!pat.exists);
    }

    #[test]
    fn test_load_test_config_assert_users_rejects_attrs_with_exists_false() {
        let repo = TempDir::new().unwrap();
        std::fs::write(
            repo.path().join("test.yaml"),
            r#"
type: test
steps: []
assert:
  users:
    mallory:
      exists: false
      shell: /bin/bash
"#,
        )
        .unwrap();
        let err = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("shell") || msg.contains("exists: false"),
            "error should mention shell/exists: {msg}"
        );
    }

    #[test]
    fn test_load_test_config_assert_groups_basic() {
        let repo = TempDir::new().unwrap();
        std::fs::write(
            repo.path().join("test.yaml"),
            r#"
type: test
steps: []
assert:
  groups:
    docker:
      exists: true
    evilusers:
      exists: false
"#,
        )
        .unwrap();
        let config = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap();
        let assert_block = config.assert.unwrap();
        assert_eq!(assert_block.groups.len(), 2);
        assert!(assert_block.groups.get("docker").unwrap().exists);
        assert!(!assert_block.groups.get("evilusers").unwrap().exists);
    }

    #[test]
    fn test_load_test_config_assert_users_and_groups_combined() {
        let repo = TempDir::new().unwrap();
        std::fs::write(
            repo.path().join("test.yaml"),
            r#"
type: test
steps: []
assert:
  users:
    bot:
      exists: true
      shell: /bin/bash
  groups:
    docker:
      exists: true
"#,
        )
        .unwrap();
        let config = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap();
        let assert_block = config.assert.unwrap();
        assert_eq!(assert_block.users.len(), 1);
        assert_eq!(assert_block.groups.len(), 1);
    }

    #[test]
    fn test_load_test_config_assert_packages_basic() {
        let repo = TempDir::new().unwrap();
        std::fs::write(
            repo.path().join("test.yaml"),
            r#"
type: test
steps: []
assert:
  packages:
    git:
      installed: true
    telnet:
      installed: false
"#,
        )
        .unwrap();
        let config = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap();
        let assert_block = config.assert.unwrap();
        assert_eq!(assert_block.packages.len(), 2);
        assert!(assert_block.packages.get("git").unwrap().installed);
        assert!(!assert_block.packages.get("telnet").unwrap().installed);
    }

    #[test]
    fn test_load_test_config_assert_packages_pattern_negative() {
        let repo = TempDir::new().unwrap();
        std::fs::write(
            repo.path().join("test.yaml"),
            r#"
type: test
steps: []
assert:
  packages:
    "*-dev":
      installed: false
"#,
        )
        .unwrap();
        let config = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap();
        let assert_block = config.assert.unwrap();
        let pat = assert_block.packages.get("*-dev").unwrap();
        assert!(!pat.installed);
    }

    #[test]
    fn test_load_test_config_assert_packages_pattern_positive() {
        let repo = TempDir::new().unwrap();
        std::fs::write(
            repo.path().join("test.yaml"),
            r#"
type: test
steps: []
assert:
  packages:
    "linux-image-*":
      installed: true
"#,
        )
        .unwrap();
        let config = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap();
        let assert_block = config.assert.unwrap();
        let pat = assert_block.packages.get("linux-image-*").unwrap();
        assert!(pat.installed);
    }

    #[test]
    fn test_load_test_config_assert_packages_rejects_unknown_field() {
        let repo = TempDir::new().unwrap();
        std::fs::write(
            repo.path().join("test.yaml"),
            r#"
type: test
steps: []
assert:
  packages:
    git:
      installed: true
      version: "2.40"
"#,
        )
        .unwrap();
        let err = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("version") || msg.contains("unknown field"),
            "error should mention unknown field: {msg}"
        );
    }
}
