//! Interactive terminal session (`atr tui`, also a bare `atr`) and
//! one-shot CI turns (`atr ask`).
//!
//! Product rule: the TUI is the provider CLI, with atr as the proxy
//! (session reuse + prompt-cache). Host chrome is slashes, pickers, and
//! the transcript layout. Plain composer text is always a provider turn
//! — atr never invents chat replies.
//!
//! A session is a normal atr task keyed `cli-<name>` (default
//! `cli-default` — the in-process TUI session, **not** a model id),
//! separate from Telegram's `tg<chat>` keys. The engine
//! runs in-process: `atr serve` is not required. Resume, the stable
//! prefix, worktree pool, and provider-switch handoff are the existing
//! task machinery.
//!
//! Serve owns daemon work (Telegram, GitHub, `atr add`, …). It never
//! auto-dispatches `cli-*` keys. The TUI / `ask` process owns those
//! sessions via [`crate::scheduler::dispatch_key`].
//!
//! The interactive UI is a session host: transcript, composer, and an
//! under-input status strip (provider / model, plus honest workspace
//! kind: `answer-only` / `dir …` / `git …`). atr-local slashes are
//! `/provider`, `/model`, `/repo`, `/new`, `/quit`, `/status`, `/help`,
//! `/cli`. `/model` and `/repo` are never sent as chat. Other `/…` may
//! be proxied. `atr ask` never opens the alternate screen.
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
use std::collections::VecDeque;
use std::ffi::OsStr;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};

/// Task-key prefix for in-process TUI / `atr ask` sessions (`cli-work`).
pub const SESSION_PREFIX: &str = "cli-";
/// Bare `atr` / `atr tui` session name. The stored task key is
/// [`tui_task_key`] → `cli-default`. That is the default chat session,
/// not a model. User-facing chrome omits it unless `/session` is set.
pub const DEFAULT_SESSION: &str = "default";
pub const ASK_PROMPT_REQUIRED: &str =
    "atr ask requires a prompt. Example: atr ask \"what is 2+2?\"  (use `atr tui` for the interactive session)";

const MODEL_META_PREFIX: &str = "tui-model:";
const WS_META_PREFIX: &str = "tui-ws:";
const REPO_RECENT_META: &str = "tui-repo-recent";
const REPO_RECENT_MAX: usize = 8;

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
    OpenRepoPicker,
    NativeCli,
}

pub fn is_tui_key(key: &str) -> bool {
    key.strip_prefix(SESSION_PREFIX)
        .is_some_and(|rest| !rest.is_empty())
}

/// `work` or `cli-work` → `cli-work`. Default name is [`DEFAULT_SESSION`].
/// Keep this key stable: `/model` meta is `tui-model:<key>:<provider>`.
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

/// User-facing session label. `None` for the default `cli-default` key
/// so chrome does not shout the internal task id as if it were a model.
pub fn session_chrome_label(key: &str) -> Option<&str> {
    let name = key.strip_prefix(SESSION_PREFIX).unwrap_or(key);
    if name == DEFAULT_SESSION {
        None
    } else {
        Some(name)
    }
}

/// Bold title: `atr tui` for the default session, `atr tui  work` otherwise.
pub fn session_title(key: &str) -> String {
    match session_chrome_label(key) {
        None => " atr tui ".into(),
        Some(name) => format!(" atr tui  {name} "),
    }
}

/// True when composer text is a provider turn: any non-slash line that
/// is not empty, not whitespace, and not control-only garbage.
/// Real English (including `what can you do for me?`) is always a turn.
pub fn is_turn_text(line: &str) -> bool {
    let t = line.trim();
    !t.is_empty() && t.chars().any(|c| !c.is_control())
}

pub fn model_meta_key(task_key: &str, provider: &str) -> String {
    format!("{MODEL_META_PREFIX}{task_key}:{provider}")
}

fn model_meta_legacy_key(task_key: &str) -> String {
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
/// the footer and under-input strip. Pin / hint / `[tui] providers`,
/// then last run, then the first enabled provider. Not "last run first"
/// (that disagreed with `/model` when the session had moved on).
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
    if let Ok(Some(pin)) = shared.db.meta_get(&format!("tui-pin:{key}")) {
        let pin = pin.trim();
        if !pin.is_empty() {
            return Some(pin.to_string());
        }
    }
    let names = shared.config.read().unwrap().tui.providers.clone();
    if names.is_empty() {
        return None;
    }
    scheduler::listed_provider_now(shared, &names)
}

