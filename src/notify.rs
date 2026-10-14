use crate::config::TelegramConfig;

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
    let send = ureq::post(&url).set("User-Agent", "atr").send_json(payload);
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
}
