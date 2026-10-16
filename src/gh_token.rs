//! One GitHub token for every atr GitHub path (issue poller, shadow, doctor).
//!
//! Resolution order (first non-empty wins), process env then `shadow.env`:
//! `ATR_GH_TOKEN`, `[shadow] gh_token_env` (default `ATR_SHADOW_GH_TOKEN`),
//! `GH_TOKEN`, `GITHUB_TOKEN`. The value is handed to `gh` as `GH_TOKEN` so
//! atr never depends on a (keychain-backed) `gh auth login`.
//!
//! Also tracks auth health: 401/403 failures raise a deduped Telegram alert
//! (once per 6h until recovery, then a "recovered" notice) and the
//! `Github-Authentication-Token-Expiration` header drives a daily warning
//! when the token expires within 14 days. Token values are never logged.

use crate::config::Config;
use chrono::{DateTime, NaiveDateTime, Utc};
use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;

pub const PRIMARY_KEY: &str = "ATR_GH_TOKEN";
pub const ALERT_EVERY: Duration = Duration::from_secs(6 * 3600);
pub const EXPIRY_WARN_EVERY: Duration = Duration::from_secs(24 * 3600);
pub const EXPIRY_WARN_DAYS: i64 = 14;
pub const PROBE_EVERY: Duration = Duration::from_secs(3600);

pub fn shadow_key(cfg: &Config) -> &str {
    if cfg.shadow.gh_token_env.trim().is_empty() {
        "ATR_SHADOW_GH_TOKEN"
    } else {
        cfg.shadow.gh_token_env.as_str()
    }
}

/// Env names checked, in priority order (deduped).
pub fn token_keys(cfg: &Config) -> Vec<String> {
    keys_for(shadow_key(cfg))
}

pub fn keys_for(shadow: &str) -> Vec<String> {
    let mut keys: Vec<String> = Vec::new();
    for k in [PRIMARY_KEY, shadow, "GH_TOKEN", "GITHUB_TOKEN"] {
        if !keys.iter().any(|x| x == k) {
            keys.push(k.to_string());
        }
    }
    keys
}

/// `(env name, value)` of the default token (single-token chain). Never log the value.
pub fn resolve(cfg: &Config, config_path: Option<&Path>) -> Option<(String, String)> {
    let def = default_cfg_path(config_path);
    resolve_with(&token_keys(cfg), Some(&def), |k| std::env::var(k).ok())
}

fn default_cfg_path(config_path: Option<&Path>) -> std::path::PathBuf {
    config_path
        .map(Path::to_path_buf)
        .unwrap_or_else(crate::config::default_config_path)
}

/// Owner part of `owner/repo` (empty when not a slug).
pub fn repo_owner(repo: &str) -> &str {
    repo.split('/').next().unwrap_or("").trim()
}

/// Env var NAME mapped for `owner` in `[github.tokens]` (case-insensitive).
pub fn owner_key<'a>(
    tokens: &'a std::collections::BTreeMap<String, String>,
    owner: &str,
) -> Option<&'a str> {
    if owner.is_empty() {
        return None;
    }
    tokens
        .iter()
        .find(|(o, k)| o.eq_ignore_ascii_case(owner) && !k.trim().is_empty())
        .map(|(_, k)| k.trim())
}

/// Keys to try for `repo`: the owner's mapped env name first, then the
/// single-token chain.
pub fn keys_for_repo(cfg: &Config, repo: Option<&str>) -> Vec<String> {
    let mut keys = Vec::new();
    if let Some(k) = repo.and_then(|r| owner_key(&cfg.github.tokens, repo_owner(r))) {
        keys.push(k.to_string());
    }
    for k in token_keys(cfg) {
        if !keys.contains(&k) {
            keys.push(k);
        }
    }
    keys
}

/// Token for `repo` (per-owner `[github.tokens]`, else the default chain).
pub fn resolve_for(
    cfg: &Config,
    config_path: Option<&Path>,
    repo: Option<&str>,
) -> Option<(String, String)> {
    resolve_with(&keys_for_repo(cfg, repo), config_path, |k| {
        std::env::var(k).ok()
    })
}

pub fn resolve_with(
    keys: &[String],
    config_path: Option<&Path>,
    env: impl Fn(&str) -> Option<String>,
) -> Option<(String, String)> {
    for k in keys {
        if let Some(v) = env(k)
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
        {
            return Some((k.clone(), v));
        }
    }
    let path = crate::telegram_setup::shadow_env_path(config_path?);
    for k in keys {
        if let Some(v) = crate::telegram_setup::read_shadow_env_var(&path, k) {
            return Some((k.clone(), v));
        }
    }
    None
}

