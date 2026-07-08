# Build Port Investigation: virt-customize → Booted-VM `build`

**Scope:** `botworkz/tools` (botforge), `botworkz/vm`, `botworkz/space`  
**Status:** Investigation only — no code changed in this PR.  
**Purpose:** De-risking pass to inform Stage B (new booted `build` command).

---

## 1. Inventory of Relevant Code

### 1.1 `crate::plan` — shared typed-document and VM runtime

**`botforge/src/plan/config.rs`**

The central loader for botforge YAML documents.

| Concept | Detail |
|---|---|
| `DocumentType` enum | `test` / `fragment` — the only two registered kinds today. Every document must carry a `type:` field; missing or unknown type is a hard load-time error. |
| `TestConfig` struct | Holds `isos: Vec<TestIso>`, `ports: Vec<PortSpec>`, `steps: Vec<TestStep>`, `diagnostics_units: Vec<String>`. |
| `RawTestDocument` | Deserialization target for a `type: test` entrypoint. Dispatches to `TestConfig` after the `type:` check. |
| `uses: @://...` | Fragment includes resolved by `resolve_uses_path()`. Only `@://` scheme supported. Path must be repo-relative and free of `.`/`..`. |
| `inputs:` / `${{ inputs.NAME }}` | Fragment input declarations (`type`, `required`, `default`). Substitution walks the entire YAML value tree before serde-deserialization. |
| Cycle/depth guards | `include_stack` detects direct cycles. `MAX_INCLUDE_DEPTH = 32` limits nesting. |
| `check_fragment_document_type()` | `uses:` targets must be `type: fragment`; `type: test` (and future `type: build`) documents may not be included as fragments. |
| `check_no_entrypoint_sections_in_fragment()` | `ports:`, `isos:`, `diagnostics_units:` are rejected inside fragments — they are entrypoint-only. |
| `validate_test_ports()` | Rejects port 0, port 22, ssh-port collision, and duplicate port numbers. |
| `validate_test_steps()` | Validates `shell:` for each step and enforces "if any `on: host` step exists, `ports:` must be declared". |

> **Cite:** `botforge/src/plan/config.rs:19–52` (DocumentType), `:72–163` (TestConfig + loader), `:244–293` (fragment checks), `:472–518` (validation).

**`botforge/src/plan/step.rs`**

| Type | Fields / Notes |
|---|---|
| `StepTarget` | `guest` (runs via SSH) / `host` (runs locally in botforge container). |
| `TestStep` | `on: StepTarget`, `name: String`, `uploads: Vec<TestUpload>`, `run: String`, `shell: Option<String>`. |
| `TestUpload` | `src: PathBuf` (resolved under repo root), `dest: String` (guest path). |
| `resolve_shell()` | Named: `bash` (default), `sh`, `python`. Custom: any string containing `{0}`. Errors on unknown single-token names and multi-token strings without `{0}`. |

> **Cite:** `botforge/src/plan/step.rs:1–85`.

**`botforge/src/plan/vm.rs`**

The VM step-execution runtime, currently only consumed by `botforge test`.

| Function | Behaviour |
|---|---|
| `run_test_flow()` | Waits for SSH (`TEST_SSH_READY_TIMEOUT = 300s`), waits for `cloud-init status --wait` (`TEST_CLOUD_INIT_TIMEOUT = 300s`), requires stable SSH, mounts ISO bootstraps, then runs each step. |
| Step dispatch (guest) | Uploads any `uploads:`, writes script to `/tmp/botforge-step-{idx}-{suffix}.sh`, SCPs it, executes via `ssh` with the `shell:` template, reads back `BOTFORGE_ENV` for accumulated env state, cleans up. |
| Step dispatch (host) | Writes script to temp file, runs locally with `accumulated_env` injected as environment variables, reads back `BOTFORGE_ENV`. |
| `cleanup_test()` | **Kills** qemu (`child.kill()`) then removes the overlay. This is safe for test (disposable overlay) but would corrupt a disk you intend to keep. |
| `collect_test_diagnostics()` | `systemctl --failed`, `journalctl` for declared units, `cloud-init status --long`. |
| `print_log_tail()` | Prints the last N lines of the QEMU log file. |
| Per-step JSONL logging | `StepLogWriter` writes `{ts, stream, line}` JSONL to `build/logs/step-{idx}-{name}.log`. Live console output forwarded concurrently with non-blocking resilience. |

> **Cite:** `botforge/src/plan/vm.rs:33–549`.

---

### 1.2 `commands/build_legacy.rs` — virt-customize (chroot) builder

Registered as `Commands::BuildLegacy` / subcommand `build-legacy`. Marked for removal once consumers migrate.

**CLI surface:** `--spec`, `--source`, `--output`, `--repo-root`.

**Spec schema (`BuildLegacySpec`):**

| Field | Behaviour |
|---|---|
| `disk_size` | Default `"10G"`. `qemu-img resize <partial> <size>` before virt-customize runs. |
| `expand_partition` | Optional. If set, runs `virt-resize --expand <partition> <partial> <expanded>` then atomically swaps the expanded copy back. Required when source filesystem is too small to hold the chroot writes. |
| `memsize` | Default `4096`. Passed to `virt-customize --memsize`. |
| `smp` | Default `4`. Passed to `virt-customize --smp`. |
| `context` | `dest: <guest-path>`, `paths: [...]`. Host paths bundled into a tar (`cp -a` + `tar -cf`), uploaded to `<dest>/ctx.tar`, extracted in-guest via `--run-command tar -C <dest> -xf <ctx.tar> && rm -f <ctx.tar>`. Runs before any step. |
| `steps` | Vec of `BuildLegacyStep` variants (see below). |

