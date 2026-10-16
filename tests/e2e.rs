mod common;

use atr::SubmitOutcome;
use common::Harness;
use std::time::Duration;

#[tokio::test]
async fn each_provider_parses_its_usage_shape() {
    let cases = [
        ("claude", "demo-11"),
        ("grok", "demo-12"),
        ("cursor", "demo-13"),
        ("codex", "demo-14"),
        ("opencode", "demo-15"),
    ];
    for (only, key) in cases {
        let providers = [
            ("claude", only == "claude", "success"),
            ("grok", only == "grok", "success"),
            ("cursor", only == "cursor", "success"),
            ("codex", only == "codex", "success"),
            ("opencode", only == "opencode", "success"),
        ];
        let h = Harness::with_providers(&providers, "127.0.0.1:0").await;
        assert_eq!(h.submit(key, "do the work"), SubmitOutcome::Accepted);
        h.wait_status(key, &["waiting"], 8000).await;
        let task = h.shared.db.get_task(key).unwrap().unwrap();
        assert_eq!(task.provider.as_deref(), Some(only));
        assert!(task.session_id.is_some(), "{only} session");
        let runs = h.shared.db.list_runs(Some(key)).unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].outcome.as_deref(), Some("success"));
        let days = h.shared.db.usage_by_day(1).unwrap();
        assert!(
            days.iter().any(|d| d.provider == only && d.cache_read > 0),
            "{only} usage {days:?}"
        );
        let usage_rows = h.shared.db.list_usage_for_run(runs[0].id).unwrap();
        assert_eq!(usage_rows.len(), 1, "{only} must store one usage row");
        match only {
            "claude" => {
                assert_eq!(usage_rows[0].input, 100);
                assert_eq!(usage_rows[0].output, 50);
                assert_eq!(usage_rows[0].cache_read, 800);
                assert_eq!(usage_rows[0].cost_kind.as_deref(), Some("est"));
                assert!((usage_rows[0].cost_usd.unwrap() - 0.0123).abs() < 1e-9);
            }
            "grok" => {
                assert_eq!(usage_rows[0].input, 10, "duplicate grok usage must not sum");
                assert_eq!(usage_rows[0].output, 12);
                assert_eq!(usage_rows[0].cache_read, 90);
                assert_eq!(usage_rows[0].cost_kind.as_deref(), Some("est"));
                assert!((usage_rows[0].cost_usd.unwrap() - 0.004).abs() < 1e-9);
            }
            "cursor" => {
                assert_eq!(usage_rows[0].input, 80);
                assert_eq!(usage_rows[0].output, 40);
                assert_eq!(usage_rows[0].cache_read, 400);
                assert_eq!(usage_rows[0].cost_kind.as_deref(), Some("plan"));
                assert!(usage_rows[0].cost_usd.is_none());
            }
            "codex" => {
                assert_eq!(
                    usage_rows[0].input, 40,
                    "codex stores uncached input, not inclusive"
                );
                assert_eq!(usage_rows[0].output, 30);
                assert_eq!(usage_rows[0].cache_read, 160);
                assert_eq!(usage_rows[0].cost_kind.as_deref(), Some("plan"));
                assert!(usage_rows[0].cost_usd.is_none());
                let hit = atr::providers::cache_hit_ratio(
                    "codex",
                    &atr::providers::UsageNumbers {
                        input: usage_rows[0].input as u64,
                        output: usage_rows[0].output as u64,
                        cache_read: usage_rows[0].cache_read as u64,
                        cache_write: usage_rows[0].cache_write as u64,
                        cost_usd: None,
                    },
                )
                .unwrap();
                assert!(
                    (hit - 160.0 / 200.0).abs() < 1e-9,
                    "codex hit {hit} (want 80% = 160/200, not 160/210 from extra cache_write)"
                );
            }
            "opencode" => {
                assert_eq!(usage_rows[0].input, 50);
                assert_eq!(usage_rows[0].output, 10);
                assert_eq!(usage_rows[0].cache_read, 100);
                assert_eq!(usage_rows[0].cost_kind.as_deref(), Some("plan"));
                assert!(usage_rows[0].cost_usd.is_none());
            }
            other => panic!("unexpected provider {other}"),
        }
        if only == "cursor" || only == "codex" || only == "opencode" {
            let status = h.shared.status_json();
            let p = status["providers"]
                .as_array()
                .unwrap()
                .iter()
                .find(|p| p["name"] == only)
                .unwrap();
            assert_eq!(p["usage"], "usage: not reported", "{p}");
            assert_eq!(p["reported_usage"], "none", "{p}");
            assert_eq!(p["reported"]["display"], "usage: not reported", "{p}");
        }
    }
}

#[tokio::test]
async fn fallback_on_rate_limit() {
    let h = Harness::with_providers(
        &[
            ("claude", true, "rate_limit"),
            ("grok", true, "success"),
            ("cursor", false, "success"),
            ("codex", false, "success"),
        ],
        "127.0.0.1:0",
    )
    .await;
    h.submit("demo-20", "please fix");
    h.wait_status("demo-20", &["waiting"], 8000).await;
    let task = h.shared.db.get_task("demo-20").unwrap().unwrap();
    assert_eq!(task.provider.as_deref(), Some("grok"));
    let runs = h.shared.db.list_runs(Some("demo-20")).unwrap();
    assert!(runs
        .iter()
        .any(|r| r.provider == "claude" && r.outcome.as_deref() == Some("limit")));
    assert!(runs
        .iter()
        .any(|r| r.provider == "grok" && r.outcome.as_deref() == Some("success")));
}

#[tokio::test]
async fn queue_does_not_supersede_live_run() {
    let h = Harness::with_providers(
        &[
            ("claude", true, "hold"),
            ("grok", false, "success"),
            ("cursor", false, "success"),
            ("codex", false, "success"),
        ],
        "127.0.0.1:0",
    )
    .await;
    {
        let mut cfg = h.shared.config.write().unwrap();
        for p in &mut cfg.providers {
            p.heartbeat_timeout = Duration::from_secs(8);
            p.health.heartbeat_timeout = Duration::from_secs(8);
            p.health.startup_timeout = Duration::from_secs(8);
        }
    }
    assert_eq!(h.submit("demo-30", "first"), SubmitOutcome::Accepted);
    h.wait_until("demo-30", 8000, |h| {
        h.shared.inflight.lock().unwrap().contains_key("demo-30")
            && h.shared
                .db
                .open_run_for_task("demo-30")
                .ok()
                .flatten()
                .is_some()
    })
    .await;
    let runs = h.shared.db.list_runs(Some("demo-30")).unwrap();
    assert_eq!(runs.len(), 1, "live run should exist before follow-up");
    let first_id = runs[0].id;
    let first_pid = runs[0].pid;
    assert_eq!(h.submit("demo-30", "second"), SubmitOutcome::Queued);
    tokio::time::sleep(Duration::from_millis(150)).await;
    let still = h.shared.db.list_runs(Some("demo-30")).unwrap();
    assert_eq!(still.len(), 1, "must not start a second run while live");
    assert_eq!(still[0].id, first_id);
    assert_eq!(still[0].pid, first_pid);
    assert!(
        h.shared.inflight.lock().unwrap().contains_key("demo-30"),
        "follow-up must not drop the live slot"
    );
    std::fs::write(h.fake_dir.join("claude.release"), "go\n").unwrap();
    h.wait_status("demo-30", &["waiting"], 10000).await;
    let runs = h.shared.db.list_runs(Some("demo-30")).unwrap();
    assert_eq!(
        runs.len(),
        2,
        "expected first + follow-up only, outcomes={:?}",
        runs.iter()
            .map(|r| (r.id, r.outcome.as_deref(), r.provider.as_str()))
            .collect::<Vec<_>>()
    );
    assert_eq!(runs[0].outcome.as_deref(), Some("success"));
    assert_eq!(runs[1].outcome.as_deref(), Some("success"));
    let inv = h.invocations();
    let claude: Vec<_> = inv.iter().filter(|v| v["kind"] == "claude").collect();
    assert!(claude.len() >= 2);
    let first_args = claude[0]["args"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|v| v.as_str())
        .collect::<Vec<_>>();
    assert!(
        first_args
            .windows(2)
            .any(|w| w[0] == "--permission-mode" && w[1] == "bypassPermissions"),
        "headless Claude must pass permission mode, got {first_args:?}"
    );
    let second_args = claude[1]["args"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|v| v.as_str())
        .collect::<Vec<_>>();
    assert!(
        second_args.windows(2).any(|w| w[0] == "--resume"),
        "follow-up must resume, got {second_args:?}"
    );
}

