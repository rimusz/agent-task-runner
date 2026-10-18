use crate::config::Config;
use crate::providers::check_version;
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

/// With lessons on, the reflect provider must exist and be enabled.
/// `[telegram.assistant] providers` wins over `provider`; say so when both
/// are set so the single pin isn't silently ignored.
fn assistant_providers_check(checks: &mut Vec<Check>, cfg: &Config) {
    let ast = &cfg.telegram.assistant;
    if !ast.enabled {
        return;
    }
    if ast.both_set() {
        push(
            checks,
            "telegram assistant",
            Status::Warn,
            format!(
                "both provider = {:?} and providers = [{}] are set; providers wins",
                ast.provider.trim(),
                ast.providers.join(", ")
            ),
            Some("remove telegram.assistant.provider (or empty providers)".into()),
        );
    } else {
        push(
            checks,
            "telegram assistant",
            Status::Ok,
            format!("provider {}", ast.display()),
            None,
        );
    }
}

#[cfg(test)]
mod assistant_providers_tests {
    use super::*;

    #[test]
    fn warns_when_both_provider_and_providers_are_set() {
        let mut cfg = crate::config::parse_config_toml(
            r#"
[[provider]]
name = "claude"
kind = "claude"
cli = "/bin/true"
[[provider]]
name = "grok"
kind = "grok"
cli = "/bin/true"
"#,
        )
        .unwrap();
        cfg.telegram.assistant.enabled = true;
        let mut checks = Vec::new();
        assistant_providers_check(&mut checks, &cfg);
        assert!(matches!(checks[0].status, Status::Ok));
        assert!(checks[0].detail.contains("first available"));
        cfg.telegram.assistant.providers = vec!["grok".into(), "claude".into()];
        cfg.telegram.assistant.provider = "claude".into();
        let mut checks = Vec::new();
        assistant_providers_check(&mut checks, &cfg);
        assert!(matches!(checks[0].status, Status::Warn));
        assert!(
            checks[0].detail.contains("providers wins"),
            "{}",
            checks[0].detail
        );
    }
}

/// Compare judges on verify; without verify commands it can't pick a
/// winner, and `atr add --compare` refuses that repo.
fn compare_check(checks: &mut Vec<Check>, cfg: &Config) {
    if !cfg.verify.enabled {
        push(
            checks,
            "compare",
            Status::Ok,
            "off: [verify] is disabled, so `atr add --compare` refuses (compare can't judge)"
                .into(),
            None,
        );
        return;
    }
    let repos = cfg.effective_repos();
    let missing: Vec<String> = repos
        .iter()
        .filter(|r| crate::verify::commands_for(cfg, &r.path, &r.path).is_empty())
        .map(|r| r.name.clone())
        .collect();
    if missing.is_empty() {
        push(
            checks,
            "compare",
            Status::Ok,
            "verify commands set; compare can judge".into(),
            None,
        );
    } else {
        push(
            checks,
            "compare",
            Status::Warn,
            format!(
                "no verify commands for {}: compare can't judge there (`atr add --compare` refuses)",
                missing.join(", ")
            ),
            Some("set [verify] commands or a [[verify.repo]] entry with commands".into()),
        );
    }
}

fn lessons_check(checks: &mut Vec<Check>, cfg: &Config) {
    let on = cfg.lessons.enabled
        || cfg
            .effective_repos()
            .iter()
            .any(|r| r.lessons == Some(true));
    if !on {
        return;
    }
    let want = if cfg.lessons.provider.is_empty() {
        cfg.driver_name()
    } else {
        Some(cfg.lessons.provider.clone())
    };
    match want.as_deref().and_then(|n| cfg.provider(n)) {
        Some(p) if p.enabled => push(
            checks,
            "lessons",
            Status::Ok,
            format!("on; reflect provider {}", p.name),
            None,
        ),
        _ => push(
            checks,
            "lessons",
            Status::Warn,
            format!(
                "on, but reflect provider {} is missing or disabled; only manual lessons will be added",
                want.as_deref().unwrap_or("(none)")
            ),
            Some("set [lessons] provider to an enabled provider, or enable the driver".into()),
        ),
    }
}

