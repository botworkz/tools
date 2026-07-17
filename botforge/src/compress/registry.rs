use anyhow::{anyhow, Result};
use std::collections::BTreeMap;
use std::sync::OnceLock;

use super::codec::{
    decompress_zlib_cluster, decompress_zstd_cluster, Compressor, ZlibCompressor, ZstdCompressor,
};

pub(crate) type CompressorFactory = fn(&str) -> Result<Box<dyn Compressor + Sync + Send>>;
pub(crate) type DecompressClusterFn = fn(&[u8], usize) -> Result<Vec<u8>>;

#[derive(Debug)]
pub(crate) struct CompressorRegistration {
    build: CompressorFactory,
    decompress_cluster: DecompressClusterFn,
}

#[derive(Default)]
pub(crate) struct CompressorRegistry {
    registrations: BTreeMap<&'static str, CompressorRegistration>,
}

impl CompressorRegistry {
    fn with_builtins() -> Self {
        let mut registry = Self::default();
        registry.register("zstd", build_zstd, decompress_zstd_cluster);
        registry.register("zlib", build_zlib, decompress_zlib_cluster);
        registry
    }

    fn register(
        &mut self,
        verb: &'static str,
        build: CompressorFactory,
        decompress_cluster: DecompressClusterFn,
    ) {
        self.registrations.insert(
            verb,
            CompressorRegistration {
                build,
                decompress_cluster,
            },
        );
    }

    fn lookup(&self, verb: &str) -> Option<&CompressorRegistration> {
        self.registrations.get(verb)
    }
}

fn build_zstd(raw_opts: &str) -> Result<Box<dyn Compressor + Sync + Send>> {
    Ok(Box::new(ZstdCompressor::from_opts(raw_opts)?))
}

fn build_zlib(raw_opts: &str) -> Result<Box<dyn Compressor + Sync + Send>> {
    Ok(Box::new(ZlibCompressor::from_opts(raw_opts)?))
}

fn built_in_registry() -> &'static CompressorRegistry {
    static REGISTRY: OnceLock<CompressorRegistry> = OnceLock::new();
    REGISTRY.get_or_init(CompressorRegistry::with_builtins)
}

pub(crate) fn known_compressor_verbs() -> Vec<&'static str> {
    built_in_registry().registrations.keys().copied().collect()
}

/// Returns the built-in compressor verbs plus any extra plugin-registered verbs.
///
/// Used in error messages when plugin verbs have been loaded so that
/// `unknown_compressor_verb_error` names all available verbs including plugins.
pub(crate) fn known_compressor_verbs_with_extras(extra: &[&str]) -> Vec<String> {
    let mut verbs: Vec<String> = built_in_registry()
        .registrations
        .keys()
        .copied()
        .map(str::to_owned)
        .collect();
    for v in extra {
        verbs.push(v.to_string());
    }
    verbs.sort();
    verbs
}

pub(crate) fn lookup_compressor(verb: &str) -> Result<&'static CompressorRegistration> {
    built_in_registry()
        .lookup(verb)
        .ok_or_else(|| unknown_compressor_verb_error(verb))
}

/// Validate a compressor verb against the built-in registry plus any extra
/// verbs supplied by loaded plugins.
///
/// Returns `Ok(())` if the verb is a built-in OR is in `extra_plugin_verbs`.
/// Returns an error naming all known verbs (built-ins + extras) if not found.
pub(crate) fn validate_compressor_verb_with_extras(
    verb: &str,
    extra_plugin_verbs: &[&str],
) -> Result<()> {
    if built_in_registry().lookup(verb).is_some() || extra_plugin_verbs.contains(&verb) {
        return Ok(());
    }
    let known = known_compressor_verbs_with_extras(extra_plugin_verbs);
    Err(anyhow!(
        "unknown compressor verb '{verb}' (known: {})",
        known.join(", ")
    ))
}

fn unknown_compressor_verb_error(verb: &str) -> anyhow::Error {
    anyhow!(
        "unknown compressor verb '{verb}' (known: {})",
        known_compressor_verbs().join(", ")
    )
}

pub(crate) fn build_registered_compressor(
    registration: &CompressorRegistration,
    raw_opts: &str,
) -> Result<Box<dyn Compressor + Sync + Send>> {
    (registration.build)(raw_opts)
}

pub(crate) fn decompress_registered_cluster(
    registration: &CompressorRegistration,
    compressed: &[u8],
    cluster_size: usize,
) -> Result<Vec<u8>> {
    (registration.decompress_cluster)(compressed, cluster_size)
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::write::DeflateEncoder;
    use flate2::Compression;
    use std::io::Write;

    #[test]
    fn known_verb_resolves() {
        let registration = lookup_compressor("zstd").expect("zstd must be registered");
        let compressor = build_registered_compressor(registration, "-19 -T0").expect("build");
        assert_eq!(compressor.id(), "zstd");
    }

    #[test]
    fn unknown_verb_is_error_with_known_verbs() {
        let err = lookup_compressor("bogus").expect_err("unknown verb must fail");
        let msg = err.to_string();
        assert!(
            msg.contains("unknown compressor"),
            "unexpected error: {msg}"
        );
        assert!(msg.contains("bogus"), "unexpected error: {msg}");
        assert!(msg.contains("zstd"), "unexpected error: {msg}");
        assert!(msg.contains("zlib"), "unexpected error: {msg}");
    }

    #[test]
    fn default_verb_is_zstd() {
        let default_cfg = super::super::config::CompressConfig::default();
        assert_eq!(default_cfg.compressor, "zstd");
    }

    #[test]
    fn zlib_is_selectable() {
        let registration = lookup_compressor("zlib").expect("zlib must be registered");
        let compressor = build_registered_compressor(registration, "").expect("build");
        assert_eq!(compressor.id(), "zlib");
    }

    #[test]
    fn registered_decompression_works_for_both_builtins() {
        let cluster_size = 4096usize;
        let data = vec![0x5a; cluster_size];

        let mut zstd = zstd::bulk::Compressor::new(3).expect("init zstd");
        zstd.include_contentsize(true).expect("contentsize");
        let zstd_compressed = zstd.compress(&data).expect("zstd compress");
        let zstd_registration = lookup_compressor("zstd").expect("lookup zstd");
        let zstd_round_trip =
            decompress_registered_cluster(zstd_registration, &zstd_compressed, cluster_size)
                .expect("zstd decompress");
        assert_eq!(zstd_round_trip, data);

        let mut zlib = DeflateEncoder::new(Vec::new(), Compression::default());
        zlib.write_all(&data).expect("zlib write");
        let zlib_compressed = zlib.finish().expect("zlib finish");
        let zlib_registration = lookup_compressor("zlib").expect("lookup zlib");
        let zlib_round_trip =
            decompress_registered_cluster(zlib_registration, &zlib_compressed, cluster_size)
                .expect("zlib decompress");
        assert_eq!(zlib_round_trip, data);
    }
}
