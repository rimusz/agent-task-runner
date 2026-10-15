//! Built-in MCP server: stdio (`atr mcp`) and Streamable HTTP (`/mcp`).
//! HTTP requires a bearer token. atr never prints that token.

use crate::app::Shared;
use crate::config::{Config, McpConfig};
use crate::github_auth::redact_secrets;
use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::io::Write;
use std::path::Path;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

pub const PROTOCOL_VERSION: &str = "2025-03-26";

const TOOLS: &[(&str, &str)] = &[
    (
        "add_task",
        "Enqueue a new atr task. Optional workspace (git repo, local dir, or none), output, key, and provider/driver hint.",
    ),
    (
        "list_tasks",
        "List tasks. Optional filters: repo, status, role, parent.",
    ),
    (
        "task_status",
        "One task: status, parent/children, timeline, last output.",
    ),
    (
        "steer_task",
        "Send a follow-up message to a running or waiting task.",
    ),
    ("cancel_task", "Cancel a task. force=true kills a live run."),
    (
        "wait_task",
        "Block until a task finishes (waiting/done/failed) or timeout_seconds elapses.",
    ),
    (
        "get_result",
        "Return a task's output text, diff, and PR URL (if any).",
    ),
    ("list_repos", "List watch targets (shorthand or [[repo]])."),
    (
        "list_providers",
        "Providers with order, enabled, cooldown, and recent usage.",
    ),
    (
        "usage_summary",
        "Token usage and cache savings. period: today, 7d, 30d, month, or YYYY-MM-DD..YYYY-MM-DD.",
    ),
    ("doctor", "Run atr doctor (ok/warn/FAIL). Secrets redacted."),
    (
        "config_get",
        "Read-only config snapshot. Secrets are redacted. Cannot change config.",
    ),
];

pub fn generate_token() -> String {
    crate::new_session_uuid().replace('-', "") + &crate::new_session_uuid().replace('-', "")
}

pub fn write_token_file(path: &Path, token: &str) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    }
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(path)
        .with_context(|| format!("write {}", path.display()))?;
    f.write_all(token.trim().as_bytes())?;
    f.write_all(b"\n")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("chmod 0600 {}", path.display()))?;
    }
    Ok(())
}

/// How to create a bearer token. Never includes the token value.
pub fn token_missing_hint(cfg: &McpConfig) -> String {
    let env = if cfg.token_env.trim().is_empty() {
        "ATR_MCP_TOKEN"
    } else {
        cfg.token_env.as_str()
    };
    format!(
        "run `atr init`, or write a 0600 token file at {}, or set {env}",
        cfg.token_file.display()
    )
}

/// HTTP/doctor error when `/mcp` is on but no token is configured.
pub fn token_missing_error(cfg: &McpConfig) -> String {
    format!("mcp token not configured; {}", token_missing_hint(cfg))
}

/// Env named by `token_env`, then `token_file`. Never logs the value.
pub fn read_token(cfg: &McpConfig) -> Option<String> {
    if !cfg.token_env.is_empty() {
        if let Ok(v) = std::env::var(&cfg.token_env) {
            let t = v.trim().to_string();
            if !t.is_empty() {
                return Some(t);
            }
        }
    }
    let text = std::fs::read_to_string(&cfg.token_file).ok()?;
    let t = text.trim().to_string();
    if t.is_empty() {
        None
    } else {
        Some(t)
    }
}

pub fn tokens_equal(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut d = 0u8;
    for (x, y) in a.bytes().zip(b.bytes()) {
        d |= x ^ y;
    }
    d == 0
}

pub fn bearer_token(header: &str) -> Option<String> {
    let h = header.trim();
    let rest = h
        .strip_prefix("Bearer ")
        .or_else(|| h.strip_prefix("bearer "))?;
    let t = rest.trim();
    if t.is_empty() {
        None
    } else {
        Some(t.to_string())
    }
}

/// DNS-rebinding guard. Missing Origin is allowed (CLI clients).
pub fn origin_allowed(origin: Option<&str>, listen: &str) -> bool {
    let Some(raw) = origin.map(str::trim).filter(|s| !s.is_empty()) else {
        return true;
    };
    let host = origin_host(raw);
    if matches!(host.as_str(), "127.0.0.1" | "localhost" | "::1" | "[::1]") {
        return true;
    }
    listen_host(listen).is_some_and(|h| h == host)
}

fn origin_host(origin: &str) -> String {
    let rest = origin
        .split("://")
        .nth(1)
        .unwrap_or(origin)
        .trim_end_matches('/');
    if let Some(h) = rest.strip_prefix('[') {
        return format!("[{}]", h.split(']').next().unwrap_or(h));
    }
    rest.split(':').next().unwrap_or(rest).to_string()
}

fn listen_host(listen: &str) -> Option<String> {
    let listen = listen.trim();
    if let Some(rest) = listen.strip_prefix('[') {
        let h = rest.split(']').next()?;
        return Some(format!("[{h}]"));
    }
    Some(listen.rsplit_once(':')?.0.to_string())
}

