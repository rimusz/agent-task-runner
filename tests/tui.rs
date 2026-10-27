//! `atr tui` / `atr ask` run the engine in-process (no `atr serve`) with fake CLIs.
use atr::{tui, Shared, SubmitOpts};
use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;
use tempfile::TempDir;

fn atr() -> Command {
    Command::new(env!("CARGO_BIN_EXE_atr"))
}

/// Shared + fake CLIs, **without** starting HTTP/`atr serve`.
async fn no_serve(providers: &[(&str, bool, &str)]) -> (TempDir, Arc<Shared>, std::path::PathBuf) {
    let dir = TempDir::new().unwrap();
    let fake_dir = dir.path().join("fake");
    fs::create_dir_all(&fake_dir).unwrap();
    let bin = dir.path().join("bin");
    fs::create_dir_all(&bin).unwrap();
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake-agent.sh");
    for name in ["claude", "grok", "cursor-agent", "codex", "opencode"] {
        let dest = bin.join(name);
        fs::copy(&script, &dest).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&dest, fs::Permissions::from_mode(0o755)).unwrap();
        }
    }
    for (name, _, beh) in providers {
        let cli = match *name {
            "cursor" => "cursor-agent",
            other => other,
        };
        fs::write(fake_dir.join(format!("{cli}.behavior")), beh).unwrap();
    }
    let repo = dir.path().join("repo");
    atr::worktree::init_sample_repo(&repo).unwrap();
    let prompt = dir.path().join("system-prompt.md");
    fs::write(&prompt, "STABLE PREFIX\n").unwrap();
    let db = dir.path().join("state.db");
    let wt = dir.path().join("work");
    let cfg_path = dir.path().join("config.toml");
    let mut text = format!(
        r#"
listen = "127.0.0.1:0"
state_db = "{db}"
worktree_root = "{wt}"
repo_path = "{repo}"
system_prompt_file = "{prompt}"
task_key_prefix = "demo"
[planner]
mode = "off"
[github]
enabled = false
[telegram]
enabled = false
"#,
        db = db.display(),
        wt = wt.display(),
        repo = repo.display(),
        prompt = prompt.display(),
    );
    for (name, enabled, _) in providers {
        text.push_str(&provider_block(name, *enabled, &bin));
    }
    fs::write(&cfg_path, text).unwrap();
    let cfg = atr::load_config(&cfg_path).unwrap();
    let shared = Shared::from_config(cfg_path.clone(), cfg).unwrap();
    (dir, shared, cfg_path)
}

fn provider_block(name: &str, enabled: bool, bin: &Path) -> String {
    let (kind, cli) = match name {
        "cursor" => ("cursor", "cursor-agent"),
        other => (other, other),
    };
    format!(
        r#"
[[provider]]
name = "{name}"
kind = "{kind}"
enabled = {enabled}
cli = "{cli}"
model = "test-model"
max_parallel = 1
run_timeout = "8s"
heartbeat_timeout = "2s"
[provider.health]
startup_timeout = "2s"
heartbeat_timeout = "2s"
"#,
        name = name,
        kind = kind,
        enabled = enabled,
        cli = bin.join(cli).display(),
    )
}

fn opts(prompt: &str, provider: Option<&str>, session: Option<&str>) -> tui::TuiOpts {
    tui::TuiOpts {
        prompt: Some(prompt.into()),
        provider: provider.map(|s| s.to_string()),
        session: session.map(|s| s.to_string()),
        ..tui::TuiOpts::default()
    }
}

#[tokio::test]
async fn oneshot_runs_in_process_without_serve() {
    let (_dir, shared, _cfg) = no_serve(&[
        ("claude", true, "success"),
        ("grok", false, "success"),
        ("cursor", false, "success"),
        ("codex", false, "success"),
    ])
    .await;
    let code = tui::run_oneshot(shared.clone(), opts("what is 2+2?", None, None))
        .await
        .unwrap();
    assert_eq!(code, std::process::ExitCode::SUCCESS);
    let task = shared.db.get_task("cli-default").unwrap().unwrap();
    assert_eq!(task.provider.as_deref(), Some("claude"));
    assert!(tui::is_tui_key(&task.key));
    let runs = shared.db.list_runs(Some("cli-default")).unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].outcome.as_deref(), Some("success"));
    let footer = tui::footer_line(&shared, "cli-default");
    assert!(footer.contains("cli-default"), "{footer}");
    assert!(footer.contains("claude"), "{footer}");
}

