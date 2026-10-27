//! Slash routing for the TUI host.
//!
//! atr-local: `/provider`, `/model`, `/new`, `/quit`, `/status`, `/help`,
//! `/cli`. Every other `/…` is forwarded to the selected provider CLI.
//! `/model` never becomes a chat turn.

use crate::app::Shared;
use crate::tui::tui_provider_now;

/// Atr-local host commands shown in the `/` autocomplete menu.
/// Order is the display order (Grok-style).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct HostCommand {
    pub name: &'static str,
    pub aliases: &'static [&'static str],
    pub hint: &'static str,
}

pub(crate) const HOST_COMMANDS: &[HostCommand] = &[
    HostCommand {
        name: "/provider",
        aliases: &[],
        hint: "list / pick atr providers",
    },
    HostCommand {
        name: "/model",
        aliases: &["/m"],
        hint: "session model (next --model)",
    },
    HostCommand {
        name: "/status",
        aliases: &[],
        hint: "session · provider · tokens",
    },
    HostCommand {
        name: "/new",
        aliases: &[],
        hint: "fresh atr session",
    },
    HostCommand {
        name: "/cli",
        aliases: &["/native"],
        hint: "open the provider's own TUI",
    },
    HostCommand {
        name: "/help",
        aliases: &["/?"],
        hint: "atr host commands",
    },
    HostCommand {
        name: "/quit",
        aliases: &["/q", "/exit"],
        hint: "leave atr",
    },
];

pub(crate) fn is_bare_slash(line: &str) -> bool {
    line.trim() == "/"
}

/// `/provider` and `/model` stay in overlays / the under-input strip.
/// Do not append them (or their confirmations) to the chat transcript.
pub(crate) fn quiet_transcript(line: &str) -> bool {
    matches!(
        parse_slash(line),
        Some(Slash::Provider(_)) | Some(Slash::Model(_))
    )
}

/// Prefix + fuzzy filter for the `/` menu. Empty when the line is not
/// an in-progress atr-local command (has args, or is a provider slash).
pub(crate) fn filter_host_commands(typed: &str) -> Vec<&'static HostCommand> {
    let q = typed.trim();
    if !q.starts_with('/') || q.contains(char::is_whitespace) {
        return Vec::new();
    }
    let mut hits: Vec<&HostCommand> = HOST_COMMANDS.iter().filter(|c| cmd_matches(c, q)).collect();
    hits.sort_by_key(|c| {
        let prefix = c.name.starts_with(q) || c.aliases.iter().any(|a| a.starts_with(q));
        (!prefix, c.name)
    });
    hits
}

fn cmd_matches(c: &HostCommand, q: &str) -> bool {
    if q == "/" {
        return true;
    }
    std::iter::once(c.name)
        .chain(c.aliases.iter().copied())
        .any(|n| n.starts_with(q) || fuzzy_subseq(n, q))
}

pub(crate) fn fuzzy_subseq(name: &str, q: &str) -> bool {
    let mut it = name.chars();
    for qc in q.chars() {
        loop {
            match it.next() {
                Some(c) if c.eq_ignore_ascii_case(&qc) => break,
                Some(_) => continue,
                None => return false,
            }
        }
    }
    true
}

