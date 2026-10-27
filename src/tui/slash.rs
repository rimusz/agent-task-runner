//! Slash routing for the TUI host.
//!
//! atr owns a short list of session commands. Every other `/…` line is
//! forwarded to the selected provider CLI (headless turn, or that CLI's
//! non-interactive control). atr never reports "unknown command" for a
//! slash — that was a fake second dialect.

use crate::app::Shared;
use crate::tui::tui_provider_now;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Slash {
    Help,
    Quit,
    Status,
    New,
    Provider(Option<String>),
    Cli,
    /// Full line (`/model`, `/compact`, …) for the provider / proxy path.
    Proxy(String),
}

pub(crate) fn parse_slash(line: &str) -> Option<Slash> {
    let raw = line.trim();
    if !raw.starts_with('/') {
        return None;
    }
    let t = strip_atr_ns(raw);
    let mut parts = t.split_whitespace();
    let cmd = parts.next()?;
    match cmd {
        "/help" | "/?" => Some(Slash::Help),
        "/quit" | "/exit" | "/q" => Some(Slash::Quit),
        "/status" => Some(Slash::Status),
        "/new" => Some(Slash::New),
        "/provider" => Some(Slash::Provider(parts.next().map(str::to_string))),
        "/cli" | "/native" => Some(Slash::Cli),
        _ => Some(Slash::Proxy(t.to_string())),
    }
}

/// `/atr provider grok` and `/atr /provider grok` are the namespaced forms.
fn strip_atr_ns(raw: &str) -> String {
    let Some(rest) = raw.strip_prefix("/atr") else {
        return raw.to_string();
    };
    let rest = rest.trim();
    if rest.is_empty() {
        return "/help".into();
    }
    if rest.starts_with('/') {
        rest.to_string()
    } else {
        format!("/{rest}")
    }
}

/// `/model` / `/m` (Grok alias). `/memory` and `/mcps` are not matches.
pub(crate) fn model_slash(line: &str) -> Option<Option<String>> {
    let mut parts = line.split_whitespace();
    let cmd = parts.next()?;
    match cmd {
        "/model" | "/m" => {
            let rest: Vec<&str> = parts.collect();
            if rest.is_empty() {
                Some(None)
            } else {
                Some(Some(rest.join(" ")))
            }
        }
        _ => None,
    }
}

pub(crate) fn slash_help() -> &'static str {
    "\
atr tui — interactive session host

Chat
  Type a message and press Enter. Each turn runs the selected provider
  CLI headless (same engine as `atr ask`): stream-json / -p / exec, not
  that CLI's own full-screen TUI.

Provider CLI slashes
  Other /commands are sent to the current provider on the next turn.
  Claude's `/model` works in print mode. Grok's `/model` picker is
  TUI-only — `grok models` lists ids, `/model <id>` maps to `--model`,
  `/cli` opens grok itself. Skills and `/compact` / `/plan` / … follow
  the same rule: forwarded, or use `/cli` when the picker is TUI-only.

atr (this host)
  /provider [name]   list / pick atr providers (enabled · current)
  /status            session, provider, model, tokens, cache
  /new               fresh atr session (same key)
  /cli               open the provider's own interactive TUI
  /quit              leave atr  (/exit /q)

  `/atr …` is the same atr commands if a provider slash collides.
"
}

#[derive(Debug, Clone)]
pub(crate) struct ProviderChoice {
    pub name: String,
    pub kind: String,
    pub enabled: bool,
    pub current: bool,
}

pub(crate) fn list_providers(shared: &Shared, key: &str) -> Vec<ProviderChoice> {
    let cfg = shared.config.read().unwrap().clone();
    let current = tui_provider_now(shared, key).or_else(|| {
        cfg.providers
            .iter()
            .find(|p| p.enabled)
            .map(|p| p.name.clone())
    });
    cfg.providers
        .iter()
        .map(|p| ProviderChoice {
            name: p.name.clone(),
            kind: p.kind.clone(),
            enabled: p.enabled,
            current: current.as_deref() == Some(p.name.as_str()),
        })
        .collect()
}

