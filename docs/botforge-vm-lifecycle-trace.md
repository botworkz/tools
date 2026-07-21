# botforge VM Lifecycle & Console Messaging Trace

> **Scope**: `botforge/` Rust crate. Covers both `botforge build` and `botforge test`
> commands. All line numbers reference the repository at the time of this analysis.

---

## 1. VM Start Sequence

### 1a. How the QEMU Process Is Started

**Build path** — `botforge/src/commands/build.rs` lines 323–336:

```
qemu_args = qemu_build_args(partial, seed_iso, ssh_port, memory, cpus, discard_enabled)
```

then either:

```
spawn_qemu_with_log(&qemu_args, &vm_log)    // non-attach (default)
spawn_qemu_attached(&qemu_args, &vm_log)    // --attach mode
```

**Test path** — `botforge/src/commands/test/mod.rs` lines 218–233:

```
qemu_args = qemu_run_args(overlay_image, seed_iso, extra_isos, ssh_port, ports, memory, cpus)
spawn_qemu_with_log(&qemu_args, &vm_log)    // non-attach (default)
spawn_qemu_attached(&qemu_args, &vm_log)    // --attach mode
```

The process launch is in `botforge/src/qemu.rs`:

- **`spawn_qemu_with_log`** (line 159): emits `🤖 (vm) Starting vm` via
  `crate::plan::print_phase("vm", "Starting vm")`, then spawns
  `qemu-system-x86_64` with stdout/stderr redirected to `vm_log`.
  **No completion message is ever emitted after this call.**

- **`spawn_qemu_attached`** (line 230): if stdin is a TTY, emits
  `🤖 (vm) Starting vm (attached console — Ctrl-A c for QEMU monitor)` (line 243)
  then spawns QEMU with inherited stdio. If stdin is not a TTY, falls back to
  `spawn_qemu_with_log`.

The hypervisor is always **`qemu-system-x86_64`** spawned directly on the host as a
child process via `std::process::Command`. There is no container wrapper, no
cloud-hypervisor, no libvirt.

For **build**: the primary drive is opened read-write directly (`qemu_build_args`
line 125) — no CoW overlay.  
For **test**: a CoW overlay (`test-overlay.qcow2`) is created first by
`create_overlay_image` → `create_qcow2_overlay`, then attached as the primary drive.

---

### 1b. Starting the VM inside the Container

There is no container layer. QEMU is spawned directly on the host. Cloud-init runs
inside the guest OS as part of its normal boot sequence; botforge only seeds it via
the cloud-init ISO and waits for it.

---

### 1c. Wait for SSH Availability

**Function**: `wait_for_ssh` — `botforge/src/ssh.rs` line 447.

```rust
pub(crate) fn wait_for_ssh(ssh: &SshOptions, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    let rt = make_rt();
    loop {
        if rt.block_on(connect_async(ssh, Duration::from_secs(10))).is_ok() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!("timed out waiting for SSH");
        }
        std::thread::sleep(Duration::from_secs(2));
    }
}
```

Called from `run_step_flow` — `botforge/src/plan/vm.rs` lines 232–235:

```rust
wait_for_ssh(
    ssh,
    remaining_budget(overall_deadline).min(TEST_SSH_READY_TIMEOUT),
)?;
```

| Parameter | Value |
|-----------|-------|
| Max timeout | `min(remaining_overall_budget, TEST_SSH_READY_TIMEOUT)` = up to **300 s** (`vm.rs:29`) |
| Poll interval | `std::thread::sleep(Duration::from_secs(2))` — flat 2-second retry |
| Per-attempt connect timeout | 10 s (hardcoded inside `wait_for_ssh`) |
| Console messages | **NONE** |

**This is the gap the user described.** After `🤖 (vm) Starting vm` is printed,
botforge enters `wait_for_ssh` silently. There is no "waiting for SSH…" banner,
no "SSH ready" completion message.

---

### 1d. Wait for Cloud-init Completion

Called from `run_step_flow` — `vm.rs` lines 237–243:

```rust
ssh_with_retry(
    ssh,
    "sudo cloud-init status --wait",
    TEST_TRANSPORT_RETRIES,         // 10
    TEST_TRANSPORT_RETRY_DELAY,     // Duration::from_secs(2)
    remaining_budget(overall_deadline).min(timeouts.cloud_init_timeout),
)?;
```

