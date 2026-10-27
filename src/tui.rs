//! Interactive terminal session (`atr tui`).
//!
//! A TUI session is a normal atr task keyed `cli-<name>` (default
//! `cli-default`), separate from Telegram's `tg<chat>` keys. The engine
//! runs in-process: `atr serve` is not required. Resume, the stable
//! prefix, worktree pool, and provider-switch handoff are the existing
//! task machinery.

use crate::app::{Shared, SubmitOpts};
use crate::db::Db;
use crate::providers::{cache_hit_ratio, UsageNumbers};
use crate::scheduler;
use crate::task::{self, OutputKind, WorkspaceKind};
use anyhow::{bail, Result};
use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};

pub const SESSION_PREFIX: &str = "cli-";
pub const DEFAULT_SESSION: &str = "default";

const MODEL_META_PREFIX: &str = "tui-model:";

#[derive(Debug, Clone, Default)]
pub struct TuiOpts {
    pub prompt: Option<String>,
    pub provider: Option<String>,
    pub session: Option<String>,
    pub dir: Option<PathBuf>,
    pub repo: Option<String>,
    pub new: bool,
}

#[derive(Debug, Clone)]
pub struct TurnResult {
    pub key: String,
    pub ok: bool,
    pub text: String,
    pub footer: String,
    pub provider: String,
    pub model: String,
}

/// `ATR_DEFAULT=tui` (case-insensitive) means a bare `atr` is `atr tui`.
pub fn default_is_tui() -> bool {
    default_from(std::env::var("ATR_DEFAULT").ok().as_deref())
}

pub fn default_from(val: Option<&str>) -> bool {
    val.is_some_and(|v| v.trim().eq_ignore_ascii_case("tui"))
}

pub fn is_tui_key(key: &str) -> bool {
    key.strip_prefix(SESSION_PREFIX)
        .is_some_and(|rest| !rest.is_empty())
}

/// `work` or `cli-work` → `cli-work`. Default name is `default`.
pub fn tui_task_key(name: &str) -> Result<String> {
    let raw = name.trim();
    let raw = raw.strip_prefix(SESSION_PREFIX).unwrap_or(raw);
    if raw.is_empty() {
        bail!("session name is empty");
    }
    if raw.len() > 40 {
        bail!("session name is too long (max 40)");
    }
    if !raw
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        bail!("session name {raw:?} must be letters, digits, '-' or '_'");
    }
    Ok(format!("{SESSION_PREFIX}{raw}"))
}

pub fn model_meta_key(task_key: &str) -> String {
    format!("{MODEL_META_PREFIX}{task_key}")
}

/// Launch preference: `--provider` / `ATR_TUI_PROVIDER` (same effect).
/// The `[tui] providers` list is a fallback order, not a pin.
pub fn launch_provider(opts_provider: Option<&str>) -> Option<String> {
    launch_provider_from(
        opts_provider,
        std::env::var("ATR_TUI_PROVIDER").ok().as_deref(),
    )
}

pub fn launch_provider_from(opts_provider: Option<&str>, env: Option<&str>) -> Option<String> {
    if let Some(p) = opts_provider.map(str::trim).filter(|s| !s.is_empty()) {
        return Some(p.to_string());
    }
    env.map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToString::to_string)
}

/// Provider this TUI session would use now: session pin, then launch env,
/// then `[tui] providers`, else `None` (first available at dispatch).
pub fn tui_provider_now(shared: &Shared, key: &str) -> Option<String> {
    if let Ok(Some(t)) = shared.db.get_task(key) {
        if let Some(p) = t
            .provider
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            return Some(p.to_string());
        }
        if !t.provider_hint.trim().is_empty() {
            return Some(t.provider_hint.trim().to_string());
        }
    }
    if let Some(p) = launch_provider(None) {
        return Some(p);
    }
    let names = shared.config.read().unwrap().tui.providers.clone();
    if names.is_empty() {
        return None;
    }
    scheduler::listed_provider_now(shared, &names)
}

