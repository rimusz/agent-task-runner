use crate::config::{Config, Provider};
use crate::db::Db;
use crate::providers::{
    cache_hit_ratio, classify_error_value, parse_blob, parse_line, result_needs_attention,
    session_id, CommandSpec, ErrorClass, ParsedEvent, UsageNumbers,
};
use anyhow::{Context, Result};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};
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

/// One row from `ps` (pid/ppid plus the group and session the kill walk needs).
#[derive(Debug, Clone, Copy)]
struct ProcRow {
    pid: u32,
    ppid: u32,
    pgid: u32,
    sid: u32,
}

/// Identity of the task's process group/session, captured *before* any kill.
/// After `setsid`, a live CLI has `pid == pgid == sid`. When the leader is
/// already gone, children keep that sid, so we still sweep `pgid`/`sid == root`.
#[derive(Debug, Clone, Copy)]
struct TaskSession {
    root: u32,
    pgid: u32,
    sid: u32,
}

/// SIGTERM → short wait → SIGKILL when the caller has no `kill_grace` handy
/// (force-cancel, interrupt, probe, keepwarm, orphan reap).
const SHORT_KILL_GRACE: Duration = Duration::from_millis(500);

pub fn kill_group(pid: u32, sig: i32) {
    kill_pgrp(pid, sig);
}

fn our_pid() -> u32 {
    std::process::id()
}

fn our_pgid() -> u32 {
    let g = unsafe { libc::getpgid(0) };
    if g > 1 {
        g as u32
    } else {
        0
    }
}

fn our_sid() -> u32 {
    let s = unsafe { libc::getsid(0) };
    if s > 1 {
        s as u32
    } else {
        0
    }
}

fn kill_pid(pid: u32, sig: i32) {
    if pid <= 1 || pid == our_pid() {
        return;
    }
    unsafe {
        libc::kill(pid as i32, sig);
    }
}

/// `kill(-pgid, sig)`. Never signal pid 1 or atr's own group.
fn kill_pgrp(pgid: u32, sig: i32) {
    if pgid <= 1 || pgid == our_pid() || pgid == our_pgid() {
        return;
    }
    unsafe {
        libc::kill(-(pgid as i32), sig);
    }
}

fn ps_formats() -> &'static [&'static str] {
    // GNU ps: `sid`. BSD/macOS ps: `sess`. Try the host's native name first.
    if cfg!(target_os = "macos") {
        &["pid=,ppid=,pgid=,sess=", "pid=,ppid=,pgid=,sid="]
    } else {
        &["pid=,ppid=,pgid=,sid=", "pid=,ppid=,pgid=,sess="]
    }
}

fn list_procs() -> Vec<ProcRow> {
    for spec in ps_formats() {
        let output = std::process::Command::new("ps")
            .args(["-axo", spec])
            .output();
        let Ok(o) = output else {
            continue;
        };
        if !o.status.success() {
            continue;
        }
        let text = String::from_utf8_lossy(&o.stdout);
        let mut rows = Vec::new();
        let mut parsed = true;
        for line in text.lines() {
            let mut it = line.split_whitespace();
            let Some(pid) = it.next().and_then(|s| s.parse::<u32>().ok()) else {
                continue;
            };
            let Some(ppid) = it.next().and_then(|s| s.parse::<u32>().ok()) else {
                parsed = false;
                break;
            };
            let Some(pgid) = it.next().and_then(|s| s.parse::<u32>().ok()) else {
                parsed = false;
                break;
            };
            let Some(sid) = it.next().and_then(|s| s.parse::<u32>().ok()) else {
                parsed = false;
                break;
            };
            rows.push(ProcRow {
                pid,
                ppid,
                pgid,
                sid,
            });
        }
        if parsed && !rows.is_empty() {
            return rows;
        }
    }
    Vec::new()
}

fn descendants_from(procs: &[ProcRow], root: u32) -> Vec<u32> {
    if root <= 1 {
        return Vec::new();
    }
    let mut kids = Vec::new();
    let mut stack = vec![root];
    let mut seen = HashSet::new();
    seen.insert(root);
    while let Some(cur) = stack.pop() {
        for row in procs {
            if row.ppid == cur && seen.insert(row.pid) {
                kids.push(row.pid);
                stack.push(row.pid);
            }
        }
    }
    kids
}

