# botforge

`botforge` is the build-time companion CLI for botworkz VM artifact workflows. It wraps the external toolchain (QEMU/KVM, xorriso, OpenSSH) so the same commands work identically inside the published container image and in local development.

## Container image

**Published image:** `ghcr.io/botworkz/tools/botforge`

```sh
docker pull ghcr.io/botworkz/tools/botforge:latest
```

botforge is distributed as a **batteries-included** image: the QEMU toolchain
(`qemu-system-x86_64`, `qemu-img`), ISO utilities (`xorriso`, `genisoimage`), and the OpenSSH
client are baked in so subcommands and image-build flows work reproducibly
without installing exact tool versions on your host.

### Runtime requirements

botforge drives KVM-capable workflows for `build`, `test` and `run`, so for those
subcommands the container needs device access plus a mount of your working
repository:

```sh
docker run --rm \
  --device /dev/kvm \
  -v "$PWD:/work" -w /work \
  ghcr.io/botworkz/tools/botforge:latest \
  --help
```

- `--device /dev/kvm` — required for `build`, `run` and `test` (KVM-only; no TCG fallback).
- `-v "$PWD:/work" -w /work` — mount your repo so botforge can read manifests,
  write artifacts, and resolve project-relative paths.

### Build the image locally

From the repo root, with [EarthBuild](https://github.com/EarthBuild/earthbuild):

```sh
earthly +botforge-image
```

This produces the stable local tag `botwork/botforge:local`.

## Commands

The `--config / -c` flag (default `shasset.yaml`) is global; it points `deps`
at the shasset manifest, `payload` at the payload config, and `build` at the
shasset manifest used to resolve `image:` assets.

| Command | Summary |
|---|---|
| `botforge build --spec <file> --output <qcow2> [--source <qcow2>] [--cache-dir <dir>] [--repo-root <dir>]` | Resolve `image:` from the shasset manifest (`--config`), fetch + verify + cache the qcow2, boot it under qemu, inject an ephemeral in-harness SSH keypair via cloud-init, run `type: build` plan steps, and commit the result on clean shutdown. `--source` is an optional local override that bypasses shasset resolution. |
| `botforge deps --out <dir> [name ...]` | Fetch + stage shasset assets into a flat output directory. |
| `botforge iso --src <dir> --out <file> [--volume-id <id>]` | Build an ISO image from a source tree. Also supports generating a cidata seed ISO with an injected SSH key. |
| `botforge payload --out <file>` | Build a payload ISO from a config-driven staging plan. |
| `botforge run …` | Launch a VM with qemu (KVM-only). |
| `botforge test …` | Boot a packed qcow2 with a cloud-init cidata seed, SSH in, and execute the steps in a `test-packed.yaml` plan. |

## Ephemeral installer identity

`botforge build` and `botforge test` provision the guest over SSH as a
**botforge-owned ephemeral installer account** rather than assuming any
particular user exists in the base image.

### How it works

1. At seed time botforge generates a per-run username of the form
   `botforge-<20-hex-chars>` (80 bits of entropy from `/dev/urandom`; unique
   per run, never reused).
2. A `#cloud-config` `users:` entry for this account is injected into the
   cidata seed ISO:
   - `sudo: 'ALL=(ALL) NOPASSWD:ALL'` — the installer must be able to `sudo`
     non-interactively to run `cloud-init status --wait`, provisioner scripts,
     and the final teardown.
   - `ssh_authorized_keys:` — the harness's ephemeral ed25519 public key.
   - `lock_passwd: true`, `shell: /bin/bash` — key-only access, login shell.
   - `- default` is preserved (harmless; keeps the base image's own default
     user).
3. All provisioning steps (`sudo cloud-init status --wait`, `sudo bash
   <provisioner>`, etc.) run as this installer.
4. **On the success path** (build only), botforge queues a final root-owned
   transient systemd service over the same SSH connection immediately before
   power-off:
   ```
   sudo systemd-run --quiet --unit botforge-installer-teardown-<installer> --collect \
     /bin/bash -lc 'set -euo pipefail;
       sleep 2;
       loginctl terminate-user <installer> >/dev/null 2>&1 || true;
       while pgrep -u <installer> >/dev/null 2>&1; do sleep 0.2; done;
       userdel -f <installer>;
       rm -rf /home/<installer>;
       rm -f /etc/sudoers.d/90-cloud-init-users;
       systemctl poweroff'
   ```
   The detached service waits for the SSH caller to return, terminates any
   remaining installer processes, removes the installer, and only then powers
   off. If that cleanup cannot complete, the build is treated as a hard error
   so no committed image can ship with the installer account present. The test
   overlay is discarded on exit, so teardown is not required there.
5. Any **shipped runner account** (e.g. a `bot` runner created by a
   provisioner such as `10-bot-user.sh`) is entirely the consuming repo's
   responsibility — botforge neither assumes nor names such accounts.

### `--ssh-user` override

`--ssh-user <name>` (on both `build` and `test`) opts out of the ephemeral
installer:

- botforge connects as the supplied user and does **not** create or delete it.
- The caller is responsible for ensuring the user exists in the base image.
- For `build`: the ephemeral public key is injected via the top-level
  `ssh_authorized_keys` in cloud-init (consumed by the default cloud-init
  user); the override works best when the supplied user IS the default
  cloud-init user.
- For `test`: pair `--ssh-user` with `--ssh-key`; the specified user must
  already have the corresponding public key in its `authorized_keys` (e.g.
  from the build that produced the image). Providing one without the other
  is a runtime error.


For `botforge test`, the test config is a YAML document with a required
`type: test` field at the top level.  Two document kinds exist:

- **`type: test`** — an entrypoint document, consumed directly by
  `botforge test`.  May carry `isos:`, `ports:`, `diagnostics_units:`, and
  `steps:`.
- **`type: fragment`** — a reusable document spliced in via `uses:`.  May
  carry `steps:` (and an `inputs:` contract) only; declaring `isos:`, `ports:`,
  or `diagnostics_units:` on a fragment is a load-time error.  The same
  fragment file is reusable from any entrypoint kind, including `type: build`.

`botforge test` requires a `type: test` document as its top-level plan; passing
any other `type:` value (including `type: fragment`) is a hard load-time error.
A `uses:` reference must point at a `type: fragment` document; pointing it at
any entrypoint document (`type: test` or `type: build`) is also a load-time
error.

The `isos:` list in test config supports two forms:

- A bare string path (attach only).
- A mapping with `path:`, `label:`, `mount:`, and optional `bootstrap:`
  (default `bootstrap.sh`) to attach the ISO, mount it by label in the guest,
  and run the bootstrap script with `sudo` before configured `steps:`.

`botforge test` also supports an optional `ports:` list for additional guest
TCP forwards to the harness. Each entry is either a bare integer guest port
(bound on `127.0.0.1`, reachable only inside the botforge container) or an
`"<addr>:<port>"` string that sets the bind address explicitly (e.g.
`"0.0.0.0:80"` to make the port reachable by sibling containers on the same
compose network). The guest port always equals the host port — no host:guest
remapping. External port remapping is a compose-layer concern. Guest SSH on
`:22` is always forwarded automatically via `--ssh-port`, bound to `0.0.0.0`
on the botforge container so it is reachable from the host and sibling
containers on the compose network.

```yaml
type: test
ports:
  - 80              # bind 127.0.0.1:80 -> guest :80  (loopback only)
  - "0.0.0.0:9901"  # bind 0.0.0.0:9901 -> guest :9901 (all interfaces)
uploads:
  - src: fixtures/envoy/**/*.yaml
    dest: /tmp/test-staging/envoy/
steps:
  - name: check-edge
    run: curl -fsS http://127.0.0.1/
```

#### Top-level `uploads:` (optional)

Both `type: test` and `type: build` support an optional top-level `uploads:`
list that stages files into the guest **once, after cloud-init is ready and
before the first `steps:` entry runs**.

Each entry supports the following fields:

- `src` — **required** repo-relative path or glob, resolved under `--repo-root`.
  `*`, `**`, `?`, and `[...]` are supported. `@<shasset>` and `@://...` are
  **not** supported here; use an `archive:` step for
  shasset assets.
- `dest` — **required** absolute guest path. For glob `src`, `dest` must end in
  `/` and is treated as a guest base directory.
- `mode` — optional; 3–4 octal digits (e.g. `"0755"`). Defaults to `"0644"`.
  Applied via `install -m <mode>` for every matched file.
- `owner` — optional; user name or numeric uid (e.g. `root`). Defaults to `root`.
  Applied via `install -o <owner>`.
- `group` — optional; group name or numeric gid (e.g. `root`). Defaults to `root`.
  Applied via `install -g <group>`.
- `overwrite` — optional boolean. Defaults to `true`. When `false`, the upload
  fails with a hard error if `dest` already exists in the guest.
- `parents` — optional boolean. Defaults to `true`. When `true`, intermediate
  destination directories are created automatically (`install -D`). When `false`,
  the parent directory must already exist.

Semantics:

- Literal `src` behaves like a single-file upload: `dest` is the final file
  path, or `dest/<basename>` when `dest` ends in `/`.
- Glob `src` preserves paths relative to the pattern's fixed literal prefix.
  Example: `images/botspace/envoy/**/*.yaml` staged to `/tmp/envoy/` places
  `images/botspace/envoy/ecds/ext_authz.yaml` at
  `/tmp/envoy/ecds/ext_authz.yaml`.
- Globs that match zero files are a hard error.
- Only regular files are staged; directories matched by a glob are skipped.
- When `src` is a glob, `mode`/`owner`/`group`/`overwrite`/`parents` apply to
  **every** matched file.

### `botforge test` step model

Each entry in `steps:` has a required `on:` field that selects where it runs:

- **`on: guest`** — runs inside the VM via SSH. `run:` executes inside the
  guest. This is the traditional test step.
- **`on: host`** — runs locally in the **botforge container / harness** (where
  botforge itself executes), _not_ inside the guest. Reaches the guest only
  via ports declared in `ports:`. Inherits the harness environment (so CI
  variables such as `GH_TOKEN` are visible).

Steps execute in the exact order written; guest and host steps may interleave
freely. This lets you flip guest state, hit the guest from outside, then
restore — all in a single ordered sequence.

#### Timeout tiers

The shared VM runtime now has three timeout layers:

| Tier | Field | `type: test` default | `type: build` default |
|---|---|---:|---:|
| per-step | `steps[].timeout` | unset | unset |
| document step default | `step_timeout` | 300 s | 1800 s |
| overall wall-clock budget | `timeout` | 1800 s | 7200 s |
| cloud-init wait | per-kind default | 300 s | 600 s |

- `steps[].timeout` is optional on both `on: guest` and `on: host` steps and
  wins when set.
- `step_timeout` applies to any step that does not set its own `timeout`.
- `timeout` is a wall-clock budget for the full flow: boot/SSH waits,
  `cloud-init status --wait`, stable-SSH checks, all steps, and graceful
  shutdown for `botforge build`.
- Fragment documents may set per-step `timeout:` values inside `steps:`, but
  top-level `step_timeout:` and `timeout:` remain entrypoint-only.
- All timeout values are integer seconds; `0` and negative values are rejected
  at config load.

#### Reusable step fragments with `uses:`

`botforge test` can splice a reusable list of steps from another YAML file in
the same repository:

- `uses: "@://path/within/repo.yaml"` resolves from the explicit
  `--repo-root` passed to `botforge test`.
- The referenced file must be a `type: fragment` document (see below). Any
  other entrypoint `type:` value — including `type: build` — is rejected as a
  non-consumable include target.
- The fragment declares its **input contract** in a top-level `inputs:` block
  (see below). The caller passes values via `with:` at the call site.
- `${{ inputs.NAME }}` placeholders in the fragment body are substituted with
  resolved input values before validation.
- Runtime `${VAR}` expansion is unchanged; only `${{ ... }}` is handled at load
  time.

```yaml
# test.yaml
type: test
steps:
  - uses: "@://smoke/vm-narrative.steps.yaml"
    with:
      target: ingress
      shell: bash
```

```yaml
# smoke/vm-narrative.steps.yaml
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
    run: |
      echo "${USER}"   # runtime env expansion, unchanged
      ./smoke-${{ inputs.target }}.sh
```

##### Fragment `inputs:` declaration

Each declared input supports:

- **`type`** — `string`, `number`, or `boolean`. Required.
- **`required`** — boolean, default `false`. When `true`, the resolved value
  must not be absent. An empty string `""` satisfies `required`.
- **`default`** — the value used when the caller omits the input or passes the
  `__default__` sentinel. May not be combined with `required: true`.

Every undeclared input has an implicit absent default (`unset`). `required:
true` means the resolved value must not be `unset`.

**`__default__` sentinel:** a caller's `with:` value of `__default__` resolves
to the declared default (or absent if none is declared), the same as omitting
the key entirely. This lets a computed expression fall back to the declared
default:

```yaml
with:
  target: ${{ steps.cond.outputs.value == 'go' && 'CUSTOM' || '__default__' }}
```

A computed expression cannot omit a key; `__default__` is the way to express
"custom OR the declared default." To opt out of the default and force an empty
string instead, emit `""` from the expression.

For this first iteration, `@://` is the only supported `uses:` scheme. Plain
filesystem paths, `../` traversal, and other schemes are rejected.

Config errors are reported at load time for: missing or invalid `on:`; any `on: host` step present when `ports:` is empty;
invalid `shell:` value; invalid `uses:` scheme/path; `with:` key not declared
by the fragment (`unexpected input '<name>' not declared by fragment <path>`);
missing required input (`missing required input '<name>'`); type mismatch
(`input '<name>' must be a <type>`); `required: true` combined with `default`
in a declaration (`input '<name>' cannot set both 'required: true' and
'default'`); fragment missing a `steps:` list; missing `type:` field on a
document (`<path> is missing required 'type:' field`); unknown `type:` value;
document kind mismatch on the root (`botforge test requires a 'type: test'
document, got 'type: <x>'`); `uses:` pointing at a non-fragment document
(`<uses> is not a consumable fragment (type: <x>)`); entrypoint-only section
(`ports:`, `isos:`, or `diagnostics_units:`) declared in a `type: fragment`
document (`<section>: is not valid in a 'type: fragment' document`); cyclic
`uses:` chain (`cyclic test step include detected: <chain>`); `uses:` nesting
exceeding the maximum include depth (`test step include depth limit (32)
exceeded: <chain>`).

On **any** step failure (guest or host) the usual guest diagnostics are
collected (`systemctl --failed`, `journalctl`, `cloud-init status`, VM log
tail) — a harness-side failure typically implicates a guest service.

Each step also writes a JSONL transcript to
`build/logs/step-<index>-<name>.log`, with one record per stdout/stderr line.
Console output still streams live unchanged while the log captures the stream
(`stdout` or `stderr`) and timestamp for each recorded line.

Before each step runs, a **bold title line** is printed to stderr:
`🤖 (<n>) <name>` (the name is bold when stderr is a TTY and `NO_COLOR` is
unset; plain otherwise). After the step completes, a **colored status marker**
replaces the old `step N ok/failed` line: green `✓ (<n>) <name>` with a
dimmed name on success, or red `✗ (<n>) <name>` on failure. The
`🤖`/`✓`/`✗` glyphs always print regardless of color support. `scp` progress
output is suppressed by default and restored under `BOTFORGE_DEBUG=1` /
`BOTFORGE_SSH_VERBOSE=1`.

In addition to the per-step numbered titles, `botforge build` emits several
**lifecycle phase lines** that frame the build lifecycle phases:

- `🤖 (setup) Preparing build environment (seed image)` — printed before the
  cloud-init cidata seed ISO is built (this ISO injects the ephemeral installer
  user and SSH key).  The raw `xorriso`/`genisoimage` banner is captured and
  suppressed on success; it is only surfaced in the error if the ISO build
  fails.  Set `BOTFORGE_DEBUG=1` to pass the raw tool output through to the
  console.
- `🤖 (compress) Compressing image (reclaim, sparsify, compression)` — printed
  before the reclaim (`fstrim`/`discard`), zero-cluster sparsify, and/or qcow2
  compression work.  Only emitted when at least one of those steps actually
  runs (i.e. `reclaim != none` or `compress.enabled`).
- `🤖 (output) Final image written to <path> (<size>)` — replaces the old
  `built image at …` line and reports the output path and human-readable disk
  size (KiB/MiB/GiB).  Set `BOTFORGE_DEBUG=1` to also print the detailed
  `qcow2 zero-cluster sparsify: …` and `final image stats: …` diagnostic lines.

#### `id:` field (optional)

Every run step has an optional `id:` field. When set, the step counter in
title and status lines is rendered as `(<n>/<id>)` instead of `(<n>)`. For
example:

```yaml
steps:
  - on: guest
    name: flip-spigot
    id: flip-spigot
    run: systemctl reload botwork-envoy
```

produces `🤖 (0/flip-spigot) flip-spigot` instead of `🤖 (0) flip-spigot`.

`id:` is a **display label only** — it is not required to be unique, is not
validated for charset, and is not addressable by other steps. When absent,
output is identical to the no-id form. Archive steps have no `id:` field.

#### `shell:` field (optional)

Every step has an optional `shell:` field that selects the interpreter used to
execute `run:`, mirroring GitHub Actions `shell:` semantics. `run:` is always
written to a temporary script file and executed by the interpreter — never
passed as a raw string to a shell.

**Named shells** (exact match):

| `shell:` value | Invocation template |
|---|---|
| _(absent)_ or `bash` | `bash --noprofile --norc -e -o pipefail {0}` |
| `sh` | `sh -e {0}` |
| `python` | `python3 {0}` |

`{0}` is replaced at runtime with the path to the temp script file.

**Custom template**: any string containing `{0}`, e.g. `python3 -u {0}` or
`perl {0}`. The string is split on whitespace; `{0}` may appear anywhere.

```yaml
steps:
  - on: guest
    name: py-check
    shell: python
    run: |
      import sys
      assert sys.version_info >= (3, 9)
  - on: host
    name: custom-interp
    shell: python3 -u {0}
    run: |
      import os, sys
      print("token present:", bool(os.environ.get("GH_TOKEN")))
```

#### `sudo:` field (optional)

Guest `run:` steps also support an optional `sudo:` boolean. When set to `true`,
botforge runs the resolved interpreter itself under `sudo -E`, so the entire
`run:` body executes as root in one shot. When absent, it defaults to `false`.

- only valid on `on: guest` steps
- rejected at config load time on `on: host` steps
- requires the guest SSH user to have passwordless sudo (the ephemeral
  installer user already does)

```yaml
steps:
  - on: guest
    name: flip-spigot
    sudo: true
    run: |
      cp /etc/envoy/rds/active.ingress.yaml /etc/envoy/rds/active.yaml
      systemctl reload botwork-envoy
```

**Default (`bash`) applies `-e -o pipefail` everywhere.**
When `shell:` is absent the default bash template is used on both `on: guest`
and `on: host` steps. This means a failing command anywhere in a multi-line
`run:` block fails the step immediately — including the left-hand side of a
pipe. Bash must be present in the guest image and the botforge container (both
currently are). If bash is unavailable the explicit `shell: sh` fallback
(`sh -e {0}`) can be set.

> **Behaviour change (guest steps):** prior to this change, guest `run:` strings
> were executed as raw SSH commands with no `set -e` or `pipefail`. They now run
> under `bash --noprofile --norc -e -o pipefail` by default. A mid-script
> non-zero exit or a failing left side of a pipe now fails the step. If you need
> the old lenient behaviour, set `shell: sh` and write your script accordingly.
> If the entire guest script should run as root, set `sudo: true` instead of
> prefixing each line with `sudo`.

**Execution model (both targets):**

- **host step** — `run:` is written to a temp file in the botforge container
  and executed via the resolved interpreter template using
  `std::process::Command`. Working directory is `repo_root`; the harness
  environment is inherited. The effective timeout is `steps[].timeout` or the
  document `step_timeout`, and the process is also bounded by the overall
  document `timeout`.
- **guest step** — `run:` is written to a temp file in the container, scp'd to
  a unique path under `/tmp` on the guest (e.g.
  `/tmp/botforge-step-<n>-<id>.sh`), then executed there via `ssh_with_retry`
  with the same 10-retry transport semantics as today. Each SSH attempt uses
  the effective step timeout and is also bounded by the overall document
  `timeout`. Guest steps execute `run:` over SSH with the same transport semantics.

Temp script files (both the local container copy and the guest `/tmp` copy for
guest steps) are removed best-effort after each step, on both success and
failure paths. A cleanup failure never masks the step result.

```yaml
type: test
ports:
  - 80
steps:
  - on: guest
    name: goss
    run: goss -g /path/goss.yaml validate
  - on: guest
    name: flip-spigot-to-ingress
    sudo: true
    run: |
      cp /etc/envoy/rds/active.ingress.yaml /etc/envoy/rds/active.yaml
      systemctl reload botwork-envoy
  - on: host
    name: vm-narrative
    run: bash smoke/vm-narrative.sh 127.0.0.1
  - on: guest
    name: flip-spigot-back
    sudo: true
    run: |
      cp /etc/envoy/rds/active.holding.yaml /etc/envoy/rds/active.yaml
      systemctl reload botwork-envoy
```

### `botforge build` spec format

A `type: build` document tells `botforge build` how to provision a new qcow2
image. All host paths are resolved relative to `--repo-root` (default: current
directory); all guest paths must be absolute.

```yaml
type: build
image: "@debian-base"           # required: @<shasset-name> resolves to the provider's default artifact
disk_size: "10G"                 # optional, default 10G
memsize: 4096                    # optional, default 4096 MiB
smp: 4                           # optional, default 4 vCPUs
step_timeout: 1800               # optional, default 1800 s; applies to each step
timeout: 7200                    # optional, default 7200 s; overall wall-clock budget
bootcmd:                         # optional; merged into the first-boot cloud-init user-data
  - echo "early boot hook"
compress:                        # optional; absent = plain rename (no compression)
  enabled: true
  compressor_args:               # optional qcow2 structural options
    cluster_size: "1M"
  compressor_opts: "-19 -T0"     # optional raw codec opts parsed by the compressor
uploads:                         # optional; guest-only pre-step staging
  - src: images/botspace/envoy/**/*.yaml
    dest: /tmp/bake-staging/envoy/
  - src: scripts/install.sh
    dest: /tmp/bake-staging/install.sh
steps:
  - on: guest
    name: provision
    sudo: true
    run: bash /tmp/bake-staging/install.sh
  - on: host
    name: verify
    run: echo "build host check"
```

#### `image:` (required)

`image:` is the only required field besides `type: build`. Its value is a
**`@`-scheme reference** identifying the source qcow2 to boot from.

**Supported form (current):** `@<shasset-name>` — resolves the named shasset
dep-provider's **default artifact** (the single qcow2 it pulls).

Example: `image: "@debian-base"` looks up the `debian-base` asset in the
shasset manifest pointed to by the global `--config` flag (default
`shasset.yaml`) and fetches its default file.

**Reserved (not yet supported):** `@://…` — the URI traversal form (walking
into repos/tarballs/paths within a provider).  The parser recognises this
form and hard-errors with a clear message so the scheme stays intact for
when traversal is implemented.  Do not use `://` in `image:` values today.

**Bare names are rejected.** `image: debian-base` (no `@`) is a parse-time
hard error.  The `@` prefix is mandatory.

When `botforge build` runs:
1. The manifest is loaded from `--config`.
2. `image:` is parsed; an `@`-prefixed name is required (hard error otherwise).
3. The named asset is looked up. An unknown key is a hard error naming the key
   and the manifest path.
4. `fetch_asset` downloads, verifies the checksum, and caches the blob (same
   mechanics as `botforge deps`).
5. A copy is materialized from the cache into `~/.cache/shasset/base-images/`
   (or the `--cache-dir` path) so qemu boots the copy without ever mutating the
   cached blob.
6. The provisioning flow (resize → boot → SSH → cloud-init → steps → shutdown)
   continues as before, using the materialized copy as the boot disk.

**`--source <qcow2>` overrides `image:` resolution.** When `--source` is
passed, `botforge build` boots that local file directly and skips shasset
entirely. This is useful for local iteration, building on a just-produced parent
qcow2, or arch experiments. Neither `--source` nor a resolvable `image:`
→ hard error.

**`--cache-dir <dir>` controls the shasset cache.** Defaults to
`~/.cache/shasset` (respecting `SHASSET_CACHE` / `XDG_CACHE_HOME` / `HOME`).
The cache is independent of the build/output dir by design — it can be mounted
as a persistent volume or CI cache to avoid re-downloading the image on
every build.

`image:` is a `type: build`-only field; it is rejected in `type: test` and
`type: fragment` documents at load time.

#### Steps in `type: build`

Steps use the same `on: guest` / `on: host` vocabulary as `type: test`. Guest
steps run inside the qemu VM via SSH; host steps run in the botforge container.
`on: host` steps in `type: build` do **not** require `ports:` (unlike
`type: test`).

Reusable `uses:` fragment includes work exactly as in `type: test`.

#### `uploads:` (optional) — guest pre-staging before `steps:`

`type: build` supports the same top-level `uploads:` list described above for
`type: test`. Entries are always guest-only and run once before the first
configured step.

- `src` must be a repo-relative file path or glob under `--repo-root`.
- `src` may not start with `@`; top-level `uploads:` is filesystem-only.
- `dest` must be an absolute guest path.
- Glob `src` requires `dest` to end in `/`, and matched files are staged with
  path preservation relative to the glob's fixed literal prefix.
- Optional `mode`, `owner`, `group`, `overwrite`, and `parents` fields are
  supported (see [Top-level `uploads:` (optional)](#top-level-uploads-optional)
  for details). Files are installed via `sudo install` so mode and ownership are
  applied atomically without a follow-up `chmod`/`chown` step.

#### `bootcmd:` (optional) — early first-boot commands

`bootcmd:` injects commands into the cloud-init **`bootcmd`** module of the
first-boot user-data that `botforge build` generates.  The `bootcmd` module
runs very early in the boot sequence — before `multi-user.target` and before
most systemd services start — making it the right hook for operations that
must complete before the service stack activates (e.g. masking units so they
never start during the provisioning boot).

**`bootcmd:` does NOT replace botforge's generated user-data.** The installer
user, SSH key injection, and sudo grant that botforge creates remain
authoritative; `bootcmd:` entries are merged in as an additional top-level key
in the same `#cloud-config` document.

Each list item is either:
- A **plain string** — cloud-init passes it to `sh -c`.
- A **sequence of strings** — cloud-init runs it directly via `execvp`
  (useful for `cloud-init-per`, `systemctl`, etc.).

**Example — mask systemd units before they start:**

```yaml
type: build
image: "@botwork-vm"
bootcmd:
  # Mask the application stack before multi-user.target; the provisioning
  # steps will unmask them after plugin installation.
  - - cloud-init-per
    - once
    - mask-app-stack
    - sh
    - -c
    - >-
      systemctl mask
      botwork-api.service
      botwork-envoy.service
      botwork-ui.service
steps:
  - on: guest
    name: install-plugins
    sudo: true
    run: bash /opt/install-plugins.sh
  - on: guest
    name: unmask-app-stack
    sudo: true
    run: systemctl unmask botwork-api.service botwork-envoy.service botwork-ui.service
```

Absent or empty `bootcmd:` produces user-data byte-identical to the current
output — there is zero behaviour change for existing specs that omit the field.

#### `compress:` (optional) — compress the output qcow2

`compress:` controls whether `botforge build` compresses the output qcow2
before committing it.  When absent (the default), the disk is committed via a
plain atomic rename — behaviour byte-identical to all prior botforge versions.

`compress:` is a **map** with one required field (`enabled:`) and four optional
fields (`compressor:`, `compressor_args:`, `compressor_opts:`, `reclaim:`):

| Field | Required | Type | Description |
|---|---|---|---|
| `enabled` | **yes** | bool | `true` ⇒ compress; `false` ⇒ plain rename (same as omitting the block) |
| `compressor` | no | enum | Native qcow2 compression codec: `zstd` (default when `enabled: true`) or `zlib`. |
| `compressor_args` | no | map[string→string] | Qcow2 structural options interpreted by botforge's native writer. Keys are sorted for determinism. Today this is primarily `cluster_size` (for example `{cluster_size: "1M"}`). |
| `compressor_opts` | no | string | Raw codec options string passed to the selected compressor implementation. For `zstd`, botforge currently supports compression level (`-19`, `-22`) and worker count (`-T0`, `-T4`), and hard-errors on unknown flags. |
| `reclaim` | no | enum | Reclaim freed blocks before commit: `none` (default), `fstrim`, or `discard`. |

A `compress:` block **without** `enabled:` is a hard parse error.  Unknown
fields inside the block (e.g. `clustersize`) are also hard errors
(`deny_unknown_fields`), catching typos at load time.

`reclaim:` always runs **before** commit/compression, and it still runs when
`enabled: false` (plain rename). This lets you reclaim space even when you
don't want compression.

- `reclaim: none` (default): no reclaim step (current behaviour).
- `reclaim: fstrim`: in-guest `sudo fstrim -av` before shutdown. botforge
  automatically attaches the build qcow2 with `discard=unmap` for this mode so
  the guest TRIM reaches the qcow2 layer without requiring an offline NBD pass.
- `reclaim: discard`: host-side offline reclaim after shutdown via
  `qemu-nbd --discard=unmap` + mount with `-o discard` + `fstrim -v`.
  More robust, but requires `qemu-nbd`.

For `reclaim: fstrim` and `reclaim: discard`, botforge
also runs a pure-Rust qcow2 zero-cluster sparsify pass before commit/compression.
It deallocates allocated-but-all-zero clusters (lossless) without introducing any
external runtime dependency.

When `enabled: true`, `botforge build` rewrites the qcow2 **in-process** and
stores compressed clusters itself.  There is no `qemu-img convert -c` shell-out
on the compression path.  The produced file remains a standard, bootable qcow2:
`compressor: zstd` writes a native zstd-compressed qcow2 readable by qemu >= 5.1,
and `compressor: zlib` continues to write the historical zlib-compressed format.

`compressor_args:` remains the place for qcow2 **structural** settings such as
`cluster_size`. `compressor_opts:` is the place for **codec** tuning. For
example, `compressor_opts: "-19 -T0"` tells the native zstd compressor to use
level 19 and all available worker threads. Unknown zstd flags are a hard error
that names the offending token.

> **Note — `-T0`/`-Tn` worker opts are accepted but not applied per-cluster.**
> qemu's `qcow2_zstd_decompress` performs a single `ZSTD_decompressStream` pass
> and requires each cluster to be exactly one self-contained zstd frame.
> Multithreaded zstd compression (libzstd `NbWorkers > 0`) can emit a
> worker-chunked multi-frame stream that qemu rejects with `-EIO`.  Per-cluster
> payloads are at most `cluster_size` (≤ 2 MiB) so multithreading yields no
> compression benefit anyway; botforge ignores the worker count for per-cluster
> work and always produces a single deterministic frame.

When compression is enabled and `compressor:` is omitted, botforge now
defaults to `zstd`. `zstd`-compressed qcow2 images require qemu >= 5.1 on any
consumer that opens or boots the produced image. If you need compatibility with
an older qemu stack, set
`compressor: zlib` explicitly.
At the end of capture, botforge logs final qcow2 stats
(`virtual_size`, `disk_size`, `cluster_size`, `allocated_data_clusters`,
`zero_clusters_deallocated`) to make image-size drift visible in CI.

#### Codec correctness — correction and resolution

Both native compressors had bugs that caused qemu boot failures while
in-process round-trip tests passed.  The shared root cause: **in-process
decoders accept output that qemu's stricter runtime decoder rejects**, so
self-consistent encode→decode tests do not catch qemu-incompatible frames.

| Codec | Bug | Fix |
|---|---|---|
| `zlib` | Used raw DEFLATE (`DeflateEncoder/Decoder`) instead of the zlib-wrapped format (`ZlibEncoder/Decoder`) that qcow2 and qemu require | Switched to `ZlibEncoder/ZlibDecoder` (PR #260) |
| `zstd` | `multithread(workers)` with `workers > 0` (triggered by `-T0`/`-Tn`) can emit a worker-chunked multi-frame stream; qemu's `qcow2_zstd_decompress` expects exactly one frame per cluster and returns `-EIO` otherwise | Removed `multithread()` call; compression is always single-threaded per cluster (PR #261) |

The blind spot in both cases: `DeflateDecoder` round-trips raw DEFLATE silently,
and `zstd::stream::read::Decoder` transparently reassembles multi-frame streams —
so encode→decode tests pass while a real qemu boot fails.  The test suite was
strengthened after each fix to assert the exact frame structure qemu requires.

**Examples:**

```yaml
# off (default) — plain atomic rename, byte-identical to prior behaviour
# (compress: absent)

# on, qemu default cluster size
compress:
  enabled: true
  # compressor defaults to zstd

# on, explicit cluster size and native zstd tuning
compress:
  enabled: true
  compressor: zstd
  compressor_args:
    cluster_size: "1M"
  compressor_opts: "-19 -T0"

# on, opt back into qemu's historical zlib compression codec
compress:
  enabled: true
  compressor: zlib

# on, reclaim guest-freed blocks before native compression
# (useful when your build deletes large temporary payloads, e.g. docker image tars)
compress:
  enabled: true
  compressor: zstd
  compressor_args:
    cluster_size: "1M"
  compressor_opts: "-19 -T0"
  reclaim: fstrim

# explicit off (equivalent to omitting the block)
compress:
  enabled: false

# plain rename output, but still reclaim space before commit
compress:
  enabled: false
  reclaim: discard
```

`compress:` is a `type: build`-only field; it is rejected in `type: test` and
`type: fragment` documents at load time.

## SSH verbosity

By default, `botforge test` (and any subcommand that SSHes into a VM) runs
`ssh`/`scp` with `-o LogLevel=ERROR`.  This suppresses the
`Warning: Permanently added … to the list of known hosts.` message that would
otherwise appear on every connection (because botforge always uses a fresh
per-run `UserKnownHostsFile=/dev/null`).

To restore full OpenSSH verbosity for debugging, set either of:

```sh
BOTFORGE_SSH_VERBOSE=1   # ssh/scp-specific toggle
BOTFORGE_DEBUG=1         # broader debug flag; implies verbose ssh
```

Accepted truthy values are `1`, `true`, and `yes` (case-insensitive).
