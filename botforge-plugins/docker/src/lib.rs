//! Docker boot-time assert plugin for the botforge plugin system.
//!
//! Provides the `assert/docker` capability slot.  The host calls
//! `plugin_assert_build_probe` to get a shell script that probes the guest,
//! runs it via SSH, then passes the stdout to `plugin_assert_evaluate` which
//! returns a JSON results document.
//!
//! # Config schema (YAML → JSON)
//!
//! ```yaml
//! assert:
//!   docker:
//!     images:
//!       nginx:latest: { exists: true }
//!       old-image:    { exists: false }
//!     networks:
//!       mynet: { exists: true }
//!     containers:
//!       web:
//!         exists: true
//!         running: true
//!         networks: [mynet, "!badnet"]
//!         ports: ["80/tcp", "!9000/tcp"]
//!         logs:
//!           contains: ["Started"]
//!           not_contains: ["ERROR"]
//!           timeout: 30
//! ```
//!
//! Bare/null map entries (`name:` with no value) behave identically to `{}`
//! (all defaults: `exists: true`).
//!
//! # Probe output order
//!
//! The probe emits sections in this fixed order:
//! 1. **Images** (if images map is non-empty) — full `docker image ls` dump,
//!    terminated by `__END_DOCKER_IMAGES__`.
//! 2. **Networks** (if networks map is non-empty) — full `docker network ls`
//!    dump, terminated by `__END_DOCKER_NETWORKS__`.
//! 3. **Container log wait loops** — for each non-glob container with
//!    `logs.contains` (sorted by container name): polling loop, then
//!    `__LOG_READY_RESULT__<name>:<0|1>`, then the last 1000 log lines, then
//!    `__END_LOG_<name>__`.
//! 4. **Containers** (if containers map is non-empty) — `docker ps -a` dump
//!    as tab-separated `Name\tState\tPorts`, terminated by
//!    `__END_DOCKER_CONTAINERS__`.
//! 5. **Network membership** (if any non-glob containers declare `networks:`)
//!    — a single batched `docker inspect` over those containers with format
//!    `{{.Name}} <net1> <net2> ...`, terminated by `__END_DOCKER_INSPECT__`.
//!
//! # Safety
//!
//! This crate deliberately uses `unsafe` for FFI boundary crossings.
//! Every `unsafe` block is accompanied by a `// SAFETY:` comment.

use std::collections::BTreeMap;
use std::ffi::{CStr, CString};
use std::os::raw::c_char;

use serde::{Deserialize, Serialize};

use botforge_plugin_host::HOST_ABI_VERSION;

// ── Static ABI metadata ───────────────────────────────────────────────────────

static SLOT_ASSERT_DOCKER: &[u8] = b"assert/docker\0";
static NAME_DOCKER: &[u8] = b"docker\0";

// ── Config schema ─────────────────────────────────────────────────────────────

fn default_true() -> bool {
    true
}

fn default_timeout() -> u32 {
    30
}

/// Top-level docker assert configuration.
///
/// Map values may be `null` (bare YAML entry with no body), which is treated
/// identically to `{}` (all-defaults struct).
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct DockerConfig {
    #[serde(default)]
    images: BTreeMap<String, Option<ImageExpect>>,
    #[serde(default)]
    networks: BTreeMap<String, Option<NetworkExpect>>,
    #[serde(default)]
    containers: BTreeMap<String, Option<ContainerExpect>>,
}

/// Expectation for a docker image.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ImageExpect {
    #[serde(default = "default_true")]
    exists: bool,
}

impl Default for ImageExpect {
    fn default() -> Self {
        Self { exists: true }
    }
}

/// Expectation for a docker network.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct NetworkExpect {
    #[serde(default = "default_true")]
    exists: bool,
}

impl Default for NetworkExpect {
    fn default() -> Self {
        Self { exists: true }
    }
}

/// Expectation for a docker container.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ContainerExpect {
    #[serde(default = "default_true")]
    exists: bool,
    /// When `None` and `exists: true`, defaults to `true` (must be running).
    #[serde(default)]
    running: Option<bool>,
    #[serde(default)]
    logs: Option<LogsExpect>,
    /// Each entry is either `"net"` (must attach) or `"!net"` (must NOT attach).
    #[serde(default)]
    networks: Vec<String>,
    /// Each entry is either `"80/tcp"` (must publish) or `"!80/tcp"` (must NOT).
    #[serde(default)]
    ports: Vec<String>,
}

impl Default for ContainerExpect {
    fn default() -> Self {
        Self {
            exists: true,
            running: None,
            logs: None,
            networks: Vec::new(),
            ports: Vec::new(),
        }
    }
}

impl ContainerExpect {
    /// Effective `running` check: `true` when `exists: true` and `running` is absent.
    fn effective_running(&self) -> Option<bool> {
        if !self.exists {
            return None;
        }
        Some(self.running.unwrap_or(true))
    }
}

/// Log-contents expectation for a container.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct LogsExpect {
    #[serde(default)]
    contains: Vec<String>,
    #[serde(default)]
    not_contains: Vec<String>,
    #[serde(default = "default_timeout")]
    timeout: u32,
}

// ── Results schema ────────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
struct CheckResult {
    label: String,
    ok: bool,
    message: Option<String>,
}