#[tokio::test]
async fn model_slash_is_atr_local_for_claude() {
    let (_dir, shared, _cfg) = no_serve(&[
        ("claude", true, "success"),
        ("grok", false, "success"),
        ("cursor", false, "success"),
        ("codex", false, "success"),
    ])
    .await;
    assert_eq!(tui::classify_line("/model"), "model");
    assert_eq!(tui::classify_line("/model sonnet"), "model");
    assert_eq!(tui::classify_line("/"), "slash-menu");
    assert_eq!(
        tui::classify_interrupt(),
        "quit",
        "Ctrl+C must exit atr, not cancel-turn-and-stay"
    );
    let bare = tui::preview_line(&shared, "cli-default", "/").unwrap();
    assert!(
        !bare.starts_with("turn"),
        "bare / must not enqueue a turn: {bare}"
    );
    let kind = tui::preview_line(&shared, "cli-default", "/model").unwrap();
    assert!(kind.starts_with("model-picker"), "{kind}");
    assert!(
        shared.db.list_runs(Some("cli-default")).unwrap().is_empty(),
        "bare /model must not enqueue a turn"
    );
    let out = tui::preview_line(&shared, "cli-default", "/model sonnet").unwrap();
    assert!(out.starts_with("note"), "{out}");
    assert!(out.contains("sonnet"), "{out}");
    assert!(
        shared.db.list_runs(Some("cli-default")).unwrap().is_empty(),
        "/model <id> must not enqueue a turn"
    );
    tui::set_session_model(&shared, "cli-default", "sonnet").unwrap();
    assert_eq!(
        tui::session_model(&shared, "cli-default").as_deref(),
        Some("sonnet")
    );
    let footer = tui::footer_line(&shared, "cli-default");
    let strip = tui::composer_status_line(&shared, "cli-default");
    assert!(footer.contains("sonnet"), "{footer}");
    assert!(
        strip.contains("sonnet"),
        "under-input strip must stay in sync with footer model: strip={strip} footer={footer}"
    );
    assert_eq!(
        tui::preview_line(&shared, "cli-default", "/compact").unwrap(),
        "turn"
    );
}

