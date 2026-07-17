use serde::Deserialize;

/// Output-compression options for `botforge build`.
///
/// Modelled as an optional map with a required `enabled:` field so it can
/// carry additional knobs without changing shape.  The struct is kept strict
/// (`deny_unknown_fields`) to catch typos at parse time.
#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub(crate) enum ReclaimMode {
    /// Default — no reclaim step before commit.
    #[default]
    None,
    /// Run in-guest `fstrim` as the last guest action before shutdown.
    Fstrim,
    /// Run host-side offline reclaim via qemu-nbd discard+fstrim after shutdown.
    Discard,
}

fn default_compressor_verb() -> String {
    "zstd".to_owned()
}

/// Output-compression options for `botforge build`.
///
/// `reclaim` is nested under `compress` because it is primarily used to make
/// qcow2 compression effective by reclaiming freed guest blocks before commit.
/// `reclaim` still runs even when `enabled: false` (plain rename) so users can
/// reclaim space without compression.
///
/// ```yaml
/// # default off — plain atomic rename (byte-identical to today)
/// # compress: absent
///
/// # on, qemu default cluster size
/// compress:
///   enabled: true
///   # compressor defaults to zstd
///
/// # on, explicit options via compressor_args map
/// compress:
///   enabled: true
///   compressor: zstd
///   compressor_args:
///     cluster_size: "1M"
///   compressor_opts: "-19 -T0"
///
/// # reclaim freed blocks before commit/compress
/// compress:
///   enabled: true
///   reclaim: fstrim
///
/// # explicit off (equivalent to omitting the block)
/// compress:
///   enabled: false
/// ```
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CompressConfig {
    /// Whether compression is enabled.  Required — a `compress:` block without
    /// `enabled:` is a hard parse error.
    pub(crate) enabled: bool,
    /// Compression algorithm passed as `-o compression_type=<val>` to
    /// the native qcow2 compression writer.
    ///
    /// Defaults to `zstd`, which requires qemu >= 5.1 only on consumers that
    /// open the produced qcow2.
    #[serde(default = "default_compressor_verb")]
    pub(crate) compressor: String,
    /// Optional qcow2-structural key=value options interpreted by botforge's
    /// native qcow2 writer. Keys are sorted (BTreeMap) so the stored config is
    /// deterministic.
    ///
    /// Example: `{cluster_size: "1M"}` changes the target qcow2 cluster size.
    #[serde(default)]
    pub(crate) compressor_args: std::collections::BTreeMap<String, String>,
    /// Optional raw codec options string passed to the selected in-process
    /// compressor implementation, which parses and validates it.
    #[serde(default)]
    pub(crate) compressor_opts: String,
    /// Optional reclaim mode that runs before commit/compress.
    ///
    /// Defaults to `none`. Runs even when `enabled: false`.
    #[serde(default)]
    pub(crate) reclaim: ReclaimMode,
}

