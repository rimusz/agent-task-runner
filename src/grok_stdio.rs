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
    hook_env: &[(String, String)],
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
        .env("ATR_PLUGIN_TEST", test_command)
        .envs(hook_env.iter().map(|(k, v)| (k.as_str(), v.as_str())));
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
    use super::run;
    use crate::db::Db;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    fn setup(body: &str) -> (tempfile::TempDir, crate::config::Provider, Db, i64) {
        let dir = tempfile::tempdir().unwrap();
        let cli = dir.path().join("grok");
        std::fs::write(
            &cli,
            body.replace("@LOG@", &dir.path().join("argv").display().to_string()),
        )
        .unwrap();
        std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(dir.path().join("p.md"), "p\n").unwrap();
        let text = format!(
            r#"
listen = "127.0.0.1:0"
state_db = "{root}/s.db"
worktree_root = "{root}"
repo_path = "{root}"
system_prompt_file = "{root}/p.md"
[[provider]]
name = "grok"
kind = "grok"
cli = "{cli}"
model = "grok-x"
plugin = true
"#,
            root = dir.path().display(),
            cli = cli.display()
        );
        let raw: crate::config::RawConfig = toml::from_str(&text).unwrap();
        let cfg = crate::config::Config::from_raw(raw).unwrap();
        let p = cfg.providers[0].clone();
        let db = Db::open(&dir.path().join("state.db")).unwrap();
        db.upsert_task("g-1", "", None).unwrap();
        let run_id = db.start_run("g-1", "grok", "grok-x").unwrap();
        (dir, p, db, run_id)
    }

    async fn go(
        dir: &tempfile::TempDir,
        p: &crate::config::Provider,
        db: &Db,
        run_id: i64,
    ) -> crate::runner::RunResult {
        run(
            db,
            p,
            dir.path(),
            &dir.path().join("plugins"),
            "do it",
            None,
            run_id,
            "g-1",
            "http://127.0.0.1:1/hook",
            &[],
            "true",
            Arc::new(Mutex::new(None)),
            Duration::from_secs(60),
        )
        .await
        .unwrap()
    }

    const OK: &str = r#"#!/bin/sh
echo "$*" > '@LOG@'
read a; read b; read c
echo '{"jsonrpc":"2.0","id":1,"result":{}}'
echo '{"jsonrpc":"2.0","id":2,"result":{"sessionId":"sess-42"}}'
echo 'not json'
echo '{"jsonrpc":"2.0","method":"session/update","params":{"text":"hi"}}'
echo '{"jsonrpc":"2.0","id":3,"result":{"stopReason":"end_turn","_meta":{"totalTokens":42}}}'
"#;

    #[tokio::test]
    async fn stdio_success_records_session_usage_and_args() {
        let (dir, p, db, run_id) = setup(OK);
        let r = go(&dir, &p, &db, run_id).await;
        assert_eq!(r.outcome, "success");
        assert_eq!(r.session_id.as_deref(), Some("sess-42"));
        assert_eq!(r.usage.input, 42);
        assert_eq!(r.snippet, "end_turn");
        let argv = std::fs::read_to_string(dir.path().join("argv")).unwrap();
        assert!(argv.starts_with("agent --plugin-dir "), "{argv}");
        assert!(argv.contains("--model grok-x"), "{argv}");
        assert!(argv.contains("--always-approve stdio"), "{argv}");
        assert!(!argv.contains(" -p "), "{argv}");
    }

    #[tokio::test]
    async fn stdio_prompt_error_is_not_success() {
        let body = r#"#!/bin/sh
read a; read b; read c
echo '{"jsonrpc":"2.0","id":3,"error":{"code":-32000,"message":"boom"}}'
"#;
        let (dir, p, db, run_id) = setup(body);
        let r = go(&dir, &p, &db, run_id).await;
        assert_ne!(r.outcome, "success");
        assert!(r.snippet.contains("boom"), "{}", r.snippet);
    }

    #[tokio::test]
    async fn stdio_nonzero_exit_without_result_fails() {
        let body = "#!/bin/sh\nread a\nexit 3\n";
        let (dir, p, db, run_id) = setup(body);
        let r = go(&dir, &p, &db, run_id).await;
        assert_eq!(r.outcome, "failed");
        assert_eq!(r.exit_code, Some(3));
    }
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
