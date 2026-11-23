//! Automatic reaper, sealed-summary settle, artifact cleanup, and health file.
//!
//! Serve runs [`selfheal_loop`]. Actions are logged at INFO with task key and
//! reason. Restart recovery (`outcome=restarted`) counts as the one automatic
//! re-queue so a serve restart cannot loop with this reaper.

use crate::app::Shared;
use crate::config::{expand_path, Config, SelfhealConfig};
use crate::db::{pid_alive, Db, RunRow, Task};
use crate::doctor::{self, Check, Report, Status};
use crate::prompt;
use crate::rfc3339;
use crate::scheduler;
use anyhow::Result;
use jiff::{SignedDuration, Timestamp};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

const HEALTH_LOG_MAX: u64 = 1024 * 1024;

/// Visible on the task after the first automatic re-queue.
pub const REAP_RESULT: &str = "selfheal: agent process gone; re-queued once";
/// Second death (or a death after `[restart]` already resumed).
pub const REAP_EXHAUSTED: &str = "selfheal: agent process gone after one automatic re-queue";
pub const SETTLE_REASON: &str = "selfheal: sealed summary.md; settled to done";

const ACTIONS_META: &str = "selfheal_actions";
const ALERT_META: &str = "selfheal_alert";
const MAX_ACTIONS: usize = 50;
const SEAL_MARKERS: &[&str] = &["<!-- atr-sealed -->", "sealed: true"];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Action {
    pub at: String,
    pub kind: String,
    pub task: String,
    pub reason: String,
}

#[derive(Debug, Default, Clone)]
pub struct TickReport {
    pub requeued: Vec<String>,
    pub needs_attention: Vec<String>,
    pub settled: Vec<String>,
    pub locks: Vec<String>,
    pub slots: Vec<String>,
    pub worktrees: Vec<String>,
    pub dirs: Vec<String>,
}

/// Serve / tests: reap, settle, cleanup once. Health is [`check_and_write`].
pub fn tick(shared: &Shared) -> Result<TickReport> {
    let cfg = shared.config.read().unwrap().clone();
    if !cfg.selfheal.enabled {
        return Ok(TickReport::default());
    }
    let mut report = TickReport::default();
    if cfg.selfheal.reap {
        let n = scheduler::reconcile(shared)?;
        if n > 0 {
            tracing::debug!(n, "selfheal reconcile");
        }
    }
    if cfg.selfheal.settle {
        settle_waiting(shared, &cfg, &mut report)?;
    }
    if cfg.selfheal.cleanup {
        cleanup(shared, &cfg, &mut report)?;
    }
    Ok(report)
}

pub async fn selfheal_loop(shared: Arc<Shared>) {
    let mut last_check = Timestamp::now() - SignedDuration::from_mins(60);
    loop {
        let cfg = shared.config.read().unwrap().clone();
        let interval = if cfg.selfheal.enabled {
            cfg.selfheal.interval.max(Duration::from_secs(1))
        } else {
            Duration::from_secs(30)
        };
        tokio::time::sleep(interval).await;
        if !cfg.selfheal.enabled {
            continue;
        }
        if let Err(e) = tick(&shared) {
            tracing::warn!("selfheal tick: {e:#}");
        }
        if cfg.selfheal.check {
            let due = (Timestamp::now() - last_check)
                .try_into()
                .map(|d: Duration| d >= cfg.selfheal.check_interval)
                .unwrap_or(true);
            if due {
                last_check = Timestamp::now();
                if let Err(e) = check_and_write(&shared).await {
                    tracing::warn!("selfheal health: {e:#}");
                }
            }
        }
    }
}

/// Reap one dead open run. First death (and not already `restarted`/`reaped`)
/// re-queues; a second death marks `needs_attention`. Never kills a live PID.
pub fn reap_dead_run(shared: &Shared, key: &str, run: &RunRow) -> Result<bool> {
    if shared.inflight.lock().unwrap().contains_key(key) {
        return Ok(false);
    }
    if !scheduler::serve_dispatches(key) {
        return Ok(false);
    }
    let Some(task) = shared.db.get_task(key)? else {
        return Ok(false);
    };
    if task.status == "held" || !task.hold_reason.is_empty() {
        return Ok(false);
    }
    if run.pid.filter(|p| *p > 0).is_some_and(pid_alive) {
        return Ok(false);
    }
    let cfg = shared.config.read().unwrap().clone();
    if run.pid.filter(|p| *p > 0).is_none()
        && !age_exceeds(run.started.as_str(), cfg.selfheal.stuck_timeout)
    {
        return Ok(false);
    }
    if missing_worktree(&task) {
        return exhaust(
            shared,
            key,
            run,
            "selfheal: agent process gone and worktree missing",
        );
    }
    if auto_requeue_used(&shared.db, key, run.id)? {
        return exhaust(shared, key, run, REAP_EXHAUSTED);
    }
    shared.db.finish_run(run.id, "reaped", Some(-2))?;
    if shared.db.pending_messages(key)?.is_empty() {
        shared
            .db
            .enqueue_message(key, prompt::REAP_CONTINUE, "selfheal")?;
    }
    if task.result_text.is_empty() {
        let _ = shared.db.set_task_result(key, REAP_RESULT, "", None);
    }
    shared.db.set_status(key, "queued")?;
    record(shared, "requeue", key, "agent process gone; re-queued once");
    tracing::info!(
        task = %key,
        run = run.id,
        reason = "agent process gone",
        "selfheal requeue"
    );
    Ok(true)
}

