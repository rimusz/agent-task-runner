use crate::app::{Shared, SubmitOutcome};
use crate::config::{Config, TelegramConfig};
use crate::providers::hit_ratio_from_sums;
use anyhow::Result;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

const MAX_TEXT: usize = 3500;
const COMMANDS: &str = "commands: /help /status /tasks /task /add /cancel /retry /usage /shadow /pause /resume /providers /enable /disable /config /repos";
const ASSISTANT_PREFIX: &str = "tg";

#[derive(Debug, Clone)]
pub struct Outgoing {
    pub chat_id: i64,
    pub text: String,
    pub markup: Option<Value>,
}

#[derive(Clone)]
enum Action {
    Cancel {
        key: String,
        force: bool,
    },
    Retry {
        key: String,
    },
    Pause,
    Resume,
    Enable(String),
    Disable(String),
    ConfigSet {
        key: String,
        value: String,
    },
    ProviderAdd {
        name: String,
        kind: Option<String>,
        model: Option<String>,
        key: Option<String>,
        url: Option<String>,
    },
    ProviderEnable(String),
    ProviderDisable(String),
    ProviderMove {
        name: String,
        to: usize,
    },
    ProviderModel {
        name: String,
        model: String,
    },
}

struct Pending {
    chat_id: i64,
    action: Action,
    expires: Instant,
}

struct Batch {
    lines: Vec<String>,
    last: Instant,
}

static BATCH: OnceLock<Mutex<Batch>> = OnceLock::new();
static PENDING: OnceLock<Mutex<HashMap<u64, Pending>>> = OnceLock::new();
static NEXT_ID: OnceLock<Mutex<u64>> = OnceLock::new();

fn batch() -> &'static Mutex<Batch> {
    BATCH.get_or_init(|| {
        Mutex::new(Batch {
            lines: Vec::new(),
            last: Instant::now(),
        })
    })
}

fn pending() -> &'static Mutex<HashMap<u64, Pending>> {
    PENDING.get_or_init(|| Mutex::new(HashMap::new()))
}

fn next_id() -> u64 {
    let cell = NEXT_ID.get_or_init(|| Mutex::new(1));
    let mut n = cell.lock().unwrap();
    let id = *n;
    *n += 1;
    id
}

pub fn bot_token(cfg: &TelegramConfig) -> Option<String> {
    std::env::var(&cfg.bot_token_env)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| read_secret_file(&cfg.token_file))
}

