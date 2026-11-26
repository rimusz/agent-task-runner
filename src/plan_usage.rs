//! Read-only vendor plan meters for `atr usage --plan`, MCP `plan_usage`,
//! and the dashboard plan line.
//!
//! HTTP GETs must use [`ALLOWED_GET_URLS`]. The only POST is
//! [`CURSOR_REFRESH_URL`]. Session tokens are read from local CLI files or
//! the login keychain for the request and are never logged, returned, or
//! written to atr's database. A fresh Cursor access token is written back
//! to the keychain item it came from, or kept in this process if that write
//! fails. A provider with no proven meter
//! returns `status: unknown`. Antigravity (`agy`) is not an HTTP GET: the
//! cache refresh runs `agy -p /usage --output-format text` (20s, no login).
//! One provider's failure does not drop the others.
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

/// The one POST `plan_usage` may make: Cursor's OAuth refresh, same body the
/// CLI sends (`grant_type=refresh_token`, `client_id=cli`).
pub const CURSOR_REFRESH_URL: &str = "https://api2.cursor.sh/oauth/token";
pub const ALLOWED_POST_URLS: &[&str] = &[CURSOR_REFRESH_URL];

const CACHE_TTL: Duration = Duration::from_secs(5 * 60);

/// Shown as the Codex dashboard `plan_line` with no `unknown:` prefix.
pub const CODEX_API_KEY_REASON: &str = "codex auth is an API key; usage not supported";

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

