use anyhow::{bail, Context, Result};
use flate2::read::ZlibDecoder;
use flate2::write::ZlibEncoder;
use flate2::Compression;
use std::io::{Read, Write};

use crate::plan::config::CompressionType;

pub(crate) trait Compressor {
    fn id(&self) -> &str;
    fn compress_cluster(&self, cluster: &[u8]) -> Result<Vec<u8>>;
}

pub(crate) fn build_compressor(
    compression_type: CompressionType,
    raw_opts: &str,
) -> Result<Box<dyn Compressor>> {
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
            let mut decoder = zstd::stream::read::Decoder::with_buffer(compressed)
                .context("failed to initialize zstd decoder")?;
            let mut out = vec![0u8; cluster_size];
            decoder
                .read_exact(&mut out)
                .context("failed to decode zstd qcow2 cluster")?;
            Ok(out)
        }
        CompressionType::Zlib => {
            let mut decoder = ZlibDecoder::new(compressed);
            let mut out = vec![0u8; cluster_size];
            decoder
                .read_exact(&mut out)
                .context("failed to decode zlib qcow2 cluster")?;
            Ok(out)
        }
    }
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
        // Per-cluster payloads are at most cluster_size (≤ 2 MiB).
        // Multithreaded zstd compression (NbWorkers > 0) can split the output
        // into a worker-chunked multi-frame stream.  qemu's qcow2_zstd_decompress
        // performs a single ZSTD_decompressStream pass and requires exactly one
        // self-contained frame per cluster — multi-frame output causes -EIO.
        // We therefore intentionally do NOT apply self.workers here; the parsed
        // worker count is preserved so existing specs with -T0/-Tn don't error,
        // but per-cluster single-threaded compression is correct and deterministic.
        let mut compressor = zstd::bulk::Compressor::new(self.level)
            .context("failed to initialize zstd compressor")?;
        compressor
            .include_contentsize(true)
            .context("failed to enable zstd content size")?;
        compressor
            .compress(cluster)
            .context("failed to encode zstd qcow2 cluster")
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
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        encoder
            .write_all(cluster)
            .context("failed to encode zlib qcow2 cluster")?;
        encoder
            .finish()
            .context("failed to finish zlib qcow2 cluster compression")
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
}
