use anyhow::{Context, Result};
use serde::{de, Deserialize};
use serde_yaml::Value;
use std::collections::{BTreeMap, HashSet};
use std::path::{Component, Path, PathBuf};

use crate::iso::BootcmdEntry;
use crate::qemu::PortSpec;
use crate::util::resolve_under_root;

use super::step::{
    deserialize_optional_positive_seconds, resolve_shell, StepTarget, TestStep, TopLevelUpload,
};

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

#[derive(Debug, Deserialize, Default)]
pub(crate) struct TestConfig {
    #[serde(default)]
    pub(crate) isos: Vec<TestIso>,
    #[serde(default)]
    pub(crate) ports: Vec<PortSpec>,
    #[serde(default)]
    pub(crate) steps: Vec<TestStep>,
    #[serde(default)]
    pub(crate) uploads: Vec<TopLevelUpload>,
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
}

/// Raw deserialization target for a top-level `botforge test` document.
/// The `type:` field is required; parsing fails with a descriptive error when it
/// is absent or carries an unrecognised value.
#[derive(Debug, Deserialize)]
struct RawTestDocument {
    #[serde(rename = "type")]
    doc_type: DocumentType,
    #[serde(default)]
    isos: Vec<TestIso>,
    #[serde(default)]
    ports: Vec<PortSpec>,
    #[serde(default)]
    steps: Vec<RawTestStep>,
    #[serde(default)]
    uploads: Vec<TopLevelUpload>,
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
}

fn default_disk_size() -> String {
    "10G".to_string()
}

fn default_memsize() -> u32 {
    4096
}

fn default_smp() -> u32 {
    4
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

/// A parsed `image:` reference from a `type: build` spec.
///
/// `@<name>` is the only supported form today: it resolves the named shasset
/// dep-provider's default artifact (a single qcow2).  The `@://…` traversal
/// form is **reserved** — the parser recognises it and hard-errors.  A future
/// `Traversal { … }` variant will be added here when traversal is implemented.
#[derive(Debug, PartialEq, Clone)]
pub(crate) enum ImageRef {
    /// `@<name>` — resolve the named shasset dep-provider's default artifact.
    ShassetDefault(String),
}

/// Parse the raw `image:` string from a `type: build` spec into an [`ImageRef`].
///
/// Delegates to [`crate::resolver::Reference::parse`] for the grammar, then
/// enforces the `image:`-specific policy:
/// - Only `Reference::Asset { path: None }` is accepted (`@<name>` form).
/// - `@` alone, `@output`, and all `://`-traversal forms are rejected.
pub(crate) fn parse_image_ref(raw: &str) -> Result<ImageRef> {
    use crate::resolver::Reference;
    let reference = Reference::parse(raw).map_err(|_| {
        anyhow::anyhow!(
            "image reference must use the `@` scheme (e.g. `@debian-base`); \
             bare names are not supported: {raw:?}"
        )
    })?;
    match reference {
        Reference::Asset { name, path: None } => Ok(ImageRef::ShassetDefault(name)),
        Reference::Repo { path: None } => {
            anyhow::bail!(
                "image reference is missing a shasset name after `@` \
                 (e.g. `@debian-base`)"
            )
        }
        Reference::Asset { path: Some(_), .. }
        | Reference::Repo { path: Some(_) }
        | Reference::Output { .. } => {
            anyhow::bail!(
                "`@` scheme traversal (`@://…`) is not yet supported for image references; \
                 use `@<shasset-name>` to resolve a provider's default artifact \
                 (e.g. `@debian-base`)"
            )
        }
    }
}

/// Output-compression options for `botforge build`.
///
/// Modelled as an optional map with a required `enabled:` field so it can
/// carry additional knobs without changing shape.  The struct is kept strict
/// (`deny_unknown_fields`) to catch typos at parse time.
///
/// ```yaml
/// # default off — plain atomic rename (byte-identical to today)
/// # compress: absent
///
/// # on, qemu default cluster size
/// compress:
///   enabled: true
///
/// # on, explicit cluster size
/// compress:
///   enabled: true
///   cluster_size: "1M"
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
    /// Optional cluster size passed verbatim as `-o cluster_size=<val>` to
    /// `qemu-img convert`.  Omitted ⇒ qemu default cluster size.
    #[serde(default)]
    pub(crate) cluster_size: Option<String>,
}

