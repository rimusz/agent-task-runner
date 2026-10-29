use crate::app::{InFlight, Shared};
use crate::clock::Clock;
use crate::config::{Config, Provider, ShadowVerifyMode};
use crate::db::{cooldown_rfc3339, pid_alive, Db};
use crate::notify;
use crate::prompt;
use crate::providers::{
    cache_hit_ratio, extract_usage, new_session_uuid, usage_cost_kind, ErrorClass,
};
use crate::runner;
use crate::worktree;
use anyhow::Result;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

fn notify_run(cfg: &crate::config::TelegramConfig, body: &str, shadow: bool) {
    if body
        .split(|c: char| !c.is_ascii_alphanumeric() && c != '-')
        .any(|k| crate::telegram::is_assistant_key(k) || crate::tui::is_tui_key(k))
    {
        return;
    }
    notify::notify_unless_shadow(cfg, body, shadow);
}

/// Held for the whole `run_claimed` (including verify). Dropped on every
/// exit so a follow-up cannot start a second CLI while this one is live.
struct InflightSlot {
    shared: Arc<Shared>,
    key: String,
}

impl Drop for InflightSlot {
    fn drop(&mut self) {
        self.shared.inflight.lock().unwrap().remove(&self.key);
    }
}

pub async fn scheduler_loop(shared: Arc<Shared>) {
    let mut tick = tokio::time::interval(Duration::from_millis(100));
    loop {
        tokio::select! {
            _ = tick.tick() => {}
            _ = shared.kick.notified() => {}
        }
        if let Err(e) = dispatch_available(&shared).await {
            tracing::error!("dispatch: {e:#}");
        }
    }
}

pub async fn dispatch_available(shared: &Arc<Shared>) -> Result<()> {
    dispatch_matching(shared, None).await
}

/// Dispatch at most the runnable work for one task key. Used by `atr tui`
/// / `atr ask` so an in-process turn does not start leftover queued tasks,
/// including `cli-*` sessions that serve never auto-dispatches.
pub async fn dispatch_key(shared: &Arc<Shared>, key: &str) -> Result<()> {
    // TUI / ask never run `reconcile_loop`. A dead CLI with `outcome` NULL
    // would block forever (`open_run_for_task` → skip) while messages sit
    // unconsumed. Reclaim first so this tick can start a real turn.
    let _ = reclaim_dead_open_run(shared, key);
    let _ = requeue_if_pending(shared, key);
    dispatch_matching(shared, Some(key)).await
}

/// Serve owns daemon work. TUI sessions (`cli-*`) are started only by
/// [`dispatch_key`] in the TUI process.
pub fn serve_dispatches(key: &str) -> bool {
    !crate::tui::is_tui_key(key)
}

async fn dispatch_matching(shared: &Arc<Shared>, only: Option<&str>) -> Result<()> {
    let keys = order_runnable(shared)?;
    for key in keys {
        if only.is_some_and(|want| want != key) {
            continue;
        }
        // Full-scheduler ticks (serve) skip TUI sessions so Telegram/GitHub
        // keep running without racing an in-process `atr tui` on `cli-*`.
        if only.is_none() && !serve_dispatches(&key) {
            continue;
        }
        if shared.inflight.lock().unwrap().contains_key(&key) {
            continue;
        }
        if shared.db.open_run_for_task(&key)?.is_some() {
            continue;
        }
        let Some(task) = shared.db.get_task(&key)? else {
            continue;
        };
        if task.role == "parent" || task.role == "compare" {
            continue;
        }
        let cfg = shared.config.read().unwrap().clone();
        // Dependencies (`after:`). Separate from reaping / max_parallel.
        match crate::deps::decide(&shared.db, &task)? {
            crate::deps::Decision::Ready => {}
            crate::deps::Decision::Waiting { pending } => {
                tracing::debug!(
                    task = %key,
                    pending = %pending.join(","),
                    "waiting on after"
                );
                continue;
            }
            crate::deps::Decision::Held { reason } => {
                hold_blocked(shared, &cfg, &task, &reason);
                continue;
            }
        }
        if crate::retry::waiting(&task, &Clock::system()) {
            tracing::debug!(
                task = %key,
                next = %task.retry_next_at,
                reason = %task.retry_reason,
                "waiting on transient retry"
            );
            continue;
        }
        // Host CPU/memory gate. Separate from per-provider max_parallel.
        {
            let host = &cfg.host;
            let snap = crate::host::sample();
            if let Some(wait) = crate::host::decide(host, &snap) {
                tracing::debug!(task = %key, reason = %wait.reason, "host gate: keeping queued");
                continue;
            }
            let last = *shared.last_host_start.lock().unwrap();
            if !crate::host::settle_ready(host, last, std::time::Instant::now()) {
                tracing::debug!(task = %key, "host settle gap");
                continue;
            }
        }
        if task.pin_strict {
            if let Some(reason) = strict_pin_unusable(&cfg, &shared.db, &task) {
                fail_strict_arm(shared, &cfg, &task, None, &reason);
                continue;
            }
        }
        if task_over_cap(&shared.db, &cfg, &key)? {
            tracing::warn!(task = %key, "per-task wall clock exceeded");
            record_task_timeout(shared, &cfg, &task);
            shared.db.set_status(&key, "failed")?;
            notify_run(
                &cfg.telegram,
                &format!("failed {key} (task timeout)"),
                task.shadow,
            );
            notify_held_dependents(shared, &cfg, &key, task.shadow);
            if task.role == crate::compare::ARM_ROLE {
                crate::compare::after_arm(shared, &key);
            }
            continue;
        }
        let inflight = shared.inflight.lock().unwrap().clone();
        if task.shadow {
            let n = inflight.values().filter(|i| i.shadow).count() as u32;
            if n >= cfg.shadow.max_parallel {
                continue;
            }
        } else if let Some(entry) = cfg.repo_for_task(&task) {
            if entry.max_parallel > 0 {
                let n = repo_inflight_count(&shared.db, &inflight, &entry);
                if n >= entry.max_parallel {
                    continue;
                }
            }
        }
        let Some(provider) = pick_provider(&cfg, &shared.db, &inflight, &task, &Clock::system())?
        else {
            continue;
        };
        log_skips(
            &cfg,
            &shared.db,
            &inflight,
            &task,
            &provider,
            &Clock::system(),
        );
        // Register in-flight before the DB claim so the periodic reconcile
        // never sees a `running` row with no in-flight entry.
        {
            let mut map = shared.inflight.lock().unwrap();
            map.insert(
                key.clone(),
                InFlight {
                    provider: provider.name.clone(),
                    pid: 0,
                    run_id: 0,
                    kill: Arc::new(Mutex::new(None)),
                    shadow: task.shadow,
                },
            );
        }
        match shared.db.claim_task(&key) {
            Ok(true) => {
                if cfg.host.enabled {
                    *shared.last_host_start.lock().unwrap() = Some(std::time::Instant::now());
                }
            }
            Ok(false) => {
                shared.inflight.lock().unwrap().remove(&key);
                continue;
            }
            Err(e) => {
                shared.inflight.lock().unwrap().remove(&key);
                return Err(e);
            }
        }
        let shared = shared.clone();
        tokio::spawn(async move {
            let _slot = InflightSlot {
                shared: shared.clone(),
                key: key.clone(),
            };
            if let Err(e) = run_claimed(shared.clone(), key.clone(), provider).await {
                fail_claimed(&shared, &key, &e);
            }
        });
    }
    Ok(())
}

/// In-process inflight plus open runs from every owner that is still alive.
/// TUI turns count toward the same `max_parallel` as serve.
fn live_provider_count(db: &Db, inflight: &HashMap<String, InFlight>, name: &str) -> u32 {
    let mut keys = HashSet::new();
    for (k, i) in inflight {
        if i.provider == name {
            keys.insert(k.clone());
        }
    }
    if let Ok(runs) = db.live_open_runs(Some(name)) {
        for r in runs {
            keys.insert(r.task_key);
        }
    }
    keys.len() as u32
}

fn live_ollama_same_model(
    cfg: &Config,
    db: &Db,
    inflight: &HashMap<String, InFlight>,
    model: &str,
) -> usize {
    let mut keys = HashSet::new();
    let same = |prov: &str| {
        cfg.provider(prov)
            .is_some_and(|o| o.is_ollama() && o.model == model)
    };
    for (k, i) in inflight {
        if same(&i.provider) {
            keys.insert(k.clone());
        }
    }
    if let Ok(runs) = db.live_open_runs(None) {
        for r in runs {
            if same(&r.provider) {
                keys.insert(r.task_key);
            }
        }
    }
    keys.len()
}

fn repo_inflight_count(
    db: &Db,
    inflight: &HashMap<String, InFlight>,
    entry: &crate::config::RepoEntry,
) -> u32 {
    inflight
        .keys()
        .filter(|k| {
            db.get_task(k)
                .ok()
                .flatten()
                .and_then(|t| t.repo)
                .map(|p| crate::config::paths_eq(std::path::Path::new(&p), &entry.path))
                .unwrap_or(false)
        })
        .count() as u32
}

/// The per-task wall clock was hit: leave a durable record (a finished run
/// with outcome `task_timeout` plus a `task_timeout` event) and capture the
/// result/diff, so the failure survives log rotation and can be learned from.
pub fn record_task_timeout(shared: &Arc<Shared>, cfg: &Config, task: &crate::db::Task) {
    let key = task.key.as_str();
    let provider = task
        .provider
        .clone()
        .filter(|p| !p.is_empty())
        .unwrap_or_else(|| "atr".into());
    let first = shared.db.wall_clock_started(key).ok().flatten();
    let runs = shared.db.list_runs(Some(key)).map(|r| r.len()).unwrap_or(0);
    let run_id = match shared.db.start_run(key, &provider, "") {
        Ok(id) => id,
        Err(e) => {
            tracing::warn!(task = %key, "record task_timeout: {e:#}");
            return;
        }
    };
    let detail = serde_json::json!({
        "type": "task_timeout",
        "task_timeout_secs": cfg.task_timeout.as_secs(),
        "first_run_started": first,
        "runs": runs,
        "provider": provider,
    });
    let _ = shared
        .db
        .insert_event(run_id, "task_timeout", &detail.to_string());
    let _ = shared.db.finish_run(run_id, "task_timeout", None);
    let _ = crate::output::capture(shared, key);
    if cfg.lessons_enabled_for(task) {
        crate::lessons::spawn_reflect(
            shared.clone(),
            key.to_string(),
            "task_timeout",
            Some(run_id),
        );
    }
}

/// Why a strictly pinned task can never run on its provider (unknown,
/// disabled, missing CLI, or lacking a needed capability), if so.
pub fn strict_pin_unusable(cfg: &Config, db: &Db, task: &crate::db::Task) -> Option<String> {
    let name = task.provider.as_deref().filter(|n| !n.is_empty())?;
    let Some(p) = cfg.provider(name) else {
        return Some(format!("provider {name} is not configured"));
    };
    let enabled = db
        .enabled_override(&p.name)
        .ok()
        .flatten()
        .unwrap_or(p.enabled);
    if !enabled {
        return Some(format!("provider {name} is disabled"));
    }
    if !p.cli.exists() {
        return Some(format!("provider {name} CLI is missing"));
    }
    let needs = crate::task::task_needs(&task.needs, &task.labels);
    if !provider_has(p, &needs) {
        return Some(format!("provider {name} lacks {}", needs.join(",")));
    }
    None
}

/// A compare arm cannot continue on its pinned provider: fail it instead of
/// failing over (so both arms never collapse onto one provider). Records an
/// `arm_unavailable` event on `run_id`, or on a finished marker run when the
/// arm never started.
pub fn fail_strict_arm(
    shared: &Shared,
    cfg: &Config,
    task: &crate::db::Task,
    run_id: Option<i64>,
    reason: &str,
) {
    let key = task.key.as_str();
    tracing::warn!(task = %key, reason, "compare arm unavailable; not failing over");
    let detail = serde_json::json!({"type": "arm_unavailable", "reason": reason}).to_string();
    match run_id {
        Some(id) => {
            let _ = shared.db.insert_event(id, "arm_unavailable", &detail);
        }
        None => {
            let provider = task.provider.clone().unwrap_or_else(|| "atr".into());
            if let Ok(id) = shared.db.start_run(key, &provider, "") {
                let _ = shared.db.insert_event(id, "arm_unavailable", &detail);
                let _ = shared.db.finish_run(id, "arm_unavailable", None);
            }
        }
    }
    let _ = shared.db.set_status(key, "failed");
    let _ = crate::output::capture(shared, key);
    notify_run(
        &cfg.telegram,
        &format!("failed {key} (compare arm: {reason})"),
        task.shadow,
    );
    crate::compare::after_arm(shared, key);
}

fn task_over_cap(db: &Db, cfg: &Config, key: &str) -> Result<bool> {
    // Measured from the first run since the clock was last restarted (retry
    // or a new turn), never from task creation.
    let Some(first) = db.wall_clock_started(key)? else {
        return Ok(false);
    };
    let Ok(ts) = chrono::DateTime::parse_from_rfc3339(&first) else {
        return Ok(false);
    };
    let age = chrono::Utc::now() - ts.with_timezone(&chrono::Utc);
    Ok(age.to_std().unwrap_or_default() > cfg.task_timeout)
}

fn order_runnable(shared: &Shared) -> Result<Vec<String>> {
    let cfg = shared.config.read().unwrap().clone();
    let keys = shared.db.runnable_keys()?;
    let last_provider = shared.db.latest_finished_provider().ok().flatten();
    let clock = Clock::system();
    let mut scored: Vec<(u8, String, String)> = Vec::new();
    for key in keys {
        let Some(task) = shared.db.get_task(&key)? else {
            continue;
        };
        let pref = task.provider.clone().filter(|s| !s.is_empty());
        let in_window = pref.as_ref().is_some_and(|p| {
            cfg.provider(p)
                .is_some_and(|prov| last_run_in_window(&shared.db, &key, prov, &clock))
        });
        let batch = pref.as_ref() == last_provider.as_ref() && pref.is_some();
        let score = if in_window {
            0
        } else if batch {
            1
        } else {
            2
        };
        scored.push((score, task.created.clone(), key));
    }
    scored.sort();
    Ok(scored.into_iter().map(|(_, _, k)| k).collect())
}

fn last_run_in_window(db: &Db, key: &str, provider: &Provider, clock: &Clock) -> bool {
    let Ok(Some((name, ended))) = db.last_finished_run(key) else {
        return false;
    };
    if name != provider.name {
        return false;
    }
    let Ok(ts) = chrono::DateTime::parse_from_rfc3339(&ended) else {
        return false;
    };
    let now = clock.now();
    now.signed_duration_since(ts.with_timezone(&chrono::Utc))
        .to_std()
        .map(|d| d <= provider.cache_window)
        .unwrap_or(false)
}

