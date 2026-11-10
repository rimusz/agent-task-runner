use crate::config::{load_config, Config};
use crate::db::Db;
use crate::http;
use crate::providers::{check_version, hit_ratio_from_sums, VersionCheck};
use crate::runner::terminate_tree_now;
use crate::scheduler::{self, reconcile};
use crate::status::{self, Caps, Enrich, GroupFilter, GroupRow};
use anyhow::{bail, Result};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::io::{self, IsTerminal, Read, Write};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::sync::Notify;

#[derive(Debug, Clone)]
pub struct InFlight {
    pub provider: String,
    pub pid: u32,
    pub run_id: i64,
    pub kill: Arc<Mutex<Option<u32>>>,
    pub shadow: bool,
}

pub struct Shared {
    pub config_path: PathBuf,
    pub config: RwLock<Config>,
    pub db: Db,
    pub inflight: Mutex<HashMap<String, InFlight>>,
    pub kick: Notify,
    pub intake_paused: AtomicBool,
    /// Last time a task was started while `[host]` gating is on (settle gap).
    pub last_host_start: Mutex<Option<std::time::Instant>>,
}

/// Result of [`Shared::interrupt_live`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InterruptOutcome {
    /// The live run was stopped; the scheduler resumes the stored session
    /// with any pending messages (or leaves the task waiting if none).
    Interrupted,
    /// Nothing was running; pending messages start a normal turn.
    NotRunning,
    /// The provider cannot resume a session, so killing the turn would lose
    /// its work. Left running; pending messages wait for it to finish.
    NotResumable,
}

/// Filters applied when building `status_json` (CLI `--all` / `--since` / `--repo`).
#[derive(Debug, Clone, Default)]
pub struct StatusJsonOpts<'a> {
    pub repo: Option<&'a str>,
    pub include_reflect: bool,
    pub since: Option<Duration>,
}

/// CLI flags for `atr status` (human view, `--json`, `--check`, `--watch`).
#[derive(Debug, Clone, Default)]
pub struct StatusCli {
    pub json: bool,
    pub repo: Option<String>,
    pub failed: bool,
    pub running: bool,
    pub waiting: bool,
    pub done: bool,
    pub since: Option<Duration>,
    pub limit: Option<usize>,
    pub watch: Option<u64>,
    pub all: bool,
    pub check: bool,
}

impl StatusCli {
    fn query(&self) -> StatusJsonOpts<'_> {
        StatusJsonOpts {
            repo: self.repo.as_deref(),
            include_reflect: self.all,
            since: self.since,
        }
    }

    fn filter(&self) -> GroupFilter {
        GroupFilter::from_flags(self.failed, self.running, self.waiting, self.done)
    }

    fn caps(&self) -> Caps {
        status::cli_caps(self.all, self.limit)
    }
}

impl InterruptOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Interrupted => "interrupted",
            Self::NotRunning => "not_running",
            Self::NotResumable => "not_resumable",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubmitOutcome {
    Accepted,
    Queued,
}

#[derive(Debug, Clone, Default)]
pub struct SubmitOpts {
    pub workspace_kind: Option<String>,
    pub output_kind: Option<String>,
    pub output_spec: Option<String>,
    pub source_kind: Option<String>,
    pub title: Option<String>,
    /// When true, an empty repo stays empty (answer-only) instead of falling
    /// back to `repo_path`.
    pub allow_empty_repo: bool,
    /// Preferred provider. Stored before the task is queued so a running
    /// `serve` cannot dispatch it to the default provider first.
    pub provider_hint: Option<String>,
    /// Required provider capabilities (`browser`, `desktop`). Stored before
    /// queueing so the scheduler never hands the task to an incapable provider.
    pub needs: Vec<String>,
    /// Task keys this one waits for (`after:`). Empty = none.
    pub after: Vec<String>,
    /// Optional CLI `--agent` for grok/claude (Telegram `/add --agent`).
    pub agent: Option<String>,
    /// Proof/verification: no file changes is success (`checked`), not no-op.
    pub check_only: bool,
}

impl Shared {
    pub fn open(config_path: PathBuf) -> Result<Arc<Self>> {
        let config = load_config(&config_path)?;
        Self::from_config(config_path, config)
    }

    pub fn from_config(config_path: PathBuf, config: Config) -> Result<Arc<Self>> {
        crate::dashboard::mark_started();
        let db = Db::open(&config.state_db)?;
        let paused = db.meta_get("intake_paused").ok().flatten().as_deref() == Some("1");
        Ok(Arc::new(Shared {
            config_path,
            config: RwLock::new(config),
            db,
            inflight: Mutex::new(HashMap::new()),
            kick: Notify::new(),
            intake_paused: AtomicBool::new(paused),
            last_host_start: Mutex::new(None),
        }))
    }

    pub fn reload(&self) -> Result<Config> {
        let cfg = load_config(&self.config_path)?;
        *self.config.write().unwrap() = cfg.clone();
        self.kick.notify_waiters();
        tracing::info!("config reloaded from {}", self.config_path.display());
        Ok(cfg)
    }

    pub fn submit(
        &self,
        key: &str,
        body: &str,
        source: &str,
        repo: Option<&str>,
        issue: Option<i64>,
        shadow: bool,
        tests_ok: bool,
    ) -> Result<SubmitOutcome> {
        self.submit_with(
            key,
            body,
            source,
            repo,
            issue,
            shadow,
            tests_ok,
            SubmitOpts::default(),
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn submit_with(
        &self,
        key: &str,
        body: &str,
        source: &str,
        repo: Option<&str>,
        issue: Option<i64>,
        shadow: bool,
        tests_ok: bool,
        opts: SubmitOpts,
    ) -> Result<SubmitOutcome> {
        self.submit_with_model(key, body, source, repo, issue, shadow, tests_ok, opts, None)
    }

    /// [`Self::submit_with`] plus a per-task model for `opts.provider_hint`,
    /// stored before queueing like the hint. Callers validate it first
    /// ([`crate::task::validate_overrides`]).
    #[allow(clippy::too_many_arguments)]
    pub fn submit_with_model(
        &self,
        key: &str,
        body: &str,
        source: &str,
        repo: Option<&str>,
        issue: Option<i64>,
        shadow: bool,
        tests_ok: bool,
        opts: SubmitOpts,
        model: Option<&str>,
    ) -> Result<SubmitOutcome> {
        let model = model.map(str::trim).filter(|s| !s.is_empty());
        let cfg = self.config.read().unwrap().clone();
        let existing = self.db.get_task(key)?;
        let source_kind = opts
            .source_kind
            .clone()
            .or_else(|| {
                crate::task::SourceKind::parse(source)
                    .ok()
                    .map(|k| k.as_str().to_string())
            })
            .unwrap_or_else(|| {
                crate::task::infer_source_kind(issue, source)
                    .as_str()
                    .into()
            });
        if existing.is_none() {
            if let Ok(kind) = crate::task::SourceKind::parse(&source_kind) {
                if !cfg.intake_allows(kind)
                    && !matches!(
                        source,
                        "review" | "verify" | "shadow" | "shadow-comment" | "plan"
                    )
                {
                    bail!("intake source {kind:?} is disabled");
                }
            }
        }
        let workspace_kind = opts.workspace_kind.clone().or_else(|| {
            existing
                .as_ref()
                .map(|t| t.workspace_kind.clone())
                .filter(|s| !s.is_empty())
        });
        let none_ws = workspace_kind.as_deref() == Some("none") || opts.allow_empty_repo;
        let repo = match repo.map(str::trim).filter(|s| !s.is_empty()) {
            Some(s) => s.to_string(),
            None if existing
                .as_ref()
                .and_then(|t| t.repo.as_deref())
                .is_some_and(|r| !r.is_empty()) =>
            {
                String::new()
            }
            None if none_ws && existing.is_none() => String::new(),
            None if shadow => crate::shadow::shadow_repo_path(&cfg).display().to_string(),
            None => cfg.repo_path.display().to_string(),
        };
        self.db.upsert_task_spec(
            key,
            &repo,
            issue,
            workspace_kind.as_deref(),
            opts.output_kind.as_deref(),
            opts.output_spec.as_deref(),
            Some(source_kind.as_str()),
        )?;
        // A named model is one run on one provider: the planning turn would pin
        // the task to the driver and its subtasks would lose the override.
        if existing.is_none() && model.is_none() && crate::plan::should_plan(&cfg, body) {
            let _ = self.db.set_task_role(key, "plan");
        }
        if let Some(title) = opts.title.as_deref().filter(|s| !s.is_empty()) {
            let _ = self.db.set_task_title(key, title);
        }
        if shadow {
            self.db.mark_shadow(key)?;
        }
        if tests_ok {
            self.db.set_protect_tests_skip(key, true)?;
        }
        if opts.check_only {
            self.db.set_check_only(key, true)?;
        }
        if let Some(h) = opts
            .provider_hint
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            self.db.set_provider_hint(key, h)?;
        }
        if let Some(m) = model {
            self.db.set_model_override(key, m)?;
        }
        if let Some(a) = opts
            .agent
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            self.db.set_task_agent(key, a)?;
        }
        if !opts.needs.is_empty() {
            let needs = crate::task::parse_needs(&opts.needs)?;
            self.db.set_task_needs(key, &needs.join(","))?;
        }
        if existing.as_ref().is_some_and(|t| t.status == "held") {
            self.db.clear_hold(key)?;
            if opts.after.is_empty() {
                self.db.set_task_deps(key, "")?;
            }
        }
        if !opts.after.is_empty() {
            crate::deps::set_after(&self.db, key, &opts.after)?;
        }
        let _ = crate::scheduler::reclaim_dead_open_run(self, key);
        let running = self.inflight.lock().unwrap().contains_key(key)
            || self.db.task_is_running(key)?
            || self.db.open_run_for_task(key)?.is_some();
        // A new turn on a finished task restarts its wall clock (internal
        // follow-ups such as review/verify/plan do not).
        if !running
            && !matches!(
                source,
                "review" | "verify" | "shadow" | "shadow-comment" | "plan" | "compare"
            )
            && existing.as_ref().is_some_and(|t| {
                matches!(
                    t.status.as_str(),
                    "waiting" | "done" | "failed" | "needs_attention" | "cancelled" | "held"
                )
            })
        {
            self.db.restart_task_clock(key)?;
        }
        self.db.enqueue_message(key, body, source)?;
        if running {
            tracing::info!(task = %key, source, "queued follow-up; live run not interrupted");
            self.kick.notify_waiters();
            return Ok(SubmitOutcome::Queued);
        }
        // Atomic: the scheduler may claim the task between the check above
        // and here; never overwrite its `running` with `queued`.
        self.db.queue_unless_running(key)?;
        self.kick.notify_waiters();
        Ok(SubmitOutcome::Accepted)
    }