`ssh_with_retry` runs the blocking `cloud-init status --wait` inside the guest;
on transport errors it retries up to 10 times with a 2-second delay. The
`connect_timeout` is bounded by the remaining overall budget and the config's
`cloud_init_timeout` field.

**Console messages: NONE.** No "waiting for cloud-init…" or "cloud-init done"
message is printed.

After cloud-init, `require_stable_ssh_with_deadline` (`vm.rs:244`, impl at
`vm.rs:864`) runs `true` as an SSH heartbeat: 5 attempts, 2 consecutive successes
required, 2-second sleep between attempts. Also **SILENT**.

---

### Start Sequence — Ordered Summary

| Order | What happens | Function / File:line | Message emitted |
|-------|-------------|----------------------|-----------------|
| 1 | Disk copy → partial (build) / overlay creation (test) | `copy_qcow2` `build.rs:275` / `create_overlay_image` `test/mod.rs:193` | **SILENT** |
| 2 | Disk resize (build only) | `resize_qcow2` `build.rs:280` | **SILENT** |
| 3 | Seed ISO generation (cloud-init) | `prepare_seed_image` `build.rs:321`, `test/mod.rs:191` | **SILENT** |
| 4 | QEMU process spawned | `spawn_qemu_with_log` `qemu.rs:159` / `spawn_qemu_attached` `qemu.rs:243` | `🤖 (vm) Starting vm` **(start only, no completion)** |
| 5 | SSH polling loop | `wait_for_ssh` `ssh.rs:447`, called from `vm.rs:232` | **SILENT** (2 s intervals, ≤300 s) |
| 6 | cloud-init wait | `ssh_with_retry("sudo cloud-init status --wait", ...)` `vm.rs:237` | **SILENT** |
| 7 | SSH stability check | `require_stable_ssh_with_deadline` `vm.rs:864` | **SILENT** |
| 8 | ISO bootstrap mounts/scripts (test only) | inline in `run_step_flow` `vm.rs:252` | **SILENT** |
| 9 | File staging (if any) | `stage_files` `vm.rs:288` | **SILENT** |
| 10 | pre-steps assert phase (test only) | `run_assert_phase` `vm.rs:124` | ` ✓/✗ (assert) <label>` per check |
| 11 | Steps begin | `print_step_title` `log.rs:177` | `🤖 (<n>[/<id>]) <name>` per step |

**The gap**: after step 4 prints `🤖 (vm) Starting vm`, there is no output until
step 10/11. Steps 5, 6, and 7 are completely silent regardless of how long they
take (potentially several minutes total).

---

## 2. VM Stop/Shutdown Sequence and Ordering Relative to Compression

### Build Stop Sequence

The orchestrating function is `cmd_build` in `botforge/src/commands/build.rs`.

**Step 5 (lines 407–434): Guest fstrim reclaim** *(only if `reclaim: fstrim`)*

```
run_guest_reclaim_fstrim(&ssh_options, overall_deadline)
  → SSH: "sudo fstrim -av"
```

VM is still running. No console message. The encompassing `"compress"` banner is
emitted just before at `build.rs:410–413`:

```
🤖 (compress) Compressing image (reclaim, sparsify, compression)
```

This banner is only emitted when `compress_phase_runs` is true (reclaim or
compression enabled).

**Step 6 (lines 437–455): Guest cloud-init clean** *(always runs)*

```
run_guest_cloud_init_clean(&ssh_options, overall_deadline)
  → SSH: "sudo cloud-init clean --logs --seed || sudo cloud-init clean --logs"
```

VM is still running. **No console message.**

**Step 7 (lines 471–495): Installer teardown** *(botforge-owned identity only)*

```
run_installer_teardown(&ssh_options, &installer_user, overall_deadline)
  → SSH: "sudo systemd-run ... /bin/bash -lc 'sleep 2; userdel ...; systemctl poweroff'"
```

VM is still running. Queues a detached transient systemd service inside the guest.
The service waits 2 s after the SSH command returns, removes the installer user, then
calls `systemctl poweroff`. **No console message.**

**Step 8 (lines 500–513): `shutdown_build_vm`** — graceful shutdown

```
shutdown_build_vm(&mut vm_child, &partial, &failed_partial,
                  &ssh_options, !botforge_owned, overall_deadline, ...)
```

Inside `shutdown_build_vm` (`vm.rs:1463`):

