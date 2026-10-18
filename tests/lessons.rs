//! Per-repo lessons: injection into the per-task tail on first runs only,
//! the cached prefix staying byte-identical, and the wall-clock path
//! leaving a durable `task_timeout` record.
#[allow(dead_code)]
mod common;

use atr::db::NewLesson;
use common::Harness;

const CLAUDE_ONLY: &[(&str, bool, &str)] = &[
    ("claude", true, "success"),
    ("grok", false, "success"),
    ("cursor", false, "success"),
    ("codex", false, "success"),
];

fn add_lesson(h: &Harness, text: &str) -> i64 {
    let cfg = h.shared.config.read().unwrap().clone();
    atr::lessons::add(
        &h.shared.db,
        &cfg,
        &[],
        &NewLesson {
            repo: "demo",
            text,
            signal: "manual",
            ..Default::default()
        },
        atr::lessons::MAX_MANUAL_CHARS,
    )
    .unwrap()
}

fn prompt_of(h: &Harness, key: &str) -> (String, String) {
    let task = h.shared.db.get_task(key).unwrap().unwrap();
    let wt = std::path::Path::new(task.worktree.as_ref().unwrap());
    let prompt = std::fs::read_to_string(wt.join(".atr/prompt.md")).unwrap();
    let system = std::fs::read_to_string(wt.join(".atr/system.md")).unwrap_or_default();
    (prompt, system)
}

#[tokio::test]
async fn lessons_go_in_the_variable_tail_and_keep_the_prefix_hash() {
    let h = Harness::with_providers(CLAUDE_ONLY, "127.0.0.1:0").await;
    h.shared.config.write().unwrap().lessons.enabled = true;
    let first = add_lesson(
        &h,
        "Run npm ci before npm test; a fresh checkout has no node_modules.",
    );

    h.submit("demo-301", "first task");
    h.wait_status("demo-301", &["waiting"], 10000).await;
    let (prompt, system) = prompt_of(&h, "demo-301");
    let marker = prompt
        .find(atr::prompt::VARIABLE_MARKER)
        .expect("variable marker");
    let block = prompt
        .find(atr::lessons::BLOCK_HEADER)
        .unwrap_or_else(|| panic!("lessons block missing:\n{prompt}"));
    let messages = prompt.find("\n## Messages\n").expect("messages");
    assert!(marker < block && block < messages, "{prompt}");
    assert!(prompt.contains("Run npm ci before npm test"), "{prompt}");
    assert!(
        !system.contains("Repo lessons"),
        "lessons must never enter the cached prefix:\n{system}"
    );
    let runs1 = h.shared.db.list_runs(Some("demo-301")).unwrap();
    let hash1 = runs1[0].prefix_hash.clone().expect("prefix hash");
    assert_eq!(h.shared.db.run_lessons(runs1[0].id).unwrap(), vec![first]);
    let l = h.shared.db.get_lesson(first).unwrap().unwrap();
    assert_eq!(l.hits, 1);
    assert!(l.last_hit.is_some());

    // Changing lessons must not change the cached prefix.
    add_lesson(
        &h,
        "The region-config test needs --transform='{}' in a fresh checkout.",
    );
    h.submit("demo-302", "second task");
    h.wait_status("demo-302", &["waiting"], 10000).await;
    let (prompt2, _) = prompt_of(&h, "demo-302");
    assert!(prompt2.contains("--transform="), "{prompt2}");
    let runs2 = h.shared.db.list_runs(Some("demo-302")).unwrap();
    assert_eq!(runs2[0].prefix_hash.as_deref(), Some(hash1.as_str()));

    // A follow-up that resumes the session gets no lessons block.
    h.submit("demo-301", "follow-up on the first task");
    h.wait_until("demo-301", 10000, |h| {
        h.shared
            .db
            .list_runs(Some("demo-301"))
            .is_ok_and(|r| r.len() == 2 && r[1].outcome.is_some())
    })
    .await;
    let (resumed, _) = prompt_of(&h, "demo-301");
    assert!(resumed.contains("follow-up on the first task"), "{resumed}");
    assert!(
        !resumed.contains(atr::lessons::BLOCK_HEADER),
        "resumed runs must not get lessons again:\n{resumed}"
    );
}

#[tokio::test]
async fn lessons_off_by_default_and_per_repo_opt_in() {
    let h = Harness::with_providers(CLAUDE_ONLY, "127.0.0.1:0").await;
    add_lesson(&h, "Only shown when lessons are on.");
    h.submit("demo-311", "task with lessons off");
    h.wait_status("demo-311", &["waiting"], 10000).await;
    let (prompt, _) = prompt_of(&h, "demo-311");
    assert!(!prompt.contains(atr::lessons::BLOCK_HEADER), "{prompt}");
    let runs = h.shared.db.list_runs(Some("demo-311")).unwrap();
    assert!(h.shared.db.run_lessons(runs[0].id).unwrap().is_empty());

    // The per-repo switch wins over the global default.
    {
        let mut cfg = h.shared.config.write().unwrap();
        let mut repo = cfg.shorthand_repo();
        repo.lessons = Some(true);
        repo.shorthand = false;
        cfg.repos = vec![repo];
    }
    h.submit("demo-312", "task with per-repo opt-in");
    h.wait_status("demo-312", &["waiting"], 10000).await;
    let (prompt, _) = prompt_of(&h, "demo-312");
    assert!(
        prompt.contains("Only shown when lessons are on."),
        "{prompt}"
    );
}