**Note:** `BuildLegacySpec` does NOT have a `type:` field. It uses a separate `serde_yaml::from_str` call with `deny_unknown_fields`, independent of `plan::config`.

**`BuildLegacyStep` variants:**

| Variant | virt-customize flag | Booted equivalent |
|---|---|---|
| `run: <script-path>` | `--run <resolved-path>` | `uploads: [{src: path, dest: /tmp/...}]` + `run: bash /tmp/...` guest step |
| `run_command: <cmd>` | `--run-command <cmd>` | `run: <cmd>` inline guest step |
| `upload: {src, dest}` | `--upload <src>:<dest>` | `uploads: [{src, dest}]` guest step |
| `copy_in: {src, dest}` | `--copy-in <src>:<dest>` | `uploads:` (for files) or tar-extract pattern for dirs |
| `mkdir: <path>` | `--mkdir <path>` | `run: mkdir -p <path>` inline guest step |
| `truncate: <path>` | `--truncate <path>` | `run: truncate -s 0 <path>` inline guest step (or move into provisioner) |
| `delete: <path>` | `--delete <path>` | `run: rm -rf <path>` inline guest step (or move into provisioner) |
| `write: {path, content}` | `--write <path>:<content>` | `run:` with heredoc or `tee` inline guest step |

**Disk lifecycle in build_legacy:**
1. Copy source → `<output>.partial` (`cp --reflink=auto`, fallback `fs::copy`).
2. `qemu-img resize <partial> <size>` (header-only grow).
3. Optional `virt-resize --expand <partition>` via scratch empty qcow2 + atomic swap.
4. Build staging dir (`create_temp_dir("botforge-build")`).
5. `virt-customize -a <partial> --memsize ... --smp ... [context steps] [steps]`.
6. Cleanup staging dir.
7. `fs::rename(<partial>, <output>)` — atomic materialization.

> **Cite:** `botforge/src/commands/build_legacy.rs:177–284` (cmd_build_legacy), `:133–176` (step types), `:292–499` (helpers).

---

### 1.3 `botforge test` vs `build-legacy` — overlap and divergence

| Dimension | `botforge test` | `botforge build-legacy` |
|---|---|---|
| Config format | `plan::config` / `type: test` | Separate `BuildLegacySpec` / no `type:` |
| Step vocabulary | `{on, name, run, uploads, shell}` — guest/host | `{run, run_command, upload, copy_in, mkdir, truncate, delete, write}` — chroot ops |
| VM/guest involvement | KVM-only; boots real qemu + cloud-init | libguestfs appliance (supermin); no real boot, no systemd |
| Disk mode | Read-only base + disposable `test-overlay.qcow2` | Copy → `partial` → **virt-customize in-place** |
| Output lifecycle | Overlay discarded; test result is pass/fail | Partial atomically renamed to output qcow2 |
| Shutdown | `child.kill()` — safe because overlay is thrown away | N/A (virt-customize is synchronous, no long-running process) |
| KVM requirement | Hard (`require_kvm()`) | Soft (uses KVM for appliance if available, else TCG) |
| Fragment/includes | `uses: @://...`, `inputs:`, `${{ }}` | None |
| Env propagation | `BOTFORGE_ENV` / `$GITHUB_ENV` compatible | None |

Both use `qemu-img`. Both copy the source before touching it. Neither modifies the source directly.

---

## 2. What Must Be ADDED to botforge for the New Booted `build`

### 2.1 Persist-and-commit disk lifecycle

The new `build` must own a disk through its entire lifecycle, unlike `test` which discards the overlay. The required runtime mode:

1. **Pre-boot disk setup:** Copy `source → <output>.partial` (same `cp --reflink=auto` logic as today). Run `qemu-img resize <partial> <disk_size>`. Do **not** run virt-resize — see §2.4.
2. **Boot the partial directly (read-write, no overlay):** Pass `<partial>` as the primary drive without the `-b <base>` CoW overlay mechanism. This means the VM writes directly to `<partial>`.
3. **Run the build steps** via the shared `plan::vm::run_test_flow` (or a renamed variant), waiting for SSH and cloud-init first.
4. **Graceful shutdown:** SSH `sudo systemctl poweroff` (or `sudo shutdown -h now`), then wait for qemu to exit with a timeout (e.g. 120 s). **Do not call `child.kill()`** until the timeout fires and shutdown has failed — kill on a live-write disk produces a filesystem that is not `fsck`-clean and may have partially-written metadata. The `cleanup_test()` kill path must not be used for build.
5. **Disk consistency gate:** Only proceed if qemu exits with status 0 (normal poweroff). If shutdown fails or qemu crashes, leave `<partial>` behind for post-mortem and return an error. Clear `<partial>` on next invocation (same pattern as current build-legacy).
6. **Atomic rename:** `fs::rename(<partial>, <output>)` — identical to build-legacy.

**Why this is safe:** A `systemctl poweroff` flushes all dirty pages and journals, unmounts filesystems, and only then sends ACPI shutdown to qemu. qemu flushes its qcow2 writeback cache during shutdown. The result is a consistent, fsck-clean disk — which is required for a qcow2 that will later be used as a build source for the next layer.

**Key difference from test's `cleanup_test()`:** The shared `cleanup_test` function in `plan/vm.rs` (line 542–549) calls `child.kill()` unconditionally. Build needs a distinct `shutdown_build_vm()` path that issues SSH poweroff, polls for qemu exit, and only kills as a last resort (leaving the disk in a "needs recovery" state, not silently keeping it).

