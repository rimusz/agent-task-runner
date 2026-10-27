//! Interactive terminal session (`atr tui`, also a bare `atr`) and
//! one-shot CI turns (`atr ask`).
//!
//! A session is a normal atr task keyed `cli-<name>` (default
//! `cli-default`), separate from Telegram's `tg<chat>` keys. The engine
//! runs in-process: `atr serve` is not required. Resume, the stable
//! prefix, worktree pool, and provider-switch handoff are the existing
//! task machinery.
//!
//! Serve owns daemon work (Telegram, GitHub, `atr add`, …). It never
//! auto-dispatches `cli-*` keys. The TUI / `ask` process owns those
//! sessions via [`crate::scheduler::dispatch_key`].
//!
//! The interactive UI is a session host: transcript, footer, input.
//! atr-local slashes are `/provider`, `/model`, `/new`, `/quit`,
//! `/status`, `/help`, `/cli`. `/model` changes the session `--model`
//! override and is never sent as chat. Other `/…` may be proxied.
//! `atr ask` never opens the alternate screen.
//!
//! First run on a TTY offers [`crate::init::ensure_config`] (the normal
//! `atr init`) when config is missing, then continues. Non-TTY / CI
//! fails with a pointer to `atr init` / `atr init --yes`.

mod history;
mod proxy;
mod slash;
mod ui;

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
pub const ASK_PROMPT_REQUIRED: &str =
    "atr ask requires a prompt. Example: atr ask \"what is 2+2?\"  (use `atr tui` for the interactive session)";

const MODEL_META_PREFIX: &str = "tui-model:";

#[derive(Debug, Clone, Default)]
pub struct TuiOpts {
    pub prompt: Option<String>,
    pub provider: Option<String>,
    pub session: Option<String>,
    pub dir: Option<PathBuf>,
    pub repo: Option<String>,
    pub new: bool,
    /// When true, a missing or blank prompt is an error (never the REPL).
    /// Used by `atr ask`.
    pub require_prompt: bool,
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

#[derive(Debug, Clone)]
pub(crate) struct Workspace {
    pub kind: WorkspaceKind,
    pub repo: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Step {
    Quit,
    Note(String),
    Turn(String),
    OpenProviderPicker,
    OpenModelPicker,
    NativeCli,
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

/// Launch preference from `--provider`. The `[tui] providers` list is a
/// fallback order, not a pin.
pub fn launch_provider(opts_provider: Option<&str>) -> Option<String> {
    opts_provider
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToString::to_string)
}

/// Provider the next turn / `/model` list should use — same value as
/// the footer. Pin / hint / `[tui] providers`, then last run, then the
/// first enabled provider. Not "last run first" (that disagreed with
/// `/model` when the session had moved on).
pub fn session_provider_now(shared: &Shared, key: &str) -> Option<String> {
    if let Some(p) = tui_provider_now(shared, key) {
        return Some(p);
    }
    if let Ok(runs) = shared.db.list_runs(Some(key)) {
        if let Some(p) = runs
            .last()
            .map(|r| r.provider.clone())
            .filter(|s| !s.is_empty())
        {
            return Some(p);
        }
    }
    shared
        .config
        .read()
        .unwrap()
        .providers
        .iter()
        .find(|p| p.enabled)
        .map(|p| p.name.clone())
}

/// Provider this TUI session would use now: session pin, then
/// `[tui] providers`, else `None` (first available at dispatch).
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

/// Apply a stored `/model <id>` override onto the provider used for this turn.
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
    warn_if_no_provider(
        &shared,
        opts.require_prompt || opts.prompt.as_deref().is_some_and(|s| !s.trim().is_empty()),
    )?;

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
    if opts.require_prompt {
        bail!("{ASK_PROMPT_REQUIRED}");
    }

