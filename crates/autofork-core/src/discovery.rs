//! Fork discovery: scan forks trees and skill folders for fork definitions.
//!
//! Forks are discovered from three kinds of places, per ancestor directory
//! (nearest-project-first) and then at the user level:
//!
//! - `.autofork/forks/` trees (autofork's own layout)
//! - `.claude/forks/` trees (a `forks/` dir next to the skills dir)
//! - `.claude/skills/**` skill folders holding a `FORK.md` next to their
//!   `SKILL.md` — a **skill-attached fork**, named after the skill; its
//!   spawn prompt tells the fork to load the skill first if it isn't
//!   already in context
//!
//! Two layouts are supported inside a forks root, in any mix:
//! - a bare `<name>.md` file with YAML frontmatter
//! - a `<name>/FORK.md` folder
//!
//! Other subfolders are organizational and are descended into (capped
//! nesting). The fork's name is the file stem or the folder name; when two
//! roots define the same name, the first-discovered wins (roots are scanned
//! nearest-project-first, so project forks override user-level ones).

use crate::frontmatter::{parse_fork_file, ForkParse, ParsedFork};
use std::path::{Path, PathBuf};

/// Cap on organizational-subfolder nesting below a forks root.
const MAX_FORK_NESTING_DEPTH: usize = 8;

/// A discovered fork definition.
#[derive(Debug, Clone)]
pub struct ForkEntry {
    pub name: String,
    /// Absolute path of the definition file (`…/<name>.md` or `…/FORK.md`).
    pub path: PathBuf,
    /// The forks root this entry came from (`…/.autofork/forks`).
    pub root: PathBuf,
    pub parsed: ParsedFork,
}

/// The forks roots relevant to `dir`: each ancestor's `.autofork/forks` and
/// `.claude/forks` (including `dir` itself), nearest first, then
/// `user_forks_root` (the user-level forks directory, e.g.
/// `~/.autofork/forks`) and `claude_dir`'s `forks/` (e.g. `~/.claude/forks`)
/// if not already among them.
pub fn fork_roots(
    dir: &Path,
    user_forks_root: Option<&Path>,
    claude_dir: Option<&Path>,
) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    let start = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
    let mut cur = Some(start.as_path());
    while let Some(d) = cur {
        for candidate in [
            d.join(".autofork").join("forks"),
            d.join(".claude").join("forks"),
        ] {
            if candidate.is_dir() {
                roots.push(candidate);
            }
        }
        cur = d.parent();
    }
    let mut push_user = |candidate: PathBuf| {
        let c = candidate
            .canonicalize()
            .unwrap_or_else(|_| candidate.clone());
        if c.is_dir() && !roots.contains(&c) {
            roots.push(c);
        }
    };
    if let Some(user) = user_forks_root {
        push_user(user.to_path_buf());
    }
    if let Some(claude) = claude_dir {
        push_user(claude.join("forks"));
    }
    roots
}

/// The skills roots relevant to `dir` (scanned only for skill-attached
/// `FORK.md` files): each ancestor's `.claude/skills`, nearest first, then
/// `claude_dir`'s `skills/`.
fn skill_roots(dir: &Path, claude_dir: Option<&Path>) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    let start = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
    let mut cur = Some(start.as_path());
    while let Some(d) = cur {
        let candidate = d.join(".claude").join("skills");
        if candidate.is_dir() {
            roots.push(candidate);
        }
        cur = d.parent();
    }
    if let Some(claude) = claude_dir {
        let candidate = claude.join("skills");
        let c = candidate
            .canonicalize()
            .unwrap_or_else(|_| candidate.clone());
        if c.is_dir() && !roots.contains(&c) {
            roots.push(c);
        }
    }
    roots
}

/// For a `FORK.md` definition: the sibling `SKILL.md`, when this fork is
/// attached to a skill folder. Any `FORK.md` with a `SKILL.md` next to it
/// counts, wherever the folder lives.
pub fn skill_sibling(fork_path: &Path) -> Option<PathBuf> {
    if fork_path.file_name().and_then(|n| n.to_str()) != Some("FORK.md") {
        return None;
    }
    let skill = fork_path.parent()?.join("SKILL.md");
    skill.is_file().then_some(skill)
}

