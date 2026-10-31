//! Drop scratch files an agent left in its worktree before atr verifies or
//! commits.
//!
//! Local models routinely write throwaway helpers (`test_fix.py`,
//! `run_tests.py`, `verify_fix.md`, `debug.py`, …) next to the real fix, even
//! when told not to. atr commits the worktree itself, so without this step
//! those files land in the PR, and a stray `test_*.py` at the repo root can be
//! collected by the verify command and fail the run.
//!
//! Rule: a **new untracked** file (not ignored by git) is scratch when the
//! task never names it (by path or file name) and either
//! - it sits at the repository root, or
//! - its file name matches a scratch pattern ([`SCRATCH_NAMES`]).
//!
//! Tracked files are never touched, and neither are new files in
//! subdirectories with ordinary names (a new `textkit/slug.py` or
//! `tests/test_slug.py` is kept). Every dropped path is logged and listed in
//! `.atr/dropped-scratch.txt` (gitignored) so the PR body can name them.
//!
//! Edits to **tracked** files get a second, narrower rule
//! ([`revert_unrequested`]): a modified or deleted tracked *docs* file
//! (`README*`, `CHANGELOG*`, `*.md`, `*.rst`, anything under `docs/`, …) is
//! restored from `HEAD` when the task never names it and never talks about
//! docs at all, and the run changed something else too. Code files are never
//! reverted, so a legitimate fix cannot be broken. Seen in sandbox PR #32: an
//! issue about `slugify` came back with `README.md` rewritten. Reverted paths
//! go to `.atr/reverted-tracked.txt` and the PR body.

use std::path::Path;

/// Where atr records what it dropped for the current task (inside `.atr/`,
/// which git ignores). Reset when a pool slot is reused.
pub const REL_PATH: &str = ".atr/dropped-scratch.txt";

/// File-name globs (matched against the base name, case-insensitive) that are
/// scratch wherever they appear, unless the task names them.
pub const SCRATCH_NAMES: &[&str] = &[
    "debug*",
    "scratch*",
    "tmp*",
    "simple_test*",
    "run_test*",
    "verify_*",
    "validate_*",
    "final_*",
    "*_verification*",
    "*.orig",
    "*.rej",
    "*.bak",
    "*.swp",
];

/// Message sources that are atr's own follow-ups, not the task. Their text
/// (verify output, retry notes) can quote scratch file names, which must not
/// count as the task asking for them.
const NON_TASK_SOURCES: &[&str] = &["verify", "retry"];

/// The task text used to decide whether a new file was asked for: the title
/// plus every message that is not one of atr's own follow-ups.
pub fn task_text(title: &str, messages: &[crate::db::Message]) -> String {
    let mut out = String::from(title);
    for m in messages {
        if NON_TASK_SOURCES.contains(&m.source.as_str()) {
            continue;
        }
        out.push('\n');
        out.push_str(&m.body);
    }
    out
}

/// Task `key`'s text: its title, its non-atr messages, and the names of
/// files attached to it.
pub fn text_for_task(
    cfg: &crate::config::Config,
    db: &crate::db::Db,
    key: &str,
    title: &str,
) -> String {
    let msgs = db.all_messages(key).unwrap_or_default();
    let mut text = task_text(title, &msgs);
    for name in crate::intake::list_attachment_names(&cfg.state_db, key) {
        text.push('\n');
        text.push_str(&name);
    }
    text
}

/// What one cleanup pass removed or restored.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Cleanup {
    /// New scratch files deleted.
    pub dropped: Vec<String>,
    /// Tracked docs files restored from `HEAD`.
    pub reverted: Vec<String>,
}

impl Cleanup {
    pub fn is_empty(&self) -> bool {
        self.dropped.is_empty() && self.reverted.is_empty()
    }
}

/// Run the enabled cleanup rules (`[scratch] drop` and `revert_docs`) for
/// task `key` in `wt`.
pub fn clean_for_task(
    cfg: &crate::config::Config,
    db: &crate::db::Db,
    key: &str,
    title: &str,
    wt: &Path,
) -> Cleanup {
    if !cfg.scratch.drop && !cfg.scratch.revert_docs {
        return Cleanup::default();
    }
    let text = text_for_task(cfg, db, key, title);
    Cleanup {
        dropped: if cfg.scratch.drop {
            drop_scratch(wt, &text)
        } else {
            Vec::new()
        },
        reverted: if cfg.scratch.revert_docs {
            revert_unrequested(wt, &text)
        } else {
            Vec::new()
        },
    }
}

