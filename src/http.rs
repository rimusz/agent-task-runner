use crate::app::{Shared, SubmitOutcome};
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;

pub fn router(shared: Arc<Shared>) -> Router {
    Router::new()
        .route("/", get(dashboard_page))
        .route("/dashboard.json", get(dashboard_json))
        .route("/health", get(health))
        .route("/status", get(status))
        .route("/tasks", post(create_task))
        .route("/tasks/:key", get(get_task))
        .route("/tasks/:key/result", get(get_result))
        .route("/tasks/:key/messages", post(post_message))
        .route("/tasks/:key/cancel", post(cancel))
        .route("/hooks", post(plugin_hook))
        .route("/mcp", post(mcp_post).get(mcp_get))
        .with_state(shared)
}

async fn dashboard_page(State(shared): State<Arc<Shared>>) -> impl IntoResponse {
    crate::dashboard::page(&shared).await
}

async fn dashboard_json(
    State(shared): State<Arc<Shared>>,
    uri: axum::http::Uri,
) -> impl IntoResponse {
    crate::dashboard::json(&shared, uri.query()).await
}

async fn health(State(shared): State<Arc<Shared>>) -> impl IntoResponse {
    let running = shared.inflight.lock().unwrap().len();
    Json(json!({
        "ok": true,
        "running": running,
        "service": "atr",
    }))
}

async fn status(State(shared): State<Arc<Shared>>) -> impl IntoResponse {
    Json(shared.status_json())
}

#[derive(Debug, Deserialize, Default)]
struct TaskIn {
    #[serde(default, alias = "text", alias = "message", alias = "prompt")]
    body: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    key: Option<String>,
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    repo: Option<String>,
    #[serde(default)]
    dir: Option<String>,
    #[serde(default)]
    workspace: Option<String>,
    #[serde(default)]
    output: Option<String>,
    #[serde(default)]
    webhook: Option<String>,
    #[serde(default)]
    patch: Option<String>,
    #[serde(default)]
    issue: Option<i64>,
    #[serde(default)]
    shadow: bool,
    #[serde(default)]
    tests_ok: bool,
    #[serde(default)]
    provider: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct MessageIn {
    #[serde(default, alias = "text", alias = "message")]
    body: Option<String>,
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    repo: Option<String>,
    #[serde(default)]
    issue: Option<i64>,
    #[serde(default)]
    shadow: bool,
    #[serde(default)]
    tests_ok: bool,
}

async fn create_task(
    State(shared): State<Arc<Shared>>,
    Json(payload): Json<TaskIn>,
) -> impl IntoResponse {
    let Some(body) = payload.body.filter(|s| !s.trim().is_empty()) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"ok": false, "error": "missing body"})),
        );
    };
    let cfg = shared.config.read().unwrap().clone();
    if let Err(e) = crate::task::check_prompt_bytes(&body, cfg.max_prompt_bytes) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"ok": false, "error": e.to_string()})),
        );
    }
    let title = payload
        .title
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| crate::task::title_from_prompt(&body));
    let (workspace_kind, repo_path, allow_empty) = match crate::task::resolve_intake_workspace(
        payload.dir.as_deref(),
        payload.repo.as_deref(),
        payload.workspace.as_deref(),
        |r| {
            cfg.resolve_repo_filter(r)
                .map(|e| e.path.display().to_string())
        },
    ) {
        Ok(v) => v,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"ok": false, "error": e.to_string()})),
            )
        }
    };
    let output_kind = match payload.output.as_deref() {
        Some(o) => match crate::task::OutputKind::parse(o) {
            Ok(k) => k,
            Err(e) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({"ok": false, "error": e.to_string()})),
                )
            }
        },
        None => crate::task::OutputKind::Reply,
    };
    let output_spec = payload.webhook.or(payload.patch);
    let key = match payload.key.filter(|s| !s.trim().is_empty()) {
        Some(k) => k,
        None => match crate::task::allocate_key(&shared.db, &cfg.task_key_prefix, &title) {
            Ok(k) => k,
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"ok": false, "error": e.to_string()})),
                )
            }
        },
    };
    drop(cfg);
    let text = format!("# {title}\n\n{body}");
    let hint = payload.provider.clone();
    match shared.submit_with(
        &key,
        &text,
        payload.source.as_deref().unwrap_or("http"),
        repo_path.as_deref(),
        payload.issue,
        payload.shadow,
        payload.tests_ok,
        crate::app::SubmitOpts {
            workspace_kind: Some(workspace_kind.as_str().into()),
            output_kind: Some(output_kind.as_str().into()),
            output_spec,
            source_kind: Some(
                payload
                    .source
                    .as_deref()
                    .and_then(|s| crate::task::SourceKind::parse(s).ok())
                    .unwrap_or(crate::task::SourceKind::Http)
                    .as_str()
                    .into(),
            ),
            title: Some(title.clone()),
            allow_empty_repo: allow_empty,
        },
    ) {
        Ok(outcome) => {
            if let Some(p) = hint.as_deref() {
                let _ = shared.db.set_provider_hint(&key, p);
            }
            match outcome {
                SubmitOutcome::Accepted => (
                    StatusCode::OK,
                    Json(json!({
                        "ok": true,
                        "status": "accepted",
                        "key": key,
                        "workspace": workspace_kind.as_str(),
                        "output": output_kind.as_str(),
                    })),
                ),
                SubmitOutcome::Queued => (
                    StatusCode::ACCEPTED,
                    Json(json!({
                        "ok": true,
                        "status": "queued",
                        "key": key,
                        "workspace": workspace_kind.as_str(),
                        "output": output_kind.as_str(),
                    })),
                ),
            }
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"ok": false, "error": e.to_string()})),
        ),
    }
}