fn scrub_and_set(set: &mut dyn FnMut(&str, Option<&str>), token: Option<&str>) {
    set("GH_TOKEN", None);
    set("GITHUB_TOKEN", None);
    set("GH_PROMPT_DISABLED", Some("1"));
    if let Some(t) = token {
        set("GH_TOKEN", Some(t));
        set("GITHUB_TOKEN", Some(t));
    }
}

pub fn apply(
    cmd: &mut tokio::process::Command,
    cfg: &Config,
    config_path: Option<&Path>,
    repo: Option<&str>,
) {
    let tok = resolve_for(cfg, config_path, repo).map(|(_, v)| v);
    scrub_and_set(
        &mut |k, v| match v {
            Some(v) => {
                cmd.env(k, v);
            }
            None => {
                cmd.env_remove(k);
            }
        },
        tok.as_deref(),
    );
}

pub fn apply_std(
    cmd: &mut std::process::Command,
    cfg: &Config,
    config_path: Option<&Path>,
    repo: Option<&str>,
) {
    let tok = resolve_for(cfg, config_path, repo).map(|(_, v)| v);
    scrub_and_set(
        &mut |k, v| match v {
            Some(v) => {
                cmd.env(k, v);
            }
            None => {
                cmd.env_remove(k);
            }
        },
        tok.as_deref(),
    );
}

/// True when `gh` stderr / an API body looks like an auth failure (401, or
/// 403 that is not a rate limit).
pub fn is_auth_failure(text: &str) -> bool {
    let t = text.to_ascii_lowercase();
    if t.contains("rate limit") {
        return false;
    }
    t.contains("http 401")
        || t.contains("bad credentials")
        || t.contains("requires authentication")
        || t.contains("gh auth login")
        || t.contains("authentication token")
        || (t.contains("http 403")
            && (t.contains("resource not accessible")
                || t.contains("forbids access")
                || t.contains("permission")))
}

/// Parse `Github-Authentication-Token-Expiration` (e.g. `2027-01-12 07:28:00 UTC`).
pub fn parse_expiration(v: &str) -> Option<DateTime<Utc>> {
    let v = v.trim();
    for fmt in [
        "%Y-%m-%d %H:%M:%S UTC",
        "%Y-%m-%d %H:%M:%S %z",
        "%Y-%m-%d %H:%M:%S %Z",
    ] {
        if let Ok(dt) = DateTime::parse_from_str(v, fmt) {
            return Some(dt.with_timezone(&Utc));
        }
        if let Ok(n) = NaiveDateTime::parse_from_str(v, fmt) {
            return Some(n.and_utc());
        }
    }
    DateTime::parse_from_rfc3339(v)
        .ok()
        .map(|d| d.with_timezone(&Utc))
}

pub fn days_left(exp: DateTime<Utc>, now: DateTime<Utc>) -> i64 {
    (exp - now).num_hours().div_euclid(24)
}

#[derive(Debug, Default, Clone)]
pub struct AuthState {
    pub failing_since: Option<DateTime<Utc>>,
    pub last_alert: Option<DateTime<Utc>>,
    pub last_expiry_warn: Option<DateTime<Utc>>,
    pub expires_at: Option<DateTime<Utc>>,
    pub last_probe: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Alert {
    Failing(String),
    Recovered,
    Expiring(i64),
}

impl AuthState {
    /// Record one GitHub call outcome. `Some(true)` = auth ok, `Some(false)` =
    /// auth failure; returns the alert to send, if any.
    pub fn record(&mut self, auth_ok: bool, what: &str, now: DateTime<Utc>) -> Option<Alert> {
        if auth_ok {
            if self.failing_since.take().is_some() {
                self.last_alert = None;
                return Some(Alert::Recovered);
            }
            return None;
        }
        if self.failing_since.is_none() {
            self.failing_since = Some(now);
        }
        let due = match self.last_alert {
            None => true,
            Some(t) => (now - t)
                .to_std()
                .map(|d| d >= ALERT_EVERY)
                .unwrap_or(false),
        };
        if due {
            self.last_alert = Some(now);
            Some(Alert::Failing(what.to_string()))
        } else {
            None
        }
    }

