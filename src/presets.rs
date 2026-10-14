//! Built-in OpenCode presets and optional config `[[preset]]` mappings.
//!
//! API keys never go in config. Built-ins that need a key store it in
//! `shadow.env`; base URL + `{env:VAR}` go in `~/.config/opencode/opencode.json`.
//! Claude, Codex, Cursor, and Grok stay on their own CLIs.

use crate::config::{is_known_kind, Config, Preset};

/// Built-in `atr providers add <name>` mapping (OpenCode + Chinese / local).
#[derive(Debug, Clone, Copy)]
pub struct BuiltinPreset {
    pub name: &'static str,
    pub kind: &'static str,
    pub model: &'static str,
    pub oc_provider: &'static str,
    pub display: &'static str,
    pub base_url: &'static str,
    pub env_key: Option<&'static str>,
    pub needs_url: bool,
}

pub const BUILTINS: &[BuiltinPreset] = &[
    BuiltinPreset {
        name: "kimi",
        kind: "opencode",
        model: "moonshotai/kimi-k2.6",
        oc_provider: "moonshotai",
        display: "Moonshot Kimi",
        base_url: "https://api.moonshot.ai/v1",
        env_key: Some("MOONSHOT_API_KEY"),
        needs_url: false,
    },
    BuiltinPreset {
        name: "qwen",
        kind: "opencode",
        model: "qwen/qwen3.5-plus",
        oc_provider: "qwen",
        display: "Qwen (DashScope)",
        base_url: "https://dashscope.aliyuncs.com/compatible-mode/v1",
        env_key: Some("DASHSCOPE_API_KEY"),
        needs_url: false,
    },
    BuiltinPreset {
        name: "glm",
        kind: "opencode",
        model: "zai/glm-4.6",
        oc_provider: "zai",
        display: "Z.ai GLM",
        base_url: "https://api.z.ai/api/paas/v4",
        env_key: Some("ZAI_API_KEY"),
        needs_url: false,
    },
    BuiltinPreset {
        name: "deepseek",
        kind: "opencode",
        model: "deepseek/deepseek-chat",
        oc_provider: "deepseek",
        display: "DeepSeek",
        base_url: "https://api.deepseek.com/v1",
        env_key: Some("DEEPSEEK_API_KEY"),
        needs_url: false,
    },
    BuiltinPreset {
        name: "minimax",
        kind: "opencode",
        model: "minimax/minimax-m2.5",
        oc_provider: "minimax",
        display: "MiniMax",
        base_url: "https://api.minimax.io/v1",
        env_key: Some("MINIMAX_API_KEY"),
        needs_url: false,
    },
    BuiltinPreset {
        name: "mimo",
        kind: "opencode",
        model: "xiaomi/mimo-v2-flash",
        oc_provider: "xiaomi",
        display: "Xiaomi MiMo",
        base_url: "https://api.xiaomimimo.com/v1",
        env_key: Some("MIMO_API_KEY"),
        needs_url: false,
    },
    BuiltinPreset {
        name: "ollama",
        kind: "opencode",
        model: "ollama/llama3.2",
        oc_provider: "ollama",
        display: "Ollama",
        base_url: "http://127.0.0.1:11434/v1",
        env_key: None,
        needs_url: true,
    },
    BuiltinPreset {
        name: "spark",
        kind: "opencode",
        model: "spark/deepseek-v4-flash-0731",
        oc_provider: "spark",
        display: "spark (local)",
        base_url: "http://127.0.0.1:8091/v1",
        env_key: None,
        needs_url: true,
    },
];

pub fn builtin(name: &str) -> Option<&'static BuiltinPreset> {
    BUILTINS.iter().find(|p| p.name == name)
}

pub fn builtin_names() -> String {
    BUILTINS
        .iter()
        .map(|p| p.name)
        .collect::<Vec<_>>()
        .join(", ")
}

pub fn model_leaf(model: &str) -> &str {
    model.rsplit_once('/').map(|(_, m)| m).unwrap_or(model)
}