pub fn pick_provider(
    cfg: &Config,
    db: &Db,
    inflight: &HashMap<String, InFlight>,
    task: &crate::db::Task,
    clock: &Clock,
) -> Result<Option<Provider>> {
    let tried = task
        .tried
        .as_deref()
        .unwrap_or("")
        .split(',')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>();

    let needs = crate::task::task_needs(&task.needs, &task.labels);
    if waiting_for_desktop(cfg, &needs) {
        tracing::debug!(task = %task.key, "desktop task waiting for an unlocked graphical session");
        return Ok(None);
    }
    let capable = |p: &Provider| provider_has(p, &needs);

    // Telegram assistant with `[telegram.assistant] providers`: the first
    // listed provider that is usable right now; wait while a listed one is
    // only busy/cooling; fall through when none of them can ever run.
    if crate::telegram::is_assistant_key(&task.key) && !cfg.telegram.assistant.providers.is_empty()
    {
        match list_pick(
            cfg,
            db,
            inflight,
            &tried,
            clock,
            &cfg.telegram.assistant.providers,
        )? {
            ListPick::Use(p) => return Ok(Some(*p)),
            ListPick::Wait => return Ok(None),
            ListPick::NoneUsable => {}
        }
    }

    // Compare arms: only the pinned provider, ever. Busy, cooling, degraded
    // or over quota means wait; disabled/missing is failed by the dispatcher.
    if task.pin_strict {
        let Some(name) = task.provider.as_deref().filter(|n| !n.is_empty()) else {
            return Ok(None);
        };
        if tried.contains(&name) {
            return Ok(None);
        }
        let Some(p) = cfg.provider(name).filter(|p| capable(p)) else {
            return Ok(None);
        };
        // A stall retry resumes on the same provider despite the degraded
        // mark it just set, exactly like a soft-pinned task.
        let resuming = task.stall_retries > 0 || task.session_id.is_some();
        return Ok(match unavailable(cfg, p, db, inflight, resuming, clock)? {
            None => Some(p.clone()),
            Some(_) => None,
        });
    }

    let mut prefers = Vec::new();
    if crate::plan::is_driver_role(&task.role) {
        if let Some(d) = cfg.driver_name() {
            prefers.push(d);
        }
    }
    if let Some(pref) = task.provider.as_deref() {
        if !prefers.iter().any(|p| p == pref) {
            prefers.push(pref.to_string());
        }
    }
    if !task.provider_hint.is_empty() && !prefers.iter().any(|p| p == &task.provider_hint) {
        prefers.push(task.provider_hint.clone());
    }

    // TUI session list is a fallback after the session pin / hint, before
    // the global `[[provider]]` order.
    let tui_list = crate::tui::is_tui_key(&task.key) && !cfg.tui.providers.is_empty();

    for pref in &prefers {
        if tried.contains(&pref.as_str()) {
            continue;
        }
        if let Some(p) = cfg.provider(pref).filter(|p| capable(p)) {
            if let Some(reason) = unavailable(cfg, p, db, inflight, true, clock)? {
                if reason == "full" {
                    return Ok(None);
                }
                if reason != "disabled" && reason != "missing-cli" {
                    return Ok(Some(p.clone()));
                }
            } else {
                return Ok(Some(p.clone()));
            }
        }
    }

    if tui_list {
        match list_pick(cfg, db, inflight, &tried, clock, &cfg.tui.providers)? {
            ListPick::Use(p) => return Ok(Some(*p)),
            ListPick::Wait => return Ok(None),
            ListPick::NoneUsable => {}
        }
    }

    for p in &cfg.providers {
        if tried.contains(&p.name.as_str()) || !capable(p) {
            continue;
        }
        if unavailable(cfg, p, db, inflight, false, clock)?.is_some() {
            continue;
        }
        return Ok(Some(p.clone()));
    }
    Ok(None)
}

enum ListPick {
    Use(Box<Provider>),
    Wait,
    NoneUsable,
}

/// Walk an ordered provider list, skipping `tried` ones.
fn list_pick(
    cfg: &Config,
    db: &Db,
    inflight: &HashMap<String, InFlight>,
    tried: &[&str],
    clock: &Clock,
    names: &[String],
) -> Result<ListPick> {
    let mut can_wait = false;
    for name in names {
        if tried.contains(&name.as_str()) {
            continue;
        }
        let Some(p) = cfg.provider(name) else {
            continue;
        };
        match unavailable(cfg, p, db, inflight, false, clock)? {
            None => return Ok(ListPick::Use(Box::new(p.clone()))),
            Some("disabled") | Some("missing-cli") => {}
            Some(reason) => {
                tracing::debug!(provider = %p.name, reason, "listed provider skipped");
                can_wait = true;
            }
        }
    }
    Ok(if can_wait {
        ListPick::Wait
    } else {
        ListPick::NoneUsable
    })
}

/// First name in `names` that is usable now, or the first still-enabled one
/// when every listed provider is only busy/cooling.
pub fn listed_provider_now(shared: &Shared, names: &[String]) -> Option<String> {
    if names.is_empty() {
        return None;
    }
    let cfg = shared.config.read().unwrap().clone();
    let inflight = shared.inflight.lock().unwrap().clone();
    match list_pick(&cfg, &shared.db, &inflight, &[], &Clock::system(), names) {
        Ok(ListPick::Use(p)) => Some(p.name),
        Ok(ListPick::Wait) => names
            .iter()
            .find(|n| {
                cfg.provider(n).is_some_and(|p| {
                    shared
                        .db
                        .enabled_override(&p.name)
                        .ok()
                        .flatten()
                        .unwrap_or(p.enabled)
                })
            })
            .cloned(),
        _ => None,
    }
}

/// The provider a new Telegram assistant turn would use right now (for the
/// "working…" message): `providers` list, else the single `provider` pin,
/// else `None` (first available, decided at dispatch).
pub fn assistant_provider_now(shared: &Shared) -> Option<String> {
    let cfg = shared.config.read().unwrap().clone();
    let ast = &cfg.telegram.assistant;
    if !ast.providers.is_empty() {
        return listed_provider_now(shared, &ast.providers);
    }
    let pin = ast.provider.trim();
    (!pin.is_empty()).then(|| pin.to_string())
}

/// Provider lists every capability the task needs.
pub fn provider_has(p: &Provider, needs: &[String]) -> bool {
    needs.iter().all(|n| p.capabilities.iter().any(|c| c == n))
}

/// A `desktop` task must wait while no logged-in, unlocked graphical session
/// is available.
fn waiting_for_desktop(cfg: &Config, needs: &[String]) -> bool {
    needs.iter().any(|n| n == "desktop") && !crate::desktop::session_ready(&cfg.desktop_check)
}

fn log_skips(
    cfg: &Config,
    db: &Db,
    inflight: &HashMap<String, InFlight>,
    task: &crate::db::Task,
    chosen: &Provider,
    clock: &Clock,
) {
    if task.provider.as_deref() == Some(chosen.name.as_str()) {
        return;
    }
    for p in &cfg.providers {
        if p.name == chosen.name {
            break;
        }
        let reason = match unavailable(cfg, p, db, inflight, false, clock) {
            Ok(Some(r)) => r,
            _ => continue,
        };
        if reason == "quota" || reason == "limit" || reason == "cooldown" {
            tracing::info!(
                provider = %p.name,
                reason,
                task = %task.key,
                overflow = %chosen.name,
                "skipping provider"
            );
            if reason == "quota" {
                notify_run(
                    &cfg.telegram,
                    &format!(
                        "quota skip {} task={} overflow={}",
                        p.name, task.key, chosen.name
                    ),
                    task.shadow,
                );
            }
        }
    }
}

/// Why `p` cannot take a new task right now (`full`, `disabled`, …).
pub fn provider_skip_reason(
    cfg: &Config,
    p: &Provider,
    db: &Db,
    inflight: &HashMap<String, InFlight>,
    existing: bool,
    clock: &Clock,
) -> Result<Option<&'static str>> {
    unavailable(cfg, p, db, inflight, existing, clock)
}

fn unavailable(
    cfg: &Config,
    p: &Provider,
    db: &Db,
    inflight: &HashMap<String, InFlight>,
    existing: bool,
    clock: &Clock,
) -> Result<Option<&'static str>> {
    let enabled = db
        .enabled_override(&p.name)
        .ok()
        .flatten()
        .unwrap_or(p.enabled);
    if !enabled {
        return Ok(Some("disabled"));
    }
    if !p.cli.exists() {
        return Ok(Some("missing-cli"));
    }
    let n = live_provider_count(db, inflight, &p.name);
    if n >= p.max_parallel {
        return Ok(Some("full"));
    }
    if p.is_ollama() {
        let same_model = live_ollama_same_model(cfg, db, inflight, &p.model);
        if same_model >= 1 {
            return Ok(Some("full"));
        }
    }
    if existing {
        return Ok(None);
    }
    let (cooling, degraded) = db.provider_blocked(&p.name)?;
    if cooling || crate::quota::limit_reset_active(db, &p.name, clock)? {
        return Ok(Some("limit"));
    }
    if degraded {
        return Ok(Some("degraded"));
    }
    if crate::quota::over_quota(db, p, clock)? {
        return Ok(Some("quota"));
    }
    Ok(None)
}

