mod common;

use common::Harness;
use std::time::Duration;

#[tokio::test]
async fn fake_provider_that_dies_mid_run_is_not_left_running() {
    let h = Harness::with_providers(&[("claude", true, "die")], "127.0.0.1:0").await;
    assert_eq!(
        h.submit("demo-die", "do the work"),
        atr::SubmitOutcome::Accepted
    );
    h.wait_status(
        "demo-die",
        &["failed", "queued", "needs_attention", "waiting"],
        8000,
    )
    .await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let t = h.shared.db.get_task("demo-die").unwrap().unwrap();
    assert_ne!(
        t.status, "running",
        "runner must settle a CLI that exits mid-run; last={}",
        t.result_text
    );
}

#[tokio::test]
async fn serve_requeues_a_dead_running_task_once() {
    let h = Harness::with_providers(&[("claude", true, "success")], "127.0.0.1:0").await;
    h.shared
        .db
        .upsert_task("demo-stuck", "/repo", None)
        .unwrap();
    h.shared.db.set_status("demo-stuck", "running").unwrap();
    let run = h.shared.db.start_run("demo-stuck", "claude", "").unwrap();
    h.shared.db.set_run_pid(run, 1_999_999_991).unwrap();
    h.shared.db.set_run_owner(run, None).unwrap();
    h.shared.db.set_task_owner("demo-stuck", None).unwrap();
    let mid = h
        .shared
        .db
        .enqueue_message("demo-stuck", "do work", "test")
        .unwrap();
    h.shared.db.mark_consumed(&[mid], run).unwrap();

    h.wait_until("demo-stuck", 8000, |h| {
        h.shared
            .db
            .list_runs(Some("demo-stuck"))
            .unwrap()
            .iter()
            .any(|r| r.id == run && r.outcome.as_deref() == Some("reaped"))
    })
    .await;
}

#[tokio::test]
async fn hanging_live_cli_is_not_reaped() {
    let h = Harness::with_providers(&[("claude", true, "hold")], "127.0.0.1:0").await;
    assert_eq!(
        h.submit("demo-hang", "do the work"),
        atr::SubmitOutcome::Accepted
    );
    h.wait_status("demo-hang", &["running"], 8000).await;
    tokio::time::sleep(Duration::from_millis(2500)).await;
    assert_eq!(
        h.shared.db.get_task("demo-hang").unwrap().unwrap().status,
        "running"
    );
    assert!(
        h.shared.inflight.lock().unwrap().contains_key("demo-hang"),
        "live hang must stay in-flight"
    );
    std::fs::write(h.fake_dir.join("claude.release"), "ok\n").unwrap();
    h.wait_status("demo-hang", &["waiting", "done"], 8000).await;
}

#[tokio::test]
async fn selfheal_status_cli_prints_health_path() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("p.md"), "p\n").unwrap();
    let cfg = dir.path().join("config.toml");
    let health = dir.path().join("health");
    std::fs::write(
        &cfg,
        format!(
            r#"
listen = "127.0.0.1:0"
state_db = "{db}"
worktree_root = "{root}/work"
repo_path = "{root}/repo"
system_prompt_file = "{root}/p.md"
display_timezone = "UTC"
[selfheal]
health_dir = "{health}"
check = false
[[provider]]
name = "claude"
kind = "claude"
cli = "/bin/true"
"#,
            db = dir.path().join("s.db").display(),
            root = dir.path().display(),
            health = health.display(),
        ),
    )
    .unwrap();
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_atr"))
        .args(["selfheal", "status"])
        .env("ATR_CONFIG", &cfg)
        .env("HOME", dir.path().join("home"))
        .output()
        .unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.status.success(), "{text}");
    assert!(text.contains("selfheal enabled="), "{text}");
    assert!(text.contains("health.json"), "{text}");
}