#[tokio::test]
async fn stall_kills_process_group() {
    let h = Harness::with_providers(
        &[
            ("claude", true, "stall"),
            ("grok", false, "success"),
            ("cursor", false, "success"),
            ("codex", false, "success"),
        ],
        "127.0.0.1:0",
    )
    .await;
    h.submit("demo-40", "hang please");
    // first stall retries the same session; second stall exhausts stall_resumes,
    // cools the only enabled provider, and parks the task queued.
    h.wait_until("demo-40", 8000, |h| {
        let Ok(Some(t)) = h.shared.db.get_task("demo-40") else {
            return false;
        };
        if t.status != "queued" {
            return false;
        }
        let runs = h.shared.db.list_runs(Some("demo-40")).unwrap();
        let (cooling, _) = h.shared.db.provider_blocked("claude").unwrap();
        cooling && runs.len() >= 2
    })
    .await;
    let runs = h.shared.db.list_runs(Some("demo-40")).unwrap();
    assert!(
        runs.iter().all(|r| r.outcome.as_deref() == Some("stalled")),
        "{runs:?}"
    );
    assert!(
        runs.len() >= 2,
        "stall is retried once on the same provider"
    );
    let (cooling, degraded) = h.shared.db.provider_blocked("claude").unwrap();
    assert!(cooling, "stall exhaust must apply unhealthy_cooldown");
    assert!(degraded, "stall should mark the provider degraded briefly");
    let inv = h.invocations();
    let claude: Vec<_> = inv.iter().filter(|v| v["kind"] == "claude").collect();
    assert!(claude.len() >= 2);
    let second_args = claude[1]["args"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|v| v.as_str())
        .collect::<Vec<_>>();
    assert!(
        second_args
            .windows(2)
            .any(|w| w[0] == "--resume" && !w[1].is_empty()),
        "stall retry must resume the session from the first event, got {second_args:?}"
    );
    for r in &runs {
        if let Some(pid) = r.pid {
            assert!(
                !atr::db::pid_alive(pid),
                "pid {pid} should be dead after stall kill"
            );
        }
    }
    let orphans = std::fs::read_to_string(h.fake_dir.join("claude.orphans")).unwrap_or_default();
    assert!(
        !orphans.trim().is_empty(),
        "stall fixture must spawn a setsid child"
    );
    for line in orphans.lines() {
        let pid: i64 = line.trim().parse().expect(line);
        assert!(
            !atr::db::pid_alive(pid),
            "setsid orphan {pid} must die with the CLI tree"
        );
    }
}