async fn run_claimed(shared: Arc<Shared>, key: String, mut provider: Provider) -> Result<()> {
    let cfg = shared.config.read().unwrap().clone();
    let assistant = crate::telegram::is_assistant_key(&key);
    if assistant {
        provider =
            crate::telegram::apply_assistant_timeout(&provider, cfg.telegram.assistant.timeout);
    }
    if crate::tui::is_tui_key(&key) {
        crate::tui::apply_session_model(&mut provider, &shared.db, &key);
    }
    crate::tui::sanitize_spawn_model(&mut provider);
    if crate::tui::is_tui_key(&key) {
        crate::tui::apply_session_knobs(&mut provider, &shared.db, &key);
    }
    let task = shared
        .db
        .get_task(&key)?
        .ok_or_else(|| anyhow::anyhow!("task {key} vanished"))?;
    let pending = shared.db.pending_messages(&key)?;
    if pending.is_empty() {
        shared.db.set_status(&key, "waiting")?;
        return Ok(());
    }

    crate::reported::refresh_grok_billing(&shared.db, &provider, &Clock::system())
        .await
        .ok();
    let already_on = shared.db.task_session(&key, &provider.name)?.is_some();
    if !already_on && crate::quota::over_quota(&shared.db, &provider, &Clock::system())? {
        shared.db.set_status(&key, "queued")?;
        shared.kick.notify_waiters();
        return Ok(());
    }
    if assistant {
        let (s2, k2, p2) = (shared.clone(), key.clone(), provider.name.clone());
        let _ =
            tokio::task::spawn_blocking(move || crate::telegram::assistant_started(&s2, &k2, &p2))
                .await;
    }

    let entry = cfg.repo_for_task(&task);
    let ws_kind = crate::task::WorkspaceKind::parse(&task.workspace_kind)
        .unwrap_or_else(|_| crate::task::infer_workspace_kind(task.repo.as_deref().unwrap_or("")));
    let output_kind =
        crate::task::OutputKind::parse(&task.output_kind).unwrap_or(crate::task::OutputKind::Reply);
    let repo = if task.shadow {
        shadow_source_repo(&cfg, &task)?
    } else {
        task.repo
            .as_deref()
            .map(std::path::PathBuf::from)
            .or_else(|| entry.as_ref().map(|e| e.path.clone()))
            .unwrap_or_else(|| cfg.repo_path.clone())
    };
    let branch_prefix = entry
        .as_ref()
        .map(|e| {
            if e.branch_prefix.is_empty() {
                cfg.worktree_branch_prefix.clone()
            } else {
                e.branch_prefix.clone()
            }
        })
        .unwrap_or_else(|| cfg.worktree_branch_prefix.clone());
    let base_branch = entry
        .as_ref()
        .map(|e| {
            if e.base.is_empty() {
                cfg.repo_default_branch.clone()
            } else {
                e.base.clone()
            }
        })
        .unwrap_or_else(|| cfg.repo_default_branch.clone());
    let wt = match ws_kind {
        crate::task::WorkspaceKind::None => {
            worktree::ensure_none_workspace(&cfg.worktree_root, &key)?
        }
        crate::task::WorkspaceKind::Dir => {
            let path = if repo.as_os_str().is_empty() {
                anyhow::bail!("dir workspace for {key} has no path");
            } else {
                repo.clone()
            };
            worktree::ensure_dir_workspace(&path)?
        }
        crate::task::WorkspaceKind::Git => {
            if !worktree::is_git_repo(&repo) && repo.as_os_str().is_empty() {
                anyhow::bail!("git workspace for {key} has no repo path");
            }
            // Compare arms get their own worktree (never a pool slot), and a
            // promoted compare parent keeps the winner's tree.
            // Never reuse an answer-only `_none` scratch path as a git tree.
            let own_tree = task.worktree.as_deref().filter(|w| {
                !task.compare_decision.is_empty()
                    && std::path::Path::new(w).exists()
                    && !worktree::is_none_workspace(std::path::Path::new(w))
            });
            if let Some(w) = own_tree {
                std::path::PathBuf::from(w)
            } else if task.role == crate::compare::ARM_ROLE {
                worktree::ensure_worktree(
                    &repo,
                    &cfg.worktree_root,
                    &key,
                    &branch_prefix,
                    &base_branch,
                )?
            } else if cfg.cache.worktree_pool {
                let existing_wt = task
                    .worktree
                    .as_deref()
                    .filter(|w| !worktree::is_none_workspace(std::path::Path::new(w)));
                let slot = pick_pool_slot(
                    &shared,
                    &repo,
                    &provider.name,
                    &key,
                    existing_wt,
                    provider.max_parallel.max(1),
                );
                let dest =
                    worktree::pool_slot_path(&cfg.worktree_root, &repo, &provider.name, slot);
                let branch = format!("{branch_prefix}/{key}");
                let keep = (dest.join(".git").exists() || dest.join(".git").is_file())
                    && worktree::current_branch(&dest).as_deref() == Some(branch.as_str());
                worktree::acquire_pool_worktree(
                    &repo,
                    &cfg.worktree_root,
                    &provider.name,
                    slot,
                    &key,
                    &branch_prefix,
                    &base_branch,
                    !keep,
                )?
            } else {
                worktree::ensure_worktree(
                    &repo,
                    &cfg.worktree_root,
                    &key,
                    &branch_prefix,
                    &base_branch,
                )?
            }
        }
    };
    // Compare arms are hardened like shadow runs (no push, gh wrapper), but
    // in their own worktree config: they share the user's repo.
    let hardened = task.shadow || task.role == crate::compare::ARM_ROLE;
    if task.shadow {
        worktree::ensure_shadow_no_push(&wt)?;
    } else if hardened {
        worktree::ensure_worktree_no_push(&wt)?;
    }
    shared.db.set_worktree(&key, &wt.display().to_string())?;
    if spawn_workspace_changed(task.worktree.as_deref(), &wt) {
        let _ = shared
            .db
            .set_provider_session(&key, Some(&provider.name), None);
    }
    crate::intake::materialize_attachments(&cfg.state_db, &key, &wt)?;

    if task.role == "plan" {
        crate::plan::apply_plan_args(&mut provider, cfg.planner.max_turns);
    }

    let run_id = match shared.db.try_start_run(
        &key,
        &provider.name,
        &provider.model,
        provider.max_parallel.max(1),
    )? {
        Some(id) => id,
        None => {
            tracing::info!(
                task = %key,
                provider = %provider.name,
                "provider at limit; waiting"
            );
            let _ = shared.db.set_status(&key, "queued");
            return Ok(());
        }
    };
    let turn_id = shared.db.start_turn(run_id, &key)?;
    let (mut session, new_session) = ensure_session(&shared.db, &key, &provider)?;
    let fork_from = grok_fork_from(
        &shared.db,
        &key,
        &provider,
        new_session,
        &pending,
        session.as_deref(),
    )?;
    if let Some(old) = &fork_from {
        let new_id = new_session_uuid();
        session = Some(new_id.clone());
        shared
            .db
            .set_provider_session(&key, Some(&provider.name), Some(&new_id))?;
        tracing::info!(task = %key, from = %old, to = %new_id, "forked grok session");
    }
    if let Some(sid) = &session {
        shared.db.set_run_session(run_id, sid)?;
        shared
            .db
            .set_provider_session(&key, Some(&provider.name), Some(sid))?;
    } else {
        shared
            .db
            .set_provider_session(&key, Some(&provider.name), None)?;
    }
    let ids: Vec<i64> = pending.iter().map(|m| m.id).collect();
    shared.db.mark_consumed(&ids, run_id)?;

    let prompt_override = entry.as_ref().and_then(|e| {
        if e.system_prompt_file.as_os_str().is_empty() {
            None
        } else {
            Some(e.system_prompt_file.as_path())
        }
    });
    let mut prefix = prompt::assemble_stable_with(&cfg, &provider, &repo, prompt_override);
    if assistant {
        prefix.push_str(&crate::telegram::assistant_stable_suffix());
    }
    let prefix_hash = prompt::hash_prefix(&prefix);
    if let Some(sid) = session.as_deref() {
        if let Ok(Some(prev)) = shared.db.last_prefix_hash(sid, &provider.name, run_id) {
            if prev != prefix_hash {
                tracing::warn!(
                    task = %key,
                    provider = %provider.name,
                    session = %sid,
                    old = %prev,
                    new = %prefix_hash,
                    "cache prefix hash changed for this session; prompt cache will miss"
                );
            }
        }
    }
    let _ = shared.db.set_run_prefix_hash(run_id, &prefix_hash);
    let bodies: Vec<String> = pending.into_iter().map(|m| m.body).collect();
    let prior: Vec<_> = shared
        .db
        .list_runs(Some(&key))?
        .into_iter()
        .filter(|r| r.id != run_id)
        .collect();
    let switching = prior
        .last()
        .map(|r| r.provider != provider.name)
        .unwrap_or(false);
    let cannot_resume = !prior.is_empty() && (new_session || session.is_none());
    let pending_h = task.handoff.as_deref().filter(|s| !s.is_empty());
    let handoff_text = if crate::handoff::needs_handoff(pending_h, switching, cannot_resume) {
        if let Some(existing) = pending_h {
            Some(existing.to_string())
        } else {
            let last = prior.last();
            crate::handoff::build_for_switch(
                &cfg,
                &shared.db,
                &key,
                &task,
                &wt,
                last.map(|r| r.provider.as_str()).unwrap_or("unknown"),
                last.and_then(|r| r.outcome.as_deref()).unwrap_or("unknown"),
                None,
                last.map(|r| r.id),
            )
            .ok()
        }
    } else {
        None
    };
    if let Some(h) = &handoff_text {
        let _ = shared.db.set_run_handoff(run_id, h);
    }
    let dir_workspace = ws_kind == crate::task::WorkspaceKind::Dir;
    let files_dir = runner::runtime_files_dir(&cfg.state_db, &key, &wt, dir_workspace);
    let checklist = if cfg.checklist.enabled && !dir_workspace {
        crate::checklist::load(&wt, cfg.checklist.max_bytes)
    } else {
        None
    };
    let resume_delta = !new_session && session.is_some() && !switching && !cannot_resume;
    let has_slot = provider.kind == "codex" || !provider.system_prompt_flag.is_empty();
    let mut rendered = if resume_delta {
        prompt::render_resume(&bodies, handoff_text.as_deref())
    } else if has_slot {
        prompt::render_variable(
            &key,
            &wt,
            &repo.display().to_string(),
            task.issue,
            &cfg.shell_command_timeout_raw,
            handoff_text.as_deref(),
            &bodies,
            task.shadow,
            cfg.checklist.enabled && !assistant,
            checklist.as_deref(),
        )
    } else {
        prompt::render(
            &prefix,
            &key,
            &wt,
            &repo.display().to_string(),
            task.issue,
            &cfg.shell_command_timeout_raw,
            handoff_text.as_deref(),
            &bodies,
            task.shadow,
            cfg.checklist.enabled && !assistant,
            checklist.as_deref(),
        )
    };
    if !resume_delta {
        if let Some(note) =
            crate::task::workspace_note(ws_kind, &wt.display().to_string(), output_kind)
        {
            rendered = crate::task::insert_before_messages(&rendered, &note);
        }
        // Repo lessons go in the per-task tail (never the cached prefix), on
        // first runs only, and never for plan/review/reflect or the assistant.
        if let Some((block, ids)) = crate::lessons::block_for_task(
            &cfg,
            &shared.db,
            &task,
            assistant,
            ws_kind == crate::task::WorkspaceKind::Git,
        ) {
            rendered = crate::task::insert_before_messages(&rendered, &block);
            let _ = shared.db.record_lessons_injected(run_id, &ids);
        }
    }
    if assistant {
        rendered.push('\n');
        rendered.push_str(&crate::telegram::assistant_snapshot(&shared).await);
    }
    if task.role == "plan" {
        rendered = crate::plan::plan_prompt(&rendered);
    }
    let _ = shared.db.set_handoff(&key, None);

    let pid_slot = Arc::new(Mutex::new(None));
    {
        let mut map = shared.inflight.lock().unwrap();
        if let Some(slot) = map.get_mut(&key) {
            slot.run_id = run_id;
            slot.kill = pid_slot.clone();
            slot.provider = provider.name.clone();
            slot.shadow = task.shadow;
        } else {
            map.insert(
                key.clone(),
                InFlight {
                    provider: provider.name.clone(),
                    pid: 0,
                    run_id,
                    kill: pid_slot.clone(),
                    shadow: task.shadow,
                },
            );
        }
    }

    notify_run(
        &cfg.telegram,
        &format!("start {key} provider={}", provider.name),
        task.shadow,
    );

    let path_prepend = if hardened {
        let bin = wt.join(".atr").join("bin");
        crate::shadow::write_gh_wrapper(&bin).ok();
        Some(bin)
    } else {
        None
    };
    let sys_path = files_dir.join("system.md");
    let attach_system = !resume_delta && has_slot;
    if attach_system {
        std::fs::create_dir_all(sys_path.parent().unwrap())?;
        std::fs::write(&sys_path, &prefix)?;
    }
    let result = runner::run_cli(
        &shared.db,
        &cfg,
        &provider,
        &wt,
        &rendered,
        session.as_deref(),
        new_session,
        run_id,
        turn_id,
        &key,
        pid_slot.clone(),
        path_prepend.as_deref(),
        fork_from.as_deref(),
        if attach_system {
            Some(sys_path.as_path())
        } else {
            None
        },
        dir_workspace,
        &files_dir,
    )
    .await;

    if shared.db.run_outcome(run_id)?.as_deref() == Some("cancelled") {
        if let Ok(r) = &result {
            if let Some(sid) = &r.session_id {
                let _ = shared
                    .db
                    .set_provider_session(&key, Some(&provider.name), Some(sid));
            }
        }
        tracing::info!(task = %key, "run was force-cancelled; no provider cooldown");
        if assistant {
            crate::telegram::deliver_assistant_result(&shared, &key, run_id, "cancelled");
        }
        return Ok(());
    }

    let result = match result {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(task = %key, "cli: {e:#}");
            shared.db.finish_run(run_id, "unavailable", None)?;
            shared.db.finish_turn(turn_id, "unavailable")?;
            shared.db.unconsume_run(run_id)?;
            shared.db.set_cooldown(
                &provider.name,
                &cooldown_rfc3339(provider.health.unhealthy_cooldown),
            )?;
            notify_run(
                &cfg.telegram,
                &format!("fallback {key} {} (spawn)", provider.name),
                task.shadow,
            );
            if hardened {
                let _ = crate::shadow::record_after_run(
                    &cfg,
                    &shared.db,
                    &key,
                    &wt,
                    &provider.name,
                    run_id,
                    "unavailable",
                    &crate::providers::UsageNumbers::default(),
                );
            }
            let result = runner::RunResult {
                outcome: "unavailable".into(),
                exit_code: None,
                session_id: None,
                usage: crate::providers::UsageNumbers::default(),
                usage_raw: Vec::new(),
                snippet: format!("{e:#}"),
                classified: Some(ErrorClass::Unavailable),
                stderr: String::new(),
            };
            handle_failover(&shared, &cfg, &key, &provider, &result, run_id, &wt).await?;
            if assistant {
                crate::telegram::deliver_assistant_result(&shared, &key, run_id, "unavailable");
            }
            shared.kick.notify_waiters();
            return Ok(());
        }
    };

    if let Some(sid) = &result.session_id {
        shared
            .db
            .set_provider_session(&key, Some(&provider.name), Some(sid))?;
    } else {
        let own = shared.db.task_session(&key, &provider.name)?;
        shared
            .db
            .set_provider_session(&key, Some(&provider.name), own.as_deref())?;
    }

    record_run_usage(
        &shared.db,
        run_id,
        Some(turn_id),
        &provider,
        &result,
        resume_delta,
    )?;

    if should_warn_low_cache(&shared.db, &key, result.session_id.as_deref())? {
        if let Some(ratio) = cache_hit_ratio(&provider.kind, &result.usage) {
            if ratio < cfg.cache_hit_warn_threshold {
                tracing::warn!(
                    provider = %provider.name,
                    task = %key,
                    hit = format!("{:.1}%", ratio * 100.0),
                    threshold = format!("{:.0}%", cfg.cache_hit_warn_threshold * 100.0),
                    "cache-hit ratio below threshold"
                );
            }
        }
    }

    shared
        .db
        .finish_run(run_id, &result.outcome, result.exit_code)?;
    shared.db.finish_turn(turn_id, &result.outcome)?;

    let verify = if result.outcome == "success" && !assistant {
        let task_now = shared.db.get_task(&key)?.unwrap_or_else(|| task.clone());
        Some(
            crate::verify::run(&cfg, &shared.db, &key, &repo, &wt, run_id, &task_now)
                .await
                .unwrap_or_else(|e| {
                    tracing::warn!(task = %key, "verify: {e:#}");
                    crate::verify::VerifyRun {
                        skipped: false,
                        passed: false,
                        checks: vec![crate::db::VerifyCheck {
                            command: "(verify)".into(),
                            passed: false,
                            exit_code: -1,
                            duration_ms: 0,
                            output: format!("{e:#}"),
                            recorded: crate::db::now_rfc3339(),
                        }],
                        tests_modified: Vec::new(),
                        reason: None,
                    }
                }),
        )
    } else {
        None
    };

    let mut final_outcome = result.outcome.clone();
    if result.outcome == "success" {
        let v = verify.as_ref().cloned().unwrap_or_default();
        if v.reason.as_deref() == Some("tests_modified") {
            final_outcome = "tests_modified".into();
            shared.db.set_run_outcome(run_id, "tests_modified")?;
            shared.db.finish_turn(turn_id, "tests_modified")?;
        } else if !v.skipped && !v.passed {
            final_outcome = "verify_failed".into();
            shared.db.set_run_outcome(run_id, "verify_failed")?;
            shared.db.finish_turn(turn_id, "verify_failed")?;
        }
    }
    if cfg.checklist.enabled {
        if let Some(text) = crate::checklist::load(&wt, cfg.checklist.max_bytes) {
            let _ = shared.db.set_run_checklist(run_id, &text);
        }
    }

    if result.outcome == "limit" {
        let _ = crate::quota::record_limit_for_run(
            &shared.db,
            &provider.name,
            &result,
            &Clock::system(),
        );
    }

    if hardened {
        let _ = crate::shadow::record_after_run(
            &cfg,
            &shared.db,
            &key,
            &wt,
            &provider.name,
            run_id,
            &final_outcome,
            &result.usage,
        );
    }

    match result.outcome.as_str() {
        "success" => {
            shared.db.set_stall_retries(&key, 0)?;
            shared.db.clear_retry(&key)?;
            shared.db.set_tried(&key, None)?;
            let _ = shared.db.set_degraded_until(&provider.name, None);
            let v = verify.unwrap_or_default();
            if v.reason.as_deref() == Some("tests_modified") {
                shared.db.set_status(&key, "needs_attention")?;
                notify_run(
                    &cfg.telegram,
                    &format!("needs_attention {key} (tests_modified)"),
                    task.shadow,
                );
            } else if !v.skipped && !v.passed {
                let retry = !task.shadow || cfg.shadow.verify_mode == ShadowVerifyMode::Retry;
                let used = shared
                    .db
                    .get_task(&key)?
                    .map(|t| t.verify_retries)
                    .unwrap_or(0);
                if retry && used < i64::from(cfg.verify.max_retries) {
                    shared.db.set_verify_retries(&key, used + 1)?;
                    let body = v.failure_followup(cfg.verify.max_output_bytes);
                    shared.db.enqueue_message(&key, &body, "verify")?;
                    shared.db.set_status(&key, "queued")?;
                    notify_run(
                        &cfg.telegram,
                        &format!("verify retry {key} provider={}", provider.name),
                        task.shadow,
                    );
                    shared.kick.notify_waiters();
                } else if retry {
                    shared.db.set_status(&key, "needs_attention")?;
                    notify_run(
                        &cfg.telegram,
                        &format!("needs_attention {key} (verify failed)"),
                        task.shadow,
                    );
                } else {
                    shared.db.set_verify_retries(&key, 0)?;
                    let more = !shared.db.pending_messages(&key)?.is_empty();
                    shared
                        .db
                        .set_status(&key, if more { "queued" } else { "waiting" })?;
                    notify_run(
                        &cfg.telegram,
                        &format!(
                            "finished {key} provider={} (verify recorded)",
                            provider.name
                        ),
                        task.shadow,
                    );
                    if more {
                        shared.kick.notify_waiters();
                    }
                }
            } else if task.role == "plan" {
                shared.db.set_verify_retries(&key, 0)?;
                finish_plan(&shared, &cfg, &key, &task, &result)?;
            } else {
                shared.db.set_verify_retries(&key, 0)?;
                match maybe_review(&shared, &cfg, &key, &task, &wt, assistant).await? {
                    ReviewAfter::Requeue => {
                        shared.db.set_status(&key, "queued")?;
                        shared.kick.notify_waiters();
                    }
                    ReviewAfter::Attention => {
                        shared.db.set_status(&key, "needs_attention")?;
                        notify_run(
                            &cfg.telegram,
                            &format!("needs_attention {key} (review)"),
                            task.shadow,
                        );
                    }
                    ReviewAfter::Skip | ReviewAfter::Done => {
                        finish_waiting(&shared, &cfg, &key, &provider.name, task.shadow)?;
                    }
                }
            }
        }
        "stalled" => {
            shared.db.unconsume_run(run_id)?;
            notify_run(
                &cfg.telegram,
                &format!(
                    "stalled {key} provider={} (CLI hang / heartbeat)",
                    provider.name
                ),
                task.shadow,
            );
            let retries = shared
                .db
                .get_task(&key)?
                .map(|t| t.stall_retries)
                .unwrap_or(0);
            shared.db.set_degraded_until(
                &provider.name,
                Some(&cooldown_rfc3339(provider.degraded_after_stall)),
            )?;
            if retries < i64::from(provider.health.stall_resumes) {
                shared.db.set_stall_retries(&key, retries + 1)?;
                shared.db.set_status(&key, "queued")?;
                tracing::info!(
                    task = %key,
                    retries = retries + 1,
                    max = provider.health.stall_resumes,
                    "retrying same session after stall"
                );
            } else {
                shared.db.set_cooldown(
                    &provider.name,
                    &cooldown_rfc3339(provider.health.unhealthy_cooldown),
                )?;
                handle_failover(&shared, &cfg, &key, &provider, &result, run_id, &wt).await?;
            }
        }
        "unavailable" => {
            shared.db.set_cooldown(
                &provider.name,
                &cooldown_rfc3339(provider.health.unhealthy_cooldown),
            )?;
            if !try_transient_retry(&shared, &cfg, &key, &task, &provider, &result, run_id)? {
                shared.db.unconsume_run(run_id)?;
                notify_run(
                    &cfg.telegram,
                    &format!("fallback {key} {} (unavailable)", provider.name),
                    task.shadow,
                );
                handle_failover(&shared, &cfg, &key, &provider, &result, run_id, &wt).await?;
            }
        }
        "limit" | "auth" => {
            if result.outcome == "limit"
                && try_transient_retry(&shared, &cfg, &key, &task, &provider, &result, run_id)?
            {
                shared.db.set_cooldown(
                    &provider.name,
                    &cooldown_rfc3339(provider.cooldown_after_limit),
                )?;
            } else {
                shared.db.set_cooldown(
                    &provider.name,
                    &cooldown_rfc3339(provider.cooldown_after_limit),
                )?;
                shared.db.unconsume_run(run_id)?;
                notify_run(
                    &cfg.telegram,
                    &format!("fallback {key} {} ({})", provider.name, result.outcome),
                    task.shadow,
                );
                handle_failover(&shared, &cfg, &key, &provider, &result, run_id, &wt).await?;
            }
        }
        "cancelled" => {
            shared.db.set_status(&key, "failed")?;
            notify_run(&cfg.telegram, &format!("cancelled {key}"), task.shadow);
            notify_held_dependents(&shared, &cfg, &key, task.shadow);
        }
        "needs_attention" => {
            shared.db.unconsume_run(run_id)?;
            notify_run(
                &cfg.telegram,
                &format!("fallback {key} {} (blocked or unfinished)", provider.name),
                task.shadow,
            );
            handle_failover(&shared, &cfg, &key, &provider, &result, run_id, &wt).await?;
        }
        _ => {
            if try_transient_retry(&shared, &cfg, &key, &task, &provider, &result, run_id)? {
                if result.classified == Some(ErrorClass::Server)
                    || result.classified == Some(ErrorClass::Unavailable)
                {
                    shared.db.set_cooldown(
                        &provider.name,
                        &cooldown_rfc3339(provider.health.unhealthy_cooldown),
                    )?;
                }
            } else if result.classified == Some(ErrorClass::Server)
                || result.classified == Some(ErrorClass::Unavailable)
            {
                shared.db.set_cooldown(
                    &provider.name,
                    &cooldown_rfc3339(provider.health.unhealthy_cooldown),
                )?;
                shared.db.unconsume_run(run_id)?;
                handle_failover(&shared, &cfg, &key, &provider, &result, run_id, &wt).await?;
            } else {
                shared.db.set_status(&key, "failed")?;
                notify_run(
                    &cfg.telegram,
                    &format!("failed {key} provider={}", provider.name),
                    task.shadow,
                );
                notify_held_dependents(&shared, &cfg, &key, task.shadow);
            }
        }
    }
    if assistant {
        crate::telegram::deliver_assistant_result(&shared, &key, run_id, &final_outcome);
    }
    if final_outcome != "success" {
        let _ = crate::output::capture(&shared, &key);
    }
    if !assistant && cfg.lessons_enabled_for(&task) {
        let status_now = shared
            .db
            .get_task(&key)
            .ok()
            .flatten()
            .map(|t| t.status)
            .unwrap_or_default();
        if let Some(signal) = crate::lessons::signal_after_run(
            &status_now,
            &final_outcome,
            task.verify_retries,
            task.review_rounds,
        ) {
            crate::lessons::spawn_reflect(shared.clone(), key.clone(), signal, Some(run_id));
        }
    }
    if task.role == crate::compare::ARM_ROLE {
        crate::compare::after_arm(&shared, &key);
    }
    shared.kick.notify_waiters();
    Ok(())
}