/// Running with no open run (setup died). Same once/exhaust rules.
pub fn reap_running_without_run(shared: &Shared, task: &Task) -> Result<bool> {
    if shared.inflight.lock().unwrap().contains_key(&task.key) {
        return Ok(false);
    }
    if !scheduler::serve_dispatches(&task.key) {
        return Ok(false);
    }
    if task.status == "held" || !task.hold_reason.is_empty() {
        return Ok(false);
    }
    if missing_worktree(task) {
        return exhaust_task(
            shared,
            &task.key,
            "selfheal: running with no run and worktree missing",
        );
    }
    if auto_requeue_used(&shared.db, &task.key, -1)? || task.result_text == REAP_RESULT {
        return exhaust_task(shared, &task.key, REAP_EXHAUSTED);
    }
    if shared.db.pending_messages(&task.key)?.is_empty() {
        shared
            .db
            .enqueue_message(&task.key, prompt::REAP_CONTINUE, "selfheal")?;
    }
    if task.result_text.is_empty() {
        let _ = shared.db.set_task_result(&task.key, REAP_RESULT, "", None);
    }
    shared.db.set_status(&task.key, "queued")?;
    record(
        shared,
        "requeue",
        &task.key,
        "running with no process; re-queued once",
    );
    tracing::info!(
        task = %task.key,
        reason = "running with no process",
        "selfheal requeue"
    );
    Ok(true)
}

fn exhaust(shared: &Shared, key: &str, run: &RunRow, reason: &str) -> Result<bool> {
    shared.db.finish_run(run.id, "failed", Some(-1))?;
    exhaust_task(shared, key, reason)
}

fn exhaust_task(shared: &Shared, key: &str, reason: &str) -> Result<bool> {
    let _ = shared.db.set_task_result(key, reason, "", None);
    shared.db.set_status(key, "needs_attention")?;
    record(shared, "needs_attention", key, reason);
    tracing::info!(task = %key, reason, "selfheal needs_attention");
    Ok(true)
}

fn auto_requeue_used(db: &Db, key: &str, except_run_id: i64) -> Result<bool> {
    let last = db
        .list_runs(Some(key))?
        .into_iter()
        .rev()
        .find(|r| r.id != except_run_id && r.outcome.is_some());
    Ok(matches!(
        last.and_then(|r| r.outcome).as_deref(),
        Some("reaped") | Some("restarted")
    ))
}

fn missing_worktree(task: &Task) -> bool {
    match task.worktree.as_deref().filter(|s| !s.is_empty()) {
        Some(p) => !Path::new(p).exists(),
        None => false,
    }
}

fn age_exceeds(ts: &str, d: Duration) -> bool {
    let Some(t) = rfc3339::parse(ts) else {
        return true;
    };
    let Ok(age) = Duration::try_from(Timestamp::now() - t) else {
        return true;
    };
    age >= d
}

fn settle_waiting(shared: &Shared, cfg: &Config, report: &mut TickReport) -> Result<()> {
    for t in shared.db.list_tasks()? {
        if t.status != "waiting" {
            continue;
        }
        if crate::telegram::is_assistant_key(&t.key) || crate::tui::is_tui_key(&t.key) {
            continue;
        }
        if shared.inflight.lock().unwrap().contains_key(&t.key) {
            continue;
        }
        if !shared.db.pending_messages(&t.key)?.is_empty() {
            continue;
        }
        let Some(wt) = t.worktree.as_deref().filter(|s| !s.is_empty()) else {
            continue;
        };
        if !has_sealed_summary(Path::new(wt), &cfg.selfheal.summary_names) {
            continue;
        }
        shared.db.set_status(&t.key, "done")?;
        if t.result_text.is_empty() {
            let _ = shared.db.set_task_result(&t.key, SETTLE_REASON, "", None);
        }
        record(shared, "settle", &t.key, SETTLE_REASON);
        tracing::info!(
            task = %t.key,
            reason = "sealed summary.md",
            "selfheal settle"
        );
        report.settled.push(t.key);
    }
    Ok(())
}

/// A summary file is sealed when it is non-empty and either not writable
/// (`chmod a-w`) or contains `<!-- atr-sealed -->` / `sealed: true`.
pub fn has_sealed_summary(worktree: &Path, names: &[String]) -> bool {
    for name in names {
        if is_sealed(&worktree.join(name)) {
            return true;
        }
        if is_sealed(&worktree.join(".atr").join(name)) {
            return true;
        }
    }
    false
}

pub fn is_sealed(path: &Path) -> bool {
    let Ok(meta) = fs::metadata(path) else {
        return false;
    };
    if !meta.is_file() || meta.len() == 0 {
        return false;
    }
    if file_not_writable(&meta) {
        return true;
    }
    let Ok(text) = fs::read_to_string(path) else {
        return false;
    };
    text.lines().any(|l| {
        let t = l.trim();
        SEAL_MARKERS.iter().any(|m| t.eq_ignore_ascii_case(m))
    })
}

fn file_not_writable(meta: &fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o222 == 0
    }
    #[cfg(not(unix))]
    {
        meta.permissions().readonly()
    }
}

fn cleanup(shared: &Shared, cfg: &Config, report: &mut TickReport) -> Result<()> {
    cleanup_slots(shared, report)?;
    let mut gc_cfg = cfg.clone();
    gc_cfg.gc.worktree_retention = cfg.selfheal.artifact_age;
    let mut gc_report = crate::gc::GcReport::default();
    crate::gc::prune_worktrees(&gc_cfg, &shared.db, false, false, &mut gc_report)?;
    for p in &gc_report.worktrees {
        record(shared, "cleanup_worktree", "", p);
        tracing::info!(task = "", reason = %p, "selfheal cleanup worktree");
        report.worktrees.push(p.clone());
    }
    cleanup_locks(shared, cfg, report)?;
    cleanup_prune_dirs(shared, cfg, report)?;
    Ok(())
}

