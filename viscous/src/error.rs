//! Error types for viscous.

use std::path::PathBuf;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("template spec missing: {0}")]
    SpecMissing(PathBuf),

    #[error("template spec invalid: {path}: {source}")]
    SpecInvalid {
        path: PathBuf,
        #[source]
        source: serde_yaml::Error,
    },

    #[error("template root is not a directory: {0}")]
    TemplateRootNotDir(PathBuf),

    #[error("required variable missing: {0}")]
    RequiredVarMissing(String),

    #[error("variable '{name}' has wrong type: expected {expected}, got {got}")]
    VarTypeMismatch {
        name: String,
        expected: String,
        got: String,
    },

    #[error("variable '{name}': {message}")]
    VarValidation { name: String, message: String },

    #[error("derived var '{name}' failed to render: {source}")]
    DerivedVarRender {
        name: String,
        #[source]
        source: liquid::Error,
    },

    #[error("liquid parse error in {path}: {source}")]
    LiquidParse {
        path: PathBuf,
        #[source]
        source: liquid::Error,
    },

    #[error("liquid render error in {path}: {source}")]
    LiquidRender {
        path: PathBuf,
        #[source]
        source: liquid::Error,
    },

    #[error("generator template not found: {0}")]
    GeneratorTemplateMissing(PathBuf),

    #[error("generator #{index}: for_each var '{var}' is not an array (got {got})")]
    ForEachNotArray {
        index: usize,
        var: String,
        got: String,
    },

    #[error("generator #{index}: for_each var '{var}' is not defined")]
    ForEachUndefined { index: usize, var: String },

    #[error("generator #{index}: duplicate dest path '{dest}' within for_each expansion")]
    WithinGeneratorCollision { index: usize, dest: PathBuf },

    #[error(
        "conflict at '{dest}': step {new_step} wants to {action} but step {existing_step} already wrote it (on_conflict={policy})"
    )]
    Conflict {
        dest: PathBuf,
        new_step: usize,
        existing_step: usize,
        action: &'static str,
        policy: String,
    },

    #[error(
        "overwrite/append at '{dest}' (step {step}) has nothing to act on (no earlier step wrote it); use 'upsert' if that's intentional"
    )]
    NothingToOverride { dest: PathBuf, step: usize },

    #[error("destination is not empty: {0}")]
    DestNotEmpty(PathBuf),

    #[error("destination is a file, not a directory: {0}")]
    DestIsFile(PathBuf),

    #[error("io error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("io error: {0}")]
    BareIo(#[from] std::io::Error),

    #[error("walkdir error: {0}")]
    Walk(#[from] walkdir::Error),
}

pub type Result<T> = std::result::Result<T, Error>;
