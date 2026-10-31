//! atr drops scratch files the agent left (root-level helpers such as
//! `test_fix.py`, `run_tests.py`; `debug*` names anywhere) before verify and
//! commit, keeps the real change, and names what it dropped in the PR body.
//! By default (`[scratch] scope = "no_shell"`) only providers without a shell
//! (OpenCode without `allow_shell`) are cleaned up; frontier agents keep their
//! new files and docs edits as in v0.1.34.
#[allow(dead_code)]
mod common;

use common::Harness;

fn git(dir: &std::path::Path, args: &[&str]) -> String {
    let o = std::process::Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .unwrap();
    String::from_utf8_lossy(&o.stdout).into_owned()
}

#[tokio::test]
async fn scratch_files_are_dropped_before_verify_and_commit() {
    let h = Harness::with_providers(&[("opencode", true, "edit_scratch")], "127.0.0.1:0").await;
    h.install_gh_stub();
    let origin = h.dir.path().join("origin.git");
    git(
        h.dir.path(),
        &["init", "-q", "--bare", origin.to_str().unwrap()],
    );
    git(
        &h.repo,
        &["remote", "add", "origin", origin.to_str().unwrap()],
    );
    std::fs::write(h.fake_dir.join("gh-allow-pr"), "").unwrap();
    {
        // Verify fails if a stray root test file is still there (pytest would
        // collect it), so a pass proves the drop happened before verify.
        let mut cfg = h.shared.config.write().unwrap();
        cfg.verify.enabled = true;
        cfg.verify.max_retries = 0;
        cfg.verify.only_listed_repos = false;
        cfg.verify.commands = vec!["test ! -e test_fix.py && test ! -e run_tests.py".into()];
    }
    let repo = h.repo.display().to_string();
    h.shared
        .submit_with(
            "demo-s1",
            "# Fix the readme\n\nadd a line to README.md and document usage",
            "test",
            Some(&repo),
            None,
            false,
            false,
            atr::SubmitOpts {
                workspace_kind: Some("git".into()),
                output_kind: Some("pr".into()),
                source_kind: Some("cli".into()),
                title: Some("Fix the readme".into()),
                ..Default::default()
            },
        )
        .unwrap();
    h.wait_status("demo-s1", &["waiting", "needs_attention"], 15000)
        .await;
    let t = h.shared.db.get_task("demo-s1").unwrap().unwrap();
    assert_eq!(t.status, "waiting", "{}", t.result_text);
    let runs = h.shared.db.list_runs(Some("demo-s1")).unwrap();
    assert_eq!(runs.last().unwrap().outcome.as_deref(), Some("success"));

    let files = git(
        &origin,
        &["ls-tree", "-r", "--name-only", "refs/heads/atr/demo-s1"],
    );
    let files: Vec<&str> = files.lines().collect();
    assert!(files.contains(&"README.md"), "{files:?}");
    assert!(files.contains(&"docs/usage.md"), "{files:?}");
    for stray in ["test_fix.py", "run_tests.py", "docs/debug_notes.md"] {
        assert!(!files.contains(&stray), "{stray} reached the PR: {files:?}");
    }

    let pr = std::fs::read_to_string(h.fake_dir.join("gh-pr.log")).unwrap();
    assert!(pr.contains("## Dropped scratch files"), "{pr}");
    for stray in ["`test_fix.py`", "`run_tests.py`", "`docs/debug_notes.md`"] {
        assert!(pr.contains(stray), "{stray} missing from PR note: {pr}");
    }
    let note_at = pr.find("## Dropped scratch files").unwrap();
    let footer_at = pr.rfind("Opened by atr").unwrap();
    assert!(note_at < footer_at, "note goes before the footer");
}

#[tokio::test]
async fn scratch_drop_can_be_turned_off() {
    let h = Harness::with_providers(&[("opencode", true, "edit_scratch")], "127.0.0.1:0").await;
    h.install_gh_stub();
    h.shared.config.write().unwrap().scratch.drop = false;
    let repo = h.repo.display().to_string();
    h.shared
        .submit_with(
            "demo-s2",
            "# Fix the readme\n\nadd a line",
            "test",
            Some(&repo),
            None,
            false,
            false,
            atr::SubmitOpts {
                workspace_kind: Some("git".into()),
                output_kind: Some("branch".into()),
                source_kind: Some("cli".into()),
                title: Some("Fix the readme".into()),
                ..Default::default()
            },
        )
        .unwrap();
    h.wait_status("demo-s2", &["waiting", "needs_attention"], 15000)
        .await;
    let t = h.shared.db.get_task("demo-s2").unwrap().unwrap();
    let wt = std::path::PathBuf::from(t.worktree.unwrap());
    assert!(wt.join("test_fix.py").exists());
    assert!(wt.join("run_tests.py").exists());
}

