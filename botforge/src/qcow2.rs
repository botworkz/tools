use anyhow::{bail, Context, Result};
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

const QCOW_MAGIC: u32 = 0x5146_49fb;
const QCOW_OFLAG_COMPRESSED: u64 = 1u64 << 62;
const QCOW_DATA_OFFSET_MASK: u64 = (1u64 << 62) - 1;
const DEFAULT_REFCOUNT_ORDER: u32 = 4;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct ZeroClusterSparsifyStats {
    pub(crate) scanned_clusters: u64,
    pub(crate) deallocated_clusters: u64,
    pub(crate) skipped_compressed_clusters: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Qcow2ImageStats {
    pub(crate) virtual_size: u64,
    pub(crate) disk_size: u64,
    pub(crate) cluster_size: u64,
    pub(crate) allocated_data_clusters: u64,
}

#[derive(Debug, Clone, Copy)]
struct Qcow2Header {
    virtual_size: u64,
    cluster_bits: u32,
    l1_size: u32,
    l1_table_offset: u64,
    refcount_table_offset: u64,
    refcount_table_clusters: u32,
    refcount_order: u32,
}

impl Qcow2Header {
    fn cluster_size(self) -> u64 {
        1u64 << self.cluster_bits
    }
}

pub(crate) fn sparsify_zero_clusters(path: &Path) -> Result<ZeroClusterSparsifyStats> {
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .with_context(|| format!("cannot open qcow2 for sparsify: {}", path.display()))?;
    let header = read_header(&mut file)?;
    let cluster_size = header.cluster_size();
    let l2_entries_per_table = cluster_size
        .checked_div(8)
        .context("invalid qcow2 cluster size for L2 table entries")?;
    let refcount_bits = refcount_bits(header.refcount_order)?;
    if refcount_bits % 8 != 0 {
        bail!(
            "unsupported qcow2 refcount order {} ({} bits): only byte-aligned refcounts are supported",
            header.refcount_order,
            refcount_bits
        );
    }
    let refcount_bytes = (refcount_bits / 8) as usize;
    let refcount_entries_per_block = cluster_size
        .checked_mul(8)
        .context("refcount block entry count overflow")?
        / refcount_bits;

    let l1_entries = read_u64_table(
        &mut file,
        header.l1_table_offset,
        usize::try_from(header.l1_size).context("l1_size does not fit usize")?,
    )?;
    let refcount_table_entries = read_u64_table(
        &mut file,
        header.refcount_table_offset,
        usize::try_from(header.refcount_table_clusters)
            .context("refcount_table_clusters does not fit usize")?
            .checked_mul(usize::try_from(l2_entries_per_table).context("cluster size too large")?)
            .context("refcount table entry count overflow")?,
    )?;

    let mut cluster_buf =
        vec![0u8; usize::try_from(cluster_size).context("cluster size too large")?];
    let mut stats = ZeroClusterSparsifyStats::default();

    for l1_entry in l1_entries {
        let l2_offset = aligned_data_offset(l1_entry, cluster_size);
        if l2_offset == 0 {
            continue;
        }
        let l2_count = usize::try_from(l2_entries_per_table).context("l2 entries overflow")?;
        for i in 0..l2_count {
            let l2e_offset = l2_offset
                .checked_add(u64::try_from(i).context("l2 index overflow")? * 8)
                .context("l2 entry offset overflow")?;
            let l2_entry = read_u64_at(&mut file, l2e_offset)?;
            if l2_entry == 0 {
                continue;
            }
            if (l2_entry & QCOW_OFLAG_COMPRESSED) != 0 {
                stats.skipped_compressed_clusters += 1;
                continue;
            }
            let data_offset = aligned_data_offset(l2_entry, cluster_size);
            if data_offset == 0 {
                continue;
            }
            stats.scanned_clusters += 1;
            if !cluster_is_all_zero(&mut file, data_offset, &mut cluster_buf)? {
                continue;
            }
            write_u64_at(&mut file, l2e_offset, 0)?;
            decrement_refcount(
                &mut file,
                &refcount_table_entries,
                data_offset / cluster_size,
                refcount_entries_per_block,
                refcount_bytes,
                cluster_size,
            )?;
            stats.deallocated_clusters += 1;
        }
    }
    Ok(stats)
}

pub(crate) fn read_qcow2_image_stats(path: &Path) -> Result<Qcow2ImageStats> {
    let mut file = File::open(path)
        .with_context(|| format!("cannot open qcow2 for stats: {}", path.display()))?;
    let header = read_header(&mut file)?;
    let cluster_size = header.cluster_size();
    let l2_entries_per_table = cluster_size
        .checked_div(8)
        .context("invalid qcow2 cluster size for L2 table entries")?;
    let l1_entries = read_u64_table(
        &mut file,
        header.l1_table_offset,
        usize::try_from(header.l1_size).context("l1_size does not fit usize")?,
    )?;

    let mut allocated_data_clusters = 0u64;
    for l1_entry in l1_entries {
        let l2_offset = aligned_data_offset(l1_entry, cluster_size);
        if l2_offset == 0 {
            continue;
        }
        let l2_count = usize::try_from(l2_entries_per_table).context("l2 entries overflow")?;
        for i in 0..l2_count {
            let l2e_offset = l2_offset
                .checked_add(u64::try_from(i).context("l2 index overflow")? * 8)
                .context("l2 entry offset overflow")?;
            let l2_entry = read_u64_at(&mut file, l2e_offset)?;
            if l2_entry == 0 {
                continue;
            }
            if (l2_entry & QCOW_OFLAG_COMPRESSED) != 0
                || aligned_data_offset(l2_entry, cluster_size) != 0
            {
                allocated_data_clusters += 1;
            }
        }
    }

    let disk_size = file
        .metadata()
        .with_context(|| format!("cannot stat qcow2: {}", path.display()))?
        .len();
    Ok(Qcow2ImageStats {
        virtual_size: header.virtual_size,
        disk_size,
        cluster_size,
        allocated_data_clusters,
    })
}

fn read_header(file: &mut File) -> Result<Qcow2Header> {
    let mut fixed = [0u8; 104];
    read_exact_at(file, 0, &mut fixed)?;
    let magic = u32::from_be_bytes(fixed[0..4].try_into().expect("slice length"));
    if magic != QCOW_MAGIC {
        bail!("not a qcow2 image (bad magic)");
    }
    let version = u32::from_be_bytes(fixed[4..8].try_into().expect("slice length"));
    if version != 2 && version != 3 {
        bail!("unsupported qcow2 version {version}");
    }
    let cluster_bits = u32::from_be_bytes(fixed[20..24].try_into().expect("slice length"));
    if !(9..=21).contains(&cluster_bits) {
        bail!("unsupported qcow2 cluster_bits {cluster_bits}");
    }

    let refcount_order = if version >= 3 {
        u32::from_be_bytes(fixed[96..100].try_into().expect("slice length"))
    } else {
        DEFAULT_REFCOUNT_ORDER
    };

    Ok(Qcow2Header {
        virtual_size: u64::from_be_bytes(fixed[24..32].try_into().expect("slice length")),
        cluster_bits,
        l1_size: u32::from_be_bytes(fixed[36..40].try_into().expect("slice length")),
        l1_table_offset: u64::from_be_bytes(fixed[40..48].try_into().expect("slice length")),
        refcount_table_offset: u64::from_be_bytes(fixed[48..56].try_into().expect("slice length")),
        refcount_table_clusters: u32::from_be_bytes(
            fixed[56..60].try_into().expect("slice length"),
        ),
        refcount_order,
    })
}

fn refcount_bits(refcount_order: u32) -> Result<u64> {
    let bits = 1u64
        .checked_shl(refcount_order)
        .with_context(|| format!("invalid refcount_order {refcount_order}"))?;
    if bits == 0 || bits > 64 {
        bail!("unsupported refcount width: {bits}");
    }
    Ok(bits)
}

fn aligned_data_offset(entry: u64, cluster_size: u64) -> u64 {
    (entry & QCOW_DATA_OFFSET_MASK) & !(cluster_size - 1)
}

fn decrement_refcount(
    file: &mut File,
    refcount_table_entries: &[u64],
    cluster_index: u64,
    entries_per_block: u64,
    refcount_bytes: usize,
    cluster_size: u64,
) -> Result<()> {
    let block_index = usize::try_from(cluster_index / entries_per_block)
        .context("refcount block index does not fit usize")?;
    let block_entry = *refcount_table_entries.get(block_index).with_context(|| {
        format!("missing refcount table entry for cluster index {cluster_index}")
    })?;
    let block_offset = aligned_data_offset(block_entry, cluster_size);
    if block_offset == 0 {
        bail!("missing refcount block for allocated cluster index {cluster_index}");
    }
    let entry_in_block = cluster_index % entries_per_block;
    let value_offset = block_offset
        .checked_add(
            entry_in_block
                .checked_mul(u64::try_from(refcount_bytes).context("refcount bytes overflow")?)
                .context("refcount entry offset overflow")?,
        )
        .context("refcount absolute offset overflow")?;

    let mut raw = vec![0u8; refcount_bytes];
    read_exact_at(file, value_offset, &mut raw)?;
    let value = read_be_uint(&raw);
    if value == 0 {
        bail!("attempted to decrement zero refcount for cluster index {cluster_index}");
    }
    write_exact_at(
        file,
        value_offset,
        &write_be_uint(value - 1, refcount_bytes),
    )?;
    Ok(())
}

fn cluster_is_all_zero(file: &mut File, offset: u64, buf: &mut [u8]) -> Result<bool> {
    read_exact_at(file, offset, buf)?;
    Ok(buf.iter().all(|b| *b == 0))
}

fn read_u64_table(file: &mut File, offset: u64, count: usize) -> Result<Vec<u64>> {
    let byte_len = count.checked_mul(8).context("table byte length overflow")?;
    let mut raw = vec![0u8; byte_len];
    read_exact_at(file, offset, &mut raw)?;
    let mut out = Vec::with_capacity(count);
    for chunk in raw.chunks_exact(8) {
        out.push(u64::from_be_bytes(chunk.try_into().expect("slice length")));
    }
    Ok(out)
}

fn read_u64_at(file: &mut File, offset: u64) -> Result<u64> {
    let mut raw = [0u8; 8];
    read_exact_at(file, offset, &mut raw)?;
    Ok(u64::from_be_bytes(raw))
}

fn write_u64_at(file: &mut File, offset: u64, value: u64) -> Result<()> {
    write_exact_at(file, offset, &value.to_be_bytes())
}

fn read_be_uint(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0u64, |acc, b| (acc << 8) | u64::from(*b))
}