pub async fn serve_stdio(shared: Arc<Shared>) -> Result<()> {
    let stdin = tokio::io::stdin();
    let mut reader = BufReader::new(stdin);
    let mut stdout = tokio::io::stdout();
    loop {
        let Some(raw) = read_stdio_message(&mut reader).await? else {
            break;
        };
        if raw.trim().is_empty() {
            continue;
        }
        for resp in handle_raw(&shared, &raw).await {
            let mut line = serde_json::to_string(&resp)?;
            line.push('\n');
            stdout.write_all(line.as_bytes()).await?;
            stdout.flush().await?;
        }
    }
    Ok(())
}

async fn read_stdio_message(reader: &mut BufReader<tokio::io::Stdin>) -> Result<Option<String>> {
    let mut first = String::new();
    let n = reader.read_line(&mut first).await?;
    if n == 0 {
        return Ok(None);
    }
    if first.to_ascii_lowercase().starts_with("content-length:") {
        let len: usize = first
            .split(':')
            .nth(1)
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0);
        loop {
            let mut hdr = String::new();
            let n = reader.read_line(&mut hdr).await?;
            if n == 0 || hdr.trim().is_empty() {
                break;
            }
        }
        let mut buf = vec![0u8; len];
        if len > 0 {
            reader.read_exact(&mut buf).await?;
        }
        Ok(Some(String::from_utf8_lossy(&buf).into_owned()))
    } else {
        Ok(Some(first))
    }
}

/// Handle a POST body (one object or a batch array). Empty vec = notifications only.
pub async fn handle_raw(shared: &Shared, raw: &str) -> Vec<Value> {
    let Ok(v) = serde_json::from_str::<Value>(raw.trim()) else {
        return vec![rpc_error(Value::Null, -32700, "parse error")];
    };
    if let Some(arr) = v.as_array() {
        let mut out = Vec::new();
        for item in arr {
            if let Some(r) = dispatch(shared, item).await {
                out.push(r);
            }
        }
        return out;
    }
    dispatch(shared, &v).await.into_iter().collect()
}

async fn dispatch(shared: &Shared, msg: &Value) -> Option<Value> {
    if !msg.is_object() {
        return Some(rpc_error(Value::Null, -32600, "invalid request"));
    }
    let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or("");
    let id = msg.get("id").cloned();
    let params = msg.get("params").cloned().unwrap_or(Value::Null);
    let id = id?;
    let result = match method {
        "initialize" => Ok(initialize()),
        "ping" | "initialized" | "notifications/initialized" => Ok(json!({})),
        "tools/list" => Ok(tools_list()),
        "tools/call" => call_tool(shared, &params).await,
        "resources/list" => Ok(resources_list()),
        "resources/read" => read_resource(shared, &params),
        "resources/templates/list" => Ok(json!({"resourceTemplates": []})),
        "prompts/list" => Ok(json!({"prompts": []})),
        "" => Err((-32600, "invalid request".into())),
        other => Err((-32601, format!("method not found: {other}"))),
    };
    Some(match result {
        Ok(v) => json!({"jsonrpc":"2.0","id": id, "result": v}),
        Err((code, msg)) => rpc_error(id, code, &msg),
    })
}

fn rpc_error(id: Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc":"2.0","id": id, "error": {"code": code, "message": redact_secrets(message)}})
}

fn initialize() -> Value {
    json!({
        "protocolVersion": PROTOCOL_VERSION,
        "capabilities": {
            "tools": {"listChanged": false},
            "resources": {"subscribe": false, "listChanged": false}
        },
        "serverInfo": {
            "name": "atr",
            "version": crate::VERSION,
        },
        "instructions": "Drive atr tasks. Serve must be running for work to dispatch. Use add_task, wait_task, get_result, steer_task, task_status. A task is a prompt plus an optional workspace (git, dir, or none). Config is read-only."
    })
}

fn tools_list() -> Value {
    let tools: Vec<Value> = TOOLS
        .iter()
        .map(|(name, desc)| {
            json!({
                "name": name,
                "description": desc,
                "inputSchema": tool_schema(name),
            })
        })
        .collect();
    json!({"tools": tools})
}

