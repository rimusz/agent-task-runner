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
        .route("/tasks/:key", get(get_task))
        .route("/tasks/:key/result", get(get_result))
        .route("/mcp", post(mcp_post).get(mcp_get))
        .merge(write_routes(shared.clone()))
        .with_state(shared)
}

/// State-changing routes: same bearer token and Origin check as `/mcp`.
fn write_routes(shared: Arc<Shared>) -> Router<Arc<Shared>> {
    Router::new()
        .route("/tasks", post(create_task))
        .route("/tasks/:key/messages", post(post_message))
        .route("/tasks/:key/cancel", post(cancel))
        .route("/tasks/:key/retry", post(retry))
        .route("/intake/pause", post(intake_pause))
        .route("/intake/resume", post(intake_resume))
        .route("/hooks", post(plugin_hook))
        .route_layer(axum::middleware::from_fn_with_state(
            shared,
            require_write_auth,
        ))
}

/// Max request body read (and discarded) before an auth denial.
const DENY_DRAIN_LIMIT: usize = 64 * 1024;

async fn require_write_auth(
    State(shared): State<Arc<Shared>>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let cfg = shared.config.read().unwrap().clone();
    if let Some(deny) = auth_failure(&cfg, req.headers()) {
        // Read the (small) request body before replying. Closing a socket
        // with unread request bytes makes the kernel send RST, and the client
        // can lose the response body (seen as a 503 with an empty body).
        let _ = axum::body::to_bytes(req.into_body(), DENY_DRAIN_LIMIT).await;
        return deny;
    }
    next.run(req).await
}

fn deny(code: StatusCode, error: String) -> Option<Response> {
    Some((code, Json(json!({"ok": false, "error": error}))).into_response())
}

/// 401 with the RFC 6750 challenge. No `resource_metadata`, so static-token
/// clients are not steered into OAuth discovery.
fn unauthorized(error: &str) -> Option<Response> {
    let body = Json(json!({"ok": false, "error": error}));
    Some(
        (
            StatusCode::UNAUTHORIZED,
            [("www-authenticate", r#"Bearer realm="atr""#)],
            body,
        )
            .into_response(),
    )
}

/// Origin + bearer check shared by `/mcp` and the write routes.
/// 403 foreign browser Origin, 503 no token configured, 401 missing/bad token.
fn auth_failure(cfg: &crate::config::Config, headers: &HeaderMap) -> Option<Response> {
    let origin = headers.get("origin").and_then(|v| v.to_str().ok());
    if !crate::mcp::origin_allowed(origin, &cfg.listen) {
        return deny(StatusCode::FORBIDDEN, "origin denied".into());
    }
    let Some(want) = crate::mcp::read_token(&cfg.mcp) else {
        return deny(
            StatusCode::SERVICE_UNAVAILABLE,
            crate::mcp::token_missing_error(&cfg.mcp),
        );
    };
    let got = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(crate::mcp::bearer_token);
    let Some(got) = got else {
        return unauthorized("missing bearer token");
    };
    if !crate::mcp::tokens_equal(&got, &want) {
        return unauthorized("invalid bearer token");
    }
    None
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
    /// Required capabilities: `"browser"`, `"desktop"`, or a list.
    #[serde(default, deserialize_with = "crate::task::de_needs")]
    needs: Vec<String>,
    /// Optional; the `Idempotency-Key` header wins when both are set.
    #[serde(default, alias = "idempotencyKey")]
    idempotency_key: Option<String>,
    #[serde(default, alias = "attachments")]
    files: Vec<crate::intake::FileIn>,
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
    headers: HeaderMap,
    Json(payload): Json<TaskIn>,
) -> impl IntoResponse {
    let Some(body) = payload.body.filter(|s| !s.trim().is_empty()) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"ok": false, "error": "missing body"})),
        );
    };
    let cfg = shared.config.read().unwrap().clone();
    let files = match crate::intake::parse_files(payload.files, cfg.max_prompt_bytes) {
        Ok(f) => f,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"ok": false, "error": e})),
            )
        }
    };
    let header_key = headers.get("idempotency-key").and_then(|v| v.to_str().ok());
    let idempotency_key =
        crate::intake::pick_idempotency_key(header_key, payload.idempotency_key.as_deref());
    let source = payload.source.clone();
    match crate::intake::create(
        &shared,
        crate::intake::CreateIn {
            body,
            title: payload.title,
            key: payload.key,
            repo: payload.repo,
            dir: payload.dir,
            workspace: payload.workspace,
            output: payload.output,
            webhook: payload.webhook,
            patch: payload.patch,
            provider: payload.provider,
            needs: payload.needs,
            source: source.clone().unwrap_or_else(|| "http".into()),
            source_kind: source
                .as_deref()
                .and_then(|s| crate::task::SourceKind::parse(s).ok())
                .unwrap_or(crate::task::SourceKind::Http)
                .as_str()
                .into(),
            issue: payload.issue,
            shadow: payload.shadow,
            tests_ok: payload.tests_ok,
            files,
            idempotency_key,
            repo_named_prefix: false,
        },
    ) {
        Ok(out) => {
            let code = if out.status == "queued" {
                StatusCode::ACCEPTED
            } else {
                StatusCode::OK
            };
            (code, Json(out.to_json()))
        }
        Err(crate::intake::CreateError::BadRequest(e)) => (
            StatusCode::BAD_REQUEST,
            Json(json!({"ok": false, "error": e})),
        ),
        Err(crate::intake::CreateError::Conflict(e)) => {
            (StatusCode::CONFLICT, Json(json!({"ok": false, "error": e})))
        }
        Err(crate::intake::CreateError::Internal(e)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"ok": false, "error": e.to_string()})),
        ),
    }
}

