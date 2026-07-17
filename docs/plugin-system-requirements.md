# `.so` Plugin System — Requirements & Challenges Report

**Scope:** Build compressor verbs and test assert verbs only.  
**Out of scope:** Publish and deploy commands.

---

## Executive Summary — Top Blockers

1. **Both extension points are closed-dispatch today.**  
   `CompressionType` is a two-variant enum with a `match`-based factory; assert kinds are
   hard-coded `if`-blocks over fixed `AssertBlock` fields.  A refactor introducing a trait +
   string-keyed registry is **required at both seams before any plugin can contribute a verb**.

2. **The `unsafe_code = "forbid"` workspace gate is a blocking policy change.**  
   Loading a `.so` requires `unsafe` for `dlopen`/symbol resolution/calls.  This cannot be
   done in a side branch — the workspace `Cargo.toml` must be amended in a dedicated reviewed
   commit first.

3. **ABI stability is Rust's hardest unsolved problem.**  
   Neither `anyhow::Result`, `Vec<u8>`, `BTreeMap<String,String>`, `SshOptions`, nor
   `AssertBlock` are FFI-safe.  Every type crossing the plugin boundary must be rebuilt as
   `#[repr(C)]`-stable or replaced by a stable-ABI layer.  This is non-trivial design work.

4. **The assert seam also requires redesigning SSH-channel access.**  
   Currently `SshOptions` (host/port/user/key path) is the context handed to every assert
   runner.  A plugin assert cannot hold or share this Rust type across the `.so` boundary
   without a forwarding callback or opaque-handle scheme.

---

## 1 — The Verb Seam Today

### 1a. Build compressor seam

**Config parsing layer** — `botforge/src/compress/config.rs`, lines 20–26:

```rust
pub(crate) enum CompressionType {
    #[default] Zstd,
    Zlib,
}
```

This enum is the YAML-facing string verb: `compressor: zstd` / `compressor: zlib`.  It lives
inside `CompressConfig` (lines 61–90), which is the `compress:` map in a build YAML document.
Key fields for verb selection:

| Field | Type | Role |
|---|---|---|
| `CompressConfig.compressor` | `CompressionType` | Selects the algorithm |
| `CompressConfig.compressor_opts` | `String` | Raw option string passed to the codec |
| `CompressConfig.compressor_args` | `BTreeMap<String,String>` | qcow2-structural args |

**YAML → serde → dispatch chain:**

1. `load_build_config()` (`config/mod.rs:401`) deserialises the YAML into `RawBuildDocument`,
   which carries `compress: Option<CompressConfig>`.  Serde maps `compressor: "zstd"` directly
   into `CompressionType::Zstd` via `#[serde(rename_all = "lowercase")]`.
2. `commit_output()` (`commands/build.rs:521`) receives `compress: Option<&CompressConfig>` and
   calls:
   ```
   compress_qcow2_image(partial, &tmp, c.compressor, &c.compressor_args, &c.compressor_opts)
   ```
   (line 539).
3. `compress_qcow2_image()` (`compress/qcow2.rs:110–116`) calls
   `build_compressor(compression_type, compressor_opts)` at line 155.
4. **The dispatch point** is `build_compressor()` in `compress/codec.rs:19–27`:

```rust
pub(crate) fn build_compressor(
    compression_type: CompressionType,
    raw_opts: &str,
) -> Result<Box<dyn Compressor + Sync + Send>> {
    match compression_type {
        CompressionType::Zstd => Ok(Box::new(ZstdCompressor::from_opts(raw_opts)?)),
        CompressionType::Zlib => Ok(Box::new(ZlibCompressor::from_opts(raw_opts)?)),
    }
}
```

**The two built-in compressors:**

- `ZstdCompressor` (`compress/codec.rs:76–150`) — struct `{ level: i32, workers: u32 }`;
  parses `-N` level and `-TN` worker tokens from `compressor_opts`; wraps the `zstd` crate's
  bulk compressor.
