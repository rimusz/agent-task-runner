//! A planner run must not leak its session into the worker: Grok's stays in
//! plan mode, so a resumed worker fails every edit with "file edits are not
//! allowed in plan mode".
#[allow(dead_code)]
mod common;

use common::Harness;
use std::fs;

const ONE: &str = r#"{"subtasks":[{"title":"Only step","prompt":"PLANNER-PROMPT do it"}]}"#;
const TWO: &str = r#"{"subtasks":[{"title":"A","prompt":"do a"},{"title":"B","prompt":"do b"}]}"#;

/// Long enough for the default `[planner] mode = "auto"`.
fn big_body() -> String {
    format!("ORIGINAL-ISSUE {}", "x".repeat(4000))
}

fn flag<'a>(args: &'a [serde_json::Value], name: &str) -> Option<&'a str> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .and_then(|v| v.as_str())
}

fn args_of(inv: &serde_json::Value) -> Vec<serde_json::Value> {
    inv["args"].as_array().unwrap().clone()
}

async fn wait_two_runs(h: &Harness, key: &str) {
    h.wait_until(key, 20000, |h| {
        h.shared
            .db
            .list_runs(Some(key))
            .unwrap()
            .iter()
            .filter(|r| r.outcome.is_some())
            .count()
            >= 2
    })
    .await;
}

fn submit_check_only(h: &Harness, key: &str, body: &str) {
    h.shared
        .submit_with(
            key,
            body,
            "test",
            Some(&h.repo.display().to_string()),
            None,
            false,
            false,
            atr::SubmitOpts {
                workspace_kind: Some("git".into()),
                output_kind: Some("pr".into()),
                source_kind: Some("cli".into()),
                check_only: true,
                ..Default::default()
            },
        )
        .unwrap();
}

#[tokio::test]
async fn grok_worker_after_plan_starts_a_fresh_session_out_of_plan_mode() {
    let h = Harness::with_providers(&[("grok", true, "success")], "127.0.0.1:0").await;
    fs::write(h.fake_dir.join("plan.json"), ONE).unwrap();
    h.submit("demo-p1", &big_body());
    wait_two_runs(&h, "demo-p1").await;

    // Agent runs only: atr also calls grok to read back a run's usage.
    let inv: Vec<_> = h
        .invocations()
        .iter()
        .map(args_of)
        .filter(|a| flag(a, "--session-id").is_some())
        .collect();
    assert_eq!(inv.len(), 2, "{inv:?}");
    let (plan, work) = (&inv[0], &inv[1]);
    assert_eq!(flag(plan, "--permission-mode"), Some("plan"), "{plan:?}");
    assert_ne!(flag(work, "--permission-mode"), Some("plan"), "{work:?}");
    assert_eq!(flag(work, "--resume"), None, "{work:?}");
    assert!(flag(work, "--json-schema").is_none(), "{work:?}");
    assert_ne!(
        flag(plan, "--session-id"),
        flag(work, "--session-id"),
        "worker reused the planner session"
    );
}

#[tokio::test]
async fn claude_worker_after_plan_has_no_resume_and_keeps_the_task_text() {
    let h = Harness::with_providers(&[("claude", true, "success")], "127.0.0.1:0").await;
    fs::write(h.fake_dir.join("plan.json"), ONE).unwrap();
    fs::write(h.fake_dir.join("claude.capture"), "").unwrap();
    h.submit("demo-p2", &big_body());
    wait_two_runs(&h, "demo-p2").await;

    let inv = h.invocations();
    assert_eq!(inv.len(), 2, "{inv:?}");
    let work = args_of(&inv[1]);
    assert_eq!(flag(&work, "--resume"), None, "{work:?}");
    // A fresh session has no memory: the prompt must carry the whole task.
    let stdin = fs::read_to_string(h.fake_dir.join("claude.stdin")).unwrap();
    let worker = stdin.split("=====ATR-FAKE-END=====").nth(1).unwrap();
    assert!(worker.contains("ORIGINAL-ISSUE"), "{worker}");
    assert!(worker.contains("PLANNER-PROMPT"), "{worker}");
}

#[tokio::test]
async fn unusable_plan_runs_the_original_message_on_a_fresh_session() {
    // The fake CLI answers "done" without plan.json: not a subtask list.
    let h = Harness::with_providers(&[("claude", true, "success")], "127.0.0.1:0").await;
    fs::write(h.fake_dir.join("claude.capture"), "").unwrap();
    h.submit("demo-p3", &big_body());
    wait_two_runs(&h, "demo-p3").await;

    let inv = h.invocations();
    assert_eq!(flag(&args_of(&inv[1]), "--resume"), None, "{inv:?}");
    let stdin = fs::read_to_string(h.fake_dir.join("claude.stdin")).unwrap();
    let worker = stdin.split("=====ATR-FAKE-END=====").nth(1).unwrap();
    assert!(worker.contains("ORIGINAL-ISSUE"), "{worker}");
}

