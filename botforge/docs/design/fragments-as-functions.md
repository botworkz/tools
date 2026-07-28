# Fragments as functions: scoped invocation + typed inputs/outputs

**Status:** initial investigation report (no implementation). Deliverable of the
"Fragments as functions" issue; implementation is split into follow-up issues
from the staged plan at the end of this document.

This report traces the current architecture (loader, substitution, execution,
id handling), locates the load-time vs runtime seam, and proposes a scoped
side-chain invocation model with declared, typed `outputs:` — the symmetric,
validated egress counterpart of the typed `inputs:` ingress landed in the
expression/typed-input work (#544).

All file references are relative to `botforge/src/` unless noted, at the
commit this report was written against.

---

## 1. Findings: current architecture

### 1.1 Include / expansion loader — what "inlining" does today

Fragments are **flattened into the parent step list at load time**. There is
no fragment object at runtime; by the time any step executes, the notion of a
fragment has been erased.

Entry points:

- `load_test_config()` — `config/mod.rs:336-414`
- `load_build_config()` — `config/mod.rs:416-503` (same machinery)
- `load_publish_config()` — `config/mod.rs:691-780` (`for:` expansion only;
  no `uses:` support)

The pipeline, per `uses:` include:

1. **Expansion driver** — `expand_test_steps()` (`config/mod.rs:975-1044`)
   iterates `Vec<RawTestStep>`; `RawTestStep::Step` goes through
   `expand_raw_step()`, `RawTestStep::Include` triggers fragment loading.
2. **Path resolution** — `resolve_uses_path()` (`config/mod.rs:1223-1249`):
   only the `@://<path>` scheme, resolved under the repo root, traversal
   rejected. There is no remote scheme yet, but the grammar reserves room for
   one.
3. **Cycle + depth guards** — the include stack (seeded with the root document
   at `config/mod.rs:355`) detects cycles (`config/mod.rs:995-1001`);
   `MAX_INCLUDE_DEPTH = 32` (`config/mod.rs:32`) bounds nesting.
4. **Fragment load + validation** — `load_test_steps_fragment()`
   (`config/mod.rs:1046-1081`): enforces `type: botforge/fragment` via
   `check_fragment_document_type()` (`config/mod.rs:1089-1113`) and rejects
   entrypoint-only sections (`ports`, `isos`, `name`, timeouts, …) via
   `check_no_entrypoint_sections_in_fragment()` (`config/mod.rs:1120-1148`).
5. **Input resolution** — `extract_fragment_input_declarations()`
   (`config/expressions/mod.rs:150-167`) parses the `inputs:` block;
   `resolve_fragment_inputs()` (`config/expressions/mod.rs:179-261`) enforces
   the #544 boundary rules (R1: `default:` XOR `required: true`; R2: strict
   native typing, no implicit string→number/bool coercion; R3: finite numbers
   only; R4: the `__default__` sentinel) and produces a
   `TypedInputMap = BTreeMap<String, EvaluatedValue>`.
6. **Substitution (pass 1)** — `substitute_inputs_in_value()`
   (`config/expressions/mod.rs:345-350`) resolves `${{ inputs.* }}` across the
   whole fragment YAML value, deferring `${{ args.* }}` for the later pass.
7. **Recursive splice** — the fragment's steps re-enter `expand_test_steps()`
   (`config/mod.rs:1016-1029`) and the resulting flat steps are appended to
   the parent's list. Fragment `files:` are accumulated into a flat
   `files_acc` and fragment `cloud_init:` is deep-merged into a single
   accumulator (`config/mod.rs:1034-1039`).

The net effect: an arbitrarily deep tree of fragments becomes **one flat
`Vec<TestStep>`** with **one flat file list** and **one merged cloud-init
document**. Nothing downstream (validation, planning, execution, logging)
knows fragments ever existed.

### 1.2 Substitution passes — entirely load-time, two passes

Both passes run inside config loading, strictly before any execution:

- **Pass 1 (`inputs`)**: at include time, per fragment
  (`substitute_inputs_in_value`, `config/expressions/mod.rs:345-350`).
- **Pass 2 (`args` / `for:`)**: at step-expansion time
  (`expand_raw_step()`, `config/expressions/mod.rs:52-110`;
  `substitute_args_in_value`, `config/expressions/mod.rs:357-362`).

Key mechanics:

- The engine (`substitute_namespace_in_value`,
  `config/expressions/mod.rs:375-438`) distinguishes **pure expressions**
  (`field: ${{ expr }}` → typed YAML scalar via
  `EvaluatedValue::to_yaml_value()`, `config/expressions/value.rs:56-74`) from
  **interpolation** (mixed text → stringified via
  `to_interpolated_string()`).
- **Deferral is hard-coded to exactly two namespaces**:
  `is_deferred_namespace()` (`config/expressions/eval.rs:224-230`) allows only
  `inputs` ↔ `args` to survive each other's pass; any other namespace is a
  hard error at evaluation.
- **Invariant A** — `check_no_residual_expressions()`
  (`config/expressions/mod.rs:561-582`): after the final (args) pass, **no
  `${{ }}` may remain anywhere in a step**. This is the single most important
  constraint for this design: a `${{ steps.x.outputs.y }}` reference cannot
  currently survive to runtime — the loader would reject it.
- The expression AST (`config/expressions/parser.rs:7-26`) already supports
  references of the shape `namespace.name`, operators (`==`, `!=`, `&&`,
  `||`, `!`), literals, and function calls; `EvaluatedValue`
  (`config/expressions/value.rs:11-17`) carries `String | Number | Bool |
  Empty` with defined truthiness, YAML emission and interpolation rules. This
  machinery is directly reusable for an `outputs.*` namespace.

### 1.3 Step execution layer — where a `$BOTFORGE_OUTPUT` sink would live

Execution is a **strictly sequential loop** in `run_step_flow()`
(`plan/vm.rs:218-385`): one step at a time, in flat list order, threading a
single mutable `accumulated_env: Vec<(String, String)>` (`plan/vm.rs:324`)
across **all** steps, guest and host alike.

There is already a working per-step **emit-sink → capture → merge** cycle for
env vars — the exact mechanical pattern typed outputs need:

- **Guest steps** (`StepTarget::Guest`, `plan/vm.rs:402-539`): the `run:`
  body is written to a local temp script, scp'd to
  `/tmp/botforge-step-{idx}-{suffix}.sh`, and executed over SSH.
  - *Sink initialization*: `guest_env_init_cmd()` (`plan/vm.rs:1216-1220`)
    creates a world-writable `/tmp/botforge-env-{idx}-{suffix}` on the guest.
  - *Sink advertisement*: `build_guest_ssh_cmd()` (`plan/vm.rs:1222-1259`)
    builds `sudo -E env K='v' … BOTFORGE_ENV='/tmp/botforge-env-…' <interpreter>
    <script>` — the sink path is an env var, the accumulated env is injected
    as an `env` prefix.
  - *Capture*: after the step, the harness runs `cat <remote_env_path>` over
    SSH and parses the result.
- **Host steps** (`plan/vm.rs:540-601`, `run_host_step()`
  `plan/vm.rs:1054-1161`): local temp env file created world-writable, passed
  via `.env("BOTFORGE_ENV", …)`, accumulated env injected via `.envs()`, file
  read back with `read_to_string` after the step.
- **Parsing** — `parse_env_file()` (`plan/vm.rs:1271-1319`): `KEY=VALUE`
  lines plus `KEY<<DELIM` heredoc multiline values (same wire format GitHub
  Actions uses for `$GITHUB_OUTPUT`). Values are **untyped, unquoted, raw
  strings**.
- **Merge** — `env_merge()` (`plan/vm.rs:1323-1331`): last-write-wins into
  the flat accumulated env; merging is **best-effort** (parse failures are
  silently ignored — `plan/vm.rs:563-567`).
- **Output capture for `expect:`** — `run_ssh_step_capturing()`
  (`plan/vm.rs:618-700`) and `run_host_step_capturing()`
  (`plan/vm.rs:707-833`) already capture stdout/stderr into a `StepCapture`
  struct and surface the exit code, checked by `check_expect_block()`
  (`plan/vm.rs:839-871`).
- **Publish prepare steps** — `run_local_steps()` (`plan/vm.rs:1347-1461`)
  reuses the host-step machinery and returns the accumulated env to the
  publish command, which interpolates `${VAR}` into target fields.

Deficiencies of the env channel as a value-return mechanism, confirmed in
code:

- **Untyped**: `parse_env_file` yields raw strings; nothing validates them.
- **Undeclared**: any step may emit any key; nothing checks a key was
  expected, or that an expected key was actually emitted.