- `ZlibCompressor` (`compress/codec.rs:152–187`) — zero-field unit struct; accepts no opts;
  wraps `flate2::write::DeflateEncoder`.

Both implement the `Compressor` trait (`compress/codec.rs:9–17`):

```rust
pub(crate) trait Compressor: Sync {
    fn id(&self) -> &str;
    fn compress_cluster(&self, cluster: &[u8]) -> Result<Vec<u8>>;
    fn workers(&self) -> u32;
}
```

There is also `decompress_cluster()` (`compress/codec.rs:29–60`), which has its own
`match compression_type { ... }` dispatch used when reading the source image.

### 1b. Test assert seam

**Config layer** — `botforge/src/assert.rs:151–168`:

```rust
pub(crate) struct AssertBlock {
    pub(crate) files:    BTreeMap<String, AssertFile>,
    pub(crate) users:    BTreeMap<String, AssertUser>,
    pub(crate) groups:   BTreeMap<String, AssertGroup>,
    pub(crate) packages: BTreeMap<String, AssertPackage>,
    pub(crate) services: BTreeMap<String, AssertService>,
}
```

The YAML key `assert:` in a `type: botforge/test` document is deserialised directly into
`Option<AssertBlock>` via `RawTestDocument.assert` (`config/mod.rs:151–152`).  The sub-keys
(`files:`, `users:`, etc.) are the "verbs" — each is a named struct field with its own
expectation type.

**YAML → serde → dispatch chain:**

1. `load_test_config()` (`config/mod.rs:326`) deserialises into `RawTestDocument`, validates
   via `validate_assert_block()` (`assert.rs:174`), and stores in
   `TestConfig.assert: Option<AssertBlock>` (line 113).
2. `run_test_flow()` (`plan/vm.rs:77–112`) wraps the assert block in a closure
   (`pre_steps: Option<Box<PreStepsHook>>`), at line 89–91, passing `run_assert_phase` as the
   hook.
3. `run_step_flow()` invokes the hook at line 242.
4. **The dispatch point** is `run_assert_phase()` in `plan/vm.rs:124–145`:

```rust
fn run_assert_phase(
    ssh: &SshOptions,
    assert_block: &AssertBlock,
    installer_username: Option<&str>,
) -> Result<()> {
    if !assert_block.files.is_empty()    { run_assert_files(ssh, assert_block)?; }
    if !assert_block.users.is_empty()    { run_assert_users(ssh, assert_block, installer_username)?; }
    if !assert_block.groups.is_empty()   { run_assert_groups(ssh, assert_block, installer_username)?; }
    if !assert_block.packages.is_empty() { run_assert_packages(ssh, assert_block)?; }
    if !assert_block.services.is_empty() { run_assert_services(ssh, assert_block)?; }
    Ok(())
}
```

Each `run_assert_*` function (`assert.rs:276, 496, 744, 934, 1108`) receives `&SshOptions` and
`&AssertBlock`, generates a shell script, runs it via `ssh_capture_stdout()`, and parses text
output to produce pass/fail.

**Requirement:** The plugin system must intercept both `build_compressor()` (`compress/codec.rs:19`)
and `run_assert_phase()` (`plan/vm.rs:124`) — these are the two verb resolution and execution
entry points.

**Challenge:** Both are today closed, imperative dispatch — not tables, not registries.

---

## 2 — Registration Surface (Extension-Friendliness)

### 2a. Compressors

**Selection mechanism:** Closed `match` over a closed enum (`CompressionType`) in
`build_compressor()` (`compress/codec.rs:23–26`).  Serde binds the enum from YAML strings at
deserialisation time (`#[serde(rename_all = "lowercase")]`, `compress/config.rs:21`).

Any new compressor verb requires:

1. A new variant in `CompressionType`.
2. A new arm in the `match` inside `build_compressor()`.
3. A new arm in `decompress_cluster()` (`compress/codec.rs:34–59`).
4. A new arm in the error-context string in `commit_output()` (`commands/build.rs:549–552`).

