//! Daily login check for enabled providers, run inside `atr serve`.
//!
//! Cursor is read from this process's login keychain. An SSH session often
//! cannot unlock that keychain, so a lock there is not reported as logged
//! out and does not page Telegram. Values (tokens, refresh tokens) are never
//! logged or included in the alert text.

use crate::config::Provider;
use crate::telegram;
use jiff::{SignedDuration, Timestamp};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::time::Duration;

const WARN_BEFORE: SignedDuration = SignedDuration::from_hours(24);
const SPARK_ADDR: &str = "127.0.0.1:8091";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Note {
    pub provider: String,
    pub status: Status,
    pub detail: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Ok,
    Warn,
    Fail,
}

/// Real home in a release build. Tests stay off the login keychain unless
/// `ATR_AUTH_PROBE_HOME` is set.
pub fn enabled() -> bool {
    #[cfg(test)]
    {
        std::env::var_os("ATR_AUTH_PROBE_HOME").is_some()
    }
    #[cfg(not(test))]
    {
        true
    }
}

pub fn probe_home() -> PathBuf {
    if let Some(h) = std::env::var_os("ATR_AUTH_PROBE_HOME") {
        return PathBuf::from(h);
    }
    #[cfg(test)]
    {
        PathBuf::from("/tmp/atr-auth-probe-empty")
    }
    #[cfg(not(test))]
    crate::config::expand_path("~")
}

pub fn ssh_session() -> bool {
    std::env::var_os("SSH_CONNECTION").is_some() || std::env::var_os("SSH_TTY").is_some()
}

pub async fn daily_loop(shared: std::sync::Arc<crate::app::Shared>) {
    loop {
        let cfg = shared.config.read().unwrap().clone();
        let providers = cfg.providers.clone();
        let notes = tokio::task::spawn_blocking(move || probe_providers(&providers))
            .await
            .unwrap_or_default();
        if let Some(text) = alert_text(&notes) {
            tracing::warn!("{text}");
            telegram::send_alert(&cfg.telegram, &text, true);
        } else if !notes.is_empty() {
            tracing::info!(n = notes.len(), "auth probe ok");
        }
        tokio::time::sleep(Duration::from_secs(24 * 60 * 60)).await;
    }
}

pub fn probe_providers(providers: &[Provider]) -> Vec<Note> {
    let home = probe_home();
    let now = Timestamp::now();
    let ssh = ssh_session();
    providers
        .iter()
        .filter(|p| p.enabled)
        .filter_map(|p| probe_one(&home, p, now, ssh))
        .collect()
}

pub fn alert_text(notes: &[Note]) -> Option<String> {
    let bad: Vec<_> = notes.iter().filter(|n| n.status != Status::Ok).collect();
    if bad.is_empty() {
        return None;
    }
    let mut out = String::from("auth probe:");
    for n in bad {
        out.push_str(&format!("\n- {}: {}", n.provider, n.detail));
    }
    Some(out)
}

fn probe_one(home: &Path, p: &Provider, now: Timestamp, ssh: bool) -> Option<Note> {
    let kind = if crate::dashboard::spark_usage_hidden(&p.name, &p.preset, &p.model) {
        "spark"
    } else {
        p.kind.as_str()
    };
    let detail = match kind {
        "claude" => claude(home, now),
        "grok" => grok(home, now),
        "cursor" => cursor(now, ssh),
        "codex" => codex(home, now),
        "spark" => spark(),
        _ => return None,
    };
    let (status, detail) = detail;
    Some(Note {
        provider: p.name.clone(),
        status,
        detail,
    })
}

