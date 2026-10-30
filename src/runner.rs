use crate::config::{Config, Provider};
use crate::db::Db;
use crate::providers::{
    cache_hit_ratio, classify_error_value, parse_blob, parse_line, result_needs_attention,
    session_id, CommandSpec, ErrorClass, ParsedEvent, UsageNumbers,
};
use anyhow::{Context, Result};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::time::{sleep, Instant};

#[derive(Debug, Clone)]
pub struct RunResult {
    pub outcome: String,
    pub exit_code: Option<i32>,
    pub session_id: Option<String>,
    pub usage: UsageNumbers,
    pub usage_raw: Vec<serde_json::Value>,
    pub snippet: String,
    pub classified: Option<ErrorClass>,
    pub stderr: String,
}

pub fn kill_group(pid: u32, sig: i32) {
    unsafe {
        libc::kill(-(pid as i32), sig);
    }
}

/// Snapshot of `root` plus every descendant, walking `ps` parent links.
/// cursor-agent (and similar) put shell tools in their own process groups,
/// so `killpg` on the CLI session leaves those children behind.
pub fn descendant_pids(root: u32) -> Vec<u32> {
    let output = match std::process::Command::new("ps")
        .args(["-axo", "pid=,ppid="])
        .output()
    {
        Ok(o) if o.status.success() => o.stdout,
        _ => return Vec::new(),
    };
    let text = String::from_utf8_lossy(&output);
    let mut pairs = Vec::new();
    for line in text.lines() {
        let mut it = line.split_whitespace();
        let Some(pid) = it.next().and_then(|s| s.parse::<u32>().ok()) else {
            continue;
        };
        let Some(ppid) = it.next().and_then(|s| s.parse::<u32>().ok()) else {
            continue;
        };
        pairs.push((pid, ppid));
    }
    let mut kids = Vec::new();
    let mut stack = vec![root];
    let mut seen = HashSet::new();
    seen.insert(root);
    while let Some(cur) = stack.pop() {
        for (pid, ppid) in &pairs {
            if *ppid == cur && seen.insert(*pid) {
                kids.push(*pid);
                stack.push(*pid);
            }
        }
    }
    kids
}

fn apply_kill(root: u32, tree: &[u32], sig: i32) {
    for &pid in tree.iter().rev() {
        unsafe {
            libc::kill(-(pid as i32), sig);
            libc::kill(pid as i32, sig);
        }
    }
    kill_group(root, sig);
    unsafe {
        libc::kill(root as i32, sig);
    }
}

/// Kill the CLI session and every descendant, including those that called `setsid`.
pub fn kill_process_tree(root: u32, sig: i32) {
    if root == 0 {
        return;
    }
    let tree = descendant_pids(root);
    apply_kill(root, &tree, sig);
}

/// Force-cancel: TERM then KILL the snapshotted tree so orphans cannot reparent.
pub fn terminate_tree_now(root: u32) {
    if root == 0 {
        return;
    }
    let tree = descendant_pids(root);
    apply_kill(root, &tree, libc::SIGTERM);
    apply_kill(root, &tree, libc::SIGKILL);
}

/// Prompt/system files for a run. Dir workspaces keep these in the state
/// directory so atr does not create `.atr/` inside the user's folder.
pub fn runtime_files_dir(
    state_db: &Path,
    key: &str,
    worktree: &Path,
    dir_workspace: bool,
) -> std::path::PathBuf {
    if dir_workspace {
        let root = state_db.parent().unwrap_or_else(|| Path::new("."));
        root.join("prompts").join(key)
    } else {
        worktree.join(".atr")
    }
}