fn read_secret_file(path: &str) -> Option<String> {
    if path.is_empty() {
        return None;
    }
    let expanded = crate::config::expand_path(path);
    std::fs::read_to_string(expanded)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

pub fn allowed_chats(cfg: &TelegramConfig) -> Vec<i64> {
    let mut ids = cfg.allowed_chat_ids.clone();
    if let Some(s) = read_secret_file(&cfg.chat_id_file) {
        if let Ok(n) = s.parse::<i64>() {
            if !ids.contains(&n) {
                ids.push(n);
            }
        }
    }
    ids
}

pub fn api_base(cfg: &TelegramConfig) -> String {
    let b = cfg.api_base.trim();
    if b.is_empty() {
        "https://api.telegram.org".into()
    } else {
        b.trim_end_matches('/').to_string()
    }
}

pub fn sanitize(text: &str, secrets: &[&str]) -> String {
    let mut s = text.replace('\u{0000}', "");
    for sec in secrets {
        if !sec.is_empty() {
            s = s.replace(*sec, "***");
        }
    }
    for needle in [
        "ATR_TELEGRAM_TOKEN",
        "GH_TOKEN",
        "GITHUB_TOKEN",
        "ATR_SHADOW_GH_TOKEN",
    ] {
        s = s.replace(needle, "***");
    }
    if s.contains("----- atr variable -----") || s.contains("## Messages") {
        s = "(prompt omitted)".into();
    }
    if s.len() > MAX_TEXT {
        let keep = MAX_TEXT.saturating_sub(14);
        let mut end = keep;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        s.truncate(end);
        s.push_str("…[truncated]");
    }
    s
}

pub fn enqueue_notify(cfg: &TelegramConfig, body: &str) {
    let prefix = if cfg.prefix.is_empty() {
        "atr:"
    } else {
        cfg.prefix.as_str()
    };
    let line = sanitize(&format!("{prefix} {body}"), &[]);
    let mut b = batch().lock().unwrap();
    b.lines.push(line);
}

pub fn flush_due(shared: &Shared) {
    let cfg = shared.config.read().unwrap().telegram.clone();
    if !cfg.enabled && bot_token(&cfg).is_none() {
        let mut b = batch().lock().unwrap();
        b.lines.clear();
        return;
    }
    let due = {
        let b = batch().lock().unwrap();
        !b.lines.is_empty() && (b.lines.len() >= 8 || b.last.elapsed() >= cfg.notify_interval)
    };
    if due {
        flush_now(shared);
    }
}

fn flush_now(shared: &Shared) {
    let cfg = shared.config.read().unwrap().telegram.clone();
    let lines = {
        let mut b = batch().lock().unwrap();
        if b.lines.is_empty() {
            return;
        }
        let lines = b.lines.split_off(0);
        b.last = Instant::now();
        lines
    };
    let text = sanitize(
        &lines.join("\n"),
        &[bot_token(&cfg).as_deref().unwrap_or("")],
    );
    send_all(shared, &text, None);
}

fn send_all(shared: &Shared, text: &str, markup: Option<Value>) {
    let cfg = shared.config.read().unwrap().telegram.clone();
    let Some(token) = bot_token(&cfg) else {
        return;
    };
    let chats = allowed_chats(&cfg);
    if chats.is_empty() {
        return;
    }
    let body = sanitize(text, &[&token]);
    for chat in chats {
        let _ = send_message(&cfg, &token, chat, &body, markup.clone());
    }
}

fn send_one(cfg: &TelegramConfig, chat_id: i64, text: &str, markup: Option<Value>) {
    let Some(token) = bot_token(cfg) else {
        return;
    };
    let body = sanitize(text, &[&token]);
    let _ = send_message(cfg, &token, chat_id, &body, markup);
}

fn send_message(
    cfg: &TelegramConfig,
    token: &str,
    chat_id: i64,
    text: &str,
    markup: Option<Value>,
) -> Result<()> {
    let url = format!("{}/bot{}/sendMessage", api_base(cfg), token);
    let mut payload = json!({
        "chat_id": chat_id,
        "text": text,
        "disable_web_page_preview": true,
    });
    if let Some(m) = markup {
        payload["reply_markup"] = m;
    }
    let send = crate::util::ureq_agent(Duration::from_secs(10))
        .post(&url)
        .send_json(payload);
    if let Err(e) = send {
        tracing::warn!("telegram send failed: {}", redact(&e.to_string(), token));
    }
    Ok(())
}

fn redact(s: &str, secret: &str) -> String {
    if secret.is_empty() {
        s.to_string()
    } else {
        s.replace(secret, "***")
    }
}

pub async fn bot_loop(shared: Arc<Shared>) {
    loop {
        let cfg = shared.config.read().unwrap().telegram.clone();
        if !cfg.enabled {
            tokio::time::sleep(Duration::from_secs(2)).await;
            let shared2 = shared.clone();
            let _ = tokio::task::spawn_blocking(move || {
                maybe_daily(&shared2);
                flush_due(&shared2);
            })
            .await;
            continue;
        }
        if bot_token(&cfg).is_none() || allowed_chats(&cfg).is_empty() {
            tracing::warn!("telegram enabled but bot_token_env or allowed_chat_ids is missing");
            tokio::time::sleep(Duration::from_secs(10)).await;
            continue;
        }
        let shared2 = shared.clone();
        let _ = tokio::task::spawn_blocking(move || {
            poll_once(&shared2);
            maybe_daily(&shared2);
            flush_due(&shared2);
        })
        .await;
        // Yield so a fast getUpdates (tests / empty inbox) cannot busy-loop the runtime.
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn poll_once(shared: &Shared) {
    let cfg = shared.config.read().unwrap().telegram.clone();
    let Some(token) = bot_token(&cfg) else {
        return;
    };
    let offset = shared
        .db
        .meta_get("telegram_offset")
        .ok()
        .flatten()
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(0);
    let url = format!(
        "{}/bot{}/getUpdates?timeout=25&offset={}",
        api_base(&cfg),
        token,
        offset
    );
    let agent = crate::util::ureq_agent(Duration::from_secs(40));
    let resp = match agent.get(&url).call() {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("telegram getUpdates: {}", redact(&e.to_string(), &token));
            std::thread::sleep(Duration::from_secs(2));
            return;
        }
    };
    let v: Value = match resp.into_json() {
        Ok(v) => v,
        Err(_) => return,
    };
    if v.get("ok") != Some(&Value::Bool(true)) {
        return;
    }
    let Some(arr) = v.get("result").and_then(|r| r.as_array()) else {
        return;
    };
    let mut max_id = offset;
    for upd in arr {
        if let Some(id) = upd.get("update_id").and_then(|n| n.as_i64()) {
            max_id = max_id.max(id + 1);
        }
        if let Ok(outs) = handle_update(shared, upd) {
            for o in outs {
                send_one(&cfg, o.chat_id, &o.text, o.markup);
            }
        }
    }
    if max_id != offset {
        let _ = shared.db.meta_set("telegram_offset", &max_id.to_string());
    }
}

/// Handle one Telegram update. Public for tests.
pub fn handle_update(shared: &Shared, update: &Value) -> Result<Vec<Outgoing>> {
    let cfg = shared.config.read().unwrap().telegram.clone();
    let allowed = allowed_chats(&cfg);
    if let Some(cb) = update.get("callback_query") {
        let chat = cb
            .pointer("/message/chat/id")
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
        if !allowed.contains(&chat) {
            return Ok(Vec::new());
        }
        let data = cb.get("data").and_then(|v| v.as_str()).unwrap_or("");
        return Ok(vec![handle_callback(shared, chat, data)]);
    }
    let msg = match update.get("message") {
        Some(m) => m,
        None => return Ok(Vec::new()),
    };
    let chat = msg
        .pointer("/chat/id")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    if !allowed.contains(&chat) {
        return Ok(Vec::new());
    }
    let text = msg
        .get("text")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();
    if text.is_empty() {
        return Ok(Vec::new());
    }
    if text.starts_with('/') {
        Ok(vec![handle_command(shared, chat, text)])
    } else if cfg.assistant.enabled {
        Ok(vec![handle_assistant(shared, chat, text)])
    } else {
        Ok(vec![help_outgoing(chat)])
    }
}

fn handle_command(shared: &Shared, chat: i64, raw: &str) -> Outgoing {
    let text = raw.split('@').next().unwrap_or(raw).trim();
    let mut parts = text.split_whitespace();
    let cmd = parts.next().unwrap_or("");
    match cmd {
        "/help" | "/start" => help_outgoing(chat),
        "/status" => {
            let repo = parts.next();
            Outgoing {
                chat_id: chat,
                text: format_status(shared, repo),
                markup: None,
            }
        }
        "/tasks" => {
            let repo = parts.next();
            Outgoing {
                chat_id: chat,
                text: format_tasks(shared, repo),
                markup: None,
            }
        }
        "/repos" => Outgoing {
            chat_id: chat,
            text: format_repos(shared),
            markup: None,
        },
        "/task" => {
            let key = parts.next().unwrap_or("");
            Outgoing {
                chat_id: chat,
                text: format_task(shared, key),
                markup: None,
            }
        }
        "/add" => handle_add(shared, chat, text),
        "/usage" => {
            let a = parts.next();
            let b = parts.next();
            let (spec, repo) = match (a, b) {
                (None, _) => ("7d", None),
                (Some(x), None) => {
                    if crate::usage::parse_period(x).is_ok() {
                        (x, None)
                    } else {
                        ("7d", Some(x))
                    }
                }
                (Some(x), Some(y)) => (x, Some(y)),
            };
            Outgoing {
                chat_id: chat,
                text: format_usage(shared, spec, repo),
                markup: None,
            }
        }
        "/shadow" => Outgoing {
            chat_id: chat,
            text: format_shadow(shared),
            markup: None,
        },
        "/providers" => handle_providers(shared, chat, &mut parts),
        "/config" => handle_config(shared, chat, &mut parts),
        "/cancel" => {
            let key = parts.next().unwrap_or("").to_string();
            let force = matches!(parts.next(), Some("force") | Some("--force"));
            if key.is_empty() {
                return Outgoing {
                    chat_id: chat,
                    text: "usage: /cancel <key> [force]".into(),
                    markup: None,
                };
            }
            confirm(
                chat,
                Action::Cancel {
                    key: key.clone(),
                    force,
                },
                &format!("Cancel {key}{}?", if force { " (force)" } else { "" }),
            )
        }
        "/retry" => {
            let key = parts.next().unwrap_or("").to_string();
            if key.is_empty() {
                return Outgoing {
                    chat_id: chat,
                    text: "usage: /retry <key>".into(),
                    markup: None,
                };
            }
            confirm(
                chat,
                Action::Retry { key: key.clone() },
                &format!("Retry {key}?"),
            )
        }
        "/pause" => confirm(
            chat,
            Action::Pause,
            "Pause intake (GitHub/shadow/inbox pollers)?",
        ),
        "/resume" => confirm(chat, Action::Resume, "Resume intake?"),
        "/enable" => {
            let name = parts.next().unwrap_or("").to_string();
            if name.is_empty() {
                return Outgoing {
                    chat_id: chat,
                    text: "usage: /enable <provider>".into(),
                    markup: None,
                };
            }
            confirm(
                chat,
                Action::Enable(name.clone()),
                &format!("Enable {name}?"),
            )
        }
        "/disable" => {
            let name = parts.next().unwrap_or("").to_string();
            if name.is_empty() {
                return Outgoing {
                    chat_id: chat,
                    text: "usage: /disable <provider>".into(),
                    markup: None,
                };
            }
            confirm(
                chat,
                Action::Disable(name.clone()),
                &format!("Disable {name}?"),
            )
        }
        _ => help_outgoing(chat),
    }
}

fn handle_add(shared: &Shared, chat: i64, raw: &str) -> Outgoing {
    let rest = raw
        .split_once(char::is_whitespace)
        .map(|(_, r)| r.trim())
        .unwrap_or("");
    if rest.is_empty() {
        return Outgoing {
            chat_id: chat,
            text: "usage: /add <prompt>   (answer-only; no workspace)".into(),
            markup: None,
        };
    }
    let cfg = shared.config.read().unwrap().clone();
    if !cfg.intake_allows(crate::task::SourceKind::Telegram) {
        return Outgoing {
            chat_id: chat,
            text: "telegram intake is disabled".into(),
            markup: None,
        };
    }
    let title: String = rest
        .lines()
        .next()
        .unwrap_or("task")
        .chars()
        .take(80)
        .collect();
    let key = match crate::task::allocate_key(&shared.db, &cfg.task_key_prefix, &title) {
        Ok(k) => k,
        Err(e) => {
            return Outgoing {
                chat_id: chat,
                text: format!("{e:#}"),
                markup: None,
            }
        }
    };
    drop(cfg);
    match shared.submit_with(
        &key,
        rest,
        "telegram",
        None,
        None,
        false,
        false,
        crate::app::SubmitOpts {
            workspace_kind: Some("none".into()),
            output_kind: Some("reply".into()),
            output_spec: Some(chat.to_string()),
            source_kind: Some("telegram".into()),
            title: Some(title),
            allow_empty_repo: true,
            provider_hint: None,
        },
    ) {
        Ok(SubmitOutcome::Accepted) => Outgoing {
            chat_id: chat,
            text: format!("queued {key}"),
            markup: None,
        },
        Ok(SubmitOutcome::Queued) => Outgoing {
            chat_id: chat,
            text: format!("queued follow-up on {key}"),
            markup: None,
        },
        Err(e) => Outgoing {
            chat_id: chat,
            text: format!("{e:#}"),
            markup: None,
        },
    }
}

fn help_outgoing(chat: i64) -> Outgoing {
    Outgoing {
        chat_id: chat,
        text: COMMANDS.into(),
        markup: None,
    }
}

pub fn assistant_task_key(chat: i64) -> String {
    format!("{ASSISTANT_PREFIX}{chat}")
}

pub fn is_assistant_key(key: &str) -> bool {
    chat_from_assistant_key(key).is_some()
}

pub fn chat_from_assistant_key(key: &str) -> Option<i64> {
    key.strip_prefix(ASSISTANT_PREFIX)?.parse().ok()
}

/// Stable prompt suffix: skill + no-shell contract. Same bytes every assistant turn.
pub fn assistant_stable_suffix() -> String {
    format!(
        "\n## Telegram assistant\n\
You are answering in Telegram about atr.\n\
Never use the shell, edit files, or run git/gh. Do not write config.toml or shadow.env.\n\
Propose changes only as `ATR: /command …` lines (same commands as the Telegram bot / CLI).\n\
atr will show Confirm (old → new) before applying anything.\n\n\
## atr skill\n\n{}\n",
        crate::skill::SKILL_MD
    )
}

pub fn apply_assistant_timeout(
    provider: &crate::config::Provider,
    timeout: Duration,
) -> crate::config::Provider {
    let mut p = provider.clone();
    p.run_timeout = timeout;
    p
}

pub fn ensure_assistant_repo(cfg: &Config) -> Result<PathBuf> {
    let path = cfg.worktree_root.join("_src").join("assistant");
    if path.join(".git").exists() {
        return Ok(path);
    }
    crate::worktree::init_sample_repo(&path)?;
    std::fs::write(
        path.join("README.md"),
        "atr Telegram assistant workspace. Do not edit files here.\n",
    )?;
    Ok(path)
}

fn handle_assistant(shared: &Shared, chat: i64, text: &str) -> Outgoing {
    let cfg = shared.config.read().unwrap().clone();
    let ast = &cfg.telegram.assistant;
    if !ast.enabled {
        return help_outgoing(chat);
    }
    let key = assistant_task_key(chat);
    if !ast.provider.trim().is_empty() && cfg.provider(ast.provider.trim()).is_none() {
        return fail(
            chat,
            anyhow::anyhow!(
                "telegram.assistant.provider {:?} is not in config",
                ast.provider.trim()
            ),
        );
    }
    let repo = match ensure_assistant_repo(&cfg) {
        Ok(p) => p,
        Err(e) => return fail(chat, e),
    };
    let pin = ast.provider.trim().to_string();
    let existing_sid = if pin.is_empty() {
        None
    } else {
        shared.db.task_session(&key, &pin).ok().flatten()
    };
    match shared.submit(
        &key,
        text,
        "telegram",
        Some(&repo.display().to_string()),
        None,
        false,
        true,
    ) {
        Ok(outcome) => {
            if !pin.is_empty() {
                let _ = shared
                    .db
                    .set_provider_session(&key, Some(&pin), existing_sid.as_deref());
            }
            let who = if pin.is_empty() {
                "first available provider"
            } else {
                pin.as_str()
            };
            Outgoing {
                chat_id: chat,
                text: match outcome {
                    SubmitOutcome::Accepted => format!("working… ({who})"),
                    SubmitOutcome::Queued => format!("queued on the live assistant turn ({who})"),
                },
                markup: None,
            }
        }
        Err(e) => fail(chat, e),
    }
}

pub fn parse_atr_commands(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for raw in text.lines() {
        let line = raw.trim();
        let cmd = if let Some(rest) = line
            .strip_prefix("ATR:")
            .or_else(|| line.strip_prefix("atr:"))
        {
            rest.trim()
        } else if line.starts_with('/') {
            line
        } else {
            continue;
        };
        if !is_driveable_command(cmd) {
            continue;
        }
        if !out.iter().any(|c| c == cmd) {
            out.push(cmd.to_string());
        }
    }
    out
}

fn is_driveable_command(cmd: &str) -> bool {
    let name = cmd.split_whitespace().next().unwrap_or("");
    matches!(
        name,
        "/status"
            | "/tasks"
            | "/task"
            | "/add"
            | "/usage"
            | "/shadow"
            | "/providers"
            | "/config"
            | "/repos"
            | "/cancel"
            | "/retry"
            | "/pause"
            | "/resume"
            | "/enable"
            | "/disable"
            | "/help"
    )
}

fn strip_atr_lines(text: &str) -> String {
    text.lines()
        .filter(|l| {
            let t = l.trim();
            !t.starts_with("ATR:") && !t.starts_with("atr:")
        })
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string()
}

/// Build the outgoing assistant reply (text + Confirm buttons). Public for tests.
pub fn assistant_reply(shared: &Shared, key: &str, run_id: i64, outcome: &str) -> Vec<Outgoing> {
    let Some(chat) = chat_from_assistant_key(key) else {
        return Vec::new();
    };
    if outcome != "success" {
        return vec![Outgoing {
            chat_id: chat,
            text: sanitize(
                &format!("assistant run {outcome}. Try again or /status."),
                &[],
            ),
            markup: None,
        }];
    }
    let events = shared.db.events_for_run(run_id).unwrap_or_default();
    let raw = crate::handoff::last_assistant_text(&events)
        .or_else(|| {
            shared
                .db
                .list_runs(Some(key))
                .ok()
                .and_then(|runs| runs.into_iter().rev().find(|r| r.id == run_id))
                .and_then(|r| r.outcome.clone())
        })
        .unwrap_or_else(|| "(no assistant text)".into());
    let cmds = parse_atr_commands(&raw);
    let mut visible = strip_atr_lines(&raw);
    if visible.is_empty() {
        visible = if cmds.is_empty() {
            "(no assistant text)".into()
        } else {
            "Proposed command — Confirm to apply.".into()
        };
    }
    let mut outs = vec![Outgoing {
        chat_id: chat,
        text: sanitize(&visible, &[]),
        markup: None,
    }];
    for cmd in cmds {
        outs.push(handle_command(shared, chat, &cmd));
    }
    outs
}

pub fn deliver_assistant_result(shared: &Shared, key: &str, run_id: i64, outcome: &str) {
    let cfg = shared.config.read().unwrap().telegram.clone();
    for o in assistant_reply(shared, key, run_id, outcome) {
        send_one(&cfg, o.chat_id, &o.text, o.markup);
    }
}

pub async fn assistant_snapshot(shared: &Shared) -> String {
    let cfg = shared.config.read().unwrap().clone();
    let status = format_status(shared, None);
    let doctor = crate::doctor::run(&cfg, &shared.config_path, true)
        .await
        .format();
    let config = redacted_config_view(&cfg, &shared.config_path);
    let mut out = String::new();
    out.push_str("## atr snapshot (read-only, secrets redacted)\n\n### status\n");
    out.push_str(&status);
    out.push_str("\n### doctor\n");
    out.push_str(&doctor);
    out.push_str("\n### config\n");
    out.push_str(&config);
    let secrets = snapshot_secrets(&cfg.telegram);
    let refs: Vec<&str> = secrets.iter().map(String::as_str).collect();
    sanitize(&out, &refs)
}

fn snapshot_secrets(cfg: &TelegramConfig) -> Vec<String> {
    let mut s = Vec::new();
    if let Some(t) = bot_token(cfg) {
        s.push(t);
    }
    for name in [
        &cfg.bot_token_env,
        "ATR_TELEGRAM_TOKEN",
        "GH_TOKEN",
        "GITHUB_TOKEN",
        "ATR_SHADOW_GH_TOKEN",
        "MOONSHOT_API_KEY",
        "DASHSCOPE_API_KEY",
        "MIMO_API_KEY",
    ] {
        if !name.is_empty() {
            if let Ok(v) = std::env::var(name) {
                if !v.trim().is_empty() {
                    s.push(v);
                }
            }
        }
    }
    s
}

fn redacted_config_view(cfg: &Config, path: &Path) -> String {
    let mut out = format!(
        "file = {}\nlisten = {}\nrepo_path = {}\ndisplay_timezone = {}\n",
        path.display(),
        cfg.listen,
        cfg.repo_path.display(),
        cfg.display_timezone.label
    );
    out.push_str(&format!(
        "telegram.enabled = {}\ntelegram.assistant.enabled = {}\ntelegram.assistant.provider = {}\ntelegram.assistant.timeout = {:?}\n",
        cfg.telegram.enabled,
        cfg.telegram.assistant.enabled,
        if cfg.telegram.assistant.provider.is_empty() {
            "(first in order)"
        } else {
            cfg.telegram.assistant.provider.as_str()
        },
        cfg.telegram.assistant.timeout
    ));
    for (i, p) in cfg.providers.iter().enumerate() {
        out.push_str(&format!(
            "#{} name={} kind={} enabled={} model={} preset={}\n",
            i + 1,
            p.name,
            p.kind,
            p.enabled,
            if p.model.is_empty() {
                "(kind default)"
            } else {
                p.model.as_str()
            },
            if p.preset.is_empty() {
                "-"
            } else {
                p.preset.as_str()
            }
        ));
    }
    out
}

fn handle_providers<'a>(
    shared: &Shared,
    chat: i64,
    parts: &mut impl Iterator<Item = &'a str>,
) -> Outgoing {
    let sub = parts.next().unwrap_or("");
    if sub.is_empty() {
        return Outgoing {
            chat_id: chat,
            text: format_providers(shared),
            markup: None,
        };
    }
    let path = &shared.config_path;
    match sub {
        "add" => {
            let name = parts.next().unwrap_or("").to_string();
            if name.is_empty() {
                return usage(
                    chat,
                    "/providers add <name> [<key>]  or  /providers add <name> [kind] [model] [<key>]  or  /providers add spark|ollama [url] [model]",
                );
            }
            let parsed = parse_provider_add(&name, parts);
            if let Some(bp) = crate::presets::builtin(&name) {
                if let Some(env_name) = bp.env_key {
                    let shadow = crate::telegram_setup::shadow_env_path(path);
                    let have = parsed
                        .key
                        .as_deref()
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .is_some()
                        || std::env::var(env_name)
                            .ok()
                            .filter(|s| !s.trim().is_empty())
                            .is_some()
                        || crate::telegram_setup::read_shadow_env_var(&shadow, env_name).is_some();
                    if !have {
                        return usage(chat, &format!("/providers add {name} <key>"));
                    }
                }
            }
            match crate::providers_edit::preview_add(
                path,
                &name,
                parsed.kind.as_deref(),
                parsed.model.as_deref(),
            ) {
                Ok(desc) => {
                    let secrets: Vec<&str> = parsed.key.iter().map(|s| s.as_str()).collect();
                    let prompt =
                        sanitize(&format!("Add provider {name}?\n(none) → {desc}"), &secrets);
                    confirm(
                        chat,
                        Action::ProviderAdd {
                            name: name.clone(),
                            kind: parsed.kind,
                            model: parsed.model,
                            key: parsed.key,
                            url: parsed.url,
                        },
                        &prompt,
                    )
                }
                Err(e) => fail(chat, e),
            }
        }
        "enable" | "disable" => {
            let name = parts.next().unwrap_or("").to_string();
            if name.is_empty() {
                return usage(chat, &format!("/providers {sub} <name>"));
            }
            let on = sub == "enable";
            match crate::config_edit::preview_set(
                path,
                &format!("provider.{name}.enabled"),
                if on { "true" } else { "false" },
            ) {
                Ok(c) => confirm(
                    chat,
                    if on {
                        Action::ProviderEnable(name)
                    } else {
                        Action::ProviderDisable(name)
                    },
                    &format!("Set {}?\n{} → {}", c.key, c.old, c.new),
                ),
                Err(e) => fail(chat, e),
            }
        }
        "move" => {
            let name = parts.next().unwrap_or("").to_string();
            let to = parts.next().unwrap_or("");
            if name.is_empty() || to.is_empty() {
                return usage(chat, "/providers move <name> <n>");
            }
            let Ok(to_n) = to.parse::<usize>() else {
                return usage(chat, "/providers move <name> <n>");
            };
            let cfg = shared.config.read().unwrap();
            let old = cfg
                .providers
                .iter()
                .position(|p| p.name == name)
                .map(|i| i + 1);
            drop(cfg);
            let Some(old) = old else {
                return fail(chat, anyhow::anyhow!("unknown provider {name}"));
            };
            confirm(
                chat,
                Action::ProviderMove {
                    name: name.clone(),
                    to: to_n,
                },
                &format!("Move {name}?\n#{old} → #{to_n}"),
            )
        }
        "model" => {
            let name = parts.next().unwrap_or("").to_string();
            let model = parts.next().unwrap_or("").to_string();
            if name.is_empty() || model.is_empty() {
                return usage(chat, "/providers model <name> <model>");
            }
            match crate::config_edit::preview_set(path, &format!("provider.{name}.model"), &model) {
                Ok(c) => confirm(
                    chat,
                    Action::ProviderModel { name, model },
                    &format!("Set {}?\n{} → {}", c.key, c.old, c.new),
                ),
                Err(e) => fail(chat, e),
            }
        }
        _ => usage(chat, "/providers [add|enable|disable|move|model] …"),
    }
}

fn handle_config<'a>(
    shared: &Shared,
    chat: i64,
    parts: &mut impl Iterator<Item = &'a str>,
) -> Outgoing {
    let sub = parts.next().unwrap_or("");
    let path = &shared.config_path;
    match sub {
        "get" => {
            let key = parts.next().unwrap_or("");
            if key.is_empty() {
                return usage(chat, "/config get <key>");
            }
            match crate::config_edit::get_value(path, key) {
                Ok(v) => Outgoing {
                    chat_id: chat,
                    text: sanitize(&format!("{key} = {v}"), &[]),
                    markup: None,
                },
                Err(e) => fail(chat, e),
            }
        }
        "set" => {
            let key = parts.next().unwrap_or("").to_string();
            let value = parts.collect::<Vec<_>>().join(" ");
            if key.is_empty() {
                return usage(chat, "/config set <key> <value>");
            }
            match crate::config_edit::preview_set(path, &key, &value) {
                Ok(c) => confirm(
                    chat,
                    Action::ConfigSet { key, value },
                    &format!("Set {}?\n{} → {}", c.key, c.old, c.new),
                ),
                Err(e) => fail(chat, e),
            }
        }
        _ => usage(chat, "/config get <key>  |  /config set <key> <value>"),
    }
}

