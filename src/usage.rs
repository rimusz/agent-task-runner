//! Token usage periods and cache-savings math.
//!
//! Stored `input` is uncached for every kind. Codex/OpenAI-style CLIs report
//! an inclusive input (cached already inside it); extract subtracts cached so
//! the column is comparable. Hit% and $ saved still use per-kind weights:
//! Claude-style includes cache_write in the hit denominator; Codex does not
//! (write was already inside the original inclusive input).

use crate::app::Shared;
use crate::config::Config;
use crate::db::UsageDay;
use crate::providers::hit_ratio_from_sums;
use crate::tz::DisplayTz;
use anyhow::{bail, Result};
use chrono::{DateTime, Datelike, Duration, NaiveDate, Utc};
use serde_json::{json, Value};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Period {
    Today,
    Days(u32),
    Month,
    Custom { from: NaiveDate, to: NaiveDate },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupBy {
    Provider,
    Day,
}

#[derive(Debug, Clone)]
pub struct SavingsRow {
    pub provider: String,
    pub kind: String,
    pub day: Option<String>,
    pub input: i64,
    pub output: i64,
    pub cache_read: i64,
    pub cache_write: i64,
    pub cost_usd: f64,
    pub hit: Option<f64>,
    pub tokens_saved: i64,
    pub saved_usd: Option<f64>,
    pub runs: i64,
}

#[derive(Debug, Clone)]
pub struct DayPoint {
    pub day: String,
    pub input: i64,
    pub output: i64,
    pub cache_read: i64,
    pub cache_write: i64,
    pub tokens_saved: i64,
}

#[derive(Debug, Clone)]
pub struct SavingsReport {
    pub period: String,
    pub from: String,
    pub to: String,
    pub timezone: String,
    pub by_provider: Vec<SavingsRow>,
    pub by_day_provider: Vec<SavingsRow>,
    pub by_day: Vec<DayPoint>,
    pub total: SavingsRow,
    /// Runs at or above `[cache] headline_min_input` prompt tokens.
    pub headline: SavingsRow,
    pub first_run: SavingsRow,
    pub resume: SavingsRow,
    /// Runs below the headline threshold (shown separately, not in headline hit%).
    pub short: SavingsRow,
    pub headline_min_input: i64,
}

impl Period {
    pub fn label(&self) -> String {
        match self {
            Period::Today => "today".into(),
            Period::Days(n) => format!("{n}d"),
            Period::Month => "month".into(),
            Period::Custom { .. } => "custom".into(),
        }
    }

    pub fn range(&self, tz: &DisplayTz, now: DateTime<Utc>) -> Result<(NaiveDate, NaiveDate)> {
        let today = parse_ymd(&tz.day(now))?;
        match self {
            Period::Today => Ok((today, today)),
            Period::Days(n) => {
                if *n == 0 {
                    bail!("period days must be > 0");
                }
                let start = today - Duration::days(i64::from(n.saturating_sub(1)));
                Ok((start, today))
            }
            Period::Month => {
                let start = NaiveDate::from_ymd_opt(today.year(), today.month(), 1)
                    .ok_or_else(|| anyhow::anyhow!("invalid month"))?;
                Ok((start, today))
            }
            Period::Custom { from, to } => {
                if to < from {
                    bail!("--to {to} is before --from {from}");
                }
                Ok((*from, *to))
            }
        }
    }
}

pub fn parse_ymd(raw: &str) -> Result<NaiveDate> {
    NaiveDate::parse_from_str(raw.trim(), "%Y-%m-%d")
        .map_err(|_| anyhow::anyhow!("date {raw:?} is not YYYY-MM-DD"))
}

pub fn parse_period(spec: &str) -> Result<Period> {
    let s = spec.trim().to_ascii_lowercase();
    if let Some((a, b)) = s.split_once("..") {
        return Ok(Period::Custom {
            from: parse_ymd(a)?,
            to: parse_ymd(b)?,
        });
    }
    match s.as_str() {
        "today" | "day" => Ok(Period::Today),
        "7d" | "week" => Ok(Period::Days(7)),
        "30d" => Ok(Period::Days(30)),
        "month" | "this-month" | "this_month" => Ok(Period::Month),
        other if other.ends_with('d') => {
            let n: u32 = other
                .trim_end_matches('d')
                .parse()
                .map_err(|_| anyhow::anyhow!("unknown period {spec:?}"))?;
            if n == 0 {
                bail!("period days must be > 0");
            }
            if n == 1 {
                return Ok(Period::Today);
            }
            Ok(Period::Days(n))
        }
        _ => bail!("unknown period {spec:?} (today, 7d, 30d, month, or YYYY-MM-DD..YYYY-MM-DD)"),
    }
}

pub fn parse_group(raw: &str) -> Result<GroupBy> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "provider" | "providers" => Ok(GroupBy::Provider),
        "day" | "days" => Ok(GroupBy::Day),
        other => bail!("--by {other:?} (provider or day)"),
    }
}