/// True when `path` (repo-relative, `/`-separated) is scratch for a task
/// whose text is `task`.
pub fn is_scratch(path: &str, task: &str) -> bool {
    let path = path.trim_start_matches("./");
    if path.is_empty() || path.starts_with(".atr/") {
        return false;
    }
    if mentions(task, path) {
        return false;
    }
    let name = path.rsplit('/').next().unwrap_or(path);
    if mentions(task, name) {
        return false;
    }
    let top_level = !path.contains('/');
    let lower = name.to_ascii_lowercase();
    top_level
        || SCRATCH_NAMES
            .iter()
            .any(|p| crate::verify::glob_match(p, &lower))
}

/// `needle` appears in `text` as a whole token (not inside a longer name).
fn mentions(text: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return false;
    }
    let is_name_char = |c: char| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '/');
    let mut from = 0;
    while let Some(i) = text[from..].find(needle) {
        let start = from + i;
        let end = start + needle.len();
        let before = text[..start].chars().next_back();
        let after = text[end..].chars().next();
        // A trailing '.' is sentence punctuation, not part of the name.
        let after_ok = match after {
            None => true,
            Some('.') => text[end + 1..]
                .chars()
                .next()
                .is_none_or(|c| !is_name_char(c)),
            Some(c) => !is_name_char(c),
        };
        // A leading '/' is fine: "src/foo.py" mentions "foo.py".
        let before_ok = before.is_none_or(|c| c == '/' || !is_name_char(c));
        if before_ok && after_ok {
            return true;
        }
        from = start + needle.len().max(1);
    }
    false
}

/// New untracked, non-ignored files in the worktree (repo-relative).
pub fn untracked_files(wt: &Path) -> Vec<String> {
    let out = crate::util::git_capture()
        .args(["ls-files", "--others", "--exclude-standard", "-z"])
        .current_dir(wt)
        .output();
    match out {
        Ok(o) if o.status.success() => o
            .stdout
            .split(|b| *b == 0)
            .filter(|s| !s.is_empty())
            .map(|s| String::from_utf8_lossy(s).into_owned())
            .collect(),
        _ => Vec::new(),
    }
}

/// Delete scratch files from the worktree. Returns the dropped paths (sorted).
/// Empty directories left behind are removed too. Appends the list to
/// [`REL_PATH`] so delivery can report it.
pub fn drop_scratch(wt: &Path, task: &str) -> Vec<String> {
    let mut dropped: Vec<String> = untracked_files(wt)
        .into_iter()
        .filter(|p| is_scratch(p, task))
        .collect();
    dropped.sort();
    dropped.retain(|rel| {
        let abs = wt.join(rel);
        match std::fs::remove_file(&abs) {
            Ok(()) => {
                let mut dir = abs.parent();
                while let Some(d) = dir {
                    if d == wt || std::fs::remove_dir(d).is_err() {
                        break;
                    }
                    dir = d.parent();
                }
                true
            }
            Err(e) => {
                tracing::warn!("drop scratch {rel}: {e}");
                false
            }
        }
    });
    append_list(wt, REL_PATH, &dropped);
    dropped
}

/// Every path atr dropped for the current task so far.
pub fn recorded(wt: &Path) -> Vec<String> {
    read_list(wt, REL_PATH)
}

/// Every tracked path atr restored for the current task so far.
pub fn recorded_reverts(wt: &Path) -> Vec<String> {
    read_list(wt, REVERTED_REL_PATH)
}

/// Where atr records the tracked files it restored for the current task.
pub const REVERTED_REL_PATH: &str = ".atr/reverted-tracked.txt";

/// Base names (case-insensitive, any extension) that are docs.
const DOC_STEMS: &[&str] = &[
    "readme",
    "changelog",
    "changes",
    "history",
    "contributing",
    "license",
    "licence",
    "notice",
    "authors",
    "code_of_conduct",
];

/// Extensions that are docs wherever they appear.
const DOC_EXTS: &[&str] = &["md", "markdown", "rst", "adoc", "asciidoc"];

/// Words that mean the task is (also) about docs; any docs edit is then kept.
const DOC_WORDS: &[&str] = &[
    "readme",
    "docs",
    "doc",
    "documentation",
    "document",
    "documented",
    "changelog",
    "markdown",
    "license",
];