pub async fn run_cli(
    db: &Db,
    cfg: &Config,
    provider: &Provider,
    worktree: &Path,
    prompt: &str,
    session: Option<&str>,
    new_session: bool,
    run_id: i64,
    turn_id: i64,
    task_key: &str,
    inflight_pid: Arc<Mutex<Option<u32>>>,
    path_prepend: Option<&Path>,
    fork_from: Option<&str>,
    system_prompt: Option<&Path>,
    dir_workspace: bool,
    files_dir: &Path,
) -> Result<RunResult> {
    if provider.kind == "grok"
        && provider.plugin
        && new_session
        && fork_from.is_none()
        && !cfg.dry_run
    {
        let dir = crate::plugin::ensure_plugin(&cfg.state_db, &cfg.plugin.test_command)?;
        let hook = format!("http://{}/hooks", cfg.listen.trim());
        return crate::grok_stdio::run(
            db,
            provider,
            worktree,
            &dir,
            prompt,
            session,
            run_id,
            task_key,
            &hook,
            &crate::mcp::hook_env(&cfg.mcp),
            &cfg.plugin.test_command,
            inflight_pid,
            provider.run_timeout,
        )
        .await;
    }

    std::fs::create_dir_all(files_dir)?;
    let prompt_path = files_dir.join("prompt.md");
    std::fs::write(&prompt_path, prompt)?;

    let mut spec = crate::providers::build_command_opts(
        provider,
        worktree,
        &prompt_path,
        prompt,
        session,
        new_session,
        fork_from,
        system_prompt,
    );
    if dir_workspace {
        crate::providers::apply_dir_confinement(provider, &mut spec, worktree);
    }
    tracing::info!(
        provider = %provider.name,
        program = %spec.program.display(),
        args = ?display_args(&spec),
        "starting CLI"
    );

    if cfg.dry_run {
        return Ok(RunResult {
            outcome: "success".into(),
            exit_code: Some(0),
            session_id: session.map(|s| s.to_string()),
            usage: UsageNumbers::default(),
            usage_raw: vec![],
            snippet: "dry-run".into(),
            classified: None,
            stderr: String::new(),
        });
    }

    let mut cmd = Command::new(&spec.program);
    cmd.args(&spec.args)
        .current_dir(&spec.cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .env("ATR_PROVIDER", &provider.name);
    for (k, v) in &spec.env {
        cmd.env(k, v);
    }
    if let Some(extra) = path_prepend {
        let mut path = extra.display().to_string();
        if let Ok(old) = std::env::var("PATH") {
            path.push(':');
            path.push_str(&old);
        }
        cmd.env("PATH", path);
        cmd.env("ATR_SHADOW", "1");
        cmd.env(
            "ATR_SHADOW_GH_LOG",
            worktree.join(".atr").join("gh.log").display().to_string(),
        );
        cmd.env_remove("GH_TOKEN");
        cmd.env_remove("GITHUB_TOKEN");
        if !cfg.shadow.gh_token_env.is_empty() {
            cmd.env_remove(&cfg.shadow.gh_token_env);
        }
        if !cfg.github.cli.is_empty() {
            match crate::shadow::resolve_github_cli(&cfg.github.cli) {
                Ok(real) => {
                    cmd.env("ATR_SHADOW_GH_REAL", real.display().to_string());
                }
                Err(e) => {
                    tracing::error!("shadow gh resolve: {e:#}");
                    anyhow::bail!("{e:#}");
                }
            }
        }
    }
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    let mut child = crate::util::retry_exec_busy(|| cmd.spawn())
        .with_context(|| format!("spawn {} ({})", spec.program.display(), provider.name))?;
    let pid = child.id().unwrap_or(0);
    let mut seen_descendants = HashSet::new();
    if pid > 0 {
        db.set_run_pid(run_id, pid)?;
        *inflight_pid.lock().unwrap() = Some(pid);
        note_descendants(pid, &mut seen_descendants);
    }

    if let Some(text) = spec.stdin_text {
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(text.as_bytes()).await;
        }
    } else {
        drop(child.stdin.take());
    }

    let stdout = child.stdout.take().context("cli stdout")?;
    let stderr = child.stderr.take().context("cli stderr")?;
    let err_buf = Arc::new(Mutex::new(String::new()));
    let err_clone = err_buf.clone();
    tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if let Ok(mut buf) = err_clone.lock() {
                buf.push_str(&line);
                buf.push('\n');
            }
        }
    });

    let mut lines = BufReader::new(stdout).lines();
    let mut usage = UsageNumbers::default();
    let mut usage_raw = Vec::new();
    let mut session_id_found = session.map(|s| s.to_string());
    let mut snippet = String::new();
    let mut classified = None;
    let mut saw_error_event = false;
    let mut stdout_blob = String::new();

    let heartbeat = if provider.health.heartbeat_timeout > Duration::ZERO {
        provider.health.heartbeat_timeout
    } else {
        provider.heartbeat_timeout
    };
    let startup = if provider.health.startup_timeout > Duration::ZERO {
        provider.health.startup_timeout
    } else {
        heartbeat
    };
    let run_cap = provider.run_timeout;
    let started = Instant::now();
    let mut saw_output = false;
    let mut next_beat = Instant::now() + startup;

    loop {
        let until_beat = next_beat.saturating_duration_since(Instant::now());
        let until_cap = run_cap.saturating_sub(started.elapsed());
        if until_cap.is_zero() {
            tracing::warn!(provider = %provider.name, run_id, "run wall-clock cap exceeded");
            note_descendants(pid, &mut seen_descendants);
            terminate(pid, cfg.kill_grace).await;
            let _ = child.wait().await;
            reap_leftovers(&seen_descendants, cfg.kill_grace);
            return Ok(finish(
                "stalled",
                None,
                session_id_found,
                usage,
                usage_raw,
                snippet,
                Some(ErrorClass::Other),
                String::new(),
            ));
        }
        tokio::select! {
            line = lines.next_line() => {
                match line {
                    Ok(Some(line)) => {
                        saw_output = true;
                        next_beat = Instant::now() + heartbeat;
                        let _ = db.touch_run(run_id);
                        note_descendants(pid, &mut seen_descendants);
                        stdout_blob.push_str(&line);
                        stdout_blob.push('\n');
                        let _ = crate::reported::store_claude_line(
                            db,
                            provider,
                            &line,
                            &crate::clock::Clock::system(),
                        );
                        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) {
                            if crate::handoff::should_store_event(&v) {
                                let _ = db.insert_event(
                                    run_id,
                                    &crate::handoff::event_kind(&v),
                                    &line,
                                );
                            }
                            if let Some(id) = session_id(&v) {
                                session_id_found = Some(id.clone());
                                let _ = db.set_run_session(run_id, &id);
                                let _ = db.set_provider_session(
                                    task_key,
                                    Some(&provider.name),
                                    Some(&id),
                                );
                            }
                        }
                        if let Some(ev) = parse_line(&provider.kind, &line) {
                            apply_event(
                                &provider.kind,
                                ev,
                                &mut usage,
                                &mut usage_raw,
                                &mut session_id_found,
                                &mut snippet,
                                &mut classified,
                                &mut saw_error_event,
                                db,
                                run_id,
                                task_key,
                                &provider.name,
                            )?;
                        }
                    }
                    Ok(None) => break,
                    Err(e) => {
                        tracing::warn!("stdout read: {e}");
                        break;
                    }
                }
            }
            _ = sleep(until_beat) => {
                let (outcome, class) = if !saw_output {
                    tracing::warn!(
                        provider = %provider.name,
                        run_id,
                        "startup timeout; no first output"
                    );
                    ("unavailable", ErrorClass::Unavailable)
                } else {
                    tracing::warn!(
                        provider = %provider.name,
                        run_id,
                        "heartbeat timeout; killing process group"
                    );
                    ("stalled", ErrorClass::Other)
                };
                note_descendants(pid, &mut seen_descendants);
                terminate(pid, cfg.kill_grace).await;
                let _ = child.wait().await;
                reap_leftovers(&seen_descendants, cfg.kill_grace);
                return Ok(finish(
                    outcome,
                    None,
                    session_id_found,
                    usage,
                    usage_raw,
                    snippet,
                    Some(class),
                    String::new(),
                ));
            }
        }
    }

    note_descendants(pid, &mut seen_descendants);
    let status = child.wait().await?;
    reap_leftovers(&seen_descendants, cfg.kill_grace);
    let code = status.code();
    let stderr_text = err_buf.lock().unwrap().clone();
    if usage.input == 0 && usage.output == 0 && usage.cache_read == 0 {
        for ev in parse_blob(&provider.kind, &stdout_blob) {
            apply_event(
                &provider.kind,
                ev,
                &mut usage,
                &mut usage_raw,
                &mut session_id_found,
                &mut snippet,
                &mut classified,
                &mut saw_error_event,
                db,
                run_id,
                task_key,
                &provider.name,
            )?;
        }
    }
    if db.events_for_run(run_id).unwrap_or_default().is_empty() && !stdout_blob.trim().is_empty() {
        let _ = db.insert_event(run_id, "stdout", stdout_blob.trim());
    }
    let _ = crate::reported::store_claude_transcript(
        db,
        provider,
        &stdout_blob,
        &crate::clock::Clock::system(),
    );
    // Limit/auth only from structured error events, and only when the CLI
    // failed or emitted an error/result event that says so.
    if !saw_error_event && status.success() {
        classified = None;
    }
    let blocked = usage_raw.iter().any(result_needs_attention);
    let outcome = if blocked {
        "needs_attention"
    } else if status.success() && !saw_error_event {
        "success"
    } else {
        match classified {
            Some(ErrorClass::Limit) => "limit",
            Some(ErrorClass::Auth) => "auth",
            Some(ErrorClass::Unavailable) => "unavailable",
            _ => "failed",
        }
    };

    if outcome != "success" {
        tracing::warn!(
            provider = %provider.name,
            task = %task_key,
            exit = ?code,
            outcome,
            stderr = %truncate(&stderr_text, 2000),
            last_output = %last_lines(&stdout_blob, 12),
            "CLI run failed"
        );
    }

    if let Some(sid) = &session_id_found {
        let _ = db.set_run_session(run_id, sid);
        let _ = db.set_provider_session(task_key, Some(&provider.name), Some(sid));
        if provider.kind == "codex" && provider.model.trim().is_empty() {
            // `codex exec --json` never names the model; its rollout does.
            let home = codex_home();
            let sid_owned = sid.clone();
            if let Ok(Some(model)) =
                tokio::task::spawn_blocking(move || codex_model_from_rollout(&home?, &sid_owned))
                    .await
            {
                let _ = db.set_run_model(run_id, &model);
            }
        }
        if provider.kind == "grok" {
            if let Some(u) = read_grok_usage(&provider.cli, sid).await {
                usage = u.clone();
                usage_raw.push(serde_json::json!({
                    "source": "grok usage",
                    "sessionId": sid,
                    "usage": {
                        "inputTokens": u.input + u.cache_read,
                        "cachedReadTokens": u.cache_read,
                        "cacheCreationTokens": u.cache_write,
                        "outputTokens": u.output,
                        "cost_usd": u.cost_usd,
                    }
                }));
            }
        }
    }
    let _ = turn_id;

    Ok(finish(
        outcome,
        code,
        session_id_found,
        usage,
        usage_raw,
        snippet,
        classified,
        stderr_text,
    ))
}

