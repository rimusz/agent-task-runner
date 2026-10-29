//! Task `after:` dependencies: wait for success, hold on failure, reject cycles.

use crate::db::{Db, Task};
use anyhow::{bail, Result};
use serde::Deserializer;
use std::collections::{HashMap, HashSet};

/// Outcome of checking a task's `after` list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Ready,
    Waiting { pending: Vec<String> },
    Held { reason: String },
}

pub fn parse_after<S: AsRef<str>>(items: &[S]) -> Vec<String> {
    let mut out = Vec::new();
    for item in items {
        for raw in item.as_ref().split(|c: char| c == ',' || c.is_whitespace()) {
            let k = raw.trim();
            if k.is_empty() {
                continue;
            }
            if !out.iter().any(|e| e == k) {
                out.push(k.to_string());
            }
        }
    }
    out
}

pub fn split_deps(raw: &str) -> Vec<String> {
    parse_after(&[raw])
}

/// Persist `after`, rejecting missing keys, self-refs, and cycles.
pub fn set_after(db: &Db, key: &str, raw: &[String]) -> Result<Vec<String>> {
    let after = parse_after(raw);
    if after.is_empty() {
        db.set_task_deps(key, "")?;
        return Ok(after);
    }
    check_acyclic(db, key, &after)?;
    db.set_task_deps(key, &after.join(","))?;
    Ok(after)
}

pub fn check_acyclic(db: &Db, self_key: &str, after: &[String]) -> Result<()> {
    if after.iter().any(|k| k == self_key) {
        bail!("{self_key} cannot wait for itself");
    }
    let mut graph: HashMap<String, Vec<String>> = HashMap::new();
    for t in db.list_all_tasks()? {
        graph.insert(t.key.clone(), split_deps(&t.deps));
    }
    graph.insert(self_key.to_string(), after.to_vec());
    for dep in after {
        if dep == self_key {
            bail!("{self_key} cannot wait for itself");
        }
        if !graph.contains_key(dep) {
            bail!("no task {dep}");
        }
        if let Some(loop_path) = cycle_from(&graph, self_key, dep) {
            bail!("that would wait in a circle: {}", loop_path.join(" → "));
        }
    }
    Ok(())
}

fn cycle_from(
    graph: &HashMap<String, Vec<String>>,
    self_key: &str,
    start: &str,
) -> Option<Vec<String>> {
    fn walk(
        graph: &HashMap<String, Vec<String>>,
        self_key: &str,
        cur: &str,
        path: &mut Vec<String>,
        seen: &mut HashSet<String>,
    ) -> Option<Vec<String>> {
        if cur == self_key {
            let mut out = path.clone();
            out.push(cur.to_string());
            return Some(out);
        }
        if !seen.insert(cur.to_string()) {
            return None;
        }
        path.push(cur.to_string());
        for next in graph.get(cur).into_iter().flatten() {
            if let Some(hit) = walk(graph, self_key, next, path, seen) {
                return Some(hit);
            }
        }
        path.pop();
        None
    }
    let mut path = vec![self_key.to_string()];
    let mut seen = HashSet::new();
    walk(graph, self_key, start, &mut path, &mut seen)
}

pub fn decide(db: &Db, task: &Task) -> Result<Decision> {
    decide_with(&split_deps(&task.deps), |k| db.get_task(k))
}