#[tokio::test]
async fn model_slash_lists_real_grok_ids_not_prose() {
    let (_dir, shared, _cfg) = no_serve(&[
        ("claude", true, "success"),
        ("grok", true, "success"),
        ("cursor", false, "success"),
        ("codex", false, "success"),
    ])
    .await;
    // Last run is claude so a last-run-first footer would disagree with
    // /model after the session moves to grok.
    let code = tui::run_oneshot(shared.clone(), opts("warmup", Some("claude"), None))
        .await
        .unwrap();
    assert_eq!(code, std::process::ExitCode::SUCCESS);
    assert_eq!(shared.db.list_runs(Some("cli-default")).unwrap().len(), 1);

    let switch = tui::preview_line(&shared, "cli-default", "/provider grok").unwrap();
    assert!(switch.starts_with("note"), "{switch}");
    assert!(switch.contains("grok"), "{switch}");
    assert_eq!(
        tui::session_provider_now(&shared, "cli-default").as_deref(),
        Some("grok")
    );

    let footer = tui::footer_line(&shared, "cli-default");
    let strip = tui::composer_status_line(&shared, "cli-default");
    assert!(footer.contains("grok /"), "{footer}");
    assert!(
        strip.starts_with("grok /"),
        "strip must follow session provider: strip={strip} footer={footer}"
    );

    let list = tui::preview_line(&shared, "cli-default", "/model").unwrap();
    assert!(list.starts_with("model-picker"), "{list}");
    assert!(
        list.contains("Models for grok"),
        "header must match session provider in footer: {list}\nfooter={footer}"
    );
    assert_eq!(
        shared.db.list_runs(Some("cli-default")).unwrap().len(),
        1,
        "bare /model must not enqueue a turn"
    );
    for want in [
        "grok-4.7",
        "grok-4.7-build-fast",
        "grok-4.6",
        "grok-4.5",
        "cline-pass-glm-5.2",
        "cursor-composer-2.5",
        "deepseek-v4-flash",
        "cursor-grok-4.7",
    ] {
        assert!(list.contains(want), "missing {want} in {list}");
    }
    for bad in ["You", "Default", "Available"] {
        let leaked = list.lines().any(|line| {
            line.split_whitespace()
                .any(|tok| tok.eq_ignore_ascii_case(bad))
        });
        assert!(!leaked, "prose token {bad:?} leaked into {list}");
    }

    let set = tui::preview_line(&shared, "cli-default", "/model grok-4.7").unwrap();
    assert!(set.starts_with("note"), "{set}");
    assert!(set.contains("grok-4.7"), "{set}");
    assert_eq!(
        tui::session_model(&shared, "cli-default").as_deref(),
        Some("grok-4.7")
    );
    let footer = tui::footer_line(&shared, "cli-default");
    let strip = tui::composer_status_line(&shared, "cli-default");
    assert!(footer.contains("grok / grok-4.7"), "{footer}");
    assert!(
        strip.starts_with("grok / grok-4.7"),
        "under-input strip must match footer session provider/model: strip={strip} footer={footer}"
    );
    assert!(
        footer.ends_with(&strip) || footer.contains(&strip),
        "footer and strip share the same chrome: strip={strip} footer={footer}"
    );
    assert_eq!(
        shared.db.list_runs(Some("cli-default")).unwrap().len(),
        1,
        "/model <id> must not enqueue a turn"
    );

    let list = tui::preview_line(&shared, "cli-default", "/model").unwrap();
    let num = list
        .lines()
        .find_map(|line| {
            let mut parts = line.split_whitespace();
            let n = parts.next()?.parse::<usize>().ok()?;
            let name = parts.next()?;
            (name == "grok-4.7-build-fast").then_some(n)
        })
        .unwrap_or_else(|| panic!("grok-4.7-build-fast not numbered in {list}"));
    let by_num = tui::preview_line(&shared, "cli-default", &format!("/model {num}")).unwrap();
    assert!(by_num.starts_with("note"), "{by_num}");
    assert_eq!(
        tui::session_model(&shared, "cli-default").as_deref(),
        Some("grok-4.7-build-fast")
    );
    let footer = tui::footer_line(&shared, "cli-default");
    let strip = tui::composer_status_line(&shared, "cli-default");
    assert!(footer.contains("grok / grok-4.7-build-fast"), "{footer}");
    assert!(
        strip.starts_with("grok / grok-4.7-build-fast"),
        "strip must follow /model pick: strip={strip} footer={footer}"
    );
    assert_eq!(shared.db.list_runs(Some("cli-default")).unwrap().len(), 1);
}

#[tokio::test]
async fn model_slash_lists_cursor_ids_not_available_header() {
    let (_dir, shared, _cfg) = no_serve(&[
        ("claude", false, "success"),
        ("grok", false, "success"),
        ("cursor", true, "success"),
        ("codex", false, "success"),
    ])
    .await;
    let switch = tui::preview_line(&shared, "cli-default", "/provider cursor").unwrap();
    assert!(switch.starts_with("note"), "{switch}");
    let list = tui::preview_line(&shared, "cli-default", "/model").unwrap();
    assert!(list.starts_with("model-picker"), "{list}");
    assert!(list.contains("Models for cursor"), "{list}");
    assert!(list.contains("composer-2.5"), "{list}");
    assert!(list.contains("auto"), "{list}");
    assert!(
        !list.contains("Available models")
            && !list.contains("Composer 2.5")
            && !list.contains("Auto (default)"),
        "cursor header/display names must not be picker rows: {list}"
    );
}