> **Cite:** `botforge/src/plan/vm.rs:542–549` (cleanup_test — must not be reused for build), `botforge/src/commands/build_legacy.rs:200–284` (disk lifecycle to replicate minus virt-customize).

### 2.2 `type: build` entrypoint in plan/config.rs

Add `Build` to `DocumentType`:

```rust
enum DocumentType {
    Test,
    Fragment,
    Build,   // NEW
}
```

Add `BuildConfig` struct (analogous to `TestConfig`):

```rust
pub(crate) struct BuildConfig {
    pub(crate) disk_size: String,    // default "10G"
    pub(crate) memsize: u32,         // default 4096
    pub(crate) smp: u32,             // default 4
    pub(crate) steps: Vec<TestStep>, // SAME type as test steps
}
```

Add `RawBuildDocument` (analogous to `RawTestDocument`) and `load_build_config()`.

Sections that are **not valid** in a `type: build` document (and should be rejected with a descriptive error, analogous to `check_no_entrypoint_sections_in_fragment`):
- `ports:` — build does not forward ports
- `isos:` — build does not attach extra ISOs (cloud-init seed is generated internally, same as test)
- `diagnostics_units:` — build does not collect systemd failure diagnostics

Sections that are **build-only** (rejected in `type: test` and `type: fragment`):
- `disk_size:`
- `memsize:`
- `smp:`

Note: `disk_size`, `memsize`, `smp` can optionally be allowed in fragments (as parameterised values) if the team wants reusable "build profile" fragments, but the simpler first cut is to make them entrypoint-only.

> **Cite:** `botforge/src/plan/config.rs:19–52` (DocumentType), `:72–163` (TestConfig pattern to follow).

### 2.3 Step-vocabulary unification

Build steps **are** `TestStep`. The `{on, name, run, uploads, shell}` vocabulary applies unchanged:

- `on: guest` — SSH into the booted VM and run the script.
- `on: host` — run locally in the botforge container (same as test). For build this is mainly useful for compression or post-processing steps before shutdown.
- `uploads:` — SCP files into the guest before running (replaces `copy_in`/`upload`/context tarball for simple cases).
- `run:` — inline shell (replaces `run_command`) or a reference to a provisioner script uploaded via `uploads:`.
- `shell:` — same interpreter selection as test.

**Mapping legacy virt-customize ops to booted steps:**

| Legacy op | Booted equivalent |
|---|---|
| `run: images/.../provisioner.sh` | `uploads: [{src: images/.../provisioner.sh, dest: /tmp/prov.sh}]` + `run: bash /tmp/prov.sh` (or inline `run:` if the script is short) |
| `run_command: some command` | `run: some command` (inline, `on: guest`) |
| `upload: {src: f, dest: /path/f}` | `uploads: [{src: f, dest: /path/f}]` on a guest step |
| `copy_in: {src: dir, dest: /parent}` | tar the directory on host, upload the tar, extract in guest — one `on: host` step + one `on: guest` step; or absorbed into the context mechanism (see §2.5) |
| `mkdir: /path` | `run: mkdir -p /path` (`on: guest`) |
| `truncate: /etc/machine-id` | `run: truncate -s 0 /etc/machine-id` (`on: guest`) — or remain in provisioner script |
| `delete: /var/lib/dbus/machine-id` | `run: rm -f /var/lib/dbus/machine-id` (`on: guest`) — or remain in provisioner script |
| `write: {path, content}` | `run: tee /path <<'EOF'\ncontent\nEOF` (`on: guest`) |

For the three `botwork-base` / `botwork-docker` / `botwork` `build.yaml` files, the `truncate:` and `delete:` steps already have duplicates inside the provisioner scripts (e.g. `99-cleanup.sh` already does the machine-id steps). In the booted model these can be consolidated: just run the provisioner and let it handle hygiene inline, removing the standalone `truncate`/`delete` lines.

The `context:` bulk-staging mechanism (used only by `images/botwork/build.yaml`) is the most complex translation. See §2.5.

> **Cite:** `botworkz/vm:images/botwork-base/build.yaml`, `images/botwork-docker/build.yaml`, `images/botwork/build.yaml`.

### 2.4 Disk sizing in the booted world

Build-legacy uses two mechanisms:
1. `qemu-img resize` — grows the virtual disk size (header-only, no partition/fs change).
2. `virt-resize --expand <partition>` — grows a named partition + filesystem into the new space (offline, requires a source→target copy).

In the booted model:
- **`qemu-img resize` is still needed** before boot, so the VM boots with the full declared virtual size.
- **`virt-resize` is replaced by cloud-init's `growpart` module**, which runs on first boot and expands the root partition + filesystem to fill the disk. Debian Trixie cloud images ship cloud-init with `growpart` enabled by default; the Debian genericcloud image already contains the `growpart`/`resize2fs` userspace tools.
- **botforge does not need to run `virt-resize`** for the booted case. The guest handles it on boot, before the provisioning steps run (cloud-init runs before SSH becomes available, and `run_test_flow` already waits for `cloud-init status --wait`).
- **Exception:** The 16G growth on `images/botwork/build.yaml` (from 12G botwork-docker to 16G botwork) requires enough space for the context tarball + docker image tarballs. With booted provisioning the context tarball is replaced by individual uploads and `botforge deps`-style asset fetching (see §5), which distributes the I/O and avoids a single 500MB+ tar. This removes the ENOSPC pressure that necessitated `expand_partition` in the first place.

If the disk size declared in the spec is smaller than the source image's virtual size, `qemu-img resize` will reject it — no special handling needed.

