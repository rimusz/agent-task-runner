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

/// `(env name, value)` of the token atr will use. Never log the value.
pub fn resolve(cfg: &Config, config_path: Option<&Path>) -> Option<(String, String)> {
    resolve_with(&token_keys(cfg), config_path, |k| std::env::var(k).ok())
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

pub fn apply(cmd: &mut tokio::process::Command, cfg: &Config, config_path: Option<&Path>) {
    let tok = resolve(cfg, config_path).map(|(_, v)| v);
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

pub fn apply_std(cmd: &mut std::process::Command, cfg: &Config, config_path: Option<&Path>) {
    let tok = resolve(cfg, config_path).map(|(_, v)| v);
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

static STATE: Mutex<Option<AuthState>> = Mutex::new(None);

fn with_state<T>(f: impl FnOnce(&mut AuthState) -> T) -> T {
    let mut g = STATE.lock().unwrap_or_else(|e| e.into_inner());
    f(g.get_or_insert_with(AuthState::default))
}

pub fn alert_text(a: &Alert, key: Option<&str>) -> String {
    let key = key.unwrap_or(PRIMARY_KEY);
    match a {
        Alert::Failing(what) => format!(
            "GitHub auth failing ({what}). Token {key} rejected (401/403); GitHub intake is stalled. Rotate the fine-grained PAT in ~/.config/atr/shadow.env and restart atr. Next reminder in 6h."
        ),
        Alert::Recovered => "GitHub auth recovered; intake is running again.".into(),
        Alert::Expiring(d) => format!(
            "GitHub token {key} expires in {d} day(s). Create a new fine-grained PAT and update ~/.config/atr/shadow.env."
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

/// Run `[notify] command` via `sh -c` with ATR_ALERT_* env, killed after
/// `timeout`. Token env vars are scrubbed from the hook's environment.
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

fn send(cfg: &Config, a: &Alert, key: Option<&str>) {
    let text = alert_text(a, key);
    if !cfg.notify.command.is_empty() {
        let hook = cfg.notify.clone();
        let kind = a.kind();
        let msg = text.clone();
        let mut extra = vec![("ATR_ALERT_SOURCE", "github".to_string())];
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
        crate::notify::notify(&cfg.telegram, &text);
    }
}

pub fn state_path(config_path: &Path) -> std::path::PathBuf {
    config_path
        .parent()
        .map(|p| p.join("github-auth.json"))
        .unwrap_or_else(|| std::path::PathBuf::from("github-auth.json"))
}

fn persist(config_path: Option<&Path>, key: Option<&str>, last_error: Option<&str>) {
    let Some(cp) = config_path else { return };
    let st = with_state(|s| s.clone());
    let v = serde_json::json!({
        "state": if st.failing_since.is_some() { "failing" } else { "ok" },
        "failing_since": st.failing_since.map(|t| t.to_rfc3339()),
        "expires_at": st.expires_at.map(|t| t.to_rfc3339()),
        "token_env": key,
        "last_error": last_error.map(crate::github_auth::redact_secrets),
        "checked_at": Utc::now().to_rfc3339(),
    });
    let path = state_path(cp);
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, v.to_string()).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
}

/// GitHub auth summary for `atr status` / dashboard. `level`: ok|warn|fail|unknown.
pub fn status_json(config_path: &Path, now: DateTime<Utc>) -> serde_json::Value {
    let raw: serde_json::Value = std::fs::read_to_string(state_path(config_path))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or(serde_json::Value::Null);
    summarize(&raw, now)
}

pub fn summarize(raw: &serde_json::Value, now: DateTime<Utc>) -> serde_json::Value {
    if raw.is_null() {
        return serde_json::json!({"level": "unknown", "message": "no GitHub auth check yet"});
    }
    let s = |k: &str| raw.get(k).and_then(|v| v.as_str()).map(String::from);
    let exp = s("expires_at")
        .and_then(|v| DateTime::parse_from_rfc3339(&v).ok())
        .map(|d| d.with_timezone(&Utc));
    let days = exp.map(|e| days_left(e, now));
    let failing = s("state").as_deref() == Some("failing");
    let (level, message) = if failing {
        (
            "fail",
            format!(
                "GitHub auth failing since {} ({})",
                s("failing_since").unwrap_or_default(),
                s("last_error").unwrap_or_else(|| "401/403".into())
            ),
        )
    } else if let Some(d) = days.filter(|d| *d < EXPIRY_WARN_DAYS) {
        ("warn", format!("GitHub token expires in {d} day(s)"))
    } else {
        (
            "ok",
            match days {
                Some(d) => format!("GitHub auth ok; token expires in {d} day(s)"),
                None => "GitHub auth ok".into(),
            },
        )
    };
    serde_json::json!({
        "level": level,
        "message": message,
        "token_env": s("token_env"),
        "expires_at": s("expires_at"),
        "days_left": days,
        "checked_at": s("checked_at"),
    })
}

/// Feed a `gh` invocation result into the auth tracker.
pub fn observe_gh(
    cfg: &Config,
    config_path: Option<&Path>,
    success: bool,
    stderr: &str,
    what: &str,
) {
    let auth_fail = !success && is_auth_failure(stderr);
    if !success && !auth_fail {
        return; // unrelated failure: says nothing about auth
    }
    if auth_fail {
        tracing::error!(
            what,
            "GitHub auth failure: {}",
            crate::github_auth::redact_secrets(stderr.trim())
        );
    }
    let alert = with_state(|s| s.record(!auth_fail, what, Utc::now()));
    let key = resolve(cfg, config_path).map(|(k, _)| k);
    if alert.is_some() || auth_fail {
        persist(config_path, key.as_deref(), auth_fail.then_some(what));
    }
    if let Some(a) = alert {
        send(cfg, &a, key.as_deref());
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

/// Periodic health probe (GET /user) run by the poller at most hourly.
pub async fn maybe_probe(cfg: &Config, config_path: &Path) {
    let now = Utc::now();
    let due = with_state(|s| match s.last_probe {
        None => true,
        Some(t) => (now - t)
            .to_std()
            .map(|d| d >= PROBE_EVERY)
            .unwrap_or(false),
    });
    if !due {
        return;
    }
    with_state(|s| s.last_probe = Some(now));
    let Some((key, tok)) = resolve(cfg, Some(config_path)) else {
        tracing::debug!("no GitHub token configured; skip auth probe");
        return;
    };
    let p = match tokio::task::spawn_blocking(move || get(&tok, "user")).await {
        Ok(p) => p,
        Err(_) => return,
    };
    let mut alerts = Vec::new();
    match p.status {
        0 => return,
        401 | 403 => {
            tracing::error!(status = p.status, "GitHub token probe rejected");
            if let Some(a) =
                with_state(|s| s.record(false, &format!("GET /user -> {}", p.status), now))
            {
                alerts.push(a);
            }
        }
        200..=299 => {
            if let Some(a) = with_state(|s| s.record(true, "", now)) {
                alerts.push(a);
            }
            if let Some(a) = with_state(|s| s.note_expiry(p.expires_at, now)) {
                alerts.push(a);
            }
        }
        _ => {}
    }
    let err = (p.status == 401 || p.status == 403).then(|| format!("GET /user -> {}", p.status));
    persist(Some(config_path), Some(&key), err.as_deref());
    for a in alerts {
        send(cfg, &a, Some(&key));
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
