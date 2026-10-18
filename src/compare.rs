//! Comparison mode: one task, two provider arms, one winner.
//!
//! A compare task is a parent `K` (role `compare`, status `waiting`, skipped
//! by the scheduler) plus two arm children `K-a` and `K-b` (role `arm`).
//! Each arm carries the same message, `output_kind = text` (an arm never
//! delivers anything itself) and a strict provider pin: it runs only on its
//! own provider and never fails over, so both arms can't collapse onto one
//! provider. See `docs/HOW-IT-WORKS.md` ("Compare").
//!
//! Arms run in their own worktrees (never pool slots) hardened against
//! pushing. When both arms are terminal the judge picks the arm that passed
//! verify with the smallest diff against the repo's base branch, promotes
//! its tree to the parent (branch renamed to `<prefix>/K`, output delivered
//! from there), and removes the loser.

use std::cmp::Ordering;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

use crate::app::Shared;
use crate::clock::Clock;
use crate::config::Config;
use crate::db::{Db, Task};

/// Parent role for a compare task.
pub const PARENT_ROLE: &str = "compare";
/// Role for each compare arm.
pub const ARM_ROLE: &str = "arm";
/// Arm suffixes, in order: the first listed arm wins a full tie.
pub const ARM_SUFFIXES: [&str; 2] = ["a", "b"];

/// What `create` needs to set up a compare task.
#[derive(Debug, Clone, Default)]
pub struct CompareSpec {
    pub key: String,
    pub repo: String,
    pub issue: Option<i64>,
    pub message: String,
    /// The parent's output (`pr`, `reply`, ...); arms always use `text`.
    pub output_kind: String,
    pub output_spec: String,
    pub title: String,
    pub labels: String,
    /// Exactly two distinct provider names, in arm order.
    pub providers: Vec<String>,
}

/// The arm key for `suffix` under `parent`.
pub fn arm_key(parent: &str, suffix: &str) -> String {
    format!("{parent}-{suffix}")
}

/// Parse `a,b` into two distinct provider names known to `cfg`.
pub fn parse_arms(cfg: &Config, spec: &str) -> Result<Vec<String>> {
    let arms: Vec<String> = spec
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if arms.len() != 2 {
        bail!("--compare takes exactly two providers, e.g. --compare grok,claude");
    }
    if arms[0] == arms[1] {
        bail!("--compare needs two different providers (same-provider variants are not supported yet)");
    }
    for a in &arms {
        if cfg.provider(a).is_none() {
            let known: Vec<&str> = cfg.providers.iter().map(|p| p.name.as_str()).collect();
            bail!(
                "unknown provider {a:?} in --compare (known: {})",
                known.join(", ")
            );
        }
    }
    Ok(arms)
}

/// Create the compare parent and its two strictly pinned arms, queued.
/// Returns the arm keys in order.
pub fn create(db: &Db, spec: &CompareSpec) -> Result<Vec<String>> {
    if spec.providers.len() != 2 || spec.providers[0] == spec.providers[1] {
        bail!("a compare task needs exactly two different providers");
    }
    if spec.repo.trim().is_empty() {
        bail!("a compare task needs a repo (arms are judged on their git diff)");
    }
    if db.get_task(&spec.key)?.is_some() {
        bail!("task {} already exists", spec.key);
    }
    let output = if spec.output_kind.trim().is_empty() {
        "reply"
    } else {
        spec.output_kind.trim()
    };
    db.upsert_task_spec(
        &spec.key,
        &spec.repo,
        spec.issue,
        Some("git"),
        Some(output),
        Some(&spec.output_spec),
        None,
    )?;
    db.set_task_role(&spec.key, PARENT_ROLE)?;
    if !spec.title.is_empty() {
        db.set_task_title(&spec.key, &spec.title)?;
    }
    if !spec.labels.is_empty() {
        db.set_task_labels(&spec.key, &spec.labels)?;
    }
    db.enqueue_message(&spec.key, &spec.message, "compare")?;
    db.set_status(&spec.key, "waiting")?;

    let mut keys = Vec::new();
    for (provider, suffix) in spec.providers.iter().zip(ARM_SUFFIXES) {
        let key = arm_key(&spec.key, suffix);
        db.upsert_task_spec(
            &key,
            &spec.repo,
            spec.issue,
            Some("git"),
            Some("text"),
            None,
            None,
        )?;
        db.set_parent_key(&key, &spec.key)?;
        db.set_task_role(&key, ARM_ROLE)?;
        if !spec.title.is_empty() {
            db.set_task_title(&key, &format!("{} [{provider}]", spec.title))?;
        }
        if !spec.labels.is_empty() {
            db.set_task_labels(&key, &spec.labels)?;
        }
        db.set_provider_session(&key, Some(provider), None)?;
        db.set_pin_strict(&key, true)?;
        db.enqueue_message(&key, &spec.message, "compare")?;
        db.set_status(&key, "queued")?;
        keys.push(key);
    }
    Ok(keys)
}

