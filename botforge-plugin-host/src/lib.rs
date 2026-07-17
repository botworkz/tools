//! Plugin host for the botforge `.so` plugin system.
//!
//! # Trust boundary (load-bearing invariant)
//!
//! **The plugin knows nothing about the host environment.**  A plugin has no
//! ambient access to environment variables, process secrets, or any host state
//! unless the host explicitly hands something across the ABI.  The host is the
//! sole broker of capabilities and credentials: anything a plugin needs is passed
//! to it through the ABI by the host.  The `core/ping` handshake capability (this
//! PR's only capability) is deliberately auth-free to uphold this invariant.
//!
//! # Config-driven discovery (no autoload)
//!
//! Nothing loads unless the plugin is explicitly listed in the botforge config
//! under the `plugins:` key.  A `.so` present on disk but absent from config is
//! never loaded.
//!
//! ## Path resolution (two roots)
//!
//! 1. **Repo-relative** — a bare or `./`-prefixed path is resolved against the
//!    botforge context root (consistent with how other botforge paths work).
//! 2. **Absolute / system dir** — absolute paths are used as-is.  The canonical
//!    home for container-shipped plugins is `/usr/share/botforge/plugins/`.
//!
//! # ABI version handshake
//!
//! Every plugin must export `extern "C" fn abi_version() -> u32`.  The host calls
//! it and does a **hard exact match** against [`HOST_ABI_VERSION`].  A mismatch
//! produces a [`LoadError::AbiVersionMismatch`] naming both versions.
//!
//! Range-based or negotiated ABI versions are explicitly deferred; exact-match
//! only for v0.
//!
//! # `provides:` semantics
//!
//! The plugin self-declares which capability slots it provides via the
//! `plugin_provides_count` / `plugin_provides_slot` / `plugin_provides_name` ABI
//! exports.  The config `provides:` list (when present) acts as an **allow-list**
//! that constrains what the host actually wires; when absent the host wires all
//! capabilities the plugin declares.
//!
//! A future `strict_mode` config knob (NOT in this PR) may make `provides:`
//! mandatory for untrusted sources.
//!
//! # `(slot, name)` collision and reconciliation
//!
//! The registry is keyed by `(slot, name)` where `slot` is a namespaced
//! `<domain>/<capability>` string (e.g. `core/ping`) and `name` is the capability
//! name the plugin registers under.
//!
//! Collision rule: a `(slot, name)` that is already wired (by a built-in or a
//! previously-loaded plugin) **blocks** the new plugin from loading.  The full
//! collision check runs as a **code-free reconciliation pass** — no plugin
//! capability logic ever runs — before any capability is wired.  A collision
//! produces a [`LoadError::CapabilityCollision`] naming the slot, name, and both
//! providers.  Only when the plugin's *entire* provided set reconciles cleanly is
//! anything wired.
//!
//! Built-ins are modeled as pre-registered entries, so "a plugin cannot redefine a
//! built-in" falls out of the same `(slot, name)` check for free.
//!
//! The SAME name in a DIFFERENT slot is NOT a collision.
//!
//! # `core/ping` handshake seam
//!
//! `core/ping` is a lightweight host-level diagnostic/handshake capability, not
//! a general-purpose plugin feature. Its sole purpose is to prove the full path
//! end to end:
//!
//! > load → `abi_version()` hard-match → read `provides` → reconcile/wire →
//! > call across boundary → get the correct sentinel back
//!
//! The "must return 42" contract exists only for this self-test seam. It takes
//! no host-environment access (trust boundary upheld), and persists as the
//! loader's permanent handshake check.
//!
//! ## Plugin ABI contract for `core/ping`
//!
//! ```c
//! // Called by the host to execute a ping.  Must return PING_SENTINEL (42u32).
//! uint32_t plugin_core_ping(void);
//! ```
//!
//! # Safety policy
//!
//! This crate is the **sole sanctioned location** in the botforge workspace where
//! `unsafe` code is permitted.  All other workspace members declare
//! `#![forbid(unsafe_code)]`; this crate intentionally omits that attribute.
//!
//! Every `unsafe` block is accompanied by a `// SAFETY:` comment explaining the
//! invariant that makes it sound.

use std::collections::HashMap;
use std::ffi::CStr;
use std::path::{Path, PathBuf};

use libloading::{Library, Symbol};
use thiserror::Error;

// ── ABI version ──────────────────────────────────────────────────────────────

/// Monotone integer ABI version the host expects every plugin to report.
///
/// A plugin whose `abi_version()` export returns any value other than this
/// constant is rejected with [`LoadError::AbiVersionMismatch`].  Increment
/// this constant (and rebuild all plugins) whenever the plugin ABI changes in
/// a backwards-incompatible way.
pub const HOST_ABI_VERSION: u32 = 1;