fn claude(home: &Path, now: Timestamp) -> (Status, String) {
    let path = home.join(".claude/.credentials.json");
    let env_set = std::env::var_os("CLAUDE_CODE_OAUTH_TOKEN").is_some_and(|v| !v.is_empty());
    let text = std::fs::read_to_string(&path).ok();
    let v = text
        .as_deref()
        .and_then(|t| serde_json::from_str::<Value>(t).ok());
    let oauth = v.as_ref().and_then(|v| v.get("claudeAiOauth"));
    let has_access = oauth
        .and_then(|o| o.get("accessToken"))
        .and_then(|t| t.as_str())
        .is_some_and(|t| !t.trim().is_empty());
    if !has_access && !env_set {
        return (
            Status::Warn,
            "logged out (no Claude credentials and CLAUDE_CODE_OAUTH_TOKEN is unset)".into(),
        );
    }
    let exp = oauth.and_then(claude_expiry);
    match exp {
        Some(t) if t <= now => (
            Status::Fail,
            format!("Claude access expired at {}", crate::rfc3339::millis(t)),
        ),
        Some(t) if t <= now + WARN_BEFORE => (
            Status::Warn,
            format!("Claude access expires at {}", crate::rfc3339::millis(t)),
        ),
        Some(t) => (
            Status::Ok,
            format!("logged in; expires {}", crate::rfc3339::millis(t)),
        ),
        None if env_set && !has_access => (
            Status::Ok,
            "logged in via CLAUDE_CODE_OAUTH_TOKEN; expiry is not in a credentials file".into(),
        ),
        None => (
            Status::Ok,
            "logged in; credentials have no expiry metadata".into(),
        ),
    }
}

fn claude_expiry(oauth: &Value) -> Option<Timestamp> {
    for key in ["expiresAt", "expiresAtMs"] {
        if let Some(n) = oauth.get(key).and_then(num_i64) {
            let secs = if n > 10_000_000_000 { n / 1000 } else { n };
            return Timestamp::from_second(secs).ok();
        }
    }
    let token = oauth.get("accessToken").and_then(|t| t.as_str())?;
    crate::plan_usage::jwt_exp_unix(token).and_then(|s| Timestamp::from_second(s).ok())
}

fn grok(home: &Path, now: Timestamp) -> (Status, String) {
    let path = ["/.grok/auth.json", "/.config/grok/credentials.json"]
        .into_iter()
        .map(|rel| home.join(rel.trim_start_matches('/')))
        .find(|p| p.is_file());
    let Some(path) = path else {
        return (Status::Warn, "logged out (no grok auth file)".into());
    };
    let Ok(text) = std::fs::read_to_string(&path) else {
        return (
            Status::Fail,
            "logged out (grok auth file unreadable)".into(),
        );
    };
    let Ok(v) = serde_json::from_str::<Value>(&text) else {
        return (Status::Fail, "grok auth file is not json".into());
    };
    let refresh = v
        .get("refresh_token")
        .and_then(|t| t.as_str())
        .is_some_and(|t| !t.trim().is_empty());
    let exp = grok_expiry(&v);
    if !refresh && exp.is_none() && !has_access(&v) {
        return (Status::Warn, "logged out (grok auth has no token)".into());
    }
    match exp {
        Some(t) if t <= now && !refresh => (
            Status::Fail,
            format!(
                "grok access expired at {} and there is no refresh token",
                crate::rfc3339::millis(t)
            ),
        ),
        Some(t) if t <= now + WARN_BEFORE && !refresh => (
            Status::Warn,
            format!(
                "grok access expires at {} and there is no refresh token",
                crate::rfc3339::millis(t)
            ),
        ),
        Some(t) if refresh => (
            Status::Ok,
            format!(
                "session still refreshable; expires_at {}",
                crate::rfc3339::millis(t)
            ),
        ),
        Some(t) => (
            Status::Ok,
            format!("logged in; expires {}", crate::rfc3339::millis(t)),
        ),
        None if refresh => (
            Status::Ok,
            "session still refreshable; no expires_at".into(),
        ),
        None => (Status::Ok, "logged in; no expires_at".into()),
    }
}

fn has_access(v: &Value) -> bool {
    v.get("access_token")
        .or_else(|| v.pointer("/tokens/access_token"))
        .and_then(|t| t.as_str())
        .is_some_and(|t| !t.trim().is_empty())
}

fn grok_expiry(v: &Value) -> Option<Timestamp> {
    let raw = v.get("expires_at")?;
    if let Some(n) = num_i64(raw) {
        let secs = if n > 10_000_000_000 { n / 1000 } else { n };
        return Timestamp::from_second(secs).ok();
    }
    raw.as_str().and_then(crate::rfc3339::parse)
}

