//! Session-scoped TUI knobs (`/effort`, `/context`) applied on the next spawn.
//!
//! Per-kind only — no invented flags. Values come from the provider CLIs
//! atr already spawns:
//! * grok: `--effort` (docs; alias of `--reasoning-effort`)
//! * claude: `--effort`, `--autocompact`
//! * cursor: parameterized `--model` id (`name[effort=…,context=…]`)
//! * codex: `-c model_reasoning_effort="…"` (atr `config_overrides`)

use crate::config::Provider;
use crate::db::Db;

const EFFORT_META: &str = "tui-effort:";
const CONTEXT_META: &str = "tui-context:";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Knob {
    Effort,
    Context,
}

impl Knob {
    pub(crate) fn slash(self) -> &'static str {
        match self {
            Self::Effort => "/effort",
            Self::Context => "/context",
        }
    }

    pub(crate) fn word(self) -> &'static str {
        match self {
            Self::Effort => "effort",
            Self::Context => "context",
        }
    }
}

pub(crate) fn effort_meta_key(key: &str, provider: &str) -> String {
    format!("{EFFORT_META}{key}:{provider}")
}

pub(crate) fn context_meta_key(key: &str, provider: &str) -> String {
    format!("{CONTEXT_META}{key}:{provider}")
}

pub(crate) fn effort_values(kind: &str) -> Option<&'static [&'static str]> {
    match kind {
        "grok" => Some(&["low", "medium", "high", "xhigh"]),
        "claude" => Some(&["low", "medium", "high", "xhigh", "max", "ultra"]),
        "cursor" => Some(&["low", "medium", "high"]),
        "codex" => Some(&["low", "medium", "high", "xhigh"]),
        _ => None,
    }
}

pub(crate) fn context_values(kind: &str) -> Option<&'static [&'static str]> {
    match kind {
        "claude" => Some(&["auto", "100k", "200k", "500k", "1m"]),
        "cursor" => Some(&["300k", "1m"]),
        _ => None,
    }
}

pub(crate) fn values(knob: Knob, kind: &str) -> Option<&'static [&'static str]> {
    match knob {
        Knob::Effort => effort_values(kind),
        Knob::Context => context_values(kind),
    }
}

pub(crate) fn picker_items(knob: Knob, kind: &str) -> Vec<String> {
    let mut items: Vec<String> = values(knob, kind)
        .unwrap_or(&[])
        .iter()
        .map(|s| (*s).to_string())
        .collect();
    items.push("default".into());
    items
}

pub(crate) fn unsupported_note(knob: Knob, kind: &str) -> String {
    format!("{} is not supported for {kind}", knob.word())
}

fn is_clear(raw: &str) -> bool {
    matches!(
        raw.trim().to_ascii_lowercase().as_str(),
        "default" | "clear" | "none" | "off" | "unset"
    )
}

fn canon_effort(raw: &str) -> Option<String> {
    let t = raw.trim().to_ascii_lowercase();
    let t = match t.as_str() {
        "extra-high" | "extra_high" | "x-high" => "xhigh",
        "ultracode" => "ultra",
        "med" => "medium",
        other => other,
    };
    Some(t.to_string())
}

fn canon_context(raw: &str) -> Option<String> {
    let t = raw.trim().to_ascii_lowercase().replace('_', "");
    let t = match t.as_str() {
        "auto" => "auto",
        "100k" | "100000" => "100k",
        "200k" | "200000" => "200k",
        "300k" | "300000" => "300k",
        "500k" | "500000" => "500k",
        "1m" | "1000000" | "1048576" | "1050000" => "1m",
        _ => return None,
    };
    Some(t.to_string())
}

/// `Ok(None)` clears. `Err` is not a usable value for this kind.
pub(crate) fn parse_knob(knob: Knob, kind: &str, raw: &str) -> Result<Option<String>, String> {
    let Some(allowed) = values(knob, kind) else {
        return Err(unsupported_note(knob, kind));
    };
    let raw = raw.trim();
    if raw.is_empty() {
        return Err(format!("usage: {} <value>", knob.slash()));
    }
    if is_clear(raw) {
        return Ok(None);
    }
    let canon = match knob {
        Knob::Effort => canon_effort(raw),
        Knob::Context => canon_context(raw),
    };
    let Some(canon) = canon else {
        return Err(format!(
            "no {} matching {raw:?} for {kind}.\n{}",
            knob.word(),
            usage_line(knob, kind)
        ));
    };
    if allowed.iter().any(|a| *a == canon) {
        return Ok(Some(canon));
    }
    Err(format!(
        "no {} matching {raw:?} for {kind}.\n{}",
        knob.word(),
        usage_line(knob, kind)
    ))
}

pub(crate) fn usage_line(knob: Knob, kind: &str) -> String {
    match values(knob, kind) {
        Some(v) => format!(
            "usage: {} [{} | default]\n       {kind} next spawn only",
            knob.slash(),
            v.join(" | ")
        ),
        None => unsupported_note(knob, kind),
    }
}