/// Sentinel value returned by a correct `core/ping` implementation.
///
/// The host asserts that `plugin_core_ping()` returns exactly this value.
pub const PING_SENTINEL: u32 = 42;

// ── Errors ───────────────────────────────────────────────────────────────────

/// Structured errors for plugin loading and capability wiring.
#[derive(Debug, Error)]
pub enum LoadError {
    /// The `.so` file was not found at the given path.
    #[error("plugin file not found: {path}")]
    FileNotFound { path: PathBuf },

    /// `libloading`/`dlopen` failed to open the library.
    #[error("failed to open plugin {path}: {source}")]
    DlopenFailed {
        path: PathBuf,
        #[source]
        source: libloading::Error,
    },

    /// A required symbol could not be resolved in the library.
    #[error("plugin {plugin} is missing required symbol '{symbol}'")]
    MissingSymbol {
        plugin: String,
        symbol: &'static str,
    },

    /// The plugin's `abi_version()` return value does not match the host.
    #[error(
        "ABI version mismatch for plugin '{plugin}': \
         plugin reports {plugin_version}, host requires {host_version}"
    )]
    AbiVersionMismatch {
        plugin: String,
        plugin_version: u32,
        host_version: u32,
    },

    /// The plugin's capability-enumeration ABI returned an invalid C string.
    #[error("plugin '{plugin}' returned invalid UTF-8 in capability {index}: {source}")]
    CapabilityEnumerationFailed {
        plugin: String,
        index: u32,
        #[source]
        source: std::str::Utf8Error,
    },

    /// A `(slot, name)` pair exported by the plugin is already registered.
    ///
    /// Nothing from the offending plugin is wired.
    #[error(
        "capability collision: slot '{slot}' name '{name}' \
         is already registered by '{existing_provider}', \
         cannot also register it for '{new_provider}'"
    )]
    CapabilityCollision {
        slot: String,
        name: String,
        existing_provider: String,
        new_provider: String,
    },

    /// A provided capability slot is unknown to the host.
    #[error(
        "plugin '{plugin}' declares unknown capability slot '{slot}'; \
         config provides: filter may be wrong"
    )]
    UnknownCapabilitySlot { plugin: String, slot: String },
}

// ── Capability handles ────────────────────────────────────────────────────────

/// A callable handle to a wired `core/ping` capability.
///
/// # Safety
///
/// The function pointer is valid for as long as the originating
/// [`LoadedPlugin`] (and hence its [`Library`]) stays alive.  Callers must not
/// use a `PingHandle` after the plugin has been dropped.
pub struct PingHandle {
    /// Raw function pointer resolved from the plugin.
    ///
    /// SAFETY argument: see [`LoadedPlugin::load`] — the pointer is obtained
    /// via `libloading::Symbol::into_raw` after a successful `dlsym`, and the
    /// symbol remains valid for the lifetime of the owning `Library`.
    func: unsafe extern "C" fn() -> u32,
}

impl PingHandle {
    /// Call the plugin's `core/ping` entrypoint and return the result.
    pub fn call(&self) -> u32 {
        // SAFETY: The function pointer was obtained from a successfully
        // dlopen-ed library and the symbol was verified to exist.  The
        // calling convention is `extern "C"` on both sides.  The library
        // must stay live; see struct-level safety note.
        unsafe { (self.func)() }
    }
}

// ── Loaded plugin ─────────────────────────────────────────────────────────────

/// A plugin that has been successfully opened and version-checked.
///
/// `LoadedPlugin` holds the open [`Library`] handle; dropping it closes
/// the `.so`.  All capability handles derived from this library are invalid
/// after the drop, so keep `LoadedPlugin` alive as long as capability handles
/// are in use.
pub struct LoadedPlugin {
    /// Human-readable name from the config entry.
    pub name: String,
    /// ABI version read from the plugin (already matched against
    /// [`HOST_ABI_VERSION`]).
    pub abi_version: u32,
    /// Capability `(slot, name)` pairs self-declared by the plugin
    /// (possibly filtered by the config `provides:` allow-list).
    pub provides: Vec<(String, String)>,
    /// `core/ping` handle, present if the plugin wired that capability.
    pub ping: Option<PingHandle>,
    /// The open library handle.  Must stay alive as long as any capability
    /// handles derived from it are in use.
    _lib: Library,
}

impl std::fmt::Debug for LoadedPlugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoadedPlugin")
            .field("name", &self.name)
            .field("abi_version", &self.abi_version)
            .field("provides", &self.provides)
            .field("ping", &self.ping.is_some())
            .finish_non_exhaustive()
    }
}