    pub fn set_intake_paused(&self, paused: bool) {
        self.intake_paused.store(paused, Ordering::Relaxed);
        let _ = self
            .db
            .meta_set("intake_paused", if paused { "1" } else { "0" });
    }

    /// Re-queue a finished/failed/cancelled task (Telegram /retry, `atr retry`,
    /// POST /tasks/:key/retry). A running task is refused.
    pub fn retry(&self, key: &str) -> Result<()> {
        let Some(t) = self.db.get_task(key)? else {
            bail!("unknown task {key}");
        };
        if t.status == "running" || self.inflight.lock().unwrap().contains_key(key) {
            bail!("task {key} is running");
        }
        // A retry is a fresh attempt: restart the wall clock, and (unless the
        // task is strictly pinned, e.g. a compare arm) pick the provider again
        // from the current config instead of sticking to the last one.
        self.db.restart_task_clock(key)?;
        self.db.clear_retry(key)?;
        // Fresh session for a provider without a shell: a resumed local-model
        // session tends to say "already done" and stop. Providers with a shell
        // resume their session as in v0.1.34, except a planning run: its session
        // is read-only and the next run must not inherit it. The worktree is kept.
        let no_shell = self
            .config
            .read()
            .unwrap()
            .provider(t.provider.as_deref().unwrap_or_default())
            .is_some_and(|p| !p.has_shell());
        if (no_shell || t.role == "plan") && !crate::telegram::is_assistant_key(key) {
            self.db.clear_task_sessions(key)?;
        }
        if t.status == "held" {
            self.db.clear_hold(key)?;
            self.db.set_task_deps(key, "")?;
        }
        if !t.pin_strict {
            self.reresolve_provider(&t)?;
        }
        // `runnable_keys` also requires an unconsumed message. A finished run
        // marks its prompt consumed; without unconsuming, retry would sit
        // queued forever (the scheduler ticks but finds nothing to start).
        if self.db.pending_messages(key)?.is_empty() {
            if let Some(run_id) = self.db.last_finished_run_id(key)? {
                self.db.unconsume_run(run_id)?;
            }
        }
        self.db.set_status(key, "queued")?;
        self.kick.notify_waiters();
        Ok(())
    }

    /// Drop the task's current provider pin and failover history so the
    /// scheduler chooses again from the current config. A Telegram assistant
    /// task is re-pinned to what `[telegram.assistant]` picks now. Per-provider
    /// sessions are kept here; `retry` clears them first (except assistant chats).
    fn reresolve_provider(&self, t: &crate::db::Task) -> Result<()> {
        self.db.set_tried(&t.key, None)?;
        self.db.set_stall_retries(&t.key, 0)?;
        self.db.set_provider_session(&t.key, None, None)?;
        if crate::telegram::is_assistant_key(&t.key) {
            if let Some(p) = crate::scheduler::assistant_provider_now(self) {
                let sid = self.db.task_session(&t.key, &p).ok().flatten();
                self.db
                    .set_provider_session(&t.key, Some(&p), sid.as_deref())?;
            }
        } else if crate::tui::is_tui_key(&t.key) {
            if let Some(p) = crate::tui::tui_provider_now(self, &t.key) {
                let sid = self.db.task_session(&t.key, &p).ok().flatten();
                self.db
                    .set_provider_session(&t.key, Some(&p), sid.as_deref())?;
            }
        }
        Ok(())
    }

    pub fn cancel_ok_line(key: &str, force: bool) -> String {
        if force {
            format!("cancelled {key} (force)")
        } else {
            format!("cancelled {key}")
        }
    }

    /// Stop a task's live turn without cancelling the task. Enqueue the new
    /// message first (`submit`); the scheduler then sees outcome
    /// `interrupted`, keeps the provider session (no cooldown, no failover),
    /// and resumes it with the pending messages plus a "continue your
    /// unfinished task" preamble. If the run never reported a resumable
    /// session, its messages are unconsumed so a fresh run sees them again.
    pub fn interrupt_live(&self, key: &str) -> Result<InterruptOutcome> {
        let Some((run_id, provider, pid)) = self.live_run(key)? else {
            return Ok(InterruptOutcome::NotRunning);
        };
        let resumable = {
            let cfg = self.config.read().unwrap();
            cfg.providers
                .iter()
                .find(|p| p.name == provider)
                .map(crate::providers::supports_resume)
                .unwrap_or(false)
        };
        if !resumable {
            tracing::info!(task = %key, %provider, "interrupt skipped; provider cannot resume");
            return Ok(InterruptOutcome::NotResumable);
        }
        tracing::info!(task = %key, run = run_id, "interrupt live run");
        // Mark first so the scheduler reads `interrupted`, not a failure.
        self.db.finish_run(run_id, "interrupted", Some(-2))?;
        if pid > 0 {
            terminate_tree_now(pid);
        }
        self.kick.notify_waiters();
        Ok(InterruptOutcome::Interrupted)
    }

    /// (run id, provider, pid) of the task's live run: this process's
    /// in-flight slot, else an open run with a live pid in the DB (the run
    /// belongs to `atr serve` while the caller is e.g. stdio `atr mcp`).
    fn live_run(&self, key: &str) -> Result<Option<(i64, String, u32)>> {
        if let Some(info) = self.inflight.lock().unwrap().get(key).cloned() {
            if info.run_id <= 0 {
                return Ok(None);
            }
            let pid = info
                .kill
                .lock()
                .ok()
                .and_then(|g| *g)
                .filter(|p| *p > 0)
                .unwrap_or(info.pid);
            return Ok(Some((info.run_id, info.provider, pid)));
        }
        let Some(run_id) = self.db.open_run_for_task(key)? else {
            return Ok(None);
        };
        let row = self
            .db
            .list_runs(Some(key))?
            .into_iter()
            .rev()
            .find(|r| r.id == run_id);
        Ok(row.and_then(|r| {
            let pid = r.pid.filter(|p| *p > 0 && crate::db::pid_alive(*p))?;
            Some((r.id, r.provider, pid as u32))
        }))
    }

    pub fn cancel(&self, key: &str, force: bool) -> Result<()> {
        let running = self.inflight.lock().unwrap().get(key).cloned();
        if let Some(info) = running {
            if !force {
                bail!("task {key} is running; re-run with --force to kill the live process group");
            }
            tracing::warn!(task = %key, "force-cancel live run");
            let pid = info
                .kill
                .lock()
                .ok()
                .and_then(|g| *g)
                .filter(|p| *p > 0)
                .unwrap_or(info.pid);
            if pid > 0 {
                terminate_tree_now(pid);
            }
            self.db.finish_run(info.run_id, "cancelled", Some(-2))?;
            self.db.set_status(key, "failed")?;
            self.inflight.lock().unwrap().remove(key);
            let _ = crate::deps::hold_dependents(&self.db, key);
            return Ok(());
        }
        if let Some(task) = self.db.get_task(key)? {
            if task.status == "running" && !force {
                bail!("task {key} is running; re-run with --force to cancel");
            }
            self.db.set_status(key, "failed")?;
            let _ = crate::deps::hold_dependents(&self.db, key);
            Ok(())
        } else {
            bail!("unknown task {key}")
        }
    }

    pub fn status_json(&self) -> Value {
        self.status_json_filtered(None)
    }

    pub fn status_json_filtered(&self, repo: Option<&str>) -> Value {
        self.status_json_opts(&StatusJsonOpts {
            repo,
            ..StatusJsonOpts::default()
        })
    }