**Verdict: closed / hostile to extension. Refactor-first required.**

The `Compressor` trait already exists (lines 9–17), so the execution abstraction is in place;
the bottleneck is the enum + match in the selection layer.  The required refactor: replace
`CompressionType` (enum) + `build_compressor(enum, opts)` (match) with a
`HashMap<&str, Box<dyn CompressorFactory>>` registry keyed by verb string.  Serde deserialisation
of `compressor:` would change from a typed enum to a plain `String` looked up in the registry
at plan-load time.

### 2b. Asserts

**Selection mechanism:** Closed struct (`AssertBlock`) with hard-coded named fields, dispatched
via an if-per-field chain in `run_assert_phase()` (`plan/vm.rs:124–145`).  Each assert kind has
a distinct expectation type with no shared trait and is dispatched by an if-guard on the
specific field.  There is no `AssertKind` trait, no registry, no generic dispatch.

**Verdict: closed / hostile to extension. Refactor-first required.**

The required refactor is deeper: (a) introduce an `AssertKind` trait (with a method such as
`fn run(&self, ssh: &SshConn, entries: ...) -> Result<()>`), (b) replace the fixed `AssertBlock`
fields with a `HashMap<String, Box<dyn AssertKind>>` or similar registry, and (c) update both
the YAML deserialisation layer and `run_assert_phase`.

**Decision:** Both refactors are non-trivial and must precede any plugin wiring.  They should
be done in the same set of commits that introduces the plugin host so that internal and external
registration paths are unified from the start.

---

## 3 — Data Crossing the Plugin Boundary

### 3a. Compressor plugin boundary

After the registry refactor a plugin provides a factory that produces a `Box<dyn Compressor>`.
Data flowing across the seam:

| Direction | Type | Notes |
|---|---|---|
| CLI → plugin (construction) | `&str` (raw opts string) | Plain byte string |
| CLI → plugin (construction args) | `BTreeMap<String,String>` | Not FFI-safe |
| CLI → plugin (per-cluster, compression) | `&[u8]` (cluster bytes) | 64 KiB–2 MiB. **Buffered, not streaming.** |
| Plugin → CLI (per cluster) | `Vec<u8>` (compressed bytes) | Heap-allocated, owned |
| Plugin → CLI (error) | `anyhow::Result` error | Not FFI-safe |

**The per-cluster interface is buffered, not streaming.**  `compress_cluster(&[u8]) -> Result<Vec<u8>>`
takes a fully-materialised cluster buffer and returns a fully-materialised compressed buffer.
This is favourable for FFI — it reduces the interface to pointer+length pairs.  `Read`/`Write`
traits do not appear at the `Compressor` trait boundary (though they appear internally in
`ZlibCompressor` via `DeflateEncoder`).

Types that must become FFI-stable: the input slice `(ptr, len)`, the output buffer (a
`#[repr(C)]` owned byte buffer or write-back callback), and the error channel.

### 3b. Assert plugin boundary

Current `run_assert_*` functions receive:

| Type | Field / Usage | Notes |
|---|---|---|
| `&SshOptions` | `host: String`, `port: u16`, `user: String`, `key: PathBuf` | Connection parameters only — no open socket/handle |
| `&AssertBlock` | The parsed YAML config | Contains `BTreeMap<String, AssertFile>` etc. |
| `Option<&str>` | `installer_username` | For user/group exclusion |
| Return: `Result<()>` | Pass/fail | Not FFI-safe |

After the registry refactor the assert plugin would receive:

- An **opaque SSH execution handle** so it can run remote commands on the guest VM.  Currently
  this is done via `ssh_capture_stdout()` (`ssh.rs`), which takes `&SshOptions` and returns a
  `String`.  The plugin boundary needs either (a) a callback function pointer
  `fn(cmd: *const c_char, out: *mut c_char, ...) -> i32` or (b) an opaque handle to a
  pre-created SSH executor.
- **Per-entry config** as a serialised blob (JSON/YAML) or a flat C array of
  `{key, value_json}` pairs.
