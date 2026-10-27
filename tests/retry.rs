//! Retry regressions (atr bug report on `atr retry tg873752741`):
//! - the per-task wall clock restarts on retry or a new turn, instead of
//!   counting from the task's first run days ago;
//! - a retried task without a strict pin picks its provider again from the
//!   current config (a Telegram assistant task follows `[telegram.assistant]`);
//! - retry on a finished task whose last prompt was consumed must become
//!   dispatchable (`runnable_keys` / a new run) without a new submit.
#[allow(dead_code)]
mod common;

use common::Harness;
use std::time::Duration;

const PROVIDERS: &[(&str, bool, &str)] = &[
    ("claude", true, "assistant"),
    ("grok", true, "success"),
    ("cursor", false, "success"),
    ("codex", false, "success"),
];

/// Make the task look like it first ran long ago: shrink `task_timeout` so
/// the existing run is far past it.
fn age_out(h: &Harness) {
    std::thread::sleep(Duration::from_millis(50));
    h.shared.config.write().unwrap().task_timeout = Duration::from_millis(20);
}

fn outcomes(h: &Harness, key: &str) -> Vec<String> {
    h.shared
        .db
        .list_runs(Some(key))
        .unwrap()
        .into_iter()
        .map(|r| r.outcome.unwrap_or_default())
        .collect()
}

#[tokio::test]
async fn retry_restarts_the_wall_clock() {
    let h = Harness::with_providers(PROVIDERS, "127.0.0.1:0").await;
    h.submit("demo-r1", "first attempt");
    h.wait_status("demo-r1", &["waiting"], 10000).await;
    h.shared.db.set_status("demo-r1", "failed").unwrap();
    // The first run is now far past the (tiny) cap, like Oct 16 vs. today.
    age_out(&h);
    h.shared
        .db
        .enqueue_message("demo-r1", "first attempt (unfinished)", "cli")
        .unwrap();
    h.shared.retry("demo-r1").unwrap();
    // The retried run must start instead of failing at once: the cap only
    // counts runs since the retry.
    h.wait_until("demo-r1", 10000, |h| {
        h.shared.db.list_runs(Some("demo-r1")).unwrap().len() >= 2
    })
    .await;
    let o = outcomes(&h, "demo-r1");
    assert_ne!(o[1], "task_timeout", "retry hit the old wall clock: {o:?}");
    let t = h.shared.db.get_task("demo-r1").unwrap().unwrap();
    assert!(!t.clock_started.is_empty());
}

#[tokio::test]
async fn new_turn_on_a_finished_task_restarts_the_wall_clock() {
    let h = Harness::with_providers(PROVIDERS, "127.0.0.1:0").await;
    h.submit("demo-r2", "first turn");
    h.wait_status("demo-r2", &["waiting"], 10000).await;
    age_out(&h);
    h.submit("demo-r2", "second turn, days later");
    h.wait_until("demo-r2", 10000, |h| {
        h.shared.db.list_runs(Some("demo-r2")).unwrap().len() >= 2
    })
    .await;
    let o = outcomes(&h, "demo-r2");
    assert_ne!(o[1], "task_timeout", "{o:?}");
}

#[tokio::test]
async fn internal_requeue_still_hits_the_wall_clock() {
    let h = Harness::with_providers(PROVIDERS, "127.0.0.1:0").await;
    h.submit("demo-r3", "attempt");
    h.wait_status("demo-r3", &["waiting"], 10000).await;
    age_out(&h);
    h.shared
        .db
        .enqueue_message("demo-r3", "verify follow-up", "verify")
        .unwrap();
    h.shared.db.set_status("demo-r3", "queued").unwrap();
    h.shared.kick.notify_waiters();
    h.wait_status("demo-r3", &["failed"], 10000).await;
    assert_eq!(outcomes(&h, "demo-r3").last().unwrap(), "task_timeout");
}

