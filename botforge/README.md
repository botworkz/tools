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

### `botforge build` spec format

A build spec is a YAML document with three top-level concerns: disk knobs,
an optional staged build context, and an ordered list of steps. All host
paths are resolved relative to `--repo-root` (default: current directory);
all guest paths must be absolute.

```yaml
# Optional disk knobs (defaults shown).
disk_size: 10G        # rewrites the qcow2 header's virtual size field
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
- Rewrites the qcow2 header's `size` field in place so the declared
  virtual size matches the spec's `disk_size` (grow-only; no clusters
  are allocated, no `qemu-img` invocation).
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

## What is **not** here

There is no `botforge pack` subcommand. Image builds used to drive Packer +
HCL; that has been replaced by `botforge build` + a YAML spec per image,
which dispatches `virt-customize` directly. The change deleted the dependency
on `releases.hashicorp.com` (and Packer plugin distribution generally) and
~150 MB of HCL toolchain from the runtime image.
