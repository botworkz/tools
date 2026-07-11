use anyhow::{bail, Context, Result};
use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use crate::compressor::{build_compressor, decompress_cluster};
use crate::plan::config::CompressionType;

const QCOW_MAGIC: u32 = 0x5146_49fb;
const QCOW_OFLAG_COPIED: u64 = 1u64 << 63;
const QCOW_OFLAG_COMPRESSED: u64 = 1u64 << 62;
const QCOW_OFLAG_ZERO: u64 = 1;
const QCOW_DATA_OFFSET_MASK: u64 = (1u64 << 62) - 1;
const QCOW2_COMPRESSED_SECTOR_SIZE: u64 = 512;
const QCOW2_INCOMPAT_DATA_FILE: u64 = 1u64 << 2;
const QCOW2_INCOMPAT_COMPRESSION: u64 = 1u64 << 3;
const QCOW2_INCOMPAT_EXTL2: u64 = 1u64 << 4;
const QCOW2_COMPRESSION_TYPE_ZLIB: u8 = 0;
const QCOW2_COMPRESSION_TYPE_ZSTD: u8 = 1;
const DEFAULT_REFCOUNT_ORDER: u32 = 4;
const QCOW_HEADER_LENGTH_V3: u32 = 112;

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
    backing_file_offset: u64,
    virtual_size: u64,
    cluster_bits: u32,
    l1_size: u32,
    l1_table_offset: u64,
    refcount_table_offset: u64,
    refcount_table_clusters: u32,
    incompatible_features: u64,
    refcount_order: u32,
    header_length: u32,
    compression_type: u8,
}

impl Qcow2Header {
    fn cluster_size(self) -> u64 {
        1u64 << self.cluster_bits
    }

    fn compression_type(self) -> Result<CompressionType> {
        if (self.incompatible_features & QCOW2_INCOMPAT_COMPRESSION) == 0 {
            return Ok(CompressionType::Zlib);
        }
        match self.compression_type {
            QCOW2_COMPRESSION_TYPE_ZLIB => Ok(CompressionType::Zlib),
            QCOW2_COMPRESSION_TYPE_ZSTD => Ok(CompressionType::Zstd),
            other => bail!("unsupported qcow2 compression type {other}"),
        }
    }

    fn csize_shift(self) -> u32 {
        62 - (self.cluster_bits - 8)
    }

    fn csize_mask(self) -> u64 {
        (1u64 << (self.cluster_bits - 8)) - 1
    }

    fn cluster_offset_mask(self) -> u64 {
        (1u64 << self.csize_shift()) - 1
    }
}

pub(crate) fn compress_qcow2_image(
    source: &Path,
    dest: &Path,
    compression_type: CompressionType,
    compressor_args: &BTreeMap<String, String>,
    compressor_opts: &str,
) -> Result<()> {
    let mut source_image = SourceImage::open(source)?;
    source_image.ensure_supported()?;

    let cluster_size =
        resolve_target_cluster_size(source_image.header.cluster_size(), compressor_args)?;
    let virtual_size = source_image.header.virtual_size;
    let l2_entries_per_table = cluster_size
        .checked_div(8)
        .context("invalid qcow2 cluster size for L2 tables")?;
    let guest_cluster_count = div_ceil(virtual_size, cluster_size);
    let l1_size = div_ceil(guest_cluster_count, l2_entries_per_table);
    let l1_table_clusters = div_ceil(
        l1_size
            .checked_mul(8)
            .context("l1 table byte length overflow")?,
        cluster_size,
    );
    let l1_table_offset = cluster_size;
    let first_l2_offset = l1_table_offset
        .checked_add(
            l1_table_clusters
                .checked_mul(cluster_size)
                .context("l1 table cluster span overflow")?,
        )
        .context("l2 table offset overflow")?;
    let data_start = first_l2_offset
        .checked_add(
            l1_size
                .checked_mul(cluster_size)
                .context("l2 table span overflow")?,
        )
        .context("data area offset overflow")?;

    let mut output = OpenOptions::new()
        .create(true)
        .truncate(true)
        .read(true)
        .write(true)
        .open(dest)
        .with_context(|| format!("cannot create compressed qcow2: {}", dest.display()))?;
    let compressor = build_compressor(compression_type, compressor_opts)?;
    let _compressor_id = compressor.id();

    let total_l2_entries = usize::try_from(
        l1_size
            .checked_mul(l2_entries_per_table)
            .context("l2 entry count overflow")?,
    )
    .context("l2 entry count too large")?;
    let mut l2_entries = vec![0u64; total_l2_entries];
    let mut refcounts = BTreeMap::<u64, u64>::new();
    let mut cluster_buf =
        vec![0u8; usize::try_from(cluster_size).context("cluster size too large")?];
    let mut allocator = DataAllocator::new(cluster_size, data_start);

    for guest_cluster_index in 0..guest_cluster_count {
        cluster_buf.fill(0);
        let guest_offset = guest_cluster_index
            .checked_mul(cluster_size)
            .context("guest offset overflow")?;
        let remaining = virtual_size.saturating_sub(guest_offset);
        let to_read = remaining.min(cluster_size) as usize;
        source_image.read_virtual_range(guest_offset, &mut cluster_buf[..to_read])?;
        if cluster_buf.iter().all(|b| *b == 0) {
            continue;
        }

        let compressed = compressor.compress_cluster(&cluster_buf)?;
        let entry = if compressed.len() < cluster_buf.len() {
            let host_offset = allocator.allocate_compressed(compressed.len() as u64)?;
            write_exact_at(&mut output, host_offset, &compressed)?;
            increment_refcount_range(
                &mut refcounts,
                host_offset,
                compressed.len() as u64,
                cluster_size,
            )?;
            encode_compressed_l2_entry(host_offset, compressed.len() as u64, cluster_size)?
        } else {
            let host_offset = allocator.allocate_raw()?;
            write_exact_at(&mut output, host_offset, &cluster_buf)?;
            increment_refcount_range(&mut refcounts, host_offset, cluster_size, cluster_size)?;
            host_offset | QCOW_OFLAG_COPIED
        };
        l2_entries
            [usize::try_from(guest_cluster_index).context("guest cluster index too large")?] =
            entry;
    }

    let refcount_table_offset = align_up(allocator.next_offset, cluster_size);
    let host_clusters_before_refcounts = refcount_table_offset / cluster_size;
    let (refcount_table_clusters, refcount_block_clusters, total_clusters) =
        compute_refcount_layout(host_clusters_before_refcounts, cluster_size)?;
    let refcount_block_offset = refcount_table_offset
        .checked_add(
            refcount_table_clusters
                .checked_mul(cluster_size)
                .context("refcount table span overflow")?,
        )
        .context("refcount block offset overflow")?;
    let file_len = refcount_block_offset
        .checked_add(
            refcount_block_clusters
                .checked_mul(cluster_size)
                .context("refcount block span overflow")?,
        )
        .context("qcow2 file length overflow")?;
    output
        .set_len(file_len)
        .with_context(|| format!("cannot resize compressed qcow2: {}", dest.display()))?;

    increment_refcount_range(&mut refcounts, 0, data_start, cluster_size)?;
    increment_refcount_range(
        &mut refcounts,
        refcount_table_offset,
        refcount_table_clusters
            .checked_add(refcount_block_clusters)
            .context("refcount metadata cluster count overflow")?
            .checked_mul(cluster_size)
            .context("refcount metadata size overflow")?,
        cluster_size,
    )?;

    write_header(
        &mut output,
        HeaderWriteSpec {
            compression_type,
            cluster_size,
            virtual_size,
            l1_size,
            l1_table_offset,
            refcount_table_offset,
            refcount_table_clusters,
        },
    )?;
    write_l1_table(
        &mut output,
        l1_table_offset,
        first_l2_offset,
        l1_size,
        cluster_size,
    )?;
    write_l2_tables(
        &mut output,
        first_l2_offset,
        l1_size,
        cluster_size,
        &l2_entries,
    )?;
    write_refcount_table(
        &mut output,
        refcount_table_offset,
        refcount_block_offset,
        refcount_table_clusters,
        refcount_block_clusters,
        cluster_size,
    )?;
    write_refcount_blocks(
        &mut output,
        refcount_block_offset,
        refcount_block_clusters,
        cluster_size,
        total_clusters,
        &refcounts,
    )?;
    output
        .sync_all()
        .with_context(|| format!("cannot flush compressed qcow2: {}", dest.display()))?;
    Ok(())
}

