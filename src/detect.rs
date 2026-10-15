//! Discover installed agent CLIs (PATH + common dirs), cheap signed-in probes,
//! and OpenCode models. Shared by `atr init` and `atr providers detect`.

use crate::config::{Config, Provider};
use crate::presets::{self, BuiltinPreset};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;

const SUBSCRIPTION_ORDER: &[&str] = &["claude", "grok", "cursor", "codex"];

const HOME_BIN_DIRS: &[&str] = &[
    ".local/bin",
    "bin",
    ".claude/local",
    ".cursor/bin",
    ".opencode/bin",
    ".codex/bin",
    ".npm-global/bin",
    ".kimi/bin",
    ".qwen/bin",
    ".zcode/bin",
    ".mimo/bin",
    ".minimax/bin",
];

const SYSTEM_BIN_DIRS: &[&str] = &["/usr/local/bin", "/opt/homebrew/bin", "/opt/local/bin"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Auth {
    Yes,
    No,
    Unknown,
    KeychainLocked,
}

impl Auth {
    pub fn signed_in(self) -> bool {
        matches!(self, Self::Yes)
    }
}

#[derive(Debug, Clone, Copy)]
pub struct CliSpec {
    pub id: &'static str,
    /// atr `kind` when this CLI can be a provider. `None` = vendor-only.
    pub kind: Option<&'static str>,
    pub binaries: &'static [&'static str],
    pub version_args: &'static [&'static str],
    pub auth_args: &'static [&'static str],
    pub login: &'static str,
    pub vendor_note: Option<&'static str>,
}

pub const SPECS: &[CliSpec] = &[
    CliSpec {
        id: "claude",
        kind: Some("claude"),
        binaries: &["claude"],
        version_args: &["--version"],
        auth_args: &["auth", "status"],
        login: "claude",
        vendor_note: None,
    },
    CliSpec {
        id: "grok",
        kind: Some("grok"),
        binaries: &["grok"],
        version_args: &["--version"],
        // `grok auth status` does not exist (exit 2). Probe the auth file
        // or billing stdio instead. Never call `grok login`.
        auth_args: &[],
        login: "grok",
        vendor_note: None,
    },
    CliSpec {
        id: "cursor",
        kind: Some("cursor"),
        binaries: &["cursor-agent"],
        version_args: &["--version"],
        // `cursor-agent status` (alias whoami). There is no `auth status`
        // subcommand: those words are taken as a prompt and start a paid
        // agent run (~40 s) during init/doctor.
        auth_args: &["status"],
        login: "sign in to the Cursor app",
        vendor_note: None,
    },
    CliSpec {
        id: "codex",
        kind: Some("codex"),
        binaries: &["codex"],
        version_args: &["--version"],
        auth_args: &["login", "status"],
        login: "codex login",
        vendor_note: None,
    },
    CliSpec {
        id: "opencode",
        kind: Some("opencode"),
        binaries: &["opencode"],
        version_args: &["--version"],
        auth_args: &["auth", "list"],
        login: "opencode auth login",
        vendor_note: None,
    },
    CliSpec {
        id: "kimi",
        kind: None,
        binaries: &["kimi", "kimi-code"],
        version_args: &["--version"],
        auth_args: &["auth", "status"],
        login: "kimi",
        vendor_note: Some(
            "interactive only — atr has no kind; use OpenCode: atr providers add kimi",
        ),
    },
    CliSpec {
        id: "qwen",
        kind: None,
        binaries: &["qwen", "qwen-code"],
        version_args: &["--version"],
        auth_args: &["auth", "status"],
        login: "qwen",
        vendor_note: Some(
            "interactive only — atr has no kind; use OpenCode: atr providers add qwen",
        ),
    },
    CliSpec {
        id: "zcode",
        kind: None,
        binaries: &["zcode", "z-code"],
        version_args: &["--version"],
        auth_args: &["auth", "status"],
        login: "zcode",
        vendor_note: Some(
            "interactive only — atr has no kind; use OpenCode: atr providers add glm",
        ),
    },
    CliSpec {
        id: "deepseek",
        kind: None,
        binaries: &["deepseek", "dsh"],
        version_args: &["--version"],
        auth_args: &["auth", "status"],
        login: "deepseek",
        vendor_note: Some(
            "interactive only — atr has no kind; use OpenCode: atr providers add deepseek",
        ),
    },
    CliSpec {
        id: "minimax",
        kind: None,
        binaries: &["minimax", "minimax-code"],
        version_args: &["--version"],
        auth_args: &["auth", "status"],
        login: "minimax",
        vendor_note: Some(
            "interactive only — atr has no kind; use OpenCode: atr providers add minimax",
        ),
    },
    CliSpec {
        id: "mimo",
        kind: None,
        binaries: &["mimo", "mimo-code"],
        version_args: &["--version"],
        auth_args: &["auth", "status"],
        login: "mimo",
        vendor_note: Some(
            "interactive only — atr has no kind; use OpenCode: atr providers add mimo",
        ),
    },
];