/// The arms of compare parent `key`, in arm order.
pub fn arms_of(db: &Db, key: &str) -> Result<Vec<crate::db::Task>> {
    let mut kids: Vec<_> = db
        .children_of(key)?
        .into_iter()
        .filter(|t| t.role == ARM_ROLE)
        .collect();
    kids.sort_by(|a, b| a.key.cmp(&b.key));
    Ok(kids)
}

/// Arm statuses after which the arm will not run again by itself.
pub fn is_terminal(status: &str) -> bool {
    matches!(
        status,
        "waiting" | "done" | "failed" | "needs_attention" | "cancelled"
    )
}

/// One arm as the judge saw it.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ArmReport {
    pub key: String,
    pub provider: String,
    pub status: String,
    /// Outcome of the arm's last run (`success`, `verify_failed`, `limit`,
    /// `arm_unavailable`, `task_timeout`, ...); empty when it never ran.
    pub outcome: String,
    /// `passed`, `failed`, `skipped` (success with no checks) or `none`
    /// (never reached verify).
    pub verify: String,
    pub files: u64,
    pub added: u64,
    pub removed: u64,
    pub verify_retries: u64,
    pub tokens: i64,
    pub cost_usd: f64,
    pub duration_ms: i64,
    /// Tail of the last failed verify check, for the no-winner notice.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub verify_tail: String,
    /// `won`, `lost`, or empty (no winner).
    #[serde(default)]
    pub result: String,
}

impl ArmReport {
    pub fn diff(&self) -> u64 {
        self.added + self.removed
    }

    fn candidate(&self) -> bool {
        self.status == "waiting" && self.outcome == "success" && self.verify == "passed"
    }
}

/// The judge's decision, stored as JSON in the parent's `compare_decision`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Decision {
    pub winner: Option<String>,
    pub winner_provider: Option<String>,
    pub reason: String,
    pub walkover: bool,
    pub decided_at: String,
    pub arms: Vec<ArmReport>,
    #[serde(default)]
    pub pr_url: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub error: String,
}

/// The repo's own base branch for `task` (`[[repo]] base`, else
/// `repo_default_branch`), the same one its worktree was cut from.
pub fn base_branch(cfg: &Config, task: &Task) -> String {
    cfg.repo_for_task(task)
        .map(|e| e.base)
        .filter(|b| !b.is_empty())
        .unwrap_or_else(|| cfg.repo_default_branch.clone())
}

