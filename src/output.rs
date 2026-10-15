//! Per-task result capture and delivery.

use crate::app::Shared;
use crate::db::Task;
use crate::task::{OutputKind, SourceKind, WorkspaceKind};
use anyhow::Result;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::Command;

pub fn result_payload(shared: &Shared, task: &Task) -> Value {
    let text = if task.result_text.trim().is_empty() {
        last_text(shared, &task.key)
    } else {
        task.result_text.clone()
    };
    let diff = if task.result_diff.trim().is_empty() {
        task.worktree
            .as_deref()
            .map(|p| git_diff(Path::new(p)))
            .unwrap_or_default()
    } else {
        task.result_diff.clone()
    };
    json!({
        "key": task.key,
        "status": task.status,
        "title": task.title,
        "text": text,
        "diff": diff,
        "pr_url": task.result_pr_url,
        "workspace": task.workspace_kind,
        "output": task.output_kind,
        "source": task.source_kind,
        "repo": task.repo,
        "issue": task.issue,
        "worktree": task.worktree,
        "provider": task.provider,
    })
}

pub fn last_text(shared: &Shared, key: &str) -> String {
    let runs = shared.db.list_runs(Some(key)).unwrap_or_default();
    let Some(last) = runs.last() else {
        return String::new();
    };
    let events = shared.db.events_for_run(last.id).unwrap_or_default();
    crate::handoff::last_assistant_text(&events).unwrap_or_default()
}

pub fn git_diff(wt: &Path) -> String {
    if !crate::worktree::is_git_repo(wt) {
        return String::new();
    }
    let staged = Command::new("git")
        .args(["diff", "HEAD"])
        .current_dir(wt)
        .output();
    match staged {
        Ok(o) => {
            let mut s = String::from_utf8_lossy(&o.stdout).into_owned();
            if s.len() > 80_000 {
                s.truncate(80_000);
                s.push_str("\n…truncated…\n");
            }
            s
        }
        Err(_) => String::new(),
    }
}

pub fn capture(shared: &Shared, key: &str) -> Result<Value> {
    let Some(task) = shared.db.get_task(key)? else {
        anyhow::bail!("unknown task {key}");
    };
    let text = last_text(shared, key);
    let diff = task
        .worktree
        .as_deref()
        .map(|p| git_diff(Path::new(p)))
        .unwrap_or_default();
    shared.db.set_task_result(key, &text, &diff, None)?;
    let mut task = task;
    task.result_text = text;
    task.result_diff = diff;
    Ok(result_payload(shared, &task))
}

pub fn finalize(shared: &Shared, key: &str) -> Result<()> {
    let payload = capture(shared, key)?;
    let Some(task) = shared.db.get_task(key)? else {
        return Ok(());
    };
    if task.shadow {
        return Ok(());
    }
    let output = OutputKind::parse(&task.output_kind).unwrap_or(OutputKind::Reply);
    let source = SourceKind::from_message_source(&task.source_kind);
    let workspace = WorkspaceKind::parse(&task.workspace_kind)
        .unwrap_or_else(|_| crate::task::infer_workspace_kind(task.repo.as_deref().unwrap_or("")));
    match output {
        OutputKind::Reply => deliver_reply(shared, &task, source, &payload),
        OutputKind::Text => Ok(()),
        OutputKind::Webhook => post_webhook(&task.output_spec, &payload),
        OutputKind::Patch => write_patch(&task, workspace, &payload),
        OutputKind::Branch => push_branch(&task),
        OutputKind::PullRequest => open_pr(shared, &task, &payload),
        OutputKind::IssueComment => issue_comment(shared, &task, &payload),
    }
}

fn deliver_reply(shared: &Shared, task: &Task, source: SourceKind, payload: &Value) -> Result<()> {
    match source {
        SourceKind::Github => Ok(()),
        SourceKind::Telegram => {
            let cfg = shared.config.read().unwrap().telegram.clone();
            let text = payload.get("text").and_then(|v| v.as_str()).unwrap_or("");
            let body = if text.trim().is_empty() {
                format!("finished {}", task.key)
            } else {
                let mut s = format!("{}: {text}", task.key);
                if s.chars().count() > 3500 {
                    s = format!("{}…", s.chars().take(3499).collect::<String>());
                }
                s
            };
            crate::notify::notify_unless_shadow(&cfg, &body, task.shadow);
            Ok(())
        }
        SourceKind::Inbox => {
            let spec = task.output_spec.trim();
            if !spec.is_empty() {
                crate::inbox::write_result_file(Path::new(spec), &task.key, payload);
            }
            Ok(())
        }
        SourceKind::Mcp | SourceKind::Cli | SourceKind::Http => Ok(()),
    }
}

fn post_webhook(url: &str, payload: &Value) -> Result<()> {
    let url = url.trim();
    if url.is_empty() {
        anyhow::bail!("webhook output needs a URL");
    }
    match ureq::post(url)
        .set("User-Agent", "atr")
        .set("Content-Type", "application/json")
        .send_json(payload.clone())
    {
        Ok(_) => Ok(()),
        Err(e) => {
            tracing::warn!("webhook {url} failed: {e}");
            Ok(())
        }
    }
}