pub fn session_model(db: &Db, key: &str) -> Option<String> {
    db.meta_get(&model_meta_key(key))
        .ok()
        .flatten()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Apply a stored `/model` override onto the provider used for this turn.
pub fn apply_session_model(provider: &mut crate::config::Provider, db: &Db, key: &str) {
    if let Some(m) = session_model(db, key) {
        provider.model = m;
    }
}

pub async fn run(shared: Arc<Shared>, opts: TuiOpts) -> Result<ExitCode> {
    let session_name = opts
        .session
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(DEFAULT_SESSION);
    let key = tui_task_key(session_name)?;
    let pin = launch_provider(opts.provider.as_deref());
    if let Some(p) = pin.as_deref() {
        let cfg = shared.config.read().unwrap().clone();
        if cfg.provider(p).is_none() {
            bail!("provider {p:?} is not in config");
        }
    }
    if opts.new {
        reset_session(&shared, &key)?;
    }
    let ws = resolve_workspace(&shared, &opts)?;

    if let Some(text) = opts
        .prompt
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        let out = run_turn(&shared, &key, &ws, text, pin.as_deref()).await?;
        print_oneshot(&out)?;
        return Ok(if out.ok {
            ExitCode::SUCCESS
        } else {
            ExitCode::from(1)
        });
    }

    interactive(shared, key, ws, pin).await
}