/// Files and added/removed lines in `wt` against its merge-base with
/// `base`: committed and uncommitted tracked changes plus untracked files,
/// excluding `.atr/`.
pub fn diff_stats(wt: &Path, base: &str) -> (u64, u64, u64) {
    let git = |args: &[&str]| -> Option<String> {
        let o = Command::new("git").current_dir(wt).args(args).output().ok()?;
        o.status
            .success()
            .then(|| String::from_utf8_lossy(&o.stdout).into_owned())
    };
    let from = git(&["merge-base", "HEAD", base])
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| base.to_string());
    let (mut files, mut added, mut removed) = (0u64, 0u64, 0u64);
    if let Some(out) = git(&["diff", "--numstat", &from, "--", ".", ":(exclude).atr"]) {
        for line in out.lines() {
            let mut parts = line.splitn(3, '\t');
            let (Some(a), Some(r), Some(_)) = (parts.next(), parts.next(), parts.next()) else {
                continue;
            };
            files += 1;
            // Binary files show `-`; count them as one changed line.
            added += a.parse::<u64>().unwrap_or(1);
            removed += r.parse::<u64>().unwrap_or(0);
        }
    }
    if let Some(out) = git(&["ls-files", "--others", "--exclude-standard", "-z"]) {
        for path in out.split('\0').filter(|p| !p.is_empty()) {
            if path == ".atr" || path.starts_with(".atr/") {
                continue;
            }
            files += 1;
            let lines = std::fs::read(wt.join(path))
                .map(|b| {
                    let n = b.iter().filter(|c| **c == b'\n').count() as u64;
                    if !b.is_empty() && !b.ends_with(b"\n") {
                        n + 1
                    } else {
                        n.max(1)
                    }
                })
                .unwrap_or(1);
            added += lines;
        }
    }
    (files, added, removed)
}

fn duration_ms(started: &str, ended: Option<&str>) -> i64 {
    let (Ok(s), Some(Ok(e))) = (
        chrono::DateTime::parse_from_rfc3339(started),
        ended.map(chrono::DateTime::parse_from_rfc3339),
    ) else {
        return 0;
    };
    (e - s).num_milliseconds().max(0)
}

/// Gather what the judge needs about one arm from the DB and its worktree.
pub fn arm_report(cfg: &Config, db: &Db, arm: &Task) -> ArmReport {
    let runs = db.list_runs(Some(&arm.key)).unwrap_or_default();
    let last = runs.last();
    let outcome = last.and_then(|r| r.outcome.clone()).unwrap_or_default();
    let mut rep = ArmReport {
        key: arm.key.clone(),
        provider: arm.provider.clone().unwrap_or_default(),
        status: arm.status.clone(),
        outcome: outcome.clone(),
        verify: "none".into(),
        ..Default::default()
    };
    for r in &runs {
        if r.outcome.as_deref() == Some("verify_failed") {
            rep.verify_retries += 1;
        }
        rep.duration_ms += duration_ms(&r.started, r.ended.as_deref());
        for u in db.list_usage_for_run(r.id).unwrap_or_default() {
            rep.tokens += u.input + u.output;
            rep.cost_usd += u.cost_usd.unwrap_or(0.0);
        }
    }
    if let Some(r) = last {
        let checks = db.verify_checks_for_run(r.id).unwrap_or_default();
        rep.verify = match outcome.as_str() {
            "success" if checks.is_empty() => "skipped".into(),
            "success" if checks.iter().all(|c| c.passed) => "passed".into(),
            "success" | "verify_failed" | "tests_modified" => "failed".into(),
            _ => "none".into(),
        };
        if let Some(c) = checks.iter().rev().find(|c| !c.passed) {
            let out = c.output.trim();
            let tail: String = out
                .chars()
                .rev()
                .take(400)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect();
            rep.verify_tail = format!("{}: {tail}", c.command);
        }
    }
    if let Some(wt) = arm.worktree.as_deref().filter(|w| Path::new(w).exists()) {
        let (f, a, r) = diff_stats(Path::new(wt), &base_branch(cfg, arm));
        rep.files = f;
        rep.added = a;
        rep.removed = r;
    }
    rep
}