pub fn session_model_for(db: &Db, key: &str, provider: &str) -> Option<String> {
    let provider = provider.trim();
    if provider.is_empty() {
        return None;
    }
    db.meta_get(&model_meta_key(key, provider))
        .ok()
        .flatten()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// `/model` override for the session's **current** provider only.
pub fn session_model(shared: &Shared, key: &str) -> Option<String> {
    let p = session_provider_now(shared, key)?;
    session_model_for(&shared.db, key, &p)
}

/// Last run that used this provider name (not a foreign provider's model).
/// Catalog junk (`Available`, …) is dropped — never a spawn `--model`.
pub(crate) fn last_run_model_for(shared: &Shared, key: &str, provider: &str) -> Option<String> {
    let runs = shared.db.list_runs(Some(key)).ok()?;
    let raw = runs
        .iter()
        .rev()
        .find(|r| r.provider == provider)
        .and_then(|r| r.model.as_deref())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToString::to_string)?;
    let kind = shared
        .config
        .read()
        .ok()?
        .provider(provider)
        .map(|p| p.kind.clone())
        .unwrap_or_default();
    proxy::accept_model_name(&kind, &raw).then_some(raw)
}

/// Apply a stored `/model <id>` override onto the provider used for this turn.
pub fn apply_session_model(provider: &mut crate::config::Provider, db: &Db, key: &str) {
    if let Some(m) = session_model_for(db, key, &provider.name) {
        if proxy::accept_model_name(&provider.kind, &m) {
            provider.model = m;
        }
    }
    sanitize_spawn_model(provider);
}

/// Drop catalog-junk / alias `--model` values before CLI argv is built.
pub fn sanitize_spawn_model(provider: &mut crate::config::Provider) {
    if proxy::accept_model_name(&provider.kind, &provider.model) {
        return;
    }
    if !provider.model.trim().is_empty() {
        tracing::warn!(
            provider = %provider.name,
            kind = %provider.kind,
            model = %provider.model,
            "ignoring unusable --model"
        );
    }
    provider.model.clear();
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
        save_workspace(
            &shared,
            &key,
            &Workspace {
                kind: WorkspaceKind::None,
                repo: None,
            },
        );
    }
    let ws = resolve_workspace(&shared, &key, &opts)?;
    save_workspace(&shared, &key, &ws);
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

