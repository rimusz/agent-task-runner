use crate::app::Shared;
use crate::config::{expand_path, Config};
use crate::db::Db;
use anyhow::Result;
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

#[derive(Debug, Default, Clone)]
pub struct GcReport {
    pub worktrees: Vec<String>,
    pub skipped_dirty: Vec<String>,
    pub skipped_shadow: Vec<String>,
    pub events: u64,
    pub runs: u64,
    pub sessions: u64,
    pub session_files: bool,
    /// `--older-than 1d` or `[gc] worktree_retention=14d`
    pub worktree_how: String,
    /// `--history-older-than 7d` or `[gc] event_retention=30d`
    pub event_how: String,
    /// `--history-older-than 7d` or `[gc] run_retention=30d`
    pub run_how: String,
    /// Set when `atr gc --tasks` ran.
    pub task_gc: bool,
    pub tasks: Vec<String>,
    pub skipped_live: Vec<String>,
}

impl GcReport {
    pub fn text(&self, dry_run: bool) -> String {
        let verb = if dry_run { "would delete" } else { "deleted" };
        let mut out = if dry_run {
            String::from("gc dry-run\n")
        } else {
            String::from("gc\n")
        };
        out.push_str(&format!(
            "{} {verb} worktrees: {}\n",
            self.worktree_how,
            self.worktrees.len()
        ));
        for p in &self.worktrees {
            out.push_str(&format!("  worktree\t{p}\n"));
        }
        for p in &self.skipped_dirty {
            out.push_str(&format!("  skip dirty\t{p}\n"));
        }
        for p in &self.skipped_shadow {
            out.push_str(&format!("  skip shadow-young\t{p}\n"));
        }
        out.push_str(&format!(
            "{} {verb} events: {}\n",
            self.event_how, self.events
        ));
        out.push_str(&format!("{} {verb} runs: {}\n", self.run_how, self.runs));
        if self.session_files {
            out.push_str(&format!(
                "session_files=true {verb} sessions: {}\n",
                self.sessions
            ));
        } else {
            out.push_str("session_files=false (no CLI session files)\n");
        }
        if self.task_gc {
            out.push_str(&format!("{verb} tasks: {}\n", self.tasks.len()));
            for k in &self.tasks {
                out.push_str(&format!("  task\t{k}\n"));
            }
            for k in &self.skipped_live {
                out.push_str(&format!("  skip live\t{k}\n"));
            }
        }
        out
    }
}

/// `atr gc --tasks [--repo PATH] [--status failed|done]`
#[derive(Debug, Clone, Default)]
pub struct TaskGc {
    pub repo: Option<String>,
    pub status: Option<String>,
}

impl TaskGc {
    fn matches(&self, t: &crate::db::Task) -> bool {
        if let Some(repo) = self.repo.as_deref() {
            let Some(have) = t.repo.as_deref().filter(|s| !s.is_empty()) else {
                return false;
            };
            if have != repo && Path::new(have) != Path::new(repo) {
                return false;
            }
        }
        match self.status.as_deref() {
            Some("failed") => matches!(t.status.as_str(), "failed" | "cancelled" | "held"),
            Some("done") => matches!(t.status.as_str(), "waiting" | "done"),
            Some(_) => false,
            None => crate::db::Db::removable_status(&t.status),
        }
    }
}