    pub fn note_expiry(&mut self, exp: Option<DateTime<Utc>>, now: DateTime<Utc>) -> Option<Alert> {
        self.expires_at = exp;
        let exp = exp?;
        let days = days_left(exp, now);
        if days >= EXPIRY_WARN_DAYS {
            return None;
        }
        let due = match self.last_expiry_warn {
            None => true,
            Some(t) => (now - t)
                .to_std()
                .map(|d| d >= EXPIRY_WARN_EVERY)
                .unwrap_or(false),
        };
        if due {
            self.last_expiry_warn = Some(now);
            Some(Alert::Expiring(days))
        } else {
            None
        }
    }
}

static STATE: Mutex<Option<std::collections::BTreeMap<String, AuthState>>> = Mutex::new(None);

/// Per-token state, keyed by the token's env var NAME.
fn with_state<T>(key: &str, f: impl FnOnce(&mut AuthState) -> T) -> T {
    let mut g = STATE.lock().unwrap_or_else(|e| e.into_inner());
    f(g.get_or_insert_with(Default::default)
        .entry(key.to_string())
        .or_default())
}

/// Human label for a token: `owner (ENV)` or just `ENV`.
pub fn token_label(key: &str, owner: Option<&str>) -> String {
    match owner.filter(|o| !o.is_empty()) {
        Some(o) => format!("{key} (owner {o})"),
        None => key.to_string(),
    }
}

pub fn alert_text(a: &Alert, key: Option<&str>) -> String {
    alert_text_for(a, key.unwrap_or(PRIMARY_KEY), None)
}

pub fn alert_text_for(a: &Alert, key: &str, owner: Option<&str>) -> String {
    let label = token_label(key, owner);
    match a {
        Alert::Failing(what) => format!(
            "GitHub auth failing ({what}). Token {label} rejected (401/403); GitHub intake for it is stalled. Rotate the fine-grained PAT in ~/.config/atr/shadow.env and restart atr. Next reminder in 6h."
        ),
        Alert::Recovered => format!("GitHub auth recovered for {label}; intake is running again."),
        Alert::Expiring(d) => format!(
            "GitHub token {label} expires in {d} day(s). Create a new fine-grained PAT and update ~/.config/atr/shadow.env."
        ),
    }
}

impl Alert {
    pub fn kind(&self) -> &'static str {
        match self {
            Alert::Failing(_) => "github_auth_failed",
            Alert::Recovered => "github_auth_recovered",
            Alert::Expiring(_) => "github_token_expiring",
        }
    }
}

fn is_token_var(name: &str) -> bool {
    let n = name.to_ascii_uppercase();
    n.contains("GH_TOKEN") || n.contains("GITHUB_TOKEN")
}

/// Run `[notify] command` via `sh -c` with ATR_ALERT_* env, killed after
/// `timeout`. GitHub token env vars are scrubbed from the hook's environment.
pub fn run_hook(
    hook: &crate::config::NotifyConfig,
    kind: &str,
    message: &str,
    extra: &[(&str, String)],
) -> Option<i32> {
    if hook.command.is_empty() {
        return None;
    }
    let mut cmd = std::process::Command::new("sh");
    cmd.arg("-c").arg(&hook.command);
    for (k, _) in std::env::vars_os() {
        if let Some(k) = k.to_str() {
            if is_token_var(k) {
                cmd.env_remove(k);
            }
        }
    }
    for k in keys_for("ATR_SHADOW_GH_TOKEN") {
        cmd.env_remove(k);
    }
    cmd.env("ATR_ALERT_KIND", kind)
        .env("ATR_ALERT_MESSAGE", message);
    for (k, v) in extra {
        cmd.env(k, v);
    }
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("notify hook spawn failed: {e}");
            return None;
        }
    };
    let deadline = std::time::Instant::now() + hook.timeout.max(Duration::from_millis(100));
    loop {
        match child.try_wait() {
            Ok(Some(st)) => {
                if !st.success() {
                    tracing::warn!(kind, "notify hook exited {st}");
                }
                return st.code();
            }
            Ok(None) if std::time::Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                tracing::warn!(kind, "notify hook timed out; killed");
                return None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(_) => return None,
        }
    }
}

fn send(cfg: &Config, a: &Alert, key: &str, owner: Option<&str>) {
    let text = alert_text_for(a, key, owner);
    if !cfg.notify.command.is_empty() {
        let hook = cfg.notify.clone();
        let kind = a.kind();
        let msg = text.clone();
        let mut extra = vec![
            ("ATR_ALERT_SOURCE", "github".to_string()),
            ("ATR_ALERT_TOKEN_ENV", key.to_string()),
            ("ATR_ALERT_OWNER", owner.unwrap_or("").to_string()),
        ];
        if let Alert::Expiring(d) = a {
            extra.push(("ATR_ALERT_DAYS_LEFT", d.to_string()));
        }
        std::thread::spawn(move || {
            let extra: Vec<(&str, String)> = extra.iter().map(|(k, v)| (*k, v.clone())).collect();
            run_hook(&hook, kind, &msg, &extra);
        });
    }
    match a {
        Alert::Failing(_) => tracing::error!("{text}"),
        Alert::Expiring(_) => tracing::warn!("{text}"),
        Alert::Recovered => tracing::info!("{text}"),
    }
    if cfg.telegram.enabled || crate::notify::configured(&cfg.telegram) {
        let button = !matches!(a, Alert::Recovered);
        if !crate::telegram::send_alert(&cfg.telegram, &text, button) {
            crate::notify::notify(&cfg.telegram, &text);
        }
    }
}

