//! Compare mode, PR 1: the parent/arm task model and the strict provider
//! pin. An arm runs only on its pinned provider; every path that would fail
//! a normal task over to another provider fails the arm instead
//! (`arm_unavailable`), so both arms never collapse onto one provider.
#[allow(dead_code)]
mod common;

use common::Harness;

fn providers(claude: &str, claude_on: bool) -> Vec<(&str, bool, &str)> {
    vec![
        ("claude", claude_on, claude),
        ("grok", true, "success"),
        ("cursor", false, "success"),
        ("codex", false, "success"),
    ]
}

async fn harness(claude: &str, claude_on: bool) -> Harness {
    Harness::with_providers(&providers(claude, claude_on), "127.0.0.1:0").await
}

fn create(h: &Harness, key: &str) -> Vec<String> {
    let keys = atr::compare::create(
        &h.shared.db,
        &atr::compare::CompareSpec {
            key: key.into(),
            repo: h.repo.display().to_string(),
            issue: Some(1),
            message: "compare me".into(),
            output_kind: "pr".into(),
            providers: vec!["claude".into(), "grok".into()],
            ..Default::default()
        },
    )
    .unwrap();
    h.shared.kick.notify_waiters();
    keys
}

fn run_providers(h: &Harness, key: &str) -> Vec<String> {
    h.shared
        .db
        .list_runs(Some(key))
        .unwrap()
        .into_iter()
        .map(|r| r.provider)
        .collect()
}

fn has_arm_unavailable(h: &Harness, key: &str) -> bool {
    h.shared.db.list_runs(Some(key)).unwrap().iter().any(|r| {
        h.shared
            .db
            .events_for_run(r.id)
            .unwrap()
            .iter()
            .any(|e| e.kind == "arm_unavailable")
    })
}

/// The claude arm must fail on claude alone, and the grok arm must finish
/// on grok, with neither arm ever running on the other's provider.
async fn assert_arm_failed_without_failover(h: &Harness, key: &str) {
    let a = format!("{key}-a");
    let b = format!("{key}-b");
    let st = h.wait_status(&a, &["failed"], 30000).await;
    assert_eq!(st, "failed");
    h.wait_status(&b, &["waiting"], 30000).await;
    let pa = run_providers(h, &a);
    assert!(!pa.is_empty(), "arm a never ran");
    assert!(
        pa.iter().all(|p| p == "claude"),
        "arm a failed over: {pa:?}"
    );
    assert!(has_arm_unavailable(h, &a), "no arm_unavailable event");
    let pb = run_providers(h, &b);
    assert!(pb.iter().all(|p| p == "grok"), "arm b moved: {pb:?}");
    let ta = h.shared.db.get_task(&a).unwrap().unwrap();
    assert_eq!(ta.provider.as_deref(), Some("claude"));
    assert!(ta.pin_strict);
    // The parent never runs; it stays waiting for the judge.
    assert!(run_providers(h, key).is_empty());
    assert_eq!(
        h.shared.db.get_task(key).unwrap().unwrap().status,
        "waiting"
    );
}

#[tokio::test]
async fn compare_creates_parent_and_two_pinned_arms() {
    let h = harness("success", true).await;
    let keys = create(&h, "demo-c1");
    assert_eq!(keys, vec!["demo-c1-a", "demo-c1-b"]);
    let parent = h.shared.db.get_task("demo-c1").unwrap().unwrap();
    assert_eq!(parent.role, "compare");
    assert_eq!(parent.output_kind, "pr");
    for (k, p) in [("demo-c1-a", "claude"), ("demo-c1-b", "grok")] {
        h.wait_status(k, &["waiting"], 15000).await;
        let t = h.shared.db.get_task(k).unwrap().unwrap();
        assert_eq!(t.role, "arm");
        assert_eq!(t.parent_key.as_deref(), Some("demo-c1"));
        assert_eq!(t.output_kind, "text");
        assert!(t.pin_strict);
        assert_eq!(run_providers(&h, k), vec![p.to_string()]);
    }
    assert!(run_providers(&h, "demo-c1").is_empty());
    let arms = atr::compare::arms_of(&h.shared.db, "demo-c1").unwrap();
    assert_eq!(arms.len(), 2);
    // A second create with the same key is refused.
    let again = atr::compare::create(
        &h.shared.db,
        &atr::compare::CompareSpec {
            key: "demo-c1".into(),
            repo: h.repo.display().to_string(),
            message: "x".into(),
            providers: vec!["claude".into(), "grok".into()],
            ..Default::default()
        },
    );
    assert!(again.is_err());
}

