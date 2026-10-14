use crate::clock::Clock;
use crate::config::Provider;
use crate::db::{Db, ProviderUsageRow};
use anyhow::{Context, Result};
use chrono::{DateTime, Duration as ChronoDuration, TimeZone, Utc};
use serde_json::Value;
use std::path::Path;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::time::timeout;

#[derive(Debug, Clone, PartialEq)]
pub struct UsageWindow {
    pub window: String,
    pub utilization: Option<f64>,
    /// Grok extra paid credits in dollars (`prepaidBalance` cents / 100).
    /// Informational only — atr stays on the plan and never spends these.
    pub balance: Option<f64>,
    pub resets_at: Option<DateTime<Utc>>,
    pub status: Option<String>,
}

fn claude_rate_limit_info(value: &Value) -> Option<&Value> {
    value
        .get("rate_limit_info")
        .or_else(|| value.get("rateLimitInfo"))
        .or_else(|| value.get("rate_limits"))
        .or_else(|| value.get("rateLimits"))
        .or_else(|| value.get("message").and_then(claude_rate_limit_info))
}

pub fn parse_claude_rate_limit(value: &Value) -> Option<Vec<UsageWindow>> {
    let ty = value.get("type").and_then(|v| v.as_str()).unwrap_or("");
    let info = claude_rate_limit_info(value);
    if ty != "rate_limit_event" && info.is_none() {
        return None;
    }
    let info = info.unwrap_or(value);
    let status = info
        .get("status")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let windows = info
        .get("unifiedWindows")
        .or_else(|| info.get("unified_windows"))
        .or_else(|| info.get("windows"));
    let mut out = Vec::new();
    if let Some(obj) = windows.and_then(|v| v.as_object()) {
        for (name, w) in obj {
            out.push(UsageWindow {
                window: name.clone(),
                utilization: number_01(w.get("utilization").or_else(|| w.get("used"))),
                balance: None,
                resets_at: unix_or_rfc(w.get("resetsAt").or_else(|| w.get("resets_at"))),
                status: status.clone(),
            });
        }
    }
    if out.is_empty() {
        if let Some(name) = info
            .get("rateLimitType")
            .or_else(|| info.get("rate_limit_type"))
            .and_then(|v| v.as_str())
        {
            out.push(UsageWindow {
                window: name.into(),
                utilization: number_01(info.get("utilization")),
                balance: None,
                resets_at: unix_or_rfc(info.get("resetsAt").or_else(|| info.get("resets_at"))),
                status: status.clone(),
            });
        }
    }
    if out.is_empty() {
        return None;
    }
    Some(out)
}

pub fn parse_grok_billing(value: &Value) -> Option<UsageWindow> {
    let root = value.get("result").unwrap_or(value);
    let config = root.get("config").unwrap_or(root);
    let period = config
        .get("currentPeriod")
        .or_else(|| config.get("current_period"));
    let end = period.and_then(|p| p.get("end").or_else(|| p.get("endsAt")));
    let window = period
        .and_then(|p| p.get("type"))
        .and_then(|v| v.as_str())
        .unwrap_or("prepaid");
    // prepaidBalance is extra paid credits in cents (14032 = $140.32), not plan allowance.
    let extra_credits_usd = config
        .pointer("/prepaidBalance/val")
        .or_else(|| config.pointer("/prepaid_balance/val"))
        .or_else(|| config.get("prepaidBalance"))
        .and_then(json_f64)
        .map(|cents| cents / 100.0);
    // creditUsagePercent is a percent of the weekly plan (1.0 = 1%). Always /100.
    let utilization = config
        .get("creditUsagePercent")
        .or_else(|| config.get("credit_usage_percent"))
        .and_then(json_f64)
        .map(|pct| (pct / 100.0).max(0.0));
    if end.is_none() && extra_credits_usd.is_none() && utilization.is_none() {
        return None;
    }
    let window = crate::db::canonical_usage_window(window);
    Some(UsageWindow {
        window,
        utilization,
        balance: extra_credits_usd,
        resets_at: unix_or_rfc(end),
        status: root
            .get("subscription_tier")
            .or_else(|| root.get("subscriptionTier"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
    })
}

fn json_f64(v: &Value) -> Option<f64> {
    v.as_f64()
        .or_else(|| v.as_i64().map(|n| n as f64))
        .or_else(|| v.as_u64().map(|n| n as f64))
        .or_else(|| v.as_str()?.parse().ok())
}

fn number_01(v: Option<&Value>) -> Option<f64> {
    let n = json_f64(v?)?;
    if n > 1.0 {
        Some(n / 100.0)
    } else {
        Some(n.max(0.0))
    }
}

fn unix_or_rfc(v: Option<&Value>) -> Option<DateTime<Utc>> {
    let v = v?;
    if let Some(n) = v.as_i64().or_else(|| v.as_u64().map(|n| n as i64)) {
        if n > 1_000_000_000_000 {
            return Utc.timestamp_millis_opt(n).single();
        }
        return Utc.timestamp_opt(n, 0).single();
    }
    if let Some(f) = v.as_f64() {
        return Utc.timestamp_opt(f as i64, 0).single();
    }
    let s = v.as_str()?;
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|t| t.with_timezone(&Utc))
        .or_else(|| {
            s.parse::<i64>()
                .ok()
                .and_then(|n| Utc.timestamp_opt(n, 0).single())
        })
}

pub fn store_windows(db: &Db, provider: &str, rows: &[UsageWindow], clock: &Clock) -> Result<()> {
    let now = clock.rfc3339();
    for r in rows {
        let reset = r
            .resets_at
            .map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Millis, true));
        db.upsert_provider_usage(
            provider,
            &r.window,
            r.utilization,
            r.balance,
            r.utilization.map(|u| u * 100.0),
            reset.as_deref(),
            r.status.as_deref(),
            &now,
        )?;
    }
    Ok(())
}

