//! A worker run that "succeeds" but changes nothing must be reported as a
//! no-op (needs_attention + a NO-OP report), never as success, and must not
//! open a pull request.
#[allow(dead_code)]
mod common;

use common::Harness;

#[tokio::test]
async fn pr_task_with_no_changes_is_a_noop_not_success() {
    let h = Harness::with_providers(&[("grok", true, "success")], "127.0.0.1:0").await;
    h.install_gh_stub();
    let repo = h.repo.display().to_string();
    h.shared
        .submit_with(
            "demo-n1",
            "# Nothing to do\n\nalready fixed upstream",
            "test",
            Some(&repo),
            None,
            false,
            false,
            atr::SubmitOpts {
                workspace_kind: Some("git".into()),
                output_kind: Some("pr".into()),
                source_kind: Some("cli".into()),
                title: Some("Nothing to do".into()),
                ..Default::default()
            },
        )
        .unwrap();
    h.wait_status("demo-n1", &["needs_attention", "waiting"], 15000)
        .await;
    let t = h.shared.db.get_task("demo-n1").unwrap().unwrap();
    assert_eq!(t.status, "needs_attention");
    assert!(
        t.result_text.starts_with("NO-OP demo-n1: no file changes"),
        "{}",
        t.result_text
    );
    let runs = h.shared.db.list_runs(Some("demo-n1")).unwrap();
    assert_eq!(runs.last().unwrap().outcome.as_deref(), Some("no_op"));
    assert!(!h.fake_dir.join("gh-pr.log").exists(), "no PR for a no-op");
    assert!(t.result_pr_url.is_empty());
}

#[tokio::test]
async fn run_that_edits_the_main_checkout_is_blocked_not_delivered() {
    let h = Harness::with_providers(&[("grok", true, "edit_main")], "127.0.0.1:0").await;
    h.install_gh_stub();
    let repo = h.repo.display().to_string();
    h.shared
        .submit_with(
            "demo-m1",
            "# Fix the readme\n\nadd a line",
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
    h.wait_status("demo-m1", &["needs_attention", "waiting"], 15000)
        .await;
    let t = h.shared.db.get_task("demo-m1").unwrap().unwrap();
    assert_eq!(t.status, "needs_attention");
    assert!(
        t.result_text
            .starts_with("BLOCKED demo-m1: the agent changed the main checkout"),
        "{}",
        t.result_text
    );
    assert!(t.result_text.contains("README.md"), "{}", t.result_text);
    let runs = h.shared.db.list_runs(Some("demo-m1")).unwrap();
    assert_eq!(
        runs.last().unwrap().outcome.as_deref(),
        Some("main_checkout_edit")
    );
    assert!(
        !h.fake_dir.join("gh-pr.log").exists(),
        "an escaped run must not open a PR"
    );
}
