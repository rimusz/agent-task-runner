//! Telegram reachability tracking (getUpdates health) and long-reply
//! splitting. Never logs the bot token.

use chrono::{DateTime, Utc};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

/// Consecutive getUpdates failures before a WARN log.
pub const WARN_AFTER: u32 = 3;
/// Unreachable this long → `telegram_unreachable` notify-hook alert.
pub const ALERT_AFTER: Duration = Duration::from_secs(15 * 60);
/// Telegram hard limit is 4096; keep headroom for HTML tags/escaping.
pub const CHUNK_LIMIT: usize = 3500;
/// More chunks than this → send one `.md` document instead.
pub const MAX_CHUNKS: usize = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel {
    Debug,
    Warn,
    Info,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HealthEvent {
    Unreachable,
    Recovered,
}

impl HealthEvent {
    pub fn kind(&self) -> &'static str {
        match self {
            HealthEvent::Unreachable => "telegram_unreachable",
            HealthEvent::Recovered => "telegram_recovered",
        }
    }
}

#[derive(Debug, Default, Clone)]
pub struct PollHealth {
    pub consecutive: u32,
    pub failing_since: Option<DateTime<Utc>>,
    pub alerted: bool,
    pub last_ok: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
}

impl PollHealth {
    /// Record one getUpdates outcome. Returns the log level for this event and
    /// an optional hook alert. Changed = state worth persisting.
    pub fn record(
        &mut self,
        ok: bool,
        err: Option<&str>,
        now: DateTime<Utc>,
    ) -> (LogLevel, Option<HealthEvent>, bool) {
        if ok {
            let was_failing = self.consecutive > 0;
            let warned = self.consecutive >= WARN_AFTER;
            let event = self.alerted.then_some(HealthEvent::Recovered);
            self.consecutive = 0;
            self.failing_since = None;
            self.alerted = false;
            self.last_error = None;
            let first_ok = self.last_ok.is_none();
            self.last_ok = Some(now);
            let level = if warned {
                LogLevel::Info
            } else {
                LogLevel::Debug
            };
            return (level, event, was_failing || first_ok);
        }
        self.consecutive += 1;
        self.last_error = err.map(String::from);
        if self.failing_since.is_none() {
            self.failing_since = Some(now);
        }
        let level = if self.consecutive == WARN_AFTER {
            LogLevel::Warn
        } else {
            LogLevel::Debug
        };
        let mut event = None;
        if !self.alerted {
            if let Some(since) = self.failing_since {
                if (now - since)
                    .to_std()
                    .map(|d| d >= ALERT_AFTER)
                    .unwrap_or(false)
                {
                    self.alerted = true;
                    event = Some(HealthEvent::Unreachable);
                }
            }
        }
        let changed = self.consecutive == 1 || level == LogLevel::Warn || event.is_some();
        (level, event, changed)
    }

    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "consecutive_failures": self.consecutive,
            "failing_since": self.failing_since.map(|t| t.to_rfc3339()),
            "alerted": self.alerted,
            "last_ok": self.last_ok.map(|t| t.to_rfc3339()),
            "last_error": self.last_error,
            "checked_at": Utc::now().to_rfc3339(),
        })
    }
}

static HEALTH: Mutex<Option<PollHealth>> = Mutex::new(None);

pub fn state_path(config_path: &Path) -> PathBuf {
    config_path
        .parent()
        .map(|p| p.join("telegram-health.json"))
        .unwrap_or_else(|| PathBuf::from("telegram-health.json"))
}

fn short_err(e: &str) -> String {
    let e = e.trim();
    let mut s: String = e.chars().take(160).collect();
    if e.chars().count() > 160 {
        s.push('…');
    }
    s
}