/// Next turn using the persisted `/repo` workspace (same as interactive Enter).
pub async fn run_session_turn(
    shared: Arc<Shared>,
    session: &str,
    text: &str,
) -> Result<TurnResult> {
    let key = tui_task_key(session)?;
    let ws = load_workspace(&shared, &key).unwrap_or(Workspace {
        kind: WorkspaceKind::None,
        repo: None,
    });
    run_turn(&shared, &key, &ws, text, None).await
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
    mut ws: Workspace,
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
        match step_line(&shared, &key, &mut ws, pin.as_deref(), &line)? {
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
            Step::OpenRepoPicker => {
                println!("{}", repo_picker_text(&shared, &ws));
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
    ws: &mut Workspace,
    pin: Option<&str>,
    line: &str,
) -> Result<Step> {
    let line = line.trim();
    if slash::is_bare_slash(line) {
        return Ok(Step::Note(String::new()));
    }
    if slash::parse_slash(line).is_none() && !is_turn_text(line) {
        return Ok(Step::Note(String::new()));
    }
    match slash::parse_slash(line) {
        None => Ok(Step::Turn(line.to_string())),
        Some(slash::Slash::Quit) => Ok(Step::Quit),
        Some(slash::Slash::Help) => Ok(Step::Note(slash::slash_help().to_string())),
        Some(slash::Slash::Status) => Ok(Step::Note(status_text(shared, key, ws))),
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
        Some(slash::Slash::Repo(None)) => {
            reload_tui_config(shared);
            Ok(Step::OpenRepoPicker)
        }
        Some(slash::Slash::Repo(Some(spec))) => apply_repo_pick(shared, key, ws, &spec),
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
    if let Some((kind, _, pname)) = proxy::current_kind_cli(shared, key) {
        if !proxy::accept_model_name(&kind, name) {
            return Ok(Step::Note(format!(
                "no model matching {name:?}.\n{}",
                slash::model_pick_usage(&pname, &kind, &list)
            )));
        }
    }
    set_model_note(shared, key, name)
}

fn set_model_note(shared: &Shared, key: &str, name: &str) -> Result<Step> {
    store_session_model(shared, key, name)?;
    let ws = load_workspace(shared, key).unwrap_or(Workspace {
        kind: WorkspaceKind::None,
        repo: None,
    });
    Ok(Step::Note(format!(
        "model {name} (this session; next turn uses the provider --model flag)\n{}",
        status_text(shared, key, &ws)
    )))
}

pub(crate) fn apply_repo_pick(
    shared: &Shared,
    key: &str,
    ws: &mut Workspace,
    input: &str,
) -> Result<Step> {
    reload_tui_config(shared);
    let list = repo_choices(shared, ws);
    let tokens: Vec<String> = list.iter().map(|r| r.token.clone()).collect();
    let spec = slash::pick_choice(&tokens, input).unwrap_or_else(|| input.trim().to_string());
    if spec.is_empty() {
        return Ok(Step::OpenRepoPicker);
    }
    match resolve_repo_spec(shared, &spec) {
        Ok(next) => {
            switch_workspace(shared, key, ws, next)?;
            Ok(Step::Note(workspace_chrome_line(shared, ws)))
        }
        Err(e) => Ok(Step::Note(format!(
            "{e:#}.\n{}",
            slash::repo_pick_usage(&list)
        ))),
    }
}

fn repo_picker_text(shared: &Shared, ws: &Workspace) -> String {
    reload_tui_config(shared);
    slash::repo_pick_usage(&repo_choices(shared, ws))
}

pub(crate) fn repo_choices(shared: &Shared, ws: &Workspace) -> Vec<slash::RepoChoice> {
    let cfg = shared.config.read().unwrap().clone();
    let current = ws
        .repo
        .as_deref()
        .filter(|_| ws.kind == WorkspaceKind::Git)
        .map(Path::new);
    slash::list_repos(&cfg, current, &load_recent_repos(shared))
}

fn model_picker_text(shared: &Shared, key: &str, ws: &Workspace) -> String {
    let list = proxy::collect_models(shared, key, ws, proxy::CliFetch::Blocking);
    let (kind, _, pname) = proxy::current_kind_cli(shared, key)
        .unwrap_or_else(|| ("?".into(), Default::default(), "none".into()));
    slash::model_pick_usage(&pname, &kind, &list)
}

/// Raw-mode Ctrl+C: always exit atr (same as `/quit`). Never "cancel the
/// turn and stay in the TUI". Esc still dismisses overlays only.
pub fn classify_interrupt() -> &'static str {
    "quit"
}

/// Local FIFO for composer lines parked while a turn is busy.
/// Never written to the live provider stdin or the task message table
/// until the current turn is idle and the line is submitted again.
#[derive(Debug, Default, Clone)]
pub struct PendingTurns {
    lines: VecDeque<String>,
}

impl PendingTurns {
    pub fn park(&mut self, line: &str) -> bool {
        let t = line.trim();
        if t.is_empty() {
            return false;
        }
        self.lines.push_back(t.to_string());
        true
    }

    pub fn pop(&mut self) -> Option<String> {
        self.lines.pop_front()
    }

    pub fn len(&self) -> usize {
        self.lines.len()
    }

    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }
}

/// Enter while a provider turn is live: host slashes stay live; chat
/// and proxied `/…` park in [`PendingTurns`] (Grok / Claude / Codex style).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BusyEnter {
    Ignore,
    Host,
    Queue,
}

pub fn busy_enter_action(line: &str) -> BusyEnter {
    let line = line.trim();
    if slash::is_bare_slash(line) || (!is_turn_text(line) && slash::parse_slash(line).is_none()) {
        return BusyEnter::Ignore;
    }
    match slash::parse_slash(line) {
        Some(slash::Slash::Help)
        | Some(slash::Slash::Quit)
        | Some(slash::Slash::Status)
        | Some(slash::Slash::New)
        | Some(slash::Slash::Provider(_))
        | Some(slash::Slash::Model(_))
        | Some(slash::Slash::Repo(_))
        | Some(slash::Slash::Cli) => BusyEnter::Host,
        None | Some(slash::Slash::Proxy(_)) => BusyEnter::Queue,
    }
}

/// Park a busy-Enter chat line. Host slashes are not parked.
pub fn park_busy_line(pending: &mut PendingTurns, line: &str) -> BusyEnter {
    let action = busy_enter_action(line);
    if action == BusyEnter::Queue {
        pending.park(line);
    }
    action
}

/// Submit the oldest parked line the same way idle Enter does (`run_turn`).
pub async fn submit_parked_turn(
    shared: Arc<Shared>,
    session: &str,
    pending: &mut PendingTurns,
) -> Result<Option<TurnResult>> {
    let Some(line) = pending.pop() else {
        return Ok(None);
    };
    let key = tui_task_key(session)?;
    let ws = load_workspace(&shared, &key).unwrap_or(Workspace {
        kind: WorkspaceKind::None,
        repo: None,
    });
    Ok(Some(run_turn(&shared, &key, &ws, &line, None).await?))
}

