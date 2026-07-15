use anyhow::{bail, Context, Result};
use flate2::read::DeflateDecoder;
use flate2::write::DeflateEncoder;
use flate2::Compression;
use std::io::{Read, Write};

use crate::plan::compress::CompressionType;

pub(crate) trait Compressor: Sync {
    fn id(&self) -> &str;
    fn compress_cluster(&self, cluster: &[u8]) -> Result<Vec<u8>>;
    /// Number of rayon worker threads to use for cluster-level parallelism.
    /// `0` means "use all available cores"; `n` means exactly `n` threads.
    /// `ZlibCompressor` always returns `1` (serial); zstd derives this from
    /// the `-T0`/`-Tn` option parsed by `ZstdCompressor::from_opts`.
    fn workers(&self) -> u32;
}

pub(crate) fn build_compressor(
    compression_type: CompressionType,
    raw_opts: &str,
) -> Result<Box<dyn Compressor + Sync + Send>> {
    match compression_type {
        CompressionType::Zstd => Ok(Box::new(ZstdCompressor::from_opts(raw_opts)?)),
        CompressionType::Zlib => Ok(Box::new(ZlibCompressor::from_opts(raw_opts)?)),
    }
}

pub(crate) fn decompress_cluster(
    compression_type: CompressionType,
    compressed: &[u8],
    cluster_size: usize,
) -> Result<Vec<u8>> {
    match compression_type {
        CompressionType::Zstd => {
            // Trim the input to exactly the first zstd frame, discarding any
            // trailing sector-padding bytes.  The streaming decoder would
            // otherwise try to parse padding zeros as a second frame and fail
            // with "Unknown frame descriptor".
            let frame_end = zstd::zstd_safe::find_frame_compressed_size(compressed)
                .unwrap_or(compressed.len())
                .min(compressed.len());
            let mut decoder = zstd::stream::read::Decoder::with_buffer(&compressed[..frame_end])
                .context("failed to initialize zstd decoder")?;
            let mut out = Vec::with_capacity(cluster_size);
            decoder
                .read_to_end(&mut out)
                .context("failed to decode zstd qcow2 cluster")?;
            pad_decompressed(out, cluster_size, "zstd")
        }
        CompressionType::Zlib => {
            let mut decoder = DeflateDecoder::new(compressed);
            let mut out = Vec::with_capacity(cluster_size);
            decoder
                .read_to_end(&mut out)
                .context("failed to decode zlib qcow2 cluster")?;
            pad_decompressed(out, cluster_size, "zlib")
        }
    }
}