- **Unnamespaced**: one flat bag shared by every step in the whole run;
  collisions are silent last-write-wins (`env_merge`).
- **Failure-tolerant to a fault**: a step that fails to emit, or emits
  garbage, is indistinguishable from one that emitted nothing (merge is
  best-effort).
- **No masking**: there is **no secret masking/redaction anywhere** in step
  logging — `StepLogWriter` (`plan/log.rs:79-104`) records raw lines, and
  accumulated env values appear verbatim in the guest SSH command line
  (`build_guest_ssh_cmd`). A secret smuggled through the env bag leaks into
  step logs and (guest) into the process table.

### 1.4 ID handling — display-only today

`RunStep.id` (`step.rs:86-90`) is explicitly documented as:

> Purely a display label today — it is not required to be unique and is not
> addressable by other steps.

- `display_id()` (`step.rs:170-175`) feeds the `(<index>/<id>)` step title;
  archive steps never carry ids.
- **No uniqueness validation exists** — `validate_test_steps()`
  (`config/mod.rs:1375`) and `validate_build_steps()` (`config/mod.rs:1412`)
  check shells/ports/archive rules but never ids.

What breaks the moment ids become addressable under the current inlining
model:

- **Reuse collides.** Including the same fragment twice (or two fragments
  that happen to pick the same id) yields duplicate ids in the flat list.
  With ids addressable, either the loader must reject the collision (making
  fragments non-reusable — absurd) or references become ambiguous.
- **Remote fragments are unfixable.** For a future remote `uses:` scheme, the
  caller neither controls nor sees the fragment's internal ids; global
  uniqueness across the expanded tree cannot be guaranteed by anyone.
- **No stable address for deep values.** In a flat list there is no path
  notation that survives flattening; a value produced three fragments deep in
  a sibling sub-tree has no reachable name.

Both failures share one root cause — **inlining removes scope** — and one fix:
a scope boundary per fragment invocation.

---

## 2. The load-time vs runtime seam

Substitution today is entirely load-time (§1.2); outputs are inherently
runtime — they do not exist until a step has run. The central design decision
is where the resolved-at-load / resolved-after-execution boundary sits.

### Option A — keep everything load-time (rejected)

Pre-run fragments eagerly to compute outputs before expanding callers. Fails
immediately: steps depend on a booted guest, on prior steps' side effects,
and on `if:` conditions; execution order *is* the semantics. Not viable.

### Option B — a third, deferred, post-execution substitution pass (recommended)

Keep the two load-time passes exactly as they are, and add a **runtime
resolution pass** for the new namespaces (`steps.*`, and at call sites the
invocation-output namespace):

- **Load time**: the expression parser already produces
  `Reference { namespace, name }` nodes. Extend deferral so that references
  into the runtime namespaces are *recognized, syntax- and target-validated
  where possible, and deferred past Invariant A* instead of rejected.
  Invariant A (`check_no_residual_expressions`) is **narrowed, not dropped**:
  after load, the only `${{ }}` spans allowed to remain are those in runtime
  namespaces; anything else is still a hard load-time error.
- **Runtime**: in the execution loop (`run_step_flow`), immediately before a
  step runs, run the *same* substitution engine over the step's fields with
  the runtime namespace populated from captured outputs. After this pass,
  Invariant A applies absolutely: any residual `${{ }}` is a hard runtime
  failure.

This is the natural seam because:

- The execution loop is already per-step and already threads mutable state
  (`accumulated_env`); an `outputs` store slots in beside it.
- The `if:` condition is already the one field with special typed handling
  (`deserialize_step_condition`, `step.rs`); runtime resolution of `if:`
  against outputs is what makes conditional flows on computed values work.
- Steps are stored as parsed `TestStep` structs after load; the runtime pass
  operates on the small set of string-bearing fields (`run`, `name`, `if:`
  pre-parse, `with:` values at invocation boundaries) rather than raw YAML.

**Trade-off**: step definitions become late-bound — a type error in a
`${{ steps.x.outputs.y }}` usage may only surface at runtime. Mitigation:
load-time *reference validation* (the target step id exists in scope, the
named output is declared, the declared type is known) catches everything
except the emitted value itself, which is intrinsically runtime.

### Option C — full runtime graph engine (deferred)

