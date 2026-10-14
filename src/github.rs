use crate::app::Shared;
use anyhow::Result;
use serde::Deserialize;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;
use tokio::process::Command;

#[derive(Debug, Deserialize)]
struct Label {
    name: String,
}

#[derive(Debug, Deserialize)]
struct Issue {
    number: i64,
    title: String,
    body: Option<String>,
    #[serde(default)]
    labels: Vec<Label>,
}

#[derive(Debug, Deserialize)]
struct Comment {
    id: i64,
    body: Option<String>,
    user: Option<User>,
}

#[derive(Debug, Deserialize)]
struct User {
    login: Option<String>,
    #[serde(rename = "type")]
    kind: Option<String>,
}

pub async fn poll_loop(shared: Arc<Shared>) {
    loop {
        let (enabled, interval) = {
            let cfg = shared.config.read().unwrap();
            (cfg.github.enabled, cfg.github_poll_interval)
        };
        if enabled {
            if let Err(e) = poll_once(&shared).await {
                tracing::warn!("github poll failed: {e:#}");
            }
        }
        tokio::time::sleep(interval.max(Duration::from_secs(5))).await;
    }
}

pub async fn poll_once(shared: &Shared) -> Result<()> {
    if shared.intake_paused.load(Ordering::Relaxed) {
        tracing::debug!("intake paused; skip github poll");
        return Ok(());
    }
    let cfg = shared.config.read().unwrap().clone();
    if !cfg.github.enabled {
        return Ok(());
    }
    let targets: Vec<_> = cfg
        .enabled_watch_repos()
        .into_iter()
        .filter(|r| !r.github.is_empty() && !r.label.is_empty())
        .collect();
    if targets.is_empty() {
        return Ok(());
    }
    let gh = crate::shadow::resolve_github_cli(&cfg.github.cli)
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| cfg.github.cli.clone());
    for repo in targets {
        if let Err(e) = poll_repo(shared, &cfg, &gh, &repo).await {
            tracing::warn!(repo = %repo.github, "github poll failed: {e:#}");
        }
    }
    Ok(())
}

async fn poll_repo(
    shared: &Shared,
    cfg: &crate::config::Config,
    gh: &str,
    repo: &crate::config::RepoEntry,
) -> Result<()> {
    let github = repo.github.as_str();
    let label = repo.label.as_str();
    let repo_path = repo.path.display().to_string();

    let out = Command::new(gh)
        .args([
            "issue",
            "list",
            "--repo",
            github,
            "--label",
            label,
            "--state",
            "open",
            "--limit",
            "50",
            "--json",
            "number,title,body,labels",
        ])
        .output()
        .await?;
    if !out.status.success() {
        anyhow::bail!(
            "gh issue list failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let issues: Vec<Issue> = serde_json::from_slice(&out.stdout)?;
    for issue in issues {
        let key = cfg.issue_task_key(repo, issue.number);
        let existed = shared.db.get_task(&key)?.is_some();
        if !existed {
            let body = format!(
                "# {}\n\n{}\n\nSource: GitHub {github}#{}",
                issue.title,
                issue.body.unwrap_or_default(),
                issue.number
            );
            shared.submit(
                &key,
                &body,
                "github",
                Some(&repo_path),
                Some(issue.number),
                false,
                false,
            )?;
        }
        let names: Vec<_> = issue.labels.iter().map(|l| l.name.as_str()).collect();
        if !names.is_empty() {
            let _ = shared.db.set_task_labels(&key, &names.join(","));
        }
        if let Err(e) = ingest_comments(shared, gh, github, &key, issue.number).await {
            tracing::debug!(task = %key, "comment poll: {e:#}");
        }
    }
    Ok(())
}

async fn ingest_comments(
    shared: &Shared,
    gh: &str,
    repo: &str,
    key: &str,
    number: i64,
) -> Result<()> {
    let path = format!("repos/{repo}/issues/{number}/comments");
    let out = Command::new(gh)
        .args(["api", &path, "--paginate"])
        .output()
        .await?;
    if !out.status.success() {
        return Ok(());
    }
    let comments: Vec<Comment> = serde_json::from_slice(&out.stdout).unwrap_or_default();
    let cursor = shared
        .db
        .comment_cursor(key)?
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(0);
    let mut max_id = cursor;
    for c in comments {
        if c.id <= cursor {
            continue;
        }
        max_id = max_id.max(c.id);
        if c.user
            .as_ref()
            .and_then(|u| u.kind.as_deref())
            .is_some_and(|t| t.eq_ignore_ascii_case("bot"))
        {
            continue;
        }
        let login = c
            .user
            .as_ref()
            .and_then(|u| u.login.clone())
            .unwrap_or_else(|| "unknown".into());
        let body = c.body.unwrap_or_default();
        if body.trim().is_empty() {
            continue;
        }
        let text = format!("GitHub comment by {login}:\n{body}");
        shared.submit(
            key,
            &text,
            "github-comment",
            None,
            Some(number),
            false,
            false,
        )?;
    }
    if max_id > cursor {
        shared.db.set_comment_cursor(key, &max_id.to_string())?;
    }
    Ok(())
}