fn codex_home() -> Option<PathBuf> {
    if let Some(h) = std::env::var_os("CODEX_HOME").filter(|h| !h.is_empty()) {
        return Some(PathBuf::from(h));
    }
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".codex"))
}

/// Model from the first `turn_context` in Codex's rollout file for `sid`
/// (`<home>/sessions/YYYY/MM/DD/rollout-<ts>-<sid>.jsonl`). Newest dirs first.
pub fn codex_model_from_rollout(home: &Path, sid: &str) -> Option<String> {
    if sid.is_empty() || sid.contains('/') {
        return None;
    }
    let suffix = format!("-{sid}.jsonl");
    let file = find_newest(&home.join("sessions"), &suffix, 4)?;
    let text = std::fs::read_to_string(file).ok()?;
    for line in text.lines() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let model = match v.get("type").and_then(|t| t.as_str()) {
            Some("turn_context") => v.pointer("/payload/model"),
            Some("event_msg") => v.pointer("/payload/thread_settings/model"),
            _ => None,
        };
        if let Some(m) = model.and_then(|m| m.as_str()).filter(|m| !m.is_empty()) {
            return Some(m.to_string());
        }
    }
    None
}

fn find_newest(dir: &Path, suffix: &str, depth: u32) -> Option<PathBuf> {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
        .ok()?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .collect();
    entries.sort();
    entries.reverse();
    for p in entries {
        if p.is_dir() {
            if depth > 0 {
                if let Some(f) = find_newest(&p, suffix, depth - 1) {
                    return Some(f);
                }
            }
        } else if p
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.ends_with(suffix))
        {
            return Some(p);
        }
    }
    None
}