fn cursor(now: Timestamp, ssh: bool) -> (Status, String) {
    match cursor_keychain(now) {
        CursorKey::Usable { expires } => match expires {
            Some(t) if t <= now + WARN_BEFORE => (
                Status::Warn,
                format!("cursor access expires at {}", crate::rfc3339::millis(t)),
            ),
            Some(t) => (
                Status::Ok,
                format!("logged in; access expires {}", crate::rfc3339::millis(t)),
            ),
            None => (Status::Ok, "logged in; access token has no exp".into()),
        },
        CursorKey::Refreshable => (
            Status::Ok,
            "access token expired or missing; refresh token is in the login keychain".into(),
        ),
        CursorKey::Locked if ssh => (
            Status::Warn,
            "keychain locked in this SSH session; the serve GUI session is the auth check".into(),
        ),
        CursorKey::Locked => (
            Status::Fail,
            "cursor login keychain is not readable in this session".into(),
        ),
        CursorKey::Missing => (
            Status::Warn,
            "logged out (no cursor-access-token or cursor-refresh-token in the login keychain)"
                .into(),
        ),
    }
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
enum CursorKey {
    Usable { expires: Option<Timestamp> },
    Refreshable,
    Locked,
    Missing,
}

fn cursor_keychain(now: Timestamp) -> CursorKey {
    #[cfg(not(target_os = "macos"))]
    {
        let _ = now;
        CursorKey::Missing
    }
    #[cfg(target_os = "macos")]
    {
        match security_item("cursor-access-token") {
            Sec::Value(token) => {
                if !crate::plan_usage::jwt_expired(&token, now.as_second()) {
                    let exp = crate::plan_usage::jwt_exp_unix(&token)
                        .and_then(|s| Timestamp::from_second(s).ok());
                    return CursorKey::Usable { expires: exp };
                }
            }
            Sec::Locked => return CursorKey::Locked,
            Sec::Missing => {}
        }
        match security_item("cursor-refresh-token") {
            Sec::Value(_) => CursorKey::Refreshable,
            Sec::Locked => CursorKey::Locked,
            Sec::Missing => CursorKey::Missing,
        }
    }
}

#[cfg(target_os = "macos")]
enum Sec {
    Value(String),
    Locked,
    Missing,
}

#[cfg(target_os = "macos")]
fn security_item(service: &str) -> Sec {
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
        .output();
    let Ok(out) = out else {
        return Sec::Missing;
    };
    if !out.status.success() {
        return match out.status.code() {
            Some(36) => Sec::Locked,
            _ => Sec::Missing,
        };
    }
    match String::from_utf8(out.stdout) {
        Ok(s) if !s.trim().is_empty() => Sec::Value(s.trim().to_string()),
        _ => Sec::Missing,
    }
}

fn codex(home: &Path, now: Timestamp) -> (Status, String) {
    let path = home.join(".codex/auth.json");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return (Status::Warn, "logged out (no ~/.codex/auth.json)".into());
    };
    let Ok(v) = serde_json::from_str::<Value>(&text) else {
        return (Status::Fail, "codex auth.json is not json".into());
    };
    let api_key = v
        .get("OPENAI_API_KEY")
        .and_then(|t| t.as_str())
        .is_some_and(|t| !t.trim().is_empty());
    let access = v
        .pointer("/tokens/access_token")
        .and_then(|t| t.as_str())
        .filter(|t| !t.is_empty());
    if access.is_none() && api_key {
        return (Status::Ok, "API key; no expiry".into());
    }
    let Some(access) = access else {
        return (Status::Warn, "logged out (codex auth has no token)".into());
    };
    let refresh = v
        .pointer("/tokens/refresh_token")
        .and_then(|t| t.as_str())
        .is_some_and(|t| !t.is_empty());
    match crate::plan_usage::jwt_exp_unix(access).and_then(|s| Timestamp::from_second(s).ok()) {
        Some(t) if t <= now && !refresh => (
            Status::Fail,
            format!("codex access expired at {}", crate::rfc3339::millis(t)),
        ),
        Some(t) if t <= now + WARN_BEFORE && !refresh => (
            Status::Warn,
            format!("codex access expires at {}", crate::rfc3339::millis(t)),
        ),
        _ => (Status::Ok, "auth present".into()),
    }
}