> **Cite:** `botforge/src/commands/build_legacy.rs:41–73` (BuildLegacySpec fields + expand_partition rationale), `images/botwork/build.yaml` (16G / disk sizing comment).

### 2.5 The `context:` bulk-staging seam

`images/botwork/build.yaml` uses `context:` to bundle five tree-structured sources into a single tarball before virt-customize:
- `images/botwork/payload/envoy`
- `images/botwork/payload/systemd`
- `images/botwork/payload/firstboot`
- `build/bin` (botwork-launcher + botwork-tools binaries)
- `build/images/baked/` → guest path `images/` (service tarballs: session-broker, config-broker, control-plane, db-migrate, api, ui, mcp-echo, postgres, curl)

In the booted model, `context:` maps to a combination of:
1. **`uploads:`** for the payload trees (envoy, systemd, firstboot — small, static files already in the repo tree).
2. **`@shasset` asset refs** for `build/bin` and `build/images/baked/` (see §5).
3. The guest provisioner (`20-botwork-stack.sh`) continues to be uploaded and run; it expects files at `/tmp/botwork-build-context/...` which the uploads will recreate.

An alternative: keep a first-class `context:` block in `type: build` documents (analogous to build-legacy, but implemented as a set of sequential SCP uploads rather than a single tar). This is simpler for Phase 1 but defers the asset-ref story. See §5 and the roadmap.

### 2.6 qcow2-as-terminal-outcome seam

The `build` command's signature is:
```
botforge build \
  --repo-root <path> \
  --spec <path-to-type-build-yaml> \
  --source <source.qcow2> \
  --output <output.qcow2>
```

The output is the qcow2 written to `<output>` after graceful shutdown, identical in shape to build-legacy's output today. The caller (`pack.sh`) does not need to change its call site at the semantic level — only the subcommand name changes from `build-legacy` to `build`.

---

## 3. What Can Be DROPPED from the botforge Container

### 3.1 Per-package audit

| Package | What it provides | Which command uses it | Needed after build-legacy gone? |
|---|---|---|---|
| `libguestfs-tools` | `virt-customize`, `virt-resize`, `virt-filesystems`, `guestfish`, etc. | `build-legacy` only | **NO** — drop |
| `linux-image-amd64` | Kernel for the libguestfs supermin appliance (~50–100 MB) | Implicitly by libguestfs | **NO** — drop (pulled only for libguestfs appliance build) |
| `qemu-system-x86` | `qemu-system-x86_64` | `build`, `test`, `run` | **YES** — keep |
| `qemu-utils` | `qemu-img` | All commands that touch qcow2 headers | **YES** — keep (used for resize, overlay, convert) |
| `xorriso` | ISO building (alternative to genisoimage) | `test`, `payload`, `iso` (detect_iso_tool picks one) | **YES** — keep one of xorriso/genisoimage |
| `genisoimage` | ISO building | Same as xorriso | One is sufficient; can pick one and drop the other |
| `openssh-client` | `ssh`, `scp`, `ssh-keygen` | `build`, `test` | **YES** — keep |
| `cloud-image-utils` | `cloud-localds` (writes NoCloud seed ISOs) | `test` (write_seed_files) | **YES** — keep |
| `libnss-wrapper` | Lets virt-customize run as a non-root uid by wrapping nss calls | `build-legacy` only (docker-entrypoint.sh sets it up for libguestfs) | **NO** — drop |
| `jq` | JSON processing | Used by `docker-entrypoint.sh` for uid/gid nss wrapping | **MAYBE** — check docker-entrypoint.sh; if nss wrapping is removed with libguestfs, drop |
| `curl` | HTTP downloads | `deps` command (shasset fetches) | **YES** — keep |
| `ca-certificates` | TLS trust roots | Any HTTPS access | **YES** — keep |

> **Cite:** `botforge/Dockerfile:52–66`, `botforge/docker-entrypoint.sh`.

### 3.2 Estimated savings

- `libguestfs-tools` + `linux-image-amd64` + `libnss-wrapper`: collectively ~300–500 MB of installed content (the kernel image alone is ~50 MB, and the libguestfs appliance cache under `/tmp/.cache` grows to ~200–400 MB at runtime).
- The `RUN mkdir -p /tmp/.cache && chmod 1777 /tmp/.cache` and `ENV LIBGUESTFS_BACKEND=direct` lines become dead weight and should also be removed.
- Net: the runtime image would shrink by roughly **300–500 MB** installed size and the container attack surface drops significantly (no kernel-level code paths via the libguestfs appliance, no LD_PRELOAD nss wrapper).

### 3.3 Ordering constraint

The libguestfs/virt-customize toolchain can only be dropped from the Dockerfile **after both `botworkz/vm` and `botworkz/space` have stopped calling `build-legacy`** and the botforge image pin in both repos has been bumped to a version that no longer bakes `build-legacy` (or that still bakes it but behind a gate that prevents it from being called). The simplest sequencing:

1. Merge new booted `build` (Stage B).
2. Port `botworkz/vm` to `type: build` + new command.
3. Port `botworkz/space` (if applicable).
4. Remove `build-legacy` from botforge source.
5. Drop the chroot toolchain from the Dockerfile.

Steps 4 and 5 are locked to each other and are the last thing — never earlier.

---

## 4. Migration Plan for vm and space

### 4.1 `botworkz/vm` migration

**Current callsite** (`scripts/pack.sh` → `build_image()`):
```bash
run_botforge_compose image-build -- \
  build-legacy \
  --repo-root "${REPO_ROOT}" \
  --spec "${spec}" \
  --source "${src}" \
  --output "${out}"
```