pub fn run(
    shared: &Shared,
    dry_run: bool,
    force: bool,
    older_than: Option<Duration>,
    history_older_than: Option<Duration>,
    tasks: Option<&TaskGc>,
) -> Result<GcReport> {
    let mut cfg = shared.config.read().unwrap().clone();
    let worktree_how = retention_how(
        "--older-than",
        older_than,
        "worktree_retention",
        cfg.gc.worktree_retention,
    );
    let event_how = retention_how(
        "--history-older-than",
        history_older_than,
        "event_retention",
        cfg.gc.event_retention,
    );
    let run_how = retention_how(
        "--history-older-than",
        history_older_than,
        "run_retention",
        cfg.gc.run_retention,
    );
    if let Some(d) = older_than {
        cfg.gc.worktree_retention = d;
    }
    if let Some(d) = history_older_than {
        cfg.gc.event_retention = d;
        cfg.gc.run_retention = d;
    }
    let mut report = run_with(&cfg, &shared.db, dry_run, force, &home_dir(), tasks)?;
    report.worktree_how = worktree_how;
    report.event_how = event_how;
    report.run_how = run_how;
    report.session_files = cfg.gc.session_files;
    Ok(report)
}

fn retention_how(
    flag: &str,
    override_d: Option<Duration>,
    key: &str,
    configured: Duration,
) -> String {
    match override_d {
        Some(d) => format!("{flag} {}", crate::duration::format_duration(d)),
        None => format!(
            "[gc] {key}={}",
            crate::duration::format_duration(configured)
        ),
    }
}

pub fn run_with(
    cfg: &Config,
    db: &Db,
    dry_run: bool,
    force: bool,
    home: &Path,
    tasks: Option<&TaskGc>,
) -> Result<GcReport> {
    let mut report = GcReport::default();
    prune_worktrees(cfg, db, dry_run, force, &mut report)?;
    report.events = db.prune_events(&cutoff(cfg.gc.event_retention), dry_run)?;
    report.runs = db.prune_runs(&cutoff(cfg.gc.run_retention), dry_run)?;
    if !dry_run {
        let _ = db.expire_lessons(&cutoff(cfg.lessons.ttl));
    }
    if cfg.gc.session_files {
        report.sessions = prune_session_files(cfg, db, home, dry_run)?;
    }
    if let Some(filter) = tasks {
        if let Some(s) = filter.status.as_deref() {
            if !matches!(s, "failed" | "done") {
                anyhow::bail!("--status must be failed or done");
            }
        }
        prune_tasks(db, &cfg.state_db, dry_run, filter, &mut report)?;
    }
    Ok(report)
}

fn prune_tasks(
    db: &Db,
    state_db: &Path,
    dry_run: bool,
    filter: &TaskGc,
    report: &mut GcReport,
) -> Result<()> {
    report.task_gc = true;
    for t in db.list_all_tasks()? {
        if !filter.matches(&t) {
            continue;
        }
        if db.delete_refuse_reason(&t.key)?.is_some() {
            report.skipped_live.push(t.key);
            continue;
        }
        report.tasks.push(t.key.clone());
        if !dry_run {
            db.delete_task(&t.key)?;
            crate::intake::remove_attachments(state_db, &t.key);
        }
    }
    Ok(())
}