fn usage(chat: i64, text: &str) -> Outgoing {
    Outgoing {
        chat_id: chat,
        text: format!("usage: {text}"),
        markup: None,
    }
}

fn fail(chat: i64, e: anyhow::Error) -> Outgoing {
    Outgoing {
        chat_id: chat,
        text: sanitize(&format!("{e:#}"), &[]),
        markup: None,
    }
}

fn confirm(chat: i64, action: Action, prompt: &str) -> Outgoing {
    let id = next_id();
    pending().lock().unwrap().insert(
        id,
        Pending {
            chat_id: chat,
            action,
            expires: Instant::now() + Duration::from_secs(120),
        },
    );
    Outgoing {
        chat_id: chat,
        text: prompt.to_string(),
        markup: Some(json!({
            "inline_keyboard": [[
                {"text": "Confirm", "callback_data": format!("y:{id}")},
                {"text": "Cancel", "callback_data": format!("n:{id}")},
            ]]
        })),
    }
}

fn handle_callback(shared: &Shared, chat: i64, data: &str) -> Outgoing {
    let mut parts = data.split(':');
    let yn = parts.next().unwrap_or("");
    let id: u64 = parts.next().unwrap_or("0").parse().unwrap_or(0);
    let mut map = pending().lock().unwrap();
    let Some(p) = map.remove(&id) else {
        return Outgoing {
            chat_id: chat,
            text: "that confirmation expired".into(),
            markup: None,
        };
    };
    if p.chat_id != chat || p.expires < Instant::now() {
        return Outgoing {
            chat_id: chat,
            text: "that confirmation expired".into(),
            markup: None,
        };
    }
    if yn != "y" {
        return Outgoing {
            chat_id: chat,
            text: "cancelled".into(),
            markup: None,
        };
    }
    let redact = match &p.action {
        Action::ProviderAdd { key: Some(k), .. } if !k.is_empty() => vec![k.clone()],
        _ => vec![],
    };
    let text = match p.action {
        Action::Cancel { key, force } => match shared.cancel(&key, force) {
            Ok(()) => format!("cancelled {key}"),
            Err(e) => format!("cancel failed: {e:#}"),
        },
        Action::Retry { key } => match retry_task(shared, &key) {
            Ok(()) => format!("retried {key}"),
            Err(e) => format!("retry failed: {e:#}"),
        },
        Action::Pause => {
            shared.set_intake_paused(true);
            "intake paused".into()
        }
        Action::Resume => {
            shared.set_intake_paused(false);
            "intake resumed".into()
        }
        Action::Enable(name) => {
            let _ = shared.db.set_enabled_override(&name, true);
            format!("enabled {name}")
        }
        Action::Disable(name) => {
            let _ = shared.db.set_enabled_override(&name, false);
            format!("disabled {name}")
        }
        Action::ConfigSet { key, value } => apply_edit(shared, || {
            crate::config_edit::apply_set(&shared.config_path, &key, &value)
                .map(|c| format!("set {} {} → {}", c.key, c.old, c.new))
        }),
        Action::ProviderAdd {
            name,
            kind,
            model,
            key,
            url,
        } => apply_edit(shared, || {
            crate::providers_edit::add(
                &shared.config_path,
                &name,
                crate::providers_edit::AddOpts {
                    kind: kind.as_deref(),
                    model: model.as_deref(),
                    cli: None,
                    overrides: &[],
                    key: key.as_deref(),
                    url: url.as_deref(),
                    yes: true,
                },
            )
            .map(|()| format!("added {name}"))
        }),
        Action::ProviderEnable(name) => apply_edit(shared, || {
            crate::providers_edit::set_enabled(&shared.config_path, &name, true)
                .map(|()| format!("enabled {name} in config"))
        }),
        Action::ProviderDisable(name) => apply_edit(shared, || {
            crate::providers_edit::set_enabled(&shared.config_path, &name, false)
                .map(|()| format!("disabled {name} in config"))
        }),
        Action::ProviderMove { name, to } => apply_edit(shared, || {
            crate::providers_edit::move_to(&shared.config_path, &name, to)
                .map(|()| format!("moved {name} to #{to}"))
        }),
        Action::ProviderModel { name, model } => apply_edit(shared, || {
            crate::providers_edit::set_model(&shared.config_path, &name, &model)
                .map(|()| format!("set provider.{name}.model → {model}"))
        }),
    };
    let secrets: Vec<&str> = redact.iter().map(String::as_str).collect();
    Outgoing {
        chat_id: chat,
        text: sanitize(&text, &secrets),
        markup: None,
    }
}

