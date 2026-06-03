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
docker build -f packer-tools/Dockerfile -t packer-tools:local packer-tools
```

### Building the container image with Earthly (EarthBuild)

This repository uses the maintained [EarthBuild/earthbuild](https://github.com/EarthBuild/earthbuild) fork, not sunset upstream Earthly. The Earthfile wraps `packer-tools/Dockerfile`, which remains the source of truth for the image contents.

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

## botforge

`botforge` is the build-time companion CLI for VM artifact workflows.

Current commands:

- `botforge deps --out <dir> [--executable] [<name>]` — fetches file assets from `shasset.yaml` using the `shasset` library and stages each asset flat at `<dir>/<asset-filename>`, where the filename comes from the manifest `filename` field or the URI basename (not the manifest key). It also handles `oci://` image assets in the same manifest: `docker pull` by digest, `docker tag` to `botwork/<asset-key>:local`, then `docker save` flat to `<dir>/<asset-filename>` (or `<dir>/<asset-key>.tar` when `filename` is unset). `docker` must be on `PATH`; `oci://` URIs must be pinned with `@sha256:<64-hex>`; manifest `checksum` is ignored for `oci://` assets.
- `botforge iso [--src <dir>] --out <file.iso> [--volume-id <id>]` — builds an ISO from a directory tree using `xorriso` (or `genisoimage` fallback). Seed ISO mode is folded in: pass `--ssh-public-key <KEY>` or `--ssh-public-key-file <PATH>` (and optional `--user-data-template <PATH>`) to generate cloud-init `user-data`/`meta-data` and build from that temp tree; `--src` is not required in seed mode. Use `--volume-id cidata` for NoCloud seed images.
- `botforge payload --config <payload.yaml> --out <payload.iso> [--staging-dir <dir>] [--volume-id botwork-payload]` — config-driven payload builder. It stages configured image tarballs into `images/`, stages configured overlay/systemd files at their configured relative payload paths, writes `bootstrap.sh` to load images/install files/reload+manage services, and then builds an ISO from the staged tree (layout preserved, not flattened).
- `botforge pack [--repo-root <dir>] [--compress] [--key <path>] [--compose-service <name>] [--compose-file <path>]` — runs a KVM-only Packer build of the base VM image via docker compose, then optionally compresses the qcow2. Requires `/dev/kvm`; dependencies and baked images must already be staged beforehand because botforge v1 does not build them. KVM is required; there are no tcg/accelerator options.
- `botforge run --base-image <qcow2> --overlay-image <overlay.qcow2> --seed-iso <cidata.iso> [--payload-iso <payload.iso>] [--ssh-port 2222] [--foreground]` — KVM-only qemu launcher. Creates the overlay via `qemu-img create -f qcow2 -F qcow2 -b <base> <overlay>`, then boots qemu with host SSH forwarding to guest port 22.
- `botforge test --test-config <test.yaml> --base-image <qcow2> --ssh-key <private-key> [--ssh-host 127.0.0.1] [--ssh-port 2222] [--ssh-user bot] [--repo-root <dir>] [--keep-running]` — KVM-only config-driven test orchestration: builds cidata seed from `<ssh-key>.pub`, creates overlay, boots qemu in background, waits for SSH/cloud-init, runs upload+command validation steps, and collects diagnostics on failure.

Example `payload.yaml`:

```yaml
images:
  - source: build/images/payload/auth-broker.tar
files:
  - source: overlay/envoy/lds/listener.yaml
    staging_path: envoy/lds/listener.yaml
    install_path: /etc/botwork/envoy/lds/listener.yaml
    mode: "0644"
  - source: overlay/systemd/botwork-auth-broker.service
    staging_path: systemd/botwork-auth-broker.service
    install_path: /etc/systemd/system/botwork-auth-broker.service
services:
  enable: [botwork-auth-broker, botwork-session-broker, botwork-envoy]
  restart: [botwork-auth-broker, botwork-session-broker, botwork-envoy]
```