#[tokio::test]
async fn model_slash_claude_lists_full_ids_not_only_aliases() {
    let (_dir, shared, _cfg) = no_serve(&[
        ("claude", true, "success"),
        ("grok", false, "success"),
        ("cursor", false, "success"),
        ("codex", false, "success"),
    ])
    .await;
    let list = tui::preview_line(&shared, "cli-default", "/model").unwrap();
    assert!(list.starts_with("model-picker"), "{list}");
    for want in [
        "claude-sonnet-5-5",
        "claude-opus-5-5",
        "claude-haiku-5-5",
        "claude-fable-5-1",
    ] {
        assert!(list.contains(want), "missing full id {want} in {list}");
    }
    let numbered: Vec<&str> = list
        .lines()
        .filter_map(|l| {
            let mut parts = l.split_whitespace();
            let _n = parts.next()?.parse::<usize>().ok()?;
            parts.next()
        })
        .collect();
    assert!(
        numbered.iter().any(|id| id.starts_with("claude-")),
        "Claude /model must list real ids, not only short aliases: {list}"
    );
    assert!(
        numbered.iter().any(|id| *id == "sonnet"),
        "short aliases stay secondary: {list}"
    );
    assert!(
        !list.contains("Available models") && !list.contains("Usage:"),
        "claude help prose must not be picker rows: {list}"
    );
}

#[tokio::test]
async fn session_model_is_isolated_per_provider() {
    let (_dir, shared, _cfg) = no_serve(&[
        ("claude", true, "success"),
        ("grok", false, "success"),
        ("cursor", true, "success"),
        ("codex", false, "success"),
    ])
    .await;
    let pin = tui::preview_line(&shared, "cli-default", "/provider cursor").unwrap();
    assert!(pin.starts_with("note"), "{pin}");
    assert_eq!(
        tui::session_provider_now(&shared, "cli-default").as_deref(),
        Some("cursor")
    );

    let set_a = tui::preview_line(&shared, "cli-default", "/model composer-2.5").unwrap();
    assert!(set_a.starts_with("note"), "{set_a}");
    assert_eq!(
        tui::session_model(&shared, "cli-default").as_deref(),
        Some("composer-2.5")
    );
    let strip_a = tui::composer_status_line(&shared, "cli-default");
    assert!(
        strip_a.starts_with("cursor / composer-2.5"),
        "strip={strip_a}"
    );
    let code = tui::run_oneshot(
        shared.clone(),
        opts("warmup last-run", Some("cursor"), None),
    )
    .await
    .unwrap();
    assert_eq!(code, std::process::ExitCode::SUCCESS);

    let switch_b = tui::preview_line(&shared, "cli-default", "/provider claude").unwrap();
    assert!(switch_b.starts_with("note"), "{switch_b}");
    assert_eq!(
        tui::session_provider_now(&shared, "cli-default").as_deref(),
        Some("claude")
    );
    assert_ne!(
        tui::session_model(&shared, "cli-default").as_deref(),
        Some("composer-2.5")
    );
    let strip_b = tui::composer_status_line(&shared, "cli-default");
    assert!(strip_b.starts_with("claude /"), "strip={strip_b}");
    assert!(
        !strip_b.contains("composer-2.5"),
        "foreign session model leaked into strip: {strip_b}"
    );

    let list_b = tui::preview_line(&shared, "cli-default", "/model").unwrap();
    assert!(list_b.starts_with("model-picker"), "{list_b}");
    assert!(list_b.contains("Models for claude"), "{list_b}");
    let session_rows: Vec<&str> = list_b.lines().filter(|l| l.contains("session")).collect();
    assert!(
        session_rows
            .iter()
            .all(|l| !l.contains("composer-2.5") && !l.contains("gpt-6.1-sol")),
        "foreign id must not be collect_models session/current: {list_b}"
    );
    let current = list_b.lines().find(|l| {
        l.split_whitespace()
            .next()
            .is_some_and(|n| n.parse::<usize>().is_ok())
            && l.contains('*')
    });
    if let Some(cur) = current {
        assert!(
            !cur.contains("composer-2.5") && !cur.contains("gpt-6.1-sol"),
            "foreign id marked current: {cur}\n{list_b}"
        );
    }

    let set_b = tui::preview_line(&shared, "cli-default", "/model claude-sonnet-5-5").unwrap();
    assert!(set_b.starts_with("note"), "{set_b}");
    assert_eq!(
        tui::session_model(&shared, "cli-default").as_deref(),
        Some("claude-sonnet-5-5")
    );
    let strip_b = tui::composer_status_line(&shared, "cli-default");
    assert!(
        strip_b.starts_with("claude / claude-sonnet-5-5"),
        "strip={strip_b}"
    );

    let back_a = tui::preview_line(&shared, "cli-default", "/provider cursor").unwrap();
    assert!(back_a.starts_with("note"), "{back_a}");
    assert_eq!(
        tui::session_model(&shared, "cli-default").as_deref(),
        Some("composer-2.5")
    );
    let strip_back = tui::composer_status_line(&shared, "cli-default");
    assert!(
        strip_back.starts_with("cursor / composer-2.5"),
        "A→B→A must restore A's model: strip={strip_back}"
    );

    let back_b = tui::preview_line(&shared, "cli-default", "/provider claude").unwrap();
    assert!(back_b.starts_with("note"), "{back_b}");
    assert_eq!(
        tui::session_model(&shared, "cli-default").as_deref(),
        Some("claude-sonnet-5-5")
    );
    let strip_b2 = tui::composer_status_line(&shared, "cli-default");
    assert!(
        strip_b2.starts_with("claude / claude-sonnet-5-5"),
        "A→B→A→B must keep B's model: strip={strip_b2}"
    );
}