fn cleanup_slots(shared: &Shared, report: &mut TickReport) -> Result<()> {
    for (path, key) in shared.db.list_pool_slots()? {
        let Some(t) = shared.db.get_task(&key)? else {
            shared.db.release_pool_slots(&key)?;
            record(shared, "cleanup_slot", &key, &path);
            tracing::info!(task = %key, reason = %path, "selfheal cleanup slot (no task)");
            report.slots.push(key);
            continue;
        };
        if t.status == "running" || shared.inflight.lock().unwrap().contains_key(&key) {
            continue;
        }
        if !Db::removable_status(&t.status) {
            continue;
        }
        shared.db.release_pool_slots(&key)?;
        record(shared, "cleanup_slot", &key, &path);
        tracing::info!(task = %key, reason = %path, "selfheal cleanup slot");
        report.slots.push(key);
    }
    Ok(())
}

fn cleanup_locks(shared: &Shared, cfg: &Config, report: &mut TickReport) -> Result<()> {
    let live_pids = live_run_pids(&shared.db)?;
    let atr_pid = crate::util::pid_path(cfg);
    for path in expand_lock_patterns(&cfg.selfheal.lock_files) {
        if same_path(&path, &atr_pid) {
            continue;
        }
        if belongs_to_running_task(shared, &path) {
            continue;
        }
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        let Some((pid, started)) = crate::util::parse_pidfile(&text) else {
            continue;
        };
        let alive = match started {
            Some(s) => crate::owner::owner_alive(pid, s),
            None => pid_alive(pid),
        };
        if alive || live_pids.contains(&pid) {
            continue;
        }
        if !lock_older_than(&path, cfg.selfheal.stuck_timeout) {
            continue;
        }
        match fs::remove_file(&path) {
            Ok(()) => {
                let disp = path.display().to_string();
                record(shared, "cleanup_lock", "", &disp);
                tracing::info!(task = "", reason = %disp, "selfheal cleanup lock");
                report.locks.push(disp);
            }
            Err(e) => tracing::warn!(path = %path.display(), "selfheal lock remove: {e}"),
        }
    }
    Ok(())
}

fn cleanup_prune_dirs(shared: &Shared, cfg: &Config, report: &mut TickReport) -> Result<()> {
    let cut = Timestamp::now()
        - SignedDuration::try_from(cfg.selfheal.artifact_age)
            .unwrap_or(SignedDuration::from_hours(24 * 14));
    let running = running_keys(shared);
    for raw in &cfg.selfheal.prune_dirs {
        let dir = expand_path(raw);
        let Ok(rd) = fs::read_dir(&dir) else {
            continue;
        };
        for ent in rd.flatten() {
            let p = ent.path();
            if !p.is_dir() {
                continue;
            }
            let name = p
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_string();
            if running.contains(&name) {
                continue;
            }
            let Ok(meta) = fs::metadata(&p) else {
                continue;
            };
            let mtime = meta.modified().ok().and_then(|t| {
                Timestamp::from_second(
                    t.duration_since(std::time::UNIX_EPOCH)
                        .ok()?
                        .as_secs()
                        .try_into()
                        .ok()?,
                )
                .ok()
            });
            if mtime.is_some_and(|t| t > cut) {
                continue;
            }
            match fs::remove_dir_all(&p) {
                Ok(()) => {
                    let disp = p.display().to_string();
                    record(shared, "cleanup_dir", &name, &disp);
                    tracing::info!(task = %name, reason = %disp, "selfheal cleanup dir");
                    report.dirs.push(disp);
                }
                Err(e) => tracing::warn!(path = %p.display(), "selfheal dir remove: {e}"),
            }
        }
    }
    Ok(())
}

fn expand_lock_patterns(patterns: &[String]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for raw in patterns {
        let expanded = expand_path(raw);
        let name = expanded
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();
        if name.contains('*') {
            let Some(parent) = expanded.parent() else {
                continue;
            };
            let Ok(rd) = fs::read_dir(parent) else {
                continue;
            };
            for ent in rd.flatten() {
                let p = ent.path();
                if p.is_file()
                    && glob_name(p.file_name().and_then(|s| s.to_str()).unwrap_or(""), &name)
                {
                    out.push(p);
                }
            }
        } else if expanded.is_file() {
            out.push(expanded);
        }
    }
    out
}

fn glob_name(name: &str, pat: &str) -> bool {
    if let Some((pre, suf)) = pat.split_once('*') {
        name.starts_with(pre) && name.ends_with(suf) && name.len() >= pre.len() + suf.len()
    } else {
        name == pat
    }
}

fn live_run_pids(db: &Db) -> Result<Vec<i64>> {
    Ok(db
        .open_runs()?
        .into_iter()
        .filter_map(|r| r.pid.filter(|p| *p > 0))
        .collect())
}

fn running_keys(shared: &Shared) -> Vec<String> {
    let inflight: Vec<String> = shared.inflight.lock().unwrap().keys().cloned().collect();
    let mut keys = inflight;
    if let Ok(tasks) = shared.db.list_tasks() {
        for t in tasks {
            if t.status == "running" {
                keys.push(t.key);
            }
        }
    }
    keys
}

fn belongs_to_running_task(shared: &Shared, path: &Path) -> bool {
    let Ok(tasks) = shared.db.list_tasks() else {
        return false;
    };
    for t in tasks {
        if t.status != "running" && !shared.inflight.lock().unwrap().contains_key(&t.key) {
            continue;
        }
        if let Some(wt) = t.worktree.as_deref().filter(|s| !s.is_empty()) {
            if path.starts_with(wt) {
                return true;
            }
        }
    }
    false
}

fn lock_older_than(path: &Path, timeout: Duration) -> bool {
    let Ok(meta) = fs::metadata(path) else {
        return false;
    };
    let Ok(mtime) = meta.modified() else {
        return false;
    };
    mtime.elapsed().map(|age| age >= timeout).unwrap_or(false)
}

fn same_path(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(x), Ok(y)) => x == y,
        _ => a == b,
    }
}