async fn read_grok_usage(cli: &Path, sid: &str) -> Option<UsageNumbers> {
    let out = tokio::process::Command::new(cli)
        .args(["usage", sid])
        .output()
        .await
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    crate::providers::parse_grok_usage_cli(&v)
}

fn apply_event(
    kind: &str,
    ev: ParsedEvent,
    usage: &mut UsageNumbers,
    usage_raw: &mut Vec<serde_json::Value>,
    session_id_found: &mut Option<String>,
    snippet: &mut String,
    classified: &mut Option<ErrorClass>,
    saw_error_event: &mut bool,
    db: &Db,
    run_id: i64,
    task_key: &str,
    provider: &str,
) -> Result<()> {
    match ev {
        ParsedEvent::Heartbeat => {}
        ParsedEvent::Session(id) => {
            *session_id_found = Some(id.clone());
            let _ = db.set_run_session(run_id, &id);
            let _ = db.set_provider_session(task_key, Some(provider), Some(&id));
        }
        ParsedEvent::Usage {
            usage: u,
            raw,
            result_text,
            is_error,
        } => {
            if let Some(id) = session_id(&raw) {
                *session_id_found = Some(id.clone());
                let _ = db.set_run_session(run_id, &id);
                let _ = db.set_provider_session(task_key, Some(provider), Some(&id));
            }
            if is_error {
                *saw_error_event = true;
                if classified.is_none() {
                    *classified = classify_error_value(&raw);
                }
            }
            usage.merge(&u);
            usage_raw.push(raw);
            if let Some(t) = result_text {
                *snippet = truncate(&t, 400);
            }
            if let Some(ratio) = cache_hit_ratio(kind, usage) {
                tracing::info!(
                    kind,
                    hit = format!("{:.1}%", ratio * 100.0),
                    "cache-hit ratio"
                );
            }
        }
        ParsedEvent::Failed(msg) => {
            *saw_error_event = true;
            *snippet = truncate(&msg, 400);
            if classified.is_none() {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&format!(
                    r#"{{"type":"turn.failed","error":{{"message":{}}}}}"#,
                    serde_json::Value::String(msg.clone())
                )) {
                    *classified = classify_error_value(&v);
                }
            }
        }
    }
    Ok(())
}