fn tool_schema(name: &str) -> Value {
    match name {
        "add_task" => json!({
            "type": "object",
            "properties": {
                "title": {"type": "string"},
                "body": {"type": "string"},
                "repo": {"type": "string", "description": "Git workspace: watch repo name, owner/name, or path"},
                "dir": {"type": "string", "description": "Local directory workspace (edit in place, stay inside)"},
                "workspace": {"type": "string", "description": "git, dir, or none"},
                "output": {"type": "string", "description": "reply, pr, issue_comment, branch, patch, text, webhook"},
                "webhook": {"type": "string"},
                "patch": {"type": "string"},
                "key": {"type": "string"},
                "provider": {"type": "string", "description": "Provider or driver hint"},
                "hint": {"type": "string"}
            },
            "required": ["body"]
        }),
        "wait_task" => json!({
            "type": "object",
            "properties": {
                "key": {"type": "string"},
                "timeout_seconds": {"type": "number", "description": "Default 120, max 3600"}
            },
            "required": ["key"]
        }),
        "get_result" => json!({
            "type": "object",
            "properties": {
                "key": {"type": "string"}
            },
            "required": ["key"]
        }),
        "list_tasks" => json!({
            "type": "object",
            "properties": {
                "repo": {"type": "string"},
                "status": {"type": "string"},
                "role": {"type": "string"},
                "parent": {"type": "string"}
            }
        }),
        "task_status" => json!({
            "type": "object",
            "properties": {
                "key": {"type": "string"}
            },
            "required": ["key"]
        }),
        "steer_task" => json!({
            "type": "object",
            "properties": {
                "key": {"type": "string"},
                "message": {"type": "string"}
            },
            "required": ["key", "message"]
        }),
        "cancel_task" => json!({
            "type": "object",
            "properties": {
                "key": {"type": "string"},
                "force": {"type": "boolean"}
            },
            "required": ["key"]
        }),
        "usage_summary" => json!({
            "type": "object",
            "properties": {
                "period": {"type": "string", "description": "today, 7d, 30d, month, or YYYY-MM-DD..YYYY-MM-DD"},
                "repo": {"type": "string"}
            }
        }),
        "doctor" => json!({"type": "object", "properties": {
            "cache": {"type": "boolean"}
        }}),
        _ => json!({"type": "object", "properties": {}}),
    }
}

fn resources_list() -> Value {
    json!({
        "resources": [
            {
                "uri": "atr://status",
                "name": "status",
                "description": "atr status snapshot (JSON)",
                "mimeType": "application/json"
            },
            {
                "uri": "atr://dashboard",
                "name": "dashboard",
                "description": "Dashboard snapshot (JSON)",
                "mimeType": "application/json"
            }
        ]
    })
}

fn read_resource(shared: &Shared, params: &Value) -> Result<Value, (i64, String)> {
    let uri = params.get("uri").and_then(|v| v.as_str()).unwrap_or("");
    let (name, body) = match uri {
        "atr://status" => ("status", shared.status_json()),
        "atr://dashboard" => ("dashboard", crate::dashboard::snapshot(shared)),
        _ => return Err((-32602, format!("unknown resource {uri}"))),
    };
    Ok(json!({
        "contents": [{
            "uri": uri,
            "mimeType": "application/json",
            "text": redact_secrets(&body.to_string()),
            "name": name
        }]
    }))
}

async fn call_tool(shared: &Shared, params: &Value) -> Result<Value, (i64, String)> {
    let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
    let args = params.get("arguments").cloned().unwrap_or(json!({}));
    let out = match name {
        "add_task" => add_task(shared, &args),
        "wait_task" => wait_task(shared, &args).await,
        "get_result" => get_result(shared, &args),
        "list_tasks" => list_tasks(shared, &args),
        "task_status" => task_status(shared, &args),
        "steer_task" => steer_task(shared, &args),
        "cancel_task" => cancel_task(shared, &args),
        "list_repos" => list_repos(shared),
        "list_providers" => list_providers(shared),
        "usage_summary" => usage_summary(shared, &args),
        "doctor" => doctor(shared, &args).await,
        "config_get" => config_get(shared),
        "" => return Err((-32602, "missing tool name".into())),
        other => return Err((-32602, format!("unknown tool {other}"))),
    };
    match out {
        Ok(v) => Ok(tool_text(&v)),
        Err(e) => Ok(json!({
            "content": [{"type": "text", "text": redact_secrets(&e)}],
            "isError": true
        })),
    }
}

fn tool_text(v: &Value) -> Value {
    json!({
        "content": [{"type": "text", "text": redact_secrets(&pretty(v))}],
        "isError": false
    })
}

fn pretty(v: &Value) -> String {
    serde_json::to_string_pretty(v).unwrap_or_else(|_| v.to_string())
}

fn arg_str(args: &Value, keys: &[&str]) -> Option<String> {
    for k in keys {
        if let Some(s) = args.get(*k).and_then(|v| v.as_str()) {
            let t = s.trim();
            if !t.is_empty() {
                return Some(t.to_string());
            }
        }
    }
    None
}

