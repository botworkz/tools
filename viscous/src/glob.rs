//! Tiny glob matcher.
//!
//! Supports `*`, `?`, and `**`. Patterns are anchored to the relative path
//! root (the template directory for static-walker matches).
//!
//! Deliberately lightweight to avoid pulling in `globset`/`ignore` — viscous
//! needs minimal pattern support, not gitignore-grade behaviour.

use regex::Regex;

/// A compiled set of glob patterns.
#[derive(Debug, Default)]
pub struct Matcher {
    patterns: Vec<Regex>,
}

impl Matcher {
    pub fn new(patterns: &[String]) -> Self {
        let compiled = patterns.iter().filter_map(|p| compile(p)).collect();
        Self { patterns: compiled }
    }

    pub fn matches(&self, rel_path: &str) -> bool {
        self.patterns.iter().any(|p| p.is_match(rel_path))
    }
}

fn compile(glob: &str) -> Option<Regex> {
    let mut re = String::from("^");
    let mut chars = glob.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '*' => {
                if chars.peek() == Some(&'*') {
                    chars.next();
                    if chars.peek() == Some(&'/') {
                        chars.next();
                        // `**/` matches zero or more path segments.
                        re.push_str("(?:.*/)?");
                    } else {
                        re.push_str(".*");
                    }
                } else {
                    re.push_str("[^/]*");
                }
            }
            '?' => re.push_str("[^/]"),
            '.' | '+' | '(' | ')' | '|' | '^' | '$' | '{' | '}' | '[' | ']' | '\\' => {
                re.push('\\');
                re.push(c);
            }
            other => re.push(other),
        }
    }
    re.push('$');
    Regex::new(&re).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn star_matches_within_segment() {
        let m = Matcher::new(&["*.bak".to_string()]);
        assert!(m.matches("foo.bak"));
        assert!(!m.matches("foo/bar.bak")); // `*` does not cross slashes
    }

    #[test]
    fn double_star_crosses_segments() {
        let m = Matcher::new(&["**/*.yml".to_string()]);
        assert!(m.matches("ci.yml"));
        assert!(m.matches(".github/workflows/ci.yml"));
    }

    #[test]
    fn literal_paths_match_exactly() {
        let m = Matcher::new(&["LICENSE".to_string()]);
        assert!(m.matches("LICENSE"));
        assert!(!m.matches("MIT-LICENSE"));
    }

    #[test]
    fn escapes_regex_metachars() {
        let m = Matcher::new(&["foo.bar".to_string()]);
        assert!(m.matches("foo.bar"));
        assert!(!m.matches("foozbar"));
    }
}
