use crate::app::{Shared, SubmitOutcome};
use crate::config::{Config, TelegramConfig};
use crate::providers::hit_ratio_from_sums;
use anyhow::Result;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

const MAX_TEXT: usize = 3500;
const COMMANDS: &str = "\
atr Telegram

Watch
• status — what's running
• tasks / task <id> — queue
• usage [period] — spend & cache

Work
• add <prompt> — queue a task
• cancel / retry <id>

Control
• pause · resume · providers · repos
• enable / disable · config · shadow
• providers: add · enable · disable · move · model
• config: get · set

Prefix with / (e.g. /status).
Plain text talks to the assistant when it is on; writes still need Confirm.";

/// Shown for free-text when `[telegram.assistant] enabled = false`.
const ASSISTANT_OFF_TIP: &str = "\
Assistant is off.

Enable:
/config set telegram.assistant.enabled true

Or /help for commands.";
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

#[cfg(test)]
pub(crate) fn pending_notify_lines() -> Vec<String> {
    batch().lock().unwrap().lines.clone()
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

/// Send assistant Markdown as Telegram HTML; fall back to plain text if Telegram rejects it.
fn send_one_markdown(cfg: &TelegramConfig, chat_id: i64, text: &str, markup: Option<Value>) {
    let Some(token) = bot_token(cfg) else {
        return;
    };
    let body = sanitize(text, &[&token]);
    let html = markdown_to_telegram_html(&body);
    match send_message_mode(cfg, &token, chat_id, &html, markup.clone(), Some("HTML")) {
        Err(e) if e.starts_with("HTTP 400") => {
            tracing::warn!("telegram rejected HTML formatting; resending as plain text");
            let _ = send_message_mode(cfg, &token, chat_id, &body, markup, None);
        }
        _ => {}
    }
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Convert common assistant Markdown (fenced code, `code`, **bold**, # headings,
/// [text](url)) into Telegram HTML. Everything else is escaped verbatim.
pub(crate) fn markdown_to_telegram_html(md: &str) -> String {
    let mut out = String::new();
    let mut in_fence = false;
    for line in md.split('\n') {
        if line.trim_start().starts_with("```") {
            out.push_str(if in_fence { "</pre>" } else { "<pre>" });
            in_fence = !in_fence;
            out.push('\n');
            continue;
        }
        if in_fence {
            out.push_str(&html_escape(line));
            out.push('\n');
            continue;
        }
        let t = line.trim_start();
        let hashes = t.chars().take_while(|c| *c == '#').count();
        if (1..=6).contains(&hashes) && t[hashes..].starts_with(' ') {
            out.push_str("<b>");
            out.push_str(&inline_md(t[hashes..].trim()));
            out.push_str("</b>");
        } else {
            out.push_str(&inline_md(line));
        }
        out.push('\n');
    }
    if in_fence {
        out.push_str("</pre>\n");
    }
    while out.ends_with('\n') {
        out.pop();
    }
    out
}

fn inline_md(line: &str) -> String {
    let mut out = String::new();
    let mut rest = line;
    while !rest.is_empty() {
        if let Some(r) = rest.strip_prefix('`') {
            if let Some(end) = r.find('`') {
                out.push_str("<code>");
                out.push_str(&html_escape(&r[..end]));
                out.push_str("</code>");
                rest = &r[end + 1..];
                continue;
            }
        }
        if let Some(r) = rest.strip_prefix("**") {
            if let Some(end) = r.find("**") {
                if end > 0 {
                    out.push_str("<b>");
                    out.push_str(&inline_md(&r[..end]));
                    out.push_str("</b>");
                    rest = &r[end + 2..];
                    continue;
                }
            }
        }
        if let Some(r) = rest.strip_prefix('[') {
            if let Some(close) = r.find("](") {
                if let Some(end) = r[close + 2..].find(')') {
                    let label = &r[..close];
                    let url = &r[close + 2..close + 2 + end];
                    if url.starts_with("http://") || url.starts_with("https://") {
                        out.push_str(&format!(
                            "<a href=\"{}\">{}</a>",
                            html_escape(url).replace('"', "&quot;"),
                            html_escape(label)
                        ));
                        rest = &r[close + 3 + end..];
                        continue;
                    }
                }
            }
        }
        let c = rest.chars().next().unwrap();
        out.push_str(&html_escape(&c.to_string()));
        rest = &rest[c.len_utf8()..];
    }
    out
}

fn send_message(
    cfg: &TelegramConfig,
    token: &str,
    chat_id: i64,
    text: &str,
    markup: Option<Value>,
) -> Result<()> {
    let _ = send_message_mode(cfg, token, chat_id, text, markup, None);
    Ok(())
}

fn send_message_mode(
    cfg: &TelegramConfig,
    token: &str,
    chat_id: i64,
    text: &str,
    markup: Option<Value>,
    parse_mode: Option<&str>,
) -> std::result::Result<Option<i64>, String> {
    let url = format!("{}/bot{}/sendMessage", api_base(cfg), token);
    let mut payload = json!({
        "chat_id": chat_id,
        "text": text,
        "disable_web_page_preview": true,
    });
    if let Some(m) = markup {
        payload["reply_markup"] = m;
    }
    if let Some(pm) = parse_mode {
        payload["parse_mode"] = json!(pm);
    }
    let agent = crate::util::ureq_agent(SEND_HTTP_TIMEOUT);
    let mut message_id = None;
    retry_send(
        &SendPolicy::default(),
        token,
        || match agent.post(&url).send_json(payload.clone()) {
            Ok(resp) => {
                message_id = resp
                    .into_json::<Value>()
                    .ok()
                    .and_then(|v| v.pointer("/result/message_id").and_then(|n| n.as_i64()));
                SendOutcome::Sent
            }
            Err(e) => classify_send(Err(e)),
        },
        std::thread::sleep,
    )?;
    Ok(message_id)
}

/// Edit a previously sent message's text (single attempt; best effort).
fn edit_message(
    cfg: &TelegramConfig,
    token: &str,
    chat_id: i64,
    message_id: i64,
    text: &str,
) -> bool {
    let url = format!("{}/bot{}/editMessageText", api_base(cfg), token);
    let payload = json!({"chat_id": chat_id, "message_id": message_id, "text": text});
    match crate::util::ureq_agent(SEND_HTTP_TIMEOUT)
        .post(&url)
        .send_json(payload)
    {
        Ok(_) => true,
        Err(e) => {
            tracing::warn!("telegram edit failed: {}", redact(&e.to_string(), token));
            false
        }
    }
}

/// Prefix marking the assistant's interim "on it" message.
pub const INTERIM_PREFIX: &str = "⏳";

fn interim_meta_key(chat: i64) -> String {
    format!("telegram_interim:{chat}")
}

/// Short label for a provider: `claude (opus)` or just `claude`.
pub fn provider_label(name: &str, model: &str) -> String {
    let m = model.trim();
    if m.is_empty() || m == name {
        name.to_string()
    } else {
        format!("{name} ({m})")
    }
}

pub fn interim_working_text(label: Option<&str>) -> String {
    match label {
        Some(l) => format!("{INTERIM_PREFIX} {l} is on it…"),
        None => format!("{INTERIM_PREFIX} on it…"),
    }
}

pub const INTERIM_QUEUED_TEXT: &str = "⏳ queued behind the current reply…";

pub fn interim_done_text(provider: &str, secs: u64, ok: bool) -> String {
    if ok {
        format!("✓ {provider} · {secs}s")
    } else {
        format!("✗ {provider} · {secs}s")
    }
}

fn provider_label_for(shared: &Shared, name: &str) -> String {
    let cfg = shared.config.read().unwrap();
    let model = cfg
        .provider(name)
        .map(|p| p.model.clone())
        .unwrap_or_default();
    provider_label(name, &model)
}

fn inflight_provider(shared: &Shared, key: &str) -> Option<String> {
    shared
        .inflight
        .lock()
        .unwrap()
        .get(key)
        .map(|f| f.provider.clone())
        .filter(|p| !p.is_empty())
}

/// Send an outgoing message; interim assistant messages are tracked so they
/// can be edited when a provider is chosen and when the reply arrives.
fn send_outgoing(shared: &Shared, cfg: &TelegramConfig, o: Outgoing) {
    let Some(token) = bot_token(cfg) else {
        return;
    };
    let mut text = o.text;
    let interim = text.starts_with(INTERIM_PREFIX);
    if interim && text != INTERIM_QUEUED_TEXT {
        // Provider may already be picked by the time we send.
        if let Some(p) = inflight_provider(shared, &assistant_task_key(o.chat_id)) {
            text = interim_working_text(Some(&provider_label_for(shared, &p)));
        }
    }
    let body = sanitize(&text, &[&token]);
    let mid = send_message_mode(cfg, &token, o.chat_id, &body, o.markup, None);
    if interim {
        if let Ok(Some(id)) = mid {
            let now = chrono::Utc::now().timestamp();
            let _ = shared
                .db
                .meta_set(&interim_meta_key(o.chat_id), &format!("{id}:{now}:{body}"));
        }
    }
}

fn interim_get(shared: &Shared, chat: i64) -> Option<(i64, i64, String)> {
    let v = shared.db.meta_get(&interim_meta_key(chat)).ok().flatten()?;
    let mut it = v.splitn(3, ':');
    let id = it.next()?.parse().ok()?;
    let t = it.next()?.parse().ok()?;
    Some((id, t, it.next().unwrap_or("").to_string()))
}

/// Called by the scheduler once a provider is picked for an assistant turn.
pub fn assistant_started(shared: &Shared, key: &str, provider: &str) {
    let Some(chat) = chat_from_assistant_key(key) else {
        return;
    };
    let cfg = shared.config.read().unwrap().telegram.clone();
    let Some(token) = bot_token(&cfg) else {
        return;
    };
    let Some((id, t, old)) = interim_get(shared, chat) else {
        return;
    };
    let text = interim_working_text(Some(&provider_label_for(shared, provider)));
    if old != text && edit_message(&cfg, &token, chat, id, &text) {
        let _ = shared
            .db
            .meta_set(&interim_meta_key(chat), &format!("{id}:{t}:{text}"));
    }
}

fn finish_interim(
    shared: &Shared,
    cfg: &TelegramConfig,
    chat: i64,
    provider: Option<&str>,
    ok: bool,
) {
    let Some((id, t, _)) = interim_get(shared, chat) else {
        return;
    };
    let _ = shared.db.meta_set(&interim_meta_key(chat), "");
    let Some(token) = bot_token(cfg) else {
        return;
    };
    let secs = (chrono::Utc::now().timestamp() - t).max(0) as u64;
    let text = interim_done_text(provider.unwrap_or("assistant"), secs, ok);
    edit_message(cfg, &token, chat, id, &text);
}

/// Per-attempt HTTP timeout for sendMessage. Generous because a read timeout is
/// ambiguous (the message may already be posted) and is never retried.
const SEND_HTTP_TIMEOUT: Duration = Duration::from_secs(30);

/// Retry policy for Telegram sends: a few attempts, exponential backoff, bounded total time.
#[derive(Debug, Clone)]
pub(crate) struct SendPolicy {
    pub max_attempts: u32,
    pub base_delay: Duration,
    pub max_delay: Duration,
    /// Upper bound on time spent sleeping between attempts.
    pub max_total_sleep: Duration,
}

impl Default for SendPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 4,
            base_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(8),
            max_total_sleep: Duration::from_secs(30),
        }
    }
}

