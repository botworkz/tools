mod codec;
mod config;
mod qcow2;
mod registry;

pub(crate) use config::{CompressConfig, ReclaimMode};
#[allow(unused_imports)]
pub(crate) use qcow2::{
    compress_qcow2_image, create_qcow2_overlay, parse_size_string, read_qcow2_image_stats,
    read_virtual_sector0, resize_qcow2_image, sparsify_zero_clusters, Qcow2ImageStats,
    ZeroClusterSparsifyStats,
};
pub(crate) use registry::{
    known_compressor_verbs_with_extras, validate_compressor_verb_with_extras,
};
