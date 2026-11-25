//! Read-only vendor plan meters for `atr usage --plan`, MCP `plan_usage`,
//! and the dashboard plan line.
//!
//! HTTP adapters are GET-only and must use [`ALLOWED_GET_URLS`]. Session
//! tokens are read from local CLI files for the request and are never logged,
//! returned, or written to atr's database. A provider with no proven meter
//! returns `status: unknown`. One provider's failure does not drop the others.
//! Results are cached in memory for about five minutes.

use crate::config::Provider;
use crate::db::Db;
use crate::rfc3339;
use anyhow::Result;
use jiff::Timestamp;
use serde::Serialize;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Exact URLs `plan_usage` may GET. Anything else is rejected before the socket.
pub const ALLOWED_GET_URLS: &[&str] = &[
    "https://api.anthropic.com/api/oauth/usage",
    "https://cursor.com/api/usage-summary",
    "https://chatgpt.com/backend-api/wham/usage",
];

const CACHE_TTL: Duration = Duration::from_secs(5 * 60);

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct PlanRow {
    pub provider: String,
    pub metric: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub used: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pct: Option<f64>,
    /// UTC RFC 3339.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resets_at: Option<String>,
    pub source: String,
    pub read_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// `ok`, `unknown`, or `error`.
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

struct Cache {
    at: Instant,
    rows: Vec<PlanRow>,
}

static CACHE: Mutex<Option<Cache>> = Mutex::new(None);

pub fn cache_clear() {
    *CACHE.lock().expect("plan cache") = None;
}

fn cache_get() -> Option<Vec<PlanRow>> {
    let guard = CACHE.lock().expect("plan cache");
    let c = guard.as_ref()?;
    if c.at.elapsed() > CACHE_TTL {
        return None;
    }
    Some(c.rows.clone())
}

fn cache_put(rows: Vec<PlanRow>) {
    *CACHE.lock().expect("plan cache") = Some(Cache {
        at: Instant::now(),
        rows,
    });
}

/// Last successful (or attempted) rows, including a stale cache. Empty when
/// nothing has been read in this process.
pub fn cached_rows() -> Vec<PlanRow> {
    CACHE
        .lock()
        .expect("plan cache")
        .as_ref()
        .map(|c| c.rows.clone())
        .unwrap_or_default()
}

pub fn line_for(provider: &str) -> String {
    let rows: Vec<_> = cached_rows()
        .into_iter()
        .filter(|r| r.provider == provider)
        .collect();
    if rows.is_empty() {
        return String::new();
    }
    rows.iter()
        .map(|r| match r.status.as_str() {
            "ok" => {
                let pct = r.pct.map(|p| format!(" {p:.1}%")).unwrap_or_default();
                let reset = r
                    .resets_at
                    .as_deref()
                    .map(|t| format!(" resets {t}"))
                    .unwrap_or_default();
                format!("{}{pct}{reset}", r.metric)
            }
            "unknown" => format!(
                "unknown: {}",
                r.reason.as_deref().unwrap_or("no plan meter")
            ),
            _ => format!(
                "error: {}",
                r.error.as_deref().unwrap_or("plan read failed")
            ),
        })
        .collect::<Vec<_>>()
        .join("; ")
}

pub fn session_home() -> PathBuf {
    if let Some(h) = std::env::var_os("ATR_PLAN_USAGE_HOME") {
        return PathBuf::from(h);
    }
    #[cfg(test)]
    {
        PathBuf::from("/tmp/atr-plan-usage-test-empty")
    }
    #[cfg(not(test))]
    crate::config::expand_path("~")
}

pub fn disabled() -> bool {
    std::env::var("ATR_PLAN_USAGE").ok().as_deref() == Some("off")
}

struct Target {
    name: String,
    kind: String,
    preset: String,
    model: String,
    cli: PathBuf,
}

fn targets_from(providers: &[Provider]) -> Vec<Target> {
    let mut out: Vec<Target> = providers
        .iter()
        .map(|p| Target {
            name: p.name.clone(),
            kind: p.kind.clone(),
            preset: p.preset.clone(),
            model: p.model.clone(),
            cli: p.cli.clone(),
        })
        .collect();
    for kind in [
        "claude", "grok", "cursor", "codex", "gemini", "opencode", "agy",
    ] {
        if !out.iter().any(|t| t.kind == kind) {
            out.push(Target {
                name: kind.to_string(),
                kind: kind.to_string(),
                preset: String::new(),
                model: String::new(),
                cli: PathBuf::from(kind),
            });
        }
    }
    out
}

fn local_reason(t: &Target) -> Option<&'static str> {
    let preset = t.preset.to_ascii_lowercase();
    let name = t.name.to_ascii_lowercase();
    let model = t.model.to_ascii_lowercase();
    if t.kind == "opencode" && (preset == "spark" || name == "spark" || model.starts_with("spark/"))
    {
        return Some("local Spark vLLM has no plan meter");
    }
    if t.kind == "opencode"
        && (preset == "ollama"
            || name == "ollama"
            || model.starts_with("ollama/")
            || crate::config::is_ollama_model(&t.preset, &t.model))
    {
        return Some("local Ollama has no plan meter");
    }
    None
}