#[derive(Debug)]
pub(crate) enum SendOutcome {
    Sent,
    /// Not delivered: DNS/connect error, 5xx, 429 (optional retry_after).
    Retry {
        msg: String,
        after: Option<Duration>,
    },
    /// Permanent or ambiguous (4xx, read timeout after send): do not retry.
    Fatal(String),
}

pub(crate) fn classify_send(r: std::result::Result<ureq::Response, ureq::Error>) -> SendOutcome {
    match r {
        Ok(_) => SendOutcome::Sent,
        Err(ureq::Error::Status(code, resp)) => {
            let body = resp.into_string().unwrap_or_default();
            classify_status(code, &body)
        }
        Err(ureq::Error::Transport(t)) => classify_transport(t.kind(), t.to_string()),
    }
}

/// Retry only when the request clearly never reached Telegram (DNS / connect).
/// I/O errors such as "timed out reading response" may follow a delivered
/// message, so retrying them would double-post.
pub(crate) fn classify_transport(kind: ureq::ErrorKind, msg: String) -> SendOutcome {
    match kind {
        ureq::ErrorKind::Dns | ureq::ErrorKind::ConnectionFailed => {
            SendOutcome::Retry { msg, after: None }
        }
        _ => SendOutcome::Fatal(format!("{msg} (delivery unknown; not retried)")),
    }
}

pub(crate) fn classify_status(code: u16, body: &str) -> SendOutcome {
    let msg = format!(
        "HTTP {code}: {}",
        body.chars().take(300).collect::<String>()
    );
    if code == 429 {
        let after = serde_json::from_str::<Value>(body)
            .ok()
            .and_then(|v| {
                v.pointer("/parameters/retry_after")
                    .and_then(|n| n.as_u64())
            })
            .map(Duration::from_secs);
        SendOutcome::Retry { msg, after }
    } else if (500..600).contains(&code) {
        SendOutcome::Retry { msg, after: None }
    } else {
        SendOutcome::Fatal(msg)
    }
}