/// Unknown keys are ignored on load (so rollbacks work); surface them here.
fn unknown_keys_check(checks: &mut Vec<Check>, cfg: &Config) {
    if cfg.unknown_keys.is_empty() {
        return;
    }
    let list: Vec<String> = cfg.unknown_keys.iter().map(|k| k.to_string()).collect();
    push(
        checks,
        "config keys",
        Status::Warn,
        format!("ignored unknown key(s): {}", list.join(", ")),
        Some(
            "fix the typo, remove the key, or upgrade atr if it belongs to a newer version".into(),
        ),
    );
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
    unknown_keys_check(&mut checks, cfg);
    lessons_check(&mut checks, cfg);
    compare_check(&mut checks, cfg);
    assistant_providers_check(&mut checks, cfg);

    github_auth_checks(&mut checks, cfg, config_path);
    github_token_api_check(&mut checks, cfg, config_path).await;
    repo_path_check(&mut checks, cfg);
    listen_check(&mut checks, cfg);
    reported_usage_check(&mut checks, cfg);
    skip_at_check(&mut checks, cfg);
    telegram_check(&mut checks, cfg, config_path);
    mcp_check(&mut checks, cfg);
    path_check(&mut checks);

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
        if !v.ok_path {
            push(
                &mut checks,
                &format!("cli {}", p.name),
                Status::Fail,
                format!("{} missing", p.cli.display()),
                Some("install the CLI or set [[provider]] cli".into()),
            );
        } else if v.keychain_locked {
            let (st, detail, hint) = locked_version_verdict(
                &p.cli.display().to_string(),
                crate::providers::recorded_version(config_path, &p.name),
            );
            push(&mut checks, &format!("cli {}", p.name), st, detail, hint);
        } else if let Some(err) = v.error.as_deref() {
            let hint = if looks_like_auth_fail(err) {
                crate::presets::auth_login_hint(&p.kind).to_string()
            } else {
                format!("check that `{} --version` runs", p.cli.display())
            };
            push(
                &mut checks,
                &format!("cli {}", p.name),
                Status::Warn,
                format!("{} version unreadable: {err}", p.cli.display()),
                Some(hint),
            );
        } else {
            push(
                &mut checks,
                &format!("cli {}", p.name),
                Status::Ok,
                format!(
                    "{} {}",
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

    let legacy: Vec<&str> = cfg
        .providers
        .iter()
        .filter(|p| p.legacy_expected_version)
        .map(|p| p.name.as_str())
        .collect();
    if !legacy.is_empty() {
        push(
            &mut checks,
            "expected_version",
            Status::Ok,
            format!(
                "ignored (CLI version pins were removed); safe to delete from [[provider]] {}",
                legacy.join(", ")
            ),
            None,
        );
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

/// Warn when the running `atr` binary's directory is not on `PATH`.
fn path_check(checks: &mut Vec<Check>) {
    let Some(dir) = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
    else {
        return;
    };
    let path = std::env::var_os("PATH").unwrap_or_default();
    if dir_on_path(&dir, &path) {
        push(
            checks,
            "atr on PATH",
            Status::Ok,
            dir.display().to_string(),
            None,
        );
    } else {
        push(
            checks,
            "atr on PATH",
            Status::Warn,
            format!("{} not on PATH", dir.display()),
            Some(format!(
                "add `export PATH=\"{}:$PATH\"` to ~/.zshenv (zsh) or ~/.bashrc (bash)",
                dir.display()
            )),
        );
    }
}

fn dir_on_path(dir: &std::path::Path, path: &std::ffi::OsStr) -> bool {
    let want = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    std::env::split_paths(path).any(|p| std::fs::canonicalize(&p).unwrap_or(p) == want)
}

fn mcp_check(checks: &mut Vec<Check>, cfg: &Config) {
    if crate::mcp::read_token(&cfg.mcp).is_some() {
        push(
            checks,
            "http write auth",
            Status::Ok,
            "POST /tasks, /messages, /cancel, /hooks need the bearer token".into(),
            None,
        );
    } else {
        push(
            checks,
            "http write auth",
            Status::Warn,
            "no token: HTTP write routes refuse (503); serve creates one at start".into(),
            Some(crate::mcp::token_missing_hint(&cfg.mcp)),
        );
    }
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

/// Keychain locked (doctor over SSH): show the version `atr serve` saw in
/// its own session; if none, skip instead of warning.
fn locked_version_verdict(
    cli: &str,
    recorded: Option<(String, String)>,
) -> (Status, String, Option<String>) {
    match recorded {
        Some((ver, at)) if !ver.is_empty() => (
            Status::Ok,
            format!("{cli} {ver} (seen by service at {at})"),
            None,
        ),
        _ => (
            Status::Ok,
            format!(
                "{cli} version skipped over SSH (keychain locked); service session records it at start"
            ),
            None,
        ),
    }
}

/// Live check of the shared atr GitHub token (the one the poller and shadow
/// use): present, valid (GET /user), repo access, days to expiry.
async fn github_token_api_check(checks: &mut Vec<Check>, cfg: &Config, config_path: &Path) {
    let need = cfg.intake_allows(crate::task::SourceKind::Github) || cfg.shadow.enabled;
    if !need {
        push(
            checks,
            "github token api",
            Status::Ok,
            "skipped (github/shadow off)".into(),
            None,
        );
        return;
    }
    for tu in crate::gh_token::tokens_in_use(cfg, Some(config_path)) {
        let name = match &tu.owner {
            Some(o) => format!("github token api {o}"),
            None => "github token api".to_string(),
        };
        let label = crate::gh_token::token_label(&tu.key, tu.owner.as_deref());
        let Some(tok) = tu.value.clone() else {
            let tried = if tu.owner.is_some() {
                format!("{} (then default chain)", tu.key)
            } else {
                crate::gh_token::token_keys(cfg).join(" / ")
            };
            push(
                checks,
                &name,
                Status::Fail,
                format!("no token in {tried}"),
                Some(format!(
                    "put {}=<fine-grained PAT> in ~/.config/atr/shadow.env (0600); see docs/GITHUB.md",
                    tu.key
                )),
            );
            continue;
        };
        let repos = tu.repos.clone();
        let res = tokio::task::spawn_blocking(move || {
            let user = crate::gh_token::get(&tok, "user");
            let repo_res: Vec<(String, u16)> = repos
                .iter()
                .map(|r| {
                    (
                        r.clone(),
                        crate::gh_token::get(&tok, &format!("repos/{r}")).status,
                    )
                })
                .collect();
            (user, repo_res)
        })
        .await;
        let Ok((user, repo_res)) = res else {
            push(checks, &name, Status::Warn, "probe panicked".into(), None);
            continue;
        };
        let (st, detail, hint) = token_api_verdict(&label, &user, &repo_res, chrono::Utc::now());
        push(checks, &name, st, detail, hint);
    }
}

fn token_api_verdict(
    key: &str,
    user: &crate::gh_token::Probe,
    repos: &[(String, u16)],
    now: chrono::DateTime<chrono::Utc>,
) -> (Status, String, Option<String>) {
    let rotate = Some(format!(
        "create a new fine-grained PAT and set {key} in ~/.config/atr/shadow.env, then restart atr"
    ));
    match user.status {
        0 => {
            return (
                Status::Warn,
                format!(
                    "{key} set; api.github.com unreachable ({})",
                    user.error.clone().unwrap_or_default()
                ),
                None,
            )
        }
        401 => {
            return (
                Status::Fail,
                format!("{key} rejected: 401 (revoked/expired)"),
                rotate,
            )
        }
        200..=299 => {}
        s => return (Status::Fail, format!("{key}: GET /user -> {s}"), rotate),
    }
    let mut detail = format!("{key} ok, login={}", user.login.as_deref().unwrap_or("?"));
    let mut status = Status::Ok;
    let mut hint = None;
    match user.expires_at {
        Some(exp) => {
            let d = crate::gh_token::days_left(exp, now);
            detail.push_str(&format!("; expires in {d}d"));
            if d < crate::gh_token::EXPIRY_WARN_DAYS {
                status = Status::Warn;
                hint = rotate.clone();
            }
        }
        None => detail.push_str("; no expiry"),
    }
    let bad: Vec<String> = repos
        .iter()
        .filter(|(_, s)| !(200..300).contains(s))
        .map(|(r, s)| format!("{r}={s}"))
        .collect();
    if !repos.is_empty() {
        if bad.is_empty() {
            detail.push_str(&format!("; repo access {}/{}", repos.len(), repos.len()));
        } else {
            detail.push_str(&format!("; no access: {}", bad.join(", ")));
            status = Status::Fail;
            hint = Some(
                "grant the PAT these repos (Contents/Issues/Pull requests RW, Metadata R)".into(),
            );
        }
    }
    (status, detail, hint)
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
    let h = crate::telegram_health::status_json(cfg, config_path, chrono::Utc::now());
    let (st, hint) = match h["level"].as_str().unwrap_or("unknown") {
        "fail" => (
            Status::Fail,
            Some("check this host's network/DNS/proxy to api.telegram.org; see docs/TROUBLESHOOTING.md#6-telegram".to_string()),
        ),
        "warn" => (Status::Warn, Some("transient network trouble to api.telegram.org; watch atr logs".to_string())),
        _ => (Status::Ok, None),
    };
    push(
        checks,
        "telegram reachability",
        st,
        h["message"].as_str().unwrap_or("").to_string(),
        hint,
    );
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
listen = "127.0.0.1:0"
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

    #[test]
    fn lessons_doctor_line() {
        let (_dir, cfg, _path) = mini_cfg("");
        let mut checks = Vec::new();
        lessons_check(&mut checks, &cfg);
        assert!(checks.is_empty(), "off by default");
        let (_dir, cfg, _path) = mini_cfg("[lessons]\nenabled = true\n");
        let mut checks = Vec::new();
        lessons_check(&mut checks, &cfg);
        assert_eq!(checks[0].status, Status::Ok, "{:?}", checks[0]);
        assert!(checks[0].detail.contains("claude"));
        let (_dir, cfg, _path) = mini_cfg("[lessons]\nenabled = true\nprovider = \"nope\"\n");
        let mut checks = Vec::new();
        lessons_check(&mut checks, &cfg);
        assert_eq!(checks[0].status, Status::Warn);
    }

    #[test]
    fn unknown_config_keys_warn_in_doctor() {
        let (_dir, cfg, _path) =
            mini_cfg("[future_table]\nenabled = true\n[telegram]\nenabeld = false\n");
        let mut checks = Vec::new();
        unknown_keys_check(&mut checks, &cfg);
        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].name, "config keys");
        assert_eq!(checks[0].status, Status::Warn);
        assert!(
            checks[0].detail.contains("`future_table`"),
            "{}",
            checks[0].detail
        );
        assert!(
            checks[0]
                .detail
                .contains("`telegram.enabeld` (did you mean `enabled`?)"),
            "{}",
            checks[0].detail
        );

        let (_dir, cfg, _path) = mini_cfg("");
        let mut checks = Vec::new();
        unknown_keys_check(&mut checks, &cfg);
        assert!(checks.is_empty());
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
listen = "127.0.0.1:0"
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

    #[test]
    fn dir_on_path_matches_entries() {
        let d = tempfile::tempdir().unwrap();
        let p = std::env::join_paths([std::path::Path::new("/usr/bin"), d.path()]).unwrap();
        assert!(dir_on_path(d.path(), &p));
        assert!(!dir_on_path(d.path(), std::ffi::OsStr::new("/usr/bin")));
    }

    #[tokio::test]
    async fn http_write_auth_check() {
        let (dir, mut cfg, path) = mini_cfg("");
        cfg.mcp.token_env = "ATR_WRITE_DOCTOR_TEST_UNSET".into();
        cfg.mcp.token_file = dir.path().join("none.token");
        let report = run(&cfg, &path, false).await;
        let c = report
            .checks
            .iter()
            .find(|c| c.name == "http write auth")
            .unwrap();
        assert_eq!(c.status, Status::Warn, "{c:?}");
        crate::mcp::write_token_file(&cfg.mcp.token_file, "doctor-write-tok").unwrap();
        let report = run(&cfg, &path, false).await;
        let c = report
            .checks
            .iter()
            .find(|c| c.name == "http write auth")
            .unwrap();
        assert_eq!(c.status, Status::Ok, "{c:?}");
        assert!(!report.format().contains("doctor-write-tok"));
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

    fn probe(status: u16, days: Option<i64>) -> crate::gh_token::Probe {
        crate::gh_token::Probe {
            status,
            login: Some("me".into()),
            expires_at: days.map(|d| {
                chrono::Utc::now() + chrono::Duration::days(d) + chrono::Duration::hours(1)
            }),
            error: None,
        }
    }

    #[test]
    fn token_api_verdicts() {
        let now = chrono::Utc::now();
        let ok = vec![("o/r".to_string(), 200)];
        assert_eq!(
            token_api_verdict("ATR_GH_TOKEN", &probe(200, Some(60)), &ok, now).0,
            Status::Ok
        );
        let (s, d, _) = token_api_verdict("ATR_GH_TOKEN", &probe(200, Some(5)), &ok, now);
        assert_eq!(s, Status::Warn);
        assert!(d.contains("expires in 5d"), "{d}");
        assert_eq!(
            token_api_verdict("ATR_GH_TOKEN", &probe(401, None), &ok, now).0,
            Status::Fail
        );
        let bad = vec![("o/r".to_string(), 404)];
        assert_eq!(
            token_api_verdict("ATR_GH_TOKEN", &probe(200, None), &bad, now).0,
            Status::Fail
        );
        assert_eq!(
            token_api_verdict("ATR_GH_TOKEN", &probe(0, None), &ok, now).0,
            Status::Warn
        );
    }

    #[test]
    fn locked_shows_service_version_or_skips() {
        let (s, d, h) = locked_version_verdict("/c", Some(("2.0".into(), "t".into())));
        assert_eq!(s, Status::Ok);
        assert!(
            d.contains("/c 2.0 (seen by service at t)") && h.is_none(),
            "{d}"
        );
        let (s, d, _) = locked_version_verdict("/c", None);
        assert_eq!(s, Status::Ok);
        assert!(d.contains("skipped over SSH"), "{d}");
        assert!(!d.contains("pin"));
    }

    #[test]
    fn recorded_version_reads_new_then_legacy_file() {
        let dir = tempfile::tempdir().unwrap();
        let cp = dir.path().join("config.toml");
        std::fs::write(
            dir.path().join("cli-pins.json"),
            r#"{"cursor":{"pin":"ok","version":"v1","checked_at":"t"}}"#,
        )
        .unwrap();
        assert_eq!(
            crate::providers::recorded_version(&cp, "cursor").unwrap().0,
            "v1"
        );
        std::fs::write(
            crate::providers::versions_path(&cp),
            r#"{"cursor":{"version":"v2","checked_at":"t2"}}"#,
        )
        .unwrap();
        assert_eq!(
            crate::providers::recorded_version(&cp, "cursor").unwrap().0,
            "v2"
        );
        assert!(crate::providers::recorded_version(&cp, "x").is_none());
        assert!(crate::providers::versions_path(&cp).ends_with("cli-versions.json"));
    }

    #[test]
    fn legacy_expected_version_is_accepted_and_flagged() {
        let cfg = crate::config::parse_config_toml(
            "[[provider]]\nname = \"claude\"\nkind = \"claude\"\ncli = \"/bin/true\"\nexpected_version = \"1.2.3\"\n",
        )
        .unwrap();
        assert!(cfg.provider("claude").unwrap().legacy_expected_version);
    }
}
