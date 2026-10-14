use crate::clock::Clock;
use crate::config::{Provider, QuotaUnit, QuotaWindow};
use crate::db::Db;
use crate::runner::RunResult;
use anyhow::Result;
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use serde_json::Value;

#[derive(Debug, Clone)]
pub struct WindowUsage {
    pub window_raw: String,
    pub unit: QuotaUnit,
    pub used: f64,
    pub budget: f64,
    pub remaining: f64,
    pub ratio: f64,
    pub threshold: f64,
    pub over: bool,
    pub reset: Option<DateTime<Utc>>,
}

impl WindowUsage {
    pub fn to_json(&self) -> serde_json::Value {
        self.to_json_tz(None)
    }

    pub fn to_json_tz(&self, tz: Option<&crate::tz::DisplayTz>) -> serde_json::Value {
        let reset = self.reset.map(|t| match tz {
            Some(z) => z.format(t),
            None => t.to_rfc3339(),
        });
        serde_json::json!({
            "window": self.window_raw,
            "unit": self.unit.as_str(),
            "used": self.used,
            "budget": self.budget,
            "remaining": self.remaining,
            "ratio": self.ratio,
            "threshold": self.threshold,
            "over": self.over,
            "reset": reset,
        })
    }
}

pub fn window_usage(
    db: &Db,
    provider: &str,
    w: &QuotaWindow,
    clock: &Clock,
) -> Result<WindowUsage> {
    let now = clock.now();
    let since = now - ChronoDuration::from_std(w.window).unwrap_or(ChronoDuration::hours(5));
    let since_s = since.to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let (input, output, cost, runs) = db.usage_since(provider, &since_s)?;
    let used = match w.unit {
        QuotaUnit::Usd => cost,
        QuotaUnit::Tokens => (input + output) as f64,
        QuotaUnit::Runs => runs as f64,
    };
    let remaining = (w.budget - used).max(0.0);
    let ratio = if w.budget > 0.0 { used / w.budget } else { 0.0 };
    let over = w.budget > 0.0 && ratio >= w.threshold;
    let reset = rolling_reset(db, provider, w, &since_s, now)?;
    Ok(WindowUsage {
        window_raw: w.window_raw.clone(),
        unit: w.unit,
        used,
        budget: w.budget,
        remaining,
        ratio,
        threshold: w.threshold,
        over,
        reset,
    })
}

fn rolling_reset(
    db: &Db,
    provider: &str,
    w: &QuotaWindow,
    since: &str,
    now: DateTime<Utc>,
) -> Result<Option<DateTime<Utc>>> {
    let Some(oldest) = db.usage_oldest(provider, since)? else {
        return Ok(None);
    };
    let Ok(ts) = DateTime::parse_from_rfc3339(&oldest) else {
        return Ok(None);
    };
    let end = ts.with_timezone(&Utc)
        + ChronoDuration::from_std(w.window).unwrap_or(ChronoDuration::hours(5));
    if end > now {
        Ok(Some(end))
    } else {
        Ok(None)
    }
}

/// True when any configured window is at or above its skip threshold,
/// or a reported-usage window is over `skip_at` / not allowed.
pub fn over_quota(db: &Db, p: &Provider, clock: &Clock) -> Result<bool> {
    for w in &p.quota {
        if window_usage(db, &p.name, w, clock)?.over {
            return Ok(true);
        }
    }
    crate::reported::reported_over(db, p, clock)
}

pub fn snapshots(db: &Db, p: &Provider, clock: &Clock) -> Result<Vec<WindowUsage>> {
    let mut out = Vec::with_capacity(p.quota.len());
    for w in &p.quota {
        out.push(window_usage(db, &p.name, w, clock)?);
    }
    Ok(out)
}

/// Earliest reset among active (over-threshold) windows, else any window, else CLI.
pub fn earliest_reset(
    windows: &[WindowUsage],
    cli_reset: Option<&str>,
    now: DateTime<Utc>,
) -> Option<DateTime<Utc>> {
    let parse = |s: &str| {
        DateTime::parse_from_rfc3339(s)
            .ok()
            .map(|t| t.with_timezone(&Utc))
    };
    let mut times: Vec<DateTime<Utc>> = windows
        .iter()
        .filter(|w| w.over)
        .filter_map(|w| w.reset)
        .collect();
    if times.is_empty() {
        times.extend(windows.iter().filter_map(|w| w.reset));
    }
    if let Some(c) = cli_reset.and_then(parse) {
        if c > now {
            times.push(c);
        }
    }
    times.into_iter().min()
}

