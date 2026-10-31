//! Per-task result capture and delivery.

use crate::app::Shared;
use crate::db::Task;
use crate::task::{OutputKind, SourceKind, WorkspaceKind};
use anyhow::Result;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

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
    let staged = crate::util::git_capture()
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
    capture_with(shared, key, None)
}

/// Like [`capture`], with the result text given instead of read from the
/// task's own last run (a compare parent has no runs of its own).
fn capture_with(shared: &Shared, key: &str, text: Option<&str>) -> Result<Value> {
    let Some(task) = shared.db.get_task(key)? else {
        anyhow::bail!("unknown task {key}");
    };
    let text = match text {
        Some(t) => t.to_string(),
        None => last_text(shared, key),
    };
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
    finalize_with_text(shared, key, None)
}

/// Deliver `key`'s output, using `text` as the result text when given.
pub fn finalize_with_text(shared: &Shared, key: &str, text: Option<&str>) -> Result<()> {
    let payload = capture_with(shared, key, text)?;
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
        OutputKind::Triage => record_triage(shared, &task, &payload),
    }
}

/// Base branch for a task's repo (`[[repo]] base`, else `repo_default_branch`).
pub fn base_branch(cfg: &crate::config::Config, task: &Task) -> String {
    cfg.repo_for_task(task)
        .map(|e| e.base)
        .filter(|b| !b.is_empty())
        .unwrap_or_else(|| cfg.repo_default_branch.clone())
}

fn git_out(wt: &Path, args: &[&str]) -> Option<String> {
    crate::util::git_capture()
        .args(args)
        .current_dir(wt)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
}

/// Uncommitted changes in the worktree (`.atr/` is excluded by git).
pub fn has_uncommitted(wt: &Path) -> bool {
    git_out(wt, &["status", "--porcelain", "--untracked-files=all"])
        .is_some_and(|s| !s.trim().is_empty())
}

/// Commits on HEAD that are not on `base`.
pub fn commits_ahead(wt: &Path, base: &str) -> u64 {
    git_out(wt, &["rev-list", "--count", &format!("{base}..HEAD")])
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
}

/// True when the worktree has nothing to deliver: no commits past `base`
/// and no uncommitted edits.
pub fn worktree_unchanged(wt: &Path, base: &str) -> bool {
    !has_uncommitted(wt) && commits_ahead(wt, base) == 0
}

/// Why a finished worker run did nothing (a no-op), or `None` when it
/// produced something to deliver. A no-op is never reported as success.
pub fn noop_reason(cfg: &crate::config::Config, task: &Task, text: &str) -> Option<String> {
    let output = OutputKind::parse(&task.output_kind).unwrap_or(OutputKind::Reply);
    let leak = if crate::triage::has_tool_markup(text) {
        " (the model printed a tool call as plain text instead of calling the tool)"
    } else {
        ""
    };
    match output {
        OutputKind::Triage => {
            let labels = crate::triage::parse_spec(&task.output_spec)
                .map(|s| s.labels)
                .unwrap_or_default();
            crate::triage::parse_suggestion(text, &labels)
                .err()
                .map(|e| format!("no usable triage suggestion: {e:#}{leak}"))
        }
        o if o.needs_git() => {
            let wt = task.worktree.as_deref().map(Path::new)?;
            if !crate::worktree::is_git_repo(wt) {
                return None;
            }
            let base = base_branch(cfg, task);
            if worktree_unchanged(wt, &base) {
                Some(format!(
                    "no file changes in the worktree (nothing past {base}){leak}"
                ))
            } else {
                None
            }
        }
        // Replies, text, and webhooks deliver whatever text there is; only
        // an issue comment has nothing to deliver without text.
        OutputKind::IssueComment => {
            if crate::triage::strip_tool_markup(text).trim().is_empty() {
                Some(format!("the model returned no answer text{leak}"))
            } else {
                None
            }
        }
        _ => None,
    }
}

/// The text atr stores as a no-op task's result.
pub fn noop_report(task: &Task, reason: &str, text: &str) -> String {
    let said = crate::triage::sanitize_comment(text);
    let said: String = said.chars().take(600).collect();
    let mut out = format!("NO-OP {}: {reason}. Nothing was delivered.", task.key);
    if !said.trim().is_empty() {
        out.push_str("\nLast model text: ");
        out.push_str(said.trim());
    }
    out
}

