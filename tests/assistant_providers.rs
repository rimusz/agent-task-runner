//! `[telegram.assistant] providers`: the assistant uses the first listed
//! provider that is usable right now, falls back down the list, waits while
//! listed providers are only busy/cooling, and keeps today's behaviour when
//! no listed provider can ever run.
#[allow(dead_code)]
mod common;

use common::Harness;
use std::time::Duration;

async fn harness(grok: &str, claude_on: bool, list: &[&str]) -> Harness {
    let h = Harness::with_providers(
        &[
            ("claude", claude_on, "assistant"),
            ("grok", true, grok),
            ("cursor", false, "success"),
            ("codex", false, "success"),
        ],
        "127.0.0.1:0",
    )
    .await;
    {
        let mut cfg = h.shared.config.write().unwrap();
        cfg.telegram.enabled = true;
        cfg.telegram.allowed_chat_ids = vec![42];
        cfg.telegram.assistant.enabled = true;
        cfg.telegram.assistant.timeout = Duration::from_secs(60);
        cfg.telegram.assistant.providers = list.iter().map(|s| s.to_string()).collect();
    }
    h
}

fn ask(h: &Harness) -> String {
    let upd = serde_json::json!({"message":{"chat":{"id":42},"text":"status please"}});
    let out = atr::telegram::handle_update(&h.shared, &upd).unwrap();
    out[0].text.clone()
}

fn cool(h: &Harness, provider: &str) {
    let until = (chrono::Utc::now() + chrono::Duration::minutes(10)).to_rfc3339();
    h.shared.db.set_cooldown(provider, &until).unwrap();
}

fn run_providers(h: &Harness) -> Vec<String> {
    h.shared
        .db
        .list_runs(Some("tg42"))
        .unwrap()
        .into_iter()
        .map(|r| r.provider)
        .collect()
}

#[tokio::test]
async fn first_listed_available_provider_is_used_and_named() {
    let h = harness("success", true, &["grok", "claude"]).await;
    let text = ask(&h);
    assert!(text.to_lowercase().contains("grok"), "{text}");
    h.wait_status("tg42", &["waiting"], 12000).await;
    assert_eq!(run_providers(&h), vec!["grok"]);
    let task = h.shared.db.get_task("tg42").unwrap().unwrap();
    assert_eq!(task.provider.as_deref(), Some("grok"));
}

#[tokio::test]
async fn cooling_first_choice_falls_back_to_the_next() {
    let h = harness("success", true, &["grok", "claude"]).await;
    cool(&h, "grok");
    let text = ask(&h);
    assert!(text.to_lowercase().contains("claude"), "{text}");
    h.wait_status("tg42", &["waiting"], 12000).await;
    assert_eq!(run_providers(&h), vec!["claude"]);
    // The status/dashboard line names the provider actually used.
    let cfg = h.shared.config.read().unwrap().clone();
    let v = atr::dashboard::telegram_assistant(&cfg, &h.shared.db);
    assert_eq!(v["last_provider"], "claude");
    let line = atr::dashboard::telegram_assistant_line(&v).unwrap();
    assert!(
        line.contains("provider claude (config: grok > claude)"),
        "{line}"
    );
}

#[tokio::test]
async fn rate_limited_run_moves_down_the_list() {
    let h = harness("rate_limit", true, &["grok", "claude"]).await;
    ask(&h);
    h.wait_status("tg42", &["waiting"], 15000).await;
    assert_eq!(run_providers(&h), vec!["grok", "claude"]);
}

#[tokio::test]
async fn waits_while_listed_providers_are_only_cooling() {
    // claude is enabled but not listed: the assistant must not use it.
    let h = harness("success", true, &["grok"]).await;
    cool(&h, "grok");
    ask(&h);
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let task = h.shared.db.get_task("tg42").unwrap().unwrap();
    assert_eq!(task.status, "queued");
    assert!(run_providers(&h).is_empty());
    let past = (chrono::Utc::now() - chrono::Duration::minutes(1)).to_rfc3339();
    h.shared.db.set_cooldown("grok", &past).unwrap();
    h.shared.kick.notify_waiters();
    h.wait_status("tg42", &["waiting"], 12000).await;
    assert_eq!(run_providers(&h), vec!["grok"]);
}

#[tokio::test]
async fn unusable_list_keeps_todays_first_available_behaviour() {
    let h = harness("success", true, &["grok"]).await;
    h.shared.db.set_enabled_override("grok", false).unwrap();
    ask(&h);
    h.wait_status("tg42", &["waiting"], 12000).await;
    assert_eq!(run_providers(&h), vec!["claude"]);
}

#[tokio::test]
async fn empty_list_keeps_the_single_provider_pin() {
    let h = harness("success", true, &[]).await;
    h.shared.config.write().unwrap().telegram.assistant.provider = "grok".into();
    let text = ask(&h);
    assert!(text.to_lowercase().contains("grok"), "{text}");
    h.wait_status("tg42", &["waiting"], 12000).await;
    assert_eq!(run_providers(&h), vec!["grok"]);
}