fn record(shared: &Shared, kind: &str, task: &str, reason: &str) {
    let action = Action {
        at: rfc3339::millis(Timestamp::now()),
        kind: kind.into(),
        task: task.into(),
        reason: reason.into(),
    };
    let mut acts = load_actions(&shared.db);
    acts.push(action);
    if acts.len() > MAX_ACTIONS {
        let skip = acts.len() - MAX_ACTIONS;
        acts.drain(..skip);
    }
    if let Ok(s) = serde_json::to_string(&acts) {
        let _ = shared.db.meta_set(ACTIONS_META, &s);
    }
}

fn load_actions(db: &Db) -> Vec<Action> {
    db.meta_get(ACTIONS_META)
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn health_paths(cfg: &SelfhealConfig) -> (PathBuf, PathBuf) {
    (
        cfg.health_dir.join("health.json"),
        cfg.health_dir.join("health.txt"),
    )
}

pub fn load_health_file(cfg: &SelfhealConfig) -> Option<Value> {
    fs::read_to_string(health_paths(cfg).0)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
}

/// Doctor checks plus queue/disk checks; write health.json and health.txt.
/// Notify `[notify] command` on a new FAIL or recovery, de-duplicated.
pub async fn check_and_write(shared: &Shared) -> Result<Value> {
    let cfg = shared.config.read().unwrap().clone();
    let mut report = doctor::run_with_listen(
        &cfg,
        &shared.config_path,
        false,
        None,
        doctor::ListenCheck::Held,
    )
    .await;
    push_queue_checks(&mut report.checks, &cfg, shared);
    report.problems = report
        .checks
        .iter()
        .filter(|c| c.status != Status::Ok)
        .count();
    let snap = snapshot(&cfg, shared, &report);
    write_health(&cfg.selfheal, &snap)?;
    maybe_alert(shared, &cfg, &snap);
    Ok(snap)
}

pub fn push_queue_checks(checks: &mut Vec<Check>, cfg: &Config, shared: &Shared) {
    stuck_check(checks, cfg, shared);
    retries_check(checks, cfg, shared);
    cooldown_check(checks, cfg, shared);
    disk_check(checks, cfg);
}

fn stuck_check(checks: &mut Vec<Check>, cfg: &Config, shared: &Shared) {
    let mut stuck = Vec::new();
    let inflight = shared.inflight.lock().unwrap().clone();
    if let Ok(tasks) = shared.db.list_tasks() {
        for t in tasks {
            if t.status != "running" {
                continue;
            }
            if inflight.contains_key(&t.key) {
                continue;
            }
            if !scheduler::serve_dispatches(&t.key) {
                continue;
            }
            let open = shared.db.open_run_for_task(&t.key).ok().flatten();
            let dead = match open {
                Some(id) => shared
                    .db
                    .list_runs(Some(&t.key))
                    .ok()
                    .and_then(|rs| rs.into_iter().rev().find(|r| r.id == id))
                    .map(|r| {
                        let pid_dead = !r.pid.filter(|p| *p > 0).is_some_and(pid_alive);
                        let quiet = r
                            .last_event_at
                            .as_deref()
                            .map(|ts| age_exceeds(ts, cfg.selfheal.stuck_timeout))
                            .unwrap_or(true);
                        pid_dead && (r.pid.filter(|p| *p > 0).is_some() || quiet)
                    })
                    .unwrap_or(true),
                None => true,
            };
            if dead {
                stuck.push(t.key);
            }
        }
    }
    if stuck.is_empty() {
        checks.push(Check {
            name: "stuck runs".into(),
            status: Status::Ok,
            detail: "none".into(),
            hint: None,
        });
    } else {
        checks.push(Check {
            name: "stuck runs".into(),
            status: Status::Fail,
            detail: stuck.join(", "),
            hint: Some("atr selfheal re-queues once, then needs_attention".into()),
        });
    }
}

fn retries_check(checks: &mut Vec<Check>, cfg: &Config, shared: &Shared) {
    let max = i64::from(cfg.retry.max_attempts);
    let mut keys = Vec::new();
    if let Ok(tasks) = shared.db.list_tasks() {
        for t in tasks {
            if t.retry_attempt >= max
                && max > 0
                && matches!(t.status.as_str(), "queued" | "failed" | "needs_attention")
            {
                keys.push(t.key);
            }
        }
    }
    if keys.is_empty() {
        checks.push(Check {
            name: "retries exhausted".into(),
            status: Status::Ok,
            detail: "none".into(),
            hint: None,
        });
    } else {
        checks.push(Check {
            name: "retries exhausted".into(),
            status: Status::Fail,
            detail: keys.join(", "),
            hint: Some("atr retry <key>".into()),
        });
    }
}

fn cooldown_check(checks: &mut Vec<Check>, cfg: &Config, shared: &Shared) {
    let enabled: Vec<_> = cfg.providers.iter().filter(|p| p.enabled).collect();
    if enabled.is_empty() {
        checks.push(Check {
            name: "provider cooldown".into(),
            status: Status::Ok,
            detail: "no enabled providers".into(),
            hint: None,
        });
        return;
    }
    let mut cooling = Vec::new();
    for p in &enabled {
        if shared
            .db
            .provider_blocked(&p.name)
            .ok()
            .is_some_and(|(c, _)| c)
        {
            cooling.push(p.name.as_str());
        }
    }
    if cooling.len() == enabled.len() {
        checks.push(Check {
            name: "provider cooldown".into(),
            status: Status::Fail,
            detail: format!("all {} enabled providers in cooldown", enabled.len()),
            hint: Some("wait for cooldown or atr retry".into()),
        });
    } else {
        checks.push(Check {
            name: "provider cooldown".into(),
            status: Status::Ok,
            detail: if cooling.is_empty() {
                "none cooling".into()
            } else {
                format!("cooling: {}", cooling.join(", "))
            },
            hint: None,
        });
    }
}

fn disk_check(checks: &mut Vec<Check>, cfg: &Config) {
    let min = cfg.selfheal.disk_min_mb;
    if min == 0 {
        checks.push(Check {
            name: "disk".into(),
            status: Status::Ok,
            detail: "check off (disk_min_mb = 0)".into(),
            hint: None,
        });
        return;
    }
    match disk_free_mb(&cfg.state_db) {
        Some(free) if free < min => checks.push(Check {
            name: "disk".into(),
            status: Status::Fail,
            detail: format!("{free} MiB free < {min} MiB"),
            hint: Some("free disk on the state_db volume".into()),
        }),
        Some(free) => checks.push(Check {
            name: "disk".into(),
            status: Status::Ok,
            detail: format!("{free} MiB free"),
            hint: None,
        }),
        None => checks.push(Check {
            name: "disk".into(),
            status: Status::Warn,
            detail: "could not sample free space".into(),
            hint: None,
        }),
    }
}

fn disk_free_mb(path: &Path) -> Option<u64> {
    let disks = sysinfo::Disks::new_with_refreshed_list();
    let abs = path
        .canonicalize()
        .ok()
        .or_else(|| path.parent().and_then(|p| p.canonicalize().ok()))
        .unwrap_or_else(|| path.to_path_buf());
    let mut best: Option<(usize, u64)> = None;
    for d in disks.iter() {
        let mp = d.mount_point();
        if abs.starts_with(mp) {
            let n = mp.as_os_str().len();
            if best.map(|(b, _)| n > b).unwrap_or(true) {
                best = Some((n, d.available_space() / (1024 * 1024)));
            }
        }
    }
    best.map(|(_, mb)| mb)
}

fn snapshot(cfg: &Config, shared: &Shared, report: &Report) -> Value {
    let overall = overall_status(report);
    let host = hostname();
    let checked = rfc3339::millis(Timestamp::now());
    json!({
        "status": overall,
        "host": host,
        "checked_at": checked,
        "checks": report.checks.iter().map(|c| json!({
            "name": c.name,
            "status": match c.status {
                Status::Ok => "ok",
                Status::Warn => "warn",
                Status::Fail => "fail",
            },
            "detail": c.detail,
        })).collect::<Vec<_>>(),
        "selfheal": {
            "enabled": cfg.selfheal.enabled,
            "reap": cfg.selfheal.reap,
            "settle": cfg.selfheal.settle,
            "cleanup": cfg.selfheal.cleanup,
            "check": cfg.selfheal.check,
            "actions": load_actions(&shared.db),
        }
    })
}

fn overall_status(report: &Report) -> &'static str {
    if report.checks.iter().any(|c| c.status == Status::Fail) {
        "fail"
    } else if report.checks.iter().any(|c| c.status == Status::Warn) {
        "warn"
    } else {
        "ok"
    }
}

