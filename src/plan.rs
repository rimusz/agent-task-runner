//! Driver planner: split a large issue into linked child tasks.

use crate::config::{Config, PlannerMode, Provider};
use crate::db::{Db, Task};
use anyhow::Result;
use serde::Deserialize;
use serde_json::Value;

pub const PLAN_SCHEMA: &str = r#"{"type":"object","properties":{"subtasks":{"type":"array","items":{"type":"object","properties":{"title":{"type":"string"},"prompt":{"type":"string"},"provider_hint":{"type":"string"},"deps":{"type":"array","items":{"type":"string"}}},"required":["title","prompt"]}}},"required":["subtasks"]}"#;

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct Subtask {
    pub title: String,
    pub prompt: String,
    #[serde(default)]
    pub provider_hint: String,
    #[serde(default)]
    pub deps: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct Plan {
    #[serde(default)]
    pub subtasks: Vec<Subtask>,
}

pub fn should_plan(cfg: &Config, body: &str) -> bool {
    match cfg.planner.mode {
        PlannerMode::Off => false,
        PlannerMode::Always => true,
        PlannerMode::Auto => body.chars().count() >= cfg.planner.auto_chars,
    }
}

pub fn is_driver_role(role: &str) -> bool {
    matches!(role, "plan" | "review")
}

pub fn plan_prompt(issue: &str) -> String {
    format!(
        "You are a read-only planner. Do not edit files, run writes, or commit.\n\
         Read the issue and the repository. Split the work only if it needs\n\
         more than one independent change.\n\
         Return JSON matching this schema:\n{PLAN_SCHEMA}\n\
         deps is a list of earlier subtask titles this one must wait for.\n\
         provider_hint is an optional atr provider name (claude, grok, …).\n\
         Keep at most a handful of subtasks. If the issue is one change,\n\
         return a single subtask.\n\n---\n\n{issue}"
    )
}

pub fn apply_plan_args(p: &mut Provider, max_turns: u32) {
    strip_output_format(&mut p.args);
    if p.kind == "grok" {
        p.permission_mode = "plan".into();
        p.plugin = false;
    }
    p.extra_args.extend([
        "--max-turns".into(),
        max_turns.max(1).to_string(),
        "--output-format".into(),
        "json".into(),
        "--json-schema".into(),
        PLAN_SCHEMA.into(),
    ]);
}

fn strip_output_format(args: &mut Vec<String>) {
    let mut out = Vec::with_capacity(args.len());
    let mut skip = false;
    for a in args.drain(..) {
        if skip {
            skip = false;
            continue;
        }
        if a == "--output-format" {
            skip = true;
            continue;
        }
        out.push(a);
    }
    *args = out;
}

pub fn parse_plan(text: &str) -> Result<Plan> {
    if let Some(p) = parse_plan_value(&first_json_value(text)?) {
        return Ok(trim_plan(p));
    }
    anyhow::bail!("planner result was not a subtask list")
}

fn parse_plan_value(v: &Value) -> Option<Plan> {
    if let Ok(p) = serde_json::from_value::<Plan>(v.clone()) {
        if !p.subtasks.is_empty() || v.get("subtasks").is_some() {
            return Some(p);
        }
    }
    for key in ["structuredOutput", "structured_output", "result"] {
        if let Some(inner) = v.get(key) {
            if let Some(p) = parse_plan_value(inner) {
                return Some(p);
            }
            if let Some(s) = inner.as_str() {
                if let Ok(p) = parse_plan(s) {
                    return Some(p);
                }
            }
        }
    }
    None
}

pub fn first_json_value(text: &str) -> Result<Value> {
    let t = text.trim();
    if let Ok(v) = serde_json::from_str::<Value>(t) {
        return Ok(v);
    }
    if let Some(block) = fenced_json(t) {
        if let Ok(v) = serde_json::from_str::<Value>(block) {
            return Ok(v);
        }
    }
    if let Some((start, end)) = find_object(t) {
        if let Ok(v) = serde_json::from_str::<Value>(&t[start..end]) {
            return Ok(v);
        }
    }
    anyhow::bail!("no JSON object in planner output")
}

pub fn fenced_json(text: &str) -> Option<&str> {
    let after = text.split_once("```")?;
    let rest = after.1.trim_start();
    let rest = rest
        .strip_prefix("json")
        .or_else(|| rest.strip_prefix("JSON"))
        .unwrap_or(rest)
        .trim_start_matches(['\r', '\n']);
    let (body, _) = rest.split_once("```")?;
    Some(body.trim())
}

fn find_object(text: &str) -> Option<(usize, usize)> {
    let start = text.find('{')?;
    let mut depth = 0i32;
    for (i, c) in text[start..].char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some((start, start + i + 1));
                }
            }
            _ => {}
        }
    }
    None
}