Example `test.yaml`:

```yaml
isos:
  - build/botspace-payload.iso
steps:
  - name: base-goss
    uploads:
      - { src: build/goss-0.4.9, dest: /tmp/goss }
      - { src: ../vm/test/goss.yaml, dest: /tmp/goss.yaml }
    run: sudo install -m0755 /tmp/goss /usr/local/bin/goss && sudo goss -g /tmp/goss.yaml validate
  - name: payload-goss
    uploads:
      - { src: test/goss-payload.yaml, dest: /tmp/goss-payload.yaml }
    run: sudo goss -g /tmp/goss-payload.yaml validate
diagnostics_units:
  - ssh
  - botwork-launcher
  - botwork-envoy
  - botwork-session-broker
  - botwork-auth-broker
```

Example mixed manifest for `deps`:

```yaml
assets:
  session-broker:
    uri: oci://ghcr.io/botworkz/botwork/session-broker@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
  botwork-launcher:
    uri: github-release://botworkz/botwork/v${version}/botwork-launcher
    version: 0.0.1
    checksum: sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb
```

For v1, image resolution is registry-only (`oci://` pull-by-digest). Image build-from-source (sibling/earthly) is intentionally deferred to a future `dev-pack` flow.

## shasset

`shasset` is a **generic, verified-asset downloader and registry manager**. It maintains a declarative manifest (`shasset.yaml`) of named assets — each with a URI, optional version, and mandatory SHA-256 checksum for fetch/verify — and downloads and verifies those assets on demand.

**What shasset IS:**
- A registry (`shasset.yaml`) of named assets: URI, optional version, checksum, optional filename, optional auth.
- CRUD operations on that registry (`add`, `remove`, `get`).
- A verified downloader: fetch one or all named assets, verify each against its checksum, and fail loudly on any mismatch or error.

**What shasset is explicitly NOT (out of scope):**
- No knowledge of container images / registries / digest pins (Dependabot + Earthly already manage `FROM ...@sha256` pins).
- No "sibling build" / `*_REF` logic — that belongs in consumer repos.
- No awareness of qcow2, skopeo, tar export, or project-specific tooling.
- It is a generic tool anyone could vendor.

**Published image:** `ghcr.io/botworkz/tools/shasset`

**Pull:**
```sh
docker pull ghcr.io/botworkz/tools/shasset:latest
```

### `shasset.yaml` schema

```yaml
settings:                 # all optional; CLI flags override
  concurrency: 4          # parallel downloads (default: 4)
  retries: 3              # per-asset retry attempts on transient errors (default: 3)
  backoff:
    base_ms: 500          # initial backoff in milliseconds (default: 500)
    max_ms: 8000          # maximum backoff cap in milliseconds (default: 8000)
    factor: 2             # exponential multiplier (default: 2)

assets:
  <name>:
    uri: https://example.com/releases/download/v${version}/tool-${version}.tar.gz
    version: 0.0.1                     # OPTIONAL; defaults to empty string
    checksum: sha256:<64-hex>        # REQUIRED for fetch/verify
    filename: tool-0.0.1.tar.gz     # OPTIONAL: forced output filename
    auth: ${GH_TOKEN}               # OPTIONAL: env-var template, resolved at runtime
```

Rules:
- `${version}` in `uri` and `filename` is expanded to the asset's `version` value (empty when omitted).
- URI schemes are dispatch-based:
  - `http` / `https`: direct download (optional bearer auth).
  - `github-release://<owner>/<repo>/<tag>/<asset-name>`: `${version}` is expanded before parsing, the asset name is the final path segment, and the tag is everything between `<repo>` and the asset name so tags may contain `/` (for example `github-release://owner/repo/release/0.0.3/asset`). The handler resolves the release asset id via GitHub API, then downloads via `releases/assets/{id}`.
