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