pub fn state_path(config_path: &Path) -> std::path::PathBuf {
    config_path
        .parent()
        .map(|p| p.join("github-auth.json"))
        .unwrap_or_else(|| std::path::PathBuf::from("github-auth.json"))
}

fn read_state_file(config_path: &Path) -> serde_json::Value {
    std::fs::read_to_string(state_path(config_path))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or(serde_json::Value::Null)
}

/// Update this token's entry in `github-auth.json` (`{"tokens": {ENV: {...}}}`).
fn persist(config_path: Option<&Path>, key: &str, owner: Option<&str>, last_error: Option<&str>) {
    let Some(cp) = config_path else { return };
    let st = with_state(key, |s| s.clone());
    let mut root = read_state_file(cp);
    if root.get("tokens").and_then(|t| t.as_object()).is_none() {
        root = serde_json::json!({"tokens": {}});
    }
    let entry = serde_json::json!({
        "state": if st.failing_since.is_some() { "failing" } else { "ok" },
        "failing_since": st.failing_since.map(|t| t.to_rfc3339()),
        "expires_at": st.expires_at.map(|t| t.to_rfc3339()),
        "token_env": key,
        "owner": owner,
        "last_error": last_error.map(crate::github_auth::redact_secrets),
        "checked_at": Utc::now().to_rfc3339(),
    });
    root["tokens"][key] = entry;
    let path = state_path(cp);
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, root.to_string()).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
}

/// GitHub auth summary for `atr status` / dashboard. Overall `level` is the
/// worst token (fail > warn > ok > unknown); `tokens` lists each one.
pub fn status_json(config_path: &Path, now: DateTime<Utc>) -> serde_json::Value {
    summarize(&read_state_file(config_path), now)
}

fn summarize_one(raw: &serde_json::Value, now: DateTime<Utc>) -> serde_json::Value {
    let s = |k: &str| raw.get(k).and_then(|v| v.as_str()).map(String::from);
    let exp = s("expires_at")
        .and_then(|v| DateTime::parse_from_rfc3339(&v).ok())
        .map(|d| d.with_timezone(&Utc));
    let days = exp.map(|e| days_left(e, now));
    let label = token_label(
        &s("token_env").unwrap_or_else(|| "token".into()),
        s("owner").as_deref(),
    );
    let failing = s("state").as_deref() == Some("failing");
    let (level, message) = if failing {
        (
            "fail",
            format!(
                "{label}: auth failing since {} ({})",
                s("failing_since").unwrap_or_default(),
                s("last_error").unwrap_or_else(|| "401/403".into())
            ),
        )
    } else if let Some(d) = days.filter(|d| *d < EXPIRY_WARN_DAYS) {
        ("warn", format!("{label}: expires in {d} day(s)"))
    } else {
        (
            "ok",
            match days {
                Some(d) => format!("{label}: ok; expires in {d} day(s)"),
                None => format!("{label}: ok"),
            },
        )
    };
    serde_json::json!({
        "level": level,
        "message": message,
        "token_env": s("token_env"),
        "owner": s("owner"),
        "expires_at": s("expires_at"),
        "days_left": days,
        "checked_at": s("checked_at"),
    })
}

fn rank(level: &str) -> u8 {
    match level {
        "fail" => 3,
        "warn" => 2,
        "ok" => 1,
        _ => 0,
    }
}

pub fn summarize(raw: &serde_json::Value, now: DateTime<Utc>) -> serde_json::Value {
    let entries: Vec<serde_json::Value> =
        if let Some(m) = raw.get("tokens").and_then(|t| t.as_object()) {
            m.values().map(|v| summarize_one(v, now)).collect()
        } else if raw.get("state").is_some() {
            vec![summarize_one(raw, now)] // pre-per-owner single-token file
        } else {
            Vec::new()
        };
    if entries.is_empty() {
        return serde_json::json!({"level": "unknown", "message": "no GitHub auth check yet", "tokens": []});
    }
    let worst = entries
        .iter()
        .map(|e| e["level"].as_str().unwrap_or("unknown"))
        .max_by_key(|l| rank(l))
        .unwrap_or("unknown")
        .to_string();
    let message = entries
        .iter()
        .filter(|e| worst == "ok" || e["level"].as_str() == Some(worst.as_str()))
        .filter_map(|e| e["message"].as_str())
        .collect::<Vec<_>>()
        .join("; ");
    let days_left = entries.iter().filter_map(|e| e["days_left"].as_i64()).min();
    serde_json::json!({
        "level": worst,
        "message": message,
        "days_left": days_left,
        "tokens": entries,
    })
}