fn write_be_uint(value: u64, width: usize) -> Vec<u8> {
    let mut out = vec![0u8; width];
    for (idx, byte) in out.iter_mut().enumerate() {
        let shift = (width - idx - 1) * 8;
        *byte = ((value >> shift) & 0xff) as u8;
    }
    out
}

fn read_exact_at(file: &mut File, offset: u64, buf: &mut [u8]) -> Result<()> {
    file.seek(SeekFrom::Start(offset))
        .with_context(|| format!("cannot seek to offset {offset}"))?;
    file.read_exact(buf)
        .with_context(|| format!("cannot read {} bytes at offset {offset}", buf.len()))
}

fn write_exact_at(file: &mut File, offset: u64, buf: &[u8]) -> Result<()> {
    file.seek(SeekFrom::Start(offset))
        .with_context(|| format!("cannot seek to offset {offset}"))?;
    file.write_all(buf)
        .with_context(|| format!("cannot write {} bytes at offset {offset}", buf.len()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn deallocates_zero_clusters_and_preserves_non_zero_clusters() {
        let tmp = tempdir().expect("tempdir");
        let image = tmp.path().join("sample.qcow2");
        write_test_image(&image).expect("write test qcow2");

        let stats = sparsify_zero_clusters(&image).expect("sparsify");
        assert_eq!(stats.scanned_clusters, 2);
        assert_eq!(stats.deallocated_clusters, 1);
        assert_eq!(stats.skipped_compressed_clusters, 0);

        let mut file = File::open(&image).expect("open image");
        let header = read_header(&mut file).expect("header");
        let cluster_size = header.cluster_size();
        let l2_offset = aligned_data_offset(
            read_u64_at(&mut file, header.l1_table_offset).expect("l1 entry"),
            cluster_size,
        );
        let first = read_u64_at(&mut file, l2_offset).expect("l2e0");
        let second = read_u64_at(&mut file, l2_offset + 8).expect("l2e1");
        assert_eq!(first, 0, "zero data cluster should be deallocated");
        assert_ne!(aligned_data_offset(second, cluster_size), 0);

        let refcount_block = aligned_data_offset(
            read_u64_at(&mut file, header.refcount_table_offset).expect("refcount table entry"),
            cluster_size,
        );
        let zero_cluster_index = 5u64;
        let nonzero_cluster_index = 6u64;
        let zero_refcount = read_exact_refcount16(&mut file, refcount_block, zero_cluster_index);
        let nonzero_refcount =
            read_exact_refcount16(&mut file, refcount_block, nonzero_cluster_index);
        assert_eq!(zero_refcount, 0);
        assert_eq!(nonzero_refcount, 1);
    }

    #[test]
    fn read_stats_reports_expected_allocated_cluster_count() {
        let tmp = tempdir().expect("tempdir");
        let image = tmp.path().join("sample.qcow2");
        write_test_image(&image).expect("write test qcow2");
        let before = read_qcow2_image_stats(&image).expect("stats before");
        assert_eq!(before.cluster_size, 4096);
        assert_eq!(before.allocated_data_clusters, 2);

        sparsify_zero_clusters(&image).expect("sparsify");
        let after = read_qcow2_image_stats(&image).expect("stats after");
        assert_eq!(after.allocated_data_clusters, 1);
        assert_eq!(after.virtual_size, 8192);
        assert!(after.disk_size >= 7 * 4096);
    }

    fn write_test_image(path: &Path) -> Result<()> {
        let cluster_size = 4096u64;
        let mut file = File::create(path)?;
        file.set_len(cluster_size * 7)?;

        // Header (v3, 104-byte header)
        write_exact_at(&mut file, 0, &QCOW_MAGIC.to_be_bytes())?;
        write_exact_at(&mut file, 4, &3u32.to_be_bytes())?;
        write_exact_at(&mut file, 20, &12u32.to_be_bytes())?;
        write_exact_at(&mut file, 24, &(cluster_size * 2).to_be_bytes())?;
        write_exact_at(&mut file, 36, &1u32.to_be_bytes())?;
        write_exact_at(&mut file, 40, &cluster_size.to_be_bytes())?; // L1 @ cluster 1
        write_exact_at(&mut file, 48, &(cluster_size * 3).to_be_bytes())?; // refcount table @ cluster 3
        write_exact_at(&mut file, 56, &1u32.to_be_bytes())?; // one refcount table cluster
        write_exact_at(&mut file, 96, &4u32.to_be_bytes())?; // 16-bit refcounts
        write_exact_at(&mut file, 100, &104u32.to_be_bytes())?;

        // L1 table entry -> L2 table @ cluster 2
        write_u64_at(&mut file, cluster_size, cluster_size * 2)?;

        // L2 entries: cluster 0 => zero data cluster @ cluster 5, cluster 1 => non-zero @ cluster 6
        write_u64_at(&mut file, cluster_size * 2, cluster_size * 5)?;
        write_u64_at(&mut file, cluster_size * 2 + 8, cluster_size * 6)?;

        // Refcount table entry -> refcount block @ cluster 4
        write_u64_at(&mut file, cluster_size * 3, cluster_size * 4)?;

        // Refcounts in refcount block (16-bit entries)
        for idx in 0u64..=6 {
            write_exact_at(&mut file, cluster_size * 4 + idx * 2, &1u16.to_be_bytes())?;
        }

        // cluster 6 is non-zero
        write_exact_at(&mut file, cluster_size * 6, &[0x5a; 32])?;
        Ok(())
    }

    fn read_exact_refcount16(file: &mut File, block_offset: u64, cluster_index: u64) -> u16 {
        let mut raw = [0u8; 2];
        read_exact_at(file, block_offset + (cluster_index * 2), &mut raw).expect("read refcount");
        u16::from_be_bytes(raw)
    }
}
