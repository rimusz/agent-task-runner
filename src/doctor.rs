use crate::config::Config;
use crate::providers::{check_version, pin_label};
use serde::Serialize;
use std::net::TcpListener;
use std::path::Path;
use std::process::Command;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Ok,
    Warn,
    Fail,
}

#[derive(Debug, Clone, Serialize)]
pub struct Check {
    pub name: String,
    pub status: Status,
    pub detail: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub checks: Vec<Check>,
    pub problems: usize,
}

impl Report {
    pub fn problems(&self) -> usize {
        self.problems
    }

    pub fn ok(&self) -> bool {
        !self.checks.iter().any(|c| c.status == Status::Fail)
    }

    pub fn format(&self) -> String {
        let mut out = String::new();
        for c in &self.checks {
            let tag = match c.status {
                Status::Ok => "ok  ",
                Status::Warn => "warn",
                Status::Fail => "FAIL",
            };
            out.push_str(&format!("{tag}  {:<22} {}\n", c.name, c.detail));
            if c.status != Status::Ok {
                if let Some(h) = &c.hint {
                    out.push_str(&format!("      fix: {h}\n"));
                }
            }
        }
        out.push_str(&format!(
            "{} problem{}\n",
            self.problems,
            if self.problems == 1 { "" } else { "s" }
        ));
        out
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_else(|_| "{}".into())
    }
}

fn push(checks: &mut Vec<Check>, name: &str, status: Status, detail: String, hint: Option<String>) {
    checks.push(Check {
        name: name.to_string(),
        status,
        detail: crate::github_auth::redact_secrets(&detail),
        hint: hint.map(|h| crate::github_auth::redact_secrets(&h)),
    });
}

pub async fn run(cfg: &Config, config_path: &Path, cache: bool) -> Report {
    let mut checks = Vec::new();
    push(
        &mut checks,
        "config",
        Status::Ok,
        config_path.display().to_string(),
        None,
    );

    github_auth_checks(&mut checks, cfg, config_path);
    repo_path_check(&mut checks, cfg);
    listen_check(&mut checks, cfg);
    reported_usage_check(&mut checks, cfg);
    skip_at_check(&mut checks, cfg);
    telegram_check(&mut checks, cfg, config_path);
    mcp_check(&mut checks, cfg);

    if !cfg.system_prompt_file.exists() {
        push(
            &mut checks,
            "system_prompt",
            Status::Fail,
            format!("{} missing", cfg.system_prompt_file.display()),
            Some("run atr init, or copy config/system-prompt.md".into()),
        );
    } else {
        push(
            &mut checks,
            "system_prompt",
            Status::Ok,
            cfg.system_prompt_file.display().to_string(),
            None,
        );
    }

    for p in &cfg.providers {
        let v = check_version(p).await;
        let pin = pin_label(&v);
        if !v.ok_path {
            push(
                &mut checks,
                &format!("cli {}", p.name),
                Status::Fail,
                format!("{} missing", p.cli.display()),
                Some("install the CLI or set [[provider]] cli".into()),
            );
        } else if v.keychain_locked {
            push(
                &mut checks,
                &format!("cli {}", p.name),
                Status::Warn,
                format!("{} pin={pin} (keychain locked over SSH)", p.cli.display()),
                Some("run atr doctor from the desktop session".into()),
            );
        } else if !v.match_ok {
            let hint = if v.error.as_deref().is_some_and(looks_like_auth_fail) {
                crate::presets::auth_login_hint(&p.kind).to_string()
            } else {
                "update expected_version or the CLI".into()
            };
            push(
                &mut checks,
                &format!("cli {}", p.name),
                Status::Warn,
                format!(
                    "{} pin={pin} expected={} found={}",
                    p.cli.display(),
                    v.expected.as_deref().unwrap_or("-"),
                    v.version_text.as_deref().unwrap_or("-")
                ),
                Some(hint),
            );
        } else {
            push(
                &mut checks,
                &format!("cli {}", p.name),
                Status::Ok,
                format!(
                    "{} pin={pin} {}",
                    p.cli.display(),
                    v.version_text.as_deref().unwrap_or("-")
                ),
                None,
            );
        }
        if p.kind == "opencode" {
            opencode_preset_probe(&mut checks, p, config_path);
        }
        if p.kind == "grok" && v.ok_path {
            grok_min_version_check(&mut checks, p, &v);
        }
    }

    if cache {
        let (ok, text) = crate::app::doctor_cache(cfg);
        if ok {
            push(
                &mut checks,
                "cache prefix",
                Status::Ok,
                "stable".into(),
                None,
            );
        } else {
            push(
                &mut checks,
                "cache prefix",
                Status::Fail,
                text.replace('\n', " "),
                Some("keep the system prompt and tool preamble byte-stable".into()),
            );
        }
    }

    let problems = checks.iter().filter(|c| c.status != Status::Ok).count();
    Report { checks, problems }
}