async fn handle_failover(
    shared: &Shared,
    cfg: &Config,
    key: &str,
    current: &Provider,
    result: &runner::RunResult,
    run_id: i64,
    worktree: &std::path::Path,
) -> Result<()> {
    if let Some(task) = shared.db.get_task(key)?.filter(|t| t.pin_strict) {
        fail_strict_arm(
            shared,
            cfg,
            &task,
            Some(run_id),
            &format!("{} ended with {}", current.name, result.outcome),
        );
        return Ok(());
    }
    let mut tried = shared
        .db
        .get_task(key)?
        .and_then(|t| t.tried)
        .unwrap_or_default();
    if !tried.is_empty() {
        tried.push(',');
    }
    tried.push_str(&current.name);
    shared.db.set_tried(key, Some(&tried))?;
    shared.db.set_provider_session(key, None, None)?;

    let task = shared
        .db
        .get_task(key)?
        .ok_or_else(|| anyhow::anyhow!("task {key} vanished during failover"))?;
    let brief = crate::handoff::build_for_switch(
        cfg,
        &shared.db,
        key,
        &task,
        worktree,
        &current.name,
        &result.outcome,
        Some(result),
        Some(run_id),
    )
    .unwrap_or_else(|e| {
        format!(
            "Previous provider {} ended ({}). Continue in the same worktree. Last output: {} ({e:#})",
            current.name,
            result.outcome,
            if result.snippet.is_empty() {
                "(none)"
            } else {
                &result.snippet
            }
        )
    });
    shared.db.set_handoff(key, Some(&brief))?;
    let _ = shared.db.set_run_handoff(run_id, &brief);
    shared.db.set_status(key, "queued")?;

    let inflight = shared.inflight.lock().unwrap().clone();
    let task = shared.db.get_task(key)?.unwrap();
    if pick_provider(cfg, &shared.db, &inflight, &task, &Clock::system())?.is_none() {
        if waiting_for_desktop(cfg, &crate::task::task_needs(&task.needs, &task.labels)) {
            // stays queued until the graphical session is back
            shared.db.set_tried(key, None)?;
        } else if settle_no_provider(cfg, &shared.db, key, &Clock::system())? == "queued" {
            // wait until a cooldown or quota window ends
        } else {
            notify_run(
                &cfg.telegram,
                &format!("failed {key} (no remaining provider)"),
                task.shadow,
            );
        }
    }
    Ok(())
}

fn settle_no_provider(cfg: &Config, db: &Db, key: &str, clock: &Clock) -> Result<&'static str> {
    if waiting_on_cooldown(cfg, db, clock)? {
        db.set_tried(key, None)?;
        db.set_status(key, "queued")?;
        Ok("queued")
    } else {
        db.set_status(key, "failed")?;
        Ok("failed")
    }
}

fn waiting_on_cooldown(cfg: &Config, db: &Db, clock: &Clock) -> Result<bool> {
    for p in &cfg.providers {
        let enabled = db
            .enabled_override(&p.name)
            .ok()
            .flatten()
            .unwrap_or(p.enabled);
        if !enabled || !p.cli.exists() {
            continue;
        }
        let (cooling, _) = db.provider_blocked(&p.name)?;
        if cooling
            || crate::quota::limit_reset_active(db, &p.name, clock)?
            || crate::quota::over_quota(db, p, clock)?
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn shadow_source_repo(cfg: &Config, task: &crate::db::Task) -> Result<std::path::PathBuf> {
    if let Some(r) = task.repo.as_deref().filter(|s| !s.is_empty()) {
        let p = std::path::PathBuf::from(r);
        if p.join(".git").exists() || p.join(".git").is_file() {
            return Ok(p);
        }
    }
    crate::shadow::ensure_shadow_repo(cfg)
}

enum ReviewAfter {
    Skip,
    Requeue,
    Done,
    Attention,
}

fn failover_has_next(
    shared: &Shared,
    cfg: &Config,
    task: &crate::db::Task,
    current: &Provider,
) -> Result<bool> {
    if task.pin_strict {
        return Ok(false);
    }
    let mut tried: Vec<&str> = task
        .tried
        .as_deref()
        .unwrap_or("")
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    if !tried.contains(&current.name.as_str()) {
        tried.push(current.name.as_str());
    }
    let mut probe = task.clone();
    probe.tried = Some(tried.join(","));
    let mut inflight = shared.inflight.lock().unwrap().clone();
    // This run is still in-flight; a real failover happens after it drops.
    inflight.remove(&task.key);
    Ok(pick_provider(cfg, &shared.db, &inflight, &probe, &Clock::system())?.is_some())
}

fn try_transient_retry(
    shared: &Shared,
    cfg: &Config,
    key: &str,
    task: &crate::db::Task,
    provider: &Provider,
    result: &runner::RunResult,
    run_id: i64,
) -> Result<bool> {
    if !cfg.retry.enabled {
        return Ok(false);
    }
    if task.pin_strict {
        return Ok(false);
    }
    let now = Clock::system().now();
    let class = crate::retry::classify_result(result, now);
    if !class.transient {
        return Ok(false);
    }
    // Another free provider still wins (existing failover). Retry is for
    // when this task would otherwise sit failed or wait on the same CLI.
    if failover_has_next(shared, cfg, task, provider)? {
        return Ok(false);
    }
    let attempt = shared
        .db
        .get_task(key)?
        .map(|t| t.retry_attempt)
        .unwrap_or(task.retry_attempt)
        + 1;
    let Some(delay) = crate::retry::backoff_delay(&cfg.retry, attempt as u32) else {
        return Ok(false);
    };
    let next = now + chrono::Duration::from_std(delay).unwrap_or_default();
    let next_at = next.to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    shared
        .db
        .set_retry(key, attempt, Some(&next_at), &class.reason)?;
    let session = shared.db.task_session(key, &provider.name)?;
    let resume = crate::retry::can_resume(provider, session.as_deref());
    if resume {
        shared
            .db
            .enqueue_message(key, &crate::retry::continue_prompt(&class.reason), "retry")?;
    } else {
        shared.db.unconsume_run(run_id)?;
        shared
            .db
            .set_provider_session(key, Some(&provider.name), None)?;
    }
    shared.db.set_status(key, "queued")?;
    let wait = crate::duration::format_duration(delay);
    let how = if resume { "resume" } else { "rerun" };
    notify_run(
        &cfg.telegram,
        &format!("retry {key} in {wait} ({}, {how})", class.reason),
        task.shadow,
    );
    tracing::info!(
        task = %key,
        attempt,
        next = %next_at,
        reason = %class.reason,
        resume,
        "scheduled transient retry"
    );
    Ok(true)
}

fn hold_blocked(shared: &Shared, cfg: &Config, task: &crate::db::Task, reason: &str) {
    if let Err(e) = shared.db.set_status(&task.key, "held") {
        tracing::warn!(task = %task.key, "hold status: {e:#}");
        return;
    }
    if let Err(e) = shared.db.set_hold(&task.key, reason, task.hold_notified) {
        tracing::warn!(task = %task.key, "hold reason: {e:#}");
    }
    if task.hold_notified {
        return;
    }
    notify_run(
        &cfg.telegram,
        &format!("held {} ({reason})", task.key),
        task.shadow,
    );
    let _ = shared.db.set_hold(&task.key, reason, true);
}

fn notify_held_dependents(shared: &Shared, cfg: &Config, key: &str, shadow: bool) {
    let held = match crate::deps::hold_dependents(&shared.db, key) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(task = %key, "hold dependents: {e:#}");
            return;
        }
    };
    for (child, reason) in held {
        let already = shared
            .db
            .get_task(&child)
            .ok()
            .flatten()
            .is_some_and(|t| t.hold_notified);
        if already {
            continue;
        }
        notify_run(&cfg.telegram, &format!("held {child} ({reason})"), shadow);
        let _ = shared.db.set_hold(&child, &reason, true);
    }
}

fn finish_waiting(
    shared: &Shared,
    cfg: &Config,
    key: &str,
    provider: &str,
    shadow: bool,
) -> Result<()> {
    let more = !shared.db.pending_messages(key)?.is_empty();
    shared
        .db
        .set_status(key, if more { "queued" } else { "waiting" })?;
    notify_run(
        &cfg.telegram,
        &format!("finished {key} provider={provider}"),
        shadow,
    );
    if more {
        shared.kick.notify_waiters();
    } else if let Err(e) = crate::output::finalize(shared, key) {
        tracing::warn!(task = %key, "output delivery: {e:#}");
    }
    maybe_finish_parent(shared, key)?;
    Ok(())
}

fn maybe_finish_parent(shared: &Shared, child_key: &str) -> Result<()> {
    let Some(task) = shared.db.get_task(child_key)? else {
        return Ok(());
    };
    let Some(parent) = task.parent_key else {
        return Ok(());
    };
    // Compare parents are settled by the judge (`compare::after_arm`).
    if task.role == crate::compare::ARM_ROLE {
        return Ok(());
    }
    let kids = shared.db.children_of(&parent)?;
    if kids.is_empty() {
        return Ok(());
    }
    if kids
        .iter()
        .all(|k| matches!(k.status.as_str(), "waiting" | "done"))
    {
        shared.db.set_status(&parent, "waiting")?;
    }
    Ok(())
}