pub fn store_claude_line(db: &Db, provider: &Provider, line: &str, clock: &Clock) -> Result<bool> {
    if provider.reported_usage != "claude_stream" {
        return Ok(false);
    }
    let Ok(v) = serde_json::from_str::<Value>(line.trim()) else {
        return Ok(false);
    };
    let Some(rows) = parse_claude_rate_limit(&v) else {
        return Ok(false);
    };
    store_windows(db, &provider.name, &rows, clock)?;
    Ok(true)
}

/// Scan a whole Claude transcript so a trailing `rate_limit_event` is stored
/// even if it arrived in the same read as process exit (verify follow-ups too).
pub fn store_claude_transcript(
    db: &Db,
    provider: &Provider,
    blob: &str,
    clock: &Clock,
) -> Result<bool> {
    if provider.reported_usage != "claude_stream" {
        return Ok(false);
    }
    let mut any = false;
    for line in blob.lines() {
        if store_claude_line(db, provider, line, clock)? {
            any = true;
        }
    }
    Ok(any)
}

/// True when a stored reported window is over skip_at (before reset) or not allowed.
/// Grok extra credits are never spent: skip at the plan window, same `skip_at` as Claude.
pub fn reported_over(db: &Db, p: &Provider, clock: &Clock) -> Result<bool> {
    if p.reported_usage.is_empty() || p.reported_usage == "none" {
        return Ok(false);
    }
    let now = clock.now();
    for row in db.list_provider_usage(&p.name)? {
        if window_over(&row, p, now) {
            return Ok(true);
        }
    }
    Ok(false)
}