pub fn limit_reset_active(db: &Db, name: &str, clock: &Clock) -> Result<bool> {
    Ok(crate::db::ts_in_future(
        db.quota_reset_at(name)?.as_deref(),
        clock.now(),
    ))
}

/// Parse a CLI-reported reset time from a structured event or error body.
pub fn extract_reset_at(value: &Value, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    if let Some(t) = from_known_fields(value, now) {
        return Some(t);
    }
    walk_reset(value, now, 0)
}

fn from_known_fields(value: &Value, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    const KEYS: &[&str] = &[
        "resetsAt",
        "resets_at",
        "resetAt",
        "reset_at",
        "reset",
        "retryAfter",
        "retry_after",
        "retryAfterMs",
        "retry_after_ms",
    ];
    for key in KEYS {
        if let Some(v) = find_key(value, key, 0) {
            if let Some(t) = parse_reset_value(v, now, key.contains("ms") || key.ends_with("Ms")) {
                return Some(t);
            }
        }
    }
    if let Some(msg) = value
        .pointer("/error/message")
        .or_else(|| value.get("result"))
        .or_else(|| value.get("message"))
        .and_then(|v| v.as_str())
    {
        if let Some(t) = reset_from_message(msg, now) {
            return Some(t);
        }
    }
    None
}

fn walk_reset(value: &Value, now: DateTime<Utc>, depth: u8) -> Option<DateTime<Utc>> {
    if depth > 6 {
        return None;
    }
    match value {
        Value::Object(map) => {
            for (k, v) in map {
                let kl = k.to_ascii_lowercase();
                if kl.contains("reset") || kl.contains("retry_after") || kl.contains("retryafter") {
                    if let Some(t) = parse_reset_value(v, now, kl.contains("ms")) {
                        return Some(t);
                    }
                }
                if let Some(t) = walk_reset(v, now, depth + 1) {
                    return Some(t);
                }
            }
            None
        }
        Value::Array(items) => items.iter().find_map(|v| walk_reset(v, now, depth + 1)),
        _ => None,
    }
}

fn find_key<'a>(value: &'a Value, key: &str, depth: u8) -> Option<&'a Value> {
    if depth > 6 {
        return None;
    }
    match value {
        Value::Object(map) => {
            if let Some(v) = map.get(key) {
                return Some(v);
            }
            for v in map.values() {
                if let Some(hit) = find_key(v, key, depth + 1) {
                    return Some(hit);
                }
            }
            None
        }
        Value::Array(items) => items.iter().find_map(|v| find_key(v, key, depth + 1)),
        _ => None,
    }
}

fn parse_reset_value(v: &Value, now: DateTime<Utc>, millis: bool) -> Option<DateTime<Utc>> {
    if let Some(s) = v.as_str() {
        return parse_reset_str(s, now);
    }
    if let Some(n) = v.as_i64().or_else(|| v.as_u64().map(|u| u as i64)) {
        return parse_reset_number(n, now, millis);
    }
    if let Some(f) = v.as_f64() {
        return parse_reset_number(f as i64, now, millis);
    }
    None
}

fn parse_reset_str(s: &str, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    let t = s.trim();
    if t.is_empty() {
        return None;
    }
    if let Ok(d) = DateTime::parse_from_rfc3339(t) {
        return Some(d.with_timezone(&Utc));
    }
    if let Ok(n) = t.parse::<i64>() {
        return parse_reset_number(n, now, false);
    }
    if let Ok(d) = crate::duration::parse_duration(t) {
        return Some(now + ChronoDuration::from_std(d).ok()?);
    }
    reset_from_message(t, now)
}

fn parse_reset_number(n: i64, now: DateTime<Utc>, millis: bool) -> Option<DateTime<Utc>> {
    if millis || n > 1_000_000_000_000 {
        return DateTime::from_timestamp(n / 1000, 0);
    }
    if n > 1_000_000_000 {
        return DateTime::from_timestamp(n, 0);
    }
    if n >= 0 {
        return Some(now + ChronoDuration::seconds(n));
    }
    None
}