/// Snapshot of `root` plus every descendant, walking `ps` parent links.
/// cursor-agent (and similar) put shell tools in their own process groups,
/// so `killpg` on the CLI session leaves those children behind.
pub fn descendant_pids(root: u32) -> Vec<u32> {
    descendants_from(&list_procs(), root)
}

fn session_from(procs: &[ProcRow], root: u32) -> TaskSession {
    if let Some(row) = procs.iter().find(|r| r.pid == root) {
        return TaskSession {
            root,
            pgid: row.pgid,
            sid: row.sid,
        };
    }
    let pgid = unsafe { libc::getpgid(root as i32) };
    let sid = unsafe { libc::getsid(root as i32) };
    TaskSession {
        root,
        // Leader already gone: after setsid(), pid was the group and session.
        pgid: if pgid > 1 { pgid as u32 } else { root },
        sid: if sid > 1 { sid as u32 } else { root },
    }
}

fn safe_group_id(id: u32) -> Option<u32> {
    if id <= 1 || id == our_pid() || id == our_pgid() || id == our_sid() {
        None
    } else {
        Some(id)
    }
}

/// PIDs to tear down: ppid descendants plus anything still in the task's
/// process group or session (`nohup cmd &` reparents to init but keeps pgid/sid).
fn members_of(sess: &TaskSession, procs: &[ProcRow]) -> Vec<u32> {
    let mut seen = HashSet::new();
    if sess.root > 1 {
        seen.insert(sess.root);
    }
    for pid in descendants_from(procs, sess.root) {
        if pid > 1 {
            seen.insert(pid);
        }
    }
    let pgid = safe_group_id(sess.pgid);
    let sid = safe_group_id(sess.sid);
    let self_pid = our_pid();
    for row in procs {
        if row.pid <= 1 || row.pid == self_pid {
            continue;
        }
        let by_pgid = pgid.is_some_and(|g| row.pgid == g);
        let by_sid = sid.is_some_and(|s| row.sid == s);
        if by_pgid || by_sid {
            seen.insert(row.pid);
        }
    }
    seen.into_iter().collect()
}

/// CLI pid plus every process we will signal: descendants and session mates.
pub fn task_pids(root: u32) -> Vec<u32> {
    if root <= 1 {
        return Vec::new();
    }
    let procs = list_procs();
    members_of(&session_from(&procs, root), &procs)
}

/// CPU and IO counters for one process, used as a heartbeat liveness probe.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct ProcActivity {
    pub cpu_ticks: u64,
    pub io_bytes: u64,
}

/// Snapshot CPU/IO for `root` and every current descendant / session mate.
pub(crate) fn sample_tree_activity(root: u32) -> HashMap<u32, ProcActivity> {
    let mut map = HashMap::new();
    if root == 0 {
        return map;
    }
    for pid in task_pids(root) {
        if let Some(s) = sample_pid_activity(pid) {
            map.insert(pid, s);
        }
    }
    map
}

pub(crate) fn sample_pid_activity(pid: u32) -> Option<ProcActivity> {
    if pid == 0 {
        return None;
    }
    sample_proc(pid).or_else(|| sample_ps(pid))
}

fn sample_proc(pid: u32) -> Option<ProcActivity> {
    let text = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after = text.rsplit_once(')')?.1;
    let mut fields = after.split_whitespace();
    let utime: u64 = fields.nth(11)?.parse().ok()?;
    let stime: u64 = fields.next()?.parse().ok()?;
    let mut io_bytes = 0u64;
    if let Ok(io) = std::fs::read_to_string(format!("/proc/{pid}/io")) {
        for line in io.lines() {
            if let Some(v) = line.strip_prefix("read_bytes: ") {
                io_bytes = io_bytes.saturating_add(v.trim().parse().unwrap_or(0));
            } else if let Some(v) = line.strip_prefix("write_bytes: ") {
                io_bytes = io_bytes.saturating_add(v.trim().parse().unwrap_or(0));
            }
        }
    }
    Some(ProcActivity {
        cpu_ticks: utime.saturating_add(stime),
        io_bytes,
    })
}