fn add_task(shared: &Shared, args: &Value) -> Result<Value, String> {
    let body = arg_str(args, &["body", "prompt", "text", "message"]).ok_or("missing body")?;
    let cfg = shared.config.read().unwrap().clone();
    crate::task::check_prompt_bytes(&body, cfg.max_prompt_bytes).map_err(|e| e.to_string())?;
    let title = arg_str(args, &["title"]).unwrap_or_else(|| crate::task::title_from_prompt(&body));
    let repo_filter = arg_str(args, &["repo"]);
    let dir = arg_str(args, &["dir", "path"]);
    let workspace_in = arg_str(args, &["workspace"]);
    let output_in = arg_str(args, &["output"]);
    let webhook = arg_str(args, &["webhook"]);
    let patch = arg_str(args, &["patch"]);
    let entry = repo_filter
        .as_deref()
        .and_then(|f| cfg.resolve_repo_filter(f));
    let (workspace_kind, repo_path, allow_empty) = crate::task::resolve_intake_workspace(
        dir.as_deref(),
        repo_filter.as_deref(),
        workspace_in.as_deref(),
        |r| {
            cfg.resolve_repo_filter(r)
                .map(|e| e.path.display().to_string())
        },
    )
    .map_err(|e| e.to_string())?;
    let output_kind = output_in
        .as_deref()
        .map(crate::task::OutputKind::parse)
        .transpose()
        .map_err(|e| e.to_string())?
        .unwrap_or(crate::task::OutputKind::Reply);
    if output_kind.needs_git() && workspace_kind != crate::task::WorkspaceKind::Git {
        return Err(format!(
            "output {} needs a git workspace",
            output_kind.as_str()
        ));
    }
    let output_spec = webhook.or(patch).unwrap_or_default();
    let key = match arg_str(args, &["key"]) {
        Some(k) => k,
        None => unique_key(shared, &cfg, entry.as_ref(), &title)?,
    };
    let text = format!("# {title}\n\n{body}");
    let outcome = shared
        .submit_with(
            &key,
            &text,
            "mcp",
            repo_path.as_deref(),
            None,
            false,
            false,
            crate::app::SubmitOpts {
                workspace_kind: Some(workspace_kind.as_str().into()),
                output_kind: Some(output_kind.as_str().into()),
                output_spec: if output_spec.is_empty() {
                    None
                } else {
                    Some(output_spec)
                },
                source_kind: Some("mcp".into()),
                title: Some(title.clone()),
                allow_empty_repo: allow_empty,
                provider_hint: arg_str(args, &["provider", "hint", "driver"]),
            },
        )
        .map_err(|e| e.to_string())?;
    Ok(json!({
        "ok": true,
        "key": key,
        "status": match outcome {
            crate::SubmitOutcome::Accepted => "accepted",
            crate::SubmitOutcome::Queued => "queued",
        },
        "title": title,
        "workspace": workspace_kind.as_str(),
        "output": output_kind.as_str(),
    }))
}

fn unique_key(
    shared: &Shared,
    cfg: &Config,
    entry: Option<&crate::config::RepoEntry>,
    title: &str,
) -> Result<String, String> {
    let prefix = entry
        .map(|e| {
            if e.shorthand {
                cfg.task_key_prefix.clone()
            } else {
                e.name.clone()
            }
        })
        .unwrap_or_else(|| cfg.task_key_prefix.clone());
    crate::task::allocate_key(&shared.db, &prefix, title).map_err(|e| e.to_string())
}

pub fn slugify(title: &str) -> String {
    crate::task::slugify(title)
}

fn terminal_status(status: &str) -> bool {
    matches!(
        status,
        "waiting" | "done" | "failed" | "needs_attention" | "cancelled"
    )
}

async fn wait_task(shared: &Shared, args: &Value) -> Result<Value, String> {
    let key = arg_str(args, &["key"]).ok_or("missing key")?;
    let timeout = args
        .get("timeout_seconds")
        .and_then(|v| v.as_f64().or_else(|| v.as_u64().map(|n| n as f64)))
        .unwrap_or(120.0)
        .clamp(1.0, 3600.0);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs_f64(timeout);
    loop {
        let t = shared
            .db
            .get_task(&key)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("unknown task {key}"))?;
        if terminal_status(&t.status) {
            return get_result(shared, args);
        }
        let now = std::time::Instant::now();
        if now >= deadline {
            return Ok(json!({
                "ok": false,
                "key": key,
                "status": t.status,
                "timed_out": true,
            }));
        }
        let slice = deadline
            .saturating_duration_since(now)
            .min(std::time::Duration::from_millis(200));
        tokio::time::sleep(slice).await;
    }
}

fn get_result(shared: &Shared, args: &Value) -> Result<Value, String> {
    let key = arg_str(args, &["key"]).ok_or("missing key")?;
    let t = shared
        .db
        .get_task(&key)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("unknown task {key}"))?;
    let mut payload = crate::output::result_payload(shared, &t);
    payload["ok"] = json!(true);
    payload["timed_out"] = json!(false);
    Ok(payload)
}