pub async fn refresh(providers: &[Provider], db: &Db) -> Vec<PlanRow> {
    if let Some(rows) = cache_get() {
        return rows;
    }
    let read_at = rfc3339::millis(Timestamp::now());
    if disabled() {
        let rows = targets_from(providers)
            .into_iter()
            .map(|t| unknown(&t.name, "plan", "ATR_PLAN_USAGE=off", &read_at))
            .collect();
        cache_put(rows);
        return cache_get().unwrap_or_default();
    }
    let targets = targets_from(providers);
    let home = session_home();
    let stored = claude_stored(db, &targets, &read_at);
    let grok_cli = targets
        .iter()
        .find(|t| t.kind == "grok")
        .map(|t| t.cli.clone());
    let grok_names: Vec<String> = targets
        .iter()
        .filter(|t| t.kind == "grok")
        .map(|t| t.name.clone())
        .collect();
    let http_targets = targets.clone();
    let read_at_http = read_at.clone();
    let http_job =
        tokio::task::spawn_blocking(move || read_http(&home, &http_targets, &read_at_http));
    let read_at_grok = read_at.clone();
    let grok_job = async move {
        let Some(cli) = grok_cli else {
            return Vec::new();
        };
        match crate::reported::probe_grok_billing(&cli, Duration::from_secs(8)).await {
            Ok(window) => grok_names
                .into_iter()
                .map(|name| grok_row(&name, &window, &read_at_grok))
                .collect(),
            Err(e) => grok_names
                .into_iter()
                .map(|name| {
                    errored(
                        &name,
                        "weekly",
                        "grok agent stdio _x.ai/billing",
                        &scrub(&e.to_string()),
                        &read_at_grok,
                    )
                })
                .collect(),
        }
    };
    let (http_rows, grok_rows) = tokio::join!(http_job, grok_job);
    let mut rows = http_rows.unwrap_or_else(|e| {
        vec![errored(
            "plan_usage",
            "plan",
            "http",
            &scrub(&e.to_string()),
            &read_at,
        )]
    });
    rows.extend(grok_rows);
    rows.extend(stored_if_missing(&rows, stored));
    let mut rows = hide_placeholder_when_stored_ok(rows);
    rows.sort_by(|a, b| (&a.provider, &a.metric).cmp(&(&b.provider, &b.metric)));
    cache_put(rows);
    cache_get().unwrap_or_default()
}

impl Clone for Target {
    fn clone(&self) -> Self {
        Self {
            name: self.name.clone(),
            kind: self.kind.clone(),
            preset: self.preset.clone(),
            model: self.model.clone(),
            cli: self.cli.clone(),
        }
    }
}

/// Drop the placeholder `unknown` row (source `none`) for a provider that
/// already has a current stored window. Stale stored rows stay.
fn hide_placeholder_when_stored_ok(mut rows: Vec<PlanRow>) -> Vec<PlanRow> {
    let have_stored: Vec<String> = rows
        .iter()
        .filter(|r| r.status == "ok" && r.source.starts_with("stored"))
        .map(|r| r.provider.clone())
        .collect();
    rows.retain(|r| {
        if r.status != "unknown" || r.source.starts_with("stored") {
            return true;
        }
        !have_stored.iter().any(|n| n == &r.provider)
    });
    rows
}

fn stored_if_missing(rows: &[PlanRow], stored: Vec<PlanRow>) -> Vec<PlanRow> {
    stored
        .into_iter()
        .filter(|s| {
            !rows
                .iter()
                .any(|r| r.provider == s.provider && r.metric == s.metric && r.status == "ok")
        })
        .collect()
}

fn claude_stored(db: &Db, targets: &[Target], read_at: &str) -> Vec<PlanRow> {
    let now = Timestamp::now();
    let mut out = Vec::new();
    for t in targets.iter().filter(|t| t.kind == "claude") {
        let Ok(list) = db.list_provider_usage(&t.name) else {
            continue;
        };
        for row in list {
            let Some(util) = row.utilization else {
                continue;
            };
            // A passed resets_at is a finished window. Do not roll the old
            // utilization forward: the row does not prove the next window's use.
            if !resets_still_current(row.resets_at.as_deref(), now) {
                let when = row.resets_at.clone().unwrap_or_default();
                out.push(PlanRow {
                    provider: t.name.clone(),
                    metric: row.window,
                    used: None,
                    limit: None,
                    pct: None,
                    resets_at: row.resets_at,
                    source: "stored claude_stream (stale)".into(),
                    read_at: read_at.into(),
                    error: None,
                    status: "unknown".into(),
                    reason: Some(format!(
                        "stored claude_stream window resets_at {when} has passed; not a current meter"
                    )),
                });
                continue;
            }
            out.push(PlanRow {
                provider: t.name.clone(),
                metric: row.window,
                used: Some(round1(util * 100.0)),
                limit: Some(100.0),
                pct: Some(round1(util * 100.0)),
                resets_at: row.resets_at,
                source: "stored claude_stream".into(),
                read_at: read_at.into(),
                error: None,
                status: "ok".into(),
                reason: None,
            });
        }
    }
    out
}

/// Missing or unparseable `resets_at` is kept (nothing proves it ended).
fn resets_still_current(resets_at: Option<&str>, now: Timestamp) -> bool {
    let Some(raw) = resets_at.map(str::trim).filter(|s| !s.is_empty()) else {
        return true;
    };
    match rfc3339::parse(raw) {
        Some(t) => t > now,
        None => true,
    }
}

fn read_http(home: &Path, targets: &[Target], read_at: &str) -> Vec<PlanRow> {
    let mut rows = Vec::new();
    let claude_names: Vec<_> = names(targets, "claude");
    if !claude_names.is_empty() {
        rows.extend(map_kind(
            &claude_names,
            read_claude(home),
            "https://api.anthropic.com/api/oauth/usage",
            read_at,
        ));
    }
    let cursor_names = names(targets, "cursor");
    if !cursor_names.is_empty() {
        rows.extend(map_kind(
            &cursor_names,
            read_cursor(home),
            "https://cursor.com/api/usage-summary",
            read_at,
        ));
    }
    let codex_names = names(targets, "codex");
    if !codex_names.is_empty() {
        rows.extend(map_kind(
            &codex_names,
            read_codex(home),
            "https://chatgpt.com/backend-api/wham/usage",
            read_at,
        ));
    }
    for t in targets {
        if let Some(reason) = local_reason(t) {
            rows.push(unknown(&t.name, "plan", reason, read_at));
            continue;
        }
        if t.kind == "gemini" {
            rows.push(unknown(
                &t.name,
                "plan",
                "no proven GET plan meter (the Code Assist quota call that returns remaining fraction is a POST, which plan_usage will not send)",
                read_at,
            ));
        } else if t.kind == "agy" {
            rows.push(unknown(
                &t.name,
                "plan",
                "no proven GET plan meter (Antigravity /usage is an interactive panel; statusline quota JSON is not a read-only GET)",
                read_at,
            ));
        } else if t.kind == "opencode" {
            rows.push(unknown(
                &t.name,
                "plan",
                "no proven GET plan meter for this OpenCode CLI session",
                read_at,
            ));
        }
    }
    rows
}

