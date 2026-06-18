//! Walk the static template tree and produce static plan operations.
//!
//! "Static tree" = everything in the template directory except:
//!   - `__template__.yaml` (the spec itself)
//!   - `__templates__/` (generator templates, invoked declaratively)
//!   - User-declared `ignore:` globs
//!
//! Each file in the static tree is rendered through liquid (filename + body)
//! unless it matches a `verbatim:` glob — in which case the body is copied
//! byte-for-byte but the filename is still rendered.

use crate::engine;
use crate::error::{Error, Result};
use crate::glob::Matcher;
use crate::spec::{Spec, GENERATORS_DIRNAME, SPEC_FILENAME};
use liquid::Parser;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

/// One walked static-tree entry, with the path rewritten through liquid and
/// the body resolved.
pub struct StaticEntry {
    pub source: PathBuf,
    pub dest: PathBuf,
    pub bytes: Vec<u8>,
}

/// Walk `template_dir`, rendering filenames and (unless verbatim) contents.
pub fn walk(
    template_dir: &Path,
    spec: &Spec,
    parser: &Parser,
    vars: &liquid::Object,
) -> Result<Vec<StaticEntry>> {
    let ignore = Matcher::new(&spec.ignore);
    let verbatim = Matcher::new(&spec.verbatim);

    let mut out = Vec::new();
    let walker = WalkDir::new(template_dir)
        .sort_by_file_name()
        .into_iter()
        .filter_entry(|e| keep_entry(template_dir, e));

    for entry in walker {
        let entry = entry?;
        if !entry.file_type().is_file() {
            continue;
        }
        let abs = entry.path();
        let rel = abs.strip_prefix(template_dir).map_err(|_| Error::Io {
            path: abs.to_path_buf(),
            source: std::io::Error::other("could not strip template_dir prefix"),
        })?;
        let rel_str = rel.to_string_lossy().replace('\\', "/");
        if ignore.matches(&rel_str) {
            continue;
        }

        // Render the destination path through liquid; supports `{{var}}` in
        // any path segment.
        let dest_str = engine::render_expr(parser, &rel_str, vars)?;
        let dest = PathBuf::from(dest_str);

        let raw = std::fs::read(abs).map_err(|e| Error::Io {
            path: abs.to_path_buf(),
            source: e,
        })?;

        let bytes = if verbatim.matches(&rel_str) || !looks_like_text(&raw) {
            raw
        } else {
            let source = String::from_utf8(raw.clone()).unwrap_or_default();
            engine::render(parser, &source, vars, abs)?.into_bytes()
        };

        out.push(StaticEntry {
            source: abs.to_path_buf(),
            dest,
            bytes,
        });
    }
    Ok(out)
}

fn keep_entry(template_dir: &Path, entry: &walkdir::DirEntry) -> bool {
    // Always keep the root itself.
    if entry.depth() == 0 {
        return true;
    }
    let rel = match entry.path().strip_prefix(template_dir) {
        Ok(r) => r,
        Err(_) => return false,
    };
    // Skip the spec file and the generators directory at the top level.
    if entry.depth() == 1 {
        let name = rel.file_name().and_then(|s| s.to_str()).unwrap_or("");
        if name == SPEC_FILENAME || name == GENERATORS_DIRNAME {
            return false;
        }
    }
    // Skip nested .git dirs too — sometimes templates are kept under git.
    if entry.file_type().is_dir() {
        if let Some(name) = rel.file_name().and_then(|s| s.to_str()) {
            if name == ".git" {
                return false;
            }
        }
    }
    true
}

/// Heuristic: if a file contains a NUL byte in the first 8 KiB, treat it as
/// binary and skip liquid rendering. Catches images, compiled assets, etc.
fn looks_like_text(bytes: &[u8]) -> bool {
    let take = bytes.len().min(8192);
    !bytes[..take].contains(&0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::parser;
    use std::fs;

    fn touch(p: &Path, body: &str) {
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(p, body).unwrap();
    }

    #[test]
    fn skips_spec_and_generators_dir() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        touch(&root.join(SPEC_FILENAME), "name: t\n");
        touch(&root.join("__templates__/foo.liquid"), "hello");
        touch(&root.join("src/main.rs"), "fn main() {}");
        touch(&root.join("Cargo.toml"), "[package]\nname = \"x\"");

        let spec = crate::spec::Spec {
            name: "t".into(),
            description: "".into(),
            version: "".into(),
            vars: Default::default(),
            derived: Default::default(),
            generate: vec![],
            verbatim: vec![],
            ignore: vec![],
        };
        let p = parser().unwrap();
        let entries = walk(root, &spec, &p, &liquid::Object::new()).unwrap();
        let dests: Vec<_> = entries
            .iter()
            .map(|e| e.dest.to_string_lossy().to_string())
            .collect();
        assert!(dests.contains(&"Cargo.toml".to_string()));
        assert!(dests.contains(&"src/main.rs".to_string()));
        assert!(!dests.iter().any(|d| d.contains("__template")));
    }

    #[test]
    fn renders_filename_and_body() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        touch(&root.join(SPEC_FILENAME), "name: t\n");
        touch(&root.join("src/{{ comp }}.rs"), "pub struct {{ comp }};");

        let spec = crate::spec::Spec {
            name: "t".into(),
            description: "".into(),
            version: "".into(),
            vars: Default::default(),
            derived: Default::default(),
            generate: vec![],
            verbatim: vec![],
            ignore: vec![],
        };
        let p = parser().unwrap();
        let mut vars = liquid::Object::new();
        vars.insert(
            "comp".into(),
            liquid_core::Value::scalar("Button".to_string()),
        );

        let entries = walk(root, &spec, &p, &vars).unwrap();
        let entry = entries
            .iter()
            .find(|e| e.dest == Path::new("src/Button.rs"))
            .expect("rendered dest missing");
        assert_eq!(entry.bytes, b"pub struct Button;");
    }

    #[test]
    fn ignore_globs_drop_files() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        touch(&root.join(SPEC_FILENAME), "name: t\n");
        touch(&root.join("keep.txt"), "k");
        touch(&root.join("drop.bak"), "d");

        let spec = crate::spec::Spec {
            name: "t".into(),
            description: "".into(),
            version: "".into(),
            vars: Default::default(),
            derived: Default::default(),
            generate: vec![],
            verbatim: vec![],
            ignore: vec!["*.bak".into()],
        };
        let p = parser().unwrap();
        let entries = walk(root, &spec, &p, &liquid::Object::new()).unwrap();
        let dests: Vec<_> = entries
            .iter()
            .map(|e| e.dest.to_string_lossy().to_string())
            .collect();
        assert!(dests.contains(&"keep.txt".to_string()));
        assert!(!dests.contains(&"drop.bak".to_string()));
    }
}