fn list_tasks(shared: &Shared, args: &Value) -> Result<Value, String> {
    let cfg = shared.config.read().unwrap().clone();
    let repo = arg_str(args, &["repo"]);
    let status = arg_str(args, &["status"]);
    let role = arg_str(args, &["role"]);
    let parent = arg_str(args, &["parent"]);
    let tasks: Vec<Value> = shared
        .db
        .list_tasks()
        .map_err(|e| e.to_string())?
        .into_iter()
        .filter(|t| repo.as_deref().is_none_or(|f| cfg.task_matches_repo(t, f)))
        .filter(|t| status.as_deref().is_none_or(|s| t.status == s))
        .filter(|t| role.as_deref().is_none_or(|r| t.role == r))
        .filter(|t| match parent.as_deref() {
            None => true,
            Some("-") | Some("") => t.parent_key.is_none(),
            Some(p) => t.parent_key.as_deref() == Some(p),
        })
        .map(|t| {
            json!({
                "key": t.key,
                "status": t.status,
                "role": t.role,
                "parent": t.parent_key,
                "title": t.title,
                "provider": t.provider,
                "hint": t.provider_hint,
                "repo": cfg.task_repo_name(&t),
                "issue": t.issue,
                "workspace": t.workspace_kind,
                "output": t.output_kind,
                "source": t.source_kind,
                "updated": t.updated,
            })
        })
        .collect();
    Ok(json!({"tasks": tasks}))
}

fn task_status(shared: &Shared, args: &Value) -> Result<Value, String> {
    let key = arg_str(args, &["key"]).ok_or("missing key")?;
    let cfg = shared.config.read().unwrap().clone();
    let t = shared
        .db
        .get_task(&key)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("unknown task {key}"))?;
    let runs = shared.db.list_runs(Some(&key)).map_err(|e| e.to_string())?;
    let last_id = runs.last().map(|r| r.id);
    let events = if let Some(id) = last_id {
        shared.db.events_for_run(id).unwrap_or_default()
    } else {
        Vec::new()
    };
    let last_output = crate::handoff::last_assistant_text(&events).unwrap_or_default();
    let timeline: Vec<Value> = events
        .iter()
        .rev()
        .take(80)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .map(|e| {
            json!({
                "kind": e.kind,
                "text": clip(&e.raw, 400),
            })
        })
        .collect();
    let kids = shared.db.children_of(&key).unwrap_or_default();
    Ok(json!({
        "key": t.key,
        "status": t.status,
        "role": t.role,
        "parent": t.parent_key,
        "title": t.title,
        "provider": t.provider,
        "hint": t.provider_hint,
        "repo": cfg.task_repo_name(&t),
        "issue": t.issue,
        "workspace": t.workspace_kind,
        "output": t.output_kind,
        "source": t.source_kind,
        "session_id": t.session_id,
        "updated": t.updated,
        "children": kids.iter().map(|c| json!({"key": c.key, "status": c.status, "title": c.title})).collect::<Vec<_>>(),
        "last_output": clip(&last_output, 4000),
        "timeline": timeline,
        "last_run": runs.last().map(|r| json!({
            "id": r.id,
            "provider": r.provider,
            "outcome": r.outcome,
            "started": r.started,
            "ended": r.ended,
        })),
    }))
}

fn clip(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(n).collect::<String>())
    }
}

fn steer_task(shared: &Shared, args: &Value) -> Result<Value, String> {
    let key = arg_str(args, &["key"]).ok_or("missing key")?;
    let message = arg_str(args, &["message", "body", "text"]).ok_or("missing message")?;
    if shared
        .db
        .get_task(&key)
        .map_err(|e| e.to_string())?
        .is_none()
    {
        return Err(format!("unknown task {key}"));
    }
    let outcome = shared
        .submit(&key, &message, "mcp", None, None, false, false)
        .map_err(|e| e.to_string())?;
    Ok(json!({
        "ok": true,
        "key": key,
        "status": match outcome {
            crate::SubmitOutcome::Accepted => "accepted",
            crate::SubmitOutcome::Queued => "queued",
        },
    }))
}

fn cancel_task(shared: &Shared, args: &Value) -> Result<Value, String> {
    let key = arg_str(args, &["key"]).ok_or("missing key")?;
    let force = args.get("force").and_then(|v| v.as_bool()).unwrap_or(false);
    shared.cancel(&key, force).map_err(|e| e.to_string())?;
    Ok(json!({"ok": true, "key": key, "force": force}))
}

fn list_repos(shared: &Shared) -> Result<Value, String> {
    let cfg = shared.config.read().unwrap().clone();
    let repos: Vec<Value> = cfg
        .effective_repos()
        .iter()
        .map(|r| {
            json!({
                "name": r.name,
                "enabled": r.enabled,
                "github": r.github,
                "label": r.label,
                "path": r.path.display().to_string(),
                "max_parallel": r.max_parallel,
                "shorthand": r.shorthand,
            })
        })
        .collect();
    Ok(json!({"repos": repos}))
}

fn list_providers(shared: &Shared) -> Result<Value, String> {
    let status = shared.status_json();
    Ok(json!({
        "driver": status.get("driver"),
        "providers": status.get("providers"),
    }))
}

fn usage_summary(shared: &Shared, args: &Value) -> Result<Value, String> {
    let spec = arg_str(args, &["period"]).unwrap_or_else(|| "7d".into());
    let period = crate::usage::parse_period(&spec).map_err(|e| e.to_string())?;
    let repo = arg_str(args, &["repo"]);
    let report = crate::usage::report_filtered(shared, &period, repo.as_deref())
        .map_err(|e| e.to_string())?;
    Ok(report.to_json())
}