async fn get_task(State(shared): State<Arc<Shared>>, Path(key): Path<String>) -> impl IntoResponse {
    match shared.db.get_task(&key) {
        Ok(Some(t)) => (
            StatusCode::OK,
            Json(json!({
                "ok": true,
                "key": t.key,
                "status": t.status,
                "workspace": t.workspace_kind,
                "output": t.output_kind,
                "source": t.source_kind,
                "repo": t.repo,
                "issue": t.issue,
                "title": t.title,
            })),
        ),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(json!({"ok": false, "error": format!("unknown task {key}")})),
        ),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"ok": false, "error": e.to_string()})),
        ),
    }
}

async fn get_result(
    State(shared): State<Arc<Shared>>,
    Path(key): Path<String>,
) -> impl IntoResponse {
    match shared.db.get_task(&key) {
        Ok(Some(t)) => {
            let mut payload = crate::output::result_payload(&shared, &t);
            payload["ok"] = json!(true);
            (StatusCode::OK, Json(payload))
        }
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(json!({"ok": false, "error": format!("unknown task {key}")})),
        ),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"ok": false, "error": e.to_string()})),
        ),
    }
}

async fn post_message(
    State(shared): State<Arc<Shared>>,
    Path(key): Path<String>,
    Json(payload): Json<MessageIn>,
) -> impl IntoResponse {
    let Some(body) = payload.body.filter(|s| !s.trim().is_empty()) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"ok": false, "error": "missing body"})),
        );
    };
    match shared.submit(
        &key,
        &body,
        payload.source.as_deref().unwrap_or("http"),
        payload.repo.as_deref(),
        payload.issue,
        payload.shadow,
        payload.tests_ok,
    ) {
        Ok(SubmitOutcome::Accepted) => (
            StatusCode::OK,
            Json(json!({"ok": true, "status": "accepted", "key": key})),
        ),
        Ok(SubmitOutcome::Queued) => (
            StatusCode::ACCEPTED,
            Json(json!({"ok": true, "status": "queued", "key": key})),
        ),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"ok": false, "error": e.to_string()})),
        ),
    }
}

#[derive(Debug, Deserialize, Default)]
struct CancelIn {
    #[serde(default)]
    force: bool,
}

async fn cancel(
    State(shared): State<Arc<Shared>>,
    Path(key): Path<String>,
    payload: Option<Json<CancelIn>>,
) -> impl IntoResponse {
    let force = payload.map(|j| j.force).unwrap_or(false);
    match shared.cancel(&key, force) {
        Ok(()) => (StatusCode::OK, Json(json!({"ok": true, "key": key}))),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(json!({"ok": false, "error": e.to_string()})),
        ),
    }
}