/// Resolved configuration for a `botforge build` run.
#[derive(Debug)]
pub(crate) struct BuildConfig {
    /// Parsed `image:` reference naming the source qcow2 to boot from.
    pub(crate) image: ImageRef,
    pub(crate) disk_size: String,
    pub(crate) memsize: u32,
    pub(crate) smp: u32,
    pub(crate) steps: Vec<TestStep>,
    pub(crate) uploads: Vec<TopLevelUpload>,
    pub(crate) step_timeout: u64,
    pub(crate) timeout: u64,
    pub(crate) cloud_init_timeout: u64,
    /// Optional cloud-init `bootcmd:` entries to merge into the first-boot
    /// user-data.  Absent/empty ⇒ no `bootcmd:` key emitted (zero change to
    /// existing behaviour).
    pub(crate) bootcmd: Vec<BootcmdEntry>,
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
    /// Raw `image:` reference (e.g. `@debian-base`). Required; parsed via
    /// [`parse_image_ref`] into an [`ImageRef`] after deserialization.
    #[serde(rename = "image", default)]
    image: Option<String>,
    #[serde(default = "default_disk_size")]
    disk_size: String,
    #[serde(default = "default_memsize")]
    memsize: u32,
    #[serde(default = "default_smp")]
    smp: u32,
    #[serde(default)]
    steps: Vec<RawTestStep>,
    #[serde(default)]
    uploads: Vec<TopLevelUpload>,
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
    /// Optional cloud-init `bootcmd:` entries.  Each item is either a plain
    /// shell string or a sequence of strings (exec/argv form).  Absent or
    /// empty ⇒ no `bootcmd:` key in the generated user-data.
    #[serde(default)]
    bootcmd: Vec<BootcmdEntry>,
    /// Optional output-compression config.  Absent ⇒ plain atomic rename.
    #[serde(default)]
    compress: Option<CompressConfig>,
}

#[derive(Debug, Deserialize)]
struct RawTestStepFragment {
    #[serde(default)]
    steps: Vec<RawTestStep>,
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
    let raw: RawTestDocument = serde_yaml::from_value(value)
        .with_context(|| format!("invalid test config: {}", path.display()))?;
    if !raw.doc_type.is_test_entrypoint() {
        anyhow::bail!(
            "botforge test requires a 'type: test' document, got 'type: {}'",
            raw.doc_type.as_str()
        );
    }
    // Seed the stack with the root document so that a fragment including the root
    // is caught by the cycle check (A → B → A).
    let mut include_stack = vec![path.to_path_buf()];
    Ok(TestConfig {
        isos: raw.isos,
        ports: raw.ports,
        steps: expand_test_steps(repo_root, path, raw.steps, &mut include_stack)?,
        uploads: {
            validate_top_level_uploads("test", &raw.uploads)
                .with_context(|| format!("invalid test config: {}", path.display()))?;
            raw.uploads
        },
        diagnostics_units: raw.diagnostics_units,
        step_timeout: raw.step_timeout,
        timeout: raw.timeout,
        cloud_init_timeout: default_test_cloud_init_timeout(),
    })
}