/// `git status` of the main checkout, taken before and after a worktree run
/// so atr notices when an agent escaped its worktree and edited the repo the
/// worktree was cut from. `None` when the folder is not a git checkout.
pub fn main_checkout_snapshot(repo: &Path) -> Option<String> {
    let head = git_out(repo, &["rev-parse", "HEAD"])?;
    let status = git_out(repo, &["status", "--porcelain", "--untracked-files=all"])?;
    Some(format!("HEAD {}\n{status}", head.trim()))
}

/// Paths whose status in the main checkout changed during the run (sorted).
pub fn main_checkout_changes(before: &str, after: &str) -> Vec<String> {
    let lines = |s: &str| -> std::collections::BTreeSet<String> {
        s.lines().skip(1).map(|l| l.to_string()).collect()
    };
    let (b, a) = (lines(before), lines(after));
    let mut out: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for l in a.symmetric_difference(&b) {
        let path = l.get(3..).unwrap_or(l).trim().to_string();
        if !path.is_empty() {
            out.insert(path);
        }
    }
    if before.lines().next() != after.lines().next() {
        out.insert("(HEAD moved)".into());
    }
    out.into_iter().collect()
}

/// Report for a run that touched the main checkout instead of its worktree.
pub fn main_checkout_report(task: &Task, repo: &Path, changed: &[String]) -> String {
    let mut list: Vec<&str> = changed.iter().take(20).map(String::as_str).collect();
    if changed.len() > 20 {
        list.push("...");
    }
    format!(
        "BLOCKED {}: the agent changed the main checkout {} (outside its worktree): {}. \
Nothing was delivered; review and restore those files there.",
        task.key,
        repo.display(),
        list.join(", ")
    )
}

/// Commit any uncommitted worktree edits on the task branch (the model
/// never commits). Returns true when a commit was made.
pub fn commit_pending(wt: &Path, task: &Task) -> Result<bool> {
    if !has_uncommitted(wt) {
        return Ok(false);
    }
    let add = crate::util::git_capture()
        .args(["add", "-A", "--", "."])
        .current_dir(wt)
        .output()?;
    if !add.status.success() {
        anyhow::bail!("git add: {}", String::from_utf8_lossy(&add.stderr).trim());
    }
    let title = if task.title.trim().is_empty() {
        task.key.clone()
    } else {
        task.title.trim().to_string()
    };
    let msg = format!("{title}\n\natr task {}", task.key);
    let mut commit = crate::util::git_capture();
    // Fall back to a neutral identity only when the repo has none.
    if git_out(wt, &["config", "user.email"]).is_none_or(|s| s.trim().is_empty()) {
        commit.args(["-c", "user.name=atr", "-c", "user.email=atr@localhost"]);
    }
    let out = commit
        .args(["commit", "--no-verify", "-q", "-F", "-"])
        .current_dir(wt)
        .stdin(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut c| {
            use std::io::Write;
            if let Some(mut w) = c.stdin.take() {
                w.write_all(msg.as_bytes())?;
            }
            c.wait_with_output()
        })?;
    if !out.status.success() {
        anyhow::bail!(
            "git commit: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(true)
}

/// The task's brief: its first message without atr's `# title` line.
fn task_brief(shared: &Shared, task: &Task) -> String {
    let first = shared
        .db
        .all_messages(&task.key)
        .ok()
        .and_then(|m| m.into_iter().next())
        .map(|m| m.body)
        .unwrap_or_default();
    let mut body = first.trim().to_string();
    if let Some(rest) = body.strip_prefix(&format!("# {}", task.title.trim())) {
        body = rest.trim().to_string();
    }
    body
}

fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(max - 1).collect::<String>())
    }
}

/// Insert a "Dropped scratch files" note before the PR body footer.
pub fn with_dropped_note(body: String, dropped: &[String]) -> String {
    note_before_footer(
        body,
        "Dropped scratch files",
        "atr removed these new files before committing (the task did not ask for them):",
        dropped,
    )
}