Model every step/invocation as a node in a dependency graph resolved lazily,
enabling parallelism from day one. Rejected *for now*: it discards the
simple, well-understood sequential loop and conflates this work with
`matrix:`/parallelism. The proposed model is forward-compatible with it
(§8) — invocations are exactly the nodes such a graph would schedule.

**Recommendation: Option B.** Framed for a human to confirm: accept late
binding of output references (with load-time declaration/reference checks) in
exchange for keeping the loader/runner split intact.

---

## 3. Proposed model: scoped invocation replaces inlining

### 3.1 Invocation as a unit

A `uses:` entry stops being a splice instruction and becomes an **invocation
step** — a single unit in the parent's step list:

```yaml
steps:
  - id: extract
    uses: "@://fragments/jq.steps.yaml"
    with:
      json: ${{ steps.fetch.outputs.body }}
      filter: ".token"
  - name: use it
    run: do-thing --token '${{ steps.extract.outputs.result }}'
```

- The fragment is still loaded, type-checked, and input-validated at load
  time exactly as today (§1.1 steps 2–6). What changes is step 7: instead of
  splicing the fragment's steps into the parent list, the loader produces a
  `TestStep::Invoke` (working name) holding the fragment's *own* step list,
  its resolved typed inputs, and its declared `outputs:` wiring.
- At runtime, the executor runs the invocation's inner steps **in their own
  scope**: a fresh inner `steps.*` output store and a fragment-local env
  accumulation. Only the declared `outputs:` cross back.
- `with:` values may contain runtime references (resolved by the deferred
  pass in the *caller's* scope, immediately before the invocation runs), then
  type-validated against the fragment's `inputs:` declarations exactly as
  today — the existing R1–R4 boundary is unchanged, it just fires at
  invocation time when the value is runtime-computed.

### 3.2 IDs become fragment-local

- `id:` uniqueness is enforced **per scope** (the root document is one scope;
  each fragment body is one scope). This is a new load-time check (none
  exists today, §1.4) — cheap, local, compositional, and remote-safe: a
  fragment's ids only ever need to be unique *within that one file*.
- An invocation step's own `id:` is the caller's **handle**:
  `${{ steps.<invocation-id>.outputs.<name> }}`. Callers can never reference
  a fragment's internal step ids — the expression resolver only looks up ids
  in the current scope. Encapsulation is a property of the lookup, not a lint.
- Step log naming already uses the flat index (`step_log_path`,
  `plan/log.rs:122-127`); under scoping this becomes a hierarchical index
  (e.g. `3.2` = second step of the invocation at parent index 3), which also
  fixes today's log-name ambiguity for free.

### 3.3 What stops being flattened (and what doesn't)

- **Steps**: no longer flattened — owned by their invocation.
- **`files:` / `cloud_init:`**: still accumulated globally at load time as
  today (`config/mod.rs:1018,1034-1039`). These are VM-provisioning concerns
  that genuinely are global; scoping them is out of scope here.

---

## 4. Typed outputs: declaration, emission, capture, coercion

### 4.1 Step-level `outputs:` (the emit side)

Steps declare their own typed outputs (the resolved fork: steps have
first-class typed outputs; fragment outputs are wiring):

```yaml
- id: jq
  name: run filter
  run: |
    value="$(jq -r "$FILTER" <<<"$JSON")"
    echo "value=$value" >> "$BOTFORGE_OUTPUT"
  outputs:
    value:
      type: string          # string | number | boolean; default: string
      required: true        # OR default: <typed value> — exactly one
    attempts:
      type: number
      default: 1
      on_type_error: fail   # fail (default) | empty
```

**Emission mechanism** — a second sink file alongside the env file, reusing
the proven machinery byte for byte:

- Guest: `guest_env_init_cmd`-style init of
  `/tmp/botforge-output-{idx}-{suffix}`; `BOTFORGE_OUTPUT=<path>` added to
  the `env` prefix in `build_guest_ssh_cmd`; `cat` back after the step.
- Host: temp file + `.env("BOTFORGE_OUTPUT", …)` in `run_host_step`;
  `read_to_string` after.
- Wire format: identical to `parse_env_file` (`KEY=value` + heredoc), which
  is also the `$GITHUB_OUTPUT` format.

Why a separate sink rather than reusing `$BOTFORGE_ENV`: the env file is the
*legacy untyped* channel with best-effort merge semantics; outputs need
strict semantics (declared keys only, hard failures). Separating them lets
`$BOTFORGE_ENV` remain backward-compatible while `$BOTFORGE_OUTPUT` is strict
from day one.

### 4.2 Capture-time coercion (the declared boundary)

The wire is strings; the *declared* type coerces/validates at capture. This
is coercion at an explicit declared boundary — consistent with #544's rule —
not the implicit coercion inputs forbid.

At step completion, in the executor (the same place `parse_env_file` +
`env_merge` run today, `plan/vm.rs:459-469` / `563-567`), a strict capture
routine:

1. Parse the output sink with the `parse_env_file` grammar. **Undeclared keys
   in the sink are a hard runtime failure** (unlike the env channel).
2. For each declared output:
   - emitted → coerce the raw string to the declared type
     (`"23"` → `Number(23.0)`, `"true"`/`"false"` → `Bool`; `type: string`
     takes the value verbatim). Parse failure → **hard runtime failure**,
     unless the output declares `on_type_error: empty`, in which case the
     value becomes `Empty` (falsy `""` on interpolation) — leniency is
     per-output and declared, never global.
   - not emitted → if `default:` declared, the default is used and is itself
     type-validated (load-time, since defaults are static); if
     `required: true`, **hard runtime failure** at step completion.
3. Store the result as `EvaluatedValue`s (`config/expressions/value.rs`) in
   the scope's output store, keyed `(step_id, output_name)`.

**Required/default rule** (symmetric with input R1): `required:` defaults to
`false`; every declared output must set `required: true` **or** `default:`,
and **not both** — both or neither is a hard **load-time** error, checked in
the same place input declarations are checked. `required: true` *emission*
enforcement is **runtime** (step completion). Default substitution is
runtime; default *type* validation is load-time.

Declaring `outputs:` requires the step to have an `id:` (load-time error
otherwise) — outputs without an address are unreachable by construction.

### 4.3 Fragment-level `outputs:` (the wiring side)

A fragment's `outputs:` block re-exports inner step outputs:

```yaml
type: botforge/fragment
inputs:
  json:   { type: string, required: true }
  filter: { type: string, required: true }
steps:
  - id: jq
    run: …
    outputs:
      value: { type: string, required: true }
outputs:
  result:
    type: string                              # restated deliberately
    value: ${{ steps.jq.outputs.value }}
    required: true
```

Rules (all locked decisions from the issue):

- `type:` defaults to `string` when omitted — at **both** levels.
- The fragment output's declared type **must equal** the declared type of the
  step output it re-exports. Mismatch is a **hard error at load/wire time**:
  both declarations are static YAML in the same file, so the check needs no
  execution — it runs when the fragment is loaded. This restatement is
  intentionally non-DRY: correctness over DRY; the redundancy is the guard
  that a re-export cannot silently change or lose a type.
- `value:` must be a pure `${{ steps.<id>.outputs.<name> }}` expression over
  the fragment's own scope (first iteration; expressions over multiple
  outputs can come later). Referencing an unknown step id or undeclared
  output name is a load-time error.