#[derive(Debug, Clone)]
pub struct DetectEnv {
    pub path: OsString,
    pub home: PathBuf,
    pub search_system: bool,
}

impl DetectEnv {
    pub fn from_process() -> Self {
        Self {
            path: std::env::var_os("PATH").unwrap_or_default(),
            home: crate::config::expand_path("~"),
            search_system: true,
        }
    }
}

#[cfg(test)]
thread_local! {
    static TEST_ENV: std::cell::RefCell<Option<DetectEnv>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub fn set_test_env(env: Option<DetectEnv>) {
    TEST_ENV.with(|c| *c.borrow_mut() = env);
}

pub fn current_env() -> DetectEnv {
    #[cfg(test)]
    {
        if let Some(e) = TEST_ENV.with(|c| c.borrow().clone()) {
            return e;
        }
    }
    DetectEnv::from_process()
}

#[derive(Debug, Clone)]
pub struct FoundCli {
    pub spec_id: String,
    pub kind: Option<String>,
    pub binary: String,
    pub path: PathBuf,
    pub version: Option<String>,
    pub auth: Auth,
    pub login: String,
    pub vendor_note: Option<String>,
}

#[derive(Debug, Clone)]
pub struct OpencodeInfo {
    pub providers: Vec<String>,
    pub models: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct Discovery {
    pub found: Vec<FoundCli>,
    pub missing: Vec<String>,
    pub opencode: Option<OpencodeInfo>,
}

#[derive(Debug, Clone)]
pub struct ProviderPick {
    pub name: String,
    pub kind: String,
    pub enabled: bool,
    pub cli: Option<PathBuf>,
    pub version: Option<String>,
    pub preset: Option<String>,
    pub model: Option<String>,
}

pub fn discover(env: &DetectEnv) -> Discovery {
    let mut found = Vec::new();
    let mut missing = Vec::new();
    for spec in SPECS {
        if let Some(hit) = find_spec(spec, env) {
            found.push(hit);
        } else {
            missing.push(spec.id.to_string());
        }
    }
    let opencode = found
        .iter()
        .find(|f| f.spec_id == "opencode")
        .map(|f| probe_opencode(&f.path));
    Discovery {
        found,
        missing,
        opencode,
    }
}

fn find_spec(spec: &CliSpec, env: &DetectEnv) -> Option<FoundCli> {
    for name in spec.binaries {
        if let Some(path) = find_binary(name, env) {
            let version = probe_version(&path, spec.version_args);
            let auth = if spec.id == "grok" {
                probe_grok_auth(&path, env)
            } else {
                let mut auth = probe_auth(&path, spec.auth_args);
                if auth == Auth::Unknown {
                    auth = file_auth(spec.id, env);
                }
                auth
            };
            return Some(FoundCli {
                spec_id: spec.id.into(),
                kind: spec.kind.map(str::to_string),
                binary: (*name).into(),
                path,
                version,
                auth,
                login: spec.login.into(),
                vendor_note: spec.vendor_note.map(str::to_string),
            });
        }
    }
    None
}

pub fn find_binary(name: &str, env: &DetectEnv) -> Option<PathBuf> {
    if name.is_empty() {
        return None;
    }
    let p = Path::new(name);
    if p.is_absolute() && is_cli(p) {
        return Some(p.to_path_buf());
    }
    let mut dirs: Vec<PathBuf> = std::env::split_paths(&env.path).collect();
    for rel in HOME_BIN_DIRS {
        dirs.push(env.home.join(rel));
    }
    if env.search_system {
        for d in SYSTEM_BIN_DIRS {
            dirs.push(PathBuf::from(d));
        }
    }
    let mut seen = std::collections::BTreeSet::new();
    for dir in dirs {
        let cand = dir.join(name);
        let key = cand.to_string_lossy().to_string();
        if !seen.insert(key) {
            continue;
        }
        if is_cli(&cand) {
            return Some(cand);
        }
    }
    None
}

fn is_cli(p: &Path) -> bool {
    if !p.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        p.metadata()
            .map(|m| m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        true
    }
}

fn probe_version(cli: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new(cli).args(args).output().ok()?;
    let text = combined(&out);
    first_version_token(&text)
}

/// Auth file first, then `_x.ai/billing` over `grok agent stdio`.
/// Never runs `grok auth status` or `grok login`.
fn probe_grok_auth(cli: &Path, env: &DetectEnv) -> Auth {
    if file_auth("grok", env) == Auth::Yes {
        return Auth::Yes;
    }
    match probe_grok_billing_sync(cli) {
        Auth::Yes => Auth::Yes,
        Auth::KeychainLocked => Auth::KeychainLocked,
        Auth::No | Auth::Unknown => Auth::No,
    }
}

fn probe_grok_billing_sync(cli: &Path) -> Auth {
    let mut child = match Command::new(cli)
        .args(["agent", "stdio"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(_) => return Auth::Unknown,
    };
    if let Some(mut stdin) = child.stdin.take() {
        use std::io::Write;
        let _ = stdin.write_all(
            br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{}}}
{"jsonrpc":"2.0","id":2,"method":"_x.ai/billing","params":{}}
"#,
        );
    }
    let pid = child.id();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_secs(5));
        let _ = Command::new("kill").arg("-9").arg(pid.to_string()).status();
    });
    let started = std::time::Instant::now();
    let mut stdout = match child.stdout.take() {
        Some(s) => s,
        None => {
            let _ = child.kill();
            return Auth::Unknown;
        }
    };
    use std::io::{BufRead, BufReader};
    let mut lines = BufReader::new(&mut stdout).lines();
    while started.elapsed() < std::time::Duration::from_secs(5) {
        let Some(Ok(line)) = lines.next() else {
            break;
        };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else {
            let low = line.to_ascii_lowercase();
            if crate::providers::looks_like_keychain_lock(&line) {
                let _ = child.kill();
                return Auth::KeychainLocked;
            }
            if low.contains("not logged") || low.contains("unauthorized") {
                let _ = child.kill();
                return Auth::No;
            }
            continue;
        };
        if v.get("id") == Some(&serde_json::Value::from(2)) {
            let _ = child.kill();
            if v.get("error").is_some() {
                return Auth::No;
            }
            return Auth::Yes;
        }
    }
    let _ = child.kill();
    Auth::Unknown
}

