use crate::config::Provider;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;
use tokio::time::timeout;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct UsageNumbers {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub cost_usd: Option<f64>,
}

/// Subscription CLIs with no dollar figure are `plan`; a reported estimate is `est`.
pub fn usage_cost_kind(cost_usd: Option<f64>) -> &'static str {
    if cost_usd.is_some() {
        "est"
    } else {
        "plan"
    }
}

impl UsageNumbers {
    pub fn merge(&mut self, other: &UsageNumbers) {
        self.input += other.input;
        self.output += other.output;
        self.cache_read += other.cache_read;
        self.cache_write += other.cache_write;
        self.cost_usd = match (self.cost_usd, other.cost_usd) {
            (Some(a), Some(b)) => Some(a + b),
            (Some(a), None) => Some(a),
            (None, Some(b)) => Some(b),
            (None, None) => None,
        };
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum ParsedEvent {
    Heartbeat,
    Session(String),
    Usage {
        usage: UsageNumbers,
        raw: Value,
        result_text: Option<String>,
        is_error: bool,
    },
    Failed(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorClass {
    Limit,
    Auth,
    Server,
    Unavailable,
    Other,
}

#[derive(Debug, Clone)]
pub struct CommandSpec {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub stdin_text: Option<String>,
}

pub fn build_command(
    p: &Provider,
    worktree: &Path,
    prompt_path: &Path,
    prompt: &str,
    session: Option<&str>,
    new_session: bool,
) -> CommandSpec {
    build_command_opts(
        p,
        worktree,
        prompt_path,
        prompt,
        session,
        new_session,
        None,
        None,
    )
}

/// `fork_from` is the original Grok session to `-r` when retrying via
/// `--fork-session -s <new uuid>` so the original is not overwritten.
pub fn build_command_opts(
    p: &Provider,
    worktree: &Path,
    prompt_path: &Path,
    prompt: &str,
    session: Option<&str>,
    new_session: bool,
    fork_from: Option<&str>,
    system_prompt: Option<&Path>,
) -> CommandSpec {
    if p.kind == "codex" {
        return build_codex(p, worktree, prompt_path, prompt, session, system_prompt);
    }
    if p.kind == "opencode" {
        return build_opencode(
            p,
            worktree,
            prompt_path,
            session,
            new_session,
            system_prompt,
        );
    }

    let mut args = p.args.clone();
    if !p.model.is_empty() && !p.model_flag.is_empty() {
        args.push(p.model_flag.clone());
        args.push(p.model.clone());
    }
    if let Some(old) = fork_from.filter(|s| !s.is_empty()) {
        if let Some(new_id) = session {
            args.push("-r".into());
            args.push(old.to_string());
            args.push("--fork-session".into());
            args.push("-s".into());
            args.push(new_id.to_string());
        }
    } else if let Some(sid) = session {
        if new_session && !p.session_id_flag.is_empty() {
            args.push(p.session_id_flag.clone());
            args.push(sid.to_string());
        } else if !new_session && !p.resume_flag.is_empty() {
            args.push(p.resume_flag.clone());
            args.push(sid.to_string());
        }
    }
    if !p.permission_mode.is_empty() {
        let flag = if p.permission_mode_flag.is_empty() {
            "--permission-mode"
        } else {
            p.permission_mode_flag.as_str()
        };
        args.push(flag.into());
        args.push(p.permission_mode.clone());
    }
    if !p.cwd_flag.is_empty() {
        args.push(p.cwd_flag.clone());
        args.push(worktree.display().to_string());
    }
    args.extend(p.extra_args.iter().cloned());
    attach_system_prompt(p, &mut args, system_prompt);
    if p.kind == "grok" && !args.iter().any(|a| a == "--max-turns") {
        let n = crate::config::Config::worker_max_turns(p);
        args.push("--max-turns".into());
        args.push(n.to_string());
    }
    for ov in &p.config_overrides {
        args.push("-c".into());
        args.push(ov.clone());
    }

    let stdin_text = attach_prompt(p, &mut args, prompt_path, prompt);

    CommandSpec {
        program: p.cli.clone(),
        args,
        cwd: worktree.to_path_buf(),
        stdin_text,
    }
}

/// Best-effort confinement for `--dir` workspaces. Codex already sandboxes
/// with `-s`; Claude gets `--add-dir` so the folder is an explicit allowlist
/// even if the CLI would otherwise walk up to a git root. Grok/OpenCode
/// already pass `--cwd` / `--dir`. This is not a hard jail.
pub fn apply_dir_confinement(p: &Provider, spec: &mut CommandSpec, dir: &Path) {
    match p.kind.as_str() {
        "claude" if !spec.args.iter().any(|a| a == "--add-dir") => {
            spec.args.push("--add-dir".into());
            spec.args.push(dir.display().to_string());
        }
        "codex" if !spec.args.iter().any(|a| a == "-s") => {
            spec.args.push("-s".into());
            spec.args.push(if p.sandbox.is_empty() {
                "workspace-write".into()
            } else {
                p.sandbox.clone()
            });
        }
        _ => {}
    }
}

fn build_codex(
    p: &Provider,
    worktree: &Path,
    prompt_path: &Path,
    prompt: &str,
    session: Option<&str>,
    system_prompt: Option<&Path>,
) -> CommandSpec {
    let mut args = Vec::new();
    if let Some(sid) = session {
        args.push("exec".into());
        args.push("resume".into());
        args.push(sid.to_string());
        args.push("--json".into());
        args.push("--skip-git-repo-check".into());
    } else {
        args.push("exec".into());
        args.push("--json".into());
        // `dir` and `none` workspaces are not git repos (or not trusted);
        // without this codex exits 1 before reading the prompt.
        args.push("--skip-git-repo-check".into());
        args.push("-C".into());
        args.push(worktree.display().to_string());
        if !p.sandbox.is_empty() {
            args.push("-s".into());
            args.push(p.sandbox.clone());
        }
    }
    if !p.model.is_empty() {
        let flag = if p.model_flag.is_empty() {
            "-m"
        } else {
            p.model_flag.as_str()
        };
        args.push(flag.into());
        args.push(p.model.clone());
    }
    if !p.profile.is_empty() {
        args.push("--profile".into());
        args.push(p.profile.clone());
    }
    args.extend(p.args.iter().cloned());
    args.extend(p.extra_args.iter().cloned());
    for ov in &p.config_overrides {
        args.push("-c".into());
        args.push(ov.clone());
    }
    if let Some(sys) = system_prompt {
        args.push("-c".into());
        args.push(format!("instructions_file=\"{}\"", sys.display()));
    }
    let _ = prompt_path;
    CommandSpec {
        program: p.cli.clone(),
        args,
        cwd: worktree.to_path_buf(),
        stdin_text: Some(prompt.to_string()),
    }
}

fn build_opencode(
    p: &Provider,
    worktree: &Path,
    prompt_path: &Path,
    session: Option<&str>,
    new_session: bool,
    system_prompt: Option<&Path>,
) -> CommandSpec {
    let mut args = vec![
        "run".into(),
        "--format".into(),
        "json".into(),
        "--auto".into(),
        "--dir".into(),
        worktree.display().to_string(),
    ];
    if !p.model.is_empty() {
        let flag = if p.model_flag.is_empty() {
            "--model"
        } else {
            p.model_flag.as_str()
        };
        args.push(flag.into());
        args.push(p.model.clone());
    }
    if let Some(sid) = session {
        if !new_session {
            let flag = if p.resume_flag.is_empty() {
                "--session"
            } else {
                p.resume_flag.as_str()
            };
            args.push(flag.into());
            args.push(sid.to_string());
        }
    }
    args.extend(p.args.iter().cloned());
    args.extend(p.extra_args.iter().cloned());
    attach_system_prompt(p, &mut args, system_prompt);
    // Prompt is a file attach, never argv/`ps`.
    args.push("--file".into());
    args.push(prompt_path.display().to_string());
    args.push("Follow the attached prompt.".into());
    CommandSpec {
        program: p.cli.clone(),
        args,
        cwd: worktree.to_path_buf(),
        stdin_text: None,
    }
}

fn attach_system_prompt(p: &Provider, args: &mut Vec<String>, path: Option<&Path>) {
    let Some(path) = path else {
        return;
    };
    if p.kind == "codex" {
        return;
    }
    let flag = p.system_prompt_flag.trim();
    if flag.is_empty() {
        return;
    }
    args.push(flag.to_string());
    args.push(path.display().to_string());
}

fn attach_prompt(
    p: &Provider,
    args: &mut Vec<String>,
    prompt_path: &Path,
    prompt: &str,
) -> Option<String> {
    match p.prompt_mode.as_str() {
        "stdin" | "arg" => Some(prompt.to_string()),
        _ => {
            let prefix = p.prompt_file_prefix.as_str();
            args.push(format!("{prefix}{}", prompt_path.display()));
            None
        }
    }
}

pub fn parse_line(kind: &str, line: &str) -> Option<ParsedEvent> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    let value: Value = serde_json::from_str(line).ok()?;
    parse_value(kind, &value)
}

pub fn parse_blob(kind: &str, text: &str) -> Vec<ParsedEvent> {
    let mut out = Vec::new();
    for line in text.lines() {
        if let Some(ev) = parse_line(kind, line) {
            out.push(ev);
        }
    }
    if out.iter().any(|e| matches!(e, ParsedEvent::Usage { .. })) {
        return out;
    }
    if let Ok(value) = serde_json::from_str::<Value>(text.trim()) {
        if let Some(ev) = parse_value(kind, &value) {
            out.push(ev);
        }
    }
    out
}

pub fn parse_value(kind: &str, value: &Value) -> Option<ParsedEvent> {
    if let Some(id) = session_id(value) {
        if looks_like_usage(value) || is_error_event(value) || type_is(value, "error") {
            // fall through; session is also applied by the caller
        } else {
            return Some(ParsedEvent::Session(id));
        }
    }

    if type_is(value, "turn.failed") || type_is(value, "error") {
        let msg = value
            .pointer("/error/data/message")
            .or_else(|| value.pointer("/error/message"))
            .or_else(|| value.get("error"))
            .or_else(|| value.get("message"))
            .or_else(|| value.get("result"))
            .map(value_to_string)
            .unwrap_or_else(|| value.to_string());
        return Some(ParsedEvent::Failed(msg));
    }

    let ty = value.get("type").and_then(|v| v.as_str()).unwrap_or("");
    if matches!(
        ty,
        "text"
            | "thought"
            | "tool_call"
            | "tool_call_update"
            | "end"
            | "SubagentCompleted"
            | "subagent_completed"
    ) {
        if looks_like_usage(value) {
            // fall through
        } else if session_id(value).is_some() {
            return Some(ParsedEvent::Session(session_id(value).unwrap()));
        } else {
            return Some(ParsedEvent::Heartbeat);
        }
    }

    if looks_like_usage(value) {
        let usage = extract_usage(kind, value);
        let is_error = value
            .get("is_error")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
            || type_is(value, "result")
                && value.get("subtype").and_then(|v| v.as_str()) == Some("error")
            || result_needs_attention(value);
        let result_text = value
            .get("result")
            .or_else(|| value.get("text"))
            .map(value_to_string);
        return Some(ParsedEvent::Usage {
            usage,
            raw: value.clone(),
            result_text,
            is_error,
        });
    }

    if session_id(value).is_some() && type_is(value, "thread.started") {
        return Some(ParsedEvent::Session(session_id(value).unwrap()));
    }

    if value.is_object() {
        return Some(ParsedEvent::Heartbeat);
    }
    None
}

fn type_is(value: &Value, want: &str) -> bool {
    value.get("type").and_then(|v| v.as_str()) == Some(want)
}

fn looks_like_usage(value: &Value) -> bool {
    value.get("usage").is_some()
        || value.get("total_cost_usd").is_some()
        || type_is(value, "turn.completed")
        || type_is(value, "result")
        || type_is(value, "step_finish")
        || type_is(value, "usage")
        || value.pointer("/part/tokens").is_some()
}

/// xAI billing ticks per US dollar.
pub const GROK_TICKS_PER_USD: f64 = 10_000_000_000.0;

/// `grok usage <sid>` session object. `inputTokens` includes cache reads.
pub fn parse_grok_usage_cli(value: &Value) -> Option<UsageNumbers> {
    let session = value.get("session").unwrap_or(value);
    if session.get("inputTokens").is_none()
        && session.get("outputTokens").is_none()
        && session.get("cachedReadTokens").is_none()
    {
        return None;
    }
    let input_incl = grab_u64(session, &["inputTokens", "input_tokens"]);
    let cache_read = grab_u64(
        session,
        &[
            "cachedReadTokens",
            "cache_read_input_tokens",
            "cacheReadTokens",
        ],
    );
    let cache_write = grab_u64(
        session,
        &[
            "cacheCreationTokens",
            "cache_creation_input_tokens",
            "cacheWriteTokens",
        ],
    );
    let output = grab_u64(session, &["outputTokens", "output_tokens"]);
    // Grok Build reports cost in xAI "USD ticks": 1 USD = 10^10 ticks
    // (https://docs.x.ai/developers/cost-tracking). A plain dollar field is
    // taken as-is.
    let cost_usd = grab_f64(session, &["costUsdTicks", "cost_usd_ticks"])
        .map(|t| t / GROK_TICKS_PER_USD)
        .or_else(|| grab_f64(session, &["total_cost_usd", "cost_usd"]));
    Some(UsageNumbers {
        input: input_incl.saturating_sub(cache_read),
        output,
        cache_read,
        cache_write,
        cost_usd,
    })
}

/// Whether atr can resume a stored session for this provider (resume flag,
/// or a CLI with a built-in resume subcommand).
pub fn supports_resume(p: &Provider) -> bool {
    matches!(p.kind.as_str(), "codex" | "opencode") || !p.resume_flag.is_empty()
}

pub fn session_id(value: &Value) -> Option<String> {
    for key in [
        "session_id",
        "sessionId",
        "sessionID",
        "thread_id",
        "threadId",
        "chatId",
        "chat_id",
    ] {
        if let Some(s) = value.get(key).and_then(|v| v.as_str()) {
            if !s.is_empty() {
                return Some(s.to_string());
            }
        }
    }
    if let Some(part) = value.get("part") {
        if let Some(id) = session_id(part) {
            return Some(id);
        }
    }
    None
}

fn value_to_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn grab_u64(obj: &Value, keys: &[&str]) -> u64 {
    for key in keys {
        if let Some(v) = obj.get(*key) {
            if let Some(n) = v.as_u64() {
                return n;
            }
            if let Some(n) = v.as_i64() {
                return n.max(0) as u64;
            }
            if let Some(n) = v.as_f64() {
                return n.max(0.0) as u64;
            }
        }
    }
    0
}

fn grab_f64(obj: &Value, keys: &[&str]) -> Option<f64> {
    for key in keys {
        if let Some(v) = obj.get(*key) {
            if let Some(n) = v.as_f64() {
                return Some(n);
            }
            if let Some(n) = v.as_u64() {
                return Some(n as f64);
            }
        }
    }
    None
}

pub fn extract_usage(kind: &str, value: &Value) -> UsageNumbers {
    let part_tokens = value.pointer("/part/tokens").cloned();
    let usage = value
        .get("usage")
        .cloned()
        .or(part_tokens)
        .unwrap_or_else(|| value.clone());
    let mut input = grab_u64(&usage, &["input_tokens", "inputTokens", "input"]);
    let output = grab_u64(&usage, &["output_tokens", "outputTokens", "output"]);
    let cache_obj = usage.get("cache");
    let cache_read = cache_obj
        .map(|c| grab_u64(c, &["read"]))
        .unwrap_or(0)
        .max(grab_u64(
            &usage,
            &[
                "cache_read_input_tokens",
                "cacheReadTokens",
                "cached_input_tokens",
                "cache_read",
            ],
        ));
    let cache_write = cache_obj
        .map(|c| grab_u64(c, &["write"]))
        .unwrap_or(0)
        .max(grab_u64(
            &usage,
            &[
                "cache_creation_input_tokens",
                "cacheWriteTokens",
                "cache_write_input_tokens",
                "cache_write",
            ],
        ));
    let cost = grab_f64(value, &["total_cost_usd", "cost_usd"])
        .or_else(|| {
            value
                .pointer("/part/cost")
                .and_then(|v| v.as_f64())
                .or_else(|| {
                    value
                        .pointer("/part/cost")
                        .and_then(|v| v.as_u64().map(|n| n as f64))
                })
        })
        .or_else(|| grab_f64(&usage, &["total_cost_usd", "cost_usd", "costUSD"]));
    // Codex (and spark via the Codex CLI) report OpenAI-style input that
    // already includes cached tokens (and cache_write). Store uncached so
    // totals match Claude/Grok/Cursor. Codex hit = cache_read / original
    // input (= input + cache_read after this subtract); do not add write.
    if kind == "codex" {
        input = input.saturating_sub(cache_read);
    }
    UsageNumbers {
        input,
        output,
        cache_read,
        cache_write,
        cost_usd: cost,
    }
}

/// Cache-hit ratio. `input` is uncached for every kind (Codex is normalized
/// at extract). Claude-style: cache_read / (input + cache_read + cache_write).
/// Codex-kind: cache_write is already inside the original input, so
/// hit = cache_read / (input + cache_read).
pub fn cache_hit_ratio(kind: &str, u: &UsageNumbers) -> Option<f64> {
    let mut den = u.input.saturating_add(u.cache_read);
    if kind != "codex" {
        den = den.saturating_add(u.cache_write);
    }
    if den == 0 {
        return None;
    }
    Some(u.cache_read as f64 / den as f64)
}

pub fn hit_ratio_from_sums(
    kind: &str,
    input: i64,
    cache_read: i64,
    cache_write: i64,
) -> Option<f64> {
    cache_hit_ratio(
        kind,
        &UsageNumbers {
            input: input.max(0) as u64,
            output: 0,
            cache_read: cache_read.max(0) as u64,
            cache_write: cache_write.max(0) as u64,
            cost_usd: None,
        },
    )
}

/// Classify only from a structured error/result event. Never scan free-text
/// stdout of a successful run — a Grok transcript that mentions "429" or
/// "quota" must not trigger a cooldown.
pub fn classify_error_value(value: &Value) -> Option<ErrorClass> {
    let is_err = is_error_event(value)
        || type_is(value, "error")
        || value.get("subtype").and_then(|v| v.as_str()) == Some("error");
    let error = value.get("error");
    if !is_err && error.is_none() {
        return None;
    }
    let code = error
        .and_then(|e| {
            e.get("code")
                .or_else(|| e.get("status"))
                .or_else(|| e.get("statusCode"))
                .or_else(|| e.pointer("/data/statusCode"))
        })
        .and_then(|v| {
            v.as_i64()
                .or_else(|| v.as_u64().map(|n| n as i64))
                .or_else(|| v.as_str()?.parse().ok())
        });
    let kind = error
        .and_then(|e| e.get("type").or_else(|| e.get("code")))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let msg = if is_err {
        value
            .pointer("/error/data/message")
            .or_else(|| value.pointer("/error/message"))
            .or_else(|| value.get("result"))
            .or_else(|| value.get("message"))
            .map(value_to_string)
            .unwrap_or_default()
    } else {
        error
            .and_then(|e| e.get("message"))
            .or_else(|| error.and_then(|e| e.pointer("/data/message")))
            .map(value_to_string)
            .unwrap_or_default()
    };
    classify_structured_fields(code, kind, &msg)
}

fn classify_structured_fields(code: Option<i64>, kind: &str, message: &str) -> Option<ErrorClass> {
    let kind_l = kind.to_ascii_lowercase().replace(['_', '-'], "");
    let msg = message.to_ascii_lowercase();
    if kind_l.contains("globalratelimit") {
        return Some(ErrorClass::Unavailable);
    }
    if code == Some(429)
        || kind_l.contains("ratelimited")
        || kind_l.contains("usagelimitreached")
        || kind_l.contains("usagepoolexhausted")
        || (kind_l.contains("rate") && kind_l.contains("limit") && !kind_l.contains("global"))
        || kind_l.contains("quota")
        || msg.contains("rate limit")
        || msg.contains("too many requests")
        || msg.contains("usage limit")
        || msg.contains("hit your limit")
        || msg.contains("out of extra usage")
        || (msg.contains("quota") && (msg.contains("exceed") || msg.contains("limit")))
    {
        return Some(ErrorClass::Limit);
    }
    if code == Some(401)
        || code == Some(403)
        || kind_l.contains("auth")
        || kind_l.contains("unauthorized")
        || msg.contains("unauthorized")
        || msg.contains("not logged in")
        || msg.contains("authentication")
        || msg.contains("auth failed")
        || msg.contains("forbidden")
        || msg.contains("invalid api key")
        || msg.contains("missing api key")
        || msg.contains("no api key")
        || msg.contains("not authenticated")
    {
        return Some(ErrorClass::Auth);
    }
    if kind_l.contains("serviceunavailable")
        || kind_l.contains("networkerror")
        || matches!(code, Some(504 | 529))
        || msg.contains("connection reset")
        || msg.contains("connection refused")
        || msg.contains("name or service not known")
        || msg.contains("no such host")
        || msg.contains("nodename nor servname")
        || msg.contains("dns")
        || msg.contains("network is unreachable")
        || msg.contains("overloaded")
        || msg.contains("networkerror")
        || msg.contains("serviceunavailable")
    {
        return Some(ErrorClass::Unavailable);
    }
    if msg.contains("timed out") || msg.contains("timeout") || kind_l.contains("timeout") {
        return Some(ErrorClass::Unavailable);
    }
    if matches!(code, Some(500 | 502 | 503))
        || kind_l.contains("server")
        || msg.contains("internal server error")
        || msg.contains("service unavailable")
    {
        return Some(ErrorClass::Server);
    }
    None
}

pub fn looks_like_keychain_lock(text: &str) -> bool {
    let t = text.to_ascii_lowercase();
    if t.contains("user interaction is not allowed") {
        return true;
    }
    t.contains("keychain")
        && (t.contains("lock")
            || t.contains("denied")
            || t.contains("unable")
            || t.contains("could not")
            || t.contains("cannot")
            || t.contains("ssh")
            || t.contains("login"))
}

#[derive(Debug, Clone)]
pub struct VersionCheck {
    pub name: String,
    pub cli: PathBuf,
    pub ok_path: bool,
    pub version_text: Option<String>,
    pub error: Option<String>,
    pub keychain_locked: bool,
}

pub async fn check_version(p: &Provider) -> VersionCheck {
    let ok_path = p.cli.exists();
    if !ok_path {
        return VersionCheck {
            name: p.name.clone(),
            cli: p.cli.clone(),
            ok_path: false,
            version_text: None,
            error: Some("cli path does not exist".into()),
            keychain_locked: false,
        };
    }
    let mut cmd = Command::new(&p.cli);
    cmd.args(&p.version_args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let result = timeout(
        Duration::from_secs(10),
        crate::util::output_retry_busy(&mut cmd),
    )
    .await;
    match result {
        Ok(Ok(out)) => {
            let text = format!(
                "{}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
            let keychain_locked = looks_like_keychain_lock(&text);
            VersionCheck {
                name: p.name.clone(),
                cli: p.cli.clone(),
                ok_path: true,
                version_text: Some(text.trim().to_string()),
                error: keychain_locked
                    .then(|| "keychain locked (SSH session), run inside the GUI session".into()),
                keychain_locked,
            }
        }
        Ok(Err(e)) => {
            let keychain_locked = looks_like_keychain_lock(&e.to_string());
            VersionCheck {
                name: p.name.clone(),
                cli: p.cli.clone(),
                ok_path: true,
                version_text: None,
                error: Some(if keychain_locked {
                    "keychain locked (SSH session), run inside the GUI session".into()
                } else {
                    e.to_string()
                }),
                keychain_locked,
            }
        }
        Err(_) => VersionCheck {
            name: p.name.clone(),
            cli: p.cli.clone(),
            ok_path: true,
            version_text: None,
            error: Some("version check timed out".into()),
            keychain_locked: false,
        },
    }
}

/// `cli-versions.json` next to config: CLI versions seen by `atr serve` (which
/// runs in the session that can read the keychain) so `atr doctor` over SSH
/// can show them.
pub fn versions_path(config_path: &std::path::Path) -> PathBuf {
    config_path
        .parent()
        .map(|d| d.join("cli-versions.json"))
        .unwrap_or_else(|| PathBuf::from("cli-versions.json"))
}

/// Pre-rename file name, still read for compatibility.
fn legacy_versions_path(config_path: &std::path::Path) -> PathBuf {
    versions_path(config_path).with_file_name("cli-pins.json")
}

/// Service-seen version for `name`: `(version, checked_at)`.
pub fn recorded_version(config_path: &std::path::Path, name: &str) -> Option<(String, String)> {
    for path in [
        versions_path(config_path),
        legacy_versions_path(config_path),
    ] {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else {
            continue;
        };
        if let Some(e) = v.get(name) {
            let s = |k: &str| e.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string();
            return Some((s("version"), s("checked_at")));
        }
    }
    None
}

pub async fn warn_versions(providers: &[Provider]) {
    warn_versions_recorded(providers, None).await
}

pub async fn warn_versions_recorded(providers: &[Provider], config_path: Option<&std::path::Path>) {
    let mut rec = serde_json::Map::new();
    let now = chrono::Utc::now().to_rfc3339();
    for p in providers {
        if !p.enabled {
            continue;
        }
        let v = check_version(p).await;
        if v.ok_path && !v.keychain_locked {
            rec.insert(
                p.name.clone(),
                serde_json::json!({
                    "version": v.version_text.clone().unwrap_or_default(),
                    "checked_at": now,
                }),
            );
        }
        if !v.ok_path {
            tracing::warn!(
                provider = %p.name,
                cli = %p.cli.display(),
                "CLI missing; will skip until the path exists"
            );
            continue;
        }
        if v.keychain_locked {
            tracing::warn!(
                provider = %p.name,
                "CLI version unreadable (keychain locked over SSH); continuing"
            );
            continue;
        }
        if let Some(err) = &v.error {
            tracing::warn!(provider = %p.name, %err, "could not read CLI version");
            continue;
        }
        tracing::info!(provider = %p.name, version = ?v.version_text, "CLI version");
    }
    if let Some(cp) = config_path {
        if !rec.is_empty() {
            let path = versions_path(cp);
            let tmp = path.with_extension("json.tmp");
            if std::fs::write(&tmp, serde_json::Value::Object(rec).to_string()).is_ok() {
                let _ = std::fs::rename(&tmp, &path);
            }
        }
    }
}

/// UUID v4 for CLIs that accept `--session-id` on a brand-new session.
pub fn new_session_uuid() -> String {
    let mut b = [0u8; 16];
    fill_random(&mut b);
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7], b[8], b[9], b[10], b[11], b[12], b[13], b[14],
        b[15]
    )
}

fn fill_random(buf: &mut [u8]) {
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        use std::io::Read;
        if f.read_exact(buf).is_ok() {
            return;
        }
    }
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    for (i, b) in buf.iter_mut().enumerate() {
        *b ^= ((t >> ((i % 8) * 8)) & 0xff) as u8 ^ (i as u8).wrapping_mul(31);
    }
}

/// Claude can exit 0 with subtype=success while permission_denials is
/// non-empty, or say the work is unfinished. Those must not be success.
pub fn result_needs_attention(value: &Value) -> bool {
    if let Some(arr) = value.get("permission_denials").and_then(|v| v.as_array()) {
        if !arr.is_empty() {
            return true;
        }
    }
    let stop = value
        .get("stop_reason")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if stop.eq_ignore_ascii_case("permission_denied")
        || stop.eq_ignore_ascii_case("tool_denied")
        || stop.eq_ignore_ascii_case("unfinished")
    {
        return true;
    }
    let result = value
        .get("result")
        .or_else(|| value.get("text"))
        .map(value_to_string)
        .unwrap_or_default();
    let r = result.to_ascii_lowercase();
    r.contains("permission den")
        || r.contains("don't have permission")
        || r.contains("do not have permission")
        || r.contains("wasn't able to finish")
        || r.contains("was not able to finish")
        || r.contains("unable to finish")
        || r.contains("couldn't complete")
        || r.contains("could not complete")
        || r.contains("not able to complete")
        || r.contains("work is not finished")
        || r.contains("i was blocked")
        || r.contains("blocked from")
}

pub fn is_error_event(value: &Value) -> bool {
    value
        .get("is_error")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
        || type_is(value, "turn.failed")
        || type_is(value, "error")
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::config::Provider;
    use std::time::Duration;

    pub(crate) fn stub(kind: &str) -> Provider {
        Provider {
            name: kind.into(),
            kind: kind.into(),
            preset: String::new(),
            enabled: true,
            cli: PathBuf::from(kind),
            legacy_expected_version: false,
            version_args: vec!["--version".into()],
            model: "m".into(),
            model_flag: if kind == "codex" {
                "-m".into()
            } else {
                "--model".into()
            },
            max_parallel: 1,
            cooldown_after_limit: Duration::from_secs(1),
            degraded_after_stall: Duration::from_secs(2),
            heartbeat_timeout: Duration::from_secs(1),
            run_timeout: Duration::from_secs(1),
            resume_flag: "--resume".into(),
            session_id_flag: String::new(),
            cwd_flag: String::new(),
            permission_mode: String::new(),
            permission_mode_flag: "--permission-mode".into(),
            prompt_mode: "arg".into(),
            prompt_file_prefix: "@".into(),
            profile: String::new(),
            sandbox: "workspace-write".into(),
            args: if kind == "codex" {
                vec![]
            } else {
                vec!["-p".into()]
            },
            extra_args: vec![],
            config_overrides: if kind == "codex" {
                vec!["model_provider=\"openai\"".into()]
            } else {
                vec![]
            },
            quota: vec![],
            reported_usage: String::new(),
            skip_at: 0.9,
            reported_usage_interval: Duration::from_secs(600),
            reported_usage_timeout: Duration::from_secs(20),
            health: Default::default(),
            plugin: false,
            max_turns: 0,
            cache_window: Duration::from_secs(3600),
            system_prompt_flag: crate::config::default_system_prompt_flag(kind).into(),
            keep_alive: String::new(),
            capabilities: Vec::new(),
        }
    }

    #[test]
    fn cost_kind_plan_when_no_dollars() {
        assert_eq!(usage_cost_kind(None), "plan");
        assert_eq!(usage_cost_kind(Some(0.01)), "est");
    }

    #[test]
    fn codex_kind_stores_uncached_input() {
        let v: Value = serde_json::from_str(
            r#"{"usage":{"input_tokens":200,"cached_input_tokens":160,"cache_write_input_tokens":10,"output_tokens":30}}"#,
        )
        .unwrap();
        let spark = extract_usage("codex", &v);
        assert_eq!(spark.input, 40);
        assert_eq!(spark.cache_read, 160);
        assert_eq!(spark.cache_write, 10);
        let hit = cache_hit_ratio("codex", &spark).unwrap();
        assert!(
            (hit - 160.0 / 200.0).abs() < 1e-9,
            "codex hit {hit} (cache_read / original input, no cache_write)"
        );
        let claude_shaped = extract_usage(
            "claude",
            &serde_json::from_str(
                r#"{"usage":{"input_tokens":100,"cache_read_input_tokens":800,"cache_creation_input_tokens":20,"output_tokens":50}}"#,
            )
            .unwrap(),
        );
        assert_eq!(
            claude_shaped.input, 100,
            "non-codex input stays uncached as reported"
        );
        assert_eq!(claude_shaped.cache_read, 800);
    }

    #[test]
    fn claude_result_shape() {
        let line = r#"{"type":"result","subtype":"success","session_id":"s1","total_cost_usd":0.02,"usage":{"input_tokens":11,"cache_creation_input_tokens":22,"cache_read_input_tokens":33,"output_tokens":44}}"#;
        match parse_line("claude", line).unwrap() {
            ParsedEvent::Usage { usage, .. } => {
                assert_eq!(usage.input, 11);
                assert_eq!(usage.cache_write, 22);
                assert_eq!(usage.cache_read, 33);
                assert_eq!(usage.output, 44);
                assert_eq!(usage.cost_usd, Some(0.02));
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(
            session_id(&serde_json::from_str(line).unwrap()).as_deref(),
            Some("s1")
        );
    }

    #[test]
    fn grok_json_shape() {
        let blob = r#"{"text":"hi","sessionId":"g1","usage":{"input_tokens":1,"cache_read_input_tokens":2,"cache_creation_input_tokens":3,"output_tokens":4,"reasoning_tokens":5,"total_tokens":6},"total_cost_usd":0.01}"#;
        let evs = parse_blob("grok", blob);
        let usage = evs
            .iter()
            .find_map(|e| match e {
                ParsedEvent::Usage { usage, .. } => Some(usage.clone()),
                _ => None,
            })
            .unwrap();
        assert_eq!(usage.input, 1);
        assert_eq!(usage.cache_read, 2);
        assert_eq!(usage.cache_write, 3);
        assert_eq!(usage.output, 4);
        assert_eq!(usage.cost_usd, Some(0.01));
    }

    #[test]
    fn cursor_camel_case() {
        let line = r#"{"type":"result","subtype":"success","result":"ok","session_id":"c1","usage":{"inputTokens":8,"outputTokens":9,"cacheReadTokens":10,"cacheWriteTokens":11}}"#;
        match parse_line("cursor", line).unwrap() {
            ParsedEvent::Usage { usage, .. } => {
                assert_eq!(
                    usage,
                    UsageNumbers {
                        input: 8,
                        output: 9,
                        cache_read: 10,
                        cache_write: 11,
                        cost_usd: None
                    }
                );
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn codex_events() {
        let started = r#"{"type":"thread.started","thread_id":"thr-1"}"#;
        assert_eq!(
            parse_line("codex", started),
            Some(ParsedEvent::Session("thr-1".into()))
        );
        let done = r#"{"type":"turn.completed","usage":{"input_tokens":200,"cached_input_tokens":160,"cache_write_input_tokens":10,"output_tokens":30,"reasoning_output_tokens":8}}"#;
        match parse_line("codex", done).unwrap() {
            ParsedEvent::Usage { usage, .. } => {
                assert_eq!(usage.input, 40, "uncached = input - cached");
                assert_eq!(usage.cache_read, 160);
                assert_eq!(usage.cache_write, 10);
                assert_eq!(usage.output, 30);
                let hit = cache_hit_ratio("codex", &usage).unwrap();
                assert!(
                    (hit - 160.0 / 200.0).abs() < 1e-9,
                    "hit {hit} want 80% (160/200), not 160/210 from extra cache_write"
                );
            }
            other => panic!("{other:?}"),
        }
        let fail = r#"{"type":"turn.failed","error":{"message":"rate limit exceeded"}}"#;
        match parse_line("codex", fail).unwrap() {
            ParsedEvent::Failed(m) => assert!(m.contains("rate limit")),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn opencode_json_events() {
        let start = r#"{"type":"step_start","sessionID":"ses_abc","part":{"type":"step-start"}}"#;
        match parse_line("opencode", start).unwrap() {
            ParsedEvent::Session(id) => assert_eq!(id, "ses_abc"),
            other => panic!("{other:?}"),
        }
        let done = r#"{"type":"step_finish","sessionID":"ses_abc","part":{"type":"step-finish","reason":"stop","cost":0.001,"tokens":{"input":671,"output":8,"reasoning":0,"cache":{"read":21415,"write":0}}}}"#;
        match parse_line("opencode", done).unwrap() {
            ParsedEvent::Usage { usage, .. } => {
                assert_eq!(usage.input, 671);
                assert_eq!(usage.output, 8);
                assert_eq!(usage.cache_read, 21415);
                assert_eq!(usage.cost_usd, Some(0.001));
            }
            other => panic!("{other:?}"),
        }
        let err = r#"{"type":"error","sessionID":"ses_abc","error":{"name":"APIError","data":{"message":"unauthorized","statusCode":401}}}"#;
        match parse_line("opencode", err).unwrap() {
            ParsedEvent::Failed(m) => assert!(m.contains("unauthorized"), "{m}"),
            other => panic!("{other:?}"),
        }
        let ev: Value = serde_json::from_str(err).unwrap();
        assert_eq!(classify_error_value(&ev), Some(ErrorClass::Auth));
    }

    #[test]
    fn opencode_command_uses_file_not_argv_prompt() {
        let mut p = stub("opencode");
        p.model = "moonshotai/kimi-k2.6".into();
        p.cli = PathBuf::from("opencode");
        p.resume_flag = "--session".into();
        let spec = build_command(
            &p,
            Path::new("/wt"),
            Path::new("/wt/.atr/prompt.md"),
            "SECRET-PROMPT",
            Some("ses_1"),
            false,
        );
        assert_eq!(spec.program, PathBuf::from("opencode"));
        assert!(spec.args.iter().any(|a| a == "run"));
        assert!(spec.args.windows(2).any(|w| w == ["--format", "json"]));
        assert!(spec.args.contains(&"--auto".into()));
        assert!(spec
            .args
            .windows(2)
            .any(|w| w == ["--model", "moonshotai/kimi-k2.6"]));
        assert!(spec.args.windows(2).any(|w| w == ["--session", "ses_1"]));
        assert!(spec
            .args
            .windows(2)
            .any(|w| w == ["--file", "/wt/.atr/prompt.md"]));
        assert!(!spec.args.iter().any(|a| a.contains("SECRET-PROMPT")));
        assert!(spec.stdin_text.is_none());
    }

    #[test]
    fn classify_only_structured_errors() {
        let success_mentions_quota: Value = serde_json::from_str(
            r#"{"type":"result","is_error":false,"result":"fixed issue 429 and mentioned quota","usage":{"input_tokens":1,"output_tokens":1}}"#,
        )
        .unwrap();
        assert_eq!(classify_error_value(&success_mentions_quota), None);

        let limit: Value = serde_json::from_str(
            r#"{"type":"result","is_error":true,"result":"rate limit exceeded (429)"}"#,
        )
        .unwrap();
        assert_eq!(classify_error_value(&limit), Some(ErrorClass::Limit));

        let auth: Value = serde_json::from_str(
            r#"{"type":"result","is_error":true,"result":"authentication failed: unauthorized"}"#,
        )
        .unwrap();
        assert_eq!(classify_error_value(&auth), Some(ErrorClass::Auth));

        let failed: Value = serde_json::from_str(
            r#"{"type":"turn.failed","error":{"code":429,"message":"too many requests"}}"#,
        )
        .unwrap();
        assert_eq!(classify_error_value(&failed), Some(ErrorClass::Limit));

        let grok_limit: Value = serde_json::from_str(
            r#"{"type":"error","error":{"type":"UsageLimitReached","message":"weekly cap"}}"#,
        )
        .unwrap();
        assert_eq!(classify_error_value(&grok_limit), Some(ErrorClass::Limit));
        let grok_pool: Value = serde_json::from_str(
            r#"{"type":"error","error":{"type":"UsagePoolExhausted","message":"pool empty"}}"#,
        )
        .unwrap();
        assert_eq!(classify_error_value(&grok_pool), Some(ErrorClass::Limit));
        let grok_rl: Value = serde_json::from_str(
            r#"{"type":"error","error":{"type":"RateLimited","message":"slow down"}}"#,
        )
        .unwrap();
        assert_eq!(classify_error_value(&grok_rl), Some(ErrorClass::Limit));

        let grok_unavail: Value = serde_json::from_str(
            r#"{"type":"error","error":{"type":"ServiceUnavailable","message":"try later"}}"#,
        )
        .unwrap();
        assert_eq!(
            classify_error_value(&grok_unavail),
            Some(ErrorClass::Unavailable)
        );
        let grok_net: Value = serde_json::from_str(
            r#"{"type":"error","error":{"type":"NetworkError","message":"connection reset by peer"}}"#,
        )
        .unwrap();
        assert_eq!(
            classify_error_value(&grok_net),
            Some(ErrorClass::Unavailable)
        );
        let grok_global: Value = serde_json::from_str(
            r#"{"type":"error","error":{"type":"GlobalRateLimit","message":"global"}}"#,
        )
        .unwrap();
        assert_eq!(
            classify_error_value(&grok_global),
            Some(ErrorClass::Unavailable)
        );
        let dns: Value = serde_json::from_str(
            r#"{"type":"error","error":{"message":"getaddrinfo: nodename nor servname provided"}}"#,
        )
        .unwrap();
        assert_eq!(classify_error_value(&dns), Some(ErrorClass::Unavailable));
        let over: Value = serde_json::from_str(
            r#"{"type":"result","is_error":true,"result":"overloaded (529)"}"#,
        )
        .unwrap();
        assert_eq!(classify_error_value(&over), Some(ErrorClass::Unavailable));
        let gw: Value = serde_json::from_str(
            r#"{"type":"turn.failed","error":{"code":504,"message":"gateway timeout"}}"#,
        )
        .unwrap();
        assert_eq!(classify_error_value(&gw), Some(ErrorClass::Unavailable));
    }

    #[test]
    fn keychain_lock_is_detected() {
        assert!(looks_like_keychain_lock(
            "Could not access the login keychain (locked)"
        ));
        assert!(looks_like_keychain_lock("User interaction is not allowed."));
        assert!(!looks_like_keychain_lock("cursor-agent 2026.10.01"));
    }

    #[tokio::test]
    async fn version_check_detects_locked_keychain() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("cursor-agent");
        std::fs::write(
            &script,
            "#!/bin/sh\necho 'Could not access the login keychain (locked)' >&2\nexit 1\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let mut p = stub("cursor");
        p.cli = script;
        let v = check_version(&p).await;
        assert!(v.keychain_locked);
        assert_eq!(
            v.error.as_deref(),
            Some("keychain locked (SSH session), run inside the GUI session")
        );
    }

    #[test]
    fn prompt_is_never_passed_on_argv() {
        let mut p = stub("claude");
        p.prompt_mode = "stdin".into();
        let spec = build_command(
            &p,
            Path::new("/wt"),
            Path::new("/wt/.atr/prompt.md"),
            "SECRET PROMPT BODY",
            None,
            false,
        );
        assert!(spec.args.iter().all(|a| !a.contains("SECRET PROMPT BODY")));
        assert_eq!(spec.stdin_text.as_deref(), Some("SECRET PROMPT BODY"));

        let mut g = stub("grok");
        g.prompt_mode = "file".into();
        g.prompt_file_prefix = "--prompt-file=".into();
        let spec = build_command(
            &g,
            Path::new("/wt"),
            Path::new("/wt/.atr/prompt.md"),
            "SECRET PROMPT BODY",
            None,
            false,
        );
        assert!(spec
            .args
            .iter()
            .any(|a| a == "--prompt-file=/wt/.atr/prompt.md"));
        assert!(spec.args.iter().all(|a| !a.contains("SECRET PROMPT BODY")));
    }

    #[test]
    fn codex_command_uses_overrides() {
        let p = stub("codex");
        let spec = build_command(
            &p,
            Path::new("/wt"),
            Path::new("/wt/.atr/prompt.md"),
            "hello",
            None,
            false,
        );
        assert!(spec
            .args
            .windows(2)
            .any(|w| w[0] == "-c" && w[1].contains("model_provider")));
        assert!(spec.args.contains(&"exec".into()));
        assert!(spec.args.iter().all(|a| a != "hello"));
        assert_eq!(spec.stdin_text.as_deref(), Some("hello"));
        let resume = build_command(
            &p,
            Path::new("/wt"),
            Path::new("/wt/.atr/prompt.md"),
            "more",
            Some("thr-1"),
            false,
        );
        assert_eq!(resume.args[0], "exec");
        assert_eq!(resume.args[1], "resume");
        assert_eq!(resume.args[2], "thr-1");
    }

    #[test]
    fn grok_new_session_uses_session_id_flag() {
        let mut g = stub("grok");
        g.session_id_flag = "--session-id".into();
        g.resume_flag = "--resume".into();
        g.prompt_mode = "file".into();
        g.prompt_file_prefix = "--prompt-file=".into();
        let sid = "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee";
        let start = build_command(
            &g,
            Path::new("/wt"),
            Path::new("/wt/.atr/prompt.md"),
            "hi",
            Some(sid),
            true,
        );
        assert!(
            start
                .args
                .windows(2)
                .any(|w| w[0] == "--session-id" && w[1] == sid),
            "new grok session must pass --session-id, got {:?}",
            start.args
        );
        assert!(!start.args.iter().any(|a| a == "--resume"));

        let resume = build_command(
            &g,
            Path::new("/wt"),
            Path::new("/wt/.atr/prompt.md"),
            "more",
            Some(sid),
            false,
        );
        assert!(
            resume
                .args
                .windows(2)
                .any(|w| w[0] == "--resume" && w[1] == sid),
            "stall retry must --resume the generated id, got {:?}",
            resume.args
        );
        assert!(!resume.args.iter().any(|a| a == "--session-id"));
        assert!(start.args.windows(2).any(|w| w == ["--max-turns", "30"]));
    }

    #[test]
    fn grok_fork_session_flags() {
        let mut g = stub("grok");
        g.kind = "grok".into();
        g.prompt_mode = "file".into();
        g.prompt_file_prefix = "--prompt-file=".into();
        let spec = build_command_opts(
            &g,
            Path::new("/wt"),
            Path::new("/wt/.atr/prompt.md"),
            "retry",
            Some("new-sid"),
            false,
            Some("old-sid"),
            None,
        );
        assert!(
            spec.args.windows(2).any(|w| w == ["-r", "old-sid"]),
            "{:?}",
            spec.args
        );
        assert!(spec.args.iter().any(|a| a == "--fork-session"));
        assert!(spec.args.windows(2).any(|w| w == ["-s", "new-sid"]));
        assert!(!spec.args.iter().any(|a| a == "--resume"));
    }

    #[test]
    fn grok_usage_cli_subtracts_cache_reads() {
        let v: Value = serde_json::from_str(
            r#"{"session":{"inputTokens":120,"cachedReadTokens":20,"cacheCreationTokens":5,"outputTokens":8,"costUsdTicks":2500,"turns":[{}]}}"#,
        )
        .unwrap();
        let u = parse_grok_usage_cli(&v).unwrap();
        assert_eq!(u.input, 100);
        assert_eq!(u.cache_read, 20);
        assert_eq!(u.cache_write, 5);
        assert_eq!(u.output, 8);
        assert!((u.cost_usd.unwrap() - 2.5e-7).abs() < 1e-15);
        // Real Grok Build 1.0.52 output from the Mini: 126833600 ticks = $0.0126836.
        let real: Value = serde_json::from_str(
            r#"{"sessionId":"s","session":{"inputTokens":19090,"outputTokens":142,"cachedReadTokens":1152,"cacheCreationTokens":0,"costUsdTicks":126833600,"primaryModelId":"grok-4.7-build"}}"#,
        )
        .unwrap();
        let r = parse_grok_usage_cli(&real).unwrap();
        assert!((r.cost_usd.unwrap() - 0.01268336).abs() < 1e-12);
        assert_eq!(r.input, 19090 - 1152);
    }

    #[test]
    fn grok_stream_timeline_types() {
        for ty in [
            "text",
            "thought",
            "tool_call",
            "tool_call_update",
            "end",
            "SubagentCompleted",
        ] {
            let line = format!(r#"{{"type":"{ty}","text":"x"}}"#);
            assert!(parse_line("grok", &line).is_some(), "{ty} should parse");
        }
        let usage = parse_line(
            "grok",
            r#"{"type":"usage","usage":{"input_tokens":1,"output_tokens":2}}"#,
        )
        .unwrap();
        assert!(matches!(usage, ParsedEvent::Usage { .. }));
    }

    #[test]
    fn claude_permission_mode_is_passed() {
        let mut p = stub("claude");
        p.permission_mode = "bypassPermissions".into();
        p.prompt_mode = "stdin".into();
        let spec = build_command(
            &p,
            Path::new("/wt"),
            Path::new("/wt/.atr/prompt.md"),
            "hi",
            None,
            false,
        );
        assert!(
            spec.args
                .windows(2)
                .any(|w| w[0] == "--permission-mode" && w[1] == "bypassPermissions"),
            "headless Claude needs --permission-mode, got {:?}",
            spec.args
        );
    }

    #[test]
    fn permission_denials_is_not_success() {
        let denied: Value = serde_json::from_str(
            r#"{"type":"result","subtype":"success","is_error":false,"result":"ok","permission_denials":[{"tool_name":"Edit"}],"usage":{"input_tokens":1,"output_tokens":1}}"#,
        )
        .unwrap();
        assert!(result_needs_attention(&denied));
        match parse_value("claude", &denied).unwrap() {
            ParsedEvent::Usage { is_error, .. } => assert!(is_error),
            other => panic!("{other:?}"),
        }

        let unfinished: Value = serde_json::from_str(
            r#"{"type":"result","subtype":"success","is_error":false,"result":"I wasn't able to finish the work","permission_denials":[],"usage":{"input_tokens":1,"output_tokens":1}}"#,
        )
        .unwrap();
        assert!(result_needs_attention(&unfinished));

        let ok: Value = serde_json::from_str(
            r#"{"type":"result","subtype":"success","is_error":false,"result":"done","permission_denials":[],"usage":{"input_tokens":1,"output_tokens":1}}"#,
        )
        .unwrap();
        assert!(!result_needs_attention(&ok));
    }

    #[test]
    fn session_uuid_is_v4() {
        let id = new_session_uuid();
        let parts: Vec<_> = id.split('-').collect();
        assert_eq!(parts.len(), 5, "{id}");
        assert_eq!(parts[0].len(), 8);
        assert_eq!(parts[1].len(), 4);
        assert_eq!(parts[2].len(), 4);
        assert!(parts[2].starts_with('4'), "{id}");
        assert_eq!(parts[3].len(), 4);
        assert!(
            matches!(parts[3].as_bytes()[0], b'8' | b'9' | b'a' | b'b'),
            "{id}"
        );
        assert_eq!(parts[4].len(), 12);
    }

    #[test]
    fn each_cli_gets_its_system_prompt_slot() {
        let sys = Path::new("/wt/.atr/system.md");
        let mut claude = stub("claude");
        claude.prompt_mode = "stdin".into();
        let c = build_command_opts(
            &claude,
            Path::new("/wt"),
            Path::new("/wt/.atr/prompt.md"),
            "var",
            None,
            true,
            None,
            Some(sys),
        );
        assert!(
            c.args
                .windows(2)
                .any(|w| w == ["--append-system-prompt-file", "/wt/.atr/system.md"]),
            "{:?}",
            c.args
        );
        assert!(!c.args.iter().any(|a| a == "var"), "prefix stays off argv");

        let mut grok = stub("grok");
        grok.prompt_mode = "file".into();
        grok.prompt_file_prefix = "--prompt-file=".into();
        let g = build_command_opts(
            &grok,
            Path::new("/wt"),
            Path::new("/wt/.atr/prompt.md"),
            "var",
            None,
            true,
            None,
            Some(sys),
        );
        assert!(
            g.args
                .windows(2)
                .any(|w| w == ["--rules", "/wt/.atr/system.md"]),
            "{:?}",
            g.args
        );

        let mut oc = stub("opencode");
        oc.kind = "opencode".into();
        let o = build_command_opts(
            &oc,
            Path::new("/wt"),
            Path::new("/wt/.atr/prompt.md"),
            "var",
            None,
            true,
            None,
            Some(sys),
        );
        assert!(
            o.args
                .windows(2)
                .any(|w| w == ["--system", "/wt/.atr/system.md"]),
            "{:?}",
            o.args
        );

        let mut cx = stub("codex");
        cx.kind = "codex".into();
        let x = build_command_opts(
            &cx,
            Path::new("/wt"),
            Path::new("/wt/.atr/prompt.md"),
            "var",
            None,
            true,
            None,
            Some(sys),
        );
        assert!(
            x.args
                .iter()
                .any(|a| a.contains("instructions_file=") && a.contains("system.md")),
            "{:?}",
            x.args
        );
        assert_eq!(x.stdin_text.as_deref(), Some("var"));

        let resume = build_command_opts(
            &claude,
            Path::new("/wt"),
            Path::new("/wt/.atr/prompt.md"),
            "only delta",
            Some("sid"),
            false,
            None,
            None,
        );
        assert!(!resume
            .args
            .iter()
            .any(|a| a.contains("append-system-prompt") || a == "--rules"));
    }

    #[test]
    fn dir_confinement_adds_claude_add_dir_and_codex_sandbox() {
        let mut claude = stub("claude");
        claude.permission_mode = "bypassPermissions".into();
        let mut spec = build_command(
            &claude,
            Path::new("/notes"),
            Path::new("/state/prompts/t/prompt.md"),
            "hi",
            None,
            true,
        );
        apply_dir_confinement(&claude, &mut spec, Path::new("/notes"));
        assert!(
            spec.args.windows(2).any(|w| w == ["--add-dir", "/notes"]),
            "{:?}",
            spec.args
        );

        let mut codex = stub("codex");
        codex.sandbox = String::new();
        let mut spec = build_command(
            &codex,
            Path::new("/notes"),
            Path::new("/state/prompts/t/prompt.md"),
            "hi",
            None,
            true,
        );
        apply_dir_confinement(&codex, &mut spec, Path::new("/notes"));
        assert!(
            spec.args.windows(2).any(|w| w == ["-s", "workspace-write"]),
            "{:?}",
            spec.args
        );
    }
    #[test]
    fn codex_always_skips_git_repo_check() {
        // Regression: `dir`/`none` workspaces are not git repos; without the
        // flag codex exits 1 before reading the prompt. Both new and resumed
        // sessions must carry it.
        let p = stub("codex");
        for session in [None, Some("thr-9")] {
            let spec = build_command(
                &p,
                Path::new("/tmp/not-a-repo"),
                Path::new("/tmp/not-a-repo/.atr/prompt.md"),
                "hi",
                session,
                false,
            );
            assert!(
                spec.args.iter().any(|a| a == "--skip-git-repo-check"),
                "session={session:?} args={:?}",
                spec.args
            );
        }
    }

    #[test]
    fn grok_usage_cost_is_ticks_not_dollars() {
        // Regression: costUsdTicks is 1e-10 USD; reading it as dollars made
        // ~19k tokens show as $126.
        let v = serde_json::json!({"costUsdTicks": 1_268_336_000u64, "inputTokens": 19000});
        let u = parse_grok_usage_cli(&v).expect("usage");
        let cost = u.cost_usd.expect("cost");
        assert!((cost - 0.1268336).abs() < 1e-9, "cost={cost}");
        assert!(cost < 1.0);
        assert_eq!(GROK_TICKS_PER_USD, 1e10);
    }
}