fn cutoff(d: Duration) -> String {
    let ts = Utc::now() - ChronoDuration::from_std(d).unwrap_or(ChronoDuration::days(30));
    ts.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

fn home_dir() -> PathBuf {
    expand_path("~")
}

fn prune_worktrees(
    cfg: &Config,
    db: &Db,
    dry_run: bool,
    force: bool,
    report: &mut GcReport,
) -> Result<()> {
    let keep_after = Utc::now()
        - ChronoDuration::from_std(cfg.gc.worktree_retention).unwrap_or(ChronoDuration::days(14));
    let shadow_min = Utc::now()
        - ChronoDuration::from_std(cfg.gc.shadow_min_age).unwrap_or(ChronoDuration::days(7));
    for t in db.list_tasks()? {
        if !is_done_or_cancelled(&t.status) {
            continue;
        }
        let Some(wt) = t.worktree.as_deref().filter(|s| !s.is_empty()) else {
            continue;
        };
        let dest = PathBuf::from(wt);
        if !dest.exists() {
            continue;
        }
        if crate::worktree::is_pool_worktree(&dest) {
            continue;
        }
        if !under_root(&dest, &cfg.worktree_root) {
            tracing::warn!(path = %dest.display(), "gc skip worktree outside worktree_root");
            continue;
        }
        let updated = parse_ts(&t.updated).unwrap_or(Utc::now());
        if updated > keep_after {
            continue;
        }
        if t.shadow && updated > shadow_min {
            report.skipped_shadow.push(dest.display().to_string());
            continue;
        }
        if !force && is_dirty(&dest) {
            report.skipped_dirty.push(dest.display().to_string());
            continue;
        }
        report.worktrees.push(dest.display().to_string());
        if dry_run {
            continue;
        }
        let repo = t
            .repo
            .as_deref()
            .map(PathBuf::from)
            .unwrap_or_else(|| cfg.repo_path.clone());
        if let Err(e) = crate::worktree::remove_worktree(&repo, &dest, force) {
            tracing::warn!(path = %dest.display(), "gc worktree remove: {e:#}");
        } else {
            let _ = db.clear_worktree(&t.key);
        }
    }
    Ok(())
}

fn is_done_or_cancelled(status: &str) -> bool {
    crate::db::Db::removable_status(status)
}

fn under_root(path: &Path, root: &Path) -> bool {
    let Ok(p) = path.canonicalize() else {
        return path.starts_with(root);
    };
    let Ok(r) = root.canonicalize() else {
        return p.starts_with(root);
    };
    p.starts_with(r)
}

fn is_dirty(worktree: &Path) -> bool {
    let out = std::process::Command::new("git")
        .current_dir(worktree)
        .args(["status", "--porcelain"])
        .output();
    match out {
        Ok(o) if o.status.success() => !String::from_utf8_lossy(&o.stdout).trim().is_empty(),
        _ => true,
    }
}

fn parse_ts(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|t| t.with_timezone(&Utc))
}

/// Known per-CLI session directories under `home`.
pub fn known_session_dirs(kind: &str, home: &Path) -> Vec<PathBuf> {
    match kind {
        "claude" => vec![
            home.join(".claude").join("projects"),
            home.join(".claude").join("sessions"),
        ],
        "grok" => vec![
            home.join(".grok").join("sessions"),
            home.join(".grok").join("projects"),
        ],
        "cursor" => vec![
            home.join(".cursor").join("chats"),
            home.join(".cursor").join("projects"),
        ],
        "codex" => vec![home.join(".codex").join("sessions")],
        _ => Vec::new(),
    }
}

fn prune_session_files(cfg: &Config, db: &Db, home: &Path, dry_run: bool) -> Result<u64> {
    let run_cut = cutoff(cfg.gc.run_retention);
    let mut n = 0u64;
    for t in db.list_tasks()? {
        let runs = db.list_runs(Some(&t.key))?;
        for r in runs {
            if r.ended.is_none() {
                continue;
            }
            if r.ended.as_deref().unwrap_or("") > run_cut.as_str() {
                continue;
            }
            let Some(sid) = r.session_id.as_deref().filter(|s| !s.is_empty()) else {
                continue;
            };
            let kind = cfg
                .provider(&r.provider)
                .map(|p| p.kind.as_str())
                .unwrap_or(r.provider.as_str());
            for dir in known_session_dirs(kind, home) {
                n += delete_session_matches(&dir, sid, dry_run)?;
            }
        }
    }
    Ok(n)
}

fn delete_session_matches(dir: &Path, session_id: &str, dry_run: bool) -> Result<u64> {
    if !dir.is_dir() {
        return Ok(0);
    }
    let mut n = 0u64;
    let walk = walk_files(dir, 4);
    for path in walk {
        let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
        if !name.contains(session_id) {
            continue;
        }
        n += 1;
        if !dry_run {
            let _ = std::fs::remove_file(&path);
        }
    }
    Ok(n)
}

fn walk_files(dir: &Path, depth: u8) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if depth == 0 {
        return out;
    }
    let Ok(rd) = std::fs::read_dir(dir) else {
        return out;
    };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            out.extend(walk_files(&p, depth - 1));
        } else if p.is_file() {
            out.push(p);
        }
    }
    out
}