fn hostname() -> String {
    std::process::Command::new("hostname")
        .output()
        .ok()
        .and_then(|o| {
            let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
            (!s.is_empty()).then_some(s)
        })
        .unwrap_or_else(|| "unknown".into())
}

fn write_health(cfg: &SelfhealConfig, snap: &Value) -> Result<()> {
    fs::create_dir_all(&cfg.health_dir)?;
    let (json_path, txt_path) = health_paths(cfg);
    let body = serde_json::to_vec_pretty(snap)?;
    atomic_write(&json_path, &body)?;
    let line = health_txt_line(snap);
    atomic_write(&txt_path, line.as_bytes())?;
    Ok(())
}

fn health_txt_line(snap: &Value) -> String {
    let status = snap["status"].as_str().unwrap_or("?");
    let at = snap["checked_at"].as_str().unwrap_or("");
    let host = snap["host"].as_str().unwrap_or("");
    let checks = snap["checks"].as_array().map(|a| a.len()).unwrap_or(0);
    let fails: Vec<&str> = snap["checks"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|c| c["status"].as_str() == Some("fail"))
        .filter_map(|c| c["name"].as_str())
        .collect();
    let tag = match status {
        "fail" => "FAIL",
        "warn" => "warn",
        "ok" => "ok",
        other => other,
    };
    if fails.is_empty() {
        format!("{tag} {at} host={host} checks={checks} fail=0\n")
    } else {
        format!(
            "{tag} {at} host={host} checks={checks} fail={}: {}\n",
            fails.len(),
            fails.join(",")
        )
    }
}

fn append_health_log(cfg: &SelfhealConfig, kind: &str, message: &str) {
    append_health_log_limited(cfg, kind, message, HEALTH_LOG_MAX);
}

fn append_health_log_limited(cfg: &SelfhealConfig, kind: &str, message: &str, max: u64) {
    let path = cfg.health_dir.join("health.log");
    if let Err(e) = fs::create_dir_all(&cfg.health_dir) {
        tracing::warn!("selfheal health.log dir: {e}");
        return;
    }
    let line = format!(
        "{} {} {}\n",
        rfc3339::millis(Timestamp::now()),
        kind,
        message
    );
    let size = fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    if size > 0 && size.saturating_add(line.len() as u64) > max {
        let bak = cfg.health_dir.join("health.log.1");
        let _ = fs::remove_file(&bak);
        if let Err(e) = fs::rename(&path, &bak) {
            tracing::warn!("selfheal health.log rotate: {e}");
        }
    }
    match OpenOptions::new().create(true).append(true).open(&path) {
        Ok(mut f) => {
            if let Err(e) = f.write_all(line.as_bytes()) {
                tracing::warn!("selfheal health.log write: {e}");
            }
        }
        Err(e) => tracing::warn!("selfheal health.log open: {e}"),
    }
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    fs::write(&tmp, bytes)?;
    fs::rename(&tmp, path)?;
    Ok(())
}

