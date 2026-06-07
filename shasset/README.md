# shasset

`shasset` is a **generic, verified-asset downloader and registry manager**. It maintains a declarative manifest (`shasset.yaml`) of named assets — each with a URI, optional version, and mandatory SHA-256 checksum — and downloads and verifies those assets on demand.

**What shasset IS:**
- A registry (`shasset.yaml`) of named assets: URI, optional version, checksum, optional filename, optional auth.
- CRUD operations on that registry (`add`, `remove`, `get`).
- A verified downloader: fetch one or all named assets, verify each against its checksum, and fail loudly on any mismatch or error.

**What shasset is explicitly NOT (out of scope):**
- No management of image **digest pins** — Dependabot + EarthBuild manage `FROM ...@sha256` pins, and `shasset/bin/update-deps` resolves them at pin-update time. `shasset` itself can *fetch* an `oci://` image by digest, but it does not maintain the pin file.
- No "sibling build" / `*_REF` logic — that belongs in consumer repos.
- No awareness of qcow2, skopeo, tar export, or project-specific tooling.
- It is a generic tool anyone could vendor.

## Container image

**Published image:** `ghcr.io/botworkz/tools/shasset`

```sh
docker pull ghcr.io/botworkz/tools/shasset:latest
```

Run against your working directory:

```sh
docker run --rm -v "$PWD:/work" -w /work -e GH_TOKEN \
  ghcr.io/botworkz/tools/shasset fetch --out build/deps
```

Build the image locally with [EarthBuild](https://github.com/EarthBuild/earthbuild) (from the repo root):

```sh
earthly +shasset-image
```

This produces the stable local tag `botwork/shasset:local`.

## `shasset.yaml` schema

```yaml
settings:                 # all optional; CLI flags override
  concurrency: 4          # parallel downloads (default: 4)
  retries: 3              # per-asset retry attempts on transient errors (default: 3)
  backoff:
    base_ms: 500          # initial backoff in milliseconds (default: 500)
    max_ms: 8000          # maximum backoff cap in milliseconds (default: 8000)
    factor: 2             # exponential multiplier (default: 2)

assets:
  <filename>`. |
| `prune [--cache-dir <filename>`.
- `fetch --link` creates `<filename>` as a symlink to the cache blob instead of copying.
- By default, cache blobs are re-verified before use; `--no-reverify` skips that check.
- `prune` removes cached blobs not referenced by the current manifest and clears stale `quarantine/*` entries. Use `--dry-run` to report what would be removed without deleting anything; the command prints a human-readable summary of blobs removed, bytes reclaimed, and quarantine entries cleared.

## Retry and backoff

Transient errors (HTTP 5xx, 429, connection/DNS/timeout failures, and other transport-level read failures) are retried with exponential backoff up to the configured `retries` count. HTTP 4xx errors (except 429) and checksum mismatches are **not** retried — they fail immediately. Empty/zero-byte downloads are always errors.

## `bin/update-deps`

`shasset/bin/update-deps` is a bash tool that updates managed digest/sha256 pins in a consumer `deps.lock` file from a declarative manifest. It is **complementary** to shasset itself:

- `update-deps` operates at **pin-update time**: it resolves image digests (via `skopeo` or `docker buildx imagetools`) and release-asset sha256s (via `curl + sha256sum`) and writes them into a `deps.lock` shell file.
- `shasset` operates at **fetch time**: it owns a `shasset.yaml` registry and downloads + verifies assets against the pins it stores.

Both may coexist in the same consumer repo.

### Usage
*****__CODE_BLOCK_0_0__*****

Options:

- `--manifest <FILE_NAME>`

`<FILE_NAME>` support `${VAR}` interpolation using variables loaded from `deps.lock`.

Example:
*****__CODE_BLOCK_0_0__*****

## Shared bash helpers (`lib/`)

`shasset/lib/` contains bash helper libraries that can be vendored by sibling consumer repos. Source `common.sh` first, then any sibling-locator libraries as needed:

- `lib/common.sh` — logging (`log_info`, `log_warn`, `log_error`, `die`), `ensure_command`, `verify_sha256`, ephemeral SSH keypair helpers, accelerator selection (`pick_accelerator`, `packer_accelerator`), and repo-relative path helpers.
- `lib/botworkz.sh` — locates a sibling `botworkz/mcp` checkout (`BOTWORKZ_MCP_DIR`, defaults to `${REPO_ROOT}/../mcp`).
- `lib/tools.sh` — locates a sibling `botworkz/botwork` checkout (`BOTWORK_TOOLS_DIR`, defaults to `${REPO_ROOT}/../botwork`) and provides cargo-build / release-download helpers for `botwork-launcher` and `botwork-tools`.
- `lib/botwork.sh` — locates a sibling `botworkz/mcp-extra` checkout (`BOTWORK_MCP_EXTRA_DIR`, defaults to `${REPO_ROOT}/../mcp-extra`).*****__CODE_BLOCK_0_1__*****