fn finish_plan(
    shared: &Shared,
    cfg: &Config,
    key: &str,
    task: &crate::db::Task,
    result: &runner::RunResult,
) -> Result<()> {
    match crate::plan::parse_plan(&result.snippet).or_else(|_| {
        let events = shared.db.events_for_run(
            shared
                .db
                .list_runs(Some(key))?
                .into_iter()
                .rev()
                .find(|r| r.outcome.as_deref() == Some("success"))
                .map(|r| r.id)
                .unwrap_or(0),
        )?;
        let blob = events
            .iter()
            .map(|e| e.raw.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        crate::plan::parse_plan(&blob)
    }) {
        Ok(plan) => {
            let kids = crate::plan::apply_plan(&shared.db, task, &plan, cfg.planner.max_subtasks)?;
            if kids.len() <= 1 && kids.first().map(|k| k.as_str()) == Some(key) {
                shared.db.set_status(key, "queued")?;
            } else if kids.is_empty() {
                shared.db.set_task_role(key, "worker")?;
                shared.db.set_status(key, "queued")?;
            } else {
                notify_run(
                    &cfg.telegram,
                    &format!("planned {key} → {} subtasks", kids.len()),
                    task.shadow,
                );
            }
            shared.kick.notify_waiters();
        }
        Err(e) => {
            tracing::warn!(task = %key, "planner parse failed: {e:#}; running as one task");
            shared.db.set_task_role(key, "worker")?;
            shared.db.set_status(key, "queued")?;
            shared.kick.notify_waiters();
        }
    }
    Ok(())
}

async fn maybe_review(
    shared: &Shared,
    cfg: &Config,
    key: &str,
    task: &crate::db::Task,
    wt: &std::path::Path,
    assistant: bool,
) -> Result<ReviewAfter> {
    if !cfg.review.enabled
        || assistant
        || task.shadow
        || task.role == "arm"
        || crate::plan::is_driver_role(&task.role)
    {
        return Ok(ReviewAfter::Skip);
    }
    let Some(mut driver) = pick_driver_fallback(cfg, &shared.db) else {
        return Ok(ReviewAfter::Skip);
    };
    crate::review::apply_review_args(&mut driver, cfg.review.max_turns);
    let diff = git_diff(wt);
    let prompt = crate::review::review_prompt(key, &diff);
    let run_id = shared.db.start_run(key, &driver.name, &driver.model)?;
    let turn_id = shared.db.start_turn(run_id, key)?;
    let pid_slot = Arc::new(Mutex::new(None));
    let files_dir = runner::runtime_files_dir(&cfg.state_db, key, wt, false);
    let result = runner::run_cli(
        &shared.db, cfg, &driver, wt, &prompt, None, true, run_id, turn_id, key, pid_slot, None,
        None, None, false, &files_dir,
    )
    .await;
    let result = match result {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(task = %key, "review: {e:#}");
            let _ = shared.db.finish_run(run_id, "failed", None);
            return Ok(ReviewAfter::Skip);
        }
    };
    shared
        .db
        .finish_run(run_id, &result.outcome, result.exit_code)?;
    shared.db.finish_turn(turn_id, &result.outcome)?;
    if result.outcome != "success" {
        tracing::warn!(task = %key, outcome = %result.outcome, "review skipped");
        return Ok(ReviewAfter::Skip);
    }
    match crate::review::parse_verdict(&result.snippet) {
        Ok(crate::review::Verdict::Approve) => Ok(ReviewAfter::Done),
        Ok(crate::review::Verdict::RequestChanges(comments)) => {
            let used = task.review_rounds;
            if used >= i64::from(cfg.review.max_rounds) {
                return Ok(ReviewAfter::Attention);
            }
            shared.db.set_review_rounds(key, used + 1)?;
            let body = if comments.is_empty() {
                "Review requested changes. Address the review comments and continue.".into()
            } else {
                format!("Review requested changes:\n{comments}")
            };
            shared.db.enqueue_message(key, &body, "review")?;
            notify_run(
                &cfg.telegram,
                &format!("review changes {key} round {}", used + 1),
                task.shadow,
            );
            Ok(ReviewAfter::Requeue)
        }
        Err(e) => {
            tracing::warn!(task = %key, "review parse: {e:#}");
            Ok(ReviewAfter::Skip)
        }
    }
}

pub(crate) fn pick_driver_fallback(cfg: &Config, db: &Db) -> Option<Provider> {
    let empty = HashMap::new();
    let dummy = crate::db::Task {
        role: "review".into(),
        ..Default::default()
    };
    pick_provider(cfg, db, &empty, &dummy, &Clock::system())
        .ok()
        .flatten()
}

fn git_diff(wt: &std::path::Path) -> String {
    let out = crate::util::git_capture()
        .args(["diff", "HEAD"])
        .current_dir(wt)
        .output();
    match out {
        Ok(o) => {
            let mut s = String::from_utf8_lossy(&o.stdout).into_owned();
            if s.len() > 80_000 {
                s.truncate(80_000);
                s.push_str("\n…truncated…\n");
            }
            if s.trim().is_empty() {
                "(no unstaged diff; review the branch)".into()
            } else {
                s
            }
        }
        Err(_) => "(git diff unavailable)".into(),
    }
}

fn grok_fork_from(
    db: &Db,
    key: &str,
    provider: &Provider,
    new_session: bool,
    pending: &[crate::db::Message],
    session: Option<&str>,
) -> Result<Option<String>> {
    if provider.kind != "grok" || new_session {
        return Ok(None);
    }
    let retry = pending
        .iter()
        .any(|m| m.source == "verify" || m.source == "review")
        || matches!(
            db.last_run_outcome(key)?.as_deref(),
            Some("stalled" | "verify_failed")
        );
    if retry {
        Ok(session.filter(|s| !s.is_empty()).map(|s| s.to_string()))
    } else {
        Ok(None)
    }
}

fn spawn_workspace_changed(previous: Option<&str>, next: &std::path::Path) -> bool {
    match previous.map(str::trim).filter(|s| !s.is_empty()) {
        None => false,
        Some(prev) => !crate::config::paths_eq(std::path::Path::new(prev), next),
    }
}

fn ensure_session(db: &Db, key: &str, provider: &Provider) -> Result<(Option<String>, bool)> {
    // Never resume another provider's session. Sessions are stored per
    // (task, provider); the task-level session_id is only the current one.
    if let Some(s) = db.task_session(key, &provider.name)? {
        if !s.is_empty() {
            return Ok((Some(s), false));
        }
    }
    if provider.session_id_flag.is_empty() {
        return Ok((None, false));
    }
    let sid = new_session_uuid();
    db.set_provider_session(key, Some(&provider.name), Some(&sid))?;
    Ok((Some(sid), true))
}

/// One usage row per run. Last CLI usage event wins (streaming CLIs repeat totals).
fn pick_pool_slot(
    shared: &Shared,
    repo: &std::path::Path,
    provider: &str,
    key: &str,
    existing: Option<&str>,
    max_slots: u32,
) -> u32 {
    if let Some(path) = existing {
        if let Some(slot) = worktree::slot_from_pool_path(std::path::Path::new(path), provider) {
            if slot < max_slots {
                return slot;
            }
        }
    }
    let inflight = shared.inflight.lock().unwrap();
    let mut used = std::collections::HashSet::new();
    let reserve = |used: &mut std::collections::HashSet<u32>, t: &crate::db::Task| {
        if t.key == key {
            return;
        }
        if t.provider.as_deref() != Some(provider) {
            return;
        }
        let Some(wt) = t.worktree.as_deref() else {
            return;
        };
        let Some(slot) = worktree::slot_from_pool_path(std::path::Path::new(wt), provider) else {
            return;
        };
        let repo_ok = t
            .repo
            .as_deref()
            .is_some_and(|r| crate::config::paths_eq(std::path::Path::new(r), repo));
        if repo_ok {
            used.insert(slot);
        }
    };
    for k in inflight.keys() {
        if let Ok(Some(t)) = shared.db.get_task(k) {
            reserve(&mut used, &t);
        }
    }
    if let Ok(tasks) = shared.db.list_tasks() {
        for t in tasks {
            if matches!(t.status.as_str(), "queued" | "running" | "waiting") {
                reserve(&mut used, &t);
            }
        }
    }
    for i in 0..max_slots {
        if !used.contains(&i) {
            return i;
        }
    }
    0
}

fn record_run_usage(
    db: &Db,
    run_id: i64,
    turn_id: Option<i64>,
    provider: &Provider,
    result: &runner::RunResult,
    is_resume: bool,
) -> Result<()> {
    let (u, raw) = if let Some(raw) = result.usage_raw.last() {
        (extract_usage(&provider.kind, raw), raw.to_string())
    } else if result.usage.input
        + result.usage.output
        + result.usage.cache_read
        + result.usage.cache_write
        > 0
        || result.usage.cost_usd.is_some()
    {
        (result.usage.clone(), "{}".into())
    } else {
        return Ok(());
    };
    db.insert_usage(
        run_id,
        turn_id,
        &provider.name,
        u.input,
        u.output,
        u.cache_read,
        u.cache_write,
        u.cost_usd,
        Some(usage_cost_kind(u.cost_usd)),
        &raw,
    )?;
    db.set_usage_resume(run_id, is_resume)
}

/// Low-cache warning is only meaningful after a session has had a successful
/// run. A stall (or a brand-new session) is a cold first run.
fn should_warn_low_cache(db: &Db, key: &str, session: Option<&str>) -> Result<bool> {
    let Some(sid) = session.filter(|s| !s.is_empty()) else {
        return Ok(false);
    };
    Ok(db.successful_session_runs(key, sid)? > 0)
}

pub(crate) fn fail_claimed(shared: &Shared, key: &str, err: &anyhow::Error) {
    let reason = format!("{err:#}");
    tracing::error!(task = %key, "run failed: {reason}");
    if let Ok(Some(run_id)) = shared.db.open_run_for_task(key) {
        let _ = shared.db.finish_run(run_id, "failed", Some(-1));
    }
    let _ = shared.db.set_task_result(key, &reason, "", None);
    let _ = shared.db.set_status(key, "failed");
}

/// Startup only: a run left open by a previous `atr serve` whose CLI is still
/// alive (serve was SIGKILLed; the CLI runs in its own process group) would
/// keep the task `running` forever with nobody reading its output. Kill it
/// when the PID still looks like that provider's CLI, so `reconcile` fails it.
pub fn reap_orphans_at_start(shared: &Shared) -> Result<usize> {
    let cfg = shared.config.read().unwrap().clone();
    let mut n = 0;
    for r in shared.db.open_runs()? {
        if shared.inflight.lock().unwrap().contains_key(&r.task_key) {
            continue;
        }
        if r.owner_alive() {
            tracing::info!(
                run = r.id,
                task = %r.task_key,
                owner_pid = r.owner_pid,
                "leaving open run; owner process is still alive"
            );
            continue;
        }
        let Some(pid) = r.pid.filter(|p| *p > 0) else {
            continue;
        };
        if !pid_alive(pid) {
            continue;
        }
        let Some(p) = cfg.provider(&r.provider) else {
            continue;
        };
        if !pid_looks_like(pid, &p.cli) {
            tracing::warn!(
                run = r.id,
                pid,
                "open run PID alive but not the provider CLI; leaving it"
            );
            continue;
        }
        tracing::warn!(run = r.id, task = %r.task_key, pid, "killing orphaned CLI from a previous serve");
        crate::runner::terminate_tree_now(pid as u32);
        n += 1;
    }
    if n > 0 {
        std::thread::sleep(std::time::Duration::from_millis(300));
    }
    Ok(n)
}

fn pid_looks_like(pid: i64, cli: &std::path::Path) -> bool {
    let want = cli
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_string();
    if want.is_empty() {
        return false;
    }
    let out = std::process::Command::new("ps")
        .args(["-o", "command=", "-p", &pid.to_string()])
        .output();
    match out {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).contains(&want),
        _ => false,
    }
}

/// Runs `reconcile` every minute so a task whose CLI died outside the
/// scheduler's view does not stay `running` until the next restart.
pub async fn reconcile_loop(shared: std::sync::Arc<Shared>) {
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        let _ = shared.db.touch_owner();
        match reconcile(&shared) {
            Ok(n) if n > 0 => tracing::warn!(n, "reconciled orphan runs"),
            Ok(_) => {}
            Err(e) => tracing::warn!("reconcile: {e:#}"),
        }
    }
}

/// Fail an open run (`outcome` NULL) whose CLI is dead or never started.
/// In-flight runs owned by this process are left alone. Does not overwrite
/// a non-empty `tasks.result_text` (that may be an older success).
pub fn reclaim_dead_open_run(shared: &Shared, key: &str) -> Result<bool> {
    if shared.inflight.lock().unwrap().contains_key(key) {
        return Ok(false);
    }
    let me = crate::owner::current();
    if shared
        .db
        .task_owner(key)?
        .is_some_and(|o| o.is_alive() && o != me)
    {
        return Ok(false);
    }
    let Some(run_id) = shared.db.open_run_for_task(key)? else {
        return Ok(false);
    };
    let runs = shared.db.list_runs(Some(key))?;
    let Some(r) = runs.into_iter().rev().find(|row| row.id == run_id) else {
        return Ok(false);
    };
    if r.owner_alive() && r.owner_pid != Some(me.pid) {
        return Ok(false);
    }
    let alive = r.pid.filter(|p| *p > 0).is_some_and(pid_alive);
    if alive {
        return Ok(false);
    }
    tracing::warn!(
        run = r.id,
        task = %key,
        pid = ?r.pid,
        "reclaiming dead or never-started run"
    );
    shared.db.finish_run(run_id, "failed", Some(-1))?;
    if let Ok(Some(t)) = shared.db.get_task(key) {
        if t.status == "running" {
            shared.db.set_status(key, "failed")?;
        }
        if t.result_text.is_empty() {
            let _ = shared
                .db
                .set_task_result(key, "orphaned run (process gone)", "", None);
        }
    }
    Ok(true)
}

/// After a zombie is closed, pending messages must become `queued` or
/// `runnable_keys` (status IN queued/waiting) will never see them.
pub fn requeue_if_pending(shared: &Shared, key: &str) -> Result<bool> {
    if shared.inflight.lock().unwrap().contains_key(key) {
        return Ok(false);
    }
    if shared.db.open_run_for_task(key)?.is_some() {
        return Ok(false);
    }
    if shared.db.pending_messages(key)?.is_empty() {
        return Ok(false);
    }
    // Human-needed `held` (after: failed) must survive serve restart / TUI
    // reclaim. `queue_unless_running` would otherwise flip it to queued.
    if shared.db.get_task(key)?.is_some_and(|t| t.status == "held") {
        return Ok(false);
    }
    shared.db.queue_unless_running(key)
}