fn write_patch(task: &Task, workspace: WorkspaceKind, payload: &Value) -> Result<()> {
    if workspace != WorkspaceKind::Git {
        anyhow::bail!("patch output needs a git workspace");
    }
    let diff = payload.get("diff").and_then(|v| v.as_str()).unwrap_or("");
    let path = patch_path(task);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    std::fs::write(&path, diff)?;
    tracing::info!(task = %task.key, path = %path.display(), "wrote patch");
    Ok(())
}

fn patch_path(task: &Task) -> PathBuf {
    let spec = task.output_spec.trim();
    if spec.is_empty() {
        return PathBuf::from(format!("{}.patch", task.key));
    }
    let p = PathBuf::from(spec);
    if p.is_dir() || spec.ends_with('/') || p.extension().is_none() {
        p.join(format!("{}.patch", task.key))
    } else {
        p
    }
}

fn push_branch(task: &Task) -> Result<()> {
    let Some(wt) = task.worktree.as_deref().map(Path::new) else {
        anyhow::bail!("no worktree to push");
    };
    if !crate::worktree::is_git_repo(wt) {
        anyhow::bail!("branch output needs a git worktree");
    }
    let status = Command::new("git")
        .args(["push", "-u", "origin", "HEAD"])
        .current_dir(wt)
        .status();
    match status {
        Ok(s) if s.success() => Ok(()),
        Ok(s) => {
            tracing::warn!(task = %task.key, "git push failed ({s})");
            Ok(())
        }
        Err(e) => {
            tracing::warn!(task = %task.key, "git push: {e}");
            Ok(())
        }
    }
}

fn open_pr(shared: &Shared, task: &Task, payload: &Value) -> Result<()> {
    let _ = push_branch(task);
    let cfg = shared.config.read().unwrap().clone();
    let Some(wt) = task.worktree.as_deref() else {
        return Ok(());
    };
    let title = if task.title.trim().is_empty() {
        task.key.clone()
    } else {
        task.title.clone()
    };
    let body = payload.get("text").and_then(|v| v.as_str()).unwrap_or("");
    let gh = crate::shadow::resolve_github_cli(&cfg.github.cli)
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| cfg.github.cli.clone());
    let mut args = vec![
        "pr".into(),
        "create".into(),
        "--title".into(),
        title,
        "--body".into(),
        body.to_string(),
    ];
    if let Some(entry) = cfg.repo_for_task(task) {
        if !entry.github.is_empty() {
            args.push("--repo".into());
            args.push(entry.github);
        }
    }
    let out = Command::new(&gh).args(&args).current_dir(wt).output();
    match out {
        Ok(o) if o.status.success() => {
            let url = String::from_utf8_lossy(&o.stdout)
                .lines()
                .find(|l| l.contains("http"))
                .unwrap_or("")
                .trim()
                .to_string();
            if !url.is_empty() {
                let _ = shared.db.set_task_result(
                    &task.key,
                    &task.result_text,
                    &task.result_diff,
                    Some(&url),
                );
            }
        }
        Ok(o) => {
            tracing::warn!(
                task = %task.key,
                "gh pr create: {}",
                String::from_utf8_lossy(&o.stderr)
            );
        }
        Err(e) => tracing::warn!(task = %task.key, "gh pr create: {e}"),
    }
    Ok(())
}

fn issue_comment(shared: &Shared, task: &Task, payload: &Value) -> Result<()> {
    let Some(issue) = task.issue else {
        tracing::warn!(task = %task.key, "issue_comment output needs an issue number");
        return Ok(());
    };
    let cfg = shared.config.read().unwrap().clone();
    let gh = crate::shadow::resolve_github_cli(&cfg.github.cli)
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| cfg.github.cli.clone());
    let body = payload.get("text").and_then(|v| v.as_str()).unwrap_or("");
    let mut args = vec![
        "issue".into(),
        "comment".into(),
        issue.to_string(),
        "--body".into(),
        body.to_string(),
    ];
    if let Some(entry) = cfg.repo_for_task(task) {
        if !entry.github.is_empty() {
            args.push("--repo".into());
            args.push(entry.github);
        }
    }
    let status = Command::new(&gh).args(&args).status();
    if let Err(e) = status {
        tracing::warn!(task = %task.key, "gh issue comment: {e}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patch_path_from_dir_or_file() {
        let t = Task {
            key: "demo-1".into(),
            output_spec: "/tmp/patches".into(),
            ..Task::default()
        };
        assert_eq!(patch_path(&t), PathBuf::from("/tmp/patches/demo-1.patch"));
        let t = Task {
            key: "demo-1".into(),
            output_spec: "/tmp/out.patch".into(),
            ..Task::default()
        };
        assert_eq!(patch_path(&t), PathBuf::from("/tmp/out.patch"));
    }
}