/// True for a docs path: a README/CHANGELOG/LICENSE-style name, a docs
/// extension, or anything under a top-level `docs/` or `doc/` folder.
pub fn is_doc_path(path: &str) -> bool {
    let path = path.trim_start_matches("./");
    let lower = path.to_ascii_lowercase();
    if lower.starts_with("docs/") || lower.starts_with("doc/") {
        return true;
    }
    let name = lower.rsplit('/').next().unwrap_or(&lower);
    let (stem, ext) = match name.split_once('.') {
        Some((s, e)) => (s, e.rsplit('.').next().unwrap_or(e)),
        None => (name, ""),
    };
    DOC_STEMS.contains(&stem) || DOC_EXTS.contains(&ext)
}

/// True when the task talks about docs in general (README, docs,
/// documentation, changelog, …), as a whole word.
pub fn task_wants_docs(task: &str) -> bool {
    task.to_ascii_lowercase()
        .split(|c: char| !c.is_ascii_alphanumeric())
        .any(|w| DOC_WORDS.contains(&w))
}

/// True when the tracked edit to `path` should be restored for this task.
pub fn is_unrequested_tracked(path: &str, task: &str) -> bool {
    let path = path.trim_start_matches("./");
    if path.is_empty() || path.starts_with(".atr/") || !is_doc_path(path) {
        return false;
    }
    let name = path.rsplit('/').next().unwrap_or(path);
    !(mentions(task, path) || mentions(task, name) || task_wants_docs(task))
}

/// Tracked files that differ from `HEAD` (modified, deleted, type-changed),
/// as `(status, path)`. Renames are reported as delete + add.
pub fn changed_tracked(wt: &Path) -> Vec<(char, String)> {
    let out = crate::util::git_capture()
        .args(["diff", "--name-status", "--no-renames", "-z", "HEAD", "--"])
        .current_dir(wt)
        .output();
    let Ok(o) = out else { return Vec::new() };
    if !o.status.success() {
        return Vec::new();
    }
    let parts: Vec<String> = o
        .stdout
        .split(|b| *b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| String::from_utf8_lossy(s).into_owned())
        .collect();
    parts
        .chunks(2)
        .filter_map(|c| match c {
            [st, path] => st.chars().next().map(|ch| (ch, path.clone())),
            _ => None,
        })
        .collect()
}

/// Restore tracked docs files the task never asked about (see the module
/// docs). Only modified/deleted files are restored, and only when the run
/// changed at least one other path, so a docs-only task is never emptied.
/// Returns the restored paths (sorted) and appends them to
/// [`REVERTED_REL_PATH`].
pub fn revert_unrequested(wt: &Path, task: &str) -> Vec<String> {
    let changed = changed_tracked(wt);
    let mut revert: Vec<String> = changed
        .iter()
        .filter(|(st, p)| matches!(st, 'M' | 'D' | 'T') && is_unrequested_tracked(p, task))
        .map(|(_, p)| p.clone())
        .collect();
    if revert.is_empty() {
        return revert;
    }
    let other_tracked = changed.iter().any(|(_, p)| !revert.contains(p));
    let other_new = untracked_files(wt)
        .iter()
        .any(|p| !p.starts_with(".atr/") && !is_scratch(p, task));
    if !other_tracked && !other_new {
        return Vec::new();
    }
    revert.sort();
    revert.retain(|rel| {
        let ok = crate::util::git_capture()
            .args(["checkout", "HEAD", "--", rel])
            .current_dir(wt)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if !ok {
            tracing::warn!("revert tracked {rel}: git checkout failed");
        }
        ok
    });
    append_list(wt, REVERTED_REL_PATH, &revert);
    revert
}

fn append_list(wt: &Path, rel: &str, add: &[String]) {
    if add.is_empty() {
        return;
    }
    let mut all = read_list(wt, rel);
    for p in add {
        if !all.contains(p) {
            all.push(p.clone());
        }
    }
    let file = wt.join(rel);
    if let Some(parent) = file.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&file, all.join("\n") + "\n");
}

