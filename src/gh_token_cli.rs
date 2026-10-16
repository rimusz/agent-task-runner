//! `atr github token set|list|remove`: manage per-owner GitHub tokens.
//! Values go only to `shadow.env` (0600, atomic replace); config.toml gets
//! the owner -> env var NAME mapping under `[github.tokens]`. Token values
//! are never echoed, logged, or printed.

use anyhow::{bail, Context, Result};
use std::io::{BufRead, IsTerminal, Write};
use std::path::Path;
use toml_edit::DocumentMut;

/// `ATR_GH_TOKEN_<OWNER>` with non-alphanumerics mapped to `_`.
pub fn default_env_var(owner: &str) -> String {
    let up: String = owner
        .trim()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect();
    format!("ATR_GH_TOKEN_{up}")
}

pub fn valid_env_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with(|c: char| c.is_ascii_digit())
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn valid_owner(owner: &str) -> bool {
    !owner.is_empty()
        && owner
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
}

/// Atomically set or remove `key` in an env file, keeping other lines; mode 0600.
pub fn write_env_atomic(path: &Path, key: &str, value: Option<&str>) -> Result<bool> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    }
    let existing = std::fs::read_to_string(path).unwrap_or_default();
    let assign = format!("{key}=");
    let export = format!("export {key}=");
    let mut out = String::new();
    let mut found = false;
    for line in existing.lines() {
        let t = line.trim_start();
        if t.starts_with(&assign) || t.starts_with(&export) {
            found = true;
            if let Some(v) = value {
                out.push_str(&format!("{key}={v}\n"));
            }
        } else {
            out.push_str(line);
            out.push('\n');
        }
    }
    if !found {
        if let Some(v) = value {
            out.push_str(&format!("{key}={v}\n"));
        }
    }
    let tmp = path.with_extension("env.tmp");
    {
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts
            .open(&tmp)
            .with_context(|| format!("write {}", tmp.display()))?;
        f.write_all(out.as_bytes())?;
        f.sync_all().ok();
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    }
    std::fs::rename(&tmp, path).with_context(|| format!("replace {}", path.display()))?;
    Ok(found)
}

/// Add/update (`Some`) or remove (`None`) `[github.tokens] owner = "ENV"`,
/// preserving comments and formatting. Owner match is case-insensitive.
pub fn edit_tokens_table(
    text: &str,
    owner: &str,
    env: Option<&str>,
) -> Result<(String, Option<String>)> {
    let mut doc = text.parse::<DocumentMut>().context("parse config.toml")?;
    if !doc.contains_key("github") {
        doc["github"] = toml_edit::table();
    }
    let gh = doc["github"]
        .as_table_mut()
        .context("[github] is not a table")?;
    if !gh.contains_key("tokens") {
        let mut t = toml_edit::Table::new();
        t.set_implicit(false);
        gh.insert("tokens", toml_edit::Item::Table(t));
    }
    let tokens = gh["tokens"]
        .as_table_like_mut()
        .context("[github.tokens] is not a table")?;
    let existing: Vec<String> = tokens
        .iter()
        .map(|(k, _)| k.to_string())
        .filter(|k| k.eq_ignore_ascii_case(owner))
        .collect();
    let mut previous = None;
    for k in &existing {
        if let Some(v) = tokens.get(k).and_then(|i| i.as_str()) {
            previous = Some(v.to_string());
        }
        if env.is_none() || k != owner {
            tokens.remove(k);
        }
    }
    if let Some(e) = env {
        tokens.insert(owner, toml_edit::value(e));
    }
    Ok((doc.to_string(), previous))
}

fn write_config(path: &Path, text: &str) -> Result<()> {
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, text).with_context(|| format!("write {}", tmp.display()))?;
    #[cfg(unix)]
    if let Ok(meta) = std::fs::metadata(path) {
        let _ = std::fs::set_permissions(&tmp, meta.permissions());
    }
    std::fs::rename(&tmp, path).with_context(|| format!("replace {}", path.display()))
}

