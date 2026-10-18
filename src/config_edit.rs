use crate::util::sighup_daemon;
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use toml_edit::{DocumentMut, Item, Table, Value};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    pub key: String,
    pub old: String,
    pub new: String,
}

pub fn is_secret_key(key: &str) -> bool {
    key.split('.').any(is_secret_segment)
}

fn is_secret_segment(seg: &str) -> bool {
    let s = seg.trim().to_ascii_lowercase();
    s.contains("token")
        || s.contains("secret")
        || s.contains("password")
        || s.contains("api_key")
        || s.contains("apikey")
}

fn refuse_secret(key: &str) -> Result<()> {
    if is_secret_key(key) {
        bail!("refused: secret keys are not readable or settable via Telegram");
    }
    Ok(())
}

pub fn backup_config(path: &Path) -> Result<PathBuf> {
    if !path.exists() {
        bail!("no config at {}", path.display());
    }
    let dest = {
        let mut name = path
            .file_name()
            .map(|n| n.to_os_string())
            .unwrap_or_else(|| "config.toml".into());
        name.push(".bak");
        path.with_file_name(name)
    };
    crate::util::copy_file(path, &dest)?;
    Ok(dest)
}

pub fn get_value(path: &Path, key: &str) -> Result<String> {
    refuse_secret(key)?;
    let key = normalize_key(key)?;
    let doc = read_doc(path)?;
    if let Some(v) = lookup(&doc, &key) {
        return Ok(v);
    }
    if key_is_known(path, &key)? {
        return Ok("(unset)".into());
    }
    bail!("unknown config key `{key}`")
}

pub fn preview_set(path: &Path, key: &str, value: &str) -> Result<Change> {
    refuse_secret(key)?;
    let key = normalize_key(key)?;
    let mut doc = read_doc(path)?;
    let old = lookup(&doc, &key).unwrap_or_else(|| "(unset)".into());
    let before = doc.to_string();
    set_in_doc(&mut doc, &key, &parse_value(value)?)?;
    check_edit(&before, &doc.to_string())?;
    let new = lookup(&doc, &key).unwrap_or_else(|| value.to_string());
    Ok(Change { key, old, new })
}

pub fn apply_set(path: &Path, key: &str, value: &str) -> Result<Change> {
    let change = preview_set(path, key, value)?;
    backup_config(path)?;
    let mut doc = read_doc(path)?;
    let before = doc.to_string();
    set_in_doc(&mut doc, &change.key, &parse_value(value)?)?;
    let text = doc.to_string();
    check_edit(&before, &text)?;
    std::fs::write(path, text).with_context(|| format!("write {}", path.display()))?;
    sighup_ok(path);
    Ok(change)
}

fn normalize_key(key: &str) -> Result<String> {
    let key = key.trim();
    if key.is_empty() {
        bail!("empty key");
    }
    if key.contains("..") || key.starts_with('.') || key.ends_with('.') {
        bail!("invalid key `{key}`");
    }
    Ok(key.to_string())
}

fn parse_value(raw: &str) -> Result<Value> {
    let t = raw.trim();
    if t == "true" {
        return Ok(Value::from(true));
    }
    if t == "false" {
        return Ok(Value::from(false));
    }
    if let Ok(n) = t.parse::<i64>() {
        return Ok(Value::from(n));
    }
    if t.starts_with('[') && t.ends_with(']') {
        let parsed = format!("v = {t}")
            .parse::<DocumentMut>()
            .with_context(|| format!("parse value {t}"))?;
        if let Some(v) = parsed.get("v").and_then(|i| i.as_value()) {
            return Ok(v.clone());
        }
    }
    let s = if t.len() >= 2
        && ((t.starts_with('"') && t.ends_with('"')) || (t.starts_with('\'') && t.ends_with('\'')))
    {
        t[1..t.len() - 1].to_string()
    } else {
        t.to_string()
    };
    Ok(Value::from(s))
}

fn read_doc(path: &Path) -> Result<DocumentMut> {
    let text = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    text.parse()
        .with_context(|| format!("parse {}", path.display()))
}

fn key_is_known(path: &Path, key: &str) -> Result<bool> {
    let mut doc = read_doc(path)?;
    if set_in_doc(&mut doc, key, &Value::from("x")).is_err() {
        return Ok(false);
    }
    let before = read_doc(path)?.to_string();
    match check_edit(&before, &doc.to_string()) {
        Ok(()) => Ok(true),
        Err(e) => Ok(!e.to_string().contains("unknown config key")),
    }
}

/// The edited text must parse, and the edit must not add a key this binary
/// does not know (unknown keys already in the file, e.g. from a newer atr,
/// are left alone).
fn check_edit(before: &str, after: &str) -> Result<()> {
    let cfg = crate::config::parse_config_toml(after)?;
    let old = crate::config::unknown_keys(before);
    if let Some(k) = cfg.unknown_keys.iter().find(|k| !old.contains(k)) {
        match &k.nearest {
            Some(n) => bail!(
                "unknown config key `{}`; nearest valid key is `{n}`",
                k.path
            ),
            None => bail!("unknown config key `{}`", k.path),
        }
    }
    Ok(())
}