fn mcp_check(checks: &mut Vec<Check>, cfg: &Config) {
    if !cfg.mcp.enabled {
        push(
            checks,
            "mcp http",
            Status::Ok,
            "disabled (atr mcp stdio still works)".into(),
            None,
        );
        return;
    }
    let have = crate::mcp::read_token(&cfg.mcp).is_some();
    let from_env = std::env::var(&cfg.mcp.token_env)
        .ok()
        .is_some_and(|s| !s.trim().is_empty());
    let file_ok = cfg.mcp.token_file.is_file();
    if have {
        let where_ = if from_env {
            format!("{} set", cfg.mcp.token_env)
        } else if file_ok {
            format!("token file {} (0600)", cfg.mcp.token_file.display())
        } else {
            "configured".into()
        };
        push(checks, "mcp token", Status::Ok, where_, None);
    } else {
        push(
            checks,
            "mcp token",
            Status::Warn,
            "HTTP /mcp enabled, no token".into(),
            Some(crate::mcp::token_missing_hint(&cfg.mcp)),
        );
    }
}

fn grok_min_version_check(
    checks: &mut Vec<Check>,
    p: &crate::config::Provider,
    v: &crate::providers::VersionCheck,
) {
    let found = v.version_text.as_deref().unwrap_or("");
    let min = crate::detect::GROK_MIN_VERSION;
    let min_s = format!("{}.{}.{}", min.0, min.1, min.2);
    if found.is_empty() {
        push(
            checks,
            "grok version",
            Status::Warn,
            format!("{} version unknown (need >= {min_s})", p.name),
            Some("planner JSON, plugin stdio, and grok usage need 1.0.52+".into()),
        );
        return;
    }
    if crate::detect::grok_meets_min(found) {
        push(
            checks,
            "grok version",
            Status::Ok,
            format!("{found} (>= {min_s})"),
            None,
        );
    } else {
        push(
            checks,
            "grok version",
            Status::Fail,
            format!("{found} < {min_s}"),
            Some("upgrade grok (planner JSON / plugin / usage)".into()),
        );
    }
}

fn looks_like_auth_fail(text: &str) -> bool {
    let t = text.to_ascii_lowercase();
    t.contains("unauthorized")
        || t.contains("not logged in")
        || t.contains("not authenticated")
        || t.contains("api key")
        || t.contains("auth login")
        || t.contains("authentication")
}

