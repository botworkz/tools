//! Minimal qcow2 header manipulation.
//!
//! `botforge build` only ever needs to grow the declared virtual size of a
//! qcow2 before handing it to `virt-customize`. That is a single big-endian
//! `u64` write to the header at offset `0x18`; no clusters are allocated,
//! no refcounts touched, no in-guest filesystem is involved. Anything more
//! sophisticated belongs in a real qcow2 library or in the appliance.
//!
//! Doing this in-process drops the `qemu-img` dependency from `botforge
//! build`'s code path. `qemu-utils` stays in the image because `botforge
//! run` / `botforge test` still call `qemu-img create` to build qcow2
//! overlays; the eventual goal is for `build` to be runnable as a static
//! binary outside the container, and this is the first step toward that.

use anyhow::{bail, Context, Result};
use std::fs::OpenOptions;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

/// qcow2 magic bytes: `"QFI\xfb"`.
const QCOW2_MAGIC: [u8; 4] = *b"QFI\xfb";

/// Byte offset of the big-endian `u64` `size` field in the qcow2 header
/// (the *virtual* disk size, not the on-host file size). Stable across
/// qcow2 v2 and v3 — lives in the v2-mandatory portion of the header,
/// not in any extension. See `docs/interop/qcow2.txt` in the qemu tree.
const QCOW2_SIZE_OFFSET: u64 = 0x18;

/// Grow the declared virtual size of a qcow2 image to `new_size_bytes`.
///
/// Equivalent to `qemu-img resize <disk> <new_size>` when
/// `new_size >= current_size`. Refuses to shrink: shrinking a qcow2 in
/// place requires rewriting L1/L2 tables and refcount blocks, which is
/// firmly out of scope here.
///
/// No clusters are allocated; the extra range reads as zero until something
/// writes to it, which is exactly what cloud-init / growpart inside the
/// guest expect.
pub(crate) fn grow_qcow2_virtual_size(disk: &Path, new_size_bytes: u64) -> Result<()> {
    let mut f = OpenOptions::new()
        .read(true)
        .write(true)
        .open(disk)
        .with_context(|| format!("cannot open qcow2: {}", disk.display()))?;

    let mut magic = [0u8; 4];
    f.read_exact(&mut magic)
        .with_context(|| format!("cannot read qcow2 magic from {}", disk.display()))?;
    if magic != QCOW2_MAGIC {
        bail!("not a qcow2 image: {}", disk.display());
    }

    let mut version_bytes = [0u8; 4];
    f.read_exact(&mut version_bytes)
        .with_context(|| format!("cannot read qcow2 version from {}", disk.display()))?;
    let version = u32::from_be_bytes(version_bytes);
    if version != 2 && version != 3 {
        bail!(
            "unsupported qcow2 version {} in {}; only v2 and v3 are handled",
            version,
            disk.display()
        );
    }

    f.seek(SeekFrom::Start(QCOW2_SIZE_OFFSET))?;
    let mut size_buf = [0u8; 8];
    f.read_exact(&mut size_buf)
        .with_context(|| format!("cannot read qcow2 virtual size from {}", disk.display()))?;
    let current_size = u64::from_be_bytes(size_buf);

    if new_size_bytes < current_size {
        bail!(
            "refusing to shrink qcow2 {} from {} to {} bytes",
            disk.display(),
            current_size,
            new_size_bytes
        );
    }
    if new_size_bytes == current_size {
        return Ok(());
    }

    f.seek(SeekFrom::Start(QCOW2_SIZE_OFFSET))?;
    f.write_all(&new_size_bytes.to_be_bytes())
        .with_context(|| format!("cannot rewrite qcow2 virtual size in {}", disk.display()))?;
    f.sync_all()
        .with_context(|| format!("cannot fsync qcow2 {}", disk.display()))?;
    Ok(())
}