pub(crate) fn read_virtual_sector0(path: &Path) -> Result<[u8; 512]> {
    let mut image = SourceImage::open(path)?;
    image.ensure_supported()?;
    let mut sector = [0u8; 512];
    image.read_virtual_range(0, &mut sector)?;
    Ok(sector)
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

struct SourceImage {
    file: File,
    header: Qcow2Header,
    l1_entries: Vec<u64>,
}

impl SourceImage {
    fn open(path: &Path) -> Result<Self> {
        let mut file = File::open(path).with_context(|| {
            format!(
                "cannot open qcow2 for native compression: {}",
                path.display()
            )
        })?;
        let header = read_header(&mut file)?;
        let l1_entries = read_u64_table(
            &mut file,
            header.l1_table_offset,
            usize::try_from(header.l1_size).context("l1_size does not fit usize")?,
        )?;
        Ok(Self {
            file,
            header,
            l1_entries,
        })
    }

    fn ensure_supported(&self) -> Result<()> {
        if self.header.header_length < 104 {
            bail!(
                "native qcow2 compression requires qcow2 header_length >= 104, got {}",
                self.header.header_length
            );
        }
        if self.header.backing_file_offset != 0 {
            bail!("native qcow2 compression does not yet support backing files");
        }
        let unsupported = self.header.incompatible_features
            & !(QCOW2_INCOMPAT_COMPRESSION | QCOW2_INCOMPAT_DATA_FILE);
        if unsupported != 0 {
            bail!(
                "native qcow2 compression does not yet support incompatible qcow2 features: 0x{unsupported:x}"
            );
        }
        if (self.header.incompatible_features & QCOW2_INCOMPAT_DATA_FILE) != 0 {
            bail!("native qcow2 compression does not yet support external qcow2 data files");
        }
        if (self.header.incompatible_features & QCOW2_INCOMPAT_EXTL2) != 0 {
            bail!("native qcow2 compression does not yet support extended L2 qcow2 images");
        }
        let _ = self.header.compression_type()?;
        Ok(())
    }

    fn read_virtual_range(&mut self, guest_offset: u64, buf: &mut [u8]) -> Result<()> {
        let mut cursor = 0usize;
        let source_cluster_size = self.header.cluster_size();
        while cursor < buf.len() {
            let offset = guest_offset
                .checked_add(u64::try_from(cursor).context("guest range cursor overflow")?)
                .context("guest range offset overflow")?;
            let cluster_index = offset / source_cluster_size;
            let offset_in_cluster = usize::try_from(offset % source_cluster_size)
                .context("cluster offset does not fit usize")?;
            let source_cluster = self.read_guest_cluster(cluster_index)?;
            let chunk =
                (buf.len() - cursor).min(source_cluster.len().saturating_sub(offset_in_cluster));
            buf[cursor..cursor + chunk]
                .copy_from_slice(&source_cluster[offset_in_cluster..offset_in_cluster + chunk]);
            cursor += chunk;
        }
        Ok(())
    }

    fn read_guest_cluster(&mut self, cluster_index: u64) -> Result<Vec<u8>> {
        let cluster_size = self.header.cluster_size();
        let mut zero_cluster =
            vec![0u8; usize::try_from(cluster_size).context("cluster size too large")?];
        let l2_entries_per_table = cluster_size
            .checked_div(8)
            .context("invalid qcow2 cluster size for L2 tables")?;
        let l1_index = usize::try_from(cluster_index / l2_entries_per_table)
            .context("l1 index does not fit usize")?;
        let Some(&l1_entry) = self.l1_entries.get(l1_index) else {
            return Ok(zero_cluster);
        };
        let l2_offset = aligned_data_offset(l1_entry, cluster_size);
        if l2_offset == 0 {
            return Ok(zero_cluster);
        }
        let l2_index = cluster_index % l2_entries_per_table;
        let l2_entry = read_u64_at(
            &mut self.file,
            l2_offset
                .checked_add(l2_index.checked_mul(8).context("l2 index overflow")?)
                .context("l2 entry offset overflow")?,
        )?;
        if l2_entry == 0 || (l2_entry & QCOW_OFLAG_ZERO) != 0 {
            return Ok(zero_cluster);
        }
        if (l2_entry & QCOW_OFLAG_COMPRESSED) != 0 {
            let (compressed_offset, compressed_size) =
                parse_compressed_l2_entry(self.header, l2_entry)?;
            let mut compressed = vec![
                0u8;
                usize::try_from(compressed_size)
                    .context("compressed cluster too large")?
            ];
            read_exact_at(&mut self.file, compressed_offset, &mut compressed)?;
            return decompress_cluster(
                self.header.compression_type()?,
                &compressed,
                usize::try_from(cluster_size).context("cluster size too large")?,
            );
        }
        let data_offset = aligned_data_offset(l2_entry, cluster_size);
        if data_offset == 0 {
            return Ok(zero_cluster);
        }
        read_exact_at(&mut self.file, data_offset, &mut zero_cluster)?;
        Ok(zero_cluster)
    }
}

#[derive(Debug, Clone, Copy)]
struct DataAllocator {
    cluster_size: u64,
    next_offset: u64,
}

impl DataAllocator {
    fn new(cluster_size: u64, next_offset: u64) -> Self {
        Self {
            cluster_size,
            next_offset,
        }
    }

    fn allocate_compressed(&mut self, size: u64) -> Result<u64> {
        if size == 0 || size > self.cluster_size {
            bail!("compressed qcow2 cluster has invalid size {size}");
        }
        let cluster_start = align_down(self.next_offset, self.cluster_size);
        let used = self.next_offset.saturating_sub(cluster_start);
        let offset = if used != 0 && used + size <= self.cluster_size {
            self.next_offset
        } else {
            align_up(self.next_offset, self.cluster_size)
        };
        // Advance next_offset to the next 512-byte sector boundary so that no two
        // compressed clusters share a sector.  qemu reads each compressed cluster by
        // computing nb_csectors from the L2 entry and reading nb_csectors*512 bytes
        // starting at the sector-aligned base; if adjacent clusters shared a sector,
        // qemu's zstd decoder would receive trailing bytes from the next cluster's
        // frame and return -EIO.
        self.next_offset = align_up(
            offset
                .checked_add(size)
                .context("compressed data offset overflow")?,
            QCOW2_COMPRESSED_SECTOR_SIZE,
        );
        Ok(offset)
    }

    fn allocate_raw(&mut self) -> Result<u64> {
        let offset = align_up(self.next_offset, self.cluster_size);
        self.next_offset = offset
            .checked_add(self.cluster_size)
            .context("raw data offset overflow")?;
        Ok(offset)
    }
}

#[derive(Debug, Clone, Copy)]
struct HeaderWriteSpec {
    compression_type: CompressionType,
    cluster_size: u64,
    virtual_size: u64,
    l1_size: u64,
    l1_table_offset: u64,
    refcount_table_offset: u64,
    refcount_table_clusters: u64,
}

fn resolve_target_cluster_size(
    source_cluster_size: u64,
    compressor_args: &BTreeMap<String, String>,
) -> Result<u64> {
    let mut cluster_size = source_cluster_size;
    for (key, value) in compressor_args {
        match key.as_str() {
            "cluster_size" => cluster_size = parse_cluster_size(value)?,
            other => bail!("unsupported qcow2 structural compression option '{other}'"),
        }
    }
    Ok(cluster_size)
}

fn parse_cluster_size(raw: &str) -> Result<u64> {
    let raw = raw.trim();
    if raw.is_empty() {
        bail!("cluster_size cannot be empty");
    }
    let split_idx = raw.find(|c: char| !c.is_ascii_digit()).unwrap_or(raw.len());
    let (digits, suffix) = raw.split_at(split_idx);
    let mut value = digits
        .parse::<u64>()
        .with_context(|| format!("invalid qcow2 cluster_size '{raw}'"))?;
    let suffix = suffix.to_ascii_lowercase();
    value = match suffix.as_str() {
        "" => value,
        "k" => value.checked_mul(1024).context("cluster_size overflow")?,
        "m" => value
            .checked_mul(1024)
            .and_then(|v| v.checked_mul(1024))
            .context("cluster_size overflow")?,
        "g" => value
            .checked_mul(1024)
            .and_then(|v| v.checked_mul(1024))
            .and_then(|v| v.checked_mul(1024))
            .context("cluster_size overflow")?,
        _ => bail!("invalid qcow2 cluster_size suffix in '{raw}'"),
    };
    if !(512..=2 * 1024 * 1024).contains(&value) || !value.is_power_of_two() {
        bail!("invalid qcow2 cluster_size '{raw}': expected a power of two between 512 and 2M");
    }
    Ok(value)
}

fn compute_refcount_layout(
    host_clusters_before_refcounts: u64,
    cluster_size: u64,
) -> Result<(u64, u64, u64)> {
    let entries_per_refcount_block = cluster_size
        .checked_div(2)
        .context("invalid refcount block entry size")?;
    let entries_per_refcount_table_cluster = cluster_size
        .checked_div(8)
        .context("invalid refcount table entry size")?;
    let mut refcount_block_clusters = 1u64;
    let mut refcount_table_clusters = 1u64;
    loop {
        let total_clusters = host_clusters_before_refcounts
            .checked_add(refcount_table_clusters)
            .and_then(|v| v.checked_add(refcount_block_clusters))
            .context("qcow2 cluster count overflow")?;
        let needed_refcount_block_clusters = div_ceil(total_clusters, entries_per_refcount_block);
        let needed_refcount_table_clusters = div_ceil(
            needed_refcount_block_clusters,
            entries_per_refcount_table_cluster,
        );
        if needed_refcount_block_clusters == refcount_block_clusters
            && needed_refcount_table_clusters == refcount_table_clusters
        {
            return Ok((
                refcount_table_clusters,
                refcount_block_clusters,
                total_clusters,
            ));
        }
        refcount_block_clusters = needed_refcount_block_clusters;
        refcount_table_clusters = needed_refcount_table_clusters;
    }
}

fn encode_compressed_l2_entry(offset: u64, compressed_size: u64, cluster_size: u64) -> Result<u64> {
    let header = Qcow2Header {
        backing_file_offset: 0,
        virtual_size: 0,
        cluster_bits: cluster_size.trailing_zeros(),
        l1_size: 0,
        l1_table_offset: 0,
        refcount_table_offset: 0,
        refcount_table_clusters: 0,
        incompatible_features: 0,
        refcount_order: DEFAULT_REFCOUNT_ORDER,
        header_length: QCOW_HEADER_LENGTH_V3,
        compression_type: QCOW2_COMPRESSION_TYPE_ZLIB,
    };
    let nb_csectors = (offset + compressed_size - 1) / QCOW2_COMPRESSED_SECTOR_SIZE
        - (offset / QCOW2_COMPRESSED_SECTOR_SIZE);
    if (offset & !header.cluster_offset_mask()) != 0 {
        bail!("compressed qcow2 cluster offset does not fit qcow2 L2 entry");
    }
    if (nb_csectors & !header.csize_mask()) != 0 {
        bail!("compressed qcow2 cluster span does not fit qcow2 L2 entry");
    }
    Ok(offset | QCOW_OFLAG_COMPRESSED | (nb_csectors << header.csize_shift()))
}

fn parse_compressed_l2_entry(header: Qcow2Header, l2_entry: u64) -> Result<(u64, u64)> {
    let offset = l2_entry & header.cluster_offset_mask();
    let nb_csectors = ((l2_entry >> header.csize_shift()) & header.csize_mask()) + 1;
    let compressed_size = nb_csectors
        .checked_mul(QCOW2_COMPRESSED_SECTOR_SIZE)
        .and_then(|v| v.checked_sub(offset & (QCOW2_COMPRESSED_SECTOR_SIZE - 1)))
        .context("compressed qcow2 cluster size overflow")?;
    Ok((offset, compressed_size))
}

fn increment_refcount_range(
    refcounts: &mut BTreeMap<u64, u64>,
    offset: u64,
    len: u64,
    cluster_size: u64,
) -> Result<()> {
    if len == 0 {
        return Ok(());
    }
    let start = offset / cluster_size;
    let end = (offset + len - 1) / cluster_size;
    for cluster_index in start..=end {
        *refcounts.entry(cluster_index).or_default() += 1;
    }
    Ok(())
}

fn write_header(file: &mut File, spec: HeaderWriteSpec) -> Result<()> {
    let mut header =
        vec![0u8; usize::try_from(spec.cluster_size).context("cluster size too large")?];
    let incompatible_features = if matches!(spec.compression_type, CompressionType::Zstd) {
        QCOW2_INCOMPAT_COMPRESSION
    } else {
        0
    };
    write_be_u32(&mut header, 0, QCOW_MAGIC);
    write_be_u32(&mut header, 4, 3);
    write_be_u32(&mut header, 20, spec.cluster_size.trailing_zeros());
    write_be_u64(&mut header, 24, spec.virtual_size);
    write_be_u32(
        &mut header,
        36,
        u32::try_from(spec.l1_size).context("l1_size does not fit u32")?,
    );
    write_be_u64(&mut header, 40, spec.l1_table_offset);
    write_be_u64(&mut header, 48, spec.refcount_table_offset);
    write_be_u32(
        &mut header,
        56,
        u32::try_from(spec.refcount_table_clusters)
            .context("refcount_table_clusters does not fit u32")?,
    );
    write_be_u64(&mut header, 72, incompatible_features);
    write_be_u32(&mut header, 96, DEFAULT_REFCOUNT_ORDER);
    write_be_u32(&mut header, 100, QCOW_HEADER_LENGTH_V3);
    header[104] = match spec.compression_type {
        CompressionType::Zstd => QCOW2_COMPRESSION_TYPE_ZSTD,
        CompressionType::Zlib => QCOW2_COMPRESSION_TYPE_ZLIB,
    };
    write_exact_at(file, 0, &header)
}

fn write_l1_table(
    file: &mut File,
    l1_table_offset: u64,
    first_l2_offset: u64,
    l1_size: u64,
    cluster_size: u64,
) -> Result<()> {
    let mut raw = vec![
        0u8;
        usize::try_from(align_up(l1_size * 8, cluster_size))
            .context("l1 table too large")?
    ];
    for idx in 0..usize::try_from(l1_size).context("l1_size too large")? {
        let offset = first_l2_offset
            .checked_add(u64::try_from(idx).context("l1 index overflow")? * cluster_size)
            .context("l2 table offset overflow")?;
        raw[idx * 8..(idx + 1) * 8].copy_from_slice(&(offset | QCOW_OFLAG_COPIED).to_be_bytes());
    }
    write_exact_at(file, l1_table_offset, &raw)
}

fn write_l2_tables(
    file: &mut File,
    first_l2_offset: u64,
    l1_size: u64,
    cluster_size: u64,
    l2_entries: &[u64],
) -> Result<()> {
    let entries_per_table = usize::try_from(cluster_size / 8).context("cluster size too large")?;
    for table_idx in 0..usize::try_from(l1_size).context("l1_size too large")? {
        let mut table = vec![0u8; usize::try_from(cluster_size).context("cluster size too large")?];
        let range_start = table_idx
            .checked_mul(entries_per_table)
            .context("l2 range start overflow")?;
        let range_end = range_start
            .checked_add(entries_per_table)
            .context("l2 range end overflow")?;
        for (entry_idx, entry) in l2_entries[range_start..range_end].iter().enumerate() {
            table[entry_idx * 8..(entry_idx + 1) * 8].copy_from_slice(&entry.to_be_bytes());
        }
        let offset = first_l2_offset
            .checked_add(u64::try_from(table_idx).context("table index overflow")? * cluster_size)
            .context("l2 table offset overflow")?;
        write_exact_at(file, offset, &table)?;
    }
    Ok(())
}

fn write_refcount_table(
    file: &mut File,
    refcount_table_offset: u64,
    refcount_block_offset: u64,
    refcount_table_clusters: u64,
    refcount_block_clusters: u64,
    cluster_size: u64,
) -> Result<()> {
    let mut raw = vec![
        0u8;
        usize::try_from(refcount_table_clusters * cluster_size)
            .context("refcount table too large")?
    ];
    for idx in 0..usize::try_from(refcount_block_clusters)
        .context("refcount block cluster count too large")?
    {
        let offset = refcount_block_offset
            .checked_add(
                u64::try_from(idx).context("refcount block index overflow")? * cluster_size,
            )
            .context("refcount block table offset overflow")?;
        raw[idx * 8..(idx + 1) * 8].copy_from_slice(&offset.to_be_bytes());
    }
    write_exact_at(file, refcount_table_offset, &raw)
}

fn write_refcount_blocks(
    file: &mut File,
    refcount_block_offset: u64,
    refcount_block_clusters: u64,
    cluster_size: u64,
    total_clusters: u64,
    refcounts: &BTreeMap<u64, u64>,
) -> Result<()> {
    let entries_per_block = cluster_size / 2;
    let mut raw = vec![
        0u8;
        usize::try_from(refcount_block_clusters * cluster_size)
            .context("refcount blocks too large")?
    ];
    for (&cluster_index, &count) in refcounts {
        if cluster_index >= total_clusters {
            bail!("refcount cluster index {cluster_index} exceeds qcow2 size");
        }
        if count > u64::from(u16::MAX) {
            bail!("refcount {count} exceeds 16-bit qcow2 refcount capacity");
        }
        let block_index = cluster_index / entries_per_block;
        let entry_index = cluster_index % entries_per_block;
        let pos = usize::try_from(
            block_index
                .checked_mul(cluster_size)
                .and_then(|v| v.checked_add(entry_index * 2))
                .context("refcount block position overflow")?,
        )
        .context("refcount block position too large")?;
        raw[pos..pos + 2].copy_from_slice(&(count as u16).to_be_bytes());
    }
    write_exact_at(file, refcount_block_offset, &raw)
}

fn read_header(file: &mut File) -> Result<Qcow2Header> {
    let mut fixed = [0u8; QCOW_HEADER_LENGTH_V3 as usize];
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
    let header_length = if version >= 3 {
        u32::from_be_bytes(fixed[100..104].try_into().expect("slice length"))
    } else {
        72
    };
    let compression_type = if header_length > 104 {
        fixed[104]
    } else {
        QCOW2_COMPRESSION_TYPE_ZLIB
    };

    Ok(Qcow2Header {
        backing_file_offset: u64::from_be_bytes(fixed[8..16].try_into().expect("slice length")),
        virtual_size: u64::from_be_bytes(fixed[24..32].try_into().expect("slice length")),
        cluster_bits,
        l1_size: u32::from_be_bytes(fixed[36..40].try_into().expect("slice length")),
        l1_table_offset: u64::from_be_bytes(fixed[40..48].try_into().expect("slice length")),
        refcount_table_offset: u64::from_be_bytes(fixed[48..56].try_into().expect("slice length")),
        refcount_table_clusters: u32::from_be_bytes(
            fixed[56..60].try_into().expect("slice length"),
        ),
        incompatible_features: if version >= 3 {
            u64::from_be_bytes(fixed[72..80].try_into().expect("slice length"))
        } else {
            0
        },
        refcount_order,
        header_length,
        compression_type,
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

fn write_be_u32(buf: &mut [u8], offset: usize, value: u32) {
    buf[offset..offset + 4].copy_from_slice(&value.to_be_bytes());
}

fn write_be_u64(buf: &mut [u8], offset: usize, value: u64) {
    buf[offset..offset + 8].copy_from_slice(&value.to_be_bytes());
}

fn div_ceil(value: u64, divisor: u64) -> u64 {
    value.div_ceil(divisor)
}

fn align_up(value: u64, align: u64) -> u64 {
    if value == 0 {
        return 0;
    }
    value.div_ceil(align) * align
}

fn align_down(value: u64, align: u64) -> u64 {
    value / align * align
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

    // Verify header flags, sector-disjoint compressed clusters, and per-cluster
    // data integrity using the sector-rounded read span that qemu uses.
    #[test]
    fn native_compress_writes_valid_zstd_qcow2() {
        let tmp = tempdir().expect("tempdir");
        let source = tmp.path().join("source.qcow2");
        let dest = tmp.path().join("dest.qcow2");
        // Use a multi-cluster fixture so adjacent-cluster packing is exercised.
        write_multi_cluster_test_image(&source, 4).expect("write source qcow2");

        compress_qcow2_image(
            &source,
            &dest,
            CompressionType::Zstd,
            &BTreeMap::new(),
            "-19 -T0",
        )
        .expect("compress");

        let mut file = File::open(&dest).expect("open dest");
        let header = read_header(&mut file).expect("read header");
        assert_eq!(header.header_length, QCOW_HEADER_LENGTH_V3);
        assert_eq!(header.compression_type, QCOW2_COMPRESSION_TYPE_ZSTD);
        assert_ne!(header.incompatible_features & QCOW2_INCOMPAT_COMPRESSION, 0);

        let stats = read_qcow2_image_stats(&dest).expect("stats");
        assert_eq!(stats.virtual_size, 4 * 4096);
        assert_eq!(stats.cluster_size, 4096);
        assert_eq!(stats.allocated_data_clusters, 4);

        // Assert every compressed cluster occupies a sector range disjoint from
        // every other: [offset/512, offset/512 + nb_csectors) must not overlap.
        // This is what qemu requires; without the sector-alignment fix the old
        // tight-packing allocator would cause adjacent entries to share sectors
        // and qemu's zstd decoder would return -EIO.
        assert_compressed_clusters_sector_disjoint(&dest, &header, &mut file);

        // Verify each cluster decompresses correctly using only the sector-rounded
        // span (nb_csectors * 512 bytes from the sector-aligned base) — the same
        // byte range qemu hands to its decompressor.
        let mut image = SourceImage::open(&dest).expect("open image");
        let cluster_size = 4096usize;
        for i in 0u64..4 {
            let mut cluster = vec![0u8; cluster_size];
            image
                .read_virtual_range(i * 4096, &mut cluster)
                .expect("read guest cluster");
            let expected_byte = 0x41u8 + (i as u8); // 'A', 'B', 'C', 'D'
            assert_eq!(
                cluster,
                vec![expected_byte; cluster_size],
                "cluster {i} data mismatch"
            );
        }
    }

    // Confirm the zlib path still produces sector-disjoint compressed clusters
    // and round-trips correctly.
    #[test]
    fn native_compress_writes_valid_zlib_qcow2() {
        let tmp = tempdir().expect("tempdir");
        let source = tmp.path().join("source.qcow2");
        let dest = tmp.path().join("dest.qcow2");
        write_multi_cluster_test_image(&source, 4).expect("write source qcow2");

        compress_qcow2_image(&source, &dest, CompressionType::Zlib, &BTreeMap::new(), "")
            .expect("compress");

        let mut file = File::open(&dest).expect("open dest");
        let header = read_header(&mut file).expect("read header");
        assert_eq!(header.compression_type, QCOW2_COMPRESSION_TYPE_ZLIB);

        assert_compressed_clusters_sector_disjoint(&dest, &header, &mut file);

        let mut image = SourceImage::open(&dest).expect("open image");
        for i in 0u64..4 {
            let mut cluster = vec![0u8; 4096];
            image
                .read_virtual_range(i * 4096, &mut cluster)
                .expect("read guest cluster");
            let expected_byte = 0x41u8 + (i as u8);
            assert_eq!(
                cluster,
                vec![expected_byte; 4096],
                "cluster {i} data mismatch"
            );
        }
    }

    /// Check that no two compressed L2 entries share a 512-byte sector.
    /// This mirrors what qemu enforces: each compressed cluster occupies a
    /// contiguous run of whole sectors starting at its own sector-aligned base,
    /// and no neighbouring cluster's frame bytes fall in that span.
    fn assert_compressed_clusters_sector_disjoint(
        _dest: &Path,
        header: &Qcow2Header,
        file: &mut File,
    ) {
        let cluster_size = header.cluster_size();
        let l2_entries_per_table = (cluster_size / 8) as usize;
        let l1_size = header.l1_size as usize;

        let l1_entries = read_u64_table(file, header.l1_table_offset, l1_size).expect("l1 table");

        // Collect (base_sector, end_sector_exclusive, global_cluster_index).
        let mut spans: Vec<(u64, u64, usize)> = Vec::new();
        for (l1_idx, &l1_entry) in l1_entries.iter().enumerate() {
            if l1_entry == 0 {
                continue;
            }
            let l2_offset = aligned_data_offset(l1_entry, cluster_size);
            let l2_table = read_u64_table(file, l2_offset, l2_entries_per_table).expect("l2 table");
            for (l2_idx, &l2_entry) in l2_table.iter().enumerate() {
                if l2_entry & QCOW_OFLAG_COMPRESSED == 0 {
                    continue;
                }
                let (offset, _) =
                    parse_compressed_l2_entry(*header, l2_entry).expect("parse l2 entry");
                // nb_csectors is stored as (value - 1) in the L2 entry field.
                let nb_csectors = ((l2_entry >> header.csize_shift()) & header.csize_mask()) + 1;
                let base_sector = offset / QCOW2_COMPRESSED_SECTOR_SIZE;
                let end_sector = base_sector + nb_csectors;
                let global = l1_idx * l2_entries_per_table + l2_idx;
                spans.push((base_sector, end_sector, global));
            }
        }

        assert!(
            spans.len() >= 2,
            "fixture must produce at least 2 compressed clusters to test packing"
        );

        // Pairwise overlap check.
        for i in 0..spans.len() {
            for j in (i + 1)..spans.len() {
                let (base_i, end_i, idx_i) = spans[i];
                let (base_j, end_j, idx_j) = spans[j];
                // Disjoint iff end_i <= base_j or end_j <= base_i.
                assert!(
                    end_i <= base_j || end_j <= base_i,
                    "compressed clusters {idx_i} and {idx_j} share sectors: \
                     [{base_i},{end_i}) overlaps [{base_j},{end_j})"
                );
            }
        }
    }

    /// Creates a plain (uncompressed) qcow2 with `cluster_count` non-zero guest
    /// clusters. Each cluster is fully populated with a distinct repeating byte
    /// value ('A'=0x41 for cluster 0, 'B' for cluster 1, …).
    /// The image uses 4 KiB clusters and 16-bit refcounts (refcount_order = 4).
    ///
    /// Layout (all offsets in units of `cluster_size = 4096`):
    ///   0  – header
    ///   1  – L1 table  (1 entry → L2 at cluster 2)
    ///   2  – L2 table  (cluster_count entries → data clusters 5…)
    ///   3  – refcount table (1 entry → refcount block at cluster 4)
    ///   4  – refcount block
    ///   5…5+cluster_count-1 – data clusters
    fn write_multi_cluster_test_image(path: &Path, cluster_count: u64) -> Result<()> {
        assert!(
            cluster_count >= 2,
            "need at least 2 clusters to exercise packing"
        );
        let cluster_size = 4096u64;
        let total_clusters = 5 + cluster_count;
        let mut file = File::create(path)?;
        file.set_len(cluster_size * total_clusters)?;

        write_exact_at(&mut file, 0, &QCOW_MAGIC.to_be_bytes())?;
        write_exact_at(&mut file, 4, &3u32.to_be_bytes())?;
        write_exact_at(&mut file, 20, &12u32.to_be_bytes())?; // cluster_bits = 12
        write_exact_at(&mut file, 24, &(cluster_size * cluster_count).to_be_bytes())?;
        write_exact_at(&mut file, 36, &1u32.to_be_bytes())?; // l1_size = 1
        write_exact_at(&mut file, 40, &cluster_size.to_be_bytes())?; // l1_table_offset
        write_exact_at(&mut file, 48, &(cluster_size * 3).to_be_bytes())?; // refcount_table_offset
        write_exact_at(&mut file, 56, &1u32.to_be_bytes())?; // refcount_table_clusters
        write_exact_at(&mut file, 96, &4u32.to_be_bytes())?; // refcount_order = 4 (16-bit)
        write_exact_at(&mut file, 100, &104u32.to_be_bytes())?; // header_length

        write_u64_at(&mut file, cluster_size, cluster_size * 2)?;
        for i in 0..cluster_count {
            write_u64_at(&mut file, cluster_size * 2 + i * 8, cluster_size * (5 + i))?;
        }
        write_u64_at(&mut file, cluster_size * 3, cluster_size * 4)?;
        for idx in 0..total_clusters {
            write_exact_at(&mut file, cluster_size * 4 + idx * 2, &1u16.to_be_bytes())?;
        }

        // Full cluster of a distinct value per cluster.
        for i in 0..cluster_count {
            let byte = 0x41u8 + (i as u8); // 'A', 'B', 'C', …
            write_exact_at(
                &mut file,
                cluster_size * (5 + i),
                &vec![byte; cluster_size as usize],
            )?;
        }

        Ok(())
    }

    /// Creates a plain (uncompressed) qcow2 with `cluster_count` non-zero guest
    /// clusters at a configurable `cluster_size` (must be a power of two, ≥ 512).
    ///
    /// Each cluster is filled with a pattern that produces a compressed frame
    /// spanning multiple 512-byte sectors — important for testing the `nb_csectors`
    /// read path at 1 MiB cluster size. The first `sector_stride` bytes of each
    /// cluster contain 512-byte blocks where block b of cluster c has value
    /// `((b as u8).wrapping_add(c as u8).wrapping_mul(7).wrapping_add(0x41))`.
    /// For cluster_size = 4096 this is 8 blocks = 4 KiB; for cluster_size = 1M
    /// this is 512 blocks = 256 KiB. The remainder is a non-zero, cluster-distinct
    /// value so full-cluster round-trip assertions cover every byte.
    ///
    /// Layout (all offsets in units of `cluster_size`):
    ///   0  – header
    ///   1  – L1 table  (1 entry → L2 at cluster 2)
    ///   2  – L2 table  (cluster_count entries → data clusters 5…)
    ///   3  – refcount table (1 entry → refcount block at cluster 4)
    ///   4  – refcount block
    ///   5…5+cluster_count-1 – data clusters
    fn write_test_image_with_cluster_size(
        path: &Path,
        cluster_count: u64,
        cluster_size: u64,
    ) -> Result<()> {
        assert!(
            cluster_count >= 2,
            "need at least 2 clusters to exercise packing"
        );
        assert!(
            cluster_size.is_power_of_two() && cluster_size >= 512,
            "cluster_size must be a power of two >= 512"
        );
        let cluster_bits = cluster_size.trailing_zeros();
        let total_clusters = 5 + cluster_count;
        let mut file = File::create(path)?;
        file.set_len(cluster_size * total_clusters)?;

        // Header
        write_exact_at(&mut file, 0, &QCOW_MAGIC.to_be_bytes())?;
        write_exact_at(&mut file, 4, &3u32.to_be_bytes())?;
        write_exact_at(&mut file, 20, &cluster_bits.to_be_bytes())?;
        write_exact_at(&mut file, 24, &(cluster_size * cluster_count).to_be_bytes())?;
        write_exact_at(&mut file, 36, &1u32.to_be_bytes())?; // l1_size = 1
        write_exact_at(&mut file, 40, &cluster_size.to_be_bytes())?; // l1_table_offset
        write_exact_at(&mut file, 48, &(cluster_size * 3).to_be_bytes())?; // refcount_table_offset
        write_exact_at(&mut file, 56, &1u32.to_be_bytes())?; // refcount_table_clusters
        write_exact_at(&mut file, 96, &4u32.to_be_bytes())?; // refcount_order = 4 (16-bit)
        write_exact_at(&mut file, 100, &104u32.to_be_bytes())?; // header_length

        // L1 table: single entry pointing to L2 at cluster 2
        write_u64_at(&mut file, cluster_size, cluster_size * 2)?;

        // L2 table: cluster_count entries pointing to data clusters (5, 6, …)
        for i in 0..cluster_count {
            write_u64_at(&mut file, cluster_size * 2 + i * 8, cluster_size * (5 + i))?;
        }

        // Refcount table: single entry pointing to refcount block at cluster 4
        write_u64_at(&mut file, cluster_size * 3, cluster_size * 4)?;

        // Refcount block: every cluster has refcount 1
        for idx in 0..total_clusters {
            write_exact_at(&mut file, cluster_size * 4 + idx * 2, &1u16.to_be_bytes())?;
        }

        // Data clusters: fill the first `sector_count` 512-byte blocks with a
        // cluster-distinct pattern so the compressed frame spans multiple sectors.
        // At 4K cluster_size: 8 blocks; at 1M: 512 blocks (256 KiB of pattern data).
        let sector_count = (cluster_size / 512).min(512);
        let sector = 512usize;
        let mut sector_buf = vec![0u8; sector];
        let cluster_bytes = usize::try_from(cluster_size).expect("cluster_size fits usize");
        let sector_bytes = usize::try_from(sector_count).expect("sector_count fits usize") * sector;
        for i in 0..cluster_count {
            let cluster_offset = cluster_size * (5 + i);
            for b in 0..sector_count {
                let value = (b as u8)
                    .wrapping_add(i as u8)
                    .wrapping_mul(7)
                    .wrapping_add(0x41);
                sector_buf.fill(value);
                write_exact_at(&mut file, cluster_offset + b * 512, &sector_buf)?;
            }
            let tail_fill = 0x80u8.wrapping_add(i as u8);
            if sector_bytes < cluster_bytes {
                let tail = vec![tail_fill; cluster_bytes - sector_bytes];
                write_exact_at(&mut file, cluster_offset + sector_bytes as u64, &tail)?;
            }
        }

        Ok(())
    }

    /// Returns the expected first-sector value for cluster `i` block `b` in the
    /// `write_test_image_with_cluster_size` fixture.
    fn expected_sector_value(cluster_idx: u64, block_idx: u64) -> u8 {
        (block_idx as u8)
            .wrapping_add(cluster_idx as u8)
            .wrapping_mul(7)
            .wrapping_add(0x41)
    }

    fn expected_tail_value(cluster_idx: u64) -> u8 {
        0x80u8.wrapping_add(cluster_idx as u8)
    }

    /// Assert that every guest cluster read from `image_path` via `SourceImage`
    /// matches the data written by `write_test_image_with_cluster_size`.
    fn assert_roundtrip_data(image_path: &Path, cluster_count: u64, cluster_size: u64) {
        let mut image = SourceImage::open(image_path).expect("open image for round-trip check");
        assert_eq!(
            image.header.cluster_size(),
            cluster_size,
            "cluster_size mismatch in compressed image header"
        );
        let sector_count = (cluster_size / 512).min(512) as usize;
        let cluster_usize = usize::try_from(cluster_size).expect("cluster_size fits usize");

        for i in 0..cluster_count {
            let mut buf = vec![0u8; cluster_usize];
            image
                .read_virtual_range(i * cluster_size, &mut buf)
                .unwrap_or_else(|e| panic!("read_virtual_range cluster {i}: {e:#}"));

            for b in 0..sector_count {
                let expected = expected_sector_value(i, b as u64);
                let sector_slice = &buf[b * 512..(b + 1) * 512];
                assert!(
                    sector_slice.iter().all(|&x| x == expected),
                    "cluster {i} sector {b}: expected all 0x{expected:02x}, \
                     got first byte 0x{:02x}",
                    sector_slice[0]
                );
            }
            let tail_expected = expected_tail_value(i);
            assert!(
                buf[sector_count * 512..]
                    .iter()
                    .all(|&x| x == tail_expected),
                "cluster {i}: tail bytes should be 0x{tail_expected:02x}"
            );
        }
    }

    fn write_test_image(path: &Path) -> Result<()> {
        let cluster_size = 4096u64;
        let mut file = File::create(path)?;
        file.set_len(cluster_size * 7)?;

        write_exact_at(&mut file, 0, &QCOW_MAGIC.to_be_bytes())?;
        write_exact_at(&mut file, 4, &3u32.to_be_bytes())?;
        write_exact_at(&mut file, 20, &12u32.to_be_bytes())?;
        write_exact_at(&mut file, 24, &(cluster_size * 2).to_be_bytes())?;
        write_exact_at(&mut file, 36, &1u32.to_be_bytes())?;
        write_exact_at(&mut file, 40, &cluster_size.to_be_bytes())?;
        write_exact_at(&mut file, 48, &(cluster_size * 3).to_be_bytes())?;
        write_exact_at(&mut file, 56, &1u32.to_be_bytes())?;
        write_exact_at(&mut file, 96, &4u32.to_be_bytes())?;
        write_exact_at(&mut file, 100, &104u32.to_be_bytes())?;

        write_u64_at(&mut file, cluster_size, cluster_size * 2)?;
        write_u64_at(&mut file, cluster_size * 2, cluster_size * 5)?;
        write_u64_at(&mut file, cluster_size * 2 + 8, cluster_size * 6)?;
        write_u64_at(&mut file, cluster_size * 3, cluster_size * 4)?;

        for idx in 0u64..=6 {
            write_exact_at(&mut file, cluster_size * 4 + idx * 2, &1u16.to_be_bytes())?;
        }

        write_exact_at(&mut file, cluster_size * 6, &[0x5a; 32])?;
        Ok(())
    }

    fn read_exact_refcount16(file: &mut File, block_offset: u64, cluster_index: u64) -> u16 {
        let mut raw = [0u8; 2];
        read_exact_at(file, block_offset + (cluster_index * 2), &mut raw).expect("read refcount");
        u16::from_be_bytes(raw)
    }

    fn write_custom_test_image_with_cluster_size(
        path: &Path,
        cluster_contents: &[Vec<u8>],
        cluster_size: u64,
    ) -> Result<()> {
        assert!(
            cluster_size.is_power_of_two() && cluster_size >= 512,
            "cluster_size must be a power of two >= 512"
        );
        assert!(
            !cluster_contents.is_empty(),
            "need at least one cluster to write a test image"
        );
        for (idx, cluster) in cluster_contents.iter().enumerate() {
            assert_eq!(
                cluster.len(),
                usize::try_from(cluster_size).expect("cluster_size fits usize"),
                "cluster {idx} length must match cluster_size"
            );
        }

        let cluster_bits = cluster_size.trailing_zeros();
        let cluster_count = u64::try_from(cluster_contents.len()).expect("cluster count fits u64");
        let total_clusters = 5 + cluster_count;
        let mut file = File::create(path)?;
        file.set_len(cluster_size * total_clusters)?;

        write_exact_at(&mut file, 0, &QCOW_MAGIC.to_be_bytes())?;
        write_exact_at(&mut file, 4, &3u32.to_be_bytes())?;
        write_exact_at(&mut file, 20, &cluster_bits.to_be_bytes())?;
        write_exact_at(
            &mut file,
            24,
            &(cluster_size
                .checked_mul(cluster_count)
                .context("virtual size overflow")?)
            .to_be_bytes(),
        )?;
        write_exact_at(&mut file, 36, &1u32.to_be_bytes())?;
        write_exact_at(&mut file, 40, &cluster_size.to_be_bytes())?;
        write_exact_at(&mut file, 48, &(cluster_size * 3).to_be_bytes())?;
        write_exact_at(&mut file, 56, &1u32.to_be_bytes())?;
        write_exact_at(&mut file, 96, &4u32.to_be_bytes())?;
        write_exact_at(&mut file, 100, &104u32.to_be_bytes())?;

        write_u64_at(&mut file, cluster_size, cluster_size * 2)?;
        for i in 0..cluster_count {
            write_u64_at(&mut file, cluster_size * 2 + i * 8, cluster_size * (5 + i))?;
        }
        write_u64_at(&mut file, cluster_size * 3, cluster_size * 4)?;
        for idx in 0..total_clusters {
            write_exact_at(&mut file, cluster_size * 4 + idx * 2, &1u16.to_be_bytes())?;
        }
        for (idx, cluster) in cluster_contents.iter().enumerate() {
            write_exact_at(
                &mut file,
                cluster_size * (5 + u64::try_from(idx).expect("cluster index fits u64")),
                cluster,
            )?;
        }

        Ok(())
    }

    fn make_incompressible_cluster(cluster_size: usize) -> Vec<u8> {
        let mut out = vec![0u8; cluster_size];
        let mut state = 0x9e37_79b9_7f4a_7c15u64;
        for byte in &mut out {
            state ^= state << 7;
            state ^= state >> 9;
            state ^= state << 8;
            *byte = (state >> 24) as u8;
        }
        out
    }

    fn make_mbr_like_cluster(cluster_size: usize) -> Vec<u8> {
        let mut out = vec![0u8; cluster_size];
        let mut state = 0x0123_4567_89ab_cdefu64;
        for byte in &mut out {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            *byte = (state >> 16) as u8;
        }
        out[510] = 0x55;
        out[511] = 0xAA;
        out
    }

    fn make_dense_non_zero_compressible_cluster(cluster_size: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(cluster_size);
        let pattern: [u8; 16] = [
            0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee,
            0xf0, 0x0f,
        ];
        while out.len() < cluster_size {
            let to_take = (cluster_size - out.len()).min(pattern.len());
            out.extend_from_slice(&pattern[..to_take]);
        }
        out
    }

    fn make_realistic_cluster_contents(cluster_size: usize) -> Vec<Vec<u8>> {
        vec![
            make_mbr_like_cluster(cluster_size),
            make_incompressible_cluster(cluster_size),
            make_dense_non_zero_compressible_cluster(cluster_size),
        ]
    }

    fn assert_roundtrip_guest_bytes(path: &Path, expected: &[u8], target_cluster_size: u64) {
        let mut image = SourceImage::open(path).expect("open image for realistic round-trip");
        let cluster_size = usize::try_from(target_cluster_size).expect("cluster_size fits usize");
        assert_eq!(
            image.header.cluster_size(),
            target_cluster_size,
            "cluster_size mismatch in compressed image header"
        );
        let guest_cluster_count = div_ceil(
            u64::try_from(expected.len()).expect("expected bytes len fits u64"),
            target_cluster_size,
        );
        for cluster_idx in 0..guest_cluster_count {
            let mut actual = vec![0u8; cluster_size];
            image
                .read_virtual_range(cluster_idx * target_cluster_size, &mut actual)
                .unwrap_or_else(|e| panic!("read_virtual_range cluster {cluster_idx}: {e:#}"));
            let start = usize::try_from(cluster_idx * target_cluster_size).expect("start fits");
            let end = start + cluster_size;
            assert_eq!(
                &actual[..],
                &expected[start..end],
                "cluster {cluster_idx} mismatch"
            );
        }
    }

    fn assert_compressed_entries_decode_with_declared_codec(
        path: &Path,
        expected: &[u8],
        target_cluster_size: u64,
        compression_type: CompressionType,
    ) {
        let mut file = File::open(path).expect("open compressed image");
        let header = read_header(&mut file).expect("read header");
        let l1_entries = read_u64_table(
            &mut file,
            header.l1_table_offset,
            usize::try_from(header.l1_size).expect("l1_size fits usize"),
        )
        .expect("read l1 table");
        let l2_entries_per_table =
            usize::try_from(target_cluster_size / 8).expect("l2 entries fits usize");
        let cluster_size = usize::try_from(target_cluster_size).expect("cluster_size fits usize");
        let guest_cluster_count = div_ceil(
            u64::try_from(expected.len()).expect("expected bytes len fits u64"),
            target_cluster_size,
        );

        for cluster_idx in 0..guest_cluster_count {
            let l1_index =
                usize::try_from(cluster_idx / (target_cluster_size / 8)).expect("l1 idx fits");
            let l2_index =
                usize::try_from(cluster_idx % (target_cluster_size / 8)).expect("l2 idx fits");
            let l2_table_offset = aligned_data_offset(
                *l1_entries
                    .get(l1_index)
                    .unwrap_or_else(|| panic!("missing l1 entry for cluster {cluster_idx}")),
                target_cluster_size,
            );
            let l2_entries =
                read_u64_table(&mut file, l2_table_offset, l2_entries_per_table).expect("read l2");
            let l2_entry = l2_entries[l2_index];
            if (l2_entry & QCOW_OFLAG_COMPRESSED) == 0 {
                continue;
            }
            let (compressed_offset, compressed_size) =
                parse_compressed_l2_entry(header, l2_entry).expect("parse compressed l2");
            let mut compressed =
                vec![0u8; usize::try_from(compressed_size).expect("compressed size fits usize")];
            read_exact_at(&mut file, compressed_offset, &mut compressed).expect("read compressed");

            let decoded = match compression_type {
                CompressionType::Zstd => {
                    // Assert qemu-compatible single-frame structure.
                    // zstd::stream::read::Decoder transparently accepts multi-frame /
                    // worker-chunked output, so the decode below would pass even for
                    // broken frames.  These assertions catch that blind spot:
                    //
                    // (1) The frame header must pledge the content size (equals cluster_size).
                    // (2) No second zstd frame may follow the first frame.  In qcow2 the stored
                    //     payload is padded to a sector boundary, so the trailing bytes after the
                    //     first frame are padding (zeros), not a second frame.  Worker-chunked
                    //     multithreaded output would have a real second frame there.
                    let content_size = zstd::zstd_safe::get_frame_content_size(&compressed)
                        .unwrap_or_else(|_| {
                            panic!("cluster {cluster_idx}: zstd frame header missing or corrupt")
                        })
                        .unwrap_or_else(|| {
                            panic!(
                                "cluster {cluster_idx}: zstd frame does not pledge content \
                                     size (qemu requires single-frame with include_contentsize)"
                            )
                        });
                    assert_eq!(
                        content_size as usize, cluster_size,
                        "cluster {cluster_idx}: pledged content size must equal cluster_size"
                    );
                    let first_frame_bytes = zstd::zstd_safe::find_frame_compressed_size(
                        &compressed,
                    )
                    .unwrap_or_else(|e| {
                        panic!(
                            "cluster {cluster_idx}: cannot determine first frame size: \
                                     {e:?}"
                        )
                    });
                    assert!(
                        first_frame_bytes <= compressed.len(),
                        "cluster {cluster_idx}: first frame ({first_frame_bytes} B) exceeds \
                         stored payload ({} B)",
                        compressed.len()
                    );
                    // The bytes after the first frame are sector padding.  Assert they are NOT a
                    // second valid zstd frame — that would indicate a multi-frame stream.
                    let trailing = &compressed[first_frame_bytes..];
                    if !trailing.is_empty()
                        && zstd::zstd_safe::find_frame_compressed_size(trailing).is_ok()
                    {
                        panic!(
                            "cluster {cluster_idx}: a second valid zstd frame was found \
                             after the first frame — multithreaded/worker-chunked output \
                             detected (qemu rejects multi-frame clusters)"
                        );
                    }

                    let mut decoder =
                        zstd::stream::read::Decoder::with_buffer(compressed.as_slice())
                            .expect("init zstd decoder");
                    let mut out = vec![0u8; cluster_size];
                    use std::io::Read;
                    decoder.read_exact(&mut out).expect("decode zstd");
                    out
                }
                CompressionType::Zlib => {
                    let mut decoder = flate2::read::ZlibDecoder::new(compressed.as_slice());
                    let mut out = vec![0u8; cluster_size];
                    use std::io::Read;
                    decoder.read_exact(&mut out).expect("decode zlib");
                    out
                }
            };

            let start = usize::try_from(cluster_idx * target_cluster_size).expect("start fits");
            let end = start + cluster_size;
            assert_eq!(
                &decoded[..],
                &expected[start..end],
                "strict codec decode mismatch at cluster {cluster_idx}"
            );
        }
    }

    fn run_realistic_round_trip_test(
        compression_type: CompressionType,
        source_cluster_size: u64,
        target_cluster_size: Option<u64>,
        compressor_opts: &str,
    ) {
        let tmp = tempdir().expect("tempdir");
        let source = tmp.path().join("realistic-source.qcow2");
        let dest = tmp.path().join("realistic-compressed.qcow2");
        let source_cluster_size_usize =
            usize::try_from(source_cluster_size).expect("source cluster size fits usize");
        let cluster_contents = make_realistic_cluster_contents(source_cluster_size_usize);
        write_custom_test_image_with_cluster_size(&source, &cluster_contents, source_cluster_size)
            .expect("write realistic source image");

        let mut compressor_args = BTreeMap::new();
        if let Some(target) = target_cluster_size {
            compressor_args.insert("cluster_size".to_owned(), target.to_string());
        }

        compress_qcow2_image(
            &source,
            &dest,
            compression_type,
            &compressor_args,
            compressor_opts,
        )
        .expect("compress realistic source");

        let expected: Vec<u8> = cluster_contents.into_iter().flatten().collect();
        let target_size = target_cluster_size.unwrap_or(source_cluster_size);
        assert_roundtrip_guest_bytes(&dest, &expected, target_size);
        assert_compressed_entries_decode_with_declared_codec(
            &dest,
            &expected,
            target_size,
            compression_type,
        );
    }

    fn qemu_img_available() -> bool {
        std::process::Command::new("qemu-img")
            .arg("--version")
            .output()
            .is_ok()
    }

    fn assert_qemu_img_check(path: &Path) {
        let output = std::process::Command::new("qemu-img")
            .args(["check", "-f", "qcow2"])
            .arg(path)
            .output()
            .expect("run qemu-img check");
        assert!(
            output.status.success(),
            "qemu-img check failed for {} (status: {}):\nstdout:\n{}\nstderr:\n{}",
            path.display(),
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }

    fn assert_guest_clusters_match(path: &Path, cluster_contents: &[Vec<u8>], cluster_size: u64) {
        let mut image = SourceImage::open(path).expect("open image for guest-cluster check");
        let cluster_size = usize::try_from(cluster_size).expect("cluster_size fits usize");
        for (idx, expected) in cluster_contents.iter().enumerate() {
            let mut actual = vec![0u8; cluster_size];
            image
                .read_virtual_range(
                    u64::try_from(idx).expect("cluster index fits u64")
                        * u64::try_from(cluster_size).expect("cluster_size fits u64"),
                    &mut actual,
                )
                .unwrap_or_else(|e| panic!("read_virtual_range cluster {idx}: {e:#}"));
            assert_eq!(actual, *expected, "cluster {idx} data mismatch");
        }
    }

    fn run_copied_flag_test(compression_type: CompressionType, cluster_size: u64) {
        let tmp = tempdir().expect("tempdir");
        let source = tmp.path().join("source.qcow2");
        let dest = tmp.path().join("compressed.qcow2");
        let cluster_size_usize = usize::try_from(cluster_size).expect("cluster_size fits usize");
        let cluster_contents = vec![
            vec![0u8; cluster_size_usize],
            vec![0x41; cluster_size_usize],
            make_incompressible_cluster(cluster_size_usize),
            vec![0x5a; cluster_size_usize],
        ];
        write_custom_test_image_with_cluster_size(&source, &cluster_contents, cluster_size)
            .expect("write source image");

        let mut compressor_args = BTreeMap::new();
        if cluster_size == 1_048_576 {
            compressor_args.insert("cluster_size".to_owned(), "1M".to_owned());
        }
        let compressor_opts = if compression_type == CompressionType::Zstd {
            "-19 -T0"
        } else {
            ""
        };
        compress_qcow2_image(
            &source,
            &dest,
            compression_type,
            &compressor_args,
            compressor_opts,
        )
        .expect("compress mixed source");

        let mut file = File::open(&dest).expect("open dest");
        let header = read_header(&mut file).expect("read header");
        let l1_entries = read_u64_table(
            &mut file,
            header.l1_table_offset,
            usize::try_from(header.l1_size).expect("l1_size fits usize"),
        )
        .expect("read l1 table");
        assert_eq!(
            l1_entries.len(),
            1,
            "mixed fixture should use exactly one L1 entry"
        );
        assert_ne!(
            l1_entries[0] & QCOW_OFLAG_COPIED,
            0,
            "L1 entry must set COPIED"
        );

        let l2_entries_per_table =
            usize::try_from(cluster_size / 8).expect("l2 entries per table fits usize");
        let l2_offset = aligned_data_offset(l1_entries[0], cluster_size);
        let l2_entries =
            read_u64_table(&mut file, l2_offset, l2_entries_per_table).expect("read l2");
        assert_eq!(
            l2_entries[0], 0,
            "zero cluster entry should stay unallocated"
        );
        assert_eq!(l2_entries[4], 0, "unused L2 entry should stay zero");

        for &compressed_idx in &[1usize, 3usize] {
            let entry = l2_entries[compressed_idx];
            assert_ne!(
                entry & QCOW_OFLAG_COMPRESSED,
                0,
                "compressed cluster {compressed_idx} should set COMPRESSED"
            );
            assert_eq!(
                entry & QCOW_OFLAG_COPIED,
                0,
                "compressed cluster {compressed_idx} must not set COPIED"
            );
        }

        let raw_entry = l2_entries[2];
        assert_eq!(
            raw_entry & QCOW_OFLAG_COMPRESSED,
            0,
            "raw cluster should not set COMPRESSED"
        );
        assert_ne!(
            raw_entry & QCOW_OFLAG_COPIED,
            0,
            "raw cluster should set COPIED"
        );
        assert_ne!(
            aligned_data_offset(raw_entry, cluster_size),
            0,
            "raw cluster should point at allocated data"
        );

        let stats = read_qcow2_image_stats(&dest).expect("read stats");
        assert_eq!(stats.cluster_size, cluster_size);
        assert_eq!(stats.virtual_size, cluster_size * 4);
        assert_eq!(stats.allocated_data_clusters, 3);

        assert_guest_clusters_match(&dest, &cluster_contents, cluster_size);

        let sparsify_stats = sparsify_zero_clusters(&dest).expect("sparsify mixed image");
        assert_eq!(sparsify_stats.scanned_clusters, 1);
        assert_eq!(sparsify_stats.deallocated_clusters, 0);
        assert_eq!(sparsify_stats.skipped_compressed_clusters, 2);

        assert_guest_clusters_match(&dest, &cluster_contents, cluster_size);

        if qemu_img_available() {
            assert_qemu_img_check(&dest);
        }
    }

    // -------------------------------------------------------------------------
    // Round-trip tests: compress a source, open the compressed output as a
    // SourceImage, and verify every guest cluster decodes byte-identically.
    // This is the exact scenario (compressed source → recompress) that the
    // botwork-docker CI failure exposed: the read path must decode every cluster
    // via the qemu-compatible nb_csectors*512 span without mis-decoding even a
    // single cluster.
    // -------------------------------------------------------------------------

    fn run_round_trip_test(compression_type: CompressionType, cluster_size_bytes: u64) {
        let tmp = tempdir().expect("tempdir");
        let source = tmp.path().join("source.qcow2");
        let dest = tmp.path().join("compressed.qcow2");

        // Four clusters so adjacent-cluster packing is exercised.
        write_test_image_with_cluster_size(&source, 4, cluster_size_bytes)
            .expect("write source image");

        let mut compressor_args = BTreeMap::new();

        if cluster_size_bytes == 1_048_576 {
            compressor_args.insert("cluster_size".to_owned(), "1M".to_owned());
        }
        let zstd_opts = "-19 -T0";
        let extra_opts = if compression_type == CompressionType::Zstd {
            zstd_opts
        } else {
            ""
        };

        compress_qcow2_image(
            &source,
            &dest,
            compression_type,
            &compressor_args,
            extra_opts,
        )
        .expect("compress");

        assert_compressed_clusters_sector_disjoint(
            &dest,
            &read_header(&mut File::open(&dest).expect("open")).expect("read header"),
            &mut File::open(&dest).expect("open"),
        );

        // Read every cluster back through SourceImage and assert byte equality.
        assert_roundtrip_data(&dest, 4, cluster_size_bytes);
    }

    #[test]
    fn native_compress_round_trip_zstd_4k() {
        run_round_trip_test(CompressionType::Zstd, 4096);
    }

    #[test]
    fn native_compress_round_trip_zlib_4k() {
        run_round_trip_test(CompressionType::Zlib, 4096);
    }

    #[test]
    fn native_compress_round_trip_zstd_1m() {
        run_round_trip_test(CompressionType::Zstd, 1_048_576);
    }

    #[test]
    fn native_compress_round_trip_zlib_1m() {
        run_round_trip_test(CompressionType::Zlib, 1_048_576);
    }

    // -------------------------------------------------------------------------
    // Double-compress (A → B → C) tests.
    //
    // Compress source A to get compressed image B, then open B as a SourceImage
    // and re-compress it to produce C.  Assert that C's guest clusters decode
    // byte-identically to A's original data.  This is the botwork-docker
    // scenario exactly: the second native-compress build uses a natively-
    // compressed qcow2 as its source and must produce a bootable image.
    // -------------------------------------------------------------------------

    fn run_double_compress_test(compression_type: CompressionType, cluster_size_bytes: u64) {
        let tmp = tempdir().expect("tempdir");
        let source = tmp.path().join("a.qcow2");
        let intermediate = tmp.path().join("b.qcow2");
        let final_image = tmp.path().join("c.qcow2");

        write_test_image_with_cluster_size(&source, 4, cluster_size_bytes)
            .expect("write source image A");

        let mut compressor_args = BTreeMap::new();
        if cluster_size_bytes == 1_048_576 {
            compressor_args.insert("cluster_size".to_owned(), "1M".to_owned());
        }
        let zstd_opts = "-19 -T0";
        let extra_opts = if compression_type == CompressionType::Zstd {
            zstd_opts
        } else {
            ""
        };

        // A → B
        compress_qcow2_image(
            &source,
            &intermediate,
            compression_type,
            &compressor_args,
            extra_opts,
        )
        .expect("compress A → B");

        // B → C (source is itself a natively-compressed qcow2)
        compress_qcow2_image(
            &intermediate,
            &final_image,
            compression_type,
            &compressor_args,
            extra_opts,
        )
        .expect("compress B → C");

        // C must decode to the same data as A.
        assert_roundtrip_data(&final_image, 4, cluster_size_bytes);
    }

    #[test]
    fn native_compress_double_compress_zstd_4k() {
        run_double_compress_test(CompressionType::Zstd, 4096);
    }

    #[test]
    fn native_compress_double_compress_zlib_4k() {
        run_double_compress_test(CompressionType::Zlib, 4096);
    }

    #[test]
    fn native_compress_double_compress_zstd_1m() {
        run_double_compress_test(CompressionType::Zstd, 1_048_576);
    }

    #[test]
    fn native_compress_double_compress_zlib_1m() {
        run_double_compress_test(CompressionType::Zlib, 1_048_576);
    }

    #[test]
    fn native_compress_sets_copied_flags_zstd_4k() {
        run_copied_flag_test(CompressionType::Zstd, 4096);
    }

    #[test]
    fn native_compress_sets_copied_flags_zlib_4k() {
        run_copied_flag_test(CompressionType::Zlib, 4096);
    }

    #[test]
    fn native_compress_sets_copied_flags_zstd_1m() {
        run_copied_flag_test(CompressionType::Zstd, 1_048_576);
    }

    #[test]
    fn native_compress_sets_copied_flags_zlib_1m() {
        run_copied_flag_test(CompressionType::Zlib, 1_048_576);
    }

    #[test]
    fn native_compress_round_trip_realistic_zlib_default_cluster_size() {
        run_realistic_round_trip_test(CompressionType::Zlib, 65_536, None, "");
    }

    #[test]
    fn native_compress_round_trip_realistic_zstd_default_cluster_size() {
        run_realistic_round_trip_test(CompressionType::Zstd, 65_536, None, "-19 -T0");
    }

    #[test]
    fn native_compress_round_trip_realistic_zlib_small_target_cluster_size() {
        run_realistic_round_trip_test(CompressionType::Zlib, 65_536, Some(4096), "");
    }

    #[test]
    fn native_compress_round_trip_realistic_zstd_small_target_cluster_size() {
        run_realistic_round_trip_test(CompressionType::Zstd, 65_536, Some(4096), "-19 -T0");
    }

    // -------------------------------------------------------------------------
    // Regression: zstd -T0 must produce single-frame, content-sized clusters.
    //
    // Before the fix, ZstdCompressor::compress_cluster called multithread(workers)
    // which could emit a worker-chunked multi-frame stream that qemu's
    // qcow2_zstd_decompress (single ZSTD_decompressStream pass) rejects with
    // -EIO.  The assert_compressed_entries_decode_with_declared_codec helper
    // now catches this: it asserts each stored payload is exactly one zstd frame
    // with a pledged content size.
    // -------------------------------------------------------------------------

    #[test]
    fn native_compress_zstd_t0_produces_single_frame_content_sized_clusters() {
        let tmp = tempdir().expect("tempdir");
        let source = tmp.path().join("source.qcow2");
        let dest = tmp.path().join("dest.qcow2");

        let cluster_size: u64 = 65_536;
        let cluster_size_usize = usize::try_from(cluster_size).expect("fits usize");
        let cluster_contents = make_realistic_cluster_contents(cluster_size_usize);
        write_custom_test_image_with_cluster_size(&source, &cluster_contents, cluster_size)
            .expect("write source");

        compress_qcow2_image(
            &source,
            &dest,
            CompressionType::Zstd,
            &BTreeMap::new(),
            "-19 -T0",
        )
        .expect("compress");

        // assert_compressed_entries_decode_with_declared_codec now verifies:
        //   (1) each zstd cluster has a pledged content size == cluster_size, and
        //   (2) each stored payload is exactly one frame (no trailing bytes).
        let expected: Vec<u8> = cluster_contents.into_iter().flatten().collect();
        assert_compressed_entries_decode_with_declared_codec(
            &dest,
            &expected,
            cluster_size,
            CompressionType::Zstd,
        );
    }

    // -------------------------------------------------------------------------
    // Optional qemu-img compatibility check.
    //
    // If `qemu-img` is available in PATH, compress a 1M-cluster zstd image and
    // run `qemu-img check` on it.  If qemu-img is not installed the test is
    // silently skipped, so it does not fail CI environments without qemu.
    // -------------------------------------------------------------------------

    #[test]
    fn native_compress_qemu_img_check_zstd_1m() {
        // Skip if qemu-img is not available.
        if !qemu_img_available() {
            eprintln!("qemu-img not found — skipping qemu-img check test");
            return;
        }

        let tmp = tempdir().expect("tempdir");
        let source = tmp.path().join("source.qcow2");
        let dest = tmp.path().join("compressed.qcow2");

        write_test_image_with_cluster_size(&source, 4, 1_048_576).expect("write source");

        let mut args = BTreeMap::new();
        args.insert("cluster_size".to_owned(), "1M".to_owned());
        compress_qcow2_image(&source, &dest, CompressionType::Zstd, &args, "-19 -T0")
            .expect("compress");

        assert_qemu_img_check(&dest);
    }

    #[test]
    fn native_compress_qemu_img_check_double_compress_zstd_1m() {
        if !qemu_img_available() {
            eprintln!("qemu-img not found — skipping qemu-img check test");
            return;
        }

        let tmp = tempdir().expect("tempdir");
        let source = tmp.path().join("a.qcow2");
        let intermediate = tmp.path().join("b.qcow2");
        let final_image = tmp.path().join("c.qcow2");

        write_test_image_with_cluster_size(&source, 4, 1_048_576).expect("write source A");

        let mut args = BTreeMap::new();
        args.insert("cluster_size".to_owned(), "1M".to_owned());

        compress_qcow2_image(
            &source,
            &intermediate,
            CompressionType::Zstd,
            &args,
            "-19 -T0",
        )
        .expect("compress A → B");
        compress_qcow2_image(
            &intermediate,
            &final_image,
            CompressionType::Zstd,
            &args,
            "-19 -T0",
        )
        .expect("compress B → C");

        assert_qemu_img_check(&final_image);
    }

    #[test]
    fn native_compress_qemu_img_check_zlib_1m() {
        if !qemu_img_available() {
            eprintln!("qemu-img not found — skipping qemu-img check test");
            return;
        }

        let tmp = tempdir().expect("tempdir");
        let source = tmp.path().join("source.qcow2");
        let dest = tmp.path().join("compressed.qcow2");

        write_test_image_with_cluster_size(&source, 4, 1_048_576).expect("write source");

        let mut args = BTreeMap::new();
        args.insert("cluster_size".to_owned(), "1M".to_owned());
        compress_qcow2_image(&source, &dest, CompressionType::Zlib, &args, "").expect("compress");

        assert_qemu_img_check(&dest);
    }

    #[test]
    fn native_compress_qemu_img_check_double_compress_zlib_1m() {
        if !qemu_img_available() {
            eprintln!("qemu-img not found — skipping qemu-img check test");
            return;
        }

        let tmp = tempdir().expect("tempdir");
        let source = tmp.path().join("a.qcow2");
        let intermediate = tmp.path().join("b.qcow2");
        let final_image = tmp.path().join("c.qcow2");

        write_test_image_with_cluster_size(&source, 4, 1_048_576).expect("write source A");

        let mut args = BTreeMap::new();
        args.insert("cluster_size".to_owned(), "1M".to_owned());

        compress_qcow2_image(&source, &intermediate, CompressionType::Zlib, &args, "")
            .expect("compress A → B");
        compress_qcow2_image(
            &intermediate,
            &final_image,
            CompressionType::Zlib,
            &args,
            "",
        )
        .expect("compress B → C");

        assert_qemu_img_check(&final_image);
    }
}