/// What a TUI line would do. Tests lock `/model` out of the turn path.
pub fn classify_line(line: &str) -> &'static str {
    let line = line.trim();
    if slash::is_bare_slash(line) {
        return "slash-menu";
    }
    if !is_turn_text(line) && slash::parse_slash(line).is_none() {
        return "ignore";
    }
    match slash::parse_slash(line) {
        None => "chat",
        Some(slash::Slash::Help) => "help",
        Some(slash::Slash::Quit) => "quit",
        Some(slash::Slash::Status) => "status",
        Some(slash::Slash::New) => "new",
        Some(slash::Slash::Provider(_)) => "provider",
        Some(slash::Slash::Model(_)) => "model",
        Some(slash::Slash::Repo(_)) => "repo",
        Some(slash::Slash::Cli) => "cli",
        Some(slash::Slash::Proxy(_)) => "proxy",
    }
}

fn store_session_model(shared: &Shared, key: &str, model: &str) -> Result<()> {
    let Some(provider) = session_provider_now(shared, key) else {
        bail!("no provider for session {key}");
    };
    let model = model.trim();
    let kind = shared
        .config
        .read()
        .unwrap()
        .provider(&provider)
        .map(|p| p.kind.clone());
    if let Some(kind) = kind {
        if !proxy::accept_model_name(&kind, model) {
            bail!("not a usable model id: {model}");
        }
    }
    shared.db.meta_set(&model_meta_key(key, &provider), model)?;
    Ok(())
}

pub fn set_session_model(shared: &Shared, key: &str, model: &str) -> Result<()> {
    store_session_model(shared, key, model)
}