#[tokio::test]
async fn grok_stall_retry_resumes_generated_session_id() {
    let h = Harness::with_providers(
        &[
            ("claude", false, "success"),
            ("grok", true, "stall"),
            ("cursor", false, "success"),
            ("codex", false, "success"),
        ],
        "127.0.0.1:0",
    )
    .await;
    h.submit("demo-41", "hang grok please");
    h.wait_until("demo-41", 8000, |h| {
        let Ok(Some(t)) = h.shared.db.get_task("demo-41") else {
            return false;
        };
        if t.status != "queued" {
            return false;
        }
        let runs = h.shared.db.list_runs(Some("demo-41")).unwrap();
        let (cooling, _) = h.shared.db.provider_blocked("grok").unwrap();
        cooling && runs.len() >= 2
    })
    .await;
    let runs = h.shared.db.list_runs(Some("demo-41")).unwrap();
    let sid = runs
        .first()
        .and_then(|r| r.session_id.clone())
        .expect("grok session id must be recorded before spawn");
    assert!(
        sid.contains('-') && sid.len() >= 32,
        "generated session should be a uuid, got {sid}"
    );
    let inv = h.invocations();
    let grok: Vec<_> = inv.iter().filter(|v| v["kind"] == "grok").collect();
    assert!(grok.len() >= 2, "stall retry, got {grok:?}");
    let first: Vec<_> = grok[0]["args"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    let second: Vec<_> = grok[1]["args"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    assert!(
        first
            .windows(2)
            .any(|w| w[0] == "--session-id" && w[1] == sid),
        "new grok session must pass --session-id {sid}, got {first:?}"
    );
    assert!(!first.contains(&"--resume"));
    assert!(
        second.windows(2).any(|w| w[0] == "-r" && w[1] == sid),
        "stall retry must fork -r {sid}, got {second:?}"
    );
    assert!(second.contains(&"--fork-session"));
    assert!(
        second.windows(2).any(|w| w[0] == "-s" && w[1] != sid),
        "fork must pass a new -s uuid, got {second:?}"
    );
    assert!(!second.contains(&"--session-id"));
}

#[tokio::test]
async fn unavailable_failovers_and_cools() {
    let h = Harness::with_providers(
        &[
            ("claude", true, "unavailable"),
            ("grok", true, "success"),
            ("cursor", false, "success"),
            ("codex", false, "success"),
        ],
        "127.0.0.1:0",
    )
    .await;
    h.submit("demo-43", "please recover");
    h.wait_status("demo-43", &["waiting"], 8000).await;
    let task = h.shared.db.get_task("demo-43").unwrap().unwrap();
    assert_eq!(task.provider.as_deref(), Some("grok"));
    let runs = h.shared.db.list_runs(Some("demo-43")).unwrap();
    assert!(runs
        .iter()
        .any(|r| { r.provider == "claude" && r.outcome.as_deref() == Some("unavailable") }));
    assert!(runs
        .iter()
        .any(|r| r.provider == "grok" && r.outcome.as_deref() == Some("success")));
    let (cooling, _) = h.shared.db.provider_blocked("claude").unwrap();
    assert!(cooling, "Unavailable must set unhealthy_cooldown");
}

#[tokio::test]
async fn unavailable_only_provider_stays_queued() {
    let h = Harness::with_providers(
        &[
            ("claude", true, "unavailable"),
            ("grok", false, "success"),
            ("cursor", false, "success"),
            ("codex", false, "success"),
        ],
        "127.0.0.1:0",
    )
    .await;
    h.submit("demo-44", "nowhere to go");
    h.wait_until("demo-44", 8000, |h| {
        let Ok(Some(t)) = h.shared.db.get_task("demo-44") else {
            return false;
        };
        if t.status != "queued" {
            return false;
        }
        let runs = h.shared.db.list_runs(Some("demo-44")).unwrap();
        let (cooling, _) = h.shared.db.provider_blocked("claude").unwrap();
        cooling
            && runs
                .iter()
                .any(|r| r.outcome.as_deref() == Some("unavailable"))
    })
    .await;
    assert_ne!(
        h.shared.db.get_task("demo-44").unwrap().unwrap().status,
        "failed"
    );
}

#[tokio::test]
async fn claude_stream_usage_shows_in_status() {
    let h = Harness::with_providers(
        &[
            ("claude", true, "success"),
            ("grok", false, "success"),
            ("cursor", false, "success"),
            ("codex", false, "success"),
        ],
        "127.0.0.1:0",
    )
    .await;
    h.submit("demo-45", "track usage");
    h.wait_status("demo-45", &["waiting"], 8000).await;
    let status = h.shared.status_json();
    let claude = status["providers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == "claude")
        .unwrap();
    assert_eq!(claude["reported_usage"], "claude_stream", "{claude}");
    let usage = claude["usage"].as_str().expect("usage");
    assert!(usage.contains("5h 0.0% used"), "{usage}");
    assert!(usage.contains("7d 3.0% used"), "{usage}");
    assert!(!usage.contains("0.03"), "{usage}");
    let reported = &claude["reported"];
    let windows = reported["windows"].as_array().expect("windows");
    assert!(
        windows
            .iter()
            .any(|w| w["name"] == "5h" && w["percent_used"].as_f64() == Some(0.0)),
        "{reported:?}"
    );
    assert!(
        windows
            .iter()
            .any(|w| w["name"] == "7d" && (w["percent_used"].as_f64().unwrap() - 3.0).abs() < 1e-9),
        "{reported:?}"
    );
    for p in status["providers"].as_array().unwrap() {
        if p["name"] != "claude" {
            assert_eq!(p["usage"], "usage: not reported", "{}", p["name"]);
            assert_eq!(
                p["reported"]["display"], "usage: not reported",
                "{}",
                p["name"]
            );
            if p["name"] == "cursor" || p["name"] == "codex" {
                assert_eq!(p["reported_usage"], "none", "{}", p["name"]);
            }
        }
    }
    let first_updated = h
        .shared
        .db
        .list_provider_usage("claude")
        .unwrap()
        .into_iter()
        .find(|w| w.window == "five_hour")
        .unwrap()
        .updated_at;
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert_eq!(h.submit("demo-45", "second turn"), SubmitOutcome::Accepted);
    h.wait_status("demo-45", &["waiting"], 8000).await;
    assert_eq!(h.shared.db.list_runs(Some("demo-45")).unwrap().len(), 2);
    let again = h
        .shared
        .db
        .list_provider_usage("claude")
        .unwrap()
        .into_iter()
        .find(|w| w.window == "five_hour")
        .unwrap()
        .updated_at;
    assert!(
        again > first_updated,
        "every Claude run (including follow-up turns) must store rate_limit_event; {first_updated} then {again}"
    );
}

#[tokio::test]
async fn grok_billing_probe_shows_balance() {
    let h = Harness::with_providers(
        &[
            ("claude", false, "success"),
            ("grok", true, "success"),
            ("cursor", false, "success"),
            ("codex", false, "success"),
        ],
        "127.0.0.1:0",
    )
    .await;
    h.submit("demo-46", "bill me");
    h.wait_status("demo-46", &["waiting"], 8000).await;
    let status = h.shared.status_json();
    let grok = status["providers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == "grok")
        .unwrap();
    assert_eq!(grok["reported_usage"], "grok_billing");
    let usage = grok["usage"].as_str().expect("usage");
    assert!(usage.contains("weekly 1.0% used"), "{usage}");
    assert!(usage.contains("extra credits $140.32"), "{usage}");
    assert!(!usage.contains("14032"), "{usage}");
    assert!(!usage.contains("0.01"), "{usage}");
    let reported = &grok["reported"];
    let windows = reported["windows"].as_array().expect("windows");
    assert!(
        windows
            .iter()
            .any(|w| w["name"] == "weekly"
                && (w["percent_used"].as_f64().unwrap() - 1.0).abs() < 1e-9),
        "{reported:?}"
    );
    assert_eq!(reported["extra_credits"], "$140.32");
}

#[tokio::test]
async fn grok_billing_refreshed_at_serve_start() {
    let h = Harness::with_providers(
        &[
            ("claude", false, "success"),
            ("grok", true, "success"),
            ("cursor", false, "success"),
            ("codex", false, "success"),
        ],
        "127.0.0.1:0",
    )
    .await;
    let status = h.shared.status_json();
    let grok = status["providers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == "grok")
        .unwrap();
    let usage = grok["usage"].as_str().expect("usage");
    assert!(
        usage.contains("weekly 1.0% used"),
        "daemon start must probe grok billing without a task: {usage}"
    );
    assert!(usage.contains("extra credits $140.32"), "{usage}");
    assert!(!usage.contains("14032"), "{usage}");
}

#[tokio::test]
async fn degraded_clears_after_window_and_on_success() {
    let h = Harness::with_providers(
        &[
            ("claude", true, "success"),
            ("grok", false, "success"),
            ("cursor", false, "success"),
            ("codex", false, "success"),
        ],
        "127.0.0.1:0",
    )
    .await;
    let past = (chrono::Utc::now() - chrono::Duration::minutes(1))
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    h.shared
        .db
        .set_degraded_until("claude", Some(&past))
        .unwrap();
    let status = h.shared.status_json();
    let claude = status["providers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == "claude")
        .unwrap();
    assert_eq!(claude["degraded"], false, "{claude}");
    h.submit("demo-42", "after expired degrade");
    h.wait_status("demo-42", &["waiting"], 8000).await;
    let (_, degraded) = h.shared.db.provider_blocked("claude").unwrap();
    assert!(!degraded);
    let runs = h.shared.db.list_runs(Some("demo-42")).unwrap();
    assert_eq!(runs[0].outcome.as_deref(), Some("success"));
}

#[tokio::test]
async fn config_reload_changes_provider_order() {
    let h = Harness::with_providers(
        &[
            ("claude", false, "success"),
            ("grok", true, "success"),
            ("cursor", false, "success"),
            ("codex", false, "success"),
        ],
        "127.0.0.1:0",
    )
    .await;
    h.submit("demo-50", "via grok");
    h.wait_status("demo-50", &["waiting"], 8000).await;
    assert_eq!(
        h.shared
            .db
            .get_task("demo-50")
            .unwrap()
            .unwrap()
            .provider
            .as_deref(),
        Some("grok")
    );

    let cfg_path = h.shared.config_path.clone();
    let text = std::fs::read_to_string(&cfg_path).unwrap();
    let text = text.replace(
        "name = \"claude\"\nkind = \"claude\"\nenabled = false",
        "name = \"claude\"\nkind = \"claude\"\nenabled = true",
    );
    let text = text.replace(
        "name = \"grok\"\nkind = \"grok\"\nenabled = true",
        "name = \"grok\"\nkind = \"grok\"\nenabled = false",
    );
    std::fs::write(&cfg_path, text).unwrap();

    unsafe {
        libc::kill(std::process::id() as i32, libc::SIGHUP);
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    let names: Vec<_> = h
        .shared
        .config
        .read()
        .unwrap()
        .providers
        .iter()
        .map(|p| (p.name.clone(), p.enabled))
        .collect();
    assert!(
        names.iter().any(|(n, e)| n == "claude" && *e),
        "SIGHUP should enable claude, got {names:?}"
    );
    assert!(
        names.iter().any(|(n, e)| n == "grok" && !*e),
        "SIGHUP should disable grok, got {names:?}"
    );

    h.submit("demo-51", "via claude after reload");
    h.wait_status("demo-51", &["waiting"], 8000).await;
    assert_eq!(
        h.shared
            .db
            .get_task("demo-51")
            .unwrap()
            .unwrap()
            .provider
            .as_deref(),
        Some("claude")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn http_submit_uses_exit_contract_codes() {
    let h = Harness::with_providers(
        &[
            ("claude", true, "success 0.8"),
            ("grok", false, "success"),
            ("cursor", false, "success"),
            ("codex", false, "success"),
        ],
        "127.0.0.1:0",
    )
    .await;
    let url = format!("http://{}/tasks/demo-60/messages", h.addr);
    let first = ureq::post(&url)
        .send_json(serde_json::json!({"body": "one", "source": "relay"}))
        .unwrap();
    assert_eq!(first.status(), 200);
    h.wait_status("demo-60", &["running"], 8000).await;
    let second = ureq::post(&url)
        .send_json(serde_json::json!({"body": "two", "source": "relay"}))
        .unwrap();
    assert_eq!(second.status(), 202);
    let health: serde_json::Value = ureq::get(&format!("http://{}/health", h.addr))
        .call()
        .unwrap()
        .into_json()
        .unwrap();
    assert_eq!(health["ok"], true);
    h.wait_status("demo-60", &["waiting"], 10000).await;

    let page = ureq::get(&format!("http://{}/", h.addr)).call().unwrap();
    assert_eq!(page.status(), 200);
    let html = page.into_string().unwrap();
    assert!(html.contains("dashboard.json"), "{html}");
    assert!(!html.contains("<button"), "{html}");
    assert!(!html.contains("<form"), "{html}");
    let dash: serde_json::Value = ureq::get(&format!("http://{}/dashboard.json", h.addr))
        .call()
        .unwrap()
        .into_json()
        .unwrap();
    assert_eq!(dash["ok"], true);
    assert_eq!(dash["bind"], "127.0.0.1");
    let finished = dash["finished"].as_array().unwrap();
    assert!(
        finished
            .iter()
            .any(|t| t["key"] == "demo-60" && t["result"] == "success"),
        "{dash}"
    );
    assert!(dash["providers"]
        .as_array()
        .unwrap()
        .iter()
        .any(|p| { p["name"] == "claude" && p.get("usage").and_then(|u| u.as_str()).is_some() }));
    assert!(dash["today"].as_array().is_some());
    assert!(dash["logs"].as_array().is_some());
    assert!(dash["savings"]["by_provider"].is_array());
    assert!(dash["savings"]["by_day"].is_array());
    assert!(html.contains("Token savings"), "{html}");
    assert!(html.contains("id=\"period\""), "{html}");
    let month: serde_json::Value =
        ureq::get(&format!("http://{}/dashboard.json?period=month", h.addr))
            .call()
            .unwrap()
            .into_json()
            .unwrap();
    assert_eq!(month["savings"]["period"], "month");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_does_not_cool_down_provider() {
    let h = Harness::with_providers(
        &[
            ("claude", true, "hold"),
            ("grok", false, "success"),
            ("cursor", false, "success"),
            ("codex", false, "success"),
        ],
        "127.0.0.1:0",
    )
    .await;
    h.submit("demo-70", "long run");
    // claim_task sets status=running before start_run fills inflight.run_id.
    // Cancel with run_id 0 cannot finish the live run. Wait until both.
    h.wait_until("demo-70", 8000, |h| {
        let live = h
            .shared
            .inflight
            .lock()
            .unwrap()
            .get("demo-70")
            .is_some_and(|i| i.run_id > 0);
        let running = h
            .shared
            .db
            .get_task("demo-70")
            .ok()
            .flatten()
            .is_some_and(|t| t.status == "running");
        live && running
    })
    .await;
    let task = h.shared.db.get_task("demo-70").unwrap().unwrap();
    assert_eq!(task.status, "running");
    assert_eq!(task.provider.as_deref(), Some("claude"));
    h.shared.cancel("demo-70", true).unwrap();
    h.wait_status("demo-70", &["failed"], 8000).await;
    h.wait_until("demo-70", 8000, |h| {
        h.shared
            .db
            .list_runs(Some("demo-70"))
            .ok()
            .is_some_and(|runs| {
                runs.iter()
                    .any(|r| r.outcome.as_deref() == Some("cancelled"))
            })
    })
    .await;
    let (cooling, degraded) = h.shared.db.provider_blocked("claude").unwrap();
    assert!(!cooling, "cancel must not apply a limit cooldown");
    assert!(!degraded, "cancel must not mark the provider degraded");
}

#[tokio::test]
async fn success_text_mentioning_quota_is_not_a_limit() {
    let h = Harness::with_providers(
        &[
            ("claude", true, "success"),
            ("grok", false, "success"),
            ("cursor", false, "success"),
            ("codex", false, "success"),
        ],
        "127.0.0.1:0",
    )
    .await;
    h.submit("demo-80", "mentions 429 and quota in the result");
    h.wait_status("demo-80", &["waiting"], 8000).await;
    let runs = h.shared.db.list_runs(Some("demo-80")).unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].outcome.as_deref(), Some("success"));
    let (cooling, _) = h.shared.db.provider_blocked("claude").unwrap();
    assert!(!cooling);
}

#[tokio::test]
async fn failover_handoff_includes_diff_and_summary() {
    let h = Harness::with_providers(
        &[
            ("claude", true, "edit_stall"),
            ("grok", true, "success"),
            ("cursor", false, "success"),
            ("codex", false, "success"),
        ],
        "127.0.0.1:0",
    )
    .await;
    h.submit("demo-90", "please add a handoff note to the readme");
    h.wait_status("demo-90", &["waiting"], 10000).await;
    let task = h.shared.db.get_task("demo-90").unwrap().unwrap();
    assert_eq!(task.provider.as_deref(), Some("grok"));
    let wt = std::path::Path::new(task.worktree.as_ref().unwrap());
    let system = std::fs::read_to_string(wt.join(".atr/system.md")).unwrap();
    assert!(
        system.contains("STABLE PREFIX"),
        "stable prefix must live in the CLI system-prompt slot:\n{system}"
    );
    let prompt = std::fs::read_to_string(wt.join(".atr/prompt.md")).unwrap();
    assert!(
        !prompt.contains("STABLE PREFIX"),
        "first run with a slot must not re-send the prefix in the prompt:\n{prompt}"
    );
    let marker = prompt.find("----- atr variable -----").expect("marker");
    let handoff_at = prompt.find("## Provider handoff").expect("handoff heading");
    assert!(marker < handoff_at, "handoff is after the variable marker");
    assert!(prompt[marker..handoff_at].contains("Task key: demo-90"));
    assert!(
        prompt.contains("I added atr-handoff-note"),
        "summary from stored assistant event, got:\n{prompt}"
    );
    assert!(
        prompt.contains("atr-handoff-note") && prompt.contains("README"),
        "diff or status must mention the edited file, got:\n{prompt}"
    );
    assert!(
        prompt.contains("ATR_NEW.md") || prompt.contains("brand-new-untracked"),
        "handoff must include the untracked file, got:\n{prompt}"
    );
    assert!(
        prompt.contains("please add a handoff note"),
        "original task must be in the handoff"
    );
    assert!(
        prompt.contains("printf") || prompt.contains("Commands already run"),
        "captured bash command, got:\n{prompt}"
    );
    let runs = h.shared.db.list_runs(Some("demo-90")).unwrap();
    let grok = runs.iter().find(|r| r.provider == "grok").unwrap();
    assert!(
        grok.handoff
            .as_deref()
            .unwrap_or("")
            .contains("atr-handoff-note"),
        "handoff stored per incoming run: {:?}",
        grok.handoff
    );
    let verbose = atr::app::task_verbose_text(&h.shared, "demo-90").unwrap();
    assert!(verbose.contains("--- handoff (run"));
    assert!(verbose.contains("atr-handoff-note"));
}

#[tokio::test]
async fn lost_session_gets_handoff() {
    let h = Harness::with_providers(
        &[
            ("claude", true, "success"),
            ("grok", false, "success"),
            ("cursor", false, "success"),
            ("codex", false, "success"),
        ],
        "127.0.0.1:0",
    )
    .await;
    h.submit("demo-91", "first turn of the task");
    h.wait_status("demo-91", &["waiting"], 8000).await;
    h.shared
        .db
        .set_provider_session("demo-91", Some("claude"), None)
        .unwrap();
    h.submit("demo-91", "follow-up after the session vanished");
    h.wait_status("demo-91", &["waiting"], 8000).await;
    let task = h.shared.db.get_task("demo-91").unwrap().unwrap();
    let prompt = std::fs::read_to_string(
        std::path::Path::new(task.worktree.as_ref().unwrap()).join(".atr/prompt.md"),
    )
    .unwrap();
    assert!(prompt.contains("## Provider handoff"), "{prompt}");
    assert!(prompt.contains("first turn of the task"), "{prompt}");
    let runs = h.shared.db.list_runs(Some("demo-91")).unwrap();
    assert_eq!(runs.len(), 2);
    assert!(
        runs[1].handoff.as_deref().unwrap_or("").contains("claude"),
        "{:?}",
        runs[1].handoff
    );
}

#[tokio::test]
async fn disabled_provider_does_not_resume_foreign_session() {
    let h = Harness::with_providers(
        &[
            ("claude", true, "success"),
            ("grok", true, "success"),
            ("cursor", false, "success"),
            ("codex", false, "success"),
        ],
        "127.0.0.1:0",
    )
    .await;
    h.submit("demo-92", "start on claude");
    h.wait_status("demo-92", &["waiting"], 8000).await;
    let task = h.shared.db.get_task("demo-92").unwrap().unwrap();
    assert_eq!(task.provider.as_deref(), Some("claude"));
    let claude_sid = task.session_id.clone().expect("claude session");
    {
        let mut cfg = h.shared.config.write().unwrap();
        for p in &mut cfg.providers {
            if p.name == "claude" {
                p.enabled = false;
            }
        }
    }
    h.submit("demo-92", "continue after claude was disabled");
    h.wait_status("demo-92", &["waiting"], 8000).await;
    let task = h.shared.db.get_task("demo-92").unwrap().unwrap();
    assert_eq!(task.provider.as_deref(), Some("grok"));
    let inv = h.invocations();
    let grok: Vec<_> = inv.iter().filter(|v| v["kind"] == "grok").collect();
    assert!(!grok.is_empty(), "grok must run after claude is disabled");
    let args: Vec<_> = grok[0]["args"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    assert!(
        !args
            .windows(2)
            .any(|w| w[0] == "--resume" && w[1] == claude_sid),
        "must not --resume another provider's session {claude_sid}, got {args:?}"
    );
    assert!(
        !args.iter().any(|a| *a == claude_sid),
        "grok argv must not contain claude session {claude_sid}, got {args:?}"
    );
    assert!(
        args.windows(2)
            .any(|w| w[0] == "--session-id" && w[1] != claude_sid),
        "grok should start its own session, got {args:?}"
    );
}

#[tokio::test]
async fn blocked_claude_is_not_success_and_failsover() {
    let h = Harness::with_providers(
        &[
            ("claude", true, "permission_denied"),
            ("grok", true, "success"),
            ("cursor", false, "success"),
            ("codex", false, "success"),
        ],
        "127.0.0.1:0",
    )
    .await;
    h.submit("demo-93", "please edit the readme");
    h.wait_status("demo-93", &["waiting"], 8000).await;
    let task = h.shared.db.get_task("demo-93").unwrap().unwrap();
    assert_eq!(task.provider.as_deref(), Some("grok"));
    let runs = h.shared.db.list_runs(Some("demo-93")).unwrap();
    assert!(
        runs.iter()
            .any(|r| r.provider == "claude" && r.outcome.as_deref() == Some("needs_attention")),
        "blocked Claude must not be logged as success: {runs:?}"
    );
    assert!(
        runs.iter()
            .any(|r| r.provider == "grok" && r.outcome.as_deref() == Some("success")),
        "{runs:?}"
    );
}

#[tokio::test]
async fn shadow_poller_makes_no_gh_writes() {
    let h = Harness::with_providers(
        &[
            ("claude", true, "success"),
            ("grok", false, "success"),
            ("cursor", false, "success"),
            ("codex", false, "success"),
        ],
        "127.0.0.1:0",
    )
    .await;
    let gh = h.install_gh_stub();
    std::fs::write(
        h.fake_dir.join("gh-issues.json"),
        r#"[{"number":42,"title":"shadow me","body":"please fix in shadow"}]"#,
    )
    .unwrap();
    h.enable_shadow(1, &gh);
    atr::shadow::poll_once(&h.shared).await.unwrap();
    let key = "demo-shadow-42";
    let task = h.shared.db.get_task(key).unwrap().expect("shadow task");
    assert!(task.shadow, "poller must mark the task shadow");
    h.wait_status(key, &["waiting"], 8000).await;
    let task = h.shared.db.get_task(key).unwrap().unwrap();
    let prompt = std::fs::read_to_string(
        std::path::Path::new(task.worktree.as_ref().unwrap()).join(".atr/prompt.md"),
    )
    .unwrap();
    assert!(
        prompt.contains("## Shadow mode"),
        "shadow rules must be in the variable prompt:\n{prompt}"
    );
    let log = std::fs::read_to_string(h.fake_dir.join("gh-invocations.log")).unwrap_or_default();
    assert!(
        log.contains("issue list"),
        "poller must list issues, got:\n{log}"
    );
    assert!(
        !h.fake_dir.join("gh-writes.log").exists(),
        "shadow mode must not call gh writes:\n{log}"
    );
    for line in log.lines() {
        let args: Vec<&str> = line.split_whitespace().collect();
        assert!(
            !atr::shadow::gh_is_write(&args),
            "write-shaped gh invocation: {line}"
        );
    }
    let report = atr::shadow::report_text(&h.shared.db, None).unwrap();
    assert!(report.contains(key), "{report}");
    assert!(report.contains("TOTALS"), "{report}");
    assert!(
        report.contains("diffstat"),
        "report table must include a diffstat column:\n{report}"
    );
    let json = atr::shadow::report_json(&h.shared.db, None).unwrap();
    assert!(json.contains(key), "{json}");
    assert!(json.contains("diffstat"), "{json}");
    let src = h.dir.path().join("shadow-src");
    assert!(
        src.join(".git").exists() || src.join(".git").is_file(),
        "poller must create shadow.repo_path from shadow.repo"
    );
    assert_eq!(
        task.repo.as_deref(),
        Some(src.to_str().unwrap()),
        "first poll must store shadow.repo_path"
    );
    let env_dump = std::fs::read_to_string(h.fake_dir.join("claude.env")).unwrap_or_default();
    assert!(
        !env_dump.contains("GH_TOKEN=") || env_dump.contains("GH_TOKEN=\n"),
        "agent must not inherit GH_TOKEN:\n{env_dump}"
    );
    assert!(
        !env_dump.contains("GITHUB_TOKEN=") || env_dump.contains("GITHUB_TOKEN=\n"),
        "agent must not inherit GITHUB_TOKEN:\n{env_dump}"
    );
    let wt = std::path::PathBuf::from(task.worktree.as_ref().unwrap());
    let pushurl = std::process::Command::new("git")
        .current_dir(&wt)
        .args(["config", "--get", "remote.origin.pushurl"])
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&pushurl.stdout).trim(),
        "no_push",
        "shadow worktree must set pushurl=no_push"
    );
    let hook = wt.join(".atr/hooks/pre-push");
    assert!(hook.exists(), "pre-push hook must exist");
}

#[tokio::test]
async fn shadow_follow_up_comment_keeps_repo_path() {
    let h = Harness::with_providers(
        &[
            ("claude", true, "success"),
            ("grok", false, "success"),
            ("cursor", false, "success"),
            ("codex", false, "success"),
        ],
        "127.0.0.1:0",
    )
    .await;
    let gh = h.install_gh_stub();
    std::fs::write(
        h.fake_dir.join("gh-issues.json"),
        r#"[{"number":8,"title":"keep repo","body":"first"}]"#,
    )
    .unwrap();
    h.enable_shadow(1, &gh);
    atr::shadow::poll_once(&h.shared).await.unwrap();
    let key = "demo-shadow-8";
    let first = h.shared.db.get_task(key).unwrap().expect("shadow task");
    let shadow_src = h.dir.path().join("shadow-src");
    let want = shadow_src.to_str().unwrap();
    assert_eq!(first.repo.as_deref(), Some(want));
    std::fs::write(
        h.fake_dir.join("gh-comments.json"),
        r#"[{"id":11,"body":"follow up please","user":{"login":"demo-user","type":"User"}}]"#,
    )
    .unwrap();
    atr::shadow::poll_once(&h.shared).await.unwrap();
    let again = h.shared.db.get_task(key).unwrap().unwrap();
    assert_eq!(
        again.repo.as_deref(),
        Some(want),
        "follow-up comment must not replace shadow.repo_path with the main repo"
    );
    assert_eq!(
        h.shared.db.comment_cursor(key).unwrap().as_deref(),
        Some("11"),
        "comment must be ingested"
    );
}

#[tokio::test]
async fn shadow_respects_max_parallel() {
    let h = Harness::with_providers(
        &[
            ("claude", true, "success 1.2"),
            ("grok", false, "success"),
            ("cursor", false, "success"),
            ("codex", false, "success"),
        ],
        "127.0.0.1:0",
    )
    .await;
    {
        let mut cfg = h.shared.config.write().unwrap();
        cfg.shadow.max_parallel = 1;
        for p in &mut cfg.providers {
            p.heartbeat_timeout = Duration::from_secs(8);
            p.health.heartbeat_timeout = Duration::from_secs(8);
            p.health.startup_timeout = Duration::from_secs(8);
        }
    }
    h.submit_shadow("demo-shadow-1", "first shadow");
    h.wait_status("demo-shadow-1", &["running"], 8000).await;
    h.submit_shadow("demo-shadow-2", "second shadow");
    tokio::time::sleep(Duration::from_millis(250)).await;
    let running = h
        .shared
        .db
        .list_tasks()
        .unwrap()
        .into_iter()
        .filter(|t| t.shadow && t.status == "running")
        .count();
    let inflight = h
        .shared
        .inflight
        .lock()
        .unwrap()
        .values()
        .filter(|i| i.shadow)
        .count();
    assert!(
        running.max(inflight) <= 1,
        "shadow cap is 1, running={running} inflight={inflight}"
    );
    h.wait_status("demo-shadow-1", &["waiting"], 10000).await;
    h.wait_status("demo-shadow-2", &["waiting"], 10000).await;
}

#[tokio::test]
async fn shadow_reaps_leftovers_on_normal_exit() {
    let h = Harness::with_providers(
        &[
            ("claude", true, "leave_orphan"),
            ("grok", false, "success"),
            ("cursor", false, "success"),
            ("codex", false, "success"),
        ],
        "127.0.0.1:0",
    )
    .await;
    h.submit_shadow("demo-shadow-orphan", "leave a child");
    h.wait_status("demo-shadow-orphan", &["waiting"], 8000)
        .await;
    let orphans = std::fs::read_to_string(h.fake_dir.join("claude.orphans")).unwrap_or_default();
    assert!(
        !orphans.trim().is_empty(),
        "fake agent must spawn an orphan"
    );
    for line in orphans.lines() {
        let pid: i64 = line.trim().parse().expect("pid");
        let start = std::time::Instant::now();
        while atr::db::pid_alive(pid) && start.elapsed() < Duration::from_millis(1500) {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(
            !atr::db::pid_alive(pid),
            "leftover pid {pid} must be reaped after a successful shadow run"
        );
    }
}

#[tokio::test]
async fn shadow_strips_tokens_and_blocks_auth_token() {
    let h = Harness::with_providers(
        &[
            ("claude", true, "success"),
            ("grok", false, "success"),
            ("cursor", false, "success"),
            ("codex", false, "success"),
        ],
        "127.0.0.1:0",
    )
    .await;
    let gh = h.install_gh_stub();
    h.enable_shadow(1, &gh);
    std::env::set_var("GH_TOKEN", "atr-shadow-test-token-not-for-agent");
    std::env::set_var("GITHUB_TOKEN", "atr-shadow-test-token-not-for-agent");
    std::env::set_var("ATR_SHADOW_GH_TOKEN", "atr-shadow-gh-token-secret");
    h.submit_shadow("demo-shadow-tok", "no token leak");
    h.wait_status("demo-shadow-tok", &["waiting"], 8000).await;
    let env_dump = std::fs::read_to_string(h.fake_dir.join("claude.env")).unwrap();
    assert!(
        !env_dump.contains("atr-shadow-test-token-not-for-agent"),
        "agent env leaked the token:\n{env_dump}"
    );
    assert!(
        !env_dump.contains("atr-shadow-gh-token-secret"),
        "agent env leaked shadow.gh_token_env:\n{env_dump}"
    );
    assert!(
        !env_dump.contains("ATR_SHADOW_GH_TOKEN=") || env_dump.contains("ATR_SHADOW_GH_TOKEN=\n"),
        "ATR_SHADOW_GH_TOKEN must be stripped:\n{env_dump}"
    );
    let wrap = std::path::PathBuf::from(
        h.shared
            .db
            .get_task("demo-shadow-tok")
            .unwrap()
            .unwrap()
            .worktree
            .unwrap(),
    )
    .join(".atr/bin/gh");
    assert!(wrap.exists(), "shadow gh wrapper");
    let tok = std::process::Command::new(&wrap)
        .args(["auth", "token"])
        .env("ATR_SHADOW_GH_REAL", gh.display().to_string())
        .env_remove("ATR_SHADOW_GH_NESTED")
        .output()
        .unwrap();
    assert!(!tok.status.success(), "gh auth token must be refused");
    let status = std::process::Command::new(&wrap)
        .args(["auth", "status", "--show-token"])
        .env("ATR_SHADOW_GH_REAL", gh.display().to_string())
        .env_remove("ATR_SHADOW_GH_NESTED")
        .output()
        .unwrap();
    assert!(
        !status.status.success(),
        "gh auth status --show-token must be refused"
    );
}

#[tokio::test]
async fn grok_persists_stream_events() {
    let h = Harness::with_providers(
        &[
            ("claude", false, "success"),
            ("grok", true, "success"),
            ("cursor", false, "success"),
            ("codex", false, "success"),
        ],
        "127.0.0.1:0",
    )
    .await;
    h.submit("demo-grok-events", "please persist grok events");
    h.wait_status("demo-grok-events", &["waiting"], 8000).await;
    let runs = h.shared.db.list_runs(Some("demo-grok-events")).unwrap();
    assert_eq!(runs.len(), 1);
    let events = h.shared.db.events_for_run(runs[0].id).unwrap();
    assert!(
        !events.is_empty(),
        "Grok stream events must be stored like other providers"
    );
    assert!(
        events
            .iter()
            .any(|e| e.raw.contains("sessionId") || e.raw.contains("text")),
        "{events:?}"
    );
}

#[tokio::test]
async fn shadow_report_includes_diffstat() {
    let h = Harness::with_providers(
        &[
            ("claude", true, "edit_success"),
            ("grok", false, "success"),
            ("cursor", false, "success"),
            ("codex", false, "success"),
        ],
        "127.0.0.1:0",
    )
    .await;
    h.submit_shadow("demo-shadow-diff", "edit something");
    h.wait_status("demo-shadow-diff", &["waiting"], 8000).await;
    let rows = h.shared.db.list_shadow_results(None).unwrap();
    assert_eq!(rows.len(), 1);
    assert!(
        !rows[0].diffstat.is_empty() && rows[0].diffstat != "-",
        "expected a short diffstat, got {:?}",
        rows[0].diffstat
    );
    assert!(
        rows[0].diffstat.contains("ATR_A.md")
            && rows[0].diffstat.contains("ATR_B.md")
            && rows[0].diffstat.contains("untracked"),
        "diffstat must list and mark every untracked file, got {:?}",
        rows[0].diffstat
    );
    let report = atr::shadow::report_text(&h.shared.db, None).unwrap();
    assert!(report.contains("diffstat"), "{report}");
    assert!(
        report.contains("ATR_A") && report.contains('…'),
        "table truncates the multi-file diffstat with an ellipsis:\n{report}"
    );
    for line in report.lines() {
        if line.starts_with("demo-shadow-diff") || line.contains("demo-shadow-diff") {
            assert!(
                line.contains('…') || line.split_whitespace().count() >= 8,
                "row should stay a single aligned line:\n{line}"
            );
        }
    }
    let json = atr::shadow::report_json(&h.shared.db, None).unwrap();
    assert!(json.contains("diffstat"), "{json}");
    assert!(
        json.contains("ATR_A.md") && json.contains("ATR_B.md") && json.contains("untracked"),
        "JSON must list every untracked file and mark them:\n{json}"
    );
}

#[tokio::test]
async fn shadow_repo_path_is_cloned_when_missing() {
    let h = Harness::with_providers(
        &[
            ("claude", true, "success"),
            ("grok", false, "success"),
            ("cursor", false, "success"),
            ("codex", false, "success"),
        ],
        "127.0.0.1:0",
    )
    .await;
    let gh = h.install_gh_stub();
    let dest = h.dir.path().join("auto-clone");
    assert!(!dest.exists());
    {
        let mut cfg = h.shared.config.write().unwrap();
        cfg.shadow.enabled = true;
        cfg.shadow.repo = "example/demo-1".into();
        cfg.shadow.labels = vec!["for:atr".into()];
        cfg.shadow.repo_path = dest.clone();
        cfg.github.cli = gh.display().to_string();
    }
    std::fs::write(h.fake_dir.join("clone-src"), h.repo.display().to_string()).unwrap();
    std::fs::write(
        h.fake_dir.join("gh-issues.json"),
        r#"[{"number":7,"title":"clone me","body":"from missing path"}]"#,
    )
    .unwrap();
    atr::shadow::poll_once(&h.shared).await.unwrap();
    assert!(
        dest.join(".git").exists() || dest.join(".git").is_file(),
        "missing shadow.repo_path must be cloned from shadow.repo"
    );
    let task = h
        .shared
        .db
        .get_task("demo-shadow-7")
        .unwrap()
        .expect("shadow task");
    assert_eq!(
        task.repo.as_deref(),
        Some(dest.to_str().unwrap()),
        "poller must point the task at the shadow clone"
    );
}

#[tokio::test]
async fn verify_fails_then_passes_on_same_session() {
    let h = Harness::with_providers(
        &[
            ("claude", true, "success"),
            ("grok", false, "success"),
            ("cursor", false, "success"),
            ("codex", false, "success"),
        ],
        "127.0.0.1:0",
    )
    .await;
    let script =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake-verify.sh");
    let dest = h.dir.path().join("bin").join("verify.sh");
    std::fs::copy(&script, &dest).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let count = h.fake_dir.join("verify.count");
    {
        let mut cfg = h.shared.config.write().unwrap();
        cfg.verify.enabled = true;
        cfg.verify.max_retries = 1;
        cfg.verify.timeout = Duration::from_secs(5);
        cfg.verify.commands = vec![format!("{} {}", dest.display(), count.display())];
    }
    h.submit("demo-verify-1", "please make the checks pass");
    h.wait_status("demo-verify-1", &["waiting"], 12000).await;
    let task = h.shared.db.get_task("demo-verify-1").unwrap().unwrap();
    assert_eq!(task.status, "waiting");
    assert_eq!(task.provider.as_deref(), Some("claude"));
    let runs = h.shared.db.list_runs(Some("demo-verify-1")).unwrap();
    assert_eq!(
        runs.len(),
        2,
        "one follow-up turn after verify fail: {runs:?}"
    );
    assert_eq!(
        runs[0].outcome.as_deref(),
        Some("verify_failed"),
        "{:?}",
        runs[0].outcome
    );
    assert_eq!(runs[1].outcome.as_deref(), Some("success"));
    assert_eq!(runs[0].session_id, runs[1].session_id);
    let first = h.shared.db.verify_checks_for_run(runs[0].id).unwrap();
    assert_eq!(first.len(), 1);
    assert!(!first[0].passed, "{first:?}");
    let second = h.shared.db.verify_checks_for_run(runs[1].id).unwrap();
    assert_eq!(second.len(), 1);
    assert!(second[0].passed, "{second:?}");
    let verbose = atr::app::task_verbose_text(&h.shared, "demo-verify-1").unwrap();
    assert!(verbose.contains("--- verify (run"), "{verbose}");
    assert!(
        verbose.contains("FAIL") || verbose.contains("verify failed"),
        "{verbose}"
    );
    assert!(verbose.contains("[pass]"), "{verbose}");
}

#[tokio::test]
async fn verify_exhausted_retries_needs_attention() {
    let h = Harness::with_providers(
        &[
            ("claude", true, "success"),
            ("grok", false, "success"),
            ("cursor", false, "success"),
            ("codex", false, "success"),
        ],
        "127.0.0.1:0",
    )
    .await;
    {
        let mut cfg = h.shared.config.write().unwrap();
        cfg.verify.enabled = true;
        cfg.verify.max_retries = 1;
        cfg.verify.timeout = Duration::from_secs(5);
        cfg.verify.commands = vec!["sh -c 'echo always-fail; exit 1'".into()];
    }
    h.submit("demo-verify-2", "this will not pass checks");
    h.wait_status("demo-verify-2", &["needs_attention"], 12000)
        .await;
    let runs = h.shared.db.list_runs(Some("demo-verify-2")).unwrap();
    assert_eq!(runs.len(), 2, "{runs:?}");
    assert!(
        runs.iter()
            .all(|r| r.outcome.as_deref() == Some("verify_failed")),
        "{runs:?}"
    );
}

#[tokio::test]
async fn shadow_verify_record_does_not_follow_up() {
    let h = Harness::with_providers(
        &[
            ("claude", true, "success"),
            ("grok", false, "success"),
            ("cursor", false, "success"),
            ("codex", false, "success"),
        ],
        "127.0.0.1:0",
    )
    .await;
    {
        let mut cfg = h.shared.config.write().unwrap();
        cfg.verify.enabled = true;
        cfg.verify.max_retries = 1;
        cfg.verify.timeout = Duration::from_secs(5);
        cfg.verify.commands = vec!["sh -c 'echo shadow-verify-fail; exit 1'".into()];
        cfg.shadow.verify_mode = atr::config::ShadowVerifyMode::Record;
    }
    h.submit_shadow("demo-shadow-verify", "shadow check");
    h.wait_status("demo-shadow-verify", &["waiting"], 12000)
        .await;
    let runs = h.shared.db.list_runs(Some("demo-shadow-verify")).unwrap();
    assert_eq!(
        runs.len(),
        1,
        "record mode must not send a follow-up: {runs:?}"
    );
    assert_eq!(runs[0].outcome.as_deref(), Some("verify_failed"));
    let rows = h.shared.db.list_shadow_results(None).unwrap();
    assert_eq!(rows.len(), 1);
    assert!(
        rows[0].verify.contains("FAIL") || rows[0].outcome == "verify_failed",
        "{:?}",
        rows[0].verify
    );
}

fn commit_failing_test(repo: &std::path::Path) {
    std::fs::create_dir_all(repo.join("tests")).unwrap();
    std::fs::write(repo.join("tests/check.sh"), "#!/bin/sh\nexit 1\n").unwrap();
    std::process::Command::new("git")
        .current_dir(repo)
        .args(["add", "tests/check.sh"])
        .status()
        .unwrap();
    std::process::Command::new("git")
        .current_dir(repo)
        .args(["commit", "-m", "failing test"])
        .status()
        .unwrap();
}

fn enable_protect_verify(h: &Harness) {
    let mut cfg = h.shared.config.write().unwrap();
    cfg.verify.enabled = true;
    cfg.verify.protect_tests = true;
    cfg.verify.only_listed_repos = false;
    cfg.verify.max_retries = 1;
    cfg.verify.timeout = Duration::from_secs(5);
    cfg.verify.commands = vec!["sh tests/check.sh".into()];
    cfg.checklist.enabled = true;
}

#[tokio::test]
async fn protect_tests_needs_attention_and_keeps_edits() {
    let h = Harness::with_providers(
        &[
            ("claude", true, "weaken_tests"),
            ("grok", false, "success"),
            ("cursor", false, "success"),
            ("codex", false, "success"),
        ],
        "127.0.0.1:0",
    )
    .await;
    commit_failing_test(&h.repo);
    enable_protect_verify(&h);
    h.submit("demo-protect-1", "please make tests pass");
    h.wait_status("demo-protect-1", &["needs_attention"], 12000)
        .await;
    let task = h.shared.db.get_task("demo-protect-1").unwrap().unwrap();
    let wt = std::path::PathBuf::from(task.worktree.as_ref().unwrap());
    let tests = std::fs::read_to_string(wt.join("tests/check.sh")).unwrap();
    assert!(
        tests.contains("AGENT"),
        "agent test edits must not be lost:\n{tests}"
    );
    let runs = h.shared.db.list_runs(Some("demo-protect-1")).unwrap();
    assert_eq!(runs.len(), 1, "{runs:?}");
    assert_eq!(runs[0].outcome.as_deref(), Some("tests_modified"));
    let checks = h.shared.db.verify_checks_for_run(runs[0].id).unwrap();
    assert!(
        checks
            .iter()
            .any(|c| c.command == "(protect_tests)" && !c.passed),
        "{checks:?}"
    );
    let verbose = atr::app::task_verbose_text(&h.shared, "demo-protect-1").unwrap();
    assert!(verbose.contains("tests_modified"), "{verbose}");
    assert!(verbose.contains("tests/check.sh"), "{verbose}");
    assert!(verbose.contains("checklist"), "{verbose}");
    assert!(verbose.contains("weaken the failing test"), "{verbose}");
    let prompt = std::fs::read_to_string(wt.join(".atr/prompt.md")).unwrap();
    let (head, tail) = atr::prompt::split_variable(&prompt).unwrap();
    assert!(tail.contains("## Checklist"), "{tail}");
    assert!(!head.contains("Checklist"), "{head}");
    let snap = h.shared.db.run_checklist(runs[0].id).unwrap();
    assert!(
        snap.as_deref()
            .is_some_and(|s| s.contains("weaken the failing test")),
        "{snap:?}"
    );
}

#[tokio::test]
async fn protect_tests_opt_out_flag() {
    let h = Harness::with_providers(
        &[
            ("claude", true, "weaken_tests"),
            ("grok", false, "success"),
            ("cursor", false, "success"),
            ("codex", false, "success"),
        ],
        "127.0.0.1:0",
    )
    .await;
    commit_failing_test(&h.repo);
    enable_protect_verify(&h);
    h.submit_tests_ok("demo-protect-2", "legit test fix");
    h.wait_status("demo-protect-2", &["waiting"], 12000).await;
    let runs = h.shared.db.list_runs(Some("demo-protect-2")).unwrap();
    assert_eq!(runs.len(), 1, "{runs:?}");
    assert_eq!(runs[0].outcome.as_deref(), Some("success"));
}

#[tokio::test]
async fn protect_tests_opt_out_label() {
    let h = Harness::with_providers(
        &[
            ("claude", true, "weaken_tests"),
            ("grok", false, "success"),
            ("cursor", false, "success"),
            ("codex", false, "success"),
        ],
        "127.0.0.1:0",
    )
    .await;
    commit_failing_test(&h.repo);
    enable_protect_verify(&h);
    h.submit("demo-protect-3", "labelled test fix");
    h.shared
        .db
        .set_task_labels("demo-protect-3", "for:atr,atr:tests-ok")
        .unwrap();
    h.wait_status("demo-protect-3", &["waiting"], 12000).await;
    let runs = h.shared.db.list_runs(Some("demo-protect-3")).unwrap();
    assert_eq!(runs.len(), 1, "{runs:?}");
    assert_eq!(runs[0].outcome.as_deref(), Some("success"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn telegram_fake_api_status_confirm_and_acl() {
    tokio::time::timeout(
        Duration::from_secs(15),
        telegram_fake_api_status_confirm_and_acl_inner(),
    )
    .await
    .expect("telegram fake-api e2e exceeded 15s (likely a blocked wait)");
}

async fn telegram_fake_api_status_confirm_and_acl_inner() {
    use axum::body::Bytes;
    use axum::extract::State;
    use axum::http::Uri;
    use axum::{Json, Router};
    use serde_json::{json, Value};
    use std::sync::{Arc, Mutex};
    use tokio::net::TcpListener;

    atr::telegram::clear_notify_batch();

    #[derive(Clone)]
    struct FakeTg {
        token: String,
        updates: Arc<Mutex<Vec<Value>>>,
        sent: Arc<Mutex<Vec<Value>>>,
        hits: Arc<Mutex<Vec<String>>>,
    }

    fn query_offset(uri: &Uri) -> i64 {
        uri.query()
            .unwrap_or("")
            .split('&')
            .find_map(|p| p.strip_prefix("offset="))
            .and_then(|s| s.parse().ok())
            .unwrap_or(0)
    }

    async fn telegram_api(uri: Uri, State(fake): State<FakeTg>, body: Bytes) -> Json<Value> {
        fake.hits.lock().unwrap().push(uri.to_string());
        let prefix = format!("/bot{}", fake.token);
        let path = uri.path();
        if !path.starts_with(&prefix) {
            return Json(json!({"ok": false, "error_code": 401}));
        }
        let rest = &path[prefix.len()..];
        if rest == "/getUpdates" {
            let offset = query_offset(&uri);
            let mut store = fake.updates.lock().unwrap();
            let mut out = Vec::new();
            store.retain(|u| {
                let id = u.get("update_id").and_then(|v| v.as_i64()).unwrap_or(0);
                if id >= offset {
                    out.push(u.clone());
                    false
                } else {
                    true
                }
            });
            return Json(json!({"ok": true, "result": out}));
        }
        if rest == "/sendMessage" {
            let payload: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
            fake.sent.lock().unwrap().push(payload);
            return Json(json!({"ok": true, "result": {"message_id": 1}}));
        }
        Json(json!({"ok": false, "error_code": 404}))
    }

    let token = "e2e-tg-token";
    let fake = FakeTg {
        token: token.into(),
        updates: Arc::new(Mutex::new(vec![
            json!({"update_id": 10, "message": {"chat": {"id": 99}, "text": "/status"}}),
            json!({"update_id": 11, "message": {"chat": {"id": 42}, "text": "/status"}}),
        ])),
        sent: Arc::new(Mutex::new(Vec::new())),
        hits: Arc::new(Mutex::new(Vec::new())),
    };
    let app = Router::new()
        .fallback(telegram_api)
        .with_state(fake.clone());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });

    std::env::set_var("ATR_E2E_TG_TOKEN", token);
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("p.md"), "p\n").unwrap();
    let dbp = dir.path().join("s.db");
    let cfg_txt = format!(
        r#"
listen = "127.0.0.1:0"
state_db = "{db}"
worktree_root = "{root}"
repo_path = "{root}"
system_prompt_file = "{root}/p.md"
display_timezone = "UTC"
[github]
poll_interval = "60s"
[telegram]
enabled = true
bot_token_env = "ATR_E2E_TG_TOKEN"
allowed_chat_ids = [42]
prefix = "atr:"
notify_interval = "1s"
daily_summary = ""
api_base = "http://{addr}"
[[provider]]
name = "claude"
kind = "claude"
cli = "/bin/true"
"#,
        db = dbp.display(),
        root = dir.path().display(),
        addr = addr,
    );
    let raw: atr::config::RawConfig = toml::from_str(&cfg_txt).unwrap();
    let parsed = atr::config::Config::from_raw(raw).unwrap();
    let shared = atr::Shared::from_config(dir.path().join("c.toml"), parsed).unwrap();
    {
        let tg = shared.config.read().unwrap().telegram.clone();
        assert!(tg.enabled);
        assert_eq!(tg.api_base, format!("http://{addr}"));
        assert_eq!(atr::telegram::bot_token(&tg).as_deref(), Some(token));
        assert_eq!(atr::telegram::allowed_chats(&tg), vec![42]);
    }
    assert!(shared.status_json().get("quota_reset").is_some());
    assert_eq!(shared.status_json()["timezone"], "UTC");

    let bot = shared.clone();
    tokio::spawn(async move {
        atr::telegram::bot_loop(bot).await;
    });

    let deadline = std::time::Instant::now() + Duration::from_secs(8);
    loop {
        let sent = fake.sent.lock().unwrap().clone();
        if sent.iter().any(|m| {
            m.get("chat_id") == Some(&json!(42))
                && m.get("text")
                    .and_then(|t| t.as_str())
                    .is_some_and(|t| t.contains("intake on") && t.contains("Providers"))
        }) {
            break;
        }
        if std::time::Instant::now() > deadline {
            let hits = fake.hits.lock().unwrap().clone();
            panic!("timed out waiting for /status reply: sent={sent:?} hits={hits:?}");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    {
        let sent = fake.sent.lock().unwrap().clone();
        assert!(
            sent.iter().all(|m| m.get("chat_id") != Some(&json!(99))),
            "unknown chat must be ignored: {sent:?}"
        );
        for m in &sent {
            let text = m.get("text").and_then(|t| t.as_str()).unwrap_or("");
            assert!(!text.contains(token), "{text}");
            assert!(!text.contains("ATR_E2E_TG_TOKEN"), "{text}");
        }
    }

    fake.updates.lock().unwrap().push(json!({
        "update_id": 12,
        "message": {"chat": {"id": 42}, "text": "/pause"}
    }));
    let deadline = std::time::Instant::now() + Duration::from_secs(8);
    let data = loop {
        let sent = fake.sent.lock().unwrap().clone();
        if let Some(m) = sent.iter().rev().find(|m| {
            m.get("text")
                .and_then(|t| t.as_str())
                .is_some_and(|t| t.contains("Pause intake"))
                && m.get("reply_markup").is_some()
        }) {
            break m["reply_markup"]["inline_keyboard"][0][0]["callback_data"]
                .as_str()
                .unwrap()
                .to_string();
        }
        if std::time::Instant::now() > deadline {
            panic!("timed out waiting for pause confirm: {sent:?}");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    fake.updates.lock().unwrap().push(json!({
        "update_id": 13,
        "callback_query": {"data": data, "message": {"chat": {"id": 42}}}
    }));
    let deadline = std::time::Instant::now() + Duration::from_secs(8);
    loop {
        if shared
            .intake_paused
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            break;
        }
        let sent = fake.sent.lock().unwrap().clone();
        if sent.iter().any(|m| {
            m.get("text")
                .and_then(|t| t.as_str())
                .is_some_and(|t| t.contains("intake paused"))
        }) {
            break;
        }
        if std::time::Instant::now() > deadline {
            panic!("timed out waiting for pause to apply: {sent:?}");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(shared
        .intake_paused
        .load(std::sync::atomic::Ordering::Relaxed));
    std::mem::forget(dir);
}

#[tokio::test]
async fn telegram_assistant_uses_fake_cli_and_counts_usage() {
    let h = Harness::with_providers(
        &[
            ("claude", true, "assistant"),
            ("grok", true, "success"),
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
    }
    let upd = serde_json::json!({"message":{"chat":{"id":42},"text":"how do I add kimi?"}});
    let out = atr::telegram::handle_update(&h.shared, &upd).unwrap();
    assert!(
        out[0].text.starts_with(atr::telegram::INTERIM_PREFIX),
        "{}",
        out[0].text
    );
    h.wait_status("tg42", &["waiting"], 12000).await;
    let task = h.shared.db.get_task("tg42").unwrap().unwrap();
    assert_eq!(task.provider.as_deref(), Some("claude"));
    assert!(task.session_id.is_some(), "{task:?}");
    let runs = h.shared.db.list_runs(Some("tg42")).unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].outcome.as_deref(), Some("success"));
    let usage = h.shared.db.list_usage_for_run(runs[0].id).unwrap();
    assert_eq!(usage.len(), 1, "assistant usage counted like any run");
    assert_eq!(usage[0].input, 20);
    let wt = std::path::Path::new(task.worktree.as_ref().unwrap());
    let system = std::fs::read_to_string(wt.join(".atr/system.md")).unwrap();
    assert!(system.contains("## Telegram assistant"), "{system}");
    assert!(
        system.contains("atr skill") || system.contains("# atr"),
        "{system}"
    );
    let prompt = std::fs::read_to_string(wt.join(".atr/prompt.md")).unwrap();
    assert!(prompt.contains("how do I add kimi?"), "{prompt}");
    assert!(prompt.contains("atr snapshot"), "{prompt}");
    assert!(prompt.contains("secrets redacted"), "{prompt}");
    assert!(!prompt.contains("tok-secret"));
    let replies = atr::telegram::assistant_reply(&h.shared, "tg42", runs[0].id, "success");
    assert!(
        replies
            .iter()
            .any(|o| o.text.contains("Kimi") || o.text.contains("kimi")),
        "{replies:?}"
    );
    let confirm = replies
        .iter()
        .find(|o| o.markup.is_some())
        .expect("confirm for proposed command");
    assert!(
        confirm.text.contains("→") || confirm.text.contains("->"),
        "{}",
        confirm.text
    );
    assert!(confirm.text.contains("grok"), "{}", confirm.text);

    let sid = task.session_id.clone();
    let follow = serde_json::json!({"message":{"chat":{"id":42},"text":"why did smoke fail?"}});
    let out = atr::telegram::handle_update(&h.shared, &follow).unwrap();
    assert!(
        out[0].text.starts_with(atr::telegram::INTERIM_PREFIX),
        "{}",
        out[0].text
    );
    h.wait_status("tg42", &["waiting"], 12000).await;
    let task2 = h.shared.db.get_task("tg42").unwrap().unwrap();
    assert_eq!(task2.session_id, sid, "one conversation resumes");
    assert_eq!(h.shared.db.list_runs(Some("tg42")).unwrap().len(), 2);
}