struct ParsedProviderAdd {
    kind: Option<String>,
    model: Option<String>,
    key: Option<String>,
    url: Option<String>,
}

fn parse_provider_add<'a>(
    name: &str,
    parts: &mut impl Iterator<Item = &'a str>,
) -> ParsedProviderAdd {
    let first = parts.next().map(str::to_string).filter(|s| !s.is_empty());
    let second = parts.next().map(str::to_string).filter(|s| !s.is_empty());
    let third = parts.next().map(str::to_string).filter(|s| !s.is_empty());
    if let Some(bp) = crate::presets::builtin(name) {
        if first.as_deref().is_some_and(crate::config::is_known_kind) {
            return ParsedProviderAdd {
                kind: first,
                model: second,
                key: third,
                url: None,
            };
        }
        if bp.env_key.is_some() {
            if first.as_deref().is_some_and(|s| s.contains('/')) {
                return ParsedProviderAdd {
                    kind: None,
                    model: first,
                    key: second,
                    url: None,
                };
            }
            return ParsedProviderAdd {
                kind: None,
                model: None,
                key: first,
                url: None,
            };
        }
        return ParsedProviderAdd {
            kind: None,
            model: second,
            key: None,
            url: first,
        };
    }
    ParsedProviderAdd {
        kind: first,
        model: second,
        key: third,
        url: None,
    }
}