#[tokio::test]
async fn split_plan_leaves_no_session_on_the_parent() {
    let h = Harness::with_providers(&[("grok", true, "success")], "127.0.0.1:0").await;
    fs::write(h.fake_dir.join("plan.json"), TWO).unwrap();
    h.submit("demo-p4", &big_body());
    h.wait_until("demo-p4", 20000, |h| {
        h.shared.db.get_task("demo-p4-s2").unwrap().is_some()
    })
    .await;
    assert_eq!(h.shared.db.task_session("demo-p4", "grok").unwrap(), None);
    let parent = h.shared.db.get_task("demo-p4").unwrap().unwrap();
    assert_eq!(parent.role, "parent");
    assert_eq!(parent.session_id, None);
}

#[tokio::test]
async fn retry_of_a_failed_plan_run_starts_fresh() {
    let h = Harness::with_providers(&[("grok", true, "success")], "127.0.0.1:0").await;
    // Hold the planner so the task is still role=plan with a session.
    fs::write(h.fake_dir.join("grok.behavior"), "hold").unwrap();
    h.submit("demo-p5", &big_body());
    h.wait_until("demo-p5", 20000, |h| !h.invocations().is_empty())
        .await;
    assert!(h
        .shared
        .db
        .task_session("demo-p5", "grok")
        .unwrap()
        .is_some());
    h.shared.cancel("demo-p5", true).unwrap();
    h.wait_status("demo-p5", &["cancelled", "failed"], 20000)
        .await;
    assert_eq!(
        h.shared.db.get_task("demo-p5").unwrap().unwrap().role,
        "plan"
    );
    h.shared.retry("demo-p5").unwrap();
    assert_eq!(h.shared.db.task_session("demo-p5", "grok").unwrap(), None);
    let _ = fs::write(h.fake_dir.join("grok.release"), "");
}

#[tokio::test]
async fn check_only_plan_run_is_success_then_worker_checked() {
    // check_only must not seal the planner as `checked`; the worker inherits
    // the flag and a no-diff worker run is `checked`.
    let h = Harness::with_providers(&[("grok", true, "success")], "127.0.0.1:0").await;
    h.install_gh_stub();
    fs::write(h.fake_dir.join("plan.json"), ONE).unwrap();
    submit_check_only(&h, "demo-cplan", &big_body());
    wait_two_runs(&h, "demo-cplan").await;
    h.wait_status("demo-cplan", &["waiting"], 15000).await;

    let t = h.shared.db.get_task("demo-cplan").unwrap().unwrap();
    assert!(t.check_only);
    assert_eq!(t.role, "worker", "planner must become the worker");
    assert_eq!(t.status, "waiting");

    let runs = h.shared.db.list_runs(Some("demo-cplan")).unwrap();
    let outcomes: Vec<_> = runs.iter().filter_map(|r| r.outcome.as_deref()).collect();
    assert!(
        outcomes.len() >= 2,
        "expected plan then worker, got {outcomes:?}"
    );
    assert_eq!(outcomes[0], "success", "plan-role run must not be checked");
    assert!(!outcomes[..outcomes.len() - 1].contains(&"checked"));
    assert_eq!(*outcomes.last().unwrap(), "checked");
    assert!(
        !h.fake_dir.join("gh-pr.log").exists(),
        "no PR for check-only"
    );
}

#[tokio::test]
async fn check_only_split_plan_children_inherit_flag() {
    let h = Harness::with_providers(&[("grok", true, "success")], "127.0.0.1:0").await;
    h.install_gh_stub();
    fs::write(h.fake_dir.join("plan.json"), TWO).unwrap();
    submit_check_only(&h, "demo-cplit", &big_body());
    h.wait_until("demo-cplit", 20000, |h| {
        h.shared.db.get_task("demo-cplit-s2").unwrap().is_some()
    })
    .await;

    let parent = h.shared.db.get_task("demo-cplit").unwrap().unwrap();
    assert_eq!(parent.role, "parent");
    assert!(parent.check_only);
    let plan_runs = h.shared.db.list_runs(Some("demo-cplit")).unwrap();
    assert_eq!(
        plan_runs.last().and_then(|r| r.outcome.as_deref()),
        Some("success"),
        "parent plan run must not be sealed as checked"
    );

    for key in ["demo-cplit-s1", "demo-cplit-s2"] {
        let child = h.shared.db.get_task(key).unwrap().unwrap();
        assert!(child.check_only, "{key} must inherit check_only");
        assert_eq!(child.role, "worker");
        h.wait_status(key, &["waiting"], 15000).await;
        let t = h.shared.db.get_task(key).unwrap().unwrap();
        assert_eq!(t.status, "waiting", "{key}");
        let outcomes: Vec<_> = h
            .shared
            .db
            .list_runs(Some(key))
            .unwrap()
            .into_iter()
            .filter_map(|r| r.outcome)
            .collect();
        assert_eq!(
            outcomes.last().map(String::as_str),
            Some("checked"),
            "{key} {outcomes:?}"
        );
    }
}