#[tokio::test]
async fn model_slash_codex_lists_more_than_config_row() {
    let (_dir, shared, _cfg) = no_serve(&[
        ("claude", false, "success"),
        ("grok", false, "success"),
        ("cursor", false, "success"),
        ("codex", true, "success"),
    ])
    .await;
    let switch = tui::preview_line(&shared, "cli-default", "/provider codex").unwrap();
    assert!(switch.starts_with("note"), "{switch}");
    let list = tui::preview_line(&shared, "cli-default", "/model").unwrap();
    assert!(list.starts_with("model-picker"), "{list}");
    assert!(list.contains("gpt-6.1-sol"), "{list}");
    assert!(list.contains("gpt-6-luna"), "{list}");
    let ids = list
        .lines()
        .filter(|l| {
            l.split_whitespace()
                .next()
                .is_some_and(|n| n.parse::<usize>().is_ok())
        })
        .count();
    assert!(
        ids > 1,
        "codex /model must not be a single config row: {list}"
    );
    assert!(!list.contains("Available models"), "{list}");
}

#[test]
fn composer_path_line_tildes_home() {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/home/tester".into());
    let under = std::path::PathBuf::from(&home).join("Projects/github/rimusz");
    assert_eq!(tui::composer_path_line(&under), "~/Projects/github/rimusz");
    assert_eq!(tui::composer_path_line(std::path::Path::new(&home)), "~");
    assert_eq!(
        tui::composer_path_line(std::path::Path::new("/opt/src")),
        "/opt/src"
    );
}

#[tokio::test]
async fn oneshot_rejects_missing_prompt() {
    let (_dir, shared, _cfg) = no_serve(&[
        ("claude", true, "success"),
        ("grok", false, "success"),
        ("cursor", false, "success"),
        ("codex", false, "success"),
    ])
    .await;
    let err = tui::run_oneshot(
        shared,
        tui::TuiOpts {
            prompt: None,
            require_prompt: true,
            ..tui::TuiOpts::default()
        },
    )
    .await
    .unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("requires a prompt"), "{msg}");
    assert!(msg.contains("atr tui"), "{msg}");
}

#[tokio::test]
async fn ask_fails_when_no_enabled_provider() {
    let (_dir, shared, _cfg) = no_serve(&[
        ("claude", false, "success"),
        ("grok", false, "success"),
        ("cursor", false, "success"),
        ("codex", false, "success"),
    ])
    .await;
    let err = tui::run_oneshot(shared, opts("hello", None, None))
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("No enabled provider"), "{err}");
    assert!(err.contains("providers detect"), "{err}");
}

#[tokio::test]
async fn atr_tui_provider_prefers_named_provider() {
    let (_dir, shared, _cfg) = no_serve(&[
        ("claude", true, "success"),
        ("grok", true, "success"),
        ("cursor", false, "success"),
        ("codex", false, "success"),
    ])
    .await;
    tui::run(shared.clone(), opts("hello", Some("grok"), Some("work")))
        .await
        .unwrap();
    let task = shared.db.get_task("cli-work").unwrap().unwrap();
    assert_eq!(task.key, "cli-work");
    assert_eq!(task.provider.as_deref(), Some("grok"));
    assert_ne!(task.provider.as_deref(), Some("claude"));
}