    pub fn status_json_opts(&self, opts: &StatusJsonOpts<'_>) -> Value {
        let cfg = self.config.read().unwrap().clone();
        let inflight = self.inflight.lock().unwrap().clone();
        let tasks = if opts.include_reflect {
            self.db.list_all_tasks().unwrap_or_default()
        } else {
            self.db.list_tasks().unwrap_or_default()
        };
        let totals = self.db.usage_totals(1).unwrap_or_default();
        let clock = crate::clock::Clock::system();
        let mut top_reset: Option<chrono::DateTime<chrono::Utc>> = None;
        let providers: Vec<Value> = cfg
            .providers
            .iter()
            .enumerate()
            .map(|(idx, p)| {
                let order = idx + 1;
                let (cooling, degraded) =
                    self.db.provider_blocked(&p.name).unwrap_or((false, false));
                let n = {
                    let mut keys = std::collections::HashSet::new();
                    for (k, i) in &inflight {
                        if i.provider == p.name {
                            keys.insert(k.clone());
                        }
                    }
                    if let Ok(runs) = self.db.live_open_runs(Some(&p.name)) {
                        for r in runs {
                            keys.insert(r.task_key);
                        }
                    }
                    keys.len()
                };
                let usage = totals.iter().find(|u| u.provider == p.name);
                let hit = usage.and_then(|u| {
                    hit_ratio_from_sums(&p.kind, u.input, u.cache_read, u.cache_write)
                });
                let windows = crate::quota::snapshots(&self.db, p, &clock).unwrap_or_default();
                let reported_over =
                    crate::reported::reported_over(&self.db, p, &clock).unwrap_or(false);
                let skip_new = cooling
                    || crate::quota::limit_reset_active(&self.db, &p.name, &clock).unwrap_or(false)
                    || windows.iter().any(|w| w.over)
                    || reported_over;
                let cli_reset = self.db.quota_reset_at(&p.name).ok().flatten();
                let mut reset_at =
                    crate::quota::earliest_reset(&windows, cli_reset.as_deref(), clock.now());
                for t in crate::reported::reported_resets(&self.db, &p.name, clock.now()) {
                    reset_at = Some(match reset_at {
                        Some(cur) => cur.min(t),
                        None => t,
                    });
                }
                if cooling {
                    if let Some(until) = self.db.cooldown_until(&p.name).ok().flatten() {
                        if let Ok(t) = chrono::DateTime::parse_from_rfc3339(&until) {
                            let t = t.with_timezone(&chrono::Utc);
                            reset_at = Some(match reset_at {
                                Some(cur) => cur.min(t),
                                None => t,
                            });
                        }
                    }
                }
                if let Some(t) = reset_at {
                    top_reset = Some(match top_reset {
                        Some(cur) => cur.min(t),
                        None => t,
                    });
                }
                let enabled = self
                    .db
                    .enabled_override(&p.name)
                    .ok()
                    .flatten()
                    .unwrap_or(p.enabled);
                json!({
                    "order": order,
                    "name": p.name,
                    "kind": p.kind,
                    "enabled": enabled,
                    "inflight": n,
                    "max_parallel": p.max_parallel,
                    "cooling": cooling,
                    "degraded": degraded,
                    "cache_hit": hit,
                    "skip_new": skip_new,
                    "quota_reset": reset_at.map(|t| cfg.display_timezone.format(t)),
                    "quota": windows
                        .iter()
                        .map(|w| w.to_json_tz(Some(&cfg.display_timezone)))
                        .collect::<Vec<_>>(),
                    "reported_usage": if p.reported_usage.is_empty() {
                        "none"
                    } else {
                        p.reported_usage.as_str()
                    },
                    "usage": crate::reported::format_provider_usage(
                        &crate::reported::provider_usage_view(&self.db, &p.name),
                        &cfg.display_timezone,
                        clock.now(),
                    ),
                    "reported": crate::reported::reported_json(
                        &self.db,
                        &p.name,
                        &cfg.display_timezone,
                        clock.now(),
                    ),
                })
            })
            .collect();
        let cache = crate::usage::report(self, &crate::usage::Period::Days(7))
            .ok()
            .map(|r| {
                json!({
                    "headline_min_input": r.headline_min_input,
                    "headline_hit": r.headline.hit,
                    "first_run_hit": r.first_run.hit,
                    "resume_hit": r.resume.hit,
                    "short_hit": r.short.hit,
                    "headline_runs": r.headline.runs,
                    "first_run_runs": r.first_run.runs,
                    "resume_runs": r.resume.runs,
                    "short_runs": r.short.runs,
                })
            })
            .unwrap_or_else(|| json!({"headline_min_input": cfg.cache.headline_min_input}));
        let host_snap = crate::host::sample();
        let host = json!({
            "enabled": cfg.host.enabled,
            "load": host_snap.load,
            "cores": host_snap.cores,
            "available_memory_mb": host_snap.available_memory_mb,
            "waiting": crate::host::decide(&cfg.host, &host_snap).map(|w| w.reason),
            "line": crate::host::format_line(&cfg.host, &host_snap),
        });
        let cutoff = opts.since.map(|d| {
            chrono::Utc::now()
                - chrono::Duration::from_std(d).unwrap_or_else(|_| chrono::Duration::days(30))
        });
        let inflight_keys: HashSet<String> = inflight.keys().cloned().collect();
        let live_open: HashSet<String> = self
            .db
            .live_open_runs(None)
            .unwrap_or_default()
            .into_iter()
            .map(|r| r.task_key)
            .collect();
        let mut open_by_key: HashMap<String, crate::db::RunRow> = HashMap::new();
        for r in self.db.open_runs().unwrap_or_default() {
            open_by_key.entry(r.task_key.clone()).or_insert(r);
        }
        let secrets = crate::dashboard::collect_secrets(&cfg);
        let tz = &cfg.display_timezone;
        let mut task_vals = Vec::new();
        let mut grouped = Vec::new();
        for t in &tasks {
            if let Some(f) = opts.repo {
                if !cfg.task_matches_repo(t, f) {
                    continue;
                }
            }
            if let Some(cut) = cutoff {
                if let Ok(u) = chrono::DateTime::parse_from_rfc3339(&t.updated) {
                    if u.with_timezone(&chrono::Utc) < cut {
                        continue;
                    }
                }
            }
            let val = crate::output::with_task_model(
                json!({
                    "key": t.key,
                    "status": t.status,
                    "provider": t.provider,
                    "session_id": t.session_id,
                    "shadow": t.shadow,
                    "repo": cfg.task_repo_name(t),
                    "parent": t.parent_key,
                    "role": t.role,
                    "title": t.title,
                    "deps": t.deps,
                    "after": crate::deps::split_deps(&t.deps),
                    "issue": t.issue,
                    "workspace": t.workspace_kind,
                    "output": t.output_kind,
                    "source": t.source_kind,
                    "retry_attempt": t.retry_attempt,
                    "retry_next_at": t.retry_next_at,
                    "retry_reason": t.retry_reason,
                    "hold_reason": t.hold_reason,
                }),
                t,
            );
            task_vals.push(val.clone());
            let live = status::is_live(&t.status, &t.key, &inflight_keys, &live_open);
            let bucket = status::classify(&t.status, live);
            let last = self
                .db
                .list_runs(Some(&t.key))
                .ok()
                .and_then(|rs| rs.into_iter().last());
            let open = open_by_key.get(&t.key);
            let why = match bucket {
                status::Bucket::Failing => status::failing_why(
                    &t.result_text,
                    last.as_ref().and_then(|r| r.outcome.as_deref()),
                ),
                status::Bucket::Waiting => status::waiting_why(t),
                status::Bucket::Running | status::Bucket::Done | status::Bucket::Other => {
                    String::new()
                }
            };
            let (elapsed, elapsed_s) = match open {
                Some(r) => (
                    Some(crate::dashboard::format_elapsed(&r.started)),
                    status::elapsed_secs(&r.started),
                ),
                None => (None, None),
            };
            let progress = crate::dashboard::first_line(
                &crate::dashboard::progress_for(self, t, &secrets),
                80,
            );
            let model = if bucket == status::Bucket::Running {
                let from_run = open.and_then(|r| r.model.clone()).filter(|s| !s.is_empty());
                Some(from_run.unwrap_or_else(|| {
                    if t.model_override.is_empty() {
                        "-".into()
                    } else {
                        t.model_override.clone()
                    }
                }))
            } else {
                None
            };
            let item = status::enrich_item(
                val,
                Enrich {
                    exit_code: last.as_ref().and_then(|r| r.exit_code),
                    last_outcome: last.as_ref().and_then(|r| r.outcome.clone()),
                    elapsed,
                    elapsed_s,
                    progress: if progress.is_empty() {
                        None
                    } else {
                        Some(progress)
                    },
                    why,
                    updated: tz.format_str(&t.updated),
                    model,
                    stall_retries: t.stall_retries,
                },
            );
            grouped.push(GroupRow {
                bucket,
                updated: t.updated.clone(),
                status: t.status.clone(),
                item,
            });
        }
        let groups = status::assemble_groups(grouped);
        json!({
            "version": crate::VERSION,
            "listen": cfg.listen,
            "timezone": cfg.display_timezone.label,
            "quota_reset": top_reset.map(|t| cfg.display_timezone.format(t)),
            "intake_paused": self.intake_paused.load(Ordering::Relaxed),
            "github_auth": crate::gh_token::status_json(&self.config_path, chrono::Utc::now()),
            "telegram_health": crate::telegram_health::status_json(&cfg, &self.config_path, chrono::Utc::now()),
            "running": inflight.keys().cloned().collect::<Vec<_>>(),
            "providers": providers,
            "shadow": {
                "enabled": cfg.shadow.enabled,
                "inflight": inflight.values().filter(|i| i.shadow).count(),
                "max_parallel": cfg.shadow.max_parallel,
            },
            "repos": crate::dashboard::repo_rows(&cfg, &self.config_path),
            "telegram_assistant": crate::dashboard::telegram_assistant(&cfg, &self.db),
            "tasks": task_vals,
            "groups": groups,
            "host": host,
            "retry": {
                "enabled": cfg.retry.enabled,
                "max_attempts": cfg.retry.max_attempts,
                "backoff": cfg.retry.backoff_raw,
            },
            "restart": {
                "resume": cfg.restart.resume,
            },
            "driver": cfg.driver_name(),
            "planner": cfg.planner.mode.as_str(),
            "review": cfg.review.enabled,
            "cache": cache,
        })
    }
}