pub(crate) fn cache_put(rows: Vec<PlanRow>) {
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
            "unknown" => {
                let reason = r.reason.as_deref().unwrap_or("no plan meter");
                if reason == CODEX_API_KEY_REASON {
                    reason.to_string()
                } else {
                    format!("unknown: {reason}")
                }
            }
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
    // `agy` is not synthesized. The `/usage` probe runs only for a configured
    // provider (its `cli`), never a PATH lookup named `agy`.
    for kind in ["claude", "grok", "cursor", "codex", "gemini", "opencode"] {
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
    let agy_cli = agy_probe_cli(providers);
    let agy_names: Vec<String> = providers
        .iter()
        .filter(|p| p.kind == "agy")
        .map(|p| p.name.clone())
        .collect();
    let read_at_agy = read_at.clone();
    let agy_job = tokio::task::spawn_blocking(move || {
        let Some(cli) = agy_cli else {
            return Vec::new();
        };
        map_kind(
            &agy_names,
            run_agy_usage(&cli),
            "agy -p /usage",
            &read_at_agy,
        )
    });
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
    let (http_rows, grok_rows, agy_rows) = tokio::join!(http_job, grok_job, agy_job);
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
    rows.extend(agy_rows.unwrap_or_else(|e| {
        vec![errored(
            "plan_usage",
            "plan",
            "agy -p /usage",
            &scrub(&e.to_string()),
            &read_at,
        )]
    }));
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

pub fn allow_post(url: &str) -> bool {
    ALLOWED_POST_URLS.contains(&url)
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

fn http_post_json(url: &str, headers: &[(&str, &str)], body: &str) -> Result<String, String> {
    if !allow_post(url) {
        return Err("url not on plan_usage POST allow-list".into());
    }
    let agent = crate::util::ureq_agent(Duration::from_secs(20));
    let mut req = agent.post(url);
    for (k, v) in headers {
        req = req.set(k, v);
    }
    match req.send_string(body) {
        Ok(resp) => resp.into_string().map_err(|e| format!("read body: {e}")),
        Err(ureq::Error::Status(code, resp)) => {
            let text = resp.into_string().unwrap_or_default();
            Err(refresh_http_error(code, &text))
        }
        Err(e) => Err(format!("http transport: {e}")),
    }
}

/// Status and a short OAuth `error` code. The body is not returned.
fn refresh_http_error(code: u16, body: &str) -> String {
    let code_name = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(str::to_string))
        .filter(|s| {
            !s.is_empty()
                && s.len() < 40
                && s.chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        });
    match code_name {
        Some(name) => format!("cursor token refresh http {code} {name}"),
        None => format!("cursor token refresh http {code}"),
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
    let token = match cursor_access_for_usage(&token) {
        Ok(t) => t,
        Err(e) => return Err(scrub(&e)),
    };
    let user = cursor_user_id(&token, &explicit)
        .ok_or_else(|| "unknown: Cursor session has no user id".to_string())?;
    match cursor_usage_request(&user, &token) {
        Ok(body) => parse_cursor_usage(&body).map_err(|e| scrub(&e.replace(&token, "[redacted]"))),
        Err(e) if e == "http 401" => {
            let fresh = refresh_cursor_access().map_err(|err| scrub(&err))?;
            let user = cursor_user_id(&fresh, &explicit)
                .ok_or_else(|| "unknown: Cursor session has no user id".to_string())?;
            let body = cursor_usage_request(&user, &fresh)
                .map_err(|err| scrub(&err.replace(&fresh, "[redacted]")))?;
            parse_cursor_usage(&body).map_err(|e| scrub(&e.replace(&fresh, "[redacted]")))
        }
        Err(e) => Err(scrub(&e.replace(&token, "[redacted]"))),
    }
}

fn cursor_usage_request(user: &str, token: &str) -> Result<String, String> {
    let cookie = format!("WorkosCursorSessionToken={user}%3A%3A{token}");
    http_get(
        "https://cursor.com/api/usage-summary",
        &[
            ("Cookie", cookie.as_str()),
            ("Accept", "application/json"),
            ("Origin", "https://cursor.com"),
        ],
    )
}

/// Use a still-valid access token. An expired JWT is replaced from the
/// keychain refresh token before the usage call.
fn cursor_access_for_usage(token: &str) -> Result<String, String> {
    if !jwt_expired(token, Timestamp::now().as_second()) {
        return Ok(token.to_string());
    }
    if let Some(mem) = cursor_access_mem() {
        if !jwt_expired(&mem, Timestamp::now().as_second()) {
            return Ok(mem);
        }
    }
    refresh_cursor_access()
}

fn cursor_access_mem() -> Option<String> {
    CURSOR_ACCESS_MEM.lock().ok().and_then(|g| g.clone())
}

fn cursor_access_mem_put(token: String) {
    if let Ok(mut g) = CURSOR_ACCESS_MEM.lock() {
        *g = Some(token);
    }
}

static CURSOR_ACCESS_MEM: Mutex<Option<String>> = Mutex::new(None);

/// `exp` is in the past, or within 60s. A token with no `exp` is not treated
/// as expired (a 401 still refreshes).
pub(crate) fn jwt_expired(token: &str, now_unix: i64) -> bool {
    jwt_exp_unix(token).is_some_and(|exp| exp <= now_unix.saturating_add(60))
}

pub(crate) fn jwt_exp_unix(token: &str) -> Option<i64> {
    let jwt = token
        .split_once("::")
        .map(|(_, rest)| rest)
        .unwrap_or(token);
    let payload = jwt.split('.').nth(1)?;
    let bytes = b64url(payload)?;
    let v: Value = serde_json::from_slice(&bytes).ok()?;
    v.get("exp").and_then(|n| {
        n.as_i64()
            .or_else(|| n.as_u64().and_then(|u| i64::try_from(u).ok()))
            .or_else(|| n.as_f64().map(|f| f as i64))
    })
}

pub(crate) struct RefreshedCursor {
    access: String,
    refresh: Option<String>,
}

pub(crate) fn parse_cursor_refresh(body: &str) -> Result<RefreshedCursor, String> {
    let v: Value =
        serde_json::from_str(body).map_err(|_| "cursor refresh was not json".to_string())?;
    if v.get("shouldLogout").and_then(|b| b.as_bool()) == Some(true) {
        return Err("cursor refresh signed the session out".into());
    }
    let access = v
        .get("access_token")
        .and_then(|t| t.as_str())
        .filter(|t| !t.is_empty())
        .ok_or_else(|| "cursor refresh had no access_token".to_string())?;
    if !cursor_session_token_usable(access) {
        return Err("cursor refresh access_token was not a session JWT".into());
    }
    let refresh = v
        .get("refresh_token")
        .and_then(|t| t.as_str())
        .filter(|t| !t.is_empty())
        .map(str::to_string);
    Ok(RefreshedCursor {
        access: access.to_string(),
        refresh,
    })
}

fn cursor_refresh_body(refresh: &str) -> String {
    serde_json::json!({
        "grant_type": "refresh_token",
        "client_id": "cli",
        "refresh_token": refresh,
    })
    .to_string()
}

fn refresh_cursor_access() -> Result<String, String> {
    let refresh = keychain_item("cursor-refresh-token")?;
    let dpop = if refresh.starts_with("session_rt_") {
        Some(cursor_dpop_proof()?)
    } else {
        None
    };
    let mut headers = vec![
        ("content-type", "application/json"),
        ("accept", "application/json"),
    ];
    if let Some(proof) = dpop.as_deref() {
        headers.push(("DPoP", proof));
    }
    let body = cursor_refresh_body(&refresh);
    let text = http_post_json(CURSOR_REFRESH_URL, &headers, &body)?;
    let parsed = parse_cursor_refresh(&text).map_err(|e| scrub(&e))?;
    cursor_access_mem_put(parsed.access.clone());
    if let Err(e) = keychain_set("cursor-access-token", &parsed.access) {
        tracing::warn!(error = %e, "cursor access token kept in memory; keychain write failed");
    }
    if let Some(next) = parsed.refresh.as_deref() {
        if next != refresh {
            if let Err(e) = keychain_set("cursor-refresh-token", next) {
                tracing::warn!(error = %e, "cursor refresh token was not written back");
            }
        }
    }
    Ok(parsed.access)
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
#[cfg(not(target_os = "macos"))]
fn keychain_cursor_access_token() -> Result<String, String> {
    Err(
        "Cursor access token in state.vscdb is not a session JWT and there is no macOS keychain item cursor-access-token to read"
            .into(),
    )
}

#[cfg(target_os = "macos")]
fn keychain_cursor_access_token() -> Result<String, String> {
    keychain_item("cursor-access-token").map_err(|e| {
        if e.contains("not readable") {
            format!("{e}. state.vscdb cursorAuth/accessToken is not a session JWT")
        } else {
            e
        }
    })
}

/// Login-keychain generic password. Stdout is the secret and is never logged.
/// Exit 36 is the locked / no-UI session (`errSecInteractionNotAllowed`).
/// Exit 44 is item not found.
#[cfg(target_os = "macos")]
fn keychain_item(service: &str) -> Result<String, String> {
    // Unit tests must not read or write the developer login keychain. A
    // signed-in Mac would otherwise return live Cursor usage or http 401.
    if cfg!(test) {
        return Err(format!(
            "macOS keychain item {service} (account cursor-user) is not readable from this session (security exit 36)"
        ));
    }
    let out = std::process::Command::new("/usr/bin/security")
        .args([
            "find-generic-password",
            "-s",
            service,
            "-a",
            "cursor-user",
            "-w",
        ])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("could not run security for {service}: {e}"))?;
    if !out.status.success() {
        let code = out.status.code().unwrap_or(-1);
        return Err(format!(
            "macOS keychain item {service} (account cursor-user) is not readable from this session (security exit {code})"
        ));
    }
    let token =
        String::from_utf8(out.stdout).map_err(|_| format!("keychain {service} was not utf-8"))?;
    let token = token.trim().to_string();
    if token.is_empty() {
        return Err(format!(
            "macOS keychain item {service} (account cursor-user) was empty"
        ));
    }
    Ok(token)
}

#[cfg(not(target_os = "macos"))]
fn keychain_item(_service: &str) -> Result<String, String> {
    Err("cursor keychain items are only read on macOS".into())
}

/// `security -i` so the secret is not a process argument. Same update the
/// Cursor CLI uses (`add-generic-password -U`).
fn keychain_set(service: &str, value: &str) -> Result<(), String> {
    if !cfg!(target_os = "macos") {
        return Err("no macOS keychain".into());
    }
    if cfg!(test) {
        return Err(format!(
            "macOS keychain item {service} (account cursor-user) is not readable from this session (security exit 36)"
        ));
    }
    if !keychain_safe(value) || !keychain_safe(service) {
        return Err("refusing to write a keychain value with an unsafe character".into());
    }
    let line = format!("add-generic-password -a cursor-user -s {service} -w {value} -U\n");
    let mut child = std::process::Command::new("/usr/bin/security")
        .arg("-i")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| format!("could not run security: {e}"))?;
    use std::io::Write;
    child
        .stdin
        .as_mut()
        .ok_or_else(|| "security stdin missing".to_string())?
        .write_all(line.as_bytes())
        .map_err(|e| format!("security stdin: {e}"))?;
    drop(child.stdin.take());
    let status = child.wait().map_err(|e| format!("security wait: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!(
            "security add-generic-password exited {}",
            status.code().unwrap_or(-1)
        ))
    }
}

fn keychain_safe(s: &str) -> bool {
    !s.is_empty()
        && s.chars().all(|c| {
            c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | ':' | '+' | '/' | '=')
        })
}