/// `--period` wins; `--from`/`--to` is a custom range; else `--days` (default 7).
pub fn resolve(
    period: Option<&str>,
    days: u32,
    from: Option<&str>,
    to: Option<&str>,
) -> Result<Period> {
    if from.is_some() || to.is_some() {
        let f = from.ok_or_else(|| anyhow::anyhow!("--from is required with --to"))?;
        let t = to.ok_or_else(|| anyhow::anyhow!("--to is required with --from"))?;
        return Ok(Period::Custom {
            from: parse_ymd(f)?,
            to: parse_ymd(t)?,
        });
    }
    if let Some(p) = period {
        if p.eq_ignore_ascii_case("custom") {
            bail!("custom period needs --from and --to");
        }
        return parse_period(p);
    }
    if days == 1 {
        return Ok(Period::Today);
    }
    if days == 0 {
        bail!("--days must be > 0");
    }
    Ok(Period::Days(days))
}

pub fn period_from_query(q: Option<&str>) -> Period {
    let mut period = None;
    let mut from = None;
    let mut to = None;
    if let Some(q) = q {
        for part in q.split('&') {
            let Some((k, v)) = part.split_once('=') else {
                continue;
            };
            let v = v.replace('+', " ");
            match k {
                "period" => period = Some(v),
                "from" => from = Some(v),
                "to" => to = Some(v),
                _ => {}
            }
        }
    }
    resolve(period.as_deref(), 1, from.as_deref(), to.as_deref()).unwrap_or(Period::Today)
}

pub fn openai_inclusive_input(kind: &str) -> bool {
    kind == "codex"
}

pub fn tokens_saved(cache_read: i64) -> i64 {
    cache_read.max(0)
}

/// Billed vs full input-token weights. Claude-style: cache read is 0.1× and
/// cache write is 1.25×. Codex: cached input is 0.5×; do not add write
/// (already inside the original inclusive input).
pub fn token_weights(kind: &str, input: f64, cache_read: f64, cache_write: f64) -> (f64, f64) {
    if openai_inclusive_input(kind) {
        (input + 0.5 * cache_read, input + cache_read)
    } else {
        (
            input + 0.1 * cache_read + 1.25 * cache_write,
            input + cache_read + cache_write,
        )
    }
}

/// Est. $ saved vs paying full input price for cached tokens. `None` when
/// there is no positive estimated cost (subscription `plan` rows).
pub fn estimated_usd_saved(
    kind: &str,
    input: i64,
    cache_read: i64,
    cache_write: i64,
    cost_usd: f64,
) -> Option<f64> {
    if cost_usd <= 0.0 {
        return None;
    }
    let (billed, full) = token_weights(
        kind,
        input.max(0) as f64,
        cache_read.max(0) as f64,
        cache_write.max(0) as f64,
    );
    if billed <= 0.0 {
        return None;
    }
    Some((cost_usd * (full / billed - 1.0)).max(0.0))
}

pub fn kind_of(cfg: &Config, provider: &str) -> String {
    cfg.provider(provider)
        .map(|p| p.kind.clone())
        .unwrap_or_else(|| provider.to_string())
}

#[derive(Default)]
struct Acc {
    input: i64,
    output: i64,
    cache_read: i64,
    cache_write: i64,
    cost_usd: f64,
    runs: i64,
}

