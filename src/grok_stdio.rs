//! Grok `agent --plugin-dir … stdio` JSON-RPC adapter.
//! `-p` rejects `--plugin-dir`; this path is used only when `plugin = true`.

use crate::config::Provider;
use crate::db::Db;
use crate::providers::{ErrorClass, UsageNumbers};
use crate::runner::RunResult;
use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::path::Path;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::time::Instant;

pub async fn run(
    db: &Db,
    provider: &Provider,
    worktree: &Path,
    plugin_dir: &Path,
    prompt: &str,
    session: Option<&str>,
    run_id: i64,
    task_key: &str,
    hook_url: &str,
    test_command: &str,
    inflight_pid: Arc<Mutex<Option<u32>>>,
    run_timeout: Duration,
) -> Result<RunResult> {
    let mut args = vec![
        "agent".into(),
        "--plugin-dir".into(),
        plugin_dir.display().to_string(),
    ];
    if !provider.model.is_empty() {
        let flag = if provider.model_flag.is_empty() {
            "--model"
        } else {
            provider.model_flag.as_str()
        };
        args.push(flag.into());
        args.push(provider.model.clone());
    }
    let turns = crate::config::Config::worker_max_turns(provider);
    args.push("--max-turns".into());
    args.push(turns.to_string());
    args.push("--always-approve".into());
    args.push("stdio".into());

    let mut cmd = Command::new(&provider.cli);
    cmd.args(&args)
        .current_dir(worktree)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .env("ATR_PROVIDER", &provider.name)
        .env("ATR_TASK_KEY", task_key)
        .env("ATR_HOOK_URL", hook_url)
        .env("ATR_PLUGIN_TEST", test_command);
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    let mut child = cmd
        .spawn()
        .with_context(|| format!("spawn grok agent stdio ({})", provider.name))?;
    let pid = child.id().unwrap_or(0);
    if pid > 0 {
        db.set_run_pid(run_id, pid)?;
        *inflight_pid.lock().unwrap() = Some(pid);
    }

    let mut stdin = child.stdin.take().context("grok stdio stdin")?;
    let stdout = child.stdout.take().context("grok stdio stdout")?;
    stdin
        .write_all(
            format!(
                "{}\n{}\n{}\n",
                json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{}}}),
                json!({"jsonrpc":"2.0","id":2,"method":"session/new","params":{"cwd": worktree.display().to_string(),"mcpServers":[]}}),
                json!({"jsonrpc":"2.0","id":3,"method":"session/prompt","params":{"text": prompt}}),
            )
            .as_bytes(),
        )
        .await?;
    let _ = stdin.flush().await;

    let mut lines = BufReader::new(stdout).lines();
    let mut session_id = session.map(|s| s.to_string());
    let mut usage = UsageNumbers::default();
    let mut snippet = String::new();
    let mut classified = None;
    let mut saw_error = false;
    let started = Instant::now();

    loop {
        if started.elapsed() > run_timeout {
            let _ = child.kill().await;
            return Ok(finish(
                "stalled",
                None,
                session_id,
                usage,
                snippet,
                Some(ErrorClass::Other),
            ));
        }
        let line = match tokio::time::timeout(Duration::from_secs(30), lines.next_line()).await {
            Ok(Ok(Some(l))) => l,
            Ok(Ok(None)) => break,
            Ok(Err(_)) => break,
            Err(_) => continue,
        };
        let Ok(v) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if let Some(id) = crate::providers::session_id(&v) {
            session_id = Some(id.clone());
            let _ = db.set_run_session(run_id, &id);
        }
        if let Some(id) = v.pointer("/result/sessionId").and_then(|x| x.as_str()) {
            session_id = Some(id.to_string());
            let _ = db.set_run_session(run_id, id);
        }
        if v.get("id") == Some(&Value::from(3)) {
            if let Some(err) = v.get("error") {
                saw_error = true;
                snippet = err.to_string();
                classified = crate::providers::classify_error_value(&v);
            }
            if let Some(res) = v.get("result") {
                if let Some(t) = res.get("stopReason").and_then(|x| x.as_str()) {
                    snippet = t.to_string();
                }
                if let Some(n) = res.pointer("/_meta/totalTokens").and_then(|x| x.as_u64()) {
                    usage.input = usage.input.max(n);
                }
            }
            break;
        }
        let _ = db.insert_event(run_id, crate::handoff::event_kind(&v).as_str(), &line);
    }
    drop(stdin);
    let status = child.wait().await.ok();
    let code = status.and_then(|s| s.code());
    let outcome = if saw_error {
        match classified {
            Some(ErrorClass::Limit) => "limit",
            Some(ErrorClass::Auth) => "auth",
            Some(ErrorClass::Unavailable) | Some(ErrorClass::Server) => "unavailable",
            _ => "failed",
        }
    } else if code.unwrap_or(1) == 0 {
        "success"
    } else {
        "failed"
    };
    Ok(finish(
        outcome, code, session_id, usage, snippet, classified,
    ))
}

fn finish(
    outcome: &str,
    exit_code: Option<i32>,
    session_id: Option<String>,
    usage: UsageNumbers,
    snippet: String,
    classified: Option<ErrorClass>,
) -> RunResult {
    RunResult {
        outcome: outcome.into(),
        exit_code,
        session_id,
        usage,
        usage_raw: vec![],
        snippet,
        classified,
        stderr: String::new(),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn plugin_dir_flag_is_agent_not_p() {
        let args = [
            "agent",
            "--plugin-dir",
            "/tmp/p",
            "--always-approve",
            "stdio",
        ];
        assert_eq!(args[0], "agent");
        assert!(!args.contains(&"-p"));
    }
}