/// DPoP proof for `session_rt_` refresh tokens. The private key is the
/// `cursor-dpop-key` login-keychain item (WebCrypto P-256 JWK). Hardware
/// keys are not signed here.
fn cursor_dpop_proof() -> Result<String, String> {
    let raw = keychain_item("cursor-dpop-key")?;
    dpop_proof_es256(
        &raw,
        CURSOR_REFRESH_URL,
        "POST",
        Timestamp::now().as_second(),
    )
}

pub(crate) fn dpop_proof_es256(
    jwk_json: &str,
    htu: &str,
    htm: &str,
    iat: i64,
) -> Result<String, String> {
    let v: Value =
        serde_json::from_str(jwk_json).map_err(|_| "cursor dpop key was not json".to_string())?;
    if v.get("kind").and_then(|k| k.as_str()) == Some("hardware-p256") {
        return Err("cursor refresh needs a hardware DPoP key this process cannot sign".into());
    }
    let d = v
        .get("privateKeyD")
        .and_then(|s| s.as_str())
        .or_else(|| v.pointer("/d").and_then(|s| s.as_str()))
        .ok_or_else(|| "cursor dpop key has no private scalar".to_string())?;
    let d_bytes =
        b64url(d).ok_or_else(|| "cursor dpop private scalar was not base64url".to_string())?;
    if d_bytes.len() != 32 {
        return Err("cursor dpop private scalar was not 32 bytes".into());
    }
    let jwk = v.get("publicJwk").cloned().unwrap_or(v.clone());
    let x = jwk
        .get("x")
        .and_then(|s| s.as_str())
        .ok_or_else(|| "cursor dpop key has no public x".to_string())?;
    let y = jwk
        .get("y")
        .and_then(|s| s.as_str())
        .ok_or_else(|| "cursor dpop key has no public y".to_string())?;
    let x_bytes = b64url(x).ok_or_else(|| "cursor dpop public x was not base64url".to_string())?;
    let y_bytes = b64url(y).ok_or_else(|| "cursor dpop public y was not base64url".to_string())?;
    if x_bytes.len() != 32 || y_bytes.len() != 32 {
        return Err("cursor dpop public point was not 32-byte coordinates".into());
    }
    let mut public = Vec::with_capacity(65);
    public.push(0x04);
    public.extend_from_slice(&x_bytes);
    public.extend_from_slice(&y_bytes);
    let rng = ring::rand::SystemRandom::new();
    let signing = ring::signature::EcdsaKeyPair::from_private_key_and_public_key(
        &ring::signature::ECDSA_P256_SHA256_FIXED_SIGNING,
        &d_bytes,
        &public,
        &rng,
    )
    .map_err(|_| "cursor dpop key was not a P-256 key pair".to_string())?;
    let header = serde_json::json!({
        "typ": "dpop+jwt",
        "alg": "ES256",
        "jwk": {"kty": "EC", "crv": "P-256", "x": x, "y": y}
    });
    let payload = serde_json::json!({
        "jti": b64url_encode(&iat.to_le_bytes()),
        "htm": htm.to_ascii_uppercase(),
        "htu": htu,
        "iat": iat
    });
    let signing_input = format!(
        "{}.{}",
        b64url_encode(header.to_string().as_bytes()),
        b64url_encode(payload.to_string().as_bytes())
    );
    let sig = signing
        .sign(&rng, signing_input.as_bytes())
        .map_err(|_| "cursor dpop sign failed".to_string())?;
    Ok(format!("{signing_input}.{}", b64url_encode(sig.as_ref())))
}

