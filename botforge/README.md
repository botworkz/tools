# botforge

`botforge` is the build-time companion CLI for botworkz VM artifact workflows. It wraps the external toolchain (QEMU/KVM, libguestfs, xorriso, OpenSSH) so the same commands work identically inside the published container image and in local development.

## Container image

**Published image:** `ghcr.io/botworkz/tools/botforge`

```sh
docker pull ghcr.io/botworkz/tools/botforge:latest
```

botforge is distributed as a **batteries-included** image: the QEMU toolchain
(`qemu-system-x86_64`, `qemu-img`), libguestfs (`virt-customize`, `virt-copy-in`,
`guestfish`, …), ISO utilities (`xorriso`, `genisoimage`), and the OpenSSH
client are baked in so subcommands and image-build flows work reproducibly
without installing exact tool versions on your host.

### Runtime requirements

botforge drives KVM-capable workflows for `test` and `run`, so for those
subcommands the container needs device access plus a mount of your working
repository:

```sh
docker run --rm \
  --device /dev/kvm \
  -v "$PWD:/work" -w /work \
  ghcr.io/botworkz/tools/botforge:latest \
  --help
```

- `--device /dev/kvm` — required for `run` and `test` (KVM-only; no TCG fallback).
- `-v "$PWD:/work" -w /work` — mount your repo so botforge can read manifests,
  write artifacts, and resolve project-relative paths.

`botforge build` benefits from `/dev/kvm` for libguestfs hardware acceleration
but does not strictly require it. With `LIBGUESTFS_BACKEND=direct` (the image
default) and no `/dev/kvm`, libguestfs falls back to TCG inside its supermin
appliance.

### Build the image locally