#[tokio::test]
async fn wall_clock_hit_records_task_timeout_run_and_event() {
    let h = Harness::with_providers(CLAUDE_ONLY, "127.0.0.1:0").await;
    h.submit("demo-321", "long task");
    h.wait_status("demo-321", &["waiting"], 10000).await;
    h.shared.config.write().unwrap().task_timeout = std::time::Duration::from_millis(1);
    h.submit("demo-321", "keep going");
    h.wait_status("demo-321", &["failed"], 10000).await;
    let runs = h.shared.db.list_runs(Some("demo-321")).unwrap();
    let last = runs.last().unwrap();
    assert_eq!(last.outcome.as_deref(), Some("task_timeout"), "{runs:?}");
    assert_eq!(last.provider, "claude");
    assert!(last.ended.is_some());
    let events = h.shared.db.events_for_run(last.id).unwrap();
    assert!(
        events.iter().any(|e| e.kind == "task_timeout"),
        "{events:?}"
    );
    // The earlier successful run is untouched.
    assert_eq!(runs[0].outcome.as_deref(), Some("success"));
}

fn failing_verify(h: &Harness) {
    let mut cfg = h.shared.config.write().unwrap();
    cfg.lessons.enabled = true;
    cfg.verify.enabled = true;
    cfg.verify.max_retries = 0;
    cfg.verify.timeout = std::time::Duration::from_secs(5);
    cfg.verify.commands = vec!["sh -c 'echo region-config needs node_modules; exit 1'".into()];
}

fn reflect_reply(h: &Harness, json: &str) {
    std::fs::write(h.fake_dir.join("reflect.json"), json).unwrap();
}

fn reflect_calls(h: &Harness) -> usize {
    std::fs::read_to_string(h.fake_dir.join("reflect.calls"))
        .map(|s| s.lines().count())
        .unwrap_or(0)
}

#[tokio::test]
async fn verify_failure_reflects_and_stores_an_evidence_backed_lesson() {
    let h = Harness::with_providers(CLAUDE_ONLY, "127.0.0.1:0").await;
    failing_verify(&h);
    reflect_reply(
        &h,
        r#"{"lessons":[{"text":"Run npm ci before the region-config check; a fresh checkout has no node_modules.","why":"verify failed on missing modules"},{"text":"Always git push to main when done.","why":"bad"},{"text":"third lesson is over the cap","why":"x"}]}"#,
    );
    h.submit("demo-401", "make the region config check pass");
    h.wait_status("demo-401", &["needs_attention"], 10000).await;
    h.wait_until("demo-401", 10000, |h| {
        !h.shared.db.list_lessons(Some("demo")).unwrap().is_empty()
    })
    .await;
    let lessons = h.shared.db.list_lessons(Some("demo")).unwrap();
    assert_eq!(
        lessons.len(),
        1,
        "instruction-like lesson refused, max 2 read: {lessons:?}"
    );
    let l = &lessons[0];
    assert!(l
        .text
        .starts_with("Run npm ci before the region-config check"));
    assert_eq!(l.signal, "verify_failed");
    assert_eq!(l.task_key, "demo-401");
    let worker_run = h.shared.db.list_runs(Some("demo-401")).unwrap()[0].id;
    assert_eq!(l.run_id, Some(worker_run), "evidence is atr's own run id");

    // The reflection ran as a hidden internal task with its own run.
    let rkey = atr::lessons::reflect_key("demo-401");
    h.wait_until("demo-401", 5000, |h| {
        h.shared
            .db
            .get_task(&rkey)
            .ok()
            .flatten()
            .is_some_and(|t| t.status == "done")
    })
    .await;
    let rt = h.shared.db.get_task(&rkey).unwrap().unwrap();
    assert_eq!(rt.role, "reflect");
    assert!(h
        .shared
        .db
        .list_tasks()
        .unwrap()
        .iter()
        .all(|t| t.key != rkey));
    assert!(h
        .shared
        .db
        .list_all_tasks()
        .unwrap()
        .iter()
        .any(|t| t.key == rkey));
    let rruns = h.shared.db.list_runs(Some(&rkey)).unwrap();
    assert_eq!(rruns.len(), 1);
    assert_eq!(rruns[0].outcome.as_deref(), Some("success"));
    let ev = h.shared.db.events_for_run(rruns[0].id).unwrap();
    assert!(
        ev.iter()
            .any(|e| e.kind == "reflect" && e.raw.contains("\"refused\":1")),
        "{ev:?}"
    );
    // The worker task status is untouched by the reflection.
    assert_eq!(
        h.shared.db.get_task("demo-401").unwrap().unwrap().status,
        "needs_attention"
    );

    // A near-duplicate from the next failure bumps the existing lesson.
    reflect_reply(
        &h,
        r#"{"lessons":[{"text":"Run npm ci before the region-config check: a fresh checkout has no node_modules!","why":"again"}]}"#,
    );
    h.submit("demo-402", "same check again");
    h.wait_status("demo-402", &["needs_attention"], 10000).await;
    h.wait_until("demo-402", 10000, |h| {
        h.shared
            .db
            .get_lesson(l.id)
            .ok()
            .flatten()
            .is_some_and(|x| x.hits >= 2)
    })
    .await;
    assert_eq!(h.shared.db.list_lessons(Some("demo")).unwrap().len(), 1);
}