/// Run a TUI slash/chat classify+apply without spawning a turn.
/// Returns `turn` only when the line would enqueue a provider run.
pub fn preview_line(shared: &Shared, key: &str, line: &str) -> Result<String> {
    let mut ws = load_workspace(shared, key).unwrap_or(Workspace {
        kind: WorkspaceKind::None,
        repo: None,
    });
    match step_line(shared, key, &mut ws, None, line)? {
        Step::Turn(_) => Ok("turn".into()),
        Step::Note(s) => Ok(format!("note\n{s}")),
        Step::OpenModelPicker => Ok(format!(
            "model-picker\n{}",
            model_picker_text(shared, key, &ws)
        )),
        Step::OpenProviderPicker => Ok("provider-picker".into()),
        Step::OpenRepoPicker => Ok(format!("repo-picker\n{}", repo_picker_text(shared, &ws))),
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
    let status = composer_status_line(shared, key);
    let ws = load_workspace(shared, key).unwrap_or(Workspace {
        kind: WorkspaceKind::None,
        repo: None,
    });
    let workspace = workspace_chrome_line(shared, &ws);
    let head = match session_chrome_label(key) {
        None => format!("atr tui · {status} · {workspace}"),
        Some(name) => format!("atr tui  {name} · {status} · {workspace}"),
    };
    format!("{head}\n{BANNER_COMMANDS}\n")
}

/// Second welcome line. Host slashes stay visible here; `/repo` is one of them.
pub const BANNER_COMMANDS: &str = "/  host commands · /repo workspace · /cli provider TUI · /quit";

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

fn resolve_workspace(shared: &Shared, key: &str, opts: &TuiOpts) -> Result<Workspace> {
    if let Some(d) = &opts.dir {
        let p = task::resolve_dir(&d.display().to_string())?;
        return Ok(Workspace {
            kind: WorkspaceKind::Dir,
            repo: Some(p.display().to_string()),
        });
    }
    if let Some(r) = opts.repo.as_deref() {
        return resolve_repo_spec(shared, r);
    }
    if let Some(ws) = load_workspace(shared, key) {
        return Ok(ws);
    }
    Ok(Workspace {
        kind: WorkspaceKind::None,
        repo: None,
    })
}

fn reload_tui_config(shared: &Shared) {
    if let Err(e) = shared.reload() {
        tracing::warn!(error = %e, "tui config reload skipped");
    }
}

/// Same resolve as `atr tui --repo` / `resolve_repo_filter`, plus `.` and `none`.
/// Config entries that are not git roots are refused (no mega-repo).
fn resolve_repo_spec(shared: &Shared, spec: &str) -> Result<Workspace> {
    let spec = spec.trim();
    if spec.is_empty() {
        bail!("empty /repo spec");
    }
    if spec.eq_ignore_ascii_case("none") {
        return Ok(Workspace {
            kind: WorkspaceKind::None,
            repo: None,
        });
    }
    if spec == "." {
        let Some(root) = slash::cwd_git_root() else {
            bail!("current directory is not inside a git repository");
        };
        return Ok(Workspace {
            kind: WorkspaceKind::Git,
            repo: Some(root.display().to_string()),
        });
    }
    let cfg = shared.config.read().unwrap().clone();
    if let Some(entry) = cfg.resolve_repo_filter(spec) {
        if crate::worktree::is_git_repo(&entry.path) {
            return Ok(Workspace {
                kind: WorkspaceKind::Git,
                repo: Some(entry.path.display().to_string()),
            });
        }
        // Shorthand mega-folder (`repo_path=~/src`) is not one repo. Fall
        // through to a one-level child name or a typed git path.
    }
    if let Some(p) = slash::mega_folder_child(&cfg, spec) {
        return Ok(Workspace {
            kind: WorkspaceKind::Git,
            repo: Some(p.display().to_string()),
        });
    }
    let p = task::resolve_git_repo(spec)?;
    Ok(Workspace {
        kind: WorkspaceKind::Git,
        repo: Some(p.display().to_string()),
    })
}

fn switch_workspace(shared: &Shared, key: &str, ws: &mut Workspace, next: Workspace) -> Result<()> {
    if workspace_same(ws, &next) {
        return Ok(());
    }
    // Any existing cli-* row (even a leftover answer-only `_none` tree)
    // must die so the next spawn cannot resume that cwd / grok session.
    if shared.db.get_task(key)?.is_some() {
        reset_session(shared, key)?;
    }
    if next.kind == WorkspaceKind::Git {
        if let Some(p) = next.repo.as_deref() {
            remember_recent_repo(shared, p);
        }
    }
    *ws = next;
    save_workspace(shared, key, ws);
    Ok(())
}

fn workspace_same(a: &Workspace, b: &Workspace) -> bool {
    if a.kind != b.kind {
        return false;
    }
    match (a.repo.as_deref(), b.repo.as_deref()) {
        (None, None) => true,
        (Some(x), Some(y)) => crate::config::paths_eq(Path::new(x), Path::new(y)),
        _ => false,
    }
}

/// Drop a leftover `_none` tree / provider session when chrome is now git/dir
/// so the next dispatch cannot resume answer-only cwd.
fn reconcile_task_workspace(shared: &Shared, key: &str, ws: &Workspace) -> Result<()> {
    let Some(t) = shared.db.get_task(key)? else {
        return Ok(());
    };
    if workspace_matches_task(&t, ws) {
        return Ok(());
    }
    let running = t.status == "running" || shared.inflight.lock().unwrap().contains_key(key);
    if running {
        return Ok(());
    }
    forget_task_sessions(shared, key)?;
    shared.db.clear_worktree(key)?;
    Ok(())
}

fn workspace_matches_task(t: &crate::db::Task, ws: &Workspace) -> bool {
    let kind = WorkspaceKind::parse(&t.workspace_kind)
        .unwrap_or_else(|_| task::infer_workspace_kind(t.repo.as_deref().unwrap_or("")));
    if kind != ws.kind {
        return false;
    }
    match ws.kind {
        WorkspaceKind::None => !t
            .worktree
            .as_deref()
            .is_some_and(|w| !crate::worktree::is_none_workspace(Path::new(w)) && !w.is_empty()),
        WorkspaceKind::Dir | WorkspaceKind::Git => {
            let want = ws.repo.as_deref().unwrap_or("");
            let have = t.repo.as_deref().unwrap_or("");
            !want.is_empty()
                && crate::config::paths_eq(Path::new(have), Path::new(want))
                && t.worktree
                    .as_deref()
                    .is_none_or(|w| !crate::worktree::is_none_workspace(Path::new(w)))
        }
    }
}

fn forget_task_sessions(shared: &Shared, key: &str) -> Result<()> {
    let cfg = shared.config.read().unwrap().clone();
    for p in &cfg.providers {
        let _ = shared.db.set_provider_session(key, Some(&p.name), None);
    }
    let _ = shared.db.set_provider_session(key, None, None);
    Ok(())
}

fn save_workspace(shared: &Shared, key: &str, ws: &Workspace) {
    let value = match ws.kind {
        WorkspaceKind::None => "none".to_string(),
        WorkspaceKind::Dir => format!("dir\t{}", ws.repo.as_deref().unwrap_or("")),
        WorkspaceKind::Git => format!("git\t{}", ws.repo.as_deref().unwrap_or("")),
    };
    let _ = shared
        .db
        .meta_set(&format!("{WS_META_PREFIX}{key}"), &value);
}

fn load_workspace(shared: &Shared, key: &str) -> Option<Workspace> {
    let raw = shared
        .db
        .meta_get(&format!("{WS_META_PREFIX}{key}"))
        .ok()??;
    let raw = raw.trim();
    if raw.is_empty() || raw == "none" {
        return Some(Workspace {
            kind: WorkspaceKind::None,
            repo: None,
        });
    }
    let (kind, path) = raw.split_once('\t').unwrap_or((raw, ""));
    let repo = path.trim();
    match kind {
        "dir" if !repo.is_empty() => Some(Workspace {
            kind: WorkspaceKind::Dir,
            repo: Some(repo.to_string()),
        }),
        "git" if !repo.is_empty() => Some(Workspace {
            kind: WorkspaceKind::Git,
            repo: Some(repo.to_string()),
        }),
        "none" => Some(Workspace {
            kind: WorkspaceKind::None,
            repo: None,
        }),
        _ => None,
    }
}

pub(crate) fn load_recent_repos(shared: &Shared) -> Vec<String> {
    let raw = shared
        .db
        .meta_get(REPO_RECENT_META)
        .ok()
        .flatten()
        .unwrap_or_default();
    raw.lines()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToString::to_string)
        .collect()
}

