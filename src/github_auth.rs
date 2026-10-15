//! Detect GitHub login / token / SSH without ever printing secrets.
//! Used by `atr init` (guide) and `atr doctor` (checks).

use crate::telegram_setup;
use std::path::Path;
use std::process::Command;

const TOKEN_KEYS: &[&str] = &["GH_TOKEN", "GITHUB_TOKEN"];

/// Fine-grained PAT permissions atr needs for a write-capable agent.
pub const FINE_GRAINED_WRITE: &[(&str, &str)] = &[
    ("contents", "write"),
    ("issues", "write"),
    ("pull_requests", "write"),
    ("metadata", "read"),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Presence {
    Unset,
    Env,
    ShadowEnv,
    Both,
}

impl Presence {
    pub fn is_set(self) -> bool {
        !matches!(self, Self::Unset)
    }

    pub fn detail(self, key: &str) -> String {
        match self {
            Self::Unset => format!("{key} unset"),
            Self::Env => format!("{key} set"),
            Self::ShadowEnv => format!("{key} in shadow.env"),
            Self::Both => format!("{key} set"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Snapshot {
    pub gh_ok: bool,
    pub gh_detail: String,
    pub gh_hint: Option<String>,
    pub token_key: String,
    pub token: Presence,
    pub scopes: Option<String>,
    pub scopes_kind: Option<String>,
    pub missing_perms: Vec<String>,
    pub ssh_ok: bool,
    pub ssh_detail: String,
    pub shadow: Presence,
    pub shadow_key: String,
    pub shadow_env_mode: Option<u32>,
}

pub struct ProbeOpts<'a> {
    pub config_path: &'a Path,
    pub gh_cli: &'a str,
    pub shadow_token_env: &'a str,
    pub probe_ssh: bool,
    pub probe_scopes: bool,
}

/// File/env presence only — no `gh` or `ssh`. Used when GitHub is off.
pub fn probe_local(opts: ProbeOpts<'_>) -> Snapshot {
    let token_key = first_present_token_key(opts.config_path).unwrap_or("GH_TOKEN");
    let token = presence(opts.config_path, token_key);
    let (token_key, token) = if token.is_set() {
        (token_key.to_string(), token)
    } else {
        let alt = presence(opts.config_path, "GITHUB_TOKEN");
        if alt.is_set() {
            ("GITHUB_TOKEN".into(), alt)
        } else {
            ("GH_TOKEN".into(), Presence::Unset)
        }
    };
    let shadow_key = if opts.shadow_token_env.trim().is_empty() {
        "ATR_SHADOW_GH_TOKEN"
    } else {
        opts.shadow_token_env
    };
    Snapshot {
        gh_ok: false,
        gh_detail: "skipped".into(),
        gh_hint: None,
        token_key,
        token,
        scopes: None,
        scopes_kind: None,
        missing_perms: Vec::new(),
        ssh_ok: false,
        ssh_detail: "skipped".into(),
        shadow: presence(opts.config_path, shadow_key),
        shadow_key: shadow_key.to_string(),
        shadow_env_mode: env_file_mode(&telegram_setup::shadow_env_path(opts.config_path)),
    }
}

pub fn probe(opts: ProbeOpts<'_>) -> Snapshot {
    let cli = if opts.gh_cli.trim().is_empty() {
        "gh"
    } else {
        opts.gh_cli
    };
    let (gh_ok, gh_raw) = gh_auth_status(cli);
    let gh_detail = summarize_gh_status(&gh_raw, gh_ok);

    let token_key = first_present_token_key(opts.config_path).unwrap_or("GH_TOKEN");
    let token = presence(opts.config_path, token_key);
    // If GH_TOKEN is unset, still report GITHUB_TOKEN when that one is set.
    let (token_key, token) = if token.is_set() {
        (token_key.to_string(), token)
    } else {
        let alt = presence(opts.config_path, "GITHUB_TOKEN");
        if alt.is_set() {
            ("GITHUB_TOKEN".into(), alt)
        } else {
            ("GH_TOKEN".into(), Presence::Unset)
        }
    };

    let mut scopes = parse_token_scopes(&gh_raw);
    let mut scopes_kind = if scopes.is_some() {
        Some("classic".into())
    } else {
        parse_fine_grained_perms(&gh_raw).map(|s| {
            scopes = Some(s);
            "fine-grained".into()
        })
    };
    if opts.probe_scopes && (scopes.is_none() || gh_ok || token.is_set()) {
        if let Some((kind, text)) = fetch_scopes(cli, opts.config_path) {
            if scopes.is_none() {
                scopes = Some(text);
                scopes_kind = Some(kind);
            }
        }
    }
    let missing_perms = match scopes_kind.as_deref() {
        Some("fine-grained") => missing_fine_grained(scopes.as_deref().unwrap_or("")),
        Some("classic") => missing_classic(scopes.as_deref().unwrap_or("")),
        _ => Vec::new(),
    };

    let (ssh_ok, ssh_detail) = if opts.probe_ssh {
        ssh_github()
    } else {
        (false, "skipped".into())
    };

    let shadow_key = if opts.shadow_token_env.trim().is_empty() {
        "ATR_SHADOW_GH_TOKEN"
    } else {
        opts.shadow_token_env
    };
    let shadow = presence(opts.config_path, shadow_key);
    let shadow_env_mode = env_file_mode(&telegram_setup::shadow_env_path(opts.config_path));

    Snapshot {
        gh_ok,
        gh_detail: redact_secrets(&gh_detail),
        gh_hint: if gh_ok {
            None
        } else {
            Some(
                "gh auth login  (headless/LaunchAgent: GH_TOKEN in ~/.config/atr/shadow.env)"
                    .into(),
            )
        },
        token_key,
        token,
        scopes: scopes.map(|s| redact_secrets(&s)),
        scopes_kind,
        missing_perms,
        ssh_ok,
        ssh_detail: redact_secrets(&ssh_detail),
        shadow,
        shadow_key: shadow_key.to_string(),
        shadow_env_mode,
    }
}

pub fn format_init_guide(snap: &Snapshot) -> String {
    let mut out = String::from("GitHub authentication:\n");
    out.push_str(&format!(
        "  gh auth login : {}\n",
        if snap.gh_ok {
            snap.gh_detail.as_str()
        } else {
            "not logged in"
        }
    ));
    out.push_str(&format!(
        "  {}      : {}\n",
        snap.token_key,
        snap.token.detail(&snap.token_key)
    ));
    if let Some(s) = &snap.scopes {
        out.push_str(&format!(
            "  token scopes  : {} ({})\n",
            s,
            snap.scopes_kind.as_deref().unwrap_or("unknown")
        ));
    }
    out.push_str(&format!(
        "  SSH           : {}\n",
        if snap.ssh_ok {
            snap.ssh_detail.as_str()
        } else if snap.ssh_detail == "skipped" {
            "not checked"
        } else {
            snap.ssh_detail.as_str()
        }
    ));
    out.push_str(
        "  atr needs GitHub to read issues/comments, push branches, open PRs, and post comments.\n",
    );
    if snap.gh_ok || snap.token.is_set() {
        if !snap.missing_perms.is_empty() {
            out.push_str(&format!(
                "  missing permissions: {}. Fine-grained needs Contents RW, Issues RW, Pull requests RW, Metadata R.\n",
                snap.missing_perms.join(", ")
            ));
        }
        if !snap.ssh_ok {
            out.push_str(
                "  SSH is optional unless remotes are git@github.com. Test with: ssh -T git@github.com\n",
            );
        }
        if snap.token == Presence::ShadowEnv {
            out.push_str(
                "  Token is in shadow.env — restart atr serve so atr-serve.sh sources it.\n",
            );
        }
    } else {
        out.push_str(
            "  Not set up yet. Desktop: gh auth login. Headless / LaunchAgent: put GH_TOKEN in ~/.config/atr/shadow.env (mode 0600).\n",
        );
        out.push_str(
            "  Fine-grained PAT: Contents RW, Issues RW, Pull requests RW, Metadata R. Never put a token in config.toml.\n",
        );
        out.push_str("  SSH remotes: ssh-keygen, add the key on GitHub, ssh -T git@github.com (still need gh/token for API).\n");
    }
    out.push_str("  Full steps: README § GitHub authentication. Secrets are never printed.\n");
    redact_secrets(&out)
}

pub fn token_in_process_env() -> bool {
    TOKEN_KEYS.iter().any(|k| {
        std::env::var(k)
            .map(|v| !v.trim().is_empty())
            .unwrap_or(false)
    })
}

fn first_present_token_key(config_path: &Path) -> Option<&'static str> {
    TOKEN_KEYS
        .iter()
        .copied()
        .find(|k| presence(config_path, k).is_set())
}

pub fn presence(config_path: &Path, key: &str) -> Presence {
    let env = std::env::var(key)
        .map(|v| !v.trim().is_empty())
        .unwrap_or(false);
    let file =
        telegram_setup::read_shadow_env_var(&telegram_setup::shadow_env_path(config_path), key)
            .is_some();
    match (env, file) {
        (true, true) => Presence::Both,
        (true, false) => Presence::Env,
        (false, true) => Presence::ShadowEnv,
        (false, false) => Presence::Unset,
    }
}

fn env_file_mode(path: &Path) -> Option<u32> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let meta = std::fs::metadata(path).ok()?;
        Some(meta.permissions().mode() & 0o777)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        None
    }
}

fn gh_auth_status(cli: &str) -> (bool, String) {
    match Command::new(cli).args(["auth", "status"]).output() {
        Ok(out) => {
            let text = combined_output(&out);
            (out.status.success(), text)
        }
        Err(e) => (false, format!("cannot run {cli}: {e}")),
    }
}

fn combined_output(out: &std::process::Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

fn summarize_gh_status(raw: &str, ok: bool) -> String {
    let redacted = redact_secrets(raw);
    if !ok {
        if redacted.contains("cannot run") {
            return redacted
                .lines()
                .next()
                .unwrap_or("gh missing")
                .trim()
                .into();
        }
        return "gh auth status failed".into();
    }
    for line in redacted.lines() {
        let t = line.trim();
        let low = t.to_ascii_lowercase();
        if low.contains("logged in") {
            return strip_account_noise(t);
        }
    }
    "logged in".into()
}

fn strip_account_noise(line: &str) -> String {
    // Keep "Logged in to github.com account NAME (keyring)" but drop token-ish tails.
    let t = line.trim().trim_start_matches(['✓', '✔', '-', '*']).trim();
    t.to_string()
}

fn fetch_scopes(cli: &str, config_path: &Path) -> Option<(String, String)> {
    let tok = load_token_value(config_path);
    let mut cmd = Command::new(cli);
    cmd.args(["api", "-i", "/user"]);
    if let Some((_, ref value)) = tok {
        cmd.env("GH_TOKEN", value);
        cmd.env("GITHUB_TOKEN", value);
    }
    let out = cmd.output().ok()?;
    let text = combined_output(&out);
    let headers = header_block(&text);
    if let Some(s) = parse_fine_grained_perms(headers) {
        return Some(("fine-grained".into(), s));
    }
    if let Some(s) = parse_oauth_scopes(headers) {
        return Some(("classic".into(), s));
    }
    None
}

/// Read a token only to pass it to `gh`. Caller must not print the value.
fn load_token_value(config_path: &Path) -> Option<(String, String)> {
    for key in TOKEN_KEYS {
        if let Ok(v) = std::env::var(key) {
            if !v.trim().is_empty() {
                return Some(((*key).into(), v));
            }
        }
        if let Some(v) =
            telegram_setup::read_shadow_env_var(&telegram_setup::shadow_env_path(config_path), key)
        {
            return Some(((*key).into(), v));
        }
    }
    None
}

fn header_block(text: &str) -> &str {
    text.split("\r\n\r\n")
        .next()
        .and_then(|s| s.split("\n\n").next())
        .unwrap_or(text)
}

pub fn parse_oauth_scopes(text: &str) -> Option<String> {
    for line in text.lines() {
        let t = line.trim();
        let low = t.to_ascii_lowercase();
        if let Some(rest) = low
            .strip_prefix("x-oauth-scopes:")
            .or_else(|| low.strip_prefix("token scopes:"))
        {
            let raw = t.split_once(':').map(|(_, r)| r).unwrap_or(rest);
            let scopes = normalize_scope_list(raw);
            if !scopes.is_empty() {
                return Some(scopes);
            }
        }
    }
    parse_token_scopes(text)
}

pub fn parse_token_scopes(text: &str) -> Option<String> {
    for line in text.lines() {
        let t = line.trim();
        let low = t.to_ascii_lowercase();
        if low.contains("token scopes") {
            if let Some((_, rest)) = t.split_once(':') {
                let scopes = normalize_scope_list(rest);
                if !scopes.is_empty() {
                    return Some(scopes);
                }
            }
        }
    }
    None
}

pub fn parse_fine_grained_perms(text: &str) -> Option<String> {
    for line in text.lines() {
        let t = line.trim();
        if t.to_ascii_lowercase()
            .starts_with("x-accepted-github-permissions:")
        {
            let raw = t.split_once(':').map(|(_, r)| r).unwrap_or("");
            let perms = normalize_perm_list(raw);
            if !perms.is_empty() {
                return Some(perms);
            }
        }
    }
    None
}

fn normalize_scope_list(raw: &str) -> String {
    let mut out: Vec<String> = raw
        .split([',', ';', ' '])
        .map(|s| s.trim().trim_matches('\'').trim_matches('"').to_string())
        .filter(|s| !s.is_empty())
        .collect();
    out.sort();
    out.dedup();
    out.join(",")
}

fn normalize_perm_list(raw: &str) -> String {
    let mut out: Vec<String> = raw
        .split(';')
        .map(|s| {
            s.split_whitespace()
                .collect::<Vec<_>>()
                .join("")
                .to_ascii_lowercase()
        })
        .filter(|s| s.contains('='))
        .collect();
    out.sort();
    out.dedup();
    out.join(";")
}

pub fn missing_classic(scopes: &str) -> Vec<String> {
    let set: Vec<String> = scopes
        .split(',')
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty())
        .collect();
    if set.iter().any(|s| s == "repo") {
        Vec::new()
    } else {
        vec!["repo".into()]
    }
}

pub fn missing_fine_grained(perms: &str) -> Vec<String> {
    let map: std::collections::BTreeMap<String, String> = perms
        .split(';')
        .filter_map(|p| {
            let (k, v) = p.split_once('=')?;
            Some((k.trim().to_ascii_lowercase(), v.trim().to_ascii_lowercase()))
        })
        .collect();
    let mut missing = Vec::new();
    for (name, need) in FINE_GRAINED_WRITE {
        match map.get(*name).map(String::as_str) {
            Some("write") => {}
            Some("read") if *need == "read" => {}
            Some("read") => missing.push(format!("{name}=write (have read)")),
            Some(other) => missing.push(format!("{name}={need} (have {other})")),
            None => missing.push(format!("{name}={need}")),
        }
    }
    missing
}

fn ssh_github() -> (bool, String) {
    let mut cmd = Command::new("ssh");
    cmd.args([
        "-T",
        "-o",
        "BatchMode=yes",
        "-o",
        "ConnectTimeout=5",
        "-o",
        "StrictHostKeyChecking=accept-new",
        "git@github.com",
    ]);
    match cmd.output() {
        Ok(out) => {
            let text = redact_secrets(&combined_output(&out));
            let low = text.to_ascii_lowercase();
            if low.contains("successfully authenticated")
                || (low.contains("hi ") && low.contains("github"))
            {
                (true, "authenticated".into())
            } else if low.contains("permission denied") {
                (false, "not authenticated (publickey)".into())
            } else if low.contains("timed out") || low.contains("connection refused") {
                (false, "unreachable".into())
            } else {
                let line = text
                    .lines()
                    .map(str::trim)
                    .find(|l| !l.is_empty())
                    .unwrap_or("ssh -T failed");
                (false, line.to_string())
            }
        }
        Err(e) => (false, format!("cannot run ssh: {e}")),
    }
}

/// Replace token-shaped strings so doctor/init never print secrets.
pub fn redact_secrets(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < text.len() {
        if let Some(len) = token_len_at(text, i) {
            out.push_str("***");
            i += len;
            continue;
        }
        let ch = text[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

fn token_len_at(text: &str, i: usize) -> Option<usize> {
    if !text.is_char_boundary(i) {
        return None;
    }
    const PREFIXES: &[&str] = &[
        "github_pat_",
        "gho_",
        "ghu_",
        "ghs_",
        "ghr_",
        "ghp_",
        "ghy_",
    ];
    let rest = text.as_bytes().get(i..)?;
    for p in PREFIXES {
        let pb = p.as_bytes();
        if rest
            .get(..pb.len())
            .is_some_and(|b| b.eq_ignore_ascii_case(pb))
        {
            let mut n = pb.len();
            while n < rest.len() && (rest[n].is_ascii_alphanumeric() || rest[n] == b'_') {
                n += 1;
            }
            if n > pb.len() + 8 {
                return Some(n);
            }
        }
    }
    None
}

pub fn looks_like_secret(text: &str) -> bool {
    redact_secrets(text) != text
}

pub fn probe_opts_from_config<'a>(
    cfg: &'a crate::config::Config,
    config_path: &'a Path,
) -> ProbeOpts<'a> {
    ProbeOpts {
        config_path,
        gh_cli: if cfg.github.cli.trim().is_empty() {
            "gh"
        } else {
            cfg.github.cli.as_str()
        },
        shadow_token_env: cfg.shadow.gh_token_env.as_str(),
        probe_ssh: cfg.intake_allows(crate::task::SourceKind::Github),
        probe_scopes: cfg.intake_allows(crate::task::SourceKind::Github) || cfg.shadow.enabled,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redact_secrets_strips_pats_and_classic() {
        let s = redact_secrets(
            "token ghp_abcdefghijklmnopqrstuvwxyz0123456789 and github_pat_11AAAA_bbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        );
        assert!(!s.contains("ghp_"), "{s}");
        assert!(!s.contains("github_pat_11"), "{s}");
        assert!(!s.contains("abcdefghijklmnopqrstuvwxyz"), "{s}");
        assert!(s.contains("***"), "{s}");
        assert!(!looks_like_secret(
            "Logged in to github.com account rimusz (keyring)"
        ));
        assert!(looks_like_secret("ghs_abcdefghijklmnopqrstuvwxyz012345"));
        let utf = redact_secrets(
            "README § GitHub — ✓ Logged in ghp_abcdefghijklmnopqrstuvwxyz0123456789",
        );
        assert!(
            utf.contains('§') && utf.contains('—') && utf.contains('✓'),
            "{utf}"
        );
        assert!(utf.contains("***"), "{utf}");
        assert!(!utf.contains("ghp_"), "{utf}");
    }

    #[test]
    fn parse_classic_scopes_from_status_and_headers() {
        let status = "github.com\n  ✓ Logged in to github.com account rimusz (keyring)\n  - Token scopes: 'gist', 'read:org', 'repo', 'workflow'\n";
        assert_eq!(
            parse_token_scopes(status).as_deref(),
            Some("gist,read:org,repo,workflow")
        );
        let headers = "HTTP/2.0 200 OK\nX-OAuth-Scopes: repo, workflow\n\n{\"login\":\"x\"}\n";
        assert_eq!(
            parse_oauth_scopes(headers).as_deref(),
            Some("repo,workflow")
        );
        assert!(missing_classic("repo,workflow").is_empty());
        assert_eq!(missing_classic("gist"), vec!["repo".to_string()]);
    }

    #[test]
    fn parse_fine_grained_permissions() {
        let headers = "HTTP/2.0 200 OK\nX-Accepted-GitHub-Permissions: contents=write; issues=write; metadata=read; pull_requests=write\n\n{}\n";
        let perms = parse_fine_grained_perms(headers).expect("perms");
        assert!(perms.contains("contents=write"), "{perms}");
        assert!(perms.contains("pull_requests=write"), "{perms}");
        assert!(missing_fine_grained(&perms).is_empty(), "{perms}");
        let read_only = "contents=read;issues=read;metadata=read;pull_requests=read";
        let miss = missing_fine_grained(read_only);
        assert!(
            miss.iter().any(|m| m.contains("contents=write")),
            "{miss:?}"
        );
    }

    #[test]
    fn format_init_guide_never_contains_secret() {
        let snap = Snapshot {
            gh_ok: false,
            gh_detail: "gh auth status failed".into(),
            gh_hint: None,
            token_key: "GH_TOKEN".into(),
            token: Presence::Unset,
            scopes: None,
            scopes_kind: None,
            missing_perms: Vec::new(),
            ssh_ok: false,
            ssh_detail: "not authenticated (publickey)".into(),
            shadow: Presence::Unset,
            shadow_key: "ATR_SHADOW_GH_TOKEN".into(),
            shadow_env_mode: None,
        };
        let text = format_init_guide(&snap);
        assert!(text.contains("GitHub authentication"), "{text}");
        assert!(text.contains("read issues"), "{text}");
        assert!(text.contains("gh auth login"), "{text}");
        assert!(text.contains("shadow.env"), "{text}");
        assert!(text.contains("Contents RW"), "{text}");
        assert!(!looks_like_secret(&text), "{text}");
    }

    #[test]
    fn presence_reads_shadow_env_without_exposing_value() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.toml");
        std::fs::write(&cfg, "listen = \"127.0.0.1:1\"\n").unwrap();
        let secret = "ghp_TESTSECRETVALUE_must_not_appear_zzzzzzzz";
        std::fs::write(
            dir.path().join("shadow.env"),
            format!("GH_TOKEN={secret}\n"),
        )
        .unwrap();
        let p = presence(&cfg, "GH_TOKEN");
        assert_eq!(p, Presence::ShadowEnv);
        assert_eq!(p.detail("GH_TOKEN"), "GH_TOKEN in shadow.env");
        let snap = Snapshot {
            gh_ok: false,
            gh_detail: "gh auth status failed".into(),
            gh_hint: None,
            token_key: "GH_TOKEN".into(),
            token: p,
            scopes: None,
            scopes_kind: None,
            missing_perms: Vec::new(),
            ssh_ok: false,
            ssh_detail: "not authenticated (publickey)".into(),
            shadow: Presence::Unset,
            shadow_key: "ATR_SHADOW_GH_TOKEN".into(),
            shadow_env_mode: Some(0o600),
        };
        let guide = format_init_guide(&snap);
        assert!(!guide.contains(secret), "{guide}");
        assert!(!guide.contains("TESTSECRET"), "{guide}");
        assert!(guide.contains("shadow.env"), "{guide}");
        assert!(!looks_like_secret(&guide), "{guide}");
    }
}