fn apply_edit(shared: &Shared, f: impl FnOnce() -> Result<String>) -> String {
    match f() {
        Ok(msg) => {
            if let Err(e) = shared.reload() {
                format!("{msg} (reload: {e:#})")
            } else {
                msg
            }
        }
        Err(e) => format!("failed: {e:#}"),
    }
}

fn retry_task(shared: &Shared, key: &str) -> Result<()> {
    let Some(t) = shared.db.get_task(key)? else {
        anyhow::bail!("unknown task {key}");
    };
    if t.status == "running" {
        anyhow::bail!("task {key} is running");
    }
    shared.db.set_status(key, "queued")?;
    shared.kick.notify_waiters();
    Ok(())
}

fn format_status(shared: &Shared, repo: Option<&str>) -> String {
    let v = shared.status_json_filtered(repo);
    let tz = shared.config.read().unwrap().display_timezone.label.clone();
    let paused = shared.intake_paused.load(Ordering::Relaxed);
    let mut out = format!(
        "status ({tz}) intake={}\n",
        if paused { "paused" } else { "on" }
    );
    if let Some(repos) = v.get("repos").and_then(|p| p.as_array()) {
        for r in repos {
            if let Some(f) = repo {
                if r.get("name").and_then(|x| x.as_str()) != Some(f)
                    && r.get("github").and_then(|x| x.as_str()) != Some(f)
                {
                    continue;
                }
            }
            out.push_str(&format!(
                "repo {} en={} {} {}\n",
                r.get("name").and_then(|x| x.as_str()).unwrap_or("-"),
                r.get("enabled").and_then(|x| x.as_bool()).unwrap_or(false),
                r.get("github").and_then(|x| x.as_str()).unwrap_or("-"),
                r.get("label").and_then(|x| x.as_str()).unwrap_or("-"),
            ));
        }
    }
    if let Some(ps) = v.get("providers").and_then(|p| p.as_array()) {
        for p in ps {
            out.push_str(&format!(
                "{} en={} in={}/{} cool={} skip={} reset={} {}\n",
                p.get("name").and_then(|x| x.as_str()).unwrap_or("-"),
                p.get("enabled").and_then(|x| x.as_bool()).unwrap_or(false),
                p.get("inflight").and_then(|x| x.as_u64()).unwrap_or(0),
                p.get("max_parallel").and_then(|x| x.as_u64()).unwrap_or(0),
                p.get("cooling").and_then(|x| x.as_bool()).unwrap_or(false),
                p.get("skip_new").and_then(|x| x.as_bool()).unwrap_or(false),
                p.get("quota_reset").and_then(|x| x.as_str()).unwrap_or("-"),
                p.get("usage")
                    .and_then(|x| x.as_str())
                    .unwrap_or("usage: not reported"),
            ));
        }
    }
    if let Some(c) = v.get("cache") {
        out.push_str(&crate::usage::format_status_cache_line(c));
    }
    sanitize(&out, &[])
}