/// Parse a qemu-style size string (`10G`, `512M`, `1024K`, `1073741824`)
/// into bytes. Suffixes use binary (1024-based) multipliers, matching
/// `qemu-img resize`.
pub(crate) fn parse_size(s: &str) -> Result<u64> {
    let s = s.trim();
    if s.is_empty() {
        bail!("empty size string");
    }

    let bytes = s.as_bytes();
    let last = bytes[bytes.len() - 1];
    let (number, multiplier) = match last {
        b'k' | b'K' => (&s[..s.len() - 1], 1024u64),
        b'm' | b'M' => (&s[..s.len() - 1], 1024u64.pow(2)),
        b'g' | b'G' => (&s[..s.len() - 1], 1024u64.pow(3)),
        b't' | b'T' => (&s[..s.len() - 1], 1024u64.pow(4)),
        b'p' | b'P' => (&s[..s.len() - 1], 1024u64.pow(5)),
        b'e' | b'E' => (&s[..s.len() - 1], 1024u64.pow(6)),
        b'0'..=b'9' => (s, 1u64),
        _ => bail!("unrecognised size suffix in '{}'", s),
    };

    let n: u64 = number
        .parse()
        .with_context(|| format!("invalid size '{}': not a u64", s))?;
    n.checked_mul(multiplier)
        .with_context(|| format!("size '{}' overflows u64", s))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Seek, SeekFrom, Write};
    use tempfile::NamedTempFile;

    /// Build the smallest synthetic qcow2 v3 header that our reader will
    /// accept: magic, version=3, plausible cluster_bits, and the `size`
    /// field at offset 0x18. The rest of the qcow2 structure (L1,
    /// refcount table, clusters) is irrelevant to a header-only rewrite.
    fn make_qcow2_with_size(initial_virtual_size: u64) -> NamedTempFile {
        let mut f = NamedTempFile::new().unwrap();
        // v3 header is 104 bytes; round up to 0x70 for headroom.
        let mut header = vec![0u8; 0x70];
        header[0..4].copy_from_slice(b"QFI\xfb");
        // version = 3.
        header[4..8].copy_from_slice(&3u32.to_be_bytes());
        // cluster_bits = 16 means 64 KiB clusters; this matches qemu's default.
        header[0x14..0x18].copy_from_slice(&16u32.to_be_bytes());
        header[0x18..0x20].copy_from_slice(&initial_virtual_size.to_be_bytes());
        f.write_all(&header).unwrap();
        f.flush().unwrap();
        f
    }

    fn read_virtual_size(path: &std::path::Path) -> u64 {
        let mut f = std::fs::File::open(path).unwrap();
        f.seek(SeekFrom::Start(QCOW2_SIZE_OFFSET)).unwrap();
        let mut buf = [0u8; 8];
        std::io::Read::read_exact(&mut f, &mut buf).unwrap();
        u64::from_be_bytes(buf)
    }

    #[test]
    fn grow_rewrites_virtual_size_in_header() {
        let tmp = make_qcow2_with_size(1024u64.pow(3)); // 1 GiB
        let new_size = 10 * 1024u64.pow(3); // 10 GiB
        grow_qcow2_virtual_size(tmp.path(), new_size).unwrap();
        assert_eq!(read_virtual_size(tmp.path()), new_size);
    }

    #[test]
    fn grow_is_idempotent_at_same_size() {
        let initial = 8 * 1024u64.pow(3);
        let tmp = make_qcow2_with_size(initial);
        grow_qcow2_virtual_size(tmp.path(), initial).unwrap();
        assert_eq!(read_virtual_size(tmp.path()), initial);
    }

    #[test]
    fn grow_refuses_to_shrink() {
        let tmp = make_qcow2_with_size(10 * 1024u64.pow(3));
        let err = grow_qcow2_virtual_size(tmp.path(), 1024u64.pow(3)).unwrap_err();
        assert!(err.to_string().contains("refusing to shrink"), "{err}");
    }

    #[test]
    fn grow_rejects_non_qcow2() {
        let mut tmp = NamedTempFile::new().unwrap();
        tmp.write_all(b"this is not a qcow2 file at all").unwrap();
        tmp.flush().unwrap();
        let err = grow_qcow2_virtual_size(tmp.path(), 1024).unwrap_err();
        assert!(err.to_string().contains("not a qcow2"), "{err}");
    }

    #[test]
    fn grow_rejects_unknown_qcow2_version() {
        let mut tmp = NamedTempFile::new().unwrap();
        let mut header = vec![0u8; 0x70];
        header[0..4].copy_from_slice(b"QFI\xfb");
        header[4..8].copy_from_slice(&99u32.to_be_bytes()); // bogus version
        tmp.write_all(&header).unwrap();
        tmp.flush().unwrap();
        let err = grow_qcow2_virtual_size(tmp.path(), 1024).unwrap_err();
        assert!(
            err.to_string().contains("unsupported qcow2 version"),
            "{err}"
        );
    }

    #[test]
    fn grow_only_touches_the_size_field() {
        // The rest of the header must be left untouched: regressions in the
        // seek/write logic would silently corrupt qcow2 metadata otherwise.
        let initial = 1024u64.pow(3);
        let tmp = make_qcow2_with_size(initial);
        let before = std::fs::read(tmp.path()).unwrap();

        let new_size = 5 * 1024u64.pow(3);
        grow_qcow2_virtual_size(tmp.path(), new_size).unwrap();
        let after = std::fs::read(tmp.path()).unwrap();

        assert_eq!(before.len(), after.len(), "file length must not change");
        assert_eq!(
            before[..QCOW2_SIZE_OFFSET as usize],
            after[..QCOW2_SIZE_OFFSET as usize],
            "header bytes before the size field must be unchanged"
        );
        assert_eq!(
            before[QCOW2_SIZE_OFFSET as usize + 8..],
            after[QCOW2_SIZE_OFFSET as usize + 8..],
            "header bytes after the size field must be unchanged"
        );
    }

    #[test]
    fn parse_size_accepts_qemu_suffixes() {
        assert_eq!(parse_size("0").unwrap(), 0);
        assert_eq!(parse_size("1024").unwrap(), 1024);
        assert_eq!(parse_size("1k").unwrap(), 1024);
        assert_eq!(parse_size("1K").unwrap(), 1024);
        assert_eq!(parse_size("1M").unwrap(), 1024 * 1024);
        assert_eq!(parse_size("1G").unwrap(), 1024u64.pow(3));
        assert_eq!(parse_size("10G").unwrap(), 10 * 1024u64.pow(3));
        assert_eq!(parse_size("1T").unwrap(), 1024u64.pow(4));
        // Whitespace is tolerated, mirroring `qemu-img`'s lenient parser.
        assert_eq!(parse_size("  10G  ").unwrap(), 10 * 1024u64.pow(3));
    }

    #[test]
    fn parse_size_rejects_garbage() {
        assert!(parse_size("").is_err());
        assert!(parse_size("abc").is_err());
        assert!(parse_size("10X").is_err());
        assert!(parse_size("--5G").is_err());
    }

    #[test]
    fn parse_size_detects_overflow() {
        // 99999999999 * 1024^6 overflows u64.
        assert!(parse_size("99999999999E").is_err());
    }
}