fn names(targets: &[Target], kind: &str) -> Vec<String> {
    targets
        .iter()
        .filter(|t| t.kind == kind)
        .map(|t| t.name.clone())
        .collect()
}

fn map_kind(
    names: &[String],
    parsed: std::result::Result<Vec<Metric>, String>,
    source: &str,
    read_at: &str,
) -> Vec<PlanRow> {
    match parsed {
        Ok(metrics) => names
            .iter()
            .flat_map(|name| {
                metrics.iter().map(|m| PlanRow {
                    provider: name.clone(),
                    metric: m.metric.clone(),
                    used: m.used,
                    limit: m.limit,
                    pct: m.pct,
                    resets_at: m.resets_at.clone(),
                    source: source.into(),
                    read_at: read_at.into(),
                    error: None,
                    status: "ok".into(),
                    reason: None,
                })
            })
            .collect(),
        Err(e) if e.starts_with("unknown:") => names
            .iter()
            .map(|name| {
                unknown(
                    name,
                    "plan",
                    e.trim_start_matches("unknown:").trim(),
                    read_at,
                )
            })
            .collect(),
        Err(e) => names
            .iter()
            .map(|name| errored(name, "plan", source, &e, read_at))
            .collect(),
    }
}

#[derive(Debug)]
struct Metric {
    metric: String,
    used: Option<f64>,
    limit: Option<f64>,
    pct: Option<f64>,
    resets_at: Option<String>,
}

fn unknown(provider: &str, metric: &str, reason: &str, read_at: &str) -> PlanRow {
    PlanRow {
        provider: provider.into(),
        metric: metric.into(),
        used: None,
        limit: None,
        pct: None,
        resets_at: None,
        source: "none".into(),
        read_at: read_at.into(),
        error: None,
        status: "unknown".into(),
        reason: Some(reason.into()),
    }
}

fn errored(provider: &str, metric: &str, source: &str, error: &str, read_at: &str) -> PlanRow {
    PlanRow {
        provider: provider.into(),
        metric: metric.into(),
        used: None,
        limit: None,
        pct: None,
        resets_at: None,
        source: source.into(),
        read_at: read_at.into(),
        error: Some(error.into()),
        status: "error".into(),
        reason: None,
    }
}

fn grok_row(name: &str, window: &crate::reported::UsageWindow, read_at: &str) -> PlanRow {
    let pct = window.utilization.map(|u| round1(u * 100.0));
    PlanRow {
        provider: name.into(),
        metric: if window.window.is_empty() {
            "weekly".into()
        } else {
            window.window.clone()
        },
        used: pct,
        limit: pct.map(|_| 100.0),
        pct,
        resets_at: window.resets_at.map(rfc3339::millis),
        source: "grok agent stdio _x.ai/billing".into(),
        read_at: read_at.into(),
        error: None,
        status: "ok".into(),
        reason: None,
    }
}

pub fn allow_get(url: &str) -> bool {
    ALLOWED_GET_URLS.contains(&url)
}

/// GET `url` only when it is on the allow-list. Redirects are off so a
/// credential header cannot follow to another host. The error string is the
/// status or a transport failure, never a header value.
pub fn http_get(url: &str, headers: &[(&str, &str)]) -> Result<String, String> {
    if !allow_get(url) {
        return Err(format!("url not on plan_usage GET allow-list: {url}"));
    }
    let agent = ureq::AgentBuilder::new()
        .redirects(0)
        .timeout(Duration::from_secs(8))
        .build();
    let mut req = agent.get(url);
    for (k, v) in headers {
        req = req.set(k, v);
    }
    match req.call() {
        Ok(resp) => resp.into_string().map_err(|e| format!("read body: {e}")),
        Err(ureq::Error::Status(code, _)) => Err(format!("http {code}")),
        Err(e) => Err(format!("http transport: {e}")),
    }
}

fn scrub(msg: &str) -> String {
    let mut s = msg.replace('\n', " ");
    if s.len() > 240 {
        s.truncate(240);
    }
    s
}

fn read_claude(home: &Path) -> std::result::Result<Vec<Metric>, String> {
    let path = home.join(".claude/.credentials.json");
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(_) => {
            return Err(
                "unknown: no ~/.claude/.credentials.json (stored claude_stream is used when present)"
                    .into(),
            );
        }
    };
    let v: Value = serde_json::from_str(&text).map_err(|_| "claude credentials are not json")?;
    let token = v
        .pointer("/claudeAiOauth/accessToken")
        .and_then(|t| t.as_str())
        .filter(|t| !t.is_empty())
        .ok_or_else(|| "unknown: claude credentials have no accessToken".to_string())?;
    let auth = format!("Bearer {token}");
    let body = http_get(
        "https://api.anthropic.com/api/oauth/usage",
        &[
            ("Authorization", auth.as_str()),
            ("anthropic-beta", "oauth-2025-04-20"),
            ("User-Agent", "claude-code/2.1.0"),
            ("Accept", "application/json"),
        ],
    )
    .map_err(|e| scrub(&e.replace(token, "[redacted]")))?;
    parse_claude_usage(&body).map_err(|e| scrub(&e.replace(token, "[redacted]")))
}

