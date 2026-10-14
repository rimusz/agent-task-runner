//! Merge `~/.config/opencode/opencode.json` for atr's OpenCode presets.
//!
//! Keys are never written here — only `{env:VAR}` plus a base URL.

use crate::config::Config;
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};

#[cfg(test)]
thread_local! {
    static TEST_PATH: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub fn set_test_path(path: Option<PathBuf>) {
    TEST_PATH.with(|c| *c.borrow_mut() = path);
}

pub fn config_path() -> PathBuf {
    #[cfg(test)]
    {
        if let Some(p) = TEST_PATH.with(|c| c.borrow().clone()) {
            return p;
        }
    }
    if let Ok(p) = std::env::var("ATR_OPENCODE_CONFIG") {
        if !p.trim().is_empty() {
            return crate::config::expand_path(&p);
        }
    }
    crate::config::expand_path("~/.config/opencode/opencode.json")
}

pub fn merge_provider(
    path: &Path,
    provider_id: &str,
    display: &str,
    base_url: &str,
    api_key_env: Option<&str>,
    model_leaf: &str,
) -> Result<()> {
    if provider_id.is_empty() {
        bail!("opencode provider id is empty");
    }
    let mut root = if path.exists() {
        let text = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        if text.trim().is_empty() {
            json!({})
        } else {
            serde_json::from_str(&text).with_context(|| format!("parse {}", path.display()))?
        }
    } else {
        json!({})
    };
    if !root.is_object() {
        bail!("{} must be a JSON object", path.display());
    }
    if root.get("$schema").is_none() {
        root["$schema"] = json!("https://opencode.ai/config.json");
    }
    {
        let map = root.as_object_mut().expect("object");
        let providers = map.entry("provider").or_insert_with(|| json!({}));
        if !providers.is_object() {
            *providers = json!({});
        }
        let entry = providers
            .as_object_mut()
            .expect("provider")
            .entry(provider_id)
            .or_insert_with(|| json!({}));
        if !entry.is_object() {
            *entry = json!({});
        }
        let obj = entry.as_object_mut().expect("entry");
        obj.entry("npm")
            .or_insert(json!("@ai-sdk/openai-compatible"));
        obj.entry("name").or_insert(json!(display));
        let options = obj.entry("options").or_insert(json!({}));
        if !options.is_object() {
            *options = json!({});
        }
        options["baseURL"] = json!(base_url);
        if let Some(env) = api_key_env {
            options["apiKey"] = json!(format!("{{env:{env}}}"));
        }
        if provider_id.eq_ignore_ascii_case("ollama") {
            options
                .as_object_mut()
                .expect("options")
                .entry("keepAlive")
                .or_insert(json!("24h"));
        }
        let models = obj.entry("models").or_insert(json!({}));
        if !models.is_object() {
            *models = json!({});
        }
        if !model_leaf.is_empty() {
            models
                .as_object_mut()
                .expect("models")
                .entry(model_leaf)
                .or_insert(json!({ "name": model_leaf }));
        }
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let body = serde_json::to_string_pretty(&root)?;
    fs::write(path, format!("{body}\n")).with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

pub fn provider_base_url(path: &Path, provider_id: &str) -> Option<String> {
    let text = fs::read_to_string(path).ok()?;
    let v: Value = serde_json::from_str(&text).ok()?;
    v.pointer(&format!("/provider/{provider_id}/options/baseURL"))?
        .as_str()
        .map(str::to_string)
        .filter(|s| !s.is_empty())
}

/// Set `provider.<id>.options.keepAlive` so Ollama keeps the model loaded.
pub fn ensure_keep_alive(path: &Path, provider_id: &str, keep_alive: &str) -> Result<()> {
    let keep_alive = keep_alive.trim();
    if keep_alive.is_empty() || provider_id.is_empty() {
        return Ok(());
    }
    let mut root = if path.exists() {
        let text = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        if text.trim().is_empty() {
            json!({})
        } else {
            serde_json::from_str(&text).with_context(|| format!("parse {}", path.display()))?
        }
    } else {
        json!({})
    };
    if !root.is_object() {
        bail!("{} must be a JSON object", path.display());
    }
    {
        let map = root.as_object_mut().expect("object");
        let providers = map.entry("provider").or_insert_with(|| json!({}));
        if !providers.is_object() {
            *providers = json!({});
        }
        let entry = providers
            .as_object_mut()
            .expect("provider")
            .entry(provider_id)
            .or_insert_with(|| json!({}));
        if !entry.is_object() {
            *entry = json!({});
        }
        let options = entry
            .as_object_mut()
            .expect("entry")
            .entry("options")
            .or_insert(json!({}));
        if !options.is_object() {
            *options = json!({});
        }
        options["keepAlive"] = json!(keep_alive);
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let body = serde_json::to_string_pretty(&root)?;
    fs::write(path, format!("{body}\n")).with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

/// Write a long Ollama `keepAlive` into opencode.json for each Ollama provider.
pub fn apply_ollama_keep_alive(cfg: &Config) {
    for p in &cfg.providers {
        if !p.is_ollama() || p.keep_alive.trim().is_empty() {
            continue;
        }
        if let Err(e) = ensure_keep_alive(&config_path(), "ollama", &p.keep_alive) {
            tracing::warn!(provider = %p.name, "opencode keepAlive: {e:#}");
        }
    }
}

/// `127.0.0.1:8091` → `http://127.0.0.1:8091/v1`. Full URLs are kept.
pub fn normalize_base_url(raw: &str, fallback: &str) -> String {
    let s = raw.trim();
    if s.is_empty() {
        return fallback.to_string();
    }
    let s = if s.starts_with("http://") || s.starts_with("https://") {
        s.to_string()
    } else {
        format!("http://{s}")
    };
    if s.contains("/v1") || s.contains("/v4") || s.contains("/compatible-mode") {
        s
    } else {
        format!("{}/v1", s.trim_end_matches('/'))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_writes_env_ref_not_raw_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("opencode.json");
        merge_provider(
            &path,
            "moonshotai",
            "Moonshot Kimi",
            "https://api.moonshot.ai/v1",
            Some("MOONSHOT_API_KEY"),
            "kimi-k2.6",
        )
        .unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("{env:MOONSHOT_API_KEY}"), "{text}");
        assert!(!text.contains("sk-"), "{text}");
        assert!(text.contains("https://api.moonshot.ai/v1"), "{text}");
        merge_provider(
            &path,
            "spark",
            "spark (local)",
            "http://127.0.0.1:8091/v1",
            None,
            "deepseek-v4-flash-0731",
        )
        .unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("moonshotai"), "{text}");
        assert!(text.contains("127.0.0.1:8091"), "{text}");
        assert_eq!(
            provider_base_url(&path, "spark").as_deref(),
            Some("http://127.0.0.1:8091/v1")
        );
    }

    #[test]
    fn ollama_keep_alive_is_written() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("opencode.json");
        merge_provider(
            &path,
            "ollama",
            "Ollama",
            "http://127.0.0.1:11434/v1",
            None,
            "llama3.2",
        )
        .unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("\"keepAlive\": \"24h\""), "{text}");
        ensure_keep_alive(&path, "ollama", "48h").unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("\"keepAlive\": \"48h\""), "{text}");
    }

    #[test]
    fn normalize_host_port() {
        assert_eq!(
            normalize_base_url("127.0.0.1:8091", "http://x/v1"),
            "http://127.0.0.1:8091/v1"
        );
        assert_eq!(
            normalize_base_url("http://127.0.0.1:11434/v1", "http://x/v1"),
            "http://127.0.0.1:11434/v1"
        );
    }
}
