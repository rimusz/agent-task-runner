//! Transient provider-error retries: classify, back off, persist.
//!
//! Design (not code) from shepherdr's live retry: errors that clear on their
//! own are waited out; lasting errors go to the operator (or existing failover)
//! at once. Quota with a parsed reset stays in `quota.rs`.

use crate::clock::Clock;
use crate::config::{Provider, RetryConfig};
use crate::db::Task;
use crate::providers::ErrorClass;
use crate::quota;
use crate::runner::RunResult;
use chrono::{DateTime, Utc};
use std::time::Duration;

/// Why a run failed, for a status line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Class {
    /// Clears on its own: wait and try again.
    pub transient: bool,
    pub reason: String,
}

/// Classify a finished run. `has_quota_reset` is true when `quota.rs` already
/// parsed a reset time (those waits are handled there, not here).
pub fn classify(
    outcome: &str,
    classified: Option<ErrorClass>,
    text: &str,
    has_quota_reset: bool,
) -> Class {
    let t = text.to_ascii_lowercase();
    if matches!(classified, Some(ErrorClass::Auth)) || outcome == "auth" || looks_like_auth(&t) {
        return Class {
            transient: false,
            reason: "signed out".into(),
        };
    }
    if outcome == "needs_attention" || looks_like_invalid(&t) {
        return Class {
            transient: false,
            reason: if looks_like_invalid(&t) {
                "invalid request".into()
            } else {
                "task needs attention".into()
            },
        };
    }
    // Limit with a known reset is already parked by quota.rs.
    if (matches!(classified, Some(ErrorClass::Limit)) || outcome == "limit") && has_quota_reset {
        return Class {
            transient: false,
            reason: "quota reset known".into(),
        };
    }
    if looks_like_capacity(&t) {
        return Class {
            transient: true,
            reason: capacity_reason(&t),
        };
    }
    if matches!(classified, Some(ErrorClass::Server)) || looks_like_5xx(&t) {
        return Class {
            transient: true,
            reason: "provider server error".into(),
        };
    }
    if matches!(classified, Some(ErrorClass::Unavailable))
        || outcome == "unavailable"
        || looks_like_dropped(&t)
    {
        return Class {
            transient: true,
            reason: if looks_like_dropped(&t) {
                "connection dropped".into()
            } else {
                "provider unavailable".into()
            },
        };
    }
    // HTTP 429 / rate limit without a parsed reset: wait with backoff.
    if matches!(classified, Some(ErrorClass::Limit)) || outcome == "limit" || looks_like_429(&t) {
        return Class {
            transient: true,
            reason: "rate limited".into(),
        };
    }
    Class {
        transient: false,
        reason: "task failed".into(),
    }
}

pub fn classify_result(result: &RunResult, now: DateTime<Utc>) -> Class {
    let has_reset = result
        .usage_raw
        .iter()
        .any(|v| quota::extract_reset_at(v, now).is_some())
        || quota::reset_from_message(&result.stderr, now).is_some()
        || quota::reset_from_message(&result.snippet, now).is_some();
    let mut text = result.stderr.clone();
    if !result.snippet.is_empty() {
        text.push('\n');
        text.push_str(&result.snippet);
    }
    classify(&result.outcome, result.classified, &text, has_reset)
}

/// Delay before the next try. `attempt` is 1-based (first retry = 1).
pub fn backoff_delay(cfg: &RetryConfig, attempt: u32) -> Option<Duration> {
    if !cfg.enabled || attempt == 0 || attempt > cfg.max_attempts {
        return None;
    }
    if cfg.backoff.is_empty() {
        return None;
    }
    let idx = (attempt as usize)
        .saturating_sub(1)
        .min(cfg.backoff.len() - 1);
    Some(cfg.backoff[idx])
}

/// True when a queued task is waiting out a persisted retry.
pub fn waiting(task: &Task, clock: &Clock) -> bool {
    if task.retry_next_at.is_empty() {
        return false;
    }
    crate::db::ts_in_future(Some(&task.retry_next_at), clock.now())
}

pub fn continue_prompt(reason: &str) -> String {
    format!("Your last turn was cut off by the provider ({reason}). Continue where you left off.")
}

/// Session resume is possible when the provider has a resume flag (or a
/// kind that resumes without one) and we still have a session id.
pub fn can_resume(provider: &Provider, session: Option<&str>) -> bool {
    let sid = session.map(str::trim).filter(|s| !s.is_empty());
    if sid.is_none() {
        return false;
    }
    !provider.resume_flag.is_empty() || provider.kind == "codex" || provider.kind == "opencode"
}

fn looks_like_auth(t: &str) -> bool {
    t.contains("unauthorized")
        || t.contains("not logged in")
        || t.contains("authentication")
        || t.contains("invalid api key")
        || t.contains("missing api key")
        || t.contains("not authenticated")
        || t.contains("please run /login")
}

fn looks_like_invalid(t: &str) -> bool {
    t.contains("invalid argument")
        || t.contains("unknown flag")
        || t.contains("unexpected argument")
        || t.contains("model not found")
        || t.contains("does not exist")
        || t.contains("context window")
        || t.contains("context length")
}

