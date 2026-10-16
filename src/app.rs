use crate::config::{load_config, Config};
use crate::db::Db;
use crate::http;
use crate::providers::{check_version, hit_ratio_from_sums, VersionCheck};
use crate::runner::terminate_tree_now;
use crate::scheduler::{self, reconcile};
use anyhow::{bail, Result};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
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
}

impl Shared {
    pub fn open(config_path: PathBuf) -> Result<Arc<Self>> {
        let config = load_config(&config_path)?;
        Self::from_config(config_path, config)
    }

    pub fn from_config(config_path: PathBuf, config: Config) -> Result<Arc<Self>> {
        let db = Db::open(&config.state_db)?;
        let paused = db.meta_get("intake_paused").ok().flatten().as_deref() == Some("1");
        Ok(Arc::new(Shared {
            config_path,
            config: RwLock::new(config),
            db,
            inflight: Mutex::new(HashMap::new()),
            kick: Notify::new(),
            intake_paused: AtomicBool::new(paused),
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
        if existing.is_none() && crate::plan::should_plan(&cfg, body) {
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
        if let Some(h) = opts
            .provider_hint
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            self.db.set_provider_hint(key, h)?;
        }
        if !opts.needs.is_empty() {
            let needs = crate::task::parse_needs(&opts.needs)?;
            self.db.set_task_needs(key, &needs.join(","))?;
        }
        let running = self.inflight.lock().unwrap().contains_key(key)
            || self.db.task_is_running(key)?
            || self.db.open_run_for_task(key)?.is_some();
        self.db.enqueue_message(key, body, source)?;
        if running {
            tracing::info!(task = %key, source, "queued follow-up; live run not interrupted");
            self.kick.notify_waiters();
            return Ok(SubmitOutcome::Queued);
        }
        let status = self
            .db
            .get_task(key)?
            .map(|t| t.status)
            .unwrap_or_else(|| "queued".into());
        if status != "running" {
            self.db.set_status(key, "queued")?;
        }
        self.kick.notify_waiters();
        Ok(SubmitOutcome::Accepted)
    }

    pub fn set_intake_paused(&self, paused: bool) {
        self.intake_paused.store(paused, Ordering::Relaxed);
        let _ = self
            .db
            .meta_set("intake_paused", if paused { "1" } else { "0" });
    }

    pub fn cancel_ok_line(key: &str, force: bool) -> String {
        if force {
            format!("cancelled {key} (force)")
        } else {
            format!("cancelled {key}")
        }
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
            return Ok(());
        }
        if let Some(task) = self.db.get_task(key)? {
            if task.status == "running" && !force {
                bail!("task {key} is running; re-run with --force to cancel");
            }
            self.db.set_status(key, "failed")?;
            Ok(())
        } else {
            bail!("unknown task {key}")
        }
    }

    pub fn status_json(&self) -> Value {
        self.status_json_filtered(None)
    }

    pub fn status_json_filtered(&self, repo: Option<&str>) -> Value {
        let cfg = self.config.read().unwrap().clone();
        let inflight = self.inflight.lock().unwrap().clone();
        let tasks = self.db.list_tasks().unwrap_or_default();
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
                let n = inflight.values().filter(|i| i.provider == p.name).count();
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
        json!({
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
            "repos": cfg.effective_repos().iter().map(|r| json!({
                "name": r.name,
                "enabled": r.enabled,
                "github": r.github,
                "label": r.label,
                "path": r.path.display().to_string(),
                "max_parallel": r.max_parallel,
                "shorthand": r.shorthand,
            })).collect::<Vec<_>>(),
            "tasks": tasks.iter().filter(|t| {
                repo.map(|f| cfg.task_matches_repo(t, f)).unwrap_or(true)
            }).map(|t| json!({
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
                "issue": t.issue,
                "workspace": t.workspace_kind,
                "output": t.output_kind,
                "source": t.source_kind,
            })).collect::<Vec<_>>(),
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
    let v = shared.status_json_filtered(repo);
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
    out.push_str(&format!("running: {running}\n"));
    if let Some(sh) = v.get("shadow") {
        if sh.get("enabled").and_then(|x| x.as_bool()).unwrap_or(false) {
            out.push_str(&format!(
                "shadow inflight={}/{}\n",
                sh.get("inflight").and_then(|x| x.as_u64()).unwrap_or(0),
                sh.get("max_parallel").and_then(|x| x.as_u64()).unwrap_or(0)
            ));
        }
    }
    let tasks = v
        .get("tasks")
        .and_then(|x| x.as_array())
        .cloned()
        .unwrap_or_default();
    out.push_str(&format!("tasks: {}\n", tasks.len()));
    for t in tasks.iter().take(8) {
        out.push_str(&format!(
            "  {}  {}  {}  {}\n",
            t.get("key").and_then(|x| x.as_str()).unwrap_or("-"),
            t.get("repo").and_then(|x| x.as_str()).unwrap_or("-"),
            t.get("status").and_then(|x| x.as_str()).unwrap_or("-"),
            t.get("provider").and_then(|x| x.as_str()).unwrap_or("-")
        ));
    }
    if tasks.len() > 8 {
        out.push_str(&format!("  … {} more\n", tasks.len() - 8));
    }
    if let Some(c) = v.get("cache") {
        out.push_str(&crate::usage::format_status_cache_line(c));
    }
    out
}

pub fn print_status(shared: &Shared, as_json: bool, repo: Option<&str>) {
    if as_json {
        println!(
            "{}",
            serde_json::to_string_pretty(&shared.status_json_filtered(repo)).unwrap()
        );
    } else {
        print!("{}", format_status_table(shared, repo));
    }
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
        assert!(status["repos"].as_array().is_some_and(|a| !a.is_empty()));
        assert_eq!(status["tasks"][0]["repo"], "repo");
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
}
