use crate::config::{Config, HandoffConfig};
use crate::db::{Db, EventRow, Task};
use crate::runner::RunResult;
use anyhow::Result;
use serde_json::Value;
use std::path::Path;
use std::process::Command;

/// Persist stream events that can later seed a provider-switch brief.
pub fn should_store_event(value: &Value) -> bool {
    if !value.is_object() {
        return false;
    }
    let ty = value.get("type").and_then(|v| v.as_str()).unwrap_or("");
    if matches!(ty, "system" | "progress" | "heartbeat") {
        return false;
    }
    if value.get("subtype").and_then(|v| v.as_str()) == Some("progress") {
        return false;
    }
    ty == "assistant"
        || ty == "tool"
        || ty == "tool_use"
        || ty == "tool_result"
        || ty == "user"
        || ty == "result"
        || ty == "error"
        || ty == "turn.failed"
        || ty == "item.completed"
        || ty == "function_call"
        || ty == "text"
        || ty == "thought"
        || ty == "tool_call"
        || ty == "tool_call_update"
        || ty == "usage"
        || ty == "end"
        || ty == "SubagentCompleted"
        || ty == "subagent_completed"
        || value.get("message").is_some()
        || value.pointer("/message/content").is_some()
        || value.get("text").is_some()
        || value.get("sessionId").is_some()
        || value.get("usage").is_some()
}

pub fn event_kind(value: &Value) -> String {
    if let Some(t) = value.get("type").and_then(|v| v.as_str()) {
        if !t.is_empty() {
            return t.to_string();
        }
    }
    if value.get("text").is_some() {
        return "text".into();
    }
    "event".into()
}

/// Grok `streaming-json` sends the answer as `{"type":"text","data":"…"}`
/// deltas. Join the last run of deltas (thoughts/usage/end in between are
/// skipped; a tool call ends the run).
/// OpenCode `run --format json` prints `{"type":"text","part":{"type":"text",
/// "text":…,"messageID":…}}`. The answer is the last message's text parts
/// (latest version of each part id), joined in order.
fn opencode_text(events: &[EventRow]) -> Option<String> {
    let parsed: Vec<Value> = events
        .iter()
        .filter_map(|e| serde_json::from_str::<Value>(&e.raw).ok())
        .filter(|v| {
            v.get("type").and_then(|t| t.as_str()) == Some("text")
                && v.pointer("/part/type").and_then(|t| t.as_str()) == Some("text")
                && v.pointer("/part/text").and_then(|t| t.as_str()).is_some()
        })
        .collect();
    let last = parsed.last()?;
    let msg = last.pointer("/part/messageID").and_then(|m| m.as_str());
    let mut order: Vec<String> = Vec::new();
    let mut by_id: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for v in &parsed {
        if v.pointer("/part/messageID").and_then(|m| m.as_str()) != msg {
            continue;
        }
        let id = v
            .pointer("/part/id")
            .and_then(|i| i.as_str())
            .unwrap_or("")
            .to_string();
        let text = v
            .pointer("/part/text")
            .and_then(|t| t.as_str())
            .unwrap_or("");
        if !by_id.contains_key(&id) {
            order.push(id.clone());
        }
        by_id.insert(id, text.to_string());
    }
    let joined = order
        .iter()
        .filter_map(|id| by_id.get(id))
        .map(|t| t.trim())
        .filter(|t| !t.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    if joined.is_empty() {
        None
    } else {
        Some(joined)
    }
}

fn grok_stream_text(events: &[EventRow]) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    for ev in events.iter().rev() {
        let Ok(v) = serde_json::from_str::<Value>(&ev.raw) else {
            continue;
        };
        let ty = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
        match ty {
            "text" => match v.get("data").and_then(|d| d.as_str()) {
                Some(d) => parts.push(d.to_string()),
                None => break,
            },
            "thought" | "usage" | "end" => continue,
            _ => {
                if !parts.is_empty() {
                    break;
                }
                // Nothing after the last tool call yet: no grok answer.
                return None;
            }
        }
    }
    if parts.is_empty() {
        return None;
    }
    parts.reverse();
    let joined = parts.concat();
    if joined.trim().is_empty() {
        None
    } else {
        Some(joined.trim().to_string())
    }
}

