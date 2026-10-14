use crate::config::default_atr_dir;
use crate::util::sighup_daemon;
use anyhow::{anyhow, bail, Context, Result};
use serde_json::Value;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use toml_edit::{Array, DocumentMut};

const TEST_TEXT: &str = "atr: telegram is set up. You will get task notifications here.";

pub fn shadow_env_path(config_path: &Path) -> PathBuf {
    config_path
        .parent()
        .map(|p| p.join("shadow.env"))
        .unwrap_or_else(|| default_atr_dir().join("shadow.env"))
}

pub fn read_shadow_env_var(path: &Path, key: &str) -> Option<String> {
    let text = fs::read_to_string(path).ok()?;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line);
        let (k, v) = line.split_once('=')?;
        if k.trim() == key {
            return Some(unquote(v.trim())).filter(|s| !s.is_empty());
        }
    }
    None
}

fn unquote(s: &str) -> String {
    if s.len() >= 2 {
        let b = s.as_bytes();
        if (b[0] == b'"' && b[s.len() - 1] == b'"') || (b[0] == b'\'' && b[s.len() - 1] == b'\'') {
            return s[1..s.len() - 1].to_string();
        }
    }
    s.to_string()
}

pub fn upsert_shadow_env(path: &Path, key: &str, value: &str) -> Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    }
    let existing = if path.exists() {
        fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?
    } else {
        String::new()
    };
    let assign = format!("{key}=");
    let export = format!("export {key}=");
    let mut found = false;
    let mut out = String::new();
    for line in existing.lines() {
        let t = line.trim_start();
        if t.starts_with(&assign) || t.starts_with(&export) {
            out.push_str(&format!("{key}={value}\n"));
            found = true;
        } else {
            out.push_str(line);
            out.push('\n');
        }
    }
    if !found {
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(&format!("{key}={value}\n"));
    }
    fs::write(path, out).with_context(|| format!("write {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .with_context(|| format!("chmod 0600 {}", path.display()))?;
    }
    Ok(())
}

pub fn resolve_token(cfg: &crate::config::TelegramConfig, shadow: &Path) -> Option<String> {
    crate::telegram::bot_token(cfg).or_else(|| read_shadow_env_var(shadow, &cfg.bot_token_env))
}

pub fn parse_start_chat_id(updates: &Value) -> Option<i64> {
    let arr = updates.get("result")?.as_array()?;
    for upd in arr {
        let msg = match upd.get("message") {
            Some(m) => m,
            None => continue,
        };
        let text = msg
            .get("text")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim();
        let cmd = text
            .split('@')
            .next()
            .unwrap_or("")
            .split_whitespace()
            .next()
            .unwrap_or("");
        if cmd != "/start" {
            continue;
        }
        if let Some(id) = msg.pointer("/chat/id").and_then(|v| v.as_i64()) {
            return Some(id);
        }
    }
    None
}

fn api_host(api_base: &str) -> String {
    let b = api_base.trim();
    if b.is_empty() {
        "https://api.telegram.org".into()
    } else {
        b.trim_end_matches('/').to_string()
    }
}

fn redact(s: &str, token: &str) -> String {
    if token.is_empty() {
        s.to_string()
    } else {
        s.replace(token, "***")
    }
}

pub fn write_telegram_table(config_path: &Path, enabled: bool, chat_ids: &[i64]) -> Result<()> {
    let text = fs::read_to_string(config_path)
        .with_context(|| format!("read {}", config_path.display()))?;
    let mut doc = text.parse::<DocumentMut>().context("parse config.toml")?;
    if !doc.contains_key("telegram") {
        doc["telegram"] = toml_edit::table();
    }
    doc["telegram"]["enabled"] = toml_edit::value(enabled);
    let mut arr = Array::new();
    for id in chat_ids {
        arr.push(*id);
    }
    doc["telegram"]["allowed_chat_ids"] = toml_edit::value(arr);
    fs::write(config_path, doc.to_string())
        .with_context(|| format!("write {}", config_path.display()))?;
    Ok(())
}

pub fn send_test_message(token: &str, chat_id: i64, api_base: &str) -> Result<()> {
    let url = format!("{}/bot{}/sendMessage", api_host(api_base), token);
    let payload = serde_json::json!({
        "chat_id": chat_id,
        "text": TEST_TEXT,
        "disable_web_page_preview": true,
    });
    ureq::post(&url)
        .set("User-Agent", "atr")
        .send_json(payload)
        .map_err(|e| anyhow!("send test message: {}", redact(&e.to_string(), token)))?;
    Ok(())
}