fn parse_claude_usage(body: &str) -> std::result::Result<Vec<Metric>, String> {
    let v: Value = serde_json::from_str(body).map_err(|e| format!("claude usage json: {e}"))?;
    let mut out = Vec::new();
    for key in [
        "five_hour",
        "seven_day",
        "seven_day_opus",
        "seven_day_sonnet",
    ] {
        let Some(w) = v.get(key).filter(|w| !w.is_null()) else {
            continue;
        };
        let pct = json_f64_at(w.get("utilization"));
        out.push(Metric {
            metric: key.into(),
            used: pct,
            limit: pct.map(|_| 100.0),
            pct,
            resets_at: w
                .get("resets_at")
                .and_then(|r| r.as_str())
                .map(normalize_time),
        });
    }
    if let Some(extra) = v.get("extra_usage").filter(|e| !e.is_null()) {
        let used_cents = json_f64_at(extra.get("used_credits"));
        let limit_cents = json_f64_at(extra.get("monthly_limit"));
        if used_cents.is_some() || limit_cents.is_some() {
            let used = used_cents.map(|c| c / 100.0);
            let limit = limit_cents.map(|c| c / 100.0);
            out.push(Metric {
                metric: "extra_usage_usd".into(),
                used,
                limit,
                pct: match (used, limit) {
                    (Some(u), Some(l)) if l > 0.0 => Some(round1(u / l * 100.0)),
                    _ => json_f64_at(extra.get("utilization")),
                },
                resets_at: None,
            });
        }
    }
    if out.is_empty() {
        return Err("claude usage response had no windows".into());
    }
    Ok(out)
}

fn read_cursor(home: &Path) -> std::result::Result<Vec<Metric>, String> {
    let db = cursor_state_db(home);
    let sqlite_token = if db.exists() {
        read_sqlite_item(&db, "cursorAuth/accessToken")
    } else {
        None
    };
    let explicit = cursor_explicit_user_id(home, db.exists().then_some(db.as_path()));
    let mut token = sqlite_token.clone().unwrap_or_default();
    if !cursor_session_token_usable(&token) {
        match keychain_cursor_access_token() {
            Ok(kc) => token = kc,
            Err(reason) if sqlite_token.is_some() => {
                return Err(format!("unknown: {reason}"));
            }
            Err(_) => {}
        }
    }
    if token.trim().is_empty() {
        if !db.exists() {
            return Err("unknown: no Cursor state.vscdb (cursorAuth/accessToken)".into());
        }
        return Err("unknown: Cursor state.vscdb has no cursorAuth/accessToken".into());
    }
    if !cursor_session_token_usable(&token) {
        return Err(
            "unknown: Cursor access token is not a WorkOS session JWT (expected user_…::jwt or a JWT). state.vscdb cursorAuth/accessToken is encrypted when cursor.storage.authTokenEncryptionAtRest is set; macOS keychain item cursor-access-token (account cursor-user) was not readable"
                .into(),
        );
    }
    let user = cursor_user_id(&token, &explicit)
        .ok_or_else(|| "unknown: Cursor session has no user id".to_string())?;
    let cookie = format!("WorkosCursorSessionToken={user}%3A%3A{token}");
    let body = http_get(
        "https://cursor.com/api/usage-summary",
        &[
            ("Cookie", cookie.as_str()),
            ("Accept", "application/json"),
            ("Origin", "https://cursor.com"),
        ],
    )
    .map_err(|e| scrub(&e.replace(&token, "[redacted]")))?;
    parse_cursor_usage(&body).map_err(|e| scrub(&e.replace(&token, "[redacted]")))
}

fn cursor_state_db(home: &Path) -> PathBuf {
    if cfg!(target_os = "macos") {
        home.join("Library/Application Support/Cursor/User/globalStorage/state.vscdb")
    } else if cfg!(target_os = "windows") {
        home.join("AppData/Roaming/Cursor/User/globalStorage/state.vscdb")
    } else {
        home.join(".config/Cursor/User/globalStorage/state.vscdb")
    }
}

