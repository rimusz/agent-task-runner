//! Interrupt-and-continue: stop a live turn, resume the same session with the
//! new message plus a "continue your unfinished task" preamble.

#[allow(dead_code)]
mod common;

use common::Harness;
use std::fs;

const KEY: &str = "demo-intr";

async fn live_harness(beh: &str) -> Harness {
    let h = Harness::with_providers(
        &[
            ("claude", true, beh),
            ("grok", false, "success"),
            ("cursor", false, "success"),
            ("codex", false, "success"),
        ],
        "127.0.0.1:0",
    )
    .await;
    fs::write(h.fake_dir.join("claude.capture"), "").unwrap();
    h.submit(KEY, "long run: refactor the parser");
    wait_live(&h).await;
    h
}

async fn wait_live(h: &Harness) {
    h.wait_until(KEY, 8000, |h| {
        let live = h
            .shared
            .inflight
            .lock()
            .unwrap()
            .get(KEY)
            .is_some_and(|i| i.run_id > 0 && i.kill.lock().unwrap().is_some_and(|p| p > 0));
        // The session id line has been read once the run row has it.
        let sid = h
            .shared
            .db
            .list_runs(Some(KEY))
            .ok()
            .and_then(|r| r.last().and_then(|r| r.session_id.clone()))
            .is_some();
        live && sid
    })
    .await;
}

fn claude_resume_args(h: &Harness) -> Vec<Vec<String>> {
    let raw = fs::read_to_string(h.fake_dir.join("invocations.jsonl")).unwrap_or_default();
    raw.lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|v| v["kind"] == "claude")
        .map(|v| {
            v["args"]
                .as_array()
                .unwrap()
                .iter()
                .map(|a| a.as_str().unwrap_or("").to_string())
                .collect()
        })
        .collect()
}

fn outcomes(h: &Harness) -> Vec<Option<String>> {
    h.shared
        .db
        .list_runs(Some(KEY))
        .unwrap()
        .into_iter()
        .map(|r| r.outcome)
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_steer_interrupt_resumes_same_session_with_preamble() {
    let h = live_harness("hold_session").await;
    // The resumed turn should just finish.
    fs::write(h.fake_dir.join("claude.behavior"), "success").unwrap();
    let raw = serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": {"name": "steer_task", "arguments": {
            "key": KEY, "message": "change of plan: also add tests", "interrupt": true
        }}
    })
    .to_string();
    let resp = atr::mcp::handle_raw(&h.shared, &raw).await;
    let text = resp[0]["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("\"interrupt\""), "{text}");
    assert!(text.contains("interrupted"), "{text}");

    h.wait_until(KEY, 15000, |h| {
        let o = outcomes(h);
        o.len() >= 2 && o.last().unwrap().as_deref() == Some("success")
    })
    .await;
    let o = outcomes(&h);
    assert_eq!(o[0].as_deref(), Some("interrupted"), "{o:?}");

    let calls = claude_resume_args(&h);
    let last = calls.last().unwrap();
    let i = last.iter().position(|a| a == "--resume").expect("resumed");
    assert_eq!(last[i + 1], "claude-hold-sid", "{last:?}");

    let stdin = fs::read_to_string(h.fake_dir.join("claude.stdin")).unwrap();
    let prompts: Vec<&str> = stdin.split("=====ATR-FAKE-END=====").collect();
    let second = prompts[1];
    assert!(second.contains("previous turn was interrupted"), "{second}");
    assert!(second.contains("continue your unfinished task"), "{second}");
    assert!(
        second.contains("change of plan: also add tests"),
        "{second}"
    );
    assert!(
        !second.contains("long run: refactor the parser"),
        "resume sends only the delta: {second}"
    );

    let (cooling, degraded) = h.shared.db.provider_blocked("claude").unwrap();
    assert!(
        !cooling && !degraded,
        "interrupt must not cool down the provider"
    );
    let t = h.shared.db.get_task(KEY).unwrap().unwrap();
    assert_eq!(t.provider.as_deref(), Some("claude"), "no failover");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interrupt_without_message_leaves_task_waiting() {
    let h = live_harness("hold_session").await;
    assert_eq!(
        h.shared.interrupt_live(KEY).unwrap(),
        atr::InterruptOutcome::Interrupted
    );
    h.wait_status(KEY, &["waiting"], 8000).await;
    assert_eq!(outcomes(&h), vec![Some("interrupted".to_string())]);
    assert!(h.shared.db.pending_messages(KEY).unwrap().is_empty());
    let (cooling, degraded) = h.shared.db.provider_blocked("claude").unwrap();
    assert!(!cooling && !degraded);
    assert_eq!(
        h.shared.db.task_session(KEY, "claude").unwrap().as_deref(),
        Some("claude-hold-sid"),
        "session kept for the next message"
    );
    // Nothing live any more: a second interrupt is a no-op.
    assert_eq!(
        h.shared.interrupt_live(KEY).unwrap(),
        atr::InterruptOutcome::NotRunning
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interrupt_refuses_provider_that_cannot_resume() {
    let h = live_harness("hold_session").await;
    {
        let mut cfg = h.shared.config.write().unwrap();
        let p = cfg
            .providers
            .iter_mut()
            .find(|p| p.name == "claude")
            .unwrap();
        p.resume_flag.clear();
    }
    assert_eq!(
        h.shared
            .submit(KEY, "follow-up", "test", None, None, false, false)
            .unwrap(),
        atr::SubmitOutcome::Queued
    );
    assert_eq!(
        h.shared.interrupt_live(KEY).unwrap(),
        atr::InterruptOutcome::NotResumable
    );
    // Still live; the run was not marked or killed.
    assert_eq!(outcomes(&h), vec![None]);
    fs::write(h.fake_dir.join("claude.behavior"), "success").unwrap();
    fs::write(h.fake_dir.join("claude.release"), "").unwrap();
    h.wait_until(KEY, 15000, |h| {
        outcomes(h)
            .iter()
            .filter(|o| o.as_deref() == Some("success"))
            .count()
            >= 2
    })
    .await;
}