impl Acc {
    fn add(&mut self, input: i64, output: i64, cr: i64, cw: i64, cost: f64, runs: i64) {
        self.input += input;
        self.output += output;
        self.cache_read += cr;
        self.cache_write += cw;
        self.cost_usd += cost;
        self.runs += runs;
    }
}

fn row_from_sums(
    provider: &str,
    kind: &str,
    day: Option<String>,
    input: i64,
    output: i64,
    cache_read: i64,
    cache_write: i64,
    cost_usd: f64,
    runs: i64,
) -> SavingsRow {
    SavingsRow {
        provider: provider.to_string(),
        kind: kind.to_string(),
        day,
        input,
        output,
        cache_read,
        cache_write,
        cost_usd,
        hit: hit_ratio_from_sums(kind, input, cache_read, cache_write),
        tokens_saved: tokens_saved(cache_read),
        saved_usd: estimated_usd_saved(kind, input, cache_read, cache_write, cost_usd),
        runs,
    }
}

/// Full prompt size for the headline threshold.
pub fn prompt_tokens(kind: &str, input: i64, cache_read: i64, cache_write: i64) -> i64 {
    if openai_inclusive_input(kind) {
        input.saturating_add(cache_read)
    } else {
        input.saturating_add(cache_read).saturating_add(cache_write)
    }
}

fn hit_denom(kind: &str, input: i64, cache_read: i64, cache_write: i64) -> i64 {
    let mut d = input.saturating_add(cache_read);
    if !openai_inclusive_input(kind) {
        d = d.saturating_add(cache_write);
    }
    d
}

fn total_row(rows: &[SavingsRow], from: &str, to: &str) -> SavingsRow {
    let mut input = 0i64;
    let mut output = 0i64;
    let mut cache_read = 0i64;
    let mut cache_write = 0i64;
    let mut cost_usd = 0.0;
    let mut runs = 0i64;
    let mut den = 0i64;
    let mut saved_sum = 0.0;
    let mut saved_any = false;
    for r in rows {
        input += r.input;
        output += r.output;
        cache_read += r.cache_read;
        cache_write += r.cache_write;
        cost_usd += r.cost_usd;
        runs += r.runs;
        den += hit_denom(&r.kind, r.input, r.cache_read, r.cache_write);
        if let Some(s) = r.saved_usd {
            saved_sum += s;
            saved_any = true;
        }
    }
    let hit = if den == 0 {
        None
    } else {
        Some(cache_read as f64 / den as f64)
    };
    let day = if from == to {
        Some(from.to_string())
    } else {
        Some(format!("{from}..{to}"))
    };
    SavingsRow {
        provider: "total".into(),
        kind: String::new(),
        day,
        input,
        output,
        cache_read,
        cache_write,
        cost_usd,
        hit,
        tokens_saved: tokens_saved(cache_read),
        saved_usd: if saved_any { Some(saved_sum) } else { None },
        runs,
    }
}

