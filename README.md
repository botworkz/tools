# tools

Container images and tooling published from `botworkz/tools`.

## Images

### packer-tools

A Debian-based container image bundling [Packer](https://www.packer.io/), QEMU, and image-creation utilities for building virtual machine images.

**Included tools:** `packer`, `qemu-system-x86`, `qemu-utils`, `cloud-image-utils`, `genisoimage`, `xorriso`, `jq`, `curl`, `openssh-client`.

**Published image:** `ghcr.io/botworkz/tools/packer-tools`

**Pull:**
```sh
docker pull ghcr.io/botworkz/tools/packer-tools:latest
```

**Build locally:**
```sh
docker build -f containers/packer-tools/Dockerfile -t packer-tools:local containers/packer-tools
```