**After migration:** Same call, `build-legacy` → `build`. The call site does not change otherwise.

**Three `build.yaml` translations:**

**`images/botwork-base/build.yaml`** — simplest layer, good prototype candidate:

```yaml
# Before (build-legacy):
disk_size: 10G
memsize: 4096
smp: 4
steps:
- run: images/botwork-base/provisioners/00-base.sh
- run: images/botwork-base/provisioners/10-bot-user.sh
- run: images/botwork-base/provisioners/99-cleanup.sh
- truncate: /etc/machine-id
- delete: /var/lib/dbus/machine-id
```

```yaml
# After (booted build):
type: build
disk_size: 10G
memsize: 4096
smp: 4
steps:
- on: guest
  name: base packages
  uploads:
  - src: images/botwork-base/provisioners/00-base.sh
    dest: /tmp/prov-00-base.sh
  run: bash /tmp/prov-00-base.sh
- on: guest
  name: bot user
  uploads:
  - src: images/botwork-base/provisioners/10-bot-user.sh
    dest: /tmp/prov-10-bot-user.sh
  run: bash /tmp/prov-10-bot-user.sh
- on: guest
  name: cleanup
  uploads:
  - src: images/botwork-base/provisioners/99-cleanup.sh
    dest: /tmp/prov-99-cleanup.sh
  run: bash /tmp/prov-99-cleanup.sh
```

Note: `truncate:` and `delete:` are removed because `99-cleanup.sh` already contains the machine-id hygiene steps (verified at `images/botwork-base/provisioners/99-cleanup.sh`). The provisioner is idempotent on this point.

However: `99-cleanup.sh` contains `journalctl --rotate` and `journalctl --vacuum-time=1s` with a fallback `rm -rf /var/log/journal/*`. In the virt-customize offline chroot, journald is not running so the `journalctl` calls fall back to `rm`. In the **booted** model, journald IS running, so the `journalctl --rotate && --vacuum-time=1s` path executes correctly. This is actually an improvement.

The `dd if=/dev/zero of=/EMPTY` free-space zero-fill in `99-cleanup.sh` also works correctly in the booted model — the guest writes zeroes to free disk space, then `qemu-img convert -c` compresses them away.

**`images/botwork-docker/build.yaml`** — same pattern as botwork-base, just one provisioner:

```yaml
type: build
disk_size: 12G
memsize: 4096
smp: 4
steps:
- on: guest
  name: docker engine
  uploads:
  - src: images/botwork-docker/provisioners/15-docker.sh
    dest: /tmp/prov-15-docker.sh
  run: bash /tmp/prov-15-docker.sh
- on: guest
  name: cleanup
  uploads:
  - src: images/botwork-docker/provisioners/99-cleanup.sh
    dest: /tmp/prov-99-cleanup.sh
  run: bash /tmp/prov-99-cleanup.sh
```

**`images/botwork/build.yaml`** — hardest layer (context: staging, 16G disk, many assets):

The `context:` block bundles `build/bin` (botwork-launcher, botwork-tools) and `build/images/baked/` (9 docker image tarballs). In the booted model these map to:

```yaml
type: build
disk_size: 16G   # see §2.4 — 16G still needed for asset staging headroom
memsize: 4096
smp: 4
steps:
# Stage static payload (small, repo-resident):
- on: guest
  name: stage payload
  uploads:
  - src: images/botwork/payload/envoy
    dest: /tmp/botwork-build-context/envoy
  - src: images/botwork/payload/systemd
    dest: /tmp/botwork-build-context/systemd
  - src: images/botwork/payload/firstboot
    dest: /tmp/botwork-build-context/firstboot
  run: echo "payload staged"
# Stage binaries (botwork-launcher, botwork-tools):
- on: host
  name: fetch binaries
  run: botforge deps --out "${REPO_ROOT}/build/bin" --executable
- on: guest
  name: upload binaries
  uploads:
  - src: build/bin
    dest: /tmp/botwork-build-context/bin
  run: echo "binaries staged"
# Stage docker image tarballs (via botforge deps / @shasset — see §5):
- on: host
  name: fetch docker images
  run: botforge deps --out "${REPO_ROOT}/build/images/baked"
- on: guest
  name: upload docker images
  uploads:
  - src: build/images/baked
    dest: /tmp/botwork-build-context/images
  run: echo "images staged"
# Run provisioner:
- on: guest
  name: botwork stack
  uploads:
  - src: images/botwork/provisioners/20-botwork-stack.sh
    dest: /tmp/prov-20-botwork-stack.sh
  run: bash /tmp/prov-20-botwork-stack.sh
- on: guest
  name: cleanup
  uploads:
  - src: images/botwork/provisioners/99-cleanup.sh
    dest: /tmp/prov-99-cleanup.sh
  run: bash /tmp/prov-99-cleanup.sh
```

**Hard seams in botwork:**

1. **`context:` → individual uploads**: The provisioner `20-botwork-stack.sh` expects paths under `/tmp/botwork-build-context/` — that path contract is maintained in the booted spec above without structural change to the provisioner.

2. **Machine-id hygiene**: In virt-customize, `truncate: /etc/machine-id` and `delete: /var/lib/dbus/machine-id` run after the cloud-init seed has already written a machine-id. In the booted model, cloud-init generates a machine-id on first boot. The provisioner's cleanup step (`99-cleanup.sh`) already handles this correctly: it truncates `/etc/machine-id`, removes `/var/lib/dbus/machine-id`, and creates the symlink. This happens **after** all provisioner steps, just before shutdown. Result: the output image ships with a blank machine-id, which is correct.