/// Read a secret: hidden prompt on a TTY, else first line of stdin.
pub fn read_secret(prompt: &str) -> Result<String> {
    let stdin = std::io::stdin();
    let line = if stdin.is_terminal() {
        eprint!("{prompt}");
        let _ = std::io::stderr().flush();
        let _guard = NoEcho::new();
        let mut s = String::new();
        stdin.lock().read_line(&mut s)?;
        eprintln!();
        s
    } else {
        let mut s = String::new();
        stdin.lock().read_line(&mut s)?;
        s
    };
    let t = line.trim().to_string();
    if t.is_empty() {
        bail!("empty token");
    }
    if t.chars().any(|c| c.is_whitespace() || c.is_control()) {
        bail!("token contains whitespace/control characters");
    }
    Ok(t)
}

struct NoEcho {
    #[cfg(unix)]
    orig: Option<libc::termios>,
}

impl NoEcho {
    fn new() -> Self {
        #[cfg(unix)]
        unsafe {
            let mut t: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(0, &mut t) == 0 {
                let orig = t;
                t.c_lflag &= !libc::ECHO;
                t.c_lflag |= libc::ECHONL;
                libc::tcsetattr(0, libc::TCSANOW, &t);
                return Self { orig: Some(orig) };
            }
            Self { orig: None }
        }
        #[cfg(not(unix))]
        Self {}
    }
}

impl Drop for NoEcho {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Some(o) = self.orig {
            unsafe {
                libc::tcsetattr(0, libc::TCSANOW, &o);
            }
        }
    }
}

fn owner_repos(cfg: &crate::config::Config, owner: &str) -> Vec<String> {
    let mut v: Vec<String> = cfg
        .effective_repos()
        .iter()
        .map(|r| r.github.clone())
        .filter(|g| crate::gh_token::repo_owner(g).eq_ignore_ascii_case(owner))
        .collect();
    let sr = crate::shadow::shadow_repo(cfg);
    if crate::gh_token::repo_owner(&sr).eq_ignore_ascii_case(owner) && !v.contains(&sr) {
        v.push(sr);
    }
    v.dedup();
    v
}

/// One-line validation summary (no secrets) and whether it is a hard failure.
pub fn validate(token: &str, repos: &[String]) -> (String, bool) {
    let user = crate::gh_token::get(token, "user");
    match user.status {
        0 => {
            return (
                format!(
                    "could not reach GitHub ({}); saved anyway",
                    user.error.unwrap_or_default()
                ),
                false,
            )
        }
        200..=299 => {}
        s => return (format!("GitHub rejected the token: GET /user -> {s}"), true),
    }
    let mut out = format!("login={}", user.login.unwrap_or_else(|| "?".into()));
    match user.expires_at {
        Some(e) => {
            let d = crate::gh_token::days_left(e, chrono::Utc::now());
            out.push_str(&format!("; expires in {d}d"));
            if d < crate::gh_token::EXPIRY_WARN_DAYS {
                out.push_str(" (warn: <14d)");
            }
        }
        None => out.push_str("; no expiry"),
    }
    let mut bad = false;
    for r in repos {
        let st = crate::gh_token::get(token, &format!("repos/{r}")).status;
        let ok = (200..300).contains(&st);
        bad |= !ok;
        out.push_str(&format!(
            "; {r}={}",
            if ok { "ok".into() } else { st.to_string() }
        ));
    }
    if repos.is_empty() {
        out.push_str("; no watched repos for this owner yet");
    }
    (out, bad)
}

fn reload(config_path: &Path) -> String {
    match crate::config::load_config(config_path) {
        Ok(cfg) => match crate::util::sighup_daemon(&cfg) {
            Ok(()) => "sent SIGHUP to atr serve (config reloaded)".into(),
            Err(e) => format!("serve not signalled ({e})"),
        },
        Err(e) => format!("config reload skipped ({e:#})"),
    }
}