async fn interactive(
    shared: Arc<Shared>,
    key: String,
    ws: Workspace,
    pin: Option<String>,
) -> Result<ExitCode> {
    let tty = std::io::stdin().is_terminal();
    if tty {
        print_banner(&shared, &key)?;
    }
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    loop {
        if tty {
            eprint!("> ");
            let _ = std::io::stderr().flush();
        }
        let Some(line) = lines.next_line().await? else {
            break;
        };
        let line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }
        match parse_slash(&line) {
            Some(Slash::Quit) => break,
            Some(Slash::Help) => {
                println!("{}", slash_help());
                continue;
            }
            Some(Slash::Status) => {
                println!("{}", footer_line(&shared, &key));
                continue;
            }
            Some(Slash::New) => {
                reset_session(&shared, &key)?;
                if let Some(p) = pin.as_deref() {
                    pin_session_provider(&shared, &key, p)?;
                }
                println!("new session {key}");
                continue;
            }
            Some(Slash::Provider(name)) => {
                set_session_provider(&shared, &key, &name)?;
                println!("provider {name} (next turn; handoff if switching)");
                continue;
            }
            Some(Slash::Model(model)) => {
                shared.db.meta_set(&model_meta_key(&key), &model)?;
                println!("model {model} (this session)");
                continue;
            }
            Some(Slash::Unknown(cmd)) => {
                eprintln!("unknown command {cmd}. {help}", help = slash_help());
                continue;
            }
            None => {
                let out = run_turn(&shared, &key, &ws, &line, pin.as_deref()).await?;
                if !out.text.is_empty() {
                    println!("{}", out.text);
                }
                eprintln!("{}", out.footer);
                if !out.ok {
                    eprintln!("turn failed");
                }
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn print_banner(shared: &Shared, key: &str) -> Result<()> {
    let footer = footer_line(shared, key);
    eprintln!("atr tui  {footer}");
    eprintln!("{}", slash_help());
    Ok(())
}

fn print_oneshot(out: &TurnResult) -> Result<()> {
    if !out.text.is_empty() {
        println!("{}", out.text);
    }
    eprintln!("{}", out.footer);
    Ok(())
}

fn slash_help() -> &'static str {
    "/help  /status  /provider <name>  /model <name>  /new  /quit"
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Slash {
    Help,
    Quit,
    Status,
    New,
    Provider(String),
    Model(String),
    Unknown(String),
}

fn parse_slash(line: &str) -> Option<Slash> {
    let t = line.trim();
    if !t.starts_with('/') {
        return None;
    }
    let mut parts = t.split_whitespace();
    let cmd = parts.next()?;
    match cmd {
        "/help" | "/?" => Some(Slash::Help),
        "/quit" | "/exit" | "/q" => Some(Slash::Quit),
        "/status" => Some(Slash::Status),
        "/new" => Some(Slash::New),
        "/provider" => match parts.next() {
            Some(n) => Some(Slash::Provider(n.to_string())),
            None => Some(Slash::Unknown("/provider needs a name".into())),
        },
        "/model" => {
            let rest: Vec<&str> = parts.collect();
            if rest.is_empty() {
                Some(Slash::Unknown("/model needs a name".into()))
            } else {
                Some(Slash::Model(rest.join(" ")))
            }
        }
        other => Some(Slash::Unknown(other.to_string())),
    }
}

#[derive(Debug, Clone)]
struct Workspace {
    kind: WorkspaceKind,
    repo: Option<String>,
}

fn resolve_workspace(shared: &Shared, opts: &TuiOpts) -> Result<Workspace> {
    let cfg = shared.config.read().unwrap().clone();
    if let Some(d) = &opts.dir {
        let p = task::resolve_dir(&d.display().to_string())?;
        return Ok(Workspace {
            kind: WorkspaceKind::Dir,
            repo: Some(p.display().to_string()),
        });
    }
    if let Some(r) = opts.repo.as_deref() {
        if let Some(entry) = cfg.resolve_repo_filter(r) {
            return Ok(Workspace {
                kind: WorkspaceKind::Git,
                repo: Some(entry.path.display().to_string()),
            });
        }
        let p = task::resolve_git_repo(r)?;
        return Ok(Workspace {
            kind: WorkspaceKind::Git,
            repo: Some(p.display().to_string()),
        });
    }
    Ok(Workspace {
        kind: WorkspaceKind::None,
        repo: None,
    })
}

fn reset_session(shared: &Shared, key: &str) -> Result<()> {
    if let Some(t) = shared.db.get_task(key)? {
        if t.status == "running" || shared.inflight.lock().unwrap().contains_key(key) {
            bail!("session {key} is running");
        }
        if t.status == "queued" {
            shared.db.set_status(key, "cancelled")?;
        }
        if let Some(reason) = shared.db.delete_refuse_reason(key)? {
            if !reason.contains("unknown task") {
                bail!("{reason}");
            }
        } else {
            shared.db.delete_task(key)?;
        }
    }
    let _ = shared.db.meta_set(&model_meta_key(key), "");
    Ok(())
}

fn pin_session_provider(shared: &Shared, key: &str, name: &str) -> Result<()> {
    if shared.db.get_task(key)?.is_none() {
        return Ok(());
    }
    let sid = shared.db.task_session(key, name).ok().flatten();
    shared
        .db
        .set_provider_session(key, Some(name), sid.as_deref())?;
    shared.db.set_provider_hint(key, name)?;
    Ok(())
}

fn set_session_provider(shared: &Shared, key: &str, name: &str) -> Result<()> {
    let cfg = shared.config.read().unwrap().clone();
    if cfg.provider(name).is_none() {
        bail!("provider {name:?} is not in config");
    }
    if shared.db.get_task(key)?.is_none() {
        // Remember until the first prompt creates the task.
        shared.db.meta_set(&format!("tui-pin:{key}"), name)?;
        return Ok(());
    }
    pin_session_provider(shared, key, name)
}

async fn run_turn(
    shared: &Arc<Shared>,
    key: &str,
    ws: &Workspace,
    text: &str,
    launch_pin: Option<&str>,
) -> Result<TurnResult> {
    let cfg = shared.config.read().unwrap().clone();
    task::check_prompt_bytes(text, cfg.max_prompt_bytes)?;
    let none = ws.kind == WorkspaceKind::None;
    let outcome = shared.submit_with(
        key,
        text,
        "cli",
        ws.repo.as_deref(),
        None,
        false,
        true,
        SubmitOpts {
            workspace_kind: Some(ws.kind.as_str().to_string()),
            output_kind: Some(OutputKind::Reply.as_str().to_string()),
            output_spec: None,
            source_kind: Some("cli".into()),
            title: Some(format!("tui {}", key.trim_start_matches(SESSION_PREFIX))),
            allow_empty_repo: none,
            provider_hint: launch_pin.map(|s| s.to_string()),
            needs: Vec::new(),
        },
    )?;
    let _ = outcome;
    // Chat turns are not planner jobs, even when `[planner] mode = always`.
    if shared.db.get_task(key)?.is_some_and(|t| t.role == "plan") {
        let _ = shared.db.set_task_role(key, "");
    }
    if let Some(p) = launch_pin {
        pin_session_provider(shared, key, p)?;
    } else if let Ok(Some(pending)) = shared.db.meta_get(&format!("tui-pin:{key}")) {
        if !pending.trim().is_empty() {
            pin_session_provider(shared, key, pending.trim())?;
            let _ = shared.db.meta_set(&format!("tui-pin:{key}"), "");
        }
    }
    wait_turn(shared, key).await?;
    Ok(collect_result(shared, key))
}

async fn wait_turn(shared: &Arc<Shared>, key: &str) -> Result<()> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3600);
    let mut ticks = 0u32;
    loop {
        scheduler::dispatch_key(shared, key).await?;
        ticks += 1;
        let inflight = shared.inflight.lock().unwrap().contains_key(key);
        let task = shared.db.get_task(key)?;
        let status = task.as_ref().map(|t| t.status.as_str()).unwrap_or("");
        let pending = shared.db.pending_messages(key).ok().unwrap_or_default();
        if !inflight
            && pending.is_empty()
            && matches!(
                status,
                "waiting" | "done" | "failed" | "needs_attention" | "cancelled"
            )
        {
            return Ok(());
        }
        if !inflight && status == "queued" && ticks >= 3 {
            let cfg = shared.config.read().unwrap().clone();
            let empty = std::collections::HashMap::new();
            if let Some(t) = &task {
                if scheduler::pick_provider(
                    &cfg,
                    &shared.db,
                    &empty,
                    t,
                    &crate::clock::Clock::system(),
                )?
                .is_none()
                {
                    bail!("no provider available for session {key}");
                }
            }
        }
        if tokio::time::Instant::now() > deadline {
            bail!("timed out waiting for session {key}");
        }
        tokio::select! {
            _ = shared.kick.notified() => {}
            _ = tokio::time::sleep(Duration::from_millis(40)) => {}
        }
    }
}

fn collect_result(shared: &Shared, key: &str) -> TurnResult {
    let runs = shared.db.list_runs(Some(key)).unwrap_or_default();
    let last = runs.last();
    let ok = last
        .and_then(|r| r.outcome.as_deref())
        .is_some_and(|o| o == "success");
    let text = last
        .and_then(|r| {
            let events = shared.db.events_for_run(r.id).ok()?;
            crate::handoff::last_assistant_text(&events)
        })
        .or_else(|| {
            shared
                .db
                .get_task(key)
                .ok()
                .flatten()
                .map(|t| t.result_text)
                .filter(|s| !s.trim().is_empty())
        })
        .unwrap_or_else(|| {
            last.and_then(|r| r.outcome.clone())
                .unwrap_or_else(|| "(no assistant text)".into())
        });
    let footer = footer_line(shared, key);
    let (provider, model) = last
        .map(|r| (r.provider.clone(), r.model.clone().unwrap_or_default()))
        .unwrap_or_else(|| (String::new(), String::new()));
    TurnResult {
        key: key.to_string(),
        ok,
        text,
        footer,
        provider,
        model,
    }
}

pub fn footer_line(shared: &Shared, key: &str) -> String {
    let cfg = shared.config.read().unwrap().clone();
    let task = shared.db.get_task(key).ok().flatten();
    let runs = shared.db.list_runs(Some(key)).unwrap_or_default();
    let last = runs.last();
    let provider = last
        .map(|r| r.provider.clone())
        .or_else(|| task.as_ref().and_then(|t| t.provider.clone()))
        .or_else(|| tui_provider_now(shared, key))
        .unwrap_or_else(|| {
            cfg.providers
                .iter()
                .find(|p| p.enabled)
                .map(|p| p.name.clone())
                .unwrap_or_else(|| "—".into())
        });
    let model = session_model(&shared.db, key)
        .or_else(|| last.and_then(|r| r.model.clone()).filter(|s| !s.is_empty()))
        .or_else(|| {
            cfg.provider(&provider)
                .map(|p| p.model.clone())
                .filter(|s| !s.is_empty())
        })
        .unwrap_or_else(|| "default".into());
    let mut parts = vec![key.to_string(), format!("{provider} / {model}")];
    if let Some(r) = last {
        if let Ok(rows) = shared.db.list_usage_for_run(r.id) {
            let mut u = UsageNumbers::default();
            for row in &rows {
                u.input += row.input.max(0) as u64;
                u.output += row.output.max(0) as u64;
                u.cache_read += row.cache_read.max(0) as u64;
                u.cache_write += row.cache_write.max(0) as u64;
            }
            let tok = u.input.saturating_add(u.output);
            if tok > 0 || u.cache_read > 0 {
                parts.push(format!("{tok} tok"));
            }
            let kind = cfg
                .provider(&provider)
                .map(|p| p.kind.as_str())
                .unwrap_or("");
            if let Some(hit) = cache_hit_ratio(kind, &u) {
                parts.push(format!("cache {:.0}%", hit * 100.0));
            }
        }
    }
    parts.join(" · ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_key_shape() {
        assert_eq!(tui_task_key("default").unwrap(), "cli-default");
        assert_eq!(tui_task_key("cli-work").unwrap(), "cli-work");
        assert_eq!(tui_task_key("  review-1 ").unwrap(), "cli-review-1");
        assert!(tui_task_key("").is_err());
        assert!(tui_task_key("has space").is_err());
        assert!(tui_task_key("tg42").is_ok());
        assert!(is_tui_key("cli-default"));
        assert!(is_tui_key("cli-work"));
        assert!(!is_tui_key("tg42"));
        assert!(!is_tui_key("cli-"));
        assert!(!is_tui_key("demo-1"));
        assert_eq!(model_meta_key("cli-default"), "tui-model:cli-default");
    }

    #[test]
    fn slash_commands() {
        assert_eq!(parse_slash("hello"), None);
        assert_eq!(parse_slash("/quit"), Some(Slash::Quit));
        assert_eq!(parse_slash("/q"), Some(Slash::Quit));
        assert_eq!(
            parse_slash("/provider grok"),
            Some(Slash::Provider("grok".into()))
        );
        assert_eq!(
            parse_slash("/model grok-code"),
            Some(Slash::Model("grok-code".into()))
        );
        assert!(matches!(parse_slash("/nope"), Some(Slash::Unknown(_))));
    }

    #[test]
    fn launch_provider_prefers_flag_then_env() {
        assert_eq!(
            launch_provider_from(None, Some("grok")).as_deref(),
            Some("grok")
        );
        assert_eq!(
            launch_provider_from(Some("claude"), Some("grok")).as_deref(),
            Some("claude")
        );
        assert_eq!(launch_provider_from(None, Some("")), None);
        assert_eq!(launch_provider_from(None, None), None);
    }

    #[test]
    fn atr_default_routing() {
        assert!(default_from(Some("tui")));
        assert!(default_from(Some("TUI")));
        assert!(default_from(Some(" tui ")));
        assert!(!default_from(Some("serve")));
        assert!(!default_from(None));
        assert!(!default_from(Some("")));
    }
}