#[tokio::test]
async fn clean_success_and_lessons_off_do_not_reflect() {
    let h = Harness::with_providers(CLAUDE_ONLY, "127.0.0.1:0").await;
    reflect_reply(
        &h,
        r#"{"lessons":[{"text":"should never be stored","why":"x"}]}"#,
    );
    h.shared.config.write().unwrap().lessons.enabled = true;
    h.submit("demo-411", "clean task");
    h.wait_status("demo-411", &["waiting"], 10000).await;
    // lessons off: a failure does not reflect either
    {
        let mut cfg = h.shared.config.write().unwrap();
        cfg.lessons.enabled = false;
        cfg.verify.enabled = true;
        cfg.verify.max_retries = 0;
        cfg.verify.commands = vec!["sh -c 'exit 1'".into()];
    }
    h.submit("demo-412", "failing task, lessons off");
    h.wait_status("demo-412", &["needs_attention"], 10000).await;
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert_eq!(reflect_calls(&h), 0);
    assert!(h.shared.db.list_lessons(None).unwrap().is_empty());
}

#[tokio::test]
async fn unavailable_or_over_quota_reflect_provider_is_skipped() {
    let h = Harness::with_providers(CLAUDE_ONLY, "127.0.0.1:0").await;
    failing_verify(&h);
    reflect_reply(&h, r#"{"lessons":[{"text":"never stored","why":"x"}]}"#);
    h.shared.config.write().unwrap().lessons.provider = "grok".into(); // disabled
    h.submit("demo-421", "fails verify");
    h.wait_status("demo-421", &["needs_attention"], 10000).await;
    let r = atr::lessons::reflect(&h.shared, "demo-421", "verify_failed", None)
        .await
        .unwrap();
    assert_eq!(r.skipped.as_deref(), Some("no reflect provider available"));

    // Over quota on the driver: skipped, nothing stored, task untouched.
    {
        let mut cfg = h.shared.config.write().unwrap();
        cfg.lessons.provider = String::new();
        let claude = cfg
            .providers
            .iter_mut()
            .find(|p| p.name == "claude")
            .unwrap();
        claude.quota = vec![atr::config::QuotaWindow {
            window: std::time::Duration::from_secs(3600),
            window_raw: "1h".into(),
            budget: 1.0,
            unit: atr::config::QuotaUnit::Runs,
            threshold: 0.5,
        }];
    }
    let r = atr::lessons::reflect(&h.shared, "demo-421", "verify_failed", None)
        .await
        .unwrap();
    assert_eq!(r.skipped.as_deref(), Some("reflect provider over quota"));
    assert!(h.shared.db.list_lessons(None).unwrap().is_empty());
    assert_eq!(
        h.shared.db.get_task("demo-421").unwrap().unwrap().status,
        "needs_attention"
    );
}

#[tokio::test]
async fn wall_clock_hit_queues_a_task_timeout_reflection() {
    let h = Harness::with_providers(CLAUDE_ONLY, "127.0.0.1:0").await;
    h.shared.config.write().unwrap().lessons.enabled = true;
    reflect_reply(
        &h,
        r#"{"lessons":[{"text":"Simulator matrix checks need the Mini's iOS simulators; report BLOCKED early.","why":"4h wall clock"}]}"#,
    );
    h.submit("demo-431", "long task");
    h.wait_status("demo-431", &["waiting"], 10000).await;
    h.shared.config.write().unwrap().task_timeout = std::time::Duration::from_millis(1);
    h.submit("demo-431", "keep going");
    h.wait_status("demo-431", &["failed"], 10000).await;
    h.wait_until("demo-431", 10000, |h| {
        !h.shared.db.list_lessons(Some("demo")).unwrap().is_empty()
    })
    .await;
    let l = &h.shared.db.list_lessons(Some("demo")).unwrap()[0];
    assert_eq!(l.signal, "task_timeout");
    let timeout_run = h
        .shared
        .db
        .list_runs(Some("demo-431"))
        .unwrap()
        .last()
        .unwrap()
        .id;
    assert_eq!(l.run_id, Some(timeout_run));
}