pub fn set(config_path: &Path, owner: &str, env_var: Option<&str>, token: &str) -> Result<String> {
    if !valid_owner(owner) {
        bail!("invalid owner {owner:?}");
    }
    let env = env_var
        .map(str::to_string)
        .unwrap_or_else(|| default_env_var(owner));
    if !valid_env_name(&env) {
        bail!("invalid env var name {env:?}");
    }
    if !config_path.exists() {
        bail!("no config at {}; run atr init first", config_path.display());
    }
    let shadow = crate::telegram_setup::shadow_env_path(config_path);
    let text = std::fs::read_to_string(config_path)?;
    let (new_text, prev) = edit_tokens_table(&text, owner, Some(&env))?;
    let cfg = crate::config::parse_config_toml(&new_text).context("config invalid after edit")?;
    let (summary, hard_fail) = validate(token, &owner_repos(&cfg, owner));
    let replaced = write_env_atomic(&shadow, &env, Some(token))?;
    write_config(config_path, &new_text)?;
    if let Some(p) = prev.filter(|p| *p != env) {
        let _ = write_env_atomic(&shadow, &p, None);
    }
    let mut out = format!(
        "{} {env} in {} (0600); [github.tokens] {owner} = \"{env}\"\n",
        if replaced { "replaced" } else { "stored" },
        shadow.display()
    );
    out.push_str(&format!("check: {summary}\n"));
    out.push_str(&format!("{}\n", reload(config_path)));
    if std::env::var_os(&env).is_some() || replaced {
        out.push_str("note: if atr serve has this variable in its environment, run `atr service restart` to pick up the new value\n");
    }
    if hard_fail {
        out.push_str("WARNING: validation failed; fix the token's repository access/permissions\n");
    }
    Ok(out)
}

pub fn remove(config_path: &Path, owner: &str) -> Result<String> {
    let text = std::fs::read_to_string(config_path)
        .with_context(|| format!("read {}", config_path.display()))?;
    let (new_text, prev) = edit_tokens_table(&text, owner, None)?;
    let Some(env) = prev else {
        bail!("no [github.tokens] entry for {owner}");
    };
    write_config(config_path, &new_text)?;
    let shadow = crate::telegram_setup::shadow_env_path(config_path);
    let had = write_env_atomic(&shadow, &env, None)?;
    Ok(format!(
        "removed [github.tokens] {owner} and {env}{} from {}; {owner} repos now use the default token\n{}\n",
        if had { "" } else { " (was not in file)" },
        shadow.display(),
        reload(config_path)
    ))
}

