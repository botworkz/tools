# viscous

> **vis·cous** (adj.) — thick, sticky, slow-pouring. Like a template that takes
> its time to deposit a well-formed crate exactly where you asked for it.

`viscous` is an **opinionated, agent-friendly directory template generator**.
It reads a directory containing a `__template__.yaml` spec and writes a
rendered copy into a destination directory, with declarative `for_each`,
`when`, and per-step conflict resolution.

## Why another templating tool?

Existing ecosystem tools — `cookiecutter`, `cargo-generate`, `copier`,
`hygen` — all assume a human at a terminal answering interactive prompts.
viscous is the inverse: vars come in as JSON, missing required ones fail
loudly with a list of what's needed, and `viscous plan` returns a
fully-introspectable manifest of every file it would emit (and why) without
touching the filesystem.

The thing it deliberately is *not*: a registry, an installer, a "template
catalogue". A template is a directory on disk. Where it came from
(`git clone`, hand-rolled, scp'd, vendored) is the user's problem. viscous
only knows how to render one.

## At a glance

```sh
# What does this template want?
viscous describe ./my-template

# What would it produce? (no I/O)
viscous plan ./my-template ./out --vars vars.yaml

# Do it.
viscous generate ./my-template ./out --vars vars.yaml
```

## Template layout

A template is any directory containing `__template__.yaml`. Files live in
one of two trees:

```text
my-template/
├── __template__.yaml          # spec: vars schema, generators, options
├── __templates__/             # generator templates (invoked declaratively)
│   ├── component.rs.liquid
│   └── tailwind.config.js.liquid
├── Cargo.toml                 # static tree from here down
└── src/
    ├── lib.rs
    └── routes/{{ route_dir }}/mod.rs   # filename templating works in static tree too
```

- **Static tree** — every file outside `__templates__/` is walked
  alphabetically, has its filename and body rendered through liquid, and is
  emitted 1:1. Static is the implicit step 0.
- **Generator tree** — files under `__templates__/` are only emitted when
  referenced by an entry in `generate:`. Generators support fan-out
  (`for_each`), conditional emission (`when`), computed dest paths, and
  per-entry conflict policy.

Both trees feed one [`Plan`]; conflicts between them are resolved by the
generator's `on_conflict` flag.

## `__template__.yaml`

```yaml
name: leptos-webview
description: Minimal CSR Leptos app for webview embedding
version: 0.1.0

vars:
  project_name:
    type: string
    required: true

  routes:
    type: array
    items: { type: string }
    default: ["/"]

  components:
    type: array
    items:
      type: object
      schema:
        name:  { type: string, required: true }
        props: { type: array, items: { type: string }, default: [] }
    default: []

  use_tailwind:
    type: bool
    default: false

derived:
  crate_name: "{{ project_name | snake_case }}"

generate:
  - template: __templates__/component.rs.liquid
    for_each: components
    as: component
    dest: "src/components/{{ component.name | snake_case }}.rs"

  - template: __templates__/route.rs.liquid
    for_each: routes
    dest: "src/routes/route{{ item | replace: '/', '_' }}.rs"

  - template: __templates__/tailwind.config.js.liquid
    when: use_tailwind
    dest: tailwind.config.js
```

### Var types

| Type     | JSON shape           | Notes                                                 |
| -------- | -------------------- | ----------------------------------------------------- |
| `string` | `"…"`                |                                                       |
| `bool`   | `true` / `false`     |                                                       |
| `int`    | integer number       |                                                       |
| `float`  | any number           |                                                       |
| `array`  | JSON array           | `items:` declares the per-element schema              |
| `object` | JSON object          | `schema:` declares per-field schemas                  |

Defaults are applied for missing optional vars. Missing required vars are
hard errors. Extra keys in input that aren't in the spec are silently
dropped — the spec is the contract.

### Derived vars

`derived:` lets you compute extra vars in liquid against the resolved scope.
Each derived var is added to the scope before any file is rendered, so
templates can use `{{ crate_name }}` without re-deriving it everywhere.

### Generator fields

| Field         | Required | Purpose                                                                     |
| ------------- | -------- | --------------------------------------------------------------------------- |
| `template`    | yes      | Path to the liquid template (relative to template root).                    |
| `dest`        | yes      | Where to write it; liquid-rendered against the current scope.               |
| `for_each`    | no       | Array var to iterate; one output file per item.                             |
| `as`          | no       | Loop variable name (default `item`).                                        |
| `when`        | no       | Liquid boolean expression; step is skipped when falsy.                      |
| `on_conflict` | no       | `error` (default) / `overwrite` / `skip` / `append` / `upsert`.             |

### Conflict modes

`on_conflict` controls what happens when an earlier step already wrote to
the same dest (including the implicit static-tree step 0).

| Mode        | If no earlier file       | If earlier file exists                               |
| ----------- | ------------------------ | ---------------------------------------------------- |
| `error`     | create                   | hard error — surfaces accidental clobbers (default) |
| `overwrite` | hard error               | replace the earlier file's contents                  |
| `skip`      | create                   | no-op                                                |
| `append`    | hard error               | append `\n` + new content to the earlier file        |
| `upsert`    | create                   | replace the earlier file's contents                  |

The `overwrite` and `append` modes deliberately error when there's nothing
to override — that catches the rename-drift bug where you intended to patch
a file the upstream template no longer emits. Use `upsert` when "create or
replace" is genuinely what you want.

A **within-generator** dest collision (two items of the same `for_each`
resolving to the same path) is always a hard error, regardless of
`on_conflict`.

## Execution order

Strict and documented, so plans are reproducible:

1. Vars are resolved against the spec; defaults applied; derived vars
   computed (in key order) and merged into scope.
2. The static tree is walked depth-first, alphabetically at each level.
   Every file produces a step-0 op.
3. `generate:` entries are processed in yaml list order; within each
   `for_each`, items are processed in array order.

## Filters

Liquid stdlib, plus a small case-conversion library:

| Filter              | Example input → output                |
| ------------------- | ------------------------------------- |
| `snake_case`        | `HelloWorld` → `hello_world`          |
| `kebab_case`        | `HelloWorld` → `hello-world`          |
| `pascal_case`       | `hello_world` → `HelloWorld`          |
| `camel_case`        | `hello_world` → `helloWorld`          |
| `shouty_snake_case` | `helloWorld` → `HELLO_WORLD`          |
| `title_case`        | `hello_world` → `Hello World`         |

## CLI

```sh
viscous describe TEMPLATE
viscous plan     TEMPLATE DEST [--vars FILE] [--set k=v]... [--with-bodies]
viscous generate TEMPLATE DEST [--vars FILE] [--set k=v]... [--policy MODE]
```

- `--vars FILE` reads JSON or YAML (extension-detected).
- `--set key=value` overrides individual vars; values are parsed as JSON
  first, falling back to plain strings (so `--set use_tailwind=true`
  becomes the bool `true`).
- `--policy` controls how the **destination directory's existing contents**
  are treated, not the inter-step conflict semantics:
  - `require-empty` (default) — refuse to write into a populated dest.
  - `merge` — write into the dest, but error if any individual file already
    exists on disk.
  - `overwrite` — clobber colliding on-disk files freely.

## Library

```rust
use viscous::{build_plan, apply, DestPolicy, Spec};

let spec = Spec::load_from_dir("./tpl")?;
let plan = build_plan("./tpl", &spec, &vars_json, std::path::Path::new("./out"))?;

// dry-run: introspect plan.ops without touching disk
for op in &plan.ops {
    println!("{:?} {} ({}B)", op.action, op.dest.display(), op.size);
}

// commit
apply(&plan, DestPolicy::RequireEmpty)?;
```

The library is pure: `build_plan` returns a [`Plan`] containing the bytes
it intends to write, but never touches the filesystem. Only `apply` does.

## Tests as fixtures

End-to-end behaviour is verified by `tests/fixtures/<scenario>/` directories,
each containing:

```text
template/   # input template (with __template__.yaml)
vars.yaml   # input vars
expected/   # snapshot of the destination tree after `generate`
```

The runner walks both trees and compares them byte-for-byte. Adding a new
scenario is just adding a directory.

## What viscous is not

- A template **registry**. Templates are directories on disk; how they got
  there is none of viscous's business.
- A template **catalogue manager**. There's no `list`, no `install`, no
  fetcher. Use `git clone`, `tar`, `scp`, whatever.
- A **runtime** with hooks or plugins. Liquid + a fixed filter library is
  the whole extension surface. If you need pre/post hooks, that's the
  caller's job.
- **Interactive**. Vars come in as JSON; missing-required is a structured
  error, not a prompt.