#[tokio::test]
async fn retried_assistant_task_follows_the_reloaded_provider() {
    let h = Harness::with_providers(PROVIDERS, "127.0.0.1:0").await;
    {
        let mut cfg = h.shared.config.write().unwrap();
        cfg.telegram.enabled = true;
        cfg.telegram.allowed_chat_ids = vec![42];
        cfg.telegram.assistant.enabled = true;
        cfg.telegram.assistant.provider = "claude".into();
        cfg.telegram.assistant.timeout = Duration::from_secs(60);
    }
    let upd = serde_json::json!({"message":{"chat":{"id":42},"text":"hello"}});
    atr::telegram::handle_update(&h.shared, &upd).unwrap();
    h.wait_status("tg42", &["waiting"], 12000).await;
    let t = h.shared.db.get_task("tg42").unwrap().unwrap();
    assert_eq!(t.provider.as_deref(), Some("claude"));
    // The user switches the assistant to grok (config reload), and the task
    // is retried after failing.
    h.shared.config.write().unwrap().telegram.assistant.provider = "grok".into();
    h.shared.db.set_status("tg42", "failed").unwrap();
    h.shared
        .db
        .enqueue_message("tg42", "hello again", "telegram")
        .unwrap();
    h.shared.retry("tg42").unwrap();
    h.wait_until("tg42", 12000, |h| {
        h.shared.db.list_runs(Some("tg42")).unwrap().len() >= 2
            && h.shared.db.get_task("tg42").unwrap().unwrap().status == "waiting"
    })
    .await;
    let runs = h.shared.db.list_runs(Some("tg42")).unwrap();
    assert_eq!(runs[1].provider, "grok", "retry stayed on claude");
}

#[tokio::test]
async fn retried_task_re_resolves_from_current_provider_config() {
    let h = Harness::with_providers(PROVIDERS, "127.0.0.1:0").await;
    h.submit("demo-r5", "work");
    h.wait_status("demo-r5", &["waiting"], 10000).await;
    let t = h.shared.db.get_task("demo-r5").unwrap().unwrap();
    assert_eq!(t.provider.as_deref(), Some("claude"));
    // claude gets disabled; a retry must not stay on it.
    h.shared.db.set_enabled_override("claude", false).unwrap();
    h.shared.db.set_status("demo-r5", "failed").unwrap();
    h.shared
        .db
        .enqueue_message("demo-r5", "try again", "cli")
        .unwrap();
    h.shared.retry("demo-r5").unwrap();
    let t = h.shared.db.get_task("demo-r5").unwrap().unwrap();
    assert_eq!(t.provider, None, "retry must drop the old provider pin");
    h.wait_until("demo-r5", 10000, |h| {
        h.shared.db.list_runs(Some("demo-r5")).unwrap().len() >= 2
    })
    .await;
    assert_eq!(
        h.shared.db.list_runs(Some("demo-r5")).unwrap()[1].provider,
        "grok"
    );
}

#[tokio::test]
async fn retry_dispatches_without_a_new_submit() {
    let h = Harness::with_providers(PROVIDERS, "127.0.0.1:0").await;
    h.submit("demo-r7", "do the work");
    h.wait_status("demo-r7", &["waiting"], 10000).await;
    assert!(
        h.shared.db.pending_messages("demo-r7").unwrap().is_empty(),
        "finished run should have consumed its messages"
    );
    assert!(
        !h.shared
            .db
            .runnable_keys()
            .unwrap()
            .iter()
            .any(|k| k == "demo-r7"),
        "consumed task must not be dispatchable before retry"
    );
    h.shared.retry("demo-r7").unwrap();
    // No atr submit / enqueue — retry itself must make the task dispatchable.
    h.wait_until("demo-r7", 10000, |h| {
        h.shared.db.list_runs(Some("demo-r7")).unwrap().len() >= 2
    })
    .await;
}

#[tokio::test]
async fn retried_compare_arm_keeps_its_strict_pin() {
    let h = Harness::with_providers(PROVIDERS, "127.0.0.1:0").await;
    h.submit("demo-r6", "x");
    h.wait_status("demo-r6", &["waiting"], 10000).await;
    h.shared.db.set_pin_strict("demo-r6", true).unwrap();
    h.shared.db.set_status("demo-r6", "failed").unwrap();
    h.shared.retry("demo-r6").unwrap();
    let t = h.shared.db.get_task("demo-r6").unwrap().unwrap();
    assert_eq!(t.provider.as_deref(), Some("claude"));
    assert!(t.pin_strict);
}