#[derive(Debug, Serialize)]
struct Results {
    checks: Vec<CheckResult>,
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn is_glob_pattern(s: &str) -> bool {
    s.contains('*') || s.contains('?') || s.contains('[')
}

/// Shell-single-quote a string so it is safe to embed in a sh script.
fn sq(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Parse published host ports from the `docker ps --format '{{.Ports}}'` field.
///
/// Each entry looks like `0.0.0.0:80->80/tcp` or `:::80->80/tcp` (published)
/// or `80/tcp` (exposed but not published).  A port is considered published
/// when a `HOSTIP:HOSTPORT->CONTAINERPORT/proto` mapping is present.  Returns
/// the deduplicated set of `CONTAINERPORT/proto` values that are published.
fn parse_published_ports(ports_str: &str) -> Vec<String> {
    if ports_str.is_empty() {
        return Vec::new();
    }
    let mut published = std::collections::BTreeSet::new();
    for entry in ports_str.split(", ") {
        let entry = entry.trim();
        if let Some(pos) = entry.find("->") {
            let container_port = &entry[pos + 2..];
            if !container_port.is_empty() {
                published.insert(container_port.to_owned());
            }
        }
    }
    published.into_iter().collect()
}

// ── Probe script generation ───────────────────────────────────────────────────

fn build_probe_script(config: &DockerConfig) -> String {
    let mut out = String::new();
    out.push_str("set -e\n\n");

    // ── IMAGES — one bulk listing for all keys (exact + glob) ────────────────
    if !config.images.is_empty() {
        out.push_str(
            "docker image ls --format '{{.Repository}}:{{.Tag}}' 2>/dev/null\n\
             printf '__END_DOCKER_IMAGES__\\n'\n",
        );
    }

    // ── NETWORKS — one bulk listing for all keys ──────────────────────────────
    if !config.networks.is_empty() {
        out.push_str(
            "docker network ls --format '{{.Name}}' 2>/dev/null\n\
             printf '__END_DOCKER_NETWORKS__\\n'\n",
        );
    }

    // ── CONTAINER LOG READINESS WAIT LOOPS ───────────────────────────────────
    // Emitted only for non-glob containers that declare logs.contains.
    // BTreeMap iteration order is sorted by key — deterministic.
    for (cname, opt_expect) in &config.containers {
        if is_glob_pattern(cname) {
            continue;
        }
        let cexpect = match opt_expect {
            Some(e) => e,
            None => continue, // default ContainerExpect has no logs
        };
        let Some(logs) = cexpect.logs.as_ref() else {
            continue;
        };
        if logs.contains.is_empty() {
            continue;
        }

        let timeout = logs.timeout;
        let cname_sq = sq(cname);

        out.push_str(&format!(
            "_deadline=$(( $(date +%s) + {timeout} ))\n\
             while true; do\n\
             \x20 _found=1\n"
        ));
        for substr in &logs.contains {
            out.push_str(&format!(
                "  docker logs {c} 2>&1 | grep -qF {s} || _found=0\n",
                c = cname_sq,
                s = sq(substr)
            ));
        }
        out.push_str(&format!(
            "  if [ \"$_found\" -eq 1 ] || [ \"$(date +%s)\" -ge \"$_deadline\" ]; then break; fi\n\
             \x20 sleep 2\n\
             done\n\
             printf '__LOG_READY_RESULT__{cname}:%s\\n' \"$_found\"\n\
             docker logs {c} 2>&1 | tail -1000\n\
             printf '__END_LOG_{cname}__\\n'\n",
            cname = cname,
            c = cname_sq
        ));
    }

    // ── CONTAINERS — one bulk ps -a listing (TSV: Name\tState\tPorts) ─────────
    if !config.containers.is_empty() {
        out.push_str(
            "docker ps -a --format '{{.Names}}\\t{{.State}}\\t{{.Ports}}' 2>/dev/null\n\
             printf '__END_DOCKER_CONTAINERS__\\n'\n",
        );
    }

    // ── BATCHED INSPECT for containers with networks: checks ──────────────────
    // Collect non-glob containers that have at least one network check.
    let containers_needing_inspect: Vec<&String> = config
        .containers
        .iter()
        .filter(|(k, v)| !is_glob_pattern(k) && v.as_ref().is_some_and(|e| !e.networks.is_empty()))
        .map(|(k, _)| k)
        .collect();

    if !containers_needing_inspect.is_empty() {
        let names: String = containers_needing_inspect
            .iter()
            .map(|n| sq(n))
            .collect::<Vec<_>>()
            .join(" ");
        // Format: "/container-name net1 net2 ..." (one line per container)
        out.push_str(&format!(
            "docker inspect {names} \
             --format '{{{{.Name}}}} {{{{range $k,$v := .NetworkSettings.Networks}}}}{{{{$k}}}} {{{{end}}}}' \
             2>/dev/null || true\n\
             printf '__END_DOCKER_INSPECT__\\n'\n",
        ));
    }

    out
}

// ── Probe evaluation ──────────────────────────────────────────────────────────

fn evaluate_probe(config: &DockerConfig, probe_stdout: &str) -> Results {
    let mut checks: Vec<CheckResult> = Vec::new();
    let mut lines = probe_stdout.lines().peekable();

    // ── IMAGES ───────────────────────────────────────────────────────────────
    if !config.images.is_empty() {
        // Consume image ls dump until sentinel.
        let mut all_images: Vec<String> = Vec::new();
        loop {
            match lines.next() {
                None | Some("__END_DOCKER_IMAGES__") => break,
                Some(l) if !l.is_empty() => all_images.push(l.to_owned()),
                _ => {}
            }
        }

        for (name, opt_expect) in &config.images {
            let expect = opt_expect.as_ref().cloned().unwrap_or_default();
            let found = if is_glob_pattern(name) {
                all_images.iter().any(|img| glob_match(name, img))
            } else {
                all_images.iter().any(|img| img == name)
            };
            let ok = found == expect.exists;
            let message = if !ok {
                if expect.exists {
                    if is_glob_pattern(name) {
                        Some(format!("no images matching pattern {name:?} found"))
                    } else {
                        Some(format!("image {name} is absent (expected present)"))
                    }
                } else if is_glob_pattern(name) {
                    let matches: Vec<&str> = all_images
                        .iter()
                        .filter(|img| glob_match(name, img))
                        .map(String::as_str)
                        .collect();
                    Some(format!(
                        "images matching pattern {name:?} found: {}",
                        matches.join(", ")
                    ))
                } else {
                    Some(format!("image {name} is present (expected absent)"))
                }
            } else {
                None
            };
            checks.push(CheckResult {
                label: format!(
                    "docker image {name} {}",
                    if expect.exists { "present" } else { "absent" }
                ),
                ok,
                message,
            });
        }
    }

    // ── NETWORKS ─────────────────────────────────────────────────────────────
    if !config.networks.is_empty() {
        let mut all_networks: Vec<String> = Vec::new();
        loop {
            match lines.next() {
                None | Some("__END_DOCKER_NETWORKS__") => break,
                Some(l) if !l.is_empty() => all_networks.push(l.to_owned()),
                _ => {}
            }
        }

        for (name, opt_expect) in &config.networks {
            let expect = opt_expect.as_ref().cloned().unwrap_or_default();
            let found = if is_glob_pattern(name) {
                all_networks.iter().any(|net| glob_match(name, net))
            } else {
                all_networks.iter().any(|net| net == name)
            };
            let ok = found == expect.exists;
            let message = if !ok {
                if expect.exists {
                    if is_glob_pattern(name) {
                        Some(format!("no networks matching pattern {name:?} found"))
                    } else {
                        Some(format!("network {name} is absent (expected present)"))
                    }
                } else if is_glob_pattern(name) {
                    let matches: Vec<&str> = all_networks
                        .iter()
                        .filter(|net| glob_match(name, net))
                        .map(String::as_str)
                        .collect();
                    Some(format!(
                        "networks matching pattern {name:?} found: {}",
                        matches.join(", ")
                    ))
                } else {
                    Some(format!("network {name} is present (expected absent)"))
                }
            } else {
                None
            };
            checks.push(CheckResult {
                label: format!(
                    "docker network {name} {}",
                    if expect.exists { "present" } else { "absent" }
                ),
                ok,
                message,
            });
        }
    }

    // ── CONTAINER LOG READINESS RESULTS ──────────────────────────────────────
    // Per-container log results: map from container name to (ready_bool, log_lines).
    let mut log_results: BTreeMap<String, (bool, Vec<String>)> = BTreeMap::new();

    for (cname, opt_expect) in &config.containers {
        if is_glob_pattern(cname) {
            continue;
        }
        let cexpect = match opt_expect {
            Some(e) => e,
            None => continue,
        };
        let Some(logs) = cexpect.logs.as_ref() else {
            continue;
        };
        if logs.contains.is_empty() {
            continue;
        }

        let ready_sentinel = format!("__LOG_READY_RESULT__{}:", cname);
        let end_sentinel = format!("__END_LOG_{}__", cname);

        let mut ready = false;
        let mut log_lines: Vec<String> = Vec::new();

        loop {
            match lines.next() {
                None => break,
                Some(l) if l.starts_with(&ready_sentinel) => {
                    let val = l.trim_start_matches(&ready_sentinel);
                    ready = val == "1";
                    loop {
                        match lines.next() {
                            None => break,
                            Some(ll) if ll == end_sentinel => break,
                            Some(ll) => log_lines.push(ll.to_owned()),
                        }
                    }
                    break;
                }
                Some(_) => {}
            }
        }

        log_results.insert(cname.clone(), (ready, log_lines));
    }

    // ── CONTAINERS ───────────────────────────────────────────────────────────
    if !config.containers.is_empty() {
        // Parse the docker ps -a TSV dump.
        // Format: Name\tState\tPorts  (tab-separated, docker interprets \t as tab)
        struct PsRow {
            state: String,
            published_ports: Vec<String>,
        }
        let mut ps_map: BTreeMap<String, PsRow> = BTreeMap::new();

        loop {
            match lines.next() {
                None | Some("__END_DOCKER_CONTAINERS__") => break,
                Some(l) => {
                    let mut parts = l.splitn(3, '\t');
                    let name = parts.next().unwrap_or("").trim().to_owned();
                    let state = parts.next().unwrap_or("").trim().to_owned();
                    let ports_str = parts.next().unwrap_or("").trim();
                    let published_ports = parse_published_ports(ports_str);
                    if !name.is_empty() {
                        ps_map.insert(
                            name,
                            PsRow {
                                state,
                                published_ports,
                            },
                        );
                    }
                }
            }
        }

        // Parse batched docker inspect dump for network membership.
        // Only present when containers_needing_inspect is non-empty.
        let containers_needing_inspect: Vec<&String> = config
            .containers
            .iter()
            .filter(|(k, v)| {
                !is_glob_pattern(k) && v.as_ref().is_some_and(|e| !e.networks.is_empty())
            })
            .map(|(k, _)| k)
            .collect();

        let mut inspect_networks: BTreeMap<String, Vec<String>> = BTreeMap::new();
        if !containers_needing_inspect.is_empty() {
            loop {
                match lines.next() {
                    None | Some("__END_DOCKER_INSPECT__") => break,
                    Some("") => {}
                    Some(l) => {
                        let mut parts = l.split_whitespace();
                        if let Some(raw_name) = parts.next() {
                            // docker inspect .Name starts with "/"
                            let name = raw_name.strip_prefix('/').unwrap_or(raw_name).to_owned();
                            let nets: Vec<String> = parts.map(str::to_owned).collect();
                            inspect_networks.insert(name, nets);
                        }
                    }
                }
            }
        }

        // Evaluate per-container checks.
        for (cname, opt_expect) in &config.containers {
            let cexpect = opt_expect.as_ref().cloned().unwrap_or_default();

            if is_glob_pattern(cname) {
                // Pattern containers: existence check only (from PS map).
                let matches: Vec<&str> = ps_map
                    .keys()
                    .filter(|n| glob_match(cname, n))
                    .map(String::as_str)
                    .collect();
                let found = !matches.is_empty();
                let ok = found == cexpect.exists;
                let message = if !ok {
                    if cexpect.exists {
                        Some(format!("no containers matching pattern {cname:?} found"))
                    } else {
                        Some(format!(
                            "containers matching pattern {cname:?} found: {}",
                            matches.join(", ")
                        ))
                    }
                } else {
                    None
                };
                checks.push(CheckResult {
                    label: format!(
                        "docker container {cname} {}",
                        if cexpect.exists { "present" } else { "absent" }
                    ),
                    ok,
                    message,
                });
                continue;
            }

            // Exact container check.
            let row = ps_map.get(cname);
            let present = row.is_some();

            // Existence check.
            let exists_ok = present == cexpect.exists;
            checks.push(CheckResult {
                label: format!(
                    "docker container {cname} {}",
                    if cexpect.exists { "present" } else { "absent" }
                ),
                ok: exists_ok,
                message: if !exists_ok {
                    if cexpect.exists {
                        Some(format!("container {cname} is absent (expected present)"))
                    } else {
                        Some(format!("container {cname} is present (expected absent)"))
                    }
                } else {
                    None
                },
            });

            // Further checks only when container is present.
            if !present {
                continue;
            }
            let row = row.unwrap();
            let is_running = row.state == "running";

            // Running check.
            if let Some(expected_running) = cexpect.effective_running() {
                let running_ok = is_running == expected_running;
                checks.push(CheckResult {
                    label: format!(
                        "docker container {cname} {}",
                        if expected_running {
                            "running"
                        } else {
                            "stopped"
                        }
                    ),
                    ok: running_ok,
                    message: if !running_ok {
                        if expected_running {
                            Some(format!(
                                "container {cname} is not running (expected running)"
                            ))
                        } else {
                            Some(format!(
                                "container {cname} is running (expected not running)"
                            ))
                        }
                    } else {
                        None
                    },
                });
            }

            // Network membership checks (from batched inspect).
            for net_spec in &cexpect.networks {
                let (net_name, must_attach) = if let Some(stripped) = net_spec.strip_prefix('!') {
                    (stripped, false)
                } else {
                    (net_spec.as_str(), true)
                };
                let attached = inspect_networks
                    .get(cname)
                    .is_some_and(|nets| nets.iter().any(|n| n == net_name));
                let ok = attached == must_attach;
                let message = if !ok {
                    if must_attach {
                        Some(format!(
                            "container {cname} is not attached to network {net_name}"
                        ))
                    } else {
                        Some(format!(
                            "container {cname} is unexpectedly attached to network {net_name}"
                        ))
                    }
                } else {
                    None
                };
                checks.push(CheckResult {
                    label: format!(
                        "docker container {cname} network {net_name} {}",
                        if must_attach {
                            "attached"
                        } else {
                            "not attached"
                        }
                    ),
                    ok,
                    message,
                });
            }

            // Port checks (from PS dump published_ports).
            for port_spec in &cexpect.ports {
                let (port, must_publish) = if let Some(stripped) = port_spec.strip_prefix('!') {
                    (stripped, false)
                } else {
                    (port_spec.as_str(), true)
                };
                let published = row.published_ports.iter().any(|p| p == port);
                let ok = published == must_publish;
                let message = if !ok {
                    if must_publish {
                        Some(format!("container {cname} does not publish port {port}"))
                    } else {
                        Some(format!(
                            "container {cname} unexpectedly publishes port {port}"
                        ))
                    }
                } else {
                    None
                };
                checks.push(CheckResult {
                    label: format!(
                        "docker container {cname} port {port} {}",
                        if must_publish {
                            "published"
                        } else {
                            "not published"
                        }
                    ),
                    ok,
                    message,
                });
            }

            // Log checks from the pre-collected log results.
            if let Some(logs) = cexpect.logs.as_ref() {
                let (ready, log_lines) = log_results
                    .get(cname.as_str())
                    .map(|(r, l)| (*r, l.clone()))
                    .unwrap_or((false, Vec::new()));

                // not_contains: evaluated immediately from captured logs.
                for substr in &logs.not_contains {
                    let found = log_lines.iter().any(|l| l.contains(substr.as_str()));
                    let ok = !found;
                    let message = if !ok {
                        Some(format!(
                            "container {cname} logs contain unexpected string {substr:?}"
                        ))
                    } else {
                        None
                    };
                    checks.push(CheckResult {
                        label: format!("docker container {cname} logs not_contains {substr:?}"),
                        ok,
                        message,
                    });
                }

                // contains: the ready flag tells us whether the wait loop found all substrings.
                if !logs.contains.is_empty() {
                    let ok = ready;
                    let message = if !ok {
                        Some(format!(
                            "container {cname} logs did not contain all expected strings within {} seconds",
                            logs.timeout
                        ))
                    } else {
                        None
                    };
                    checks.push(CheckResult {
                        label: format!("docker container {cname} logs contains"),
                        ok,
                        message,
                    });
                }
            }
        }
    }

    Results { checks }
}

// ── Glob matching ─────────────────────────────────────────────────────────────

/// Minimal glob matcher supporting `*`, `?`, and `[...]`.
fn glob_match(pattern: &str, value: &str) -> bool {
    glob_match_impl(pattern.as_bytes(), value.as_bytes())
}

fn glob_match_impl(pat: &[u8], val: &[u8]) -> bool {
    match (pat.first(), val.first()) {
        (None, None) => true,
        (Some(b'*'), _) => {
            if glob_match_impl(&pat[1..], val) {
                return true;
            }
            if !val.is_empty() {
                return glob_match_impl(pat, &val[1..]);
            }
            false
        }
        (Some(b'?'), Some(_)) => glob_match_impl(&pat[1..], &val[1..]),
        (Some(b'['), Some(&vc)) => {
            let end = match pat[1..].iter().position(|&b| b == b']') {
                Some(i) => i + 1,
                None => return false,
            };
            let class = &pat[1..end];
            let (negate, class) = if class.first() == Some(&b'!') {
                (true, &class[1..])
            } else {
                (false, class)
            };
            let mut matched = false;
            let mut i = 0;
            while i < class.len() {
                if i + 2 < class.len() && class[i + 1] == b'-' {
                    if vc >= class[i] && vc <= class[i + 2] {
                        matched = true;
                    }
                    i += 3;
                } else {
                    if vc == class[i] {
                        matched = true;
                    }
                    i += 1;
                }
            }
            if matched != negate {
                glob_match_impl(&pat[end + 1..], &val[1..])
            } else {
                false
            }
        }
        (Some(&pc), Some(&vc)) if pc == vc => glob_match_impl(&pat[1..], &val[1..]),
        _ => false,
    }
}

// ── FFI helpers ───────────────────────────────────────────────────────────────

/// Parse a NUL-terminated C string pointer into a Rust `&str`.
///
/// Returns `None` if `ptr` is null or the bytes are not valid UTF-8.
///
/// # Safety
///
/// `ptr` must be either null or a valid NUL-terminated C string for at least
/// the duration of the borrow.
unsafe fn cstr_to_str<'a>(ptr: *const c_char) -> Option<&'a str> {
    if ptr.is_null() {
        return None;
    }
    // SAFETY: caller guarantees ptr is a valid NUL-terminated C string.
    unsafe { CStr::from_ptr(ptr) }.to_str().ok()
}

/// Allocate a NUL-terminated C string and write its pointer to `*out`.
///
/// Ownership is transferred to the caller; must be freed with
/// `plugin_assert_free`.
///
/// # Safety
///
/// `out` must be a valid non-null pointer to a `*mut c_char`.
unsafe fn write_cstring_out(s: String, out: *mut *mut c_char) {
    let cs = CString::new(s).unwrap_or_default();
    // SAFETY: into_raw transfers ownership; freed by plugin_assert_free.
    unsafe { *out = cs.into_raw() };
}

// ── ABI exports ───────────────────────────────────────────────────────────────

#[no_mangle]
pub extern "C" fn abi_version() -> u32 {
    HOST_ABI_VERSION
}

#[no_mangle]
pub extern "C" fn plugin_provides_count() -> u32 {
    1
}

#[no_mangle]
pub extern "C" fn plugin_provides_slot(index: u32) -> *const c_char {
    match index {
        0 => SLOT_ASSERT_DOCKER.as_ptr().cast(),
        _ => std::ptr::null(),
    }
}

#[no_mangle]
pub extern "C" fn plugin_provides_name(index: u32) -> *const c_char {
    match index {
        0 => NAME_DOCKER.as_ptr().cast(),
        _ => std::ptr::null(),
    }
}

/// Build the guest probe shell script from a JSON-encoded `DockerConfig`.
///
/// On success, writes the script to `*out_ptr` and returns 0.
/// On failure, writes a human-readable error string to `*out_error` and
/// returns a non-zero error code.
///
/// # Safety
///
/// `config_json` must be a valid NUL-terminated C string.
/// `out_ptr` and `out_error` must be valid non-null pointers to `*mut c_char`.
#[no_mangle]
pub unsafe extern "C" fn plugin_assert_build_probe(
    config_json: *const c_char,
    out_ptr: *mut *mut c_char,
    out_error: *mut *mut c_char,
) -> i32 {
    // SAFETY: caller guarantees config_json is a valid NUL-terminated C string.
    let json_str = match unsafe { cstr_to_str(config_json) } {
        Some(s) => s,
        None => return 1,
    };

    let config: DockerConfig = match serde_json::from_str(json_str) {
        Ok(c) => c,
        Err(e) => {
            if !out_error.is_null() {
                // SAFETY: out_error is a valid non-null pointer per ABI contract.
                unsafe { write_cstring_out(e.to_string(), out_error) };
            }
            return 2;
        }
    };

    let script = build_probe_script(&config);

    // SAFETY: out_ptr is a valid non-null pointer per ABI contract.
    unsafe { write_cstring_out(script, out_ptr) };
    0
}

/// Evaluate captured probe stdout against a JSON-encoded `DockerConfig`.
///
/// On success, writes the results JSON to `*out_ptr` and returns 0.
/// On failure, writes a human-readable error string to `*out_error` and
/// returns a non-zero error code.
///
/// # Safety
///
/// `config_json` and `probe_stdout` must be valid NUL-terminated C strings.
/// `out_ptr` and `out_error` must be valid non-null pointers to `*mut c_char`.
#[no_mangle]
pub unsafe extern "C" fn plugin_assert_evaluate(
    config_json: *const c_char,
    probe_stdout: *const c_char,
    out_ptr: *mut *mut c_char,
    out_error: *mut *mut c_char,
) -> i32 {
    // SAFETY: caller guarantees both are valid NUL-terminated C strings.
    let json_str = match unsafe { cstr_to_str(config_json) } {
        Some(s) => s,
        None => return 1,
    };
    let stdout_str = match unsafe { cstr_to_str(probe_stdout) } {
        Some(s) => s,
        None => return 1,
    };

    let config: DockerConfig = match serde_json::from_str(json_str) {
        Ok(c) => c,
        Err(e) => {
            if !out_error.is_null() {
                // SAFETY: out_error is a valid non-null pointer per ABI contract.
                unsafe { write_cstring_out(e.to_string(), out_error) };
            }
            return 2;
        }
    };

    let results = evaluate_probe(&config, stdout_str);
    let json = match serde_json::to_string(&results) {
        Ok(j) => j,
        Err(_) => return 3,
    };

    // SAFETY: out_ptr is a valid non-null pointer per ABI contract.
    unsafe { write_cstring_out(json, out_ptr) };
    0
}

/// Free a C string previously returned by `plugin_assert_build_probe` or
/// `plugin_assert_evaluate`.
///
/// # Safety
///
/// `ptr` must be either null or a pointer previously returned by this plugin
/// via `plugin_assert_build_probe` or `plugin_assert_evaluate`.
/// Must be called exactly once per non-null pointer.
#[no_mangle]
pub unsafe extern "C" fn plugin_assert_free(ptr: *mut c_char) {
    if ptr.is_null() {
        return;
    }
    // SAFETY: ptr was created by CString::into_raw in this plugin.
    unsafe { drop(CString::from_raw(ptr)) };
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_match_star() {
        assert!(glob_match("nginx:*", "nginx:latest"));
        assert!(glob_match("*:latest", "nginx:latest"));
        assert!(!glob_match("nginx:*", "apache:latest"));
    }

    #[test]
    fn glob_match_question() {
        assert!(glob_match("web?", "web1"));
        assert!(glob_match("web?", "web2"));
        assert!(!glob_match("web?", "web12"));
    }

    #[test]
    fn glob_match_exact() {
        assert!(glob_match("nginx", "nginx"));
        assert!(!glob_match("nginx", "apache"));
    }

    #[test]
    fn is_glob_pattern_detects_patterns() {
        assert!(is_glob_pattern("nginx:*"));
        assert!(is_glob_pattern("web?"));
        assert!(is_glob_pattern("[abc]"));
        assert!(!is_glob_pattern("nginx:latest"));
    }

    // ── parse_published_ports tests ───────────────────────────────────────────

    #[test]
    fn parse_published_ports_empty() {
        assert!(parse_published_ports("").is_empty());
    }

    #[test]
    fn parse_published_ports_exposed_only() {
        // Exposed but not published (no -> mapping)
        assert!(parse_published_ports("80/tcp").is_empty());
    }

    #[test]
    fn parse_published_ports_ipv4_and_ipv6() {
        let ports = parse_published_ports("0.0.0.0:9400->9400/tcp, :::9400->9400/tcp");
        assert_eq!(ports, vec!["9400/tcp"]);
    }

    #[test]
    fn parse_published_ports_multiple_ports() {
        let ports = parse_published_ports(
            "0.0.0.0:80->80/tcp, :::80->80/tcp, 0.0.0.0:443->443/tcp, :::443->443/tcp",
        );
        assert!(ports.contains(&"80/tcp".to_owned()));
        assert!(ports.contains(&"443/tcp".to_owned()));
        assert_eq!(ports.len(), 2);
    }

    // ── probe_script tests ────────────────────────────────────────────────────

    #[test]
    fn probe_script_empty_config() {
        let config = DockerConfig {
            images: BTreeMap::new(),
            networks: BTreeMap::new(),
            containers: BTreeMap::new(),
        };
        let script = build_probe_script(&config);
        assert!(script.starts_with("set -e\n"));
        // No images → no image ls
        assert!(!script.contains("docker image ls"));
        // No containers → no ps
        assert!(!script.contains("docker ps"));
    }

    #[test]
    fn probe_script_images_emit_bulk_listing() {
        let mut images = BTreeMap::new();
        images.insert(
            "nginx:latest".to_owned(),
            Some(ImageExpect { exists: true }),
        );
        images.insert("nginx:*".to_owned(), Some(ImageExpect { exists: true }));
        let config = DockerConfig {
            images,
            networks: BTreeMap::new(),
            containers: BTreeMap::new(),
        };
        let script = build_probe_script(&config);
        assert!(script.contains("docker image ls"), "script: {script}");
        assert!(script.contains("__END_DOCKER_IMAGES__"), "script: {script}");
        // No per-image inspect
        assert!(!script.contains("docker image inspect"), "script: {script}");
    }

    #[test]
    fn probe_script_networks_emit_bulk_listing() {
        let mut networks = BTreeMap::new();
        networks.insert("mynet".to_owned(), Some(NetworkExpect { exists: true }));
        let config = DockerConfig {
            images: BTreeMap::new(),
            networks,
            containers: BTreeMap::new(),
        };
        let script = build_probe_script(&config);
        assert!(script.contains("docker network ls"), "script: {script}");
        assert!(
            script.contains("__END_DOCKER_NETWORKS__"),
            "script: {script}"
        );
        assert!(
            !script.contains("docker network inspect"),
            "script: {script}"
        );
    }

    #[test]
    fn probe_script_containers_emit_ps_dump() {
        let mut containers = BTreeMap::new();
        containers.insert(
            "web".to_owned(),
            Some(ContainerExpect {
                exists: true,
                running: None,
                logs: None,
                networks: vec![],
                ports: vec![],
            }),
        );
        let config = DockerConfig {
            images: BTreeMap::new(),
            networks: BTreeMap::new(),
            containers,
        };
        let script = build_probe_script(&config);
        assert!(script.contains("docker ps -a"), "script: {script}");
        assert!(
            script.contains("__END_DOCKER_CONTAINERS__"),
            "script: {script}"
        );
        // No per-container inspect on the existence/running path
        assert!(!script.contains("docker inspect 'web'"), "script: {script}");
    }

    #[test]
    fn probe_script_container_with_networks_emits_batched_inspect() {
        let mut containers = BTreeMap::new();
        containers.insert(
            "web".to_owned(),
            Some(ContainerExpect {
                exists: true,
                running: None,
                logs: None,
                networks: vec!["mynet".to_owned()],
                ports: vec![],
            }),
        );
        let config = DockerConfig {
            images: BTreeMap::new(),
            networks: BTreeMap::new(),
            containers,
        };
        let script = build_probe_script(&config);
        assert!(
            script.contains("docker inspect 'web'"),
            "batched inspect: {script}"
        );
        assert!(
            script.contains("__END_DOCKER_INSPECT__"),
            "inspect sentinel: {script}"
        );
        // Only one docker inspect call (batched)
        assert_eq!(
            script.matches("docker inspect").count(),
            1,
            "only one inspect call: {script}"
        );
    }

    #[test]
    fn probe_script_container_without_networks_no_inspect() {
        let mut containers = BTreeMap::new();
        containers.insert(
            "web".to_owned(),
            Some(ContainerExpect {
                exists: true,
                running: None,
                logs: None,
                networks: vec![],
                ports: vec![],
            }),
        );
        let config = DockerConfig {
            images: BTreeMap::new(),
            networks: BTreeMap::new(),
            containers,
        };
        let script = build_probe_script(&config);
        assert!(
            !script.contains("__END_DOCKER_INSPECT__"),
            "script: {script}"
        );
    }

    #[test]
    fn probe_script_no_per_container_port_call() {
        // Ports are now read from docker ps --format; no docker port subprocess.
        let mut containers = BTreeMap::new();
        containers.insert(
            "web".to_owned(),
            Some(ContainerExpect {
                exists: true,
                running: None,
                logs: None,
                networks: vec![],
                ports: vec!["80/tcp".to_owned()],
            }),
        );
        let config = DockerConfig {
            images: BTreeMap::new(),
            networks: BTreeMap::new(),
            containers,
        };
        let script = build_probe_script(&config);
        assert!(!script.contains("docker port"), "script: {script}");
    }

    #[test]
    fn probe_script_batched_inspect_multiple_containers() {
        let mut containers = BTreeMap::new();
        for name in &["alpha", "beta", "gamma"] {
            containers.insert(
                name.to_string(),
                Some(ContainerExpect {
                    exists: true,
                    running: None,
                    logs: None,
                    networks: vec!["mynet".to_owned()],
                    ports: vec![],
                }),
            );
        }
        let config = DockerConfig {
            images: BTreeMap::new(),
            networks: BTreeMap::new(),
            containers,
        };
        let script = build_probe_script(&config);
        // Only ONE docker inspect call total
        assert_eq!(
            script.matches("docker inspect").count(),
            1,
            "only one batched inspect: {script}"
        );
        assert!(script.contains("'alpha'"), "alpha in inspect: {script}");
        assert!(script.contains("'beta'"), "beta in inspect: {script}");
        assert!(script.contains("'gamma'"), "gamma in inspect: {script}");
    }

    // ── evaluate_probe tests ──────────────────────────────────────────────────

    fn make_image_config(name: &str, exists: bool) -> DockerConfig {
        let mut images = BTreeMap::new();
        images.insert(name.to_owned(), Some(ImageExpect { exists }));
        DockerConfig {
            images,
            networks: BTreeMap::new(),
            containers: BTreeMap::new(),
        }
    }

    fn make_network_config(name: &str, exists: bool) -> DockerConfig {
        let mut networks = BTreeMap::new();
        networks.insert(name.to_owned(), Some(NetworkExpect { exists }));
        DockerConfig {
            images: BTreeMap::new(),
            networks,
            containers: BTreeMap::new(),
        }
    }

    #[test]
    fn evaluate_exact_image_present() {
        let config = make_image_config("nginx:latest", true);
        // Image dump contains nginx:latest; sentinel follows.
        let results = evaluate_probe(&config, "nginx:latest\n__END_DOCKER_IMAGES__\n");
        assert_eq!(results.checks.len(), 1);
        assert!(results.checks[0].ok);
        assert_eq!(results.checks[0].label, "docker image nginx:latest present");
    }

    #[test]
    fn evaluate_exact_image_absent_when_expected_absent() {
        let config = make_image_config("old:latest", false);
        let results = evaluate_probe(&config, "__END_DOCKER_IMAGES__\n");
        assert_eq!(results.checks.len(), 1);
        assert!(results.checks[0].ok);
    }

    #[test]
    fn evaluate_exact_image_present_when_expected_absent_fails() {
        let config = make_image_config("old:latest", false);
        let results = evaluate_probe(&config, "old:latest\n__END_DOCKER_IMAGES__\n");
        assert_eq!(results.checks.len(), 1);
        assert!(!results.checks[0].ok);
    }

    #[test]
    fn evaluate_pattern_image_matches() {
        let mut images = BTreeMap::new();
        images.insert("nginx:*".to_owned(), Some(ImageExpect { exists: true }));
        let config = DockerConfig {
            images,
            networks: BTreeMap::new(),
            containers: BTreeMap::new(),
        };
        let results = evaluate_probe(&config, "nginx:latest\nnginx:1.25\n__END_DOCKER_IMAGES__\n");
        assert_eq!(results.checks.len(), 1);
        assert!(results.checks[0].ok);
    }

    #[test]
    fn evaluate_exact_network_present() {
        let config = make_network_config("mynet", true);
        let results = evaluate_probe(&config, "mynet\n__END_DOCKER_NETWORKS__\n");
        assert_eq!(results.checks.len(), 1);
        assert!(results.checks[0].ok);
    }

    #[test]
    fn evaluate_exact_network_absent_when_expected_absent() {
        let config = make_network_config("badnet", false);
        let results = evaluate_probe(&config, "__END_DOCKER_NETWORKS__\n");
        assert_eq!(results.checks.len(), 1);
        assert!(results.checks[0].ok);
    }

    #[test]
    fn evaluate_exact_container_present_running() {
        let mut containers = BTreeMap::new();
        containers.insert(
            "web".to_owned(),
            Some(ContainerExpect {
                exists: true,
                running: None,
                logs: None,
                networks: vec![],
                ports: vec![],
            }),
        );
        let config = DockerConfig {
            images: BTreeMap::new(),
            networks: BTreeMap::new(),
            containers,
        };
        // PS dump: name\tstate\tports (tab-separated)
        let results = evaluate_probe(&config, "web\trunning\t\n__END_DOCKER_CONTAINERS__\n");
        assert_eq!(results.checks.len(), 2); // exists + running
        assert!(results.checks[0].ok, "exists: {:?}", results.checks[0]);
        assert!(results.checks[1].ok, "running: {:?}", results.checks[1]);
    }

    #[test]
    fn evaluate_container_absent_fails_exists_check() {
        let mut containers = BTreeMap::new();
        containers.insert(
            "web".to_owned(),
            Some(ContainerExpect {
                exists: true,
                running: None,
                logs: None,
                networks: vec![],
                ports: vec![],
            }),
        );
        let config = DockerConfig {
            images: BTreeMap::new(),
            networks: BTreeMap::new(),
            containers,
        };
        // Empty PS dump
        let results = evaluate_probe(&config, "__END_DOCKER_CONTAINERS__\n");
        assert_eq!(results.checks.len(), 1);
        assert!(
            !results.checks[0].ok,
            "should fail: {:?}",
            results.checks[0]
        );
    }

    #[test]
    fn evaluate_container_network_attached() {
        let mut containers = BTreeMap::new();
        containers.insert(
            "web".to_owned(),
            Some(ContainerExpect {
                exists: true,
                running: Some(true),
                logs: None,
                networks: vec!["mynet".to_owned()],
                ports: vec![],
            }),
        );
        let config = DockerConfig {
            images: BTreeMap::new(),
            networks: BTreeMap::new(),
            containers,
        };
        // PS dump + inspect dump
        let stdout =
            "web\trunning\t\n__END_DOCKER_CONTAINERS__\n/web mynet \n__END_DOCKER_INSPECT__\n";
        let results = evaluate_probe(&config, stdout);
        let net_check = results
            .checks
            .iter()
            .find(|c| c.label.contains("network"))
            .unwrap();
        assert!(net_check.ok, "network check: {:?}", net_check);
    }

    #[test]
    fn evaluate_container_network_not_attached_when_required_fails() {
        let mut containers = BTreeMap::new();
        containers.insert(
            "web".to_owned(),
            Some(ContainerExpect {
                exists: true,
                running: Some(true),
                logs: None,
                networks: vec!["mynet".to_owned()],
                ports: vec![],
            }),
        );
        let config = DockerConfig {
            images: BTreeMap::new(),
            networks: BTreeMap::new(),
            containers,
        };
        // Container present but on different network
        let stdout =
            "web\trunning\t\n__END_DOCKER_CONTAINERS__\n/web othernet \n__END_DOCKER_INSPECT__\n";
        let results = evaluate_probe(&config, stdout);
        let net_check = results
            .checks
            .iter()
            .find(|c| c.label.contains("network"))
            .unwrap();
        assert!(!net_check.ok, "should fail: {:?}", net_check);
    }

    #[test]
    fn evaluate_container_network_negation() {
        let mut containers = BTreeMap::new();
        containers.insert(
            "web".to_owned(),
            Some(ContainerExpect {
                exists: true,
                running: None,
                logs: None,
                networks: vec!["!badnet".to_owned()],
                ports: vec![],
            }),
        );
        let config = DockerConfig {
            images: BTreeMap::new(),
            networks: BTreeMap::new(),
            containers,
        };
        // Container on goodnet, not on badnet → !badnet check passes
        let stdout =
            "web\trunning\t\n__END_DOCKER_CONTAINERS__\n/web goodnet \n__END_DOCKER_INSPECT__\n";
        let results = evaluate_probe(&config, stdout);
        let net_check = results
            .checks
            .iter()
            .find(|c| c.label.contains("network"))
            .unwrap();
        assert!(net_check.ok, "negation check: {:?}", net_check);
    }

    #[test]
    fn evaluate_container_port_published() {
        let mut containers = BTreeMap::new();
        containers.insert(
            "web".to_owned(),
            Some(ContainerExpect {
                exists: true,
                running: None,
                logs: None,
                networks: vec![],
                ports: vec!["9400/tcp".to_owned()],
            }),
        );
        let config = DockerConfig {
            images: BTreeMap::new(),
            networks: BTreeMap::new(),
            containers,
        };
        let stdout =
            "web\trunning\t0.0.0.0:9400->9400/tcp, :::9400->9400/tcp\n__END_DOCKER_CONTAINERS__\n";
        let results = evaluate_probe(&config, stdout);
        let port_check = results
            .checks
            .iter()
            .find(|c| c.label.contains("port"))
            .unwrap();
        assert!(port_check.ok, "port published check: {:?}", port_check);
    }

    #[test]
    fn evaluate_container_port_not_published_when_required_fails() {
        let mut containers = BTreeMap::new();
        containers.insert(
            "web".to_owned(),
            Some(ContainerExpect {
                exists: true,
                running: None,
                logs: None,
                networks: vec![],
                ports: vec!["9400/tcp".to_owned()],
            }),
        );
        let config = DockerConfig {
            images: BTreeMap::new(),
            networks: BTreeMap::new(),
            containers,
        };
        // Port exposed but not published
        let stdout = "web\trunning\t9400/tcp\n__END_DOCKER_CONTAINERS__\n";
        let results = evaluate_probe(&config, stdout);
        let port_check = results
            .checks
            .iter()
            .find(|c| c.label.contains("port"))
            .unwrap();
        assert!(!port_check.ok, "should fail: {:?}", port_check);
    }

    #[test]
    fn evaluate_container_port_negation() {
        let mut containers = BTreeMap::new();
        containers.insert(
            "web".to_owned(),
            Some(ContainerExpect {
                exists: true,
                running: None,
                logs: None,
                networks: vec![],
                ports: vec!["!9000/tcp".to_owned()],
            }),
        );
        let config = DockerConfig {
            images: BTreeMap::new(),
            networks: BTreeMap::new(),
            containers,
        };
        // 9000 not published → !9000/tcp passes
        let stdout = "web\trunning\t0.0.0.0:80->80/tcp\n__END_DOCKER_CONTAINERS__\n";
        let results = evaluate_probe(&config, stdout);
        let port_check = results
            .checks
            .iter()
            .find(|c| c.label.contains("port"))
            .unwrap();
        assert!(port_check.ok, "negation port check: {:?}", port_check);
    }

    // ── null/bare map entry tests (Item 2) ────────────────────────────────────

    #[test]
    fn null_map_values_deserialize_without_error() {
        let json = r#"{"images":{"x:local":null},"networks":{"n":null},"containers":{"c":null}}"#;
        let config: DockerConfig =
            serde_json::from_str(json).expect("null map values must deserialize without error");
        assert!(config.images.get("x:local").is_some());
        assert!(config.networks.get("n").is_some());
        assert!(config.containers.get("c").is_some());
    }

    #[test]
    fn null_image_behaves_as_exists_true() {
        // null → default → {exists: true}
        let json = r#"{"images":{"x:local":null},"networks":{},"containers":{}}"#;
        let config: DockerConfig = serde_json::from_str(json).unwrap();
        let stdout = "x:local\n__END_DOCKER_IMAGES__\n";
        let results = evaluate_probe(&config, stdout);
        assert_eq!(results.checks.len(), 1, "{:?}", results.checks);
        assert!(results.checks[0].ok, "null image should behave as exists:true — check should pass when image is present: {:?}", results.checks[0]);
    }

    #[test]
    fn null_network_behaves_as_exists_true() {
        let json = r#"{"images":{},"networks":{"n":null},"containers":{}}"#;
        let config: DockerConfig = serde_json::from_str(json).unwrap();
        let stdout = "n\n__END_DOCKER_NETWORKS__\n";
        let results = evaluate_probe(&config, stdout);
        assert_eq!(results.checks.len(), 1, "{:?}", results.checks);
        assert!(
            results.checks[0].ok,
            "null network → exists:true: {:?}",
            results.checks[0]
        );
    }

    #[test]
    fn null_container_behaves_as_exists_true() {
        let json = r#"{"images":{},"networks":{},"containers":{"c":null}}"#;
        let config: DockerConfig = serde_json::from_str(json).unwrap();
        // Container present and running
        let stdout = "c\trunning\t\n__END_DOCKER_CONTAINERS__\n";
        let results = evaluate_probe(&config, stdout);
        // exists check + running check (default effective_running = true)
        assert!(results.checks.len() >= 1, "{:?}", results.checks);
        assert!(
            results.checks[0].ok,
            "null container → exists:true: {:?}",
            results.checks[0]
        );
    }

    #[test]
    fn roundtrip_json_config() {
        let json = r#"{"images":{"nginx:latest":{"exists":true}},"networks":{},"containers":{"web":{"exists":true,"running":null,"logs":null,"networks":[],"ports":[]}}}"#;
        let config: DockerConfig = serde_json::from_str(json).unwrap();
        assert!(config.images.contains_key("nginx:latest"));
        assert!(config.containers.contains_key("web"));
    }
}
