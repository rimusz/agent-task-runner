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
            (
                cfg.intake_allows(crate::task::SourceKind::Github),
                cfg.github_poll_interval,
            )
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
    if !cfg.intake_allows(crate::task::SourceKind::Github) {
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
            shared.submit_with(
                &key,
                &body,
                "github",
                Some(&repo_path),
                Some(issue.number),
                false,
                false,
                crate::app::SubmitOpts {
                    workspace_kind: Some("git".into()),
                    output_kind: Some("reply".into()),
                    output_spec: None,
                    source_kind: Some("github".into()),
                    title: Some(issue.title.clone()),
                    allow_empty_repo: false,
                },
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::load_config;
    use std::sync::Arc;

    fn shared(dir: &std::path::Path, extra: &str) -> Arc<Shared> {
        let src =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake-gh.sh");
        let bin = dir.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::create_dir_all(dir.join("fake")).unwrap();
        let gh = bin.join("gh");
        std::fs::copy(&src, &gh).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let cfg_path = dir.join("config.toml");
        let prompt = dir.join("p.md");
        std::fs::write(&prompt, "p\n").unwrap();
        std::fs::write(
            &cfg_path,
            format!(
                r#"
listen = "127.0.0.1:0"
state_db = "{db}"
worktree_root = "{root}"
repo_path = "{root}/repo"
system_prompt_file = "{prompt}"
display_timezone = "UTC"
[github]
enabled = true
cli = "{gh}"
repo = "owner/acme"
label = "for:atr"
poll_interval = "60s"
{extra}
[[provider]]
name = "claude"
kind = "claude"
cli = "/bin/true"
"#,
                db = dir.join("s.db").display(),
                root = dir.display(),
                prompt = prompt.display(),
                gh = gh.display(),
            ),
        )
        .unwrap();
        let cfg = load_config(&cfg_path).unwrap();
        Shared::from_config(cfg_path, cfg).unwrap()
    }

    #[tokio::test]
    async fn label_poll_creates_task_and_comment_follow_up() {
        let dir = tempfile::tempdir().unwrap();
        let fake = dir.path().join("fake");
        std::fs::create_dir_all(&fake).unwrap();
        std::fs::write(
            fake.join("gh-issues.json"),
            r#"[{"number":12,"title":"Fix login","body":"do the work","labels":[{"name":"for:atr"}]}]"#,
        )
        .unwrap();
        std::fs::write(
            fake.join("gh-comments.json"),
            r#"[{"id":99,"body":"also tests","user":{"login":"alice","type":"User"}}]"#,
        )
        .unwrap();
        let s = shared(dir.path(), "");
        poll_once(&s).await.unwrap();
        let t = s.db.get_task("task-12").unwrap().expect("labelled issue");
        assert_eq!(t.issue, Some(12));
        assert_eq!(t.source_kind, "github");
        assert_eq!(t.workspace_kind, "git");
        assert_eq!(t.output_kind, "reply");
        let msgs = s.db.pending_messages("task-12").unwrap();
        assert!(msgs.iter().any(|m| m.source == "github"));
        assert!(
            msgs.iter().any(|m| m.source == "github-comment"),
            "{msgs:?}"
        );
    }

    #[tokio::test]
    async fn disabled_github_source_skips_poll() {
        let dir = tempfile::tempdir().unwrap();
        let fake = dir.path().join("fake");
        std::fs::create_dir_all(&fake).unwrap();
        std::fs::write(
            fake.join("gh-issues.json"),
            r#"[{"number":12,"title":"Fix login","body":"do the work","labels":[{"name":"for:atr"}]}]"#,
        )
        .unwrap();
        let s = shared(
            dir.path(),
            r#"
[[source]]
kind = "github"
enabled = false
"#,
        );
        poll_once(&s).await.unwrap();
        assert!(s.db.get_task("task-12").unwrap().is_none());
    }
}