fn lookup(doc: &DocumentMut, key: &str) -> Option<String> {
    let parts: Vec<&str> = key.split('.').collect();
    let item = if parts.first().copied() == Some("provider") && parts.len() >= 3 {
        let table = find_named_table(doc, "provider", parts[1])?;
        walk_table(table, &parts[2..])?
    } else if parts.first().copied() == Some("repo") && parts.len() >= 3 {
        let table = find_named_table(doc, "repo", parts[1])?;
        walk_table(table, &parts[2..])?
    } else {
        walk_table(doc.as_table(), &parts)?
    };
    Some(fmt_item(item))
}

fn find_named_table<'a>(doc: &'a DocumentMut, kind: &str, name: &str) -> Option<&'a Table> {
    let arr = doc.get(kind)?.as_array_of_tables()?;
    arr.iter()
        .find(|t| t.get("name").and_then(|v| v.as_str()) == Some(name))
}

fn walk_table<'a>(table: &'a Table, path: &[&str]) -> Option<&'a Item> {
    if path.is_empty() {
        return None;
    }
    let mut cur = table.get(path[0])?;
    for p in &path[1..] {
        cur = cur.get(p)?;
    }
    Some(cur)
}

fn fmt_item(item: &Item) -> String {
    match item {
        Item::Value(v) => v.to_string().trim().to_string(),
        Item::None => "(unset)".into(),
        other => other.to_string().trim().to_string(),
    }
}

fn set_in_doc(doc: &mut DocumentMut, key: &str, val: &Value) -> Result<()> {
    let parts: Vec<&str> = key.split('.').collect();
    if parts.first().copied() == Some("provider") {
        set_named_array(doc, "provider", "provider", &parts, val)
    } else if parts.first().copied() == Some("repo") {
        set_named_array(doc, "repo", "repo", &parts, val)
    } else {
        set_nested(doc.as_table_mut(), &parts, val)
    }
}

fn set_named_array(
    doc: &mut DocumentMut,
    kind: &str,
    label: &str,
    parts: &[&str],
    val: &Value,
) -> Result<()> {
    if parts.len() < 3 {
        bail!("usage: {label}.<name>.<field>");
    }
    let name = parts[1];
    let arr = doc
        .get_mut(kind)
        .and_then(|i| i.as_array_of_tables_mut())
        .ok_or_else(|| anyhow::anyhow!("no [[{label}]] table"))?;
    let idx = arr
        .iter()
        .position(|t| t.get("name").and_then(|v| v.as_str()) == Some(name))
        .ok_or_else(|| anyhow::anyhow!("unknown {label} {name}"))?;
    let table = arr
        .get_mut(idx)
        .ok_or_else(|| anyhow::anyhow!("unknown {label} {name}"))?;
    set_nested(table, &parts[2..], val)
}

fn set_nested(table: &mut Table, path: &[&str], val: &Value) -> Result<()> {
    if path.is_empty() {
        bail!("empty key");
    }
    if path.len() == 1 {
        table[path[0]] = Item::Value(val.clone());
        return Ok(());
    }
    if table.get(path[0]).and_then(|i| i.as_table()).is_none() {
        table[path[0]] = toml_edit::table();
    }
    let child = table[path[0]]
        .as_table_mut()
        .ok_or_else(|| anyhow::anyhow!("{} is not a table", path[0]))?;
    set_nested(child, &path[1..], val)
}