1. **Start message** (`vm.rs:1472`):
   `crate::plan::print_phase("vm", "Stopping vm")`
   → emits `🤖 (vm) Stopping vm`
2. If `request_poweroff` is `true` (non-botforge-owned only): sends
   `sudo systemctl poweroff` over SSH (best-effort, errors ignored).
3. Polls `child.try_wait()` every 500 ms until the process exits or
   `BUILD_POWEROFF_TIMEOUT` (120 s, `vm.rs:1453`) / overall deadline fires.
4. **On clean exit** (`vm.rs:1535`):
   `crate::plan::print_phase_status("vm", "Stopping vm", true)`
   → emits ` ✓ (vm) Stopping vm`
5. **On failure/timeout** (`vm.rs:1532`, `1539`):
   `crate::plan::print_phase_status("vm", "Stopping vm", false)`
   → emits ` ✗ (vm) Stopping vm`

**Step 9 (lines 515–517): Host discard reclaim** *(only if `reclaim: discard`)*

```
reclaim_host_discard_offline(&partial)   // qemu-nbd + mount + fstrim on host
```

VM is dead. **No console message.**

**Step 10 (lines 519–534): Zero-cluster sparsify**

```
sparsify_zero_clusters(&partial)
```

VM is dead. **No console message** (debug-only `eprintln!` behind `BOTFORGE_DEBUG`).

**Step 11 (line 548): `commit_output`** — host-side compression

```
commit_output(&partial, &output, qcow2_compress)
  → compress_qcow2_image(partial, &tmp, compressor, ...)
  → std::fs::rename(tmp, output)
  → std::fs::remove_file(partial)
```

VM is dead. **No console message inside `commit_output`** — the only encompassing
banner was the `"compress"` one from step 5 (before VM shutdown).

**Step 12 (lines 564–571): Output message**

```
crate::plan::print_phase("output", &format!("Final image written to {} ({})", ...))
```

Emits: `🤖 (output) Final image written to <path> (<size>)`

---

### Critical Ordering: Shutdown vs Compression

```
steps complete
  │
  ▼
[compress banner]  🤖 (compress) Compressing image (reclaim, sparsify, compression)
  │                  build.rs:410   ← BEFORE VM shutdown
  │
  ▼  VM still running ──────────────────────────────────────────────────────────────┐
run_guest_reclaim_fstrim()   SSH: "sudo fstrim -av"               build.rs:416      │
  ↓                                                                                  │
run_guest_cloud_init_clean() SSH: "cloud-init clean"              build.rs:437      │
  ↓                                                                                  │
run_installer_teardown()     SSH: queues detached poweroff svc    build.rs:473      │
  ↓                                                                                 ─┘
shutdown_build_vm():                                               build.rs:500
  → 🤖 (vm) Stopping vm                 [start msg,   vm.rs:1472]
  → [if non-owned: SSH "sudo systemctl poweroff"]
  → poll child.try_wait() 500 ms × up to 120 s
  → ✓/✗ (vm) Stopping vm               [done msg,    vm.rs:1535/1532/1539]
  │
  ▼  VM dead ────────────────────────────────────────────────────────────────────────┐
reclaim_host_discard_offline()           build.rs:515                               │
  ↓                                                                                  │
sparsify_zero_clusters()                 build.rs:519                               │
  ↓                                                                                  │
commit_output() / compress_qcow2_image() build.rs:548   ← actual compression        │
  ↓                                                                                 ─┘
🤖 (output) Final image written to <path> (<size>)       build.rs:564
```

**The user's observation is correct**: the `"compress"` banner appears before VM
shutdown. The ordering in source is:

1. `"compress"` banner printed (`build.rs:410`)
2. Guest-side reclaim/clean/teardown (VM still live)
3. `shutdown_build_vm` → `"Stopping vm"` start message (`vm.rs:1472`)
4. VM polls to exit
5. `"Stopping vm"` done message (`vm.rs:1535`)
6. Host-side sparsify + `compress_qcow2_image` (actual compression, VM dead)

Guest-side work inside the VM:
- `sudo fstrim -av` (reclaim: fstrim)
- `cloud-init clean` (always)
- `userdel` + `systemctl poweroff` via detached systemd unit (botforge-owned)

Host-side work after VM exits:
- `qemu-nbd` + mount + fstrim (reclaim: discard)
- `sparsify_zero_clusters`
- `compress_qcow2_image`
- final rename/commit

---