pub fn report_from_days(
    cfg: &Config,
    period: &Period,
    from: NaiveDate,
    to: NaiveDate,
    days: &[UsageDay],
) -> SavingsReport {
    let tz = &cfg.display_timezone;
    let mut by_prov: std::collections::BTreeMap<String, Acc> = std::collections::BTreeMap::new();
    let mut by_day_prov: std::collections::BTreeMap<(String, String), Acc> =
        std::collections::BTreeMap::new();
    let mut by_day_acc: std::collections::BTreeMap<String, Acc> = std::collections::BTreeMap::new();

    let mut d = from;
    while d <= to {
        by_day_acc.entry(d.to_string()).or_default();
        d = match d.succ_opt() {
            Some(n) => n,
            None => break,
        };
    }

    for row in days {
        by_prov.entry(row.provider.clone()).or_default().add(
            row.input,
            row.output,
            row.cache_read,
            row.cache_write,
            row.cost_usd,
            row.runs,
        );
        by_day_prov
            .entry((row.day.clone(), row.provider.clone()))
            .or_default()
            .add(
                row.input,
                row.output,
                row.cache_read,
                row.cache_write,
                row.cost_usd,
                row.runs,
            );
        by_day_acc.entry(row.day.clone()).or_default().add(
            row.input,
            row.output,
            row.cache_read,
            row.cache_write,
            0.0,
            0,
        );
    }

    let by_provider: Vec<SavingsRow> = by_prov
        .into_iter()
        .map(|(name, a)| {
            let kind = kind_of(cfg, &name);
            row_from_sums(
                &name,
                &kind,
                None,
                a.input,
                a.output,
                a.cache_read,
                a.cache_write,
                a.cost_usd,
                a.runs,
            )
        })
        .collect();
    let by_day_provider: Vec<SavingsRow> = by_day_prov
        .into_iter()
        .map(|((day, name), a)| {
            let kind = kind_of(cfg, &name);
            row_from_sums(
                &name,
                &kind,
                Some(day),
                a.input,
                a.output,
                a.cache_read,
                a.cache_write,
                a.cost_usd,
                a.runs,
            )
        })
        .collect();
    let by_day: Vec<DayPoint> = by_day_acc
        .into_iter()
        .map(|(day, a)| DayPoint {
            day,
            input: a.input,
            output: a.output,
            cache_read: a.cache_read,
            cache_write: a.cache_write,
            tokens_saved: tokens_saved(a.cache_read),
        })
        .collect();
    let from_s = from.to_string();
    let to_s = to.to_string();
    let total = total_row(&by_provider, &from_s, &to_s);
    let empty = row_from_sums("none", "", None, 0, 0, 0, 0, 0.0, 0);
    SavingsReport {
        period: period.label(),
        from: from_s,
        to: to_s,
        timezone: tz.label.clone(),
        by_provider,
        by_day_provider,
        by_day,
        total,
        headline: empty.clone(),
        first_run: empty.clone(),
        resume: empty.clone(),
        short: empty,
        headline_min_input: cfg.cache.headline_min_input,
    }
}

pub fn report(shared: &Shared, period: &Period) -> Result<SavingsReport> {
    report_filtered(shared, period, None)
}

pub fn report_filtered(
    shared: &Shared,
    period: &Period,
    repo: Option<&str>,
) -> Result<SavingsReport> {
    let cfg = shared.config.read().unwrap().clone();
    let tz = &cfg.display_timezone;
    let (from, to) = period.range(tz, Utc::now())?;
    let paths = repo.map(|f| {
        let mut out = Vec::new();
        if let Some(r) = cfg.resolve_repo_filter(f) {
            out.push(r.path.display().to_string());
            if let Some(n) = r.path.file_name().and_then(|n| n.to_str()) {
                out.push(n.to_string());
            }
            out.push(r.name.clone());
        } else {
            out.push(f.to_string());
        }
        out
    });
    let days = shared.db.usage_by_day_range_filtered(
        &from.to_string(),
        &to.to_string(),
        tz,
        paths.as_deref(),
    )?;
    let mut report = report_from_days(&cfg, period, from, to, &days);
    let details =
        shared
            .db
            .usage_detail_range(&from.to_string(), &to.to_string(), tz, paths.as_deref())?;
    apply_cache_splits(&cfg, &mut report, &details);
    Ok(report)
}

fn apply_cache_splits(
    cfg: &Config,
    report: &mut SavingsReport,
    details: &[crate::db::UsageDetail],
) {
    let min = cfg.cache.headline_min_input;
    let mut headline = Acc::default();
    let mut first = Acc::default();
    let mut resume = Acc::default();
    let mut short = Acc::default();
    let mut h_den = 0i64;
    let mut f_den = 0i64;
    let mut r_den = 0i64;
    let mut s_den = 0i64;
    let mut h_cr = 0i64;
    let mut f_cr = 0i64;
    let mut r_cr = 0i64;
    let mut s_cr = 0i64;
    for d in details {
        let kind = kind_of(cfg, &d.provider);
        let tokens = prompt_tokens(&kind, d.input, d.cache_read, d.cache_write);
        let den = hit_denom(&kind, d.input, d.cache_read, d.cache_write);
        if tokens >= min {
            headline.add(
                d.input,
                d.output,
                d.cache_read,
                d.cache_write,
                d.cost_usd,
                1,
            );
            h_den += den;
            h_cr += d.cache_read;
            if d.is_resume {
                resume.add(
                    d.input,
                    d.output,
                    d.cache_read,
                    d.cache_write,
                    d.cost_usd,
                    1,
                );
                r_den += den;
                r_cr += d.cache_read;
            } else {
                first.add(
                    d.input,
                    d.output,
                    d.cache_read,
                    d.cache_write,
                    d.cost_usd,
                    1,
                );
                f_den += den;
                f_cr += d.cache_read;
            }
        } else {
            short.add(
                d.input,
                d.output,
                d.cache_read,
                d.cache_write,
                d.cost_usd,
                1,
            );
            s_den += den;
            s_cr += d.cache_read;
        }
    }
    let hit = |cr: i64, den: i64| {
        if den == 0 {
            None
        } else {
            Some(cr as f64 / den as f64)
        }
    };
    report.headline_min_input = min;
    report.headline = row_from_acc("headline", &headline, hit(h_cr, h_den));
    report.first_run = row_from_acc("first-run", &first, hit(f_cr, f_den));
    report.resume = row_from_acc("resume", &resume, hit(r_cr, r_den));
    report.short = row_from_acc("short", &short, hit(s_cr, s_den));
}

