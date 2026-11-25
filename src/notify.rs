use crate::config::{Config, TelegramConfig, TincanConfig};
use crate::db::Task;
use anyhow::{bail, Result};

/// Stable Telegram notification kinds. `[telegram] events` filters these.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Event {
    Start,
    Finished,
    VerifyRetry,
    Failed,
    NeedsAttention,
    Stalled,
    Fallback,
    Cancelled,
    Retry,
    Held,
    Planned,
    Review,
    QuotaSkip,
    Compare,
    Hook,
    /// Run wall-clock cap (`run_timeout`). Never a stall / CLI hang.
    CapReached,
}

impl Event {
    pub const ALL: &[Event] = &[
        Event::Start,
        Event::Finished,
        Event::VerifyRetry,
        Event::Failed,
        Event::NeedsAttention,
        Event::Stalled,
        Event::Fallback,
        Event::Cancelled,
        Event::Retry,
        Event::Held,
        Event::Planned,
        Event::Review,
        Event::QuotaSkip,
        Event::Compare,
        Event::Hook,
        Event::CapReached,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Event::Start => "start",
            Event::Finished => "finished",
            Event::VerifyRetry => "verify_retry",
            Event::Failed => "failed",
            Event::NeedsAttention => "needs_attention",
            Event::Stalled => "stalled",
            Event::Fallback => "fallback",
            Event::Cancelled => "cancelled",
            Event::Retry => "retry",
            Event::Held => "held",
            Event::Planned => "planned",
            Event::Review => "review",
            Event::QuotaSkip => "quota_skip",
            Event::Compare => "compare",
            Event::Hook => "hook",
            Event::CapReached => "cap_reached",
        }
    }

    pub fn parse(name: &str) -> Result<Self> {
        Self::parse_in(name, "telegram.events")
    }

    pub fn parse_in(name: &str, field: &str) -> Result<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "start" => Ok(Event::Start),
            "finished" => Ok(Event::Finished),
            "verify_retry" => Ok(Event::VerifyRetry),
            "failed" => Ok(Event::Failed),
            "needs_attention" => Ok(Event::NeedsAttention),
            "stalled" => Ok(Event::Stalled),
            "fallback" => Ok(Event::Fallback),
            "cancelled" => Ok(Event::Cancelled),
            "retry" => Ok(Event::Retry),
            "held" => Ok(Event::Held),
            "planned" => Ok(Event::Planned),
            "review" => Ok(Event::Review),
            "quota_skip" => Ok(Event::QuotaSkip),
            "compare" => Ok(Event::Compare),
            "hook" => Ok(Event::Hook),
            "cap_reached" => Ok(Event::CapReached),
            other => bail!(
                "unknown {field} value {other:?}; expected one of: {}",
                Event::ALL
                    .iter()
                    .map(|e| e.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }
}

/// Default `[notify.tincan] events` when the list is omitted or empty.
pub const TINCAN_DEFAULT_EVENTS: &[Event] = &[
    Event::Finished,
    Event::Failed,
    Event::NeedsAttention,
    Event::Stalled,
    Event::CapReached,
    Event::Review,
    Event::Held,
    Event::Cancelled,
];

pub fn default_tincan_event_names() -> Vec<String> {
    TINCAN_DEFAULT_EVENTS
        .iter()
        .map(|e| e.as_str().to_string())
        .collect()
}

/// Canonicalize `[telegram] events`. Empty input stays empty (send every event).
pub fn parse_event_names(raw: &[String]) -> Result<Vec<String>> {
    parse_event_names_in(raw, "telegram.events")
}

pub fn parse_event_names_in(raw: &[String], field: &str) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for name in raw {
        let name = name.trim();
        if name.is_empty() {
            continue;
        }
        let event = Event::parse_in(name, field)?;
        let canon = event.as_str().to_string();
        if !out.iter().any(|e| e == &canon) {
            out.push(canon);
        }
    }
    Ok(out)
}

pub fn names_allow(events: &[String], event: Event) -> bool {
    if events.is_empty() || events.iter().any(|e| e == event.as_str()) {
        return true;
    }
    // Existing configs that list `stalled` (Mini) still receive cap alerts.
    // The Telegram text is never stall / CLI hang / heartbeat.
    event == Event::CapReached && events.iter().any(|e| e == Event::Stalled.as_str())
}

pub fn allows_event(cfg: &TelegramConfig, event: Event) -> bool {
    names_allow(&cfg.events, event)
}

pub fn tincan_allows(cfg: &TincanConfig, event: Event) -> bool {
    // Parse fills the default set; empty here still means that subset,
    // not Telegram's empty-means-all.
    if cfg.events.is_empty() {
        return names_allow(&default_tincan_event_names(), event);
    }
    names_allow(&cfg.events, event)
}

struct TelegramDest {
    token: String,
    chat_id: String,
}

fn dest(cfg: &TelegramConfig) -> Option<TelegramDest> {
    let token = crate::telegram::bot_token(cfg)?;
    let chat_id = crate::telegram::allowed_chats(cfg)
        .first()
        .map(|id| id.to_string())?;
    Some(TelegramDest { token, chat_id })
}

pub fn notify(cfg: &TelegramConfig, body: &str) {
    notify_unless_shadow(cfg, body, false);
}