pub fn list(config_path: &Path) -> Result<String> {
    let cfg = crate::config::load_config(config_path)?;
    let mut rows: Vec<(String, String, Option<String>)> = cfg
        .github
        .tokens
        .iter()
        .map(|(o, k)| (o.clone(), k.clone(), None))
        .collect();
    if let Some((k, _)) = crate::gh_token::resolve(&cfg, Some(config_path)) {
        rows.push(("(default)".into(), k, None));
    } else {
        rows.push((
            "(default)".into(),
            crate::gh_token::PRIMARY_KEY.into(),
            None,
        ));
    }
    let keys: Vec<String> = crate::gh_token::keys_for(crate::gh_token::shadow_key(&cfg));
    let mut out = format!(
        "{:<16} {:<28} {:<6} days  repos\n",
        "owner", "env var", "status"
    );
    for (owner, key, _) in rows.iter_mut() {
        let value = if owner == "(default)" {
            crate::gh_token::resolve_with(&keys, Some(config_path), |k| std::env::var(k).ok())
                .map(|(_, v)| v)
        } else {
            crate::gh_token::resolve_with(std::slice::from_ref(key), Some(config_path), |k| {
                std::env::var(k).ok()
            })
            .map(|(_, v)| v)
        };
        let repos = if owner == "(default)" {
            Vec::new()
        } else {
            owner_repos(&cfg, owner)
        };
        let (status, days) = match value {
            None => ("fail".to_string(), "-".to_string()),
            Some(tok) => {
                let p = crate::gh_token::get(&tok, "user");
                let days = p
                    .expires_at
                    .map(|e| crate::gh_token::days_left(e, chrono::Utc::now()));
                let st = match p.status {
                    0 => "?",
                    200..=299 if days.is_some_and(|d| d < crate::gh_token::EXPIRY_WARN_DAYS) => {
                        "warn"
                    }
                    200..=299 => "ok",
                    _ => "fail",
                };
                (
                    st.to_string(),
                    days.map(|d| d.to_string()).unwrap_or_else(|| "-".into()),
                )
            }
        };
        out.push_str(&format!(
            "{:<16} {:<28} {:<6} {:<5} {}\n",
            owner,
            key,
            status,
            days,
            if repos.is_empty() {
                "-".into()
            } else {
                repos.join(",")
            }
        ));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_var_names() {
        assert_eq!(default_env_var("TaskrApps"), "ATR_GH_TOKEN_TASKRAPPS");
        assert_eq!(default_env_var("my-org.x"), "ATR_GH_TOKEN_MY_ORG_X");
        assert!(valid_env_name("ATR_GH_TOKEN_X"));
        assert!(!valid_env_name("1BAD"));
        assert!(!valid_env_name("A-B"));
        assert!(!valid_owner("a/b"));
    }

    #[test]
    fn tokens_table_edit_preserves_comments() {
        let text = "# top comment\n[github]\nenabled = false # keep me\n\n[[provider]]\nname = \"claude\"\nkind = \"claude\"\ncli = \"/bin/true\"\n";
        let (t, prev) =
            edit_tokens_table(text, "TaskrApps", Some("ATR_GH_TOKEN_TASKRAPPS")).unwrap();
        assert!(prev.is_none());
        assert!(
            t.contains("# top comment") && t.contains("# keep me"),
            "{t}"
        );
        assert!(
            t.contains("[github.tokens]") && t.contains("TaskrApps = \"ATR_GH_TOKEN_TASKRAPPS\""),
            "{t}"
        );
        let cfg = crate::config::parse_config_toml(&t).unwrap();
        assert_eq!(cfg.github.tokens.len(), 1);
        // case-insensitive replace
        let (t2, prev) = edit_tokens_table(&t, "taskrapps", Some("OTHER")).unwrap();
        assert_eq!(prev.as_deref(), Some("ATR_GH_TOKEN_TASKRAPPS"));
        let cfg = crate::config::parse_config_toml(&t2).unwrap();
        assert_eq!(cfg.github.tokens.len(), 1);
        assert_eq!(
            cfg.github.tokens.get("taskrapps").map(String::as_str),
            Some("OTHER")
        );
        let (t3, prev) = edit_tokens_table(&t2, "TASKRAPPS", None).unwrap();
        assert_eq!(prev.as_deref(), Some("OTHER"));
        assert!(crate::config::parse_config_toml(&t3)
            .unwrap()
            .github
            .tokens
            .is_empty());
    }

    #[test]
    fn env_file_atomic_upsert_and_remove() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("shadow.env");
        std::fs::write(&p, "# c\nATR_TELEGRAM_TOKEN=keep\nATR_GH_TOKEN_X=old\n").unwrap();
        assert!(write_env_atomic(&p, "ATR_GH_TOKEN_X", Some("new")).unwrap());
        let t = std::fs::read_to_string(&p).unwrap();
        assert!(
            t.contains("ATR_GH_TOKEN_X=new")
                && !t.contains("old")
                && t.contains("keep")
                && t.contains("# c")
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&p).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        assert!(!write_env_atomic(&p, "ATR_GH_TOKEN_Y", Some("y")).unwrap());
        assert!(write_env_atomic(&p, "ATR_GH_TOKEN_X", None).unwrap());
        let t = std::fs::read_to_string(&p).unwrap();
        assert!(!t.contains("ATR_GH_TOKEN_X") && t.contains("ATR_GH_TOKEN_Y=y"));
    }

    #[test]
    fn set_and_remove_roundtrip_offline() {
        let dir = tempfile::tempdir().unwrap();
        let cp = dir.path().join("config.toml");
        std::fs::write(&cp, "# mine\n[github]\nenabled = false\n\n[[provider]]\nname = \"claude\"\nkind = \"claude\"\ncli = \"/bin/true\"\n").unwrap();
        // Unreachable API: validation is a soft warning, values still stored.
        std::env::set_var("ATR_GITHUB_API", "http://127.0.0.1:9");
        let out = set(&cp, "TaskrApps", None, "tok-secret-value").unwrap();
        assert!(!out.contains("tok-secret-value"), "{out}");
        assert!(out.contains("ATR_GH_TOKEN_TASKRAPPS"));
        let env = std::fs::read_to_string(dir.path().join("shadow.env")).unwrap();
        assert!(env.contains("ATR_GH_TOKEN_TASKRAPPS=tok-secret-value"));
        let lst = list(&cp).unwrap();
        assert!(
            lst.contains("TaskrApps") && !lst.contains("tok-secret-value"),
            "{lst}"
        );
        let out = remove(&cp, "taskrapps").unwrap();
        assert!(out.contains("removed"));
        let env = std::fs::read_to_string(dir.path().join("shadow.env")).unwrap();
        assert!(!env.contains("ATR_GH_TOKEN_TASKRAPPS"));
        assert!(std::fs::read_to_string(&cp).unwrap().contains("# mine"));
        assert!(remove(&cp, "taskrapps").is_err());
        std::env::remove_var("ATR_GITHUB_API");
    }
}