fn row_from_acc(name: &str, a: &Acc, hit: Option<f64>) -> SavingsRow {
    SavingsRow {
        provider: name.into(),
        kind: String::new(),
        day: None,
        input: a.input,
        output: a.output,
        cache_read: a.cache_read,
        cache_write: a.cache_write,
        cost_usd: a.cost_usd,
        hit,
        tokens_saved: tokens_saved(a.cache_read),
        saved_usd: None,
        runs: a.runs,
    }
}

pub fn repo_from_query(q: Option<&str>) -> Option<String> {
    let q = q?;
    for part in q.split('&') {
        let Some((k, v)) = part.split_once('=') else {
            continue;
        };
        if k == "repo" {
            let v = v.replace('+', " ");
            let v = v.trim();
            if !v.is_empty() {
                return Some(v.to_string());
            }
        }
    }
    None
}

pub fn format_cache_lines(report: &SavingsReport) -> String {
    format!(
        "headline hit% (runs ≥ {} prompt tokens): {}  first-run {}  resume {}\nshort (<{} tokens) hit% {} ({} runs; excluded from headline)\n",
        report.headline_min_input,
        fmt_hit(report.headline.hit),
        fmt_hit(report.first_run.hit),
        fmt_hit(report.resume.hit),
        report.headline_min_input,
        fmt_hit(report.short.hit),
        report.short.runs,
    )
}

fn fmt_hit(h: Option<f64>) -> String {
    h.map(|x| format!("{:.0}%", x * 100.0))
        .unwrap_or_else(|| "-".into())
}

/// One-line cache summary for `atr status` / Telegram from `status_json` cache.
pub fn format_status_cache_line(cache: &Value) -> String {
    let pct = |k: &str| {
        cache
            .get(k)
            .and_then(|v| v.as_f64())
            .map(|x| format!("{:.0}%", x * 100.0))
            .unwrap_or_else(|| "-".into())
    };
    let n = |k: &str| cache.get(k).and_then(|v| v.as_i64()).unwrap_or(0);
    format!(
        "cache headline={} ({} runs ≥ {} tokens)  first-run {}  resume {}  short {} ({} runs excluded)\n",
        pct("headline_hit"),
        n("headline_runs"),
        n("headline_min_input"),
        pct("first_run_hit"),
        pct("resume_hit"),
        pct("short_hit"),
        n("short_runs"),
    )
}

fn fmt_saved_usd(v: Option<f64>) -> String {
    v.map(|x| format!("{x:.4}")).unwrap_or_else(|| "-".into())
}

fn fmt_row_cells(r: &SavingsRow) -> String {
    format!(
        "{:>8} {:>8} {:>11} {:>8} {:>6} {:>8}",
        r.input,
        r.cache_read,
        r.cache_write,
        r.output,
        fmt_hit(r.hit),
        fmt_saved_usd(r.saved_usd)
    )
}