pub async fn serve(shared: Arc<Shared>) -> Result<()> {
    let k = scheduler::reap_orphans_at_start(&shared)?;
    if k > 0 {
        tracing::warn!(k, "killed orphaned CLI runs from a previous serve");
    }
    let resumed = scheduler::recover_restart_orphans(&shared)?;
    if resumed > 0 {
        tracing::info!(resumed, "resumed tasks interrupted by atr restart");
    }
    let n = reconcile(&shared)?;
    if n > 0 {
        tracing::warn!(n, "reconciled orphan runs");
    }
    let cfg = shared.config.read().unwrap().clone();
    cfg.validate_for_serve()?;
    match crate::mcp::ensure_token(&cfg.mcp) {
        Ok(true) => tracing::info!(
            file = %cfg.mcp.token_file.display(),
            "created HTTP token file (0600) for write routes and /mcp"
        ),
        Ok(false) => {}
        Err(e) => tracing::warn!("could not create HTTP token file: {e:#}"),
    }
    crate::opencode_cfg::apply_ollama_keep_alive(&cfg);
    crate::util::write_pid(&cfg)?;
    let _ = shared.db.register_owner(crate::owner::KIND_SERVE);
    if cfg.shadow.enabled {
        crate::shadow::validate_github_cli(&cfg.github.cli)?;
    }
    if !cfg.listen_is_loopback() {
        tracing::warn!(listen = %cfg.listen, "HTTP listen is not loopback");
    }
    crate::providers::warn_versions_recorded(&cfg.providers, Some(&shared.config_path)).await;
    crate::reported::refresh_grok_billing_at_start(
        &shared.db,
        &cfg.providers,
        &crate::clock::Clock::system(),
    )
    .await;

    let listener = TcpListener::bind(&cfg.listen).await?;
    let addr = listener.local_addr()?;
    tracing::info!(%addr, "atr listening");
    if cfg.dashboard.enabled {
        let port = addr.port();
        tracing::info!(
            bind = %cfg.dashboard.bind,
            "dashboard http://{}:{}/",
            cfg.dashboard.bind,
            port
        );
        if let Some(extra) = crate::dashboard::extra_bind(&cfg.listen, &cfg.dashboard.bind) {
            match TcpListener::bind(&extra).await {
                Ok(extra_l) => {
                    let dash = shared.clone();
                    tokio::spawn(async move {
                        if let Err(e) =
                            axum::serve(extra_l, crate::http::dashboard_only(dash)).await
                        {
                            tracing::warn!("dashboard bind: {e:#}");
                        }
                    });
                    tracing::info!(%extra, "dashboard extra bind");
                }
                Err(e) => tracing::warn!(%extra, "dashboard bind failed: {e}"),
            }
        }
    }

    let http = shared.clone();
    let sched = shared.clone();
    let gh = shared.clone();
    let shadow = shared.clone();
    let tg = shared.clone();
    let inbox = shared.clone();
    let gc = shared.clone();
    let hup = shared.clone();
    let rec = shared.clone();
    let kw = shared.clone();

    tokio::select! {
        r = axum::serve(listener, http::router(http)) => r?,
        _ = scheduler::reconcile_loop(rec) => {}
        _ = scheduler::scheduler_loop(sched) => {}
        _ = crate::github::poll_loop(gh) => {}
        _ = crate::shadow::poll_loop(shadow) => {}
        _ = crate::telegram::bot_loop(tg) => {}
        _ = crate::inbox::poll_loop(inbox) => {}
        _ = crate::gc::daily_loop(gc) => {}
        _ = sighup_loop(hup) => {}
        _ = crate::keepwarm::keepwarm_loop(kw) => {}
    }
    Ok(())
}

pub async fn serve_background(shared: Arc<Shared>) -> Result<SocketAddr> {
    let n = reconcile(&shared)?;
    if n > 0 {
        tracing::warn!(n, "reconciled orphan runs");
    }
    let cfg = shared.config.read().unwrap().clone();
    cfg.validate_for_serve()?;
    crate::opencode_cfg::apply_ollama_keep_alive(&cfg);
    crate::util::write_pid(&cfg)?;
    let _ = shared.db.register_owner(crate::owner::KIND_SERVE);
    if cfg.shadow.enabled {
        crate::shadow::validate_github_cli(&cfg.github.cli)?;
    }
    crate::providers::warn_versions_recorded(&cfg.providers, Some(&shared.config_path)).await;
    crate::reported::refresh_grok_billing_at_start(
        &shared.db,
        &cfg.providers,
        &crate::clock::Clock::system(),
    )
    .await;
    let listener = TcpListener::bind(&cfg.listen).await?;
    let addr = listener.local_addr()?;
    let http = shared.clone();
    tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, http::router(http)).await {
            tracing::error!("http: {e:#}");
        }
    });
    let sched = shared.clone();
    tokio::spawn(async move {
        scheduler::scheduler_loop(sched).await;
    });
    let gh = shared.clone();
    tokio::spawn(async move {
        crate::github::poll_loop(gh).await;
    });
    let shadow = shared.clone();
    tokio::spawn(async move {
        crate::shadow::poll_loop(shadow).await;
    });
    let tg = shared.clone();
    tokio::spawn(async move {
        crate::telegram::bot_loop(tg).await;
    });
    let inbox = shared.clone();
    tokio::spawn(async move {
        crate::inbox::poll_loop(inbox).await;
    });
    let gc = shared.clone();
    tokio::spawn(async move {
        crate::gc::daily_loop(gc).await;
    });
    let hup = shared.clone();
    tokio::spawn(async move {
        sighup_loop(hup).await;
    });
    Ok(addr)
}

async fn sighup_loop(shared: Arc<Shared>) {
    let Ok(mut hup) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup()) else {
        return;
    };
    while hup.recv().await.is_some() {
        match shared.reload() {
            Ok(_) => tracing::info!("SIGHUP: config reloaded"),
            Err(e) => tracing::error!("SIGHUP reload failed: {e:#}"),
        }
    }
}

pub async fn doctor(config: &Config) -> (bool, Vec<VersionCheck>) {
    let mut ok = true;
    if config.providers.is_empty() {
        ok = false;
    }
    if !config.system_prompt_file.exists() {
        ok = false;
    }
    if config.shadow.enabled && crate::shadow::validate_github_cli(&config.github.cli).is_err() {
        ok = false;
    }
    let mut checks = Vec::new();
    for p in &config.providers {
        let v = check_version(p).await;
        if !v.ok_path {
            ok = false;
        }
        checks.push(v);
    }
    (ok, checks)
}

/// Render two prompts per provider and check the cache prefix is stable.
pub fn doctor_cache(config: &Config) -> (bool, String) {
    let mut ok = true;
    let mut out = String::new();
    for p in &config.providers {
        let stable = crate::prompt::assemble_stable(config, p, &config.repo_path);
        let hash = crate::prompt::hash_prefix(&stable);
        let a = crate::prompt::render(
            &stable,
            "acme-cache-a",
            std::path::Path::new("/tmp/wt-a"),
            "/tmp/repo-a",
            Some(1),
            &config.shell_command_timeout_raw,
            Some("handoff one"),
            &["first task body".into()],
            false,
            config.checklist.enabled,
            Some("- [ ] cache-a"),
        );
        let b = crate::prompt::render(
            &stable,
            "acme-cache-b",
            std::path::Path::new("/tmp/wt-b"),
            "/tmp/repo-b",
            Some(2),
            &config.shell_command_timeout_raw,
            Some("handoff two"),
            &["second task body".into()],
            true,
            config.checklist.enabled,
            Some("- [x] cache-b"),
        );
        let Some((ha, ta)) = crate::prompt::split_variable(&a) else {
            ok = false;
            out.push_str(&format!(
                "cache\t{}\tFAIL\tmissing variable marker\n",
                p.name
            ));
            continue;
        };
        let Some((hb, tb)) = crate::prompt::split_variable(&b) else {
            ok = false;
            out.push_str(&format!(
                "cache\t{}\tFAIL\tmissing variable marker\n",
                p.name
            ));
            continue;
        };
        let prefix_ok = ha == hb
            && ha.trim_end() == stable.trim_end()
            && !ha.contains("acme-cache-a")
            && !ha.contains("handoff one")
            && ta.contains("acme-cache-a")
            && tb.contains("acme-cache-b")
            && !ha.contains("Checklist")
            && ha.find("## Task header").is_some()
            && ha.find("## Provider contract").is_some();
        if prefix_ok {
            out.push_str(&format!("cache\t{}\tok\tsha256={hash}\n", p.name));
        } else {
            ok = false;
            out.push_str(&format!(
                "cache\t{}\tFAIL\tprefix not stable across tasks/handoffs\n",
                p.name
            ));
        }
    }
    (ok, out)
}

/// Split each CLI argument on whitespace so `atr tasks rm "a b"` removes both.
pub fn expand_task_keys(keys: &[String]) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for raw in keys {
        for part in raw.split_whitespace() {
            if !part.is_empty() {
                out.push(part.to_string());
            }
        }
    }
    if out.is_empty() {
        bail!("atr tasks rm requires at least one task key");
    }
    Ok(out)
}

pub fn remove_tasks(shared: &Shared, keys: &[String], dry_run: bool) -> Result<()> {
    let keys = expand_task_keys(keys)?;
    let mut errors = Vec::new();
    let live: std::collections::HashSet<String> =
        shared.inflight.lock().unwrap().keys().cloned().collect();
    for key in &keys {
        if live.contains(key) {
            errors.push(format!("task {key} is running"));
            continue;
        }
        if let Some(reason) = shared.db.delete_refuse_reason(key)? {
            errors.push(reason);
            continue;
        }
        if dry_run {
            println!("would remove\t{key}");
            continue;
        }
        shared.db.delete_task(key)?;
        println!("removed\t{key}");
    }
    if !errors.is_empty() {
        bail!("{}", errors.join("; "));
    }
    Ok(())
}

pub fn print_tasks(
    shared: &Shared,
    key: Option<&str>,
    verbose: bool,
    repo: Option<&str>,
) -> Result<()> {
    if let Some(key) = key {
        print_task_detail(shared, key, verbose)?;
        return Ok(());
    }
    let cfg = shared.config.read().unwrap().clone();
    let tz = &cfg.display_timezone;
    for t in shared.db.list_tasks()? {
        if let Some(f) = repo {
            if !cfg.task_matches_repo(&t, f) {
                continue;
            }
        }
        println!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
            t.key,
            cfg.task_repo_name(&t),
            t.status,
            t.provider.as_deref().unwrap_or("-"),
            t.session_id.as_deref().unwrap_or("-"),
            t.worktree.as_deref().unwrap_or("-"),
            if t.shadow { "shadow" } else { "-" },
            tz.format_str(&t.updated)
        );
        if verbose {
            if let Some(h) = t.handoff.as_deref().filter(|s| !s.is_empty()) {
                println!("pending_handoff\t{}\n{h}", t.key);
            }
        }
    }
    Ok(())
}