fn maybe_alert(shared: &Shared, cfg: &Config, snap: &Value) {
    let now = snap["status"].as_str().unwrap_or("ok");
    let prev = shared
        .db
        .meta_get(ALERT_META)
        .ok()
        .flatten()
        .unwrap_or_else(|| "ok".into());
    let _ = shared.db.meta_set(ALERT_META, now);
    let kind = if prev != "fail" && now == "fail" {
        Some("selfheal_failed")
    } else if prev == "fail" && now != "fail" {
        Some("selfheal_recovered")
    } else {
        None
    };
    let Some(kind) = kind else {
        return;
    };
    let line = health_txt_line(snap);
    let msg = line.trim();
    tracing::info!(kind, reason = msg, "selfheal notify");
    append_health_log(&cfg.selfheal, kind, msg);
    if cfg.notify.command.is_empty() {
        return;
    }
    let hook = cfg.notify.clone();
    let extra = vec![
        ("ATR_ALERT_SOURCE", "selfheal".to_string()),
        ("ATR_ALERT_HOST", snap["host"].as_str().unwrap_or("").into()),
    ];
    let message = msg.to_string();
    std::thread::spawn(move || {
        crate::gh_token::run_hook(&hook, kind, &message, &extra);
    });
}

pub fn status_json(shared: &Shared) -> Value {
    let cfg = shared.config.read().unwrap().clone();
    let file = load_health_file(&cfg.selfheal).unwrap_or(json!(null));
    json!({
        "enabled": cfg.selfheal.enabled,
        "reap": cfg.selfheal.reap,
        "settle": cfg.selfheal.settle,
        "cleanup": cfg.selfheal.cleanup,
        "check": cfg.selfheal.check,
        "health_json": health_paths(&cfg.selfheal).0.display().to_string(),
        "health_txt": health_paths(&cfg.selfheal).1.display().to_string(),
        "health": file,
        "actions": load_actions(&shared.db),
    })
}

pub fn status_text(shared: &Shared) -> String {
    let v = status_json(shared);
    let mut out = format!(
        "selfheal enabled={} reap={} settle={} cleanup={} check={}\n",
        v["enabled"], v["reap"], v["settle"], v["cleanup"], v["check"]
    );
    out.push_str(&format!(
        "health.json {}\n",
        v["health_json"].as_str().unwrap_or("")
    ));
    if v["health"].is_null() {
        out.push_str("health: (none yet; atr serve writes it)\n");
    } else {
        out.push_str(&health_txt_line(&v["health"]));
    }
    let acts = v["actions"].as_array().cloned().unwrap_or_default();
    out.push_str(&format!("recent actions: {}\n", acts.len()));
    for a in acts.iter().rev().take(15) {
        out.push_str(&format!(
            "  {}  {}  {}  {}\n",
            a["at"].as_str().unwrap_or(""),
            a["kind"].as_str().unwrap_or(""),
            a["task"].as_str().unwrap_or(""),
            a["reason"].as_str().unwrap_or(""),
        ));
    }
    out
}