/// Discover all forks visible from `dir` (project roots nearest-first, then
/// user-level; forks dirs before skill folders at each level group).
/// Returns entries in discovery order plus collision warnings.
pub fn discover_forks(
    dir: &Path,
    user_forks_root: Option<&Path>,
    claude_dir: Option<&Path>,
) -> (Vec<ForkEntry>, Vec<String>) {
    let mut entries: Vec<ForkEntry> = Vec::new();
    let mut warnings = Vec::new();
    for root in fork_roots(dir, user_forks_root, claude_dir) {
        scan_forks_dir(&root, &root, 0, &mut entries, &mut warnings);
    }
    for root in skill_roots(dir, claude_dir) {
        scan_skills_dir(&root, &root, 0, &mut entries, &mut warnings);
    }
    (entries, warnings)
}

fn insert_entry(
    entries: &mut Vec<ForkEntry>,
    warnings: &mut Vec<String>,
    name: String,
    path: PathBuf,
    root: &Path,
) {
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) => {
            warnings.push(format!("failed to read {}: {e}", path.display()));
            return;
        }
    };
    let parsed = match parse_fork_file(&name, &content) {
        ForkParse::Fork(p) => p,
        // A companion note that only looks like a fork: warn so a missing
        // `fork: true` marker can't silently disable a real fork.
        ForkParse::NotFork { fork_like: true } => {
            warnings.push(format!(
                "{} has fork-like frontmatter but no `fork: true` — not treated as a fork",
                path.display()
            ));
            return;
        }
        // A plain companion `.md` (or explicit `fork: false`): silently skip.
        ForkParse::NotFork { fork_like: false } => {
            tracing::debug!(path = %path.display(), "not a fork (no `fork: true`), skipping");
            return;
        }
        ForkParse::Invalid => {
            warnings.push(format!(
                "fork '{name}' at {} has invalid frontmatter YAML, skipped",
                path.display()
            ));
            return;
        }
    };
    // Only real forks reserve a name / shadow others.
    if let Some(existing) = entries.iter().find(|e| e.name == name) {
        if existing.path != path {
            warnings.push(format!(
                "fork '{name}' at {} shadowed by {}",
                path.display(),
                existing.path.display()
            ));
        }
        return;
    }
    entries.push(ForkEntry {
        name,
        path,
        root: root.to_path_buf(),
        parsed,
    });
}