pub(crate) fn session_effort(db: &Db, key: &str, provider: &str) -> Option<String> {
    read_meta(db, &effort_meta_key(key, provider))
}

pub(crate) fn session_context(db: &Db, key: &str, provider: &str) -> Option<String> {
    read_meta(db, &context_meta_key(key, provider))
}

fn read_meta(db: &Db, k: &str) -> Option<String> {
    db.meta_get(k)
        .ok()
        .flatten()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

pub(crate) fn store_knob(
    db: &Db,
    key: &str,
    provider: &str,
    knob: Knob,
    value: Option<&str>,
) -> anyhow::Result<()> {
    let meta = match knob {
        Knob::Effort => effort_meta_key(key, provider),
        Knob::Context => context_meta_key(key, provider),
    };
    db.meta_set(&meta, value.unwrap_or(""))?;
    Ok(())
}

pub(crate) fn clear_session_knobs(db: &Db, key: &str, provider: &str) {
    let _ = db.meta_set(&effort_meta_key(key, provider), "");
    let _ = db.meta_set(&context_meta_key(key, provider), "");
}

/// Apply stored `/effort` / `/context` onto the provider used for this turn.
/// Call after [`super::sanitize_spawn_model`] so Cursor brackets are not
/// dropped as a junk `--model` id.
pub fn apply_session_knobs(provider: &mut Provider, db: &Db, key: &str) {
    let effort = session_effort(db, key, &provider.name);
    let context = session_context(db, key, &provider.name);
    apply_knobs_to_provider(provider, effort.as_deref(), context.as_deref());
}

pub(crate) fn apply_knobs_to_provider(
    provider: &mut Provider,
    effort: Option<&str>,
    context: Option<&str>,
) {
    match provider.kind.as_str() {
        "grok" => {
            if let Some(e) = effort {
                replace_flag(&mut provider.extra_args, "--reasoning-effort", None);
                replace_flag(&mut provider.extra_args, "--effort", Some(e));
            }
        }
        "claude" => {
            if let Some(e) = effort {
                replace_flag(&mut provider.extra_args, "--effort", Some(e));
            }
            if let Some(c) = context {
                replace_flag(&mut provider.extra_args, "--autocompact", Some(c));
            }
        }
        "cursor" => {
            if effort.is_none() && context.is_none() {
                return;
            }
            if provider.model.trim().is_empty() {
                return;
            }
            provider.model = cursor_model_with_params(&provider.model, effort, context);
        }
        "codex" => {
            if let Some(e) = effort {
                replace_override(
                    &mut provider.config_overrides,
                    "model_reasoning_effort",
                    &format!("model_reasoning_effort=\"{e}\""),
                );
            }
        }
        _ => {}
    }
}

fn replace_flag(args: &mut Vec<String>, flag: &str, value: Option<&str>) {
    let mut i = 0;
    while i < args.len() {
        if args[i] == flag {
            args.remove(i);
            if i < args.len() && !args[i].starts_with('-') {
                args.remove(i);
            }
        } else {
            i += 1;
        }
    }
    if let Some(v) = value {
        args.push(flag.to_string());
        args.push(v.to_string());
    }
}

fn replace_override(ovs: &mut Vec<String>, key: &str, entry: &str) {
    let prefix = format!("{key}=");
    ovs.retain(|o| !o.starts_with(&prefix));
    ovs.push(entry.to_string());
}

pub(crate) fn cursor_model_with_params(
    model: &str,
    effort: Option<&str>,
    context: Option<&str>,
) -> String {
    let (base, mut params) = split_cursor_model(model);
    upsert_param(&mut params, "effort", effort);
    upsert_param(&mut params, "context", context);
    if params.is_empty() {
        return base;
    }
    let body = params
        .into_iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join(",");
    format!("{base}[{body}]")
}

fn split_cursor_model(model: &str) -> (String, Vec<(String, String)>) {
    let t = model.trim();
    let Some(open) = t.find('[') else {
        return (t.to_string(), Vec::new());
    };
    if !t.ends_with(']') || open == 0 {
        return (t.to_string(), Vec::new());
    }
    let base = t[..open].to_string();
    let inner = &t[open + 1..t.len() - 1];
    let params = inner
        .split(',')
        .filter_map(|pair| {
            let (k, v) = pair.split_once('=')?;
            let k = k.trim();
            let v = v.trim();
            if k.is_empty() || v.is_empty() {
                return None;
            }
            Some((k.to_string(), v.to_string()))
        })
        .collect();
    (base, params)
}

fn upsert_param(params: &mut Vec<(String, String)>, key: &str, value: Option<&str>) {
    params.retain(|(k, _)| k != key);
    if let Some(v) = value {
        params.push((key.to_string(), v.to_string()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::build_command;

    fn stub(kind: &str) -> Provider {
        let mut p = Provider::test_stub(kind);
        p.kind = kind.into();
        p.model_flag = if kind == "codex" {
            "-m".into()
        } else {
            "--model".into()
        };
        p.args = if matches!(kind, "codex" | "opencode") {
            vec![]
        } else {
            vec!["-p".into()]
        };
        p.config_overrides = if kind == "codex" {
            vec!["model_provider=\"openai\"".into()]
        } else {
            vec![]
        };
        p
    }

    #[test]
    fn parse_grok_claude_and_reject_opencode() {
        assert_eq!(
            parse_knob(Knob::Effort, "grok", "HIGH").unwrap().as_deref(),
            Some("high")
        );
        assert_eq!(
            parse_knob(Knob::Effort, "grok", "x-high")
                .unwrap()
                .as_deref(),
            Some("xhigh")
        );
        assert_eq!(parse_knob(Knob::Effort, "grok", "default").unwrap(), None);
        assert!(parse_knob(Knob::Effort, "grok", "ultra").is_err());
        assert_eq!(
            parse_knob(Knob::Effort, "claude", "max")
                .unwrap()
                .as_deref(),
            Some("max")
        );
        assert_eq!(
            parse_knob(Knob::Context, "claude", "200K")
                .unwrap()
                .as_deref(),
            Some("200k")
        );
        assert_eq!(
            parse_knob(Knob::Context, "claude", "1M")
                .unwrap()
                .as_deref(),
            Some("1m")
        );
        assert!(parse_knob(Knob::Effort, "opencode", "high")
            .unwrap_err()
            .contains("not supported for opencode"));
        assert!(parse_knob(Knob::Context, "grok", "1m")
            .unwrap_err()
            .contains("not supported for grok"));
    }

    #[test]
    fn grok_argv_gets_effort_flag() {
        let mut p = stub("grok");
        apply_knobs_to_provider(&mut p, Some("high"), None);
        let spec = build_command(
            &p,
            std::path::Path::new("/wt"),
            std::path::Path::new("/wt/.atr/prompt.md"),
            "hi",
            None,
            true,
        );
        assert!(
            spec.args.windows(2).any(|w| w == ["--effort", "high"]),
            "grok spawn must pass --effort: {:?}",
            spec.args
        );
        assert!(
            !spec.args.iter().any(|a| a == "--reasoning-effort"),
            "prefer documented --effort: {:?}",
            spec.args
        );
        assert!(!spec.args.iter().any(|a| a == "--autocompact"));
    }

    #[test]
    fn claude_argv_gets_effort_and_autocompact() {
        let mut p = stub("claude");
        apply_knobs_to_provider(&mut p, Some("max"), Some("200k"));
        let spec = build_command(
            &p,
            std::path::Path::new("/wt"),
            std::path::Path::new("/wt/.atr/prompt.md"),
            "hi",
            None,
            true,
        );
        assert!(
            spec.args.windows(2).any(|w| w == ["--effort", "max"]),
            "{:?}",
            spec.args
        );
        assert!(
            spec.args.windows(2).any(|w| w == ["--autocompact", "200k"]),
            "{:?}",
            spec.args
        );
    }

    #[test]
    fn cursor_rewrites_parameterized_model() {
        let mut p = stub("cursor");
        p.model = "claude-opus-4-8".into();
        apply_knobs_to_provider(&mut p, Some("high"), Some("1m"));
        assert_eq!(p.model, "claude-opus-4-8[effort=high,context=1m]");
        let spec = build_command(
            &p,
            std::path::Path::new("/wt"),
            std::path::Path::new("/wt/.atr/prompt.md"),
            "hi",
            None,
            true,
        );
        assert!(
            spec.args
                .windows(2)
                .any(|w| w == ["--model", "claude-opus-4-8[effort=high,context=1m]"]),
            "{:?}",
            spec.args
        );
    }

    #[test]
    fn cursor_keeps_foreign_bracket_params() {
        let out = cursor_model_with_params(
            "claude-opus-4-8[fast=false,effort=low]",
            Some("high"),
            Some("1m"),
        );
        assert_eq!(out, "claude-opus-4-8[fast=false,effort=high,context=1m]");
    }

    #[test]
    fn codex_gets_reasoning_override() {
        let mut p = stub("codex");
        apply_knobs_to_provider(&mut p, Some("xhigh"), Some("1m"));
        let spec = build_command(
            &p,
            std::path::Path::new("/wt"),
            std::path::Path::new("/wt/.atr/prompt.md"),
            "hi",
            None,
            true,
        );
        assert!(
            spec.args
                .windows(2)
                .any(|w| w == ["-c", "model_reasoning_effort=\"xhigh\""]),
            "{:?}",
            spec.args
        );
        assert!(
            !spec.args.iter().any(|a| a.contains("model_context_window")),
            "codex context is not a clean spawn flag: {:?}",
            spec.args
        );
    }

    #[test]
    fn opencode_is_untouched() {
        let mut p = stub("opencode");
        let before = p.extra_args.clone();
        apply_knobs_to_provider(&mut p, Some("high"), Some("1m"));
        assert_eq!(p.extra_args, before);
        assert!(!p.model.contains('['));
    }
}
