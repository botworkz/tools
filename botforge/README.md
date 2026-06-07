# botforge

`botforge` is the build-time companion CLI for botworkz VM artifact workflows. It wraps the external toolchain (QEMU/KVM, Packer, xorriso, OpenSSH) so the same commands work identically inside the published container image and in local development.

## Container image

**Published image:** `ghcr.io/botworkz/tools/botforge`
*****__CODE_BLOCK_0_2__*****

botforge is distributed as a **batteries-included** image: the QEMU toolchain (`qemu-system-x86_64`, `qemu-img`), Packer, ISO utilities (`xorriso`, `genisoimage`), and the OpenSSH client are baked in so subcommands work reproducibly without installing exact tool versions on your host.

### Runtime requirements

botforge drives KVM, so the container needs device access plus a mount of your working repository:
*****__CODE_BLOCK_0_3__*****

- `--device /dev/kvm` — required for `pack`, `run`, and `test` (KVM-only; no TCG fallback).
- `-v "$PWD:/work" -w /work` — mount your repo so botforge can read manifests, write artifacts, and resolve project-relative paths.

### Build the image locally

From the repo root, with [EarthBuild](https://github.com/EarthBuild/earthbuild):
*****__CODE_BLOCK_0_4__*****

This produces the stable local tag `botwork/botforge:local`.

## Commands

The `--config / -c` flag (default `shasset.yaml`) is global; it points `deps` at the shasset manifest and `payload` at the payload config.

| Command | Summary |
|---|---|
| `botforge deps --out  [--volume-id 

For `botforge test`, the `isos:` list in test config supports two forms:

- A bare string path (attach only).
- A mapping with `path:`, `label:`, `mount:`, and optional `bootstrap:` (default `bootstrap.sh`) to attach the ISO, mount it by label in the guest, and run the bootstrap script with `sudo` before configured `steps:`.