fn probe_auth(cli: &Path, args: &[&str]) -> Auth {
    if args.is_empty() {
        return Auth::Unknown;
    }
    match Command::new(cli).args(args).output() {
        Ok(out) => parse_auth_output(&combined(&out), out.status.success()),
        Err(_) => Auth::Unknown,
    }
}

fn combined(out: &std::process::Output) -> String {
    strip_ansi(&format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    ))
}

pub fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' && chars.peek() == Some(&'[') {
            chars.next();
            for c in chars.by_ref() {
                if ('@'..='~').contains(&c) {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

pub fn parse_auth_output(text: &str, success: bool) -> Auth {
    let text = strip_ansi(text);
    let low = text.to_ascii_lowercase();
    if looks_like_unknown_subcommand(&low) {
        return Auth::Unknown;
    }
    if crate::providers::looks_like_keychain_lock(&text) {
        return Auth::KeychainLocked;
    }
    if low.contains("not logged")
        || low.contains("not authenticated")
        || low.contains("not signed")
        || low.contains("no provider")
        || low.contains("please log in")
        || low.contains("please login")
        || (low.contains("run `") && low.contains("login"))
        || (low.contains("unauthorized") && !low.contains("logged in"))
    {
        return Auth::No;
    }
    if low.contains("logged in")
        || low.contains("signed in")
        || low.contains("authenticated")
        || low.contains("active account")
    {
        return Auth::Yes;
    }
    if success && !text.trim().is_empty() {
        return Auth::Yes;
    }
    if success {
        return Auth::Unknown;
    }
    Auth::No
}

fn looks_like_unknown_subcommand(low: &str) -> bool {
    (low.contains("unknown") || low.contains("unrecognized") || low.contains("invalid"))
        && (low.contains("command") || low.contains("argument") || low.contains("usage"))
        || low.contains("unexpected argument")
}

fn file_auth(id: &str, env: &DetectEnv) -> Auth {
    let home = &env.home;
    let paths: &[&str] = match id {
        "claude" => &[".claude.json", ".claude/.credentials.json"],
        "grok" => &[".grok/auth.json", ".config/grok/credentials.json"],
        "cursor" => &[".cursor/cli-config.json", ".cursor/argv.json"],
        "codex" => &[".codex/auth.json"],
        "opencode" => &[".local/share/opencode/auth.json"],
        _ => &[],
    };
    for rel in paths {
        let p = home.join(rel);
        if p.is_file() {
            if let Ok(meta) = p.metadata() {
                if meta.len() > 2 {
                    return Auth::Yes;
                }
            }
        }
    }
    Auth::Unknown
}

fn probe_opencode(cli: &Path) -> OpencodeInfo {
    let providers = match Command::new(cli).args(["auth", "list"]).output() {
        Ok(out) => parse_auth_providers(&combined(&out)),
        Err(_) => Vec::new(),
    };
    let mut models = match Command::new(cli).args(["models"]).output() {
        Ok(out) => parse_models(&combined(&out)),
        Err(_) => Vec::new(),
    };
    for bp in presets::BUILTINS.iter().filter(|b| b.needs_url) {
        if crate::opencode_cfg::provider_base_url(
            &crate::opencode_cfg::config_path(),
            bp.oc_provider,
        )
        .is_some()
            && !models
                .iter()
                .any(|m| m.starts_with(&format!("{}/", bp.oc_provider)))
        {
            models.push(bp.model.to_string());
        }
    }
    OpencodeInfo { providers, models }
}

const AUTH_PROVIDER_STOP: &[&str] = &[
    "logged",
    "in",
    "no",
    "providers",
    "provider",
    "list",
    "auth",
    "login",
    "unknown",
    "command",
    "error",
    "usage",
    "help",
    "use",
    "at",
    "to",
    "the",
    "a",
    "an",
    "and",
    "or",
    "for",
    "with",
    "from",
    "again",
    "add",
    "run",
    "please",
    "need",
    "credentials",
    "configured",
    "none",
    "signed",
    "log",
];

pub fn parse_auth_providers(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in text.lines() {
        let t = line
            .trim()
            .trim_start_matches(['✓', '✔', '•', '-', '*'])
            .trim();
        if t.is_empty() {
            continue;
        }
        let low = t.to_ascii_lowercase();
        if low.contains("unknown command")
            || low.contains("unrecognized")
            || low.contains("log in again")
            || low.contains("usage:")
            || t.contains('`')
        {
            continue;
        }
        if t.contains(' ') && !t.contains('/') {
            // "Logged in to moonshotai" — take last token if it looks like an id
            if let Some(last) = t.split_whitespace().last() {
                if is_provider_id(last) {
                    push_unique_provider(&mut out, last);
                }
            }
            continue;
        }
        let id = t.split_whitespace().next().unwrap_or(t);
        if is_provider_id(id) {
            push_unique_provider(&mut out, id);
        }
    }
    out
}

fn push_unique_provider(out: &mut Vec<String>, s: &str) {
    let s = s.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '-' && c != '_');
    push_unique(out, s);
}

fn is_provider_id(s: &str) -> bool {
    let s = s.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '-' && c != '_');
    if s.len() < 2 || s.chars().all(|c| c.is_ascii_digit()) {
        return false;
    }
    s.chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
        && !AUTH_PROVIDER_STOP.contains(&s)
}

pub fn parse_models(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in text.lines() {
        let t = line.trim();
        if t.is_empty() || t.starts_with('#') {
            continue;
        }
        let tok = t.split_whitespace().next().unwrap_or("");
        if tok.contains('/')
            && tok
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '/' || c == '-' || c == '_' || c == '.')
        {
            push_unique(&mut out, tok);
        }
    }
    out
}