impl Default for CompressConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            compressor: default_compressor_verb(),
            compressor_args: Default::default(),
            compressor_opts: String::new(),
            reclaim: ReclaimMode::None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ReclaimMode;
    use crate::config::load_build_config;
    use tempfile::TempDir;

    fn write_build_config(repo: &TempDir, name: &str, content: &str) {
        std::fs::write(repo.path().join(name), content).unwrap();
    }

    // -----------------------------------------------------------------
    // compress field tests
    // -----------------------------------------------------------------

    #[test]
    fn test_load_build_config_compress_absent_is_none() {
        let repo = TempDir::new().unwrap();
        write_build_config(
        &repo,
        "build.yaml",
        "type: botforge/build\nname: build\nimage: \"@base\"\noutput: \"out.qcow2\"\nsteps: []\n",
    );
        let config = load_build_config(repo.path(), &repo.path().join("build.yaml")).unwrap();
        assert!(
            config.compress.is_none(),
            "absent compress must deserialize as None"
        );
    }

    #[test]
    fn test_load_build_config_compress_enabled_true_no_cluster_size() {
        let repo = TempDir::new().unwrap();
        write_build_config(
        &repo,
        "build.yaml",
        "type: botforge/build\nname: build\nimage: \"@base\"\noutput: \"out.qcow2\"\nsteps: []\ncompress:\n  enabled: true\n",
    );
        let config = load_build_config(repo.path(), &repo.path().join("build.yaml")).unwrap();
        let compress = config.compress.expect("compress should be Some");
        assert!(compress.enabled, "enabled must be true");
        assert_eq!(
            compress.compressor, "zstd",
            "compressor must default to zstd"
        );
        assert!(
            compress.compressor_args.is_empty(),
            "compressor_args must default to empty"
        );
        assert!(
            compress.compressor_opts.is_empty(),
            "compressor_opts must default to empty"
        );
        assert_eq!(
            compress.reclaim,
            ReclaimMode::None,
            "reclaim must default to none"
        );
    }

    #[test]
    fn test_load_build_config_compress_enabled_true_with_cluster_size_in_args() {
        let repo = TempDir::new().unwrap();
        write_build_config(
        &repo,
        "build.yaml",
        "type: botforge/build\nname: build\nimage: \"@base\"\noutput: \"out.qcow2\"\nsteps: []\ncompress:\n  enabled: true\n  compressor_args:\n    cluster_size: \"1M\"\n",
    );
        let config = load_build_config(repo.path(), &repo.path().join("build.yaml")).unwrap();
        let compress = config.compress.expect("compress should be Some");
        assert!(compress.enabled);
        assert_eq!(compress.compressor, "zstd");
        assert_eq!(
            compress
                .compressor_args
                .get("cluster_size")
                .map(String::as_str),
            Some("1M")
        );
        assert_eq!(compress.reclaim, ReclaimMode::None);
    }

    #[test]
    fn test_load_build_config_compress_explicit_compressor_zstd() {
        let repo = TempDir::new().unwrap();
        write_build_config(
        &repo,
        "build.yaml",
        "type: botforge/build\nname: build\nimage: \"@base\"\noutput: \"out.qcow2\"\nsteps: []\ncompress:\n  enabled: true\n  compressor: zstd\n",
    );
        let config = load_build_config(repo.path(), &repo.path().join("build.yaml")).unwrap();
        let compress = config.compress.expect("compress should be Some");
        assert!(compress.enabled);
        assert_eq!(compress.compressor, "zstd");
    }

    #[test]
    fn test_load_build_config_compress_explicit_compressor_zlib() {
        let repo = TempDir::new().unwrap();
        write_build_config(
        &repo,
        "build.yaml",
        "type: botforge/build\nname: build\nimage: \"@base\"\noutput: \"out.qcow2\"\nsteps: []\ncompress:\n  enabled: true\n  compressor: zlib\n",
    );
        let config = load_build_config(repo.path(), &repo.path().join("build.yaml")).unwrap();
        let compress = config.compress.expect("compress should be Some");
        assert!(compress.enabled);
        assert_eq!(compress.compressor, "zlib");
    }

    #[test]
    fn test_load_build_config_compress_explicit_compressor_args() {
        let repo = TempDir::new().unwrap();
        write_build_config(
        &repo,
        "build.yaml",
        "type: botforge/build\nname: build\nimage: \"@base\"\noutput: \"out.qcow2\"\nsteps: []\ncompress:\n  enabled: true\n  compressor_args:\n    cluster_size: \"1M\"\n",
    );
        let config = load_build_config(repo.path(), &repo.path().join("build.yaml")).unwrap();
        let compress = config.compress.expect("compress should be Some");
        assert_eq!(
            compress
                .compressor_args
                .get("cluster_size")
                .map(String::as_str),
            Some("1M")
        );
        assert_eq!(compress.compressor_args.len(), 1);
    }

    #[test]
    fn test_load_build_config_compress_explicit_compressor_opts() {
        let repo = TempDir::new().unwrap();
        write_build_config(
        &repo,
        "build.yaml",
        "type: botforge/build\nname: build\nimage: \"@base\"\noutput: \"out.qcow2\"\nsteps: []\ncompress:\n  enabled: true\n  compressor_opts: \"-19 -T0\"\n",
    );
        let config = load_build_config(repo.path(), &repo.path().join("build.yaml")).unwrap();
        let compress = config.compress.expect("compress should be Some");
        assert_eq!(compress.compressor_opts, "-19 -T0");
    }

    #[test]
    fn test_load_build_config_compress_enabled_false() {
        let repo = TempDir::new().unwrap();
        write_build_config(
        &repo,
        "build.yaml",
        "type: botforge/build\nname: build\nimage: \"@base\"\noutput: \"out.qcow2\"\nsteps: []\ncompress:\n  enabled: false\n",
    );
        let config = load_build_config(repo.path(), &repo.path().join("build.yaml")).unwrap();
        let compress = config.compress.expect("compress should be Some");
        assert!(!compress.enabled, "enabled must be false");
        assert_eq!(compress.compressor, "zstd");
        assert_eq!(compress.reclaim, ReclaimMode::None);
    }

    #[test]
    fn test_load_build_config_compress_reclaim_fstrim() {
        let repo = TempDir::new().unwrap();
        write_build_config(
        &repo,
        "build.yaml",
        "type: botforge/build\nname: build\nimage: \"@base\"\noutput: \"out.qcow2\"\nsteps: []\ncompress:\n  enabled: true\n  reclaim: fstrim\n",
    );
        let config = load_build_config(repo.path(), &repo.path().join("build.yaml")).unwrap();
        let compress = config.compress.expect("compress should be Some");
        assert!(compress.enabled);
        assert_eq!(compress.reclaim, ReclaimMode::Fstrim);
    }

    #[test]
    fn test_load_build_config_compress_reclaim_discard() {
        let repo = TempDir::new().unwrap();
        write_build_config(
        &repo,
        "build.yaml",
        "type: botforge/build\nname: build\nimage: \"@base\"\noutput: \"out.qcow2\"\nsteps: []\ncompress:\n  enabled: true\n  reclaim: discard\n",
    );
        let config = load_build_config(repo.path(), &repo.path().join("build.yaml")).unwrap();
        let compress = config.compress.expect("compress should be Some");
        assert!(compress.enabled);
        assert_eq!(compress.reclaim, ReclaimMode::Discard);
    }

    #[test]
    fn test_load_build_config_compress_reclaim_none() {
        let repo = TempDir::new().unwrap();
        write_build_config(
        &repo,
        "build.yaml",
        "type: botforge/build\nname: build\nimage: \"@base\"\noutput: \"out.qcow2\"\nsteps: []\ncompress:\n  enabled: true\n  reclaim: none\n",
    );
        let config = load_build_config(repo.path(), &repo.path().join("build.yaml")).unwrap();
        let compress = config.compress.expect("compress should be Some");
        assert!(compress.enabled);
        assert_eq!(compress.reclaim, ReclaimMode::None);
    }

    #[test]
    fn test_load_build_config_compress_reclaim_sparsify_is_unknown_variant() {
        let repo = TempDir::new().unwrap();
        write_build_config(
        &repo,
        "build.yaml",
        "type: botforge/build\nname: build\nimage: \"@base\"\noutput: \"out.qcow2\"\nsteps: []\ncompress:\n  enabled: true\n  reclaim: sparsify\n",
    );
        let err = load_build_config(repo.path(), &repo.path().join("build.yaml")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("sparsify") || msg.contains("unknown variant"),
            "sparsify reclaim mode should now be rejected as unknown variant: {msg}"
        );
    }

    #[test]
    fn test_load_build_config_compress_enabled_false_reclaim_fstrim() {
        let repo = TempDir::new().unwrap();
        write_build_config(
        &repo,
        "build.yaml",
        "type: botforge/build\nname: build\nimage: \"@base\"\noutput: \"out.qcow2\"\nsteps: []\ncompress:\n  enabled: false\n  reclaim: fstrim\n",
    );
        let config = load_build_config(repo.path(), &repo.path().join("build.yaml")).unwrap();
        let compress = config.compress.expect("compress should be Some");
        assert!(!compress.enabled);
        assert_eq!(compress.reclaim, ReclaimMode::Fstrim);
    }

    #[test]
    fn test_load_build_config_compress_missing_enabled_is_error() {
        let repo = TempDir::new().unwrap();
        write_build_config(
        &repo,
        "build.yaml",
        "type: botforge/build\nname: build\nimage: \"@base\"\noutput: \"out.qcow2\"\nsteps: []\ncompress:\n  reclaim: fstrim\n",
    );
        let err = load_build_config(repo.path(), &repo.path().join("build.yaml")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("enabled") || msg.contains("missing"),
            "error should mention missing enabled field: {msg}"
        );
    }

    #[test]
    fn test_load_build_config_compress_reclaim_missing_enabled_is_error() {
        let repo = TempDir::new().unwrap();
        write_build_config(
        &repo,
        "build.yaml",
        "type: botforge/build\nname: build\nimage: \"@base\"\noutput: \"out.qcow2\"\nsteps: []\ncompress:\n  reclaim: fstrim\n",
    );
        let err = load_build_config(repo.path(), &repo.path().join("build.yaml")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("enabled") || msg.contains("missing"),
            "error should mention missing enabled field: {msg}"
        );
    }

    #[test]
    fn test_load_build_config_compress_unknown_field_is_error() {
        let repo = TempDir::new().unwrap();
        write_build_config(
        &repo,
        "build.yaml",
        "type: botforge/build\nname: build\nimage: \"@base\"\noutput: \"out.qcow2\"\nsteps: []\ncompress:\n  enabled: true\n  bogus: 1\n",
    );
        let err = load_build_config(repo.path(), &repo.path().join("build.yaml")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("bogus") || msg.contains("unknown"),
            "error should mention unknown field: {msg}"
        );
    }

    #[test]
    fn test_load_build_config_compress_reclaim_unknown_value_is_error() {
        let repo = TempDir::new().unwrap();
        write_build_config(
        &repo,
        "build.yaml",
        "type: botforge/build\nname: build\nimage: \"@base\"\noutput: \"out.qcow2\"\nsteps: []\ncompress:\n  enabled: true\n  reclaim: bogus\n",
    );
        let err = load_build_config(repo.path(), &repo.path().join("build.yaml")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("bogus") || msg.contains("unknown variant"),
            "error should mention reclaim enum variant parse failure: {msg}"
        );
    }

    #[test]
    fn test_load_build_config_compress_compressor_validation_deferred_to_use_time() {
        // Compressor verb validation is deferred from config load to use time
        // so that plugin-provided verbs (e.g. "pigz") are accepted at load
        // time.  Loading a config with an unknown verb SUCCEEDS; the error
        // occurs at build time when validate_compressor_verb_with_extras is
        // called against the combined built-in + plugin registry.
        let repo = TempDir::new().unwrap();
        write_build_config(
        &repo,
        "build.yaml",
        "type: botforge/build\nname: build\nimage: \"@base\"\noutput: \"out.qcow2\"\nsteps: []\ncompress:\n  enabled: true\n  compressor: bogus\n",
    );
        let config = load_build_config(repo.path(), &repo.path().join("build.yaml")).unwrap();
        let compress = config.compress.expect("compress should be Some");
        assert_eq!(compress.compressor, "bogus");
        // Verify that the use-time validation function does reject unknown verbs:
        let err =
            super::super::registry::validate_compressor_verb_with_extras("bogus", &[]).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("bogus") && msg.contains("unknown compressor"),
            "use-time validation must reject unknown verb: {msg}"
        );
        // And that plugin verbs can be accepted by the extras variant:
        let ok = super::super::registry::validate_compressor_verb_with_extras("bogus", &["bogus"]);
        assert!(ok.is_ok(), "verb in extras must be accepted");
    }

    #[test]
    fn test_load_build_config_compress_compression_type_key_is_unknown_field() {
        let repo = TempDir::new().unwrap();
        write_build_config(
        &repo,
        "build.yaml",
        "type: botforge/build\nname: build\nimage: \"@base\"\noutput: \"out.qcow2\"\nsteps: []\ncompress:\n  enabled: true\n  compression_type: zstd\n",
    );
        let err = load_build_config(repo.path(), &repo.path().join("build.yaml")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("compression_type") || msg.contains("unknown field"),
            "error should mention the removed compression_type key: {msg}"
        );
    }

    #[test]
    fn test_load_build_config_compress_cluster_size_top_level_is_unknown_field() {
        let repo = TempDir::new().unwrap();
        write_build_config(
        &repo,
        "build.yaml",
        "type: botforge/build\nname: build\nimage: \"@base\"\noutput: \"out.qcow2\"\nsteps: []\ncompress:\n  enabled: true\n  cluster_size: \"1M\"\n",
    );
        let err = load_build_config(repo.path(), &repo.path().join("build.yaml")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("cluster_size") || msg.contains("unknown field"),
            "cluster_size at top level should now be an unknown field error: {msg}"
        );
    }

    #[test]
    fn test_load_build_config_compress_reclaim_typo_key_is_error() {
        let repo = TempDir::new().unwrap();
        write_build_config(
        &repo,
        "build.yaml",
        "type: botforge/build\nname: build\nimage: \"@base\"\noutput: \"out.qcow2\"\nsteps: []\ncompress:\n  enabled: true\n  recliam: fstrim\n",
    );
        let err = load_build_config(repo.path(), &repo.path().join("build.yaml")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("recliam") || msg.contains("unknown field"),
            "error should mention typo key in strict compress map: {msg}"
        );
    }

    #[test]
    fn test_load_test_config_rejects_compress_section() {
        let repo = TempDir::new().unwrap();
        std::fs::write(
            repo.path().join("test.yaml"),
            "type: botforge/test\nname: test\ncompress:\n  enabled: true\nsteps: []\n",
        )
        .unwrap();
        let err = crate::config::load_test_config(repo.path(), &repo.path().join("test.yaml"))
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("compress") && msg.contains("type: botforge/test"),
            "error should reject compress in test doc: {msg}"
        );
    }

    #[test]
    fn test_load_fragment_rejects_compress_section() {
        let repo = TempDir::new().unwrap();
        std::fs::write(
            repo.path().join("frag.yaml"),
            "type: botforge/fragment\ncompress:\n  enabled: true\nsteps: []\n",
        )
        .unwrap();
        std::fs::write(
            repo.path().join("test.yaml"),
            "type: botforge/test\nname: test\nsteps:\n  - uses: \"@://frag.yaml\"\n",
        )
        .unwrap();
        let err = crate::config::load_test_config(repo.path(), &repo.path().join("test.yaml"))
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("compress"),
            "error should reject compress in fragment doc: {msg}"
        );
    }
}