    interactive(shared, key, ws, pin).await
}

/// One-shot path used by `atr ask`. Never opens the interactive TUI.
pub async fn run_oneshot(shared: Arc<Shared>, mut opts: TuiOpts) -> Result<ExitCode> {
    opts.require_prompt = true;
    run(shared, opts).await
}

fn interactive_tty() -> bool {
    std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
}

async fn interactive(
    shared: Arc<Shared>,
    key: String,
    ws: Workspace,
    pin: Option<String>,
) -> Result<ExitCode> {
    if interactive_tty() {
        match ui::run(shared.clone(), key.clone(), ws.clone(), pin.clone()).await {
            Ok(code) => return Ok(code),
            Err(e) => {
                eprintln!("tui: {e:#}; falling back to line mode");
            }
        }
    }
    line_interactive(shared, key, ws, pin).await
}

async fn line_interactive(
    shared: Arc<Shared>,
    key: String,
    ws: Workspace,
    pin: Option<String>,
) -> Result<ExitCode> {
    let tty = std::io::stdin().is_terminal();
    if tty {
        eprint!("{}", banner_text(&shared, &key));
    }
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    loop {
        if tty {
            eprint!("› ");
            let _ = std::io::stderr().flush();
        }
        let Some(line) = lines.next_line().await? else {
            break;
        };
        let line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }
        match step_line(&shared, &key, &ws, pin.as_deref(), &line)? {
            Step::Quit => break,
            Step::Note(msg) => {
                if !msg.is_empty() {
                    println!("{msg}");
                }
            }
            Step::OpenProviderPicker => {
                println!(
                    "{}",
                    slash::provider_pick_usage(&slash::list_providers(&shared, &key))
                );
            }
            Step::OpenModelPicker => {
                println!("{}", model_picker_text(&shared, &key, &ws));
            }
            Step::NativeCli => {
                if interactive_tty() {
                    match proxy::run_native_cli(&shared, &key, &ws) {
                        Ok(msg) => println!("{msg}"),
                        Err(e) => eprintln!("{e:#}"),
                    }
                } else {
                    println!("{}", native_cli_hint(&shared, &key));
                }
            }
            Step::Turn(text) => {
                let out = run_turn(&shared, &key, &ws, &text, pin.as_deref()).await?;
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

pub(crate) fn step_line(
    shared: &Shared,
    key: &str,
    ws: &Workspace,
    pin: Option<&str>,
    line: &str,
) -> Result<Step> {
    let line = line.trim();
    if line.is_empty() || slash::is_bare_slash(line) {
        return Ok(Step::Note(String::new()));
    }
    match slash::parse_slash(line) {
        None => Ok(Step::Turn(line.to_string())),
        Some(slash::Slash::Quit) => Ok(Step::Quit),
        Some(slash::Slash::Help) => Ok(Step::Note(slash::slash_help().to_string())),
        Some(slash::Slash::Status) => Ok(Step::Note(footer_line(shared, key))),
        Some(slash::Slash::New) => {
            reset_session(shared, key)?;
            if let Some(p) = pin {
                pin_session_provider(shared, key, p)?;
            }
            Ok(Step::Note(format!("new session {key}")))
        }
        Some(slash::Slash::Provider(None)) => Ok(Step::OpenProviderPicker),
        Some(slash::Slash::Provider(Some(name))) => apply_provider_pick(shared, key, ws, &name),
        Some(slash::Slash::Model(None)) => Ok(Step::OpenModelPicker),
        Some(slash::Slash::Model(Some(name))) => apply_model_pick(shared, key, ws, &name),
        Some(slash::Slash::Cli) => Ok(Step::NativeCli),
        Some(slash::Slash::Proxy(cmd)) => Ok(Step::Turn(cmd)),
    }
}

pub(crate) fn apply_model_pick(
    shared: &Shared,
    key: &str,
    ws: &Workspace,
    input: &str,
) -> Result<Step> {
    let list = proxy::collect_models(shared, key, ws, proxy::CliFetch::Cached);
    let names: Vec<String> = list.iter().map(|m| m.name.clone()).collect();
    if let Some(name) = slash::pick_choice(&names, input) {
        return set_model_note(shared, key, &name);
    }
    if input.trim().parse::<usize>().is_ok() {
        let (kind, _, pname) = proxy::current_kind_cli(shared, key)
            .unwrap_or_else(|| ("?".into(), Default::default(), "none".into()));
        return Ok(Step::Note(format!(
            "no model matching {input:?}.\n{}",
            slash::model_pick_usage(&pname, &kind, &list)
        )));
    }
    let name = input.trim();
    if name.is_empty() {
        return Ok(Step::OpenModelPicker);
    }
    set_model_note(shared, key, name)
}

fn set_model_note(shared: &Shared, key: &str, name: &str) -> Result<Step> {
    shared.db.meta_set(&model_meta_key(key), name)?;
    Ok(Step::Note(format!(
        "model {name} (this session; next turn uses the provider --model flag)\n{}",
        footer_line(shared, key)
    )))
}

fn model_picker_text(shared: &Shared, key: &str, ws: &Workspace) -> String {
    let list = proxy::collect_models(shared, key, ws, proxy::CliFetch::Blocking);
    let (kind, _, pname) = proxy::current_kind_cli(shared, key)
        .unwrap_or_else(|| ("?".into(), Default::default(), "none".into()));
    slash::model_pick_usage(&pname, &kind, &list)
}

/// What a TUI line would do. Tests lock `/model` out of the turn path.
pub fn classify_line(line: &str) -> &'static str {
    let line = line.trim();
    if slash::is_bare_slash(line) {
        return "slash-menu";
    }
    match slash::parse_slash(line) {
        None => "chat",
        Some(slash::Slash::Help) => "help",
        Some(slash::Slash::Quit) => "quit",
        Some(slash::Slash::Status) => "status",
        Some(slash::Slash::New) => "new",
        Some(slash::Slash::Provider(_)) => "provider",
        Some(slash::Slash::Model(_)) => "model",
        Some(slash::Slash::Cli) => "cli",
        Some(slash::Slash::Proxy(_)) => "proxy",
    }
}

pub fn set_session_model(shared: &Shared, key: &str, model: &str) -> Result<()> {
    shared.db.meta_set(&model_meta_key(key), model.trim())?;
    Ok(())
}

/// Run a TUI slash/chat classify+apply without spawning a turn.
/// Returns `turn` only when the line would enqueue a provider run.
pub fn preview_line(shared: &Shared, key: &str, line: &str) -> Result<String> {
    let ws = Workspace {
        kind: WorkspaceKind::None,
        repo: None,
    };
    match step_line(shared, key, &ws, None, line)? {
        Step::Turn(_) => Ok("turn".into()),
        Step::Note(s) => Ok(format!("note\n{s}")),
        Step::OpenModelPicker => Ok(format!(
            "model-picker\n{}",
            model_picker_text(shared, key, &ws)
        )),
        Step::OpenProviderPicker => Ok("provider-picker".into()),
        Step::Quit => Ok("quit".into()),
        Step::NativeCli => Ok("cli".into()),
    }
}

pub(crate) fn apply_provider_pick(
    shared: &Shared,
    key: &str,
    ws: &Workspace,
    input: &str,
) -> Result<Step> {
    let list = slash::list_providers(shared, key);
    let names: Vec<String> = list.iter().map(|p| p.name.clone()).collect();
    match slash::pick_choice(&names, input) {
        Some(name) => {
            let prev = proxy::current_kind_cli(shared, key);
            set_session_provider(shared, key, &name)?;
            if let Some((kind, cli, _)) = prev {
                let same = proxy::current_kind_cli(shared, key)
                    .is_some_and(|(k, c, _)| k == kind && c == cli);
                if !same {
                    proxy::invalidate_cli_models(&kind, &cli);
                }
            }
            proxy::begin_session_models_refresh(shared, key, ws);
            Ok(Step::Note(format!(
                "provider {name} (next turn; handoff if switching)"
            )))
        }
        None => Ok(Step::Note(format!(
            "no provider matching {input:?}.\n{}",
            slash::provider_pick_usage(&list)
        ))),
    }
}

fn native_cli_hint(shared: &Shared, key: &str) -> String {
    match proxy::current_kind_cli(shared, key) {
        Some((kind, _, name)) => format!(
            "/cli needs a TTY so {name} ({kind}) can take over the terminal.\n\
             Run `{kind}` in a real terminal, or start `atr tui` on a TTY."
        ),
        None => "/cli needs a TTY and a provider. /provider first.".into(),
    }
}

fn warn_if_no_provider(shared: &Shared, oneshot: bool) -> Result<()> {
    let cfg = shared.config.read().unwrap().clone();
    let Some(msg) = crate::init::no_provider_next_steps(&cfg) else {
        return Ok(());
    };
    eprint!("{msg}");
    if oneshot {
        bail!("{}", msg.trim_end());
    }
    let tty = interactive_tty();
    if !tty {
        bail!("{}", msg.trim_end());
    }
    if !crate::init::yn(
        "Continue anyway (turns will fail until a provider is ready)",
        false,
        false,
    )? {
        bail!("{}", msg.trim_end());
    }
    Ok(())
}

pub fn banner_text(shared: &Shared, key: &str) -> String {
    let cfg = shared.config.read().unwrap().clone();
    let footer = footer_line(shared, key);
    let provider = tui_provider_now(shared, key)
        .or_else(|| {
            cfg.providers
                .iter()
                .find(|p| p.enabled)
                .map(|p| p.name.clone())
        })
        .unwrap_or_else(|| "none".into());
    format!(
        "atr tui  {footer}\nsession  {key}\nprovider {provider}\nlist     {}\n{}\n\
         /help · other /slashes go to the provider · /quit\n",
        provider_list_short(&cfg),
        daemon_line(&shared.config_path),
    )
}

pub fn provider_list_short(cfg: &crate::config::Config) -> String {
    if cfg.providers.is_empty() {
        return "none".into();
    }
    cfg.providers
        .iter()
        .map(|p| {
            if p.enabled {
                p.name.clone()
            } else {
                format!("{} (off)", p.name)
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

pub fn daemon_line(config_path: &std::path::Path) -> String {
    let label = crate::service::launchd_label_for(config_path);
    let loaded = crate::service::service_loaded(&label)
        || crate::service::service_loaded(crate::service::LAUNCHD_LABEL);
    format_daemon_line(loaded)
}

pub fn format_daemon_line(loaded: bool) -> String {
    if loaded {
        "daemon   loaded — check `atr status`".into()
    } else {
        "daemon   not loaded — `atr service install` or `atr serve` later".into()
    }
}

fn print_oneshot(out: &TurnResult) -> Result<()> {
    if !out.text.is_empty() {
        println!("{}", out.text);
    }
    eprintln!("{}", out.footer);
    Ok(())
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
    enqueue_turn(shared, key, ws, text, launch_pin)?;
    wait_turn(shared, key).await?;
    Ok(collect_result(shared, key))
}

pub(crate) fn enqueue_turn(
    shared: &Shared,
    key: &str,
    ws: &Workspace,
    text: &str,
    launch_pin: Option<&str>,
) -> Result<()> {
    let cfg = shared.config.read().unwrap().clone();
    task::check_prompt_bytes(text, cfg.max_prompt_bytes)?;
    let none = ws.kind == WorkspaceKind::None;
    let _ = shared.submit_with(
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
    Ok(())
}

async fn wait_turn(shared: &Arc<Shared>, key: &str) -> Result<()> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3600);
    let mut ticks = 0u32;
    loop {
        scheduler::dispatch_key(shared, key).await?;
        ticks += 1;
        if turn_is_idle(shared, key)? {
            return Ok(());
        }
        if ticks >= 3 {
            check_provider_available(shared, key)?;
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

pub(crate) fn turn_is_idle(shared: &Shared, key: &str) -> Result<bool> {
    let inflight = shared.inflight.lock().unwrap().contains_key(key);
    let task = shared.db.get_task(key)?;
    let status = task.as_ref().map(|t| t.status.as_str()).unwrap_or("");
    let pending = shared.db.pending_messages(key).ok().unwrap_or_default();
    Ok(!inflight
        && pending.is_empty()
        && matches!(
            status,
            "waiting" | "done" | "failed" | "needs_attention" | "cancelled"
        ))
}

pub(crate) fn check_provider_available(shared: &Shared, key: &str) -> Result<()> {
    let inflight = shared.inflight.lock().unwrap().contains_key(key);
    let task = shared.db.get_task(key)?;
    let status = task.as_ref().map(|t| t.status.as_str()).unwrap_or("");
    if inflight || status != "queued" {
        return Ok(());
    }
    let cfg = shared.config.read().unwrap().clone();
    let empty = std::collections::HashMap::new();
    if let Some(t) = &task {
        if scheduler::pick_provider(&cfg, &shared.db, &empty, t, &crate::clock::Clock::system())?
            .is_none()
        {
            bail!("no provider available for session {key}");
        }
    }
    Ok(())
}

pub(crate) fn collect_result(shared: &Shared, key: &str) -> TurnResult {
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
    let runs = shared.db.list_runs(Some(key)).unwrap_or_default();
    let last = runs.last();
    let provider = session_provider_now(shared, key).unwrap_or_else(|| "—".into());
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
    fn launch_provider_uses_flag_only() {
        assert_eq!(launch_provider(Some("grok")).as_deref(), Some("grok"));
        assert_eq!(
            launch_provider(Some("  claude ")).as_deref(),
            Some("claude")
        );
        assert_eq!(launch_provider(Some("")), None);
        assert_eq!(launch_provider(None), None);
    }

    #[test]
    fn model_lines_never_classify_as_chat_or_proxy() {
        assert_eq!(classify_line("/model"), "model");
        assert_eq!(classify_line("/model sonnet"), "model");
        assert_eq!(classify_line("/m"), "model");
        assert_eq!(classify_line("/atr model opus"), "model");
        assert_eq!(classify_line("/compact"), "proxy");
        assert_eq!(classify_line("hello"), "chat");
        assert_eq!(classify_line("/provider"), "provider");
        assert_eq!(classify_line("/"), "slash-menu");
        assert_eq!(classify_line(" / "), "slash-menu");
    }

    #[test]
    fn ask_prompt_required_mentions_ask_and_tui() {
        assert!(ASK_PROMPT_REQUIRED.contains("atr ask"));
        assert!(ASK_PROMPT_REQUIRED.contains("atr tui"));
    }

    #[test]
    fn banner_helpers_are_tight() {
        assert!(format_daemon_line(true).contains("atr status"));
        assert!(format_daemon_line(false).contains("atr service install"));
        let cfg = crate::config::parse_config_toml(
            r#"
[[provider]]
name = "claude"
kind = "claude"
enabled = true
[[provider]]
name = "grok"
kind = "grok"
enabled = false
"#,
        )
        .unwrap();
        let list = provider_list_short(&cfg);
        assert!(list.contains("claude"), "{list}");
        assert!(list.contains("grok (off)"), "{list}");
    }
}