fn remember_recent_repo(shared: &Shared, path: &str) {
    let path = path.trim();
    if path.is_empty() {
        return;
    }
    let mut items = load_recent_repos(shared);
    items.retain(|p| !crate::config::paths_eq(Path::new(p), Path::new(path)));
    items.insert(0, path.to_string());
    items.truncate(REPO_RECENT_MAX);
    let _ = shared.db.meta_set(REPO_RECENT_META, &items.join("\n"));
}

/// Honest workspace chrome: `answer-only` | `dir <path>` | `git <name|path> [@ branch]`.
/// Never paints process cwd as the workspace when [`WorkspaceKind::None`].
pub(crate) fn workspace_chrome_line(shared: &Shared, ws: &Workspace) -> String {
    let mut line = workspace_kind_text(shared, ws);
    if tui_serve_loaded(shared) {
        line.push_str(" · serve");
    }
    line
}

/// Test helper: chrome for an explicit kind/path (ignores session meta).
pub fn workspace_chrome_for(shared: &Shared, kind: &str, repo: Option<&str>) -> Result<String> {
    let ws = match kind {
        "none" => Workspace {
            kind: WorkspaceKind::None,
            repo: None,
        },
        "dir" => Workspace {
            kind: WorkspaceKind::Dir,
            repo: Some(repo.unwrap_or("").to_string()),
        },
        "git" => Workspace {
            kind: WorkspaceKind::Git,
            repo: Some(repo.unwrap_or("").to_string()),
        },
        other => bail!("unknown workspace kind {other}"),
    };
    Ok(workspace_chrome_line(shared, &ws))
}

fn workspace_kind_text(shared: &Shared, ws: &Workspace) -> String {
    match ws.kind {
        WorkspaceKind::None => "answer-only".into(),
        WorkspaceKind::Dir => {
            let path = ws.repo.as_deref().unwrap_or("");
            format!("dir {}", composer_path_line(Path::new(path)))
        }
        WorkspaceKind::Git => {
            let path = ws.repo.as_deref().unwrap_or("");
            let p = Path::new(path);
            let cfg = shared.config.read().unwrap().clone();
            let label = cfg
                .repo_for_path(p)
                .map(|r| r.name)
                .filter(|n| !n.is_empty())
                .unwrap_or_else(|| composer_path_line(p));
            let mut s = format!("git {label}");
            if let Some(b) = crate::worktree::current_branch(p) {
                s.push_str(" @ ");
                s.push_str(&b);
            }
            if cfg.effective_repos().iter().any(|r| {
                r.enabled
                    && crate::worktree::is_git_repo(&r.path)
                    && crate::config::paths_eq(&r.path, p)
            }) {
                s.push_str(" (watched)");
            }
            s
        }
    }
}

fn tui_serve_loaded(shared: &Shared) -> bool {
    let label = crate::service::launchd_label_for(&shared.config_path);
    crate::service::service_loaded(&label)
        || crate::service::service_loaded(crate::service::LAUNCHD_LABEL)
}