fn read_sqlite_item(db: &Path, key: &str) -> Option<String> {
    let conn = rusqlite::Connection::open_with_flags(
        db,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .ok()?;
    let mut stmt = conn
        .prepare("SELECT value FROM ItemTable WHERE key = ?1")
        .ok()?;
    let val: rusqlite::types::Value = stmt.query_row([key], |r| r.get(0)).ok()?;
    match val {
        rusqlite::types::Value::Text(s) => Some(s),
        rusqlite::types::Value::Blob(b) => String::from_utf8(b).ok(),
        _ => None,
    }
}

/// WorkOS user id (`user_…`) from an auth id, a raw id, or empty.
fn workos_user_id(raw: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    let tail = raw.rsplit('|').next().unwrap_or(raw).trim();
    if tail.starts_with("user_") && tail.len() > "user_".len() {
        Some(tail.to_string())
    } else if raw.starts_with("user_") {
        Some(raw.to_string())
    } else {
        None
    }
}

/// User id stored beside the access token. Newer Cursor builds omit
/// `cursorAuth/userId` and encrypt `cursorAuth/accessToken`, so the id is
/// taken from `stripeMembershipAuthId`, `cachedScopedProfile.authId`,
/// `glass.lastSignedInAuthId`, or `~/.cursor/cli-config.json` `authInfo.authId`.
fn cursor_explicit_user_id(home: &Path, db: Option<&Path>) -> String {
    let mut candidates: Vec<String> = Vec::new();
    if let Some(db) = db {
        for key in [
            "cursorAuth/userId",
            "cursorAuth/stripeMembershipAuthId",
            "glass.lastSignedInAuthId",
        ] {
            if let Some(v) = read_sqlite_item(db, key) {
                candidates.push(v);
            }
        }
        if let Some(profile) = read_sqlite_item(db, "cursorAuth/cachedScopedProfile") {
            if let Ok(v) = serde_json::from_str::<Value>(&profile) {
                if let Some(id) = v.get("authId").and_then(|s| s.as_str()) {
                    candidates.push(id.to_string());
                }
            }
        }
    }
    let cli = home.join(".cursor/cli-config.json");
    if let Ok(text) = std::fs::read_to_string(&cli) {
        if let Ok(v) = serde_json::from_str::<Value>(&text) {
            if let Some(id) = v.pointer("/authInfo/authId").and_then(|s| s.as_str()) {
                candidates.push(id.to_string());
            }
        }
    }
    for c in &candidates {
        if let Some(id) = workos_user_id(c) {
            return id;
        }
    }
    candidates
        .into_iter()
        .find(|c| !c.trim().is_empty())
        .unwrap_or_default()
}

fn cursor_session_token_usable(token: &str) -> bool {
    let token = token.trim();
    if token.is_empty() || token.contains(char::is_whitespace) {
        return false;
    }
    if let Some((id, rest)) = token.split_once("::") {
        return id.starts_with("user_") && rest.split('.').nth(2).is_some();
    }
    let mut parts = token.split('.');
    parts.next().is_some()
        && parts.next().is_some()
        && parts.next().is_some()
        && !token.contains(':')
}

/// Plaintext session from the Cursor macOS keychain. The `state.vscdb` copy
/// is ciphertext when `cursor.storage.authTokenEncryptionAtRest` is true.
/// Stderr is dropped so a prompt or blob cannot land in the error string.
fn keychain_cursor_access_token() -> Result<String, String> {
    #[cfg(not(target_os = "macos"))]
    {
        return Err(
            "Cursor access token in state.vscdb is not a session JWT and there is no macOS keychain item cursor-access-token to read"
                .into(),
        );
    }
    #[cfg(target_os = "macos")]
    {
        let out = std::process::Command::new("/usr/bin/security")
            .args([
                "find-generic-password",
                "-s",
                "cursor-access-token",
                "-a",
                "cursor-user",
                "-w",
            ])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .output()
            .map_err(|e| format!("could not run security for cursor-access-token: {e}"))?;
        if !out.status.success() {
            let code = out.status.code().unwrap_or(-1);
            return Err(format!(
                "macOS keychain item cursor-access-token (account cursor-user) is not readable from this session (security exit {code}). state.vscdb cursorAuth/accessToken is not a session JWT"
            ));
        }
        let token = String::from_utf8(out.stdout)
            .map_err(|_| "keychain cursor-access-token was not utf-8".to_string())?;
        let token = token.trim().to_string();
        if token.is_empty() {
            return Err(
                "macOS keychain item cursor-access-token (account cursor-user) was empty".into(),
            );
        }
        Ok(token)
    }
}

fn cursor_user_id(token: &str, explicit: &str) -> Option<String> {
    if let Some(id) = workos_user_id(explicit) {
        return Some(id);
    }
    let explicit = explicit.trim();
    if !explicit.is_empty() && !explicit.contains('|') {
        return Some(explicit.to_string());
    }
    if let Some((id, _)) = token.split_once("::") {
        if let Some(id) = workos_user_id(id) {
            return Some(id);
        }
    }
    let payload = token.split('.').nth(1)?;
    let bytes = b64url(payload)?;
    let v: Value = serde_json::from_slice(&bytes).ok()?;
    let sub = v.get("sub").and_then(|s| s.as_str())?;
    let id = sub.rsplit('|').next().unwrap_or(sub);
    if id.is_empty() {
        None
    } else {
        Some(id.to_string())
    }
}

fn b64url(s: &str) -> Option<Vec<u8>> {
    fn val(c: u8) -> Option<u8> {
        match c {
            b'A'..=b'Z' => Some(c - b'A'),
            b'a'..=b'z' => Some(c - b'a' + 26),
            b'0'..=b'9' => Some(c - b'0' + 52),
            b'+' | b'-' => Some(62),
            b'/' | b'_' => Some(63),
            _ => None,
        }
    }
    let mut out = Vec::new();
    let mut buf: u32 = 0;
    let mut bits = 0;
    for &c in s.as_bytes() {
        if c == b'=' {
            break;
        }
        let v = val(c)?;
        buf = (buf << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
        }
    }
    Some(out)
}

fn parse_cursor_usage(body: &str) -> std::result::Result<Vec<Metric>, String> {
    let v: Value = serde_json::from_str(body).map_err(|e| format!("cursor usage json: {e}"))?;
    let reset = v
        .get("billingCycleEnd")
        .and_then(|s| s.as_str())
        .map(normalize_time);
    let auto = first_f64(
        &v,
        &[
            "/individualUsage/plan/autoPercentUsed",
            "/planUsage/autoPercentUsed",
        ],
    );
    let api = first_f64(
        &v,
        &[
            "/individualUsage/plan/apiPercentUsed",
            "/planUsage/apiPercentUsed",
        ],
    );
    let mut out = Vec::new();
    if let Some(pct) = auto {
        out.push(percent_metric("cursor_models", pct, reset.clone()));
    }
    if let Some(pct) = api {
        out.push(percent_metric("other_models", pct, reset.clone()));
    }
    if let Some((used, limit)) = on_demand(&v) {
        let (used, limit) = if limit >= 100.0 && is_whole(used) && is_whole(limit) {
            (used / 100.0, limit / 100.0)
        } else {
            (used, limit)
        };
        out.push(Metric {
            metric: "on_demand_usd".into(),
            pct: if limit > 0.0 {
                Some(round1(used / limit * 100.0))
            } else {
                None
            },
            used: Some(used),
            limit: Some(limit),
            resets_at: reset,
        });
    }
    if out.is_empty() {
        return Err("cursor usage-summary had no plan fields".into());
    }
    Ok(out)
}

fn on_demand(v: &Value) -> Option<(f64, f64)> {
    let pairs = [
        (
            "/individualUsage/onDemand/used",
            "/individualUsage/onDemand/limit",
        ),
        ("/teamUsage/onDemand/used", "/teamUsage/onDemand/limit"),
        (
            "/spendLimitUsage/individualUsed",
            "/spendLimitUsage/individualLimit",
        ),
    ];
    for (u, l) in pairs {
        if let (Some(used), Some(limit)) = (
            v.pointer(u).and_then(num_f64),
            v.pointer(l).and_then(num_f64),
        ) {
            return Some((used, limit));
        }
    }
    None
}

fn percent_metric(metric: &str, pct: f64, resets_at: Option<String>) -> Metric {
    Metric {
        metric: metric.into(),
        used: Some(pct),
        limit: Some(100.0),
        pct: Some(pct),
        resets_at,
    }
}

fn read_codex(home: &Path) -> std::result::Result<Vec<Metric>, String> {
    let path = home.join(".codex/auth.json");
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(_) => return Err("unknown: no ~/.codex/auth.json".into()),
    };
    let v: Value = serde_json::from_str(&text).map_err(|_| "codex auth.json is not json")?;
    let token = v
        .pointer("/tokens/access_token")
        .and_then(|t| t.as_str())
        .filter(|t| !t.is_empty());
    let Some(token) = token else {
        if v.get("OPENAI_API_KEY").and_then(|k| k.as_str()).is_some() {
            return Err(
                "unknown: codex auth is an API key; ChatGPT plan wham/usage does not apply".into(),
            );
        }
        return Err("unknown: codex auth.json has no tokens.access_token".into());
    };
    let account = v
        .pointer("/tokens/account_id")
        .and_then(|t| t.as_str())
        .unwrap_or("");
    let auth = format!("Bearer {token}");
    let mut headers = vec![
        ("Authorization", auth.as_str()),
        ("Accept", "application/json"),
    ];
    if !account.is_empty() {
        headers.push(("ChatGPT-Account-Id", account));
    }
    let body = http_get("https://chatgpt.com/backend-api/wham/usage", &headers)
        .map_err(|e| scrub(&e.replace(token, "[redacted]")))?;
    parse_codex_usage(&body).map_err(|e| scrub(&e.replace(token, "[redacted]")))
}