pub fn reset_from_message(msg: &str, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    let lower = msg.to_ascii_lowercase();
    for needle in ["resets at ", "reset at ", "resets ", "try again at "] {
        if let Some(idx) = lower.find(needle) {
            let rest = msg[idx + needle.len()..].trim();
            let token = rest.split_whitespace().next().unwrap_or(rest);
            if let Ok(d) = DateTime::parse_from_rfc3339(token.trim_end_matches(['.', ',', ';'])) {
                return Some(d.with_timezone(&Utc));
            }
        }
    }
    for needle in [
        "try again in ",
        "retry after ",
        "resets in ",
        "available in ",
    ] {
        if let Some(idx) = lower.find(needle) {
            let rest = msg[idx + needle.len()..].trim();
            if let Some(d) = parse_human_duration(rest) {
                return Some(now + d);
            }
        }
    }
    None
}

fn parse_human_duration(s: &str) -> Option<ChronoDuration> {
    let t = s.trim();
    if let Ok(d) = crate::duration::parse_duration(t.split_whitespace().next().unwrap_or(t)) {
        return ChronoDuration::from_std(d).ok();
    }
    let mut parts = t.split_whitespace();
    let n: i64 = parts
        .next()?
        .trim_end_matches(['.', ',', ';'])
        .parse()
        .ok()?;
    let unit = parts.next().unwrap_or("s").to_ascii_lowercase();
    if unit.starts_with("hour") || unit == "h" {
        Some(ChronoDuration::hours(n))
    } else if unit.starts_with("min") || unit == "m" {
        Some(ChronoDuration::minutes(n))
    } else if unit.starts_with("sec") || unit == "s" {
        Some(ChronoDuration::seconds(n))
    } else if unit.starts_with("day") || unit == "d" {
        Some(ChronoDuration::days(n))
    } else {
        None
    }
}

pub fn reset_from_run(result: &RunResult, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    for raw in &result.usage_raw {
        if let Some(t) = extract_reset_at(raw, now) {
            return Some(t);
        }
    }
    if !result.snippet.is_empty() {
        if let Ok(v) = serde_json::from_str::<Value>(&result.snippet) {
            if let Some(t) = extract_reset_at(&v, now) {
                return Some(t);
            }
        }
        if let Some(t) = reset_from_message(&result.snippet, now) {
            return Some(t);
        }
    }
    if !result.stderr.is_empty() {
        if let Some(t) = reset_from_message(&result.stderr, now) {
            return Some(t);
        }
    }
    None
}