fn finish(
    outcome: &str,
    exit_code: Option<i32>,
    session_id: Option<String>,
    usage: UsageNumbers,
    usage_raw: Vec<serde_json::Value>,
    snippet: String,
    classified: Option<ErrorClass>,
    stderr: String,
) -> RunResult {
    RunResult {
        outcome: outcome.into(),
        exit_code,
        session_id,
        usage,
        usage_raw,
        snippet,
        classified,
        stderr,
    }
}

async fn terminate(pid: u32, grace: Duration) {
    if pid == 0 {
        return;
    }
    let tree = descendant_pids(pid);
    apply_kill(pid, &tree, libc::SIGTERM);
    let start = Instant::now();
    while start.elapsed() < grace {
        if tree_dead(pid, &tree) {
            return;
        }
        sleep(Duration::from_millis(40)).await;
    }
    if !tree_dead(pid, &tree) {
        apply_kill(pid, &tree, libc::SIGKILL);
    }
}

fn tree_dead(root: u32, tree: &[u32]) -> bool {
    !crate::db::pid_alive(i64::from(root))
        && tree.iter().all(|p| !crate::db::pid_alive(i64::from(*p)))
}

fn note_descendants(root: u32, into: &mut HashSet<u32>) {
    if root == 0 {
        return;
    }
    for p in descendant_pids(root) {
        into.insert(p);
    }
}