/// Compare two candidates by the tie-break order. `Less` means `a` wins.
/// Returns the ordering and the criterion that decided it.
fn rank(a: &ArmReport, b: &ArmReport) -> (Ordering, &'static str) {
    let cost = if a.cost_usd > 0.0 && b.cost_usd > 0.0 {
        a.cost_usd.partial_cmp(&b.cost_usd).unwrap_or(Ordering::Equal)
    } else {
        Ordering::Equal
    };
    let steps: [(Ordering, &'static str); 6] = [
        (a.diff().cmp(&b.diff()), "smaller diff"),
        (a.files.cmp(&b.files), "fewer files"),
        (a.verify_retries.cmp(&b.verify_retries), "fewer verify retries"),
        (a.tokens.cmp(&b.tokens), "fewer tokens"),
        (cost, "lower cost"),
        (a.duration_ms.cmp(&b.duration_ms), "shorter duration"),
    ];
    for (o, why) in steps {
        if o != Ordering::Equal {
            return (o, why);
        }
    }
    (Ordering::Equal, "first arm listed (full tie)")
}

/// Pick the winner from `arms` (in arm order). Pure; no side effects.
pub fn judge(arms: &[ArmReport]) -> Decision {
    let mut cands: Vec<&ArmReport> = arms.iter().filter(|a| a.candidate()).collect();
    // An empty diff can't beat a real change (it would always be smallest).
    if cands.iter().any(|a| a.diff() > 0) {
        cands.retain(|a| a.diff() > 0);
    }
    let mut d = Decision {
        decided_at: crate::db::now_rfc3339(),
        arms: arms.to_vec(),
        ..Default::default()
    };
    let winner = match cands.as_slice() {
        [] => {
            d.reason = "no arm passed verify".into();
            return d;
        }
        [only] => {
            let other_never_verified = arms
                .iter()
                .filter(|a| a.key != only.key)
                .all(|a| a.verify == "none");
            d.walkover = other_never_verified;
            d.reason = if other_never_verified {
                "walkover: the other arm ended before verify".into()
            } else if arms.iter().filter(|a| a.candidate()).count() > 1 {
                "the other arm passed verify with no changes".into()
            } else {
                "only arm that passed verify".into()
            };
            (*only).clone()
        }
        [a, b, ..] => {
            let (o, why) = rank(a, b);
            let (w, l) = if o == Ordering::Greater { (b, a) } else { (a, b) };
            d.reason = match why {
                "smaller diff" => format!("smaller diff ({} vs {} lines)", w.diff(), l.diff()),
                "fewer files" => format!("fewer files ({} vs {})", w.files, l.files),
                other => other.to_string(),
            };
            (*w).clone()
        }
    };
    d.winner = Some(winner.key.clone());
    d.winner_provider = Some(winner.provider.clone());
    for a in &mut d.arms {
        a.result = if a.key == winner.key { "won" } else { "lost" }.into();
    }
    d
}

/// Called whenever an arm reaches a terminal state. Once both arms are
/// terminal, judges exactly once (an atomic claim on the parent), then
/// promotes the winner and cleans up the loser, or parks the parent in
/// `needs_attention` when no arm passed.
pub fn after_arm(shared: &Shared, arm_key: &str) {
    if let Err(e) = after_arm_inner(shared, arm_key) {
        tracing::warn!(task = %arm_key, "compare judge: {e:#}");
    }
}

fn after_arm_inner(shared: &Shared, arm_key: &str) -> Result<()> {
    let Some(arm) = shared.db.get_task(arm_key)? else {
        return Ok(());
    };
    let Some(parent_key) = arm.parent_key.clone().filter(|_| arm.role == ARM_ROLE) else {
        return Ok(());
    };
    let Some(parent) = shared.db.get_task(&parent_key)? else {
        return Ok(());
    };
    if parent.role != PARENT_ROLE || !parent.compare_decision.is_empty() {
        return Ok(());
    }
    let arms = arms_of(&shared.db, &parent_key)?;
    if arms.is_empty() || !arms.iter().all(|a| is_terminal(&a.status)) {
        return Ok(());
    }
    if !shared.db.claim_compare_decision(&parent_key)? {
        return Ok(());
    }
    let cfg = shared.config.read().unwrap().clone();
    let reports: Vec<ArmReport> = arms
        .iter()
        .map(|a| arm_report(&cfg, &shared.db, a))
        .collect();
    let mut d = judge(&reports);
    match d.winner.clone() {
        Some(w) => {
            let winner = arms.iter().find(|a| a.key == w).cloned().unwrap();
            if let Err(e) = promote(shared, &cfg, &parent, &winner, &mut d) {
                d.error = format!("{e:#}");
                save(&shared.db, &parent_key, &d)?;
                shared.db.set_status(&parent_key, "needs_attention")?;
                notify(
                    &cfg,
                    &format!("needs_attention {parent_key} (compare: promoting {w} failed: {e:#})"),
                );
                return Ok(());
            }
            for loser in arms.iter().filter(|a| a.key != w) {
                cleanup_loser(shared, &cfg, loser);
            }
            notify(
                &cfg,
                &format!(
                    "compare {parent_key}: {w} ({}) won, {}{}",
                    d.winner_provider.as_deref().unwrap_or(""),
                    d.reason,
                    if d.pr_url.is_empty() {
                        String::new()
                    } else {
                        format!(" {}", d.pr_url)
                    }
                ),
            );
        }
        None => {
            save(&shared.db, &parent_key, &d)?;
            shared.db.set_status(&parent_key, "needs_attention")?;
            let mut msg = format!("needs_attention {parent_key} (compare: no arm passed verify)");
            for a in &d.arms {
                msg.push_str(&format!(
                    "\n{} {}: {} verify={}",
                    a.key, a.provider, a.outcome, a.verify
                ));
                if !a.verify_tail.is_empty() {
                    msg.push_str(&format!("\n  {}", a.verify_tail));
                }
            }
            notify(&cfg, &msg);
        }
    }
    Ok(())
}

fn notify(cfg: &Config, msg: &str) {
    crate::notify::notify_unless_shadow(&cfg.telegram, msg, false);
}

fn save(db: &Db, key: &str, d: &Decision) -> Result<()> {
    db.set_compare_decision(key, &serde_json::to_string(d)?)
}

/// The comparison footer appended to the winner's PR/result text.
pub fn footer(d: &Decision) -> String {
    let mut s = String::from("\n\n---\n**atr compare**");
    if let Some(w) = &d.winner {
        s.push_str(&format!(
            ": `{w}` ({}) won: {}.\n\n| arm | provider | outcome | verify | diff |\n|---|---|---|---|---|\n",
            d.winner_provider.as_deref().unwrap_or(""),
            d.reason
        ));
    }
    for a in &d.arms {
        s.push_str(&format!(
            "| {}{} | {} | {} | {} | +{} -{} ({} files) |\n",
            a.key,
            if a.result == "won" { " 🏆" } else { "" },
            a.provider,
            if a.outcome.is_empty() { "-" } else { &a.outcome },
            a.verify,
            a.added,
            a.removed,
            a.files
        ));
    }
    s
}

fn git_in(dir: &Path, args: &[&str]) -> Result<String> {
    let o = Command::new("git").current_dir(dir).args(args).output()?;
    if !o.status.success() {
        bail!(
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&o.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&o.stdout).into_owned())
}

/// The winner's tree becomes the parent's: un-hardened, its branch renamed
/// from `<prefix>/K-x` to `<prefix>/K`, the parent takes over its worktree,
/// provider and session, and the parent's output is delivered from there.
fn promote(
    shared: &Shared,
    cfg: &Config,
    parent: &Task,
    winner: &Task,
    d: &mut Decision,
) -> Result<()> {
    let wt = winner
        .worktree
        .clone()
        .filter(|w| Path::new(w).exists())
        .ok_or_else(|| anyhow::anyhow!("winner {} has no worktree", winner.key))?;
    let wt_path = PathBuf::from(&wt);
    crate::worktree::clear_worktree_no_push(&wt_path)?;
    if let Some(cur) = crate::worktree::current_branch(&wt_path) {
        let suffix = format!("/{}", winner.key);
        if let Some(prefix) = cur.strip_suffix(&suffix) {
            let new = format!("{prefix}/{}", parent.key);
            if let Err(e) = git_in(&wt_path, &["branch", "-m", &cur, &new]) {
                tracing::warn!(task = %parent.key, "compare: keeping arm branch {cur}: {e:#}");
            }
        }
    }
    let db = &shared.db;
    let runs = db.list_runs(Some(&winner.key))?;
    let session = runs
        .iter()
        .rev()
        .find_map(|r| r.session_id.clone())
        .or_else(|| winner.session_id.clone());
    db.set_worktree(&parent.key, &wt)?;
    db.set_provider_session(&parent.key, winner.provider.as_deref(), session.as_deref())?;
    db.set_task_role(&parent.key, "worker")?;
    // The parent's message was answered by the arms; don't replay it.
    if let Some(last) = runs.last() {
        let ids: Vec<i64> = db
            .pending_messages(&parent.key)?
            .iter()
            .map(|m| m.id)
            .collect();
        db.mark_consumed(&ids, last.id)?;
        let _ = db.insert_event(
            last.id,
            "compare",
            &serde_json::json!({"type": "compare", "result": "won", "parent": parent.key})
                .to_string(),
        );
    }
    db.clear_worktree(&winner.key)?;
    db.set_status(&winner.key, "done")?;
    save(db, &parent.key, d)?;

    let text = format!(
        "{}{}",
        crate::output::last_text(shared, &winner.key),
        footer(d)
    );
    db.set_status(&parent.key, "waiting")?;
    if let Err(e) = crate::output::finalize_with_text(shared, &parent.key, Some(&text)) {
        tracing::warn!(task = %parent.key, "compare output: {e:#}");
    }
    d.pr_url = db
        .get_task(&parent.key)?
        .map(|t| t.result_pr_url)
        .unwrap_or_default();
    save(db, &parent.key, d)?;
    let _ = cfg;
    Ok(())
}

/// Remove the losing arm's worktree and its local (never pushed) branch,
/// and mark it `done` (lost). Its `shadow_results` row is kept.
fn cleanup_loser(shared: &Shared, cfg: &Config, loser: &Task) {
    let db = &shared.db;
    if let Some(wt) = loser.worktree.as_deref().filter(|w| Path::new(w).exists()) {
        let wt = PathBuf::from(wt);
        let branch = crate::worktree::current_branch(&wt);
        let repo = loser
            .repo
            .as_deref()
            .map(PathBuf::from)
            .unwrap_or_else(|| cfg.repo_path.clone());
        if crate::worktree::is_pool_worktree(&wt) {
            let _ = crate::worktree::clear_worktree_no_push(&wt);
            let _ = git_in(&wt, &["checkout", "-q", "--detach"]);
        } else if wt.starts_with(&cfg.worktree_root) {
            if let Err(e) = crate::worktree::remove_worktree(&repo, &wt, true) {
                tracing::warn!(task = %loser.key, "compare: remove loser worktree: {e:#}");
            }
        }
        if let Some(b) = branch.filter(|b| b.ends_with(&format!("/{}", loser.key))) {
            let _ = git_in(&repo, &["branch", "-D", &b]);
        }
        let _ = db.clear_worktree(&loser.key);
    }
    if let Some(last) = db.list_runs(Some(&loser.key)).ok().and_then(|r| r.last().cloned()) {
        let _ = db.insert_event(
            last.id,
            "compare",
            &serde_json::json!({"type": "compare", "result": "lost"}).to_string(),
        );
    }
    let _ = db.set_status(&loser.key, "done");
}

/// Validate and create a compare task from the CLI: two usable providers,
/// neither over quota, and verify commands for the repo (compare can't
/// judge without them).
pub fn add(shared: &Shared, cfg: &Config, spec: &CompareSpec) -> Result<Vec<String>> {
    let repo = Path::new(&spec.repo);
    if !cfg.verify.enabled || crate::verify::commands_for(cfg, repo, repo).is_empty() {
        bail!(
            "compare needs verify commands for {} (enable [verify] and set commands or [[verify.repo]]); without them it can't judge",
            spec.repo
        );
    }
    let clock = Clock::system();
    for name in &spec.providers {
        let probe = Task {
            key: spec.key.clone(),
            provider: Some(name.clone()),
            labels: spec.labels.clone(),
            ..Default::default()
        };
        if let Some(why) = crate::scheduler::strict_pin_unusable(cfg, &shared.db, &probe) {
            bail!("can't compare: {why}");
        }
        let p = cfg.provider(name).unwrap();
        if crate::quota::over_quota(&shared.db, p, &clock)? {
            bail!("can't compare: provider {name} is over quota (a compare never degrades to one run)");
        }
    }
    let keys = create(&shared.db, spec)?;
    shared.kick.notify_waiters();
    Ok(keys)
}

/// `atr compare show K`: arms, outcomes, diff sizes, decision and PR URL.
pub fn show_text(cfg: &Config, db: &Db, key: &str) -> Result<String> {
    let Some(parent) = db.get_task(key)? else {
        bail!("unknown task {key}");
    };
    let arms = arms_of(db, key)?;
    if arms.is_empty() {
        bail!("{key} is not a compare task");
    }
    let decision: Option<Decision> = serde_json::from_str(&parent.compare_decision).ok();
    let mut s = format!("{key}  status={}\n", parent.status);
    let reports: Vec<ArmReport> = match &decision {
        Some(d) if !d.arms.is_empty() => d.arms.clone(),
        _ => arms.iter().map(|a| arm_report(cfg, db, a)).collect(),
    };
    for a in &reports {
        s.push_str(&format!(
            "  {}{}  {}  status={}  outcome={}  verify={}  +{} -{} ({} files)  retries={}  tokens={}\n",
            a.key,
            if a.result == "won" { " 🏆" } else { "" },
            a.provider,
            a.status,
            if a.outcome.is_empty() { "-" } else { &a.outcome },
            a.verify,
            a.added,
            a.removed,
            a.files,
            a.verify_retries,
            a.tokens
        ));
    }
    match &decision {
        Some(d) => {
            s.push_str(&format!(
                "decision: {}{}\n",
                d.winner.as_deref().unwrap_or("no winner"),
                if d.reason.is_empty() {
                    String::new()
                } else {
                    format!(" ({})", d.reason)
                }
            ));
            if !d.pr_url.is_empty() {
                s.push_str(&format!("pr: {}\n", d.pr_url));
            }
            if !d.error.is_empty() {
                s.push_str(&format!("error: {}\n", d.error));
            }
        }
        None => s.push_str("decision: pending (arms still running)\n"),
    }
    Ok(s)
}

/// JSON for `atr compare show K --json`.
pub fn show_json(cfg: &Config, db: &Db, key: &str) -> Result<serde_json::Value> {
    let Some(parent) = db.get_task(key)? else {
        bail!("unknown task {key}");
    };
    let arms = arms_of(db, key)?;
    if arms.is_empty() {
        bail!("{key} is not a compare task");
    }
    let decision: Option<Decision> = serde_json::from_str(&parent.compare_decision).ok();
    let live: Vec<ArmReport> = arms.iter().map(|a| arm_report(cfg, db, a)).collect();
    Ok(serde_json::json!({
        "key": key,
        "status": parent.status,
        "arms": decision.as_ref().filter(|d| !d.arms.is_empty()).map(|d| d.arms.clone()).unwrap_or(live),
        "decision": decision,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn arm(key: &str, verify: &str, added: u64) -> ArmReport {
        ArmReport {
            key: key.into(),
            provider: key.into(),
            status: if verify == "passed" { "waiting" } else { "needs_attention" }.into(),
            outcome: if verify == "passed" { "success" } else { "verify_failed" }.into(),
            verify: verify.into(),
            files: 1,
            added,
            ..Default::default()
        }
    }

    #[test]
    fn smaller_diff_wins() {
        let d = judge(&[arm("k-a", "passed", 40), arm("k-b", "passed", 12)]);
        assert_eq!(d.winner.as_deref(), Some("k-b"));
        assert!(d.reason.contains("smaller diff (12 vs 40"), "{}", d.reason);
        assert_eq!(d.arms[0].result, "lost");
        assert_eq!(d.arms[1].result, "won");
        assert!(!d.walkover);
    }

    #[test]
    fn tie_breaks_in_order() {
        let mut a = arm("k-a", "passed", 10);
        let mut b = arm("k-b", "passed", 10);
        b.files = 2;
        assert_eq!(judge(&[a.clone(), b.clone()]).winner.as_deref(), Some("k-a"));
        b.files = 1;
        a.verify_retries = 1;
        let d = judge(&[a.clone(), b.clone()]);
        assert_eq!(d.winner.as_deref(), Some("k-b"));
        assert_eq!(d.reason, "fewer verify retries");
        a.verify_retries = 0;
        a.tokens = 500;
        b.tokens = 100;
        assert_eq!(judge(&[a.clone(), b.clone()]).reason, "fewer tokens");
        b.tokens = 500;
        a.cost_usd = 0.02;
        b.cost_usd = 0.01;
        assert_eq!(judge(&[a.clone(), b.clone()]).winner.as_deref(), Some("k-b"));
        b.cost_usd = 0.02;
        a.duration_ms = 10;
        b.duration_ms = 20;
        let d = judge(&[a.clone(), b.clone()]);
        assert_eq!(d.winner.as_deref(), Some("k-a"));
        assert_eq!(d.reason, "shorter duration");
        b.duration_ms = 10;
        let d = judge(&[a, b]);
        assert_eq!(d.winner.as_deref(), Some("k-a"));
        assert!(d.reason.contains("first arm"));
    }

    #[test]
    fn verify_is_required_and_skipped_is_not_a_pass() {
        let mut b = arm("k-b", "passed", 5);
        b.verify = "skipped".into();
        let d = judge(&[arm("k-a", "failed", 1), b]);
        assert_eq!(d.winner, None);
        assert_eq!(d.reason, "no arm passed verify");
        assert!(d.arms.iter().all(|a| a.result.is_empty()));
    }

    #[test]
    fn walkover_when_other_arm_never_reached_verify() {
        let mut a = arm("k-a", "none", 0);
        a.status = "failed".into();
        a.outcome = "limit".into();
        let d = judge(&[a, arm("k-b", "passed", 30)]);
        assert_eq!(d.winner.as_deref(), Some("k-b"));
        assert!(d.walkover);
        let d = judge(&[arm("k-a", "failed", 3), arm("k-b", "passed", 30)]);
        assert!(!d.walkover);
        assert_eq!(d.reason, "only arm that passed verify");
    }

    #[test]
    fn empty_diff_does_not_beat_a_real_change() {
        let d = judge(&[arm("k-a", "passed", 0), arm("k-b", "passed", 30)]);
        assert_eq!(d.winner.as_deref(), Some("k-b"));
        let d = judge(&[arm("k-a", "passed", 0), arm("k-b", "passed", 0)]);
        assert_eq!(d.winner.as_deref(), Some("k-a"));
    }

    #[test]
    fn footer_lists_both_arms() {
        let d = judge(&[arm("k-a", "passed", 40), arm("k-b", "passed", 12)]);
        let f = footer(&d);
        assert!(f.contains("`k-b` (k-b) won"), "{f}");
        assert!(f.contains("| k-b 🏆 |"), "{f}");
        assert!(f.contains("+40 -0"), "{f}");
    }
}