fn opencode_preset_probe(checks: &mut Vec<Check>, p: &crate::config::Provider, config_path: &Path) {
    let preset_name = if !p.preset.is_empty() {
        p.preset.as_str()
    } else {
        p.name.as_str()
    };
    let bp = crate::presets::builtin(preset_name);
    let mut have_key = false;
    if let Some(bp) = bp {
        if let Some(env_name) = bp.env_key {
            let shadow = crate::telegram_setup::shadow_env_path(config_path);
            let from_env = std::env::var(env_name)
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty());
            let from_shadow = crate::telegram_setup::read_shadow_env_var(&shadow, env_name);
            have_key = from_env.is_some() || from_shadow.is_some();
            if have_key {
                let detail = if from_env.is_some() {
                    format!("{env_name} set")
                } else {
                    format!("{env_name} in shadow.env")
                };
                push(checks, &format!("key {}", p.name), Status::Ok, detail, None);
            } else {
                push(
                    checks,
                    &format!("key {}", p.name),
                    Status::Warn,
                    format!("{env_name} unset"),
                    Some(format!(
                        "atr providers add {}  (key goes in shadow.env, never config)",
                        p.name
                    )),
                );
            }
        }
        if bp.needs_url {
            let oc = crate::opencode_cfg::config_path();
            if crate::opencode_cfg::provider_base_url(&oc, bp.oc_provider).is_some() {
                push(
                    checks,
                    &format!("url {}", p.name),
                    Status::Ok,
                    format!("{} in opencode.json", bp.oc_provider),
                    None,
                );
            } else {
                push(
                    checks,
                    &format!("url {}", p.name),
                    Status::Warn,
                    "base URL not in opencode.json".into(),
                    Some(format!("atr providers add {}", p.name)),
                );
            }
        }
    }
    if have_key || bp.is_some_and(|b| b.env_key.is_none()) {
        return;
    }
    opencode_auth_probe(checks, p);
}