pub(crate) fn filter_names(names: &[String], q: &str) -> Vec<String> {
    if q.is_empty() {
        return names.to_vec();
    }
    let ql = q.to_ascii_lowercase();
    names
        .iter()
        .filter(|n| {
            let nl = n.to_ascii_lowercase();
            nl.contains(&ql) || fuzzy_subseq(&nl, &ql)
        })
        .cloned()
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Slash {
    Help,
    Quit,
    Status,
    New,
    Provider(Option<String>),
    Model(Option<String>),
    Cli,
    /// Full line (`/compact`, `/plan`, …) for the provider proxy path.
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
        "/model" | "/m" => {
            let rest: Vec<&str> = parts.collect();
            Some(Slash::Model(if rest.is_empty() {
                None
            } else {
                Some(rest.join(" "))
            }))
        }
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

pub(crate) fn slash_help() -> &'static str {
    "\
atr tui — interactive session host

Chat
  Type a message and press Enter. Each turn runs the selected provider
  CLI headless (same engine as `atr ask`): stream-json / -p / exec, not
  that CLI's own full-screen TUI.

atr (this host)
  /provider [name]   list / pick atr providers (enabled · current)
  /model [id]        list / pick the session model (next spawn --model)
  /status            session, provider, model, tokens, cache
  /new               fresh atr session (same key)
  /cli               open the provider's own interactive TUI
  /quit              leave atr  (/exit /q)
  Ctrl+C             leave atr
  Esc                dismiss menus / pickers (never quits)

  `/model` is atr-local for every provider. It is never sent as chat.
  `/cli` is the full native experience (Grok/Claude's own /model picker).
  `/atr …` namespaces host commands if a provider slash collides.

Provider CLI slashes
  Other /commands (/compact, /plan, skills, …) go to the current
  provider on the next turn. Not /model.
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

#[derive(Debug, Clone)]
pub(crate) struct ModelChoice {
    pub name: String,
    pub tag: String,
    pub current: bool,
}

pub(crate) fn format_model_list(provider: &str, kind: &str, list: &[ModelChoice]) -> String {
    if list.is_empty() {
        return model_empty_hint(provider, kind);
    }
    let mut lines = vec![format!("Models for {provider} ({kind})  (* current)")];
    for (i, m) in list.iter().enumerate() {
        let mark = if m.current { "*" } else { " " };
        lines.push(format!("  {:>2}  {:<28} {mark}  {}", i + 1, m.name, m.tag));
    }
    lines.join("\n")
}

pub(crate) fn model_pick_usage(provider: &str, kind: &str, list: &[ModelChoice]) -> String {
    format!(
        "{}\n\nusage: /model <id or number>\n       atr providers model {provider} <id>\n       /cli opens {kind}'s own TUI",
        format_model_list(provider, kind, list)
    )
}

pub(crate) fn model_empty_hint(provider: &str, kind: &str) -> String {
    format!(
        "no models parsed — try /cli\n\
         {provider} ({kind}): atr does not invent a catalog.\n\
         Set this session: /model <id>\n\
         Persist in config: atr providers model {provider} <id>\n\
         Or /cli for {kind}'s own TUI picker."
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
        assert!(quiet_transcript("/provider"));
        assert!(quiet_transcript("/provider codex"));
        assert!(quiet_transcript("/model"));
        assert!(quiet_transcript("/model gpt-6.1-sol"));
        assert!(quiet_transcript("/m"));
        assert!(quiet_transcript("/atr provider"));
        assert!(quiet_transcript("/atr /provider grok"));
        assert!(quiet_transcript("/atr model"));
        assert!(quiet_transcript("/atr /model gpt-6.1-sol"));
        assert!(!quiet_transcript("/help"));
        assert!(!quiet_transcript("/status"));
        assert!(!quiet_transcript("/cli"));
        assert!(!quiet_transcript("hello"));
        assert_eq!(parse_slash("/model"), Some(Slash::Model(None)));
        assert_eq!(
            parse_slash("/model grok-code"),
            Some(Slash::Model(Some("grok-code".into())))
        );
        assert_eq!(parse_slash("/m"), Some(Slash::Model(None)));
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
        assert_eq!(parse_slash("/memory"), Some(Slash::Proxy("/memory".into())));
        assert_eq!(parse_slash("/mcps"), Some(Slash::Proxy("/mcps".into())));
        assert_eq!(
            parse_slash("/compact"),
            Some(Slash::Proxy("/compact".into()))
        );
    }

    #[test]
    fn help_has_host_and_proxy_sections() {
        let h = slash_help();
        assert!(h.contains("Chat"), "{h}");
        assert!(h.contains("Provider CLI"), "{h}");
        assert!(h.contains("/provider"), "{h}");
        assert!(h.contains("/model"), "{h}");
        assert!(h.contains("never sent as chat"), "{h}");
        assert!(h.contains("/cli"), "{h}");
        assert!(h.contains("Ctrl+C"), "{h}");
        assert!(!h.contains("unknown command"), "{h}");
        assert!(!h.contains("Claude's `/model` works in print mode"), "{h}");
    }

    #[test]
    fn slash_menu_filters_atr_local_only() {
        let all = filter_host_commands("/");
        assert_eq!(all.len(), HOST_COMMANDS.len(), "{all:?}");
        assert!(all.iter().any(|c| c.name == "/model"));
        assert!(all.iter().any(|c| c.name == "/provider"));
        assert!(all.iter().any(|c| c.name == "/quit"));

        let m = filter_host_commands("/m");
        assert!(m.iter().any(|c| c.name == "/model"), "{m:?}");
        assert!(m.iter().all(|c| c.name == "/model"), "{m:?}");

        let q = filter_host_commands("/q");
        assert!(q.iter().any(|c| c.name == "/quit"), "{q:?}");

        let fuzzy = filter_host_commands("/prv");
        assert!(fuzzy.iter().any(|c| c.name == "/provider"), "{fuzzy:?}");

        assert!(filter_host_commands("/compact").is_empty());
        assert!(filter_host_commands("/model grok").is_empty());
        assert!(filter_host_commands("hello").is_empty());
        assert!(is_bare_slash(" / "));
        assert!(!is_bare_slash("/model"));
    }

    #[test]
    fn filter_names_type_to_find() {
        let names = vec![
            "grok-4.7".into(),
            "grok-4.7-build-fast".into(),
            "sonnet".into(),
        ];
        assert_eq!(filter_names(&names, "").len(), 3);
        let grok = filter_names(&names, "grok-4.7");
        assert!(grok.iter().any(|s| s == "grok-4.7"));
        assert!(grok.iter().any(|s| s == "grok-4.7-build-fast"));
        assert!(!grok.iter().any(|s| s == "sonnet"));
        assert_eq!(filter_names(&names, "son").as_slice(), ["sonnet"]);
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
