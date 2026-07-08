use anyhow::{Context, Result};
use serde::Deserialize;
use serde_yaml::Value;
use std::collections::{BTreeMap, HashSet};
use std::path::{Component, Path, PathBuf};

use crate::qemu::PortSpec;
use crate::util::resolve_under_root;

const DEFAULT_SENTINEL: &str = "__default__";

/// Maximum number of active `uses:` includes on the call stack at any one time.
/// Includes the root document, which is always on the stack; so this limits nesting
/// to `MAX_INCLUDE_DEPTH - 1` fragment levels below the root.
const MAX_INCLUDE_DEPTH: usize = 32;

/// The kind of a botforge YAML document, specified by the required `type:` field.
///
/// Every document must carry exactly one `type:` discriminator.  The loader
/// dispatches on it to enforce command-boundary separation and per-kind presence
/// rules.  Future entrypoint kinds (e.g. `build`) are registered here without
/// changing the loader logic.
#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum DocumentType {
    /// An entrypoint document consumed directly by `botforge test`.
    Test,
    /// A reusable document spliced in via `uses:`.  May not carry
    /// entrypoint-only sections (`ports:`, `isos:`, `diagnostics_units:`).
    Fragment,
}

impl DocumentType {
    fn as_str(self) -> &'static str {
        match self {
            DocumentType::Test => "test",
            DocumentType::Fragment => "fragment",
        }
    }

    /// Returns `true` if this kind is the expected entrypoint for `botforge test`.
    fn is_test_entrypoint(self) -> bool {
        matches!(self, DocumentType::Test)
    }

    /// Returns `true` if this kind can be consumed via a `uses:` reference.
    fn is_consumable_fragment(self) -> bool {
        matches!(self, DocumentType::Fragment)
    }
}

#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum InputType {
    String,
    Number,
    Boolean,
}

