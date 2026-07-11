# QCOW2 compression corruption — correction and resolution

## Correction

The previous `1M/fstrim/QCOW_OFLAG_ZERO` root-cause analysis was incorrect.

Corruption is reproducible with:

- `compress.enabled: true`
- `compressor: zlib`
- default cluster size (no `cluster_size` override)
- `reclaim: none`

So reclaim/sparsify/trim paths are not required to trigger the bug.

## Actual defect

`botforge` wrote zlib-compressed qcow2 cluster payloads using **raw deflate** streams instead of zlib-wrapped streams.

- Encoder path used `flate2::write::DeflateEncoder`
- Decoder path used `flate2::read::DeflateDecoder`

That internal pair masked the issue in existing tests, but external qcow2 consumers (for example guest tools/qemu stack) treat these clusters as invalid/corrupt.

## Reproduction test coverage added

`botforge/src/qcow2.rs` now includes realistic-data round-trip tests that:

- build dense full-length clusters (MBR-like sector with `0x55AA`, incompressible cluster, dense non-zero compressible cluster),
- run standalone `compress_qcow2_image` (no reclaim),
- verify full-cluster guest-byte equality across all guest clusters via `SourceImage::read_virtual_range`,
- exercise both default target cluster size and source/target cluster-size mismatch,
- run for both zlib and zstd,
- and, critically, assert compressed entries decode with the declared codec format (strict zlib/zstd decode).

These tests fail on the old zlib deflate implementation and pass with the fix.

## Fix implemented

- Switched zlib qcow2 codec paths to zlib framing:
  - `flate2::write::ZlibEncoder`
  - `flate2::read::ZlibDecoder`

## Permanent build guard retained

`botforge/src/commands/build.rs` now validates guest sector 0 is non-zero before and after compression via a new helper in `botforge/src/qcow2.rs`:

- `pub(crate) fn read_virtual_sector0(path: &Path) -> Result<[u8; 512]>`

Build now fails fast if compression would produce an image with an all-zero virtual sector 0.
