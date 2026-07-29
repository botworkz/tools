# Fragments as Functions — Design Report

> **Status**: Stages 1–4 implemented.
> This document is the authoritative spec for the fragments-as-functions redesign,
> covering findings, the proposed model, and the staged implementation plan.

---

## §1 Findings: Current Architecture

### §1.1 Include / Expansion Loader

`uses:` entries are loaded and **flattened** at load time in
`botforge/src/config/mod.rs`:

- `expand_test_steps()` (~line 975) iterates raw step YAML nodes; when a node
  has a `uses:` key it calls `load_test_steps_fragment()`.
- `load_test_steps_fragment()` (~line 1000) recursively loads the fragment file,
  runs the two substitution passes (`inputs.*` and `args.*`), then **splices** the
  resulting `Vec<TestStep>` directly into the parent's step list.
- The caller never sees a fragment boundary; the parent document ends up with a
  flat `Vec<TestStep>` that interleaves parent and fragment steps.
- Substitution passes (load-time only): (1) `resolve_fragment_inputs` substitutes
  `${{ inputs.* }}` using the declared schema + `with:` call-site values
  (R1–R4); (2) `expand_args_in_steps` substitutes `${{ args.* }}` for `for:`
  iteration.

**Stage 1 change (implemented):** The final splice step (step 7) is replaced.
A `uses:` entry now produces a single `TestStep::Invoke(InvokeStep)` that *owns*
the fragment's steps as a `Vec<TestStep>`. The fragment is still loaded,
type-checked, and input-validated exactly as before; only the splicing is gone.

### §1.2 Step Execution Layer

`run_step_flow()` in `botforge/src/plan/vm.rs` (~line 218) was a flat sequential
loop over `Vec<TestStep>`. A single `accumulated_env: BTreeMap<String, String>` was
threaded across all steps, meaning a fragment's `env` side-effects leaked into
subsequent parent steps.

**Stage 1 change (implemented):** `run_step_flow` now calls `run_steps_inner`
recursively. When `run_steps_inner` encounters a `TestStep::Invoke`, it clones the
current `accumulated_env` into `child_env`, runs the invocation's inner steps with
`child_env`, then discards `child_env`. Env mutations inside a fragment do **not**
propagate back to the caller. The caller's env is visible *into* the fragment
(inherited via clone). `files:` and `cloud_init:` accumulation remain global (not
scoped), matching the original behaviour.

### §1.3 Substitution Passes

Two load-time passes run before execution (unchanged in Stage 1):

1. `inputs.*` substitution — `resolve_fragment_inputs` — happens inside
   `load_test_steps_fragment` before producing the `Invoke` step's inner steps.
2. `args.*` substitution — `expand_args_in_steps` — expands `for:` iterations
   within the fragment's step list.

A future Stage 3/4 will add a third, deferred, post-execution pass for
`outputs.*` / `steps.*` references. That pass is **not** in Stage 1. The full DOM
tree is retained across execution, providing the substrate for that future pass.

### §1.4 ID Handling and Namespacing

`RunStep.id` was display-only with no uniqueness validation anywhere
(`step.rs` ~line 86–90; `validate_test_steps` / `validate_build_steps` in
`config/mod.rs` never checked ids).

Under the old inlining model, including the same fragment twice would produce
duplicate ids in the flat step list — unfixable for remote fragments whose ids we
don't control.

**Stage 1 change (implemented):** A new `validate_scope_step_ids` function
(`config/mod.rs`) checks that `id:` values are unique **within each scope**. The
root document is one scope; each fragment body is one scope. A fragment's ids only
need to be unique within that one fragment file. Duplicate ids within the same
scope are a hard load-time error. The same id in different scopes is allowed — this
is the whole point: reuse-safe, remote-safe.

---

## §2 The Load-Time vs Runtime Seam

Inputs are entirely load-time: `${{ inputs.* }}` and `${{ args.* }}` are resolved
before execution starts. Outputs are inherently runtime: they don't exist until a
step has run.

The seam sits between the two: the DOM tree produced at load time is the stable
substrate that the runtime executor walks. After execution of a fragment, a future
post-execution pass would write captured output values back into the tree (or an
adjacent results map), making them available for resolution in the next scope level.