pub fn task_verbose_text(shared: &Shared, key: &str) -> Result<String> {
    let mut buf = String::new();
    let Some(t) = shared.db.get_task(key)? else {
        bail!("unknown task {key}");
    };
    let cfg = shared.config.read().unwrap().clone();
    let tz = &cfg.display_timezone;
    buf.push_str(&format!(
        "key\t{}\nrepo\t{}\nstatus\t{}\nprovider\t{}\nsession\t{}\nworktree\t{}\nshadow\t{}\ncreated\t{}\nupdated\t{}\ntimezone\t{}\n",
        t.key,
        cfg.task_repo_name(&t),
        t.status,
        t.provider.as_deref().unwrap_or("-"),
        t.session_id.as_deref().unwrap_or("-"),
        t.worktree.as_deref().unwrap_or("-"),
        t.shadow,
        tz.format_str(&t.created),
        tz.format_str(&t.updated),
        tz.label
    ));
    if !t.model_override.is_empty() {
        buf.push_str(&format!("model\t{}\n", t.model_override));
    }
    if let Some(h) = t.handoff.as_deref().filter(|s| !s.is_empty()) {
        buf.push_str("\n--- pending handoff ---\n");
        buf.push_str(h);
        if !h.ends_with('\n') {
            buf.push('\n');
        }
    }
    if let Some(wt) = t.worktree.as_deref() {
        let max = shared.config.read().unwrap().checklist.max_bytes;
        if let Some(c) = crate::checklist::load(std::path::Path::new(wt), max) {
            buf.push_str("\n--- checklist ---\n");
            buf.push_str(&c);
            if !c.ends_with('\n') {
                buf.push('\n');
            }
        }
    }
    for r in shared.db.list_runs(Some(key))? {
        buf.push_str(&format!(
            "run\t{}\t{}\t{}\t{}\t{}\t{}\n",
            r.id,
            r.provider,
            r.outcome.as_deref().unwrap_or("-"),
            r.session_id.as_deref().unwrap_or("-"),
            tz.format_str(&r.started),
            tz.format_opt(r.ended.as_deref())
        ));
        if !t.model_override.is_empty() {
            let model = r.model.as_deref().filter(|m| !m.is_empty()).unwrap_or("-");
            buf.push_str(&format!("run_model\t{}\t{model}\n", r.id));
            for e in shared.db.events_for_run(r.id).unwrap_or_default() {
                if e.kind == crate::scheduler::MODEL_PIN_EVENT {
                    let detail: Value = serde_json::from_str(&e.raw).unwrap_or_default();
                    let reason = detail["reason"].as_str().unwrap_or_default();
                    buf.push_str(&format!("run_failure\t{}\t{reason}\n", r.id));
                }
            }
        }
        if let Some(h) = r.handoff.as_deref().filter(|s| !s.is_empty()) {
            buf.push_str(&format!("--- handoff (run {}) ---\n", r.id));
            buf.push_str(h);
            if !h.ends_with('\n') {
                buf.push('\n');
            }
        }
        if let Ok(v) = shared.db.verify_summary_for_run(r.id) {
            if !v.is_empty() {
                buf.push_str(&format!("--- verify (run {}) ---\n", r.id));
                buf.push_str(&v);
                if !v.ends_with('\n') {
                    buf.push('\n');
                }
            }
        }
        if let Ok(Some(c)) = shared.db.run_checklist(r.id) {
            if !c.trim().is_empty() {
                buf.push_str(&format!("--- checklist (run {}) ---\n", r.id));
                buf.push_str(&c);
                if !c.ends_with('\n') {
                    buf.push('\n');
                }
            }
        }
    }
    Ok(buf)
}

fn print_task_detail(shared: &Shared, key: &str, verbose: bool) -> Result<()> {
    if verbose {
        print!("{}", task_verbose_text(shared, key)?);
        return Ok(());
    }
    let Some(t) = shared.db.get_task(key)? else {
        bail!("unknown task {key}");
    };
    let tz = &shared.config.read().unwrap().display_timezone;
    println!(
        "{}\t{}\t{}\t{}\t{}\t{}\t{}",
        t.key,
        t.status,
        t.provider.as_deref().unwrap_or("-"),
        t.session_id.as_deref().unwrap_or("-"),
        t.worktree.as_deref().unwrap_or("-"),
        if t.shadow { "shadow" } else { "-" },
        tz.format_str(&t.updated)
    );
    Ok(())
}

pub fn format_status_table(shared: &Shared, repo: Option<&str>) -> String {
    format_status_table_opts(
        shared,
        &StatusCli {
            repo: repo.map(str::to_string),
            ..StatusCli::default()
        },
    )
}

pub fn format_status_table_opts(shared: &Shared, cli: &StatusCli) -> String {
    let v = snapshot_for_cli(shared, cli);
    let cfg = shared.config.read().unwrap().clone();
    let tz = cfg.display_timezone.label.clone();
    let listen = v.get("listen").and_then(|x| x.as_str()).unwrap_or("-");
    let paused = shared.intake_paused.load(Ordering::Relaxed);
    let mut out = format!(
        "status ({tz})  listen={listen}  intake={}\n",
        if paused { "paused" } else { "on" }
    );
    if let Some(th) = v.get("telegram_health") {
        let level = th
            .get("level")
            .and_then(|x| x.as_str())
            .unwrap_or("unknown");
        if level != "off" {
            let msg = th.get("message").and_then(|x| x.as_str()).unwrap_or("");
            let tag = match level {
                "fail" => "FAIL",
                "warn" => "warn",
                "ok" => "ok",
                _ => "?",
            };
            out.push_str(&format!("telegram: {tag}  {msg}\n"));
        }
    }
    if let Some(ga) = v.get("github_auth") {
        let tag = |level: &str| match level {
            "fail" => "FAIL",
            "warn" => "warn",
            "ok" => "ok",
            _ => "?",
        };
        let toks = ga
            .get("tokens")
            .and_then(|t| t.as_array())
            .cloned()
            .unwrap_or_default();
        if toks.is_empty() {
            let level = ga
                .get("level")
                .and_then(|x| x.as_str())
                .unwrap_or("unknown");
            let msg = ga.get("message").and_then(|x| x.as_str()).unwrap_or("");
            out.push_str(&format!("github auth: {}  {msg}\n", tag(level)));
        }
        for t in toks {
            let level = t.get("level").and_then(|x| x.as_str()).unwrap_or("unknown");
            let msg = t.get("message").and_then(|x| x.as_str()).unwrap_or("");
            out.push_str(&format!("github auth: {}  {msg}\n", tag(level)));
        }
    }
    let providers = v
        .get("providers")
        .and_then(|p| p.as_array())
        .cloned()
        .unwrap_or_default();
    let mut rows: Vec<[String; 8]> = Vec::new();
    for p in &providers {
        let skip = {
            let mut s = Vec::new();
            if p.get("cooling").and_then(|x| x.as_bool()).unwrap_or(false) {
                s.push("cool");
            }
            if p.get("degraded").and_then(|x| x.as_bool()).unwrap_or(false) {
                s.push("degraded");
            }
            if p.get("skip_new").and_then(|x| x.as_bool()).unwrap_or(false) {
                s.push("skip");
            }
            if s.is_empty() {
                "-".into()
            } else {
                s.join(",")
            }
        };
        let inflight = format!(
            "{}/{}",
            p.get("inflight").and_then(|x| x.as_u64()).unwrap_or(0),
            p.get("max_parallel").and_then(|x| x.as_u64()).unwrap_or(0)
        );
        rows.push([
            p.get("order")
                .and_then(|x| x.as_u64())
                .map(|n| n.to_string())
                .unwrap_or_else(|| "-".into()),
            p.get("name")
                .and_then(|x| x.as_str())
                .unwrap_or("-")
                .to_string(),
            p.get("kind")
                .and_then(|x| x.as_str())
                .unwrap_or("-")
                .to_string(),
            if p.get("enabled").and_then(|x| x.as_bool()).unwrap_or(false) {
                "yes".into()
            } else {
                "no".into()
            },
            inflight,
            skip,
            p.get("quota_reset")
                .and_then(|x| x.as_str())
                .unwrap_or("-")
                .to_string(),
            p.get("usage")
                .and_then(|x| x.as_str())
                .unwrap_or("-")
                .to_string(),
        ]);
    }
    let headers = [
        "#", "name", "kind", "enabled", "inflight", "skip", "reset", "usage",
    ];
    let widths: Vec<usize> = (0..headers.len())
        .map(|i| {
            rows.iter()
                .map(|r| r[i].chars().count())
                .chain(std::iter::once(headers[i].chars().count()))
                .max()
                .unwrap_or(1)
        })
        .collect();
    let mut header_line = String::new();
    for (i, h) in headers.iter().enumerate() {
        if i > 0 {
            header_line.push_str("  ");
        }
        header_line.push_str(&format!("{:<w$}", h, w = widths[i]));
    }
    out.push_str(&header_line);
    out.push('\n');
    if rows.is_empty() {
        out.push_str("(no providers)\n");
    } else {
        for r in &rows {
            for (i, cell) in r.iter().enumerate() {
                if i > 0 {
                    out.push_str("  ");
                }
                out.push_str(&format!("{:<w$}", cell, w = widths[i]));
            }
            out.push('\n');
        }
    }
    let running = v
        .get("running")
        .and_then(|x| x.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        })
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "(none)".into());
    if let Some(repos) = v.get("repos").and_then(|x| x.as_array()) {
        if !repos.is_empty() {
            out.push_str("repos:\n");
            for r in repos {
                let name = r.get("name").and_then(|x| x.as_str()).unwrap_or("-");
                let en = if r.get("enabled").and_then(|x| x.as_bool()).unwrap_or(false) {
                    "yes"
                } else {
                    "no"
                };
                let gh = r.get("github").and_then(|x| x.as_str()).unwrap_or("-");
                let label = r.get("label").and_then(|x| x.as_str()).unwrap_or("-");
                out.push_str(&format!("  {name}  en={en}  {gh}  {label}\n"));
            }
        }
    }
    if let Some(line) = v
        .get("telegram_assistant")
        .and_then(crate::dashboard::telegram_assistant_line)
    {
        out.push_str(&line);
        out.push('\n');
    }
    out.push_str(&format!("running: {running}\n"));
    if let Some(line) = v
        .get("host")
        .and_then(|h| h.get("line"))
        .and_then(|x| x.as_str())
    {
        out.push_str(line);
        out.push('\n');
    }
    if let Some(sh) = v.get("shadow") {
        if sh.get("enabled").and_then(|x| x.as_bool()).unwrap_or(false) {
            out.push_str(&format!(
                "shadow inflight={}/{}\n",
                sh.get("inflight").and_then(|x| x.as_u64()).unwrap_or(0),
                sh.get("max_parallel").and_then(|x| x.as_u64()).unwrap_or(0)
            ));
        }
    }
    let groups = v.get("groups").cloned().unwrap_or_else(|| json!({}));
    out.push_str(&status::format_cli_task_sections(
        &groups,
        cli.filter(),
        cli.caps(),
    ));
    if let Some(c) = v.get("cache") {
        out.push_str(&crate::usage::format_status_cache_line(c));
    }
    out
}