3. **16G disk size**: Still needed. Even without the `ctx.tar` approach, the docker image tarballs themselves are ~520 MB each copy (one in `build/images/baked/`, one in `/usr/share/botwork/images/`). 16G gives adequate headroom. This is a correct carryover.

4. **`build-deps.sh` orchestration**: Currently `pack.sh` calls `build-deps.sh` before building the `botwork` layer. In the booted model this remains necessary: the docker image tarballs and binaries need to be present on the botforge host before the build steps SCP them in. The `on: host` fetch steps in the spec above absorb this. With `@shasset` notation (see §5), the `build-deps.sh` call can be entirely declarative.

**`pack.sh` changes after migration:**

- `build_image()` changes `build-legacy` → `build` (already done by the companion PR for the rename).
- `image_needs_staged_dependencies()` grep can be updated or the logic absorbed into the declarative spec (see §5).
- The `--entrypoint qemu-img image-build -- convert -O qcow2 -c ...` compression step is unaffected (it runs after `build_image()` completes and uses qemu-img directly, not the `build` subcommand).
- The manifest chain-walk, Debian image fetch, and caching logic all remain in `pack.sh` until Phase 4/5 (see §5 and roadmap).

> **Cite:** `botworkz/vm:scripts/pack.sh:143–171` (build_image), `images/botwork/build.yaml`, `images/botwork-base/provisioners/99-cleanup.sh`.

### 4.2 `botworkz/space` migration

**Current architecture** (two modes in `scripts/lib/base-image.sh` `ensure_base_image()`):

- **Sibling mode:** Shells into `../vm/scripts/pack.sh --compress`. Space builds by delegating to vm's builder, one hop removed.
- **Release mode:** Fetches the prebuilt qcow2 via `botforge deps` from `shasset.yaml`.

**Does space call `botforge build` directly?** No. Space never calls `botforge build` (or `build-legacy`) itself. The botforge image pin bump in space (0.4.25 PR) is strictly a version alignment, not a functional change.

**Delegation question: keep shelling to vm's `pack.sh`, or invoke `botforge build` directly?**

**Recommendation: keep the delegation to vm's `pack.sh`** for sibling mode, at least through Stage B.

Rationale:
- The botwork image chain (botwork-base → botwork-docker → botwork) is vm's concern. Space should not own the manifest-walk or the Debian image fetch.
- The release-mode path (`botforge deps`) already provides a clean abstraction: space consumes a pre-built artifact.
- Direct `botforge build` from space would require space to carry or duplicate vm's `manifest.yaml`, `images/*/build.yaml`, and the associated provisioner scripts — or space would need to invoke vm's `pack.sh` anyway for the chain.
- A future Phase 5 `@shasset` / declarative-chain approach might change this, but that is a natural refactor point after the booted `build` is proven, not a prerequisite.

**Space-specific adjacent work:**

Space's `test-packed*.yaml` files currently lack a `type:` field. Once the tools repo enforces `type:` on test documents as a hard load-time error (tracked separately, referenced in the prior conversation as a latent break), space's test plans will need `type: test` added. This is independent of the build migration but must be tracked — see §4.4.

### 4.3 What lands in lockstep vs. independently

| Change | Locks with | Notes |
|---|---|---|
| New `botforge build` command (tools) | N/A — additive, `build-legacy` coexists | Can ship before consumers migrate |
| `images/botwork-base/build.yaml` → `type: build` | New botforge `build` command | Prototype layer; validate first |
| `images/botwork-docker/build.yaml` → `type: build` | botwork-base migration | Sequential (parent chain) |
| `images/botwork/build.yaml` → `type: build` | botwork-docker migration | Most complex; validate last |
| `pack.sh` update (`build-legacy` → `build`) | All three `build.yaml` conversions | One PR after all yamls are done |
| Remove `build-legacy` from tools | vm and space both off `build-legacy` | Late; see §3.3 |
| Drop chroot toolchain from Dockerfile | build-legacy removal | After removal confirmed |
| `type: test` added to test-packed*.yaml in vm/space | Separate tools `type:` enforcement issue | Independent; see §4.4 |

### 4.4 Latent `type:` break on test documents

The tools `plan/config.rs` already enforces `type:` on test documents loaded by `botforge test` (any missing or unknown `type:` is a hard error). If any future change makes this enforcement stricter (e.g. removing the separate loading path that currently bypasses it) or if the tools version that enforces this strictly is bumped in vm/space, test plans without `type: test` will fail silently or loudly.

Affected files to audit (in vm and space respectively):
- `botworkz/vm`: `images/*/test/*.yaml` — check all test plans for `type: test`.
- `botworkz/space`: all `*test*.yaml` plans passed to `botforge test`.

This is independent of the build migration. File a separate issue to add `type: test` to all test documents in vm and space as a preventive measure.

> **Cite:** `botforge/src/plan/config.rs:144–163` (type enforcement in load_test_config).

---

## 5. Eliminate Bash via Richer Build Declarations + `@shasset` Asset Refs

### 5.1 How much of the surrounding bash is "staging plumbing"?

`scripts/pack.sh` and `scripts/build-deps.sh` do the following logical jobs:

| Job | Currently in | Eliminable? |
|---|---|---|
| Manifest chain-walk (parent DAG resolution) | `pack.sh:manifest_chain()` via `scripts/lib/manifest.sh` | **Yes** — declare `parent:` directly in `type: build` spec; botforge resolves the chain |
| Debian cloud image fetch + sha512 verify | `pack.sh:fetch_debian_cloud_image()` | **Yes** — `source:` declared as `@shasset` upstream-image ref |
| Intermediate image caching | `pack.sh` (checks `staged/` before building) | **Yes** — botforge can manage intermediate artifact caching |
| `build-deps.sh` call (tools binaries + docker image tarballs) | `pack.sh:image_needs_staged_dependencies()` | **Yes** — replace with `deps:` block in build spec, using `@shasset` refs |
| `qemu-img convert -c` compression | `pack.sh` after `build_image()` | **Yes** — declare `compress: true` in build spec or as a post-build step |
| Compose / KVM gid / uid wiring | `scripts/lib/tools.sh:run_botforge_compose()` | **No** — this is infrastructure; stays in `compose.yml` + shell |
| Image verification (`--backing-file` check) | `pack.sh` | **Yes** — botforge can assert no backing file on output |

With a fully declarative `type: build` spec that carries `parent:`, `source:`, `deps:`, and `compress:`, the `pack.sh` chain-walk reduces to: read the spec, call `botforge build`, done. `build-deps.sh` disappears entirely.

### 5.2 `@shasset` asset notation — proposed design

**Problem:** Build and test specs today reference assets by filesystem path under `build/`. These paths are host-infrastructure detail that leaks into declarative specs and makes specs non-portable.

**Interim notation: `@://build/some-asset.qcow2`**

Extend `resolve_uses_path()` (or add a parallel `resolve_asset_path()`) to handle `@://build/...` as a path-based asset reference routed through the asset resolver. The `@://` scheme already exists for `uses:` includes; a sub-scheme like `@://build/...` reuses the same resolver entry point.

```yaml
# In a build spec step (uploads or source):
uploads:
- src: "@://build/images/baked/session-broker.tar"
  dest: /tmp/botwork-build-context/images/session-broker.tar
```

In the Rust resolver, `@://build/...` maps to `resolve_under_root(repo_root, "build/...")` — identical to today's behaviour but expressed via the asset-ref syntax.

This gives the mechanism without banning `build/` paths immediately. It is a refactoring of the lookup, not a semantic change.

**End-state notation: `@shasset://asset-name`**

Once the resolver understands named assets, every source/dependency becomes an opaque logical name:

```yaml
# Source asset: the upstream Debian cloud image, name from shasset.yaml
source: "@shasset://debian-trixie-cloud"

# Dependency assets:
deps:
- name: session-broker
  dest: /tmp/botwork-build-context/images/session-broker.tar
- name: botwork-launcher
  dest: /tmp/botwork-build-context/bin/botwork-launcher
```

The resolver contract:
1. Look up `name` in `shasset.yaml` (the `--config` file, default `shasset.yaml`).
2. Check if the asset is already present on disk at the canonical output path (idempotency).
3. If not, fetch it via the existing `botforge deps` logic (OCI pull, HTTP download, or local copy).
4. Return the resolved local path.

This is exactly what `botforge deps` does today for individual assets. The integration is: make `botforge build` call this resolver for each `@shasset://...` ref rather than requiring a pre-call to `botforge deps` in shell.

**How `@shasset` interacts with `botforge deps`:**

`botforge deps` remains the standalone CLI for pre-fetching assets (e.g. in a separate CI step to prime caches). `@shasset://name` in a build spec is lazy: botforge resolves it at build time, skipping the fetch if the asset is already present. Both use the same underlying fetch logic.

**Migration path from interim to end-state:**

1. (Interim) Add `@://build/...` support to the asset path resolver; update specs to use it instead of bare paths.
2. (End-state) Register named asset entries in `shasset.yaml` for the upstream Debian image. Replace `@://build/...` with `@shasset://name` in specs. Declare the Debian image in shasset.
3. Remove `fetch_debian_cloud_image()` from `pack.sh` (now handled by the resolver).
4. Remove `build-deps.sh` (assets declared in spec, fetched by resolver).
5. Remove the `image_needs_staged_dependencies()` grep heuristic.

**Apply to tests as well:**

`botforge test` specs also pull assets (base image, payload ISOs, botwork-login). The `@shasset://` notation applies equally:

```yaml
type: test
# base_image is passed on the CLI today; alternatively:
isos:
- path: "@shasset://botwork-payload"
  label: botwork-payload
  mount: /mnt/botwork-payload
steps:
- on: host
  name: resolve base image
  run: |
    echo "BASE_IMAGE=$(botforge asset-path botwork)" >> "$BOTFORGE_ENV"
```

The exact integration point (CLI arg vs. spec field) is a design call for Stage B, but the notation is the same.

> **Cite:** `botworkz/vm:scripts/pack.sh` (full file), `scripts/build-deps.sh`, `scripts/lib/images.sh`, `shasset.yaml`.

---

## Phased Implementation Roadmap

Each phase is independently assignable. Phases 1–3 are the critical path for the booted `build`; Phases 4–5 are follow-on cleanup. Every phase ends with a concrete acceptance criterion.

---

### Phase 0 — Prototype: validate booted provisioning on `botwork-base` *(prototype, no PR)*
**Why first:** The booted model's correctness assumption — that a cloud-init-booted Debian VM can be provisioned via SSH and then gracefully shut down to a consistent disk — has never been validated for these specific provisioners. Machine-id hygiene, `dd` zero-fill, and `fstrim` inside a booted guest all behave differently than inside a virt-customize offline chroot. This prototype validates that assumption before any permanent migration.