/// Validates that `decoded` is no larger than `cluster_size`, then zero-pads
/// it to exactly `cluster_size` bytes.  Shared by both codec arms of
/// `decompress_cluster`.
fn pad_decompressed(mut decoded: Vec<u8>, cluster_size: usize, codec: &str) -> Result<Vec<u8>> {
    if decoded.len() > cluster_size {
        bail!(
            "{codec} qcow2 cluster decompressed to {} bytes, expected at most {cluster_size}",
            decoded.len(),
        );
    }
    decoded.resize(cluster_size, 0);
    Ok(decoded)
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ZstdCompressor {
    pub(crate) level: i32,
    pub(crate) workers: u32,
}

impl ZstdCompressor {
    pub(crate) fn from_opts(raw_opts: &str) -> Result<Self> {
        let tokens: Vec<&str> = raw_opts.split_whitespace().collect();
        let mut idx = 0usize;
        let mut parsed = Self {
            level: 3,
            workers: 0,
        };
        while idx < tokens.len() {
            let token = tokens[idx];
            if token == "--ultra" {
                idx += 1;
                continue;
            }
            if let Some(level) = parse_zstd_level(token)? {
                parsed.level = level;
                idx += 1;
                continue;
            }
            if token == "-T" {
                let value = tokens
                    .get(idx + 1)
                    .with_context(|| format!("missing value for zstd option '{token}'"))?;
                parsed.workers = parse_worker_count(value, token)?;
                idx += 2;
                continue;
            }
            if let Some(value) = token.strip_prefix("-T") {
                if value.is_empty() {
                    bail!("missing value for zstd option '{token}'");
                }
                parsed.workers = parse_worker_count(value, token)?;
                idx += 1;
                continue;
            }
            bail!("unknown zstd option '{token}'");
        }
        Ok(parsed)
    }
}

impl Compressor for ZstdCompressor {
    fn id(&self) -> &str {
        "zstd"
    }

    fn compress_cluster(&self, cluster: &[u8]) -> Result<Vec<u8>> {
        // Each cluster is compressed as a single self-contained zstd frame with
        // the content size pledged in the frame header.  qemu's
        // qcow2_zstd_decompress performs a single ZSTD_decompressStream pass and
        // requires exactly one frame per cluster — multi-frame output (produced
        // by libzstd's NbWorkers > 0 multithreaded mode) causes -EIO.
        //
        // Cluster-level parallelism (via rayon) is handled by the caller
        // (compress_qcow2_image).  Do NOT set NbWorkers here.
        let mut compressor = zstd::bulk::Compressor::new(self.level)
            .context("failed to initialize zstd compressor")?;
        compressor
            .include_contentsize(true)
            .context("failed to enable zstd content size")?;
        compressor
            .compress(cluster)
            .context("failed to encode zstd qcow2 cluster")
    }

    fn workers(&self) -> u32 {
        self.workers
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ZlibCompressor;

impl ZlibCompressor {
    pub(crate) fn from_opts(raw_opts: &str) -> Result<Self> {
        if raw_opts.trim().is_empty() {
            return Ok(Self);
        }
        let token = raw_opts
            .split_whitespace()
            .next()
            .unwrap_or_default()
            .to_owned();
        bail!("unknown zlib option '{token}'")
    }
}

impl Compressor for ZlibCompressor {
    fn id(&self) -> &str {
        "zlib"
    }

    fn compress_cluster(&self, cluster: &[u8]) -> Result<Vec<u8>> {
        let mut encoder = DeflateEncoder::new(Vec::new(), Compression::default());
        encoder
            .write_all(cluster)
            .context("failed to encode zlib qcow2 cluster")?;
        encoder
            .finish()
            .context("failed to finish zlib qcow2 cluster compression")
    }

    fn workers(&self) -> u32 {
        1
    }
}

fn parse_zstd_level(token: &str) -> Result<Option<i32>> {
    if !token.starts_with('-') || token.starts_with("--") || token == "-T" {
        return Ok(None);
    }
    let raw = &token[1..];
    if raw.is_empty() || !raw.chars().all(|c| c.is_ascii_digit()) {
        return Ok(None);
    }
    let level = raw
        .parse::<i32>()
        .with_context(|| format!("invalid zstd compression level token '{token}'"))?;
    if !(1..=22).contains(&level) {
        bail!("invalid zstd compression level '{token}': expected -1..-22");
    }
    Ok(Some(level))
}

fn parse_worker_count(value: &str, token: &str) -> Result<u32> {
    value
        .parse::<u32>()
        .with_context(|| format!("invalid worker count for zstd option '{token}': '{value}'"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zstd_opts_parse_level_and_all_cores() {
        let parsed = ZstdCompressor::from_opts("-19 -T0").expect("parse");
        assert_eq!(parsed.level, 19);
        assert_eq!(parsed.workers, 0);
    }

    #[test]
    fn zstd_opts_parse_level_and_fixed_workers() {
        let parsed = ZstdCompressor::from_opts("--ultra -22 -T4").expect("parse");
        assert_eq!(parsed.level, 22);
        assert_eq!(parsed.workers, 4);
    }

    #[test]
    fn zstd_opts_unknown_flag_is_error() {
        let err = ZstdCompressor::from_opts("-19 --foo").expect_err("unknown flag must error");
        assert!(
            err.to_string().contains("--foo"),
            "unexpected error: {err:#}"
        );
    }

    #[test]
    fn compressor_factory_dispatches_zstd() {
        let compressor = build_compressor(CompressionType::Zstd, "-19 -T0").expect("factory");
        assert_eq!(compressor.id(), "zstd");
    }

    #[test]
    fn compressor_factory_dispatches_zlib() {
        let compressor = build_compressor(CompressionType::Zlib, "").expect("factory");
        assert_eq!(compressor.id(), "zlib");
    }

    /// Regression: ZstdCompressor must produce a single self-contained frame with
    /// the content size pledged in the frame header, even when worker opts are set.
    /// qemu's qcow2_zstd_decompress does a single ZSTD_decompressStream pass and
    /// requires exactly one frame per cluster; multi-frame output from multithreaded
    /// compression causes -EIO at boot time.
    #[test]
    fn zstd_compress_cluster_is_single_frame_with_content_size() {
        // Test with both a default compressor and one configured with -T4 workers.
        for opts in &["-19 -T0", "--ultra -22 -T4", "-3"] {
            let compressor =
                ZstdCompressor::from_opts(opts).unwrap_or_else(|e| panic!("parse {opts}: {e}"));
            let cluster = vec![0x42u8; 65_536];
            let compressed = compressor
                .compress_cluster(&cluster)
                .unwrap_or_else(|e| panic!("compress with {opts}: {e}"));

            // Content size must be pledged and must equal the cluster size.
            let content_size = zstd::zstd_safe::get_frame_content_size(&compressed)
                .unwrap_or_else(|_| panic!("{opts}: zstd frame header missing or corrupt"))
                .unwrap_or_else(|| {
                    panic!("{opts}: zstd frame must pledge content size (include_contentsize)")
                });
            assert_eq!(
                content_size as usize,
                cluster.len(),
                "{opts}: pledged content size must equal cluster size"
            );

            // The first frame must span exactly all stored bytes — i.e., exactly
            // one frame, no trailing bytes (which would indicate a multi-frame /
            // worker-chunked stream that qemu cannot decode).
            let first_frame_size = zstd::zstd_safe::find_frame_compressed_size(&compressed)
                .unwrap_or_else(|e| panic!("{opts}: cannot determine first frame size: {e:?}"));
            assert_eq!(
                first_frame_size,
                compressed.len(),
                "{opts}: compressed output must be exactly one zstd frame (no trailing bytes)"
            );
        }
    }

    // -------------------------------------------------------------------------
    // Regression: decompress_cluster must zero-pad when the compressed payload
    // inflates to fewer than cluster_size bytes (e.g. the last cluster of a
    // qemu-produced image whose virtual_size is not a multiple of cluster_size).
    // The old code used read_exact on a cluster_size buffer which returned
    // "corrupt deflate stream" / "unexpected EOF" instead of padding with zeros.
    // -------------------------------------------------------------------------

    #[test]
    fn decompress_cluster_zlib_pads_short_inflate_to_cluster_size() {
        let cluster_size = 4096usize;
        let short_data = vec![0x42u8; 1024]; // fewer bytes than cluster_size
        let mut encoder = DeflateEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(&short_data).expect("encode");
        let compressed = encoder.finish().expect("finish");

        let result = decompress_cluster(CompressionType::Zlib, &compressed, cluster_size)
            .expect("decompress must succeed even when inflated length < cluster_size");

        assert_eq!(
            result.len(),
            cluster_size,
            "output must be exactly cluster_size"
        );
        assert_eq!(
            &result[..1024],
            &short_data[..],
            "inflated bytes must be preserved"
        );
        assert!(
            result[1024..].iter().all(|&b| b == 0),
            "tail bytes must be zero-padded"
        );
    }

    #[test]
    fn decompress_cluster_zstd_pads_short_inflate_to_cluster_size() {
        let cluster_size = 4096usize;
        let short_data = vec![0x42u8; 1024];
        let mut compressor = zstd::bulk::Compressor::new(3).expect("init compressor");
        compressor.include_contentsize(true).expect("contentsize");
        let compressed = compressor.compress(&short_data).expect("compress");

        let result = decompress_cluster(CompressionType::Zstd, &compressed, cluster_size)
            .expect("decompress must succeed even when inflated length < cluster_size");

        assert_eq!(
            result.len(),
            cluster_size,
            "output must be exactly cluster_size"
        );
        assert_eq!(
            &result[..1024],
            &short_data[..],
            "inflated bytes must be preserved"
        );
        assert!(
            result[1024..].iter().all(|&b| b == 0),
            "tail bytes must be zero-padded"
        );
    }

    #[test]
    fn decompress_cluster_zlib_full_size_round_trip() {
        let cluster_size = 4096usize;
        let data = vec![0x5au8; cluster_size];
        let mut encoder = DeflateEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(&data).expect("encode");
        let compressed = encoder.finish().expect("finish");

        let result = decompress_cluster(CompressionType::Zlib, &compressed, cluster_size)
            .expect("decompress full-size cluster");
        assert_eq!(result, data);
    }

    #[test]
    fn decompress_cluster_zstd_full_size_round_trip() {
        let cluster_size = 4096usize;
        let data = vec![0x5au8; cluster_size];
        let mut compressor = zstd::bulk::Compressor::new(3).expect("init compressor");
        compressor.include_contentsize(true).expect("contentsize");
        let compressed = compressor.compress(&data).expect("compress");

        let result = decompress_cluster(CompressionType::Zstd, &compressed, cluster_size)
            .expect("decompress full-size cluster");
        assert_eq!(result, data);
    }
}