fn read_list(wt: &Path, rel: &str) -> Vec<String> {
    std::fs::read_to_string(wt.join(rel))
        .map(|s| {
            s.lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .map(String::from)
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    const TASK: &str = "parse_duration: allow spaces between parts\n\n\
`parse_duration(\"1h 30m\")` raises `ValueError`. Fix it in textkit/duration.py.";

    #[test]
    fn root_level_new_files_are_scratch_unless_named() {
        // Every stray root file seen in the sandbox PRs (#20–#29).
        for p in [
            "test_duration.py",
            "test_duration_fix.py",
            "run_tests.py",
            "run_test.sh",
            "test_comprehensive.py",
            "test_simple.py",
            "verify_fix.md",
            "verify_fix.py",
            "simple_test.py",
            "test_slugify.py",
            "pytest.ini",
            "test_capitalize.py",
            "verify_implementation.py",
            "debug.py",
            "final_verification.py",
            "validate_import.py",
        ] {
            assert!(is_scratch(p, TASK), "{p} should be scratch");
        }
        assert!(!is_scratch(
            "CHANGELOG.md",
            "Add a CHANGELOG.md with the 0.1.0 entry"
        ));
        assert!(!is_scratch(
            "Makefile",
            "Add a `Makefile` with a test target."
        ));
    }

    #[test]
    fn subdirectory_files_need_a_scratch_name() {
        assert!(!is_scratch("textkit/case.py", TASK));
        assert!(!is_scratch("tests/test_duration.py", TASK));
        assert!(is_scratch("tests/debug_spaces.py", TASK));
        assert!(is_scratch("textkit/duration.py.orig", TASK));
        assert!(is_scratch("scripts/run_tests.py", TASK));
        assert!(is_scratch("tests/verify_fix.py", TASK));
    }

    #[test]
    fn named_files_are_kept() {
        let t = "Add scripts/verify_release.sh and a top-level run_tests.py runner.";
        assert!(!is_scratch("scripts/verify_release.sh", t));
        assert!(!is_scratch("run_tests.py", t));
        // A longer name that merely contains the file name does not count.
        assert!(is_scratch("test.py", "update my_test.py"));
        // Trailing sentence punctuation still counts as a mention.
        assert!(!is_scratch("NOTES.md", "Please write NOTES.md."));
    }

    #[test]
    fn atr_followups_do_not_count_as_asking() {
        let msgs = vec![
            crate::db::Message {
                id: 1,
                body: TASK.into(),
                source: "cli".into(),
            },
            crate::db::Message {
                id: 2,
                body: "verify failed: ERROR collecting test_simple.py".into(),
                source: "verify".into(),
            },
            crate::db::Message {
                id: 3,
                body: "also add NOTES.md".into(),
                source: "cli".into(),
            },
        ];
        let t = task_text("t", &msgs);
        assert!(is_scratch("test_simple.py", &t));
        assert!(!is_scratch("NOTES.md", &t));
    }

    fn git(dir: &Path, args: &[&str]) {
        let ok = Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .output()
            .unwrap()
            .status
            .success();
        assert!(ok, "git {args:?}");
    }

    #[test]
    fn drop_scratch_removes_only_scratch_and_records_it() {
        let dir = tempfile::tempdir().unwrap();
        let wt = dir.path();
        git(wt, &["init", "-q"]);
        std::fs::write(wt.join(".gitignore"), ".atr/\n").unwrap();
        std::fs::create_dir_all(wt.join("textkit")).unwrap();
        std::fs::write(wt.join("textkit/duration.py"), "x = 1\n").unwrap();
        std::fs::write(wt.join("README.md"), "r\n").unwrap();
        git(wt, &["add", "-A"]);
        git(wt, &["commit", "-q", "-m", "init"]);

        // The fix, a legit new module, and the usual litter.
        std::fs::write(wt.join("textkit/duration.py"), "x = 2\n").unwrap();
        std::fs::write(wt.join("README.md"), "r2\n").unwrap();
        std::fs::write(wt.join("textkit/units.py"), "u\n").unwrap();
        std::fs::write(wt.join("test_simple.py"), "").unwrap();
        std::fs::write(wt.join("run_tests.py"), "").unwrap();
        std::fs::create_dir_all(wt.join("scratch")).unwrap();
        std::fs::write(wt.join("scratch/debug.py"), "").unwrap();
        std::fs::create_dir_all(wt.join(".atr")).unwrap();
        std::fs::write(wt.join(".atr/checklist.md"), "- [x]\n").unwrap();

        let dropped = drop_scratch(wt, TASK);
        assert_eq!(
            dropped,
            vec!["run_tests.py", "scratch/debug.py", "test_simple.py"]
        );
        assert!(!wt.join("test_simple.py").exists());
        assert!(!wt.join("scratch").exists(), "empty dir removed");
        assert!(wt.join("textkit/units.py").exists());
        assert!(wt.join(".atr/checklist.md").exists());
        assert_eq!(
            std::fs::read_to_string(wt.join("README.md")).unwrap(),
            "r2\n"
        );
        assert_eq!(recorded(wt), dropped);

        // A later run (verify retry) adds to the record without duplicates.
        std::fs::write(wt.join("test_final.py"), "").unwrap();
        std::fs::write(wt.join("run_tests.py"), "").unwrap();
        assert_eq!(
            drop_scratch(wt, TASK),
            vec!["run_tests.py", "test_final.py"]
        );
        assert_eq!(
            recorded(wt),
            vec![
                "run_tests.py",
                "scratch/debug.py",
                "test_simple.py",
                "test_final.py"
            ]
        );
        assert!(drop_scratch(wt, TASK).is_empty());
    }

    #[test]
    fn docs_paths_and_doc_tasks() {
        for p in [
            "README.md",
            "readme",
            "docs/usage.txt",
            "CHANGELOG",
            "textkit/NOTES.md",
            "LICENSE.txt",
            "doc/index.rst",
        ] {
            assert!(is_doc_path(p), "{p} is docs");
        }
        for p in [
            "textkit/slug.py",
            "tests/test_textkit.py",
            "textkit/__init__.py",
            "requirements.txt",
            "pyproject.toml",
            "docsgen.py",
        ] {
            assert!(!is_doc_path(p), "{p} is not docs");
        }
        // Every issue in the PR #30–#37 round: none asks for docs.
        for t in [
            "slugify: strip leading and trailing dashes\n`slugify(\" -Hello World- \")` returns `\"-hello-world-\"`.",
            "Add capitalize_words(text) helper\nExport it from `textkit/__init__.py` (and `__all__`).",
            "textkit has no __version__\nPlease add `__version__ = \"0.1.0\"` to `textkit/__init__.py`.",
        ] {
            assert!(is_unrequested_tracked("README.md", t), "{t}");
            assert!(!is_unrequested_tracked("textkit/slug.py", t), "{t}");
            assert!(!is_unrequested_tracked("tests/test_textkit.py", t), "{t}");
        }
        assert!(!is_unrequested_tracked(
            "README.md",
            "Update the README example"
        ));
        assert!(!is_unrequested_tracked(
            "docs/api.md",
            "Fix the docs for slugify"
        ));
        assert!(!is_unrequested_tracked(
            "CHANGELOG.md",
            "add a line to CHANGELOG.md."
        ));
        assert!(!is_unrequested_tracked(".atr/notes.md", "x"));
    }

    #[test]
    fn revert_unrequested_restores_docs_only_alongside_a_fix() {
        let dir = tempfile::tempdir().unwrap();
        let wt = dir.path();
        git(wt, &["init", "-q"]);
        std::fs::write(wt.join(".gitignore"), ".atr/\n").unwrap();
        std::fs::create_dir_all(wt.join("textkit")).unwrap();
        std::fs::create_dir_all(wt.join("docs")).unwrap();
        std::fs::write(wt.join("textkit/slug.py"), "a\n").unwrap();
        std::fs::write(wt.join("README.md"), "# textkit\n").unwrap();
        std::fs::write(wt.join("docs/guide.md"), "g\n").unwrap();
        git(wt, &["add", "-A"]);
        git(wt, &["commit", "-q", "-m", "init"]);
        let task = "slugify: strip leading and trailing dashes";

        // Docs-only change: kept (the task may be a docs task worded oddly).
        std::fs::write(wt.join("README.md"), "# atr-local-sandbox\n").unwrap();
        assert!(revert_unrequested(wt, task).is_empty());
        assert!(recorded_reverts(wt).is_empty());

        // PR #32's shape: the fix plus a rewritten README and a deleted doc.
        std::fs::write(wt.join("textkit/slug.py"), "b\n").unwrap();
        std::fs::remove_file(wt.join("docs/guide.md")).unwrap();
        assert_eq!(
            revert_unrequested(wt, task),
            vec!["README.md", "docs/guide.md"]
        );
        assert_eq!(
            std::fs::read_to_string(wt.join("README.md")).unwrap(),
            "# textkit\n"
        );
        assert!(wt.join("docs/guide.md").exists());
        assert_eq!(
            std::fs::read_to_string(wt.join("textkit/slug.py")).unwrap(),
            "b\n",
            "the fix is never touched"
        );
        assert_eq!(recorded_reverts(wt), vec!["README.md", "docs/guide.md"]);

        // A task that names the README keeps the edit.
        std::fs::write(wt.join("README.md"), "# new\n").unwrap();
        assert!(revert_unrequested(wt, "also refresh README.md").is_empty());
        // A new legit file (not scratch) counts as "something else changed".
        git(wt, &["checkout", "--", "textkit/slug.py"]);
        std::fs::write(wt.join("textkit/units.py"), "u\n").unwrap();
        assert_eq!(revert_unrequested(wt, task), vec!["README.md"]);
        assert_eq!(recorded_reverts(wt), vec!["README.md", "docs/guide.md"]);
    }
}
