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
//! # Probe output order
//!
//! The probe emits sections in this fixed order:
//! 1. **Images** — one `present`/`absent` line per exact image (sorted),
//!    followed by full `docker image ls` output + `__END_DOCKER_IMAGES__`
//!    sentinel (only when pattern images are present).
//! 2. **Networks** — one `present`/`absent` line per exact network (sorted),
//!    followed by full `docker network ls` output + `__END_DOCKER_NETWORKS__`
//!    sentinel (only when pattern networks are present).
//! 3. **Container log wait loops** — for each container with `logs.contains`
//!    (sorted by container name): a polling loop, then `__LOG_READY_RESULT__<name>:<0|1>`,
//!    then the last 1000 log lines, then `__END_LOG_<name>__`.
//! 4. **Containers** — for each exact container (sorted): one state line
//!    `present:<running>:<networks>` or `absent`, then `docker port` output
//!    \+ `__END_PORTS_<name>__` (only when ports checks exist), followed by
//!    full `docker ps -a` output + `__END_DOCKER_CONTAINERS__` sentinel
//!    (only when pattern containers are present).
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
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct DockerConfig {
    #[serde(default)]
    images: BTreeMap<String, ImageExpect>,
    #[serde(default)]
    networks: BTreeMap<String, NetworkExpect>,
    #[serde(default)]
    containers: BTreeMap<String, ContainerExpect>,
}

/// Expectation for a docker image.
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ImageExpect {
    #[serde(default = "default_true")]
    exists: bool,
}

/// Expectation for a docker network.
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct NetworkExpect {
    #[serde(default = "default_true")]
    exists: bool,
}

/// Expectation for a docker container.
#[derive(Debug, Deserialize, Serialize)]
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
#[derive(Debug, Deserialize, Serialize)]
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

// ── Probe script generation ───────────────────────────────────────────────────