**Tasks:**
- Manually run `botwork-base`'s provisioners inside a KVM-booted Debian Trixie cloud VM (or a minimal botforge test harness) and confirm: SSH access, provisioner execution, graceful poweroff, resulting disk passes `qemu-img check`, and machine-id is blank in the output image.
- Confirm `journalctl --rotate && --vacuum-time=1s` works correctly in the booted context (vs. the offline fallback `rm -rf` used by virt-customize).
- Time the prototype build: booted provisioning adds KVM boot (~10–15 s) + cloud-init wait (~30–60 s) over the offline chroot; acceptable if total wall time stays under ~5 min for the base layer.

**Acceptance criterion:** A manually-produced `botwork-base.qcow2` via KVM boot + SSH provisioning that boots cleanly, has a blank machine-id, and passes `qemu-img check`.

---

### Phase 1 — New `botforge build` command (tools repo)
**Scope:** Add `Commands::Build` in `botforge/src/cli.rs`, implement `commands/build.rs`, register `DocumentType::Build` and `BuildConfig` in `plan/config.rs`, add `run_build_flow()` (persist-and-commit lifecycle with graceful shutdown) in `plan/vm.rs`. `build-legacy` coexists; nothing consumer-facing changes.

**Key invariants to enforce in implementation:**
- `type: build` documents reject `ports:`, `isos:`, `diagnostics_units:`.
- `type: test` documents reject `disk_size:`, `memsize:`, `smp:`.
- `type: fragment` documents reject all of the above (no change to existing check).
- `shutdown_build_vm()` issues SSH `sudo systemctl poweroff`, polls for qemu exit up to 120 s, kills only on timeout, and marks the partial as tainted on non-clean exit.
- KVM hard-required (`require_kvm()`) — no TCG fallback.

**Acceptance criterion:** `botforge build --spec images/botwork-base/build.yaml --source debian.qcow2 --output out.qcow2` produces a valid qcow2 when run on a KVM host; `botforge test` and `botforge build-legacy` continue to pass existing tests.

---

### Phase 2 — Port `botwork-base` to `type: build` (vm repo)
**Scope:** Update `images/botwork-base/build.yaml` to `type: build` + booted step format. Bump the botforge pin to the Phase 1 release. Keep `build-legacy` invocation in `pack.sh` working in parallel (or switch `build_image()` to use `build` for just the base layer with a flag).

**Acceptance criterion:** `scripts/pack.sh botwork-base` completes, producing `botwork-base.qcow2` that passes the existing `images/botwork-base/test/` smoke test.

---

### Phase 3 — Port `botwork-docker` and `botwork` to `type: build` (vm repo)
**Scope:** Translate `images/botwork-docker/build.yaml` and `images/botwork/build.yaml` as described in §4.1. The `botwork` layer requires resolving the `context:` → individual uploads translation and validating the 16G disk constraint.

**Acceptance criterion:** Full `scripts/pack.sh` run (botwork-base → botwork-docker → botwork, with `--compress`) produces a correctly-sized, bootable `debian-13-botwork.qcow2` and the full `images/botwork/test/` suite passes.

---

### Phase 4 — Remove `build-legacy`, drop chroot toolchain (tools repo)
**Scope:** Delete `commands/build_legacy.rs`, remove `Commands::BuildLegacy` from `cli.rs` and `main.rs`, remove `build-legacy` from `commands/mod.rs`. Remove `libguestfs-tools`, `linux-image-amd64`, `libnss-wrapper`, and related Dockerfile/entrypoint cruft.

**Prerequisite:** Phase 3 complete **and** both vm and space have been confirmed to no longer invoke `build-legacy`.

**Acceptance criterion:** `botforge --help` shows no `build-legacy` subcommand; the botforge image build succeeds and the published image is measurably smaller (target: ≥200 MB reduction in uncompressed image size).

---

### Phase 5 — Bash reduction: `@shasset` notation + declarative manifest chain *(tools + vm)*
**Scope:** Implement `@shasset://name` asset ref resolution in botforge. Add `source:` and `deps:` fields to `type: build` spec. Declare the Debian upstream cloud image in `shasset.yaml`. Update `images/botwork/build.yaml` to use `@shasset://` refs instead of `build/bin` and `build/images/baked/` paths. Reduce `pack.sh` to manifest-chain-walk (or absorb the chain walk into a `parent:` field in the build spec). Eliminate `build-deps.sh`.

**Acceptance criterion:** `botforge build --spec images/botwork/build.yaml --source @shasset://debian-trixie-cloud --output botwork.qcow2` runs end-to-end without calling any external shell script to pre-stage assets. `pack.sh` is reduced to ≤50 lines (chain invocation only, or entirely replaced by a `botforge build-chain` subcommand).

---

### Summary table

| Phase | Repo(s) | Acceptance criterion | Blocks |
|---|---|---|---|
| 0 | — (prototype) | Manually booted botwork-base qcow2 is clean | Phase 1 |
| 1 | tools | `botforge build` ships; tests pass | Phase 2 |
| 2 | vm | botwork-base built via `botforge build`; smoke test passes | Phase 3 |
| 3 | vm | Full chain built; botwork test suite passes | Phase 4 |
| 4 | tools (+ vm/space confirm) | No `build-legacy`; image ≥200 MB smaller | Phase 5 |
| 5 | tools + vm | No `build-deps.sh`; `pack.sh` ≤50 lines | — |

**Prototype-first recommendation:** Do not open the Phase 1 implementation issue until the Phase 0 prototype has confirmed that booted provisioning produces a clean disk. The most likely failure mode is the graceful-shutdown / disk-consistency requirement — specifically, whether `systemctl poweroff` reliably produces a fsck-clean qcow2 on the first attempt with a 10G disk and heavy write activity (the `dd` zero-fill in `99-cleanup.sh`). Validate this before committing to the design.