**DOM stays** — scoped invocation, typed cross-references (`steps.<id>.outputs`),
and deferred post-execution resolution all require retained-tree random access.
SAX/streaming is incompatible with the design and is a non-goal.

---

## §3 Proposed Model (Fragments as Functions)

### §3.1 Scoped Invocation (`TestStep::Invoke`)

A `uses:` entry becomes a `TestStep::Invoke(InvokeStep)`:

```rust
pub struct InvokeStep {
    pub uses: String,
    pub steps: Vec<TestStep>,  // the fragment's own step list, fully substituted
}
```

`TestStep` is now recursive. The fragment is still loaded and validated at load
time; only the final splice is replaced with ownership.

### §3.2 Recursive Executor with Hierarchical Indices

`run_steps_inner(steps, parent_indices, ...)` walks the step list recursively.
When it hits an `Invoke` step at index `i` it recurses with
`parent_indices = [parent..., i]`. Inner step `j` of that invocation has display
index `"3.2"` (parent index 3, inner index 2).

Dots are replaced with `-` in filesystem paths (log files, temp scripts). The
`step_log_path`, `print_step_title`, `print_step_status`, and `print_step_skipped`
functions all accept `step_display: &str` instead of `usize`.

### §3.3 Env Scoping (Locked Decision)

Env is an **inheritance-down / containment-up** boundary:

- The caller's `accumulated_env` is **cloned** into the fragment's `child_env`.
- Mutations inside the fragment are contained to `child_env`.
- After the invocation completes, `child_env` is dropped.
- `files:` and `cloud_init:` accumulation remain globally threaded (not scoped).

This eliminates the implicit env-leaking side-channel that existed before Stage 1.
Typed outputs (Stages 2–4) are the sanctioned way to return values from a fragment.

### §3.4 Per-Scope ID Uniqueness

IDs are validated per scope at load time. A duplicate within one scope is a hard
error. The same id across different scopes (e.g. two invocations of the same
fragment) is valid — ids are fragment-local.

---

## §4 Output Typing and Coercion Mechanism (Stages 2–4)

> **Not implemented in Stage 1.** Documented here for completeness per issue #555.

- Steps declare typed `outputs:` (`string`/`secret`/`number`/`bool`).
- A step emits values via `echo NAME=value >> $BF_OUT`.
- At capture time, the emitted string is coerced to the declared type (hard failure
  by default; opt-in `on_type_error: empty` for leniency).
- Fragment `outputs:` re-export step outputs with enforced step↔fragment
  type-match.