### Test Stop Sequence

Test cleanup uses `cleanup_test` called from `test/mod.rs:274–280`:

```rust
pub(crate) fn cleanup_test(vm_child: &mut Option<Child>, overlay_image: &Path) {
    crate::plan::print_phase("vm", "Stopping vm");   // vm.rs:1425
    if let Some(child) = vm_child.as_mut() {
        let _ = child.kill();     // SIGKILL — not graceful
        let _ = child.wait();
    }
    *vm_child = None;
    let _ = std::fs::remove_file(overlay_image);
}
```

- **Start message**: `🤖 (vm) Stopping vm`
- **Completion message**: **NONE** — no `print_phase_status` call in `cleanup_test`
- VM is **killed** (`child.kill()` = SIGKILL), not gracefully shut down
- No compression phase in the test path

---

## 3. Console Messaging / Progress-Tick Infrastructure

All messaging lives in **`botforge/src/plan/log.rs`**.

### 3a. "Start" vs. "Done" Functions

| Role | Function | File:line | Format |
|------|----------|-----------|--------|
| Phase start | `print_phase(label, description)` | `log.rs:197` | `🤖 (<label>) <description>` |
| Phase done | `print_phase_status(label, description, success)` | `log.rs:221` | ` ✓/✗ (<label>) <description>` |
| Step start | `print_step_title(idx, name, id)` | `log.rs:177` | `🤖 (<n>[/<id>]) <name>` |
| Step done | `print_step_status(idx, name, id, ok)` | `log.rs:228` | ` ✓/✗ (<n>[/<id>]) <name>` |
| Step skipped | `print_step_skipped(idx, name, id)` | `log.rs:258` | ` ⊘ (<n>[/<id>]) <name>` |

All functions write to **stderr** via `eprintln!` and call `stderr_color_enabled()`
to decide whether to add ANSI codes.

`stderr_color_enabled()` (`log.rs:128`) honors:

1. `--color` flag / `FORCE_COLOR=1` / `CLICOLOR_FORCE=1` → always on
2. `NO_COLOR` env var → always off (unless overridden by #1)
3. `std::io::stderr().is_terminal()` — TTY fallback

---

### 3b. Success Tick Styling (Current)

#### `phase_status_marker` success path (`log.rs:207`)

```
" \x1b[32m✓\x1b[0m \x1b[2m({label})\x1b[0m \x1b[2m{description}\x1b[0m"
    ^^^^^^ green ^^^^^^             ^^ DIM ^^               ^^ DIM ^^
```

#### `step_status_marker` success path (`log.rs:167`)

```
" \x1b[32m✓\x1b[0m \x1b[2m({counter})\x1b[0m \x1b[2m{name}\x1b[0m"
    ^^^^^^ green ^^^^^^              ^^ DIM ^^             ^^ DIM ^^
```

| Element | ANSI sequence | Rendered as |
|---------|--------------|-------------|
| `✓` tick (success) | `\x1b[32m✓\x1b[0m` | **Green, normal intensity** — NOT faint |
| `✗` cross (failure) | `\x1b[31m✗\x1b[0m` | **Red, normal intensity** |
| Counter `(label)` on success | `\x1b[2m…\x1b[0m` | **Faint/dim** |
| Description on success | `\x1b[2m…\x1b[0m` | **Faint/dim** |
| Description on failure | *(no `\x1b[2m`)* | **Normal intensity** |

The user's requested state — "tick NOT faint while the rest of the line may be
faint" — is **already the current behavior**. The tick is plain green (`\x1b[32m`,
not `\x1b[2m`); the counter and description are dim on success. No change is needed
for the tick styling itself.

The unit tests at `log.rs:421–466` explicitly assert this contract:

```rust
assert!(success_color.contains("\x1b[32m"));               // tick is green
assert!(success_color.contains("\x1b[2mmcp-smoke\x1b[0m")); // name IS dimmed
assert!(!failure_color.contains("\x1b[2mmcp-smoke\x1b[0m")); // name NOT dimmed on fail
```

---

### 3c. Elapsed-Duration Suffix

**Currently: no elapsed-time measurement or "completed in N seconds" suffix exists
anywhere in the lifecycle messaging.**

`Instant` is used in `vm.rs` and `ssh.rs` solely for deadline tracking, never for
elapsed-time display. Neither `print_phase`, `print_phase_status`,
`print_step_title`, nor `print_step_status` accept or emit a duration.