fn build_probe_script(config: &DockerConfig) -> String {
    let mut out = String::new();
    out.push_str("set -e\n\n");

    // ── IMAGES ───────────────────────────────────────────────────────────────
    let (exact_images, pattern_images): (Vec<_>, Vec<_>) =
        config.images.iter().partition(|(k, _)| !is_glob_pattern(k));

    for (name, _) in &exact_images {
        out.push_str(&format!(
            "docker image inspect {n} >/dev/null 2>&1 && printf 'present\\n' || printf 'absent\\n'\n",
            n = sq(name)
        ));
    }
    if !pattern_images.is_empty() {
        out.push_str(
            "docker image ls --format '{{.Repository}}:{{.Tag}}' 2>/dev/null\n\
             printf '__END_DOCKER_IMAGES__\\n'\n",
        );
    }

    // ── NETWORKS ─────────────────────────────────────────────────────────────
    let (exact_networks, pattern_networks): (Vec<_>, Vec<_>) =
        config.networks.iter().partition(|(k, _)| !is_glob_pattern(k));

    for (name, _) in &exact_networks {
        out.push_str(&format!(
            "docker network inspect {n} >/dev/null 2>&1 && printf 'present\\n' || printf 'absent\\n'\n",
            n = sq(name)
        ));
    }
    if !pattern_networks.is_empty() {
        out.push_str(
            "docker network ls --format '{{.Name}}' 2>/dev/null\n\
             printf '__END_DOCKER_NETWORKS__\\n'\n",
        );
    }

    // ── CONTAINER LOG READINESS ───────────────────────────────────────────────
    // Sort containers by key (BTreeMap guarantees this).
    for (cname, cexpect) in &config.containers {
        if is_glob_pattern(cname) {
            continue;
        }
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

    // ── CONTAINERS ───────────────────────────────────────────────────────────
    let (exact_containers, pattern_containers): (Vec<_>, Vec<_>) =
        config.containers.iter().partition(|(k, _)| !is_glob_pattern(k));

    for (cname, cexpect) in &exact_containers {
        let cname_sq = sq(cname);
        out.push_str(&format!(
            "if docker inspect {c} --format '{{{{.State.Running}}}}' >/dev/null 2>&1; then\n\
             \x20 _running=$(docker inspect {c} --format '{{{{.State.Running}}}}' 2>/dev/null)\n\
             \x20 _networks=$(docker inspect {c} --format '{{{{range $k,$v := .NetworkSettings.Networks}}}}{{{{$k}}}} {{{{end}}}}' 2>/dev/null)\n\
             \x20 printf 'present:%s:%s\\n' \"$_running\" \"$_networks\"\n\
             else\n\
             \x20 printf 'absent\\n'\n\
             fi\n",
            c = cname_sq
        ));

        // Emit port listing if we have ports to check.
        if !cexpect.ports.is_empty() {
            out.push_str(&format!(
                "docker port {c} 2>/dev/null || true\n\
                 printf '__END_PORTS_{cname}__\\n'\n",
                c = cname_sq,
                cname = cname
            ));
        }
    }

    if !pattern_containers.is_empty() {
        out.push_str(
            "docker ps -a --format '{{.Names}}' 2>/dev/null\n\
             printf '__END_DOCKER_CONTAINERS__\\n'\n",
        );
    }

    out
}

// ── Probe evaluation ──────────────────────────────────────────────────────────

fn evaluate_probe(config: &DockerConfig, probe_stdout: &str) -> Results {
    let mut checks: Vec<CheckResult> = Vec::new();
    let mut lines = probe_stdout.lines().peekable();

    // ── IMAGES ───────────────────────────────────────────────────────────────
    let (exact_images, pattern_images): (Vec<_>, Vec<_>) =
        config.images.iter().partition(|(k, _)| !is_glob_pattern(k));

    for (name, expect) in &exact_images {
        let line = lines.next().unwrap_or("absent").trim();
        let present = line == "present";
        let ok = present == expect.exists;
        let message = if !ok {
            if expect.exists {
                Some(format!("image {name} is absent (expected present)"))
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

    if !pattern_images.is_empty() {
        // Consume all image ls lines until sentinel.
        let mut all_images: Vec<String> = Vec::new();
        loop {
            match lines.next() {
                None => break,
                Some("__END_DOCKER_IMAGES__") => break,
                Some(l) => all_images.push(l.to_owned()),
            }
        }
        for (pattern, expect) in &pattern_images {
            let matches: Vec<&str> = all_images
                .iter()
                .filter(|img| glob_match(pattern, img))
                .map(String::as_str)
                .collect();
            let found = !matches.is_empty();
            let ok = found == expect.exists;
            let message = if !ok {
                if expect.exists {
                    Some(format!("no images matching pattern {pattern:?} found"))
                } else {
                    Some(format!(
                        "images matching pattern {pattern:?} found: {}",
                        matches.join(", ")
                    ))
                }
            } else {
                None
            };
            checks.push(CheckResult {
                label: format!(
                    "docker image {pattern} {}",
                    if expect.exists { "present" } else { "absent" }
                ),
                ok,
                message,
            });
        }
    }

    // ── NETWORKS ─────────────────────────────────────────────────────────────
    let (exact_networks, pattern_networks): (Vec<_>, Vec<_>) =
        config.networks.iter().partition(|(k, _)| !is_glob_pattern(k));

    for (name, expect) in &exact_networks {
        let line = lines.next().unwrap_or("absent").trim();
        let present = line == "present";
        let ok = present == expect.exists;
        let message = if !ok {
            if expect.exists {
                Some(format!("network {name} is absent (expected present)"))
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

    if !pattern_networks.is_empty() {
        let mut all_networks: Vec<String> = Vec::new();
        loop {
            match lines.next() {
                None => break,
                Some("__END_DOCKER_NETWORKS__") => break,
                Some(l) => all_networks.push(l.to_owned()),
            }
        }
        for (pattern, expect) in &pattern_networks {
            let matches: Vec<&str> = all_networks
                .iter()
                .filter(|net| glob_match(pattern, net))
                .map(String::as_str)
                .collect();
            let found = !matches.is_empty();
            let ok = found == expect.exists;
            let message = if !ok {
                if expect.exists {
                    Some(format!("no networks matching pattern {pattern:?} found"))
                } else {
                    Some(format!(
                        "networks matching pattern {pattern:?} found: {}",
                        matches.join(", ")
                    ))
                }
            } else {
                None
            };
            checks.push(CheckResult {
                label: format!(
                    "docker network {pattern} {}",
                    if expect.exists { "present" } else { "absent" }
                ),
                ok,
                message,
            });
        }
    }

    // ── CONTAINER LOG READINESS ───────────────────────────────────────────────
    // Per-container log results: map from container name to (ready_bool, log_lines).
    let mut log_results: BTreeMap<String, (bool, Vec<String>)> = BTreeMap::new();

    for (cname, cexpect) in &config.containers {
        if is_glob_pattern(cname) {
            continue;
        }
        let Some(logs) = cexpect.logs.as_ref() else {
            continue;
        };
        if logs.contains.is_empty() {
            continue;
        }

        let ready_sentinel = format!("__LOG_READY_RESULT__{}:", cname);
        let end_sentinel = format!("__END_LOG_{}__", cname);

        // Consume until ready sentinel.
        let mut ready = false;
        let mut log_lines: Vec<String> = Vec::new();

        loop {
            match lines.next() {
                None => break,
                Some(l) if l.starts_with(&ready_sentinel) => {
                    let val = l.trim_start_matches(&ready_sentinel);
                    ready = val == "1";
                    // Now collect log lines until end sentinel.
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
    let (exact_containers, pattern_containers): (Vec<_>, Vec<_>) =
        config.containers.iter().partition(|(k, _)| !is_glob_pattern(k));

    for (cname, cexpect) in &exact_containers {
        let state_line = lines.next().unwrap_or("absent").trim().to_owned();
        let present = state_line != "absent";

        // Presence check.
        let exists_ok = present == cexpect.exists;
        let exists_message = if !exists_ok {
            if cexpect.exists {
                Some(format!("container {cname} is absent (expected present)"))
            } else {
                Some(format!("container {cname} is present (expected absent)"))
            }
        } else {
            None
        };
        checks.push(CheckResult {
            label: format!(
                "docker container {cname} {}",
                if cexpect.exists { "present" } else { "absent" }
            ),
            ok: exists_ok,
            message: exists_message,
        });

        // Parse ports listing if needed.
        let port_lines: Vec<String> = if !cexpect.ports.is_empty() {
            let end_sentinel = format!("__END_PORTS_{}__", cname);
            let mut pl: Vec<String> = Vec::new();
            loop {
                match lines.next() {
                    None => break,
                    Some(l) if l == end_sentinel => break,
                    Some(l) => pl.push(l.to_owned()),
                }
            }
            pl
        } else {
            Vec::new()
        };

        // Further checks only matter when the container is present.
        if !present {
            continue;
        }

        // Parse state line: "present:<running>:<networks>"
        let parts: Vec<&str> = state_line.splitn(3, ':').collect();
        let running_str = parts.get(1).unwrap_or(&"false");
        let networks_str = parts.get(2).unwrap_or(&"");
        let is_running = *running_str == "true";
        let attached_networks: Vec<&str> = networks_str
            .split_whitespace()
            .filter(|s| !s.is_empty())
            .collect();

        // Running check.
        if let Some(expected_running) = cexpect.effective_running() {
            let running_ok = is_running == expected_running;
            let running_message = if !running_ok {
                if expected_running {
                    Some(format!("container {cname} is not running (expected running)"))
                } else {
                    Some(format!(
                        "container {cname} is running (expected not running)"
                    ))
                }
            } else {
                None
            };
            checks.push(CheckResult {
                label: format!(
                    "docker container {cname} {}",
                    if expected_running { "running" } else { "stopped" }
                ),
                ok: running_ok,
                message: running_message,
            });
        }

        // Network membership checks.
        for net_spec in &cexpect.networks {
            let (net_name, must_attach) = if let Some(stripped) = net_spec.strip_prefix('!') {
                (stripped, false)
            } else {
                (net_spec.as_str(), true)
            };

            let attached = attached_networks.contains(&net_name);
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
                    if must_attach { "attached" } else { "not attached" }
                ),
                ok,
                message,
            });
        }

        // Port checks: parse "docker port" output.
        // Each line is like "80/tcp -> 0.0.0.0:8080" or "80/tcp -> :::8080".
        let published_ports: Vec<String> = port_lines
            .iter()
            .filter_map(|l| l.split_whitespace().next().map(str::to_owned))
            .collect();

        for port_spec in &cexpect.ports {
            let (port, must_publish) = if let Some(stripped) = port_spec.strip_prefix('!') {
                (stripped, false)
            } else {
                (port_spec.as_str(), true)
            };

            let published = published_ports.iter().any(|p| p == port);
            let ok = published == must_publish;
            let message = if !ok {
                if must_publish {
                    Some(format!(
                        "container {cname} does not publish port {port}"
                    ))
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
                    if must_publish { "published" } else { "not published" }
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

            // Evaluate not_contains immediately (no polling needed).
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

            // For contains: the ready flag tells us whether the wait loop found all substrings.
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

    if !pattern_containers.is_empty() {
        let mut all_containers: Vec<String> = Vec::new();
        loop {
            match lines.next() {
                None => break,
                Some("__END_DOCKER_CONTAINERS__") => break,
                Some(l) => all_containers.push(l.to_owned()),
            }
        }
        for (pattern, cexpect) in &pattern_containers {
            let matches: Vec<&str> = all_containers
                .iter()
                .filter(|c| glob_match(pattern, c))
                .map(String::as_str)
                .collect();
            let found = !matches.is_empty();
            let ok = found == cexpect.exists;
            let message = if !ok {
                if cexpect.exists {
                    Some(format!(
                        "no containers matching pattern {pattern:?} found"
                    ))
                } else {
                    Some(format!(
                        "containers matching pattern {pattern:?} found: {}",
                        matches.join(", ")
                    ))
                }
            } else {
                None
            };
            checks.push(CheckResult {
                label: format!(
                    "docker container {pattern} {}",
                    if cexpect.exists { "present" } else { "absent" }
                ),
                ok,
                message,
            });
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
            // `*` matches zero or more characters.
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
            // Find the closing `]`.
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
/// # Safety
///
/// `config_json` must be a valid NUL-terminated C string.
/// `out_ptr` must be a valid non-null pointer to a `*mut c_char`.
#[no_mangle]
pub unsafe extern "C" fn plugin_assert_build_probe(
    config_json: *const c_char,
    out_ptr: *mut *mut c_char,
) -> i32 {
    // SAFETY: caller guarantees config_json is a valid NUL-terminated C string.
    let json_str = match unsafe { cstr_to_str(config_json) } {
        Some(s) => s,
        None => return 1,
    };

    let config: DockerConfig = match serde_json::from_str(json_str) {
        Ok(c) => c,
        Err(_) => return 2,
    };

    let script = build_probe_script(&config);

    // SAFETY: out_ptr is a valid non-null pointer per ABI contract.
    unsafe { write_cstring_out(script, out_ptr) };
    0
}

/// Evaluate captured probe stdout against a JSON-encoded `DockerConfig`.
///
/// # Safety
///
/// `config_json` and `probe_stdout` must be valid NUL-terminated C strings.
/// `out_ptr` must be a valid non-null pointer to a `*mut c_char`.
#[no_mangle]
pub unsafe extern "C" fn plugin_assert_evaluate(
    config_json: *const c_char,
    probe_stdout: *const c_char,
    out_ptr: *mut *mut c_char,
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
        Err(_) => return 2,
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

    #[test]
    fn probe_script_empty_config() {
        let config = DockerConfig {
            images: BTreeMap::new(),
            networks: BTreeMap::new(),
            containers: BTreeMap::new(),
        };
        let script = build_probe_script(&config);
        assert!(script.starts_with("set -e\n"));
    }

    #[test]
    fn probe_script_exact_image() {
        let mut images = BTreeMap::new();
        images.insert("nginx:latest".to_owned(), ImageExpect { exists: true });
        let config = DockerConfig {
            images,
            networks: BTreeMap::new(),
            containers: BTreeMap::new(),
        };
        let script = build_probe_script(&config);
        assert!(
            script.contains("docker image inspect 'nginx:latest'"),
            "script: {script}"
        );
        assert!(script.contains("present"));
        assert!(!script.contains("__END_DOCKER_IMAGES__"));
    }

    #[test]
    fn probe_script_pattern_image_emits_sentinel() {
        let mut images = BTreeMap::new();
        images.insert("nginx:*".to_owned(), ImageExpect { exists: true });
        let config = DockerConfig {
            images,
            networks: BTreeMap::new(),
            containers: BTreeMap::new(),
        };
        let script = build_probe_script(&config);
        assert!(script.contains("__END_DOCKER_IMAGES__"), "script: {script}");
        assert!(script.contains("docker image ls"));
    }

    #[test]
    fn evaluate_exact_image_present() {
        let mut images = BTreeMap::new();
        images.insert("nginx:latest".to_owned(), ImageExpect { exists: true });
        let config = DockerConfig {
            images,
            networks: BTreeMap::new(),
            containers: BTreeMap::new(),
        };
        let results = evaluate_probe(&config, "present\n");
        assert_eq!(results.checks.len(), 1);
        assert!(results.checks[0].ok);
        assert_eq!(results.checks[0].label, "docker image nginx:latest present");
    }

    #[test]
    fn evaluate_exact_image_absent_when_expected_absent() {
        let mut images = BTreeMap::new();
        images.insert("old:latest".to_owned(), ImageExpect { exists: false });
        let config = DockerConfig {
            images,
            networks: BTreeMap::new(),
            containers: BTreeMap::new(),
        };
        let results = evaluate_probe(&config, "absent\n");
        assert_eq!(results.checks.len(), 1);
        assert!(results.checks[0].ok);
    }

    #[test]
    fn evaluate_exact_container_present_running() {
        let mut containers = BTreeMap::new();
        containers.insert(
            "web".to_owned(),
            ContainerExpect {
                exists: true,
                running: None,
                logs: None,
                networks: vec![],
                ports: vec![],
            },
        );
        let config = DockerConfig {
            images: BTreeMap::new(),
            networks: BTreeMap::new(),
            containers,
        };
        // "present:true:" — running=true, no networks
        let results = evaluate_probe(&config, "present:true:\n");
        assert_eq!(results.checks.len(), 2); // exists + running
        assert!(results.checks[0].ok, "exists check: {:?}", results.checks[0]);
        assert!(results.checks[1].ok, "running check: {:?}", results.checks[1]);
    }

    #[test]
    fn evaluate_container_network_attached() {
        let mut containers = BTreeMap::new();
        containers.insert(
            "web".to_owned(),
            ContainerExpect {
                exists: true,
                running: Some(true),
                logs: None,
                networks: vec!["mynet".to_owned()],
                ports: vec![],
            },
        );
        let config = DockerConfig {
            images: BTreeMap::new(),
            networks: BTreeMap::new(),
            containers,
        };
        let results = evaluate_probe(&config, "present:true:mynet \n");
        let net_check = results
            .checks
            .iter()
            .find(|c| c.label.contains("network"))
            .unwrap();
        assert!(net_check.ok, "network check: {:?}", net_check);
    }

    #[test]
    fn roundtrip_json_config() {
        let json = r#"{"images":{"nginx:latest":{"exists":true}},"networks":{},"containers":{"web":{"exists":true,"running":null,"logs":null,"networks":[],"ports":[]}}}"#;
        let config: DockerConfig = serde_json::from_str(json).unwrap();
        assert!(config.images.contains_key("nginx:latest"));
        assert!(config.containers.contains_key("web"));
    }
}