pub fn format_table(report: &SavingsReport, by: GroupBy) -> String {
    let mut out = format!(
        "token savings {} {}..{} ({})\n",
        report.period, report.from, report.to, report.timezone
    );
    out.push_str(&format_cache_lines(report));
    match by {
        GroupBy::Provider => {
            out.push_str(&format!(
                "{:<12} {:>8} {:>8} {:>11} {:>8} {:>6} {:>8}\n",
                "provider", "input", "cached", "cache_write", "output", "hit%", "est$"
            ));
            for r in &report.by_provider {
                out.push_str(&format!("{:<12} {}\n", r.provider, fmt_row_cells(r)));
            }
            out.push_str(&format!(
                "{:<12} {}\n",
                report.total.provider,
                fmt_row_cells(&report.total)
            ));
        }
        GroupBy::Day => {
            out.push_str(&format!(
                "{:<12} {:<12} {:>8} {:>8} {:>11} {:>8} {:>6} {:>8}\n",
                "day", "provider", "input", "cached", "cache_write", "output", "hit%", "est$"
            ));
            for r in &report.by_day_provider {
                out.push_str(&format!(
                    "{:<12} {:<12} {}\n",
                    r.day.as_deref().unwrap_or("-"),
                    r.provider,
                    fmt_row_cells(r)
                ));
            }
            out.push_str(&format!(
                "{:<12} {:<12} {}\n",
                report.total.day.as_deref().unwrap_or("-"),
                report.total.provider,
                fmt_row_cells(&report.total)
            ));
        }
    }
    out
}

pub fn format_compact(report: &SavingsReport) -> String {
    let mut out = format!(
        "usage {} {}..{} ({})\n",
        report.period, report.from, report.to, report.timezone
    );
    for r in &report.by_provider {
        out.push_str(&format!(
            "{} in={} cached={} write={} out={} hit={} ${}\n",
            r.provider,
            r.input,
            r.cache_read,
            r.cache_write,
            r.output,
            fmt_hit(r.hit),
            fmt_saved_usd(r.saved_usd)
        ));
    }
    out.push_str(&format!(
        "total in={} cached={} write={} out={} hit={} ${}\n",
        report.total.input,
        report.total.cache_read,
        report.total.cache_write,
        report.total.output,
        fmt_hit(report.total.hit),
        fmt_saved_usd(report.total.saved_usd)
    ));
    out.push_str(&format_cache_lines(report));
    out
}

fn row_json(r: &SavingsRow) -> Value {
    json!({
        "provider": r.provider,
        "kind": r.kind,
        "day": r.day,
        "input": r.input,
        "cached": r.cache_read,
        "cache_write": r.cache_write,
        "output": r.output,
        "hit": fmt_hit(r.hit),
        "tokens_saved": r.tokens_saved,
        "saved_usd": r.saved_usd,
        "cost_usd": r.cost_usd,
        "runs": r.runs,
    })
}