/// Insert a "Reverted unrequested edits" note before the PR body footer.
pub fn with_reverted_note(body: String, reverted: &[String]) -> String {
    note_before_footer(
        body,
        "Reverted unrequested edits",
        "atr restored these tracked files to the base version (the task did not ask for docs changes):",
        reverted,
    )
}

fn note_before_footer(body: String, heading: &str, intro: &str, paths: &[String]) -> String {
    if paths.is_empty() {
        return body;
    }
    let mut note = format!("\n## {heading}\n{intro}\n");
    for p in paths.iter().take(30) {
        note.push_str(&format!("- `{}`\n", p.replace('`', "")));
    }
    if paths.len() > 30 {
        note.push_str(&format!("- … and {} more\n", paths.len() - 30));
    }
    match body.rfind("\n---\n") {
        Some(i) => format!("{}{}{}", &body[..i], note, &body[i..]),
        None => body + &note,
    }
}

/// A non-empty PR description built from the task, the diff summary, and
/// the model's own summary (sanitized).
pub fn pr_body(task: &Task, brief: &str, diffstat: &str, model_text: &str) -> String {
    let mut out = String::from("## Summary\n");
    let title = task.title.trim();
    out.push_str(if title.is_empty() { &task.key } else { title });
    out.push('\n');
    if let Some(n) = task.issue {
        out.push_str(&format!("\nFixes #{n}.\n"));
    }
    let brief = crate::triage::sanitize_comment(brief);
    if !brief.trim().is_empty() {
        out.push_str("\n### Task\n");
        for l in clip(brief.trim(), 1500).lines() {
            out.push_str("> ");
            out.push_str(l);
            out.push('\n');
        }
    }
    out.push_str("\n## Changes\n");
    let stat = diffstat.trim();
    if stat.is_empty() {
        out.push_str("(no diff summary available)\n");
    } else {
        out.push_str("```text\n");
        out.push_str(&clip(stat, 3000));
        out.push_str("\n```\n");
    }
    out.push_str("\n## Agent notes\n");
    let notes = crate::triage::sanitize_comment(model_text);
    if notes.trim().is_empty() {
        out.push_str("(the agent left no summary)\n");
    } else {
        out.push_str(&clip(notes.trim(), 2500));
        out.push('\n');
    }
    out.push_str(&format!(
        "\n---\nOpened by atr for task `{}`{}. atr committed and pushed this branch; the agent only edited files.\n",
        task.key,
        task.provider
            .as_deref()
            .map(|p| format!(" (provider `{p}`)"))
            .unwrap_or_default()
    ));
    out
}