#[tokio::test]
async fn unrequested_docs_edits_are_reverted_and_listed() {
    let h =
        Harness::with_providers(&[("opencode", true, "edit_docs_unasked")], "127.0.0.1:0").await;
    h.install_gh_stub();
    let origin = h.dir.path().join("origin.git");
    git(
        h.dir.path(),
        &["init", "-q", "--bare", origin.to_str().unwrap()],
    );
    git(
        &h.repo,
        &["remote", "add", "origin", origin.to_str().unwrap()],
    );
    std::fs::create_dir_all(h.repo.join("textkit")).unwrap();
    std::fs::write(
        h.repo.join("textkit/slug.py"),
        "def slugify(s):\n    return s\n",
    )
    .unwrap();
    git(&h.repo, &["add", "textkit/slug.py"]);
    git(
        &h.repo,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-qm",
            "slug",
        ],
    );
    std::fs::write(h.fake_dir.join("gh-allow-pr"), "").unwrap();
    {
        // Verify fails while README.md is rewritten, so a pass proves the
        // revert ran before verify.
        let mut cfg = h.shared.config.write().unwrap();
        cfg.verify.enabled = true;
        cfg.verify.max_retries = 0;
        cfg.verify.only_listed_repos = false;
        cfg.verify.commands = vec!["! grep -q atr-local-sandbox README.md".into()];
    }
    let base_readme = std::fs::read_to_string(h.repo.join("README.md")).unwrap();
    let repo = h.repo.display().to_string();
    h.shared
        .submit_with(
            "demo-s3",
            "# slugify: strip leading and trailing dashes\n\n`slugify(\" -Hello World- \")` returns `\"-hello-world-\"`.",
            "test",
            Some(&repo),
            None,
            false,
            false,
            atr::SubmitOpts {
                workspace_kind: Some("git".into()),
                output_kind: Some("pr".into()),
                source_kind: Some("cli".into()),
                title: Some("slugify: strip leading and trailing dashes".into()),
                ..Default::default()
            },
        )
        .unwrap();
    h.wait_status("demo-s3", &["waiting", "needs_attention"], 15000)
        .await;
    let t = h.shared.db.get_task("demo-s3").unwrap().unwrap();
    assert_eq!(t.status, "waiting", "{}", t.result_text);

    let base = git(&h.repo, &["rev-parse", "HEAD"]);
    let changed = git(
        &origin,
        &["diff", "--name-only", base.trim(), "refs/heads/atr/demo-s3"],
    );
    let changed: Vec<&str> = changed.lines().collect();
    assert_eq!(changed, vec!["textkit/slug.py"], "only the fix ships");
    let readme = git(&origin, &["show", "refs/heads/atr/demo-s3:README.md"]);
    assert_eq!(readme, base_readme);

    let pr = std::fs::read_to_string(h.fake_dir.join("gh-pr.log")).unwrap();
    assert!(pr.contains("## Reverted unrequested edits"), "{pr}");
    assert!(pr.contains("`README.md`"), "{pr}");
    let note_at = pr.find("## Reverted unrequested edits").unwrap();
    let footer_at = pr.rfind("Opened by atr").unwrap();
    assert!(note_at < footer_at, "note goes before the footer");
}

#[tokio::test]
async fn docs_edits_are_kept_when_revert_is_off() {
    let h =
        Harness::with_providers(&[("opencode", true, "edit_docs_unasked")], "127.0.0.1:0").await;
    h.install_gh_stub();
    std::fs::create_dir_all(h.repo.join("textkit")).unwrap();
    std::fs::write(h.repo.join("textkit/slug.py"), "x\n").unwrap();
    git(&h.repo, &["add", "textkit/slug.py"]);
    git(
        &h.repo,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-qm",
            "slug",
        ],
    );
    h.shared.config.write().unwrap().scratch.revert_docs = false;
    let repo = h.repo.display().to_string();
    h.shared
        .submit_with(
            "demo-s4",
            "# slugify: strip dashes",
            "test",
            Some(&repo),
            None,
            false,
            false,
            atr::SubmitOpts {
                workspace_kind: Some("git".into()),
                output_kind: Some("branch".into()),
                source_kind: Some("cli".into()),
                title: Some("slugify: strip dashes".into()),
                ..Default::default()
            },
        )
        .unwrap();
    h.wait_status("demo-s4", &["waiting", "needs_attention"], 15000)
        .await;
    let t = h.shared.db.get_task("demo-s4").unwrap().unwrap();
    let wt = std::path::PathBuf::from(t.worktree.unwrap());
    let readme = std::fs::read_to_string(wt.join("README.md")).unwrap_or_default();
    let log = git(&wt, &["log", "-p", "-3"]);
    assert!(
        readme.contains("atr-local-sandbox") || log.contains("atr-local-sandbox"),
        "README edit kept: {readme}"
    );
}

#[tokio::test]
async fn shell_providers_keep_new_files_and_docs_by_default() {
    // PR70 regression (sandbox PR #43): claude's new root `pyproject.toml`
    // was dropped and its README line reverted. A provider with a shell keeps
    // both unless `[scratch] scope = "all"`.
    let h = Harness::with_providers(&[("grok", true, "edit_scratch")], "127.0.0.1:0").await;
    h.install_gh_stub();
    let repo = h.repo.display().to_string();
    h.shared
        .submit_with(
            "demo-s5",
            "# Package it\n\nmake it installable",
            "test",
            Some(&repo),
            None,
            false,
            false,
            atr::SubmitOpts {
                workspace_kind: Some("git".into()),
                output_kind: Some("branch".into()),
                source_kind: Some("cli".into()),
                title: Some("Package it".into()),
                ..Default::default()
            },
        )
        .unwrap();
    h.wait_status("demo-s5", &["waiting", "needs_attention"], 15000)
        .await;
    let t = h.shared.db.get_task("demo-s5").unwrap().unwrap();
    assert_eq!(t.status, "waiting", "{}", t.result_text);
    let wt = std::path::PathBuf::from(t.worktree.unwrap());
    let tracked = git(&wt, &["ls-files"]);
    let status = git(&wt, &["status", "--porcelain", "--untracked-files=all"]);
    let log = git(&wt, &["log", "-p", "-3"]);
    for f in ["test_fix.py", "run_tests.py", "docs/debug_notes.md"] {
        assert!(
            wt.join(f).exists() || tracked.contains(f),
            "{f} was dropped for a shell provider: {status}"
        );
    }
    let readme = std::fs::read_to_string(wt.join("README.md")).unwrap_or_default();
    assert!(
        readme.contains("scratch-test fix") || log.contains("scratch-test fix"),
        "README edit reverted for a shell provider: {readme}"
    );
}