fn b64url_encode(data: &[u8]) -> String {
    const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        let n = (u32::from(data[i]) << 16) | (u32::from(data[i + 1]) << 8) | u32::from(data[i + 2]);
        out.push(T[((n >> 18) & 63) as usize] as char);
        out.push(T[((n >> 12) & 63) as usize] as char);
        out.push(T[((n >> 6) & 63) as usize] as char);
        out.push(T[(n & 63) as usize] as char);
        i += 3;
    }
    if i < data.len() {
        let mut n = u32::from(data[i]) << 16;
        out.push(T[((n >> 18) & 63) as usize] as char);
        if i + 1 < data.len() {
            n |= u32::from(data[i + 1]) << 8;
            out.push(T[((n >> 12) & 63) as usize] as char);
            out.push(T[((n >> 6) & 63) as usize] as char);
        } else {
            out.push(T[((n >> 12) & 63) as usize] as char);
        }
    }
    out
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
            return Err(format!("unknown: {CODEX_API_KEY_REASON}"));
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

/// CLI of the first configured `kind = "agy"` provider. `None` when none is
/// configured, so refresh does not spawn `agy` from `PATH`.
fn agy_probe_cli(providers: &[Provider]) -> Option<PathBuf> {
    providers
        .iter()
        .find(|p| p.kind == "agy")
        .map(|p| p.cli.clone())
}

