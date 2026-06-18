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
client are baked in so subcommands and image-build scripts work reproducibly
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

For image-build flows that drive `virt-customize` (typically out of the calling
repo's own scripts, not via a `botforge` subcommand), the container also benefits
from `/dev/kvm` for libguestfs hardware acceleration but does not strictly
require it. With `LIBGUESTFS_BACKEND=direct` (the image default) and no
`/dev/kvm`, libguestfs falls back to TCG inside its supermin appliance.

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

## What is **not** here

There is no `botforge pack` subcommand. Image builds are now driven directly by
shell scripts in the calling repo (e.g. `botworkz/vm` `images/<name>/build.sh`)
which invoke `virt-customize` against a source qcow2. Removing the Packer
wrapper deleted the dependency on `releases.hashicorp.com` (and Packer plugin
distribution generally) and ~150 MB of HCL toolchain from the runtime image.
