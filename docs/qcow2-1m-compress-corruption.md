# qcow2 1 MiB-cluster compress + sparsify/reclaim corruption

## Summary

`botforge build` could produce a structurally valid but guest-data-corrupt qcow2 when all of the following were true:

- `compress.enabled: true`
- `compressor_args.cluster_size: "1M"`
- `reclaim: fstrim`

`qemu-img check` still passed because the qcow2 metadata remained consistent; the corruption was in guest-visible data, not the container format itself.

## What was actually wrong

The earlier investigation over-attributed the failure to `read_guest_cluster()` returning zeroes for `QCOW_OFLAG_ZERO` entries. That behavior is correct per the qcow2 spec and remains unchanged: a `QCOW_OFLAG_ZERO` cluster is logically zero whether or not any stale physical bytes still exist underneath it. `fstrim` also only discards free space; it does not legitimately mark live guest data as zero.

The real defects were in the post-shutdown pipeline around `sparsify_zero_clusters()`:

1. **ZERO_ALLOC entries were not skipped early enough.**  
   `sparsify_zero_clusters()` skipped `l2_entry == 0` and compressed entries, then called `aligned_data_offset(l2_entry, cluster_size)`. For `host_offset | QCOW_OFLAG_ZERO`, masking bit 0 yielded a non-zero host offset, so the code treated the entry like an ordinary allocated cluster, read physical storage, and could deallocate or decrement refcounts for backing that should have been left alone.

2. **Refcount decrements used a stale refcount-table snapshot.**  
   The sparsify pass read the refcount table once, then mutated the image while continuing to resolve refcount blocks through that stale snapshot. That was unnecessarily risky for shared zero clusters and made the correctness story weaker than it needed to be.

3. **Tests never modeled qemu-style L2 entries.**  
   The existing fixtures wrote bare host offsets for plain clusters and never exercised `QCOW_OFLAG_COPIED`, `QCOW_OFLAG_ZERO`, or `ZERO_ALLOC` entries, so the real `fstrim` state was completely untested.

## Resolution

The fix shipped in this branch does all of the following:

- skips any `QCOW_OFLAG_ZERO` L2 entry in `sparsify_zero_clusters()` before touching physical storage, and records that via `skipped_zero_flag_clusters`
- resolves the refcount block fresh from disk for each `decrement_refcount()` call instead of using a stale table snapshot
- adds a post-shutdown sector-0 guard before sparsify/compress so an all-zero boot sector fails fast instead of producing a silently broken output image
- updates qcow2 fixtures to mark plain allocated clusters with `QCOW_OFLAG_COPIED`, matching qemu-generated images

## Regression coverage

The bug is reproduced by round-trip tests that run the same `sparsify_zero_clusters()` + `compress_qcow2_image()` pipeline over mixed live / `ZERO_PLAIN` / `ZERO_ALLOC` fixtures:

- `zero_flag_source_round_trip_zstd_4k`
- `zero_flag_source_round_trip_zlib_4k`
- `zero_flag_source_round_trip_zstd_1m`
- `zero_flag_source_round_trip_zlib_1m`

Those tests assert that:

- live clusters survive byte-for-byte
- `ZERO_PLAIN` and `ZERO_ALLOC` clusters still read back as logical zeroes
- shared zero-cluster refcounts are not decremented spuriously

Additional coverage:

- `shared_zero_plain_cluster_decrements_refcount_to_zero`
- `qemu_generated_zero_flag_round_trip_zstd_1m` (gated on `qemu-img`/`qemu-io`)

## Takeaway

`qemu-img check` was never sufficient for this bug class. The durable regression signal is guest-data round-trip integrity through `SourceImage::read_virtual_range()`, especially in the presence of qemu-generated zero-flagged L2 entries.