/// `agy -p /usage --output-format text`. No other flags and no prompt.
const AGY_USAGE_ARGS: &[&str] = &["-p", "/usage", "--output-format", "text"];
const AGY_USAGE_TIMEOUT: Duration = Duration::from_secs(20);

/// One `agy -p /usage` run. `Err` always starts with `unknown:` so
/// [`map_kind`] keeps `status: unknown` and never starts a login.
fn run_agy_usage(cli: &Path) -> std::result::Result<Vec<Metric>, String> {
    let mut child = std::process::Command::new(cli)
        .args(AGY_USAGE_ARGS)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| {
            format!(
                "unknown: agy /usage failed to start ({})",
                scrub(&e.to_string())
            )
        })?;
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() >= AGY_USAGE_TIMEOUT => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("unknown: agy /usage timed out".into());
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(e) => {
                let _ = child.kill();
                return Err(format!(
                    "unknown: agy /usage failed ({})",
                    scrub(&e.to_string())
                ));
            }
        }
    };
    let mut stdout = String::new();
    let mut stderr = String::new();
    if let Some(mut out) = child.stdout.take() {
        let _ = std::io::Read::read_to_string(&mut out, &mut stdout);
    }
    if let Some(mut err) = child.stderr.take() {
        let _ = std::io::Read::read_to_string(&mut err, &mut stderr);
    }
    interpret_agy_usage(status.success(), &stdout, &stderr)
}

fn interpret_agy_usage(
    success: bool,
    stdout: &str,
    stderr: &str,
) -> std::result::Result<Vec<Metric>, String> {
    let blob = format!("{stdout}\n{stderr}").to_ascii_lowercase();
    // Sign-in is only this phrase (any case) or a non-zero exit below.
    // A bare "authentication" substring is not a sign-in failure.
    if blob.contains("not logged into antigravity") {
        return Err("unknown: agy not signed in in this session".into());
    }
    if !success {
        return Err("unknown: agy /usage failed".into());
    }
    parse_agy_usage(stdout).map_err(|_| "unknown: unrecognized agy /usage output".to_string())
}

fn parse_agy_usage(text: &str) -> std::result::Result<Vec<Metric>, String> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.eq_ignore_ascii_case("quota:") {
            continue;
        }
        if let Some(metric) = parse_agy_usage_line(line) {
            out.push(metric);
        }
    }
    if out.is_empty() {
        return Err("unrecognized agy /usage output".into());
    }
    Ok(out)
}

fn parse_agy_usage_line(line: &str) -> Option<Metric> {
    if !line.contains("Limit Remaining") {
        return None;
    }
    let group = if line.starts_with("Gemini") {
        "gemini"
    } else if line.to_ascii_lowercase().contains("claude") {
        "claude_gpt"
    } else {
        return None;
    };
    let window = if line.contains("Five Hour") {
        "5h"
    } else if line.contains("Weekly") {
        "7d"
    } else {
        return None;
    };
    let remaining = line
        .split_whitespace()
        .find(|t| t.ends_with('%') && t.len() > 1)
        .and_then(|t| t.trim_end_matches('%').parse::<f64>().ok())
        .filter(|n| n.is_finite())?;
    let used = round1((100.0 - remaining).clamp(0.0, 100.0));
    Some(Metric {
        metric: format!("{group}_{window}"),
        used: Some(used),
        limit: Some(100.0),
        pct: Some(used),
        resets_at: agy_reset_rfc3339(line)?,
    })
}