pub fn record_limit_for_run(
    db: &Db,
    provider: &str,
    result: &RunResult,
    clock: &Clock,
) -> Result<()> {
    let now = clock.now();
    let reset = reset_from_run(result, now).or_else(|| {
        crate::reported::reported_resets(db, provider, now)
            .into_iter()
            .min()
    });
    let reset_s = reset.map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Millis, true));
    let raw = result
        .usage_raw
        .first()
        .map(|v| v.to_string())
        .unwrap_or_else(|| result.snippet.clone());
    db.record_limit(
        provider,
        reset_s.as_deref(),
        &result.snippet,
        &raw,
        &clock.rfc3339(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::QuotaWindow;
    use crate::db::Db;
    use chrono::TimeZone;
    use std::time::Duration;

    fn t0() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 13, 12, 0, 0).unwrap()
    }

    #[test]
    fn extract_resets_at_field() {
        let v: Value = serde_json::from_str(
            r#"{"type":"error","error":{"type":"rate_limit_error","resetsAt":"2026-10-13T17:00:00Z"}}"#,
        )
        .unwrap();
        let got = extract_reset_at(&v, t0()).unwrap();
        assert_eq!(
            got,
            DateTime::parse_from_rfc3339("2026-10-13T17:00:00Z")
                .unwrap()
                .with_timezone(&Utc)
        );
    }

    #[test]
    fn extract_retry_after_seconds() {
        let v: Value = serde_json::from_str(r#"{"error":{"retry_after":3600}}"#).unwrap();
        let got = extract_reset_at(&v, t0()).unwrap();
        assert_eq!(got, t0() + ChronoDuration::hours(1));
    }

    #[test]
    fn extract_try_again_in_message() {
        let got = reset_from_message("rate limit: try again in 5 hours", t0()).unwrap();
        assert_eq!(got, t0() + ChronoDuration::hours(5));
    }

    #[test]
    fn window_rolls_off_with_fake_clock() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("s.db")).unwrap();
        db.upsert_task("acme-q", "/repo", None).unwrap();
        let run = db.start_run("acme-q", "claude", "m").unwrap();
        db.finish_run(run, "success", Some(0)).unwrap();
        let at = t0() - ChronoDuration::hours(3);
        db.insert_usage_at(
            run,
            None,
            "claude",
            0,
            0,
            0,
            0,
            Some(45.0),
            Some("est"),
            "{}",
            &at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        )
        .unwrap();
        let w = QuotaWindow {
            window: Duration::from_secs(5 * 3600),
            window_raw: "5h".into(),
            budget: 50.0,
            unit: QuotaUnit::Usd,
            threshold: 0.85,
        };
        let clock = Clock::frozen(t0());
        let snap = window_usage(&db, "claude", &w, &clock).unwrap();
        assert!((snap.used - 45.0).abs() < 1e-9, "{}", snap.used);
        assert!(snap.over, "45/50 >= 0.85");
        let later = Clock::frozen(t0() + ChronoDuration::hours(3));
        let snap = window_usage(&db, "claude", &w, &later).unwrap();
        assert!(snap.used < 1.0, "usage aged out, got {}", snap.used);
        assert!(!snap.over);
    }

    #[test]
    fn earliest_reset_picks_the_soonest_active_window() {
        let soon = t0() + ChronoDuration::hours(1);
        let later = t0() + ChronoDuration::hours(5);
        let windows = vec![
            WindowUsage {
                window_raw: "7d".into(),
                unit: QuotaUnit::Usd,
                used: 90.0,
                budget: 100.0,
                remaining: 10.0,
                ratio: 0.9,
                threshold: 0.85,
                over: true,
                reset: Some(later),
            },
            WindowUsage {
                window_raw: "5h".into(),
                unit: QuotaUnit::Usd,
                used: 45.0,
                budget: 50.0,
                remaining: 5.0,
                ratio: 0.9,
                threshold: 0.85,
                over: true,
                reset: Some(soon),
            },
        ];
        assert_eq!(earliest_reset(&windows, None, t0()), Some(soon));
    }

    #[test]
    fn record_limit_uses_reported_period_end() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("s.db")).unwrap();
        let end = t0() + ChronoDuration::hours(6);
        db.upsert_provider_usage(
            "grok",
            "weekly",
            Some(0.01),
            Some(140.32),
            None,
            Some(&end.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)),
            None,
            &t0().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        )
        .unwrap();
        let result = crate::runner::RunResult {
            outcome: "limit".into(),
            exit_code: Some(1),
            session_id: None,
            usage: crate::providers::UsageNumbers::default(),
            usage_raw: Vec::new(),
            snippet: "UsageLimitReached".into(),
            classified: Some(crate::providers::ErrorClass::Limit),
            stderr: String::new(),
        };
        record_limit_for_run(&db, "grok", &result, &Clock::frozen(t0())).unwrap();
        let stored = db.quota_reset_at("grok").unwrap().unwrap();
        let got = DateTime::parse_from_rfc3339(&stored)
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(got, end);
    }

    #[test]
    fn window_json_reset_uses_display_timezone() {
        let tz = crate::tz::DisplayTz::parse("UTC").unwrap();
        let w = WindowUsage {
            window_raw: "5h".into(),
            unit: QuotaUnit::Usd,
            used: 10.0,
            budget: 50.0,
            remaining: 40.0,
            ratio: 0.2,
            threshold: 0.85,
            over: false,
            reset: Some(t0()),
        };
        let v = w.to_json_tz(Some(&tz));
        let reset = v["reset"].as_str().unwrap();
        assert!(reset.contains("UTC"), "{reset}");
        assert!(reset.contains("2026-10-13"), "{reset}");
        assert!(reset.contains("12:00"), "{reset}");
        assert!(!reset.contains("T12:00"), "{reset}");
    }
}