fn parse_codex_usage(body: &str) -> std::result::Result<Vec<Metric>, String> {
    let v: Value = serde_json::from_str(body).map_err(|e| format!("codex usage json: {e}"))?;
    let rate = v.get("rate_limit").unwrap_or(&v);
    let mut out = Vec::new();
    for key in ["primary_window", "secondary_window", "tertiary_window"] {
        let Some(w) = rate.get(key).filter(|w| !w.is_null()) else {
            continue;
        };
        let Some(pct) = json_f64_at(w.get("used_percent")) else {
            continue;
        };
        let metric = codex_metric(key, w.get("limit_window_seconds").and_then(num_f64));
        let resets_at = w
            .get("reset_at")
            .and_then(num_f64)
            .and_then(|n| Timestamp::from_second(n as i64).ok())
            .map(rfc3339::millis)
            .or_else(|| {
                let after = json_f64_at(w.get("reset_after_seconds"))?;
                let t = Timestamp::now() + jiff::SignedDuration::from_secs(after as i64);
                Some(rfc3339::millis(t))
            });
        out.push(Metric {
            metric,
            used: Some(pct),
            limit: Some(100.0),
            pct: Some(pct),
            resets_at,
        });
    }
    if let Some(ind) = v.pointer("/spend_control/individual_limit") {
        if let (Some(used), Some(limit)) = (
            json_f64_or_str(ind.get("used")),
            json_f64_or_str(ind.get("limit")),
        ) {
            out.push(Metric {
                metric: "individual_spend_usd".into(),
                pct: if limit > 0.0 {
                    Some(round1(used / limit * 100.0))
                } else {
                    json_f64_at(ind.get("used_percent"))
                },
                used: Some(used),
                limit: Some(limit),
                resets_at: ind
                    .get("reset_at")
                    .and_then(num_f64)
                    .and_then(|n| Timestamp::from_second(n as i64).ok())
                    .map(rfc3339::millis),
            });
        }
    }
    if out.is_empty() {
        return Err("wham/usage had no rate-limit windows".into());
    }
    Ok(out)
}

fn json_f64_or_str(v: Option<&Value>) -> Option<f64> {
    let v = v?;
    num_f64(v).or_else(|| v.as_str()?.parse().ok())
}

fn codex_metric(key: &str, seconds: Option<f64>) -> String {
    let label = match seconds.map(|s| s as i64) {
        Some(s) if (17_000..19_000).contains(&s) => "5h",
        Some(s) if (500_000..700_000).contains(&s) => "7d",
        Some(s) if (2_400_000..2_800_000).contains(&s) => "30d",
        Some(s) => return format!("{key}_{s}s"),
        None => {
            return match key {
                "primary_window" => "primary".into(),
                "secondary_window" => "secondary".into(),
                _ => "tertiary".into(),
            };
        }
    };
    label.into()
}

fn first_f64(v: &Value, paths: &[&str]) -> Option<f64> {
    paths.iter().find_map(|p| v.pointer(p).and_then(num_f64))
}

fn num_f64(v: &Value) -> Option<f64> {
    v.as_f64()
        .or_else(|| v.as_i64().map(|n| n as f64))
        .or_else(|| v.as_u64().map(|n| n as f64))
        .filter(|n| n.is_finite())
}

fn json_f64_at(v: Option<&Value>) -> Option<f64> {
    num_f64(v?)
}

fn is_whole(n: f64) -> bool {
    (n - n.round()).abs() < 1e-6
}

fn round1(n: f64) -> f64 {
    (n * 10.0).round() / 10.0
}

fn normalize_time(s: &str) -> String {
    if let Some(t) = rfc3339::parse(s) {
        rfc3339::millis(t)
    } else {
        s.to_string()
    }
}

pub fn format_text(rows: &[PlanRow]) -> String {
    let mut out = String::from(
        "provider    metric               pct   used   limit  resets_at (UTC)  status\n",
    );
    for r in rows {
        let _ = std::fmt::Write::write_fmt(
            &mut out,
            format_args!(
                "{:<11} {:<20} {:>6} {:>6} {:>7}  {:<22} {}\n",
                r.provider,
                r.metric,
                r.pct
                    .map(|p| format!("{p:.1}"))
                    .unwrap_or_else(|| "-".into()),
                r.used
                    .map(|p| format!("{p:.1}"))
                    .unwrap_or_else(|| "-".into()),
                r.limit
                    .map(|p| format!("{p:.1}"))
                    .unwrap_or_else(|| "-".into()),
                r.resets_at.as_deref().unwrap_or("-"),
                if r.status == "ok" {
                    "ok".into()
                } else if r.status == "unknown" {
                    format!("unknown: {}", r.reason.as_deref().unwrap_or(""))
                } else {
                    format!("error: {}", r.error.as_deref().unwrap_or(""))
                }
            ),
        );
    }
    out
}