fn opencode_auth_probe(checks: &mut Vec<Check>, p: &crate::config::Provider) {
    match Command::new(&p.cli).args(["auth", "list"]).output() {
        Ok(out) if out.status.success() => {
            let text = format!(
                "{}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
            let low = text.to_ascii_lowercase();
            if text.trim().is_empty() || low.contains("no provider") || low.contains("not logged") {
                push(
                    checks,
                    &format!("auth {}", p.name),
                    Status::Warn,
                    "no providers logged in".into(),
                    Some(crate::presets::auth_login_hint("opencode").into()),
                );
            } else {
                push(
                    checks,
                    &format!("auth {}", p.name),
                    Status::Ok,
                    "opencode auth list ok".into(),
                    None,
                );
            }
        }
        Ok(out) => {
            let err = String::from_utf8_lossy(&out.stderr);
            let detail = err.lines().next().unwrap_or("auth list failed").trim();
            push(
                checks,
                &format!("auth {}", p.name),
                Status::Warn,
                detail.to_string(),
                Some(crate::presets::auth_login_hint("opencode").into()),
            );
        }
        Err(_) => {}
    }
}

fn github_auth_checks(checks: &mut Vec<Check>, cfg: &Config, config_path: &Path) {
    // GitHub intake off: skip gh auth/token/ssh like each other. Shadow still
    // checks its own token without requiring `gh auth status`.
    let need = cfg.intake_allows(crate::task::SourceKind::Github);
    let opts = crate::github_auth::probe_opts_from_config(cfg, config_path);
    let snap = if need {
        crate::github_auth::probe(opts)
    } else {
        crate::github_auth::probe_local(opts)
    };
    push_github_auth_checks(
        checks,
        &snap,
        cfg.intake_allows(crate::task::SourceKind::Github),
        cfg.shadow.enabled,
    );
}

fn push_github_auth_checks(
    checks: &mut Vec<Check>,
    snap: &crate::github_auth::Snapshot,
    github_enabled: bool,
    shadow_enabled: bool,
) {
    if !github_enabled {
        let why = if shadow_enabled {
            "skipped (github off)"
        } else {
            "skipped (github/shadow off)"
        };
        push(checks, "gh auth", Status::Ok, why.into(), None);
        push(checks, "gh token", Status::Ok, why.into(), None);
        push(
            checks,
            "ssh github",
            Status::Ok,
            "skipped (github off)".into(),
            None,
        );
        push_shadow_token(checks, snap, shadow_enabled);
        push_env_mode(checks, snap);
        return;
    }

    if snap.gh_ok {
        push(checks, "gh auth", Status::Ok, snap.gh_detail.clone(), None);
    } else if snap.token == crate::github_auth::Presence::ShadowEnv
        && !crate::github_auth::token_in_process_env()
    {
        push(
            checks,
            "gh auth",
            Status::Warn,
            "gh auth status failed in this shell; GH_TOKEN is in shadow.env for serve".into(),
            Some("restart atr serve after editing shadow.env; or gh auth login".into()),
        );
    } else {
        push(
            checks,
            "gh auth",
            Status::Fail,
            snap.gh_detail.clone(),
            snap.gh_hint.clone(),
        );
    }

    push_token_check(checks, snap);
    if snap.ssh_ok {
        push(
            checks,
            "ssh github",
            Status::Ok,
            snap.ssh_detail.clone(),
            None,
        );
    } else {
        push(
            checks,
            "ssh github",
            Status::Warn,
            snap.ssh_detail.clone(),
            Some(
                "only needed for SSH remotes: ssh-keygen; add the key; ssh -T git@github.com"
                    .into(),
            ),
        );
    }

    push_shadow_token(checks, snap, shadow_enabled);
    push_env_mode(checks, snap);
}

fn push_token_check(checks: &mut Vec<Check>, snap: &crate::github_auth::Snapshot) {
    let mut detail = snap.token.detail(&snap.token_key);
    if let Some(s) = &snap.scopes {
        detail.push_str(&format!(
            "; scopes={} ({})",
            s,
            snap.scopes_kind.as_deref().unwrap_or("unknown")
        ));
    }
    if snap.token.is_set() {
        if snap.missing_perms.is_empty() {
            push(checks, "gh token", Status::Ok, detail, None);
        } else {
            push(
                checks,
                "gh token",
                Status::Warn,
                format!("{detail}; missing {}", snap.missing_perms.join(", ")),
                Some(
                    "fine-grained: Contents RW, Issues RW, Pull requests RW, Metadata R. Classic: repo"
                        .into(),
                ),
            );
        }
    } else if snap.gh_ok {
        let extra = snap
            .scopes
            .as_deref()
            .map(|s| format!("; scopes={s}"))
            .unwrap_or_default();
        push(
            checks,
            "gh token",
            Status::Ok,
            format!("unset (using gh auth login){extra}"),
            None,
        );
    } else {
        push(
            checks,
            "gh token",
            Status::Fail,
            detail,
            Some("put GH_TOKEN in ~/.config/atr/shadow.env (mode 0600), never in config".into()),
        );
    }
}

fn push_shadow_token(checks: &mut Vec<Check>, snap: &crate::github_auth::Snapshot, enabled: bool) {
    let key = snap.shadow_key.as_str();
    if enabled {
        if snap.shadow.is_set() {
            push(
                checks,
                "shadow token",
                Status::Ok,
                snap.shadow.detail(key),
                None,
            );
        } else {
            push(
                checks,
                "shadow token",
                Status::Fail,
                snap.shadow.detail(key),
                Some("put a read-only token in ~/.config/atr/shadow.env as this env name".into()),
            );
        }
    } else {
        push(
            checks,
            "shadow token",
            Status::Ok,
            format!("{} (shadow off)", snap.shadow.detail(key)),
            None,
        );
    }
}

fn push_env_mode(checks: &mut Vec<Check>, snap: &crate::github_auth::Snapshot) {
    let Some(mode) = snap.shadow_env_mode else {
        return;
    };
    if mode & 0o077 != 0 {
        push(
            checks,
            "shadow.env mode",
            Status::Warn,
            format!("0o{mode:o} (expected 0600)"),
            Some("chmod 600 ~/.config/atr/shadow.env".into()),
        );
    }
}

fn repo_path_check(checks: &mut Vec<Check>, cfg: &Config) {
    let repos = cfg.effective_repos();
    let empty = repos.is_empty()
        || (repos.len() == 1 && repos[0].shorthand && repos[0].path.as_os_str().is_empty());
    if empty {
        if cfg.intake_allows(crate::task::SourceKind::Github) {
            push(
                checks,
                "repo_path",
                Status::Fail,
                "empty".into(),
                Some("set repo_path or [[repo]] in config.toml (atr init / atr repos add)".into()),
            );
        } else {
            push(
                checks,
                "repo_path",
                Status::Warn,
                "empty (no default git repo)".into(),
                Some("optional for answer-only / --dir tasks; set repo_path or [[repo]] for git work".into()),
            );
        }
        return;
    }
    for r in &repos {
        let name = if r.shorthand {
            "repo_path".into()
        } else {
            format!("repo.{}", r.name)
        };
        if r.path.as_os_str().is_empty() {
            push(
                checks,
                &name,
                Status::Fail,
                "empty".into(),
                Some("set path on this [[repo]]".into()),
            );
            continue;
        }
        if !r.path.exists() {
            push(
                checks,
                &name,
                Status::Fail,
                format!("{} does not exist", r.path.display()),
                Some("create the repo or atr repos add owner/name --path <dir>".into()),
            );
            continue;
        }
        if r.path.join(".git").exists() || r.path.join(".git").is_file() {
            push(
                checks,
                &name,
                Status::Ok,
                format!("{} (git)", r.path.display()),
                None,
            );
        } else {
            push(
                checks,
                &name,
                Status::Fail,
                format!("{} is not a git repo", r.path.display()),
                Some(format!("git -C {} init", r.path.display())),
            );
        }
    }
}

fn listen_check(checks: &mut Vec<Check>, cfg: &Config) {
    if cfg.listen.trim().is_empty() {
        push(
            checks,
            "listen",
            Status::Fail,
            "empty".into(),
            Some("set listen = \"127.0.0.1:7420\"".into()),
        );
        return;
    }
    if ureq::get(&format!("http://{}/health", cfg.listen))
        .timeout(std::time::Duration::from_secs(1))
        .call()
        .is_ok()
    {
        push(
            checks,
            "listen",
            Status::Ok,
            format!("{} daemon running", cfg.listen),
            None,
        );
        return;
    }
    match TcpListener::bind(&cfg.listen) {
        Ok(_) => push(
            checks,
            "listen",
            Status::Warn,
            format!("{} free (daemon not running)", cfg.listen),
            Some("atr serve  or  atr service install".into()),
        ),
        Err(e) => push(
            checks,
            "listen",
            Status::Fail,
            format!("{} in use ({e})", cfg.listen),
            Some("stop the other process or change listen".into()),
        ),
    }
}

fn reported_usage_check(checks: &mut Vec<Check>, cfg: &Config) {
    let mut bad = Vec::new();
    for p in &cfg.providers {
        let expect = match p.kind.as_str() {
            "claude" => "claude_stream",
            "grok" => "grok_billing",
            _ => "none",
        };
        if p.reported_usage != expect {
            bad.push(format!(
                "{}={} (kind {} wants {expect})",
                p.name, p.reported_usage, p.kind
            ));
        }
    }
    if bad.is_empty() {
        push(
            checks,
            "reported_usage",
            Status::Ok,
            "matches kind".into(),
            None,
        );
    } else {
        push(
            checks,
            "reported_usage",
            Status::Warn,
            bad.join("; "),
            Some("set reported_usage to the kind default or none".into()),
        );
    }
}

fn skip_at_check(checks: &mut Vec<Check>, cfg: &Config) {
    let mut bad = Vec::new();
    for p in &cfg.providers {
        if !(0.0..=1.0).contains(&p.skip_at) {
            bad.push(format!("{}={}", p.name, p.skip_at));
        }
    }
    if bad.is_empty() {
        push(checks, "skip_at", Status::Ok, "all in 0..1".into(), None);
    } else {
        push(
            checks,
            "skip_at",
            Status::Fail,
            bad.join(", "),
            Some("set skip_at between 0 and 1".into()),
        );
    }
}

fn telegram_check(checks: &mut Vec<Check>, cfg: &Config, config_path: &Path) {
    if !cfg.telegram.enabled {
        push(checks, "telegram", Status::Ok, "disabled".into(), None);
        return;
    }
    let shadow = crate::telegram_setup::shadow_env_path(config_path);
    let key = cfg.telegram.bot_token_env.as_str();
    let from_env = std::env::var(key)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let from_shadow = crate::telegram_setup::read_shadow_env_var(&shadow, key);
    let from_file = crate::telegram::bot_token(&cfg.telegram).filter(|_| from_env.is_none());
    if from_env.is_some() || from_shadow.is_some() || from_file.is_some() {
        let detail = if from_env.is_some() {
            format!("{key} set")
        } else if from_shadow.is_some() {
            format!("{key} in shadow.env")
        } else {
            "token_file set".into()
        };
        push(checks, "telegram token", Status::Ok, detail, None);
    } else {
        push(
            checks,
            "telegram token",
            Status::Fail,
            format!("{key} unset"),
            Some("atr telegram setup  (token goes in shadow.env, not config)".into()),
        );
    }
    if cfg.telegram.allowed_chat_ids.is_empty() {
        push(
            checks,
            "telegram chat",
            Status::Fail,
            "allowed_chat_ids empty".into(),
            Some("atr telegram setup".into()),
        );
    } else {
        let ids: Vec<String> = cfg
            .telegram
            .allowed_chat_ids
            .iter()
            .map(|i| i.to_string())
            .collect();
        push(
            checks,
            "telegram chat",
            Status::Ok,
            format!("allowed_chat_ids=[{}]", ids.join(", ")),
            None,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_one_line_per_check_and_problem_count() {
        let report = Report {
            checks: vec![
                Check {
                    name: "config".into(),
                    status: Status::Ok,
                    detail: "/tmp/c.toml".into(),
                    hint: None,
                },
                Check {
                    name: "repo_path".into(),
                    status: Status::Fail,
                    detail: "empty".into(),
                    hint: Some("set repo_path in config.toml (atr init)".into()),
                },
                Check {
                    name: "listen".into(),
                    status: Status::Warn,
                    detail: "free".into(),
                    hint: Some("atr serve".into()),
                },
            ],
            problems: 2,
        };
        let text = report.format();
        assert!(text.contains("ok    config"), "{text}");
        assert!(text.contains("FAIL  repo_path"), "{text}");
        assert!(text.contains("warn  listen"), "{text}");
        assert!(text.contains("fix: set repo_path"), "{text}");
        assert!(text.contains("2 problems"), "{text}");
        assert!(!report.ok());
    }

    fn mini_cfg(telegram: &str) -> (tempfile::TempDir, crate::config::Config, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::write(dir.path().join("system-prompt.md"), "p\n").unwrap();
        let text = format!(
            r#"
listen = "127.0.0.1:17991"
repo_path = "{repo}"
system_prompt_file = "{prompt}"
[mcp]
token_env = "ATR_MCP_DOCTOR_TEST_UNSET"
token_file = "{token}"
{telegram}
[[provider]]
name = "claude"
kind = "claude"
cli = "/bin/true"
"#,
            repo = repo.display(),
            prompt = dir.path().join("system-prompt.md").display(),
            token = dir.path().join("missing.mcp.token").display(),
        );
        std::fs::write(&path, &text).unwrap();
        let cfg = crate::config::parse_config_toml(&text).unwrap();
        (dir, cfg, path)
    }

    #[tokio::test]
    async fn telegram_disabled_is_ok() {
        let (_dir, cfg, path) = mini_cfg(
            r#"
[telegram]
enabled = false
allowed_chat_ids = []
"#,
        );
        let report = run(&cfg, &path, false).await;
        let tg = report.checks.iter().find(|c| c.name == "telegram").unwrap();
        assert_eq!(tg.status, Status::Ok);
        assert_eq!(tg.detail, "disabled");
        assert!(report.checks.iter().all(|c| c.name != "telegram token"));
    }

    #[tokio::test]
    async fn telegram_enabled_requires_token_and_chat() {
        let (_dir, cfg, path) = mini_cfg(
            r#"
[telegram]
enabled = true
bot_token_env = "ATR_DOCTOR_TG_MISSING"
allowed_chat_ids = []
"#,
        );
        let report = run(&cfg, &path, false).await;
        let tok = report
            .checks
            .iter()
            .find(|c| c.name == "telegram token")
            .unwrap();
        let chat = report
            .checks
            .iter()
            .find(|c| c.name == "telegram chat")
            .unwrap();
        assert_eq!(tok.status, Status::Fail);
        assert_eq!(chat.status, Status::Fail);
        assert!(tok.hint.as_deref().unwrap_or("").contains("telegram setup"));
    }

    #[tokio::test]
    async fn telegram_enabled_ok_from_shadow_env() {
        let (dir, cfg, path) = mini_cfg(
            r#"
[telegram]
enabled = true
bot_token_env = "ATR_DOCTOR_TG_FILE"
allowed_chat_ids = [42]
"#,
        );
        let shadow = dir.path().join("shadow.env");
        std::fs::write(&shadow, "ATR_DOCTOR_TG_FILE=from-file\n").unwrap();
        let report = run(&cfg, &path, false).await;
        let tok = report
            .checks
            .iter()
            .find(|c| c.name == "telegram token")
            .unwrap();
        let chat = report
            .checks
            .iter()
            .find(|c| c.name == "telegram chat")
            .unwrap();
        assert_eq!(tok.status, Status::Ok, "{:?}", tok);
        assert!(tok.detail.contains("shadow.env"), "{}", tok.detail);
        assert_eq!(chat.status, Status::Ok);
        assert!(chat.detail.contains("42"), "{}", chat.detail);
    }

    #[tokio::test]
    async fn opencode_preset_key_from_shadow_env() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::write(dir.path().join("system-prompt.md"), "p\n").unwrap();
        crate::opencode_cfg::set_test_path(Some(dir.path().join("opencode.json")));
        let text = format!(
            r#"
listen = "127.0.0.1:17992"
repo_path = "{repo}"
system_prompt_file = "{prompt}"
[[provider]]
name = "kimi"
kind = "opencode"
preset = "kimi"
cli = "/bin/true"
"#,
            repo = repo.display(),
            prompt = dir.path().join("system-prompt.md").display(),
        );
        std::fs::write(&path, &text).unwrap();
        let cfg = crate::config::parse_config_toml(&text).unwrap();
        let report = run(&cfg, &path, false).await;
        let key = report
            .checks
            .iter()
            .find(|c| c.name == "key kimi")
            .unwrap_or_else(|| panic!("missing key kimi\n{}", report.format()));
        assert_eq!(key.status, Status::Warn, "{:?}", key);
        std::fs::write(dir.path().join("shadow.env"), "MOONSHOT_API_KEY=sk-test\n").unwrap();
        let report = run(&cfg, &path, false).await;
        let key = report
            .checks
            .iter()
            .find(|c| c.name == "key kimi")
            .unwrap_or_else(|| panic!("missing key kimi\n{}", report.format()));
        assert_eq!(key.status, Status::Ok, "{:?}", key);
        assert!(key.detail.contains("shadow.env"), "{}", key.detail);
        assert!(
            !report.checks.iter().any(|c| c.name == "auth kimi"),
            "skip opencode auth login when key is present: {}",
            report.format()
        );
        crate::opencode_cfg::set_test_path(None);
    }

    #[tokio::test]
    async fn github_off_skips_auth_token_and_ssh() {
        let (_dir, cfg, path) = mini_cfg("");
        let report = run(&cfg, &path, false).await;
        for name in ["gh auth", "gh token", "ssh github"] {
            let c = report
                .checks
                .iter()
                .find(|c| c.name == name)
                .unwrap_or_else(|| panic!("missing {name}\n{}", report.format()));
            assert_eq!(c.status, Status::Ok, "{:?}", c);
            assert!(c.detail.contains("skipped"), "{} {}", name, c.detail);
        }
        let sh = report
            .checks
            .iter()
            .find(|c| c.name == "shadow token")
            .unwrap();
        assert_eq!(sh.status, Status::Ok);
        assert!(sh.detail.contains("unset (shadow off)"), "{}", sh.detail);
    }

    #[tokio::test]
    async fn github_off_shadow_on_skips_gh_auth() {
        let (_dir, mut cfg, path) = mini_cfg("");
        cfg.shadow.enabled = true;
        let report = run(&cfg, &path, false).await;
        for name in ["gh auth", "gh token", "ssh github"] {
            let c = report
                .checks
                .iter()
                .find(|c| c.name == name)
                .unwrap_or_else(|| panic!("missing {name}\n{}", report.format()));
            assert_eq!(c.status, Status::Ok, "{:?}", c);
            assert!(
                c.detail.contains("skipped") && c.detail.contains("github off"),
                "{} {}",
                name,
                c.detail
            );
        }
        let sh = report
            .checks
            .iter()
            .find(|c| c.name == "shadow token")
            .unwrap();
        assert_eq!(sh.status, Status::Fail, "{:?}", sh);
    }

    #[tokio::test]
    async fn doctor_never_prints_github_token_value() {
        let (dir, mut cfg, path) = mini_cfg("");
        cfg.github.enabled = true;
        cfg.github.repo = "owner/repo".into();
        let secret = "ghp_DOCTORLEAKTEST_abcdefghijklmnopqrstuv";
        std::fs::write(
            dir.path().join("shadow.env"),
            format!("GH_TOKEN={secret}\n"),
        )
        .unwrap();
        let report = run(&cfg, &path, false).await;
        let blob = report.format();
        let json = report.to_json();
        assert!(
            !blob.contains(secret) && !json.contains(secret),
            "token leaked:\n{blob}\n{json}"
        );
        assert!(!crate::github_auth::looks_like_secret(&blob), "{blob}");
        let tok = report
            .checks
            .iter()
            .find(|c| c.name == "gh token")
            .unwrap_or_else(|| panic!("{}", report.format()));
        assert!(
            tok.detail.contains("GH_TOKEN in shadow.env") || tok.detail.contains("GH_TOKEN set"),
            "{}",
            tok.detail
        );
        assert!(report.checks.iter().any(|c| c.name == "ssh github"));
        assert!(report.checks.iter().any(|c| c.name == "gh auth"));
    }

    #[tokio::test]
    async fn mcp_token_warns_when_http_enabled_without_token() {
        let (_dir, cfg, path) = mini_cfg("");
        let report = run(&cfg, &path, false).await;
        let mcp = report
            .checks
            .iter()
            .find(|c| c.name == "mcp token")
            .unwrap_or_else(|| panic!("{}", report.format()));
        assert_eq!(mcp.status, Status::Warn, "{:?}", mcp);
        let text = report.format();
        assert!(text.contains("fix:"), "{text}");
        assert!(text.contains("atr init"), "{text}");
        assert!(
            text.contains("token file") || text.contains("mcp.token"),
            "{text}"
        );
        assert!(!crate::mcp::read_token(&cfg.mcp).is_some_and(|t| text.contains(&t)));
    }

    #[tokio::test]
    async fn mcp_token_ok_from_0600_file() {
        let (dir, mut cfg, path) = mini_cfg("");
        let token_path = dir.path().join("mcp.token");
        crate::mcp::write_token_file(&token_path, "doctor-mcp-file-token").unwrap();
        cfg.mcp.token_env = "ATR_MCP_DOCTOR_TEST_UNSET".into();
        cfg.mcp.token_file = token_path;
        let report = run(&cfg, &path, false).await;
        let mcp = report
            .checks
            .iter()
            .find(|c| c.name == "mcp token")
            .unwrap_or_else(|| panic!("{}", report.format()));
        assert_eq!(mcp.status, Status::Ok, "{:?}", mcp);
        assert!(mcp.detail.contains("token file"), "{}", mcp.detail);
        assert!(!report.format().contains("doctor-mcp-file-token"));
        assert!(!report.to_json().contains("doctor-mcp-file-token"));
    }
}
