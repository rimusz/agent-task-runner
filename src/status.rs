//! Shared grouping for `atr status` and Telegram `/status`.
//!
//! Task `status` is free TEXT, not an enum. atr's stored `waiting` means
//! idle/finished after a successful turn and belongs in **done**, not waiting.

use crate::db::Task;
use serde_json::{json, Map, Value};
use std::collections::HashSet;

pub const CLI_CAP_FAILING: usize = 20;
pub const CLI_CAP_RUNNING: usize = 20;
pub const CLI_CAP_WAITING: usize = 8;
pub const CLI_CAP_DONE: usize = 5;

pub const TG_CAP_FAILING: usize = 10;
pub const TG_CAP_RUNNING: usize = 8;
pub const TG_CAP_WAITING: usize = 5;
pub const TG_CAP_DONE: usize = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bucket {
    Failing,
    Running,
    Waiting,
    Done,
    Other,
}

impl Bucket {
    pub fn json_key(self) -> &'static str {
        match self {
            Self::Failing => "failing",
            Self::Running => "running",
            Self::Waiting => "waiting",
            Self::Done => "done",
            Self::Other => "other",
        }
    }

    pub fn cli_label(self) -> &'static str {
        self.json_key()
    }

    pub fn telegram_label(self) -> &'static str {
        match self {
            Self::Failing => "Failing",
            Self::Running => "Running",
            Self::Waiting => "Waiting",
            Self::Done => "Done",
            Self::Other => "Other",
        }
    }

    pub fn display_order() -> [Self; 5] {
        [
            Self::Failing,
            Self::Running,
            Self::Waiting,
            Self::Done,
            Self::Other,
        ]
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GroupFilter {
    pub failing: bool,
    pub running: bool,
    pub waiting: bool,
    pub done: bool,
}

impl GroupFilter {
    pub fn all() -> Self {
        Self {
            failing: true,
            running: true,
            waiting: true,
            done: true,
        }
    }

    pub fn from_flags(failed: bool, running: bool, waiting: bool, done: bool) -> Self {
        if !failed && !running && !waiting && !done {
            Self::all()
        } else {
            Self {
                failing: failed,
                running,
                waiting,
                done,
            }
        }
    }

    /// Telegram `/status done|failed|running|waiting|all`.
    pub fn from_name(name: &str) -> Option<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "failed" | "failing" => Some(Self {
                failing: true,
                running: false,
                waiting: false,
                done: false,
            }),
            "running" => Some(Self {
                failing: false,
                running: true,
                waiting: false,
                done: false,
            }),
            "waiting" => Some(Self {
                failing: false,
                running: false,
                waiting: true,
                done: false,
            }),
            "done" => Some(Self {
                failing: false,
                running: false,
                waiting: false,
                done: true,
            }),
            "all" => Some(Self::all()),
            _ => None,
        }
    }

    pub fn is_all(self) -> bool {
        self.failing && self.running && self.waiting && self.done
    }

    pub fn shows(self, bucket: Bucket) -> bool {
        match bucket {
            Bucket::Failing => self.failing,
            Bucket::Running => self.running,
            Bucket::Waiting => self.waiting,
            Bucket::Done => self.done,
            Bucket::Other => self.failing && self.running && self.waiting && self.done,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Caps {
    pub failing: usize,
    pub running: usize,
    pub waiting: usize,
    pub done: usize,
    pub other: usize,
}

impl Caps {
    pub fn for_bucket(self, bucket: Bucket) -> usize {
        match bucket {
            Bucket::Failing => self.failing,
            Bucket::Running => self.running,
            Bucket::Waiting => self.waiting,
            Bucket::Done => self.done,
            Bucket::Other => self.other,
        }
    }
}

pub fn cli_caps(all: bool, limit: Option<usize>) -> Caps {
    Caps {
        failing: limit.unwrap_or(CLI_CAP_FAILING),
        running: limit.unwrap_or(CLI_CAP_RUNNING),
        waiting: limit.unwrap_or(if all { usize::MAX } else { CLI_CAP_WAITING }),
        done: limit.unwrap_or(if all { usize::MAX } else { CLI_CAP_DONE }),
        other: limit.unwrap_or(CLI_CAP_FAILING),
    }
}

pub fn telegram_caps() -> Caps {
    Caps {
        failing: TG_CAP_FAILING,
        running: TG_CAP_RUNNING,
        waiting: TG_CAP_WAITING,
        done: TG_CAP_DONE,
        other: TG_CAP_FAILING,
    }
}

pub fn telegram_caps_full() -> Caps {
    Caps {
        failing: usize::MAX,
        running: usize::MAX,
        waiting: usize::MAX,
        done: usize::MAX,
        other: usize::MAX,
    }
}

/// Telegram bot message limit. Split or truncate before this.
pub const TG_MSG_LIMIT: usize = 4096;

/// Finished/idle label for the done group: last outcome, else `done`
/// (stored `waiting` means idle after success, not "waiting to run").
pub fn done_label(status: &str, last_outcome: Option<&str>) -> String {
    match last_outcome.map(str::trim).filter(|s| !s.is_empty()) {
        Some(o) => o.to_string(),
        None if status == "waiting" => "done".into(),
        None => status.to_string(),
    }
}

pub fn is_live(
    status: &str,
    key: &str,
    inflight: &HashSet<String>,
    live_open: &HashSet<String>,
) -> bool {
    status == "running" || inflight.contains(key) || live_open.contains(key)
}

pub fn classify(status: &str, live: bool) -> Bucket {
    if live {
        return Bucket::Running;
    }
    match status {
        "failed" | "needs_attention" | "cancelled" => Bucket::Failing,
        "queued" | "held" => Bucket::Waiting,
        "waiting" | "done" => Bucket::Done,
        _ => Bucket::Other,
    }
}

fn failing_rank(status: &str) -> u8 {
    match status {
        "needs_attention" => 0,
        "failed" => 1,
        "cancelled" => 2,
        _ => 3,
    }
}

pub fn waiting_why(task: &Task) -> String {
    if task.status == "held" && !task.hold_reason.is_empty() {
        return task.hold_reason.clone();
    }
    if !task.retry_reason.is_empty() {
        return if task.retry_next_at.is_empty() {
            format!("retry ({})", task.retry_reason)
        } else {
            format!("retry {} ({})", task.retry_next_at, task.retry_reason)
        };
    }
    if task.status == "queued" {
        let keys = crate::deps::split_deps(&task.deps);
        if !keys.is_empty() {
            return format!("after {}", keys.join(","));
        }
    }
    String::new()
}

pub fn failing_why(result_text: &str, last_outcome: Option<&str>) -> String {
    result_text
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(str::to_string)
        .or_else(|| {
            last_outcome
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        })
        .unwrap_or_default()
}

pub fn elapsed_secs(started: &str) -> Option<i64> {
    let ts = chrono::DateTime::parse_from_rfc3339(started).ok()?;
    Some(
        (chrono::Utc::now() - ts.with_timezone(&chrono::Utc))
            .num_seconds()
            .max(0),
    )
}

pub fn first_line(body: &str, max: usize) -> String {
    let line = body
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    if line.chars().count() > max {
        format!("{}…", line.chars().take(max).collect::<String>())
    } else {
        line.to_string()
    }
}

pub struct Enrich {
    pub exit_code: Option<i64>,
    pub last_outcome: Option<String>,
    pub elapsed: Option<String>,
    pub elapsed_s: Option<i64>,
    pub progress: Option<String>,
    pub why: String,
    pub updated: String,
    pub model: Option<String>,
    pub stall_retries: i64,
}

pub fn enrich_item(mut item: Value, extra: Enrich) -> Value {
    item["exit_code"] = match extra.exit_code {
        Some(c) => json!(c),
        None => Value::Null,
    };
    item["last_outcome"] = match extra.last_outcome {
        Some(s) => json!(s),
        None => Value::Null,
    };
    item["elapsed"] = match extra.elapsed {
        Some(s) => json!(s),
        None => Value::Null,
    };
    item["elapsed_s"] = match extra.elapsed_s {
        Some(s) => json!(s),
        None => Value::Null,
    };
    item["progress"] = match extra.progress {
        Some(s) => json!(s),
        None => Value::Null,
    };
    item["why"] = json!(extra.why);
    item["updated"] = json!(extra.updated);
    if extra.stall_retries > 0 {
        item["stall_retries"] = json!(extra.stall_retries);
    }
    if let Some(model) = extra.model {
        item["model"] = json!(model);
    }
    item
}

pub struct GroupRow {
    pub bucket: Bucket,
    pub updated: String,
    pub status: String,
    pub item: Value,
}

pub fn assemble_groups(rows: Vec<GroupRow>) -> Value {
    let mut buckets: [Vec<GroupRow>; 5] =
        [Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new()];
    for row in rows {
        let i = match row.bucket {
            Bucket::Failing => 0,
            Bucket::Running => 1,
            Bucket::Waiting => 2,
            Bucket::Done => 3,
            Bucket::Other => 4,
        };
        buckets[i].push(row);
    }
    let mut map = Map::new();
    for bucket in Bucket::display_order() {
        let i = match bucket {
            Bucket::Failing => 0,
            Bucket::Running => 1,
            Bucket::Waiting => 2,
            Bucket::Done => 3,
            Bucket::Other => 4,
        };
        let mut rows = std::mem::take(&mut buckets[i]);
        if bucket == Bucket::Other && rows.is_empty() {
            continue;
        }
        rows.sort_by(|a, b| {
            let cmp = b.updated.cmp(&a.updated);
            if cmp == std::cmp::Ordering::Equal && bucket == Bucket::Failing {
                failing_rank(&a.status).cmp(&failing_rank(&b.status))
            } else {
                cmp
            }
        });
        map.insert(
            bucket.json_key().to_string(),
            json!(rows.into_iter().map(|r| r.item).collect::<Vec<_>>()),
        );
    }
    Value::Object(map)
}

/// Groups from a status snapshot, or a fallback classification of `tasks[]`.
pub fn groups_view(v: &Value) -> Value {
    if let Some(g) = v.get("groups").filter(|g| g.is_object()) {
        return g.clone();
    }
    let inflight: HashSet<String> = v
        .get("running")
        .and_then(|x| x.as_array())
        .into_iter()
        .flatten()
        .filter_map(|x| x.as_str().map(str::to_string))
        .collect();
    let empty_open = HashSet::new();
    let mut rows = Vec::new();
    if let Some(tasks) = v.get("tasks").and_then(|x| x.as_array()) {
        for t in tasks {
            let key = t.get("key").and_then(Value::as_str).unwrap_or("");
            let status = t.get("status").and_then(Value::as_str).unwrap_or("");
            let live = is_live(status, key, &inflight, &empty_open);
            let bucket = classify(status, live);
            rows.push(GroupRow {
                bucket,
                updated: t
                    .get("updated")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                status: status.to_string(),
                item: t.clone(),
            });
        }
    }
    assemble_groups(rows)
}

pub fn apply_group_filter(groups: &Value, filter: GroupFilter) -> Value {
    let mut map = Map::new();
    for bucket in [
        Bucket::Failing,
        Bucket::Running,
        Bucket::Waiting,
        Bucket::Done,
    ] {
        let items = if filter.shows(bucket) {
            groups
                .get(bucket.json_key())
                .cloned()
                .unwrap_or_else(|| json!([]))
        } else {
            json!([])
        };
        map.insert(bucket.json_key().to_string(), items);
    }
    if filter.shows(Bucket::Other) {
        if let Some(other) = groups
            .get("other")
            .filter(|a| a.as_array().is_some_and(|x| !x.is_empty()))
        {
            map.insert("other".into(), other.clone());
        }
    }
    Value::Object(map)
}

fn group_items(groups: &Value, bucket: Bucket) -> &[Value] {
    groups
        .get(bucket.json_key())
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

fn section_header(label: &str, total: usize, shown: usize) -> String {
    if shown < total {
        format!("{label} ({total}, showing {shown})\n")
    } else {
        format!("{label} ({total})\n")
    }
}

fn json_text(t: &Value, key: &str) -> String {
    match t.get(key) {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        _ => String::new(),
    }
}

fn cell(t: &Value, key: &str) -> String {
    let s = json_text(t, key);
    if s.is_empty() {
        "-".into()
    } else {
        s
    }
}

fn stall_note(t: &Value) -> String {
    match t.get("stall_retries").and_then(Value::as_i64) {
        Some(n) if n > 0 => format!("stall {n}"),
        _ => String::new(),
    }
}

fn exit_cell(t: &Value) -> String {
    match t.get("exit_code") {
        Some(Value::Number(n)) => n.to_string(),
        _ => "-".into(),
    }
}

fn is_check_only(t: &Value) -> bool {
    t.get("check_only")
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

fn done_status_cell(t: &Value) -> String {
    let outcome = json_text(t, "last_outcome");
    let status = json_text(t, "status");
    let mut label = done_label(
        &status,
        if outcome.is_empty() {
            None
        } else {
            Some(outcome.as_str())
        },
    );
    if is_check_only(t) {
        label.push_str(" check-only");
    }
    label
}

fn cli_row(bucket: Bucket, t: &Value) -> Vec<String> {
    let key = cell(t, "key");
    let repo = cell(t, "repo");
    let status = if is_check_only(t) && bucket != Bucket::Done {
        let s = cell(t, "status");
        if s == "-" {
            "check-only".into()
        } else {
            format!("{s} check-only")
        }
    } else {
        cell(t, "status")
    };
    let provider = cell(t, "provider");
    match bucket {
        Bucket::Failing => vec![
            key,
            repo,
            status,
            provider,
            exit_cell(t),
            json_text(t, "why"),
        ],
        Bucket::Running => {
            let mut row = vec![
                key,
                repo,
                provider,
                cell(t, "model"),
                cell(t, "elapsed"),
                json_text(t, "progress"),
            ];
            let stall = stall_note(t);
            if !stall.is_empty() {
                row.push(stall);
            }
            row
        }
        Bucket::Waiting => {
            let mut row = vec![key, repo, status, provider, json_text(t, "why")];
            let stall = stall_note(t);
            if !stall.is_empty() {
                row.push(stall);
            }
            row
        }
        Bucket::Done => vec![key, repo, done_status_cell(t), provider, cell(t, "updated")],
        Bucket::Other => vec![key, repo, status, provider],
    }
}

fn align_rows(rows: &[Vec<String>]) -> String {
    if rows.is_empty() {
        return String::new();
    }
    let cols = rows.iter().map(Vec::len).max().unwrap_or(0);
    let widths: Vec<usize> = (0..cols)
        .map(|i| {
            rows.iter()
                .map(|r| r.get(i).map(|s| s.chars().count()).unwrap_or(0))
                .max()
                .unwrap_or(0)
        })
        .collect();
    let mut out = String::new();
    for r in rows {
        out.push_str("  ");
        for (i, cell) in r.iter().enumerate() {
            if i > 0 {
                out.push_str("  ");
            }
            if i + 1 == r.len() {
                out.push_str(cell);
            } else {
                out.push_str(&format!("{:<w$}", cell, w = widths[i]));
            }
        }
        out.push('\n');
    }
    out
}

fn more_line(indent: &str, extra: usize) -> String {
    format!("{indent}… {extra} more\n")
}

pub fn format_cli_task_sections(groups: &Value, filter: GroupFilter, caps: Caps) -> String {
    let mut out = String::new();
    for bucket in Bucket::display_order() {
        if !filter.shows(bucket) {
            continue;
        }
        let items = group_items(groups, bucket);
        if items.is_empty() {
            continue;
        }
        let cap = caps.for_bucket(bucket);
        let shown = items.len().min(cap);
        out.push_str(&section_header(bucket.cli_label(), items.len(), shown));
        let rows: Vec<Vec<String>> = items
            .iter()
            .take(shown)
            .map(|t| cli_row(bucket, t))
            .collect();
        out.push_str(&align_rows(&rows));
        if items.len() > shown {
            out.push_str(&more_line("  ", items.len() - shown));
        }
    }
    out
}

fn compact_retry_why(why: &str) -> String {
    let Some(rest) = why.strip_prefix("retry ") else {
        return why.to_string();
    };
    let (when, suffix) = match rest.find(" (") {
        Some(i) => (&rest[..i], &rest[i..]),
        None => return why.to_string(),
    };
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(when) {
        return format!("retry {}{suffix}", dt.format("%H:%M"));
    }
    why.to_string()
}

fn join_bits(parts: &[String]) -> String {
    parts
        .iter()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty() && *s != "-")
        .collect::<Vec<_>>()
        .join(", ")
}

fn telegram_bullet(bucket: Bucket, t: &Value) -> String {
    let key = cell(t, "key");
    let stall = stall_note(t);
    match bucket {
        Bucket::Failing => {
            let status = if is_check_only(t) {
                format!("{} check-only", cell(t, "status"))
            } else {
                cell(t, "status")
            };
            let why = first_line(&json_text(t, "why"), 80);
            if why.is_empty() {
                format!("• {key} — {status}\n")
            } else {
                format!("• {key} — {status}, {why}\n")
            }
        }
        Bucket::Running => {
            let mut bits = vec![
                json_text(t, "model"),
                json_text(t, "elapsed"),
                first_line(&json_text(t, "progress"), 60),
            ];
            if !stall.is_empty() {
                bits.push(stall);
            }
            let detail = join_bits(&bits);
            if detail.is_empty() {
                format!("• {key}\n")
            } else {
                format!("• {key} — {detail}\n")
            }
        }
        Bucket::Waiting => {
            let status = if is_check_only(t) {
                format!("{} check-only", cell(t, "status"))
            } else {
                cell(t, "status")
            };
            let extra = compact_retry_why(&json_text(t, "why"));
            let mut detail = if extra.is_empty() {
                status
            } else {
                format!("{status}, {extra}")
            };
            if !stall.is_empty() {
                detail.push_str(", ");
                detail.push_str(&stall);
            }
            format!("• {key} — {detail}\n")
        }
        Bucket::Done => {
            let status = done_status_cell(t);
            let updated = json_text(t, "updated");
            if updated.is_empty() {
                format!("• {key} — {status}\n")
            } else {
                format!("• {key} — {status}, {updated}\n")
            }
        }
        Bucket::Other => {
            let status = cell(t, "status");
            format!("• {key} — {status}\n")
        }
    }
}

fn render_telegram_sections(
    groups: &Value,
    caps: Caps,
    filter: GroupFilter,
    show_empty: bool,
) -> String {
    let mut out = String::new();
    for bucket in Bucket::display_order() {
        if !filter.shows(bucket) {
            continue;
        }
        let items = group_items(groups, bucket);
        if items.is_empty() {
            if show_empty && bucket != Bucket::Other {
                out.push_str(&section_header(bucket.telegram_label(), 0, 0));
            }
            continue;
        }
        let cap = caps.for_bucket(bucket);
        let shown = items.len().min(cap);
        out.push_str(&section_header(bucket.telegram_label(), items.len(), shown));
        for t in items.iter().take(shown) {
            out.push_str(&telegram_bullet(bucket, t));
        }
        if items.len() > shown {
            out.push_str(&more_line("", items.len() - shown));
        }
    }
    out
}

/// Phone task sections. If `budget` would overflow, drop Done first, then
/// truncate Waiting; Failing and Running stay.
pub fn format_telegram_task_sections(groups: &Value, budget: usize) -> String {
    format_telegram_task_sections_opts(groups, GroupFilter::all(), false, budget)
}

/// Phone task sections. `full` lifts the usual caps (filtered `/status done`
/// and `/status all`). Empty selected groups still print a `(0)` header when
/// `full` is a single-group filter.
pub fn format_telegram_task_sections_opts(
    groups: &Value,
    filter: GroupFilter,
    full: bool,
    budget: usize,
) -> String {
    let groups = apply_group_filter(groups, filter);
    let show_empty = full && !filter.is_all();
    if full {
        return render_telegram_sections(&groups, telegram_caps_full(), filter, show_empty);
    }
    let mut caps = telegram_caps();
    let mut body = render_telegram_sections(&groups, caps, filter, false);
    if body.len() <= budget {
        return body;
    }
    if !group_items(&groups, Bucket::Done).is_empty() && caps.done > 0 {
        caps.done = 0;
        body = render_telegram_sections(&groups, caps, filter, false);
        if body.len() <= budget {
            return body;
        }
    }
    while caps.waiting > 0 {
        caps.waiting -= 1;
        body = render_telegram_sections(&groups, caps, filter, false);
        if body.len() <= budget {
            return body;
        }
    }
    body
}

/// Split a Telegram body into chunks at most `limit` bytes, preferring
/// newline boundaries. A leftover that still overflows is truncated with a note.
pub fn split_telegram_text(text: &str, limit: usize) -> Vec<String> {
    if limit == 0 {
        return vec![text.to_string()];
    }
    if text.len() <= limit {
        return if text.is_empty() {
            Vec::new()
        } else {
            vec![text.to_string()]
        };
    }
    let note = "\n… truncated";
    let mut out = Vec::new();
    let mut cur = String::new();
    for line in text.split_inclusive('\n') {
        if line.len() > limit {
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
            let keep = limit.saturating_sub(note.len()).max(1);
            let mut chunk: String = line.chars().take(keep).collect();
            chunk.push_str(note);
            out.push(chunk);
            continue;
        }
        if cur.len() + line.len() > limit && !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
        }
        cur.push_str(line);
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

pub fn failing_count(groups: &Value) -> usize {
    group_items(groups, Bucket::Failing).len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stored_waiting_is_done_not_waiting() {
        assert_eq!(classify("waiting", false), Bucket::Done);
        assert_eq!(classify("done", false), Bucket::Done);
        assert_eq!(classify("queued", false), Bucket::Waiting);
        assert_eq!(classify("held", false), Bucket::Waiting);
        assert_eq!(classify("failed", false), Bucket::Failing);
        assert_eq!(classify("needs_attention", false), Bucket::Failing);
        assert_eq!(classify("cancelled", false), Bucket::Failing);
        assert_eq!(classify("queued", true), Bucket::Running);
        assert_eq!(classify("mystery", false), Bucket::Other);
    }

    #[test]
    fn failing_ties_rank_needs_attention_first() {
        let rows = vec![
            GroupRow {
                bucket: Bucket::Failing,
                updated: "2026-01-01T00:00:00Z".into(),
                status: "cancelled".into(),
                item: json!({"key": "c", "status": "cancelled"}),
            },
            GroupRow {
                bucket: Bucket::Failing,
                updated: "2026-01-01T00:00:00Z".into(),
                status: "failed".into(),
                item: json!({"key": "f", "status": "failed"}),
            },
            GroupRow {
                bucket: Bucket::Failing,
                updated: "2026-01-01T00:00:00Z".into(),
                status: "needs_attention".into(),
                item: json!({"key": "n", "status": "needs_attention"}),
            },
        ];
        let g = assemble_groups(rows);
        let keys: Vec<&str> = g["failing"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["key"].as_str().unwrap())
            .collect();
        assert_eq!(keys, ["n", "f", "c"]);
    }

    #[test]
    fn cli_sections_follow_bucket_order_and_omit_empty() {
        let groups = json!({
            "failing": [{"key": "f1", "repo": "r", "status": "failed", "provider": "claude", "exit_code": 1, "why": "boom"}],
            "running": [{"key": "r1", "repo": "r", "provider": "claude", "model": "m", "elapsed": "3s", "progress": "tool"}],
            "waiting": [{"key": "w1", "repo": "r", "status": "held", "provider": "claude", "why": "after x failed"}],
            "done": [
                {"key": "d1", "repo": "r", "status": "waiting", "provider": "claude", "updated": "2026-01-01 00:00 UTC"},
                {"key": "d2", "repo": "r", "status": "done", "provider": "claude", "updated": "2026-01-01 00:00 UTC"}
            ]
        });
        let text = format_cli_task_sections(&groups, GroupFilter::all(), cli_caps(false, None));
        let failing = text.find("failing (1)").unwrap();
        let running = text.find("running (1)").unwrap();
        let waiting = text.find("waiting (1)").unwrap();
        let done = text.find("done (2)").unwrap();
        assert!(
            failing < running && running < waiting && waiting < done,
            "{text}"
        );
        assert!(text.contains("f1"), "{text}");
        assert!(text.contains("d1"), "{text}");
        assert!(!text.contains("other"), "{text}");

        let only = format_cli_task_sections(
            &groups,
            GroupFilter::from_flags(true, false, false, true),
            cli_caps(false, None),
        );
        assert!(only.contains("failing (1)"), "{only}");
        assert!(only.contains("done (2)"), "{only}");
        assert!(!only.contains("running ("), "{only}");
        assert!(!only.contains("waiting ("), "{only}");
    }

    #[test]
    fn limit_zero_prints_counts_without_rows() {
        let groups = json!({
            "failing": [{"key": "f1", "status": "failed"}],
            "running": [],
            "waiting": [],
            "done": []
        });
        let text = format_cli_task_sections(&groups, GroupFilter::all(), cli_caps(false, Some(0)));
        assert!(text.contains("failing (1, showing 0)"), "{text}");
        assert!(text.contains("… 1 more"), "{text}");
        assert!(!text.contains("  f1"), "{text}");
    }

    #[test]
    fn telegram_puts_stored_waiting_under_done() {
        let groups = json!({
            "failing": [{"key": "bad", "status": "failed", "why": "nope"}],
            "running": [{"key": "live", "model": "opus", "elapsed": "3m", "progress": "edit"}],
            "waiting": [{"key": "hold", "status": "held", "why": "needs merge"}],
            "done": [{"key": "idle", "status": "waiting", "updated": "2026-01-01 00:00 UTC"}]
        });
        let text = format_telegram_task_sections(&groups, 3500);
        let fail = text.find("Failing (1)").unwrap();
        let run = text.find("Running (1)").unwrap();
        let wait = text.find("Waiting (1)").unwrap();
        let done = text.find("Done (1)").unwrap();
        assert!(fail < run && run < wait && wait < done, "{text}");
        assert!(text.contains("• bad — failed, nope"), "{text}");
        assert!(text.contains("• live — opus, 3m, edit"), "{text}");
        assert!(text.contains("• hold — held, needs merge"), "{text}");
        assert!(
            text.contains("• idle — done, 2026-01-01 00:00 UTC"),
            "{text}"
        );
        let done_at = text.find("• idle").unwrap();
        let waiting_hdr = text.find("Waiting (1)").unwrap();
        assert!(
            done_at > waiting_hdr,
            "stored waiting must not be under Waiting:\n{text}"
        );
    }

    #[test]
    fn telegram_drops_done_before_waiting_when_over_budget() {
        let groups = json!({
            "failing": [{"key": "f", "status": "failed", "why": "x"}],
            "running": [{"key": "r", "model": "m", "elapsed": "1s", "progress": "p"}],
            "waiting": [{"key": "w", "status": "queued", "why": "retry 2026-11-09T16:02:00Z (rate limit)"}],
            "done": [{"key": "d", "status": "waiting", "updated": "later"}]
        });
        let full = format_telegram_task_sections(&groups, 3500);
        assert!(full.contains("Done (1)"), "{full}");
        assert!(full.contains("retry 16:02 (rate limit)"), "{full}");
        let tight = format_telegram_task_sections(&groups, 80);
        assert!(tight.contains("Failing (1)"), "{tight}");
        assert!(tight.contains("Running (1)"), "{tight}");
        assert!(
            tight.contains("Done (1, showing 0)") || !tight.contains("• d —"),
            "{tight}"
        );
    }

    #[test]
    fn done_label_uses_outcome_not_stored_waiting() {
        assert_eq!(done_label("waiting", None), "done");
        assert_eq!(done_label("waiting", Some("success")), "success");
        assert_eq!(done_label("waiting", Some("checked")), "checked");
        assert_eq!(done_label("done", None), "done");
        assert_eq!(done_label("done", Some("")), "done");
    }

    #[test]
    fn done_row_marks_check_only_and_hides_waiting() {
        let groups = json!({
            "failing": [],
            "running": [],
            "waiting": [],
            "done": [{
                "key": "proof",
                "repo": "r",
                "status": "waiting",
                "provider": "claude",
                "updated": "later",
                "last_outcome": "checked",
                "check_only": true
            }]
        });
        let cli = format_cli_task_sections(&groups, GroupFilter::all(), cli_caps(false, None));
        assert!(cli.contains("checked check-only"), "{cli}");
        assert!(!cli.contains("  waiting  "), "{cli}");
        let tg = format_telegram_task_sections(&groups, 3500);
        assert!(tg.contains("• proof — checked check-only, later"), "{tg}");
    }

    #[test]
    fn telegram_status_name_and_split() {
        assert_eq!(
            GroupFilter::from_name("failed").unwrap(),
            GroupFilter::from_flags(true, false, false, false)
        );
        assert_eq!(GroupFilter::from_name("all").unwrap(), GroupFilter::all());
        assert!(GroupFilter::from_name("nixer").is_none());
        let parts = split_telegram_text("aa\nbb\ncc\n", 5);
        assert!(parts.len() >= 2, "{parts:?}");
        assert!(parts.iter().all(|p| p.len() <= 5), "{parts:?}");
        let one = split_telegram_text("short", 100);
        assert_eq!(one, vec!["short".to_string()]);
    }

    #[test]
    fn telegram_filtered_full_shows_empty_header() {
        let groups = json!({
            "failing": [],
            "running": [],
            "waiting": [],
            "done": [{"key": "d", "status": "waiting", "updated": "t"}]
        });
        let text = format_telegram_task_sections_opts(
            &groups,
            GroupFilter::from_flags(true, false, false, false),
            true,
            4096,
        );
        assert!(text.contains("Failing (0)"), "{text}");
        assert!(!text.contains("Done ("), "{text}");
    }

    #[test]
    fn watch_json_filter_keeps_empty_selected_groups() {
        let groups = json!({
            "failing": [{"key": "f"}],
            "running": [{"key": "r"}],
            "waiting": [],
            "done": []
        });
        let filtered =
            apply_group_filter(&groups, GroupFilter::from_flags(false, true, false, false));
        assert_eq!(filtered["failing"], json!([]));
        assert_eq!(filtered["running"][0]["key"], "r");
        assert!(filtered.get("other").is_none());
    }
}
