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
                "#!/bin/sh\nprintf '%s\\n' \"$@\" >> '{}'\necho '---' >> '{}'\nif [ \"$1\" = pr ]; then echo 'https://github.com/o/r/pull/77'; fi\nexit 0\n",
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
    fn pr_output_runs_gh_pr_create_and_records_url() {
        let e = env();
        task(&e, "o-pr", "pr", "", "cli", "summary body");
        finalize(&e.shared, "o-pr").unwrap();
        let calls = gh_calls(&e);
        assert!(
            calls.starts_with("pr\ncreate\n--title\nFix the thing\n--body\n"),
            "{calls}"
        );
        assert!(calls.contains("summary body"), "{calls}");
        let t = e.shared.db.get_task("o-pr").unwrap().unwrap();
        assert_eq!(t.result_pr_url, "https://github.com/o/r/pull/77");
    }

    #[test]
    fn issue_comment_output_comments_on_issue() {
        let e = env();
        task(&e, "o-ic", "issue_comment", "", "cli", "the answer");
        finalize(&e.shared, "o-ic").unwrap();
        let calls = gh_calls(&e);
        assert!(calls.starts_with("issue\ncomment\n5\n--body\n"), "{calls}");
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
}