#[tokio::test]
async fn session_provider_switch_uses_existing_handoff() {
    let (_dir, shared, _cfg) = no_serve(&[
        ("claude", true, "success"),
        ("grok", true, "success"),
        ("cursor", false, "success"),
        ("codex", false, "success"),
    ])
    .await;
    tui::run(shared.clone(), opts("first", Some("claude"), None))
        .await
        .unwrap();
    let sid = shared.db.task_session("cli-default", "grok").ok().flatten();
    shared
        .db
        .set_provider_session("cli-default", Some("grok"), sid.as_deref())
        .unwrap();
    shared.db.set_provider_hint("cli-default", "grok").unwrap();
    tui::run(shared.clone(), opts("second", None, None))
        .await
        .unwrap();
    let runs = shared.db.list_runs(Some("cli-default")).unwrap();
    assert_eq!(runs.len(), 2, "{runs:?}");
    assert_eq!(runs[0].provider, "claude");
    assert_eq!(runs[1].provider, "grok");
    assert!(
        runs[1].handoff.as_deref().is_some_and(|h| !h.is_empty()),
        "switch should write a handoff brief: {:?}",
        runs[1].handoff
    );
}

#[tokio::test]
async fn tui_providers_list_beats_global_order() {
    let (_dir, shared, _cfg) = no_serve(&[
        ("claude", true, "success"),
        ("grok", true, "success"),
        ("cursor", false, "success"),
        ("codex", false, "success"),
    ])
    .await;
    {
        let mut cfg = shared.config.write().unwrap();
        cfg.tui.providers = vec!["grok".into(), "claude".into()];
    }
    tui::run(shared.clone(), opts("list please", None, None))
        .await
        .unwrap();
    let task = shared.db.get_task("cli-default").unwrap().unwrap();
    assert_eq!(task.provider.as_deref(), Some("grok"));
}

#[test]
fn session_key_helpers() {
    assert_eq!(tui::tui_task_key("default").unwrap(), "cli-default");
    assert!(tui::is_tui_key("cli-default"));
    assert!(!tui::is_tui_key("tg42"));
}

#[tokio::test]
async fn serve_dispatch_skips_tui_session() {
    let (_dir, shared, _cfg) = no_serve(&[
        ("claude", true, "success"),
        ("grok", false, "success"),
        ("cursor", false, "success"),
        ("codex", false, "success"),
    ])
    .await;
    let enqueue = |key: &str, body: &str| {
        shared
            .submit_with(
                key,
                body,
                "cli",
                None,
                None,
                false,
                true,
                SubmitOpts {
                    workspace_kind: Some("none".into()),
                    output_kind: Some("reply".into()),
                    output_spec: None,
                    source_kind: Some("cli".into()),
                    title: Some(key.into()),
                    allow_empty_repo: true,
                    provider_hint: Some("claude".into()),
                    needs: Vec::new(),
                },
            )
            .unwrap()
    };
    enqueue("cli-hold", "tui turn");
    enqueue("demo-1", "worker turn");
    assert!(atr::scheduler::serve_dispatches("demo-1"));
    assert!(!atr::scheduler::serve_dispatches("cli-hold"));

    atr::scheduler::dispatch_available(&shared).await.unwrap();
    wait_status(&shared, "demo-1", &["waiting", "done"], 8000).await;
    let tui = shared.db.get_task("cli-hold").unwrap().unwrap();
    assert_eq!(tui.status, "queued", "serve must not start cli-* sessions");
    assert!(shared.db.list_runs(Some("cli-hold")).unwrap().is_empty());

    atr::scheduler::dispatch_key(&shared, "cli-hold")
        .await
        .unwrap();
    wait_status(&shared, "cli-hold", &["waiting", "done"], 8000).await;
    let runs = shared.db.list_runs(Some("cli-hold")).unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].outcome.as_deref(), Some("success"));
}

