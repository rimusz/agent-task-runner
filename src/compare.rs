//! Comparison mode: one task, two provider arms, one winner.
//!
//! A compare task is a parent `K` (role `compare`, status `waiting`, skipped
//! by the scheduler) plus two arm children `K-a` and `K-b` (role `arm`).
//! Each arm carries the same message, `output_kind = text` (an arm never
//! delivers anything itself) and a strict provider pin: it runs only on its
//! own provider and never fails over, so both arms can't collapse onto one
//! provider. See `docs/HOW-IT-WORKS.md` ("Compare").

use anyhow::{bail, Result};

use crate::config::Config;
use crate::db::Db;

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