/// Task-lifecycle notify. Empty `[telegram] events` sends everything (today).
/// Token-expiry, Telegram health, the daily summary, and command replies do
/// not go through this path and stay unfiltered.
pub fn notify_event(cfg: &TelegramConfig, event: Event, body: &str, shadow: bool) {
    if !allows_event(cfg, event) {
        tracing::debug!(event = event.as_str(), "telegram event filtered");
        return;
    }
    notify_unless_shadow(cfg, body, shadow);
}

/// Telegram plus `[notify.tincan]`. One call site so a task event cannot
/// reach Telegram and skip Tincan.
pub fn emit(cfg: &Config, event: Event, body: &str, shadow: bool, task: Option<&Task>) {
    notify_event(&cfg.telegram, event, body, shadow);
    crate::tincan::notify(cfg, event, body, shadow, task);
}

pub fn notify_unless_shadow(cfg: &TelegramConfig, body: &str, shadow: bool) {
    if shadow {
        tracing::debug!("shadow task; skip notify");
        return;
    }
    if cfg.enabled {
        crate::telegram::enqueue_notify(cfg, body);
        return;
    }
    if !configured(cfg) {
        tracing::debug!("telegram not configured; skip notify");
        return;
    }
    let Some(d) = dest(cfg) else {
        return;
    };
    let prefix = if cfg.prefix.is_empty() {
        "atr:"
    } else {
        cfg.prefix.as_str()
    };
    let text = crate::telegram::sanitize(&format!("{prefix} {body}"), &[&d.token]);
    let url = format!(
        "{}/bot{}/sendMessage",
        crate::telegram::api_base(cfg),
        d.token
    );
    let payload = serde_json::json!({
        "chat_id": d.chat_id,
        "text": text,
        "disable_web_page_preview": true,
    });
    let send = crate::util::ureq_agent(std::time::Duration::from_secs(10))
        .post(&url)
        .send_json(payload);
    if let Err(e) = send {
        tracing::warn!(
            "telegram notify failed: {}",
            redact(&e.to_string(), &d.token)
        );
    } else {
        tracing::info!("telegram notify sent");
    }
}

fn redact(s: &str, secret: &str) -> String {
    if secret.is_empty() {
        return s.to_string();
    }
    s.replace(secret, "***")
}

pub fn configured(cfg: &TelegramConfig) -> bool {
    if cfg.enabled {
        crate::telegram::bot_token(cfg).is_some() && !crate::telegram::allowed_chats(cfg).is_empty()
    } else {
        dest(cfg).is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::TelegramConfig;

    #[test]
    fn dest_uses_allowed_chat_ids_only() {
        std::env::set_var("ATR_NOTIFY_TEST_TOKEN", "tok");
        std::env::set_var("ATR_TELEGRAM_CHAT_ID", "99");
        let mut cfg = TelegramConfig {
            enabled: false,
            bot_token_env: "ATR_NOTIFY_TEST_TOKEN".into(),
            allowed_chat_ids: vec![],
            ..TelegramConfig::default()
        };
        assert!(dest(&cfg).is_none());
        cfg.allowed_chat_ids = vec![42];
        let d = dest(&cfg).unwrap();
        assert_eq!(d.chat_id, "42");
    }

    #[test]
    fn event_parse_accepts_known_names() {
        assert_eq!(Event::parse("failed").unwrap(), Event::Failed);
        assert_eq!(
            Event::parse("Needs_Attention").unwrap(),
            Event::NeedsAttention
        );
        assert_eq!(Event::parse("verify_retry").unwrap(), Event::VerifyRetry);
        assert_eq!(Event::ALL.len(), 16);
        let err = Event::parse("boom").unwrap_err().to_string();
        assert!(err.contains("unknown telegram.events"), "{err}");
        assert!(err.contains("failed"), "{err}");
    }

    #[test]
    fn empty_events_allows_every_kind() {
        let cfg = TelegramConfig::default();
        assert!(cfg.events.is_empty());
        for e in Event::ALL {
            assert!(allows_event(&cfg, *e), "{}", e.as_str());
        }
    }

    #[test]
    fn nonempty_events_filters() {
        let cfg = TelegramConfig {
            events: parse_event_names(&["failed".into(), "stalled".into()]).unwrap(),
            ..TelegramConfig::default()
        };
        assert!(allows_event(&cfg, Event::Failed));
        assert!(allows_event(&cfg, Event::Stalled));
        assert!(
            allows_event(&cfg, Event::CapReached),
            "stalled in events still admits cap_reached so existing filters keep working"
        );
        assert!(!allows_event(&cfg, Event::Start));
        assert!(!allows_event(&cfg, Event::Finished));
        assert!(!allows_event(&cfg, Event::Hook));
    }

    #[test]
    fn cap_reached_is_its_own_event() {
        assert_eq!(Event::parse("cap_reached").unwrap(), Event::CapReached);
        assert_eq!(Event::CapReached.as_str(), "cap_reached");
        let cfg = TelegramConfig {
            events: parse_event_names(&["failed".into(), "cap_reached".into()]).unwrap(),
            ..TelegramConfig::default()
        };
        assert!(allows_event(&cfg, Event::CapReached));
        assert!(!allows_event(&cfg, Event::Stalled));
    }

    #[test]
    fn tincan_empty_events_use_default_set_not_allow_all() {
        let cfg = TincanConfig {
            events: vec![],
            ..TincanConfig::default()
        };
        assert!(tincan_allows(&cfg, Event::Failed));
        assert!(tincan_allows(&cfg, Event::Finished));
        assert!(!tincan_allows(&cfg, Event::Start));
        assert!(!tincan_allows(&cfg, Event::Hook));
    }
}
