# tools

Container images and tooling published from `botworkz/tools`.

## Shared bash tooling

This repository also hosts shared bash helpers for `botworkz/vm` and `botworkz/space`:

- `lib/common.sh`
- `lib/botworkz.sh`
- `lib/tools.sh`
- `lib/botwork.sh`

Source `lib/common.sh` first, then source any sibling-locator libraries as needed.

### `update-deps` tool

`bin/update-deps` updates managed digest/sha256 pins in a consumer `deps.lock` file from a declarative manifest.

Usage:

```sh
bin/update-deps --manifest /path/to/deps.manifest --lock /path/to/deps.lock
```

Options:

- `--manifest <path>`: manifest file (default: `${DEPS_MANIFEST_PATH:-${REPO_ROOT}/deps.manifest}`)
- `--lock <path>`: lock file (default: `${DEPS_LOCK_PATH:-${REPO_ROOT}/deps.lock}`)
- `--dry-run`: print updates without writing

Manifest grammar (line-based):

- Blank lines and lines starting with `#` are ignored.
- `image <LOCK_KEY> <IMAGE_REF>`
- `release <LOCK_KEY> <URL> <FILE_NAME>`

`<IMAGE_REF>`, `<URL>`, and `<FILE_NAME>` support `${VAR}` interpolation using variables loaded from `deps.lock`.

Example:

```text
# image digest pin
image AUTH_BROKER_IMAGE_DIGEST ghcr.io/botworkz/botwork-extra/auth-broker:${BOTWORK_EXTRA_IMAGES_VERSION_LOCK}

# release asset sha256 pin
release BOTWORK_LAUNCHER_SHA256 https://github.com/botworkz/botwork/releases/download/v${BOTWORK_TOOLS_IMAGES_VERSION_LOCK}/botwork-launcher botwork-launcher
```

## Images

### packer-tools

A Debian-based container image bundling [Packer](https://www.packer.io/), QEMU, and image-creation utilities for building virtual machine images.

**Included tools:** `packer`, `qemu-system-x86`, `qemu-utils`, `cloud-image-utils`, `genisoimage`, `xorriso`, `jq`, `curl`, `openssh-client`.

**Published image:** `ghcr.io/botworkz/tools/packer-tools`

**Pull:**
```sh
docker pull ghcr.io/botworkz/tools/packer-tools:latest
```

**Build locally with Docker:**
```sh
docker build -f containers/packer-tools/Dockerfile -t packer-tools:local containers/packer-tools
```

### Building the container image with Earthly (EarthBuild)

This repository uses the maintained [EarthBuild/earthbuild](https://github.com/EarthBuild/earthbuild) fork, not sunset upstream Earthly. The Earthfile wraps `containers/packer-tools/Dockerfile`, which remains the source of truth for the image contents.

Install the pinned EarthBuild `v0.8.17` binary locally:

```sh
sudo curl -fsSL -o /usr/local/bin/earthly \
  https://github.com/EarthBuild/earthbuild/releases/download/v0.8.17/earth-linux-amd64
sudo chmod +x /usr/local/bin/earthly
earthly bootstrap
```

Build the local development image:

```sh
earthly +packer-tools-image
```

This produces the stable local tag `botwork/packer-tools:local`.

`botworkz/vm` consumes this target cross-repo via `FROM ../tools+packer-tools-image` for sibling/local build mode, so the `+packer-tools-image` target name and `botwork/packer-tools:local` tag are a stable contract.