pub fn reconcile(shared: &Shared) -> Result<usize> {
    let runs = shared.db.open_runs()?;
    let mut n = 0;
    let mut touched: Vec<String> = Vec::new();
    for r in runs {
        if reclaim_dead_open_run(shared, &r.task_key)? {
            n += 1;
            touched.push(r.task_key);
        }
    }
    for t in shared.db.list_tasks()? {
        if t.status != "running" {
            continue;
        }
        if shared.inflight.lock().unwrap().contains_key(&t.key) {
            continue;
        }
        let me = crate::owner::current();
        if shared
            .db
            .task_owner(&t.key)?
            .is_some_and(|o| o.is_alive() && o != me)
        {
            continue;
        }
        let alive = match shared.db.open_run_for_task(&t.key)? {
            Some(_) => shared
                .db
                .list_runs(Some(&t.key))?
                .into_iter()
                .rev()
                .find(|r| r.ended.is_none())
                .map(|r| {
                    let other_owner = r.owner_alive() && r.owner_pid != Some(me.pid);
                    other_owner || r.pid.filter(|p| *p > 0).is_some_and(pid_alive)
                })
                .unwrap_or(false),
            None => false,
        };
        if alive {
            continue;
        }
        tracing::warn!(task = %t.key, "reconciling orphaned running task");
        if let Ok(Some(run_id)) = shared.db.open_run_for_task(&t.key) {
            let _ = shared.db.finish_run(run_id, "failed", Some(-1));
        }
        if t.result_text.is_empty() {
            let _ = shared.db.set_task_result(
                &t.key,
                "orphaned running task (process gone or workspace setup failed)",
                "",
                None,
            );
        }
        shared.db.set_status(&t.key, "failed")?;
        n += 1;
        touched.push(t.key);
    }
    for key in touched {
        let _ = requeue_if_pending(shared, &key);
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;

    #[test]
    fn serve_skips_tui_session_keys() {
        assert!(serve_dispatches("demo-1"));
        assert!(serve_dispatches("tg42"));
        assert!(!serve_dispatches("cli-default"));
        assert!(!serve_dispatches("cli-work"));
        assert!(serve_dispatches("cli-"));
        assert!(serve_dispatches("acme-1"));
    }

    #[test]
    fn record_run_usage_last_event_wins_and_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("s.db")).unwrap();
        db.upsert_task("acme-u", "/repo", None).unwrap();
        let run = db.start_run("acme-u", "grok", "m").unwrap();
        db.finish_run(run, "success", Some(0)).unwrap();
        let grok = stub_provider("grok", "--session-id");
        let result = runner::RunResult {
            outcome: "success".into(),
            exit_code: Some(0),
            session_id: Some("s".into()),
            usage: crate::providers::UsageNumbers {
                input: 30,
                output: 6,
                cache_read: 270,
                cache_write: 15,
                cost_usd: Some(0.004),
            },
            usage_raw: vec![
                serde_json::json!({"usage":{"input_tokens":10,"output_tokens":2,"cache_read_input_tokens":90,"cache_creation_input_tokens":5}}),
                serde_json::json!({"usage":{"input_tokens":10,"output_tokens":2,"cache_read_input_tokens":90,"cache_creation_input_tokens":5}}),
                serde_json::json!({"usage":{"input_tokens":10,"output_tokens":2,"cache_read_input_tokens":90,"cache_creation_input_tokens":5},"total_cost_usd":0.004}),
            ],
            snippet: String::new(),
            classified: None,
            stderr: String::new(),
        };
        record_run_usage(&db, run, Some(1), &grok, &result, false).unwrap();
        record_run_usage(&db, run, Some(1), &grok, &result, false).unwrap();
        let rows = db.list_usage_for_run(run).unwrap();
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(rows[0].input, 10);
        assert_eq!(rows[0].output, 2);
        assert_eq!(rows[0].cache_read, 90);
        assert!((rows[0].cost_usd.unwrap() - 0.004).abs() < 1e-9);
        assert_eq!(rows[0].cost_kind.as_deref(), Some("est"));
    }

    #[test]
    fn record_run_usage_plan_when_subscription_has_no_cost() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("s.db")).unwrap();
        db.upsert_task("acme-c", "/repo", None).unwrap();
        let run = db.start_run("acme-c", "cursor", "m").unwrap();
        db.finish_run(run, "success", Some(0)).unwrap();
        let cursor = stub_provider("cursor", "--resume");
        let result = runner::RunResult {
            outcome: "success".into(),
            exit_code: Some(0),
            session_id: Some("s".into()),
            usage: crate::providers::UsageNumbers {
                input: 80,
                output: 40,
                cache_read: 400,
                cache_write: 15,
                cost_usd: None,
            },
            usage_raw: vec![serde_json::json!({
                "usage":{"inputTokens":80,"outputTokens":40,"cacheReadTokens":400,"cacheWriteTokens":15}
            })],
            snippet: String::new(),
            classified: None,
            stderr: String::new(),
        };
        record_run_usage(&db, run, Some(1), &cursor, &result, false).unwrap();
        let rows = db.list_usage_for_run(run).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].cost_kind.as_deref(), Some("plan"));
        assert!(rows[0].cost_usd.is_none());
    }

    #[test]
    fn low_cache_warn_skips_cold_session() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("s.db")).unwrap();
        db.upsert_task("acme-9", "/repo", Some(9)).unwrap();
        assert!(!should_warn_low_cache(&db, "acme-9", None).unwrap());
        let stalled = db.start_run("acme-9", "grok", "m").unwrap();
        db.set_run_session(stalled, "sess-9").unwrap();
        db.finish_run(stalled, "stalled", None).unwrap();
        assert!(
            !should_warn_low_cache(&db, "acme-9", Some("sess-9")).unwrap(),
            "a stalled first run is still a cold session"
        );
        let ok = db.start_run("acme-9", "grok", "m").unwrap();
        db.set_run_session(ok, "sess-9").unwrap();
        db.finish_run(ok, "success", Some(0)).unwrap();
        assert!(should_warn_low_cache(&db, "acme-9", Some("sess-9")).unwrap());
        assert!(!should_warn_low_cache(&db, "acme-9", Some("other")).unwrap());
    }

    fn stub_provider(name: &str, session_id_flag: &str) -> Provider {
        Provider {
            name: name.into(),
            kind: name.into(),
            preset: String::new(),
            enabled: true,
            cli: std::path::PathBuf::from(name),
            legacy_expected_version: false,
            version_args: vec!["--version".into()],
            model: "m".into(),
            model_flag: "--model".into(),
            max_parallel: 1,
            cooldown_after_limit: Duration::from_secs(1),
            degraded_after_stall: Duration::from_secs(2),
            heartbeat_timeout: Duration::from_secs(1),
            run_timeout: Duration::from_secs(1),
            resume_flag: "--resume".into(),
            session_id_flag: session_id_flag.into(),
            cwd_flag: String::new(),
            permission_mode: String::new(),
            permission_mode_flag: "--permission-mode".into(),
            prompt_mode: "arg".into(),
            prompt_file_prefix: "@".into(),
            profile: String::new(),
            sandbox: String::new(),
            args: vec![],
            extra_args: vec![],
            config_overrides: vec![],
            quota: vec![],
            reported_usage: String::new(),
            skip_at: 0.9,
            reported_usage_interval: Duration::from_secs(600),
            reported_usage_timeout: Duration::from_secs(20),
            health: Default::default(),
            plugin: false,
            max_turns: 0,
            cache_window: std::time::Duration::from_secs(3600),
            system_prompt_flag: String::new(),
            keep_alive: String::new(),
            capabilities: Vec::new(),
        }
    }

    #[test]
    fn ensure_session_is_per_provider() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("s.db")).unwrap();
        db.upsert_task("acme-8", "/repo", Some(8)).unwrap();
        db.set_provider_session("acme-8", Some("claude"), Some("claude-sid"))
            .unwrap();
        let claude = stub_provider("claude", "");
        let grok = stub_provider("grok", "--session-id");

        let (sid, new) = ensure_session(&db, "acme-8", &claude).unwrap();
        assert_eq!(sid.as_deref(), Some("claude-sid"));
        assert!(!new, "must resume this provider's own session");

        let (sid, new) = ensure_session(&db, "acme-8", &grok).unwrap();
        assert!(new, "other provider must start a fresh session");
        let grok_sid = sid.expect("grok generates a uuid when session_id_flag is set");
        assert_ne!(grok_sid, "claude-sid");
        assert_eq!(
            db.task_session("acme-8", "claude").unwrap().as_deref(),
            Some("claude-sid")
        );
        assert_eq!(
            db.task_session("acme-8", "grok").unwrap().as_deref(),
            Some(grok_sid.as_str())
        );
    }

    fn two_provider_cfg(dir: &std::path::Path, budget: f64) -> crate::config::Config {
        let cli = if std::path::Path::new("/bin/true").exists() {
            "/bin/true"
        } else {
            "/usr/bin/true"
        };
        let text = format!(
            r#"
listen = "127.0.0.1:0"
state_db = "{db}"
worktree_root = "{root}"
repo_path = "{root}"
system_prompt_file = "{root}/p.md"
[github]
poll_interval = "60s"
[[provider]]
name = "claude"
kind = "claude"
cli = "{cli}"
[[provider.quota]]
window = "5h"
budget = {budget}
unit = "usd"
threshold = 0.85
[[provider]]
name = "grok"
kind = "grok"
cli = "{cli}"
"#,
            db = dir.join("s.db").display(),
            root = dir.display(),
            cli = cli,
            budget = budget,
        );
        std::fs::write(dir.join("p.md"), "p\n").unwrap();
        let raw: crate::config::RawConfig = toml::from_str(&text).unwrap();
        crate::config::Config::from_raw(raw).unwrap()
    }

    #[test]
    fn quota_skips_new_task_but_keeps_existing() {
        use chrono::{Duration as ChronoDuration, TimeZone, Utc};
        let dir = tempfile::tempdir().unwrap();
        let cfg = two_provider_cfg(dir.path(), 50.0);
        let db = Db::open(&dir.path().join("state.db")).unwrap();
        db.upsert_task("new-1", "/repo", None).unwrap();
        db.upsert_task("old-1", "/repo", None).unwrap();
        db.set_provider_session("old-1", Some("claude"), Some("sid"))
            .unwrap();
        let run = db.start_run("old-1", "claude", "m").unwrap();
        db.finish_run(run, "success", Some(0)).unwrap();
        let t0 = Utc.with_ymd_and_hms(2026, 10, 13, 12, 0, 0).unwrap();
        db.insert_usage_at(
            run,
            None,
            "claude",
            0,
            0,
            0,
            0,
            Some(45.0),
            Some("est"),
            "{}",
            &(t0 - ChronoDuration::hours(1)).to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        )
        .unwrap();
        let clock = crate::clock::Clock::frozen(t0);
        let inflight = HashMap::new();
        let new_task = db.get_task("new-1").unwrap().unwrap();
        let picked = pick_provider(&cfg, &db, &inflight, &new_task, &clock)
            .unwrap()
            .expect("overflow to grok");
        assert_eq!(picked.name, "grok");
        let old = db.get_task("old-1").unwrap().unwrap();
        let picked = pick_provider(&cfg, &db, &inflight, &old, &clock)
            .unwrap()
            .expect("existing stays on claude");
        assert_eq!(picked.name, "claude");
        let later = crate::clock::Clock::frozen(t0 + ChronoDuration::hours(5));
        let picked = pick_provider(&cfg, &db, &inflight, &new_task, &later)
            .unwrap()
            .expect("window rolled off");
        assert_eq!(picked.name, "claude");
    }

    #[test]
    fn reported_limit_skips_new_until_reset() {
        use chrono::{Duration as ChronoDuration, TimeZone, Utc};
        let dir = tempfile::tempdir().unwrap();
        let cfg = two_provider_cfg(dir.path(), 10_000.0);
        let db = Db::open(&dir.path().join("state.db")).unwrap();
        db.upsert_task("new-2", "/repo", None).unwrap();
        db.upsert_task("old-2", "/repo", None).unwrap();
        db.set_provider_session("old-2", Some("claude"), Some("sid"))
            .unwrap();
        let t0 = Utc.with_ymd_and_hms(2026, 10, 13, 12, 0, 0).unwrap();
        let reset = t0 + ChronoDuration::hours(2);
        db.record_limit(
            "claude",
            Some(&reset.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)),
            "429",
            "{}",
            &t0.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        )
        .unwrap();
        let clock = crate::clock::Clock::frozen(t0);
        let inflight = HashMap::new();
        let new_task = db.get_task("new-2").unwrap().unwrap();
        let picked = pick_provider(&cfg, &db, &inflight, &new_task, &clock)
            .unwrap()
            .unwrap();
        assert_eq!(picked.name, "grok");
        let old = db.get_task("old-2").unwrap().unwrap();
        assert_eq!(
            pick_provider(&cfg, &db, &inflight, &old, &clock)
                .unwrap()
                .unwrap()
                .name,
            "claude"
        );
        let after = crate::clock::Clock::frozen(reset + ChronoDuration::minutes(1));
        assert_eq!(
            pick_provider(&cfg, &db, &inflight, &new_task, &after)
                .unwrap()
                .unwrap()
                .name,
            "claude"
        );
    }

    #[test]
    fn reported_usage_skips_new_task() {
        use chrono::{Duration as ChronoDuration, TimeZone, Utc};
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = two_provider_cfg(dir.path(), 10_000.0);
        cfg.providers[0].reported_usage = "claude_stream".into();
        cfg.providers[0].skip_at = 0.9;
        let db = Db::open(&dir.path().join("state.db")).unwrap();
        db.upsert_task("new-r", "/repo", None).unwrap();
        db.upsert_task("old-r", "/repo", None).unwrap();
        db.set_provider_session("old-r", Some("claude"), Some("sid"))
            .unwrap();
        let t0 = Utc.with_ymd_and_hms(2026, 10, 13, 12, 0, 0).unwrap();
        let reset = t0 + ChronoDuration::hours(2);
        db.upsert_provider_usage(
            "claude",
            "five_hour",
            Some(0.95),
            None,
            None,
            Some(&reset.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)),
            Some("allowed"),
            &t0.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        )
        .unwrap();
        let clock = crate::clock::Clock::frozen(t0);
        let inflight = HashMap::new();
        let new_task = db.get_task("new-r").unwrap().unwrap();
        assert_eq!(
            pick_provider(&cfg, &db, &inflight, &new_task, &clock)
                .unwrap()
                .unwrap()
                .name,
            "grok"
        );
        let old = db.get_task("old-r").unwrap().unwrap();
        assert_eq!(
            pick_provider(&cfg, &db, &inflight, &old, &clock)
                .unwrap()
                .unwrap()
                .name,
            "claude"
        );
    }

    #[test]
    fn all_cooling_stays_queued() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = two_provider_cfg(dir.path(), 10_000.0);
        let db = Db::open(&dir.path().join("state.db")).unwrap();
        db.upsert_task("acme-cool", "/repo", None).unwrap();
        db.set_tried("acme-cool", Some("claude,grok")).unwrap();
        db.set_status("acme-cool", "running").unwrap();
        let until = crate::db::cooldown_rfc3339(std::time::Duration::from_secs(600));
        db.set_cooldown("claude", &until).unwrap();
        db.set_cooldown("grok", &until).unwrap();
        let clock = crate::clock::Clock::system();
        assert!(waiting_on_cooldown(&cfg, &db, &clock).unwrap());
        assert_eq!(
            settle_no_provider(&cfg, &db, "acme-cool", &clock).unwrap(),
            "queued"
        );
        let task = db.get_task("acme-cool").unwrap().unwrap();
        assert_eq!(task.status, "queued");
        assert!(task.tried.as_deref().unwrap_or("").is_empty());
    }

    #[test]
    fn no_cooldown_and_no_provider_fails() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = two_provider_cfg(dir.path(), 10_000.0);
        let db = Db::open(&dir.path().join("state.db")).unwrap();
        db.upsert_task("acme-done", "/repo", None).unwrap();
        db.set_tried("acme-done", Some("claude,grok")).unwrap();
        assert!(!waiting_on_cooldown(&cfg, &db, &crate::clock::Clock::system()).unwrap());
        assert_eq!(
            settle_no_provider(&cfg, &db, "acme-done", &crate::clock::Clock::system()).unwrap(),
            "failed"
        );
    }

    #[test]
    fn repo_inflight_counts_matching_path() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("s.db")).unwrap();
        db.upsert_task("demo-1-1", "/tmp/demo-1", Some(1)).unwrap();
        db.upsert_task("demo-2-1", "/tmp/demo-2", Some(1)).unwrap();
        let mut inflight = HashMap::new();
        inflight.insert(
            "demo-1-1".into(),
            InFlight {
                provider: "claude".into(),
                pid: 1,
                run_id: 1,
                kill: std::sync::Arc::new(std::sync::Mutex::new(None)),
                shadow: false,
            },
        );
        inflight.insert(
            "demo-2-1".into(),
            InFlight {
                provider: "claude".into(),
                pid: 2,
                run_id: 2,
                kill: std::sync::Arc::new(std::sync::Mutex::new(None)),
                shadow: false,
            },
        );
        let entry = crate::config::RepoEntry {
            name: "demo-1".into(),
            path: std::path::PathBuf::from("/tmp/demo-1"),
            github: "owner/demo-1".into(),
            label: "for:atr".into(),
            enabled: true,
            branch_prefix: String::new(),
            base: String::new(),
            system_prompt_file: std::path::PathBuf::new(),
            max_parallel: 1,
            lessons: None,
            shorthand: false,
        };
        assert_eq!(repo_inflight_count(&db, &inflight, &entry), 1);
    }

    #[test]
    fn order_runnable_prefers_in_window_then_same_provider() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("p.md"), "p\n").unwrap();
        let cli = if std::path::Path::new("/bin/true").exists() {
            "/bin/true"
        } else {
            "/usr/bin/true"
        };
        let text = format!(
            r#"
listen = "127.0.0.1:0"
state_db = "{db}"
worktree_root = "{root}"
repo_path = "{root}"
system_prompt_file = "{root}/p.md"
[[provider]]
name = "claude"
kind = "claude"
cli = "{cli}"
cache_window = "1h"
[[provider]]
name = "grok"
kind = "grok"
cli = "{cli}"
"#,
            db = dir.path().join("s.db").display(),
            root = dir.path().display(),
            cli = cli,
        );
        let raw: crate::config::RawConfig = toml::from_str(&text).unwrap();
        let cfg = crate::config::Config::from_raw(raw).unwrap();
        let shared = crate::app::Shared::from_config(dir.path().join("c.toml"), cfg).unwrap();
        shared.db.upsert_task("warm", "/repo", None).unwrap();
        shared.db.upsert_task("batch", "/repo", None).unwrap();
        shared.db.upsert_task("other", "/repo", None).unwrap();
        shared.db.upsert_task("done-grok", "/repo", None).unwrap();
        shared.db.set_status("warm", "waiting").unwrap();
        shared.db.set_status("batch", "queued").unwrap();
        shared.db.set_status("other", "queued").unwrap();
        shared.db.set_status("done-grok", "done").unwrap();
        shared
            .db
            .set_provider_session("warm", Some("claude"), Some("sid-warm"))
            .unwrap();
        shared
            .db
            .set_provider_session("batch", Some("grok"), None)
            .unwrap();
        shared
            .db
            .set_provider_session("other", Some("claude"), None)
            .unwrap();
        shared.db.enqueue_message("warm", "follow", "test").unwrap();
        shared.db.enqueue_message("batch", "new", "test").unwrap();
        shared.db.enqueue_message("other", "new", "test").unwrap();
        let warm_run = shared.db.start_run("warm", "claude", "m").unwrap();
        shared.db.finish_run(warm_run, "success", Some(0)).unwrap();
        let grok_run = shared.db.start_run("done-grok", "grok", "m").unwrap();
        shared.db.finish_run(grok_run, "success", Some(0)).unwrap();
        let keys = order_runnable(&shared).unwrap();
        assert_eq!(keys[0], "warm", "in-window follow-up first: {keys:?}");
        assert_eq!(
            keys[1], "batch",
            "last finished provider batched next: {keys:?}"
        );
        assert_eq!(keys[2], "other", "{keys:?}");
    }

    #[test]
    fn ollama_same_model_is_serial() {
        let dir = tempfile::tempdir().unwrap();
        let cli = if std::path::Path::new("/bin/true").exists() {
            "/bin/true"
        } else {
            "/usr/bin/true"
        };
        let text = format!(
            r#"
listen = "127.0.0.1:0"
state_db = "{db}"
worktree_root = "{root}"
repo_path = "{root}"
system_prompt_file = "{root}/p.md"
[[provider]]
name = "ol-a"
kind = "opencode"
preset = "ollama"
model = "ollama/llama3.2"
cli = "{cli}"
max_parallel = 2
[[provider]]
name = "ol-b"
kind = "opencode"
preset = "ollama"
model = "ollama/llama3.2"
cli = "{cli}"
max_parallel = 2
[[provider]]
name = "ol-c"
kind = "opencode"
preset = "ollama"
model = "ollama/mistral"
cli = "{cli}"
"#,
            db = dir.path().join("s.db").display(),
            root = dir.path().display(),
            cli = cli,
        );
        std::fs::write(dir.path().join("p.md"), "p\n").unwrap();
        let raw: crate::config::RawConfig = toml::from_str(&text).unwrap();
        let cfg = crate::config::Config::from_raw(raw).unwrap();
        let a = cfg.provider("ol-a").unwrap();
        let b = cfg.provider("ol-b").unwrap();
        let c = cfg.provider("ol-c").unwrap();
        assert!(a.is_ollama() && b.is_ollama() && c.is_ollama());
        let db = Db::open(&dir.path().join("s.db")).unwrap();
        let clock = crate::clock::Clock::system();
        let mut inflight = HashMap::new();
        inflight.insert(
            "t1".into(),
            InFlight {
                provider: "ol-a".into(),
                pid: 1,
                run_id: 1,
                kill: std::sync::Arc::new(std::sync::Mutex::new(None)),
                shadow: false,
            },
        );
        assert_eq!(
            unavailable(&cfg, a, &db, &inflight, false, &clock).unwrap(),
            Some("full")
        );
        assert_eq!(
            unavailable(&cfg, b, &db, &inflight, false, &clock).unwrap(),
            Some("full"),
            "same Ollama model must be serial even on another provider name"
        );
        assert_eq!(
            unavailable(&cfg, c, &db, &inflight, false, &clock).unwrap(),
            None,
            "a different Ollama model can run"
        );
    }

    fn existing_true_cli() -> &'static str {
        if std::path::Path::new("/bin/true").exists() {
            "/bin/true"
        } else {
            "/usr/bin/true"
        }
    }

    fn shared_for_reconcile(dir: &std::path::Path) -> std::sync::Arc<Shared> {
        std::fs::write(dir.join("p.md"), "p\n").unwrap();
        let cli = existing_true_cli();
        let text = format!(
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
cli = "{cli}"
"#,
            db = dir.join("s.db").display(),
            root = dir.display(),
        );
        let raw: crate::config::RawConfig = toml::from_str(&text).unwrap();
        let cfg = crate::config::Config::from_raw(raw).unwrap();
        Shared::from_config(dir.join("c.toml"), cfg).unwrap()
    }

    #[test]
    fn fail_claimed_marks_running_task_failed_with_reason() {
        let dir = tempfile::tempdir().unwrap();
        let shared = shared_for_reconcile(dir.path());
        shared.db.upsert_task("task-etc", "/etc", None).unwrap();
        shared.db.set_status("task-etc", "running").unwrap();
        fail_claimed(
            &shared,
            "task-etc",
            &anyhow::anyhow!("/etc is not a git repository"),
        );
        let t = shared.db.get_task("task-etc").unwrap().unwrap();
        assert_eq!(t.status, "failed");
        assert!(
            t.result_text.contains("not a git repository"),
            "{}",
            t.result_text
        );
    }

    fn shared_with_cli(dir: &std::path::Path, cli: &str) -> std::sync::Arc<Shared> {
        std::fs::write(dir.join("p.md"), "p\n").unwrap();
        let text = format!(
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
cli = "{cli}"
"#,
            db = dir.join("s.db").display(),
            root = dir.display(),
        );
        let raw: crate::config::RawConfig = toml::from_str(&text).unwrap();
        let cfg = crate::config::Config::from_raw(raw).unwrap();
        Shared::from_config(dir.join("c.toml"), cfg).unwrap()
    }

    /// A stand-in "CLI" in its own process group, so killing its group can
    /// never reach the test runner.
    fn spawn_sleeper() -> std::process::Child {
        use std::os::unix::process::CommandExt;
        std::process::Command::new("sleep")
            .arg("60")
            .process_group(0)
            .spawn()
            .unwrap()
    }

    fn wait_exit(child: &mut std::process::Child, ms: u64) -> bool {
        let end = std::time::Instant::now() + std::time::Duration::from_millis(ms);
        while std::time::Instant::now() < end {
            if child.try_wait().unwrap().is_some() {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        false
    }

    #[test]
    fn reap_orphans_at_start_kills_left_over_cli_and_reconcile_fails_task() {
        // Regression (#5 item 5): serve was SIGKILLed while a CLI ran in its
        // own process group; the task stayed `running` forever.
        let dir = tempfile::tempdir().unwrap();
        let shared = shared_with_cli(dir.path(), "/bin/sleep");
        let mut child = spawn_sleeper();
        shared.db.upsert_task("task-left", "/etc", None).unwrap();
        shared.db.set_status("task-left", "running").unwrap();
        let run = shared.db.start_run("task-left", "claude", "").unwrap();
        shared.db.set_run_pid(run, child.id()).unwrap();
        // Previous serve is gone: clear the test-process owner stamp so this
        // looks like a leftover CLI after SIGKILL of serve.
        shared.db.set_run_owner(run, None).unwrap();
        shared.db.set_task_owner("task-left", None).unwrap();

        assert_eq!(reap_orphans_at_start(&shared).unwrap(), 1);
        assert!(wait_exit(&mut child, 5000), "orphaned CLI must be killed");
        assert!(reconcile(&shared).unwrap() >= 1);
        let t = shared.db.get_task("task-left").unwrap().unwrap();
        assert_eq!(t.status, "failed");
        assert!(t.result_text.contains("orphaned"), "{}", t.result_text);
        assert!(shared.db.open_runs().unwrap().is_empty());
    }

    #[test]
    fn reap_orphans_at_start_leaves_unrelated_or_inflight_pids_alone() {
        // A reused PID that is not the provider's CLI must never be killed,
        // and neither may a run this serve is still tracking.
        let dir = tempfile::tempdir().unwrap();
        let shared = shared_with_cli(dir.path(), "/bin/true");
        let mut child = spawn_sleeper();
        shared.db.upsert_task("task-reused", "/etc", None).unwrap();
        let run = shared.db.start_run("task-reused", "claude", "").unwrap();
        shared.db.set_run_pid(run, child.id()).unwrap();
        assert_eq!(reap_orphans_at_start(&shared).unwrap(), 0);
        assert!(child.try_wait().unwrap().is_none(), "unrelated PID killed");

        let dir2 = tempfile::tempdir().unwrap();
        let shared2 = shared_with_cli(dir2.path(), "/bin/sleep");
        shared2.db.upsert_task("task-live", "/etc", None).unwrap();
        let run2 = shared2.db.start_run("task-live", "claude", "").unwrap();
        shared2.db.set_run_pid(run2, child.id()).unwrap();
        shared2.inflight.lock().unwrap().insert(
            "task-live".into(),
            InFlight {
                provider: "claude".into(),
                pid: child.id(),
                run_id: run2,
                kill: std::sync::Arc::new(std::sync::Mutex::new(None)),
                shadow: false,
            },
        );
        assert_eq!(reap_orphans_at_start(&shared2).unwrap(), 0);
        assert_eq!(reconcile(&shared2).unwrap(), 0);
        assert!(child.try_wait().unwrap().is_none(), "in-flight CLI killed");
        let _ = child.kill();
        let _ = child.wait();
    }

    #[test]
    fn reap_orphans_at_start_spares_live_tui_owner() {
        // Serve restart must not kill a CLI owned by a live TUI / ask process.
        let dir = tempfile::tempdir().unwrap();
        let tui = shared_with_cli(dir.path(), "/bin/sleep");
        let mut child = spawn_sleeper();
        tui.db.upsert_task("cli-default", "/etc", None).unwrap();
        tui.db.set_status("cli-default", "running").unwrap();
        let run = tui.db.start_run("cli-default", "claude", "").unwrap();
        tui.db.set_run_pid(run, child.id()).unwrap();
        assert!(tui.db.list_runs(Some("cli-default")).unwrap()[0].owner_alive());

        let serve = Shared::from_config(
            dir.path().join("c.toml"),
            tui.config.read().unwrap().clone(),
        )
        .unwrap();
        assert!(
            serve.inflight.lock().unwrap().is_empty(),
            "new serve process has empty inflight"
        );
        assert_eq!(reap_orphans_at_start(&serve).unwrap(), 0);
        assert!(
            child.try_wait().unwrap().is_none(),
            "live TUI CLI must not be killed"
        );
        assert_eq!(reconcile(&serve).unwrap(), 0);
        let t = serve.db.get_task("cli-default").unwrap().unwrap();
        assert_eq!(t.status, "running");
        assert!(serve.db.open_run_for_task("cli-default").unwrap().is_some());
        let _ = child.kill();
        let _ = child.wait();
    }

    #[test]
    fn reap_orphans_at_start_still_reaps_dead_owner() {
        let dir = tempfile::tempdir().unwrap();
        let shared = shared_with_cli(dir.path(), "/bin/sleep");
        let mut child = spawn_sleeper();
        shared.db.upsert_task("cli-dead", "/etc", None).unwrap();
        shared.db.set_status("cli-dead", "running").unwrap();
        let run = shared.db.start_run("cli-dead", "claude", "").unwrap();
        shared.db.set_run_pid(run, child.id()).unwrap();
        shared
            .db
            .set_run_owner(
                run,
                Some(&crate::owner::OwnerId {
                    pid: 1_999_999_999,
                    started: "1".into(),
                }),
            )
            .unwrap();
        assert_eq!(reap_orphans_at_start(&shared).unwrap(), 1);
        assert!(wait_exit(&mut child, 5000), "dead-owner CLI must be killed");
    }

    fn live_owner_child(child: &std::process::Child) -> crate::owner::OwnerId {
        let pid = i64::from(child.id());
        crate::owner::OwnerId {
            pid,
            started: crate::owner::process_start_token(pid).expect("child start token"),
        }
    }

    #[test]
    fn reconcile_spares_live_tui_run_without_cli_pid() {
        // Window between start_run and set_run_pid: serve must not mark failed.
        // TUI is a different live process (a sleeper), not this test binary.
        let dir = tempfile::tempdir().unwrap();
        let mut tui_proc = spawn_sleeper();
        let tui_owner = live_owner_child(&tui_proc);
        let tui = shared_for_reconcile(dir.path());
        tui.db.upsert_task("cli-default", "/etc", None).unwrap();
        tui.db.set_status("cli-default", "running").unwrap();
        let run = tui.db.start_run("cli-default", "grok", "m").unwrap();
        tui.db.set_run_owner(run, Some(&tui_owner)).unwrap();
        tui.db
            .set_task_owner("cli-default", Some(&tui_owner))
            .unwrap();
        assert!(tui.db.list_runs(Some("cli-default")).unwrap()[0].owner_alive());

        let serve = Shared::from_config(
            dir.path().join("c.toml"),
            tui.config.read().unwrap().clone(),
        )
        .unwrap();
        assert!(!reclaim_dead_open_run(&serve, "cli-default").unwrap());
        assert_eq!(reconcile(&serve).unwrap(), 0);
        assert_eq!(
            serve.db.get_task("cli-default").unwrap().unwrap().status,
            "running"
        );
        assert!(serve.db.open_run_for_task("cli-default").unwrap().is_some());
        let _ = tui_proc.kill();
        let _ = tui_proc.wait();
    }

    #[test]
    fn reconcile_spares_running_tui_task_with_live_owner_no_run() {
        // Window between claim_task and start_run.
        let dir = tempfile::tempdir().unwrap();
        let mut tui_proc = spawn_sleeper();
        let tui_owner = live_owner_child(&tui_proc);
        let tui = shared_for_reconcile(dir.path());
        tui.db.upsert_task("cli-work", "/etc", None).unwrap();
        tui.db.set_status("cli-work", "queued").unwrap();
        assert!(tui.db.claim_task("cli-work").unwrap());
        tui.db.set_task_owner("cli-work", Some(&tui_owner)).unwrap();
        assert!(tui.db.task_owner_alive("cli-work").unwrap());
        assert!(tui.db.open_run_for_task("cli-work").unwrap().is_none());

        let serve = Shared::from_config(
            dir.path().join("c.toml"),
            tui.config.read().unwrap().clone(),
        )
        .unwrap();
        assert_eq!(reconcile(&serve).unwrap(), 0);
        assert_eq!(
            serve.db.get_task("cli-work").unwrap().unwrap().status,
            "running"
        );
        let _ = tui_proc.kill();
        let _ = tui_proc.wait();
    }

    #[test]
    fn reconcile_leaves_held_and_retry_for_live_owner() {
        let dir = tempfile::tempdir().unwrap();
        let mut tui_proc = spawn_sleeper();
        let tui_owner = live_owner_child(&tui_proc);
        let tui = shared_for_reconcile(dir.path());

        tui.db.upsert_task("held-1", "/etc", None).unwrap();
        tui.db.set_status("held-1", "held").unwrap();
        tui.db.set_hold("held-1", "after b failed", true).unwrap();
        tui.db.enqueue_message("held-1", "go", "cli").unwrap();
        let run = tui.db.start_run("held-1", "claude", "").unwrap();
        tui.db.set_run_owner(run, Some(&tui_owner)).unwrap();
        tui.db.set_task_owner("held-1", Some(&tui_owner)).unwrap();

        tui.db.upsert_task("retry-1", "/etc", None).unwrap();
        tui.db
            .set_retry("retry-1", 1, Some("2099-01-01T00:00:00.000Z"), "overloaded")
            .unwrap();
        tui.db.enqueue_message("retry-1", "go", "cli").unwrap();
        tui.db.set_task_owner("retry-1", Some(&tui_owner)).unwrap();

        let serve = Shared::from_config(
            dir.path().join("c.toml"),
            tui.config.read().unwrap().clone(),
        )
        .unwrap();
        assert_eq!(reap_orphans_at_start(&serve).unwrap(), 0);
        assert_eq!(reconcile(&serve).unwrap(), 0);
        let held = serve.db.get_task("held-1").unwrap().unwrap();
        assert_eq!(held.status, "held");
        assert_eq!(held.hold_reason, "after b failed");
        assert!(serve.db.open_run_for_task("held-1").unwrap().is_some());
        assert!(!requeue_if_pending(&serve, "held-1").unwrap());
        assert_eq!(serve.db.get_task("held-1").unwrap().unwrap().status, "held");
        let retry = serve.db.get_task("retry-1").unwrap().unwrap();
        assert_eq!(retry.status, "queued");
        assert_eq!(retry.retry_attempt, 1);
        assert_eq!(retry.retry_reason, "overloaded");
        let _ = tui_proc.kill();
        let _ = tui_proc.wait();
    }

    #[test]
    fn reconcile_still_fails_when_tui_owner_is_dead() {
        let dir = tempfile::tempdir().unwrap();
        let shared = shared_for_reconcile(dir.path());
        shared.db.upsert_task("cli-gone", "/etc", None).unwrap();
        shared.db.set_status("cli-gone", "running").unwrap();
        let run = shared.db.start_run("cli-gone", "claude", "m").unwrap();
        shared
            .db
            .set_run_owner(
                run,
                Some(&crate::owner::OwnerId {
                    pid: 1_999_999_999,
                    started: "1".into(),
                }),
            )
            .unwrap();
        shared
            .db
            .set_task_owner(
                "cli-gone",
                Some(&crate::owner::OwnerId {
                    pid: 1_999_999_999,
                    started: "1".into(),
                }),
            )
            .unwrap();
        assert!(reclaim_dead_open_run(&shared, "cli-gone").unwrap());
        let t = shared.db.get_task("cli-gone").unwrap().unwrap();
        assert_eq!(t.status, "failed");
    }

    #[test]
    fn unavailable_counts_live_foreign_owner_runs() {
        let dir = tempfile::tempdir().unwrap();
        let tui = shared_with_cli(dir.path(), existing_true_cli());
        tui.db.upsert_task("cli-default", "/etc", None).unwrap();
        tui.db.start_run("cli-default", "claude", "").unwrap();
        let serve = Shared::from_config(
            dir.path().join("c.toml"),
            tui.config.read().unwrap().clone(),
        )
        .unwrap();
        let cfg = serve.config.read().unwrap().clone();
        let p = cfg.provider("claude").unwrap();
        let clock = crate::clock::Clock::system();
        let inflight = HashMap::new();
        assert_eq!(
            unavailable(&cfg, p, &serve.db, &inflight, false, &clock).unwrap(),
            Some("full"),
            "a live TUI run must fill max_parallel=1 for serve"
        );
    }

    #[test]
    fn reconcile_fails_open_run_whose_pid_is_gone() {
        let dir = tempfile::tempdir().unwrap();
        let shared = shared_with_cli(dir.path(), "/bin/sleep");
        let mut child = spawn_sleeper();
        let pid = child.id();
        child.kill().unwrap();
        child.wait().unwrap();
        shared.db.upsert_task("task-gone", "/etc", None).unwrap();
        shared.db.set_status("task-gone", "running").unwrap();
        let run = shared.db.start_run("task-gone", "claude", "").unwrap();
        shared.db.set_run_pid(run, pid).unwrap();
        shared.db.set_run_owner(run, None).unwrap();
        shared.db.set_task_owner("task-gone", None).unwrap();
        assert!(reconcile(&shared).unwrap() >= 1);
        let t = shared.db.get_task("task-gone").unwrap().unwrap();
        assert_eq!(t.status, "failed");
        assert!(t.result_text.contains("process gone"), "{}", t.result_text);
    }

    #[test]
    fn reconcile_reaps_orphaned_running_task_without_run() {
        let dir = tempfile::tempdir().unwrap();
        let shared = shared_for_reconcile(dir.path());
        shared.db.upsert_task("task-orphan", "/etc", None).unwrap();
        shared.db.set_status("task-orphan", "running").unwrap();
        shared.db.set_task_owner("task-orphan", None).unwrap();
        let n = reconcile(&shared).unwrap();
        assert!(n >= 1, "expected to reap orphaned running task");
        let t = shared.db.get_task("task-orphan").unwrap().unwrap();
        assert_eq!(t.status, "failed");
        assert!(
            t.result_text.contains("orphaned") || t.result_text.contains("setup"),
            "{}",
            t.result_text
        );
    }

    #[test]
    fn reclaim_dead_open_run_reclaims_this_process_leftover_without_cli() {
        // A stale open run stamped with this process (crash after start_run,
        // or a test fixture) must not block the next TUI turn.
        let dir = tempfile::tempdir().unwrap();
        let shared = shared_for_reconcile(dir.path());
        shared.db.upsert_task("cli-default", "", None).unwrap();
        shared.db.set_status("cli-default", "running").unwrap();
        let run = shared.db.start_run("cli-default", "grok", "m").unwrap();
        shared.db.set_run_pid(run, 1_999_999_999).unwrap();
        assert!(reclaim_dead_open_run(&shared, "cli-default").unwrap());
        assert!(shared
            .db
            .open_run_for_task("cli-default")
            .unwrap()
            .is_none());
    }

    #[test]
    fn reclaim_dead_open_run_requeues_pending_without_clobbering_result_text() {
        let dir = tempfile::tempdir().unwrap();
        let shared = shared_for_reconcile(dir.path());
        shared.db.upsert_task("cli-default", "", None).unwrap();
        shared.db.set_status("cli-default", "failed").unwrap();
        shared
            .db
            .set_task_result(
                "cli-default",
                "That looks like a stray keystroke — nothing for me to act on. What did you want to do?",
                "",
                None,
            )
            .unwrap();
        let run = shared
            .db
            .start_run("cli-default", "grok", "Available")
            .unwrap();
        shared.db.set_run_pid(run, 1_999_999_999).unwrap();
        shared.db.set_run_owner(run, None).unwrap();
        shared.db.set_task_owner("cli-default", None).unwrap();
        shared
            .db
            .enqueue_message("cli-default", "what can you do for me?", "cli")
            .unwrap();
        assert!(shared
            .db
            .open_run_for_task("cli-default")
            .unwrap()
            .is_some());
        assert!(reclaim_dead_open_run(&shared, "cli-default").unwrap());
        assert!(shared
            .db
            .open_run_for_task("cli-default")
            .unwrap()
            .is_none());
        let t = shared.db.get_task("cli-default").unwrap().unwrap();
        assert!(
            t.result_text.contains("stray keystroke"),
            "do not clobber older success: {}",
            t.result_text
        );
        assert!(requeue_if_pending(&shared, "cli-default").unwrap());
        assert_eq!(
            shared.db.get_task("cli-default").unwrap().unwrap().status,
            "queued"
        );
        assert_eq!(shared.db.pending_messages("cli-default").unwrap().len(), 1);
    }

    #[test]
    fn provider_hint_wins_over_config_order() {
        // Regression: `atr add --provider grok` must go to grok even though
        // claude is first in the order and idle.
        let dir = tempfile::tempdir().unwrap();
        let cfg = two_provider_cfg(dir.path(), 10_000.0);
        let db = Db::open(&dir.path().join("state.db")).unwrap();
        db.upsert_task("hint-1", "/repo", None).unwrap();
        db.set_provider_hint("hint-1", "grok").unwrap();
        let clock = crate::clock::Clock::system();
        let inflight = HashMap::new();
        let t = db.get_task("hint-1").unwrap().unwrap();
        let picked = pick_provider(&cfg, &db, &inflight, &t, &clock)
            .unwrap()
            .expect("grok");
        assert_eq!(picked.name, "grok");
        // Already tried: falls back to the normal order.
        db.set_tried("hint-1", Some("grok")).unwrap();
        let t = db.get_task("hint-1").unwrap().unwrap();
        let picked = pick_provider(&cfg, &db, &inflight, &t, &clock)
            .unwrap()
            .expect("claude");
        assert_eq!(picked.name, "claude");
        // Unknown hint is ignored rather than stalling the task.
        db.upsert_task("hint-2", "/repo", None).unwrap();
        db.set_provider_hint("hint-2", "nope").unwrap();
        let t = db.get_task("hint-2").unwrap().unwrap();
        let picked = pick_provider(&cfg, &db, &inflight, &t, &clock)
            .unwrap()
            .expect("claude");
        assert_eq!(picked.name, "claude");
    }

    fn fake_cli(dir: &std::path::Path, name: &str) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let p = dir.join(name);
        std::fs::write(&p, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p
    }

    fn needs_cfg(dir: &std::path::Path, desktop_check: &str) -> crate::config::Config {
        let text = format!(
            r#"
listen = "127.0.0.1:0"
state_db = "{db}"
worktree_root = "{root}"
repo_path = "{root}"
system_prompt_file = "{root}/p.md"
desktop_check = "{check}"
[github]
poll_interval = "60s"
[[provider]]
name = "claude"
kind = "claude"
cli = "{a}"
[[provider]]
name = "grok"
kind = "grok"
cli = "{b}"
capabilities = "browser"
[[provider]]
name = "desk"
kind = "codex"
cli = "{c}"
capabilities = ["browser", "desktop"]
"#,
            db = dir.join("s.db").display(),
            root = dir.display(),
            check = desktop_check,
            a = fake_cli(dir, "fake-claude").display(),
            b = fake_cli(dir, "fake-grok").display(),
            c = fake_cli(dir, "fake-codex").display(),
        );
        std::fs::write(dir.join("p.md"), "p\n").unwrap();
        let raw: crate::config::RawConfig = toml::from_str(&text).unwrap();
        crate::config::Config::from_raw(raw).unwrap()
    }

    fn pick_name(cfg: &crate::config::Config, db: &Db, key: &str) -> Option<String> {
        let t = db.get_task(key).unwrap().unwrap();
        pick_provider(cfg, db, &HashMap::new(), &t, &crate::clock::Clock::system())
            .unwrap()
            .map(|p| p.name)
    }

    #[test]
    fn needs_route_to_capable_providers() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = needs_cfg(dir.path(), "true");
        assert_eq!(cfg.provider("grok").unwrap().capabilities, vec!["browser"]);
        let db = Db::open(&dir.path().join("state.db")).unwrap();
        db.upsert_task("plain", "/repo", None).unwrap();
        assert_eq!(pick_name(&cfg, &db, "plain").as_deref(), Some("claude"));

        db.upsert_task("web", "/repo", None).unwrap();
        db.set_task_needs("web", "browser").unwrap();
        assert_eq!(pick_name(&cfg, &db, "web").as_deref(), Some("grok"));

        // A hint for an incapable provider is ignored.
        db.set_provider_hint("web", "claude").unwrap();
        assert_eq!(pick_name(&cfg, &db, "web").as_deref(), Some("grok"));

        // GitHub `needs:browser` label works without stored needs.
        db.upsert_task("gh", "/repo", None).unwrap();
        db.set_task_labels("gh", "for:atr,needs:browser").unwrap();
        assert_eq!(pick_name(&cfg, &db, "gh").as_deref(), Some("grok"));

        db.upsert_task("gui", "/repo", None).unwrap();
        db.set_task_needs("gui", "desktop").unwrap();
        assert_eq!(pick_name(&cfg, &db, "gui").as_deref(), Some("desk"));

        // No capable provider left: wait instead of using an incapable one.
        db.set_tried("web", Some("grok,desk")).unwrap();
        assert_eq!(pick_name(&cfg, &db, "web"), None);
    }

    #[test]
    fn desktop_tasks_wait_for_unlocked_session() {
        let dir = tempfile::tempdir().unwrap();
        let flag = dir.path().join("session-unlocked");
        // Fake session probe: ready only while the flag file exists.
        let probe = dir.path().join("fake-session");
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::write(&probe, format!("#!/bin/sh\ntest -e '{}'\n", flag.display())).unwrap();
            std::fs::set_permissions(&probe, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let locked = needs_cfg(dir.path(), &probe.display().to_string());
        let db = Db::open(&dir.path().join("state.db")).unwrap();
        db.upsert_task("gui", "/repo", None).unwrap();
        db.set_task_needs("gui", "desktop").unwrap();
        db.upsert_task("web", "/repo", None).unwrap();
        db.set_task_needs("web", "browser").unwrap();
        assert_eq!(pick_name(&locked, &db, "gui"), None, "locked: wait");
        // Browser tasks don't need the graphical session.
        assert_eq!(pick_name(&locked, &db, "web").as_deref(), Some("grok"));

        std::fs::write(&flag, "").unwrap();
        // Different command string so the short probe cache is bypassed.
        let unlocked = needs_cfg(dir.path(), &format!("sh {}", probe.display()));
        assert_eq!(pick_name(&unlocked, &db, "gui").as_deref(), Some("desk"));
    }
}