- `required:`/`default:` follow the same R1-symmetric rule as step outputs,
  checked at load time; `required: true` enforcement (the wired step output
  actually resolved to a value) is runtime, at invocation completion.

At invocation completion the executor evaluates the wiring against the inner
scope's output store, applies defaults/required checks, and publishes the
results into the **caller's** scope under the invocation's id.

### 4.4 Namespace and deferred resolution

- `${{ steps.<id>.outputs.<name> }}` — one uniform namespace whether `<id>`
  names a plain step or an invocation; the caller cannot tell (nor should it)
  whether a handle is backed by a raw step or a whole sub-tree.
- The parser needs a small extension: references are currently two-segment
  `namespace.name` (`config/expressions/parser.rs`); `steps.*.outputs.*` is a
  four-segment path. `EvaluatedValue` needs no changes.
- Resolution timing: `steps.*` is a **deferred** namespace at load time
  (extending `is_deferred_namespace`, `config/expressions/eval.rs:224-230`)
  and is resolved by the runtime pass in the executor immediately before each
  step/invocation runs (§2 Option B). Forward references
  (`steps.later.outputs.x` in an earlier step) are a load-time error —
  sequential order makes "already executed" statically checkable.
- Load-time reference validation: within a scope, the loader knows every step
  id and every declared output name + type, so dangling references and (in a
  later stage) type-misuse in typed positions are load-time errors; only the
  emitted *values* are late-bound.

---

## 5. Secrets handling