async fn wait_status(shared: &Shared, key: &str, want: &[&str], ms: u64) {
    let start = std::time::Instant::now();
    loop {
        if let Ok(Some(t)) = shared.db.get_task(key) {
            if want.iter().any(|w| t.status == *w) {
                return;
            }
        }
        if start.elapsed() > std::time::Duration::from_millis(ms) {
            let st = shared
                .db
                .get_task(key)
                .ok()
                .flatten()
                .map(|t| t.status)
                .unwrap_or_else(|| "missing".into());
            panic!("timeout waiting for {key} in {want:?}, last={st}");
        }
        tokio::time::sleep(std::time::Duration::from_millis(40)).await;
    }
}

#[test]
fn bare_atr_launches_tui() {
    let (dir, cfg) = setup_bin_cfg();
    let out = atr()
        .env("ATR_CONFIG", &cfg)
        .env("HOME", dir.path().join("home"))
        .arg("--help")
        .output()
        .unwrap();
    assert!(out.status.success());
    let help = String::from_utf8_lossy(&out.stdout);
    assert!(help.contains("tui"), "{help}");
    assert!(help.contains("ask"), "{help}");
    assert!(help.contains("serve"), "{help}");
    assert!(!help.contains("ATR_DEFAULT"), "{help}");
    assert!(!help.contains("ATR_TUI_PROVIDER"), "{help}");

    let out = atr()
        .env("ATR_CONFIG", &cfg)
        .env("HOME", dir.path().join("home"))
        .args([
            "tui",
            "status please",
            "--session",
            "bin",
            "--provider",
            "grok",
        ])
        .output()
        .unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.status.success(), "{text}");
    assert!(text.contains("cli-bin"), "{text}");
    assert!(text.contains("grok"), "{text}");

    let mut child = atr()
        .env("ATR_CONFIG", &cfg)
        .env("HOME", dir.path().join("home"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    {
        let mut stdin = child.stdin.take().unwrap();
        writeln!(stdin, "hello from bare atr").unwrap();
    }
    let out = child.wait_with_output().unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.status.success(), "{text}");
    assert!(text.contains("cli-default"), "{text}");
}

#[test]
fn line_mode_slashes_are_not_unknown_command() {
    let (dir, cfg) = setup_bin_cfg();
    let mut child = atr()
        .env("ATR_CONFIG", &cfg)
        .env("HOME", dir.path().join("home"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    {
        let mut stdin = child.stdin.take().unwrap();
        writeln!(stdin, "/help").unwrap();
        writeln!(stdin, "/provider").unwrap();
        writeln!(stdin, "/model").unwrap();
        writeln!(stdin, "/compact").unwrap();
        writeln!(stdin, "/quit").unwrap();
    }
    let out = child.wait_with_output().unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.status.success(), "{text}");
    assert!(!text.contains("unknown command"), "{text}");
    assert!(
        !text.contains("built-in Claude") && !text.contains("nothing for me to execute"),
        "bare /model must not be forwarded as Claude chat: {text}"
    );
    assert!(text.contains("Chat"), "{text}");
    assert!(
        text.contains("Provider CLI") || text.contains("/cli"),
        "{text}"
    );
    assert!(
        text.contains("Providers") || text.contains("/provider"),
        "{text}"
    );
    assert!(
        text.contains("test-model") || text.contains("--model") || text.contains("/model"),
        "{text}"
    );
}

#[test]
fn atr_ask_oneshot_success() {
    let (dir, cfg) = setup_bin_cfg();
    let out = atr()
        .env("ATR_CONFIG", &cfg)
        .env("HOME", dir.path().join("home"))
        .args([
            "ask",
            "status please",
            "--session",
            "ci",
            "--provider",
            "grok",
        ])
        .output()
        .unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.status.success(), "{text}");
    assert!(text.contains("cli-ci"), "{text}");
    assert!(text.contains("grok"), "{text}");
}

#[test]
fn atr_ask_missing_config_non_tty_fails() {
    let dir = TempDir::new().unwrap();
    fs::create_dir_all(dir.path().join("home")).unwrap();
    let cfg = dir.path().join("missing").join("config.toml");
    let out = atr()
        .env("ATR_CONFIG", &cfg)
        .env("HOME", dir.path().join("home"))
        .args(["ask", "hello"])
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "non-TTY ask without config must fail"
    );
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(text.contains("atr init"), "{text}");
    assert!(text.contains("--yes"), "{text}");
    assert!(
        !text.contains("atr tui  "),
        "must not start the interactive TUI: {text}"
    );
    assert!(!cfg.exists(), "non-TTY must not write config");
}