pub fn discover_chat_id(token: &str, api_base: &str, wait: Duration) -> Result<i64> {
    let host = api_host(api_base);
    let deadline = Instant::now() + wait;
    let mut offset: i64 = 0;
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(15))
        .build();
    let mut last_err: Option<String> = None;
    while Instant::now() < deadline {
        let remain = deadline.saturating_duration_since(Instant::now());
        let long_poll = remain.as_secs().clamp(1, 8);
        let url = format!("{host}/bot{token}/getUpdates?timeout={long_poll}&offset={offset}");
        match agent.get(&url).set("User-Agent", "atr").call() {
            Ok(r) => {
                let v: Value = r.into_json().unwrap_or_else(|_| serde_json::json!({}));
                if v.get("ok") == Some(&Value::Bool(false)) {
                    let desc = v
                        .get("description")
                        .and_then(|d| d.as_str())
                        .unwrap_or("telegram rejected the token");
                    bail!("{}", redact(desc, token));
                }
                if let Some(id) = parse_start_chat_id(&v) {
                    return Ok(id);
                }
                if let Some(arr) = v.get("result").and_then(|r| r.as_array()) {
                    for upd in arr {
                        if let Some(id) = upd.get("update_id").and_then(|n| n.as_i64()) {
                            offset = offset.max(id + 1);
                        }
                    }
                }
            }
            Err(e) => {
                let msg = redact(&e.to_string(), token);
                if msg.contains("401") || msg.contains("Unauthorized") {
                    bail!("Telegram rejected the token (check it with @BotFather)");
                }
                last_err = Some(msg);
                std::thread::sleep(Duration::from_millis(400));
            }
        }
    }
    match last_err {
        Some(e) => bail!("timed out waiting for /start ({e})"),
        None => bail!(
            "timed out waiting for /start; send /start to the bot and run atr telegram setup again"
        ),
    }
}

pub fn apply_setup(
    config_path: &Path,
    token: &str,
    api_base: &str,
    wait: Duration,
    announce: bool,
) -> Result<String> {
    if token.trim().is_empty() {
        bail!("bot token is required");
    }
    if !config_path.exists() {
        bail!("no config at {}; run atr init first", config_path.display());
    }
    let token = token.trim();
    let shadow = shadow_env_path(config_path);
    upsert_shadow_env(&shadow, "ATR_TELEGRAM_TOKEN", token)?;
    if announce {
        println!(
            "Token stored in {} (mode 0600). It is never written to config.toml.",
            shadow.display()
        );
        println!("Open the bot in Telegram and send /start (or tap Start).");
        println!("Waiting for /start…");
        let _ = std::io::stdout().flush();
    }
    let cfg = crate::config::load_config(config_path).ok();
    let base = if !api_base.is_empty() {
        api_base.to_string()
    } else {
        cfg.as_ref()
            .map(|c| c.telegram.api_base.clone())
            .unwrap_or_default()
    };
    let chat_id = discover_chat_id(token, &base, wait)?;
    if announce {
        println!("Got chat id {chat_id}");
    }
    write_telegram_table(config_path, true, &[chat_id])?;
    send_test_message(token, chat_id, &base)?;
    if let Ok(cfg) = crate::config::load_config(config_path) {
        match sighup_daemon(&cfg) {
            Ok(()) => {
                if announce {
                    println!("sent SIGHUP to atr serve");
                }
            }
            Err(e) => {
                if announce {
                    println!("config saved; SIGHUP skipped ({e})");
                }
            }
        }
    }
    Ok(format!(
        "telegram enabled; token in {}; allowed_chat_ids=[{chat_id}]; test message sent\n",
        shadow.display()
    ))
}

pub fn setup_interactive(config_path: &Path) -> Result<String> {
    if !config_path.exists() {
        bail!("no config at {}; run atr init first", config_path.display());
    }
    println!("Create a bot in Telegram: open @BotFather, send /newbot, copy the token.");
    let token = crate::init::prompt_line("bot token", "", false)?;
    apply_setup(config_path, &token, "", Duration::from_secs(120), true)
}