#[derive(Debug, Deserialize)]
struct InputDeclaration {
    #[serde(rename = "type")]
    input_type: InputType,
    #[serde(default)]
    required: bool,
    default: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
pub(in crate::commands::test) struct TestConfig {
    #[serde(default)]
    pub(in crate::commands::test) isos: Vec<TestIso>,
    #[serde(default)]
    pub(in crate::commands::test) ports: Vec<PortSpec>,
    #[serde(default)]
    pub(in crate::commands::test) steps: Vec<TestStep>,
    #[serde(default)]
    pub(in crate::commands::test) diagnostics_units: Vec<String>,
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
    diagnostics_units: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct RawTestStepFragment {
    #[serde(default)]
    steps: Vec<RawTestStep>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum RawTestStep {
    Step(TestStep),
    Include(TestStepInclude),
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
pub(in crate::commands::test) enum TestIso {
    Attach(PathBuf),
    Bootstrap {
        path: PathBuf,
        label: String,
        mount: PathBuf,
        #[serde(default = "default_bootstrap_path")]
        bootstrap: PathBuf,
    },
}

pub(in crate::commands::test) struct TestIsoBootstrap {
    pub(in crate::commands::test) label: String,
    pub(in crate::commands::test) mount: PathBuf,
    pub(in crate::commands::test) bootstrap: PathBuf,
}

fn default_bootstrap_path() -> PathBuf {
    PathBuf::from("bootstrap.sh")
}

/// Where a test step executes: inside the guest (SSH) or on the harness host (local).
#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(in crate::commands::test) enum StepTarget {
    /// Run via SSH inside the guest VM.
    Guest,
    /// Run locally in the botforge container (harness), reaching the guest only via forwarded
    /// `ports:`. This is the botforge container / harness where botforge itself runs — not the
    /// CI runner host.
    Host,
}

#[derive(Debug, Deserialize)]
pub(in crate::commands::test) struct TestStep {
    /// Where this step executes. Required; must be `guest` or `host`.
    #[serde(rename = "on")]
    pub(in crate::commands::test) target: StepTarget,
    pub(in crate::commands::test) name: String,
    /// Files to scp into the guest before running. Only valid on `on: guest` steps.
    #[serde(default)]
    pub(in crate::commands::test) uploads: Vec<TestUpload>,
    pub(in crate::commands::test) run: String,
    /// Interpreter used to execute `run:`. Mirrors GitHub Actions `shell:` semantics.
    ///
    /// Named shells: `bash` (default), `sh`, `python`.
    /// Custom template: any string containing `{0}`, e.g. `python3 -u {0}`.
    /// When absent, defaults to `bash --noprofile --norc -e -o pipefail {0}` with
    /// automatic `sh -e {0}` fallback if bash is not available.
    #[serde(default)]
    pub(in crate::commands::test) shell: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(in crate::commands::test) struct TestUpload {
    pub(in crate::commands::test) src: PathBuf,
    pub(in crate::commands::test) dest: String,
}

pub(super) fn load_test_config(repo_root: &Path, path: &Path) -> Result<TestConfig> {
    let yaml = std::fs::read_to_string(path)
        .with_context(|| format!("cannot read test config: {}", path.display()))?;
    let raw: RawTestDocument = serde_yaml::from_str(&yaml)
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
        diagnostics_units: raw.diagnostics_units,
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

/// Reject entrypoint-only sections (`ports:`, `isos:`, `diagnostics_units:`) inside
/// a `type: fragment` document.  Serde would silently ignore them; this turns a
/// misplaced key into an explicit load-time error.
fn check_no_entrypoint_sections_in_fragment(path: &Path, value: &Value) -> Result<()> {
    let mapping = match value {
        Value::Mapping(m) => m,
        _ => return Ok(()),
    };
    for section in &["ports", "isos", "diagnostics_units"] {
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

fn resolve_fragment_inputs(
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
    let (scheme, raw_path) = uses
        .split_once("://")
        .ok_or_else(|| anyhow::anyhow!("invalid uses value '{uses}': expected @://<path>"))?;
    match scheme {
        "@" => {
            let path = PathBuf::from(raw_path);
            validate_uses_repo_path(&path)?;
            Ok(resolve_under_root(repo_root, path))
        }
        other => anyhow::bail!(
            "unsupported uses scheme '{other}' in '{uses}'; only @://<path> is supported"
        ),
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

pub(super) fn validate_test_ports(ports: &[PortSpec], ssh_port: u16) -> Result<()> {
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

pub(super) fn validate_test_steps(steps: &[TestStep], ports: &[PortSpec]) -> Result<()> {
    for step in steps {
        resolve_shell(step.shell.as_deref())
            .with_context(|| format!("test step '{}': invalid `shell:` value", step.name))?;
        if step.target == StepTarget::Host && !step.uploads.is_empty() {
            anyhow::bail!(
                "test step '{}': `uploads` is not valid on an `on: host` step; \
                 files are already local in the harness",
                step.name
            );
        }
    }
    let has_host_step = steps.iter().any(|s| s.target == StepTarget::Host);
    if has_host_step && ports.is_empty() {
        anyhow::bail!(
            "test config has `on: host` steps but no `ports:` are declared; \
             a host step reaches the guest only via forwarded ports"
        );
    }
    Ok(())
}

/// Resolve a step's `shell:` value into an argv template with a `{0}` slot.
///
/// Named shells (`bash`, `sh`, `python`) map to fixed GHA-compatible templates.
/// Custom templates must contain `{0}` as a placeholder for the script file path.
/// `None` (absent) returns the default `bash` template.
///
/// Returns `Err` for: unknown single-token named shell, or a custom multi-token
/// shell string that does not contain `{0}`.
pub(in crate::commands::test) fn resolve_shell(shell: Option<&str>) -> Result<Vec<String>> {
    match shell {
        None | Some("bash") => Ok(vec![
            "bash".to_string(),
            "--noprofile".to_string(),
            "--norc".to_string(),
            "-e".to_string(),
            "-o".to_string(),
            "pipefail".to_string(),
            "{0}".to_string(),
        ]),
        Some("sh") => Ok(vec!["sh".to_string(), "-e".to_string(), "{0}".to_string()]),
        Some("python") => Ok(vec!["python3".to_string(), "{0}".to_string()]),
        Some(custom) => {
            if custom.contains("{0}") {
                Ok(custom.split_whitespace().map(str::to_string).collect())
            } else if custom.split_whitespace().count() <= 1 {
                anyhow::bail!(
                    "unknown named shell '{}'; supported named shells: bash, sh, python. \
                     For a custom interpreter use the '{{0}}' placeholder form, \
                     e.g. '{} {{0}}'",
                    custom,
                    custom
                )
            } else {
                anyhow::bail!(
                    "custom shell '{}' does not contain the '{{0}}' placeholder; \
                     '{{0}}' must appear in the shell template to indicate where \
                     the script file path is substituted",
                    custom
                )
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        default_bootstrap_path, load_test_config, resolve_fragment_inputs, resolve_shell,
        validate_test_ports, validate_test_steps, InputDeclaration, InputType, StepTarget,
        TestConfig, TestIso, TestStep, TestUpload,
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
        assert_eq!(config.steps[0].target, StepTarget::Guest);
        assert_eq!(config.steps[0].name, "goss");
        assert_eq!(config.steps[0].run, "goss -g /path/goss.yaml validate");
        assert!(config.steps[0].uploads.is_empty());
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
        assert_eq!(config.steps[0].target, StepTarget::Host);
        assert_eq!(config.steps[0].name, "vm-narrative");
        assert_eq!(config.steps[0].run, "bash smoke/vm-narrative.sh 127.0.0.1");
    }

    #[test]
    fn test_step_parses_guest_step_with_uploads() {
        let config: TestConfig = serde_yaml::from_str(
            r#"
steps:
  - on: guest
    name: upload-and-run
    uploads:
      - src: local/file.sh
        dest: /tmp/file.sh
    run: bash /tmp/file.sh
"#,
        )
        .unwrap();

        assert_eq!(config.steps[0].uploads.len(), 1);
        assert_eq!(
            config.steps[0].uploads[0].src,
            PathBuf::from("local/file.sh")
        );
        assert_eq!(config.steps[0].uploads[0].dest, "/tmp/file.sh");
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
        assert_eq!(config.steps[0].target, StepTarget::Guest);
        assert_eq!(config.steps[1].target, StepTarget::Guest);
        assert_eq!(config.steps[2].target, StepTarget::Host);
        assert_eq!(config.steps[3].target, StepTarget::Guest);
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
    uploads:
      - src: scripts/${{ inputs.target }}.sh
        dest: /tmp/${{ inputs.target }}.sh
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
        assert_eq!(config.steps[0].name, "narrative-edge");
        assert_eq!(config.steps[0].shell.as_deref(), Some("bash"));
        assert_eq!(
            config.steps[0].uploads[0].src,
            PathBuf::from("scripts/edge.sh")
        );
        assert_eq!(config.steps[0].uploads[0].dest, "/tmp/edge.sh");
        assert!(config.steps[0].run.contains(r#"echo "${USER}""#));
        assert!(config.steps[0].run.contains("bash /tmp/edge.sh"));
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

    fn make_step(target: StepTarget, name: &str, with_uploads: bool) -> TestStep {
        TestStep {
            target,
            name: name.to_string(),
            run: "echo ok".to_string(),
            shell: None,
            uploads: if with_uploads {
                vec![TestUpload {
                    src: PathBuf::from("src/file"),
                    dest: "/tmp/file".to_string(),
                }]
            } else {
                vec![]
            },
        }
    }

    #[test]
    fn test_validate_steps_accepts_guest_with_uploads() {
        let steps = vec![make_step(StepTarget::Guest, "s", true)];
        assert!(validate_test_steps(&steps, &[loopback(80)]).is_ok());
    }

    #[test]
    fn test_validate_steps_accepts_host_without_uploads() {
        let steps = vec![make_step(StepTarget::Host, "s", false)];
        assert!(validate_test_steps(&steps, &[loopback(80)]).is_ok());
    }

    #[test]
    fn test_validate_steps_rejects_uploads_on_host_step() {
        let steps = vec![make_step(StepTarget::Host, "bad", true)];
        let err = validate_test_steps(&steps, &[loopback(80)]).unwrap_err();
        assert!(
            err.to_string().contains("uploads"),
            "error should mention 'uploads': {err}"
        );
        assert!(
            err.to_string().contains("bad"),
            "error should mention step name: {err}"
        );
    }

    #[test]
    fn test_validate_steps_rejects_host_step_without_ports() {
        let steps = vec![make_step(StepTarget::Host, "edge", false)];
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
        let steps = vec![make_step(StepTarget::Guest, "s", false)];
        assert!(validate_test_steps(&steps, &[]).is_ok());
    }

    // --- shell resolver ---

    #[test]
    fn test_resolve_shell_absent_returns_bash_template() {
        let tmpl = resolve_shell(None).unwrap();
        assert_eq!(
            tmpl,
            vec![
                "bash",
                "--noprofile",
                "--norc",
                "-e",
                "-o",
                "pipefail",
                "{0}"
            ]
        );
    }

    #[test]
    fn test_resolve_shell_bash_returns_bash_template() {
        let tmpl = resolve_shell(Some("bash")).unwrap();
        assert_eq!(
            tmpl,
            vec![
                "bash",
                "--noprofile",
                "--norc",
                "-e",
                "-o",
                "pipefail",
                "{0}"
            ]
        );
    }

    #[test]
    fn test_resolve_shell_sh_returns_sh_template() {
        let tmpl = resolve_shell(Some("sh")).unwrap();
        assert_eq!(tmpl, vec!["sh", "-e", "{0}"]);
    }

    #[test]
    fn test_resolve_shell_python_returns_python3_template() {
        let tmpl = resolve_shell(Some("python")).unwrap();
        assert_eq!(tmpl, vec!["python3", "{0}"]);
    }

    #[test]
    fn test_resolve_shell_custom_with_placeholder_is_split() {
        let tmpl = resolve_shell(Some("python3 -u {0}")).unwrap();
        assert_eq!(tmpl, vec!["python3", "-u", "{0}"]);
    }

    #[test]
    fn test_resolve_shell_custom_without_placeholder_is_error() {
        let err = resolve_shell(Some("python3 -u")).unwrap_err();
        assert!(
            err.to_string().contains("{0}"),
            "error should mention '{{0}}': {err}"
        );
    }

    #[test]
    fn test_resolve_shell_unknown_named_shell_is_error() {
        let err = resolve_shell(Some("fish")).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("fish"),
            "error should mention the shell name: {msg}"
        );
        assert!(
            msg.contains("bash") && msg.contains("sh") && msg.contains("python"),
            "error should list supported shells: {msg}"
        );
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
        assert_eq!(config.steps[0].shell.as_deref(), Some("python"));
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
        assert!(config.steps[0].shell.is_none());
    }

    // --- {0} substitution ---

    #[test]
    fn test_apply_shell_template_substitutes_placeholder() {
        let tmpl = resolve_shell(None).unwrap();
        let argv: Vec<String> = tmpl
            .iter()
            .map(|a| {
                if a == "{0}" {
                    "/tmp/my-script.sh".to_string()
                } else {
                    a.clone()
                }
            })
            .collect();
        assert_eq!(
            argv,
            vec![
                "bash",
                "--noprofile",
                "--norc",
                "-e",
                "-o",
                "pipefail",
                "/tmp/my-script.sh"
            ]
        );
    }

    #[test]
    fn test_apply_sh_template_substitutes_placeholder() {
        let tmpl = resolve_shell(Some("sh")).unwrap();
        let argv: Vec<String> = tmpl
            .iter()
            .map(|a| {
                if a == "{0}" {
                    "/tmp/step.sh".to_string()
                } else {
                    a.clone()
                }
            })
            .collect();
        assert_eq!(argv, vec!["sh", "-e", "/tmp/step.sh"]);
    }

    #[test]
    fn test_validate_steps_rejects_bad_shell() {
        let mut step = make_step(StepTarget::Guest, "bad-shell", false);
        step.shell = Some("fish".to_string());
        let err = validate_test_steps(&[step], &[]).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("fish"),
            "error should mention shell name: {msg}"
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
        assert_eq!(config.steps[0].name, "hello");
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
        assert_eq!(config.steps[0].shell.as_deref(), Some("bash"));
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
        assert_eq!(config.steps[0].name, "reused-step");
        assert_eq!(config.steps[1].name, "reused-step");
    }

    #[test]
    fn test_load_test_config_max_depth_exceeded_errors() {
        // Create a chain of MAX_INCLUDE_DEPTH fragments deep, which should trigger the
        // depth-limit error.  With the root document seeded into the stack the limit is
        // MAX_INCLUDE_DEPTH total entries, meaning MAX_INCLUDE_DEPTH - 1 fragment levels
        // below the root.  We create exactly that many chain links plus one extra to
        // ensure the limit fires.
        let repo = TempDir::new().unwrap();
        let depth = super::MAX_INCLUDE_DEPTH; // 32
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
}