fn push_unique(out: &mut Vec<String>, s: &str) {
    let s = s
        .trim_matches(|c: char| {
            !c.is_ascii_alphanumeric() && c != '-' && c != '_' && c != '/' && c != '.'
        })
        .to_string();
    if !s.is_empty() && !out.iter().any(|x| x == &s) {
        out.push(s);
    }
}

pub fn first_version_token(text: &str) -> Option<String> {
    let text = strip_ansi(text);
    for word in text.split_whitespace() {
        let t = word.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '.' && c != '-');
        if t.is_empty() {
            continue;
        }
        // Require a leading digit so "locked.\x1b[0m" is never a version.
        if !t.starts_with(|c: char| c.is_ascii_digit()) {
            continue;
        }
        if t.contains('.') && t.chars().any(|c| c.is_ascii_digit()) {
            return Some(t.to_string());
        }
    }
    None
}

/// Minimum Grok Build for planner JSON, plugin stdio, and `grok usage`.
pub const GROK_MIN_VERSION: (u32, u32, u32) = (1, 0, 52);

pub fn parse_semver(text: &str) -> Option<(u32, u32, u32)> {
    let token = first_version_token(text).unwrap_or_else(|| text.trim().to_string());
    let mut parts = token.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    let patch = parts
        .next()
        .and_then(|s| {
            s.chars()
                .take_while(|c| c.is_ascii_digit())
                .collect::<String>()
                .parse()
                .ok()
        })
        .unwrap_or(0);
    Some((major, minor, patch))
}

