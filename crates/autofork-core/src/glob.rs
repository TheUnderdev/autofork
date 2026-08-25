//! A small, dependency-free path glob matcher — enough for `changed:`
//! triggers and not a byte more.
//!
//! Supported syntax (the subset people actually write in a `changed:`
//! pattern):
//!
//! - `*` — any run of characters within one path segment (never `/`)
//! - `?` — exactly one character within one path segment
//! - `**` — a whole-segment wildcard matching zero or more segments
//! - anything else is a literal
//!
//! A `**` is only special as a *complete* segment (`a/**/b`); `a**b` degrades
//! to two `*`s in one segment, which is the intuitive reading. There are no
//! character classes and no brace expansion: a pattern that wants them is
//! better expressed as two patterns.
//!
//! [`literal_prefix`] returns the deepest wildcard-free directory of a
//! pattern — the watcher scans from there instead of from the filesystem
//! root, which is what keeps a `changed:` trigger from being a whole-disk
//! sweep.

use std::path::{Path, PathBuf};

/// Expand a leading `~` and make `pattern` absolute against `base`.
///
/// Patterns are written in fork/hook frontmatter, where "relative" naturally
/// means "relative to the project this definition belongs to" — the same
/// reading a path in the body would get. An already-absolute pattern is left
/// alone. Only textual normalization happens here: a pattern names files that
/// do not exist yet, so it can never be canonicalized.
pub fn absolutize(pattern: &str, base: &Path, home: Option<&Path>) -> String {
    let p = pattern.trim();
    if let Some(rest) = p.strip_prefix("~/") {
        if let Some(h) = home {
            return normalize_dots(&h.join(rest).to_string_lossy());
        }
    }
    if p == "~" {
        if let Some(h) = home {
            return h.to_string_lossy().into_owned();
        }
    }
    if p.starts_with('/') {
        return normalize_dots(p);
    }
    normalize_dots(&base.join(p).to_string_lossy())
}

/// Collapse `.` and `..` segments textually (no filesystem access — the
/// pattern's tail is wildcards, which cannot be canonicalized).
fn normalize_dots(path: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    for seg in path.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                // `..` past a wildcard segment is meaningless; keep it
                // textual rather than guessing.
                if matches!(out.last(), Some(&s) if s != "..") {
                    out.pop();
                } else {
                    out.push("..");
                }
            }
            s => out.push(s),
        }
    }
    format!("/{}", out.join("/"))
}

/// The deepest directory of `pattern` that contains no wildcard — where a
/// scan for it should start. A pattern with a wildcard in its first segment
/// yields `/`.
pub fn literal_prefix(pattern: &str) -> PathBuf {
    let mut out: Vec<&str> = Vec::new();
    for seg in pattern.split('/') {
        if seg.is_empty() {
            continue;
        }
        if has_wildcard(seg) {
            break;
        }
        out.push(seg);
    }
    // The last literal segment may be the file itself rather than a
    // directory; the caller scans a directory either way, and a scan root
    // that is a file is handled by the scanner (it stats it directly).
    PathBuf::from(format!("/{}", out.join("/")))
}

/// Whether the pattern contains any wildcard at all (a literal pattern is a
/// single path, which the watcher can stat directly).
pub fn has_wildcard(s: &str) -> bool {
    s.contains('*') || s.contains('?')
}

/// Whether `path` matches `pattern`. Both are treated as absolute,
/// slash-separated paths.
pub fn matches(pattern: &str, path: &str) -> bool {
    let pat: Vec<&str> = pattern.split('/').filter(|s| !s.is_empty()).collect();
    let seg: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    match_segments(&pat, &seg)
}

fn match_segments(pat: &[&str], seg: &[&str]) -> bool {
    match pat.first() {
        None => seg.is_empty(),
        Some(&"**") => {
            // `**` swallows zero or more segments; try every split point.
            // (Trailing `**` therefore matches everything below, including
            // nothing — `a/**` matches `a` itself as well as `a/b/c`.)
            for i in 0..=seg.len() {
                if match_segments(&pat[1..], &seg[i..]) {
                    return true;
                }
            }
            false
        }
        Some(p) => match seg.first() {
            Some(s) if match_one(p, s) => match_segments(&pat[1..], &seg[1..]),
            _ => false,
        },
    }
}

/// Match a single path segment against a single pattern segment (`*` and `?`,
/// no `/` crossing).
fn match_one(pat: &str, seg: &str) -> bool {
    let p: Vec<char> = pat.chars().collect();
    let s: Vec<char> = seg.chars().collect();
    // Classic two-pointer wildcard match with backtracking on the last `*`.
    let (mut pi, mut si) = (0usize, 0usize);
    let (mut star, mut mark) = (usize::MAX, 0usize);
    while si < s.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == s[si]) {
            pi += 1;
            si += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = pi;
            mark = si;
            pi += 1;
        } else if star != usize::MAX {
            pi = star + 1;
            mark += 1;
            si = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literal_and_star() {
        assert!(matches("/a/b/c.md", "/a/b/c.md"));
        assert!(!matches("/a/b/c.md", "/a/b/d.md"));
        assert!(matches("/a/*/c.md", "/a/b/c.md"));
        assert!(!matches("/a/*/c.md", "/a/b/x/c.md"));
        assert!(matches("/a/*.md", "/a/note.md"));
        assert!(!matches("/a/*.md", "/a/note.txt"));
        assert!(matches("/a/b?.md", "/a/b1.md"));
        assert!(!matches("/a/b?.md", "/a/b12.md"));
    }

    #[test]
    fn double_star_spans_segments() {
        assert!(matches("/a/**/*.md", "/a/b/c/d.md"));
        assert!(matches("/a/**/*.md", "/a/x.md"));
        assert!(matches("/a/**", "/a/b/c"));
        assert!(matches("/a/**", "/a"));
        assert!(!matches("/a/**/*.md", "/b/x.md"));
        // `**` in the middle, with a literal tail.
        assert!(matches("/h/**/HOOK.md", "/h/x/y/HOOK.md"));
        assert!(matches("/h/**/HOOK.md", "/h/HOOK.md"));
        assert!(!matches("/h/**/HOOK.md", "/h/x/HOOK.txt"));
    }

    #[test]
    fn prefix_is_the_deepest_literal_dir() {
        assert_eq!(literal_prefix("/a/b/**/*.md"), PathBuf::from("/a/b"),);
        assert_eq!(literal_prefix("/a/b/c.md"), PathBuf::from("/a/b/c.md"));
        assert_eq!(literal_prefix("/*/x"), PathBuf::from("/"));
    }

    #[test]
    fn absolutize_expands_home_and_relatives() {
        let home = PathBuf::from("/home/u");
        assert_eq!(
            absolutize("~/notes/**/*.md", Path::new("/proj"), Some(&home)),
            "/home/u/notes/**/*.md"
        );
        assert_eq!(
            absolutize("docs/*.md", Path::new("/proj"), Some(&home)),
            "/proj/docs/*.md"
        );
        assert_eq!(
            absolutize("/abs/*.md", Path::new("/proj"), Some(&home)),
            "/abs/*.md"
        );
        assert_eq!(
            absolutize("./a/../b/*.md", Path::new("/proj"), Some(&home)),
            "/proj/b/*.md"
        );
    }

    #[test]
    fn wildcard_detection() {
        assert!(has_wildcard("*.md"));
        assert!(has_wildcard("a?b"));
        assert!(!has_wildcard("plain.md"));
    }
}