- `required:` defaults to `false`; if not `true`, a `default:` is mandatory; both
  set is a contradiction / hard load-time error (symmetric with #544 R1).
- Declaration-level checks (required-or-default, not-both) are load-time.
- `required: true` enforcement (step must actually emit the value) is runtime.
- `default:` supplies the value when the step did not emit the output; the default
  is type-coerced/validated like any emitted value.

---

## §5 Secrets Handling

Stage 5 adds `type: secret` as a masked-at-display output type.

- A `secret` value is stored and propagated exactly like a normal string value.
- Masking is **declaration-driven only** (`type: secret`), not value-taint tracking.
- `secret` and non-secret output types are an absolute type-match at fragment
  boundaries (no silent downgrade).

### Use sinks vs display sinks

- **Use sinks** see the real value:
  - `${{ steps.<id>.outputs.<name> }}` interpolation into `run:`
  - `$BF_ENV` writes for downstream step env propagation
  - `$BF_OUT` capture/re-export round-trips
- **Display sinks** show `***` for secret-declared values:
  - value projections used for botforge-authored human-facing output
  - error/reporting paths that would otherwise embed a secret-declared value

### Steel-toe caps, not a sandbox

This is accidental-leak prevention for botforge-emitted output, not a security
boundary. A step that legitimately receives a secret can still exfiltrate it.

### Optional secondary safety-net

A best-effort literal scan can additionally redact exact known secret values in
botforge-emitted lines, but this is explicitly secondary and fragile (it does
not survive transformations such as encoding or slicing).

---

## §6 Fragments as Functions: `jq.steps.yaml` Example

With Stage 1 in place, a `jq.steps.yaml` fragment can be called with typed inputs
and — after Stages 2–4 land — return a typed output. The invocation is a
self-contained scope with its own env; ids are local to the fragment; the caller
sees only the declared contract.

---

## §7 Ordering / Dependency

Fragment invocations are strictly sequential within the parent step list (no
parallelism). The `Invoke` step is the natural unit of a future dependency/ordering
graph; `matrix:` / parallelism would attach here in a later stage.

---

## §8 Risks and Open Questions

- **`files:` and `cloud_init:` global accumulation**: deliberate (report §3.3).
  Future stages may revisit if fragment-local file accumulation is needed.
- **Env isolation correctness**: if a parent step later references an env variable
  that a fragment used to populate, that will now be empty. This is correct
  behaviour — callers must use declared typed outputs (Stages 2–4) instead.
- **Remote fragments**: id uniqueness is now per-scope, making remote reuse safe.
  Input/output validation across remotes is deferred to a later stage.

---

## §9 Staged Implementation Plan

| Stage | What | Status |
|-------|------|--------|
| **1** | Scoped `TestStep::Invoke` + recursive executor + hierarchical indices + per-scope id uniqueness + env isolation | **Done (this PR)** |
| **2** | Typed step `outputs:` declaration + `$BF_OUT` capture + coercion/validation | **Done** |
| **3** | Fragment `outputs:` wiring + step↔fragment type-match enforcement + `id:` on `uses:` + lazy `${{ steps.<id>.outputs.<name> }}` consumption at the fragment boundary | **Done** |
| **4** | General runtime output-reference namespace (`steps.<id>.outputs.<name>` for run+invoke ids in current scope, plus fragment-self `outputs.<name>`) with deferred/backward resolution | **Done** |
| **5** | `type: secret` output masking for botforge display/error sinks while preserving real values at use sinks | **Done** |

**Shipped runtime output-reference grammar (Stage 4):**
- `${{ steps.<id>.outputs.<name> }}` — resolves in the current scope only, against already-executed sibling run/invoke steps.
- `${{ outputs.<name> }}` — resolves only inside a fragment body, via that fragment's declared `outputs:` contract (`from_step`/`from_output`, `default`, `required`).
- Both forms are deferred runtime-only references allowed in `run:` fields; all other `${{ }}` expressions remain load-time resolved/rejected.

**Stage 1 explicitly does NOT include:** `$BF_OUT`, typed `outputs:`,
`steps.*` / `outputs.*` namespace, Invariant A changes, deferred/runtime
substitution pass, or secret masking.

---

## §10 Stage 1 Implementation Summary

### New data structures (`step.rs`)
- `InvokeStep { uses: String, steps: Vec<TestStep> }` — owns a fragment's step list.
- `TestStep::Invoke(InvokeStep)` — new variant; `TestStep` is now recursive.
- `display_name()` and `display_id()` updated to handle `Invoke`.

### Config changes (`config/mod.rs`)
- `expand_test_steps` produces `TestStep::Invoke` instead of splicing steps.
- `validate_scope_step_ids` — new load-time per-scope id-uniqueness checker.
- `steps_have_host_step` — recursive helper for the "has host step" validator check.
- All validators (`validate_test_steps`, `validate_build_steps`,
  `validate_publish_steps`) handle the new `Invoke` arm.

### Executor changes (`plan/vm.rs`)
- `build_step_display(parent_indices, step_idx) -> String` — builds hierarchical
  index strings (`"3"`, `"3.2"`, `"3.2.1"`, …).
- `run_steps_inner` — recursive walk; clones env for `Invoke` scopes.
- All step-index parameters changed from `usize` to `&str`.

### Logging changes (`plan/log.rs`)
- All logging functions accept `step_display: &str` (was `step_idx: usize`).
- Log filenames replace `.` with `-` for filesystem safety.

### Build command changes (`commands/build.rs`)
- `run_archive_step` and the `archive_executor` closure accept `step_display: &str`.