pub fn grok_meets_min(version: &str) -> bool {
    match parse_semver(version) {
        Some(v) => v >= GROK_MIN_VERSION,
        None => false,
    }
}

fn toml_model_line(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    for line in text.lines() {
        let t = line.trim();
        if t.is_empty() || t.starts_with('#') || t.starts_with('[') {
            continue;
        }
        let rest = t.strip_prefix("model")?.trim_start().strip_prefix('=')?;
        let v = rest
            .trim()
            .trim_matches(|c| c == '"' || c == '\'')
            .split_whitespace()
            .next()
            .unwrap_or("")
            .trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '.' && c != '-' && c != '_');
        if !v.is_empty() {
            return Some(v.to_string());
        }
    }
    None
}

fn json_model_field(path: &Path) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    v.get("model")
        .and_then(|x| x.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// CLI default model from the vendor config file (codex `~/.codex/config.toml`, …).
pub fn cli_default_model(kind: &str) -> Option<String> {
    cli_default_model_in(kind, &current_env().home)
}

pub fn cli_default_model_in(kind: &str, home: &Path) -> Option<String> {
    match kind {
        "codex" => toml_model_line(&home.join(".codex/config.toml")),
        "claude" => json_model_field(&home.join(".claude/settings.json"))
            .or_else(|| json_model_field(&home.join(".claude.json"))),
        "grok" => toml_model_line(&home.join(".grok/config.toml"))
            .or_else(|| toml_model_line(&home.join(".config/grok/config.toml"))),
        "cursor" => json_model_field(&home.join(".cursor/cli-config.json")),
        _ => None,
    }
}

fn same_cli_path(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(x), Ok(y)) => x == y,
        _ => false,
    }
}

fn provider_matches_found(p: &Provider, f: &FoundCli) -> bool {
    f.kind.as_deref() == Some(p.kind.as_str())
        || p.name == f.spec_id
        || p.cli.file_name() == f.path.file_name()
}

fn path_mismatch_warning(cfg: &Config, f: &FoundCli) -> Option<String> {
    let mut msgs = Vec::new();
    for p in &cfg.providers {
        if !provider_matches_found(p, f) {
            continue;
        }
        if p.cli.as_os_str().is_empty() {
            continue;
        }
        if !same_cli_path(&p.cli, &f.path) {
            msgs.push(format!(
                "             warning: config uses {} (detected {})\n",
                p.cli.display(),
                f.path.display()
            ));
        }
    }
    if msgs.is_empty() {
        None
    } else {
        Some(msgs.concat())
    }
}

pub fn format_report(d: &Discovery, cfg: Option<&Config>) -> String {
    let mut out = String::from("detected CLIs:\n");
    if d.found.is_empty() {
        out.push_str("  (none on PATH or common install dirs)\n");
    }
    for f in &d.found {
        let ver = f.version.as_deref().unwrap_or("-");
        let auth = match f.auth {
            Auth::Yes => "signed in",
            Auth::No => "not signed in",
            Auth::Unknown => "auth unknown",
            Auth::KeychainLocked => "unverified (keychain locked)",
        };
        out.push_str(&format!(
            "  {:<10} {}  {ver}  {auth}\n",
            f.spec_id,
            f.path.display()
        ));
        if matches!(f.auth, Auth::No | Auth::Unknown) {
            out.push_str(&format!("             login: {}\n", f.login));
        }
        if let Some(note) = &f.vendor_note {
            out.push_str(&format!("             {note}\n"));
        }
        if let Some(cfg) = cfg {
            if let Some(w) = path_mismatch_warning(cfg, f) {
                out.push_str(&w);
            }
        }
    }
    if let Some(oc) = &d.opencode {
        if oc.providers.is_empty() {
            out.push_str("  opencode providers: (none signed in)\n");
        } else {
            out.push_str(&format!(
                "  opencode providers: {}\n",
                oc.providers.join(", ")
            ));
        }
        if oc.models.is_empty() {
            out.push_str("  opencode models: (none — run opencode models)\n");
        } else {
            out.push_str(&format!("  opencode models: {}\n", oc.models.join(", ")));
        }
    }
    if !d.missing.is_empty() {
        out.push_str(&format!("  not found: {}\n", d.missing.join(", ")));
    }
    out
}