Current state (§1.3): no masking exists anywhere; env values are visible in
step logs and in the guest SSH command line. Any output/input mechanism that
carries OTPs or tokens must not inherit this.

Proposed, in scope for the outputs work:

- **`secret: true` on declarations** (step outputs, fragment outputs, and
  fragment inputs). A secret value is:
  - registered with a run-wide masker the moment it is captured/resolved;
  - replaced with `***` in step titles, `StepLogWriter` lines
    (`plan/log.rs:79-104`), captured `expect:` output echoes, and error
    messages;
  - excluded from interpolation into *logged* command previews.
- **Transport**: guest-bound secret inputs must not ride the
  `env K='v'` prefix of `build_guest_ssh_cmd` (visible in the guest process
  table). Minimum viable fix: write secret-bearing vars into the (0600, not
  0666) remote script or a companion env file sourced by the wrapper, rather
  than the command line. The sink files for secret outputs likewise need
  0600 + owner-only semantics instead of the current world-writable init.
- **Propagation**: secret-ness is sticky across wiring — a fragment output
  wired from a secret step output is implicitly secret; declaring it
  non-secret is a load-time error (same spirit as the type-match rule).

Masking is inherently best-effort (a step can always `echo` a secret in
transformed form), matching the GH Actions posture; the goal is to close the
*mechanical* leaks the harness itself creates.

---

## 6. Fragments as functions: the `jq.steps.yaml` case end to end

With §3–§4 in place, the motivating example works with no env smuggling:

```yaml
# fragments/jq.steps.yaml
type: botforge/fragment
inputs:
  json:   { type: string, required: true }
  filter: { type: string, required: true }
steps:
  - id: jq
    name: apply filter
    on: host
    run: |
      printf '%s' "$INPUT_JSON" | jq -r "$INPUT_FILTER" \
        | { printf 'value<<__EOF__\n'; cat; printf '__EOF__\n'; } >> "$BOTFORGE_OUTPUT"
    outputs:
      value: { type: string, required: true }
outputs:
  result:
    type: string
    required: true
    value: ${{ steps.jq.outputs.value }}
```

Caller:

```yaml
steps:
  - id: parse
    uses: "@://fragments/jq.steps.yaml"
    with: { json: "${{ steps.fetch.outputs.body }}", filter: ".version" }
  - name: assert version
    run: test "${{ steps.parse.outputs.result }}" = "1.2.3"
```

Function-call anatomy: typed args in (`with:` → R1–R4 validation), isolated
execution (own scope, own ids, own output store), typed return out (emit →
capture → coerce → wire → publish). Reusable N times in one file (ids are
scope-local), composable (fragments invoke fragments; each level surfaces
only its declared contract), and remote-safe (internal ids never escape).

One open input-side question is flagged in §9: how fragment *steps* best
consume inputs at runtime (the `INPUT_*` convention above vs. keeping
load-time `${{ inputs.* }}` textual substitution, which works unchanged).

---

## 7. Ordering and dependency

- Execution stays **strictly sequential** in this design: the flat loop of
  `run_step_flow` becomes a recursive walk (scope → steps → invocation →
  inner scope), still one step at a time. `steps.*` references are therefore
  always backward references — statically checkable (§4.4).
- The dependency structure this creates is exactly the one a future scheduler
  needs: an invocation is a node whose inputs are `with:` references and
  whose outputs are its contract. **A caller waits on a fragment's contract,
  never on buried steps.** `matrix:` on an invocation later means "N inner
  scopes with indexed handles" (`steps.parse[0].outputs.result` or an
  aggregate), and parallelism means running independent nodes concurrently —
  both attach at the invocation boundary without touching step internals.
  Neither is in scope now; the boundary is what this work must get right.

---

## 8. Risks and open questions

Risks:

1. **Invariant A relaxation** is the highest-risk change: today "no residual
   `${{ }}` after load" is a strong safety net. It must be narrowed
   *precisely* (only recognized runtime namespaces defer; everything else
   still fails at load) or typo'd namespaces will silently survive to
   runtime. Needs dedicated negative tests.
2. **`TestStep` becomes recursive** (`Invoke` holding a `Vec<TestStep>`);
   everything that pattern-matches `TestStep` (validators in
   `config/mod.rs:1375/1412`, executor, publish's archive rejection,
   logging) must handle the new variant — a broad but mechanical audit.