fn status_text(shared: &Shared, key: &str, ws: &Workspace) -> String {
    let chrome = session_chrome(shared, key).footer_line(key);
    let workspace = workspace_chrome_line(shared, ws);
    if chrome.is_empty() {
        workspace
    } else {
        format!("{chrome} · {workspace}")
    }
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
    let _ = shared.db.meta_set(&model_meta_legacy_key(key), "");
    let cfg = shared.config.read().unwrap().clone();
    for p in &cfg.providers {
        let _ = shared.db.meta_set(&model_meta_key(key, &p.name), "");
    }
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
    reconcile_task_workspace(shared, key, ws)?;
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

/// Assistant bubble for the **latest** run only. Never falls back to
/// `tasks.result_text` (that can be an older success — Rs-MBP painted
/// Claude's "stray keystroke" over a dead grok run).
pub fn collect_result(shared: &Shared, key: &str) -> TurnResult {
    let runs = shared.db.list_runs(Some(key)).unwrap_or_default();
    let last = runs.last();
    let ok = last
        .and_then(|r| r.outcome.as_deref())
        .is_some_and(|o| o == "success");
    let from_events = last.and_then(|r| {
        let events = shared.db.events_for_run(r.id).ok()?;
        crate::handoff::last_assistant_text(&events)
    });
    let text = if let Some(t) = from_events.filter(|s| !s.trim().is_empty()) {
        t
    } else {
        last.and_then(|r| r.outcome.clone())
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "(no assistant text)".into())
    };
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

struct SessionChrome {
    provider: String,
    model: String,
    extras: Vec<String>,
}

impl SessionChrome {
    /// `provider / model` plus cheap tok / cache extras.
    fn status_line(&self) -> String {
        let mut parts = vec![format!("{} / {}", self.provider, self.model)];
        parts.extend(self.extras.iter().cloned());
        parts.join(" · ")
    }

    fn footer_line(&self, key: &str) -> String {
        match session_chrome_label(key) {
            None => self.status_line(),
            Some(name) => format!("{name} · {}", self.status_line()),
        }
    }
}

fn session_chrome(shared: &Shared, key: &str) -> SessionChrome {
    let cfg = shared.config.read().unwrap().clone();
    let runs = shared.db.list_runs(Some(key)).unwrap_or_default();
    let last = runs.last();
    let provider = session_provider_now(shared, key).unwrap_or_else(|| "—".into());
    let (kind, cli) = cfg
        .provider(&provider)
        .map(|p| (p.kind.clone(), p.cli.clone()))
        .unwrap_or_default();
    let model = proxy::resolve_display_model(
        &kind,
        (!cli.as_os_str().is_empty()).then_some(cli.as_path()),
        &[
            session_model_for(&shared.db, key, &provider),
            last_run_model_for(shared, key, &provider),
            cfg.provider(&provider)
                .map(|p| p.model.clone())
                .filter(|s| !s.is_empty()),
            cfg.provider(&provider)
                .and_then(|p| crate::detect::cli_default_model(&p.kind)),
        ],
    );
    let mut extras = Vec::new();
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
                extras.push(format!("{tok} tok"));
            }
            let kind = cfg
                .provider(&provider)
                .map(|p| p.kind.as_str())
                .unwrap_or("");
            if let Some(hit) = cache_hit_ratio(kind, &u) {
                extras.push(format!("cache {:.0}%", hit * 100.0));
            }
        }
    }
    SessionChrome {
        provider,
        model,
        extras,
    }
}

/// Line-mode / `/status` chrome: `key · provider / model · tok · cache · workspace`.
/// Same session provider+model as [`composer_status_line`].
pub fn footer_line(shared: &Shared, key: &str) -> String {
    let ws = load_workspace(shared, key).unwrap_or(Workspace {
        kind: WorkspaceKind::None,
        repo: None,
    });
    status_text(shared, key, &ws)
}

/// Under-composer primary strip: `provider / model` plus tok / cache.
/// Same source as [`footer_line`] (this provider's session model, then
/// this provider's last run, then config / detect).
pub fn composer_status_line(shared: &Shared, key: &str) -> String {
    session_chrome(shared, key).status_line()
}

/// Quiet workspace path for the under-composer strip (`~/…` when under `$HOME`).
pub fn composer_path_line(path: &Path) -> String {
    shorten_home_path(path, std::env::var_os("HOME").as_deref())
}

pub(crate) fn shorten_home_path(path: &Path, home: Option<&OsStr>) -> String {
    let display = path.to_string_lossy();
    let Some(home) = home.filter(|h| !h.is_empty()) else {
        return display.into_owned();
    };
    let home = Path::new(home);
    if path == home {
        return "~".into();
    }
    match path.strip_prefix(home) {
        Ok(rest) if rest.as_os_str().is_empty() => "~".into(),
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => display.into_owned(),
    }
}