async fn get_task(State(shared): State<Arc<Shared>>, Path(key): Path<String>) -> impl IntoResponse {
    match shared.db.get_task(&key) {
        Ok(Some(t)) => {
            let cfg = shared.config.read().unwrap().clone();
            let files = crate::intake::list_attachment_names(&cfg.state_db, &t.key);
            let mut body = json!({
                "ok": true,
                "key": t.key,
                "status": t.status,
                "workspace": t.workspace_kind,
                "output": t.output_kind,
                "source": t.source_kind,
                "repo": t.repo,
                "issue": t.issue,
                "title": t.title,
            });
            if !files.is_empty() {
                body["files"] = json!(files);
            }
            (StatusCode::OK, Json(body))
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

async fn retry(State(shared): State<Arc<Shared>>, Path(key): Path<String>) -> impl IntoResponse {
    match shared.retry(&key) {
        Ok(()) => (StatusCode::OK, Json(json!({"ok": true, "key": key}))),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(json!({"ok": false, "error": e.to_string()})),
        ),
    }
}

async fn intake_pause(State(shared): State<Arc<Shared>>) -> impl IntoResponse {
    shared.set_intake_paused(true);
    Json(json!({"ok": true, "intake_paused": true}))
}

async fn intake_resume(State(shared): State<Arc<Shared>>) -> impl IntoResponse {
    shared.set_intake_paused(false);
    Json(json!({"ok": true, "intake_paused": false}))
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
    if matches!(kind, "Stop" | "StopFailure" | "SubagentStop")
        && !task.is_empty()
        && !crate::telegram::is_assistant_key(&task)
    {
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
    if let Some(deny) = auth_failure(&cfg, &headers) {
        return deny;
    }
    let raw = String::from_utf8_lossy(&body);
    let Some(payload) = crate::mcp::handle_message(&shared, &raw).await else {
        return StatusCode::ACCEPTED.into_response();
    };
    let accept = headers
        .get("accept")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
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
        .route("/tasks/:key/result", get(get_result))
        .with_state(shared)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::load_config;
    use crate::mcp::write_token_file;

    const TEST_TOKEN: &str = "http-write-test-token";

    /// Test config; adds an `[mcp]` token file unless `extra` sets its own.
    fn shared(dir: &std::path::Path, extra: &str) -> Arc<Shared> {
        let tok = dir.join("write.token");
        crate::mcp::write_token_file(&tok, TEST_TOKEN).unwrap();
        let owned;
        let extra = if extra.contains("[mcp]") {
            extra
        } else {
            owned = format!(
                "{extra}\n[mcp]\nenabled = true\ntoken_env = \"ATR_HTTP_WRITE_TEST_UNSET\"\ntoken_file = \"{}\"\n",
                tok.display()
            );
            owned.as_str()
        };
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
            ureq::Error::Status(code, resp) => {
                (code, resp.into_string().expect("read response body"))
            }
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
    async fn mcp_http_streamable_compliance_for_remote_clients() {
        let dir = tempfile::tempdir().unwrap();
        let token_path = dir.path().join("mcp.token");
        write_token_file(&token_path, "http-mcp-compat-token").unwrap();
        let extra = format!(
            "\n[mcp]\nenabled = true\ntoken_env = \"ATR_MCP_HTTP_TEST_UNSET\"\ntoken_file = \"{}\"\n",
            token_path.display()
        );
        let s = shared(dir.path(), &extra);
        let base = listen(s).await;
        let url = format!("{base}/mcp");
        let post = |body: &str| {
            ureq::post(&url)
                .set("Authorization", "Bearer http-mcp-compat-token")
                .set("Accept", "application/json, text/event-stream")
                .set("Content-Type", "application/json")
                .send_string(body)
                .unwrap()
        };
        // 401 carries a Bearer challenge without OAuth resource metadata.
        let missing = ureq::post(&url)
            .set("Content-Type", "application/json")
            .send_string(r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#)
            .unwrap_err();
        let ureq::Error::Status(401, resp) = missing else {
            panic!("expected 401");
        };
        let challenge = resp.header("www-authenticate").unwrap_or("").to_string();
        assert!(challenge.starts_with("Bearer"), "{challenge}");
        assert!(!challenge.contains("resource_metadata"), "{challenge}");
        // A batch is answered with an array, even with a single reply left.
        let batch = post(
            r#"[{"jsonrpc":"2.0","id":7,"method":"ping"},{"jsonrpc":"2.0","method":"notifications/initialized"}]"#,
        );
        assert_eq!(batch.status(), 200);
        let v: serde_json::Value = serde_json::from_str(&batch.into_string().unwrap()).unwrap();
        assert_eq!(v.as_array().map(|a| a.len()), Some(1), "{v}");
        assert_eq!(v[0]["id"], 7);
        // A client's JSON-RPC response needs no reply: 202, no body.
        let reply = post(r#"{"jsonrpc":"2.0","id":9,"result":{}}"#);
        assert_eq!(reply.status(), 202);
        let only = post(r#"[{"jsonrpc":"2.0","id":9,"error":{"code":-1,"message":"x"}}]"#);
        assert_eq!(only.status(), 202);
        // An empty batch is an invalid request.
        let empty = post("[]");
        let v: serde_json::Value = serde_json::from_str(&empty.into_string().unwrap()).unwrap();
        assert_eq!(v["error"]["code"], -32600, "{v}");
        // No OAuth discovery documents: a static-token client must not be pushed into OAuth.
        for path in [
            "/.well-known/oauth-protected-resource",
            "/.well-known/oauth-authorization-server",
        ] {
            let err = ureq::get(&format!("{base}{path}")).call().unwrap_err();
            assert_eq!(status(err), 404, "{path}");
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn mcp_http_disabled_is_404_and_no_token_is_503() {
        let dir = tempfile::tempdir().unwrap();
        let off = shared(
            dir.path(),
            &format!(
                "\n[mcp]\nenabled = false\ntoken_env = \"ATR_MCP_HTTP_TEST_UNSET\"\ntoken_file = \"{}\"\n",
                dir.path().join("missing.token").display()
            ),
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
            &format!(
                "\n[mcp]\nenabled = true\ntoken_env = \"ATR_MCP_HTTP_TEST_UNSET\"\ntoken_file = \"{}\"\n",
                dir2.path().join("missing.token").display()
            ),
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
        let err = ureq::post(&format!("http://{addr}/tasks"))
            .send_string("{}")
            .unwrap_err();
        assert_eq!(status(err), 404, "dashboard bind stays read-only");
        let err = ureq::get(&format!("http://{addr}/tasks/nope/result"))
            .call()
            .unwrap_err();
        assert_eq!(status(err), 404, "result view exists but task is unknown");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn intake_pause_resume_and_retry_routes() {
        let dir = tempfile::tempdir().unwrap();
        let s = shared(dir.path(), "");
        let base = listen(s.clone()).await;
        let post = |path: &str| -> (u16, String) {
            match ureq::post(&format!("{base}/{path}"))
                .set("Authorization", &format!("Bearer {TEST_TOKEN}"))
                .set("Content-Type", "application/json")
                .send_string("{}")
            {
                Ok(r) => (r.status(), r.into_string().unwrap_or_default()),
                Err(e) => status_body(e),
            }
        };
        assert_eq!(post("intake/pause").0, 200);
        assert!(s.intake_paused.load(std::sync::atomic::Ordering::Relaxed));
        assert_eq!(
            s.db.meta_get("intake_paused").unwrap().as_deref(),
            Some("1")
        );
        assert_eq!(post("intake/resume").0, 200);
        assert!(!s.intake_paused.load(std::sync::atomic::Ordering::Relaxed));
        let (code, body) = post("tasks/nope-1/retry");
        assert_eq!(code, 400);
        assert!(body.contains("unknown task"), "{body}");
        s.db.upsert_task("retry-1", "/repo", None).unwrap();
        s.db.set_status("retry-1", "failed").unwrap();
        assert_eq!(post("tasks/retry-1/retry").0, 200);
        assert_eq!(s.db.get_task("retry-1").unwrap().unwrap().status, "queued");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn create_task_without_issue() {
        let dir = tempfile::tempdir().unwrap();
        let s = shared(dir.path(), "");
        let base = listen(s.clone()).await;
        let resp = ureq::post(&format!("{base}/tasks"))
            .set("Authorization", &format!("Bearer {TEST_TOKEN}"))
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
            .set("Authorization", &format!("Bearer {TEST_TOKEN}"))
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
            .set("Authorization", &format!("Bearer {TEST_TOKEN}"))
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
            .set("Authorization", &format!("Bearer {TEST_TOKEN}"))
            .set("Content-Type", "application/json")
            .send_string(r#"{"body":"this prompt is definitely over thirty two bytes"}"#)
            .unwrap_err();
        let (code, body) = status_body(err);
        assert_eq!(code, 400);
        assert!(body.contains("bytes"), "{body}");
    }
    #[allow(clippy::result_large_err)]
    fn post_json(url: &str, body: &str) -> Result<ureq::Response, ureq::Error> {
        ureq::post(url)
            .set("Authorization", &format!("Bearer {TEST_TOKEN}"))
            .set("Content-Type", "application/json")
            .send_string(body)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn create_task_rejects_bad_input() {
        let dir = tempfile::tempdir().unwrap();
        let s = shared(dir.path(), "");
        let base = listen(s.clone()).await;
        let url = format!("{base}/tasks");
        for (body, want) in [
            (r#"{"body":"   "}"#, "missing body"),
            (r#"{"title":"no body"}"#, "missing body"),
            (r#"{"body":"x","output":"carrier-pigeon"}"#, ""),
            (r#"{"body":"x","dir":"/"}"#, "refusing"),
            (r#"{"body":"x","dir":"/definitely/not/here"}"#, ""),
        ] {
            let (code, text) = status_body(post_json(&url, body).unwrap_err());
            assert_eq!(code, 400, "{body} -> {text}");
            assert!(text.contains(want), "{body} -> {text}");
        }
        assert!(s.db.list_tasks().unwrap().is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn task_lifecycle_endpoints() {
        let dir = tempfile::tempdir().unwrap();
        let s = shared(dir.path(), "");
        let base = listen(s.clone()).await;
        assert_eq!(
            ureq::get(&format!("{base}/health"))
                .call()
                .unwrap()
                .status(),
            200
        );
        assert_eq!(
            ureq::get(&format!("{base}/status"))
                .call()
                .unwrap()
                .status(),
            200
        );

        let resp = post_json(
            &format!("{base}/tasks"),
            r#"{"key":"http-life","title":"Life","body":"answer me","workspace":"none","output":"text","provider":"claude"}"#,
        )
        .unwrap();
        let v: serde_json::Value = serde_json::from_str(&resp.into_string().unwrap()).unwrap();
        assert_eq!(v["key"], "http-life");
        assert_eq!(v["output"], "text");
        let t = s.db.get_task("http-life").unwrap().unwrap();
        assert_eq!(t.provider_hint, "claude");
        assert_eq!(t.needs, "");
        post_json(
            &format!("{base}/tasks"),
            r#"{"key":"http-web","body":"browse","workspace":"none","needs":"browser"}"#,
        )
        .unwrap();
        assert_eq!(s.db.get_task("http-web").unwrap().unwrap().needs, "browser");
        assert_eq!(t.title, "Life");

        let got: serde_json::Value = serde_json::from_str(
            &ureq::get(&format!("{base}/tasks/http-life"))
                .call()
                .unwrap()
                .into_string()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(got["status"], "queued");
        assert_eq!(got["workspace"], "none");

        for path in ["tasks/nope", "tasks/nope/result"] {
            let err = ureq::get(&format!("{base}/{path}")).call().unwrap_err();
            assert_eq!(status(err), 404, "{path}");
        }

        let (code, text) =
            status_body(post_json(&format!("{base}/tasks/http-life/messages"), "{}").unwrap_err());
        assert_eq!(code, 400, "{text}");
        let ok = post_json(
            &format!("{base}/tasks/http-life/messages"),
            r#"{"text":"one more thing"}"#,
        )
        .unwrap();
        assert_eq!(ok.status(), 200);
        let msgs = s.db.all_messages("http-life").unwrap();
        assert!(msgs.len() >= 2, "{msgs:?}");

        let c = post_json(&format!("{base}/tasks/http-life/cancel"), "{}").unwrap();
        assert_eq!(c.status(), 200);
        let t = s.db.get_task("http-life").unwrap().unwrap();
        assert_ne!(t.status, "queued");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn create_task_idempotency_key_replays_and_conflicts() {
        let dir = tempfile::tempdir().unwrap();
        let s = shared(dir.path(), "");
        let base = listen(s.clone()).await;
        let url = format!("{base}/tasks");
        let body = r#"{"body":"what is 2+2?","workspace":"none","output":"text"}"#;
        let first = ureq::post(&url)
            .set("Authorization", &format!("Bearer {TEST_TOKEN}"))
            .set("Content-Type", "application/json")
            .set("Idempotency-Key", "http-idem-1")
            .send_string(body)
            .unwrap();
        assert_eq!(first.status(), 200);
        let a: serde_json::Value = serde_json::from_str(&first.into_string().unwrap()).unwrap();
        let key = a["key"].as_str().unwrap().to_string();
        assert_ne!(a["replayed"], true);

        let second = ureq::post(&url)
            .set("Authorization", &format!("Bearer {TEST_TOKEN}"))
            .set("Content-Type", "application/json")
            .set("Idempotency-Key", "http-idem-1")
            .send_string(body)
            .unwrap();
        assert_eq!(second.status(), 200);
        let b: serde_json::Value = serde_json::from_str(&second.into_string().unwrap()).unwrap();
        assert_eq!(b["key"], key);
        assert_eq!(b["replayed"], true);
        assert_eq!(s.db.list_tasks().unwrap().len(), 1);
        assert_eq!(s.db.all_messages(&key).unwrap().len(), 1);

        let (code, text) = status_body(
            ureq::post(&url)
                .set("Authorization", &format!("Bearer {TEST_TOKEN}"))
                .set("Content-Type", "application/json")
                .set("Idempotency-Key", "http-idem-1")
                .send_string(r#"{"body":"a different prompt","workspace":"none","output":"text"}"#)
                .unwrap_err(),
        );
        assert_eq!(code, 409);
        assert!(text.contains("different request"), "{text}");
        assert_eq!(s.db.list_tasks().unwrap().len(), 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn create_task_files_land_in_worktree_by_name() {
        let dir = tempfile::tempdir().unwrap();
        let s = shared(dir.path(), "");
        let base = listen(s.clone()).await;
        let resp = ureq::post(&format!("{base}/tasks"))
            .set("Authorization", &format!("Bearer {TEST_TOKEN}"))
            .set("Content-Type", "application/json")
            .send_string(
                r#"{"body":"read brief.md","workspace":"none","output":"text","files":[{"name":"brief.md","text":"hello attach"}]}"#,
            )
            .unwrap();
        assert_eq!(resp.status(), 200);
        let v: serde_json::Value = serde_json::from_str(&resp.into_string().unwrap()).unwrap();
        let key = v["key"].as_str().unwrap();
        let cfg = s.config.read().unwrap().clone();
        let got = ureq::get(&format!("{base}/tasks/{key}"))
            .call()
            .unwrap()
            .into_string()
            .unwrap();
        assert!(got.contains("brief.md"), "{got}");
        let wt = crate::worktree::ensure_none_workspace(&cfg.worktree_root, key).unwrap();
        crate::intake::materialize_attachments(&cfg.state_db, key, &wt).unwrap();
        assert_eq!(
            std::fs::read_to_string(wt.join("brief.md")).unwrap(),
            "hello attach"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn plugin_hook_records_event_on_latest_run() {
        let dir = tempfile::tempdir().unwrap();
        let s = shared(dir.path(), "");
        let base = listen(s.clone()).await;
        s.db.upsert_task("hook-1", "", None).unwrap();
        let run = s.db.start_run("hook-1", "claude", "m").unwrap();
        post_json(
            &format!("{base}/hooks"),
            r#"{"task":"hook-1","hook_event_name":"PostToolUse","tool":"Edit"}"#,
        )
        .unwrap();
        // No task: accepted and ignored.
        post_json(&format!("{base}/hooks"), r#"{"event":"Stop"}"#).unwrap();
        let ev = s.db.events_for_run(run).unwrap();
        assert_eq!(ev.len(), 1, "{ev:?}");
        assert!(ev[0].raw.contains("PostToolUse"));
    }

    const WRITE_ROUTES: [&str; 7] = [
        "tasks",
        "tasks/auth-1/messages",
        "tasks/auth-1/cancel",
        "tasks/auth-1/retry",
        "intake/pause",
        "intake/resume",
        "hooks",
    ];

    #[tokio::test(flavor = "multi_thread")]
    async fn write_routes_need_bearer_token() {
        let dir = tempfile::tempdir().unwrap();
        let s = shared(dir.path(), "");
        let base = listen(s.clone()).await;
        post_json(
            &format!("{base}/tasks"),
            r#"{"key":"auth-1","body":"hi","workspace":"none","output":"text"}"#,
        )
        .unwrap();
        for path in WRITE_ROUTES {
            let url = format!("{base}/{path}");
            let missing = ureq::post(&url)
                .set("Content-Type", "application/json")
                .send_string("{}")
                .unwrap_err();
            assert_eq!(status(missing), 401, "{path} missing");
            let bad = ureq::post(&url)
                .set("Authorization", "Bearer wrong")
                .set("Content-Type", "application/json")
                .send_string("{}")
                .unwrap_err();
            assert_eq!(status(bad), 401, "{path} bad");
            let origin = ureq::post(&url)
                .set("Authorization", &format!("Bearer {TEST_TOKEN}"))
                .set("Origin", "https://evil.example")
                .set("Content-Type", "application/json")
                .send_string("{}")
                .unwrap_err();
            assert_eq!(status(origin), 403, "{path} origin");
        }
        let ok = [
            (
                "tasks",
                r#"{"key":"auth-2","body":"x","workspace":"none","output":"text"}"#,
            ),
            ("tasks/auth-1/messages", r#"{"text":"more"}"#),
            ("hooks", r#"{"task":"auth-1","event":"PostToolUse"}"#),
            ("tasks/auth-1/cancel", "{}"),
        ];
        for (path, body) in ok {
            let r = post_json(&format!("{base}/{path}"), body).unwrap();
            assert_eq!(r.status(), 200, "{path}");
        }
        // Reads stay open.
        for path in ["", "dashboard.json", "health", "status", "tasks/auth-1"] {
            let r = ureq::get(&format!("{base}/{path}")).call().unwrap();
            assert_eq!(r.status(), 200, "{path}");
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn write_routes_503_without_token() {
        let dir = tempfile::tempdir().unwrap();
        // Missing token file inside this test's own temp dir (no shared /tmp path).
        let cfg = format!(
            "\n[mcp]\nenabled = false\ntoken_env = \"ATR_HTTP_WRITE_TEST_UNSET\"\ntoken_file = \"{}\"\n",
            dir.path().join("missing.token").display()
        );
        let s = shared(dir.path(), &cfg);
        let base = listen(s).await;
        for path in WRITE_ROUTES {
            let err = ureq::post(&format!("{base}/{path}"))
                .set("Authorization", "Bearer x")
                .set("Content-Type", "application/json")
                .send_string("{}")
                .unwrap_err();
            let (code, body) = status_body(err);
            assert_eq!(code, 503, "{path}");
            assert!(body.contains("token not configured"), "{body}");
        }
    }

    fn mcp_cfg(token_path: &std::path::Path, env: &str) -> String {
        format!(
            r#"
[mcp]
enabled = true
token_env = "{env}"
token_file = "{token}"
"#,
            token = token_path.display()
        )
    }

    #[allow(clippy::result_large_err)]
    fn rpc(url: &str, auth: Option<&str>, body: &str) -> Result<ureq::Response, ureq::Error> {
        let mut req = ureq::post(url)
            .set("Content-Type", "application/json")
            .set("Accept", "application/json");
        if let Some(a) = auth {
            req = req.set("Authorization", a);
        }
        req.send_string(body)
    }

    fn tool_json(resp: ureq::Response) -> serde_json::Value {
        let v: serde_json::Value = serde_json::from_str(&resp.into_string().unwrap()).unwrap();
        assert_eq!(v["result"]["isError"], false, "{v}");
        let text = v["result"]["content"][0]["text"].as_str().unwrap();
        serde_json::from_str(text).unwrap()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn mcp_http_tools_over_bearer_token() {
        let dir = tempfile::tempdir().unwrap();
        let token_path = dir.path().join("mcp.token");
        let token = crate::mcp::generate_token();
        write_token_file(&token_path, &token).unwrap();
        let s = shared(
            dir.path(),
            &mcp_cfg(&token_path, "ATR_MCP_HTTP_TOOLS_TEST_UNSET"),
        );
        let url = format!("{}/mcp", listen(s.clone()).await);
        let auth = format!("Bearer {token}");

        let listed = rpc(
            &url,
            Some(&auth),
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#,
        )
        .unwrap();
        assert_eq!(listed.status(), 200);
        let body = listed.into_string().unwrap();
        assert!(!body.contains(&token), "token leaked in tools/list");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["result"]["tools"].as_array().unwrap().len(), 12, "{v}");

        let empty = tool_json(
            rpc(
                &url,
                Some(&auth),
                r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"list_tasks","arguments":{}}}"#,
            )
            .unwrap(),
        );
        assert_eq!(empty["tasks"].as_array().unwrap().len(), 0, "{empty}");

        let added = tool_json(
            rpc(
                &url,
                Some(&auth),
                r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"add_task","arguments":{"title":"Sum","body":"what is 2+2?","workspace":"none","output":"text"}}}"#,
            )
            .unwrap(),
        );
        let key = added["key"].as_str().unwrap().to_string();
        assert_eq!(added["workspace"], "none", "{added}");
        let t = s.db.get_task(&key).unwrap().unwrap();
        assert_eq!(t.source_kind, "mcp");

        let st = tool_json(
            rpc(
                &url,
                Some(&auth),
                &format!(
                    r#"{{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{{"name":"task_status","arguments":{{"key":"{key}"}}}}}}"#
                ),
            )
            .unwrap(),
        );
        assert_eq!(st["key"], key.as_str(), "{st}");

        let cfg = rpc(
            &url,
            Some(&auth),
            r#"{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"config_get","arguments":{}}}"#,
        )
        .unwrap()
        .into_string()
        .unwrap();
        assert!(
            !cfg.contains(&token),
            "config_get must not reveal the token"
        );

        // Tool calls are refused without the token.
        let denied = rpc(
            &url,
            None,
            r#"{"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"list_tasks","arguments":{}}}"#,
        )
        .unwrap_err();
        assert_eq!(status(denied), 401);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn mcp_http_token_rotation_without_restart() {
        let dir = tempfile::tempdir().unwrap();
        let token_path = dir.path().join("mcp.token");
        write_token_file(&token_path, "old-rotation-token").unwrap();
        let s = shared(
            dir.path(),
            &mcp_cfg(&token_path, "ATR_MCP_HTTP_ROTATE_TEST_UNSET"),
        );
        let url = format!("{}/mcp", listen(s).await);
        let ping = r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#;
        assert_eq!(
            rpc(&url, Some("Bearer old-rotation-token"), ping)
                .unwrap()
                .status(),
            200
        );
        write_token_file(&token_path, "new-rotation-token").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&token_path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        let old = rpc(&url, Some("Bearer old-rotation-token"), ping).unwrap_err();
        assert_eq!(
            status(old),
            401,
            "old token must be rejected after rotation"
        );
        assert_eq!(
            rpc(&url, Some("Bearer new-rotation-token"), ping)
                .unwrap()
                .status(),
            200
        );
        // Emptying the file disables HTTP MCP (503), it does not open it.
        std::fs::write(&token_path, "\n").unwrap();
        let empty = rpc(&url, Some("Bearer new-rotation-token"), ping).unwrap_err();
        assert_eq!(status(empty), 503);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn mcp_http_env_token_wins_and_bad_headers_401() {
        let dir = tempfile::tempdir().unwrap();
        let token_path = dir.path().join("mcp.token");
        write_token_file(&token_path, "file-token").unwrap();
        let env = "ATR_MCP_HTTP_ENV_PRECEDENCE_TEST";
        std::env::set_var(env, "env-token");
        let s = shared(dir.path(), &mcp_cfg(&token_path, env));
        let url = format!("{}/mcp", listen(s).await);
        let ping = r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#;
        assert_eq!(
            rpc(&url, Some("Bearer env-token"), ping).unwrap().status(),
            200
        );
        let file = rpc(&url, Some("Bearer file-token"), ping).unwrap_err();
        assert_eq!(status(file), 401, "env token takes precedence over file");
        for bad in [
            "env-token",
            "Token env-token",
            "Bearer",
            "Bearer ",
            "Bearer env-token-extra",
            "Bearer env-toke",
        ] {
            let e = rpc(&url, Some(bad), ping).unwrap_err();
            assert_eq!(status(e), 401, "header {bad:?} must be 401");
        }
        std::env::remove_var(env);
    }

    #[test]
    fn write_token_file_tightens_existing_mode() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("sub/mcp.token");
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, "loose\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();
        }
        write_token_file(&p, "  tight  ").unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "tight\n");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        let a = crate::mcp::generate_token();
        let b = crate::mcp::generate_token();
        assert_ne!(a, b);
        assert!(a.len() >= 32, "token too short");
    }
}