impl LoadedPlugin {
    /// Open a plugin `.so`, verify the ABI version, enumerate capabilities,
    /// and apply the optional config `provides:` filter.
    ///
    /// # Errors
    ///
    /// Returns a [`LoadError`] if:
    /// - the file does not exist,
    /// - `dlopen` fails,
    /// - `abi_version` is missing or mismatches,
    /// - the capability-enumeration symbols are missing or return bad data,
    /// - a capability the plugin declares is unknown to the host.
    pub fn load(
        plugin_name: &str,
        path: &Path,
        config_provides: Option<&[String]>,
    ) -> Result<Self, LoadError> {
        if !path.exists() {
            return Err(LoadError::FileNotFound {
                path: path.to_owned(),
            });
        }

        // SAFETY: `dlopen` is inherently unsafe.  We accept the risk here
        // as the sole designated location for FFI loading in this workspace.
        // The path has already been verified to exist above.
        let lib = unsafe {
            Library::new(path).map_err(|e| LoadError::DlopenFailed {
                path: path.to_owned(),
                source: e,
            })?
        };

        // ── ABI version handshake ─────────────────────────────────────────
        let plugin_ver: u32 = {
            // SAFETY: We are looking up the symbol `abi_version` which is
            // expected to be an `extern "C" fn() -> u32`.  If the plugin
            // exports a symbol with this name but a different signature, the
            // call below is UB.  We document this as part of the plugin ABI
            // contract: plugins MUST export exactly this signature.
            let sym: Symbol<unsafe extern "C" fn() -> u32> = unsafe {
                lib.get(b"abi_version\0")
                    .map_err(|_| LoadError::MissingSymbol {
                        plugin: plugin_name.to_owned(),
                        symbol: "abi_version",
                    })?
            };
            // SAFETY: same as above; calling the function pointer.
            unsafe { sym() }
        };

        if plugin_ver != HOST_ABI_VERSION {
            return Err(LoadError::AbiVersionMismatch {
                plugin: plugin_name.to_owned(),
                plugin_version: plugin_ver,
                host_version: HOST_ABI_VERSION,
            });
        }

        // ── Capability enumeration ────────────────────────────────────────
        //
        // The plugin exports three symbols for capability self-declaration:
        //
        //   extern "C" fn plugin_provides_count() -> u32
        //     Returns the number of (slot, name) pairs this plugin provides.
        //
        //   extern "C" fn plugin_provides_slot(index: u32) -> *const c_char
        //     Returns a NUL-terminated UTF-8 slot string for the given index.
        //     The memory is static and owned by the plugin; the host must NOT
        //     free it.
        //
        //   extern "C" fn plugin_provides_name(index: u32) -> *const c_char
        //     Returns a NUL-terminated UTF-8 capability name string for the
        //     given index.  Same ownership rules.
        //
        // Memory ownership: all returned pointers are `'static` string
        // literals inside the plugin `.so` binary.  They are valid for the
        // lifetime of the open `Library` and must never be freed by the host.

        // SAFETY: Looking up the symbol with the exact documented signature.
        let count_sym: Symbol<unsafe extern "C" fn() -> u32> = unsafe {
            lib.get(b"plugin_provides_count\0")
                .map_err(|_| LoadError::MissingSymbol {
                    plugin: plugin_name.to_owned(),
                    symbol: "plugin_provides_count",
                })?
        };
        // SAFETY: Calling the function pointer obtained via dlsym.
        let count = unsafe { count_sym() };

        // SAFETY: Looking up the symbol with the exact documented signature.
        let slot_sym: Symbol<unsafe extern "C" fn(u32) -> *const std::os::raw::c_char> = unsafe {
            lib.get(b"plugin_provides_slot\0")
                .map_err(|_| LoadError::MissingSymbol {
                    plugin: plugin_name.to_owned(),
                    symbol: "plugin_provides_slot",
                })?
        };

        // SAFETY: Looking up the symbol with the exact documented signature.
        let name_sym: Symbol<unsafe extern "C" fn(u32) -> *const std::os::raw::c_char> = unsafe {
            lib.get(b"plugin_provides_name\0")
                .map_err(|_| LoadError::MissingSymbol {
                    plugin: plugin_name.to_owned(),
                    symbol: "plugin_provides_name",
                })?
        };

        let mut all_provides: Vec<(String, String)> = Vec::with_capacity(count as usize);
        for i in 0..count {
            // SAFETY: Calling a function pointer obtained via dlsym with an
            // in-range index.  The returned pointer is a `'static` C string
            // inside the plugin binary — valid as long as the library is open.
            let slot_ptr = unsafe { slot_sym(i) };
            let name_ptr = unsafe { name_sym(i) };

            // SAFETY: The plugin contract requires both pointers to be valid
            // NUL-terminated UTF-8 strings, non-null and `'static`.  If a
            // malformed plugin violates this, behaviour is undefined; we accept
            // this as inherent to the FFI trust boundary.
            let slot = unsafe { CStr::from_ptr(slot_ptr) }
                .to_str()
                .map_err(|e| LoadError::CapabilityEnumerationFailed {
                    plugin: plugin_name.to_owned(),
                    index: i,
                    source: e,
                })?
                .to_owned();
            let cap_name = unsafe { CStr::from_ptr(name_ptr) }
                .to_str()
                .map_err(|e| LoadError::CapabilityEnumerationFailed {
                    plugin: plugin_name.to_owned(),
                    index: i,
                    source: e,
                })?
                .to_owned();

            all_provides.push((slot, cap_name));
        }

        // ── Apply config provides: filter ─────────────────────────────────
        let provides: Vec<(String, String)> = if let Some(allow_list) = config_provides {
            all_provides
                .into_iter()
                .filter(|(slot, _)| allow_list.iter().any(|a| a == slot))
                .collect()
        } else {
            // absent ⇒ implicit-all (v0 behaviour)
            all_provides
        };

        // ── Resolve capability handles ────────────────────────────────────
        let mut ping: Option<PingHandle> = None;

        for (slot, _cap_name) in &provides {
            match slot.as_str() {
                "core/ping" => {
                    // SAFETY: The symbol `plugin_core_ping` must be an
                    // `extern "C" fn() -> u32` per the documented `core/ping`
                    // ABI contract.  We use `Symbol::into_raw` to detach the
                    // lifetime from `lib` and store the raw pointer; the
                    // pointer remains valid for the lifetime of `lib` (stored
                    // in `_lib` on the returned `LoadedPlugin`).
                    let sym: Symbol<unsafe extern "C" fn() -> u32> = unsafe {
                        lib.get(b"plugin_core_ping\0")
                            .map_err(|_| LoadError::MissingSymbol {
                                plugin: plugin_name.to_owned(),
                                symbol: "plugin_core_ping",
                            })?
                    };
                    // SAFETY: `into_raw` detaches the symbol from the `Symbol`
                    // wrapper's lifetime borrow on `lib`.  We guarantee that
                    // `_lib` outlives any use of `func` because both are owned
                    // by the same `LoadedPlugin`.
                    let func = unsafe { sym.into_raw() };
                    ping = Some(PingHandle { func: *func });
                }
                other => {
                    return Err(LoadError::UnknownCapabilitySlot {
                        plugin: plugin_name.to_owned(),
                        slot: other.to_owned(),
                    });
                }
            }
        }

        Ok(LoadedPlugin {
            name: plugin_name.to_owned(),
            abi_version: plugin_ver,
            provides,
            ping,
            _lib: lib,
        })
    }
}