/// Injectable lookup so tests do not need a live scheduler.
pub fn decide_with(
    after: &[String],
    mut get: impl FnMut(&str) -> Result<Option<Task>>,
) -> Result<Decision> {
    if after.is_empty() {
        return Ok(Decision::Ready);
    }
    let mut pending = Vec::new();
    let mut halts = Vec::new();
    for key in after {
        match get(key)? {
            None => halts.push(format!("{key} is gone")),
            Some(dep) => match classify_dep(&dep.status) {
                DepKind::Success => {}
                DepKind::Open => pending.push(key.clone()),
                DepKind::Halt => {
                    let word = match dep.status.as_str() {
                        "cancelled" => "was cancelled",
                        "held" => "is held",
                        _ => "failed",
                    };
                    halts.push(format!("{key} {word}"));
                }
            },
        }
    }
    if !halts.is_empty() {
        return Ok(Decision::Held {
            reason: format!("after {}", halts.join(", ")),
        });
    }
    if !pending.is_empty() {
        return Ok(Decision::Waiting { pending });
    }
    Ok(Decision::Ready)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DepKind {
    Success,
    Open,
    Halt,
}

fn classify_dep(status: &str) -> DepKind {
    match status {
        "done" | "waiting" => DepKind::Success,
        "failed" | "cancelled" | "held" => DepKind::Halt,
        _ => DepKind::Open,
    }
}

/// Newly held dependents of a task that just failed or was cancelled.
pub fn hold_dependents(db: &Db, failed_key: &str) -> Result<Vec<(String, String)>> {
    let mut out = Vec::new();
    for t in db.list_all_tasks()? {
        if t.key == failed_key {
            continue;
        }
        if !matches!(t.status.as_str(), "queued" | "waiting") {
            continue;
        }
        if !split_deps(&t.deps).iter().any(|d| d == failed_key) {
            continue;
        }
        if let Decision::Held { reason } = decide(db, &t)? {
            db.set_status(&t.key, "held")?;
            db.set_hold(&t.key, &reason, t.hold_notified)?;
            out.push((t.key, reason));
        }
    }
    Ok(out)
}

/// Serde helper: `after = "a"`, `"a, b"`, or a list.
pub fn de_after<'de, D>(d: D) -> std::result::Result<Vec<String>, D::Error>
where
    D: Deserializer<'de>,
{
    #[derive(serde::Deserialize)]
    #[serde(untagged)]
    enum OneOrMany {
        One(String),
        Many(Vec<String>),
    }
    let v: Option<OneOrMany> = serde::Deserialize::deserialize(d)?;
    let items = match v {
        None => Vec::new(),
        Some(OneOrMany::One(s)) => vec![s],
        Some(OneOrMany::Many(v)) => v,
    };
    Ok(parse_after(&items))
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Result;

    fn task(key: &str, status: &str, deps: &str) -> Task {
        Task {
            key: key.into(),
            status: status.into(),
            deps: deps.into(),
            ..Task::default()
        }
    }

    fn lookup(rows: &[Task]) -> impl FnMut(&str) -> Result<Option<Task>> + '_ {
        move |k| Ok(rows.iter().find(|t| t.key == k).cloned())
    }

    #[test]
    fn ready_when_all_succeed() {
        let rows = vec![task("a", "done", ""), task("p", "waiting", "")];
        assert_eq!(
            decide_with(&["a".into(), "p".into()], lookup(&rows)).unwrap(),
            Decision::Ready
        );
    }

    #[test]
    fn waiting_while_open() {
        let rows = vec![task("a", "queued", ""), task("b", "running", "")];
        match decide_with(&["a".into(), "b".into()], lookup(&rows)).unwrap() {
            Decision::Waiting { pending } => assert_eq!(pending, vec!["a", "b"]),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn hold_on_failed_or_cancelled_or_missing() {
        let rows = vec![task("a", "failed", "")];
        match decide_with(&["a".into()], lookup(&rows)).unwrap() {
            Decision::Held { reason } => assert!(reason.contains("a failed"), "{reason}"),
            other => panic!("{other:?}"),
        }
        let rows = vec![task("a", "cancelled", "")];
        match decide_with(&["a".into()], lookup(&rows)).unwrap() {
            Decision::Held { reason } => assert!(reason.contains("cancelled"), "{reason}"),
            other => panic!("{other:?}"),
        }
        match decide_with(&["gone".into()], lookup(&[])).unwrap() {
            Decision::Held { reason } => assert!(reason.contains("gone"), "{reason}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn cycle_and_self_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("s.db")).unwrap();
        db.upsert_task("a", "/r", None).unwrap();
        db.upsert_task("b", "/r", None).unwrap();
        db.set_task_deps("a", "b").unwrap();
        let err = check_acyclic(&db, "b", &["a".into()])
            .unwrap_err()
            .to_string();
        assert!(err.contains("circle"), "{err}");
        let err = check_acyclic(&db, "a", &["a".into()])
            .unwrap_err()
            .to_string();
        assert!(err.contains("itself"), "{err}");
        let err = check_acyclic(&db, "c", &["nope".into()])
            .unwrap_err()
            .to_string();
        assert!(err.contains("no task"), "{err}");
    }

    #[test]
    fn hold_dependents_marks_queued_children() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("s.db")).unwrap();
        db.upsert_task("a", "/r", None).unwrap();
        db.upsert_task("b", "/r", None).unwrap();
        db.set_task_deps("b", "a").unwrap();
        db.set_status("a", "failed").unwrap();
        let held = hold_dependents(&db, "a").unwrap();
        assert_eq!(held.len(), 1);
        assert_eq!(held[0].0, "b");
        assert_eq!(db.get_task("b").unwrap().unwrap().status, "held");
    }
}