`run_step_flow` returns `Ok(overall_deadline: Instant)` and `cmd_build` captures it
as `overall_deadline` at line 375, but it is not used for any display.

To add "completed in N seconds" consistently the injection points would be:

- Capture `Instant::now()` just before `spawn_qemu_with_log` / `spawn_qemu_attached`.
  Pass the elapsed value into a modified `shutdown_build_vm` signature so the
  `print_phase_status("vm", "Stopping vm", true)` call can append it.
- For the `"compress"` phase: capture time just before the first
  guest-side reclaim call and emit elapsed inside/after `commit_output`.
- `print_phase_status` / `phase_status_marker` / `step_status_marker` would each
  need an `Option<Duration>` parameter to append
  `" (completed in N seconds)"` to the description string.
- Per-step elapsed: capture `Instant::now()` in `run_step_flow` just before
  `print_step_title` and pass elapsed to `print_step_status`.

---

## 4. Summary Deliverables

### (a) Start-sequence messages

| Step | Message | Silent? |
|------|---------|---------|
| Disk copy / overlay creation | — | ✓ SILENT |
| Disk resize | — | ✓ SILENT |
| Seed ISO preparation | — | ✓ SILENT |
| QEMU spawn | `🤖 (vm) Starting vm` | start only, **no completion** |
| SSH polling (≤300 s, 2 s intervals) | — | ✓ SILENT |
| `cloud-init status --wait` | — | ✓ SILENT |
| SSH stability check | — | ✓ SILENT |
| Steps begin | `🤖 (<n>) <step-name>` | — |

### (b) Stop-sequence messages and position relative to compression

| # | Step | Message | VM state | vs. compression |
|---|------|---------|----------|-----------------|
| 1 | `"compress"` banner | `🤖 (compress) Compressing image …` | **running** | **before VM shutdown** |
| 2 | Guest fstrim (optional) | SILENT | running | before shutdown |
| 3 | Guest cloud-init clean | SILENT | running | before shutdown |
| 4 | Installer teardown (optional) | SILENT | running (queues poweroff) | before shutdown |
| 5 | `shutdown_build_vm` start | `🤖 (vm) Stopping vm` | transitioning | after `"compress"` banner |
| 6 | Shutdown poll | SILENT | dying | — |
| 7 | `shutdown_build_vm` done | ` ✓/✗ (vm) Stopping vm` | **dead** | before host compression |
| 8 | Host discard reclaim (optional) | SILENT | dead | — |
| 9 | Zero-cluster sparsify | SILENT | dead | — |
| 10 | `compress_qcow2_image` | SILENT | dead | ← actual compression |
| 11 | Output message | `🤖 (output) Final image written to …` | dead | after compression |

### (c) Tick/checkmark — location and current styling

| File | Line | Sequence | Meaning |
|------|------|----------|---------|
| `plan/log.rs` | 167 (step), 207 (phase) | `\x1b[32m✓\x1b[0m` | Green, **normal intensity** tick |
| `plan/log.rs` | 169 (step), 208 (phase) | `\x1b[31m✗\x1b[0m` | Red, **normal intensity** cross |
| `plan/log.rs` | 167, 207 | `\x1b[2m{name}\x1b[0m` | **Dim** description on success |
| `plan/log.rs` | 169, 209 | `{name}` (no dim) | Normal description on failure |

### (d) Elapsed-time — current state

Elapsed time is **not measured or displayed anywhere** in the user-facing lifecycle
messages. `Instant` values are used internally only for deadline enforcement.

Key places where `Instant::now()` could be captured for elapsed output:

| Message to enhance | Where to start the clock | Where to emit elapsed |
|--------------------|--------------------------|-----------------------|
| VM lifetime | Before `spawn_qemu_with_log` (`build.rs:335`) | Inside `shutdown_build_vm` at `print_phase_status` call (`vm.rs:1535`) |
| compress phase | Before `run_guest_reclaim_fstrim` (`build.rs:415`) | After `commit_output` (`build.rs:548`) or in a new `print_phase_status` call |
| Per-step | Just before `print_step_title` (`vm.rs:322`) | Inside `print_step_status` (`vm.rs:342`) |

The display functions `print_phase_status` and `print_step_status` would each need
an `Option<Duration>` (or `Option<u64>` seconds) parameter to append
`" (completed in N seconds)"` to the status line.