async fn plugin_hook(
    State(shared): State<Arc<Shared>>,
    Json(payload): Json<serde_json::Value>,
) -> impl IntoResponse {
    let task = payload
        .get("task")
        .or_else(|| payload.get("task_key"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .unwrap_or_default();
    let kind = payload
        .get("hook_event_name")
        .or_else(|| payload.get("event"))
        .or_else(|| payload.get("type"))
        .and_then(|v| v.as_str())
        .unwrap_or("hook");
    let run_id = if !task.is_empty() {
        shared
            .db
            .open_run_for_task(&task)
            .ok()
            .flatten()
            .or_else(|| {
                shared
                    .db
                    .list_runs(Some(&task))
                    .ok()
                    .and_then(|r| r.into_iter().last().map(|x| x.id))
            })
    } else {
        None
    };
    if let Some(id) = run_id {
        let raw = serde_json::to_string(&payload).unwrap_or_else(|_| "{}".into());
        let _ = shared.db.insert_event(id, kind, &raw);
    }
    if matches!(kind, "Stop" | "StopFailure" | "SubagentStop") && !task.is_empty() {
        let cfg = shared.config.read().unwrap().clone();
        crate::notify::notify_unless_shadow(&cfg.telegram, &format!("hook {kind} {task}"), false);
    }
    (StatusCode::OK, Json(json!({"ok": true})))
}

async fn mcp_get() -> impl IntoResponse {
    (
        StatusCode::METHOD_NOT_ALLOWED,
        [("allow", "POST")],
        "POST JSON-RPC to /mcp",
    )
}

async fn mcp_post(State(shared): State<Arc<Shared>>, headers: HeaderMap, body: Bytes) -> Response {
    let cfg = shared.config.read().unwrap().clone();
    if !cfg.mcp.enabled {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"ok": false, "error": "mcp disabled"})),
        )
            .into_response();
    }
    let origin = headers.get("origin").and_then(|v| v.to_str().ok());
    if !crate::mcp::origin_allowed(origin, &cfg.listen) {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({"ok": false, "error": "origin denied"})),
        )
            .into_response();
    }
    let Some(want) = crate::mcp::read_token(&cfg.mcp) else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({
                "ok": false,
                "error": crate::mcp::token_missing_error(&cfg.mcp),
            })),
        )
            .into_response();
    };
    let got = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(crate::mcp::bearer_token);
    let Some(got) = got else {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"ok": false, "error": "missing bearer token"})),
        )
            .into_response();
    };
    if !crate::mcp::tokens_equal(&got, &want) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"ok": false, "error": "invalid bearer token"})),
        )
            .into_response();
    }
    let raw = String::from_utf8_lossy(&body);
    let replies = crate::mcp::handle_raw(&shared, &raw).await;
    if replies.is_empty() {
        return StatusCode::ACCEPTED.into_response();
    }
    let accept = headers
        .get("accept")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let payload = if replies.len() == 1 {
        replies.into_iter().next().unwrap()
    } else {
        serde_json::Value::Array(replies)
    };
    if accept.contains("text/event-stream") && !accept.contains("application/json") {
        let data = serde_json::to_string(&payload).unwrap_or_else(|_| "{}".into());
        return (
            StatusCode::OK,
            [
                ("content-type", "text/event-stream"),
                ("cache-control", "no-cache"),
            ],
            format!("event: message\ndata: {data}\n\n"),
        )
            .into_response();
    }
    (StatusCode::OK, Json(payload)).into_response()
}

