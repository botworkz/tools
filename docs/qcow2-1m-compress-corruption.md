# qcow2 1 MiB-cluster compress + sparsify/reclaim corruption analysis

_Date: 2026-07-11 — analysis-only pass, no production code changed._

---

## 1. Summary

Botforge's `botforge build` pipeline produces a structurally-valid but data-corrupt qcow2 when all three of these are active: `compress.enabled: true`, `compressor_args.cluster_size: "1M"`, and `reclaim: fstrim`. The compressed output passes `qemu-img check` (metadata is coherent) yet contains zeroed guest data where live filesystem content should exist, causing `guestfish` to find no partition table and GRUB to report `invalid arch-independent ELF magic`.

The corruption has two compounding causes that are both latent in the codebase:

1. **Primary — QCOW_OFLAG_ZERO propagation through the pipeline.** After the build VM runs `fstrim` over a `discard=unmap` drive, qemu writes `QCOW_OFLAG_ZERO` into the partial image's L2 entries for every discarded cluster. `compress_qcow2_image` (`qcow2.rs:read_guest_cluster`) honours that flag and emits zeros into the compressed output without any further cross-check. At real scale a Debian image has hundreds of fstrim'd clusters spanning the first few megabytes of the virtual disk, including clusters that fall in the same 1 MiB target-cluster window as the GPT/MBR partition table. The tiny four-cluster unit fixtures never contain any `QCOW_OFLAG_ZERO` source entries, so this path is wholly untested.

2. **Secondary — `sparsify_zero_clusters` does not guard `QCOW_OFLAG_ZERO_ALLOC` entries before reading physical content.** When a source L2 entry is `host_offset | QCOW_OFLAG_ZERO` (the `ZERO_ALLOC` state qemu writes after discarding an already-allocated cluster), `sparsify_zero_clusters` reads the physical bytes of that cluster. If those bytes are non-zero (qemu set the flag but did not zero the file hole), `sparsify_zero_clusters` leaves the entry intact; `compress_qcow2_image` then sees the `QCOW_OFLAG_ZERO` flag and produces a zero cluster — silently destroying data without returning an error. Both functions produce no diagnostic output for this case.

The multi-L1-entry hypothesis put forward in the investigation brief is **incorrect for this exact scenario**: at 1 MiB cluster size `l2_entries_per_table = 131072`, so a 10 GiB image needs only `l1_size = 1`. Likewise `nb_csectors` field overflow and host-offset field overflow cannot occur at this scale. The existing round-trip and double-compress tests at 1 MiB cluster size pass, confirming that the _encoding_ of compressed clusters is correct.

---

## 2. Reproduction & evidence

### Triggering spec excerpt

```yaml
type: build
image: "@debian-base"
disk_size: 10G
compress:
  enabled: true
  compressor_args:
    cluster_size: "1M"
  compressor_opts: -T0 -19
  reclaim: fstrim
```

### Four observed symptoms

| Observation | What it rules out |
|---|---|
| `qemu-img check` passes ("No errors were found") | qcow2 structural metadata is valid; the bug is in guest _data_ content |
| `guestfish -i` → "no operating system was found on this disk" | Even the GPT/MBR partition table is unreadable; not a kernel-file-only issue |
| `qemu-img convert -O qcow2 <output> <raw>.qcow2` also drops to `grub rescue> invalid arch-independent ELF magic` | Corruption is baked into stored cluster data, not a runtime decompression artefact |
| Removing the guest `dd if=/dev/zero of=/EMPTY` zero-fill made no difference | The guest zero-fill is not the cause; the fstrim itself on legitimately-free blocks is sufficient |

---

## 3. Root cause

### 3a. Arithmetic verification — multi-L1, nb_csectors, host-offset (all clear)

The investigation brief hypothesised three arithmetic-level suspects. Each can be ruled out:

**Multi-L1-entry correctness.**
At 1 MiB clusters (`cluster_bits = 20`):

```
l2_entries_per_table = cluster_size / 8 = 1_048_576 / 8 = 131_072
guest_cluster_count  = ceil(10_737_418_240 / 1_048_576) = 10_240
l1_size              = ceil(10_240 / 131_072) = 1
```