/// Called by the poll loop after each getUpdates. `err` must already be redacted.
pub fn observe_poll(cfg: &crate::config::Config, config_path: &Path, err: Option<&str>) {
    let now = Utc::now();
    let (level, event, changed, snapshot) = {
        let mut g = HEALTH.lock().unwrap_or_else(|e| e.into_inner());
        let h = g.get_or_insert_with(PollHealth::default);
        let (l, e, c) = h.record(err.is_none(), err.map(short_err).as_deref(), now);
        (l, e, c, h.clone())
    };
    match (level, err) {
        (LogLevel::Warn, Some(e)) => tracing::warn!(
            failures = snapshot.consecutive,
            "telegram getUpdates failing: {e}"
        ),
        (LogLevel::Info, _) => tracing::info!("telegram getUpdates recovered"),
        (_, Some(e)) => {
            tracing::debug!(failures = snapshot.consecutive, "telegram getUpdates: {e}")
        }
        _ => {}
    }
    if changed {
        let path = state_path(config_path);
        let tmp = path.with_extension("json.tmp");
        if std::fs::write(&tmp, snapshot.to_json().to_string()).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    }
    if let Some(ev) = event {
        let msg = match ev {
            HealthEvent::Unreachable => format!(
                "Telegram unreachable for 15+ min ({} failed polls): {}",
                snapshot.consecutive,
                snapshot.last_error.clone().unwrap_or_default()
            ),
            HealthEvent::Recovered => "Telegram reachable again.".to_string(),
        };
        if matches!(ev, HealthEvent::Unreachable) {
            tracing::error!("{msg}");
        } else {
            tracing::info!("{msg}");
        }
        let hook = cfg.notify.clone();
        if !hook.command.is_empty() {
            let kind = ev.kind();
            std::thread::spawn(move || {
                crate::gh_token::run_hook(
                    &hook,
                    kind,
                    &msg,
                    &[("ATR_ALERT_SOURCE", "telegram".to_string())],
                );
            });
        }
    }
}

/// `{level, message, ...}` for status/dashboard/doctor. level: ok|warn|fail|unknown|off.
pub fn status_json(
    cfg: &crate::config::Config,
    config_path: &Path,
    now: DateTime<Utc>,
) -> serde_json::Value {
    if !cfg.telegram.enabled {
        return serde_json::json!({"level": "off", "message": "telegram disabled"});
    }
    let raw: serde_json::Value = std::fs::read_to_string(state_path(config_path))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or(serde_json::Value::Null);
    summarize(&raw, now)
}

/// Telegram answers getUpdates with 409 when another getUpdates long-poll or a
/// webhook holds the same bot token.
pub fn is_conflict(err: &str) -> bool {
    err.contains("status code 409")
        || err.contains("409 Conflict")
        || err.contains("terminated by other getUpdates")
}

pub fn summarize(raw: &serde_json::Value, now: DateTime<Utc>) -> serde_json::Value {
    if raw.is_null() {
        return serde_json::json!({"level": "unknown", "message": "no Telegram poll recorded yet (is atr serve running?)"});
    }
    let n = raw["consecutive_failures"].as_u64().unwrap_or(0);
    let since = raw["failing_since"]
        .as_str()
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|d| d.with_timezone(&Utc));
    let mins = since.map(|s| (now - s).num_minutes()).unwrap_or(0);
    let raw_err = raw["last_error"].as_str().unwrap_or("");
    let conflict;
    let err = if is_conflict(raw_err) {
        conflict = format!(
            "409 Conflict: another poller (second `atr serve`, test run, or other bot) is using this bot token, or a webhook is set ({raw_err})"
        );
        conflict.as_str()
    } else {
        raw_err
    };
    let (level, message) = if n == 0 {
        ("ok", "Telegram reachable".to_string())
    } else if mins >= (ALERT_AFTER.as_secs() / 60) as i64 {
        (
            "fail",
            format!("Telegram unreachable for {mins} min ({n} failed polls): {err}"),
        )
    } else if n >= WARN_AFTER as u64 {
        (
            "warn",
            format!("Telegram polls failing ({n} in a row, {mins} min): {err}"),
        )
    } else {
        (
            "ok",
            format!("Telegram reachable ({n} transient poll failure(s))"),
        )
    };
    serde_json::json!({
        "level": level,
        "message": message,
        "consecutive_failures": n,
        "failing_since": raw["failing_since"],
        "last_ok": raw["last_ok"],
    })
}