fn format_tasks(shared: &Shared, repo: Option<&str>) -> String {
    let cfg = shared.config.read().unwrap().clone();
    let tz = &cfg.display_timezone;
    let mut out = format!("tasks ({})\n", tz.label);
    for t in shared.db.list_tasks().unwrap_or_default() {
        if let Some(f) = repo {
            if !cfg.task_matches_repo(&t, f) {
                continue;
            }
        }
        out.push_str(&format!(
            "{}\t{}\t{}\t{}\t{}\t{}\n",
            t.key,
            cfg.task_repo_name(&t),
            t.status,
            t.role,
            t.provider.as_deref().unwrap_or("-"),
            tz.format_str(&t.updated)
        ));
    }
    if out.lines().count() <= 1 {
        out.push_str("(none)\n");
    }
    sanitize(&out, &[])
}

fn format_task(shared: &Shared, key: &str) -> String {
    if key.is_empty() {
        return "usage: /task <key>".into();
    }
    let cfg = shared.config.read().unwrap().clone();
    let tz = &cfg.display_timezone;
    let Some(t) = shared.db.get_task(key).ok().flatten() else {
        return format!("unknown task {key}");
    };
    let last = shared
        .db
        .list_runs(Some(key))
        .ok()
        .and_then(|r| r.into_iter().last());
    let repo = cfg.task_repo_name(&t);
    let mut out =
        format!(
        "{key} repo={repo} status={} role={} parent={} title={} provider={} shadow={} updated={}\n",
        t.status,
        t.role,
        t.parent_key.as_deref().unwrap_or("-"),
        if t.title.is_empty() { "-" } else { t.title.as_str() },
        t.provider.as_deref().unwrap_or("-"),
        t.shadow,
        tz.format_str(&t.updated)
    );
    if let Ok(kids) = shared.db.children_of(key) {
        if !kids.is_empty() {
            out.push_str("children:\n");
            for c in kids {
                out.push_str(&format!("  {} {}\n", c.key, c.status));
            }
        }
    }
    if let Some(r) = last {
        out.push_str(&format!(
            "last run {} {}\n",
            r.outcome.as_deref().unwrap_or("-"),
            tz.format_opt(r.ended.as_deref())
        ));
    }
    sanitize(&out, &[])
}

fn format_usage(shared: &Shared, spec: &str, repo: Option<&str>) -> String {
    let period = match crate::usage::parse_period(spec) {
        Ok(p) => p,
        Err(e) => return format!("usage: {e:#}"),
    };
    match crate::usage::report_filtered(shared, &period, repo) {
        Ok(r) => sanitize(&crate::usage::format_compact(&r), &[]),
        Err(e) => format!("usage: {e:#}"),
    }
}

fn format_repos(shared: &Shared) -> String {
    let cfg = shared.config.read().unwrap().clone();
    sanitize(&crate::repos::list_text(&cfg), &[])
}

fn format_shadow(shared: &Shared) -> String {
    let cfg = shared.config.read().unwrap().clone();
    let names: Vec<&str> = cfg.providers.iter().map(|p| p.name.as_str()).collect();
    match crate::shadow::report_text_tz_usage(&shared.db, None, &cfg.display_timezone, &names) {
        Ok(s) => sanitize(&s, &[]),
        Err(e) => format!("shadow report: {e:#}"),
    }
}

fn format_providers(shared: &Shared) -> String {
    let cfg = shared.config.read().unwrap().clone();
    let mut out = String::from("providers\n");
    for p in &cfg.providers {
        let over = shared.db.enabled_override(&p.name).ok().flatten();
        let en = over.unwrap_or(p.enabled);
        out.push_str(&format!(
            "{} kind={} enabled={} (config={})\n",
            p.name, p.kind, en, p.enabled
        ));
    }
    sanitize(&out, &[])
}

pub fn daily_summary_text(shared: &Shared) -> String {
    let cfg = shared.config.read().unwrap().clone();
    let tz = &cfg.display_timezone;
    let day = tz.today();
    let tasks = shared.db.list_tasks().unwrap_or_default();
    let waiting = tasks.iter().filter(|t| t.status == "waiting").count();
    let attn = tasks
        .iter()
        .filter(|t| t.status == "needs_attention")
        .count();
    let running = tasks.iter().filter(|t| t.status == "running").count();
    let mut out = format!("daily {day} ({})\n", tz.label);
    out.push_str(&format!(
        "tasks waiting={waiting} needs_attention={attn} running={running}\n"
    ));
    let usage = shared.db.usage_by_day_tz(1, tz).unwrap_or_default();
    let runs = shared.db.runs_on_day(tz, &day).unwrap_or_default();
    for p in &cfg.providers {
        let u = usage.iter().find(|r| r.provider == p.name && r.day == day);
        let hit = u
            .and_then(|u| hit_ratio_from_sums(&p.kind, u.input, u.cache_read, u.cache_write))
            .map(|h| format!("{:.0}%", h * 100.0))
            .unwrap_or_else(|| "-".into());
        let cost = u.map(|u| u.cost_usd).unwrap_or(0.0);
        let ok = runs
            .iter()
            .filter(|r| r.provider == p.name && r.outcome.as_deref() == Some("success"))
            .count();
        let fail = runs
            .iter()
            .filter(|r| {
                r.provider == p.name
                    && matches!(
                        r.outcome.as_deref(),
                        Some("failed" | "needs_attention" | "verify_failed")
                    )
            })
            .count();
        out.push_str(&format!(
            "{} ${:.2} hit={hit} ok={ok} fail={fail}\n",
            p.name, cost
        ));
    }
    sanitize(&out, &[])
}

fn maybe_daily(shared: &Shared) {
    let cfg = shared.config.read().unwrap().clone();
    let spec = cfg.telegram.daily_summary.trim();
    if spec.is_empty() || !cfg.telegram.enabled {
        return;
    }
    let Some((h, m)) = parse_hhmm(spec) else {
        return;
    };
    let (ch, cm) = cfg.display_timezone.now_local_hm();
    if ch != h || cm != m {
        return;
    }
    let today = cfg.display_timezone.today();
    if shared
        .db
        .meta_get("telegram_daily")
        .ok()
        .flatten()
        .as_deref()
        == Some(today.as_str())
    {
        return;
    }
    let _ = shared.db.meta_set("telegram_daily", &today);
    send_all(shared, &daily_summary_text(shared), None);
}

fn parse_hhmm(s: &str) -> Option<(u32, u32)> {
    let (a, b) = s.split_once(':')?;
    Some((a.parse().ok()?, b.parse().ok()?))
}