- `checksum` must be `sha256:<64-hex>`. An asset without a checksum cannot be fetched; use `add --compute` to populate it.
- `filename`: when set, the downloaded file is always written with this exact name (after `${version}` expansion). When absent, the URI basename is used.
- `auth`: the `${ENV_VAR}` template is stored as-is and resolved from the process environment at fetch time. The resolved secret is sent as `Authorization: ****** **The secret is never written back to the manifest file.** If the referenced variable is unset, shasset errors clearly.

### Commands

```
shasset [--config <path>] <COMMAND>
```

Global flag: `--config <path>` — path to the manifest file (default: `shasset.yaml`).

| Command | Description |
|---|---|
| `add <name> --uri <uri> --version <ver> [--checksum <cs>] [--filename <f>] [--auth <tpl>] [--cache-dir <dir>]` | Add or update an asset. Use `--compute` instead of `--checksum` to download once into cache and auto-populate the checksum. |
| `remove <name>` | Remove a named asset. |
| `get [<name>] [--json]` | Show one or all assets. Displays the auth template, never the resolved secret. |
| `fetch [<name>] --out <dir> [--cache-dir <dir>] [--concurrency <n>] [--link] [--no-reverify]` | Download and verify one or all assets via the local blob cache, then materialize to `<dir>/<name>/<filename>`. |
| `prune [--cache-dir <dir>] [--dry-run]` | Remove unreferenced cached blobs from `blobs/sha256/<hex>` and clear stale `quarantine/*` entries while keeping blobs still referenced by the manifest. |
| `verify [<name>] --out <dir> [--json]` | Verify on-disk files against manifest checksums (no network). |

### Cache + output model

- Cache root resolution precedence is: `--cache-dir`, `SHASSET_CACHE`, `$XDG_CACHE_HOME/shasset`, `$HOME/.cache/shasset`, then `.cache/shasset`.
- Verified blobs are stored content-addressed at `blobs/sha256/<hex>`.
- Downloads stream into a quarantine temp file and are promoted only after successful checksum/truncation checks.
- `--out <dir>` is **required** for `fetch` and `verify` and is **not stored** in the manifest.
- Default `fetch` materialization copies cache blobs to `<out>/<name>/<filename>`.
- `fetch --link` creates `<out>/<name>/<filename>` as a symlink to the cache blob instead of copying.
- By default, cache blobs are re-verified before use; `--no-reverify` skips that check.
- `prune` removes cached blobs not referenced by the current manifest and clears stale `quarantine/*` entries. Use `--dry-run` to report what would be removed without deleting anything; the command prints a human-readable summary of blobs removed, bytes reclaimed, and quarantine entries cleared.

### Retry and backoff

Transient errors (HTTP 5xx, 429, connection/DNS/timeout failures, and other transport-level read failures) are retried with exponential backoff up to the configured `retries` count. HTTP 4xx errors (except 429) and checksum mismatches are **not** retried — they fail immediately. Empty/zero-byte downloads are always errors.

### Container usage

```sh
docker run --rm -v "$PWD:/work" -w /work -e GH_TOKEN \
  ghcr.io/botworkz/tools/shasset fetch --out build/deps
```

### Building the container image with Earthly (EarthBuild)

```sh
earthly +shasset-image
```

This produces the stable local tag `botwork/shasset:local`.

### Relationship to `bin/update-deps`

`bin/update-deps` is a bash tool that resolves and writes digest/sha256 **pins** into a consumer `deps.lock` file (and also handles container image digest pins via `skopeo`). It operates at pin-update time.

`shasset` is a standalone **fetch-time** verified downloader that owns its own `shasset.yaml` registry. The two tools are **complementary**: `update-deps` manages the pinning workflow (and image digests), while `shasset` handles declarative asset downloads with built-in verification. Both may exist in the same repo. This PR does not modify or remove `update-deps`.