/// Hint shown by doctor / init when a CLI probe looks like a missing login.
pub fn auth_login_hint(kind: &str) -> &'static str {
    match kind {
        "opencode" => "opencode auth login",
        "claude" => "claude",
        "grok" => "grok",
        "cursor" => "sign in to the Cursor app",
        "codex" => "codex login",
        _ => "log in with that CLI; atr does not store API keys",
    }
}

pub fn lookup<'a>(cfg: &'a Config, name: &str) -> Option<&'a Preset> {
    cfg.preset(name)
}

/// Built-in first, then config `[[preset]]`.
pub fn resolve_named(cfg: Option<&Config>, name: &str) -> Option<Resolved> {
    if let Some(b) = builtin(name) {
        return Some(Resolved {
            kind: b.kind.to_string(),
            model: b.model.to_string(),
            cli: None,
        });
    }
    cfg.and_then(|c| c.preset(name)).map(|p| {
        let cli = if p.cli.is_empty() {
            None
        } else {
            Some(p.cli.clone())
        };
        Resolved {
            kind: p.kind.clone(),
            model: p.model.clone(),
            cli,
        }
    })
}

pub fn apply(
    preset: &Preset,
    kind: Option<&str>,
    model: Option<&str>,
    cli: Option<&str>,
) -> Result<Resolved, anyhow::Error> {
    let kind = kind
        .map(str::to_string)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| preset.kind.clone());
    if !is_known_kind(&kind) {
        anyhow::bail!("unknown kind {kind}; use claude, grok, cursor, codex, or opencode");
    }
    let model = model
        .map(str::to_string)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| preset.model.clone());
    let cli = cli
        .map(str::to_string)
        .filter(|s| !s.is_empty())
        .or_else(|| {
            if preset.cli.is_empty() {
                None
            } else {
                Some(preset.cli.clone())
            }
        });
    Ok(Resolved { kind, model, cli })
}

#[derive(Debug, Clone)]
pub struct Resolved {
    pub kind: String,
    pub model: String,
    pub cli: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtins_cover_requested_names() {
        for name in [
            "kimi", "qwen", "glm", "deepseek", "minimax", "mimo", "ollama", "spark",
        ] {
            let b = builtin(name).unwrap_or_else(|| panic!("missing {name}"));
            assert_eq!(b.kind, "opencode");
            assert!(b.model.contains('/'), "{name} model {}", b.model);
        }
        assert_eq!(builtin("kimi").unwrap().env_key, Some("MOONSHOT_API_KEY"));
        assert_eq!(
            builtin("spark").unwrap().base_url,
            "http://127.0.0.1:8091/v1"
        );
        assert_eq!(
            builtin("spark").unwrap().model,
            "spark/deepseek-v4-flash-0731"
        );
        assert!(builtin("ollama").unwrap().needs_url);
        assert!(builtin("spark").unwrap().needs_url);
        assert!(!builtin("kimi").unwrap().needs_url);
        assert_eq!(model_leaf("moonshotai/kimi-k2.6"), "kimi-k2.6");
    }

    #[test]
    fn login_hints_are_exact_commands() {
        assert_eq!(auth_login_hint("opencode"), "opencode auth login");
        assert_eq!(auth_login_hint("codex"), "codex login");
        assert_eq!(auth_login_hint("claude"), "claude");
        assert_eq!(auth_login_hint("cursor"), "sign in to the Cursor app");
    }

    #[test]
    fn apply_prefers_explicit_kind_and_model() {
        let p = Preset {
            name: "kimi".into(),
            kind: "opencode".into(),
            model: "moonshotai/kimi-k2.6".into(),
            cli: String::new(),
        };
        let r = apply(&p, Some("opencode"), Some("moonshotai/other"), None).unwrap();
        assert_eq!(r.model, "moonshotai/other");
        assert_eq!(r.kind, "opencode");
    }

    #[test]
    fn resolve_named_prefers_builtin() {
        let r = resolve_named(None, "kimi").unwrap();
        assert_eq!(r.kind, "opencode");
        assert_eq!(r.model, "moonshotai/kimi-k2.6");
        assert!(resolve_named(None, "nope").is_none());
    }
}