From the repo root, with [EarthBuild](https://github.com/EarthBuild/earthbuild):

```sh
earthly +botforge-image
```

This produces the stable local tag `botwork/botforge:local`.

## Commands

The `--config / -c` flag (default `shasset.yaml`) is global; it points `deps`
at the shasset manifest and `payload` at the payload config.

| Command | Summary |
|---|---|
| `botforge build --spec <file> --source <qcow2> --output <qcow2>` | Run a virt-customize spec against a source qcow2 to produce an output qcow2. |
| `botforge deps --out <dir> [name ...]` | Fetch + stage shasset assets into a flat output directory. |
| `botforge iso --src <dir> --out <file> [--volume-id <id>]` | Build an ISO image from a source tree. Also supports generating a cidata seed ISO with an injected SSH key. |
| `botforge payload --out <file>` | Build a payload ISO from a config-driven staging plan. |
| `botforge run …` | Launch a VM with qemu (KVM-only). |
| `botforge test …` | Boot a packed qcow2 with a cloud-init cidata seed, SSH in, and execute the steps in a `test-packed.yaml` plan. |

For `botforge test`, the `isos:` list in test config supports two forms:

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
ports:
  - 80              # bind 127.0.0.1:80 -> guest :80  (loopback only)
  - "0.0.0.0:9901"  # bind 0.0.0.0:9901 -> guest :9901 (all interfaces)
steps:
  - name: check-edge
    run: curl -fsS http://127.0.0.1/
```

### `botforge test` step model

Each entry in `steps:` has a required `on:` field that selects where it runs:

- **`on: guest`** — runs inside the VM via SSH. `uploads:` scp files into the
  guest first, then `run:` executes there. This is the traditional test step.
- **`on: host`** — runs locally in the **botforge container / harness** (where
  botforge itself executes), _not_ inside the guest. Reaches the guest only
  via ports declared in `ports:`. Inherits the harness environment (so CI
  variables such as `GH_TOKEN` are visible). Has a plain execution timeout
  with no SSH transport retries. `uploads:` is not valid on host steps.

Steps execute in the exact order written; guest and host steps may interleave
freely. This lets you flip guest state, hit the guest from outside, then
restore — all in a single ordered sequence.

#### Reusable step fragments with `uses:`

`botforge test` can splice a reusable list of steps from another YAML file in
the same repository:

- `uses: "@://path/within/repo.yaml"` resolves from the explicit
  `--repo-root` passed to `botforge test`.
- The referenced file must be a mapping with a top-level `steps:` key whose
  value is a list of steps, consistent with the top-level config shape.
- The fragment declares its **input contract** in a top-level `inputs:` block
  (see below). The caller passes values via `with:` at the call site.
- `${{ inputs.NAME }}` placeholders in the fragment body are substituted with
  resolved input values before validation.
- Runtime `${VAR}` expansion is unchanged; only `${{ ... }}` is handled at load
  time.

```yaml
# test.yaml
steps:
  - uses: "@://smoke/vm-narrative.steps.yaml"
    with:
      target: ingress
      shell: bash
```

```yaml
# smoke/vm-narrative.steps.yaml
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

Config errors are reported at load time for: missing or invalid `on:`; `uploads:`
on an `on: host` step; any `on: host` step present when `ports:` is empty;
invalid `shell:` value; invalid `uses:` scheme/path; `with:` key not declared
by the fragment (`unexpected input '<name>' not declared by fragment <path>`);
missing required input (`missing required input '<name>'`); type mismatch
(`input '<name>' must be a <type>`); `required: true` combined with `default`
in a declaration (`input '<name>' cannot set both 'required: true' and
'default'`); fragment missing a `steps:` list.

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

**Execution model (both targets):**

- **host step** — `run:` is written to a temp file in the botforge container
  and executed via the resolved interpreter template using
  `std::process::Command`. Working directory is `repo_root`; the harness
  environment is inherited. The 300 s timeout and kill behaviour are unchanged.
- **guest step** — `run:` is written to a temp file in the container, scp'd to
  a unique path under `/tmp` on the guest (e.g.
  `/tmp/botforge-step-<n>-<id>.sh`), then executed there via `ssh_with_retry`
  with the same 10-retry / 300 s timeout as today. The guest `uploads:` still
  happen first, exactly as before.

Temp script files (both the local container copy and the guest `/tmp` copy for
guest steps) are removed best-effort after each step, on both success and
failure paths. A cleanup failure never masks the step result.

```yaml
ports:
  - 80
steps:
  - on: guest
    name: goss
    run: goss -g /path/goss.yaml validate
  - on: guest
    name: flip-spigot-to-ingress
    run: |
      sudo cp /etc/envoy/rds/active.ingress.yaml /etc/envoy/rds/active.yaml
      sudo systemctl reload botwork-envoy
  - on: host
    name: vm-narrative
    run: bash smoke/vm-narrative.sh 127.0.0.1
  - on: guest
    name: flip-spigot-back
    run: |
      sudo cp /etc/envoy/rds/active.holding.yaml /etc/envoy/rds/active.yaml
      sudo systemctl reload botwork-envoy
```

### `botforge build` spec format

A build spec is a YAML document with three top-level concerns: disk knobs,
an optional staged build context, and an ordered list of steps. All host
paths are resolved relative to `--repo-root` (default: current directory);
all guest paths must be absolute.

```yaml
# Optional disk knobs (defaults shown).
disk_size: 10G        # passed to `qemu-img resize`
memsize: 4096         # MB given to the libguestfs appliance
smp: 4                # vCPUs given to the libguestfs appliance

# Optional: after growing the qcow2 header, expand a specific partition
# (and the filesystem inside it) to fill the new free space. Without
# this, `disk_size` only moves the qcow2 envelope — the partition
# table and rootfs stay the source image's original size. Set this
# when virt-customize steps need to write more data into the rootfs
# than the source partition can hold.
#
# Find the right device path with `virt-filesystems --long --parts
# -a <source-qcow2>`; common values are `/dev/sda1` (Debian generic
# cloud) or `/dev/vda1`.
expand_partition: /dev/sda1

# Optional context: a host tarball that gets unpacked into the guest
# before any step runs. The host paths under `paths:` are packed into a
# tarball, uploaded once, then extracted under `dest:` in the guest.
context:
  dest: /tmp/botwork-build-context
  paths:
    # Bare string: dest inside the context = basename of the host path.
    - images/botwork/payload/envoy
    - images/botwork/payload/systemd
    # Mapped: explicit dest (must be relative, no `..`).
    - { src: build/images/baked, dest: images }
    - { src: build/bin }

# Ordered list of operations applied to the guest.
steps:
  # Copy a host script into the guest and run it as root.
  - run: images/_shared/provisioners/00-base.sh

  # Run an inline shell command in the guest.
  - run_command: "systemctl daemon-reload"

  # Upload a single host file to an absolute guest path.
  - upload: { src: build/foo, dest: /usr/local/bin/foo }

  # Recursively copy a host file or directory into a guest *directory*
  # (preserves the source basename, like virt-customize --copy-in).
  - copy_in: { src: images/botwork/payload/systemd, dest: /etc/systemd/system }

  # Filesystem ops.
  - mkdir: /var/lib/botwork
  - truncate: /etc/machine-id
  - delete: /var/lib/dbus/machine-id

  # Write a literal string to a guest file.
  - write: { path: /etc/marker, content: "hello\n" }
```

`botforge build`:

- Copies `--source` to `<output>.partial` with `cp --reflink=auto` (instant
  CoW on btrfs/xfs, falls back to a plain copy otherwise).
- Runs `qemu-img resize` to the spec's `disk_size`.
- When `expand_partition` is set, runs `virt-resize --expand <part>`
  source → fresh empty qcow2 of the new size, then atomically replaces
  the partial with the expanded copy. The named partition's filesystem
  grows to fill the new disk. (`qemu-img create` is invoked once here
  to materialise the empty target — virt-resize requires it to
  pre-exist.)
- Invokes `virt-customize` once, in argument order: context staging first
  (when present) followed by each declared step.
- Renames `<output>.partial` to `<output>` on success. On failure the
  `.partial` is left in place for post-mortem and cleared by the next run.

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

## What is **not** here

There is no `botforge pack` subcommand. Image builds used to drive Packer +
HCL; that has been replaced by `botforge build` + a YAML spec per image,
which dispatches `virt-customize` directly. The change deleted the dependency
on `releases.hashicorp.com` (and Packer plugin distribution generally) and
~150 MB of HCL toolchain from the runtime image.