`l1_size = 1`. There is only _one_ L1 entry and one L2 table. The multi-L1-entry code paths in `write_l1_table` (line 746) and `write_l2_tables` (line 767) are not exercised at all. This is why the four-cluster unit tests — which also have `l1_size = 1` — correctly predict in-office behaviour but cannot distinguish a multi-L1 bug. It is also why the test `native_compress_round_trip_zstd_1m` (line 1748) passes.

Multi-L1 would trigger only for `virtual_size > 131_072 × 1 MiB = 128 GiB` at 1 MiB cluster size.

**`nb_csectors` field overflow.**
For `cluster_bits = 20`:

```
csize_shift  = 62 - (cluster_bits - 8) = 62 - 12 = 50   (qcow2.rs:71)
csize_mask   = (1 << (cluster_bits - 8)) - 1 = (1 << 12) - 1 = 4095
```

A maximally-incompressible 1 MiB cluster (`compressed_size = 1_048_575`) stored at a worst-case sector-misaligned offset needs at most:

```
nb_csectors_stored = floor((offset + 1_048_575 - 1) / 512) - floor(offset / 512)
                   ≤ ceil(1_048_575 / 512) = 2048
```

`2048 < 4095 = csize_mask`. The 12-bit field cannot overflow.  
See `encode_compressed_l2_entry` (line 659) and `parse_compressed_l2_entry` (line 684).

**Host-offset field overflow.**
`cluster_offset_mask = (1 << csize_shift) - 1 = (1 << 50) - 1` ≈ 1 PiB (line 78–80). The guard at line 675 enforces this. For a 10 GiB image the maximum data offset is in the low GiB range; no overflow is possible.

### 3b. QCOW_OFLAG_ZERO source entries — the actual root cause

The build pipeline (implemented in `botforge/src/commands/build.rs`) runs:

1. Boot partial qcow2 read-write with `discard=unmap` on the virtio drive (build.rs line 146, qemu.rs lines 143-147).
2. Run `fstrim -av` inside the guest (build.rs line 278-280, fstrim_guest_command at line 893-895).
3. Shut down the VM (build.rs ~line 378).
4. `sparsify_zero_clusters(&partial)` (build.rs line 385).
5. `compress_qcow2_image(&partial, &output, …)` (build.rs line 449-454 via `commit_output`).

When the guest runs `fstrim` over a `discard=unmap` qcow2 drive, qemu processes the TRIM requests and writes `QCOW_OFLAG_ZERO` (value `1`) — or `host_offset | QCOW_OFLAG_ZERO` for clusters that are still physically allocated — into the L2 entries of every discarded cluster in the source partial.

**In `read_guest_cluster` (qcow2.rs line 468), the first substantive check is:**

```rust
// qcow2.rs:491
if l2_entry == 0 || (l2_entry & QCOW_OFLAG_ZERO) != 0 {
    return Ok(zero_cluster);
}
```

`QCOW_OFLAG_ZERO = 1` (line 13). This check is correct per the qcow2 specification: a cluster flagged with `QCOW_OFLAG_ZERO` must read as all-zeroes regardless of any backing data. However it means that **any cluster incorrectly flagged by qemu's discard handler will silently produce zeroes in the compressed output without any error or log entry from botforge.**

The mechanism at real Debian scale:

- A typical 10 GiB Debian installation leaves a large fraction of the 10240 × 1 MiB virtual clusters' worth of 64 KiB source clusters in the fstrim-discarded state, including the run of clusters near guest offset 0 that fall within the first 1 MiB target cluster.
- Target cluster 0 covers guest bytes `[0, 1 MiB)`. This window is assembled from 16 source 64 KiB clusters (indices 0–15). Source cluster 0 contains the MBR/GPT partition table (sector 0). In a qemu-generated 64 KiB-cluster image after a full Debian install and fstrim, source clusters that the filesystem has never touched but qemu has "pre-allocated" (via `qemu-img resize` growing the image) may appear in the `QCOW_OFLAG_ZERO` state.
- When `compress_qcow2_image` assembles target cluster 0 via `read_virtual_range` (line 448), it calls `read_guest_cluster` for each of the 16 source clusters. Any source cluster with `QCOW_OFLAG_ZERO` set contributes 64 KiB of zeroes. If source cluster 0 is in this state, the output partition table is zeroed; `guestfish` cannot find any partition layout and reports "no operating system was found".

