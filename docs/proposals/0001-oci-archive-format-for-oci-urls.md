# 0001 — Switch `oci://` output to OCI image-layout (oci-archive)

**Status:** Draft  
**Author:** Copilot  
**Created:** 2026-06-10  
**Relates to:** [botworkz/vm#68](https://github.com/botworkz/vm/pull/68)

---

## 1. Executive Summary

`shasset` today assembles a **docker-archive (Moby v1)** tar for every `oci://` asset — the same format produced by `docker save`.
That format stores only the image **config** digest (`<config_hex>.json`) in `manifest.json`; the registry's **manifest** digest (the `@sha256:…` value pinned in `shasset.yaml`) is never embedded in the tar itself.
Switching to **OCI image-layout (oci-archive)** embeds the manifest digest verbatim in `index.json`'s `manifests[0].digest`, making a hermetic on-disk assertion trivially correct.
The smallest possible diff is **one function replacement** (`assemble_docker_archive` → `assemble_oci_archive`) of approximately 75–90 lines in a single file, plus updates to one test.
The headline compatibility risk is `docker load`: Docker Engine ≥ 25.0 (January 2024) accepts oci-archive natively; older daemons reject it.
`botworkz/vm` installs Docker CE from the upstream apt repository (never the distro package), so it always receives a current release and this risk is negligible in practice.

---

## 2. Where the `oci://` URI scheme is dispatched

### 2.1 URI-scheme dispatch entry point

All asset URIs pass through **`download_via_scheme()`**, which parses the scheme with `reqwest::Url::parse` and matches on it:

```
shasset/src/fetch.rs, lines 507–526
https://github.com/botworkz/tools/blob/a371902cce4aa6eb4107d23c23b22aa33b662a00/shasset/src/fetch.rs#L507-L526
```

The match arm for `"oci"` is:

```rust
// fetch.rs L521
"oci" => try_download_oci(transport, cache_dir, name, asset, uri, auth),
```

There are no other code paths that handle `oci://` URIs; no external binary (`docker`, `skopeo`, `crane`, `oras`) is ever invoked.  All registry communication is pure Rust using `reqwest`.

### 2.2 Manifest fetch

**`try_download_oci()`** ([`fetch.rs:1231–1443`](https://github.com/botworkz/tools/blob/a371902cce4aa6eb4107d23c23b22aa33b662a00/shasset/src/fetch.rs#L1231-L1443)) performs the full pull in six numbered steps:

| Step | Lines | Action |
|------|-------|--------|
| 1 | [1245–1258](https://github.com/botworkz/tools/blob/a371902cce4aa6eb4107d23c23b22aa33b662a00/shasset/src/fetch.rs#L1245-L1258) | `GET /v2/<repo>/manifests/<digest>`, self-verify manifest bytes against the pinned digest |
| 1b | [1264–1309](https://github.com/botworkz/tools/blob/a371902cce4aa6eb4107d23c23b22aa33b662a00/shasset/src/fetch.rs#L1264-L1309) | If the manifest is an image index, re-fetch the matching platform child manifest |
| 2 | [1311–1337](https://github.com/botworkz/tools/blob/a371902cce4aa6eb4107d23c23b22aa33b662a00/shasset/src/fetch.rs#L1311-L1337) | `GET /v2/<repo>/blobs/<config_digest>`, self-verify config |
| 3 | [1339–1393](https://github.com/botworkz/tools/blob/a371902cce4aa6eb4107d23c23b22aa33b662a00/shasset/src/fetch.rs#L1339-L1393) | For each layer: `GET /v2/<repo>/blobs/<layer_digest>`, self-verify, decompress gzip if applicable |
| 4 | [1395–1402](https://github.com/botworkz/tools/blob/a371902cce4aa6eb4107d23c23b22aa33b662a00/shasset/src/fetch.rs#L1395-L1402) | Call `assemble_docker_archive()` to build the output tar in memory |
| 5 | [1406–1417](https://github.com/botworkz/tools/blob/a371902cce4aa6eb4107d23c23b22aa33b662a00/shasset/src/fetch.rs#L1406-L1417) | Write an oci-index cache entry (`cache/oci-index/<manifest_hex>.<platform_slug>`) mapping manifest digest → assembled tar sha256 |
| 6 | [1419–1435](https://github.com/botworkz/tools/blob/a371902cce4aa6eb4107d23c23b22aa33b662a00/shasset/src/fetch.rs#L1419-L1435) | Write assembled tar to quarantine path |

### 2.3 Tar-assembly function — where top-level entries are decided

**`assemble_docker_archive()`** ([`fetch.rs:1484–1558`](https://github.com/botworkz/tools/blob/a371902cce4aa6eb4107d23c23b22aa33b662a00/shasset/src/fetch.rs#L1484-L1558)) builds all tar entries in memory and writes them with the `tar` crate.
The top-level entries are constructed at:

| Entry | Source lines |
|-------|-------------|
| `manifest.json` | [1493–1504](https://github.com/botworkz/tools/blob/a371902cce4aa6eb4107d23c23b22aa33b662a00/shasset/src/fetch.rs#L1493-L1504) |
| `repositories` | [1506–1516](https://github.com/botworkz/tools/blob/a371902cce4aa6eb4107d23c23b22aa33b662a00/shasset/src/fetch.rs#L1506-L1516) |
| `<config_hex>.json` | [1518–1519](https://github.com/botworkz/tools/blob/a371902cce4aa6eb4107d23c23b22aa33b662a00/shasset/src/fetch.rs#L1518-L1519) |
| `<layer_hex>/VERSION` | [1523](https://github.com/botworkz/tools/blob/a371902cce4aa6eb4107d23c23b22aa33b662a00/shasset/src/fetch.rs#L1523) |
| `<layer_hex>/json` | [1530–1532](https://github.com/botworkz/tools/blob/a371902cce4aa6eb4107d23c23b22aa33b662a00/shasset/src/fetch.rs#L1530-L1532) |
| `<layer_hex>/layer.tar` | [1534](https://github.com/botworkz/tools/blob/a371902cce4aa6eb4107d23c23b22aa33b662a00/shasset/src/fetch.rs#L1534) |

Entries are sorted lexicographically at [line 1538](https://github.com/botworkz/tools/blob/a371902cce4aa6eb4107d23c23b22aa33b662a00/shasset/src/fetch.rs#L1538) before writing.

### 2.4 No shelled-out tools

`shasset` does **not** shell out to `docker save`, `skopeo`, `crane`, or `oras`.
All HTTP requests go through the `Transport` trait ([`fetch.rs:112–180`](https://github.com/botworkz/tools/blob/a371902cce4aa6eb4107d23c23b22aa33b662a00/shasset/src/fetch.rs#L112-L180)); the only real implementation is `ReqwestTransport` ([`fetch.rs:182–299`](https://github.com/botworkz/tools/blob/a371902cce4aa6eb4107d23c23b22aa33b662a00/shasset/src/fetch.rs#L182-L299)), which uses `reqwest::blocking::Client`.

---

## 3. Current tar format for an `oci://` URI

### 3.1 Format: docker-archive (Moby v1)

The output is definitively **docker-archive** — the format produced by `docker save` and consumed by `docker load`.

The function-level doc comment ([`fetch.rs:1228`](https://github.com/botworkz/tools/blob/a371902cce4aa6eb4107d23c23b22aa33b662a00/shasset/src/fetch.rs#L1228)) confirms this:

```
/// Pull an OCI image by digest, assemble a docker-archive tar, and return a DownloadedFile.
```

The tar-assembly function doc ([`fetch.rs:1473–1483`](https://github.com/botworkz/tools/blob/a371902cce4aa6eb4107d23c23b22aa33b662a00/shasset/src/fetch.rs#L1473-L1483)) documents the exact layout:

```
/// Assemble a docker-load-compatible image archive tarball.
///
/// Layout (entries sorted lexicographically for determinism):
/// ```text
/// <config-hex>.json               -- OCI config blob bytes
/// <layer-hex>/json                -- minimal v1 image JSON
/// <layer-hex>/layer.tar           -- uncompressed layer tar
/// <layer-hex>/VERSION             -- "1.0"
/// manifest.json                   -- docker-archive manifest
/// repositories                    -- legacy tag index
/// ```
```

### 3.2 Exact top-level entries

For a single-layer image where the config digest hex is `<cfg>` and the layer digest hex is `<lyr>`:

| Path | Content |
|------|---------|
| `<cfg>.json` | Raw OCI image config blob bytes |
| `<lyr>/json` | Minimal Moby v1 JSON (`{"id": "<lyr>", "created": "0001-01-01T00:00:00Z", …}`) |
| `<lyr>/layer.tar` | Uncompressed layer filesystem tar |
| `<lyr>/VERSION` | `1.0` |
| `manifest.json` | `[{"Config":"<cfg>.json","RepoTags":["botwork/<name>:local"],"Layers":["<lyr>/layer.tar"]}]` |
| `repositories` | `{"botwork/<name>":{"local":"<lyr>"}}` |

### 3.3 Source lines that prove the format

The `manifest.json` entry is constructed at [`fetch.rs:1498–1504`](https://github.com/botworkz/tools/blob/a371902cce4aa6eb4107d23c23b22aa33b662a00/shasset/src/fetch.rs#L1498-L1504):

```rust
let manifest_json = serde_json::to_vec(&serde_json::json!([{
    "Config": format!("{config_hex}.json"),
    "RepoTags": [local_tag],
    "Layers": layer_paths,
}]))
.context("failed to serialize docker-archive manifest.json")?;
entries.push(("manifest.json".to_string(), manifest_json));
```

The `repositories` entry is constructed at [`fetch.rs:1512–1516`](https://github.com/botworkz/tools/blob/a371902cce4aa6eb4107d23c23b22aa33b662a00/shasset/src/fetch.rs#L1512-L1516):

```rust
let repositories_json = serde_json::to_vec(&serde_json::json!({
    repo_name: { tag_name: last_layer_hex }
}))
.context("failed to serialize docker-archive repositories")?;
entries.push(("repositories".to_string(), repositories_json));
```

---

## 4. Manifest-digest preservation

### 4.1 The manifest digest is NOT in the output tar

The docker-archive `manifest.json` contains only one digest-like field:

```json
[{"Config":"<config_hex>.json","RepoTags":["botwork/session-broker:local"],"Layers":["<layer_hex>/layer.tar"]}]
```

The `Config` field is the **image config digest** (`sha256:` of the config blob), which is completely different from the **manifest digest** (`sha256:` of the manifest JSON) stored in `shasset.yaml`'s `digest:` field.

### 4.2 The manifest digest IS in the on-disk cache — but only there

`try_download_oci()` writes an oci-index cache entry at
[`fetch.rs:1406–1417`](https://github.com/botworkz/tools/blob/a371902cce4aa6eb4107d23c23b22aa33b662a00/shasset/src/fetch.rs#L1406-L1417):

```rust
std::fs::write(
    oci_index_path(cache_dir, &oci_ref.digest_hex, &platform_slug),
    &tar_hex,
)
```

This file (at `~/.cache/shasset/oci-index/<manifest_hex>.<platform_slug>`) maps the manifest digest hex to the assembled-tar sha256.
It is **only in the cache directory**, not inside the output tar.

### 4.3 What an assertion CAN and CANNOT check today

| Digest | Present in tar? | Checkable without shasset's cache? |
|--------|----------------|-------------------------------------|
| Image config digest (`sha256:` of config JSON) | ✅ Yes — as `<config_hex>.json` filename | ✅ Yes |
| Manifest digest (`sha256:` of manifest JSON) | ❌ No | ❌ No |

A hermetic CI assertion of the form "tar matches `shasset.yaml`'s `digest:` field" is **impossible** against a docker-archive tar because the manifest digest is absent from the tar.
An OCI image-layout tar places the manifest digest at `index.json` → `manifests[0].digest`, making the assertion trivially:

```python
idx = json.load(tar.extractfile("index.json"))
assert idx["manifests"][0]["digest"] == expected_digest  # from shasset.yaml
```

---

## 5. Switching cost

### 5.1 Diff estimate

The switch is confined to **one file** (`shasset/src/fetch.rs`) and amounts to:

| Change | Lines (approx) |
|--------|----------------|
| Replace `assemble_docker_archive()` function body ([`L1484–1558`](https://github.com/botworkz/tools/blob/a371902cce4aa6eb4107d23c23b22aa33b662a00/shasset/src/fetch.rs#L1484-L1558)) with `assemble_oci_archive()` | −75, +85 |
| Rename the call site at [`L1397`](https://github.com/botworkz/tools/blob/a371902cce4aa6eb4107d23c23b22aa33b662a00/shasset/src/fetch.rs#L1397) (and pass manifest bytes + manifest digest instead of only config hex + config bytes) | −1, +1 |
| Remove helper `build_layer_v1_json()` ([`L1561–1572`](https://github.com/botworkz/tools/blob/a371902cce4aa6eb4107d23c23b22aa33b662a00/shasset/src/fetch.rs#L1561-L1572)) | −12 |
| Thread manifest bytes to the call site through `try_download_oci()` | ~+3 |

**Total estimated diff: ~−90, +90 lines in one file.**  No `Cargo.toml` changes are needed; the `tar` and `serde_json` crates already present are sufficient.

The new `assemble_oci_archive()` would emit:

| OCI image-layout entry | Content |
|------------------------|---------|
| `oci-layout` | `{"imageLayoutVersion":"1.0.0"}` |
| `index.json` | `{"schemaVersion":2,"manifests":[{"mediaType":"application/vnd.oci.image.manifest.v1+json","digest":"sha256:<manifest_hex>","size":<n>,"annotations":{"org.opencontainers.image.ref.name":"botwork/<name>:local"}}]}` |
| `blobs/sha256/<manifest_hex>` | Raw manifest JSON bytes (already verified) |
| `blobs/sha256/<config_hex>` | Raw config blob bytes (already fetched) |
| `blobs/sha256/<layer_hex>` | **Compressed** layer blob bytes (see §8 note on compression) |

### 5.2 If shasset shelled out — it does not

Not applicable: there is no transport string to change.  The entire tar is built in Rust.

### 5.3 Tests to update

| Test | File:line | What it asserts | Impact |
|------|-----------|-----------------|--------|
| `oci_uri_pulls_manifest_then_config_then_layers` | [`fetch.rs:2390–2494`](https://github.com/botworkz/tools/blob/a371902cce4aa6eb4107d23c23b22aa33b662a00/shasset/src/fetch.rs#L2390-L2494) | Parses the tar and asserts `manifest.json` exists with correct `RepoTags` | **Must be rewritten** to assert `index.json` exists and `manifests[0].digest` equals the pinned manifest hex |
| `oci_uri_deterministic_tar_assembly` | [`fetch.rs:2844–2909`](https://github.com/botworkz/tools/blob/a371902cce4aa6eb4107d23c23b22aa33b662a00/shasset/src/fetch.rs#L2844-L2909) | Byte-for-byte equality across two fresh assemblies | Passes without change (still testing determinism) |
| All others (`oci_uri_self_verifies_manifest_digest`, `oci_uri_self_verifies_blob_digest`, `oci_uri_image_index_*`, `oci_new_form_*`, `oci_cache_compat_*`) | various | Fetch plumbing, retry, cache hits — do not inspect tar layout | Pass without change |

**No test fixtures are committed** to the repository; all OCI tests use in-memory mock data built at test time.

---

## 6. Reverse-compatibility risk

### 6.1 `docker load` — Docker Engine version minimum

OCI image-layout support in `docker load` was added in **Docker Engine 25.0** (January 2024).
Docker Engine < 25.0 will reject an oci-archive tar with:

```
invalid manifest: missing manifest.json
```

`botworkz/vm`'s provisioner at
[`images/_shared/provisioners/20-botwork-stack.sh`](https://github.com/botworkz/vm/blob/main/images/_shared/provisioners/20-botwork-stack.sh)
installs Docker CE **directly from Docker's official upstream apt repository** (not the distro package):

```bash
echo "deb [arch=$(dpkg --print-architecture) signed-by=/etc/apt/keyrings/docker.gpg] \
  https://download.docker.com/linux/debian ${VERSION_CODENAME} stable" \
  > /etc/apt/sources.list.d/docker.list
# …
eatmydata apt-get install -y --no-install-recommends docker-ce docker-ce-cli …
```

This always installs the latest Docker CE from Docker's repo; as of mid-2026 that is Docker 27.x.  There is **no version pinning** in the provisioner.  The format switch carries no Docker-version risk for `botworkz/vm`.

Debian 13's distro package (`docker.io`) would be Docker 25.x; Ubuntu 24.04's `docker.io` is Docker 24.x.  If any downstream consumer is using the distro package and not the Docker-upstream repo, Docker 24.x on Ubuntu 24.04 would break.

### 6.2 Shell scripts parsing `manifest.json`

No shell or Python scripts in `botworkz/tools` or `botworkz/vm` open the assembled tars and parse `manifest.json` directly.

The `botworkz/vm` provisioner ([PR#68 head, `images/_shared/provisioners/20-botwork-stack.sh`](https://github.com/botworkz/vm/blob/release-fix/images/_shared/provisioners/20-botwork-stack.sh#L52-L59)) does:

```bash
loaded_ref="$(/usr/bin/docker load -q -i "${local_tar}" | sed -n 's/^Loaded image: //p' | head -1)"
[[ -n "${loaded_ref}" ]] || { echo "could not parse loaded image ref for ${svc}" >&2; exit 1; }
docker tag "${loaded_ref}" "botwork/${svc}:local"
```

This parses the **`docker load -q` stdout**, not the tar itself.
For a docker-archive with `RepoTags: ["botwork/session-broker:local"]`, `docker load -q` emits `Loaded image: botwork/session-broker:local`.
For an oci-archive, `docker load -q` reads the tag from the `org.opencontainers.image.ref.name` annotation in `index.json`.
If the new `assemble_oci_archive()` sets that annotation to `botwork/<name>:local` (matching current `RepoTags` behaviour), `docker load -q` will still output `Loaded image: botwork/<name>:local` and the retag step works unchanged.
If the annotation is absent, `docker load -q` emits `Loaded image ID: sha256:<config_hex>` and the sed command produces an empty string, causing the provisioner to fail.

**The annotation must be included** in the new assembler.

### 6.3 Scripts depending on `<sha>.json` / `<sha>/layer.tar`

No scripts in the accessible repos parse the layer-directory structure inside the docker-archive tar.

### 6.4 Search of sibling repos

| Repo | File | Usage | Breaks with oci-archive? |
|------|------|-------|--------------------------|
| `botworkz/vm` | [`images/_shared/provisioners/20-botwork-stack.sh`](https://github.com/botworkz/vm/blob/main/images/_shared/provisioners/20-botwork-stack.sh#L42-L45) | `docker load -i *.tar` | ✅ No — Docker ≥ 25.0 accepts oci-archive |
| `botworkz/vm` (PR#68) | provisioner retag | parses `Loaded image: <ref>` from `docker load -q` | ⚠️ Only safe if `org.opencontainers.image.ref.name` annotation is included in `index.json` |
| `botworkz/vm` | [`scripts/lib/images.sh`](https://github.com/botworkz/vm/blob/main/scripts/lib/images.sh#L34-L37) | sibling mode: `docker save` → `docker load` (unrelated to shasset) | N/A |
| `botworkz/space` | (repository not accessible) | Unknown | Unknown |
| `botworkz/botforge` | Not a separate repository; `botforge` crate lives in this repo | Does not consume the tar output | N/A |

### 6.5 Phased rollout recommendation

A single-bump switch is safe for `botworkz/vm` given that it always uses Docker CE from the upstream repo.
For any other consumer that might be using an older Docker engine, a **phased approach** with an explicit `--format oci|docker` flag (defaulting to `docker` for one release cycle) would eliminate breakage risk.
Given the simplicity of the codebase and that there is only one known consumer, a single bump without a flag is the pragmatic choice, accompanied by a semver minor bump (not major — the output tar is a build artefact, not a library ABI).

---

## 7. Existing tests

### 7.1 Tests covering the `oci://` fetch path

All OCI tests are in `shasset/src/fetch.rs` in the `#[cfg(test)] mod tests` block starting at [line 1574](https://github.com/botworkz/tools/blob/a371902cce4aa6eb4107d23c23b22aa33b662a00/shasset/src/fetch.rs#L1574).

| Test name | Lines | What it asserts |
|-----------|-------|-----------------|
| `oci_uri_pulls_manifest_then_config_then_layers` | [2390–2494](https://github.com/botworkz/tools/blob/a371902cce4aa6eb4107d23c23b22aa33b662a00/shasset/src/fetch.rs#L2390-L2494) | Makes 3 HTTP requests (manifest, config, layer); asserts `manifest.json` in tar with correct `RepoTags` |
| `oci_uri_self_verifies_manifest_digest` | [2498–2544](https://github.com/botworkz/tools/blob/a371902cce4aa6eb4107d23c23b22aa33b662a00/shasset/src/fetch.rs#L2498-L2544) | Corrupt manifest bytes → "digest mismatch" error |
| `oci_uri_self_verifies_blob_digest` | [2548–2614](https://github.com/botworkz/tools/blob/a371902cce4aa6eb4107d23c23b22aa33b662a00/shasset/src/fetch.rs#L2548-L2614) | Corrupt layer bytes → "digest mismatch" error |
| `oci_uri_image_index_selects_matching_platform` | [2618–2715](https://github.com/botworkz/tools/blob/a371902cce4aa6eb4107d23c23b22aa33b662a00/shasset/src/fetch.rs#L2618-L2715) | Multi-arch index: correct child manifest selected; oci-index cache entry written |
| `oci_uri_image_index_errors_when_platform_not_found` | [2718–2765](https://github.com/botworkz/tools/blob/a371902cce4aa6eb4107d23c23b22aa33b662a00/shasset/src/fetch.rs#L2718-L2765) | Error when no matching platform child exists in index |
| `oci_uri_nested_image_index_is_rejected` | [2768–2841](https://github.com/botworkz/tools/blob/a371902cce4aa6eb4107d23c23b22aa33b662a00/shasset/src/fetch.rs#L2768-L2841) | Nested index → "nested image index not supported" error |
| `oci_uri_deterministic_tar_assembly` | [2844–2910](https://github.com/botworkz/tools/blob/a371902cce4aa6eb4107d23c23b22aa33b662a00/shasset/src/fetch.rs#L2844-L2910) | Two independent fetches of the same image produce byte-identical tars |
| `oci_new_form_issues_same_manifest_url_as_legacy_form` | [2913–2996](https://github.com/botworkz/tools/blob/a371902cce4aa6eb4107d23c23b22aa33b662a00/shasset/src/fetch.rs#L2913-L2996) | New `digest:` field form hits same manifest URL as legacy `@sha256:…` URI form |
| `oci_cache_compat_new_form_hits_legacy_cache` | [3002–3100](https://github.com/botworkz/tools/blob/a371902cce4aa6eb4107d23c23b22aa33b662a00/shasset/src/fetch.rs#L3002-L3100) | Cache populated via legacy form is a hit when re-fetched via new-form |

### 7.2 Test fixtures

**No image tar fixtures are committed** to the repository.
All OCI tests construct their test images in memory: `make_minimal_layer_tar()` ([`fetch.rs:2370–2387`](https://github.com/botworkz/tools/blob/a371902cce4aa6eb4107d23c23b22aa33b662a00/shasset/src/fetch.rs#L2370-L2387)) and `make_gzip()` ([`fetch.rs:2362–2368`](https://github.com/botworkz/tools/blob/a371902cce4aa6eb4107d23c23b22aa33b662a00/shasset/src/fetch.rs#L2362-L2368)) create minimal in-memory blobs, and mock digests are computed at runtime with `sha256_hex()` ([`fetch.rs:1683–1686`](https://github.com/botworkz/tools/blob/a371902cce4aa6eb4107d23c23b22aa33b662a00/shasset/src/fetch.rs#L1683-L1686)).

### 7.3 Test coverage gaps

- **No test asserts on `index.json`** (because no oci-archive path exists yet).
- **No test asserts the manifest digest appears anywhere in the output tar**.
  `oci_uri_pulls_manifest_then_config_then_layers` confirms `manifest.json` is present and `RepoTags` is correct, but does not validate that the registry manifest digest is preserved.
- **No end-to-end test loads the produced tar with `docker load`**.

The follow-up PR should add a test that:
1. Extracts `index.json` from the produced tar.
2. Asserts `index.json["manifests"][0]["digest"]` equals the `@sha256:…` hex used in the URI.

---

## 8. Open questions and surprises

### 8.1 Layer compression in the output tar

The current docker-archive stores layers **decompressed** (each `<layer_hex>/layer.tar` is the raw uncompressed tar).
An OCI image-layout tar typically stores layers **compressed** in `blobs/sha256/` (the blob matches what the registry served, i.e. `application/vnd.oci.image.layer.v1.tar+gzip`).
The follow-up PR must decide:

- Store compressed blobs (simpler — just keep the `layer_compressed` bytes already fetched and verified; no GzDecoder step needed) and update `mediaType` in the manifest blob accordingly.
- Store decompressed blobs (matches the current in-memory decompressed bytes, but requires computing a new `DiffID` sha256 and a new manifest blob pointing to the uncompressed layer, which changes the manifest digest from the registry's original — defeating the whole purpose).

**Recommendation: store compressed blobs**, using the already-fetched-and-verified `layer_compressed` bytes.  This means the `GzDecoder` decompression step in `try_download_oci()` at [`fetch.rs:1377–1390`](https://github.com/botworkz/tools/blob/a371902cce4aa6eb4107d23c23b22aa33b662a00/shasset/src/fetch.rs#L1377-L1390) can be removed entirely for the oci-archive path.

### 8.2 No `--format` flag exists today

The `FetchArgs` CLI struct ([`cli.rs:93–111`](https://github.com/botworkz/tools/blob/a371902cce4aa6eb4107d23c23b22aa33b662a00/shasset/src/cli.rs#L93-L111)) has no `--format` flag.
If a phased rollout is wanted, it would need to be added there.

### 8.3 No TODO/FIXME for oci-archive

A grep of `TODO`, `FIXME`, `oci-archive`, `oci_archive`, `oci archive` across all Rust source files returns no results — this is a greenfield addition, not a pre-planned one.

### 8.4 Multi-arch index: which manifest digest is used?

When `shasset.yaml` pins an **image index** (multi-arch manifest list), shasset re-fetches the platform-specific child manifest ([`fetch.rs:1264–1309`](https://github.com/botworkz/tools/blob/a371902cce4aa6eb4107d23c23b22aa33b662a00/shasset/src/fetch.rs#L1264-L1309)) and uses `effective_digest_hex` (the **child** manifest hex) as the key for the oci-index cache entry at [`fetch.rs:1412`](https://github.com/botworkz/tools/blob/a371902cce4aa6eb4107d23c23b22aa33b662a00/shasset/src/fetch.rs#L1412).

For the oci-archive `index.json`, the digest in `manifests[0].digest` should be the **child** manifest digest (single-platform manifest that was actually pulled) so that `docker load` can find the manifest blob in `blobs/sha256/`.
The registry-level manifest digest pinned in `shasset.yaml` would remain the **index** digest; an assertion must therefore compare against the child digest rather than the pinned index digest.
The follow-up PR must handle this carefully and document which digest is asserted in which context.

### 8.5 Signature verification (cosign/sigstore)

`shasset` performs no cosign or sigstore verification today.
Switching to oci-archive does not introduce or remove any such verification, but oci-archive is the prerequisite format for running `cosign verify` against a local tar in future, since cosign reads the manifest digest from `index.json`.

### 8.6 GHCR auth flow

Authentication to ghcr.io uses the OCI ****** challenge flow implemented at [`fetch.rs:1093–1197`](https://github.com/botworkz/tools/blob/a371902cce4aa6eb4107d23c23b22aa33b662a00/shasset/src/fetch.rs#L1093-L1197).
This flow is format-agnostic (it operates at the HTTP layer, not the tar layer) and will behave identically regardless of which tar format is assembled from the fetched bytes.

---

## 9. Recommended PR plan

### 9.1 Files to edit

| File | Current line range to modify | Change |
|------|------------------------------|--------|
| `shasset/src/fetch.rs` | [1395–1402](https://github.com/botworkz/tools/blob/a371902cce4aa6eb4107d23c23b22aa33b662a00/shasset/src/fetch.rs#L1395-L1402) — call site | Pass `manifest_bytes`, `manifest_hex` (the `effective_digest_hex`), and compressed layer bytes to new assembler |
| `shasset/src/fetch.rs` | [1377–1390](https://github.com/botworkz/tools/blob/a371902cce4aa6eb4107d23c23b22aa33b662a00/shasset/src/fetch.rs#L1377-L1390) — GzDecoder step | Remove decompression; store `layer_compressed` directly |
| `shasset/src/fetch.rs` | [1347](https://github.com/botworkz/tools/blob/a371902cce4aa6eb4107d23c23b22aa33b662a00/shasset/src/fetch.rs#L1347) — `layers` Vec type | Change `Vec<(String, Vec<u8>)>` element to carry compressed bytes |
| `shasset/src/fetch.rs` | [1473–1572](https://github.com/botworkz/tools/blob/a371902cce4aa6eb4107d23c23b22aa33b662a00/shasset/src/fetch.rs#L1473-L1572) — `assemble_docker_archive` + `build_layer_v1_json` | Delete both; add `assemble_oci_archive()` that emits `oci-layout`, `index.json`, `blobs/sha256/<manifest>`, `blobs/sha256/<config>`, `blobs/sha256/<layer…>` |
| `shasset/src/fetch.rs` | [2390–2494](https://github.com/botworkz/tools/blob/a371902cce4aa6eb4107d23c23b22aa33b662a00/shasset/src/fetch.rs#L2390-L2494) — `oci_uri_pulls_manifest_then_config_then_layers` test | Rewrite to assert `index.json` present; `manifests[0].digest` == pinned manifest hex; `oci-layout` present |

### 9.2 `--format` flag

Not recommended.
`botworkz/vm` is the only known consumer, and it installs Docker CE from the upstream apt repo (always ≥ 25.0).
A flag adds permanent maintenance surface for a one-time migration.
**Single-bump switch is preferred.**

### 9.3 Semver

Bump shasset's **minor version** (e.g. 0.4.x → 0.5.0).
The output tar format is a build artefact — it is not a library ABI — but it is an observable behaviour change for any consumer that reads the tar.  A minor bump is appropriate; a major bump is not warranted since the CLI interface is unchanged.

### 9.4 New tests to add

1. **Layout assertion test** — after fetch, extract the tar, assert:
   - `oci-layout` entry contains `{"imageLayoutVersion":"1.0.0"}`.
   - `index.json` entry has `manifests[0].digest == "sha256:<manifest_hex>"`.
   - `blobs/sha256/<manifest_hex>` entry exists and its sha256 matches the filename.
   - `blobs/sha256/<config_hex>` entry exists and its sha256 matches the filename.

2. **No `manifest.json` test** — assert `manifest.json` is **absent** (to prevent silent regression to docker-archive).

3. **`org.opencontainers.image.ref.name` annotation test** — assert `index.json`'s `manifests[0].annotations["org.opencontainers.image.ref.name"]` equals `"botwork/<name>:local"` so the provisioner retag step continues to work.

### 9.5 Downstream follow-ups

| Repo | PR needed | Description |
|------|-----------|-------------|
| `botworkz/vm` | Required before or alongside the shasset bump | Update the digest-assertion in `packer-build.yml` (and any future `release.yml` step) to read `index.json` from the oci-archive tar instead of parsing docker-archive `manifest.json` |
| `botworkz/vm` | Confirm PR#68 provisioner retag works | Verify `docker load -q` output format for oci-archive (requires `org.opencontainers.image.ref.name` annotation as noted in §6.2) |
| `botworkz/space` | Unknown | Repository not accessible; must be audited separately for any caller that parses the docker-archive layout |