pub fn last_assistant_text(events: &[EventRow]) -> Option<String> {
    if let Some(t) = grok_stream_text(events) {
        return Some(t);
    }
    if let Some(t) = opencode_text(events) {
        return Some(t);
    }
    for ev in events.iter().rev() {
        if let Ok(v) = serde_json::from_str::<Value>(&ev.raw) {
            if let Some(t) = assistant_text(&v) {
                if !t.trim().is_empty() {
                    return Some(t);
                }
            }
        }
    }
    None
}

pub fn commands_from_events(events: &[EventRow]) -> Vec<String> {
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for ev in events {
        if let Ok(v) = serde_json::from_str::<Value>(&ev.raw) {
            for c in commands_in_value(&v) {
                let norm = normalize_command(&c);
                if norm.is_empty() || !seen.insert(norm.clone()) {
                    continue;
                }
                out.push(norm);
            }
        }
    }
    out
}

/// Strip a leading `cd <dir> &&` / `cd <dir>;` so the same command with
/// different working-directory prefixes is only listed once.
pub fn normalize_command(cmd: &str) -> String {
    let mut t = cmd.trim().to_string();
    loop {
        let lower = t.to_ascii_lowercase();
        if !lower.starts_with("cd ") {
            break;
        }
        if let Some(idx) = t.find("&&") {
            t = t[idx + 2..].trim().to_string();
            continue;
        }
        if let Some(idx) = t.find(';') {
            t = t[idx + 1..].trim().to_string();
            continue;
        }
        break;
    }
    t.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub fn test_output_from_events(events: &[EventRow]) -> Option<String> {
    for ev in events.iter().rev() {
        if let Ok(v) = serde_json::from_str::<Value>(&ev.raw) {
            if let Some(t) = tool_result_text(&v) {
                if looks_like_test_failure(&t) {
                    return Some(t);
                }
            }
        }
    }
    None
}

fn assistant_text(value: &Value) -> Option<String> {
    // Codex `exec --json`: `{"type":"item.completed","item":{"type":
    // "agent_message","text":"…"}}` (errors are `item.type == "error"`).
    if let Some(item) = value.get("item") {
        if item.get("type").and_then(|v| v.as_str()) == Some("agent_message") {
            if let Some(t) = item.get("text").and_then(|v| v.as_str()) {
                if !t.is_empty() {
                    return Some(t.to_string());
                }
            }
        }
        return None;
    }
    if let Some(arr) = value.pointer("/message/content").and_then(|v| v.as_array()) {
        let mut parts = Vec::new();
        for item in arr {
            if item.get("type").and_then(|v| v.as_str()) == Some("tool_use") {
                continue;
            }
            if let Some(t) = item.get("text").and_then(|v| v.as_str()) {
                if !t.is_empty() {
                    parts.push(t.to_string());
                }
            }
        }
        if !parts.is_empty() {
            return Some(parts.join("\n"));
        }
    }
    if let Some(t) = value.get("text").and_then(|v| v.as_str()) {
        if !t.is_empty() && value.get("usage").is_none() {
            return Some(t.to_string());
        }
    }
    if let Some(t) = value.get("result").and_then(|v| v.as_str()) {
        if value.get("is_error").and_then(|v| v.as_bool()) != Some(true) && !t.is_empty() {
            return Some(t.to_string());
        }
    }
    None
}

fn commands_in_value(value: &Value) -> Vec<String> {
    let mut out = Vec::new();
    collect_commands(value, &mut out);
    out
}

fn collect_commands(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::Object(map) => {
            let name = map
                .get("name")
                .or_else(|| map.get("toolName"))
                .or_else(|| map.get("tool_name"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let input = map
                .get("input")
                .or_else(|| map.get("toolInput"))
                .or_else(|| map.get("tool_input"));
            if let Some(cmd) = input.and_then(|i| {
                i.get("command")
                    .or_else(|| i.get("cmd"))
                    .and_then(|v| v.as_str())
            }) {
                if looks_like_shell_tool(name) || name.is_empty() {
                    out.push(cmd.to_string());
                }
            }
            if let Some(cmd) = map.get("command").and_then(|v| v.as_str()) {
                if map.get("type").and_then(|v| v.as_str()) == Some("function_call")
                    || looks_like_shell_tool(name)
                {
                    out.push(cmd.to_string());
                }
            }
            for v in map.values() {
                collect_commands(v, out);
            }
        }
        Value::Array(arr) => {
            for v in arr {
                collect_commands(v, out);
            }
        }
        _ => {}
    }
}

fn looks_like_shell_tool(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    n.is_empty()
        || n.contains("bash")
        || n.contains("shell")
        || n.contains("command")
        || n == "exec"
}

fn tool_result_text(value: &Value) -> Option<String> {
    if let Some(arr) = value.pointer("/message/content").and_then(|v| v.as_array()) {
        for item in arr {
            if item.get("type").and_then(|v| v.as_str()) == Some("tool_result") {
                if let Some(t) = item.get("content").and_then(|v| v.as_str()) {
                    return Some(t.to_string());
                }
                if let Some(t) = item.get("content").and_then(|v| v.as_array()).map(|a| {
                    a.iter()
                        .filter_map(|x| x.get("text").and_then(|t| t.as_str()))
                        .collect::<Vec<_>>()
                        .join("\n")
                }) {
                    if !t.is_empty() {
                        return Some(t);
                    }
                }
            }
        }
    }
    if let Some(t) = value.get("content").and_then(|v| v.as_str()) {
        if value.get("type").and_then(|v| v.as_str()) == Some("tool_result") {
            return Some(t.to_string());
        }
    }
    None
}

fn looks_like_test_failure(text: &str) -> bool {
    let t = text.to_ascii_lowercase();
    t.contains("test result: failed")
        || t.contains("failures:")
        || t.contains("assertionerror")
        || t.contains("assert") && t.contains("failed")
        || t.contains("error[e")
        || t.contains("failed (")
}

pub fn provider_written_notes(worktree: &Path) -> Option<String> {
    let path = worktree.join(".atr").join("handoff.md");
    let text = std::fs::read_to_string(path).ok()?;
    let t = text.trim();
    if t.is_empty() {
        None
    } else {
        Some(t.to_string())
    }
}

pub fn git_snapshot(worktree: &Path, base: &str) -> (String, String, String) {
    let status = git(worktree, &["status", "--short"]);
    let mut stat = git(worktree, &["diff", "--stat", base]);
    let mut diff = git(worktree, &["diff", base]);
    // Never `git add -N`: that leaves intent-to-add entries in the index.
    let leftover = git(worktree, &["ls-files", "--others", "--exclude-standard"]);
    for path in leftover.lines() {
        let p = path.trim();
        if skip_untracked(p) {
            continue;
        }
        let extra = git(worktree, &["diff", "--no-index", "--", "/dev/null", p]);
        if extra.trim().is_empty() {
            let body = std::fs::read_to_string(worktree.join(p)).unwrap_or_default();
            diff.push_str(&format!("\n--- untracked: {p}\n{body}"));
        } else {
            if !diff.is_empty() && !diff.ends_with('\n') {
                diff.push('\n');
            }
            diff.push_str(&extra);
        }
        if !stat_has_path(&stat, p) {
            if !stat.is_empty() && !stat.ends_with('\n') {
                stat.push('\n');
            }
            stat.push_str(&format!(" {p} | untracked\n"));
        }
    }
    (status, stat, diff)
}

fn stat_has_path(stat: &str, path: &str) -> bool {
    stat.lines()
        .any(|l| l.split('|').next().is_some_and(|n| n.trim() == path))
}

fn skip_untracked(path: &str) -> bool {
    let p = path.trim();
    p.is_empty() || p == ".atr" || p.starts_with(".atr/")
}

fn git(worktree: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(worktree)
        .args(args)
        .output();
    match out {
        Ok(o) => {
            let mut s = String::from_utf8_lossy(&o.stdout).into_owned();
            if s.trim().is_empty() {
                let err = String::from_utf8_lossy(&o.stderr);
                if !err.trim().is_empty() && !o.status.success() {
                    s = format!("(git {})", err.trim());
                }
            }
            s
        }
        Err(e) => format!("(git unavailable: {e})"),
    }
}

pub struct HandoffParts<'a> {
    pub task_key: &'a str,
    pub issue: Option<i64>,
    pub from_provider: &'a str,
    pub outcome: &'a str,
    pub original_task: Option<&'a str>,
    pub latest_messages: &'a [String],
    pub summary: Option<&'a str>,
    pub provider_notes: Option<&'a str>,
    pub commands: &'a [String],
    pub test_output: Option<&'a str>,
    pub git_status: &'a str,
    pub git_stat: &'a str,
    pub git_diff: &'a str,
    pub verify: Option<&'a str>,
    pub checklist: Option<&'a str>,
}

pub fn render_block(cfg: &HandoffConfig, p: &HandoffParts<'_>) -> String {
    let mut sections: Vec<(u8, String)> = Vec::new();
    let mut head = format!(
        "Previous provider {} ended ({}). Continue in the same worktree.\nTask key: {}",
        p.from_provider, p.outcome, p.task_key
    );
    if let Some(n) = p.issue {
        head.push_str(&format!("\nIssue: {n}"));
    }
    sections.push((0, head));

    let mut task_sec = String::from("### Original task and latest messages");
    if let Some(orig) = p.original_task {
        task_sec.push_str("\n\nOriginal:\n");
        task_sec.push_str(orig.trim());
    }
    if !p.latest_messages.is_empty() {
        task_sec.push_str("\n\nLatest:\n");
        for (i, m) in p.latest_messages.iter().enumerate() {
            if i > 0 {
                task_sec.push_str("\n---\n");
            }
            task_sec.push_str(m.trim());
        }
    }
    sections.push((1, task_sec));

    if let Some(s) = p.summary.filter(|s| !s.trim().is_empty()) {
        sections.push((2, format!("### Previous run summary\n{}", s.trim())));
    }
    if let Some(n) = p.provider_notes.filter(|s| !s.trim().is_empty()) {
        sections.push((
            3,
            format!("### Notes written by the outgoing provider\n{}", n.trim()),
        ));
    }
    if !p.commands.is_empty() {
        let mut c = String::from("### Commands already run");
        for cmd in p.commands {
            c.push_str("\n- `");
            c.push_str(cmd.trim());
            c.push('`');
        }
        sections.push((4, c));
    }
    if let Some(t) = p.test_output.filter(|s| !s.trim().is_empty()) {
        sections.push((
            5,
            format!("### Failing test output\n```\n{}\n```", t.trim()),
        ));
    }
    if let Some(v) = p.verify.filter(|s| !s.trim().is_empty()) {
        sections.push((6, format!("### Verify checks\n{}", v.trim())));
    }
    if let Some(c) = p.checklist.filter(|s| !s.trim().is_empty()) {
        sections.push((6, format!("### Checklist\n{}", c.trim())));
    }

    let status = if p.git_status.trim().is_empty() {
        "(clean)"
    } else {
        p.git_status.trim()
    };
    let stat = if p.git_stat.trim().is_empty() {
        "(no diffstat)"
    } else {
        p.git_stat.trim()
    };
    sections.push((
        7,
        format!("### Worktree git status\n{status}\n\n### Diffstat against base\n{stat}"),
    ));
    if cfg.include_diff && !p.git_diff.trim().is_empty() {
        sections.push((
            8,
            format!("### Diff against base\n```diff\n{}\n```", p.git_diff.trim()),
        ));
    }

    pack_sections(&sections, cfg.max_bytes)
}

fn pack_sections(sections: &[(u8, String)], max_bytes: usize) -> String {
    let mut kept: Vec<String> = sections.iter().map(|(_, s)| s.clone()).collect();
    loop {
        let joined = kept.join("\n\n");
        if joined.len() <= max_bytes {
            return joined;
        }
        if kept.len() > 1 {
            // Drop the lowest-priority (last) section first — usually the diff.
            kept.pop();
            continue;
        }
        return truncate_bytes(&joined, max_bytes);
    }
}

fn truncate_bytes(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let keep = max.saturating_sub(16);
    let mut end = keep;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…[truncated]", &s[..end])
}

pub fn build_for_switch(
    cfg: &Config,
    db: &Db,
    key: &str,
    task: &Task,
    worktree: &Path,
    from_provider: &str,
    outcome: &str,
    result: Option<&RunResult>,
    outgoing_run_id: Option<i64>,
) -> Result<String> {
    let messages = db.all_messages(key).unwrap_or_default();
    let original = messages.first().map(|m| m.body.clone());
    // Latest = unhandled (not yet consumed) messages, excluding the original
    // so it is not repeated under both headings.
    let latest: Vec<String> = db
        .pending_messages(key)
        .unwrap_or_default()
        .into_iter()
        .map(|m| m.body)
        .filter(|b| original.as_deref() != Some(b.as_str()))
        .collect();

    let events = if let Some(id) = outgoing_run_id {
        db.events_for_run(id).unwrap_or_default()
    } else {
        db.events_for_task(key).unwrap_or_default()
    };
    let all_events = db.events_for_task(key).unwrap_or_default();
    let summary = last_assistant_text(&events)
        .or_else(|| result.and_then(|r| (!r.snippet.is_empty()).then(|| r.snippet.clone())));
    let mut commands = commands_from_events(&all_events);
    if commands.len() > 20 {
        commands = commands.split_off(commands.len() - 20);
    }
    let test_output = test_output_from_events(&all_events);
    let notes = provider_written_notes(worktree);
    let (git_status, git_stat, git_diff) = git_snapshot(worktree, &cfg.repo_default_branch);
    let verify = outgoing_run_id
        .and_then(|id| db.verify_summary_for_run(id).ok())
        .filter(|s| !s.is_empty());
    let checklist = if cfg.checklist.enabled {
        crate::checklist::load(worktree, cfg.checklist.max_bytes)
    } else {
        None
    };

    Ok(render_block(
        &cfg.handoff,
        &HandoffParts {
            task_key: key,
            issue: task.issue,
            from_provider,
            outcome,
            original_task: original.as_deref(),
            latest_messages: &latest,
            summary: summary.as_deref(),
            provider_notes: notes.as_deref(),
            commands: &commands,
            test_output: test_output.as_deref(),
            git_status: &git_status,
            git_stat: &git_stat,
            git_diff: &git_diff,
            verify: verify.as_deref(),
            checklist: checklist.as_deref(),
        },
    ))
}

pub fn needs_handoff(pending: Option<&str>, switching: bool, cannot_resume: bool) -> bool {
    pending.map(|s| !s.is_empty()).unwrap_or(false) || switching || cannot_resume
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opencode_answer_is_last_message_text_parts() {
        let ev = |raw: &str| EventRow {
            run_id: 1,
            kind: "x".into(),
            raw: raw.into(),
        };
        let events = vec![
            ev(
                r#"{"type":"text","part":{"id":"p1","messageID":"m1","type":"text","text":"Let me run the tests:"}}"#,
            ),
            ev(r#"{"type":"tool_use","part":{"messageID":"m1","type":"tool","tool":"read"}}"#),
            ev(
                r#"{"type":"text","part":{"id":"p2","messageID":"m2","type":"text","text":"Summary:"}}"#,
            ),
            ev(
                r#"{"type":"text","part":{"id":"p3","messageID":"m2","type":"text","text":"Added days."}}"#,
            ),
            ev(r#"{"type":"step_finish","part":{"type":"step-finish","reason":"stop"}}"#),
        ];
        assert_eq!(
            last_assistant_text(&events).as_deref(),
            Some("Summary:\nAdded days.")
        );
    }
    use crate::db::EventRow;

    fn ev(kind: &str, raw: &str) -> EventRow {
        EventRow {
            run_id: 1,
            kind: kind.into(),
            raw: raw.into(),
        }
    }

    #[test]
    fn stores_grok_stream_shape() {
        let v: Value = serde_json::from_str(
            r#"{"text":"done","sessionId":"g1","usage":{"input_tokens":1,"output_tokens":2}}"#,
        )
        .unwrap();
        assert!(should_store_event(&v));
        assert_eq!(event_kind(&v), "text");
    }

    #[test]
    fn extracts_claude_assistant_and_bash() {
        let events = vec![
            ev(
                "assistant",
                r#"{"type":"assistant","message":{"content":[{"type":"text","text":"I edited README.md"}]}}"#,
            ),
            ev(
                "assistant",
                r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash","input":{"command":"printf x >> README.md"}}]}}"#,
            ),
            ev(
                "user",
                r#"{"type":"user","message":{"content":[{"type":"tool_result","content":"test result: FAILED\nfailures:\nfoo"}]}}"#,
            ),
        ];
        assert_eq!(
            last_assistant_text(&events).as_deref(),
            Some("I edited README.md")
        );
        assert_eq!(
            commands_from_events(&events),
            vec!["printf x >> README.md".to_string()]
        );
        assert!(test_output_from_events(&events)
            .unwrap()
            .contains("test result: FAILED"));
    }

    #[test]
    fn cap_drops_diff_first() {
        let cfg = HandoffConfig {
            max_bytes: 280,
            include_diff: true,
        };
        let big_diff = "x".repeat(400);
        let out = render_block(
            &cfg,
            &HandoffParts {
                task_key: "acme-1",
                issue: Some(1),
                from_provider: "claude",
                outcome: "stalled",
                original_task: Some("please fix it"),
                latest_messages: &["please fix it".into()],
                summary: Some("I edited README.md"),
                provider_notes: None,
                commands: &["echo hi".into()],
                test_output: None,
                git_status: " M README.md",
                git_stat: " README.md | 1 +",
                git_diff: &big_diff,
                verify: None,
                checklist: None,
            },
        );
        assert!(out.len() <= 280, "{}", out.len());
        assert!(out.contains("please fix it"));
        assert!(out.contains("I edited README.md"));
        assert!(!out.contains(&big_diff));
    }

    #[test]
    fn include_diff_false_omits_patch() {
        let cfg = HandoffConfig {
            max_bytes: 10_000,
            include_diff: false,
        };
        let out = render_block(
            &cfg,
            &HandoffParts {
                task_key: "acme-1",
                issue: None,
                from_provider: "claude",
                outcome: "limit",
                original_task: Some("task"),
                latest_messages: &[],
                summary: None,
                provider_notes: None,
                commands: &[],
                test_output: None,
                git_status: " M f",
                git_stat: " f | 1 +",
                git_diff: "+hello unique-patch-line",
                verify: None,
                checklist: None,
            },
        );
        assert!(out.contains("M f"));
        assert!(!out.contains("unique-patch-line"));
    }

    #[test]
    fn checklist_is_in_handoff_and_counts() {
        let cfg = HandoffConfig {
            max_bytes: 10_000,
            include_diff: false,
        };
        let out = render_block(
            &cfg,
            &HandoffParts {
                task_key: "acme-1",
                issue: None,
                from_provider: "claude",
                outcome: "success",
                original_task: Some("task"),
                latest_messages: &[],
                summary: None,
                provider_notes: None,
                commands: &[],
                test_output: None,
                git_status: "",
                git_stat: "",
                git_diff: "",
                verify: None,
                checklist: Some("- [x] one\n- [ ] two"),
            },
        );
        assert!(out.contains("### Checklist"), "{out}");
        assert!(out.contains("- [x] one"), "{out}");
        let tiny = HandoffConfig {
            max_bytes: 80,
            include_diff: true,
        };
        let small = render_block(
            &tiny,
            &HandoffParts {
                task_key: "acme-1",
                issue: None,
                from_provider: "claude",
                outcome: "stalled",
                original_task: Some("task"),
                latest_messages: &[],
                summary: None,
                provider_notes: None,
                commands: &[],
                test_output: None,
                git_status: " M f",
                git_stat: " f | 1 +",
                git_diff: &"y".repeat(400),
                verify: None,
                checklist: Some("- [x] keep-me"),
            },
        );
        assert!(small.len() <= 80, "{}", small.len());
    }

    #[test]
    fn git_snapshot_sees_edit() {
        let dir = tempfile::tempdir().unwrap();
        crate::worktree::init_sample_repo(dir.path()).unwrap();
        std::fs::write(dir.path().join("README.md"), "test repo\nedited\n").unwrap();
        let (status, stat, diff) = git_snapshot(dir.path(), "main");
        assert!(status.contains("README.md"), "{status}");
        assert!(
            stat.contains("README.md") || diff.contains("edited"),
            "{stat}\n{diff}"
        );
        assert!(diff.contains("edited"), "{diff}");
    }

    #[test]
    fn needs_handoff_for_switch_and_lost_session() {
        assert!(!needs_handoff(None, false, false));
        assert!(needs_handoff(Some("brief"), false, false));
        assert!(needs_handoff(None, true, false));
        assert!(needs_handoff(None, false, true));
    }

    #[test]
    fn git_snapshot_includes_untracked_file() {
        let dir = tempfile::tempdir().unwrap();
        crate::worktree::init_sample_repo(dir.path()).unwrap();
        std::fs::write(dir.path().join("ATR_NEW.md"), "brand-new-untracked\n").unwrap();
        std::fs::create_dir_all(dir.path().join(".atr")).unwrap();
        std::fs::write(dir.path().join(".atr/prompt.md"), "secret-prompt-body\n").unwrap();
        let (status, stat, diff) = git_snapshot(dir.path(), "main");
        assert!(
            status.contains("ATR_NEW.md") || diff.contains("brand-new-untracked"),
            "{status}\n{stat}\n{diff}"
        );
        assert!(
            diff.contains("brand-new-untracked") || diff.contains("ATR_NEW.md"),
            "{diff}"
        );
        assert!(
            !diff.contains("secret-prompt-body"),
            "runner .atr files must not land in the handoff diff:\n{diff}"
        );
        let cached = git(dir.path(), &["diff", "--cached", "--name-only"]);
        assert!(
            cached.trim().is_empty(),
            "snapshot must not leave intent-to-add in the index:\n{cached}"
        );
        assert!(
            status.contains("??") && status.contains("ATR_NEW.md"),
            "untracked file must stay untracked, not staged: {status}"
        );
    }

    #[test]
    fn git_snapshot_lists_every_untracked_file() {
        let dir = tempfile::tempdir().unwrap();
        crate::worktree::init_sample_repo(dir.path()).unwrap();
        std::fs::write(dir.path().join("ATR_A.md"), "aaa\n").unwrap();
        std::fs::write(dir.path().join("ATR_B.md"), "bbb\n").unwrap();
        std::fs::write(dir.path().join("extra.rs"), "fn x() {}\n").unwrap();
        let (_, stat, _) = git_snapshot(dir.path(), "main");
        assert!(stat.contains("ATR_A.md"), "{stat}");
        assert!(stat.contains("ATR_B.md"), "{stat}");
        assert!(stat.contains("extra.rs"), "{stat}");
        let compact = crate::shadow::short_diffstat(&stat);
        assert!(compact.contains("ATR_A.md"), "{compact}");
        assert!(compact.contains("ATR_B.md"), "{compact}");
        assert!(compact.contains("extra.rs"), "{compact}");
    }

    #[test]
    fn commands_dedupe_after_stripping_cd_prefix() {
        let events = vec![
            ev(
                "assistant",
                r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash","input":{"command":"cd /tmp/wt && cargo test"}}]}}"#,
            ),
            ev(
                "assistant",
                r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash","input":{"command":"cargo test"}}]}}"#,
            ),
            ev(
                "assistant",
                r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash","input":{"command":"cd /tmp/wt; cargo test -- --nocapture"}}]}}"#,
            ),
        ];
        assert_eq!(
            commands_from_events(&events),
            vec![
                "cargo test".to_string(),
                "cargo test -- --nocapture".to_string()
            ]
        );
    }

    #[test]
    fn latest_does_not_repeat_the_original_message() {
        let dir = tempfile::tempdir().unwrap();
        let db = crate::db::Db::open(&dir.path().join("s.db")).unwrap();
        db.upsert_task("acme-3", "/repo", Some(3)).unwrap();
        db.enqueue_message("acme-3", "please fix the bug", "test")
            .unwrap();
        db.enqueue_message("acme-3", "also check the tests", "test")
            .unwrap();
        crate::worktree::init_sample_repo(dir.path()).unwrap();
        let cfg_path = dir.path().join("cfg.toml");
        std::fs::write(
            &cfg_path,
            format!(
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
cli = "/bin/true"
"#,
                db = dir.path().join("s.db").display(),
                root = dir.path().display(),
            ),
        )
        .unwrap();
        let cfg = crate::config::load_config(&cfg_path).unwrap();
        let task = db.get_task("acme-3").unwrap().unwrap();
        let brief = build_for_switch(
            &cfg,
            &db,
            "acme-3",
            &task,
            dir.path(),
            "claude",
            "stalled",
            None,
            None,
        )
        .unwrap();
        let orig_count = brief.matches("please fix the bug").count();
        assert_eq!(
            orig_count, 1,
            "original must appear once, not again under Latest:\n{brief}"
        );
        assert!(brief.contains("also check the tests"), "{brief}");
        assert!(brief.contains("Original:"), "{brief}");
    }
}

#[cfg(test)]
mod grok_stream_tests {
    use super::*;

    fn ev(raw: &str) -> EventRow {
        EventRow {
            run_id: 1,
            kind: String::new(),
            raw: raw.to_string(),
        }
    }

    #[test]
    fn codex_agent_message_is_the_answer() {
        let events = vec![
            ev(r#"{"type":"thread.started","thread_id":"t1"}"#),
            ev(
                r#"{"type":"item.completed","item":{"id":"item_0","type":"error","message":"Codex is ignoring 1 unrecognized configuration setting."}}"#,
            ),
            ev(
                r#"{"type":"item.completed","item":{"id":"item_1","type":"agent_message","text":"I'll check the repo first."}}"#,
            ),
            ev(
                r#"{"type":"item.completed","item":{"id":"item_2","type":"command_execution","command":"pytest -q","aggregated_output":"ok"}}"#,
            ),
            ev(
                r#"{"type":"item.completed","item":{"id":"item_3","type":"agent_message","text":"Thanks for reporting this."}}"#,
            ),
            ev(r#"{"type":"turn.completed","usage":{"input_tokens":1,"output_tokens":2}}"#),
        ];
        assert_eq!(
            last_assistant_text(&events).as_deref(),
            Some("Thanks for reporting this.")
        );
    }

    #[test]
    fn joins_grok_text_deltas() {
        let events = vec![
            ev(r#"{"type":"thought","data":"thinking"}"#),
            ev(r#"{"type":"tool_call","name":"bash"}"#),
            ev(r#"{"type":"text","data":"O"}"#),
            ev(r#"{"type":"thought","data":"x"}"#),
            ev(r#"{"type":"text","data":"K"}"#),
            ev(r#"{"type":"usage","usage":{"input_tokens":1}}"#),
            ev(r#"{"type":"end","usage":{"input_tokens":1}}"#),
        ];
        assert_eq!(last_assistant_text(&events).as_deref(), Some("OK"));
    }

    #[test]
    fn non_grok_events_fall_through() {
        let events = vec![ev(
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"hi"}]}}"#,
        )];
        assert_eq!(last_assistant_text(&events).as_deref(), Some("hi"));
    }
}