#[test]
fn atr_ask_missing_prompt_fails() {
    let (dir, cfg) = setup_bin_cfg();
    let out = atr()
        .env("ATR_CONFIG", &cfg)
        .env("HOME", dir.path().join("home"))
        .arg("ask")
        .output()
        .unwrap();
    assert!(!out.status.success(), "ask without a prompt must fail");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        text.contains("PROMPT") || text.contains("prompt") || text.contains("required"),
        "{text}"
    );
    assert!(
        !text.contains("atr tui  "),
        "must not fall into the interactive TUI: {text}"
    );

    let out = atr()
        .env("ATR_CONFIG", &cfg)
        .env("HOME", dir.path().join("home"))
        .args(["ask", "   "])
        .output()
        .unwrap();
    assert!(!out.status.success(), "blank prompt must fail");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(text.contains("requires a prompt"), "{text}");
    assert!(
        !text.contains("atr tui  "),
        "must not fall into the interactive TUI: {text}"
    );
}

#[test]
fn tui_help_documents_flags() {
    let out = atr().args(["tui", "--help"]).output().unwrap();
    assert!(out.status.success());
    let help = String::from_utf8_lossy(&out.stdout);
    assert!(help.contains("--provider"), "{help}");
    assert!(help.contains("--session"), "{help}");
    assert!(help.contains("--new"), "{help}");
    assert!(!help.contains("ATR_TUI_PROVIDER"), "{help}");
    assert!(!help.contains("ATR_DEFAULT"), "{help}");

    let out = atr().args(["ask", "--help"]).output().unwrap();
    assert!(out.status.success());
    let help = String::from_utf8_lossy(&out.stdout);
    assert!(help.contains("<PROMPT>"), "{help}");
    assert!(help.contains("--provider"), "{help}");
    assert!(help.contains("--session"), "{help}");
    assert!(help.contains("--new"), "{help}");
    assert!(!help.contains("ATR_TUI_PROVIDER"), "{help}");
    assert!(!help.contains("ATR_DEFAULT"), "{help}");
}

fn setup_bin_cfg() -> (TempDir, std::path::PathBuf) {
    let dir = TempDir::new().unwrap();
    fs::create_dir_all(dir.path().join("home")).unwrap();
    let fake_dir = dir.path().join("fake");
    fs::create_dir_all(&fake_dir).unwrap();
    let bin = dir.path().join("bin");
    fs::create_dir_all(&bin).unwrap();
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake-agent.sh");
    for name in ["claude", "grok"] {
        let dest = bin.join(name);
        fs::copy(&script, &dest).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&dest, fs::Permissions::from_mode(0o755)).unwrap();
        }
        fs::write(fake_dir.join(format!("{name}.behavior")), "success").unwrap();
    }
    let repo = dir.path().join("repo");
    atr::worktree::init_sample_repo(&repo).unwrap();
    let prompt = dir.path().join("p.md");
    fs::write(&prompt, "p\n").unwrap();
    let cfg = dir.path().join("config.toml");
    fs::write(
        &cfg,
        format!(
            r#"
listen = "127.0.0.1:0"
state_db = "{db}"
worktree_root = "{wt}"
repo_path = "{repo}"
system_prompt_file = "{prompt}"
[planner]
mode = "off"
[github]
enabled = false
[[provider]]
name = "claude"
kind = "claude"
cli = "{claude}"
model = "test-model"
run_timeout = "8s"
[[provider]]
name = "grok"
kind = "grok"
cli = "{grok}"
model = "test-model"
run_timeout = "8s"
"#,
            db = dir.path().join("state.db").display(),
            wt = dir.path().join("work").display(),
            repo = repo.display(),
            prompt = prompt.display(),
            claude = bin.join("claude").display(),
            grok = bin.join("grok").display(),
        ),
    )
    .unwrap();
    (dir, cfg)
}