fn snapshot_for_cli(shared: &Shared, cli: &StatusCli) -> Value {
    let mut v = shared.status_json_opts(&cli.query());
    if let Some(groups) = v.get("groups").cloned() {
        v["groups"] = status::apply_group_filter(&groups, cli.filter());
    }
    v
}

fn unknown_repo_message(shared: &Shared, repo: &str) -> String {
    let names: Vec<String> = shared
        .config
        .read()
        .unwrap()
        .effective_repos()
        .into_iter()
        .map(|r| r.name)
        .collect();
    format!("unknown repo {repo:?}; configured: {}", names.join(", "))
}

fn check_exit(cli: &StatusCli, v: &Value) -> ExitCode {
    if !cli.check {
        return ExitCode::SUCCESS;
    }
    let n = if cli.filter().shows(status::Bucket::Failing) {
        status::failing_count(v.get("groups").unwrap_or(&Value::Null))
    } else {
        0
    };
    if n > 0 {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    }
}

fn print_status_once(shared: &Shared, cli: &StatusCli) -> Value {
    let v = snapshot_for_cli(shared, cli);
    if cli.json {
        println!("{}", serde_json::to_string_pretty(&v).unwrap());
    } else {
        print!("{}", format_status_table_opts(shared, cli));
        let _ = io::stdout().flush();
    }
    v
}

fn sleep_or_stdin_eof(dur: Duration, eof: &std::sync::mpsc::Receiver<()>, tty: bool) -> bool {
    if tty {
        let wait = if dur.is_zero() {
            Duration::from_secs(1)
        } else {
            dur
        };
        std::thread::sleep(wait);
        false
    } else {
        match eof.recv_timeout(dur) {
            Ok(()) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => true,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => false,
        }
    }
}

/// Print `atr status`. Exit 2 for unknown `--repo`; `--check` is 1 when a
/// shown failing section is non-empty.
pub fn run_status(shared: &Shared, cli: &StatusCli) -> ExitCode {
    if let Some(repo) = cli.repo.as_deref() {
        if shared
            .config
            .read()
            .unwrap()
            .resolve_repo_filter(repo)
            .is_none()
        {
            eprintln!("{}", unknown_repo_message(shared, repo));
            return ExitCode::from(2);
        }
    }
    if let Some(secs) = cli.watch {
        if cli.json {
            eprintln!("--watch cannot be combined with --json");
            return ExitCode::from(2);
        }
        let tty = io::stdin().is_terminal();
        let (tx, rx) = std::sync::mpsc::channel();
        if !tty {
            std::thread::spawn(move || {
                let mut buf = [0u8; 8];
                loop {
                    match io::stdin().read(&mut buf) {
                        Ok(0) | Err(_) => {
                            let _ = tx.send(());
                            break;
                        }
                        Ok(_) => {}
                    }
                }
            });
        }
        let interval = Duration::from_secs(secs);
        loop {
            let last = print_status_once(shared, cli);
            if sleep_or_stdin_eof(interval, &rx, tty) {
                return check_exit(cli, &last);
            }
        }
    }
    let v = print_status_once(shared, cli);
    check_exit(cli, &v)
}

pub fn print_usage(
    shared: &Shared,
    period: &crate::usage::Period,
    by: crate::usage::GroupBy,
    repo: Option<&str>,
) -> Result<()> {
    let cfg = shared.config.read().unwrap().clone();
    let tz = &cfg.display_timezone;
    let report = crate::usage::report_filtered(shared, period, repo)?;
    print!("{}", crate::usage::format_table(&report, by));
    let clock = crate::clock::Clock::system();
    let mut header = false;
    for p in &cfg.providers {
        let windows = crate::quota::snapshots(&shared.db, p, &clock)?;
        if windows.is_empty() {
            continue;
        }
        if !header {
            println!();
            println!(
                "{:<10} {:<8} {:<8} {:>10} {:>10} {:>10} {:>6} reset ({})",
                "provider", "window", "unit", "used", "budget", "remain", "pct", tz.label
            );
            header = true;
        }
        for w in windows {
            let reset = w
                .reset
                .map(|t| tz.format(t))
                .or_else(|| {
                    shared
                        .db
                        .quota_reset_at(&p.name)
                        .ok()
                        .flatten()
                        .map(|s| tz.format_str(&s))
                })
                .unwrap_or_else(|| "-".into());
            println!(
                "{:<10} {:<8} {:<8} {:>10.2} {:>10.2} {:>10.2} {:>5.0}% {}",
                p.name,
                w.window_raw,
                w.unit.as_str(),
                w.used,
                w.budget,
                w.remaining,
                w.ratio * 100.0,
                reset
            );
        }
    }
    Ok(())
}