fn sighup_ok(path: &Path) {
    if let Ok(cfg) = crate::config::load_config(path) {
        match sighup_daemon(&cfg) {
            Ok(()) => eprintln!("sent SIGHUP to atr serve"),
            Err(e) => eprintln!("config saved; SIGHUP skipped ({e})"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn seed() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let mut f = std::fs::File::create(&path).unwrap();
        write!(
            f,
            r#"
# keep this comment
listen = "127.0.0.1:0"
repo_path = "{repo}"
worktree_root = "{root}"
system_prompt_file = "{root}/p.md"
[telegram]
enabled = false
bot_token_env = "ATR_TELEGRAM_TOKEN"
allowed_chat_ids = [42]
[shadow]
enabled = false
gh_token_env = "ATR_SHADOW_GH_TOKEN"
[[provider]]
name = "claude"
kind = "claude"
enabled = true
model = "opus"
"#,
            repo = dir.path().display(),
            root = dir.path().display()
        )
        .unwrap();
        std::fs::write(dir.path().join("p.md"), "p\n").unwrap();
        (dir, path)
    }

    #[test]
    fn get_set_preserves_comment_and_backs_up() {
        let (_dir, path) = seed();
        assert_eq!(get_value(&path, "listen").unwrap(), "\"127.0.0.1:0\"");
        let change = apply_set(&path, "listen", "127.0.0.1:1").unwrap();
        assert_eq!(change.old, "\"127.0.0.1:0\"");
        assert_eq!(change.new, "\"127.0.0.1:1\"");
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("# keep this comment"), "{text}");
        assert!(text.contains("127.0.0.1:1"), "{text}");
        let bak = path.with_file_name("config.toml.bak");
        assert!(bak.exists(), "backup missing");
        let bak_text = std::fs::read_to_string(&bak).unwrap();
        assert!(bak_text.contains("127.0.0.1:0"), "{bak_text}");
        assert!(!bak_text.contains("127.0.0.1:1"), "{bak_text}");
    }

    #[test]
    fn set_works_when_file_has_keys_from_a_newer_atr() {
        let (_dir, path) = seed();
        let mut text = std::fs::read_to_string(&path).unwrap();
        text.push_str("\n[compare]\nenabled = true\n");
        std::fs::write(&path, &text).unwrap();
        apply_set(&path, "listen", "127.0.0.1:1").unwrap();
        let after = std::fs::read_to_string(&path).unwrap();
        assert!(after.contains("127.0.0.1:1"), "{after}");
        assert!(after.contains("[compare]"), "{after}");
        // a typo is still refused even though the file already has an unknown key
        let err = preview_set(&path, "repoo_path", "/x")
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown config key"), "{err}");
    }

    #[test]
    fn unknown_key_is_rejected() {
        let (_dir, path) = seed();
        let err = preview_set(&path, "repoo_path", "/x")
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown config key"), "{err}");
        assert!(
            err.contains("repo_path") || err.contains("repoo_path"),
            "{err}"
        );
    }

    #[test]
    fn secret_keys_are_refused() {
        let (_dir, path) = seed();
        for key in [
            "telegram.bot_token_env",
            "telegram.token_file",
            "shadow.gh_token_env",
            "bot_token_env",
            "mcp.token_env",
            "mcp.token_file",
        ] {
            let g = get_value(&path, key).unwrap_err().to_string();
            assert!(g.contains("refused"), "{key}: {g}");
            assert!(!g.contains("ATR_"), "{key} must not leak env name: {g}");
            let s = preview_set(&path, key, "leak").unwrap_err().to_string();
            assert!(s.contains("refused"), "{key}: {s}");
        }
        assert_eq!(
            get_value(&path, "telegram.allowed_chat_ids").unwrap(),
            "[42]"
        );
    }

    #[test]
    fn set_provider_field_and_array() {
        let (_dir, path) = seed();
        apply_set(&path, "provider.claude.model", "sonnet").unwrap();
        assert_eq!(
            get_value(&path, "provider.claude.model").unwrap(),
            "\"sonnet\""
        );
        apply_set(&path, "telegram.allowed_chat_ids", "[42, 99]").unwrap();
        assert_eq!(
            get_value(&path, "telegram.allowed_chat_ids").unwrap(),
            "[42, 99]"
        );
        let cfg = crate::config::load_config(&path).unwrap();
        assert_eq!(cfg.provider("claude").unwrap().model, "sonnet");
        assert_eq!(cfg.telegram.allowed_chat_ids, vec![42, 99]);
        apply_set(&path, "github.label", "for:atr").unwrap();
        assert_eq!(get_value(&path, "github.label").unwrap(), "\"for:atr\"");
        let cfg = crate::config::load_config(&path).unwrap();
        assert_eq!(cfg.github.label, "for:atr");
    }

    #[test]
    fn set_and_get_assistant_providers() {
        let (_dir, path) = seed();
        assert_eq!(
            get_value(&path, "telegram.assistant.providers").unwrap(),
            "(unset)"
        );
        apply_set(&path, "telegram.assistant.providers", r#"["claude"]"#).unwrap();
        assert_eq!(
            get_value(&path, "telegram.assistant.providers").unwrap(),
            r#"["claude"]"#
        );
        let cfg = crate::config::load_config(&path).unwrap();
        assert_eq!(cfg.telegram.assistant.providers, vec!["claude"]);
        // Names are validated against [[provider]] before writing.
        let before = std::fs::read_to_string(&path).unwrap();
        let err = apply_set(
            &path,
            "telegram.assistant.providers",
            r#"["claude", "nope"]"#,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("not a [[provider]]"), "{err}");
        assert_eq!(before, std::fs::read_to_string(&path).unwrap());
    }

    #[test]
    fn invalid_value_fails_before_write() {
        let (_dir, path) = seed();
        let before = std::fs::read_to_string(&path).unwrap();
        let err = apply_set(&path, "provider.claude.skip_at", "2")
            .unwrap_err()
            .to_string();
        assert!(err.contains("skip_at"), "{err}");
        let after = std::fs::read_to_string(&path).unwrap();
        assert_eq!(before, after);
    }
}