fn looks_like_capacity(t: &str) -> bool {
    t.contains("overloaded")
        || t.contains("at capacity")
        || t.contains("model_cap")
        || t.contains(" 529")
        || t.contains("status 529")
}

fn capacity_reason(t: &str) -> String {
    if t.contains("at capacity") {
        "model at capacity".into()
    } else {
        "provider overloaded".into()
    }
}

fn looks_like_5xx(t: &str) -> bool {
    t.contains("internal server error")
        || t.contains("bad gateway")
        || t.contains("service unavailable")
        || t.contains(" 500")
        || t.contains(" 502")
        || t.contains(" 503")
        || t.contains("status 500")
        || t.contains("status 502")
        || t.contains("status 503")
        || t.contains("http 5")
}

fn looks_like_dropped(t: &str) -> bool {
    t.contains("connection reset")
        || t.contains("connection refused")
        || t.contains("connection dropped")
        || t.contains("broken pipe")
        || t.contains("timed out")
        || t.contains("timeout")
        || t.contains("dropped stream")
        || t.contains("stream ended")
        || t.contains("stream closed")
        || t.contains("stream disconnected")
        || t.contains("econnreset")
        || t.contains("etimedout")
}

fn looks_like_429(t: &str) -> bool {
    t.contains("too many requests") || t.contains(" 429") || t.contains("status 429")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::ErrorClass;

    fn cls(outcome: &str, classified: Option<ErrorClass>, text: &str, reset: bool) -> Class {
        classify(outcome, classified, text, reset)
    }

    #[test]
    fn transient_overloaded_5xx_429_and_dropped() {
        assert!(
            cls(
                "unavailable",
                Some(ErrorClass::Unavailable),
                "overloaded",
                false
            )
            .transient
        );
        assert_eq!(
            cls("failed", Some(ErrorClass::Server), "500", false).reason,
            "provider server error"
        );
        assert!(cls("limit", Some(ErrorClass::Limit), "HTTP 429", false).transient);
        assert!(cls("failed", None, "connection reset by peer", false).transient);
        assert!(cls("failed", None, "stream disconnected", false).transient);
        assert!(cls("failed", None, "ETIMEDOUT", false).transient);
        assert_eq!(
            cls("failed", None, "model at capacity", false).reason,
            "model at capacity"
        );
    }

    #[test]
    fn lasting_auth_quota_reset_invalid_and_task_logic() {
        assert!(!cls("auth", Some(ErrorClass::Auth), "", false).transient);
        assert!(!cls("limit", Some(ErrorClass::Limit), "quota", true).transient);
        assert_eq!(
            cls("limit", Some(ErrorClass::Limit), "quota", true).reason,
            "quota reset known"
        );
        assert!(!cls("failed", None, "unknown flag --foo", false).transient);
        assert!(!cls("failed", Some(ErrorClass::Other), "tests failed", false).transient);
        assert!(!cls("needs_attention", None, "", false).transient);
    }

    #[test]
    fn backoff_is_1_2_4_8_then_none() {
        let cfg = RetryConfig {
            enabled: true,
            max_attempts: 4,
            backoff: vec![
                Duration::from_secs(60),
                Duration::from_secs(120),
                Duration::from_secs(240),
                Duration::from_secs(480),
            ],
            backoff_raw: vec!["1m".into(), "2m".into(), "4m".into(), "8m".into()],
        };
        assert_eq!(backoff_delay(&cfg, 1), Some(Duration::from_secs(60)));
        assert_eq!(backoff_delay(&cfg, 2), Some(Duration::from_secs(120)));
        assert_eq!(backoff_delay(&cfg, 3), Some(Duration::from_secs(240)));
        assert_eq!(backoff_delay(&cfg, 4), Some(Duration::from_secs(480)));
        assert_eq!(backoff_delay(&cfg, 5), None);
        assert_eq!(backoff_delay(&cfg, 0), None);
        let off = RetryConfig {
            enabled: false,
            ..cfg.clone()
        };
        assert_eq!(backoff_delay(&off, 1), None);
    }

    #[test]
    fn waiting_uses_persisted_next_at() {
        let now = DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let clock = Clock::frozen(now);
        let mut task = Task {
            retry_next_at: "2026-01-01T00:02:00.000Z".into(),
            ..Task::default()
        };
        assert!(waiting(&task, &clock));
        task.retry_next_at = "2026-01-01T00:00:00.000Z".into();
        assert!(!waiting(&task, &clock));
        task.retry_next_at.clear();
        assert!(!waiting(&task, &clock));
    }

    #[test]
    fn resume_needs_session_and_flag() {
        let mut p = Provider::test_stub("grok");
        p.resume_flag = "--resume".into();
        assert!(can_resume(&p, Some("sid")));
        assert!(!can_resume(&p, Some("")));
        p.resume_flag.clear();
        p.kind = "claude".into();
        assert!(!can_resume(&p, Some("sid")));
        p.kind = "codex".into();
        assert!(can_resume(&p, Some("sid")));
    }
}
