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
    // The parent never runs. With no verify configured, the judge finds no
    // winner and parks it.
    assert!(run_providers(h, key).is_empty());
    let d = wait_decided(h, key).await;
    assert_eq!(d.winner, None);
    assert_eq!(d.arms[0].verify, "none");
    assert_eq!(d.arms[1].verify, "skipped");
    assert_eq!(
        h.shared.db.get_task(key).unwrap().unwrap().status,
        "needs_attention"
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

// ---- Compare MVP PR 2: judge, winner promotion, loser cleanup ----

async fn harness2(claude: &str, grok: &str) -> Harness {
    let h = Harness::with_providers(
        &[
            ("claude", true, claude),
            ("grok", true, grok),
            ("cursor", false, "success"),
            ("codex", false, "success"),
        ],
        "127.0.0.1:0",
    )
    .await;
    // A bare origin so the winner's push can be checked.
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
    h
}

fn git(dir: &std::path::Path, args: &[&str]) -> String {
    let o = std::process::Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .unwrap();
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn set_verify(h: &Harness, cmd: &str) {
    let mut cfg = h.shared.config.write().unwrap();
    cfg.verify.enabled = true;
    cfg.verify.max_retries = 0;
    cfg.verify.timeout = std::time::Duration::from_secs(5);
    cfg.verify.commands = vec![cmd.to_string()];
}

fn lines(h: &Harness, kind: &str, n: u32) {
    std::fs::write(h.fake_dir.join(format!("{kind}.lines")), n.to_string()).unwrap();
}

fn add_compare(h: &Harness, key: &str) -> Vec<String> {
    let cfg = h.shared.config.read().unwrap().clone();
    atr::compare::add(
        &h.shared,
        &cfg,
        &atr::compare::CompareSpec {
            key: key.into(),
            repo: h.repo.display().to_string(),
            message: "make the change".into(),
            output_kind: "pr".into(),
            title: "Make the change".into(),
            providers: vec!["claude".into(), "grok".into()],
            ..Default::default()
        },
    )
    .unwrap()
}

async fn wait_decided(h: &Harness, key: &str) -> atr::compare::Decision {
    h.wait_until(key, 40000, |h| {
        h.shared
            .db
            .get_task(key)
            .ok()
            .flatten()
            .and_then(|t| serde_json::from_str::<atr::compare::Decision>(&t.compare_decision).ok())
            .is_some_and(|d| !d.decided_at.is_empty())
    })
    .await;
    let t = h.shared.db.get_task(key).unwrap().unwrap();
    serde_json::from_str(&t.compare_decision).unwrap()
}

#[tokio::test]
async fn smaller_passing_diff_wins_and_becomes_the_pr() {
    let h = harness2("edit_lines", "edit_lines").await;
    set_verify(&h, "true");
    lines(&h, "claude", 10);
    lines(&h, "grok", 3);
    add_compare(&h, "demo-w1");
    let d = wait_decided(&h, "demo-w1").await;
    assert_eq!(d.winner.as_deref(), Some("demo-w1-b"), "{d:?}");
    assert_eq!(d.winner_provider.as_deref(), Some("grok"));
    assert!(d.reason.contains("smaller diff (3 vs 10"), "{}", d.reason);
    assert!(!d.walkover);
    assert_eq!(d.pr_url, "https://github.com/example/repo/pull/7");
    let a = &d.arms[0];
    assert_eq!((a.files, a.added, a.verify.as_str()), (1, 10, "passed"));

    // Arms could not push while running.
    for kind in ["claude", "grok"] {
        let p = std::fs::read_to_string(h.fake_dir.join(format!("{kind}.push"))).unwrap();
        let cfgdump =
            std::fs::read_to_string(h.fake_dir.join(format!("{kind}.pushcfg"))).unwrap_or_default();
        assert_eq!(p.trim(), "blocked", "{kind} arm pushed; cfg:\n{cfgdump}");
    }
    // The user's repo config was never hardened (worktree-scoped config).
    assert!(git(&h.repo, &["config", "--get", "remote.origin.pushurl"]).is_empty());
    assert!(git(&h.repo, &["config", "--get", "core.hooksPath"]).is_empty());

    // The parent took over the winner's tree, provider and session.
    let p = h.shared.db.get_task("demo-w1").unwrap().unwrap();
    assert_eq!(p.status, "waiting");
    assert_eq!(p.role, "worker");
    assert_eq!(p.provider.as_deref(), Some("grok"));
    assert!(p.session_id.is_some());
    assert_eq!(p.result_pr_url, d.pr_url);
    let wt = std::path::PathBuf::from(p.worktree.clone().unwrap());
    assert!(wt.ends_with("demo-w1-b"), "{wt:?}");
    assert_eq!(
        git(&wt, &["rev-parse", "--abbrev-ref", "HEAD"]).trim(),
        "atr/demo-w1"
    );
    // Pushed as atr/<parent>; arm branches never reached origin.
    let origin = h.dir.path().join("origin.git");
    let refs = git(&origin, &["for-each-ref", "--format=%(refname)"]);
    assert!(refs.contains("refs/heads/atr/demo-w1\n"), "{refs}");
    assert!(
        !refs.contains("demo-w1-a") && !refs.contains("demo-w1-b"),
        "{refs}"
    );
    // PR body carries the comparison footer.
    let pr = std::fs::read_to_string(h.fake_dir.join("gh-pr.log")).unwrap();
    assert!(pr.contains("atr compare"), "{pr}");
    assert!(pr.contains("demo-w1-b 🏆"), "{pr}");

    // Loser: worktree removed, local branch deleted, arm done.
    let la = h.shared.db.get_task("demo-w1-a").unwrap().unwrap();
    assert_eq!(la.status, "done");
    assert!(la.worktree.is_none());
    assert!(!h.dir.path().join("work/repo/demo-w1-a").exists());
    assert!(git(&h.repo, &["branch", "--list", "atr/demo-w1-a"])
        .trim()
        .is_empty());
    let wb = h.shared.db.get_task("demo-w1-b").unwrap().unwrap();
    assert_eq!(wb.status, "done");
    assert!(
        wb.worktree.is_none(),
        "gc must not own the winner's tree via the arm"
    );

    let cfg = h.shared.config.read().unwrap().clone();
    let show = atr::compare::show_text(&cfg, &h.shared.db, "demo-w1").unwrap();
    assert!(show.contains("decision: demo-w1-b"), "{show}");
    assert!(show.contains("pr: https://"), "{show}");
    let js = atr::compare::show_json(&cfg, &h.shared.db, "demo-w1").unwrap();
    assert_eq!(js["decision"]["winner"], "demo-w1-b");

    // A follow-up on the parent resumes the winner's provider in its tree.
    h.submit("demo-w1", "one more thing");
    h.wait_until("demo-w1", 20000, |h| {
        h.shared.db.list_runs(Some("demo-w1")).unwrap().len() == 1
            && h.shared.db.get_task("demo-w1").unwrap().unwrap().status == "waiting"
    })
    .await;
    let runs = h.shared.db.list_runs(Some("demo-w1")).unwrap();
    assert_eq!(runs[0].provider, "grok");
    let p = h.shared.db.get_task("demo-w1").unwrap().unwrap();
    assert_eq!(p.worktree.as_deref(), Some(wt.to_str().unwrap()));
}

#[tokio::test]
async fn arm_that_fails_verify_loses_even_with_a_smaller_diff() {
    let h = harness2("edit_lines", "edit_lines").await;
    set_verify(&h, "test ! -f CHANGE_claude.md");
    lines(&h, "claude", 1);
    lines(&h, "grok", 20);
    add_compare(&h, "demo-w2");
    let d = wait_decided(&h, "demo-w2").await;
    assert_eq!(d.winner.as_deref(), Some("demo-w2-b"), "{d:?}");
    assert_eq!(d.reason, "only arm that passed verify");
    assert_eq!(d.arms[0].verify, "failed");
}

#[tokio::test]
async fn walkover_when_one_arm_hits_a_limit() {
    let h = harness2("rate_limit", "edit_lines").await;
    set_verify(&h, "true");
    add_compare(&h, "demo-w3");
    let d = wait_decided(&h, "demo-w3").await;
    assert_eq!(d.winner.as_deref(), Some("demo-w3-b"), "{d:?}");
    assert!(d.walkover);
    assert_eq!(d.arms[0].verify, "none");
}

#[tokio::test]
async fn no_winner_parks_parent_in_needs_attention() {
    let h = harness2("edit_lines", "edit_lines").await;
    set_verify(&h, "false");
    add_compare(&h, "demo-w4");
    let d = wait_decided(&h, "demo-w4").await;
    assert_eq!(d.winner, None);
    assert_eq!(d.reason, "no arm passed verify");
    let p = h.shared.db.get_task("demo-w4").unwrap().unwrap();
    assert_eq!(p.status, "needs_attention");
    assert_eq!(p.role, "compare");
    assert!(p.result_pr_url.is_empty());
    assert!(!h.fake_dir.join("gh-pr.log").exists());
    // Both arm trees are kept for inspection.
    for k in ["demo-w4-a", "demo-w4-b"] {
        let t = h.shared.db.get_task(k).unwrap().unwrap();
        assert!(std::path::Path::new(&t.worktree.unwrap()).exists());
    }
}

#[tokio::test]
async fn add_refuses_without_verify_or_with_unusable_provider() {
    let h = harness2("success", "success").await;
    let cfg = h.shared.config.read().unwrap().clone();
    let spec = atr::compare::CompareSpec {
        key: "demo-w5".into(),
        repo: h.repo.display().to_string(),
        message: "x".into(),
        providers: vec!["claude".into(), "grok".into()],
        ..Default::default()
    };
    let err = atr::compare::add(&h.shared, &cfg, &spec).unwrap_err();
    assert!(err.to_string().contains("verify"), "{err}");
    set_verify(&h, "true");
    h.shared.db.set_enabled_override("grok", false).unwrap();
    let cfg = h.shared.config.read().unwrap().clone();
    let err = atr::compare::add(&h.shared, &cfg, &spec).unwrap_err();
    assert!(err.to_string().contains("disabled"), "{err}");
    assert!(h.shared.db.get_task("demo-w5").unwrap().is_none());
}