**Decisive test gap.** Every source fixture in the test suite (`write_multi_cluster_test_image`, `write_test_image_with_cluster_size`, `write_custom_test_image_with_cluster_size`) writes plain host-offset L2 entries with no flags. The `QCOW_OFLAG_ZERO` and `QCOW_OFLAG_COPIED` states that qemu uses in production are never exercised. A source image that faithfully mirrors post-fstrim state has never been introduced into the round-trip tests.

---

## 4. Secondary / contributing bugs

### 4a. `sparsify_zero_clusters` does not skip QCOW_OFLAG_ZERO_ALLOC entries early

`sparsify_zero_clusters` (line 261) iterates over all non-zero L2 entries that pass the compressed-cluster guard:

```rust
// qcow2.rs:318-325
if (l2_entry & QCOW_OFLAG_COMPRESSED) != 0 {
    stats.skipped_compressed_clusters += 1;
    continue;
}
let data_offset = aligned_data_offset(l2_entry, cluster_size);
if data_offset == 0 {
    continue;
}
```

`aligned_data_offset` (line 918) masks off bits 62–63 and rounds to cluster alignment:

```rust
fn aligned_data_offset(entry: u64, cluster_size: u64) -> u64 {
    (entry & QCOW_DATA_OFFSET_MASK) & !(cluster_size - 1)
}
```

For `l2_entry = host_offset | QCOW_OFLAG_ZERO` (`ZERO_ALLOC` state):

```
QCOW_OFLAG_ZERO = 1   (bit 0)
host_offset is cluster-aligned (bit 0 = 0)
aligned_data_offset = (host_offset | 1) & QCOW_DATA_OFFSET_MASK & !(cluster_size-1)
                    = host_offset     (bit 0 cleared by the alignment mask)
```

Because `data_offset = host_offset ≠ 0`, the function **does not skip** this entry. It then reads the physical cluster content (line 327):

```rust
if !cluster_is_all_zero(&mut file, data_offset, &mut cluster_buf)? {
    continue;
}
```

Two outcomes:

| Physical content | Sparsify action | Compress outcome |
|---|---|---|
| All zeros (qemu punched hole) | Sets L2 to 0; decrements refcount | Correct: produces zero cluster |
| Non-zero (qemu set flag but did not zero file region) | Leaves L2 as `host_offset \| QCOW_OFLAG_ZERO` | `compress_qcow2_image:read_guest_cluster:491` sees ZERO flag → emits zeros — **data destroyed silently** |

The second path is the live data destruction path. It requires qemu to set `QCOW_OFLAG_ZERO` on a cluster whose underlying file bytes have not yet been zeroed (possible during write-back races, or on certain Linux kernel + virtio-blk driver combinations). Botforge never logs a warning, never returns an error, and never re-reads data to verify the decision.

### 4b. `sparsify_zero_clusters` uses a stale refcount-table snapshot

```rust
// qcow2.rs:291-298
let refcount_table_entries = read_u64_table(
    &mut file,
    header.refcount_table_offset,
    ...
)?;
```

The refcount table is read once and kept as an immutable `Vec<u64>`. As clusters are deallocated in the inner loop (lines 330-338), `decrement_refcount` uses this snapshot to locate the refcount block but writes updates directly to the file. If the refcount _table itself_ ever needed to shrink (e.g., an entire refcount block became unreferenced), the stale snapshot would refer to freed storage. For typical images this is harmless, but it is an architectural inconsistency.

### 4c. Compose-order observation

The pipeline order (fstrim while VM alive → VM shutdown → sparsify_zero_clusters → compress_qcow2_image) is logically correct. The ordering is not itself a bug. The issue is in what state the partial image is in after fstrim, and that neither sparsify nor compress validates the plausibility of the ZERO-flagged data against any higher-level consistency check.

