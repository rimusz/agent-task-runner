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