pub fn default_subscription_picks(d: &Discovery) -> Vec<ProviderPick> {
    SUBSCRIPTION_ORDER
        .iter()
        .map(|kind| {
            let found = d.found.iter().find(|f| f.kind.as_deref() == Some(*kind));
            let signed = found.map(|f| f.auth.signed_in()).unwrap_or(false);
            ProviderPick {
                name: if *kind == "cursor" {
                    "cursor".into()
                } else {
                    (*kind).into()
                },
                kind: (*kind).into(),
                enabled: signed,
                cli: found.map(|f| f.path.clone()),
                version: found.and_then(|f| f.version.clone()),
                preset: None,
                model: None,
            }
        })
        .collect()
}

/// OpenCode presets/models that look signed-in or locally configured.
pub fn default_opencode_picks(d: &Discovery) -> Vec<ProviderPick> {
    let Some(oc) = &d.opencode else {
        return Vec::new();
    };
    let mut picks = Vec::new();
    for bp in presets::BUILTINS {
        if opencode_preset_ready(bp, oc) {
            picks.push(opencode_pick(bp, None));
        }
    }
    picks
}

fn opencode_preset_ready(bp: &BuiltinPreset, oc: &OpencodeInfo) -> bool {
    let id = bp.oc_provider;
    oc.providers.iter().any(|p| p == id || p == bp.name)
        || oc.models.iter().any(|m| m.starts_with(&format!("{id}/")))
}

fn opencode_pick(bp: &BuiltinPreset, model: Option<&str>) -> ProviderPick {
    ProviderPick {
        name: bp.name.into(),
        kind: "opencode".into(),
        enabled: true,
        cli: None,
        version: None,
        preset: Some(bp.name.into()),
        model: model.map(str::to_string),
    }
}

pub fn pick_from_token(token: &str) -> Option<ProviderPick> {
    let t = token.trim();
    if t.is_empty() {
        return None;
    }
    if let Some(bp) = presets::builtin(t) {
        return Some(opencode_pick(bp, None));
    }
    if t.contains('/') {
        if let Some(bp) = preset_for_model(t) {
            return Some(opencode_pick(bp, Some(t)));
        }
        let name = presets::model_leaf(t)
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' {
                    c
                } else {
                    '-'
                }
            })
            .collect::<String>()
            .trim_matches('-')
            .to_string();
        let name = if name.is_empty() {
            "opencode".into()
        } else {
            name
        };
        return Some(ProviderPick {
            name,
            kind: "opencode".into(),
            enabled: true,
            cli: None,
            version: None,
            preset: None,
            model: Some(t.to_string()),
        });
    }
    None
}

fn preset_for_model(model: &str) -> Option<&'static BuiltinPreset> {
    let provider = model.split('/').next()?;
    presets::BUILTINS
        .iter()
        .find(|b| b.oc_provider == provider || b.name == provider)
}

pub fn default_order_csv(picks: &[ProviderPick]) -> String {
    let mut names: Vec<&str> = picks
        .iter()
        .filter(|p| p.enabled)
        .map(|p| p.name.as_str())
        .collect();
    for p in picks {
        if !p.enabled && !names.contains(&p.name.as_str()) {
            names.push(p.name.as_str());
        }
    }
    names.join(",")
}

pub fn reorder_picks(picks: Vec<ProviderPick>, order: &str) -> Vec<ProviderPick> {
    if order.trim().is_empty() {
        return picks;
    }
    let mut next = Vec::new();
    for name in order.split(',') {
        let n = name.trim();
        if n.is_empty() {
            continue;
        }
        if let Some(p) = picks.iter().find(|p| p.name == n) {
            next.push(p.clone());
        }
    }
    for p in picks {
        if !next.iter().any(|n| n.name == p.name) {
            next.push(p);
        }
    }
    next
}