/// `None` when the stamp is missing (line is not a meter). `Some(None)` when
/// the window has not started (reset at or before 1970-01-02).
fn agy_reset_rfc3339(line: &str) -> Option<Option<String>> {
    let stamp = agy_stamp(line)?;
    let t = rfc3339::parse(&stamp)?;
    let cutoff = rfc3339::parse("1970-01-03T00:00:00Z")?;
    if t < cutoff {
        Some(None)
    } else {
        Some(Some(rfc3339::millis(t)))
    }
}

/// `2026-12-03 18:27 +04` → `2026-12-03T18:27:00+04:00`.
fn agy_stamp(line: &str) -> Option<String> {
    let b = line.as_bytes();
    let mut i = 0;
    while i + 16 <= b.len() {
        if b[i].is_ascii_digit()
            && b[i + 4] == b'-'
            && b[i + 7] == b'-'
            && b[i + 10] == b' '
            && b[i + 13] == b':'
        {
            let date = &line[i..i + 10];
            let hm = &line[i + 11..i + 16];
            let rest = line[i + 16..].trim();
            let off = rest.split_whitespace().next().unwrap_or("");
            let off = agy_offset(off)?;
            return Some(format!("{date}T{hm}:00{off}"));
        }
        i += 1;
    }
    None
}

fn agy_offset(off: &str) -> Option<String> {
    let mut chars = off.chars();
    let sign = chars.next()?;
    if sign != '+' && sign != '-' {
        return None;
    }
    let rest: String = chars.collect();
    let (hh, mm) = if let Some((h, m)) = rest.split_once(':') {
        (h.to_string(), m.to_string())
    } else if rest.len() == 4 {
        (rest[..2].to_string(), rest[2..].to_string())
    } else if rest.len() == 2 {
        (rest, "00".into())
    } else {
        return None;
    };
    if hh.len() != 2 || mm.len() != 2 {
        return None;
    }
    Some(format!("{sign}{hh}:{mm}"))
}

/// Status line for an agy provider. Groups stay separate (Gemini, Claude/GPT).
/// Each group lists `5h` then `7d` and one reset, the later of that group.
pub fn agy_status_line(rows: &[PlanRow], tz: &crate::tz::DisplayTz) -> Option<String> {
    let ok: Vec<_> = rows.iter().filter(|r| r.status == "ok").collect();
    if ok.is_empty() {
        return None;
    }
    let mut parts = Vec::new();
    for (prefix, label) in [("gemini_", "gemini"), ("claude_gpt_", "claude/gpt")] {
        let five = ok.iter().find(|r| r.metric == format!("{prefix}5h"));
        let week = ok.iter().find(|r| r.metric == format!("{prefix}7d"));
        if five.is_none() && week.is_none() {
            continue;
        }
        let mut group = Vec::new();
        let mut first = true;
        for row in [five, week].into_iter().flatten() {
            let pct = row
                .pct
                .map(|p| format!(" {}%", agy_pct_text(p)))
                .unwrap_or_default();
            let name = if first {
                first = false;
                format!("{label} {}", row.metric.trim_start_matches(prefix))
            } else {
                row.metric.trim_start_matches(prefix).to_string()
            };
            group.push(format!("{name}{pct}"));
        }
        let reset = [five, week]
            .into_iter()
            .flatten()
            .filter_map(|r| r.resets_at.as_deref())
            .filter_map(rfc3339::parse)
            .max();
        if let Some(t) = reset {
            group.push(format!("resets {}", tz.format_compact(t)));
        }
        parts.push(group.join(" · "));
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(" · "))
    }
}