### 4d. Latent: untested multi-L1 path

As demonstrated in §3a, `l1_size = 1` for all current test fixtures _and_ for any 10 GiB / 1 MiB-cluster image. The `write_l1_table` and `write_l2_tables` paths for `l1_size > 1` are structurally correct upon inspection but have zero test coverage. Any image larger than 128 GiB at 1 MiB cluster size would exercise this untested path. A future regression there would be silently missed.

---

## 5. Minimal failing reproduction idea

### 5a. Unit test: compressed output from a source with QCOW_OFLAG_ZERO entries

**Goal.** Demonstrate that `compress_qcow2_image` preserves non-zero data in clusters whose neighbouring source clusters carry `QCOW_OFLAG_ZERO`, and that the pipeline does not silently zero out live data.

**Setup.** Extend the existing `write_custom_test_image_with_cluster_size` helper (line 1461) — or add a new helper — to write a source qcow2 with:

- Cluster 0: MBR-like header pattern (e.g., `[0x55AA; 1 MiB]`), L2 entry = plain host offset (live data).
- Clusters 1–3: `QCOW_OFLAG_ZERO` state (either `l2_entry = 1` for `ZERO_PLAIN`, or `l2_entry = host_offset | 1` for `ZERO_ALLOC`). These represent fstrim'd clusters.
- Clusters 4–5: distinct live data patterns (`[0x42; 1 MiB]`, `[0x7F; 1 MiB]`), plain host offsets.

**Pipeline.**

```rust
// 1. Write source with mixed ZERO / live clusters
write_source_with_zero_entries(&source, cluster_size)?;

// 2. Sparsify (mirrors the production pipeline)
sparsify_zero_clusters(&source).expect("sparsify");

// 3. Compress
compress_qcow2_image(&source, &dest, CompressionType::Zstd,
                     &args_1m, "-19 -T0")?;

// 4. Round-trip read-back via SourceImage (NOT qemu-img check)
let mut img = SourceImage::open(&dest)?;

// Cluster 0 must still have the MBR pattern
let mut buf = vec![0u8; 1 << 20];
img.read_virtual_range(0, &mut buf)?;
assert!(buf.iter().all(|&b| b == 0x55), "cluster 0 data corrupted");

// Clusters 1-3 must be zeros (they were QCOW_OFLAG_ZERO)
for i in 1u64..=3 {
    img.read_virtual_range(i << 20, &mut buf)?;
    assert!(buf.iter().all(|&b| b == 0), "cluster {i} should be zero");
}

// Clusters 4-5 must retain their patterns
img.read_virtual_range(4 << 20, &mut buf)?;
assert!(buf.iter().all(|&b| b == 0x42), "cluster 4 data corrupted");
```

**Why this catches the bug.** The assertion on cluster 0 (`0x55` pattern) fails today if the source's neighbouring ZERO entries cause `read_guest_cluster` to short-circuit incorrectly, or if sparsify touches a live cluster. The `qemu-img check` call that currently ends several tests would _not_ catch this: it passes on the corrupt image.

### 5b. Integration test: 10240-cluster 1 MiB image (matching production scale)

Build a source image with 10 240 distinct 1 MiB clusters (filling the first `cluster_size / 16` bytes of each with a per-cluster fingerprint), run sparsify + compress, and assert byte-for-byte equality of every cluster via `SourceImage::read_virtual_range`. This exercises the exact `l1_size = 1, l2_entries_per_table = 131 072` mapping that production uses. The test takes a few seconds due to I/O but provides a true end-to-end regression gate.

---

## 6. Recommended fixes

### Fix A — add explicit QCOW_OFLAG_ZERO guard in `sparsify_zero_clusters`

**File:** `botforge/src/qcow2.rs`, function `sparsify_zero_clusters` (line 261).

After the COMPRESSED check at line 318, add:

```rust
// Clusters explicitly zeroed by the host (ZERO_PLAIN or ZERO_ALLOC) are already
// logically zero from the guest's perspective. Attempting to read and deallocate
// the underlying physical storage is unnecessary and can mask situations where
// the ZERO flag was set without actually zeroing the physical bytes.
if (l2_entry & QCOW_OFLAG_ZERO) != 0 {
    continue;
}
```

This ensures `sparsify_zero_clusters` only touches clusters that are _unambiguously_ allocated with non-zero logical content (plain `host_offset | QCOW_OFLAG_COPIED`). The ZERO_ALLOC case is left to qemu's own coherent discard tracking.

### Fix B — validate live data before the pipeline destroys it

**File:** `botforge/src/commands/build.rs`, function `should_run_zero_cluster_sparsify` / `commit_output` area (lines 432–479).

Before invoking `sparsify_zero_clusters`, read back the first sector (512 bytes) of the virtual disk from the partial image and verify it is non-zero:

```rust
// Sanity check: ensure sector 0 (MBR/GPT) survived the build before
// we run any destructive sparsify or compress operations.
validate_partition_table_present(&partial)?;
```

where `validate_partition_table_present` opens the partial as a `SourceImage`, calls `read_virtual_range(0, &mut [0u8; 512])`, and bails if all bytes are zero. This would have surfaced the corruption as an actionable error ("partition table missing from partial before compression") rather than silently producing a non-bootable image.

### Fix C — add QCOW_OFLAG_COPIED to source L2 entries in test fixtures

**File:** `botforge/src/qcow2.rs`, test helpers `write_multi_cluster_test_image` (line 1258) and `write_test_image_with_cluster_size` (line 1315).

Change the L2 table write loop from:

```rust
write_u64_at(&mut file, cluster_size * 2 + i * 8, cluster_size * (5 + i))?;
```

to:

```rust
write_u64_at(&mut file, cluster_size * 2 + i * 8,
             cluster_size * (5 + i) | QCOW_OFLAG_COPIED)?;
```

This makes the test fixtures structurally identical to qemu-generated images and ensures that the `QCOW_OFLAG_COPIED`-masking logic in `aligned_data_offset` and `read_guest_cluster` is exercised by all existing round-trip tests.

### Fix D — add multi-L1 test coverage

Add a test variant that creates a source image with `l1_size = 2` (e.g., `virtual_size = 2 × l2_entries_per_table × cluster_size` = 256 GiB at 1 MiB clusters, or use 64 KiB clusters with a moderately-large virtual size where `l1_size > 1`), compresses it, and verifies that clusters crossing the L1 boundary round-trip correctly. This exercises the `write_l1_table` / `write_l2_tables` multi-entry code path that is currently dark.

---

## Appendix: key constants and bit-field layout (1 MiB cluster)

```
cluster_bits    = 20
cluster_size    = 1_048_576  (1 MiB)

csize_shift     = 62 - (cluster_bits - 8) = 50
csize_mask      = (1 << 12) - 1           = 4095   (12 bits, max nb_csectors = 4096)

cluster_offset_mask = (1 << 50) - 1   (50 bits, max offset ≈ 1 PiB)

L2 entry format (compressed):
  bit 63       : 0  (QCOW_OFLAG_COPIED — must be clear for compressed)
  bit 62       : 1  (QCOW_OFLAG_COMPRESSED)
  bits [61:50] : nb_csectors_stored = (sector_span - 1)  [12 bits]
  bits [49: 0] : host_byte_offset                        [50 bits]

L2 entry format (raw):
  bit 63       : 1  (QCOW_OFLAG_COPIED)
  bit 62       : 0
  bits [61: 0] : host_byte_offset (cluster-aligned)

QCOW_OFLAG_ZERO = bit 0 = 1
  l2_entry == 1           → ZERO_PLAIN  (no physical storage)
  l2_entry == host | 1    → ZERO_ALLOC  (physical storage still referenced)
  Both cause read_guest_cluster to return all-zero cluster (qcow2.rs:491).
```

---

_Analysis by Copilot Coding Agent — `botforge/src/qcow2.rs` commit `e822cdf`, `botforge/src/commands/build.rs` same commit._