// ── Plugin registry ───────────────────────────────────────────────────────────

/// The capability registry: a map from `(slot, name)` to the provider name.
///
/// Built-ins are pre-seeded before any plugins are loaded; subsequent loads
/// go through the same `(slot, name)` collision check.
#[derive(Default)]
pub struct PluginRegistry {
    /// `(slot, name)` → provider name.
    entries: HashMap<(String, String), String>,
    /// Successfully loaded plugins (in load order).
    pub plugins: Vec<LoadedPlugin>,
}

impl PluginRegistry {
    /// Create an empty registry with no pre-seeded built-ins.
    pub fn new() -> Self {
        Self::default()
    }

    /// Pre-seed the registry with a built-in `(slot, name)` entry.
    ///
    /// Built-ins cannot be overwritten by plugins; any plugin that tries
    /// produces a [`LoadError::CapabilityCollision`].
    pub fn seed_builtin(&mut self, slot: impl Into<String>, name: impl Into<String>) {
        let s = slot.into();
        let n = name.into();
        self.entries.insert((s, n), "<built-in>".to_owned());
    }

    /// Load a plugin from `path`, run the code-free reconciliation pass,
    /// and wire its capabilities into the registry.
    ///
    /// # Reconciliation pass
    ///
    /// 1. Open the `.so` and read its declared `(slot, name)` set.
    /// 2. Compute the intended registrations (all or config-filtered).
    /// 3. Check **every** `(slot, name)` against the merged registry in a
    ///    **single pass, no plugin logic invoked**.
    /// 4. On any collision: return [`LoadError::CapabilityCollision`]; the
    ///    plugin's `LoadedPlugin` is dropped (`.so` is closed), nothing is
    ///    wired.
    /// 5. Only on full clean pass: register all `(slot, name)` pairs and push
    ///    the plugin into [`Self::plugins`].
    pub fn load_plugin(
        &mut self,
        plugin_name: &str,
        path: &Path,
        config_provides: Option<&[String]>,
    ) -> Result<(), LoadError> {
        let loaded = LoadedPlugin::load(plugin_name, path, config_provides)?;

        // ── Code-free reconciliation pass ─────────────────────────────────
        // Check ALL intended registrations before wiring any of them.
        for (slot, name) in &loaded.provides {
            let key = (slot.clone(), name.clone());
            if let Some(existing) = self.entries.get(&key) {
                return Err(LoadError::CapabilityCollision {
                    slot: slot.clone(),
                    name: name.clone(),
                    existing_provider: existing.clone(),
                    new_provider: plugin_name.to_owned(),
                });
            }
        }

        // Full clean pass — wire everything.
        for (slot, name) in &loaded.provides {
            self.entries
                .insert((slot.clone(), name.clone()), plugin_name.to_owned());
        }
        self.plugins.push(loaded);
        Ok(())
    }