fn reap_leftovers(pids: &HashSet<u32>, grace: Duration) {
    let alive: Vec<u32> = pids
        .iter()
        .copied()
        .filter(|&p| p > 0 && crate::db::pid_alive(i64::from(p)))
        .collect();
    if alive.is_empty() {
        return;
    }
    tracing::info!(n = alive.len(), "reaping leftover descendant processes");
    reap_leftovers_now(&alive, grace);
}

/// TERM then KILL leftover pids (and their groups) after the CLI has exited.
pub fn reap_leftovers_now(pids: &[u32], grace: Duration) {
    for &p in pids {
        if p == 0 || !crate::db::pid_alive(i64::from(p)) {
            continue;
        }
        unsafe {
            libc::kill(-(p as i32), libc::SIGTERM);
            libc::kill(p as i32, libc::SIGTERM);
        }
    }
    let start = std::time::Instant::now();
    while start.elapsed() < grace {
        if pids
            .iter()
            .all(|p| *p == 0 || !crate::db::pid_alive(i64::from(*p)))
        {
            return;
        }
        std::thread::sleep(Duration::from_millis(40));
    }
    for &p in pids {
        if p > 0 && crate::db::pid_alive(i64::from(p)) {
            unsafe {
                libc::kill(-(p as i32), libc::SIGKILL);
                libc::kill(p as i32, libc::SIGKILL);
            }
        }
    }
}

fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n {
        s.to_string()
    } else {
        format!("{}…", &s[..n])
    }
}

fn last_lines(s: &str, n: usize) -> String {
    let mut lines: Vec<&str> = s.lines().collect();
    if lines.len() > n {
        lines = lines.split_off(lines.len() - n);
    }
    lines.join("\n")
}