fn sample_ps(pid: u32) -> Option<ProcActivity> {
    let out = std::process::Command::new("ps")
        .args(["-o", "time=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let time = text.lines().next()?.trim();
    if time.is_empty() {
        return None;
    }
    Some(ProcActivity {
        cpu_ticks: parse_cputime(time)?,
        io_bytes: 0,
    })
}

fn parse_cputime(s: &str) -> Option<u64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let (days, rest) = match s.split_once('-') {
        Some((d, r)) => (d.parse::<u64>().ok()?, r),
        None => (0, s),
    };
    let parts: Vec<&str> = rest.split(':').collect();
    let secs = match parts.as_slice() {
        [h, m, sec] => {
            h.parse::<u64>().ok()? * 3600 + m.parse::<u64>().ok()? * 60 + parse_whole_secs(sec)?
        }
        [m, sec] => m.parse::<u64>().ok()? * 60 + parse_whole_secs(sec)?,
        [sec] => parse_whole_secs(sec)?,
        _ => return None,
    };
    Some(days.saturating_mul(86400).saturating_add(secs))
}

fn parse_whole_secs(s: &str) -> Option<u64> {
    s.split('.').next()?.parse().ok()
}

/// CPU ticks (~10ms on Linux) a process must burn between samples to count
/// as live. A sleeping hang (the stall fixture) stays under this; a test
/// driver does not. New pids are only a baseline — they must grow next window.
const CPU_LIVE_TICKS: u64 = 5;
const IO_LIVE_BYTES: u64 = 4096;

/// True when a pid seen in both samples burned meaningful CPU or IO.
pub(crate) fn tree_activity_grew(
    prev: &HashMap<u32, ProcActivity>,
    next: &HashMap<u32, ProcActivity>,
) -> bool {
    next.iter().any(|(pid, now)| {
        prev.get(pid).is_some_and(|old| {
            now.cpu_ticks.saturating_sub(old.cpu_ticks) >= CPU_LIVE_TICKS
                || now.io_bytes.saturating_sub(old.io_bytes) >= IO_LIVE_BYTES
        })
    })
}

/// True when a file in the worktree (not `.git/` or `.atr/`) is newer than `since`.
pub(crate) fn worktree_touched_since(worktree: &Path, since: SystemTime) -> bool {
    fn walk(path: &Path, since: SystemTime, depth: u32, left: &mut u32) -> bool {
        if *left == 0 || depth > 8 {
            return false;
        }
        let Ok(rd) = std::fs::read_dir(path) else {
            return false;
        };
        for ent in rd.flatten() {
            if *left == 0 {
                return false;
            }
            let name = ent.file_name();
            if depth == 0 && (name == ".git" || name == ".atr") {
                continue;
            }
            let Ok(ft) = ent.file_type() else {
                continue;
            };
            if ft.is_symlink() {
                continue;
            }
            *left = left.saturating_sub(1);
            if ft.is_file() {
                if let Ok(meta) = ent.metadata() {
                    if meta.modified().is_ok_and(|m| m >= since) {
                        return true;
                    }
                }
            } else if ft.is_dir() && walk(&ent.path(), since, depth + 1, left) {
                return true;
            }
        }
        false
    }
    let mut left = 4000u32;
    walk(worktree, since, 0, &mut left)
}

/// After first CLI output, silence is not a stall while the tree or worktree
/// is still doing work (long-running test driver, git, …).
pub(crate) fn silence_is_live(
    saw_output: bool,
    descendant_activity: bool,
    worktree_activity: bool,
) -> bool {
    saw_output && (descendant_activity || worktree_activity)
}

fn apply_kill(sess: &TaskSession, tree: &[u32], sig: i32) {
    for &pid in tree.iter().rev() {
        kill_pgrp(pid, sig);
        kill_pid(pid, sig);
    }
    if let Some(g) = safe_group_id(sess.pgid) {
        kill_pgrp(g, sig);
    }
    if let Some(s) = safe_group_id(sess.sid) {
        if s != sess.pgid {
            kill_pgrp(s, sig);
        }
    }
    kill_pgrp(sess.root, sig);
    kill_pid(sess.root, sig);
}

/// Kill the CLI session and every descendant, including those that called `setsid`.
pub fn kill_process_tree(root: u32, sig: i32) {
    if root <= 1 {
        return;
    }
    let procs = list_procs();
    let sess = session_from(&procs, root);
    let tree = members_of(&sess, &procs);
    apply_kill(&sess, &tree, sig);
}

/// Force-cancel: SIGTERM the snapshotted group/session, wait a short grace,
/// then SIGKILL. Re-scans pgid/sid after TERM so reparented survivors go too.
pub fn terminate_tree_now(root: u32) {
    terminate_tree_grace(root, SHORT_KILL_GRACE);
}

/// Same as [`terminate_tree_now`] with an explicit grace (stall / run_timeout).
pub fn terminate_tree_grace(root: u32, grace: Duration) {
    if root <= 1 {
        return;
    }
    let procs = list_procs();
    let sess = session_from(&procs, root);
    let mut tree: HashSet<u32> = members_of(&sess, &procs).into_iter().collect();
    let snap: Vec<u32> = tree.iter().copied().collect();
    apply_kill(&sess, &snap, libc::SIGTERM);
    let start = std::time::Instant::now();
    while start.elapsed() < grace {
        if tree_dead(sess.root, &snap) {
            return;
        }
        std::thread::sleep(Duration::from_millis(40));
    }
    let procs = list_procs();
    for pid in members_of(&sess, &procs) {
        tree.insert(pid);
    }
    let leftover: Vec<u32> = tree
        .iter()
        .copied()
        .filter(|&p| crate::db::pid_alive(i64::from(p)))
        .collect();
    if leftover.is_empty() && !crate::db::pid_alive(i64::from(sess.root)) {
        return;
    }
    apply_kill(&sess, &leftover, libc::SIGKILL);
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

/// Put this child in its own session and process group (`setsid`).
///
/// `CommandExt::process_group(0)` / `setpgid(0,0)` is not used: that makes
/// the child a group leader *in atr's session*, after which `setsid()` fails
/// with EPERM. A unique session is required so a later sid-walk cannot collide
/// with other tasks or with atr itself. `setsid()` already creates a new
/// process group (`pid == pgid == sid`), so `kill(-pgid)` reaches `nohup`
/// children that never called `setsid` themselves.
pub(crate) fn isolate_session(cmd: &mut Command) {
    unsafe {
        cmd.pre_exec(enter_own_session);
    }
}

fn enter_own_session() -> std::io::Result<()> {
    if unsafe { libc::setsid() } == -1 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// The agent child process: argv, cwd, piped stdio, and the spec's env.
///
/// `current_dir` alone does not update `$PWD`, and OpenCode 2.x takes its
/// project directory from `$PWD`; without the explicit `PWD` it runs (and
/// edits files) in whatever folder atr itself was started from, which can be
/// the main checkout or its parent.
pub(crate) fn agent_command(spec: &crate::providers::CommandSpec) -> Command {
    let mut cmd = Command::new(&spec.program);
    cmd.args(&spec.args)
        .current_dir(&spec.cwd)
        .env("PWD", &spec.cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    for (k, v) in &spec.env {
        cmd.env(k, v);
    }
    isolate_session(&mut cmd);
    cmd
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
    if let Ok(Some(task)) = db.get_task(task_key) {
        if crate::deny::applies(cfg, &task) {
            crate::deny::apply_to_spec(
                &provider.kind,
                &mut spec,
                &crate::deny::effective_rules(&cfg.telegram.deny),
            );
        }
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

    let mut cmd = agent_command(&spec);
    cmd.env("ATR_PROVIDER", &provider.name);
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

    let mut child = crate::util::retry_exec_busy(|| cmd.spawn())
        .with_context(|| format!("spawn {} ({})", spec.program.display(), provider.name))?;
    let pid = child.id().unwrap_or(0);
    let mut seen_descendants = HashSet::new();
    let mut last_activity = HashMap::new();
    let mut window_start = SystemTime::now();
    if pid > 0 {
        db.set_run_pid(run_id, pid)?;
        *inflight_pid.lock().unwrap() = Some(pid);
        note_descendants(pid, &mut seen_descendants);
        last_activity = sample_tree_activity(pid);
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
                crate::config::RUN_TIMEOUT_OUTCOME,
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
                        window_start = SystemTime::now();
                        last_activity = sample_tree_activity(pid);
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
                note_descendants(pid, &mut seen_descendants);
                let now_act = sample_tree_activity(pid);
                let grew = tree_activity_grew(&last_activity, &now_act);
                let files = worktree_touched_since(worktree, window_start);
                last_activity = now_act;
                if silence_is_live(saw_output, grew, files) {
                    tracing::debug!(
                        provider = %provider.name,
                        run_id,
                        "heartbeat silence; process tree or worktree still active"
                    );
                    let _ = db.touch_run(run_id);
                    next_beat = Instant::now() + heartbeat;
                    window_start = SystemTime::now();
                    continue;
                }
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
    if pid <= 1 {
        return;
    }
    let procs = list_procs();
    let sess = session_from(&procs, pid);
    let mut tree: HashSet<u32> = members_of(&sess, &procs).into_iter().collect();
    let snap: Vec<u32> = tree.iter().copied().collect();
    apply_kill(&sess, &snap, libc::SIGTERM);
    let start = Instant::now();
    while start.elapsed() < grace {
        if tree_dead(sess.root, &snap) {
            return;
        }
        sleep(Duration::from_millis(40)).await;
    }
    let procs = list_procs();
    for p in members_of(&sess, &procs) {
        tree.insert(p);
    }
    let leftover: Vec<u32> = tree
        .iter()
        .copied()
        .filter(|&p| crate::db::pid_alive(i64::from(p)))
        .collect();
    if leftover.is_empty() && !crate::db::pid_alive(i64::from(sess.root)) {
        return;
    }
    apply_kill(&sess, &leftover, libc::SIGKILL);
}

fn tree_dead(root: u32, tree: &[u32]) -> bool {
    !crate::db::pid_alive(i64::from(root))
        && tree.iter().all(|p| !crate::db::pid_alive(i64::from(*p)))
}

fn note_descendants(root: u32, into: &mut HashSet<u32>) {
    if root <= 1 {
        return;
    }
    for p in task_pids(root) {
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

/// TERM then KILL leftover pids (and their groups/sessions) after the CLI has exited.
pub fn reap_leftovers_now(pids: &[u32], grace: Duration) {
    let procs = list_procs();
    let mut all: HashSet<u32> = HashSet::new();
    let mut sessions: Vec<TaskSession> = Vec::new();
    for &p in pids {
        if p <= 1 || !crate::db::pid_alive(i64::from(p)) {
            continue;
        }
        let sess = session_from(&procs, p);
        for m in members_of(&sess, &procs) {
            all.insert(m);
        }
        sessions.push(sess);
    }
    if all.is_empty() {
        return;
    }
    let snap: Vec<u32> = all.iter().copied().collect();
    for sess in &sessions {
        apply_kill(sess, &snap, libc::SIGTERM);
    }
    let start = std::time::Instant::now();
    while start.elapsed() < grace {
        if snap.iter().all(|p| !crate::db::pid_alive(i64::from(*p))) {
            return;
        }
        std::thread::sleep(Duration::from_millis(40));
    }
    let procs = list_procs();
    for sess in &sessions {
        for m in members_of(sess, &procs) {
            all.insert(m);
        }
    }
    let leftover: Vec<u32> = all
        .iter()
        .copied()
        .filter(|&p| crate::db::pid_alive(i64::from(p)))
        .collect();
    if leftover.is_empty() {
        return;
    }
    for sess in &sessions {
        apply_kill(sess, &leftover, libc::SIGKILL);
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
    use std::os::unix::process::CommandExt;
    use std::process::{Command as StdCommand, Stdio as StdStdio};

    fn spawn_isolated(program: &str, args: &[&str]) -> std::process::Child {
        let mut cmd = StdCommand::new(program);
        cmd.args(args)
            .stdin(StdStdio::null())
            .stdout(StdStdio::null())
            .stderr(StdStdio::null());
        unsafe {
            cmd.pre_exec(enter_own_session);
        }
        cmd.spawn().expect("spawn isolated child")
    }

    fn wait_pidfile(path: &Path, timeout: Duration) -> u32 {
        let start = std::time::Instant::now();
        loop {
            if let Ok(text) = std::fs::read_to_string(path) {
                if let Ok(pid) = text.trim().parse::<u32>() {
                    return pid;
                }
            }
            if start.elapsed() > timeout {
                panic!("pid file {} not written", path.display());
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn assert_dead(pid: u32, label: &str) {
        let start = std::time::Instant::now();
        while crate::db::pid_alive(i64::from(pid)) && start.elapsed() < Duration::from_secs(2) {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            !crate::db::pid_alive(i64::from(pid)),
            "{label} pid {pid} must be dead"
        );
    }

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
    fn terminate_tree_kills_reparented_nohup_child() {
        // `nohup cmd &` from a non-interactive shell keeps the task pgid/sid
        // after the launcher exits and the child is reparented to init.
        let dir = tempfile::tempdir().unwrap();
        let orphan_file = dir.path().join("orphan.pid");
        let script = format!(
            r#"
import os, subprocess, time
path = {path:?}
pid = os.fork()
if pid == 0:
    subprocess.Popen(
        ["nohup", "sh", "-c", "echo $$ > " + path + "; while :; do sleep 1; done"],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        start_new_session=False,
    )
    os._exit(0)
os.waitpid(pid, 0)
time.sleep(600)
"#,
            path = orphan_file.display().to_string()
        );
        let mut child = spawn_isolated("python3", &["-c", &script]);
        let parent_pid = child.id();
        let orphan = wait_pidfile(&orphan_file, Duration::from_secs(3));
        assert!(
            crate::db::pid_alive(i64::from(orphan)),
            "nohup child should be alive before kill"
        );
        // Wait until the launcher subshell has exited so ppid is no longer
        // the CLI (the 2026-11-19 leak: walk-by-ppid misses this pid).
        let start = std::time::Instant::now();
        while descendant_pids(parent_pid).contains(&orphan)
            && start.elapsed() < Duration::from_secs(2)
        {
            std::thread::sleep(Duration::from_millis(20));
        }
        terminate_tree_now(parent_pid);
        let _ = child.wait();
        assert_dead(parent_pid, "session leader");
        assert_dead(orphan, "reparented nohup child");
    }

    #[test]
    fn terminate_tree_kills_double_fork_same_session() {
        // Classic daemonize minus setsid: grandchild's ppid is 1, but it
        // still carries the task sid. A grandchild that *also* called
        // setsid after the intermediate exited cannot be found this way.
        let dir = tempfile::tempdir().unwrap();
        let orphan_file = dir.path().join("orphan.pid");
        let script = format!(
            r#"
import os, time
pid = os.fork()
if pid == 0:
    pid2 = os.fork()
    if pid2 == 0:
        open({orphan:?}, "w").write(str(os.getpid()) + "\n")
        time.sleep(600)
        os._exit(0)
    os._exit(0)
os.waitpid(pid, 0)
time.sleep(600)
"#,
            orphan = orphan_file
        );
        let mut child = spawn_isolated("python3", &["-c", &script]);
        let parent_pid = child.id();
        let orphan = wait_pidfile(&orphan_file, Duration::from_secs(3));
        assert!(
            crate::db::pid_alive(i64::from(orphan)),
            "double-fork grandchild should be alive before kill"
        );
        assert!(
            !descendant_pids(parent_pid).contains(&orphan),
            "grandchild should already be reparented off the CLI"
        );
        terminate_tree_now(parent_pid);
        let _ = child.wait();
        assert_dead(parent_pid, "session leader");
        assert_dead(orphan, "double-fork grandchild");
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

    #[test]
    fn silence_is_live_needs_output_and_activity() {
        assert!(
            !silence_is_live(false, true, true),
            "startup timeout is unchanged even if a child exists"
        );
        assert!(
            silence_is_live(true, true, false),
            "descendant CPU/IO extends the heartbeat"
        );
        assert!(
            silence_is_live(true, false, true),
            "worktree file activity extends the heartbeat"
        );
        assert!(
            !silence_is_live(true, false, false),
            "true CLI hang still stalls"
        );
    }

    #[test]
    fn tree_activity_grew_on_cpu_io_not_idle_or_new_pid() {
        let mut prev = HashMap::new();
        prev.insert(
            1,
            ProcActivity {
                cpu_ticks: 10,
                io_bytes: 4,
            },
        );
        let mut same = HashMap::new();
        same.insert(
            1,
            ProcActivity {
                cpu_ticks: 10,
                io_bytes: 4,
            },
        );
        assert!(!tree_activity_grew(&prev, &same));

        let mut noise = same.clone();
        noise.insert(
            1,
            ProcActivity {
                cpu_ticks: 11,
                io_bytes: 4,
            },
        );
        assert!(
            !tree_activity_grew(&prev, &noise),
            "a single scheduler tick on a sleeping process is not liveness"
        );

        let mut cpu = same.clone();
        cpu.insert(
            1,
            ProcActivity {
                cpu_ticks: 16,
                io_bytes: 4,
            },
        );
        assert!(tree_activity_grew(&prev, &cpu));

        let mut io = same.clone();
        io.insert(
            1,
            ProcActivity {
                cpu_ticks: 10,
                io_bytes: 5000,
            },
        );
        assert!(tree_activity_grew(&prev, &io));

        let mut child = same;
        child.insert(
            2,
            ProcActivity {
                cpu_ticks: 80,
                io_bytes: 0,
            },
        );
        assert!(
            !tree_activity_grew(&prev, &child),
            "a new pid is a baseline, not proof of work (stall fixture children)"
        );
    }

    #[test]
    fn worktree_touched_since_sees_new_file_and_skips_atr() {
        let dir = tempfile::tempdir().unwrap();
        let before = SystemTime::now() - Duration::from_secs(2);
        std::fs::write(dir.path().join("out.log"), "driver\n").unwrap();
        assert!(worktree_touched_since(dir.path(), before));

        let future = SystemTime::now() + Duration::from_secs(60);
        assert!(!worktree_touched_since(dir.path(), future));

        let atr = dir.path().join(".atr");
        std::fs::create_dir_all(&atr).unwrap();
        std::fs::write(atr.join("prompt.md"), "atr file\n").unwrap();
        assert!(
            !worktree_touched_since(dir.path(), future),
            ".atr/ writes are atr's own and must not look like agent work"
        );
    }

    #[test]
    fn parse_cputime_linux_and_macos() {
        assert_eq!(super::parse_cputime("00:00:01"), Some(1));
        assert_eq!(super::parse_cputime("1-00:00:02"), Some(86402));
        assert_eq!(super::parse_cputime("1:02.50"), Some(62));
        assert_eq!(super::parse_cputime(""), None);
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
    #[test]
    fn agent_command_sets_pwd_to_the_worktree() {
        let spec = crate::providers::CommandSpec {
            program: PathBuf::from("opencode"),
            args: vec!["run".into()],
            cwd: PathBuf::from("/wt/local-0"),
            stdin_text: None,
            env: vec![("OPENCODE_CONFIG_CONTENT".into(), "{}".into())],
        };
        let cmd = agent_command(&spec);
        let std = cmd.as_std();
        assert_eq!(std.get_current_dir(), Some(Path::new("/wt/local-0")));
        let envs: Vec<_> = std.get_envs().collect();
        assert!(
            envs.contains(&(
                std::ffi::OsStr::new("PWD"),
                Some(std::ffi::OsStr::new("/wt/local-0"))
            )),
            "{envs:?}"
        );
        assert!(envs
            .iter()
            .any(|(k, _)| *k == std::ffi::OsStr::new("OPENCODE_CONFIG_CONTENT")));
    }
}