3. **Log/index scheme changes** (flat `step-{idx}` → hierarchical) touch log
   consumers and the acceptance-test fixtures under `botforge/test/`.
4. **Backward compatibility**: existing configs use inlined fragments with
   the flat env bag today. The env channel keeps working unchanged; but the
   switch from splice to scope changes step *count/indexing* observable in
   logs, and any config relying on env leaking *out of* a fragment into
   later parent steps changes behavior. Needs an explicit call: migrate
   silently (env still crosses the boundary for compatibility?) or make the
   scope boundary also an env boundary (cleaner; recommended, with declared
   outputs as the only crossing).
5. **Secret masking is cross-cutting** (logs, SSH command lines, expect
   echoes) and easy to leave holes in; treat as its own reviewed stage.

Open questions (flagged for humans):

- Should invocations require an `id:` always, or only when their outputs are
  referenced? (Recommend: required whenever the fragment declares outputs.)
- Runtime input consumption inside fragments: keep load-time `${{ inputs.* }}`
  textual substitution (works today, but runtime-computed `with:` values then
  need a second substitution point) vs. inject `INPUT_<NAME>` env vars at
  invocation time (uniform, but changes fragment authoring). The former needs
  the runtime pass to also handle `inputs.*` inside invocation scopes.
- Does the scope boundary also gate `$BOTFORGE_ENV` accumulation (risk 4)?
- Cross-scope `files:`/`cloud_init:` remain global (§3.3) — acceptable?
- Number representation: `EvaluatedValue::Number(f64)` — is f64 acceptable
  for typed outputs (large integers), or does this force an int/float split?

---

## 9. Staged implementation plan

Sequenced so the scope/namespace split lands **before** typed `outputs:`
exists; each stage is independently shippable and testable. Follow-up issues
to be created one per stage.

**Stage 1 — Per-fragment scope + local ids (no outputs yet).**
Introduce the invocation step (`TestStep::Invoke`): loader stops splicing and
produces the recursive structure; executor recurses with hierarchical
indices; `id:` uniqueness enforced per scope at load time (new validation —
none exists today). No expression changes; `inputs`/`args` substitution
unchanged. *Risky bits*: `TestStep` recursion audit (risk 2), log indexing
(risk 3), env-boundary decision (risk 4). Deliberately mechanical otherwise —
this is the enabling refactor.

**Stage 2 — Typed step `outputs:` + capture/coercion.**
`outputs:` declarations on run steps (type default `string`;
required-XOR-default checked at load; `id:` mandatory with outputs);
`$BOTFORGE_OUTPUT` sink init/advertise/fetch on guest and host paths
(mirroring the env-file machinery, but strict + 0600 for `secret: true`);
capture-time coercion with hard-fail default and per-output
`on_type_error: empty`; `required: true` emission check at step completion;
per-scope output store. Not yet referenceable — value is validated storage +
failure semantics, testable via unit tests on the capture routine.
*Risky bits*: guest sink permission/ownership under `sudo`.

**Stage 3 — Fragment `outputs:` wiring + type-match enforcement.**
Fragment-level `outputs:` block (type default `string`; required-XOR-default;
`value:` restricted to pure `steps.*.outputs.*` over the fragment scope);
**load-time step↔fragment type-match check** (both declarations are static —
mismatch fails at fragment load); runtime evaluation of wiring at invocation
completion, publishing into the caller's scope under the invocation id.
Secret stickiness across wiring. *Risky bits*: none structural — this stage
is mostly declaration plumbing over stages 1–2.

**Stage 4 — `steps.*`/`outputs.*` engine namespace + deferred runtime pass.**
Parser extension for four-segment `steps.<id>.outputs.<name>` paths; extend
deferral (`is_deferred_namespace`) so `steps.*` survives both load-time
passes; narrow Invariant A to permit only recognized runtime namespaces;
load-time reference validation (backward-only, id exists, output declared);
the runtime substitution pass in the executor before each step/invocation
(including `if:` truthiness and `with:` values, re-running R1–R4 input
validation on runtime-computed `with:`). Secret masking of resolved values in
logs/titles/SSH command lines lands here (or as a parallel 4b stage).
*Risky bits*: Invariant A narrowing (risk 1) and masking coverage (risk 5) —
both need dedicated negative-test suites.

After stage 4 the `jq.steps.yaml` scenario (§6) works end to end;
`matrix:`/parallelism attach later at the invocation boundary (§7) as
separate work.