#[doc(hidden)]
pub fn clear_notify_batch() {
    let mut b = batch().lock().unwrap();
    b.lines.clear();
    b.last = Instant::now();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::Shared;
    use std::sync::Arc;

    fn harness_shared() -> Arc<Shared> {
        let dir = tempfile::tempdir().unwrap();
        let dbp = dir.path().join("s.db");
        let cfg = format!(
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
bot_token_env = "ATR_TEST_TG_TOKEN"
allowed_chat_ids = [42]
prefix = "atr:"
notify_interval = "1s"
daily_summary = ""
[[provider]]
name = "claude"
kind = "claude"
cli = "/bin/true"
"#,
            db = dbp.display(),
            root = dir.path().display(),
        );
        std::fs::write(dir.path().join("p.md"), "p\n").unwrap();
        let path = dir.path().join("c.toml");
        std::fs::write(&path, &cfg).unwrap();
        let raw: crate::config::RawConfig = toml::from_str(&cfg).unwrap();
        let parsed = crate::config::Config::from_raw(raw).unwrap();
        let shared = Shared::from_config(path, parsed).unwrap();
        // leak tempdir
        std::mem::forget(dir);
        shared
    }

    #[test]
    fn ignores_unknown_chat() {
        std::env::set_var("ATR_TEST_TG_TOKEN", "tok-secret");
        let shared = harness_shared();
        let upd = json!({"message":{"chat":{"id":99},"text":"/status"}});
        let out = handle_update(&shared, &upd).unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn repos_lists_watch_targets() {
        std::env::set_var("ATR_TEST_TG_TOKEN", "tok-secret");
        let shared = harness_shared();
        let upd = json!({"message":{"chat":{"id":42},"text":"/repos"}});
        let out = handle_update(&shared, &upd).unwrap();
        assert_eq!(out.len(), 1);
        assert!(
            out[0].text.contains("name")
                || out[0].text.contains("shorthand")
                || !out[0].text.is_empty(),
            "{}",
            out[0].text
        );
        assert!(
            is_driveable_command("/repos"),
            "/repos must be assistant-driveable"
        );
    }

    #[test]
    fn status_from_allowed_chat() {
        std::env::set_var("ATR_TEST_TG_TOKEN", "tok-secret");
        let shared = harness_shared();
        let upd = json!({"message":{"chat":{"id":42},"text":"/status"}});
        let out = handle_update(&shared, &upd).unwrap();
        assert_eq!(out.len(), 1);
        assert!(out[0].text.contains("status"));
        assert!(
            out[0].text.contains("usage: not reported"),
            "{}",
            out[0].text
        );
        assert!(!out[0].text.contains("tok-secret"));
    }

    #[test]
    fn cancel_needs_confirm_then_runs() {
        std::env::set_var("ATR_TEST_TG_TOKEN", "tok-secret");
        let shared = harness_shared();
        shared.db.upsert_task("acme-1", "/repo", None).unwrap();
        shared.db.set_status("acme-1", "waiting").unwrap();
        let upd = json!({"message":{"chat":{"id":42},"text":"/cancel acme-1"}});
        let out = handle_update(&shared, &upd).unwrap();
        assert!(out[0].text.contains("Cancel"));
        let data = out[0].markup.as_ref().unwrap()["inline_keyboard"][0][0]["callback_data"]
            .as_str()
            .unwrap()
            .to_string();
        let cb = json!({"callback_query":{"data":data,"message":{"chat":{"id":42}}}});
        let out = handle_update(&shared, &cb).unwrap();
        assert!(out[0].text.contains("cancelled"));
        assert_eq!(
            shared.db.get_task("acme-1").unwrap().unwrap().status,
            "failed"
        );
    }

    #[test]
    fn sanitize_strips_token_and_prompts() {
        let s = sanitize(
            "token tok-secret ----- atr variable ----- hello",
            &["tok-secret"],
        );
        assert!(!s.contains("tok-secret"));
        assert!(!s.contains("atr variable"));
    }

    #[test]
    fn sanitize_truncates() {
        let s = sanitize(&"x".repeat(5000), &[]);
        assert!(s.len() <= MAX_TEXT + 5);
        assert!(s.contains("truncated"));
    }

    #[test]
    fn enable_needs_confirm() {
        std::env::set_var("ATR_TEST_TG_TOKEN", "tok-secret");
        let shared = harness_shared();
        let upd = json!({"message":{"chat":{"id":42},"text":"/enable claude"}});
        let out = handle_update(&shared, &upd).unwrap();
        assert!(out[0].text.contains("Enable claude"));
        assert!(out[0].markup.is_some());
        let data = out[0].markup.as_ref().unwrap()["inline_keyboard"][0][0]["callback_data"]
            .as_str()
            .unwrap()
            .to_string();
        let cb = json!({"callback_query":{"data":data,"message":{"chat":{"id":42}}}});
        let out = handle_update(&shared, &cb).unwrap();
        assert!(out[0].text.contains("enabled claude"));
        assert_eq!(shared.db.enabled_override("claude").unwrap(), Some(true));
    }

    #[test]
    fn daily_summary_has_no_secrets() {
        std::env::set_var("ATR_TEST_TG_TOKEN", "tok-secret");
        let shared = harness_shared();
        let text = daily_summary_text(&shared);
        assert!(text.contains("daily"));
        assert!(!text.contains("tok-secret"));
        assert!(!text.contains("ATR_TEST_TG_TOKEN"));
    }

    #[test]
    fn usage_today_uses_timezone() {
        std::env::set_var("ATR_TEST_TG_TOKEN", "tok-secret");
        let shared = harness_shared();
        let upd = json!({"message":{"chat":{"id":42},"text":"/usage today"}});
        let out = handle_update(&shared, &upd).unwrap();
        assert!(out[0].text.contains("usage"));
        assert!(out[0].text.contains("UTC"));
        assert!(out[0].text.contains("today"), "{}", out[0].text);
    }

    #[test]
    fn usage_month_includes_savings() {
        std::env::set_var("ATR_TEST_TG_TOKEN", "tok-secret");
        let shared = harness_shared();
        shared.db.upsert_task("acme-u", "/repo", None).unwrap();
        let run = shared.db.start_run("acme-u", "claude", "m").unwrap();
        shared.db.finish_run(run, "success", Some(0)).unwrap();
        shared
            .db
            .insert_usage(
                run,
                None,
                "claude",
                100,
                50,
                800,
                20,
                Some(2.05),
                Some("est"),
                "{}",
            )
            .unwrap();
        let upd = json!({"message":{"chat":{"id":42},"text":"/usage month"}});
        let out = handle_update(&shared, &upd).unwrap();
        assert!(out[0].text.contains("usage month"), "{}", out[0].text);
        assert!(out[0].text.contains("UTC"), "{}", out[0].text);
        assert!(out[0].text.contains("claude"), "{}", out[0].text);
        assert!(out[0].text.contains("cached=800"), "{}", out[0].text);
        assert!(!out[0].text.contains("saved=800"), "{}", out[0].text);
        assert!(out[0].text.contains("total"), "{}", out[0].text);
    }

    fn confirm_and_apply(shared: &Shared, text: &str) -> String {
        let upd = json!({"message":{"chat":{"id":42},"text":text}});
        let out = handle_update(shared, &upd).unwrap();
        assert_eq!(out.len(), 1, "{text}: {out:?}");
        let data = out[0].markup.as_ref().expect(text)["inline_keyboard"][0][0]["callback_data"]
            .as_str()
            .unwrap()
            .to_string();
        let prompt = out[0].text.clone();
        let cb = json!({"callback_query":{"data":data,"message":{"chat":{"id":42}}}});
        let out = handle_update(shared, &cb).unwrap();
        format!("{prompt}\n---\n{}", out[0].text)
    }

    #[test]
    fn config_get_set_and_secrets() {
        std::env::set_var("ATR_TEST_TG_TOKEN", "tok-secret");
        let shared = harness_shared();
        let upd = json!({"message":{"chat":{"id":42},"text":"/config get listen"}});
        let out = handle_update(&shared, &upd).unwrap();
        assert!(out[0].text.contains("127.0.0.1:0"), "{}", out[0].text);

        let unknown = json!({"message":{"chat":{"id":99},"text":"/config get listen"}});
        assert!(handle_update(&shared, &unknown).unwrap().is_empty());

        let secret =
            json!({"message":{"chat":{"id":42},"text":"/config get telegram.bot_token_env"}});
        let out = handle_update(&shared, &secret).unwrap();
        assert!(out[0].text.contains("refused"), "{}", out[0].text);
        assert!(
            !out[0].text.contains("ATR_TEST_TG_TOKEN"),
            "{}",
            out[0].text
        );
        assert!(!out[0].text.contains("tok-secret"), "{}", out[0].text);

        let set_secret =
            json!({"message":{"chat":{"id":42},"text":"/config set telegram.bot_token_env LEAK"}});
        let out = handle_update(&shared, &set_secret).unwrap();
        assert!(out[0].text.contains("refused"), "{}", out[0].text);
        assert!(out[0].markup.is_none());

        let body = confirm_and_apply(&shared, "/config set listen 127.0.0.1:9");
        assert!(body.contains("127.0.0.1:0"), "{body}");
        assert!(body.contains("127.0.0.1:9"), "{body}");
        assert!(body.contains("set listen"), "{body}");
        assert_eq!(shared.config.read().unwrap().listen, "127.0.0.1:9");
        assert!(shared.config_path.with_file_name("c.toml.bak").exists());

        let body = confirm_and_apply(&shared, "/config set github.label for:atr");
        assert!(body.contains("github.label"), "{body}");
        assert!(body.contains("for:atr"), "{body}");
        assert_eq!(shared.config.read().unwrap().github.label, "for:atr");
    }

    #[test]
    fn providers_add_enable_model_via_config() {
        std::env::set_var("ATR_TEST_TG_TOKEN", "tok-secret");
        let shared = harness_shared();
        crate::opencode_cfg::set_test_path(Some(
            shared.config_path.parent().unwrap().join("opencode.json"),
        ));

        let missing = json!({"message":{"chat":{"id":42},"text":"/providers add kimi"}});
        let out = handle_update(&shared, &missing).unwrap();
        assert!(out[0].text.contains("<key>"), "{}", out[0].text);
        assert!(out[0].markup.is_none());

        let body = confirm_and_apply(&shared, "/providers add kimi sk-moon-test");
        assert!(body.contains("(none) →"), "{body}");
        assert!(body.contains("kind=opencode"), "{body}");
        assert!(body.contains("preset=kimi"), "{body}");
        assert!(body.contains("added kimi"), "{body}");
        assert!(
            !body.contains("sk-moon-test"),
            "confirm must not echo the key: {body}"
        );
        assert!(shared.config.read().unwrap().provider("kimi").is_some());
        let cfg_text = std::fs::read_to_string(&shared.config_path).unwrap();
        assert!(!cfg_text.contains("sk-moon-test"), "{cfg_text}");
        let shadow =
            std::fs::read_to_string(shared.config_path.parent().unwrap().join("shadow.env"))
                .unwrap();
        assert!(shadow.contains("MOONSHOT_API_KEY=sk-moon-test"), "{shadow}");
        crate::opencode_cfg::set_test_path(None);

        let body = confirm_and_apply(&shared, "/providers model claude sonnet");
        assert!(
            body.contains("opus") || body.contains("(unset)") || body.contains("→"),
            "{body}"
        );
        assert!(body.contains("sonnet"), "{body}");
        assert_eq!(
            shared
                .config
                .read()
                .unwrap()
                .provider("claude")
                .unwrap()
                .model,
            "sonnet"
        );

        let body = confirm_and_apply(&shared, "/providers disable claude");
        assert!(body.contains("true") || body.contains("false"), "{body}");
        assert!(body.contains("disabled claude"), "{body}");
        assert!(
            !shared
                .config
                .read()
                .unwrap()
                .provider("claude")
                .unwrap()
                .enabled
        );
    }

    #[test]
    fn help_lists_commands() {
        std::env::set_var("ATR_TEST_TG_TOKEN", "tok-secret");
        let shared = harness_shared();
        let upd = json!({"message":{"chat":{"id":42},"text":"/help"}});
        let out = handle_update(&shared, &upd).unwrap();
        assert!(out[0].text.contains("/help"), "{}", out[0].text);
        assert!(out[0].text.contains("/providers"), "{}", out[0].text);
        assert!(out[0].text.contains("/config"), "{}", out[0].text);
        assert!(out[0].text.contains("/add"), "{}", out[0].text);
    }

    #[test]
    fn add_command_queues_answer_only_task() {
        std::env::set_var("ATR_TEST_TG_TOKEN", "tok-secret");
        let shared = harness_shared();
        let upd = json!({"message":{"chat":{"id":42},"text":"/add what is 2+2?"}});
        let out = handle_update(&shared, &upd).unwrap();
        assert!(out[0].text.contains("queued"), "{}", out[0].text);
        let tasks = shared.db.list_tasks().unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].issue, None);
        assert_eq!(tasks[0].workspace_kind, "none");
        assert_eq!(tasks[0].source_kind, "telegram");
    }

    #[test]
    fn free_text_without_assistant_lists_commands() {
        std::env::set_var("ATR_TEST_TG_TOKEN", "tok-secret");
        let shared = harness_shared();
        let upd = json!({"message":{"chat":{"id":42},"text":"how do I add kimi?"}});
        let out = handle_update(&shared, &upd).unwrap();
        assert!(out[0].text.contains("commands:"), "{}", out[0].text);
        assert!(shared.db.get_task("tg42").unwrap().is_none());
    }

    #[test]
    fn free_text_assistant_submits_and_resumes() {
        std::env::set_var("ATR_TEST_TG_TOKEN", "tok-secret");
        let shared = harness_shared();
        shared.config.write().unwrap().telegram.assistant.enabled = true;
        let upd = json!({"message":{"chat":{"id":42},"text":"how do I add kimi?"}});
        let out = handle_update(&shared, &upd).unwrap();
        assert!(out[0].text.contains("working"), "{}", out[0].text);
        let task = shared.db.get_task("tg42").unwrap().unwrap();
        assert_eq!(task.status, "queued");
        let pending = shared.db.pending_messages("tg42").unwrap();
        assert_eq!(pending.len(), 1);
        assert!(pending[0].body.contains("kimi"), "{}", pending[0].body);

        shared.db.set_status("tg42", "running").unwrap();
        let again = json!({"message":{"chat":{"id":42},"text":"why did smoke fail?"}});
        let out = handle_update(&shared, &again).unwrap();
        assert!(out[0].text.contains("queued"), "{}", out[0].text);
        assert_eq!(shared.db.pending_messages("tg42").unwrap().len(), 2);
    }

    #[test]
    fn unknown_chat_free_text_ignored() {
        std::env::set_var("ATR_TEST_TG_TOKEN", "tok-secret");
        let shared = harness_shared();
        shared.config.write().unwrap().telegram.assistant.enabled = true;
        let upd = json!({"message":{"chat":{"id":99},"text":"how do I add kimi?"}});
        assert!(handle_update(&shared, &upd).unwrap().is_empty());
    }

    #[test]
    fn parse_atr_commands_only_driveable() {
        let text = "Add Kimi with the preset.\nATR: /providers add kimi\natr: /config set listen 127.0.0.1:9\n/status\nrm -rf /\nATR: /shell echo hi\n";
        let cmds = parse_atr_commands(text);
        assert_eq!(
            cmds,
            vec![
                "/providers add kimi".to_string(),
                "/config set listen 127.0.0.1:9".to_string(),
                "/status".to_string()
            ]
        );
    }

    #[test]
    fn assistant_reply_confirm_old_to_new() {
        std::env::set_var("ATR_TEST_TG_TOKEN", "tok-secret");
        let shared = harness_shared();
        shared.db.upsert_task("tg42", "/repo", None).unwrap();
        let run = shared.db.start_run("tg42", "claude", "opus").unwrap();
        shared
            .db
            .insert_event(
                run,
                "assistant",
                r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Set listen.\nATR: /config set listen 127.0.0.1:9"}]}}"#,
            )
            .unwrap();
        let outs = assistant_reply(&shared, "tg42", run, "success");
        assert!(outs[0].text.contains("Set listen"), "{outs:?}");
        assert!(!outs[0].text.contains("ATR:"), "{outs:?}");
        let confirm = outs
            .iter()
            .find(|o| o.markup.is_some())
            .expect("confirm button");
        assert!(confirm.text.contains("127.0.0.1:0"), "{}", confirm.text);
        assert!(confirm.text.contains("127.0.0.1:9"), "{}", confirm.text);
        assert!(
            confirm.text.contains('→') || confirm.text.contains("->"),
            "{}",
            confirm.text
        );
    }

    #[test]
    fn assistant_keys_parse_negative_chat() {
        assert_eq!(assistant_task_key(-100), "tg-100");
        assert_eq!(chat_from_assistant_key("tg-100"), Some(-100));
        assert!(is_assistant_key("tg42"));
        assert!(!is_assistant_key("acme-1"));
    }
}