    /// Look up the `core/ping` handle for a named capability registration.
    ///
    /// Returns `None` if the plugin is not loaded or did not wire `core/ping`
    /// under `name`.
    pub fn get_ping(&self, name: &str) -> Option<&PingHandle> {
        // Find the plugin that registered (core/ping, name).
        let provider = self
            .entries
            .get(&("core/ping".to_owned(), name.to_owned()))?;
        self.plugins
            .iter()
            .find(|p| &p.name == provider)
            .and_then(|p| p.ping.as_ref())
    }

    /// Returns `true` if `(slot, name)` is already registered (by a built-in or
    /// a previously-loaded plugin).
    pub fn is_registered(&self, slot: &str, name: &str) -> bool {
        self.entries
            .contains_key(&(slot.to_owned(), name.to_owned()))
    }

    /// Returns the provider name for `(slot, name)`, if registered.
    pub fn provider_of(&self, slot: &str, name: &str) -> Option<&str> {
        self.entries
            .get(&(slot.to_owned(), name.to_owned()))
            .map(String::as_str)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Unit tests that do NOT require a compiled .so ────────────────────

    #[test]
    fn builtin_blocks_plugin_same_slot_name() {
        let mut reg = PluginRegistry::new();
        reg.seed_builtin("core/ping", "hello");
        assert!(reg.is_registered("core/ping", "hello"));
        assert_eq!(reg.provider_of("core/ping", "hello"), Some("<built-in>"));
    }

    #[test]
    fn different_slot_same_name_is_not_collision() {
        let mut reg = PluginRegistry::new();
        reg.seed_builtin("core/ping", "hello");
        // A DIFFERENT slot with the SAME name is not a collision.
        assert!(!reg.is_registered("build/compressor", "hello"));
    }

    #[test]
    fn registry_empty_by_default() {
        let reg = PluginRegistry::new();
        assert!(!reg.is_registered("core/ping", "anything"));
        assert!(reg.plugins.is_empty());
    }

    #[test]
    fn file_not_found_error() {
        let err = LoadedPlugin::load("missing", Path::new("/nonexistent/libmissing.so"), None)
            .unwrap_err();
        assert!(
            matches!(err, LoadError::FileNotFound { .. }),
            "expected FileNotFound, got: {err}"
        );
    }

    #[test]
    fn load_error_messages_are_informative() {
        let err = LoadError::AbiVersionMismatch {
            plugin: "myplugin".to_owned(),
            plugin_version: 99,
            host_version: 1,
        };
        let msg = err.to_string();
        assert!(msg.contains("myplugin"), "should name the plugin: {msg}");
        assert!(msg.contains("99"), "should show plugin version: {msg}");
        assert!(msg.contains('1'), "should show host version: {msg}");

        let err2 = LoadError::CapabilityCollision {
            slot: "core/ping".to_owned(),
            name: "hello".to_owned(),
            existing_provider: "plugin-a".to_owned(),
            new_provider: "plugin-b".to_owned(),
        };
        let msg2 = err2.to_string();
        assert!(msg2.contains("core/ping"), "should name slot: {msg2}");
        assert!(msg2.contains("hello"), "should name cap name: {msg2}");
        assert!(msg2.contains("plugin-a"), "should name existing: {msg2}");
        assert!(msg2.contains("plugin-b"), "should name new: {msg2}");
    }
}