pub fn status_line(cfg: &SelfhealConfig) -> Option<String> {
    let file = load_health_file(cfg)?;
    Some(health_txt_line(&file).trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::Shared;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Arc;

    fn harness() -> (tempfile::TempDir, Arc<Shared>) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("p.md"), "p\n").unwrap();
        let health = dir.path().join("health");
        let locks = dir.path().join("locks");
        let evidence = dir.path().join("evidence");
        fs::create_dir_all(&locks).unwrap();
        fs::create_dir_all(&evidence).unwrap();
        let cfg = format!(
            r#"
listen = "127.0.0.1:0"
state_db = "{db}"
worktree_root = "{root}/work"
repo_path = "{root}/repo"
system_prompt_file = "{root}/p.md"
display_timezone = "UTC"
[selfheal]
enabled = true
interval = "1s"
check_interval = "1h"
stuck_timeout = "1s"
artifact_age = "1d"
disk_min_mb = 0
health_dir = "{health}"
lock_files = ["{locks}/m1-runner.lock"]
prune_dirs = ["{evidence}"]
[[provider]]
name = "claude"
kind = "claude"
cli = "/bin/true"
"#,
            db = dir.path().join("s.db").display(),
            root = dir.path().display(),
            health = health.display(),
            locks = locks.display(),
            evidence = evidence.display(),
        );
        let raw: crate::config::RawConfig = toml::from_str(&cfg).unwrap();
        let parsed = crate::config::Config::from_raw(raw).unwrap();
        let shared = Shared::from_config(dir.path().join("c.toml"), parsed).unwrap();
        (dir, shared)
    }

    fn dead_running(shared: &Shared, key: &str) -> i64 {
        shared.db.upsert_task(key, "/repo", None).unwrap();
        shared.db.set_status(key, "running").unwrap();
        let run = shared.db.start_run(key, "claude", "").unwrap();
        shared.db.set_run_pid(run, 1_999_999_991).unwrap();
        shared.db.set_run_owner(run, None).unwrap();
        shared.db.set_task_owner(key, None).unwrap();
        let mid = shared.db.enqueue_message(key, "do work", "cli").unwrap();
        shared.db.mark_consumed(&[mid], run).unwrap();
        run
    }

    #[test]
    fn first_dead_run_is_requeued_second_needs_attention() {
        let (_dir, shared) = harness();
        let run = dead_running(&shared, "task-a");
        tick(&shared).unwrap();
        let t = shared.db.get_task("task-a").unwrap().unwrap();
        assert_eq!(t.status, "queued", "{}", t.result_text);
        let runs = shared.db.list_runs(Some("task-a")).unwrap();
        assert_eq!(
            runs.iter()
                .find(|r| r.id == run)
                .unwrap()
                .outcome
                .as_deref(),
            Some("reaped")
        );
        assert!(!shared.db.pending_messages("task-a").unwrap().is_empty());

        shared.db.set_status("task-a", "running").unwrap();
        let run2 = shared.db.start_run("task-a", "claude", "").unwrap();
        shared.db.set_run_pid(run2, 1_999_999_992).unwrap();
        shared.db.set_run_owner(run2, None).unwrap();
        shared.db.set_task_owner("task-a", None).unwrap();
        tick(&shared).unwrap();
        let t = shared.db.get_task("task-a").unwrap().unwrap();
        assert_eq!(t.status, "needs_attention");
        assert!(
            t.result_text.contains("after one automatic re-queue"),
            "{}",
            t.result_text
        );
    }

    #[test]
    fn restarted_counts_as_the_one_requeue() {
        let (_dir, shared) = harness();
        shared.db.upsert_task("task-r", "/repo", None).unwrap();
        let old = shared.db.start_run("task-r", "claude", "").unwrap();
        shared.db.finish_run(old, "restarted", Some(-2)).unwrap();
        shared.db.set_status("task-r", "running").unwrap();
        let run = shared.db.start_run("task-r", "claude", "").unwrap();
        shared.db.set_run_pid(run, 1_999_999_993).unwrap();
        shared.db.set_run_owner(run, None).unwrap();
        shared.db.set_task_owner("task-r", None).unwrap();
        tick(&shared).unwrap();
        let t = shared.db.get_task("task-r").unwrap().unwrap();
        assert_eq!(t.status, "needs_attention", "{}", t.result_text);
    }

    #[test]
    fn live_pid_is_not_reaped() {
        let (_dir, shared) = harness();
        let child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let pid = child.id();
        shared.db.upsert_task("task-live", "/repo", None).unwrap();
        shared.db.set_status("task-live", "running").unwrap();
        let run = shared.db.start_run("task-live", "claude", "").unwrap();
        shared.db.set_run_pid(run, pid).unwrap();
        shared.db.set_run_owner(run, None).unwrap();
        shared.db.set_task_owner("task-live", None).unwrap();
        tick(&shared).unwrap();
        assert_eq!(
            shared.db.get_task("task-live").unwrap().unwrap().status,
            "running"
        );
        let mut child = child;
        let _ = child.kill();
        let _ = child.wait();
    }

    #[test]
    fn inflight_is_left_alone() {
        let (_dir, shared) = harness();
        dead_running(&shared, "task-if");
        shared.inflight.lock().unwrap().insert(
            "task-if".into(),
            crate::app::InFlight {
                provider: "claude".into(),
                pid: 1,
                run_id: 1,
                kill: std::sync::Arc::new(std::sync::Mutex::new(None)),
                shadow: false,
            },
        );
        tick(&shared).unwrap();
        assert_eq!(
            shared.db.get_task("task-if").unwrap().unwrap().status,
            "running"
        );
    }

    #[test]
    fn sealed_summary_settles_waiting_unsealed_does_not() {
        let (dir, shared) = harness();
        let wt = dir.path().join("work").join("task-s");
        fs::create_dir_all(&wt).unwrap();
        let sum = wt.join("summary.md");
        fs::write(&sum, "done\n").unwrap();
        let mut perm = fs::metadata(&sum).unwrap().permissions();
        perm.set_mode(0o444);
        fs::set_permissions(&sum, perm).unwrap();
        shared.db.upsert_task("task-s", "/repo", None).unwrap();
        shared.db.set_status("task-s", "waiting").unwrap();
        shared
            .db
            .set_worktree("task-s", &wt.display().to_string())
            .unwrap();
        tick(&shared).unwrap();
        assert_eq!(
            shared.db.get_task("task-s").unwrap().unwrap().status,
            "done"
        );

        let wt2 = dir.path().join("work").join("task-open");
        fs::create_dir_all(&wt2).unwrap();
        fs::write(wt2.join("summary.md"), "still writing\n").unwrap();
        shared.db.upsert_task("task-open", "/repo", None).unwrap();
        shared.db.set_status("task-open", "waiting").unwrap();
        shared
            .db
            .set_worktree("task-open", &wt2.display().to_string())
            .unwrap();
        tick(&shared).unwrap();
        assert_eq!(
            shared.db.get_task("task-open").unwrap().unwrap().status,
            "waiting"
        );
    }

    #[test]
    fn marker_seals_writable_summary() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("summary.md");
        fs::write(&p, "notes\n<!-- atr-sealed -->\n").unwrap();
        assert!(is_sealed(&p));
        fs::write(&p, "notes\n").unwrap();
        assert!(!is_sealed(&p));
    }

    #[test]
    fn lock_holding_a_live_pid_is_kept() {
        let (dir, shared) = harness();
        shared.config.write().unwrap().selfheal.stuck_timeout = Duration::from_millis(1);
        let lock = dir.path().join("locks").join("m1-runner.lock");
        let child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        fs::write(&lock, format!("{}\n", child.id())).unwrap();
        std::thread::sleep(Duration::from_millis(20));
        tick(&shared).unwrap();
        assert!(lock.exists(), "live PID lock must stay");
        let mut child = child;
        let _ = child.kill();
        let _ = child.wait();
    }

    #[test]
    fn dead_lock_older_than_stuck_timeout_is_removed() {
        let (dir, shared) = harness();
        shared.config.write().unwrap().selfheal.stuck_timeout = Duration::from_millis(1);
        let lock = dir.path().join("locks").join("m1-runner.lock");
        fs::write(&lock, "1999999991\n").unwrap();
        std::thread::sleep(Duration::from_millis(20));
        tick(&shared).unwrap();
        assert!(!lock.exists(), "dead aged lock must go");
    }

    #[test]
    fn dead_but_fresh_lock_is_kept() {
        let (dir, shared) = harness();
        shared.config.write().unwrap().selfheal.stuck_timeout = Duration::from_secs(10 * 60);
        let lock = dir.path().join("locks").join("m1-runner.lock");
        fs::write(&lock, "1999999991\n").unwrap();
        tick(&shared).unwrap();
        assert!(lock.exists(), "fresh dead lock must wait stuck_timeout");
    }

    #[test]
    fn running_task_worktree_is_not_pruned() {
        let (dir, shared) = harness();
        let wt = dir.path().join("work").join("repo").join("task-run");
        fs::create_dir_all(&wt).unwrap();
        fs::write(wt.join("keep.txt"), "x\n").unwrap();
        shared.db.upsert_task("task-run", "/repo", None).unwrap();
        shared.db.set_status("task-run", "running").unwrap();
        shared
            .db
            .set_worktree("task-run", &wt.display().to_string())
            .unwrap();
        tick(&shared).unwrap();
        assert!(wt.join("keep.txt").exists());
    }

    #[test]
    fn notify_fires_once_on_fail_and_once_on_recovery() {
        let (dir, shared) = harness();
        let out = dir.path().join("hook.log");
        {
            let mut cfg = shared.config.write().unwrap();
            cfg.notify.command = format!("printf '%s\\n' \"$ATR_ALERT_KIND\" >> {}", out.display());
            cfg.notify.timeout = Duration::from_secs(2);
        }
        shared.db.meta_set(ALERT_META, "ok").unwrap();
        let fail = json!({"status":"fail","host":"t","checked_at":"x","checks":[{"name":"disk","status":"fail"}]});
        maybe_alert(&shared, &shared.config.read().unwrap(), &fail);
        maybe_alert(&shared, &shared.config.read().unwrap(), &fail);
        let ok = json!({"status":"ok","host":"t","checked_at":"x","checks":[]});
        maybe_alert(&shared, &shared.config.read().unwrap(), &ok);
        maybe_alert(&shared, &shared.config.read().unwrap(), &ok);
        std::thread::sleep(Duration::from_millis(200));
        let log = fs::read_to_string(&out).unwrap_or_default();
        let fails = log.lines().filter(|l| *l == "selfheal_failed").count();
        let rec = log.lines().filter(|l| *l == "selfheal_recovered").count();
        assert_eq!(fails, 1, "{log}");
        assert_eq!(rec, 1, "{log}");
        let health_log = fs::read_to_string(dir.path().join("health").join("health.log")).unwrap();
        assert_eq!(
            health_log
                .lines()
                .filter(|l| l.contains("selfheal_failed"))
                .count(),
            1,
            "{health_log}"
        );
        assert_eq!(
            health_log
                .lines()
                .filter(|l| l.contains("selfheal_recovered"))
                .count(),
            1,
            "{health_log}"
        );
    }

    #[test]
    fn health_log_rotates_near_the_size_cap() {
        let (_dir, shared) = harness();
        let cfg = shared.config.read().unwrap().selfheal.clone();
        let path = cfg.health_dir.join("health.log");
        fs::create_dir_all(&cfg.health_dir).unwrap();
        fs::write(&path, "x".repeat(80)).unwrap();
        append_health_log_limited(&cfg, "selfheal_failed", "FAIL disk", 100);
        assert!(cfg.health_dir.join("health.log.1").exists());
        let fresh = fs::read_to_string(&path).unwrap();
        assert!(fresh.contains("selfheal_failed"), "{fresh}");
        assert!(fresh.len() < 80, "{}", fresh.len());
    }

    #[test]
    fn health_txt_format() {
        let snap = json!({
            "status": "fail",
            "host": "mini",
            "checked_at": "2026-01-01T00:00:00.000Z",
            "checks": [
                {"name": "disk", "status": "fail"},
                {"name": "stuck runs", "status": "fail"},
                {"name": "config", "status": "ok"}
            ]
        });
        let line = health_txt_line(&snap);
        assert!(line.starts_with("FAIL "), "{line}");
        assert!(line.contains("host=mini"), "{line}");
        assert!(line.contains("disk,stuck runs"), "{line}");
    }

    #[test]
    fn health_files_are_written_atomically() {
        let (_dir, shared) = harness();
        let cfg = shared.config.read().unwrap().selfheal.clone();
        let snap = json!({
            "status": "ok",
            "host": "t",
            "checked_at": "2026-01-01T00:00:00.000Z",
            "checks": []
        });
        write_health(&cfg, &snap).unwrap();
        let (jp, tp) = health_paths(&cfg);
        assert!(jp.exists());
        assert!(tp.exists());
        assert!(!jp.with_extension("tmp").exists());
        let body: Value = serde_json::from_str(&fs::read_to_string(&jp).unwrap()).unwrap();
        assert_eq!(body["status"], "ok");
        assert!(fs::read_to_string(&tp).unwrap().starts_with("ok "));
    }

    #[test]
    fn old_prune_dir_is_removed_running_key_kept() {
        let (dir, shared) = harness();
        shared.config.write().unwrap().selfheal.artifact_age = Duration::from_millis(1);
        let evidence = dir.path().join("evidence");
        let old = evidence.join("old-task");
        let live = evidence.join("task-run");
        fs::create_dir_all(&old).unwrap();
        fs::create_dir_all(&live).unwrap();
        fs::write(old.join("log.txt"), "x\n").unwrap();
        fs::write(live.join("log.txt"), "x\n").unwrap();
        shared.db.upsert_task("task-run", "/repo", None).unwrap();
        shared.db.set_status("task-run", "running").unwrap();
        shared.inflight.lock().unwrap().insert(
            "task-run".into(),
            crate::app::InFlight {
                provider: "claude".into(),
                pid: 1,
                run_id: 1,
                kill: std::sync::Arc::new(std::sync::Mutex::new(None)),
                shadow: false,
            },
        );
        std::thread::sleep(Duration::from_millis(20));
        tick(&shared).unwrap();
        assert!(!old.exists(), "old evidence dir must go");
        assert!(live.join("log.txt").exists(), "running task dir must stay");
    }
}