fn spark() -> (Status, String) {
    match std::net::TcpStream::connect_timeout(
        &SPARK_ADDR.parse().expect("spark addr"),
        Duration::from_secs(2),
    ) {
        Ok(_) => (Status::Ok, format!("{SPARK_ADDR} reachable")),
        Err(_) => (Status::Warn, format!("{SPARK_ADDR} unreachable")),
    }
}

fn num_i64(v: &Value) -> Option<i64> {
    v.as_i64()
        .or_else(|| v.as_u64().and_then(|u| i64::try_from(u).ok()))
        .or_else(|| v.as_f64().map(|f| f as i64))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider(name: &str, kind: &str) -> Provider {
        let mut p = Provider::test_stub(name);
        p.kind = kind.into();
        p.enabled = true;
        p
    }

    #[test]
    fn claude_warns_before_expiry_and_grok_refresh_is_ok() {
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join(".claude")).unwrap();
        let soon = (Timestamp::now() + SignedDuration::from_hours(2)).as_second() * 1000;
        std::fs::write(
            home.path().join(".claude/.credentials.json"),
            format!(r#"{{"claudeAiOauth":{{"accessToken":"t","expiresAt":{soon}}}}}"#),
        )
        .unwrap();
        let (st, detail) = claude(home.path(), Timestamp::now());
        assert_eq!(st, Status::Warn, "{detail}");
        assert!(!detail.contains("accessToken"));

        std::fs::create_dir_all(home.path().join(".grok")).unwrap();
        std::fs::write(
            home.path().join(".grok/auth.json"),
            r#"{"access_token":"a","refresh_token":"r","expires_at":"2020-01-01T00:00:00Z"}"#,
        )
        .unwrap();
        let (st, detail) = grok(home.path(), Timestamp::now());
        assert_eq!(st, Status::Ok, "{detail}");
        assert!(detail.contains("refreshable"), "{detail}");
        assert!(!detail.contains("\"r\"") && !detail.contains("refresh_token"));

        let (st, _) = codex(home.path(), Timestamp::now());
        assert_eq!(st, Status::Warn);
        std::fs::create_dir_all(home.path().join(".codex")).unwrap();
        std::fs::write(
            home.path().join(".codex/auth.json"),
            r#"{"OPENAI_API_KEY":"sk-test"}"#,
        )
        .unwrap();
        let (st, detail) = codex(home.path(), Timestamp::now());
        assert_eq!(st, Status::Ok);
        assert_eq!(detail, "API key; no expiry");
        assert!(!detail.contains("sk-test"));
    }

    #[test]
    fn alert_lists_only_bad_rows() {
        let notes = vec![
            Note {
                provider: "claude".into(),
                status: Status::Ok,
                detail: "logged in".into(),
            },
            Note {
                provider: "spark".into(),
                status: Status::Warn,
                detail: "127.0.0.1:8091 unreachable".into(),
            },
        ];
        let text = alert_text(&notes).unwrap();
        assert!(text.contains("spark"));
        assert!(!text.contains("claude"));
        assert!(alert_text(&notes[..1]).is_none());
    }

    #[test]
    fn ssh_cursor_lock_is_a_warn_not_a_logout_page_when_classified() {
        let note = Note {
            provider: "cursor".into(),
            status: Status::Warn,
            detail: "keychain locked in this SSH session; the serve GUI session is the auth check"
                .into(),
        };
        let text = alert_text(&[note]).unwrap();
        assert!(text.contains("SSH"));
        assert!(!text.contains("logged out"));
    }

    #[test]
    fn spark_provider_is_a_port_check() {
        let mut p = provider("spark", "opencode");
        p.preset = "spark".into();
        let home = tempfile::tempdir().unwrap();
        let note = probe_one(home.path(), &p, Timestamp::now(), false).unwrap();
        assert!(note.detail.contains("8091"), "{:?}", note.detail);
    }
}