fn scan_forks_dir(
    dir: &Path,
    root: &Path,
    depth: usize,
    entries: &mut Vec<ForkEntry>,
    warnings: &mut Vec<String>,
) {
    if depth > MAX_FORK_NESTING_DEPTH {
        return;
    }
    let Ok(read) = std::fs::read_dir(dir) else {
        return;
    };
    let mut items: Vec<_> = read.filter_map(|e| e.ok()).collect();
    items.sort_by_key(|e| e.file_name());
    for item in items {
        let path = item.path();
        let Some(file_name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if file_name.starts_with('.') {
            continue;
        }
        if path.is_dir() {
            let fork_md = path.join("FORK.md");
            if fork_md.is_file() {
                insert_entry(entries, warnings, file_name.to_string(), fork_md, root);
            } else {
                scan_forks_dir(&path, root, depth + 1, entries, warnings);
            }
        } else if let Some(stem) = file_name.strip_suffix(".md") {
            if !stem.is_empty() {
                insert_entry(entries, warnings, stem.to_string(), path.clone(), root);
            }
        }
    }
}

/// Scan a skills tree (`.claude/skills/**`) for skill-attached forks: any
/// folder holding both a `SKILL.md` and a `FORK.md` defines a fork named
/// after the folder. Folders without a `SKILL.md` are organizational and are
/// descended into; a skill folder's own subdirectories are not scanned.
fn scan_skills_dir(
    dir: &Path,
    root: &Path,
    depth: usize,
    entries: &mut Vec<ForkEntry>,
    warnings: &mut Vec<String>,
) {
    if depth > MAX_FORK_NESTING_DEPTH {
        return;
    }
    let Ok(read) = std::fs::read_dir(dir) else {
        return;
    };
    let mut items: Vec<_> = read.filter_map(|e| e.ok()).collect();
    items.sort_by_key(|e| e.file_name());
    for item in items {
        let path = item.path();
        let Some(file_name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if file_name.starts_with('.') || !path.is_dir() {
            continue;
        }
        if path.join("SKILL.md").is_file() {
            let fork_md = path.join("FORK.md");
            if fork_md.is_file() {
                insert_entry(entries, warnings, file_name.to_string(), fork_md, root);
            }
        } else {
            scan_skills_dir(&path, root, depth + 1, entries, warnings);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write(path: &Path, content: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    #[test]
    fn discovers_both_layouts_and_subfolders() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("proj");
        write(
            &root.join(".autofork/forks/journal.md"),
            "---\nfork: true\ndescription: j\n---\nbody",
        );
        write(
            &root.join(".autofork/forks/cleanup/FORK.md"),
            "---\nfork: true\nrun_on: [idle]\n---\nbody",
        );
        // A companion note (no marker, no fork-like keys) is silently ignored.
        write(
            &root.join(".autofork/forks/maint/deep/notes.md"),
            "no frontmatter reference material",
        );
        // Ignored: dotfiles, non-md files, dirs without FORK.md are recursed only.
        write(&root.join(".autofork/forks/.hidden.md"), "x");
        write(&root.join(".autofork/forks/readme.txt"), "x");

        let (entries, warnings) = discover_forks(&root, None, None);
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["cleanup", "journal"]);
        assert!(warnings.is_empty(), "warnings: {warnings:?}");
        let journal = entries.iter().find(|e| e.name == "journal").unwrap();
        assert_eq!(journal.parsed.def.description.as_deref(), Some("j"));
        // Roots are canonicalized (macOS /var vs /private/var).
        assert_eq!(
            journal.root,
            root.canonicalize().unwrap().join(".autofork/forks")
        );
    }

    #[test]
    fn companion_note_with_fork_like_keys_warns() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("p");
        // A real fork.
        write(
            &root.join(".autofork/forks/real.md"),
            "---\nfork: true\nrun_on: [idle]\n---\nbody",
        );
        // A migration mistake: fork keys but no marker → warned, not a fork.
        write(
            &root.join(".autofork/forks/oops.md"),
            "---\nrun_on: [idle]\nthrottle: 1h\n---\nbody",
        );
        // Explicit opt-out and a plain note → silent.
        write(
            &root.join(".autofork/forks/note.md"),
            "---\nfork: false\nrun_on: [idle]\n---\nb",
        );
        write(&root.join(".autofork/forks/plain.md"), "just notes");

        let (entries, warnings) = discover_forks(&root, None, None);
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["real"]);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("no `fork: true`"), "{}", warnings[0]);
    }

    #[test]
    fn upward_traversal_and_project_overrides_user() {
        let tmp = tempfile::tempdir().unwrap();
        let outer = tmp.path().join("outer");
        let inner = outer.join("inner");
        let home = tmp.path().join("home");
        write(
            &outer.join(".autofork/forks/shared.md"),
            "---\nfork: true\n---\nouter body",
        );
        write(
            &inner.join(".autofork/forks/shared.md"),
            "---\nfork: true\n---\ninner body",
        );
        write(
            &inner.join(".autofork/forks/local.md"),
            "---\nfork: true\n---\nlocal",
        );
        write(
            &home.join(".autofork/forks/shared.md"),
            "---\nfork: true\n---\nhome body",
        );
        write(
            &home.join(".autofork/forks/user.md"),
            "---\nfork: true\n---\nuser",
        );
        fs::create_dir_all(&inner).unwrap();

        let (entries, warnings) = discover_forks(&inner, Some(&home.join(".autofork/forks")), None);
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["local", "shared", "user"]);
        let shared = entries.iter().find(|e| e.name == "shared").unwrap();
        assert_eq!(shared.parsed.body, "inner body");
        // Two shadow warnings: outer and home both lost to inner.
        assert_eq!(warnings.len(), 2);
    }

    #[test]
    fn invalid_yaml_is_skipped_with_warning() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("p");
        write(&root.join(".autofork/forks/bad.md"), "---\n: [oops\n---\nb");
        write(
            &root.join(".autofork/forks/good.md"),
            "---\nfork: true\n---\nfine",
        );
        let (entries, warnings) = discover_forks(&root, None, None);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "good");
        assert_eq!(warnings.len(), 1);
    }

    #[test]
    fn no_autofork_dir_is_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let (entries, warnings) = discover_forks(tmp.path(), None, None);
        assert!(entries.is_empty());
        assert!(warnings.is_empty());
    }

    #[test]
    fn claude_forks_dirs_are_roots_too() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("p");
        write(
            &root.join(".claude/forks/journal.md"),
            "---\nfork: true\n---\nbody",
        );
        write(
            &root.join(".claude/forks/deep/cleanup/FORK.md"),
            "---\nfork: true\n---\nbody",
        );
        // The user-level claude dir contributes its forks/ too.
        let claude = tmp.path().join("userclaude");
        write(
            &claude.join("forks/global.md"),
            "---\nfork: true\n---\nbody",
        );

        let (entries, warnings) = discover_forks(&root, None, Some(&claude));
        let mut names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        names.sort_unstable();
        assert_eq!(names, vec!["cleanup", "global", "journal"]);
        assert!(warnings.is_empty(), "warnings: {warnings:?}");
    }

    #[test]
    fn skill_folders_with_fork_md_define_skill_forks() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("p");
        // A skill with an attached fork.
        write(
            &root.join(".claude/skills/feedback/SKILL.md"),
            "---\nname: feedback\ndescription: d\n---\nskill body",
        );
        write(
            &root.join(".claude/skills/feedback/FORK.md"),
            "---\nfork: true\nrun_on: [idle]\n---\nfork body",
        );
        // A skill without a FORK.md defines nothing.
        write(
            &root.join(".claude/skills/plain/SKILL.md"),
            "---\nname: plain\n---\nbody",
        );
        // Nested category folders are descended.
        write(
            &root.join(".claude/skills/org/deep/diary/SKILL.md"),
            "---\nname: diary\n---\nbody",
        );
        write(
            &root.join(".claude/skills/org/deep/diary/FORK.md"),
            "---\nfork: true\n---\nfork body",
        );
        // User-level skills contribute too.
        let claude = tmp.path().join("userclaude");
        write(
            &claude.join("skills/global-skill/SKILL.md"),
            "---\nname: global-skill\n---\nbody",
        );
        write(
            &claude.join("skills/global-skill/FORK.md"),
            "---\nfork: true\n---\nfork body",
        );

        let (entries, warnings) = discover_forks(&root, None, Some(&claude));
        let mut names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        names.sort_unstable();
        assert_eq!(names, vec!["diary", "feedback", "global-skill"]);
        assert!(warnings.is_empty(), "warnings: {warnings:?}");

        let feedback = entries.iter().find(|e| e.name == "feedback").unwrap();
        assert!(feedback.path.ends_with(".claude/skills/feedback/FORK.md"));
        let skill = skill_sibling(&feedback.path).expect("skill sibling");
        assert!(skill.ends_with(".claude/skills/feedback/SKILL.md"));
    }

    #[test]
    fn skill_sibling_requires_fork_md_and_a_skill() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("d");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("FORK.md"), "x").unwrap();
        // No SKILL.md next to it → not a skill fork.
        assert!(skill_sibling(&dir.join("FORK.md")).is_none());
        // Bare fork files are never skill forks.
        std::fs::write(dir.join("journal.md"), "x").unwrap();
        assert!(skill_sibling(&dir.join("journal.md")).is_none());
        // With SKILL.md present, the sibling resolves.
        std::fs::write(dir.join("SKILL.md"), "x").unwrap();
        assert!(skill_sibling(&dir.join("FORK.md")).is_some());
    }

    #[test]
    fn forks_dirs_shadow_skill_forks_on_name_collision() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("p");
        write(
            &root.join(".autofork/forks/feedback.md"),
            "---\nfork: true\ndescription: from forks dir\n---\nbody",
        );
        write(
            &root.join(".claude/skills/feedback/SKILL.md"),
            "---\nname: feedback\n---\nbody",
        );
        write(
            &root.join(".claude/skills/feedback/FORK.md"),
            "---\nfork: true\ndescription: from skill\n---\nbody",
        );
        let (entries, warnings) = discover_forks(&root, None, None);
        assert_eq!(entries.len(), 1);
        assert_eq!(
            entries[0].parsed.def.description.as_deref(),
            Some("from forks dir")
        );
        assert_eq!(warnings.len(), 1, "collision warns: {warnings:?}");
    }
}
