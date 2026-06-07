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

### Authentication

`auth:` resolves `${ENV_VAR}` references at fetch time. The resolved string follows one of two
shapes:

- `auth: ${TOKEN}` — bare token (no colon). Used as the password with the placeholder username
  `x-access-token` for OCI token exchange. This matches GHCR / GitHub PAT conventions and works
  with most docker-distribution registries (Harbor, Gitea, generic distributions).
- `auth: ${USER}:${TOKEN}` — explicit `username:password`. Use this for ECR
  (`auth: "AWS:${ECR_TOKEN}"`), Harbor robot accounts, or any registry that rejects the
  `x-access-token` placeholder. Everything after the first `:` is the password, so passwords
  containing colons are preserved in full.

shasset always issues the initial OCI request unauthenticated and authenticates
only via the `WWW-Authenticate: Bearer` challenge that follows. Operator-supplied
`auth:` credentials are used as HTTP Basic on the token-exchange endpoint, never
as a bearer token on the manifest/blob URL itself. This matches the
docker/distribution token-spec flow and is what `skopeo`, `crane`, and the
docker CLI do.

For non-OCI URIs (`https://`, `github-release://`), the resolved string is sent as
`Authorization: ****** regardless of whether it contains a colon — colon-splitting only
applies to the OCI token-exchange leg.

### Platform selection for OCI image indices

When an `oci://` URI points at a multi-arch image index (`mediaType:
application/vnd.oci.image.index.v1+json` or the Docker equivalent), shasset
selects a child manifest using the asset's `platform:` field, defaulting to
`linux/amd64`. The selected child's tarball is what gets cached and
materialized.

```yaml
assets:
  mcp-exec-bash:
    uri: oci://ghcr.io/example/mcp-exec-bash@sha256:dfe0edd…
    filename: mcp-exec-bash.tar
    platform: linux/amd64  # default; omit for the same effect
```

Accepted forms: `os/arch` and `os/arch/variant`. `platform:` is ignored for
non-OCI URIs and for OCI URIs that resolve directly to a single-platform
manifest (no walk needed).

### Optional `version:` field

`version:` is optional in the manifest schema. When present it is used to expand
`${version}` placeholders in the `uri:` and `filename:` fields. When omitted (or set
to an empty string) those fields are used as-is.

For OCI or other digest-pinned assets whose URI does not reference `${version}`,
you can either omit the field entirely or set it purely for informational purposes
— for example so that Dependabot or a custom version-bump script can track it:

```yaml
assets:
  auth-broker:
    uri: oci://ghcr.io/botworkz/auth-broker@sha256:abc123…
    filename: auth-broker.tar
    version: "0.3.1"   # informational only — URI does not use ${version}
```

On the CLI, `--version` is now optional on `shasset add`:

```sh
# OCI asset — no version needed
shasset add auth-broker \
  --uri oci://ghcr.io/botworkz/auth-broker@sha256:abc123… \
  --filename auth-broker.tar

# Non-OCI asset with version interpolation
shasset add mytool \
  --uri https://example.com/v${version}/mytool.tar.gz \
  --version 1.2.3 \
  --checksum sha256:<64-hex>

# Non-OCI asset with informational version (no interpolation)
shasset add mytool \
  --uri https://example.com/mytool.tar.gz \
  --version 1.2.3 \
  --checksum sha256:<64-hex>
```

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