pub(crate) fn truncate_middle_end(s: &str, max: usize) -> String {
    let n = s.chars().count();
    if n <= max || max < 2 {
        return s.to_string();
    }
    let keep = max.saturating_sub(1);
    format!("…{}", s.chars().skip(n - keep).collect::<String>())
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
        assert_eq!(
            model_meta_key("cli-default", "cursor"),
            "tui-model:cli-default:cursor"
        );
        assert_ne!(
            model_meta_key("cli-default", "cursor"),
            model_meta_key("cli-default", "claude")
        );
        assert_eq!(
            model_meta_legacy_key("cli-default"),
            "tui-model:cli-default"
        );
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
        assert_eq!(classify_line("what can you do for me?"), "chat");
        assert_eq!(classify_line("/provider"), "provider");
        assert_eq!(classify_line("/repo"), "repo");
        assert_eq!(classify_line("/repo none"), "repo");
        assert_eq!(classify_line("/repo ."), "repo");
        assert_eq!(classify_line("/"), "slash-menu");
        assert_eq!(classify_line(" / "), "slash-menu");
        assert_eq!(classify_line(""), "ignore");
        assert_eq!(classify_line("   \t"), "ignore");
        assert_eq!(classify_line("\u{0001}\u{0007}"), "ignore");
        assert_eq!(classify_interrupt(), "quit");
    }

    #[test]
    fn english_prompt_is_a_provider_turn() {
        assert!(is_turn_text("what can you do for me?"));
        assert!(is_turn_text("w"));
        assert!(is_turn_text("?"));
        assert!(!is_turn_text(""));
        assert!(!is_turn_text(" \n"));
        assert!(!is_turn_text("\u{0001}"));
    }

    #[test]
    fn busy_enter_queues_chat_keeps_host_slashes() {
        assert_eq!(busy_enter_action("hello"), BusyEnter::Queue);
        assert_eq!(busy_enter_action("/compact"), BusyEnter::Queue);
        assert_eq!(busy_enter_action("/repo"), BusyEnter::Host);
        assert_eq!(busy_enter_action("/model"), BusyEnter::Host);
        assert_eq!(busy_enter_action("/quit"), BusyEnter::Host);
        assert_eq!(busy_enter_action(""), BusyEnter::Ignore);
        let mut q = PendingTurns::default();
        assert_eq!(park_busy_line(&mut q, "first"), BusyEnter::Queue);
        assert_eq!(park_busy_line(&mut q, "second"), BusyEnter::Queue);
        assert_eq!(park_busy_line(&mut q, "/repo"), BusyEnter::Host);
        assert_eq!(q.len(), 2);
        assert_eq!(q.pop().as_deref(), Some("first"));
        assert_eq!(q.pop().as_deref(), Some("second"));
        assert!(q.pop().is_none());
    }

    #[test]
    fn default_session_chrome_omits_cli_default() {
        assert_eq!(session_chrome_label("cli-default"), None);
        assert_eq!(session_title("cli-default"), " atr tui ");
        assert_eq!(session_chrome_label("cli-work"), Some("work"));
        assert_eq!(session_title("cli-work"), " atr tui  work ");
    }

    #[test]
    fn home_path_uses_tilde() {
        let home = Path::new("/Users/rimas");
        let home_os = OsStr::new("/Users/rimas");
        assert_eq!(shorten_home_path(home, Some(home_os)), "~");
        assert_eq!(
            shorten_home_path(&home.join("Projects/github/rimusz"), Some(home_os)),
            "~/Projects/github/rimusz"
        );
        assert_eq!(
            shorten_home_path(Path::new("/opt/src"), Some(home_os)),
            "/opt/src"
        );
        assert_eq!(
            shorten_home_path(Path::new("/Users/rimas/x"), None),
            "/Users/rimas/x"
        );
        assert_eq!(truncate_middle_end("abcdefghij", 6), "…fghij");
        assert_eq!(truncate_middle_end("short", 12), "short");
    }

    #[test]
    fn ask_prompt_required_mentions_ask_and_tui() {
        assert!(ASK_PROMPT_REQUIRED.contains("atr ask"));
        assert!(ASK_PROMPT_REQUIRED.contains("atr tui"));
    }

    #[test]
    fn banner_helpers_are_tight() {
        assert!(BANNER_COMMANDS.contains("/repo"), "{BANNER_COMMANDS}");
        assert!(BANNER_COMMANDS.contains("/cli"), "{BANNER_COMMANDS}");
        assert!(BANNER_COMMANDS.contains("/quit"), "{BANNER_COMMANDS}");
        assert!(format_daemon_line(true).contains("atr status"));
        assert!(format_daemon_line(false).contains("atr service install"));
        assert!(daemon_line(std::path::Path::new("/tmp")).contains("atr"));
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

    fn grok_provider(model: &str) -> crate::config::Provider {
        let cfg = crate::config::parse_config_toml(&format!(
            r#"
[[provider]]
name = "grok"
kind = "grok"
model = "{model}"
enabled = true
"#
        ))
        .unwrap();
        cfg.provider("grok").unwrap().clone()
    }

    #[test]
    fn available_is_never_a_spawn_model() {
        let mut p = grok_provider("Available");
        sanitize_spawn_model(&mut p);
        assert!(
            p.model.is_empty(),
            "Available must not reach --model: {:?}",
            p.model
        );
        let mut ok = grok_provider("grok-4.7");
        sanitize_spawn_model(&mut ok);
        assert_eq!(ok.model, "grok-4.7");
    }
}