fn deliver_reply(shared: &Shared, task: &Task, source: SourceKind, payload: &Value) -> Result<()> {
    match source {
        SourceKind::Github => Ok(()),
        SourceKind::Telegram => {
            // Assistant chat turns are delivered (formatted) by
            // telegram::deliver_assistant_result; posting here too duplicated them.
            if crate::telegram::is_assistant_key(&task.key) {
                return Ok(());
            }
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
    match crate::util::ureq_agent(std::time::Duration::from_secs(10))
        .post(url)
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
    let status = crate::util::git_command()
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
    let cfg = shared.config.read().unwrap().clone();
    let Some(wt) = task.worktree.as_deref().map(Path::new) else {
        return Ok(());
    };
    let c = crate::scratch::clean_for_task(&cfg, &shared.db, &task.key, &task.title, wt);
    if !c.dropped.is_empty() {
        tracing::info!(task = %task.key, "dropped scratch before commit: {}", c.dropped.join(", "));
    }
    if !c.reverted.is_empty() {
        tracing::info!(task = %task.key, "reverted tracked docs before commit: {}", c.reverted.join(", "));
    }
    if let Err(e) = commit_pending(wt, task) {
        tracing::warn!(task = %task.key, "commit before PR: {e:#}");
    }
    let base = base_branch(&cfg, task);
    if commits_ahead(wt, &base) == 0 {
        tracing::warn!(task = %task.key, "no commits past {base}; not opening a PR");
        return Ok(());
    }
    let _ = push_branch(task);
    let title = if task.title.trim().is_empty() {
        task.key.clone()
    } else {
        task.title.clone()
    };
    let text = payload.get("text").and_then(|v| v.as_str()).unwrap_or("");
    let diffstat = git_out(wt, &["diff", "--stat", &format!("{base}...HEAD")]).unwrap_or_default();
    let mut body = pr_body(task, &task_brief(shared, task), &diffstat, text);
    body = with_dropped_note(body, &crate::scratch::recorded(wt));
    body = with_reverted_note(body, &crate::scratch::recorded_reverts(wt));
    let gh = crate::shadow::resolve_github_cli(&cfg.github.cli)
        .unwrap_or_else(|_| PathBuf::from(&cfg.github.cli));
    let mut args: Vec<String> = vec![
        "pr".into(),
        "create".into(),
        "--title".into(),
        title,
        "--body-file".into(),
        "-".into(),
        "--base".into(),
        base,
    ];
    if let Some(branch) = crate::worktree::current_branch(wt) {
        args.push("--head".into());
        args.push(branch);
    }
    if let Some(entry) = cfg.repo_for_task(task) {
        if !entry.github.is_empty() {
            args.push("--repo".into());
            args.push(entry.github);
        }
    }
    let out = std::process::Command::new(&gh)
        .args(&args)
        .current_dir(wt)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut c| {
            use std::io::Write;
            if let Some(mut w) = c.stdin.take() {
                w.write_all(body.as_bytes())?;
            }
            c.wait_with_output()
        });
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

/// Suggest-only triage: write the suggestion to the local review list.
fn record_triage(shared: &Shared, task: &Task, payload: &Value) -> Result<()> {
    let cfg = shared.config.read().unwrap().clone();
    let text = payload.get("text").and_then(|v| v.as_str()).unwrap_or("");
    let provider = task.provider.clone().unwrap_or_default();
    match crate::triage::record(&cfg, &task.key, &task.output_spec, text, &provider) {
        Ok(s) => tracing::info!(
            task = %task.key,
            label = %s.label,
            "triage suggestion recorded (pending approval; nothing posted)"
        ),
        Err(e) => tracing::warn!(task = %task.key, "triage suggestion: {e:#}"),
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
        .unwrap_or_else(|_| PathBuf::from(&cfg.github.cli));
    let body = payload.get("text").and_then(|v| v.as_str()).unwrap_or("");
    let repo = cfg
        .repo_for_task(task)
        .map(|e| e.github)
        .filter(|g| !g.is_empty());
    // Sanitized text on stdin (`--body-file -`); never a shell string.
    if let Err(e) = crate::triage::post_comment(&gh, repo.as_deref(), issue, "issue", body) {
        tracing::warn!(task = %task.key, "gh issue comment: {e:#}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    use crate::config::load_config;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Arc;

    struct Env {
        dir: tempfile::TempDir,
        shared: Arc<Shared>,
        repo: PathBuf,
        gh_log: PathBuf,
    }

    fn env() -> Env {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        let repo = d.join("repo");
        crate::worktree::init_sample_repo(&repo).unwrap();
        let gh_log = d.join("gh.log");
        let gh = d.join("gh");
        std::fs::write(
            &gh,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" >> '{}'\ncase \"$*\" in *--body-file*) cat >> '{}';; esac\necho '---' >> '{}'\nif [ \"$1\" = pr ]; then echo 'https://github.com/o/r/pull/77'; fi\nexit 0\n",
                gh_log.display(),
                gh_log.display(),
                gh_log.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(d.join("p.md"), "p\n").unwrap();
        let cfg_path = d.join("config.toml");
        std::fs::write(
            &cfg_path,
            format!(
                r#"
listen = "127.0.0.1:0"
state_db = "{d}/s.db"
worktree_root = "{d}/wt"
repo_path = "{repo}"
system_prompt_file = "{d}/p.md"
display_timezone = "UTC"
[github]
cli = "{gh}"
[[provider]]
name = "claude"
kind = "claude"
cli = "/bin/true"
"#,
                d = d.display(),
                repo = repo.display(),
                gh = gh.display()
            ),
        )
        .unwrap();
        let cfg = load_config(&cfg_path).unwrap();
        let shared = Shared::from_config(cfg_path, cfg).unwrap();
        Env {
            dir,
            shared,
            repo,
            gh_log,
        }
    }

    /// A task whose worktree is the sample repo with one edited file and a
    /// finished run that said `text`.
    fn task(e: &Env, key: &str, out: &str, spec: &str, source: &str, text: &str) {
        let db = &e.shared.db;
        db.upsert_task_spec(
            key,
            &e.repo.display().to_string(),
            Some(5),
            Some("git"),
            Some(out),
            Some(spec),
            Some(source),
        )
        .unwrap();
        db.set_worktree(key, &e.repo.display().to_string()).unwrap();
        db.set_task_title(key, "Fix the thing").unwrap();
        std::fs::write(e.repo.join("README.md"), "test repo\nedited\n").unwrap();
        let run = db.start_run(key, "claude", "m").unwrap();
        db.insert_event(
            run,
            "result",
            &json!({"type":"result","subtype":"success","result": text}).to_string(),
        )
        .unwrap();
        db.finish_run(run, "success", Some(0)).unwrap();
    }

    fn gh_calls(e: &Env) -> String {
        std::fs::read_to_string(&e.gh_log).unwrap_or_default()
    }

    #[test]
    fn assistant_telegram_reply_not_reposted_as_notification() {
        let e = env();
        e.shared.config.write().unwrap().telegram.enabled = true;
        let payload = json!({"text": "UNIQ-ASSISTANT-`x`"});
        task(&e, "tg873752741", "reply", "", "telegram", "hi");
        let at = e.shared.db.get_task("tg873752741").unwrap().unwrap();
        deliver_reply(&e.shared, &at, SourceKind::Telegram, &payload).unwrap();
        assert!(!crate::telegram::pending_notify_lines()
            .iter()
            .any(|l| l.contains("UNIQ-ASSISTANT")));
        task(&e, "o-tg", "reply", "", "telegram", "hi");
        let ot = e.shared.db.get_task("o-tg").unwrap().unwrap();
        let payload = json!({"text": "UNIQ-OTHER"});
        deliver_reply(&e.shared, &ot, SourceKind::Telegram, &payload).unwrap();
        assert!(crate::telegram::pending_notify_lines()
            .iter()
            .any(|l| l.contains("UNIQ-OTHER")));
    }

    #[test]
    fn capture_stores_text_and_diff() {
        let e = env();
        task(&e, "o-cap", "text", "", "cli", "ALL DONE");
        let v = capture(&e.shared, "o-cap").unwrap();
        assert_eq!(v["key"], "o-cap");
        assert!(v["text"].as_str().unwrap().contains("ALL DONE"), "{v}");
        assert!(v["diff"].as_str().unwrap().contains("+edited"), "{v}");
        let t = e.shared.db.get_task("o-cap").unwrap().unwrap();
        assert!(t.result_diff.contains("+edited"));
        assert!(capture(&e.shared, "missing").is_err());
        // Text output delivers nothing anywhere.
        finalize(&e.shared, "o-cap").unwrap();
        assert!(gh_calls(&e).is_empty());
    }

    #[test]
    fn patch_output_writes_diff_file() {
        let e = env();
        let out = e.dir.path().join("patches");
        task(
            &e,
            "o-patch",
            "patch",
            &format!("{}/", out.display()),
            "cli",
            "x",
        );
        finalize(&e.shared, "o-patch").unwrap();
        let patch = std::fs::read_to_string(out.join("o-patch.patch")).unwrap();
        assert!(patch.contains("+edited"), "{patch}");
    }

    #[test]
    fn patch_output_needs_git_workspace() {
        let t = Task {
            key: "p".into(),
            ..Task::default()
        };
        let err = write_patch(&t, WorkspaceKind::None, &json!({"diff": ""})).unwrap_err();
        assert!(err.to_string().contains("git workspace"));
    }

    #[test]
    fn pr_output_commits_then_opens_pr_with_description() {
        let e = env();
        task(&e, "o-pr", "pr", "", "cli", "summary body");
        e.shared
            .db
            .enqueue_message("o-pr", "# Fix the thing\n\nslugify doubles dashes", "cli")
            .unwrap();
        // The task branch, as atr's worktree would be (base is `main`).
        assert!(Command::new("git")
            .args(["checkout", "-q", "-b", "atr/o-pr"])
            .current_dir(&e.repo)
            .status()
            .unwrap()
            .success());
        assert!(has_uncommitted(&e.repo));
        finalize(&e.shared, "o-pr").unwrap();
        // atr committed the model's uncommitted edit itself.
        assert!(!has_uncommitted(&e.repo));
        assert_eq!(commits_ahead(&e.repo, "main"), 1);
        let calls = gh_calls(&e);
        assert!(
            calls.starts_with("pr\ncreate\n--title\nFix the thing\n--body-file\n-\n--base\nmain\n--head\natr/o-pr\n"),
            "{calls}"
        );
        // The description came in on stdin and is never empty.
        assert!(calls.contains("## Summary\nFix the thing"), "{calls}");
        assert!(calls.contains("> slugify doubles dashes"), "{calls}");
        assert!(calls.contains("## Changes"), "{calls}");
        assert!(calls.contains("README.md"), "{calls}");
        assert!(calls.contains("## Agent notes\nsummary body"), "{calls}");
        let t = e.shared.db.get_task("o-pr").unwrap().unwrap();
        assert_eq!(t.result_pr_url, "https://github.com/o/r/pull/77");
    }

    #[test]
    fn pr_output_with_nothing_to_deliver_opens_no_pr() {
        let e = env();
        task(&e, "o-pr0", "pr", "", "cli", "I already did it");
        assert!(Command::new("git")
            .args(["checkout", "-q", "-f", "-b", "atr/o-pr0"])
            .current_dir(&e.repo)
            .status()
            .unwrap()
            .success());
        assert!(Command::new("git")
            .args(["checkout", "-q", "--", "."])
            .current_dir(&e.repo)
            .status()
            .unwrap()
            .success());
        let t = e.shared.db.get_task("o-pr0").unwrap().unwrap();
        let cfg = e.shared.config.read().unwrap().clone();
        let why = noop_reason(&cfg, &t, "I already did it").unwrap();
        assert!(why.contains("no file changes"), "{why}");
        finalize(&e.shared, "o-pr0").unwrap();
        assert!(gh_calls(&e).is_empty());
    }

    #[test]
    fn pr_body_is_never_empty() {
        let t = Task {
            key: "k1".into(),
            ..Task::default()
        };
        let b = pr_body(&t, "", "", "");
        assert!(b.contains("## Summary\nk1"), "{b}");
        assert!(b.contains("(the agent left no summary)"), "{b}");
        let leak = pr_body(
            &t,
            "",
            "",
            "done <function=shell><parameter=command>ls</parameter></function>",
        );
        assert!(!leak.contains("<function="), "{leak}");
    }

    #[test]
    fn noop_reasons_by_output_kind() {
        let e = env();
        let cfg = e.shared.config.read().unwrap().clone();
        let mut t = Task {
            key: "n1".into(),
            output_kind: "issue_comment".into(),
            ..Task::default()
        };
        assert!(noop_reason(&cfg, &t, "an answer").is_none());
        let why = noop_reason(
            &cfg,
            &t,
            "<function=shell><parameter=command>gh issue view 3</parameter></function></tool_call>",
        )
        .unwrap();
        assert!(why.contains("tool call as plain text"), "{why}");
        t.output_kind = "triage".into();
        t.output_spec = crate::triage::format_spec("o/r", 3, &["bug".to_string()]);
        assert!(noop_reason(&cfg, &t, "{\"label\":\"bug\",\"comment\":\"real\"}").is_none());
        assert!(noop_reason(&cfg, &t, "I think it is a bug").is_some());
        let r = noop_report(&t, "no usable triage suggestion", "I think");
        assert!(
            r.starts_with("NO-OP n1: no usable triage suggestion. Nothing was delivered."),
            "{r}"
        );
    }

    #[test]
    fn triage_approve_posts_label_and_sanitized_comment_via_stdin() {
        let e = env();
        e.shared
            .config
            .write()
            .unwrap()
            .repos
            .push(crate::config::RepoEntry {
                name: "r".into(),
                path: e.repo.clone(),
                github: "o/r".into(),
                label: "for:atr".into(),
                enabled: true,
                branch_prefix: String::new(),
                base: "main".into(),
                system_prompt_file: PathBuf::new(),
                max_parallel: 1,
                lessons: None,
                shorthand: false,
            });
        let cfg = e.shared.config.read().unwrap().clone();
        let spec = crate::triage::format_spec("o/r", 6, &["duplicate".to_string()]);
        let text = "{\"label\":\"duplicate\",\"duplicate_of\":1,\"comment\":\"Same as #1.\\u001b[31m $(rm -rf ~) `x`\"}";
        crate::triage::record(&cfg, "t-6", &spec, text, "local").unwrap();
        assert!(gh_calls(&e).is_empty(), "record must post nothing");
        let s = crate::triage::approve(
            &cfg,
            "t-6",
            &crate::triage::ApproveOpts {
                label: None,
                comment: None,
                no_comment: false,
            },
        )
        .unwrap();
        assert_eq!(s.status, "approved");
        let calls = gh_calls(&e);
        assert!(
            calls.starts_with("issue\nedit\n6\n--repo\no/r\n--add-label\nduplicate\n---\n"),
            "{calls}"
        );
        assert!(
            calls.contains("issue\ncomment\n6\n--repo\no/r\n--body-file\n-\n"),
            "{calls}"
        );
        // Shell metacharacters arrive as inert text on stdin; escapes are gone.
        assert!(calls.contains("Same as #1."), "{calls}");
        assert!(!calls.contains('\u{1b}'), "{calls}");
        // Approving twice is refused.
        assert!(crate::triage::approve(
            &cfg,
            "t-6",
            &crate::triage::ApproveOpts {
                label: None,
                comment: None,
                no_comment: false
            },
        )
        .is_err());
    }

    #[test]
    fn triage_output_records_locally_and_posts_nothing() {
        let e = env();
        let spec =
            crate::triage::format_spec("o/r", 5, &["bug".to_string(), "duplicate".to_string()]);
        task(
            &e,
            "o-tri",
            "triage",
            &spec,
            "cli",
            "```json\n{\"label\":\"bug\",\"comment\":\"Reproduces on main.\"}\n```",
        );
        finalize(&e.shared, "o-tri").unwrap();
        assert!(gh_calls(&e).is_empty(), "triage must not post");
        let cfg = e.shared.config.read().unwrap().clone();
        let s = crate::triage::load(&crate::triage::store_dir(&cfg), "o-tri").unwrap();
        assert_eq!(s.status, "pending");
        assert_eq!(s.label, "bug");
        assert_eq!((s.repo.as_str(), s.issue), ("o/r", 5));
    }

    #[test]
    fn issue_comment_output_comments_on_issue() {
        let e = env();
        task(&e, "o-ic", "issue_comment", "", "cli", "the answer");
        finalize(&e.shared, "o-ic").unwrap();
        let calls = gh_calls(&e);
        assert!(
            calls.starts_with("issue\ncomment\n5\n--body-file\n-\n"),
            "{calls}"
        );
        assert!(calls.contains("the answer"), "{calls}");
    }

    #[test]
    fn issue_comment_without_issue_is_skipped() {
        let e = env();
        let t = Task {
            key: "o-noissue".into(),
            ..Task::default()
        };
        issue_comment(&e.shared, &t, &json!({"text": "x"})).unwrap();
        assert!(gh_calls(&e).is_empty());
    }

    #[test]
    fn branch_output_pushes_head_to_origin() {
        let e = env();
        let remote = e.dir.path().join("remote.git");
        let ok = Command::new("git")
            .args(["init", "-q", "--bare", &remote.display().to_string()])
            .status()
            .unwrap();
        assert!(ok.success());
        Command::new("git")
            .args(["remote", "add", "origin", &remote.display().to_string()])
            .current_dir(&e.repo)
            .status()
            .unwrap();
        task(&e, "o-br", "branch", "", "cli", "x");
        finalize(&e.shared, "o-br").unwrap();
        let out = Command::new("git")
            .args([
                "--git-dir",
                &remote.display().to_string(),
                "branch",
                "--list",
                "main",
            ])
            .output()
            .unwrap();
        assert!(String::from_utf8_lossy(&out.stdout).contains("main"));
        // No worktree: error, not a panic.
        let t = Task::default();
        assert!(push_branch(&t).is_err());
    }

    #[test]
    fn webhook_output_posts_payload() {
        use std::io::{Read, Write};
        let e = env();
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        let h = std::thread::spawn(move || {
            let (mut s, _) = l.accept().unwrap();
            s.set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            loop {
                match s.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        buf.extend_from_slice(&chunk[..n]);
                        let text = String::from_utf8_lossy(&buf);
                        if let Some(i) = text.find("\r\n\r\n") {
                            let len = text[..i]
                                .lines()
                                .find_map(|l| {
                                    l.to_ascii_lowercase()
                                        .strip_prefix("content-length:")
                                        .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                                })
                                .unwrap_or(0);
                            if buf.len() >= i + 4 + len {
                                break;
                            }
                        }
                    }
                }
            }
            let _ = s.write_all(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n");
            String::from_utf8_lossy(&buf).into_owned()
        });
        task(
            &e,
            "o-hook",
            "webhook",
            &format!("http://{addr}/hook"),
            "cli",
            "hooked",
        );
        finalize(&e.shared, "o-hook").unwrap();
        let req = h.join().unwrap();
        assert!(req.starts_with("POST /hook"), "{req}");
        assert!(req.contains("\"key\":\"o-hook\""), "{req}");
        assert!(req.contains("hooked"), "{req}");
        assert!(post_webhook("  ", &json!({})).is_err());
    }

    #[test]
    fn inbox_reply_writes_result_file() {
        let e = env();
        let inbox = e.dir.path().join("inbox");
        task(
            &e,
            "o-inbox",
            "reply",
            &inbox.display().to_string(),
            "inbox",
            "inbox answer",
        );
        finalize(&e.shared, "o-inbox").unwrap();
        let text = std::fs::read_to_string(inbox.join("results/o-inbox.json")).unwrap();
        assert!(text.contains("inbox answer"), "{text}");
    }

    #[test]
    fn shadow_task_delivers_nothing() {
        let e = env();
        task(&e, "o-shadow", "pr", "", "cli", "x");
        e.shared.db.mark_shadow("o-shadow").unwrap();
        finalize(&e.shared, "o-shadow").unwrap();
        assert!(gh_calls(&e).is_empty());
    }

    #[test]
    fn git_diff_is_capped_and_empty_outside_git() {
        let e = env();
        std::fs::write(e.repo.join("README.md"), "y\n".repeat(60_000)).unwrap();
        let d = git_diff(&e.repo);
        assert!(d.ends_with("…truncated…\n"));
        assert!(d.len() < 80_100);
        assert_eq!(git_diff(e.dir.path()), "");
    }

    #[test]
    fn webhook_closed_port_does_not_hang() {
        let start = std::time::Instant::now();
        post_webhook("http://127.0.0.1:1/hook", &json!({"ok": true})).unwrap();
        assert!(
            start.elapsed() < std::time::Duration::from_secs(5),
            "webhook POST to a closed port must fail fast"
        );
    }

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
    #[test]
    fn main_checkout_edits_are_detected() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        crate::worktree::init_sample_repo(&repo).unwrap();
        std::fs::write(repo.join("already-dirty.txt"), "x\n").unwrap();
        let before = main_checkout_snapshot(&repo).unwrap();
        assert!(main_checkout_changes(&before, &before).is_empty());
        std::fs::write(repo.join("README.md"), "edited by the agent\n").unwrap();
        std::fs::write(repo.join("scratch.py"), "print(1)\n").unwrap();
        let after = main_checkout_snapshot(&repo).unwrap();
        let changed = main_checkout_changes(&before, &after);
        assert_eq!(
            changed,
            vec!["README.md".to_string(), "scratch.py".to_string()]
        );
        let task = Task {
            key: "fix-1".into(),
            ..Default::default()
        };
        let r = main_checkout_report(&task, &repo, &changed);
        assert!(
            r.starts_with("BLOCKED fix-1: the agent changed the main checkout"),
            "{r}"
        );
        assert!(r.contains("README.md, scratch.py"), "{r}");
    }
}