/// Split Markdown into chunks ≤ `limit` bytes, preferring paragraph breaks,
/// then line breaks, outside code fences. A fence cut across chunks is closed
/// at the end of one chunk and reopened (same info string) in the next, so
/// each chunk converts to valid HTML on its own.
pub fn split_markdown(text: &str, limit: usize) -> Vec<String> {
    let limit = limit.max(64);
    if text.len() <= limit {
        return vec![text.to_string()];
    }
    let mut chunks = Vec::new();
    let mut cur = String::new();
    let mut fence: Option<String> = None; // opening line when inside a fence
    let mut fence_at_chunk_start: Option<String> = None;
    let close_cost = |f: &Option<String>| if f.is_some() { 4 } else { 0 };

    let flush = |cur: &mut String, chunks: &mut Vec<String>, fence: &Option<String>| {
        let mut c = std::mem::take(cur);
        while c.ends_with('\n') {
            c.pop();
        }
        if fence.is_some() {
            c.push_str("\n```");
        }
        if !c.trim().is_empty() {
            chunks.push(c);
        }
    };

    // Paragraph-aware: accumulate lines, remember the last blank-line split point.
    let lines: Vec<&str> = text.split('\n').collect();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        let add = line.len() + 1;
        if !cur.is_empty() && cur.len() + add + close_cost(&fence) > limit {
            // Prefer the last paragraph break outside a fence.
            let cut = if fence.is_none() {
                para_cut_outside_fence(&cur, limit / 3)
            } else {
                None
            };
            if let Some(p) = cut {
                let rest = cur[p + 2..].to_string();
                cur.truncate(p);
                flush(&mut cur, &mut chunks, &None);
                cur = rest;
                let _ = fence_at_chunk_start.take();
                continue;
            }
            flush(&mut cur, &mut chunks, &fence);
            if let Some(open) = &fence {
                cur.push_str(open);
                cur.push('\n');
                fence_at_chunk_start = Some(open.clone());
            }
            continue;
        }
        if line.len() + close_cost(&fence) + 8 > limit {
            // Single over-long line: hard split on char boundaries.
            let mut rest = line;
            while !rest.is_empty() {
                let room = limit
                    .saturating_sub(cur.len() + close_cost(&fence) + 1)
                    .max(1);
                let mut end = room.min(rest.len());
                while end > 0 && !rest.is_char_boundary(end) {
                    end -= 1;
                }
                if end == 0 {
                    flush(&mut cur, &mut chunks, &fence);
                    if let Some(open) = &fence {
                        cur.push_str(open);
                        cur.push('\n');
                    }
                    continue;
                }
                cur.push_str(&rest[..end]);
                rest = &rest[end..];
                if !rest.is_empty() {
                    flush(&mut cur, &mut chunks, &fence);
                    if let Some(open) = &fence {
                        cur.push_str(open);
                        cur.push('\n');
                    }
                }
            }
            cur.push('\n');
            i += 1;
            continue;
        }
        if line.trim_start().starts_with("```") {
            fence = if fence.is_some() {
                None
            } else {
                Some(line.trim_start().to_string())
            };
        }
        cur.push_str(line);
        cur.push('\n');
        i += 1;
    }
    flush(&mut cur, &mut chunks, &None);
    chunks
}

/// Last `\n\n` at byte > `min` that is not inside a code fence.
fn para_cut_outside_fence(cur: &str, min: usize) -> Option<usize> {
    let mut best = None;
    let mut in_fence = false;
    let mut pos = 0;
    for line in cur.split_inclusive('\n') {
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
        }
        pos += line.len();
        // a blank line following this one => paragraph break at pos-1
        if !in_fence && cur[pos..].starts_with('\n') && pos > min {
            best = Some(pos - 1);
        }
    }
    best
}

/// True when a reply should go out as a document instead of messages.
pub fn should_send_document(chunks: usize) -> bool {
    chunks > MAX_CHUNKS
}