/// Run `attempt` until it succeeds, fails permanently, or the policy is exhausted.
/// Returns the number of attempts on success, or the last error (redacted).
pub(crate) fn retry_send(
    policy: &SendPolicy,
    token: &str,
    mut attempt: impl FnMut() -> SendOutcome,
    mut sleep: impl FnMut(Duration),
) -> std::result::Result<u32, String> {
    let mut slept = Duration::ZERO;
    let mut n = 0u32;
    loop {
        n += 1;
        match attempt() {
            SendOutcome::Sent => {
                if n > 1 {
                    tracing::info!("telegram send succeeded on attempt {n}");
                }
                return Ok(n);
            }
            SendOutcome::Fatal(m) => {
                let m = redact(&m, token);
                tracing::warn!("telegram send failed (not retryable): {m}");
                return Err(m);
            }
            SendOutcome::Retry { msg, after } => {
                let m = redact(&msg, token);
                let backoff = policy
                    .base_delay
                    .saturating_mul(1u32 << (n - 1).min(16))
                    .min(policy.max_delay);
                let delay = after.unwrap_or(backoff);
                if n >= policy.max_attempts || slept + delay > policy.max_total_sleep {
                    tracing::warn!("telegram send failed after {n} attempt(s): {m}");
                    return Err(m);
                }
                tracing::warn!(
                    "telegram send attempt {n}/{} failed: {m}; retrying in {:?}",
                    policy.max_attempts,
                    delay
                );
                sleep(delay);
                slept += delay;
            }
        }
    }
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

/// Telegram long-poll timeout (seconds) passed to getUpdates.
const LONG_POLL_SECS: u64 = 25;

/// Client timeout for getUpdates: must exceed the long poll so an idle poll is not a "timeout".
pub(crate) fn poll_http_timeout() -> Duration {
    Duration::from_secs(LONG_POLL_SECS + 15)
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
        "{}/bot{}/getUpdates?timeout={}&offset={}",
        api_base(&cfg),
        token,
        LONG_POLL_SECS,
        offset
    );
    let agent = crate::util::ureq_agent(poll_http_timeout());
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
                send_outgoing(shared, &cfg, o);
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
        Ok(vec![assistant_off_outgoing(chat)])
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
            needs: Vec::new(),
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

fn assistant_off_outgoing(chat: i64) -> Outgoing {
    Outgoing {
        chat_id: chat,
        text: ASSISTANT_OFF_TIP.into(),
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
            let label = (!pin.is_empty()).then(|| provider_label_for(shared, &pin));
            Outgoing {
                chat_id: chat,
                text: match outcome {
                    SubmitOutcome::Accepted => interim_working_text(label.as_deref()),
                    SubmitOutcome::Queued => INTERIM_QUEUED_TEXT.to_string(),
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
    if let Some(chat) = chat_from_assistant_key(key) {
        let provider = inflight_provider(shared, key);
        finish_interim(
            shared,
            &cfg,
            chat,
            provider.as_deref(),
            outcome == "success",
        );
    }
    for (i, o) in assistant_reply(shared, key, run_id, outcome)
        .into_iter()
        .enumerate()
    {
        if i == 0 && outcome == "success" {
            send_one_markdown(&cfg, o.chat_id, &o.text, o.markup);
        } else {
            send_one(&cfg, o.chat_id, &o.text, o.markup);
        }
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
    sanitize(&format_status_body(&v, &tz, paused, repo), &[])
}

/// Phone-readable `/status` body. Facts come from `status_json`; this only
/// changes presentation (no `en=` / `in=` jargon, human dates, usage indented).
fn format_status_body(v: &Value, tz: &str, paused: bool, repo: Option<&str>) -> String {
    let mut out = format!(
        "atr · {tz} · intake {}\n",
        if paused { "paused" } else { "on" }
    );
    let repos = format_status_repos(v, repo);
    if !repos.is_empty() {
        out.push_str("\nRepos\n");
        out.push_str(&repos);
    }
    let providers = format_status_providers(v);
    if !providers.is_empty() {
        out.push_str("\nProviders\n");
        out.push_str(&providers);
    }
    if let Some(c) = v.get("cache") {
        out.push_str("\nCache\n");
        out.push_str(&format_status_cache(c));
    }
    out
}

fn json_nonempty_str(v: Option<&Value>) -> Option<&str> {
    v.and_then(|x| x.as_str()).and_then(nonempty)
}

fn nonempty(s: &str) -> Option<&str> {
    let t = s.trim();
    if t.is_empty() || t == "-" {
        None
    } else {
        Some(t)
    }
}

fn on_off(on: bool) -> &'static str {
    if on {
        "on"
    } else {
        "off"
    }
}

fn join_dot(parts: &[&str]) -> String {
    parts
        .iter()
        .copied()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" · ")
}

fn repo_bullet(name: &str, github: Option<&str>, label: Option<&str>, on: bool) -> String {
    let gh = github.and_then(nonempty);
    let label = label.and_then(nonempty);
    let detail = match (gh, label) {
        (Some(g), Some(l)) => format!(" ({g}, {l})"),
        (Some(g), None) => format!(" ({g})"),
        (None, Some(l)) => format!(" ({l})"),
        (None, None) => String::new(),
    };
    format!("• {name}{detail} — {}\n", on_off(on))
}

fn format_status_repos(v: &Value, repo: Option<&str>) -> String {
    let mut out = String::new();
    let Some(repos) = v.get("repos").and_then(|p| p.as_array()) else {
        return out;
    };
    for r in repos {
        if let Some(f) = repo {
            if r.get("name").and_then(|x| x.as_str()) != Some(f)
                && r.get("github").and_then(|x| x.as_str()) != Some(f)
            {
                continue;
            }
        }
        let name = r.get("name").and_then(|x| x.as_str()).unwrap_or("-");
        let on = r.get("enabled").and_then(|x| x.as_bool()).unwrap_or(false);
        out.push_str(&repo_bullet(
            name,
            json_nonempty_str(r.get("github")),
            json_nonempty_str(r.get("label")),
            on,
        ));
    }
    out
}

fn format_status_providers(v: &Value) -> String {
    let mut out = String::new();
    let Some(ps) = v.get("providers").and_then(|p| p.as_array()) else {
        return out;
    };
    for p in ps {
        let name = p.get("name").and_then(|x| x.as_str()).unwrap_or("-");
        let on = p.get("enabled").and_then(|x| x.as_bool()).unwrap_or(false);
        let inflight = p.get("inflight").and_then(|x| x.as_u64()).unwrap_or(0);
        let max = p.get("max_parallel").and_then(|x| x.as_u64()).unwrap_or(0);
        let mut bits = vec![on_off(on).to_string(), format!("{inflight}/{max} busy")];
        if p.get("cooling").and_then(|x| x.as_bool()).unwrap_or(false) {
            bits.push("cooling".into());
        }
        if p.get("skip_new").and_then(|x| x.as_bool()).unwrap_or(false) {
            bits.push("skipping new".into());
        }
        if p.get("degraded").and_then(|x| x.as_bool()).unwrap_or(false) {
            bits.push("degraded".into());
        }
        if let Some(reset) = json_nonempty_str(p.get("quota_reset")).and_then(human_reset_date) {
            bits.push(format!("resets {reset}"));
        }
        let usage = p.get("usage").and_then(|x| x.as_str()).unwrap_or("");
        match human_usage_line(usage) {
            None => {
                bits.push("usage not reported".into());
                out.push_str(&format!("• {name} — {}\n", bits.join(" · ")));
            }
            Some(line) => {
                out.push_str(&format!("• {name} — {}\n  {line}\n", bits.join(" · ")));
            }
        }
    }
    out
}

/// Compact day+month from display-timezone stamps (`2026-10-15 07:00 Zone`)
/// or leftover ISO timestamps. None when missing.
fn human_reset_date(raw: &str) -> Option<String> {
    let s = raw.trim();
    if s.is_empty() || s == "-" {
        return None;
    }
    if let Some(date) = s.split_whitespace().next() {
        if let Ok(d) = chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d") {
            return Some(day_month(&d.format("%d %b").to_string()));
        }
    }
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
        return Some(day_month(&dt.format("%d %b").to_string()));
    }
    if s.contains('T') && s.len() > 16 {
        return Some(s.chars().take(10).collect());
    }
    Some(s.to_string())
}

fn day_month(padded: &str) -> String {
    padded.trim_start_matches('0').to_string()
}

/// Shared `format_provider_usage` text, compacted for a phone. None means
/// "not reported" (shown on the provider line, not indented).
fn human_usage_line(raw: &str) -> Option<String> {
    let s = raw.trim();
    if s.is_empty() || s == "-" {
        return None;
    }
    let s = s.strip_prefix("usage:").map(str::trim).unwrap_or(s);
    if s.eq_ignore_ascii_case("not reported") {
        return None;
    }
    let parts: Vec<String> = s
        .split(';')
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(humanize_usage_part)
        .filter(|p| !p.is_empty())
        .collect();
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(" · "))
    }
}

