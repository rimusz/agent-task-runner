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

/// Drop scratch for task `key`: the task text is its title, its non-atr
/// messages, and the names of files attached to it.
pub fn drop_for_task(
    cfg: &crate::config::Config,
    db: &crate::db::Db,
    key: &str,
    title: &str,
    wt: &Path,
) -> Vec<String> {
    let msgs = db.all_messages(key).unwrap_or_default();
    let mut text = task_text(title, &msgs);
    for name in crate::intake::list_attachment_names(&cfg.state_db, key) {
        text.push('\n');
        text.push_str(&name);
    }
    drop_scratch(wt, &text)
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
    if !dropped.is_empty() {
        let mut all = recorded(wt);
        for p in &dropped {
            if !all.contains(p) {
                all.push(p.clone());
            }
        }
        let file = wt.join(REL_PATH);
        if let Some(parent) = file.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(&file, all.join("\n") + "\n");
    }
    dropped
}

/// Every path atr dropped for the current task so far.
pub fn recorded(wt: &Path) -> Vec<String> {
    std::fs::read_to_string(wt.join(REL_PATH))
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
}