pub fn rows_json(rows: &[PlanRow]) -> Value {
    json!({ "plan_usage": rows })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allow_list_rejects_other_urls_before_any_socket() {
        for url in ALLOWED_GET_URLS {
            assert!(url.starts_with("https://"), "{url}");
            assert!(!url.contains('?'), "{url}");
        }
        let err = http_get("https://example.com/steal", &[]).unwrap_err();
        assert!(err.contains("allow-list"), "{err}");
        let err = http_get("http://api.anthropic.com/api/oauth/usage", &[]).unwrap_err();
        assert!(err.contains("allow-list"), "{err}");
        let err = http_get("https://api.anthropic.com/api/oauth/usage/../admin", &[]).unwrap_err();
        assert!(err.contains("allow-list"), "{err}");
        let src = include_str!("plan_usage.rs");
        let prod = src.split("mod tests").next().unwrap_or(src);
        assert!(
            !prod.contains("ureq::post") && !prod.contains(".post("),
            "plan_usage must not POST"
        );
    }

    #[test]
    fn parses_claude_fixture() {
        let body = r#"{"five_hour":{"utilization":33.0,"resets_at":"2026-04-11T07:00:00.528Z"},"seven_day":{"utilization":13.0,"resets_at":"2026-04-17T00:59:59Z"},"seven_day_sonnet":null,"extra_usage":{"is_enabled":true,"used_credits":500,"monthly_limit":10000}}"#;
        let rows = parse_claude_usage(body).unwrap();
        assert_eq!(rows[0].metric, "five_hour");
        assert_eq!(rows[0].pct, Some(33.0));
        assert!(rows.iter().any(|r| r.metric == "seven_day"));
        let extra = rows.iter().find(|r| r.metric == "extra_usage_usd").unwrap();
        assert_eq!(extra.used, Some(5.0));
        assert_eq!(extra.limit, Some(100.0));
    }

    #[test]
    fn parses_cursor_fixture_including_cent_on_demand() {
        let body = r#"{"billingCycleEnd":"2026-08-01T00:00:00.000Z","planUsage":{"autoPercentUsed":12.5,"apiPercentUsed":40},"individualUsage":{"onDemand":{"used":250,"limit":5000}}}"#;
        let rows = parse_cursor_usage(body).unwrap();
        assert_eq!(rows[0].metric, "cursor_models");
        assert_eq!(rows[0].pct, Some(12.5));
        assert_eq!(rows[1].metric, "other_models");
        let od = rows.iter().find(|r| r.metric == "on_demand_usd").unwrap();
        assert_eq!(od.used, Some(2.5));
        assert_eq!(od.limit, Some(50.0));
        assert!(od.resets_at.as_deref().unwrap().starts_with("2026-08-01"));
    }

    #[test]
    fn parses_codex_wham_fixture() {
        let body = r#"{"plan_type":"plus","rate_limit":{"primary_window":{"used_percent":42.5,"limit_window_seconds":18000,"reset_at":1700000100},"secondary_window":{"used_percent":18,"limit_window_seconds":604800,"reset_at":1700000200}},"spend_control":{"individual_limit":{"used":"36.5","limit":"1000","reset_at":1788220800}}}"#;
        let rows = parse_codex_usage(body).unwrap();
        assert_eq!(rows[0].metric, "5h");
        assert_eq!(rows[0].pct, Some(42.5));
        assert_eq!(rows[1].metric, "7d");
        let spend = rows
            .iter()
            .find(|r| r.metric == "individual_spend_usd")
            .unwrap();
        assert_eq!(spend.used, Some(36.5));
        assert_eq!(spend.limit, Some(1000.0));
    }

    #[test]
    fn cursor_user_id_from_jwt_sub_does_not_need_the_signature() {
        // payload {"sub":"google-oauth2|user_abc"}
        let jwt = "e30.eyJzdWIiOiJnb29nbGUtb2F1dGgyfHVzZXJfYWJjIn0.sig";
        assert_eq!(cursor_user_id(jwt, "").as_deref(), Some("user_abc"));
        assert_eq!(cursor_user_id(jwt, "explicit").as_deref(), Some("explicit"));
        assert_eq!(
            cursor_user_id(jwt, "github|user_from_auth").as_deref(),
            Some("user_from_auth")
        );
        assert_eq!(
            workos_user_id("github|user_from_auth").as_deref(),
            Some("user_from_auth")
        );
        assert!(cursor_session_token_usable(jwt));
        assert!(cursor_session_token_usable("user_abc::e30.e30.sig"));
        assert!(!cursor_session_token_usable("cipher-id:v1:notajwt"));
    }

    #[test]
    fn cursor_encrypted_sqlite_token_uses_auth_id_and_does_not_claim_missing_user() {
        let home = tempfile::tempdir().unwrap();
        let db_path = cursor_state_db(home.path());
        std::fs::create_dir_all(db_path.parent().unwrap()).unwrap();
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute(
            "CREATE TABLE ItemTable (key TEXT PRIMARY KEY, value TEXT)",
            [],
        )
        .unwrap();
        let token = "cipher-id-aaaa-bbbbbbb:v1:not-a-jwt-token-value";
        conn.execute(
            "INSERT INTO ItemTable (key, value) VALUES (?1, ?2)",
            ["cursorAuth/accessToken", token],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO ItemTable (key, value) VALUES (?1, ?2)",
            ["cursorAuth/stripeMembershipAuthId", "github|user_abc"],
        )
        .unwrap();
        let err = read_cursor(home.path()).unwrap_err();
        assert!(err.starts_with("unknown:"), "{err}");
        assert!(
            !err.contains("no user id"),
            "auth id was present, got {err}"
        );
        assert!(
            err.contains("cursor-access-token")
                || err.contains("not a session JWT")
                || err.contains("not a WorkOS"),
            "{err}"
        );
        assert!(!err.contains(token), "{err}");
        assert!(!err.contains("user_abc"), "{err}");
    }

    #[test]
    fn stored_claude_window_past_resets_at_is_stale() {
        let dir = tempfile::tempdir().unwrap();
        let db = crate::db::Db::open(&dir.path().join("state.db")).unwrap();
        db.upsert_provider_usage(
            "claude",
            "five_hour",
            Some(0.42),
            None,
            None,
            Some("2026-10-31T17:40:00.000Z"),
            Some("ok"),
            "2026-10-31T12:00:00.000Z",
        )
        .unwrap();
        db.upsert_provider_usage(
            "claude",
            "seven_day",
            Some(0.1),
            None,
            None,
            Some("2099-01-01T00:00:00.000Z"),
            Some("ok"),
            "2026-10-31T12:00:00.000Z",
        )
        .unwrap();
        let targets = [Target {
            name: "claude".into(),
            kind: "claude".into(),
            preset: String::new(),
            model: String::new(),
            cli: PathBuf::from("claude"),
        }];
        let rows = claude_stored(&db, &targets, "2026-11-25T16:02:00.000Z");
        let five = rows.iter().find(|r| r.metric == "five_hour").unwrap();
        assert_eq!(five.status, "unknown");
        assert_eq!(five.source, "stored claude_stream (stale)");
        assert!(
            five.reason.as_deref().unwrap().contains("has passed"),
            "{five:?}"
        );
        assert!(five.pct.is_none());
        let seven = rows.iter().find(|r| r.metric == "seven_day").unwrap();
        assert_eq!(seven.status, "ok");
        assert_eq!(seven.source, "stored claude_stream");
        assert_eq!(seven.pct, Some(10.0));
        assert!(!resets_still_current(
            Some("2026-10-31T17:40:00.000Z"),
            rfc3339::parse("2026-11-25T16:02:00.000Z").unwrap()
        ));
        assert!(resets_still_current(None, Timestamp::now()));
    }

    #[test]
    fn stale_stored_row_is_kept_when_another_window_is_current() {
        let rows = vec![
            unknown("claude", "plan", "no credentials", "t"),
            PlanRow {
                provider: "claude".into(),
                metric: "five_hour".into(),
                used: None,
                limit: None,
                pct: None,
                resets_at: Some("2026-10-31T17:40:00.000Z".into()),
                source: "stored claude_stream (stale)".into(),
                read_at: "t".into(),
                error: None,
                status: "unknown".into(),
                reason: Some("stored claude_stream window resets_at has passed".into()),
            },
            PlanRow {
                provider: "claude".into(),
                metric: "seven_day".into(),
                used: Some(3.0),
                limit: Some(100.0),
                pct: Some(3.0),
                resets_at: Some("2026-11-28T00:00:00.000Z".into()),
                source: "stored claude_stream".into(),
                read_at: "t".into(),
                error: None,
                status: "ok".into(),
                reason: None,
            },
        ];
        let kept = hide_placeholder_when_stored_ok(rows);
        assert!(kept.iter().all(|r| r.metric != "plan"));
        assert!(kept
            .iter()
            .any(|r| r.metric == "five_hour" && r.status == "unknown"));
        assert!(kept
            .iter()
            .any(|r| r.metric == "seven_day" && r.status == "ok"));
    }

    #[test]
    fn missing_files_are_unknown_and_do_not_include_secrets() {
        let home = tempfile::tempdir().unwrap();
        let secret = "super-secret-session-token-value";
        std::fs::create_dir_all(home.path().join(".claude")).unwrap();
        std::fs::write(
            home.path().join(".claude/.credentials.json"),
            format!(r#"{{"claudeAiOauth":{{"accessToken":"{secret}"}}}}"#),
        )
        .unwrap();
        // Allow-list passes, then the socket fails. The error must not echo the token.
        let err = read_claude(home.path()).unwrap_err();
        assert!(!err.contains(secret), "{err}");
        let empty = tempfile::tempdir().unwrap();
        let err = read_cursor(empty.path()).unwrap_err();
        assert!(err.starts_with("unknown:"), "{err}");
        let err = read_codex(empty.path()).unwrap_err();
        assert!(err.starts_with("unknown:"), "{err}");
        let err = read_claude(empty.path()).unwrap_err();
        assert!(err.starts_with("unknown:"), "{err}");
    }

    #[test]
    fn spark_and_gemini_are_unknown_without_http() {
        let home = tempfile::tempdir().unwrap();
        let targets = vec![
            Target {
                name: "spark".into(),
                kind: "opencode".into(),
                preset: "spark".into(),
                model: "spark/deepseek-v4-flash-0731".into(),
                cli: PathBuf::from("opencode"),
            },
            Target {
                name: "gemini".into(),
                kind: "gemini".into(),
                preset: String::new(),
                model: String::new(),
                cli: PathBuf::from("gemini"),
            },
            Target {
                name: "kimi".into(),
                kind: "opencode".into(),
                preset: "kimi".into(),
                model: "moonshotai/kimi-k2.6".into(),
                cli: PathBuf::from("opencode"),
            },
        ];
        let rows = read_http(home.path(), &targets, "2026-11-25T00:00:00.000Z");
        let spark = rows.iter().find(|r| r.provider == "spark").unwrap();
        assert_eq!(spark.status, "unknown");
        assert!(
            spark.reason.as_deref().unwrap().contains("Spark"),
            "{spark:?}"
        );
        let gemini = rows.iter().find(|r| r.provider == "gemini").unwrap();
        assert_eq!(gemini.status, "unknown");
        assert!(
            gemini.reason.as_deref().unwrap().contains("POST"),
            "{gemini:?}"
        );
        let kimi = rows.iter().find(|r| r.provider == "kimi").unwrap();
        assert_eq!(kimi.status, "unknown");
    }

    #[test]
    fn one_parse_failure_does_not_drop_the_other_fixture() {
        let claude = parse_claude_usage("{").unwrap_err();
        assert!(claude.contains("json"));
        assert!(parse_codex_usage(
            r#"{"rate_limit":{"primary_window":{"used_percent":1,"limit_window_seconds":18000,"reset_at":1700000100}}}"#
        )
        .is_ok());
    }
}