- **Output:** structured result (pass/fail + messages per entry).  `Result<()>` is not
  FFI-safe; a C-compatible return code + error string pointer is needed.

Types that must become FFI-stable: the SSH executor surface, the per-entry config map, the
pass/fail result.

**Requirement (both seams):** All types that cross the `.so` boundary must be either
`#[repr(C)]`-annotated value types, raw pointer + length pairs, or opaque handles with explicit
ownership contracts.  No `Vec`, `String`, `BTreeMap`, `PathBuf`, `Box<dyn Trait>`, or
`anyhow::Error` may cross as Rust types.

**Challenge:** The assert seam is harder than the compressor seam.  The compressor inputs/outputs
are pure byte buffers.  The assert inputs include an SSH session context that involves live
connections, an async runtime (russh uses Tokio internally), and complex Rust types.

---

## 4 — ABI / Stability Challenges

### 4a. The core problem

Rust has no stable ABI.  Struct layout, vtable shape, enum discriminant layout, and
`Result<T, E>` representation are all undefined and may differ between rustc versions,
optimisation levels, and crate compilation units.  The `.so` and the host binary may be compiled
by different rustc versions even when built from the same source.

### 4b. Evaluation against actual types

**Option (a): Hand-rolled `extern "C"` + `#[repr(C)]` vtable**

For the compressor seam this is tractable:

```c
// Plugin-exported entry points
OpaqueCompressor* compress_cluster_new(const char* opts, size_t opts_len);
int               compress_cluster_run(
                      const OpaqueCompressor* ctx,
                      const uint8_t* in,  size_t in_len,
                      uint8_t**      out, size_t* out_len);
void              compress_cluster_free(OpaqueCompressor* ctx);
const char*       compress_cluster_error(const OpaqueCompressor* ctx);
```

For the assert seam this is far harder.  The SSH execution callback itself involves an async
runtime (Tokio, via russh).  The plugin needs a synchronous `ssh_exec(cmd, out_ptr, out_len)`
callback — the host must wrap the async internals before exposing them.  The assert config map
(`BTreeMap<String, T>`) cannot be passed as-is; a flat JSON blob or a C array of
`{key, value_json}` pairs is needed.  This is doable but requires significant wrapper work on
both sides.

**Rustc/version-skew risk:** If host and plugin are compiled with different rustc versions, even
`extern "C"` calls are safe (C ABI is stable) — the risk is if Rust types accidentally leak
across the boundary.

**Option (b): `abi_stable` crate**

`abi_stable` provides a complete set of ABI-stable Rust type replacements (`RStr`, `RVec<u8>`,
`RResult`, `RHashMap`, module-level `#[sabi_trait]` vtables).  It allows the plugin interface
to be expressed in ergonomic Rust while remaining stable across compiler versions, and it handles
vtable compatibility through its own versioning layer.

For the compressor seam this would be the cleanest option: express `Compressor` as a
`#[sabi_trait]` and expose a factory function.  For the assert seam the SSH callback could be
expressed as an `abi_stable` function pointer type.

**Trade-off:** Adds a new workspace dependency (`abi_stable`).  Both the host crate and every
plugin must use it and must match the `abi_stable` version.  It also requires annotating plugin
interface structs with `#[derive(StableAbi)]`.

**Option (c): Serialised-payload boundary**

The plugin receives a JSON blob of its config and a function-pointer handle for I/O (for
asserts: a callback to run an SSH command and return its stdout as a JSON string).  The plugin
returns a JSON result blob.

For the compressor seam this is wasteful — serialising and deserialising 64 KiB cluster buffers
for every compression call adds significant overhead.

For the assert seam it is more viable — the number of SSH round-trips dominates latency, not
the serialisation of the config.  A plugin assert could receive its entries as a JSON string and
return a `[{key: ..., ok: ..., message: ...}]` JSON array.