fn display_args(spec: &CommandSpec) -> Vec<String> {
    spec.args
        .iter()
        .map(|a| {
            if a.len() > 80 {
                format!("{}…", &a[..80])
            } else {
                a.clone()
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Command as StdCommand, Stdio as StdStdio};

    #[test]
    fn kill_process_tree_reaches_setsid_grandchild() {
        let dir = tempfile::tempdir().unwrap();
        let parent_file = dir.path().join("parent.pid");
        let orphan_file = dir.path().join("orphan.pid");
        let script = format!(
            r#"
import os, sys, time
open({parent:?}, "w").write(str(os.getpid()) + "\n")
pid = os.fork()
if pid == 0:
    os.setsid()
    open({orphan:?}, "w").write(str(os.getpid()) + "\n")
    time.sleep(600)
    os._exit(0)
time.sleep(600)
"#,
            parent = parent_file,
            orphan = orphan_file
        );
        let mut child = StdCommand::new("python3")
            .arg("-c")
            .arg(script)
            .stdin(StdStdio::null())
            .stdout(StdStdio::null())
            .stderr(StdStdio::null())
            .spawn()
            .expect("spawn python tree");
        let parent_pid = child.id();
        let start = std::time::Instant::now();
        let orphan = loop {
            if let Ok(text) = std::fs::read_to_string(&orphan_file) {
                if let Ok(pid) = text.trim().parse::<u32>() {
                    break pid;
                }
            }
            if start.elapsed() > Duration::from_secs(3) {
                let _ = child.kill();
                panic!("orphan pid file not written");
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        assert!(
            crate::db::pid_alive(i64::from(orphan)),
            "setsid grandchild should be alive before kill"
        );
        terminate_tree_now(parent_pid);
        let _ = child.wait();
        std::thread::sleep(Duration::from_millis(80));
        assert!(
            !crate::db::pid_alive(i64::from(parent_pid)),
            "parent should be dead"
        );
        assert!(
            !crate::db::pid_alive(i64::from(orphan)),
            "setsid grandchild {orphan} must not be left under init"
        );
    }

    #[test]
    fn reap_leftovers_after_parent_exits() {
        let dir = tempfile::tempdir().unwrap();
        let orphan_file = dir.path().join("orphan.pid");
        let script = format!(
            r#"
import os, time
pid = os.fork()
if pid == 0:
    os.setsid()
    open({orphan:?}, "w").write(str(os.getpid()) + "\n")
    time.sleep(600)
    os._exit(0)
os._exit(0)
"#,
            orphan = orphan_file
        );
        let status = StdCommand::new("python3")
            .arg("-c")
            .arg(script)
            .stdin(StdStdio::null())
            .stdout(StdStdio::null())
            .stderr(StdStdio::null())
            .status()
            .expect("spawn python orphan");
        assert!(status.success(), "parent should exit 0");
        let start = std::time::Instant::now();
        let orphan = loop {
            if let Ok(text) = std::fs::read_to_string(&orphan_file) {
                if let Ok(pid) = text.trim().parse::<u32>() {
                    break pid;
                }
            }
            if start.elapsed() > Duration::from_secs(3) {
                panic!("orphan pid file not written");
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        assert!(
            crate::db::pid_alive(i64::from(orphan)),
            "setsid child must survive a normal parent exit"
        );
        reap_leftovers_now(&[orphan], Duration::from_millis(200));
        std::thread::sleep(Duration::from_millis(80));
        assert!(
            !crate::db::pid_alive(i64::from(orphan)),
            "leftover {orphan} must be reaped after a normal run exit"
        );
    }

    #[test]
    fn dir_workspace_prompt_lives_in_state_dir() {
        let state = Path::new("/var/lib/atr/state.db");
        let wt = Path::new("/Users/me/notes");
        let dir = runtime_files_dir(state, "task-notes", wt, true);
        assert_eq!(dir, Path::new("/var/lib/atr/prompts/task-notes"));
        let git = runtime_files_dir(state, "task-git", wt, false);
        assert_eq!(git, Path::new("/Users/me/notes/.atr"));
    }
}

#[cfg(test)]
mod codex_model_tests {
    use super::*;

    #[test]
    fn codex_model_read_from_rollout_turn_context() {
        let home = tempfile::tempdir().unwrap();
        let day = home.path().join("sessions/2026/10/15");
        std::fs::create_dir_all(&day).unwrap();
        let sid = "01a13eb0-56b8-7781-8407-3b0cd30dfd82";
        std::fs::write(
            day.join(format!("rollout-2026-10-15T11-31-46-{sid}.jsonl")),
            concat!(
                r#"{"type":"session_meta","payload":{"id":"x"}}"#,
                "\n",
                r#"{"type":"turn_context","payload":{"turn_id":"t","model":"gpt-6.1-sol"}}"#,
                "\n"
            ),
        )
        .unwrap();
        std::fs::write(day.join("rollout-other-zzz.jsonl"), "{}\n").unwrap();
        assert_eq!(
            codex_model_from_rollout(home.path(), sid).as_deref(),
            Some("gpt-6.1-sol")
        );
        assert_eq!(codex_model_from_rollout(home.path(), "missing"), None);
        assert_eq!(codex_model_from_rollout(home.path(), "../x"), None);
    }
}