fn trim_plan(mut plan: Plan) -> Plan {
    plan.subtasks
        .retain(|s| !s.title.trim().is_empty() && !s.prompt.trim().is_empty());
    plan
}

/// Turn a successful plan into child tasks. Returns child keys.
pub fn apply_plan(db: &Db, parent: &Task, plan: &Plan, max_subtasks: u32) -> Result<Vec<String>> {
    let cap = max_subtasks.max(1) as usize;
    let subs: Vec<_> = plan.subtasks.iter().take(cap).collect();
    if subs.is_empty() {
        db.set_task_role(&parent.key, "worker")?;
        return Ok(Vec::new());
    }
    if subs.len() == 1 {
        db.set_task_role(&parent.key, "worker")?;
        if !subs[0].provider_hint.is_empty() {
            db.set_provider_hint(&parent.key, &subs[0].provider_hint)?;
        }
        if !subs[0].title.is_empty() {
            db.set_task_title(&parent.key, &subs[0].title)?;
        }
        db.enqueue_message(&parent.key, &subs[0].prompt, "planner")?;
        return Ok(vec![parent.key.clone()]);
    }

    db.set_task_role(&parent.key, "parent")?;
    db.set_status(&parent.key, "waiting")?;
    let mut keys = Vec::new();
    let mut title_to_key = Vec::new();
    for (i, sub) in subs.iter().enumerate() {
        let key = format!("{}-s{}", parent.key, i + 1);
        db.upsert_task(&key, parent.repo.as_deref().unwrap_or(""), parent.issue)?;
        db.set_parent_key(&key, &parent.key)?;
        db.set_task_role(&key, "worker")?;
        db.set_task_title(&key, &sub.title)?;
        if !sub.provider_hint.is_empty() {
            db.set_provider_hint(&key, &sub.provider_hint)?;
        }
        if parent.shadow {
            db.mark_shadow(&key)?;
        }
        keys.push(key.clone());
        title_to_key.push((sub.title.to_ascii_lowercase(), key));
    }
    for (i, sub) in subs.iter().enumerate() {
        let child = &keys[i];
        let deps = resolve_deps(&sub.deps, &title_to_key, &keys);
        if !deps.is_empty() {
            if let Err(e) = crate::deps::set_after(db, child, &deps) {
                tracing::warn!(task = %child, "plan after rejected: {e:#}");
            }
        }
        db.enqueue_message(child, &sub.prompt, "planner")?;
        db.set_status(child, "queued")?;
    }
    Ok(keys)
}

fn resolve_deps(deps: &[String], titles: &[(String, String)], keys: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    for d in deps {
        let t = d.trim();
        if t.is_empty() {
            continue;
        }
        if keys.iter().any(|k| k == t) {
            out.push(t.to_string());
            continue;
        }
        let low = t.to_ascii_lowercase();
        if let Some((_, key)) = titles.iter().find(|(title, _)| title == &low) {
            out.push(key.clone());
            continue;
        }
        if let Some(n) = t
            .strip_prefix('s')
            .or_else(|| t.strip_prefix('S'))
            .and_then(|s| s.parse::<usize>().ok())
            .or_else(|| t.parse::<usize>().ok())
        {
            if n > 0 && n <= keys.len() {
                out.push(keys[n - 1].clone());
            }
        }
    }
    out
}

pub fn deps_ready(db: &Db, task: &Task) -> Result<bool> {
    Ok(matches!(
        crate::deps::decide(db, task)?,
        crate::deps::Decision::Ready
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_structured_and_fenced() {
        let raw = r#"{"structuredOutput":{"subtasks":[{"title":"A","prompt":"do a","deps":[]}]}}"#;
        let p = parse_plan(raw).unwrap();
        assert_eq!(p.subtasks[0].title, "A");

        let fenced = "here\n```json\n{\"subtasks\":[{\"title\":\"B\",\"prompt\":\"do b\"}]}\n```\n";
        let p = parse_plan(fenced).unwrap();
        assert_eq!(p.subtasks[0].title, "B");
    }

    #[test]
    fn auto_only_splits_large() {
        let raw: crate::config::RawConfig = toml::from_str(
            r#"
repo_path = "/tmp"
[[provider]]
name = "grok"
kind = "grok"
"#,
        )
        .unwrap();
        let cfg = crate::config::Config::from_raw(raw).unwrap();
        assert!(!should_plan(&cfg, "short"));
        assert!(should_plan(&cfg, &"x".repeat(cfg.planner.auto_chars)));
    }

    #[test]
    fn resolve_titles_and_indexes() {
        let titles = vec![("one".into(), "p-s1".into()), ("two".into(), "p-s2".into())];
        let keys = vec!["p-s1".into(), "p-s2".into()];
        assert_eq!(
            resolve_deps(&["One".into(), "1".into()], &titles, &keys),
            vec!["p-s1", "p-s1"]
        );
    }
}