**Recommendation:** A hybrid is likely best — option (a) or (b) for the compressor (to avoid
serialisation overhead on the hot per-cluster path), and option (b) or (c) for asserts (where
call frequency is low and config richness matters more).  The final decision belongs to the
maintainer (see Section 7).

**Requirement:** A formal ABI versioning scheme (semver or a monotone integer) must be embedded
in the plugin entry-point symbol name or in a `abi_version() -> u32` exported function, checked
at load time before any plugin function is called.

**Challenge:** The async SSH runtime (Tokio + russh) is currently embedded in the host.  A
plugin running assert logic via the SSH callback must either be driven by the host-provided
synchronous `ssh_exec` wrapper, or the host must expose a true `extern "C"` blocking function
that internally drives the Tokio runtime.  Neither is currently scaffolded.

---

## 5 — `unsafe` / Workspace Policy Challenge

**The gate** — `Cargo.toml` (workspace root), lines 25–26:

```toml
[workspace.lints.rust]
unsafe_code = "forbid"
```

The accompanying comment (lines 8–22) is explicit:

> "`forbid` (vs `deny`) means a downstream `#![allow(unsafe_code)]` in a child module cannot
> re-enable it; a future PR that needs an `unsafe` block has to first change the policy here in
> a stand-alone commit, which gets reviewed on its own merits."
>
> "Any expansion of this block belongs in a separate PR with its own justification — this is a
> policy gate, not a style sheet."

**Inheritance:** `botforge/Cargo.toml` carries `[lints] workspace = true`, so the workspace
policy is inherited by `botforge`.  All three member crates (`shasset`, `botforge`, `viscous`)
inherit it.  There is currently zero `unsafe` code in the entire workspace.

**What loading a `.so` requires:** `libloading` (the de-facto standard) uses `unsafe` at every
step — `Library::new()` (dlopen), `Library::get::<T>()` (dlsym + casting), and the call through
the resolved function pointer.  None of this can be written without `unsafe`.

**Required policy change:**

1. A standalone reviewed commit that either (a) changes `unsafe_code` from `"forbid"` to
   `"deny"` in `[workspace.lints.rust]`, or (b) introduces a new crate
   (e.g. `botforge-plugin-host`) that is excluded from the workspace lint policy via its own
   `[lints]` block (no `workspace = true`) and carries only the `unsafe`-requiring loading
   logic.
2. The PR for that standalone commit must include a justification explaining the specific
   `unsafe` use, its safety invariants, and why the argument is sound.

**Recommendation:** Creating a new `botforge-plugin-host` sub-crate with `unsafe_code = "allow"`
locally (without touching the workspace gate) is preferable to relaxing the whole-workspace gate.
This contains the blast radius and keeps `shasset`, `viscous`, and `botforge`'s own core logic
under `forbid`.

**Requirement:** This policy change is a prerequisite that must be merged before any `.so`
loading code is written.

**Challenge:** The workspace lint comment explicitly mirrors the same gate used across several
other `botworkz` repos — changing it may require cross-repo discussion.

---

## 6 — Plugin Discovery, Loading, and Failure Modes

No implementation exists today; requirements are derived from the code structure.

### 6a. Plugin discovery

There is no existing plugin search-path mechanism.  The `botforge` CLI currently takes workspace
YAML as its only config.  Options:

- A `plugins:` key in a `botforge.yaml` workspace marker (workspace markers are found by
  `workspace/discover.rs`) listing paths or globs to `.so` files.
- An environment variable (e.g. `BOTFORGE_PLUGIN_PATH`) listing colon-separated directories,
  analogous to `LD_LIBRARY_PATH`.
- Both: env-var overrides, workspace config as baseline.

**Requirement:** The plugin search path must be resolved and all plugins loaded (or failed to
load) **before** any plan config is parsed, because serde deserialisation of
`compressor: "myplugin"` must be able to validate against the registry.

### 6b. Registration at startup

After the registry refactor (Section 2), a plugin `.so` would be loaded via
`libloading::Library::new(path)`, and a well-known symbol (e.g. `botforge_plugin_register`)
would be resolved and called.  The registration function calls back into a host-provided
registry handle to register verbs:

- **Compressor:** `registry.register_compressor("pigz", factory_fn_ptr)` — factory receives
  `opts: *const c_char` and returns an `OpaqueCompressor` or error.
- **Assert:** `registry.register_assert("docker", factory_fn_ptr)` — factory receives config
  entries and returns an `OpaqueAssertRunner` or error.

**Requirement:** The plugin entry-point function signature must be `extern "C"` and documented
as part of the public plugin ABI contract.

### 6c. Unknown verb failure mode

With a string-keyed registry, plan loading (`load_build_config`, `load_test_config`) must look
up the verb string in the registry.  An unknown verb (not in the built-in list, not contributed
by any loaded plugin) should fail at **config-load time** with a clear error, e.g.:

> `error: unknown compressor verb 'pigz' — no built-in or loaded plugin provides this verb`

**Requirement:** Unknown verbs are a hard error at plan-load time, not at execution time.

### 6d. Built-in vs. plugin verb collision

If a plugin registers a verb matching a built-in (e.g. `"zstd"` or `"packages"`), the host
must have a defined policy:

- **Error at load time:** Plugin registration of a built-in name fails immediately — safest.
- **Plugin wins:** Flexible but dangerous.
- **Built-in wins:** Plugin registration of a known name is silently ignored or warned — confusing.

**Requirement:** The collision policy must be documented in the plugin ABI contract.  "Error at
load time" is the most defensive default.

---

## 7 — Open Spec Questions

The following decisions cannot be settled by reading the codebase and require explicit maintainer
input:

1. **Verb namespace/collision policy** — Should plugin verbs live in a namespace
   (e.g. `acme/pigz`) to avoid colliding with built-ins or with verbs from other plugins?
   If so, how does a user spell it in YAML (`compressor: "acme/pigz"`)?  If not, who wins on
   collision (see 6d)?

2. **ABI versioning scheme** — Should the plugin ABI be versioned with a monotone integer,
   semver, or a hash?  What is the compatibility policy — is a minor `botforge` version allowed
   to break the plugin ABI?  What happens when a plugin declares an older ABI version?

3. **Per-verb config schema validation** — Today `CompressConfig` uses
   `#[serde(deny_unknown_fields)]` to catch typos at parse time.  For plugin-contributed verbs,
   the host cannot validate per-verb config against the plugin's schema.  Should plugins expose
   a JSON Schema for their config, or is best-effort pass-through acceptable?

4. **Plugin trust and loading path** — Is the plugin loading path a fully trusted local
   filesystem path (no signature verification)?  Or is there a code-signing requirement for
   production builds?  Is there a sandboxing model?

5. **Compressor decompression symmetry** — `decompress_cluster()` (`compress/codec.rs:29–60`)
   also has a closed `match`.  For a plugin to produce readable artifacts, it must also
   contribute a **decompressor**.  Should `decompress_cluster` also become part of the plugin
   ABI, or will plugin-compressed images only be read by the same plugin installation?

6. **Plugin lifecycle and hot-reload** — Are plugins loaded once at startup and held for the
   lifetime of the process, or is per-plan loading/unloading considered?  Can two simultaneous
   `botforge build` invocations load the same plugin `.so` safely?

7. **Assert SSH execution model for plugins** — Currently all assert runners call
   `ssh_capture_stdout()` directly, which internally creates a Tokio current-thread runtime per
   call.  Should plugin asserts receive a blocking `ssh_exec` callback, an async handle, or a
   pre-collected set of guest facts (so the plugin has no network access at all)?

8. **`decompress_cluster` extension scope** — Is extending decompression for plugin-compressed
   read-back in scope for the first plugin release, or will the initial plugin system be
   write-only for compressors?

9. **Error reporting contract** — Should plugin errors be returned as C strings that the host
   converts to `anyhow::Error`, or is a structured error (code + message) expected?  What
   encoding (UTF-8 guaranteed)?