pub(crate) fn load_build_config(repo_root: &Path, path: &Path) -> Result<BuildConfig> {
    let yaml = std::fs::read_to_string(path)
        .with_context(|| format!("cannot read build config: {}", path.display()))?;
    let value: Value = serde_yaml::from_str(&yaml)
        .with_context(|| format!("invalid build config: {}", path.display()))?;
    check_no_test_entrypoint_sections_in_build_doc(path, &value)?;
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
             set it to a shasset dep-provider reference, e.g. `image: \"@debian-base\"`",
            path.display()
        ),
        Some(s) if s.trim().is_empty() => anyhow::bail!(
            "'image' is required in a 'type: build' document ({}): \
             set it to a shasset dep-provider reference, e.g. `image: \"@debian-base\"`",
            path.display()
        ),
        Some(s) => parse_image_ref(&s).with_context(|| {
            format!("invalid 'image' value in build config ({})", path.display())
        })?,
    };
    let mut include_stack = vec![path.to_path_buf()];
    Ok(BuildConfig {
        image,
        disk_size: raw.disk_size,
        memsize: raw.memsize,
        smp: raw.smp,
        steps: expand_test_steps(repo_root, path, raw.steps, &mut include_stack)?,
        uploads: {
            validate_top_level_uploads("build", &raw.uploads)
                .with_context(|| format!("invalid build config: {}", path.display()))?;
            raw.uploads
        },
        step_timeout: raw.step_timeout,
        timeout: raw.timeout,
        cloud_init_timeout: default_build_cloud_init_timeout(),
        bootcmd: raw.bootcmd,
        compress: raw.compress,
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
                let nested = load_test_steps_fragment(&include_path, &include.uses, &include.with)
                    .and_then(|steps| {
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
    uses: &str,
    with: &BTreeMap<String, String>,
) -> Result<Vec<RawTestStep>> {
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
    Ok(fragment.steps)
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
/// `compress:`) inside a `type: fragment` document.
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
        "compress",
        "uploads",
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

/// Reject build-only sections (`disk_size:`, `memsize:`, `smp:`, `image:`) inside a
/// `type: test` document.  Serde would silently ignore them; this turns a
/// misplaced key into an explicit load-time error.
fn check_no_build_sections_in_test_doc(path: &Path, value: &Value) -> Result<()> {
    let mapping = match value {
        Value::Mapping(m) => m,
        _ => return Ok(()),
    };
    for section in &["disk_size", "memsize", "smp", "image", "compress"] {
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
/// inside a `type: build` document.  Serde would silently ignore them; this turns a
/// misplaced key into an explicit load-time error.
fn check_no_test_entrypoint_sections_in_build_doc(path: &Path, value: &Value) -> Result<()> {
    let mapping = match value {
        Value::Mapping(m) => m,
        _ => return Ok(()),
    };
    for section in &["ports", "isos", "diagnostics_units"] {
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

fn validate_top_level_uploads(kind: &str, uploads: &[TopLevelUpload]) -> Result<()> {
    for upload in uploads {
        validate_top_level_upload(kind, upload)?;
    }
    Ok(())
}

/// Validate a `mode` string: must be 3–4 octal digits (same rule as `payload.rs`).
fn validate_mode_string(mode: &str, src: &str, kind: &str) -> Result<()> {
    if mode.len() < 3 || mode.len() > 4 || !mode.chars().all(|ch| ('0'..='7').contains(&ch)) {
        anyhow::bail!(
            "{kind} uploads entry '{src}': `mode` must be 3–4 octal digits, got '{mode}'"
        );
    }
    Ok(())
}

/// Validate an `owner` or `group` string: non-empty, no whitespace, no `/`, no shell metacharacters.
fn validate_owner_group_string(value: &str, field: &str, src: &str, kind: &str) -> Result<()> {
    if value.trim().is_empty() {
        anyhow::bail!("{kind} uploads entry '{src}': `{field}` must be non-empty");
    }
    for ch in value.chars() {
        if ch.is_whitespace()
            || matches!(
                ch,
                '/' | '\'' | '"' | '`' | '$' | ';' | '&' | '|' | '<' | '>'
            )
        {
            anyhow::bail!(
                "{kind} uploads entry '{src}': `{field}` contains invalid character '{ch}'; \
                 must not contain whitespace, '/', or shell metacharacters"
            );
        }
    }
    Ok(())
}

fn validate_top_level_upload(kind: &str, upload: &TopLevelUpload) -> Result<()> {
    let src = upload.src.trim();
    let dest = upload.dest.trim();

    if src.is_empty() {
        anyhow::bail!("{kind} uploads entry: `src` is required and must be non-empty");
    }
    if src.starts_with('@') {
        anyhow::bail!(
            "{kind} uploads entry '{src}': top-level `uploads:` only supports repo-relative files/globs; use an `archive:` step for shasset assets"
        );
    }
    validate_uses_repo_path(Path::new(src)).with_context(|| {
        format!("{kind} uploads entry '{src}': `src` must be repo-relative and contain no '.' or '..' segments")
    })?;
    if dest.is_empty() {
        anyhow::bail!(
            "{kind} uploads entry '{src}': `dest` is required and must be a non-empty absolute path"
        );
    }
    if !dest.starts_with('/') {
        anyhow::bail!(
            "{kind} uploads entry '{src}': `dest` must be an absolute guest path (got '{dest}')"
        );
    }
    if src_has_glob_metacharacters(src) && !dest.ends_with('/') {
        anyhow::bail!(
            "{kind} uploads entry '{src}': glob `src` requires `dest` to be a directory path ending with '/'"
        );
    }
    if let Some(mode) = &upload.mode {
        validate_mode_string(mode, src, kind)?;
    }
    if let Some(owner) = &upload.owner {
        validate_owner_group_string(owner, "owner", src, kind)?;
    }
    if let Some(group) = &upload.group {
        validate_owner_group_string(group, "group", src, kind)?;
    }
    Ok(())
}

pub(crate) fn src_has_glob_metacharacters(src: &str) -> bool {
    src.contains('*') || src.contains('?') || src.contains('[')
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
mod tests {
    use super::{
        default_bootstrap_path, load_build_config, load_test_config, parse_image_ref,
        resolve_fragment_inputs, validate_build_steps, validate_test_ports, validate_test_steps,
        ImageRef, InputDeclaration, InputType, TestConfig, TestIso, MAX_INCLUDE_DEPTH,
    };
    use crate::plan::step::{
        ArchiveStep, ArchiveStepSpec, RunStep, StepTarget, TestStep, TopLevelUpload,
    };
    use crate::qemu::PortSpec;
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};
    use tempfile::TempDir;

    fn loopback(port: u16) -> PortSpec {
        PortSpec {
            addr: "127.0.0.1".into(),
            port,
        }
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

    // --- parse_image_ref ---

    #[test]
    fn test_parse_image_ref_shasset_default() {
        let r = parse_image_ref("@debian-base").unwrap();
        assert_eq!(r, ImageRef::ShassetDefault("debian-base".to_string()));
    }

    #[test]
    fn test_parse_image_ref_bare_name_rejected() {
        let err = parse_image_ref("debian-base").unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("`@` scheme") || msg.contains("@ scheme"),
            "error should mention @ scheme: {msg}"
        );
    }

    #[test]
    fn test_parse_image_ref_empty_rejected() {
        let err = parse_image_ref("").unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("`@` scheme") || msg.contains("@ scheme"),
            "error should mention @ scheme: {msg}"
        );
    }

    #[test]
    fn test_parse_image_ref_at_alone_rejected() {
        let err = parse_image_ref("@").unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("missing") || msg.contains("shasset name"),
            "error should mention missing name: {msg}"
        );
    }

    #[test]
    fn test_parse_image_ref_traversal_scheme_rejected() {
        for raw in &["@://debian-base", "@debian-base://something", "@://foo/bar"] {
            let err = parse_image_ref(raw).unwrap_err();
            let msg = format!("{err:#}");
            assert!(
                msg.contains("traversal") || msg.contains("not yet supported"),
                "error should mention traversal not supported for {raw:?}: {msg}"
            );
        }
    }

    // --- BuildConfig loading ---

    fn write_build_config(repo: &TempDir, name: &str, content: &str) {
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
steps:
  - on: guest
    name: provision
    run: echo hello
"#,
        );
        let config = load_build_config(repo.path(), &repo.path().join("build.yaml")).unwrap();
        assert_eq!(
            config.image,
            ImageRef::ShassetDefault("debian-base".to_string())
        );
        assert_eq!(config.disk_size, "10G");
        assert_eq!(config.memsize, 4096);
        assert_eq!(config.smp, 4);
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
disk_size: "20G"
memsize: 8192
smp: 8
step_timeout: 2400
timeout: 9600
steps: []
"#,
        );
        let config = load_build_config(repo.path(), &repo.path().join("build.yaml")).unwrap();
        assert_eq!(
            config.image,
            ImageRef::ShassetDefault("my-base".to_string())
        );
        assert_eq!(config.disk_size, "20G");
        assert_eq!(config.memsize, 8192);
        assert_eq!(config.smp, 8);
        assert_eq!(config.step_timeout, 2400);
        assert_eq!(config.timeout, 9600);
        assert!(config.steps.is_empty());
        assert!(config.bootcmd.is_empty(), "bootcmd should default to empty");
    }

    #[test]
    fn test_load_build_config_uploads_absent_is_empty() {
        let repo = TempDir::new().unwrap();
        write_build_config(
            &repo,
            "build.yaml",
            "type: build\nimage: \"@base\"\nsteps: []\n",
        );
        let config = load_build_config(repo.path(), &repo.path().join("build.yaml")).unwrap();
        assert!(config.uploads.is_empty(), "uploads should default to empty");
    }

    #[test]
    fn test_load_build_config_parses_top_level_uploads() {
        let repo = TempDir::new().unwrap();
        write_build_config(
            &repo,
            "build.yaml",
            r#"
type: build
image: "@base"
uploads:
  - src: images/botspace/envoy/**/*.yaml
    dest: /tmp/bake-staging/envoy/
  - src: build/images/payload/*.tar
    dest: /usr/share/botwork/images/
steps: []
"#,
        );
        let config = load_build_config(repo.path(), &repo.path().join("build.yaml")).unwrap();
        assert_eq!(
            config.uploads,
            vec![
                TopLevelUpload {
                    src: "images/botspace/envoy/**/*.yaml".to_string(),
                    dest: "/tmp/bake-staging/envoy/".to_string(),
                    ..Default::default()
                },
                TopLevelUpload {
                    src: "build/images/payload/*.tar".to_string(),
                    dest: "/usr/share/botwork/images/".to_string(),
                    ..Default::default()
                },
            ]
        );
    }

    #[test]
    fn test_load_build_config_rejects_top_level_upload_at_prefixed_src() {
        let repo = TempDir::new().unwrap();
        write_build_config(
            &repo,
            "build.yaml",
            r#"
type: build
image: "@base"
uploads:
  - src: "@payload"
    dest: /tmp/payload
steps: []
"#,
        );
        let err = load_build_config(repo.path(), &repo.path().join("build.yaml")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("archive:"));
        assert!(msg.contains("@payload"));
    }

    #[test]
    fn test_load_build_config_rejects_top_level_upload_relative_dest() {
        let repo = TempDir::new().unwrap();
        write_build_config(
            &repo,
            "build.yaml",
            r#"
type: build
image: "@base"
uploads:
  - src: payload/file.txt
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
    fn test_load_build_config_rejects_top_level_upload_src_traversal() {
        let repo = TempDir::new().unwrap();
        write_build_config(
            &repo,
            "build.yaml",
            r#"
type: build
image: "@base"
uploads:
  - src: payload/../secret.txt
    dest: /tmp/secret.txt
steps: []
"#,
        );
        let err = load_build_config(repo.path(), &repo.path().join("build.yaml")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains(".."), "error should mention traversal: {msg}");
    }

    #[test]
    fn test_load_build_config_rejects_top_level_upload_glob_with_non_directory_dest() {
        let repo = TempDir::new().unwrap();
        write_build_config(
            &repo,
            "build.yaml",
            r#"
type: build
image: "@base"
uploads:
  - src: payload/*.tar
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
    fn test_load_build_config_rejects_top_level_upload_unknown_field() {
        let repo = TempDir::new().unwrap();
        write_build_config(
            &repo,
            "build.yaml",
            r#"
type: build
image: "@base"
uploads:
  - src: payload/file.txt
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
    fn test_load_build_config_parses_top_level_upload_permission_fields() {
        let repo = TempDir::new().unwrap();
        write_build_config(
            &repo,
            "build.yaml",
            r#"
type: build
image: "@base"
uploads:
  - src: payload/file.txt
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
        assert_eq!(config.uploads.len(), 1);
        let upload = &config.uploads[0];
        assert_eq!(upload.mode.as_deref(), Some("0755"));
        assert_eq!(upload.owner.as_deref(), Some("root"));
        assert_eq!(upload.group.as_deref(), Some("root"));
        assert_eq!(upload.overwrite, Some(true));
        assert_eq!(upload.parents, Some(true));
    }

    #[test]
    fn test_load_build_config_rejects_top_level_upload_invalid_mode() {
        let repo = TempDir::new().unwrap();
        write_build_config(
            &repo,
            "build.yaml",
            r#"
type: build
image: "@base"
uploads:
  - src: payload/file.txt
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
    fn test_load_build_config_rejects_top_level_upload_owner_with_slash() {
        let repo = TempDir::new().unwrap();
        write_build_config(
            &repo,
            "build.yaml",
            r#"
type: build
image: "@base"
uploads:
  - src: payload/file.txt
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
    fn test_load_build_config_rejects_top_level_upload_group_with_metachar() {
        let repo = TempDir::new().unwrap();
        write_build_config(
            &repo,
            "build.yaml",
            r#"
type: build
image: "@base"
uploads:
  - src: payload/file.txt
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
    // bootcmd field tests
    // -----------------------------------------------------------------

    #[test]
    fn test_load_build_config_bootcmd_absent_is_empty() {
        let repo = TempDir::new().unwrap();
        write_build_config(
            &repo,
            "build.yaml",
            "type: build\nimage: \"@base\"\nsteps: []\n",
        );
        let config = load_build_config(repo.path(), &repo.path().join("build.yaml")).unwrap();
        assert!(
            config.bootcmd.is_empty(),
            "absent bootcmd must deserialize as empty vec"
        );
    }

    #[test]
    fn test_load_build_config_bootcmd_string_entries() {
        use crate::iso::BootcmdEntry;
        let repo = TempDir::new().unwrap();
        write_build_config(
            &repo,
            "build.yaml",
            r#"
type: build
image: "@base"
steps: []
bootcmd:
  - echo hello
  - echo world
"#,
        );
        let config = load_build_config(repo.path(), &repo.path().join("build.yaml")).unwrap();
        assert_eq!(config.bootcmd.len(), 2);
        assert_eq!(
            config.bootcmd[0],
            BootcmdEntry::Shell("echo hello".to_string())
        );
        assert_eq!(
            config.bootcmd[1],
            BootcmdEntry::Shell("echo world".to_string())
        );
    }

    #[test]
    fn test_load_build_config_bootcmd_exec_entry() {
        use crate::iso::BootcmdEntry;
        let repo = TempDir::new().unwrap();
        write_build_config(
            &repo,
            "build.yaml",
            r#"
type: build
image: "@base"
steps: []
bootcmd:
  - [ cloud-init-per, once, mask-stack, sh, -c, "systemctl mask a.service" ]
"#,
        );
        let config = load_build_config(repo.path(), &repo.path().join("build.yaml")).unwrap();
        assert_eq!(config.bootcmd.len(), 1);
        assert_eq!(
            config.bootcmd[0],
            BootcmdEntry::Exec(vec![
                "cloud-init-per".to_string(),
                "once".to_string(),
                "mask-stack".to_string(),
                "sh".to_string(),
                "-c".to_string(),
                "systemctl mask a.service".to_string(),
            ])
        );
    }

    #[test]
    fn test_load_build_config_bootcmd_mixed_entries() {
        use crate::iso::BootcmdEntry;
        let repo = TempDir::new().unwrap();
        write_build_config(
            &repo,
            "build.yaml",
            r#"
type: build
image: "@base"
steps: []
bootcmd:
  - echo shell-entry
  - [ cloud-init-per, once, mask-stack, sh, -c, "systemctl mask a.service b.service" ]
"#,
        );
        let config = load_build_config(repo.path(), &repo.path().join("build.yaml")).unwrap();
        assert_eq!(config.bootcmd.len(), 2);
        assert_eq!(
            config.bootcmd[0],
            BootcmdEntry::Shell("echo shell-entry".to_string())
        );
        assert!(
            matches!(&config.bootcmd[1], BootcmdEntry::Exec(args) if args[0] == "cloud-init-per"),
            "second entry should be exec form: {:?}",
            config.bootcmd[1]
        );
    }

    #[test]
    fn test_load_build_config_bootcmd_is_a_known_field() {
        // Verify that `bootcmd:` is recognised as a known field and not silently
        // discarded or treated as an error.  This guards against the field being
        // accidentally removed from RawBuildDocument.
        use crate::iso::BootcmdEntry;
        let repo = TempDir::new().unwrap();
        write_build_config(
            &repo,
            "build.yaml",
            r#"
type: build
image: "@base"
steps: []
bootcmd:
  - echo hello
"#,
        );
        let config = load_build_config(repo.path(), &repo.path().join("build.yaml")).unwrap();
        assert_eq!(config.bootcmd.len(), 1);
        assert_eq!(
            config.bootcmd[0],
            BootcmdEntry::Shell("echo hello".to_string()),
            "bootcmd entry must be preserved, not silently dropped"
        );
    }

    #[test]
    fn test_load_build_config_bootcmd_empty_list_is_empty() {
        let repo = TempDir::new().unwrap();
        write_build_config(
            &repo,
            "build.yaml",
            "type: build\nimage: \"@base\"\nsteps: []\nbootcmd: []\n",
        );
        let config = load_build_config(repo.path(), &repo.path().join("build.yaml")).unwrap();
        assert!(
            config.bootcmd.is_empty(),
            "explicit empty bootcmd list must deserialize as empty vec"
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
            "type: build\nimage: \"@base\"\nsteps: []\n",
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
            "type: build\nimage: \"@base\"\nsteps: []\ncompress:\n  enabled: true\n",
        );
        let config = load_build_config(repo.path(), &repo.path().join("build.yaml")).unwrap();
        let compress = config.compress.expect("compress should be Some");
        assert!(compress.enabled, "enabled must be true");
        assert!(
            compress.cluster_size.is_none(),
            "cluster_size must default to None"
        );
    }

    #[test]
    fn test_load_build_config_compress_enabled_true_with_cluster_size() {
        let repo = TempDir::new().unwrap();
        write_build_config(
            &repo,
            "build.yaml",
            "type: build\nimage: \"@base\"\nsteps: []\ncompress:\n  enabled: true\n  cluster_size: \"1M\"\n",
        );
        let config = load_build_config(repo.path(), &repo.path().join("build.yaml")).unwrap();
        let compress = config.compress.expect("compress should be Some");
        assert!(compress.enabled);
        assert_eq!(compress.cluster_size.as_deref(), Some("1M"));
    }

    #[test]
    fn test_load_build_config_compress_enabled_false() {
        let repo = TempDir::new().unwrap();
        write_build_config(
            &repo,
            "build.yaml",
            "type: build\nimage: \"@base\"\nsteps: []\ncompress:\n  enabled: false\n",
        );
        let config = load_build_config(repo.path(), &repo.path().join("build.yaml")).unwrap();
        let compress = config.compress.expect("compress should be Some");
        assert!(!compress.enabled, "enabled must be false");
    }

    #[test]
    fn test_load_build_config_compress_missing_enabled_is_error() {
        let repo = TempDir::new().unwrap();
        write_build_config(
            &repo,
            "build.yaml",
            "type: build\nimage: \"@base\"\nsteps: []\ncompress:\n  cluster_size: \"1M\"\n",
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
            "type: build\nimage: \"@base\"\nsteps: []\ncompress:\n  enabled: true\n  bogus: 1\n",
        );
        let err = load_build_config(repo.path(), &repo.path().join("build.yaml")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("bogus") || msg.contains("unknown"),
            "error should mention unknown field: {msg}"
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
    fn test_load_fragment_rejects_uploads_section() {
        let repo = TempDir::new().unwrap();
        std::fs::write(
            repo.path().join("frag.yaml"),
            "type: fragment\nuploads:\n  - src: payload/file.txt\n    dest: /tmp/file.txt\nsteps: []\n",
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
            msg.contains("uploads"),
            "error should reject uploads in fragment doc: {msg}"
        );
    }

    #[test]
    fn test_load_test_config_uploads_absent_is_empty() {
        let repo = TempDir::new().unwrap();
        std::fs::write(repo.path().join("test.yaml"), "type: test\nsteps: []\n").unwrap();
        let config = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap();
        assert!(config.uploads.is_empty(), "uploads should default to empty");
    }

    #[test]
    fn test_load_test_config_parses_top_level_uploads() {
        let repo = TempDir::new().unwrap();
        std::fs::write(
            repo.path().join("test.yaml"),
            r#"
type: test
uploads:
  - src: fixtures/envoy/**/*.yaml
    dest: /tmp/envoy/
steps: []
"#,
        )
        .unwrap();
        let config = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap();
        assert_eq!(
            config.uploads,
            vec![TopLevelUpload {
                src: "fixtures/envoy/**/*.yaml".to_string(),
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
        write_build_config(&repo, "build.yaml", "type: build\nsteps: []\n");
        let err = load_build_config(repo.path(), &repo.path().join("build.yaml")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("'image'") && msg.contains("required"),
            "error should mention missing image: {msg}"
        );
    }

    #[test]
    fn test_load_build_config_rejects_empty_image() {
        let repo = TempDir::new().unwrap();
        write_build_config(&repo, "build.yaml", "type: build\nimage: \"\"\nsteps: []\n");
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
    fn test_load_test_config_rejects_image_section() {
        let repo = TempDir::new().unwrap();
        std::fs::write(
            repo.path().join("test.yaml"),
            "type: test\nimage: \"@debian-base\"\nsteps: []\n",
        )
        .unwrap();
        let err = load_test_config(repo.path(), &repo.path().join("test.yaml")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("image"), "error should mention image: {msg}");
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
}