pub(crate) fn format_provider_list(list: &[ProviderChoice]) -> String {
    if list.is_empty() {
        return "no providers in config. atr init / atr providers detect".into();
    }
    let mut lines = vec!["Providers  (* current)".to_string()];
    for (i, p) in list.iter().enumerate() {
        let mark = if p.current { "*" } else { " " };
        let state = if p.enabled { "enabled" } else { "disabled" };
        lines.push(format!(
            "  {:>2}  {:<16} {state:<8} {mark}  {}",
            i + 1,
            p.name,
            p.kind
        ));
    }
    lines.join("\n")
}

pub(crate) fn pick_choice(names: &[String], input: &str) -> Option<String> {
    let t = input.trim();
    if t.is_empty() {
        return None;
    }
    if let Ok(n) = t.parse::<usize>() {
        if n >= 1 && n <= names.len() {
            return Some(names[n - 1].clone());
        }
    }
    if let Some(exact) = names.iter().find(|n| n.eq_ignore_ascii_case(t)) {
        return Some(exact.clone());
    }
    let lower = t.to_ascii_lowercase();
    let hits: Vec<&String> = names
        .iter()
        .filter(|n| n.to_ascii_lowercase().starts_with(&lower))
        .collect();
    if hits.len() == 1 {
        return Some(hits[0].clone());
    }
    None
}

pub(crate) fn provider_pick_usage(list: &[ProviderChoice]) -> String {
    format!(
        "{}\n\nusage: /provider <name or number>",
        format_provider_list(list)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_slashes_never_unknown() {
        assert_eq!(parse_slash("hello"), None);
        assert_eq!(parse_slash("/quit"), Some(Slash::Quit));
        assert_eq!(parse_slash("/q"), Some(Slash::Quit));
        assert_eq!(parse_slash("/help"), Some(Slash::Help));
        assert_eq!(parse_slash("/provider"), Some(Slash::Provider(None)));
        assert_eq!(
            parse_slash("/provider grok"),
            Some(Slash::Provider(Some("grok".into())))
        );
        assert_eq!(parse_slash("/model"), Some(Slash::Proxy("/model".into())));
        assert_eq!(
            parse_slash("/model grok-code"),
            Some(Slash::Proxy("/model grok-code".into()))
        );
        assert_eq!(
            parse_slash("/compact"),
            Some(Slash::Proxy("/compact".into()))
        );
        assert_eq!(parse_slash("/nope"), Some(Slash::Proxy("/nope".into())));
        assert_eq!(
            parse_slash("/atr provider grok"),
            Some(Slash::Provider(Some("grok".into())))
        );
    }

    #[test]
    fn model_slash_does_not_eat_memory() {
        assert_eq!(model_slash("/model"), Some(None));
        assert_eq!(model_slash("/m"), Some(None));
        assert_eq!(
            model_slash("/model grok-code"),
            Some(Some("grok-code".into()))
        );
        assert_eq!(model_slash("/memory"), None);
        assert_eq!(model_slash("/mcps"), None);
        assert_eq!(model_slash("/compact"), None);
    }

    #[test]
    fn help_has_host_and_proxy_sections() {
        let h = slash_help();
        assert!(h.contains("Chat"), "{h}");
        assert!(h.contains("Provider CLI"), "{h}");
        assert!(h.contains("/provider"), "{h}");
        assert!(h.contains("/cli"), "{h}");
        assert!(!h.contains("unknown command"), "{h}");
    }

    #[test]
    fn pick_choice_number_and_name() {
        let names = vec!["claude".into(), "grok".into()];
        assert_eq!(pick_choice(&names, "2").as_deref(), Some("grok"));
        assert_eq!(pick_choice(&names, "GROK").as_deref(), Some("grok"));
        assert_eq!(pick_choice(&names, "cl").as_deref(), Some("claude"));
        assert_eq!(pick_choice(&names, "9"), None);
        assert_eq!(pick_choice(&names, ""), None);
    }
}