fn window_over(row: &ProviderUsageRow, p: &Provider, now: DateTime<Utc>) -> bool {
    if let Some(reset) = row.resets_at.as_deref().and_then(parse_rfc) {
        if reset <= now {
            return false;
        }
    }
    if let Some(st) = row.status.as_deref() {
        if !st.is_empty()
            && !st.eq_ignore_ascii_case("allowed")
            && p.reported_usage == "claude_stream"
        {
            return true;
        }
    }
    if let Some(u) = row.utilization {
        if u >= p.skip_at {
            return true;
        }
    }
    false
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResetDisplay {
    At(DateTime<Utc>),
    WindowReset,
}

fn window_length(name: &str) -> Option<ChronoDuration> {
    match crate::db::canonical_usage_window(name).as_str() {
        "five_hour" | "fivehour" | "5h" => Some(ChronoDuration::hours(5)),
        "seven_day" | "sevenday" | "7d" | "weekly" => Some(ChronoDuration::days(7)),
        other => crate::duration::parse_duration(other)
            .ok()
            .and_then(|d| ChronoDuration::from_std(d).ok())
            .filter(|d| *d > ChronoDuration::zero()),
    }
}

fn roll_reset_forward(
    start: DateTime<Utc>,
    period: ChronoDuration,
    now: DateTime<Utc>,
) -> DateTime<Utc> {
    if period <= ChronoDuration::zero() || start > now {
        return start;
    }
    let Ok(elapsed) = now.signed_duration_since(start).to_std() else {
        return start;
    };
    let Ok(step) = period.to_std() else {
        return start;
    };
    let step_secs = step.as_secs().max(1);
    let steps = elapsed.as_secs() / step_secs + 1;
    start + ChronoDuration::seconds((steps as i64).saturating_mul(step_secs as i64))
}

fn reset_display(
    window: &str,
    resets_at: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
) -> Option<ResetDisplay> {
    let t = resets_at?;
    if t > now {
        return Some(ResetDisplay::At(t));
    }
    Some(match window_length(window) {
        Some(len) => ResetDisplay::At(roll_reset_forward(t, len, now)),
        None => ResetDisplay::WindowReset,
    })
}

fn format_reset_suffix(
    window: &str,
    resets_at: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
    tz: &crate::tz::DisplayTz,
) -> String {
    match reset_display(window, resets_at, now) {
        Some(ResetDisplay::At(t)) => format!(", resets {}", tz.format_short(t)),
        Some(ResetDisplay::WindowReset) => ", window reset".into(),
        None => String::new(),
    }
}

fn json_reset_at(
    window: &str,
    resets_at: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
    tz: &crate::tz::DisplayTz,
) -> Option<String> {
    match reset_display(window, resets_at, now) {
        Some(ResetDisplay::At(t)) => Some(tz.format_short(t)),
        Some(ResetDisplay::WindowReset) => Some("window reset".into()),
        None => None,
    }
}

pub fn reported_resets(db: &Db, provider: &str, now: DateTime<Utc>) -> Vec<DateTime<Utc>> {
    let Ok(rows) = db.list_provider_usage(provider) else {
        return Vec::new();
    };
    rows.into_iter()
        .filter_map(|r| {
            let t = r.resets_at.as_deref().and_then(parse_rfc)?;
            match reset_display(&r.window, Some(t), now) {
                Some(ResetDisplay::At(t)) => Some(t),
                _ => None,
            }
        })
        .collect()
}

/// Common display shape. Provider-specific code only parses into stored
/// windows / extra credits; every surface uses [`format_provider_usage`].
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ProviderUsageView {
    pub windows: Vec<UsageWindowView>,
    /// Extra paid credits in USD. Display-only; never spent.
    pub extra_credits: Option<f64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct UsageWindowView {
    pub name: String,
    /// Percent of the plan window (1.0 = 1.0%), one decimal when formatted.
    pub percent_used: Option<f64>,
    pub resets_at: Option<DateTime<Utc>>,
}

fn window_display_name(raw: &str) -> String {
    match crate::db::canonical_usage_window(raw).as_str() {
        "five_hour" | "fivehour" => "5h".into(),
        "seven_day" | "sevenday" => "7d".into(),
        "weekly" => "weekly".into(),
        other => other.replace('_', " "),
    }
}

fn one_decimal(p: f64) -> f64 {
    (p * 10.0).round() / 10.0
}

/// Load the common usage view for one provider from stored rows.
pub fn provider_usage_view(db: &Db, provider: &str) -> ProviderUsageView {
    let Ok(rows) = db.list_provider_usage(provider) else {
        return ProviderUsageView::default();
    };
    provider_usage_view_from_rows(&rows)
}

pub fn provider_usage_view_from_rows(rows: &[ProviderUsageRow]) -> ProviderUsageView {
    let mut extra_credits = None;
    let mut windows: Vec<UsageWindowView> = Vec::new();
    for r in rows {
        if extra_credits.is_none() {
            extra_credits = extra_credits_usd(r);
        }
        let incoming = UsageWindowView {
            name: window_display_name(&r.window),
            percent_used: window_percent_used(r),
            resets_at: r.resets_at.as_deref().and_then(parse_rfc),
        };
        if let Some(existing) = windows.iter_mut().find(|w| w.name == incoming.name) {
            if incoming.percent_used.is_some() {
                existing.percent_used = incoming.percent_used;
                if incoming.resets_at.is_some() {
                    existing.resets_at = incoming.resets_at;
                }
            } else if existing.resets_at.is_none() {
                existing.resets_at = incoming.resets_at;
            }
            continue;
        }
        windows.push(incoming);
    }
    ProviderUsageView {
        windows,
        extra_credits,
    }
}

/// Utilization is a 0..1 fraction. Old Grok rows left it blank and stored
/// `creditUsagePercent` as a percent (1.0 = 1%).
fn window_percent_used(r: &ProviderUsageRow) -> Option<f64> {
    if let Some(u) = r.utilization {
        return Some(one_decimal(u * 100.0));
    }
    r.credit_usage_percent.map(one_decimal)
}

/// Old Grok rows stored `prepaidBalance` cents with utilization blank.
fn extra_credits_usd(r: &ProviderUsageRow) -> Option<f64> {
    let b = r.balance?;
    if r.utilization.is_none() && r.credit_usage_percent.is_some() && (b - b.round()).abs() < 1e-6 {
        return Some(b / 100.0);
    }
    Some(b)
}

/// Shared formatter for status, providers, dashboard, Telegram, and MCP.
/// Providers with no windows and no extra credits: `usage: not reported`.
/// Past reset times are rolled forward when the window length is known,
/// otherwise shown as `window reset`.
pub fn format_provider_usage(
    view: &ProviderUsageView,
    tz: &crate::tz::DisplayTz,
    now: DateTime<Utc>,
) -> String {
    if view.windows.is_empty() && view.extra_credits.is_none() {
        return "usage: not reported".into();
    }
    let mut parts = Vec::new();
    for w in &view.windows {
        let mut s = match w.percent_used {
            Some(p) => format!("{} {:.1}% used", w.name, one_decimal(p)),
            None => w.name.clone(),
        };
        s.push_str(&format_reset_suffix(&w.name, w.resets_at, now, tz));
        parts.push(s);
    }
    if let Some(usd) = view.extra_credits {
        parts.push(format!("extra credits ${usd:.2}"));
    }
    parts.join("; ")
}

/// One line per provider, same formatter for every name (now or later).
pub fn format_providers_usage(
    db: &Db,
    names: &[&str],
    tz: &crate::tz::DisplayTz,
    now: DateTime<Utc>,
) -> String {
    let mut out = String::new();
    for name in names {
        let text = format_provider_usage(&provider_usage_view(db, name), tz, now);
        out.push_str(&format!("{name} {text}\n"));
    }
    out
}

pub fn reported_json(
    db: &Db,
    provider: &str,
    tz: &crate::tz::DisplayTz,
    now: DateTime<Utc>,
) -> Value {
    let view = provider_usage_view(db, provider);
    reported_view_json(&view, tz, now)
}

pub fn reported_view_json(
    view: &ProviderUsageView,
    tz: &crate::tz::DisplayTz,
    now: DateTime<Utc>,
) -> Value {
    let windows: Vec<Value> = view
        .windows
        .iter()
        .map(|w| {
            serde_json::json!({
                "name": w.name,
                "percent_used": w.percent_used,
                "resets_at": json_reset_at(&w.name, w.resets_at, now, tz),
            })
        })
        .collect();
    serde_json::json!({
        "windows": windows,
        "extra_credits": view.extra_credits.map(|u| format!("${u:.2}")),
        "display": format_provider_usage(view, tz, now),
    })
}

fn parse_rfc(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|t| t.with_timezone(&Utc))
}

fn due_for_probe(db: &Db, p: &Provider, clock: &Clock) -> bool {
    let Some(last) = db.provider_usage_updated(&p.name).ok().flatten() else {
        return true;
    };
    let Some(ts) = parse_rfc(&last) else {
        return true;
    };
    clock.now() >= ts + chrono::Duration::from_std(p.reported_usage_interval).unwrap_or_default()
}

/// Probe Grok billing at most every `reported_usage_interval`. Failures leave
/// existing data and log once until a success.
pub async fn refresh_grok_billing(db: &Db, p: &Provider, clock: &Clock) -> Result<()> {
    refresh_grok_billing_opts(db, p, clock, false).await
}

/// Always probe (daemon start). Cheap, no task needed; keeps status fresh.
pub async fn refresh_grok_billing_at_start(db: &Db, providers: &[Provider], clock: &Clock) {
    for p in providers {
        if p.enabled && p.reported_usage == "grok_billing" {
            let _ = refresh_grok_billing_opts(db, p, clock, true).await;
        }
    }
}

async fn refresh_grok_billing_opts(
    db: &Db,
    p: &Provider,
    clock: &Clock,
    force: bool,
) -> Result<()> {
    if p.reported_usage != "grok_billing" {
        return Ok(());
    }
    if !force && !due_for_probe(db, p, clock) {
        return Ok(());
    }
    match probe_grok_billing(&p.cli, p.reported_usage_timeout).await {
        Ok(row) => {
            store_windows(db, &p.name, &[row], clock)?;
            let _ = db.meta_set(&format!("reported_fail:{}", p.name), "");
            Ok(())
        }
        Err(e) => {
            let key = format!("reported_fail:{}", p.name);
            if db.meta_get(&key).ok().flatten().as_deref() != Some("1") {
                tracing::warn!(provider = %p.name, "grok billing: {e:#}");
                let _ = db.meta_set(&key, "1");
            }
            Ok(())
        }
    }
}

pub async fn probe_grok_billing(cli: &Path, wait: Duration) -> Result<UsageWindow> {
    timeout(wait, probe_grok_billing_inner(cli))
        .await
        .context("grok billing timed out")?
}

async fn probe_grok_billing_inner(cli: &Path) -> Result<UsageWindow> {
    let mut child = Command::new(cli)
        .args(["agent", "stdio"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("spawn {} agent stdio", cli.display()))?;
    let mut stdin = child.stdin.take().context("grok stdin")?;
    let stdout = child.stdout.take().context("grok stdout")?;
    stdin
        .write_all(
            br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{}}}
{"jsonrpc":"2.0","id":2,"method":"_x.ai/billing","params":{}}
"#,
        )
        .await?;
    let _ = stdin.flush().await;
    // Keep stdin open: grok exits on EOF before the billing reply (several seconds).
    let mut lines = BufReader::new(stdout).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let Ok(v) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if v.get("id") != Some(&Value::from(2)) {
            continue;
        }
        let row = parse_grok_billing(&v);
        let _ = child.kill().await;
        drop(stdin);
        return row.ok_or_else(|| anyhow::anyhow!("no billing reply"));
    }
    let _ = child.kill().await;
    drop(stdin);
    anyhow::bail!("no billing reply")
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn t0() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 13, 12, 0, 0).unwrap()
    }

    const CLAUDE_SAMPLE: &str = r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed","resetsAt":1791968400,"rateLimitType":"five_hour","unifiedWindows":{"five_hour":{"utilization":0,"resetsAt":1791968400},"seven_day":{"utilization":0.03,"resetsAt":1792195200}}}}"#;
    const GROK_SAMPLE: &str = r#"{"config":{"currentPeriod":{"type":"USAGE_PERIOD_TYPE_WEEKLY","start":"2026-10-06T00:00:00Z","end":"2026-10-13T00:00:00Z"},"prepaidBalance":{"val":14032},"creditUsagePercent":1.0},"subscription_tier":"pro"}"#;

    #[test]
    fn parses_claude_stream_sample() {
        let v: Value = serde_json::from_str(CLAUDE_SAMPLE).unwrap();
        let rows = parse_claude_rate_limit(&v).expect("sample");
        assert_eq!(rows.len(), 2);
        let five = rows.iter().find(|r| r.window == "five_hour").unwrap();
        assert_eq!(five.utilization, Some(0.0));
        assert_eq!(five.status.as_deref(), Some("allowed"));
        assert_eq!(
            five.resets_at,
            Some(Utc.timestamp_opt(1791968400, 0).single().unwrap())
        );
        let week = rows.iter().find(|r| r.window == "seven_day").unwrap();
        assert!((week.utilization.unwrap() - 0.03).abs() < 1e-9);
        assert_eq!(
            week.resets_at,
            Some(Utc.timestamp_opt(1792195200, 0).single().unwrap())
        );
    }

    #[test]
    fn parses_grok_billing_sample() {
        let v: Value = serde_json::from_str(GROK_SAMPLE).unwrap();
        let row = parse_grok_billing(&v).expect("sample");
        assert!((row.balance.unwrap() - 140.32).abs() < 1e-9);
        assert!((row.utilization.unwrap() - 0.01).abs() < 1e-9);
        assert_eq!(
            row.resets_at.unwrap().to_rfc3339(),
            "2026-10-13T00:00:00+00:00"
        );
        assert_eq!(row.window, "weekly");
    }

    #[test]
    fn stores_claude_sample_and_feeds_reset() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("s.db")).unwrap();
        let t0 = Utc.with_ymd_and_hms(2026, 10, 13, 12, 0, 0).unwrap();
        let clock = Clock::frozen(t0);
        let mut p = crate::config::Provider::test_stub("claude");
        p.reported_usage = "claude_stream".into();
        assert!(store_claude_line(&db, &p, CLAUDE_SAMPLE, &clock).unwrap());
        let tz = crate::tz::DisplayTz::parse("UTC").unwrap();
        let json = reported_json(&db, "claude", &tz, t0);
        assert_eq!(json["windows"].as_array().unwrap().len(), 2);
        assert!(
            json["display"].as_str().unwrap().contains("5h 0.0% used"),
            "{json}"
        );
        assert!(
            json["display"].as_str().unwrap().contains("7d 3.0% used"),
            "{json}"
        );
        assert!(reported_resets(&db, "claude", t0).len() >= 2);
        let blob = format!("{CLAUDE_SAMPLE}\n{{\"type\":\"assistant\"}}\n");
        assert!(store_claude_transcript(&db, &p, &blob, &clock).unwrap());
        p.reported_usage = String::new();
        assert!(!store_claude_line(&db, &p, CLAUDE_SAMPLE, &clock).unwrap());
    }

    #[test]
    fn ignores_missing_claude_fields() {
        let v: Value = serde_json::from_str(r#"{"type":"assistant","message":{}}"#).unwrap();
        assert!(parse_claude_rate_limit(&v).is_none());
        let v: Value = serde_json::from_str(r#"{"type":"rate_limit_event"}"#).unwrap();
        assert!(parse_claude_rate_limit(&v).is_none());
        let v: Value = serde_json::from_str(
            r#"{"type":"result","rate_limit_info":{"status":"allowed","unifiedWindows":{"five_hour":{"utilization":0.2,"resetsAt":1791968400}}}}"#,
        )
        .unwrap();
        let rows = parse_claude_rate_limit(&v).expect("embedded on result");
        assert_eq!(rows[0].utilization, Some(0.2));
    }

    #[test]
    fn reported_over_uses_skip_at_and_status() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("s.db")).unwrap();
        let t0 = Utc.with_ymd_and_hms(2026, 10, 13, 12, 0, 0).unwrap();
        let clock = Clock::frozen(t0);
        let reset =
            (t0 + chrono::Duration::hours(2)).to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        db.upsert_provider_usage(
            "claude",
            "five_hour",
            Some(0.95),
            None,
            None,
            Some(&reset),
            Some("allowed"),
            &clock.rfc3339(),
        )
        .unwrap();
        let mut p = crate::config::Provider::test_stub("claude");
        p.reported_usage = "claude_stream".into();
        p.skip_at = 0.9;
        assert!(reported_over(&db, &p, &clock).unwrap());
        db.upsert_provider_usage(
            "claude",
            "five_hour",
            Some(0.1),
            None,
            None,
            Some(&reset),
            Some("rejected"),
            &clock.rfc3339(),
        )
        .unwrap();
        assert!(reported_over(&db, &p, &clock).unwrap());
        db.upsert_provider_usage(
            "claude",
            "five_hour",
            Some(0.1),
            None,
            None,
            Some(&reset),
            Some("allowed"),
            &clock.rfc3339(),
        )
        .unwrap();
        assert!(!reported_over(&db, &p, &clock).unwrap());
    }

    #[test]
    fn grok_skip_at_uses_plan_utilization_not_extra_credits() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("s.db")).unwrap();
        let t0 = Utc.with_ymd_and_hms(2026, 10, 13, 12, 0, 0).unwrap();
        let clock = Clock::frozen(t0);
        let end =
            (t0 + chrono::Duration::hours(5)).to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        db.upsert_provider_usage(
            "grok",
            "weekly",
            Some(0.01),
            Some(140.32),
            None,
            Some(&end),
            None,
            &clock.rfc3339(),
        )
        .unwrap();
        let mut p = crate::config::Provider::test_stub("grok");
        p.reported_usage = "grok_billing".into();
        p.skip_at = 0.9;
        assert!(!reported_over(&db, &p, &clock).unwrap());
        db.upsert_provider_usage(
            "grok",
            "weekly",
            Some(0.95),
            Some(140.32),
            None,
            Some(&end),
            None,
            &clock.rfc3339(),
        )
        .unwrap();
        assert!(reported_over(&db, &p, &clock).unwrap());
        let past =
            (t0 - chrono::Duration::hours(1)).to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        db.upsert_provider_usage(
            "grok",
            "weekly",
            Some(0.95),
            Some(140.32),
            None,
            Some(&past),
            None,
            &clock.rfc3339(),
        )
        .unwrap();
        assert!(!reported_over(&db, &p, &clock).unwrap());
    }

    #[tokio::test]
    async fn grok_billing_probe_reads_sample() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("grok");
        std::fs::write(
            &script,
            r#"#!/usr/bin/env python3
import json, select, sys, time
sample = {"config":{"currentPeriod":{"type":"USAGE_PERIOD_TYPE_WEEKLY","start":"2026-10-06T00:00:00Z","end":"2026-10-13T00:00:00Z"},"prepaidBalance":{"val":14032},"creditUsagePercent":1.0},"subscription_tier":"pro"}
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    try:
        o = json.loads(line)
    except Exception:
        continue
    if o.get("method") == "initialize":
        params = o.get("params") or {}
        if params.get("protocolVersion") != 1:
            print(json.dumps({"jsonrpc":"2.0","id":o.get("id"),"error":{"code":-32602,"message":"missing protocolVersion"}}), flush=True)
        else:
            print(json.dumps({"jsonrpc":"2.0","id":o.get("id"),"result":{}}), flush=True)
    if o.get("method") == "_x.ai/billing":
        time.sleep(0.35)
        r, _, _ = select.select([sys.stdin], [], [], 0)
        if r:
            extra = sys.stdin.readline()
            if extra == "":
                sys.exit(0)
        print(json.dumps({"jsonrpc":"2.0","id":o.get("id"),"result":sample}), flush=True)
        break
"#,
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let row = probe_grok_billing(&script, Duration::from_secs(5))
            .await
            .expect("probe");
        assert!((row.balance.unwrap() - 140.32).abs() < 1e-9);
        assert!((row.utilization.unwrap() - 0.01).abs() < 1e-9);
        assert_eq!(row.window, "weekly");
    }

    #[test]
    fn shared_formatter_is_identical_for_every_provider() {
        let tz = crate::tz::DisplayTz::parse("UTC").unwrap();
        let reset = Utc.with_ymd_and_hms(2026, 10, 16, 7, 36, 0).unwrap();
        let plan = ProviderUsageView {
            windows: vec![UsageWindowView {
                name: "weekly".into(),
                percent_used: Some(1.0),
                resets_at: Some(reset),
            }],
            extra_credits: Some(140.32),
        };
        let expected = "weekly 1.0% used, resets Oct 16 07:36 UTC; extra credits $140.32";
        for name in ["claude", "grok", "cursor", "codex", "future-agent"] {
            let _ = name;
            assert_eq!(format_provider_usage(&plan, &tz, t0()), expected);
        }
        let empty = ProviderUsageView::default();
        for name in ["claude", "grok", "cursor", "codex"] {
            let _ = name;
            assert_eq!(
                format_provider_usage(&empty, &tz, t0()),
                "usage: not reported"
            );
        }
        let claude = ProviderUsageView {
            windows: vec![
                UsageWindowView {
                    name: "5h".into(),
                    percent_used: Some(0.0),
                    resets_at: None,
                },
                UsageWindowView {
                    name: "7d".into(),
                    percent_used: Some(3.0),
                    resets_at: None,
                },
            ],
            extra_credits: None,
        };
        assert_eq!(
            format_provider_usage(&claude, &tz, t0()),
            "5h 0.0% used; 7d 3.0% used"
        );
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("s.db")).unwrap();
        let now = "2026-10-13T12:00:00.000+00:00";
        let reset_s = reset.to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        for name in ["claude", "grok", "cursor", "codex"] {
            db.upsert_provider_usage(
                name,
                "weekly",
                Some(0.01),
                Some(140.32),
                None,
                Some(&reset_s),
                None,
                now,
            )
            .unwrap();
        }
        let lines = format_providers_usage(&db, &["claude", "grok", "cursor", "codex"], &tz, t0());
        let usage = format_provider_usage(&provider_usage_view(&db, "grok"), &tz, t0());
        assert_eq!(usage, expected);
        for name in ["claude", "grok", "cursor", "codex"] {
            assert!(
                lines.contains(&format!("{name} {expected}")),
                "{name} missing identical line:\n{lines}"
            );
        }
    }

    #[test]
    fn old_grok_row_formats_percent_and_dollar_credits() {
        let tz = crate::tz::DisplayTz::parse("UTC").unwrap();
        let reset = Utc.with_ymd_and_hms(2026, 10, 16, 7, 36, 0).unwrap();
        let old = ProviderUsageRow {
            provider: "grok".into(),
            window: "weekly".into(),
            utilization: None,
            balance: Some(14032.0),
            credit_usage_percent: Some(1.0),
            resets_at: Some(reset.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)),
            status: None,
            updated_at: "2026-10-13T12:00:00.000+00:00".into(),
        };
        let view = provider_usage_view_from_rows(&[old]);
        assert_eq!(
            format_provider_usage(&view, &tz, t0()),
            "weekly 1.0% used, resets Oct 16 07:36 UTC; extra credits $140.32"
        );
        assert_eq!(view.windows[0].percent_used, Some(1.0));
        assert!((view.extra_credits.unwrap() - 140.32).abs() < 1e-9);
    }

    #[test]
    fn formatter_dedupes_weekly_window_aliases() {
        let tz = crate::tz::DisplayTz::parse("UTC").unwrap();
        let reset = Utc.with_ymd_and_hms(2026, 10, 16, 7, 36, 0).unwrap();
        let reset_s = reset.to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        let old = ProviderUsageRow {
            provider: "grok".into(),
            window: "usage_period_type_weekly".into(),
            utilization: Some(0.01),
            balance: Some(140.32),
            credit_usage_percent: Some(1.0),
            resets_at: Some(reset_s.clone()),
            status: None,
            updated_at: "2026-10-13T12:00:00.000+00:00".into(),
        };
        let fresh = ProviderUsageRow {
            provider: "grok".into(),
            window: "weekly".into(),
            utilization: Some(0.01),
            balance: Some(140.32),
            credit_usage_percent: Some(1.0),
            resets_at: Some(reset_s),
            status: None,
            updated_at: "2026-10-14T10:00:00.000+00:00".into(),
        };
        let view = provider_usage_view_from_rows(&[old, fresh]);
        assert_eq!(view.windows.len(), 1, "{:?}", view.windows);
        assert_eq!(view.windows[0].name, "weekly");
        assert_eq!(
            format_provider_usage(&view, &tz, t0()),
            "weekly 1.0% used, resets Oct 16 07:36 UTC; extra credits $140.32"
        );
    }

    #[test]
    fn expired_reset_rolls_known_window_or_says_window_reset() {
        let tz = crate::tz::DisplayTz::parse("Europe/Vilnius").unwrap();
        // 20:05 Vilnius (EEST, UTC+3) with a stored 5h reset at 17:10 Vilnius.
        let now = Utc.with_ymd_and_hms(2026, 10, 14, 17, 5, 0).unwrap();
        let past = Utc.with_ymd_and_hms(2026, 10, 14, 14, 10, 0).unwrap();
        let five = ProviderUsageView {
            windows: vec![UsageWindowView {
                name: "5h".into(),
                percent_used: Some(0.0),
                resets_at: Some(past),
            }],
            extra_credits: None,
        };
        let text = format_provider_usage(&five, &tz, now);
        assert_eq!(text, "5h 0.0% used, resets Oct 14 22:10 Europe/Vilnius");
        assert!(
            !text.contains("17:10"),
            "must not keep the expired reset time, got {text}"
        );
        let json = reported_view_json(&five, &tz, now);
        assert_eq!(
            json["windows"][0]["resets_at"].as_str(),
            Some("Oct 14 22:10 Europe/Vilnius")
        );
        assert_eq!(json["display"].as_str(), Some(text.as_str()));

        let unknown = ProviderUsageView {
            windows: vec![UsageWindowView {
                name: "session".into(),
                percent_used: Some(10.0),
                resets_at: Some(past),
            }],
            extra_credits: None,
        };
        assert_eq!(
            format_provider_usage(&unknown, &tz, now),
            "session 10.0% used, window reset"
        );

        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("s.db")).unwrap();
        db.upsert_provider_usage(
            "claude",
            "five_hour",
            Some(0.0),
            None,
            None,
            Some(&past.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)),
            Some("allowed"),
            &t0().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        )
        .unwrap();
        let rolled = reported_resets(&db, "claude", now);
        assert_eq!(rolled, vec![past + ChronoDuration::hours(5)]);
    }
}