/// `--yes`: subscriptions that are signed in, then signed-in/configured OpenCode.
/// Always includes the four subscription kinds (disabled when not signed in)
/// so a fresh config still has the usual blocks.
pub fn yes_picks(d: &Discovery) -> Vec<ProviderPick> {
    let mut picks = default_subscription_picks(d);
    picks.extend(default_opencode_picks(d));
    picks
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn write_cli(dir: &Path, name: &str, body: &str) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, body).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p
    }

    fn fake_script(signed: bool) -> String {
        let flag = if signed { "1" } else { "0" };
        format!(
            r#"#!/bin/sh
name=$(basename "$0")
if [ "$1" = "--version" ]; then echo "$name 1.2.3"; exit 0; fi
if [ "$1" = "models" ]; then
  echo "moonshotai/kimi-k2.6"
  echo "ollama/llama3.2"
  exit 0
fi
if [ "$1" = "login" ]; then
  if [ "{flag}" = "1" ]; then echo "logged in"; exit 0; fi
  echo "not logged in"; exit 1
fi
if [ "$1" = "auth" ]; then
  if [ "$2" = "list" ]; then
    if [ "{flag}" = "1" ]; then echo "moonshotai"; echo "ollama"; exit 0; fi
    echo "no providers logged in"; exit 0
  fi
  if [ "{flag}" = "1" ]; then echo "logged in as testdev"; exit 0; fi
  echo "not logged in"; exit 1
fi
exit 0
"#
        )
    }

    #[test]
    fn grok_never_calls_auth_status_or_login() {
        let spec = SPECS.iter().find(|s| s.id == "grok").unwrap();
        assert!(spec.auth_args.is_empty());
        assert_eq!(spec.login, "grok");
        assert!(!spec.login.contains("login"));
        assert!(grok_meets_min("1.0.52"));
        assert!(grok_meets_min("grok 1.1.0"));
        assert!(!grok_meets_min("1.0.51"));
    }

    #[test]
    fn first_version_token_reads_semver() {
        assert_eq!(
            first_version_token("claude 2.1.287 (built)"),
            Some("2.1.287".into())
        );
        assert_eq!(
            first_version_token("cursor-agent 2026.10.01"),
            Some("2026.10.01".into())
        );
        assert_eq!(first_version_token("locked.\x1b[0m"), None);
        assert_eq!(
            first_version_token("cursor-agent: keychain is locked.\x1b[0m"),
            None
        );
    }

    #[test]
    fn parse_auth_and_models() {
        assert_eq!(parse_auth_output("not logged in", false), Auth::No);
        assert_eq!(
            parse_auth_output("✓ Logged in to github.com", true),
            Auth::Yes
        );
        assert_eq!(
            parse_auth_output("error: unknown command auth", false),
            Auth::Unknown
        );
        assert_eq!(
            parse_auth_output("Unable to access the keychain (locked).", false),
            Auth::KeychainLocked
        );
        assert_eq!(
            parse_auth_output("User interaction is not allowed.", false),
            Auth::KeychainLocked
        );
        assert_eq!(
            parse_auth_providers("✓ moonshotai\n  ollama\nLogged in to zai\n"),
            vec!["moonshotai", "ollama", "zai"]
        );
        let garbled = parse_auth_providers(
            "1 provider\nLog in again.\nat https://opencode.ai\nUse `opencode auth login` to add\nunknown command\n",
        );
        assert!(
            garbled.is_empty(),
            "help/TUI noise must not become providers: {garbled:?}"
        );
        assert_eq!(
            parse_models("moonshotai/kimi-k2.6\nollama/llama3.2 (local)\n# skip\n"),
            vec!["moonshotai/kimi-k2.6", "ollama/llama3.2"]
        );
    }

    #[test]
    fn finds_cli_in_home_bin_not_on_path() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let bin = home.join(".local/bin");
        std::fs::create_dir_all(&bin).unwrap();
        write_cli(&bin, "claude", &fake_script(true));
        let env = DetectEnv {
            path: "/nonexistent".into(),
            home,
            search_system: false,
        };
        let d = discover(&env);
        let c = d
            .found
            .iter()
            .find(|f| f.spec_id == "claude")
            .expect("claude");
        assert_eq!(c.version.as_deref(), Some("1.2.3"));
        assert_eq!(c.auth, Auth::Yes);
        assert!(d.missing.contains(&"grok".into()));
    }

    #[test]
    fn report_prints_login_when_not_signed_in() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let bin = tmp.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        write_cli(&bin, "grok", &fake_script(false));
        write_cli(&bin, "kimi", &fake_script(true));
        let env = DetectEnv {
            path: bin.as_os_str().to_owned(),
            home,
            search_system: false,
        };
        let d = discover(&env);
        let text = format_report(&d, None);
        assert!(text.contains("grok"), "{text}");
        assert!(text.contains("not signed in"), "{text}");
        assert!(text.contains("login: grok"), "{text}");
        assert!(text.contains("kimi"), "{text}");
        assert!(text.contains("atr providers add kimi"), "{text}");
        assert!(!text.contains("sk-"), "{text}");
    }

    #[test]
    fn yes_picks_signed_in_subscriptions_then_opencode() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let bin = tmp.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        write_cli(&bin, "claude", &fake_script(true));
        write_cli(&bin, "grok", &fake_script(false));
        write_cli(&bin, "opencode", &fake_script(true));
        let env = DetectEnv {
            path: bin.as_os_str().to_owned(),
            home,
            search_system: false,
        };
        let d = discover(&env);
        assert!(d
            .opencode
            .as_ref()
            .unwrap()
            .models
            .contains(&"moonshotai/kimi-k2.6".into()));
        let picks = yes_picks(&d);
        let claude = picks.iter().find(|p| p.name == "claude").unwrap();
        let grok = picks.iter().find(|p| p.name == "grok").unwrap();
        let cursor = picks.iter().find(|p| p.name == "cursor").unwrap();
        assert!(claude.enabled);
        assert!(!grok.enabled);
        assert!(!cursor.enabled);
        assert!(
            picks.iter().any(|p| p.name == "kimi" && p.enabled),
            "{picks:?}"
        );
        let names: Vec<_> = picks.iter().map(|p| p.name.as_str()).collect();
        let c = names.iter().position(|n| *n == "claude").unwrap();
        let k = names.iter().position(|n| *n == "kimi").unwrap();
        assert!(c < k, "subscriptions first: {names:?}");
    }

    #[test]
    fn pick_from_token_preset_and_custom_model() {
        let k = pick_from_token("kimi").unwrap();
        assert_eq!(k.preset.as_deref(), Some("kimi"));
        let m = pick_from_token("moonshotai/kimi-k2.6").unwrap();
        assert_eq!(m.preset.as_deref(), Some("kimi"));
        assert_eq!(m.model.as_deref(), Some("moonshotai/kimi-k2.6"));
        let custom = pick_from_token("acme/cool-model").unwrap();
        assert_eq!(custom.kind, "opencode");
        assert_eq!(custom.model.as_deref(), Some("acme/cool-model"));
        assert!(custom.preset.is_none());
    }

    #[test]
    fn report_keychain_locked_and_path_mismatch() {
        let found = FoundCli {
            spec_id: "cursor".into(),
            kind: Some("cursor".into()),
            binary: "cursor-agent".into(),
            path: PathBuf::from("/opt/homebrew/bin/cursor-agent"),
            version: None,
            auth: Auth::KeychainLocked,
            login: "sign in to the Cursor app".into(),
            vendor_note: None,
        };
        let grok = FoundCli {
            spec_id: "grok".into(),
            kind: Some("grok".into()),
            binary: "grok".into(),
            path: PathBuf::from("/opt/homebrew/bin/grok"),
            version: Some("1.2.3".into()),
            auth: Auth::Yes,
            login: "grok".into(),
            vendor_note: None,
        };
        let d = Discovery {
            found: vec![found, grok],
            missing: vec![],
            opencode: None,
        };
        let text = format_report(&d, None);
        assert!(text.contains("unverified (keychain locked)"), "{text}");
        assert!(!text.contains("not signed in"), "{text}");
        assert!(!text.contains("login: sign in"), "{text}");
        assert!(!text.contains('\u{1b}'), "{text}");

        let cfg = crate::config::parse_config_toml(
            r#"
listen = "127.0.0.1:0"
repo_path = "/tmp"
system_prompt_file = "/tmp/p.md"
[[provider]]
name = "grok"
kind = "grok"
cli = "/Users/me/.local/bin/grok"
"#,
        )
        .unwrap();
        let text = format_report(&d, Some(&cfg));
        assert!(text.contains("warning: config uses"), "{text}");
        assert!(text.contains("/Users/me/.local/bin/grok"), "{text}");
        assert!(text.contains("/opt/homebrew/bin/grok"), "{text}");
    }

    #[test]
    fn cli_default_model_reads_codex_config() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        std::fs::create_dir_all(home.join(".codex")).unwrap();
        std::fs::write(home.join(".codex/config.toml"), "model = \"gpt-5.4\"\n").unwrap();
        assert_eq!(
            cli_default_model_in("codex", &home).as_deref(),
            Some("gpt-5.4")
        );
        assert!(cli_default_model_in("claude", &home).is_none());
    }
}