fn agy_pct_text(p: f64) -> String {
    let p = round1(p);
    if (p - p.round()).abs() < 0.05 {
        format!("{p:.0}")
    } else {
        format!("{p:.1}")
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
        let err = http_post_json("https://example.com/oauth/token", &[], "{}").unwrap_err();
        assert!(err.contains("allow-list"), "{err}");
        let src = include_str!("plan_usage.rs");
        let prod = src.split("mod tests").next().unwrap_or(src);
        assert_eq!(
            prod.matches(".post(").count(),
            1,
            "plan_usage may POST only the Cursor refresh URL"
        );
        assert!(prod.contains(CURSOR_REFRESH_URL));
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
    fn codex_api_key_line_has_no_unknown_prefix() {
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join(".codex")).unwrap();
        std::fs::write(
            home.path().join(".codex/auth.json"),
            r#"{"OPENAI_API_KEY":"sk-test-not-a-real-secret"}"#,
        )
        .unwrap();
        let err = read_codex(home.path()).unwrap_err();
        assert_eq!(err, format!("unknown: {CODEX_API_KEY_REASON}"));
        cache_clear();
        cache_put(vec![unknown(
            "codex",
            "plan",
            CODEX_API_KEY_REASON,
            "2026-11-26T00:00:00.000Z",
        )]);
        assert_eq!(line_for("codex"), CODEX_API_KEY_REASON);
        assert!(!line_for("codex").contains("unknown:"));
        assert!(!line_for("codex").contains("wham"));
        cache_clear();
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

    #[test]
    fn jwt_exp_and_refresh_body_do_not_echo_the_token() {
        let header = b64url_encode(br#"{"alg":"none"}"#);
        let payload = b64url_encode(br#"{"exp":100}"#);
        let jwt = format!("{header}.{payload}.sig");
        assert_eq!(jwt_exp_unix(&jwt), Some(100));
        assert!(jwt_expired(&jwt, 50));
        assert!(!jwt_expired(&jwt, 30));
        let body = cursor_refresh_body("refresh-example");
        assert!(body.contains("\"grant_type\":\"refresh_token\""));
        assert!(body.contains("\"client_id\":\"cli\""));
        let parsed = parse_cursor_refresh(
            r#"{"access_token":"aaa.bbb.ccc","refresh_token":"next-refresh","shouldLogout":false}"#,
        )
        .unwrap();
        assert_eq!(parsed.access, "aaa.bbb.ccc");
        assert_eq!(parsed.refresh.as_deref(), Some("next-refresh"));
        assert!(parse_cursor_refresh(r#"{"shouldLogout":true}"#).is_err());
        assert!(!allow_post("https://example.com/oauth/token"));
        assert!(allow_post(CURSOR_REFRESH_URL));
        let err = refresh_http_error(400, r#"{"error":"invalid_grant","refresh_token":"secret"}"#);
        assert_eq!(err, "cursor token refresh http 400 invalid_grant");
        assert!(!err.contains("secret"));
    }

    #[test]
    fn dpop_proof_is_three_segments() {
        // P-256 scalar 0x0707…07 and its uncompressed public point (Node prime256v1).
        let jwk = serde_json::json!({
            "kind": "webcrypto-p256",
            "v": 1,
            "privateKeyD": b64url_encode(&[7u8; 32]),
            "publicJwk": {
                "kty": "EC",
                "crv": "P-256",
                "x": "HhhTL9R1TALzBB2cdc6zO4P_2BrHzk_ogsyxyYvFiW4",
                "y": "pGwxHE4v9A3ZajZT5uRURdMt_khuztdcepDGoYiBwKM"
            }
        });
        let proof =
            dpop_proof_es256(&jwk.to_string(), CURSOR_REFRESH_URL, "POST", 1_700_000_000).unwrap();
        assert_eq!(proof.split('.').count(), 3);
        let header = b64url(proof.split('.').next().unwrap()).unwrap();
        let header: serde_json::Value = serde_json::from_slice(&header).unwrap();
        assert_eq!(header["alg"], "ES256");
        assert_eq!(header["typ"], "dpop+jwt");
        assert!(header["jwk"]["x"].as_str().unwrap().len() > 8);
        assert!(
            dpop_proof_es256(r#"{"kind":"hardware-p256"}"#, CURSOR_REFRESH_URL, "POST", 1).is_err()
        );
    }

    /// Captured from agy 1.5.18 on 2026-11-26.
    const AGY_USAGE_FIXTURE: &str = "\
Quota:
Gemini Models          Weekly Limit Remaining     99%   2026-12-03 18:27 +04
Gemini Models          Five Hour Limit Remaining  99%   2026-11-26 23:27 +04
Claude and GPT models  Weekly Limit Remaining     100%  1970-01-01 04:00 +04
Claude and GPT models  Five Hour Limit Remaining  100%  1970-01-01 04:00 +04
";

    #[test]
    fn parses_agy_usage_fixture_and_drops_unstarted_resets() {
        assert_eq!(AGY_USAGE_ARGS, ["-p", "/usage", "--output-format", "text"]);
        let rows = parse_agy_usage(AGY_USAGE_FIXTURE).unwrap();
        assert_eq!(rows.len(), 4);
        let gemini_week = rows.iter().find(|r| r.metric == "gemini_7d").unwrap();
        assert_eq!(gemini_week.pct, Some(1.0));
        assert_eq!(
            gemini_week.resets_at.as_deref(),
            Some("2026-12-03T14:27:00.000Z")
        );
        let gemini_five = rows.iter().find(|r| r.metric == "gemini_5h").unwrap();
        assert_eq!(gemini_five.pct, Some(1.0));
        assert_eq!(
            gemini_five.resets_at.as_deref(),
            Some("2026-11-26T19:27:00.000Z")
        );
        for metric in ["claude_gpt_5h", "claude_gpt_7d"] {
            let row = rows.iter().find(|r| r.metric == metric).unwrap();
            assert_eq!(row.pct, Some(0.0), "{metric}");
            assert_eq!(row.resets_at, None, "{metric} 1970 reset is not a date");
        }
        let tz = crate::tz::DisplayTz::parse("UTC").unwrap();
        let plan: Vec<PlanRow> = rows
            .iter()
            .map(|m| PlanRow {
                provider: "agy".into(),
                metric: m.metric.clone(),
                used: m.used,
                limit: m.limit,
                pct: m.pct,
                resets_at: m.resets_at.clone(),
                source: "agy -p /usage".into(),
                read_at: "t".into(),
                error: None,
                status: "ok".into(),
                reason: None,
            })
            .collect();
        assert_eq!(
            agy_status_line(&plan, &tz).as_deref(),
            Some("gemini 5h 1% · 7d 1% · resets Dec 03 · claude/gpt 5h 0% · 7d 0%")
        );
        assert!(parse_agy_usage("hello\n").is_err());
    }

    #[test]
    fn agy_auth_failure_stays_unknown_and_does_not_parse() {
        let err = interpret_agy_usage(false, "", "not logged into Antigravity\n").unwrap_err();
        assert_eq!(err, "unknown: agy not signed in in this session");
        let err = interpret_agy_usage(true, "authentication required", "").unwrap_err();
        assert_eq!(err, "unknown: unrecognized agy /usage output");
        let with_word = format!("{AGY_USAGE_FIXTURE}\nauthentication note\n");
        assert!(interpret_agy_usage(true, &with_word, "").is_ok());
        let err = interpret_agy_usage(true, "", "NOT LOGGED INTO ANTIGRAVITY\n").unwrap_err();
        assert_eq!(err, "unknown: agy not signed in in this session");
        let err = interpret_agy_usage(false, "", "authentication failed\n").unwrap_err();
        assert_eq!(err, "unknown: agy /usage failed");
        let err = interpret_agy_usage(false, AGY_USAGE_FIXTURE, "").unwrap_err();
        assert_eq!(err, "unknown: agy /usage failed");
        let err = interpret_agy_usage(true, "Quota:\nnothing\n", "").unwrap_err();
        assert_eq!(err, "unknown: unrecognized agy /usage output");
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("agy");
        std::fs::write(
            &bin,
            "#!/bin/sh\necho 'not logged into Antigravity' >&2\nexit 1\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perm = std::fs::metadata(&bin).unwrap().permissions();
            perm.set_mode(0o755);
            std::fs::set_permissions(&bin, perm).unwrap();
        }
        let err = run_agy_usage(&bin).unwrap_err();
        assert_eq!(err, "unknown: agy not signed in in this session");
    }

    #[test]
    fn agy_usage_probe_only_when_kind_agy_is_configured() {
        assert!(agy_probe_cli(&[]).is_none());
        let claude = crate::config::Provider::test_stub("claude");
        assert!(agy_probe_cli(&[claude]).is_none());
        assert!(
            !targets_from(&[]).iter().any(|t| t.kind == "agy"),
            "no synthetic PATH probe named agy"
        );
        let mut agy = crate::config::Provider::test_stub("antigravity");
        agy.kind = "agy".into();
        agy.cli = PathBuf::from("/opt/atr-test/agy-not-on-path");
        assert_eq!(
            agy_probe_cli(&[agy]).as_deref(),
            Some(std::path::Path::new("/opt/atr-test/agy-not-on-path"))
        );
    }
}