pub fn test(config_path: &Path) -> Result<String> {
    let cfg = crate::config::load_config(config_path)?;
    let shadow = shadow_env_path(config_path);
    let token = resolve_token(&cfg.telegram, &shadow)
        .ok_or_else(|| anyhow!("no bot token; run atr telegram setup"))?;
    if cfg.telegram.allowed_chat_ids.is_empty() {
        bail!("telegram.allowed_chat_ids is empty; run atr telegram setup");
    }
    for id in &cfg.telegram.allowed_chat_ids {
        send_test_message(&token, *id, &cfg.telegram.api_base)?;
    }
    let ids: Vec<String> = cfg
        .telegram
        .allowed_chat_ids
        .iter()
        .map(|i| i.to_string())
        .collect();
    Ok(format!("sent test message to {}\n", ids.join(", ")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    fn read_http_request(stream: &mut std::net::TcpStream) -> String {
        let mut buf = Vec::new();
        let mut tmp = [0u8; 2048];
        stream
            .set_read_timeout(Some(Duration::from_millis(200)))
            .ok();
        loop {
            match stream.read(&mut tmp) {
                Ok(0) => break,
                Ok(n) => {
                    buf.extend_from_slice(&tmp[..n]);
                    if buf.windows(4).any(|w| w == b"\r\n\r\n") && buf.len() > 32 {
                        if let Ok(n) = stream.read(&mut tmp) {
                            if n > 0 {
                                buf.extend_from_slice(&tmp[..n]);
                            }
                        }
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        String::from_utf8_lossy(&buf).into_owned()
    }

    fn spawn_fake_tg() -> (String, Arc<Mutex<Vec<String>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let hits = Arc::new(Mutex::new(Vec::new()));
        let hits2 = hits.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let mut s = match stream {
                    Ok(s) => s,
                    Err(_) => break,
                };
                let req = read_http_request(&mut s);
                hits2.lock().unwrap().push(req.clone());
                let body = if req.contains("getUpdates") {
                    r#"{"ok":true,"result":[{"update_id":7,"message":{"chat":{"id":4242},"text":"/start"}}]}"#
                } else if req.contains("sendMessage") {
                    r#"{"ok":true,"result":{"message_id":1}}"#
                } else {
                    r#"{"ok":false}"#
                };
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = s.write_all(resp.as_bytes());
            }
        });
        (format!("http://{addr}"), hits)
    }

    fn seed_config(dir: &Path) -> PathBuf {
        let path = dir.join("config.toml");
        std::fs::write(
            &path,
            r#"
listen = "127.0.0.1:0"
repo_path = "/tmp/repo"
[telegram]
enabled = false
bot_token_env = "ATR_TELEGRAM_TOKEN"
allowed_chat_ids = []
[[provider]]
name = "claude"
kind = "claude"
cli = "/bin/true"
"#,
        )
        .unwrap();
        path
    }

    #[test]
    fn upsert_shadow_env_mode_0600_preserves_other_keys() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shadow.env");
        std::fs::write(
            &path,
            "ATR_SHADOW_GH_TOKEN=keep-me\nATR_TELEGRAM_TOKEN=old\n",
        )
        .unwrap();
        upsert_shadow_env(&path, "ATR_TELEGRAM_TOKEN", "new-secret").unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("ATR_SHADOW_GH_TOKEN=keep-me"), "{text}");
        assert!(text.contains("ATR_TELEGRAM_TOKEN=new-secret"), "{text}");
        assert!(!text.contains("old"), "{text}");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "shadow.env must be 0600");
        }
    }

    #[test]
    fn parse_start_from_getupdates() {
        let v = json!({"ok":true,"result":[
            {"update_id":1,"message":{"chat":{"id":7},"text":"/help"}},
            {"update_id":2,"message":{"chat":{"id":99},"text":"/start@MyBot"}}
        ]});
        assert_eq!(parse_start_chat_id(&v), Some(99));
        let empty = json!({"ok":true,"result":[]});
        assert_eq!(parse_start_chat_id(&empty), None);
    }

    #[test]
    fn write_allowed_chat_ids_never_stores_token() {
        let dir = tempfile::tempdir().unwrap();
        let path = seed_config(dir.path());
        write_telegram_table(&path, true, &[42, 99]).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("enabled = true"), "{text}");
        assert!(text.contains("allowed_chat_ids = [42, 99]"), "{text}");
        assert!(!text.contains("123:ABC"), "{text}");
        assert!(!text.to_ascii_lowercase().contains("bot_token ="));
    }

    #[test]
    fn apply_setup_writes_shadow_and_config_sends_test() {
        let (base, hits) = spawn_fake_tg();
        let dir = tempfile::tempdir().unwrap();
        let path = seed_config(dir.path());
        let token = "123456:ABC-secret";
        let msg = apply_setup(&path, token, &base, Duration::from_secs(3), false).unwrap();
        assert!(msg.contains("allowed_chat_ids=[4242]"), "{msg}");
        let shadow = std::fs::read_to_string(dir.path().join("shadow.env")).unwrap();
        assert!(shadow.contains("ATR_TELEGRAM_TOKEN=123456:ABC-secret"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.path().join("shadow.env"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600);
        }
        let cfg = std::fs::read_to_string(&path).unwrap();
        assert!(cfg.contains("allowed_chat_ids = [4242]"), "{cfg}");
        assert!(cfg.contains("enabled = true"), "{cfg}");
        assert!(
            !cfg.contains("ABC-secret"),
            "token must never appear in config.toml: {cfg}"
        );
        let reqs = hits.lock().unwrap().join("\n");
        assert!(reqs.contains("getUpdates"), "{reqs}");
        assert!(reqs.contains("sendMessage"), "{reqs}");
    }
}