impl SavingsReport {
    pub fn to_json(&self) -> Value {
        json!({
            "period": self.period,
            "from": self.from,
            "to": self.to,
            "timezone": self.timezone,
            "by_provider": self.by_provider.iter().map(row_json).collect::<Vec<_>>(),
            "total": row_json(&self.total),
            "headline_min_input": self.headline_min_input,
            "headline": row_json(&self.headline),
            "first_run": row_json(&self.first_run),
            "resume": row_json(&self.resume),
            "short": row_json(&self.short),
            "by_day": self.by_day.iter().map(|d| json!({
                "day": d.day,
                "input": d.input,
                "cached": d.cache_read,
                "cache_write": d.cache_write,
                "output": d.output,
                "tokens_saved": d.tokens_saved,
            })).collect::<Vec<_>>(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn period_parse_and_month_range() {
        assert_eq!(parse_period("today").unwrap(), Period::Today);
        assert_eq!(parse_period("7d").unwrap(), Period::Days(7));
        assert_eq!(parse_period("30d").unwrap(), Period::Days(30));
        assert_eq!(parse_period("month").unwrap(), Period::Month);
        assert_eq!(
            parse_period("2026-10-01..2026-10-14").unwrap(),
            Period::Custom {
                from: parse_ymd("2026-10-01").unwrap(),
                to: parse_ymd("2026-10-14").unwrap(),
            }
        );
        let tz = DisplayTz::parse("UTC").unwrap();
        let now = Utc.with_ymd_and_hms(2026, 10, 14, 15, 0, 0).unwrap();
        assert_eq!(
            Period::Today.range(&tz, now).unwrap(),
            (
                parse_ymd("2026-10-14").unwrap(),
                parse_ymd("2026-10-14").unwrap()
            )
        );
        assert_eq!(
            Period::Days(7).range(&tz, now).unwrap(),
            (
                parse_ymd("2026-10-08").unwrap(),
                parse_ymd("2026-10-14").unwrap()
            )
        );
        assert_eq!(
            Period::Month.range(&tz, now).unwrap(),
            (
                parse_ymd("2026-10-01").unwrap(),
                parse_ymd("2026-10-14").unwrap()
            )
        );
        assert_eq!(
            resolve(Some("month"), 7, None, None).unwrap(),
            Period::Month
        );
        assert_eq!(resolve(None, 7, None, None).unwrap(), Period::Days(7));
        assert_eq!(
            resolve(None, 7, Some("2026-10-01"), Some("2026-10-03")).unwrap(),
            Period::Custom {
                from: parse_ymd("2026-10-01").unwrap(),
                to: parse_ymd("2026-10-03").unwrap(),
            }
        );
        assert_eq!(period_from_query(Some("period=month")), Period::Month);
        assert!(matches!(
            period_from_query(Some("period=custom&from=2026-10-01&to=2026-10-02")),
            Period::Custom { .. }
        ));
    }

    #[test]
    fn claude_hit_and_saved_usd() {
        // uncached 100, cached 800, write 20 → hit = 800/920
        let hit = hit_ratio_from_sums("claude", 100, 800, 20).unwrap();
        assert!((hit - 800.0 / 920.0).abs() < 1e-9);
        // billed = 100 + 80 + 25 = 205; full = 920; cost 2.05 → unit 0.01
        // saved = 2.05 * (920/205 - 1) = 7.15
        let saved = estimated_usd_saved("claude", 100, 800, 20, 2.05).unwrap();
        assert!((saved - 7.15).abs() < 1e-9);
        assert!(estimated_usd_saved("claude", 100, 800, 20, 0.0).is_none());
        assert_eq!(tokens_saved(800), 800);
    }

    #[test]
    fn codex_hit_ignores_write_and_half_price_cache() {
        // stored uncached 40, cached 160, write 10 → hit = 160/200 (not 210)
        let hit = hit_ratio_from_sums("codex", 40, 160, 10).unwrap();
        assert!((hit - 160.0 / 200.0).abs() < 1e-9);
        // billed = 40 + 80 = 120; full = 200; cost 1.20 → saved = 1.20 * (200/120 - 1) = 0.80
        let saved = estimated_usd_saved("codex", 40, 160, 10, 1.20).unwrap();
        assert!((saved - 0.80).abs() < 1e-9);
        let (billed, full) = token_weights("codex", 40.0, 160.0, 10.0);
        assert!((billed - 120.0).abs() < 1e-9);
        assert!((full - 200.0).abs() < 1e-9);
    }

    #[test]
    fn report_groups_and_fills_empty_days() {
        let cfg = empty_cfg();
        let days = vec![
            UsageDay {
                provider: "claude".into(),
                day: "2026-10-02".into(),
                input: 100,
                output: 50,
                cache_read: 800,
                cache_write: 20,
                cost_usd: 2.05,
                runs: 1,
            },
            UsageDay {
                provider: "codex".into(),
                day: "2026-10-02".into(),
                input: 40,
                output: 30,
                cache_read: 160,
                cache_write: 10,
                cost_usd: 0.0,
                runs: 1,
            },
        ];
        let period = Period::Custom {
            from: parse_ymd("2026-10-01").unwrap(),
            to: parse_ymd("2026-10-03").unwrap(),
        };
        let r = report_from_days(
            &cfg,
            &period,
            parse_ymd("2026-10-01").unwrap(),
            parse_ymd("2026-10-03").unwrap(),
            &days,
        );
        assert_eq!(r.by_day.len(), 3);
        assert_eq!(r.by_day[0].day, "2026-10-01");
        assert_eq!(r.by_day[0].tokens_saved, 0);
        assert_eq!(r.by_day[1].tokens_saved, 960);
        assert_eq!(r.by_provider.len(), 2);
        let claude = r
            .by_provider
            .iter()
            .find(|p| p.provider == "claude")
            .unwrap();
        assert_eq!(claude.tokens_saved, 800);
        assert!(claude.saved_usd.unwrap() > 0.0);
        let codex = r
            .by_provider
            .iter()
            .find(|p| p.provider == "codex")
            .unwrap();
        assert_eq!(codex.tokens_saved, 160);
        assert!(codex.saved_usd.is_none());
        assert_eq!(r.total.tokens_saved, 960);
        assert!(r.total.saved_usd.unwrap() > 0.0);
        let text = format_table(&r, GroupBy::Provider);
        assert!(text.contains("claude"), "{text}");
        assert!(text.contains("total"), "{text}");
        let compact = format_compact(&r);
        assert!(compact.contains("usage custom"), "{compact}");
        assert!(compact.contains("cached=800"), "{compact}");
        assert!(!compact.contains("saved=800"), "{compact}");
        assert!(text.contains("est$"), "{text}");
        assert!(!text.contains(" saved"), "{text}");
        assert_eq!(
            repo_from_query(Some("period=7d&repo=demo-1")).as_deref(),
            Some("demo-1")
        );
        assert_eq!(repo_from_query(Some("period=today")), None);
    }

    #[test]
    fn headline_excludes_short_and_splits_first_vs_resume() {
        let cfg = empty_cfg();
        let details = vec![
            crate::db::UsageDetail {
                provider: "claude".into(),
                input: 5_000,
                output: 10,
                cache_read: 1_000,
                cache_write: 0,
                cost_usd: 0.0,
                is_resume: false,
            },
            crate::db::UsageDetail {
                provider: "claude".into(),
                input: 10_000,
                output: 20,
                cache_read: 40_000,
                cache_write: 0,
                cost_usd: 0.0,
                is_resume: false,
            },
            crate::db::UsageDetail {
                provider: "claude".into(),
                input: 2_000,
                output: 20,
                cache_read: 80_000,
                cache_write: 0,
                cost_usd: 0.0,
                is_resume: true,
            },
        ];
        let mut report = report_from_days(
            &cfg,
            &Period::Today,
            parse_ymd("2026-10-14").unwrap(),
            parse_ymd("2026-10-14").unwrap(),
            &[],
        );
        apply_cache_splits(&cfg, &mut report, &details);
        assert_eq!(report.short.runs, 1);
        assert_eq!(report.headline.runs, 2);
        assert_eq!(report.first_run.runs, 1);
        assert_eq!(report.resume.runs, 1);
        // headline = first 50k + resume 82k; cache_read 40k+80k over den 50k+82k
        let h = report.headline.hit.unwrap();
        assert!((h - 120_000.0 / 132_000.0).abs() < 1e-9);
        let f = report.first_run.hit.unwrap();
        assert!((f - 40_000.0 / 50_000.0).abs() < 1e-9);
        let r = report.resume.hit.unwrap();
        assert!((r - 80_000.0 / 82_000.0).abs() < 1e-9);
        assert!(prompt_tokens("claude", 10_000, 40_000, 0) >= 40_000);
        assert!(prompt_tokens("claude", 5_000, 1_000, 0) < 40_000);
        let text = format_table(&report, GroupBy::Provider);
        assert!(text.contains("headline"), "{text}");
        assert!(text.contains("first-run"), "{text}");
        assert!(text.contains("excluded from headline"), "{text}");
    }

    fn empty_cfg() -> Config {
        let raw: crate::config::RawConfig = toml::from_str(
            r#"
listen = "127.0.0.1:0"
state_db = "/tmp/atr-usage-test.db"
worktree_root = "/tmp"
repo_path = "/tmp"
system_prompt_file = "/tmp/p.md"
display_timezone = "UTC"
[[provider]]
name = "claude"
kind = "claude"
cli = "/bin/true"
[[provider]]
name = "codex"
kind = "codex"
cli = "/bin/true"
"#,
        )
        .unwrap();
        Config::from_raw(raw).unwrap()
    }
}