/// GET-only router for an extra `[dashboard] bind` (Tailscale IP).
pub fn dashboard_only(shared: Arc<Shared>) -> Router {
    Router::new()
        .route("/", get(dashboard_page))
        .route("/dashboard.json", get(dashboard_json))
        .route("/health", get(health))
        .with_state(shared)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::load_config;
    use crate::mcp::write_token_file;

    fn shared(dir: &std::path::Path, extra: &str) -> Arc<Shared> {
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
{extra}
[[provider]]
name = "claude"
kind = "claude"
cli = "/bin/true"
"#,
                db = dir.join("s.db").display(),
                root = dir.display(),
                prompt = prompt.display(),
            ),
        )
        .unwrap();
        let cfg = load_config(&cfg_path).unwrap();
        Shared::from_config(cfg_path, cfg).unwrap()
    }

    async fn listen(shared: Arc<Shared>) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, router(shared)).await.ok();
        });
        format!("http://{addr}")
    }

    fn status(err: ureq::Error) -> u16 {
        status_body(err).0
    }

    fn status_body(err: ureq::Error) -> (u16, String) {
        match err {
            ureq::Error::Status(code, resp) => (code, resp.into_string().unwrap_or_default()),
            other => panic!("{other}"),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn mcp_http_auth_origin_and_json() {
        let dir = tempfile::tempdir().unwrap();
        let token_path = dir.path().join("mcp.token");
        write_token_file(&token_path, "http-mcp-test-token").unwrap();
        let extra = format!(
            r#"
[mcp]
enabled = true
token_env = "ATR_MCP_HTTP_TEST_UNSET"
token_file = "{token}"
"#,
            token = token_path.display()
        );
        let s = shared(dir.path(), &extra);
        let base = listen(s).await;
        let url = format!("{base}/mcp");
        let get = ureq::get(&url).call().unwrap_err();
        assert_eq!(status(get), 405);
        let missing = ureq::post(&url)
            .set("Content-Type", "application/json")
            .send_string(r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#)
            .unwrap_err();
        assert_eq!(status(missing), 401);
        let bad = ureq::post(&url)
            .set("Authorization", "Bearer wrong-token")
            .set("Content-Type", "application/json")
            .send_string(r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#)
            .unwrap_err();
        assert_eq!(status(bad), 401);
        let origin = ureq::post(&url)
            .set("Authorization", "Bearer http-mcp-test-token")
            .set("Origin", "https://evil.example")
            .set("Content-Type", "application/json")
            .send_string(r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#)
            .unwrap_err();
        assert_eq!(status(origin), 403);
        let ok = ureq::post(&url)
            .set("Authorization", "Bearer http-mcp-test-token")
            .set("Accept", "application/json")
            .set("Content-Type", "application/json")
            .send_string(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#)
            .unwrap();
        assert_eq!(ok.status(), 200);
        let body = ok.into_string().unwrap();
        assert!(body.contains("\"atr\""), "{body}");
        assert!(!body.contains("http-mcp-test-token"), "{body}");
        let note = ureq::post(&url)
            .set("Authorization", "Bearer http-mcp-test-token")
            .set("Content-Type", "application/json")
            .send_string(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#)
            .unwrap();
        assert_eq!(note.status(), 202);
        let sse = ureq::post(&url)
            .set("Authorization", "Bearer http-mcp-test-token")
            .set("Accept", "text/event-stream")
            .set("Content-Type", "application/json")
            .send_string(r#"{"jsonrpc":"2.0","id":2,"method":"ping"}"#)
            .unwrap();
        assert_eq!(sse.status(), 200);
        assert!(sse
            .header("content-type")
            .unwrap_or("")
            .contains("text/event-stream"));
        let sse_body = sse.into_string().unwrap();
        assert!(sse_body.contains("event: message"), "{sse_body}");
        assert!(!sse_body.contains("http-mcp-test-token"), "{sse_body}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn mcp_http_disabled_is_404_and_no_token_is_503() {
        let dir = tempfile::tempdir().unwrap();
        let off = shared(
            dir.path(),
            r#"
[mcp]
enabled = false
token_env = "ATR_MCP_HTTP_TEST_UNSET"
token_file = "/tmp/atr-mcp-missing.token"
"#,
        );
        let base = listen(off).await;
        let disabled = ureq::post(&format!("{base}/mcp"))
            .set("Authorization", "Bearer x")
            .set("Content-Type", "application/json")
            .send_string(r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#)
            .unwrap_err();
        assert_eq!(status(disabled), 404);

        let dir2 = tempfile::tempdir().unwrap();
        let on = shared(
            dir2.path(),
            r#"
[mcp]
enabled = true
token_env = "ATR_MCP_HTTP_TEST_UNSET"
token_file = "/tmp/atr-mcp-missing.token"
"#,
        );
        let base = listen(on).await;
        let missing = ureq::post(&format!("{base}/mcp"))
            .set("Authorization", "Bearer x")
            .set("Content-Type", "application/json")
            .send_string(r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#)
            .unwrap_err();
        let (code, body) = status_body(missing);
        assert_eq!(code, 503);
        assert!(body.contains("mcp token not configured"), "{body}");
        assert!(body.contains("atr init"), "{body}");
        assert!(
            body.contains("token file") || body.contains("mcp.token"),
            "{body}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn dashboard_bind_has_no_mcp() {
        let dir = tempfile::tempdir().unwrap();
        let s = shared(dir.path(), "");
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, dashboard_only(s)).await.ok();
        });
        let err = ureq::post(&format!("http://{addr}/mcp"))
            .send_string("{}")
            .unwrap_err();
        assert_eq!(status(err), 404);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn create_task_without_issue() {
        let dir = tempfile::tempdir().unwrap();
        let s = shared(dir.path(), "");
        let base = listen(s.clone()).await;
        let resp = ureq::post(&format!("{base}/tasks"))
            .set("Content-Type", "application/json")
            .send_string(r#"{"body":"what is 2+2?","workspace":"none","output":"text"}"#)
            .unwrap();
        assert_eq!(resp.status(), 200);
        let body = resp.into_string().unwrap();
        assert!(body.contains("\"ok\":true"), "{body}");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        let key = v["key"].as_str().unwrap();
        let t = s.db.get_task(key).unwrap().unwrap();
        assert_eq!(t.issue, None);
        assert_eq!(t.workspace_kind, "none");
        let got = ureq::get(&format!("{base}/tasks/{key}/result"))
            .call()
            .unwrap()
            .into_string()
            .unwrap();
        assert!(got.contains(key), "{got}");

        let notes = dir.path().join("notes");
        std::fs::create_dir_all(&notes).unwrap();
        let resp = ureq::post(&format!("{base}/tasks"))
            .set("Content-Type", "application/json")
            .send_string(&format!(
                r#"{{"body":"fix the typo","dir":"{}"}}"#,
                notes.display()
            ))
            .unwrap();
        assert_eq!(resp.status(), 200);
        let v: serde_json::Value = serde_json::from_str(&resp.into_string().unwrap()).unwrap();
        let t = s.db.get_task(v["key"].as_str().unwrap()).unwrap().unwrap();
        assert_eq!(t.workspace_kind, "dir");
        assert_eq!(t.issue, None);
        assert_eq!(t.source_kind, "http");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn create_task_repo_plus_workspace_dir() {
        let dir = tempfile::tempdir().unwrap();
        let s = shared(dir.path(), "");
        let base = listen(s.clone()).await;
        let notes = dir.path().join("notes");
        std::fs::create_dir_all(&notes).unwrap();
        let resp = ureq::post(&format!("{base}/tasks"))
            .set("Content-Type", "application/json")
            .send_string(&format!(
                r#"{{"body":"fix the typo","repo":"{}","workspace":"dir"}}"#,
                notes.display()
            ))
            .unwrap();
        assert_eq!(resp.status(), 200);
        let v: serde_json::Value = serde_json::from_str(&resp.into_string().unwrap()).unwrap();
        let t = s.db.get_task(v["key"].as_str().unwrap()).unwrap().unwrap();
        assert_eq!(t.workspace_kind, "dir");
        let repo = t.repo.unwrap();
        assert!(
            repo.contains("notes") || std::path::Path::new(&repo) == notes.as_path(),
            "{repo}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn create_task_rejects_oversized_prompt() {
        let dir = tempfile::tempdir().unwrap();
        let s = shared(dir.path(), r#"max_prompt_bytes = "32""#);
        let base = listen(s).await;
        let err = ureq::post(&format!("{base}/tasks"))
            .set("Content-Type", "application/json")
            .send_string(r#"{"body":"this prompt is definitely over thirty two bytes"}"#)
            .unwrap_err();
        let (code, body) = status_body(err);
        assert_eq!(code, 400);
        assert!(body.contains("bytes"), "{body}");
    }
}