pub async fn daily_loop(shared: Arc<Shared>) {
    loop {
        maybe_daily(&shared);
        tokio::time::sleep(Duration::from_secs(60)).await;
    }
}

fn maybe_daily(shared: &Shared) {
    let cfg = shared.config.read().unwrap().clone();
    let spec = cfg.gc.daily.trim();
    if spec.is_empty() {
        return;
    }
    let Some((h, m)) = parse_hhmm(spec) else {
        return;
    };
    let (ch, cm) = cfg.display_timezone.now_local_hm();
    if ch != h || cm != m {
        return;
    }
    let today = cfg.display_timezone.today();
    if shared.db.meta_get("gc_daily").ok().flatten().as_deref() == Some(today.as_str()) {
        return;
    }
    let _ = shared.db.meta_set("gc_daily", &today);
    match run(shared, false, false, None, None, None) {
        Ok(r) => tracing::info!("{}", r.text(false).trim()),
        Err(e) => tracing::warn!("daily gc failed: {e:#}"),
    }
}

fn parse_hhmm(s: &str) -> Option<(u32, u32)> {
    let (a, b) = s.split_once(':')?;
    Some((a.parse().ok()?, b.parse().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::Shared;
    use crate::worktree;

    fn harness() -> (tempfile::TempDir, Arc<Shared>) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("p.md"), "p\n").unwrap();
        let dbp = dir.path().join("s.db");
        let cfg = format!(
            r#"
listen = "127.0.0.1:0"
state_db = "{db}"
worktree_root = "{root}/work"
repo_path = "{root}/repo"
system_prompt_file = "{root}/p.md"
display_timezone = "UTC"
[gc]
worktree_retention = "1d"
shadow_min_age = "7d"
event_retention = "2d"
run_retention = "2d"
session_files = false
[[provider]]
name = "claude"
kind = "claude"
cli = "/bin/true"
"#,
            db = dbp.display(),
            root = dir.path().display(),
        );
        let raw: crate::config::RawConfig = toml::from_str(&cfg).unwrap();
        let parsed = crate::config::Config::from_raw(raw).unwrap();
        let shared = Shared::from_config(dir.path().join("c.toml"), parsed).unwrap();
        (dir, shared)
    }

    fn old(days: i64) -> String {
        (Utc::now() - ChronoDuration::days(days))
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
    }

    #[test]
    fn prunes_old_clean_worktree_keeps_dirty_and_young_shadow() {
        let (dir, shared) = harness();
        let repo = dir.path().join("repo");
        worktree::init_sample_repo(&repo).unwrap();
        let root = dir.path().join("work");
        let clean = worktree::ensure_worktree(&repo, &root, "demo-old", "atr", "main").unwrap();
        let dirty = worktree::ensure_worktree(&repo, &root, "demo-dirty", "atr", "main").unwrap();
        std::fs::write(dirty.join("extra.md"), "dirty\n").unwrap();
        let shadow = worktree::ensure_worktree(&repo, &root, "demo-sh", "atr", "main").unwrap();

        shared
            .db
            .upsert_task("demo-old", &repo.display().to_string(), None)
            .unwrap();
        shared
            .db
            .set_worktree("demo-old", &clean.display().to_string())
            .unwrap();
        shared.db.set_status("demo-old", "waiting").unwrap();
        shared
            .db
            .set_timestamps("demo-old", &old(20), &old(20))
            .unwrap();

        shared
            .db
            .upsert_task("demo-dirty", &repo.display().to_string(), None)
            .unwrap();
        shared
            .db
            .set_worktree("demo-dirty", &dirty.display().to_string())
            .unwrap();
        shared.db.set_status("demo-dirty", "failed").unwrap();
        shared
            .db
            .set_timestamps("demo-dirty", &old(20), &old(20))
            .unwrap();

        shared
            .db
            .upsert_task("demo-sh", &repo.display().to_string(), None)
            .unwrap();
        shared.db.mark_shadow("demo-sh").unwrap();
        shared
            .db
            .set_worktree("demo-sh", &shadow.display().to_string())
            .unwrap();
        shared.db.set_status("demo-sh", "waiting").unwrap();
        shared
            .db
            .set_timestamps("demo-sh", &old(3), &old(3))
            .unwrap();

        let report = run(&shared, false, false, None, None, None).unwrap();
        assert!(
            report.worktrees.iter().any(|p| p.contains("demo-old")),
            "{report:?}"
        );
        assert!(!clean.exists(), "clean old worktree must be removed");
        assert!(dirty.exists(), "dirty worktree must stay without --force");
        assert!(
            report
                .skipped_dirty
                .iter()
                .any(|p| p.contains("demo-dirty")),
            "{report:?}"
        );
        assert!(shadow.exists(), "young shadow worktree must stay");
        assert!(
            report.skipped_shadow.iter().any(|p| p.contains("demo-sh")),
            "{report:?}"
        );
        std::mem::forget(dir);
    }

    #[test]
    fn prunes_events_and_runs_keeps_usage() {
        let (dir, shared) = harness();
        shared.db.upsert_task("demo-1", "/repo", None).unwrap();
        let rid = shared.db.start_run("demo-1", "claude", "m").unwrap();
        shared.db.finish_run(rid, "success", Some(0)).unwrap();
        shared.db.set_run_times(rid, &old(10), &old(10)).unwrap();
        shared
            .db
            .insert_event_at(rid, "assistant", "{}", &old(10))
            .unwrap();
        shared
            .db
            .insert_usage_at(
                rid,
                None,
                "claude",
                10,
                2,
                0,
                0,
                Some(1.0),
                Some("est"),
                "{}",
                &old(10),
            )
            .unwrap();
        let report = run(&shared, false, false, None, None, None).unwrap();
        assert!(report.events >= 1, "{report:?}");
        assert!(report.runs >= 1, "{report:?}");
        let days = shared
            .db
            .usage_by_day_tz(40, &crate::tz::DisplayTz::parse("UTC").unwrap())
            .unwrap();
        assert!(
            days.iter().any(|d| d.provider == "claude" && d.input == 10),
            "usage must survive run prune: {days:?}"
        );
        assert!(shared.db.list_runs(Some("demo-1")).unwrap().is_empty());
        std::mem::forget(dir);
    }

    #[test]
    fn session_files_only_when_opt_in_and_known_path() {
        let (dir, shared) = harness();
        let home = dir.path().join("home");
        let sess = "sess-abc-123";
        let known = home.join(".claude").join("sessions");
        std::fs::create_dir_all(&known).unwrap();
        let file = known.join(format!("{sess}.jsonl"));
        std::fs::write(&file, "{}\n").unwrap();
        let unknown = dir.path().join("not-home").join(format!("{sess}.jsonl"));
        std::fs::create_dir_all(unknown.parent().unwrap()).unwrap();
        std::fs::write(&unknown, "{}\n").unwrap();

        shared.db.upsert_task("demo-1", "/repo", None).unwrap();
        let run = shared.db.start_run("demo-1", "claude", "m").unwrap();
        shared.db.set_run_session(run, sess).unwrap();
        shared.db.finish_run(run, "success", Some(0)).unwrap();
        shared.db.set_run_times(run, &old(10), &old(10)).unwrap();

        let n =
            prune_session_files(&shared.config.read().unwrap(), &shared.db, &home, false).unwrap();
        assert!(n >= 1);
        assert!(!file.exists(), "known session file must be removed");
        assert!(unknown.exists(), "must not touch unknown paths");
        std::mem::forget(dir);
    }

    #[test]
    fn dry_run_does_not_delete() {
        let (dir, shared) = harness();
        shared.db.upsert_task("demo-1", "/repo", None).unwrap();
        let rid = shared.db.start_run("demo-1", "claude", "m").unwrap();
        shared.db.finish_run(rid, "success", Some(0)).unwrap();
        shared.db.set_run_times(rid, &old(10), &old(10)).unwrap();
        shared
            .db
            .insert_event_at(rid, "assistant", "{}", &old(10))
            .unwrap();
        let report = run(&shared, true, false, None, None, None).unwrap();
        assert!(report.events >= 1);
        assert_eq!(shared.db.events_for_run(rid).unwrap().len(), 1);
        assert_eq!(shared.db.list_runs(Some("demo-1")).unwrap().len(), 1);
        std::mem::forget(dir);
    }

    #[test]
    fn older_than_overrides_retention_for_one_run() {
        let (dir, shared) = harness();
        {
            let mut cfg = shared.config.write().unwrap();
            cfg.gc.worktree_retention = Duration::from_secs(30 * 86400);
            cfg.gc.event_retention = Duration::from_secs(30 * 86400);
            cfg.gc.run_retention = Duration::from_secs(30 * 86400);
            cfg.gc.shadow_min_age = Duration::from_secs(7 * 86400);
        }
        let repo = dir.path().join("repo");
        worktree::init_sample_repo(&repo).unwrap();
        let root = dir.path().join("work");
        let clean = worktree::ensure_worktree(&repo, &root, "demo-old", "atr", "main").unwrap();
        let shadow = worktree::ensure_worktree(&repo, &root, "demo-sh", "atr", "main").unwrap();

        shared
            .db
            .upsert_task("demo-old", &repo.display().to_string(), None)
            .unwrap();
        shared
            .db
            .set_worktree("demo-old", &clean.display().to_string())
            .unwrap();
        shared.db.set_status("demo-old", "waiting").unwrap();
        shared
            .db
            .set_timestamps("demo-old", &old(10), &old(10))
            .unwrap();
        let rid = shared.db.start_run("demo-old", "claude", "m").unwrap();
        shared.db.finish_run(rid, "success", Some(0)).unwrap();
        shared.db.set_run_times(rid, &old(10), &old(10)).unwrap();
        shared
            .db
            .insert_event_at(rid, "assistant", "{}", &old(10))
            .unwrap();

        shared
            .db
            .upsert_task("demo-sh", &repo.display().to_string(), None)
            .unwrap();
        shared.db.mark_shadow("demo-sh").unwrap();
        shared
            .db
            .set_worktree("demo-sh", &shadow.display().to_string())
            .unwrap();
        shared.db.set_status("demo-sh", "waiting").unwrap();
        shared
            .db
            .set_timestamps("demo-sh", &old(3), &old(3))
            .unwrap();

        let kept = run(&shared, false, false, None, None, None).unwrap();
        assert!(clean.exists(), "config retention is 30d");
        assert_eq!(kept.events, 0);
        assert_eq!(kept.runs, 0);
        assert_eq!(shared.db.list_runs(Some("demo-old")).unwrap().len(), 1);

        let dry = run(
            &shared,
            true,
            false,
            Some(Duration::from_secs(86400)),
            Some(Duration::from_secs(2 * 86400)),
            None,
        )
        .unwrap();
        let dry_text = dry.text(true);
        assert!(
            dry_text.contains("--older-than 1d would delete worktrees"),
            "{dry_text}"
        );
        assert!(
            dry_text.contains("--history-older-than 2d would delete events"),
            "{dry_text}"
        );
        assert!(
            dry_text.contains("--history-older-than 2d would delete runs"),
            "{dry_text}"
        );
        assert!(
            !dry_text.contains("event_retention") && !dry_text.contains("worktree_retention"),
            "dry-run should name the flags, not only config keys: {dry_text}"
        );
        assert!(clean.exists(), "dry-run must not delete");
        assert_eq!(shared.db.list_runs(Some("demo-old")).unwrap().len(), 1);

        let trees = run(
            &shared,
            false,
            false,
            Some(Duration::from_secs(86400)),
            None,
            None,
        )
        .unwrap();
        assert!(
            trees.worktrees.iter().any(|p| p.contains("demo-old")),
            "{trees:?}"
        );
        assert!(!clean.exists(), "older-than 1d must prune the 10d worktree");
        assert_eq!(trees.events, 0, "older-than must not prune events");
        assert_eq!(trees.runs, 0, "older-than must not prune runs");
        assert_eq!(shared.db.list_runs(Some("demo-old")).unwrap().len(), 1);
        assert!(shadow.exists(), "shadow_min_age is not lowered");
        assert!(
            trees.skipped_shadow.iter().any(|p| p.contains("demo-sh")),
            "{trees:?}"
        );

        let hist = run(
            &shared,
            false,
            false,
            None,
            Some(Duration::from_secs(86400)),
            None,
        )
        .unwrap();
        assert!(hist.events >= 1, "{hist:?}");
        assert!(hist.runs >= 1, "{hist:?}");
        assert!(shared.db.list_runs(Some("demo-old")).unwrap().is_empty());
        std::mem::forget(dir);
    }

    #[test]
    fn gc_tasks_filters_dry_run_and_skips_running() {
        let (dir, shared) = harness();
        shared.db.upsert_task("done-a", "/repo-a", None).unwrap();
        shared.db.set_status("done-a", "waiting").unwrap();
        shared.db.upsert_task("fail-a", "/repo-a", None).unwrap();
        shared.db.set_status("fail-a", "failed").unwrap();
        shared.db.upsert_task("fail-b", "/repo-b", None).unwrap();
        shared.db.set_status("fail-b", "failed").unwrap();
        shared.db.upsert_task("live-a", "/repo-a", None).unwrap();
        shared.db.set_status("live-a", "running").unwrap();
        shared.db.upsert_task("queued-a", "/repo-a", None).unwrap();

        let dry = run(
            &shared,
            true,
            false,
            None,
            None,
            Some(&TaskGc {
                repo: None,
                status: None,
            }),
        )
        .unwrap();
        let dry_text = dry.text(true);
        assert!(dry_text.contains("would delete tasks: 3"), "{dry_text}");
        assert!(dry.tasks.contains(&"done-a".into()), "{dry:?}");
        assert!(dry.tasks.contains(&"fail-a".into()), "{dry:?}");
        assert!(dry.tasks.contains(&"fail-b".into()), "{dry:?}");
        assert!(!dry.tasks.contains(&"live-a".into()), "{dry:?}");
        assert!(shared.db.get_task("done-a").unwrap().is_some());

        let failed = run(
            &shared,
            false,
            false,
            None,
            None,
            Some(&TaskGc {
                repo: Some("/repo-a".into()),
                status: Some("failed".into()),
            }),
        )
        .unwrap();
        assert_eq!(failed.tasks, vec!["fail-a".to_string()], "{failed:?}");
        assert!(shared.db.get_task("fail-a").unwrap().is_none());
        assert!(shared.db.get_task("fail-b").unwrap().is_some());
        assert!(shared.db.get_task("done-a").unwrap().is_some());
        assert!(shared.db.get_task("live-a").unwrap().is_some());
        assert!(shared.db.get_task("queued-a").unwrap().is_some());

        let done = run(
            &shared,
            false,
            false,
            None,
            None,
            Some(&TaskGc {
                repo: None,
                status: Some("done".into()),
            }),
        )
        .unwrap();
        assert!(done.tasks.contains(&"done-a".into()), "{done:?}");
        assert!(shared.db.get_task("done-a").unwrap().is_none());
        assert!(shared.db.get_task("fail-b").unwrap().is_some());
        std::mem::forget(dir);
    }
}