/// Feed a `gh` invocation result (for `repo`) into that token's auth tracker.
pub fn observe_gh(
    cfg: &Config,
    config_path: Option<&Path>,
    repo: Option<&str>,
    success: bool,
    stderr: &str,
    what: &str,
) {
    let auth_fail = !success && is_auth_failure(stderr);
    if !success && !auth_fail {
        return; // unrelated failure: says nothing about auth
    }
    let Some((key, _)) = resolve_for(cfg, config_path, repo) else {
        if auth_fail {
            tracing::error!(what, "GitHub auth failure and no token configured");
        }
        return;
    };
    let owner = repo
        .map(repo_owner)
        .filter(|o| owner_key(&cfg.github.tokens, o) == Some(key.as_str()));
    if auth_fail {
        tracing::error!(
            what,
            token = %key,
            "GitHub auth failure: {}",
            crate::github_auth::redact_secrets(stderr.trim())
        );
    }
    let alert = with_state(&key, |s| s.record(!auth_fail, what, Utc::now()));
    if alert.is_some() || auth_fail {
        persist(config_path, &key, owner, auth_fail.then_some(what));
    }
    if let Some(a) = alert {
        send(cfg, &a, &key, owner);
    }
}

#[derive(Debug, Clone)]
pub struct Probe {
    pub status: u16,
    pub login: Option<String>,
    pub expires_at: Option<DateTime<Utc>>,
    pub error: Option<String>,
}

fn api_base() -> String {
    std::env::var("ATR_GITHUB_API").unwrap_or_else(|_| "https://api.github.com".into())
}

/// Blocking GET against the GitHub API with `token`. status 0 = transport error.
pub fn get(token: &str, path: &str) -> Probe {
    let url = format!(
        "{}/{}",
        api_base().trim_end_matches('/'),
        path.trim_start_matches('/')
    );
    let resp = crate::util::ureq_agent(Duration::from_secs(15))
        .get(&url)
        .set("Authorization", &format!("Bearer {token}"))
        .set("Accept", "application/vnd.github+json")
        .set("X-GitHub-Api-Version", "2022-11-28")
        .call();
    let resp = match resp {
        Ok(r) => r,
        Err(ureq::Error::Status(_, r)) => r,
        Err(e) => {
            return Probe {
                status: 0,
                login: None,
                expires_at: None,
                error: Some(
                    crate::github_auth::redact_secrets(&e.to_string()).replace(token, "***"),
                ),
            }
        }
    };
    let status = resp.status();
    let expires_at = resp
        .header("github-authentication-token-expiration")
        .and_then(parse_expiration);
    let body: serde_json::Value = resp.into_json().unwrap_or(serde_json::Value::Null);
    Probe {
        status,
        login: body.get("login").and_then(|v| v.as_str()).map(String::from),
        expires_at,
        error: body
            .get("message")
            .and_then(|v| v.as_str())
            .map(String::from),
    }
}

/// Repos atr talks to: enabled watch repos plus the shadow repo.
pub fn used_repos(cfg: &Config) -> Vec<String> {
    let mut repos: Vec<String> = Vec::new();
    if cfg.intake_allows(crate::task::SourceKind::Github) {
        for r in cfg.enabled_watch_repos() {
            if !r.github.is_empty() && !repos.contains(&r.github) {
                repos.push(r.github.clone());
            }
        }
    }
    if cfg.shadow.enabled {
        let r = crate::shadow::shadow_repo(cfg);
        if !r.is_empty() && !repos.contains(&r) {
            repos.push(r);
        }
    }
    repos
}

/// One entry per distinct token in use: env name, owner (if per-owner), the
/// repos it serves, and the value (never log it).
#[derive(Debug, Clone)]
pub struct TokenUse {
    pub key: String,
    pub owner: Option<String>,
    pub repos: Vec<String>,
    pub value: Option<String>,
}

pub fn tokens_in_use(cfg: &Config, config_path: Option<&Path>) -> Vec<TokenUse> {
    let mut out: Vec<TokenUse> = Vec::new();
    let repos = used_repos(cfg);
    let targets: Vec<Option<String>> = if repos.is_empty() {
        vec![None]
    } else {
        repos.into_iter().map(Some).collect()
    };
    for repo in targets {
        let resolved = resolve_for(cfg, config_path, repo.as_deref());
        let mapped = repo.as_deref().and_then(|r| {
            owner_key(&cfg.github.tokens, repo_owner(r))
                .map(|k| (repo_owner(r).to_string(), k.to_string()))
        });
        let (key, value) = match (&resolved, &mapped) {
            (Some((k, v)), _) => (k.clone(), Some(v.clone())),
            (None, Some((_, k))) => (k.clone(), None),
            (None, None) => (PRIMARY_KEY.to_string(), None),
        };
        let owner = mapped.filter(|(_, k)| *k == key).map(|(o, _)| o);
        if let Some(e) = out.iter_mut().find(|e| e.key == key) {
            if let Some(r) = repo {
                e.repos.push(r);
            }
            continue;
        }
        out.push(TokenUse {
            key,
            owner,
            repos: repo.into_iter().collect(),
            value,
        });
    }
    out
}