pub const EX_TEMPFAIL: i32 = 75;

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn tasks_list_includes_timezone_updated() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("p.md"), "p\n").unwrap();
        let cfg = format!(
            r#"
listen = "127.0.0.1:0"
state_db = "{db}"
worktree_root = "{root}"
repo_path = "{root}"
system_prompt_file = "{root}/p.md"
display_timezone = "UTC"
[[provider]]
name = "claude"
kind = "claude"
cli = "/bin/true"
"#,
            db = dir.path().join("s.db").display(),
            root = dir.path().display(),
        );
        let raw: crate::config::RawConfig = toml::from_str(&cfg).unwrap();
        let parsed = crate::config::Config::from_raw(raw).unwrap();
        let shared = Shared::from_config(dir.path().join("c.toml"), parsed).unwrap();
        shared.db.upsert_task("acme-1", "/repo", None).unwrap();
        let mut buf = Vec::new();
        {
            use std::io::Write;
            let tz = &shared.config.read().unwrap().display_timezone;
            let t = shared.db.get_task("acme-1").unwrap().unwrap();
            writeln!(
                &mut buf,
                "{}\t{}\t{}\t{}\t{}\t{}\t{}",
                t.key,
                t.status,
                t.provider.as_deref().unwrap_or("-"),
                t.session_id.as_deref().unwrap_or("-"),
                t.worktree.as_deref().unwrap_or("-"),
                if t.shadow { "shadow" } else { "-" },
                tz.format_str(&t.updated)
            )
            .unwrap();
        }
        let line = String::from_utf8(buf).unwrap();
        assert!(line.contains("acme-1"));
        assert!(line.contains("UTC"), "{line}");
        let status = shared.status_json();
        assert_eq!(status["timezone"], "UTC");
        assert_eq!(status["version"], crate::VERSION);
        // github intake off: placeholder default repo hidden
        assert!(status["repos"]
            .as_array()
            .is_some_and(|a| a.iter().all(|r| r["shorthand"] != true)));
        assert_eq!(status["tasks"][0]["repo"], "repo");
        assert_eq!(status["tasks"][0]["check_only"], false);
        std::mem::forget(dir);
    }

    #[test]
    fn follow_up_without_repo_keeps_existing() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("p.md"), "p\n").unwrap();
        let shadow = dir.path().join("shadow-src");
        std::fs::create_dir_all(&shadow).unwrap();
        let cfg = format!(
            r#"
listen = "127.0.0.1:0"
state_db = "{db}"
worktree_root = "{root}"
repo_path = "{root}/atr-scratch"
system_prompt_file = "{root}/p.md"
[shadow]
repo_path = "{shadow}"
[[provider]]
name = "claude"
kind = "claude"
cli = "/bin/true"
"#,
            db = dir.path().join("s.db").display(),
            root = dir.path().display(),
            shadow = shadow.display(),
        );
        let raw: crate::config::RawConfig = toml::from_str(&cfg).unwrap();
        let parsed = crate::config::Config::from_raw(raw).unwrap();
        let shared = Shared::from_config(dir.path().join("c.toml"), parsed).unwrap();
        shared
            .submit(
                "acme-shadow-1",
                "first",
                "test",
                Some(&shadow.display().to_string()),
                Some(1),
                true,
                false,
            )
            .unwrap();
        let first = shared.db.get_task("acme-shadow-1").unwrap().unwrap();
        assert_eq!(first.repo.as_deref(), Some(shadow.to_str().unwrap()));
        shared
            .submit(
                "acme-shadow-1",
                "follow-up",
                "shadow-comment",
                None,
                Some(1),
                true,
                false,
            )
            .unwrap();
        let again = shared.db.get_task("acme-shadow-1").unwrap().unwrap();
        assert_eq!(
            again.repo.as_deref(),
            Some(shadow.to_str().unwrap()),
            "follow-up must not replace the shadow repo with repo_path"
        );
        std::mem::forget(dir);
    }

    fn test_shared(dir: &tempfile::TempDir) -> Arc<Shared> {
        std::fs::write(dir.path().join("p.md"), "p\n").unwrap();
        let cfg = format!(
            r#"
listen = "127.0.0.1:0"
state_db = "{db}"
worktree_root = "{root}"
repo_path = "{root}"
system_prompt_file = "{root}/p.md"
display_timezone = "UTC"
[[provider]]
name = "claude"
kind = "claude"
cli = "/bin/true"
"#,
            db = dir.path().join("s.db").display(),
            root = dir.path().display(),
        );
        let raw: crate::config::RawConfig = toml::from_str(&cfg).unwrap();
        let parsed = crate::config::Config::from_raw(raw).unwrap();
        Shared::from_config(dir.path().join("c.toml"), parsed).unwrap()
    }

    #[test]
    fn status_table_is_human_not_json() {
        let dir = tempfile::tempdir().unwrap();
        let shared = test_shared(&dir);
        shared.db.upsert_task("acme-1", "/repo", None).unwrap();
        let text = format_status_table(&shared, None);
        assert!(text.starts_with("status"), "{text}");
        assert!(text.contains("intake=on"), "{text}");
        assert!(text.contains("claude"), "{text}");
        assert!(text.contains("acme-1"), "{text}");
        assert!(text.contains("cache headline="), "{text}");
        assert!(!text.trim_start().starts_with('{'), "{text}");
        assert!(text.contains("waiting (1)"), "{text}");
        std::mem::forget(dir);
    }

    fn seed_status_groups(shared: &Shared) {
        shared.db.upsert_task("fail-1", "/repo", None).unwrap();
        shared.db.set_status("fail-1", "failed").unwrap();
        shared
            .db
            .set_task_result("fail-1", "boom\nmore", "", None)
            .unwrap();
        let run = shared.db.start_run("fail-1", "claude", "").unwrap();
        shared.db.finish_run(run, "error", Some(1)).unwrap();

        shared.db.upsert_task("run-1", "/repo", None).unwrap();
        shared.db.set_status("run-1", "running").unwrap();
        let rid = shared.db.start_run("run-1", "claude", "sonnet").unwrap();
        shared
            .db
            .insert_event(rid, "tool", r#"{"tool":"Read","text":"src/app.rs"}"#)
            .unwrap();

        shared.db.upsert_task("hold-1", "/repo", None).unwrap();
        shared.db.set_status("hold-1", "held").unwrap();
        shared
            .db
            .set_hold("hold-1", "after x failed", false)
            .unwrap();

        shared.db.upsert_task("idle-1", "/repo", None).unwrap();
        shared.db.set_status("idle-1", "waiting").unwrap();

        shared.db.upsert_task("done-1", "/repo", None).unwrap();
        shared.db.set_status("done-1", "done").unwrap();
    }

    #[test]
    fn status_groups_order_and_json_keeps_tasks() {
        let dir = tempfile::tempdir().unwrap();
        let shared = test_shared(&dir);
        seed_status_groups(&shared);
        let text = format_status_table(&shared, None);
        let fail = text.find("failing (").expect(&text);
        let run = text.find("running (").expect(&text);
        let wait = text.find("waiting (").expect(&text);
        let done = text.find("done (").expect(&text);
        assert!(fail < run && run < wait && wait < done, "{text}");
        assert!(text.contains("fail-1"), "{text}");
        assert!(text.contains("run-1"), "{text}");
        assert!(text.contains("hold-1"), "{text}");
        assert!(text.contains("idle-1"), "{text}");
        let idle = text.find("idle-1").unwrap();
        assert!(idle > done, "stored waiting belongs in done:\n{text}");
        let idle_line = text[done..].lines().find(|l| l.contains("idle-1")).unwrap();
        assert!(idle_line.contains("done"), "{idle_line}");
        assert!(!idle_line.contains("waiting"), "{idle_line}");
        assert!(!text.contains("tasks:"), "{text}");

        let json = shared.status_json();
        assert!(json.get("groups").is_some(), "{json}");
        let tasks = json["tasks"].as_array().unwrap();
        let keys: Vec<&str> = tasks.iter().map(|t| t["key"].as_str().unwrap()).collect();
        assert!(keys.contains(&"fail-1") && keys.contains(&"idle-1"));
        assert!(tasks[0].get("why").is_none(), "{json}");
        assert!(tasks[0].get("groups").is_none(), "{json}");
        assert_eq!(json["groups"]["failing"][0]["key"], "fail-1");
        assert_eq!(json["groups"]["failing"][0]["exit_code"], 1);
        assert_eq!(json["groups"]["done"].as_array().unwrap().len(), 2);
        assert!(json["groups"]["done"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["key"] == "idle-1" && t["status"] == "waiting"));
        let idle_json = json["groups"]["done"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["key"] == "idle-1")
            .unwrap();
        assert_eq!(idle_json["check_only"], false);

        let failed_only = format_status_table_opts(
            &shared,
            &StatusCli {
                failed: true,
                ..StatusCli::default()
            },
        );
        assert!(failed_only.contains("failing ("), "{failed_only}");
        assert!(!failed_only.contains("running ("), "{failed_only}");
        assert!(!failed_only.contains("waiting ("), "{failed_only}");
        assert!(!failed_only.contains("done ("), "{failed_only}");

        let running_only = format_status_table_opts(
            &shared,
            &StatusCli {
                running: true,
                ..StatusCli::default()
            },
        );
        assert!(running_only.contains("running ("), "{running_only}");
        assert!(!running_only.contains("failing ("), "{running_only}");

        let combo = format_status_table_opts(
            &shared,
            &StatusCli {
                failed: true,
                done: true,
                ..StatusCli::default()
            },
        );
        let f = combo.find("failing (").expect(&combo);
        let d = combo.find("done (").expect(&combo);
        assert!(f < d, "{combo}");
        assert!(!combo.contains("running ("), "{combo}");
        std::mem::forget(dir);
    }

    #[test]
    fn status_check_and_unknown_repo_exit_codes() {
        let dir = tempfile::tempdir().unwrap();
        let shared = test_shared(&dir);
        seed_status_groups(&shared);
        let ok = run_status(
            &shared,
            &StatusCli {
                running: true,
                check: true,
                ..StatusCli::default()
            },
        );
        assert_eq!(ok, ExitCode::SUCCESS);
        let fail = run_status(
            &shared,
            &StatusCli {
                check: true,
                ..StatusCli::default()
            },
        );
        assert_eq!(fail, ExitCode::from(1));
        let unknown = run_status(
            &shared,
            &StatusCli {
                repo: Some("nope".into()),
                check: true,
                ..StatusCli::default()
            },
        );
        assert_eq!(unknown, ExitCode::from(2));
        std::mem::forget(dir);
    }

    #[test]
    fn status_json_all_includes_reflect_and_since_filters() {
        let dir = tempfile::tempdir().unwrap();
        let shared = test_shared(&dir);
        shared.db.upsert_task("work-1", "/repo", None).unwrap();
        shared.db.set_status("work-1", "done").unwrap();
        shared
            .db
            .upsert_task("work-1-reflect", "/repo", None)
            .unwrap();
        shared
            .db
            .set_task_role("work-1-reflect", "reflect")
            .unwrap();
        shared.db.set_status("work-1-reflect", "done").unwrap();
        let old = "2020-01-01T00:00:00.000Z";
        shared.db.set_timestamps("work-1", old, old).unwrap();

        let default = shared.status_json();
        let keys: Vec<&str> = default["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|t| t["key"].as_str())
            .collect();
        assert!(!keys.contains(&"work-1-reflect"), "{keys:?}");

        let all = shared.status_json_opts(&StatusJsonOpts {
            include_reflect: true,
            ..StatusJsonOpts::default()
        });
        let keys: Vec<&str> = all["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|t| t["key"].as_str())
            .collect();
        assert!(keys.contains(&"work-1-reflect"), "{keys:?}");

        let since = shared.status_json_opts(&StatusJsonOpts {
            since: Some(Duration::from_secs(86400)),
            ..StatusJsonOpts::default()
        });
        assert!(since["tasks"].as_array().unwrap().is_empty(), "{since}");
        std::mem::forget(dir);
    }

    #[test]
    fn cancel_running_without_force_hints_rerun() {
        let dir = tempfile::tempdir().unwrap();
        let shared = test_shared(&dir);
        shared.db.upsert_task("acme-run", "/repo", None).unwrap();
        shared.db.set_status("acme-run", "running").unwrap();
        let err = shared.cancel("acme-run", false).unwrap_err().to_string();
        assert!(
            err.contains("re-run with --force"),
            "status-running path: {err}"
        );

        shared.inflight.lock().unwrap().insert(
            "acme-live".into(),
            InFlight {
                provider: "claude".into(),
                pid: 0,
                run_id: 1,
                kill: Arc::new(Mutex::new(None)),
                shadow: false,
            },
        );
        let err = shared.cancel("acme-live", false).unwrap_err().to_string();
        assert!(err.contains("re-run with --force"), "inflight path: {err}");
        shared.cancel("acme-run", true).unwrap();
        assert_eq!(
            shared.db.get_task("acme-run").unwrap().unwrap().status,
            "failed"
        );
        assert_eq!(
            Shared::cancel_ok_line("acme-run", true),
            "cancelled acme-run (force)"
        );
        assert_eq!(
            Shared::cancel_ok_line("acme-run", false),
            "cancelled acme-run"
        );
        std::mem::forget(dir);
    }

    #[test]
    fn retry_unconsumes_last_run_so_task_is_runnable() {
        let dir = tempfile::tempdir().unwrap();
        let shared = test_shared(&dir);
        shared.db.upsert_task("real-1", "/repo", None).unwrap();
        let mid = shared
            .db
            .enqueue_message("real-1", "do the work", "cli")
            .unwrap();
        let run = shared.db.start_run("real-1", "claude", "").unwrap();
        shared.db.mark_consumed(&[mid], run).unwrap();
        shared.db.finish_run(run, "success", None).unwrap();
        shared.db.set_status("real-1", "waiting").unwrap();
        assert!(shared.db.pending_messages("real-1").unwrap().is_empty());
        assert!(
            !shared
                .db
                .runnable_keys()
                .unwrap()
                .iter()
                .any(|k| k == "real-1"),
            "consumed waiting task must not be dispatchable"
        );

        shared.db.set_status("real-1", "running").unwrap();
        let err = shared.retry("real-1").unwrap_err().to_string();
        assert!(err.contains("is running"), "{err}");
        shared.db.set_status("real-1", "waiting").unwrap();

        shared
            .db
            .set_provider_session("real-1", Some("claude"), Some("ses_old"))
            .unwrap();
        shared.retry("real-1").unwrap();
        // A provider with a shell resumes its session (no-shell providers
        // start fresh: tests/retry.rs).
        assert_eq!(
            shared
                .db
                .task_session("real-1", "claude")
                .unwrap()
                .as_deref(),
            Some("ses_old"),
            "retry keeps a shell provider's session"
        );
        assert_eq!(
            shared.db.get_task("real-1").unwrap().unwrap().status,
            "queued"
        );
        assert_eq!(shared.db.pending_messages("real-1").unwrap().len(), 1);
        assert!(
            shared
                .db
                .runnable_keys()
                .unwrap()
                .iter()
                .any(|k| k == "real-1"),
            "retry must leave an unconsumed message so the scheduler can dispatch"
        );
        std::mem::forget(dir);
    }

    #[test]
    fn retry_of_a_planning_run_drops_its_session() {
        let dir = tempfile::tempdir().unwrap();
        let shared = test_shared(&dir);
        shared.db.upsert_task("plan-1", "/repo", None).unwrap();
        shared.db.set_task_role("plan-1", "plan").unwrap();
        shared
            .db
            .set_provider_session("plan-1", Some("claude"), Some("ses_plan"))
            .unwrap();
        shared.db.set_status("plan-1", "failed").unwrap();
        shared.retry("plan-1").unwrap();
        assert_eq!(shared.db.task_session("plan-1", "claude").unwrap(), None);
        std::mem::forget(dir);
    }

    #[test]
    fn status_reset_shows_cooldown_end() {
        let dir = tempfile::tempdir().unwrap();
        let shared = test_shared(&dir);
        let until = crate::db::cooldown_rfc3339(std::time::Duration::from_secs(600));
        shared.db.set_cooldown("claude", &until).unwrap();
        let text = format_status_table(&shared, None);
        assert!(text.contains("cool"), "{text}");
        let reset_cell = text
            .lines()
            .find(|l| l.contains("claude"))
            .expect("claude row");
        assert!(
            !reset_cell.split_whitespace().any(|w| w == "-")
                || reset_cell.contains("UTC")
                || reset_cell.contains(':'),
            "cooldown end must appear in reset column:\n{reset_cell}\n{text}"
        );
        let json = shared.status_json();
        let reset = json["providers"][0]["quota_reset"].as_str().unwrap_or("");
        assert!(!reset.is_empty(), "{json}");
        assert!(json.get("groups").is_some(), "{json}");
        std::mem::forget(dir);
    }

    #[test]
    fn remove_tasks_dry_run_and_refuses_running() {
        let dir = tempfile::tempdir().unwrap();
        let shared = test_shared(&dir);
        shared.db.upsert_task("done-1", "/repo", None).unwrap();
        shared.db.set_status("done-1", "waiting").unwrap();
        shared.db.upsert_task("live-1", "/repo", None).unwrap();
        shared.db.set_status("live-1", "running").unwrap();

        remove_tasks(&shared, &["done-1".into()], true).unwrap();
        assert!(shared.db.get_task("done-1").unwrap().is_some());

        let err = remove_tasks(&shared, &["live-1".into()], false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("running"), "{err}");
        assert!(shared.db.get_task("live-1").unwrap().is_some());

        remove_tasks(&shared, &["done-1".into()], false).unwrap();
        assert!(shared.db.get_task("done-1").unwrap().is_none());
        std::mem::forget(dir);
    }

    #[test]
    fn remove_tasks_allows_needs_attention_refuses_queued() {
        let dir = tempfile::tempdir().unwrap();
        let shared = test_shared(&dir);
        shared.db.upsert_task("attn-1", "/repo", None).unwrap();
        shared.db.set_status("attn-1", "needs_attention").unwrap();
        shared.db.upsert_task("q-1", "/repo", None).unwrap();
        shared.db.set_status("q-1", "queued").unwrap();

        remove_tasks(&shared, &["attn-1".into()], false).unwrap();
        assert!(shared.db.get_task("attn-1").unwrap().is_none());

        let err = remove_tasks(&shared, &["q-1".into()], false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("queued"), "{err}");
        assert!(shared.db.get_task("q-1").unwrap().is_some());
        std::mem::forget(dir);
    }

    #[test]
    fn remove_tasks_splits_whitespace_quoted_keys() {
        let dir = tempfile::tempdir().unwrap();
        let shared = test_shared(&dir);
        shared.db.upsert_task("done-a", "/repo", None).unwrap();
        shared.db.set_status("done-a", "waiting").unwrap();
        shared.db.upsert_task("done-b", "/repo", None).unwrap();
        shared.db.set_status("done-b", "waiting").unwrap();
        shared.db.upsert_task("done-c", "/repo", None).unwrap();
        shared.db.set_status("done-c", "waiting").unwrap();

        assert_eq!(
            expand_task_keys(&["done-a done-b".into(), "done-c".into()]).unwrap(),
            vec!["done-a", "done-b", "done-c"]
        );
        remove_tasks(&shared, &["done-a done-b".into()], false).unwrap();
        assert!(shared.db.get_task("done-a").unwrap().is_none());
        assert!(shared.db.get_task("done-b").unwrap().is_none());
        assert!(shared.db.get_task("done-c").unwrap().is_some());

        let err = expand_task_keys(&["".into(), "   ".into()])
            .unwrap_err()
            .to_string();
        assert!(err.contains("at least one task key"), "{err}");
        std::mem::forget(dir);
    }

    #[test]
    fn submit_after_and_retry_clears_hold() {
        let dir = tempfile::tempdir().unwrap();
        let shared = test_shared(&dir);
        shared.db.upsert_task("build", "/repo", None).unwrap();
        shared
            .submit_with(
                "deploy",
                "ship it",
                "cli",
                None,
                None,
                false,
                false,
                SubmitOpts {
                    after: vec!["build".into()],
                    ..SubmitOpts::default()
                },
            )
            .unwrap();
        assert_eq!(shared.db.get_task("deploy").unwrap().unwrap().deps, "build");
        let err = shared
            .submit_with(
                "loop",
                "x",
                "cli",
                None,
                None,
                false,
                false,
                SubmitOpts {
                    after: vec!["loop".into()],
                    ..SubmitOpts::default()
                },
            )
            .unwrap_err()
            .to_string();
        assert!(err.contains("itself"), "{err}");
        shared.db.set_status("build", "failed").unwrap();
        crate::deps::hold_dependents(&shared.db, "build").unwrap();
        assert_eq!(
            shared.db.get_task("deploy").unwrap().unwrap().status,
            "held"
        );
        shared.retry("deploy").unwrap();
        let t = shared.db.get_task("deploy").unwrap().unwrap();
        assert_eq!(t.status, "queued");
        assert!(t.deps.is_empty());
        assert!(t.hold_reason.is_empty());
        std::mem::forget(dir);
    }

    #[test]
    fn naming_another_provider_drops_the_model_override() {
        let dir = tempfile::tempdir().unwrap();
        let shared = test_shared(&dir);
        let submit = |provider: &str, model: Option<&str>| {
            shared
                .submit_with_model(
                    "ov-1",
                    "x",
                    "cli",
                    None,
                    None,
                    false,
                    false,
                    SubmitOpts {
                        provider_hint: Some(provider.into()),
                        ..SubmitOpts::default()
                    },
                    model,
                )
                .unwrap();
            shared.db.get_task("ov-1").unwrap().unwrap().model_override
        };
        assert_eq!(submit("codex", Some("gpt-5.6-sol")), "gpt-5.6-sol");
        assert_eq!(
            submit("codex", None),
            "gpt-5.6-sol",
            "same provider keeps it"
        );
        assert_eq!(submit("grok", None), "", "a codex id must not reach grok");
        std::mem::forget(dir);
    }

    #[test]
    fn model_override_skips_the_planner() {
        let dir = tempfile::tempdir().unwrap();
        let shared = test_shared(&dir);
        shared.config.write().unwrap().planner.mode = crate::config::PlannerMode::Always;
        let submit = |key: &str, model: Option<&str>| {
            shared
                .submit_with_model(
                    key,
                    "x",
                    "cli",
                    None,
                    None,
                    false,
                    false,
                    SubmitOpts {
                        provider_hint: Some("claude".into()),
                        ..SubmitOpts::default()
                    },
                    model,
                )
                .unwrap();
            shared.db.get_task(key).unwrap().unwrap().role
        };
        assert_eq!(submit("ov-plan", None), "plan");
        assert_eq!(submit("ov-noplan", Some("claude-opus-5-5")), "worker");
        std::mem::forget(dir);
    }
}