fn humanize_usage_part(part: &str) -> String {
    let mut s = part.to_string();
    if let Some(i) = s.find(", resets ") {
        s.truncate(i);
    }
    s = s.replace(", window reset", "");
    s = s.replace("extra credits $", "extra $");
    s = s.replace(" used", "");
    if let Some(rest) = s.strip_prefix("weekly") {
        s = format!("week{rest}");
    }
    s = drop_whole_percent_decimals(&s);
    s.trim().to_string()
}

fn drop_whole_percent_decimals(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(idx) = rest.find(".0%") {
        out.push_str(&rest[..idx]);
        out.push('%');
        rest = &rest[idx + 3..];
    }
    out.push_str(rest);
    out
}

fn format_status_cache(cache: &Value) -> String {
    let pct = |k: &str| {
        cache
            .get(k)
            .and_then(|v| v.as_f64())
            .map(|x| format!("{:.0}%", x * 100.0))
            .unwrap_or_else(|| "-".into())
    };
    format!(
        "headline {} · first-run {} · resume {} · short {}\n",
        pct("headline_hit"),
        pct("first_run_hit"),
        pct("resume_hit"),
        pct("short_hit"),
    )
}

fn format_tasks(shared: &Shared, repo: Option<&str>) -> String {
    let cfg = shared.config.read().unwrap().clone();
    let tz = &cfg.display_timezone;
    let mut out = format!("Tasks · {}\n", tz.label);
    let mut n = 0;
    for t in shared.db.list_tasks().unwrap_or_default() {
        if let Some(f) = repo {
            if !cfg.task_matches_repo(&t, f) {
                continue;
            }
        }
        n += 1;
        out.push('\n');
        out.push_str(&format!("• {} — {}\n", t.key, t.status));
        let repo_name = cfg.task_repo_name(&t);
        let bits = join_dot(&[
            nonempty(&repo_name).unwrap_or(""),
            nonempty(&t.role).unwrap_or(""),
            t.provider.as_deref().and_then(nonempty).unwrap_or(""),
        ]);
        if !bits.is_empty() {
            out.push_str(&format!("  {bits}\n"));
        }
        out.push_str(&format!("  {}\n", tz.format_str(&t.updated)));
    }
    if n == 0 {
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
    let mut out = format!("{key} — {}\n", t.status);
    let repo = cfg.task_repo_name(&t);
    let bits = join_dot(&[
        nonempty(&repo).unwrap_or(""),
        nonempty(&t.role).unwrap_or(""),
        t.provider.as_deref().and_then(nonempty).unwrap_or(""),
    ]);
    if !bits.is_empty() {
        out.push_str(&format!("{bits}\n"));
    }
    if let Some(title) = nonempty(&t.title) {
        out.push_str(&format!("{title}\n"));
    }
    if let Some(parent) = t.parent_key.as_deref().and_then(nonempty) {
        out.push_str(&format!("parent {parent}\n"));
    }
    if t.shadow {
        out.push_str("shadow\n");
    }
    out.push_str(&format!("updated {}\n", tz.format_str(&t.updated)));
    if let Ok(kids) = shared.db.children_of(key) {
        if !kids.is_empty() {
            out.push_str("\nChildren\n");
            for c in kids {
                out.push_str(&format!("• {} — {}\n", c.key, c.status));
            }
        }
    }
    if let Some(r) = last {
        let ended = tz.format_opt(r.ended.as_deref());
        out.push_str(&format!(
            "\nLast run {}\n",
            join_dot(&[
                r.outcome.as_deref().and_then(nonempty).unwrap_or("-"),
                &ended,
            ])
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
        Ok(r) => sanitize(&format_usage_body(&r), &[]),
        Err(e) => format!("usage: {e:#}"),
    }
}

fn format_usage_body(report: &crate::usage::SavingsReport) -> String {
    let mut out = format!(
        "Usage · {}\n{} → {} · {}\n",
        report.period, report.from, report.to, report.timezone
    );
    for r in &report.by_provider {
        out.push_str(&format!("\n• {}\n  {}\n", r.provider, usage_row_lines(r)));
        out.push_str(&format!("  {}\n", usage_saved_line(r)));
    }
    out.push_str(&format!(
        "\nTotal\n{}\n{}\n",
        usage_row_lines(&report.total),
        usage_saved_line(&report.total)
    ));
    let min = report.headline_min_input;
    out.push_str(&format!(
        "\nCache\nheadline {} (≥ {min} tokens) · first-run {} · resume {}\nshort {} ({} runs)\n",
        fmt_pct(report.headline.hit),
        fmt_pct(report.first_run.hit),
        fmt_pct(report.resume.hit),
        fmt_pct(report.short.hit),
        report.short.runs
    ));
    out
}

fn usage_row_lines(r: &crate::usage::SavingsRow) -> String {
    let input = format!("in {}", r.input);
    let cached = format!("cached {}", r.cache_read);
    let write = format!("write {}", r.cache_write);
    let output = format!("out {}", r.output);
    join_dot(&[&input, &cached, &write, &output])
}

fn usage_saved_line(r: &crate::usage::SavingsRow) -> String {
    let hit = format!("hit {}", fmt_pct(r.hit));
    let saved = fmt_saved(r.saved_usd);
    join_dot(&[&hit, &saved])
}

fn fmt_pct(h: Option<f64>) -> String {
    h.map(|x| format!("{:.0}%", x * 100.0))
        .unwrap_or_else(|| "-".into())
}

fn fmt_saved(v: Option<f64>) -> String {
    v.map(|x| format!("saved ${x:.4}"))
        .unwrap_or_else(|| "saved -".into())
}

fn format_repos(shared: &Shared) -> String {
    let cfg = shared.config.read().unwrap().clone();
    let repos = cfg.effective_repos();
    let mut out = String::from("Repos\n");
    if repos.is_empty() {
        out.push_str("(none)\n");
    } else {
        for r in &repos {
            out.push_str(&repo_bullet(
                &r.name,
                nonempty(&r.github),
                nonempty(&r.label),
                r.enabled,
            ));
        }
    }
    sanitize(&out, &[])
}

fn format_shadow(shared: &Shared) -> String {
    let cfg = shared.config.read().unwrap().clone();
    let tz = &cfg.display_timezone;
    let names: Vec<&str> = cfg.providers.iter().map(|p| p.name.as_str()).collect();
    let rows = match shared.db.list_shadow_results(None) {
        Ok(r) => r,
        Err(e) => return format!("shadow report: {e:#}"),
    };
    let mut out = format!("Shadow · {}\n", tz.label);
    if !names.is_empty() {
        out.push_str("\nUsage\n");
        let now = crate::clock::Clock::system().now();
        for name in &names {
            let raw = crate::reported::format_provider_usage(
                &crate::reported::provider_usage_view(&shared.db, name),
                tz,
                now,
            );
            let line = human_usage_line(&raw).unwrap_or_else(|| "not reported".into());
            out.push_str(&format!("• {name} — {line}\n"));
        }
    }
    let mut latest = BTreeMap::new();
    for r in &rows {
        latest.insert(r.task_key.clone(), r.clone());
    }
    let latest: Vec<_> = latest.into_values().collect();
    if latest.is_empty() {
        out.push_str("\n(no runs)\n");
        return sanitize(&out, &[]);
    }
    out.push_str("\nRuns\n");
    let mut tot_ms = 0i64;
    let mut tot_in = 0i64;
    let mut tot_out = 0i64;
    let mut tot_cr = 0i64;
    let mut tot_cw = 0i64;
    let mut tot_cost = 0.0;
    for r in &rows {
        tot_ms += r.duration_ms;
        tot_in += r.input;
        tot_out += r.output;
        tot_cr += r.cache_read;
        tot_cw += r.cache_write;
        tot_cost += r.cost_usd;
    }
    for r in &latest {
        let outcome = nonempty(&r.outcome).unwrap_or("-");
        out.push_str(&format!("• {} · {} — {outcome}\n", r.task_key, r.provider));
        let dur =
            crate::duration::format_duration(Duration::from_millis(r.duration_ms.max(0) as u64));
        out.push_str(&format!(
            "  {dur} · in {} · out {} · cache {}\n",
            r.input, r.output, r.cache_read
        ));
        let hit = r
            .cache_hit
            .map(|h| format!("hit {:.0}%", h * 100.0))
            .unwrap_or_else(|| "hit -".into());
        out.push_str(&format!("  {hit} · ${:.4}\n", r.cost_usd));
        if let Some(d) = nonempty(&r.diffstat) {
            out.push_str(&format!("  {d}\n"));
        }
        if let Some(v) = nonempty(&r.verify) {
            out.push_str(&format!("  verify {v}\n"));
        }
        if let Some(t) = nonempty(&r.tests) {
            let first = t.lines().next().unwrap_or(t);
            out.push_str(&format!("  tests {first}\n"));
        }
    }
    let tot_hit = hit_ratio_from_sums("claude", tot_in, tot_cr, tot_cw)
        .map(|h| format!("hit {:.0}%", h * 100.0))
        .unwrap_or_else(|| "hit -".into());
    let tot_dur = crate::duration::format_duration(Duration::from_millis(tot_ms.max(0) as u64));
    out.push_str(&format!(
        "\nTotals · {} runs\n{tot_dur} · in {tot_in} · out {tot_out} · cache {tot_cr}\n{tot_hit} · ${tot_cost:.4}\n",
        rows.len()
    ));
    for r in &latest {
        if shared
            .db
            .session_prefix_changed(&r.task_key)
            .unwrap_or(false)
        {
            out.push_str(&format!("cache prefix changed · {}\n", r.task_key));
        }
    }
    sanitize(&out, &[])
}

fn format_providers(shared: &Shared) -> String {
    let cfg = shared.config.read().unwrap().clone();
    let mut out = String::from("Providers\n");
    if cfg.providers.is_empty() {
        out.push_str("(none)\n");
        return sanitize(&out, &[]);
    }
    for p in &cfg.providers {
        let over = shared.db.enabled_override(&p.name).ok().flatten();
        let en = over.unwrap_or(p.enabled);
        let mut line = format!("• {} · {} · {}", p.name, p.kind, on_off(en));
        if let Some(o) = over {
            if o != p.enabled {
                line.push_str(&format!(" (config {})", on_off(p.enabled)));
            }
        }
        out.push_str(&line);
        out.push('\n');
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
        let repo = shared.config.read().unwrap().effective_repos()[0].clone();
        let expected = repo_bullet(
            &repo.name,
            nonempty(&repo.github),
            nonempty(&repo.label),
            repo.enabled,
        );
        let upd = json!({"message":{"chat":{"id":42},"text":"/repos"}});
        let out = handle_update(&shared, &upd).unwrap();
        assert_eq!(out.len(), 1);
        assert!(out[0].text.starts_with("Repos\n"), "{}", out[0].text);
        assert!(out[0].text.contains(&expected), "{}", out[0].text);
        assert!(
            out[0].text.contains(" — on") || out[0].text.contains(" — off"),
            "{}",
            out[0].text
        );
        assert!(!out[0].text.contains("enabled"), "{}", out[0].text);
        assert!(!out[0].text.contains('\t'), "{}", out[0].text);
        assert!(
            is_driveable_command("/repos"),
            "/repos must be assistant-driveable"
        );
    }

    #[test]
    fn providers_list_is_readable_on_phone() {
        std::env::set_var("ATR_TEST_TG_TOKEN", "tok-secret");
        let shared = harness_shared();
        let upd = json!({"message":{"chat":{"id":42},"text":"/providers"}});
        let out = handle_update(&shared, &upd).unwrap();
        assert!(out[0].text.starts_with("Providers\n"), "{}", out[0].text);
        assert!(
            out[0].text.contains("• claude · claude · on"),
            "{}",
            out[0].text
        );
        assert!(!out[0].text.contains("kind="), "{}", out[0].text);
        assert!(!out[0].text.contains("enabled="), "{}", out[0].text);
        assert!(!out[0].text.contains("en="), "{}", out[0].text);

        shared.db.set_enabled_override("claude", false).unwrap();
        let out = handle_update(&shared, &upd).unwrap();
        assert!(
            out[0].text.contains("• claude · claude · off (config on)"),
            "{}",
            out[0].text
        );
    }

    #[test]
    fn tasks_and_task_are_readable_on_phone() {
        std::env::set_var("ATR_TEST_TG_TOKEN", "tok-secret");
        let shared = harness_shared();
        let empty = json!({"message":{"chat":{"id":42},"text":"/tasks"}});
        let out = handle_update(&shared, &empty).unwrap();
        assert!(out[0].text.starts_with("Tasks · UTC"), "{}", out[0].text);
        assert!(out[0].text.contains("(none)"), "{}", out[0].text);

        shared.db.upsert_task("acme-1", "/repo", None).unwrap();
        shared.db.set_status("acme-1", "waiting").unwrap();
        shared
            .db
            .set_provider_session("acme-1", Some("claude"), None)
            .unwrap();
        shared.db.set_task_title("acme-1", "fix login").unwrap();
        shared.db.upsert_task("acme-1-a", "/repo", None).unwrap();
        shared.db.set_parent_key("acme-1-a", "acme-1").unwrap();
        shared.db.set_status("acme-1-a", "queued").unwrap();
        let run = shared.db.start_run("acme-1", "claude", "m").unwrap();
        shared.db.finish_run(run, "success", Some(0)).unwrap();

        let list = json!({"message":{"chat":{"id":42},"text":"/tasks"}});
        let out = handle_update(&shared, &list).unwrap();
        assert!(
            out[0].text.contains("• acme-1 — waiting"),
            "{}",
            out[0].text
        );
        assert!(out[0].text.contains("claude"), "{}", out[0].text);
        assert!(out[0].text.contains("worker"), "{}", out[0].text);
        assert!(!out[0].text.contains('\t'), "{}", out[0].text);
        assert!(!out[0].text.contains("repo="), "{}", out[0].text);

        let missing = json!({"message":{"chat":{"id":42},"text":"/task"}});
        let out = handle_update(&shared, &missing).unwrap();
        assert_eq!(out[0].text, "usage: /task <key>");

        let one = json!({"message":{"chat":{"id":42},"text":"/task acme-1"}});
        let out = handle_update(&shared, &one).unwrap();
        assert!(
            out[0].text.starts_with("acme-1 — waiting"),
            "{}",
            out[0].text
        );
        assert!(out[0].text.contains("fix login"), "{}", out[0].text);
        assert!(out[0].text.contains("Children"), "{}", out[0].text);
        assert!(
            out[0].text.contains("• acme-1-a — queued"),
            "{}",
            out[0].text
        );
        assert!(out[0].text.contains("Last run success"), "{}", out[0].text);
        assert!(!out[0].text.contains("status="), "{}", out[0].text);
        assert!(!out[0].text.contains("parent="), "{}", out[0].text);
        assert!(!out[0].text.contains("shadow="), "{}", out[0].text);
    }

    #[test]
    fn shadow_report_is_readable_on_phone() {
        std::env::set_var("ATR_TEST_TG_TOKEN", "tok-secret");
        let shared = harness_shared();
        let empty = json!({"message":{"chat":{"id":42},"text":"/shadow"}});
        let out = handle_update(&shared, &empty).unwrap();
        assert!(out[0].text.starts_with("Shadow · UTC"), "{}", out[0].text);
        assert!(
            out[0].text.contains("• claude — not reported"),
            "{}",
            out[0].text
        );
        assert!(out[0].text.contains("(no runs)"), "{}", out[0].text);
        assert!(!out[0].text.contains("timezone "), "{}", out[0].text);

        shared
            .db
            .insert_shadow_result(&crate::db::ShadowResult {
                task_key: "acme-s".into(),
                run_id: None,
                provider: "claude".into(),
                outcome: "success".into(),
                duration_ms: 1200,
                input: 100,
                output: 50,
                cache_read: 800,
                cache_write: 20,
                cost_usd: 0.0123,
                cache_hit: Some(0.89),
                tests: "ok\nextra".into(),
                diff: String::new(),
                diffstat: "2 files".into(),
                verify: "pass".into(),
                recorded: crate::db::now_rfc3339(),
            })
            .unwrap();
        let out = handle_update(&shared, &empty).unwrap();
        assert!(
            out[0].text.contains("• acme-s · claude — success"),
            "{}",
            out[0].text
        );
        assert!(out[0].text.contains("in 100"), "{}", out[0].text);
        assert!(out[0].text.contains("cache 800"), "{}", out[0].text);
        assert!(out[0].text.contains("hit 89%"), "{}", out[0].text);
        assert!(out[0].text.contains("$0.0123"), "{}", out[0].text);
        assert!(out[0].text.contains("2 files"), "{}", out[0].text);
        assert!(out[0].text.contains("verify pass"), "{}", out[0].text);
        assert!(out[0].text.contains("tests ok"), "{}", out[0].text);
        assert!(out[0].text.contains("Totals · 1 runs"), "{}", out[0].text);
        assert!(!out[0].text.contains('\t'), "{}", out[0].text);
    }

    #[test]
    fn status_from_allowed_chat() {
        std::env::set_var("ATR_TEST_TG_TOKEN", "tok-secret");
        let shared = harness_shared();
        let upd = json!({"message":{"chat":{"id":42},"text":"/status"}});
        let out = handle_update(&shared, &upd).unwrap();
        assert_eq!(out.len(), 1);
        assert!(out[0].text.contains("atr ·"), "{}", out[0].text);
        assert!(out[0].text.contains("intake on"), "{}", out[0].text);
        assert!(
            out[0].text.contains("usage not reported"),
            "{}",
            out[0].text
        );
        assert!(!out[0].text.contains("en="), "{}", out[0].text);
        assert!(!out[0].text.contains("tok-secret"));
    }

    #[test]
    fn format_status_is_readable_on_phone() {
        let v = json!({
            "repos": [
                {"name": "scratch", "enabled": true, "github": "", "label": ""},
                {
                    "name": "nixer",
                    "enabled": true,
                    "github": "TaskrApps/nixer-app",
                    "label": "for:annie"
                },
            ],
            "providers": [
                {
                    "name": "claude",
                    "enabled": true,
                    "inflight": 0,
                    "max_parallel": 2,
                    "cooling": false,
                    "skip_new": false,
                    "quota_reset": "2026-10-15 07:00 Europe/Vilnius",
                    "usage": "session 1.0% used; weekly 3.0% used"
                },
                {
                    "name": "grok",
                    "enabled": true,
                    "inflight": 0,
                    "max_parallel": 2,
                    "cooling": false,
                    "skip_new": false,
                    "quota_reset": "2026-10-16 00:00 Europe/Vilnius",
                    "usage": "weekly 0.0% used; extra credits $140.32"
                },
                {
                    "name": "cursor",
                    "enabled": true,
                    "inflight": 0,
                    "max_parallel": 2,
                    "cooling": false,
                    "skip_new": false,
                    "quota_reset": "-",
                    "usage": "usage: not reported"
                },
                {
                    "name": "kimi",
                    "enabled": false,
                    "inflight": 1,
                    "max_parallel": 2,
                    "cooling": true,
                    "skip_new": true,
                    "quota_reset": "2026-10-15T12:34:56Z",
                    "usage": "weekly 1.0% used, resets Oct 16 07:36 UTC; extra credits $1.00"
                }
            ],
            "cache": {
                "headline_hit": 0.83,
                "first_run_hit": 0.85,
                "resume_hit": 0.60,
                "short_hit": 0.47
            }
        });
        let text = format_status_body(&v, "Europe/Vilnius", false, None);
        assert!(
            text.starts_with("atr · Europe/Vilnius · intake on"),
            "{text}"
        );
        assert!(text.contains("• scratch — on"), "{text}");
        assert!(
            text.contains("• nixer (TaskrApps/nixer-app, for:annie) — on"),
            "{text}"
        );
        assert!(
            text.contains("• claude — on · 0/2 busy · resets 15 Oct"),
            "{text}"
        );
        assert!(text.contains("  session 1% · week 3%"), "{text}");
        assert!(
            text.contains("• grok — on · 0/2 busy · resets 16 Oct"),
            "{text}"
        );
        assert!(text.contains("  week 0% · extra $140.32"), "{text}");
        assert!(
            text.contains("• cursor — on · 0/2 busy · usage not reported"),
            "{text}"
        );
        assert!(
            text.contains("• kimi — off · 1/2 busy · cooling · skipping new · resets 15 Oct"),
            "{text}"
        );
        assert!(text.contains("  week 1% · extra $1.00"), "{text}");
        assert!(
            text.contains("headline 83% · first-run 85% · resume 60% · short 47%"),
            "{text}"
        );
        assert!(!text.contains("en="), "{text}");
        assert!(!text.contains("in="), "{text}");
        assert!(!text.contains("cool="), "{text}");
        assert!(!text.contains("skip="), "{text}");
        assert!(!text.contains("usage:"), "{text}");
        assert!(!text.contains("2026-10-15T"), "{text}");
        assert!(text.len() < MAX_TEXT, "{}", text.len());
        let claude = text.lines().find(|l| l.contains("claude")).unwrap();
        assert!(!claude.contains("cooling"), "{claude}");
        assert!(!claude.contains("skipping"), "{claude}");
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
        assert!(out[0].text.contains("Usage"), "{}", out[0].text);
        assert!(out[0].text.contains("UTC"));
        assert!(out[0].text.contains("today"), "{}", out[0].text);
        assert!(!out[0].text.contains("in="), "{}", out[0].text);
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
        assert!(out[0].text.contains("Usage · month"), "{}", out[0].text);
        assert!(out[0].text.contains("UTC"), "{}", out[0].text);
        assert!(out[0].text.contains("• claude"), "{}", out[0].text);
        assert!(out[0].text.contains("cached 800"), "{}", out[0].text);
        assert!(!out[0].text.contains("cached="), "{}", out[0].text);
        assert!(!out[0].text.contains("saved=800"), "{}", out[0].text);
        assert!(out[0].text.contains("Total"), "{}", out[0].text);
        assert!(out[0].text.contains("Cache"), "{}", out[0].text);
        assert!(out[0].text.contains("saved $"), "{}", out[0].text);
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
        assert_eq!(out[0].text, COMMANDS);
        assert!(out[0].text.contains("Watch\n"), "{}", out[0].text);
        assert!(out[0].text.contains("Work\n"), "{}", out[0].text);
        assert!(out[0].text.contains("Control\n"), "{}", out[0].text);
        assert!(
            out[0]
                .text
                .contains("providers: add · enable · disable · move · model"),
            "{}",
            out[0].text
        );
        assert!(out[0].text.contains("config: get · set"), "{}", out[0].text);
        assert!(!out[0].text.contains("add|enable"), "{}", out[0].text);
        assert!(out[0].text.contains("Plain text"), "{}", out[0].text);
        assert!(!out[0].text.contains("commands:"), "{}", out[0].text);
        let slashed: Vec<&str> = out[0]
            .text
            .split(|c: char| !(c.is_ascii_alphanumeric() || c == '/'))
            .filter(|t| t.starts_with('/') && t.len() > 1)
            .collect();
        assert_eq!(slashed, ["/status"], "{}", out[0].text);
        assert!(
            out[0].text.lines().filter(|l| !l.trim().is_empty()).count() <= 16,
            "help should stay short: {}",
            out[0].text.lines().count()
        );
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
    fn free_text_without_assistant_tips_enable() {
        std::env::set_var("ATR_TEST_TG_TOKEN", "tok-secret");
        let shared = harness_shared();
        let upd = json!({"message":{"chat":{"id":42},"text":"how do I add kimi?"}});
        let out = handle_update(&shared, &upd).unwrap();
        assert_eq!(out[0].text, ASSISTANT_OFF_TIP);
        assert!(out[0].text.contains("Assistant is off"), "{}", out[0].text);
        assert!(
            out[0]
                .text
                .contains("/config set telegram.assistant.enabled true"),
            "{}",
            out[0].text
        );
        assert!(!out[0].text.contains("atr Telegram"), "{}", out[0].text);
        assert!(shared.db.get_task("tg42").unwrap().is_none());
    }

    #[test]
    fn free_text_assistant_submits_and_resumes() {
        std::env::set_var("ATR_TEST_TG_TOKEN", "tok-secret");
        let shared = harness_shared();
        shared.config.write().unwrap().telegram.assistant.enabled = true;
        let upd = json!({"message":{"chat":{"id":42},"text":"how do I add kimi?"}});
        let out = handle_update(&shared, &upd).unwrap();
        assert_eq!(out[0].text, "⏳ on it…");
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

    mod send_retry {
        use super::super::*;
        use std::time::Duration;

        fn fast() -> SendPolicy {
            SendPolicy::default()
        }

        #[test]
        fn success_first_try_sends_once() {
            let mut calls = 0;
            let mut sleeps = vec![];
            let r = retry_send(
                &fast(),
                "tok",
                || {
                    calls += 1;
                    SendOutcome::Sent
                },
                |d| sleeps.push(d),
            );
            assert_eq!(r, Ok(1));
            assert_eq!(calls, 1);
            assert!(sleeps.is_empty());
        }

        #[test]
        fn transient_then_success_with_backoff() {
            let mut calls = 0;
            let mut sleeps = vec![];
            let r = retry_send(
                &fast(),
                "tok",
                || {
                    calls += 1;
                    if calls < 3 {
                        SendOutcome::Retry {
                            msg: "timed out".into(),
                            after: None,
                        }
                    } else {
                        SendOutcome::Sent
                    }
                },
                |d| sleeps.push(d),
            );
            assert_eq!(r, Ok(3));
            assert_eq!(sleeps, vec![Duration::from_secs(1), Duration::from_secs(2)]);
        }

        #[test]
        fn gives_up_after_max_attempts_and_redacts_token() {
            let mut calls = 0;
            let r = retry_send(
                &fast(),
                "SECRET",
                || {
                    calls += 1;
                    SendOutcome::Retry {
                        msg: "https://x/botSECRET/sendMessage timed out".into(),
                        after: None,
                    }
                },
                |_| {},
            );
            assert_eq!(calls, 4);
            let e = r.unwrap_err();
            assert!(!e.contains("SECRET"), "{e}");
        }

        #[test]
        fn fatal_not_retried() {
            let mut calls = 0;
            let r = retry_send(
                &fast(),
                "t",
                || {
                    calls += 1;
                    classify_status(400, r#"{"ok":false}"#)
                },
                |_| {},
            );
            assert!(r.is_err());
            assert_eq!(calls, 1);
        }

        #[test]
        fn honors_retry_after_and_total_bound() {
            let o = classify_status(429, r#"{"ok":false,"parameters":{"retry_after":7}}"#);
            assert!(
                matches!(o, SendOutcome::Retry { after: Some(d), .. } if d == Duration::from_secs(7))
            );
            let mut sleeps = vec![];
            let mut calls = 0;
            let r = retry_send(
                &fast(),
                "t",
                || {
                    calls += 1;
                    if calls == 1 {
                        classify_status(429, r#"{"parameters":{"retry_after":7}}"#)
                    } else {
                        SendOutcome::Sent
                    }
                },
                |d| sleeps.push(d),
            );
            assert_eq!(r, Ok(2));
            assert_eq!(sleeps, vec![Duration::from_secs(7)]);
            // retry_after beyond the total budget: give up without sleeping
            let mut slept = false;
            let r = retry_send(
                &fast(),
                "t",
                || classify_status(429, r#"{"parameters":{"retry_after":120}}"#),
                |_| slept = true,
            );
            assert!(r.is_err());
            assert!(!slept);
        }

        #[test]
        fn five_xx_is_retryable() {
            assert!(matches!(
                classify_status(502, "bad gateway"),
                SendOutcome::Retry { .. }
            ));
            assert!(matches!(
                classify_status(403, "forbidden"),
                SendOutcome::Fatal(_)
            ));
        }

        #[test]
        fn read_timeout_is_not_retried_connect_is() {
            let o = classify_transport(ureq::ErrorKind::Io, "timed out reading response".into());
            assert!(matches!(o, SendOutcome::Fatal(_)));
            let mut calls = 0;
            let _ = retry_send(
                &fast(),
                "t",
                || {
                    calls += 1;
                    classify_transport(ureq::ErrorKind::Io, "timed out reading response".into())
                },
                |_| {},
            );
            assert_eq!(calls, 1, "ambiguous read timeout must not double-send");
            assert!(matches!(
                classify_transport(ureq::ErrorKind::ConnectionFailed, "refused".into()),
                SendOutcome::Retry { .. }
            ));
            assert!(matches!(
                classify_transport(ureq::ErrorKind::Dns, "dns".into()),
                SendOutcome::Retry { .. }
            ));
        }

        #[test]
        fn markdown_renders_to_telegram_html() {
            let h = markdown_to_telegram_html(
                "# Title\n**bold** and `a<b>` & [x](https://e.com)\n```\nfn x() -> i32 {}\n```",
            );
            assert_eq!(h, "<b>Title</b>\n<b>bold</b> and <code>a&lt;b&gt;</code> &amp; <a href=\"https://e.com\">x</a>\n<pre>\nfn x() -&gt; i32 {}\n</pre>");
            assert_eq!(
                markdown_to_telegram_html("2 * 3 ** x <tag>"),
                "2 * 3 ** x &lt;tag&gt;"
            );
            assert_eq!(
                markdown_to_telegram_html("```\nopen"),
                "<pre>\nopen\n</pre>"
            );
        }

        #[test]
        fn interim_texts_are_short() {
            assert_eq!(provider_label("claude", "opus"), "claude (opus)");
            assert_eq!(provider_label("claude", ""), "claude");
            assert_eq!(
                interim_working_text(Some("claude (opus)")),
                "⏳ claude (opus) is on it…"
            );
            assert_eq!(interim_working_text(None), "⏳ on it…");
            assert_eq!(interim_done_text("claude", 6, true), "✓ claude · 6s");
            assert!(INTERIM_QUEUED_TEXT.starts_with(INTERIM_PREFIX));
        }

        #[test]
        fn poll_client_timeout_exceeds_long_poll() {
            assert!(poll_http_timeout() > Duration::from_secs(LONG_POLL_SECS));
        }
    }
}