/// Periodic health probe (GET /user per token in use), at most hourly each.
pub async fn maybe_probe(cfg: &Config, config_path: &Path) {
    for tu in tokens_in_use(cfg, Some(config_path)) {
        let Some(tok) = tu.value.clone() else {
            tracing::debug!(token = %tu.key, "GitHub token not set; skip auth probe");
            continue;
        };
        let now = Utc::now();
        let due = with_state(&tu.key, |s| match s.last_probe {
            None => true,
            Some(t) => (now - t)
                .to_std()
                .map(|d| d >= PROBE_EVERY)
                .unwrap_or(false),
        });
        if !due {
            continue;
        }
        with_state(&tu.key, |s| s.last_probe = Some(now));
        let p = match tokio::task::spawn_blocking(move || get(&tok, "user")).await {
            Ok(p) => p,
            Err(_) => continue,
        };
        let key = tu.key.as_str();
        let mut alerts = Vec::new();
        match p.status {
            0 => continue,
            401 | 403 => {
                tracing::error!(status = p.status, token = %key, "GitHub token probe rejected");
                if let Some(a) = with_state(key, |s| {
                    s.record(false, &format!("GET /user -> {}", p.status), now)
                }) {
                    alerts.push(a);
                }
            }
            200..=299 => {
                if let Some(a) = with_state(key, |s| s.record(true, "", now)) {
                    alerts.push(a);
                }
                if let Some(a) = with_state(key, |s| s.note_expiry(p.expires_at, now)) {
                    alerts.push(a);
                }
            }
            _ => {}
        }
        let err =
            (p.status == 401 || p.status == 403).then(|| format!("GET /user -> {}", p.status));
        persist(Some(config_path), key, tu.owner.as_deref(), err.as_deref());
        for a in alerts {
            send(cfg, &a, key, tu.owner.as_deref());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn t(h: i64) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 1, 0, 0, 0).unwrap() + chrono::Duration::hours(h)
    }

    #[test]
    fn resolve_prefers_primary_then_shadow_then_gh_token() {
        let cfg = keys_for("ATR_SHADOW_GH_TOKEN");
        let env = |k: &str| match k {
            "ATR_SHADOW_GH_TOKEN" => Some("shadow".into()),
            "GH_TOKEN" => Some("gh".into()),
            _ => None,
        };
        assert_eq!(
            resolve_with(&cfg, None, env).unwrap().0,
            "ATR_SHADOW_GH_TOKEN"
        );
        let env2 = |k: &str| match k {
            "ATR_GH_TOKEN" => Some("p".into()),
            "GH_TOKEN" => Some("gh".into()),
            _ => None,
        };
        assert_eq!(resolve_with(&cfg, None, env2).unwrap().0, "ATR_GH_TOKEN");
        let env3 = |k: &str| (k == "GITHUB_TOKEN").then(|| "x".to_string());
        assert_eq!(resolve_with(&cfg, None, env3).unwrap().0, "GITHUB_TOKEN");
        assert!(resolve_with(&cfg, None, |_| Some("  ".into())).is_none());
    }

    #[test]
    fn resolve_reads_shadow_env_file() {
        let dir = tempfile::tempdir().unwrap();
        let cfgp = dir.path().join("config.toml");
        std::fs::write(dir.path().join("shadow.env"), "ATR_GH_TOKEN=filetok\n").unwrap();
        let cfg = keys_for("ATR_SHADOW_GH_TOKEN");
        let (k, v) = resolve_with(&cfg, Some(&cfgp), |_| None).unwrap();
        assert_eq!(k, "ATR_GH_TOKEN");
        assert_eq!(v, "filetok");
    }

    #[test]
    fn classifies_auth_failures() {
        assert!(is_auth_failure(
            "HTTP 401: Bad credentials (https://api.github.com/graphql)"
        ));
        assert!(is_auth_failure(
            "To get started with GitHub CLI, please run:  gh auth login"
        ));
        assert!(is_auth_failure(
            "HTTP 403: Resource not accessible by personal access token"
        ));
        assert!(!is_auth_failure("HTTP 403: API rate limit exceeded"));
        assert!(!is_auth_failure("could not resolve host api.github.com"));
    }

    #[test]
    fn alert_dedup_and_recovery() {
        let mut s = AuthState::default();
        assert!(s.record(true, "x", t(0)).is_none());
        assert_eq!(
            s.record(false, "poll", t(0)),
            Some(Alert::Failing("poll".into()))
        );
        assert!(s.record(false, "poll", t(1)).is_none());
        assert!(s.record(false, "poll", t(5)).is_none());
        assert!(matches!(
            s.record(false, "poll", t(6)),
            Some(Alert::Failing(_))
        ));
        assert_eq!(s.record(true, "", t(7)), Some(Alert::Recovered));
        assert!(s.record(true, "", t(8)).is_none());
        // fresh failure after recovery alerts immediately
        assert!(matches!(
            s.record(false, "poll", t(9)),
            Some(Alert::Failing(_))
        ));
    }

    #[test]
    fn expiry_parse_and_daily_warning() {
        let exp = parse_expiration("2026-10-10 00:00:00 UTC").unwrap();
        assert_eq!(days_left(exp, t(0)), 9);
        assert!(parse_expiration("2027-01-12 07:28:00 +0000").is_some());
        assert!(parse_expiration("garbage").is_none());
        let mut s = AuthState::default();
        assert_eq!(s.note_expiry(Some(exp), t(0)), Some(Alert::Expiring(9)));
        assert!(s.note_expiry(Some(exp), t(23)).is_none());
        assert!(matches!(
            s.note_expiry(Some(exp), t(24)),
            Some(Alert::Expiring(_))
        ));
        let far = parse_expiration("2027-01-12 07:28:00 UTC").unwrap();
        let mut s2 = AuthState::default();
        assert!(s2.note_expiry(Some(far), t(0)).is_none());
        assert!(s2.note_expiry(None, t(0)).is_none());
    }

    #[test]
    fn scrub_then_set_token() {
        let mut ops: Vec<(String, Option<String>)> = Vec::new();
        scrub_and_set(
            &mut |k, v| ops.push((k.into(), v.map(String::from))),
            Some("t"),
        );
        assert_eq!(ops[0], ("GH_TOKEN".into(), None));
        assert!(ops.contains(&("GH_TOKEN".into(), Some("t".into()))));
        let mut ops2: Vec<(String, Option<String>)> = Vec::new();
        scrub_and_set(&mut |k, v| ops2.push((k.into(), v.map(String::from))), None);
        assert!(!ops2.iter().any(|(k, v)| k == "GH_TOKEN" && v.is_some()));
    }

    #[test]
    fn hook_gets_alert_env_and_times_out() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("out");
        let hook = crate::config::NotifyConfig {
            command: format!("printf '%s|%s|%s|%s' \"$ATR_ALERT_KIND\" \"$ATR_ALERT_MESSAGE\" \"$ATR_ALERT_DAYS_LEFT\" \"${{ATR_GH_TOKEN:-none}}\" > {}", out.display()),
            timeout: Duration::from_secs(5),
        };
        assert_eq!(
            run_hook(
                &hook,
                "github_token_expiring",
                "hi",
                &[("ATR_ALERT_DAYS_LEFT", "3".into())]
            ),
            Some(0)
        );
        assert_eq!(
            std::fs::read_to_string(&out).unwrap(),
            "github_token_expiring|hi|3|none"
        );
        let slow = crate::config::NotifyConfig {
            command: "sleep 5".into(),
            timeout: Duration::from_millis(200),
        };
        let t0 = std::time::Instant::now();
        assert_eq!(run_hook(&slow, "k", "m", &[]), None);
        assert!(t0.elapsed() < Duration::from_secs(3));
        assert_eq!(
            run_hook(&crate::config::NotifyConfig::default(), "k", "m", &[]),
            None
        );
    }

    #[test]
    fn summarize_levels() {
        assert_eq!(
            summarize(&serde_json::Value::Null, t(0))["level"],
            "unknown"
        );
        let fail =
            serde_json::json!({"state":"failing","failing_since":"x","last_error":"issue list"});
        assert_eq!(summarize(&fail, t(0))["level"], "fail");
        let warn = serde_json::json!({"state":"ok","expires_at":"2026-10-05T00:00:00+00:00"});
        let w = summarize(&warn, t(0));
        assert_eq!(w["level"], "warn");
        assert_eq!(w["days_left"], 4);
        let ok = serde_json::json!({"state":"ok","expires_at":"2027-01-12T07:28:00+00:00"});
        assert_eq!(summarize(&ok, t(0))["level"], "ok");
    }

    #[test]
    fn owner_mapping_case_insensitive_with_fallback() {
        let mut m = std::collections::BTreeMap::new();
        m.insert("TaskrApps".to_string(), "TOK_T".to_string());
        m.insert("rimusz".to_string(), "TOK_R".to_string());
        assert_eq!(owner_key(&m, "taskrapps"), Some("TOK_T"));
        assert_eq!(owner_key(&m, "RIMUSZ"), Some("TOK_R"));
        assert_eq!(owner_key(&m, "other"), None);
        assert_eq!(owner_key(&m, ""), None);
        assert_eq!(repo_owner("TaskrApps/nixer-app"), "TaskrApps");
        // mapped key first, then the default chain
        let mut keys = vec!["TOK_T".to_string()];
        keys.extend(keys_for("ATR_SHADOW_GH_TOKEN"));
        let env = |k: &str| (k == "ATR_GH_TOKEN").then(|| "default".to_string());
        assert_eq!(resolve_with(&keys, None, env).unwrap().0, "ATR_GH_TOKEN");
        let env2 = |k: &str| match k {
            "TOK_T" => Some("t".to_string()),
            "ATR_GH_TOKEN" => Some("d".to_string()),
            _ => None,
        };
        assert_eq!(resolve_with(&keys, None, env2).unwrap().0, "TOK_T");
    }

    #[test]
    fn per_token_state_is_independent() {
        with_state("TEST_A", |s| *s = AuthState::default());
        with_state("TEST_B", |s| *s = AuthState::default());
        assert!(with_state("TEST_A", |s| s.record(false, "x", t(0))).is_some());
        assert!(with_state("TEST_B", |s| s.failing_since.is_none()));
        assert!(with_state("TEST_B", |s| s.record(false, "y", t(0))).is_some());
        assert_eq!(
            with_state("TEST_A", |s| s.record(true, "", t(1))),
            Some(Alert::Recovered)
        );
        assert!(with_state("TEST_B", |s| s.failing_since.is_some()));
    }

    #[test]
    fn summarize_per_token_worst_wins() {
        let raw = serde_json::json!({"tokens": {
            "TOK_R": {"state":"ok","token_env":"TOK_R","owner":"rimusz","expires_at":"2027-01-12T07:28:00+00:00"},
            "TOK_T": {"state":"failing","token_env":"TOK_T","owner":"TaskrApps","failing_since":"x","last_error":"issue list"}
        }});
        let v = summarize(&raw, t(0));
        assert_eq!(v["level"], "fail");
        assert!(v["message"]
            .as_str()
            .unwrap()
            .contains("TOK_T (owner TaskrApps)"));
        assert_eq!(v["tokens"].as_array().unwrap().len(), 2);
        let txt = alert_text_for(&Alert::Expiring(3), "TOK_R", Some("rimusz"));
        assert!(txt.contains("TOK_R (owner rimusz)") && txt.contains("3 day"));
    }

    #[test]
    fn hook_env_scrubs_token_like_vars() {
        assert!(is_token_var("ATR_GH_TOKEN_TASKRAPPS"));
        assert!(is_token_var("GITHUB_TOKEN"));
        assert!(!is_token_var("ATR_ALERT_KIND"));
    }

    #[test]
    fn alert_kinds() {
        assert_eq!(Alert::Recovered.kind(), "github_auth_recovered");
        assert_eq!(Alert::Expiring(1).kind(), "github_token_expiring");
    }

    #[test]
    fn custom_shadow_key_is_second() {
        assert_eq!(
            keys_for("MY_TOK"),
            vec!["ATR_GH_TOKEN", "MY_TOK", "GH_TOKEN", "GITHUB_TOKEN"]
        );
        assert_eq!(keys_for("GH_TOKEN").len(), 3);
    }

    #[test]
    fn alert_text_never_contains_value() {
        let s = alert_text(&Alert::Failing("poll".into()), Some("ATR_GH_TOKEN"));
        assert!(s.contains("ATR_GH_TOKEN") && s.contains("6h"));
    }
}

#[cfg(test)]
mod tokens_in_use_tests {
    #[test]
    fn groups_repos_by_owner_token() {
        let cfg = crate::config::parse_config_toml(
            "[github]\nenabled = false\n[github.tokens]\ntaskrapps = \"ATR_TEST_TOK_TASKR_UNSET\"\n[shadow]\nenabled = true\nrepo = \"TaskrApps/nixer-app\"\nlabels = [\"x\"]\n\n[[provider]]\nname = \"claude\"\nkind = \"claude\"\ncli = \"/bin/true\"\n",
        )
        .unwrap();
        let t = super::tokens_in_use(&cfg, None);
        let mapped = t.iter().find(|u| u.owner.as_deref() == Some("TaskrApps"));
        // value may resolve via the default chain if the env has one; either way
        // the owner mapping is reported when its key is the one chosen.
        if let Some(m) = mapped {
            assert_eq!(m.key, "ATR_TEST_TOK_TASKR_UNSET");
            assert_eq!(m.repos, vec!["TaskrApps/nixer-app".to_string()]);
        } else {
            assert_eq!(t.len(), 1);
            assert!(t[0].value.is_some());
        }
    }
}