/// Build a multipart/form-data body for sendDocument.
pub fn multipart_document(
    chat_id: i64,
    filename: &str,
    content: &[u8],
    caption: &str,
) -> (String, Vec<u8>) {
    let boundary = format!("atr{:x}", Utc::now().timestamp_nanos_opt().unwrap_or(0));
    let mut b: Vec<u8> = Vec::new();
    let mut field = |name: &str, val: &str| {
        b.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{val}\r\n"
            )
            .as_bytes(),
        );
    };
    field("chat_id", &chat_id.to_string());
    if !caption.is_empty() {
        field("caption", caption);
    }
    b.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"document\"; filename=\"{filename}\"\r\nContent-Type: text/markdown\r\n\r\n"
        )
        .as_bytes(),
    );
    b.extend_from_slice(content);
    b.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    (format!("multipart/form-data; boundary={boundary}"), b)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn t(m: i64) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 1, 0, 0, 0).unwrap() + chrono::Duration::minutes(m)
    }

    #[test]
    fn warn_only_after_three_then_info_on_recovery() {
        let mut h = PollHealth::default();
        assert_eq!(h.record(false, Some("timed out"), t(0)).0, LogLevel::Debug);
        assert_eq!(h.record(false, Some("timed out"), t(1)).0, LogLevel::Debug);
        assert_eq!(h.record(false, Some("timed out"), t(2)).0, LogLevel::Warn);
        assert_eq!(h.record(false, Some("timed out"), t(3)).0, LogLevel::Debug);
        let (l, ev, _) = h.record(true, None, t(4));
        assert_eq!(l, LogLevel::Info);
        assert!(ev.is_none());
        // single blip recovers quietly
        h.record(false, Some("x"), t(5));
        assert_eq!(h.record(true, None, t(6)).0, LogLevel::Debug);
    }

    #[test]
    fn unreachable_after_15_min_once_then_recovered() {
        let mut h = PollHealth::default();
        assert!(h.record(false, Some("e"), t(0)).1.is_none());
        assert!(h.record(false, Some("e"), t(14)).1.is_none());
        assert_eq!(
            h.record(false, Some("e"), t(15)).1,
            Some(HealthEvent::Unreachable)
        );
        assert!(h.record(false, Some("e"), t(30)).1.is_none());
        assert_eq!(h.record(true, None, t(31)).1, Some(HealthEvent::Recovered));
        assert_eq!(HealthEvent::Recovered.kind(), "telegram_recovered");
    }

    #[test]
    fn summarize_levels() {
        assert_eq!(
            summarize(&serde_json::Value::Null, t(0))["level"],
            "unknown"
        );
        let ok = serde_json::json!({"consecutive_failures": 0});
        assert_eq!(summarize(&ok, t(0))["level"], "ok");
        let warn = serde_json::json!({"consecutive_failures": 3, "failing_since": t(0).to_rfc3339(), "last_error": "timed out"});
        assert_eq!(summarize(&warn, t(5))["level"], "warn");
        assert_eq!(summarize(&warn, t(20))["level"], "fail");
        let blip =
            serde_json::json!({"consecutive_failures": 1, "failing_since": t(0).to_rfc3339()});
        assert_eq!(summarize(&blip, t(1))["level"], "ok");
    }

    #[test]
    fn split_short_is_identity() {
        assert_eq!(split_markdown("hi", 100), vec!["hi".to_string()]);
    }

    #[test]
    fn split_prefers_paragraphs_and_respects_limit() {
        let para = "word ".repeat(30);
        let text = (0..20)
            .map(|_| para.trim())
            .collect::<Vec<_>>()
            .join("\n\n");
        let chunks = split_markdown(&text, 500);
        assert!(chunks.len() > 1);
        for c in &chunks {
            assert!(c.len() <= 500, "{}", c.len());
            assert!(!c.starts_with('\n') && !c.ends_with('\n'));
        }
        assert_eq!(
            chunks
                .join("\n\n")
                .replace("\n\n", " ")
                .split_whitespace()
                .count(),
            600
        );
    }

    #[test]
    fn split_keeps_code_fences_balanced() {
        let mut text = String::from("intro\n\n```rust\n");
        for i in 0..200 {
            text.push_str(&format!("let x{i} = {i};\n"));
        }
        text.push_str("```\n\noutro");
        let chunks = split_markdown(&text, 600);
        assert!(chunks.len() > 2);
        for c in &chunks {
            assert!(c.len() <= 600, "{}", c.len());
            let fences = c
                .lines()
                .filter(|l| l.trim_start().starts_with("```"))
                .count();
            assert_eq!(fences % 2, 0, "unbalanced chunk:\n{c}");
            let html = crate::telegram::markdown_to_telegram_html(c);
            assert_eq!(
                html.matches("<pre>").count(),
                html.matches("</pre>").count()
            );
        }
        assert!(chunks[1].starts_with("```rust"));
    }

    #[test]
    fn split_hard_splits_long_line_on_char_boundary() {
        let text = "é".repeat(1000);
        let chunks = split_markdown(&text, 300);
        assert!(chunks.iter().all(|c| c.len() <= 300));
        assert_eq!(chunks.concat().chars().count(), 1000);
    }

    #[test]
    fn document_threshold_and_multipart() {
        assert!(!should_send_document(4));
        assert!(should_send_document(5));
        let (ct, body) = multipart_document(42, "reply.md", b"# hi", "cap");
        assert!(ct.starts_with("multipart/form-data; boundary="));
        let s = String::from_utf8(body).unwrap();
        assert!(
            s.contains("name=\"chat_id\"\r\n\r\n42")
                && s.contains("filename=\"reply.md\"")
                && s.contains("# hi")
        );
    }

    #[test]
    fn conflict_409_explained() {
        let now = chrono::Utc::now();
        let raw = serde_json::json!({"consecutive_failures": 3, "failing_since": now.to_rfc3339(), "last_error": "https://api.telegram.org/bot***/getUpdates: status code 409"});
        let v = summarize(&raw, now);
        assert!(v["message"].as_str().unwrap().contains("another poller"));
        assert!(!is_conflict("timed out"));
    }
}