async fn doctor(shared: &Shared, args: &Value) -> Result<Value, String> {
    let cfg = shared.config.read().unwrap().clone();
    let cache = args.get("cache").and_then(|v| v.as_bool()).unwrap_or(false);
    let report = crate::doctor::run(&cfg, &shared.config_path, cache).await;
    let parsed: Value = serde_json::from_str(&report.to_json()).unwrap_or(json!({}));
    Ok(parsed)
}

fn config_get(shared: &Shared) -> Result<Value, String> {
    let cfg = shared.config.read().unwrap().clone();
    let token_configured = read_token(&cfg.mcp).is_some();
    Ok(json!({
        "file": shared.config_path.display().to_string(),
        "listen": cfg.listen,
        "driver": cfg.driver_name(),
        "planner": cfg.planner.mode.as_str(),
        "review": cfg.review.enabled,
        "task_key_prefix": cfg.task_key_prefix,
        "display_timezone": cfg.display_timezone.label,
        "repos": cfg.effective_repos().iter().map(|r| json!({
            "name": r.name,
            "enabled": r.enabled,
            "github": r.github,
            "label": r.label,
            "path": r.path.display().to_string(),
            "shorthand": r.shorthand,
        })).collect::<Vec<_>>(),
        "providers": cfg.providers.iter().map(|p| json!({
            "name": p.name,
            "kind": p.kind,
            "enabled": p.enabled,
            "model": p.model,
            "preset": p.preset,
        })).collect::<Vec<_>>(),
        "mcp": {
            "enabled": cfg.mcp.enabled,
            "token_env": cfg.mcp.token_env,
            "token_file": cfg.mcp.token_file.display().to_string(),
            "token_configured": token_configured,
        },
        "telegram": {
            "enabled": cfg.telegram.enabled,
            "assistant": cfg.telegram.assistant.enabled,
        },
        "github": {
            "enabled": cfg.github.enabled,
            "repo": cfg.github.repo,
            "label": cfg.github.label,
        },
        "sources": cfg.sources.iter().map(|s| json!({
            "kind": s.kind.as_str(),
            "enabled": s.enabled,
            "dir": s.dir.display().to_string(),
        })).collect::<Vec<_>>(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::load_config;

    fn shared(dir: &std::path::Path) -> Arc<Shared> {
        let cfg_path = dir.join("config.toml");
        let prompt = dir.join("p.md");
        std::fs::write(&prompt, "p\n").unwrap();
        std::fs::write(
            &cfg_path,
            format!(
                r#"
listen = "127.0.0.1:0"
state_db = "{db}"
worktree_root = "{root}"
repo_path = "{root}"
system_prompt_file = "{prompt}"
display_timezone = "UTC"
[mcp]
enabled = true
token_env = "ATR_MCP_TOKEN_TEST_UNSET"
token_file = "{token}"
[[provider]]
name = "claude"
kind = "claude"
cli = "/bin/true"
"#,
                db = dir.join("s.db").display(),
                root = dir.display(),
                prompt = prompt.display(),
                token = dir.join("mcp.token").display(),
            ),
        )
        .unwrap();
        let cfg = load_config(&cfg_path).unwrap();
        Shared::from_config(cfg_path, cfg).unwrap()
    }

    #[tokio::test]
    async fn add_task_saves_provider_hint_before_queueing() {
        // Regression (#5 item 7): the hint was stored after queueing, so
        // serve could dispatch to the default provider first.
        let dir = tempfile::tempdir().unwrap();
        let s = shared(dir.path());
        let add = handle_raw(
            &s,
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"add_task","arguments":{"title":"Hinted","body":"x","provider":" grok "}}}"#,
        )
        .await;
        assert_eq!(add[0]["result"]["isError"], false, "{}", add[0]);
        let t = s.db.get_task("task-hinted").unwrap().unwrap();
        assert_eq!(t.provider_hint, "grok");
        assert_eq!(t.status, "queued");
    }

    #[tokio::test]
    async fn initialize_and_tools() {
        let dir = tempfile::tempdir().unwrap();
        let s = shared(dir.path());
        let init = handle_raw(
            &s,
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"t","version":"1"}}}"#,
        )
        .await;
        assert_eq!(init.len(), 1);
        assert_eq!(init[0]["result"]["serverInfo"]["name"], "atr");
        let listed = handle_raw(&s, r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#).await;
        let names: Vec<_> = listed[0]["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|t| t["name"].as_str())
            .collect();
        for want in [
            "add_task",
            "wait_task",
            "get_result",
            "list_tasks",
            "task_status",
            "steer_task",
            "cancel_task",
            "list_repos",
            "list_providers",
            "usage_summary",
            "doctor",
            "config_get",
        ] {
            assert!(names.contains(&want), "{names:?} missing {want}");
        }
    }

    #[tokio::test]
    async fn add_list_steer_cancel_and_resources() {
        let dir = tempfile::tempdir().unwrap();
        let s = shared(dir.path());
        let add = handle_raw(
            &s,
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"add_task","arguments":{"title":"Fix login","body":"do it","provider":"claude"}}}"#,
        )
        .await;
        let text = add[0]["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("task-fix-login"), "{text}");
        assert!(text.contains("\"workspace\": \"none\""), "{text}");
        assert!(!text.to_ascii_lowercase().contains("secret"), "{text}");
        let list = handle_raw(
            &s,
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"list_tasks","arguments":{"status":"queued"}}}"#,
        )
        .await;
        let listed = list[0]["result"]["content"][0]["text"].as_str().unwrap();
        assert!(listed.contains("task-fix-login"), "{listed}");
        let steer = handle_raw(
            &s,
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"steer_task","arguments":{"key":"task-fix-login","message":"also tests"}}}"#,
        )
        .await;
        let steered = steer[0]["result"]["content"][0]["text"].as_str().unwrap();
        assert!(
            steered.contains("accepted") || steered.contains("queued"),
            "{steered}"
        );
        let st = handle_raw(
            &s,
            r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"task_status","arguments":{"key":"task-fix-login"}}}"#,
        )
        .await;
        let body = st[0]["result"]["content"][0]["text"].as_str().unwrap();
        assert!(body.contains("timeline"), "{body}");
        let res = handle_raw(
            &s,
            r#"{"jsonrpc":"2.0","id":5,"method":"resources/read","params":{"uri":"atr://status"}}"#,
        )
        .await;
        assert!(res[0]["result"]["contents"][0]["text"]
            .as_str()
            .unwrap()
            .contains("tasks"));
        let cancel = handle_raw(
            &s,
            r#"{"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"cancel_task","arguments":{"key":"task-fix-login","force":false}}}"#,
        )
        .await;
        assert_eq!(cancel[0]["result"]["isError"], false);
    }

    #[tokio::test]
    async fn add_task_defaults_none_and_title_from_prompt() {
        let dir = tempfile::tempdir().unwrap();
        let s = shared(dir.path());
        let add = handle_raw(
            &s,
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"add_task","arguments":{"body":"Derive a title from this prompt"}}}"#,
        )
        .await;
        let text = add[0]["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("task-derive-a-title"), "{text}");
        assert!(text.contains("\"workspace\": \"none\""), "{text}");
        let t =
            s.db.get_task("task-derive-a-title-from-this-prompt")
                .unwrap()
                .or_else(|| {
                    s.db.list_tasks()
                        .unwrap()
                        .into_iter()
                        .find(|t| t.title.contains("Derive a title"))
                })
                .unwrap();
        assert_eq!(t.workspace_kind, "none");
        assert_eq!(t.title, "Derive a title from this prompt");
        assert_eq!(t.issue, None);
    }

    #[tokio::test]
    async fn add_task_rejects_oversized_prompt() {
        let dir = tempfile::tempdir().unwrap();
        let cfg_path = dir.path().join("config.toml");
        let prompt = dir.path().join("p.md");
        std::fs::write(&prompt, "p\n").unwrap();
        std::fs::write(
            &cfg_path,
            format!(
                r#"
listen = "127.0.0.1:0"
state_db = "{db}"
worktree_root = "{root}"
repo_path = "{root}"
system_prompt_file = "{prompt}"
display_timezone = "UTC"
max_prompt_bytes = "32"
[mcp]
enabled = true
token_env = "ATR_MCP_TOKEN_TEST_UNSET"
token_file = "{token}"
[[provider]]
name = "claude"
kind = "claude"
cli = "/bin/true"
"#,
                db = dir.path().join("s.db").display(),
                root = dir.path().display(),
                prompt = prompt.display(),
                token = dir.path().join("mcp.token").display(),
            ),
        )
        .unwrap();
        let cfg = load_config(&cfg_path).unwrap();
        let s = Shared::from_config(cfg_path, cfg).unwrap();
        let add = handle_raw(
            &s,
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"add_task","arguments":{"body":"this prompt is definitely over thirty two bytes"}}}"#,
        )
        .await;
        assert_eq!(add[0]["result"]["isError"], true);
        let text = add[0]["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("bytes"), "{text}");
    }

    #[tokio::test]
    async fn add_task_none_workspace_wait_and_get_result() {
        let dir = tempfile::tempdir().unwrap();
        let s = shared(dir.path());
        let add = handle_raw(
            &s,
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"add_task","arguments":{"title":"Math","body":"what is 2+2?","workspace":"none","output":"text"}}}"#,
        )
        .await;
        let text = add[0]["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("task-math"), "{text}");
        assert!(text.contains("none"), "{text}");
        let t = s.db.get_task("task-math").unwrap().unwrap();
        assert_eq!(t.issue, None);
        assert_eq!(t.workspace_kind, "none");
        assert_eq!(t.output_kind, "text");
        s.db.set_status("task-math", "waiting").unwrap();
        s.db.set_task_result("task-math", "4", "", None).unwrap();
        let waited = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            handle_raw(
                &s,
                r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"wait_task","arguments":{"key":"task-math","timeout_seconds":2}}}"#,
            ),
        )
        .await
        .expect("wait_task exceeded test budget");
        let body = waited[0]["result"]["content"][0]["text"].as_str().unwrap();
        assert!(body.contains("\"text\""), "{body}");
        assert!(body.contains("4"), "{body}");
        let got = handle_raw(
            &s,
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"get_result","arguments":{"key":"task-math"}}}"#,
        )
        .await;
        let g = got[0]["result"]["content"][0]["text"].as_str().unwrap();
        assert!(g.contains("4"), "{g}");
        assert!(!g.to_ascii_lowercase().contains("secret"), "{g}");
    }

    #[tokio::test]
    async fn wait_task_times_out_when_still_queued() {
        let dir = tempfile::tempdir().unwrap();
        let s = shared(dir.path());
        s.db.upsert_task("task-slow", dir.path().to_str().unwrap(), None)
            .unwrap();
        s.db.set_status("task-slow", "queued").unwrap();
        let waited = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            handle_raw(
                &s,
                r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"wait_task","arguments":{"key":"task-slow","timeout_seconds":1}}}"#,
            ),
        )
        .await
        .expect("wait_task timeout path exceeded test budget");
        let body = waited[0]["result"]["content"][0]["text"].as_str().unwrap();
        assert!(body.contains("timed_out"), "{body}");
        assert!(body.contains("queued"), "{body}");
    }

    #[tokio::test]
    async fn config_get_redacts_and_omits_token() {
        let dir = tempfile::tempdir().unwrap();
        let token_path = dir.path().join("mcp.token");
        write_token_file(&token_path, "super-secret-mcp-token-value").unwrap();
        let s = shared(dir.path());
        let out = handle_raw(
            &s,
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"config_get","arguments":{}}}"#,
        )
        .await;
        let text = out[0]["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(!text.contains("super-secret-mcp-token-value"), "{text}");
        assert!(text.contains("token_configured"), "{text}");
        assert!(text.contains("true"), "{text}");
        let repos = handle_raw(
            &s,
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"list_repos"}}"#,
        )
        .await;
        assert!(repos[0]["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("repos"));
        let usage = handle_raw(
            &s,
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"usage_summary","arguments":{"period":"7d"}}}"#,
        )
        .await;
        assert_eq!(usage[0]["result"]["isError"], false);
        let dash = handle_raw(
            &s,
            r#"{"jsonrpc":"2.0","id":4,"method":"resources/read","params":{"uri":"atr://dashboard"}}"#,
        )
        .await;
        assert_eq!(dash[0]["jsonrpc"], "2.0");
        assert!(dash[0]["result"]["contents"][0]["text"].is_string());
    }

    #[test]
    fn slug_and_auth_helpers() {
        assert_eq!(slugify("Fix Login!"), "fix-login");
        assert_eq!(slugify("..."), "task");
        assert!(tokens_equal("abc", "abc"));
        assert!(!tokens_equal("abc", "abd"));
        assert!(!tokens_equal("abc", "ab"));
        assert_eq!(bearer_token("Bearer tok").as_deref(), Some("tok"));
        assert!(bearer_token("Basic x").is_none());
        assert!(origin_allowed(None, "127.0.0.1:7420"));
        assert!(origin_allowed(
            Some("http://127.0.0.1:7420"),
            "127.0.0.1:7420"
        ));
        assert!(!origin_allowed(
            Some("https://evil.example"),
            "127.0.0.1:7420"
        ));
        assert!(origin_allowed(
            Some("http://100.64.0.10:7420"),
            "100.64.0.10:7420"
        ));
    }

    #[test]
    fn token_file_is_0600_and_unread_value_stays_private() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("mcp.token");
        write_token_file(&p, "hidden-token").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        let cfg = McpConfig {
            enabled: true,
            token_env: "ATR_MCP_TOKEN_TEST_UNSET".into(),
            token_file: p,
        };
        assert_eq!(read_token(&cfg).as_deref(), Some("hidden-token"));
    }

    #[test]
    fn token_missing_error_explains_init_and_file() {
        let cfg = McpConfig {
            enabled: true,
            token_env: "ATR_MCP_TOKEN".into(),
            token_file: std::path::PathBuf::from("/tmp/atr-mcp.token"),
        };
        let err = token_missing_error(&cfg);
        assert!(err.contains("mcp token not configured"), "{err}");
        assert!(err.contains("atr init"), "{err}");
        assert!(err.contains("/tmp/atr-mcp.token"), "{err}");
        assert!(err.contains("ATR_MCP_TOKEN"), "{err}");
        assert!(!err.contains("hidden-token"), "{err}");
    }

    #[test]
    fn notification_has_no_reply() {
        let dir = tempfile::tempdir().unwrap();
        let s = shared(dir.path());
        let rt = tokio::runtime::Runtime::new().unwrap();
        let out = rt.block_on(handle_raw(
            &s,
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
        ));
        assert!(out.is_empty());
    }
}