#[tokio::test]
async fn strict_arm_does_not_fail_over_on_limit() {
    let h = harness("rate_limit", true).await;
    create(&h, "demo-c2");
    assert_arm_failed_without_failover(&h, "demo-c2").await;
}

#[tokio::test]
async fn strict_arm_does_not_fail_over_on_auth() {
    let h = harness("auth", true).await;
    create(&h, "demo-c3");
    assert_arm_failed_without_failover(&h, "demo-c3").await;
}

#[tokio::test]
async fn strict_arm_does_not_fail_over_when_unavailable() {
    let h = harness("unavailable", true).await;
    create(&h, "demo-c4");
    assert_arm_failed_without_failover(&h, "demo-c4").await;
}

#[tokio::test]
async fn strict_arm_does_not_fail_over_on_server_error() {
    let h = harness("server", true).await;
    create(&h, "demo-c5");
    assert_arm_failed_without_failover(&h, "demo-c5").await;
}

#[tokio::test]
async fn strict_arm_does_not_fail_over_on_needs_attention() {
    let h = harness("permission_denied", true).await;
    create(&h, "demo-c6");
    assert_arm_failed_without_failover(&h, "demo-c6").await;
}

#[tokio::test]
async fn strict_arm_does_not_fail_over_after_stall_limit() {
    let h = harness("stall", true).await;
    create(&h, "demo-c7");
    assert_arm_failed_without_failover(&h, "demo-c7").await;
    // stall_resumes = 1: one same-provider retry, then the arm fails.
    assert_eq!(run_providers(&h, "demo-c7-a").len(), 2);
}

#[cfg(unix)]
#[tokio::test]
async fn strict_arm_does_not_fail_over_on_spawn_error() {
    use std::os::unix::fs::PermissionsExt;
    let h = harness("success", true).await;
    let cli = h.dir.path().join("bin/claude");
    std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o644)).unwrap();
    create(&h, "demo-c8");
    assert_arm_failed_without_failover(&h, "demo-c8").await;
}

#[tokio::test]
async fn strict_arm_fails_when_pinned_provider_is_disabled() {
    let h = harness("success", false).await;
    create(&h, "demo-c9");
    let st = h.wait_status("demo-c9-a", &["failed"], 15000).await;
    assert_eq!(st, "failed");
    h.wait_status("demo-c9-b", &["waiting"], 15000).await;
    let runs = h.shared.db.list_runs(Some("demo-c9-a")).unwrap();
    assert_eq!(runs.len(), 1, "only the marker run: {runs:?}");
    assert_eq!(runs[0].outcome.as_deref(), Some("arm_unavailable"));
    assert!(has_arm_unavailable(&h, "demo-c9-a"));
    assert_eq!(run_providers(&h, "demo-c9-b"), vec!["grok".to_string()]);
}

#[tokio::test]
async fn strict_arm_waits_while_pinned_provider_cools_down() {
    let h = harness("success", true).await;
    let until = (chrono::Utc::now() + chrono::Duration::minutes(10)).to_rfc3339();
    h.shared.db.set_cooldown("claude", &until).unwrap();
    create(&h, "demo-c10");
    h.wait_status("demo-c10-b", &["waiting"], 15000).await;
    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
    h.shared.kick.notify_waiters();
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    let a = h.shared.db.get_task("demo-c10-a").unwrap().unwrap();
    assert_eq!(a.status, "queued", "arm must wait, not move or fail");
    assert!(run_providers(&h, "demo-c10-a").is_empty());
    // Once the cooldown ends, the arm runs on its own provider.
    let past = (chrono::Utc::now() - chrono::Duration::minutes(1)).to_rfc3339();
    h.shared.db.set_cooldown("claude", &past).unwrap();
    h.shared.kick.notify_waiters();
    h.wait_status("demo-c10-a", &["waiting"], 15000).await;
    assert_eq!(run_providers(&h, "demo-c10-a"), vec!["claude".to_string()]);
}

#[tokio::test]
async fn normal_tasks_still_fail_over() {
    let h = harness("rate_limit", true).await;
    h.submit("demo-c11", "soft pin");
    h.wait_status("demo-c11", &["waiting"], 15000).await;
    let p = run_providers(&h, "demo-c11");
    assert!(p.contains(&"grok".to_string()), "{p:?}");
}

#[tokio::test]
async fn parse_arms_validates() {
    let h = harness("success", true).await;
    let cfg = h.shared.config.read().unwrap().clone();
    assert_eq!(
        atr::compare::parse_arms(&cfg, "claude, grok").unwrap(),
        vec!["claude", "grok"]
    );
    for bad in ["claude", "claude,claude", "claude,nope", "a,b,c", ""] {
        assert!(atr::compare::parse_arms(&cfg, bad).is_err(), "{bad}");
    }
}
