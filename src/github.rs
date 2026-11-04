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
        let (probe, cfg) = {
            let cfg = shared.config.read().unwrap().clone();
            (enabled || cfg.shadow.enabled, cfg)
        };
        if probe {
            crate::gh_token::maybe_probe(&cfg, &shared.config_path).await;
        }
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
    let (owned, targets): (Vec<_>, Vec<_>) = cfg
        .enabled_watch_repos()
        .into_iter()
        .filter(|r| !r.github.is_empty() && !r.label.is_empty())
        .partition(|r| crate::shadow::owns_repo(&cfg, r));
    for r in &owned {
        tracing::info!(
            "github poll: skipping {}, owned by [shadow] (labels {})",
            r.github,
            r.label
        );
    }
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
    let repo_path = repo.path.display().to_string();

    // One list per label: gh ANDs repeated --label, and the label set means any.
    // A failed label does not hold back what the others found.
    let mut issues: Vec<Issue> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut failed = None;
    for label in repo.labels() {
        match list_issues(shared, cfg, gh, github, &label).await {
            Ok(found) => issues.extend(found.into_iter().filter(|i| seen.insert(i.number))),
            Err(e) => failed = Some(e),
        }
    }
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
            let labels: Vec<&str> = issue.labels.iter().map(|l| l.name.as_str()).collect();
            let needs = crate::task::needs_from_labels(&labels);
            let (provider_hint, model) = crate::task::label_overrides(cfg, &key, &labels, &needs);
            shared.submit_with_model(
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
                    provider_hint,
                    needs,
                    after: Vec::new(),
                },
                model.as_deref(),
            )?;
        }
        let names: Vec<_> = issue.labels.iter().map(|l| l.name.as_str()).collect();
        if !names.is_empty() {
            let _ = shared.db.set_task_labels(&key, &names.join(","));
        }
        if let Err(e) = ingest_comments(shared, cfg, gh, github, &key, issue.number).await {
            tracing::debug!(task = %key, "comment poll: {e:#}");
        }
    }
    match failed {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

async fn list_issues(
    shared: &Shared,
    cfg: &crate::config::Config,
    gh: &str,
    github: &str,
    label: &str,
) -> Result<Vec<Issue>> {
    let mut cmd = Command::new(gh);
    crate::gh_token::apply(&mut cmd, cfg, Some(&shared.config_path), Some(github));
    let out = cmd
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
    crate::gh_token::observe_gh(
        cfg,
        Some(&shared.config_path),
        Some(github),
        out.status.success(),
        &String::from_utf8_lossy(&out.stderr),
        &format!("issue list {github}"),
    );
    if !out.status.success() {
        anyhow::bail!(
            "gh issue list failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(serde_json::from_slice(&out.stdout)?)
}

async fn ingest_comments(
    shared: &Shared,
    cfg: &crate::config::Config,
    gh: &str,
    repo: &str,
    key: &str,
    number: i64,
) -> Result<()> {
    let path = format!("repos/{repo}/issues/{number}/comments");
    let mut cmd = Command::new(gh);
    crate::gh_token::apply(&mut cmd, cfg, Some(&shared.config_path), Some(repo));
    let out = cmd.args(["api", &path, "--paginate"]).output().await?;
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
            r#"[{"number":12,"title":"Fix login","body":"do the work","labels":[{"name":"for:atr"},{"name":"needs:browser"}]}]"#,
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
        assert_eq!(t.needs, "browser");
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

    const CODEX: &str = r#"
[[provider]]
name = "codex"
kind = "codex"
cli = "/bin/true"
"#;

    async fn poll_labels(dir: &std::path::Path, labels: &str, extra: &str) -> crate::db::Task {
        let fake = dir.join("fake");
        std::fs::create_dir_all(&fake).unwrap();
        std::fs::write(
            fake.join("gh-issues.json"),
            format!(
                r#"[{{"number":12,"title":"Fix login","body":"do the work","labels":[{labels}]}}]"#
            ),
        )
        .unwrap();
        let s = shared(dir, extra);
        poll_once(&s).await.unwrap();
        s.db.get_task("task-12").unwrap().expect("labelled issue")
    }

    #[tokio::test]
    async fn labels_set_provider_and_model_override() {
        let dir = tempfile::tempdir().unwrap();
        let t = poll_labels(
            dir.path(),
            r#"{"name":"for:atr"},{"name":"provider:codex"},{"name":"model:gpt-5.6-sol"}"#,
            CODEX,
        )
        .await;
        assert_eq!(t.provider_hint, "codex");
        assert_eq!(t.model_override, "gpt-5.6-sol");
    }

    #[tokio::test]
    async fn unknown_provider_label_is_ignored_without_failing_the_poll() {
        let dir = tempfile::tempdir().unwrap();
        let t = poll_labels(
            dir.path(),
            r#"{"name":"for:atr"},{"name":"provider:nope"},{"name":"model:gpt-5.6-sol"}"#,
            CODEX,
        )
        .await;
        assert_eq!(t.status, "queued");
        assert_eq!(t.provider_hint, "");
        assert_eq!(t.model_override, "");
    }

    #[tokio::test]
    async fn incapable_provider_label_is_dropped_and_needs_kept() {
        let dir = tempfile::tempdir().unwrap();
        let t = poll_labels(
            dir.path(),
            r#"{"name":"for:atr"},{"name":"needs:browser"},{"name":"provider:codex"},{"name":"model:gpt-5.6-sol"}"#,
            CODEX,
        )
        .await;
        assert_eq!(t.needs, "browser");
        assert_eq!(t.provider_hint, "");
        assert_eq!(t.model_override, "");
    }

    fn label_answer(dir: &std::path::Path, label: &str, issue: &str) {
        let by = dir.join("fake/gh-issues-by-label");
        std::fs::create_dir_all(&by).unwrap();
        std::fs::write(by.join(format!("{label}.json")), issue).unwrap();
    }

    fn issue_json(number: i64, labels: &[&str]) -> String {
        let names: Vec<String> = labels
            .iter()
            .map(|l| format!(r#"{{"name":"{l}"}}"#))
            .collect();
        format!(
            r#"[{{"number":{number},"title":"Fix login","body":"x","labels":[{}]}}]"#,
            names.join(",")
        )
    }

    fn repo_block(name: &str, github: &str, label: &str) -> String {
        format!(
            "[[repo]]\nname = \"{name}\"\npath = \"/tmp/{name}\"\ngithub = \"{github}\"\nlabel = \"{label}\"\n"
        )
    }

    fn task_keys(s: &Shared) -> Vec<String> {
        s.db.list_tasks()
            .unwrap()
            .into_iter()
            .map(|t| t.key)
            .collect()
    }

    fn gh_log(dir: &std::path::Path) -> String {
        std::fs::read_to_string(dir.join("fake/gh-invocations.log")).unwrap_or_default()
    }

    #[tokio::test]
    async fn issue_with_both_labels_becomes_one_task() {
        let dir = tempfile::tempdir().unwrap();
        let both = issue_json(12, &["for:atr", "for:agnt"]);
        label_answer(dir.path(), "for:atr", &both);
        label_answer(dir.path(), "for:agnt", &both);
        let s = shared(
            dir.path(),
            &repo_block("demo", "owner/demo", "for:atr, for:agnt"),
        );
        poll_once(&s).await.unwrap();
        assert_eq!(task_keys(&s), vec!["demo-12"]);
        let msgs = s.db.pending_messages("demo-12").unwrap();
        assert_eq!(msgs.iter().filter(|m| m.source == "github").count(), 1);
        // One list per label: gh would AND them on one call.
        let log = gh_log(dir.path());
        assert!(log.contains("--label for:atr --state"), "{log}");
        assert!(log.contains("--label for:agnt --state"), "{log}");
        assert!(!log.contains("for:atr,for:agnt"), "{log}");
    }

    #[tokio::test]
    async fn issue_with_only_the_second_label_is_picked_up() {
        let dir = tempfile::tempdir().unwrap();
        label_answer(dir.path(), "for:agnt", &issue_json(7, &["for:agnt"]));
        let s = shared(
            dir.path(),
            &repo_block("demo", "owner/demo", "for:atr,for:agnt"),
        );
        poll_once(&s).await.unwrap();
        assert_eq!(task_keys(&s), vec!["demo-7"]);
    }

    #[tokio::test]
    async fn single_label_ignores_an_issue_with_another_label() {
        let dir = tempfile::tempdir().unwrap();
        label_answer(dir.path(), "for:agnt", &issue_json(7, &["for:agnt"]));
        let s = shared(dir.path(), &repo_block("demo", "owner/demo", "for:atr"));
        poll_once(&s).await.unwrap();
        assert!(task_keys(&s).is_empty());
        let log = gh_log(dir.path());
        assert_eq!(log.matches("issue list").count(), 1, "{log}");
    }

    #[tokio::test]
    async fn shorthand_github_label_list_polls_each_label() {
        let dir = tempfile::tempdir().unwrap();
        label_answer(dir.path(), "for:agnt", &issue_json(7, &["for:agnt"]));
        let s = shared(dir.path(), "");
        s.config.write().unwrap().github.label = "for:atr, for:agnt".into();
        poll_once(&s).await.unwrap();
        assert_eq!(task_keys(&s), vec!["task-7"]);
    }

    #[tokio::test]
    async fn failed_label_keeps_what_the_others_found_and_reports_the_error() {
        let dir = tempfile::tempdir().unwrap();
        label_answer(dir.path(), "for:agnt", &issue_json(7, &["for:agnt"]));
        std::fs::write(dir.path().join("fake/gh-issues-by-label/for:atr.fail"), "").unwrap();
        let s = shared(
            dir.path(),
            &repo_block("demo", "owner/demo", "for:atr,for:agnt"),
        );
        let cfg = s.config.read().unwrap().clone();
        let gh = cfg.github.cli.clone();
        let repo = cfg.enabled_watch_repos().remove(0);
        let err = poll_repo(&s, &cfg, &gh, &repo).await.unwrap_err();
        assert!(err.to_string().contains("gh issue list failed"), "{err}");
        assert_eq!(task_keys(&s), vec!["demo-7"]);
    }

    fn shadow_block(enabled: bool, repo: &str) -> String {
        format!("[shadow]\nenabled = {enabled}\nrepo = \"{repo}\"\n")
    }

    fn write_issue(dir: &std::path::Path, number: i64) {
        let fake = dir.join("fake");
        std::fs::create_dir_all(&fake).unwrap();
        std::fs::write(
            fake.join("gh-issues.json"),
            issue_json(number, &["for:atr"]),
        )
        .unwrap();
    }

    #[tokio::test]
    async fn poller_skips_the_repo_shadow_owns_and_not_the_others() {
        let dir = tempfile::tempdir().unwrap();
        write_issue(dir.path(), 12);
        let extra = format!(
            "{}{}{}",
            shadow_block(true, "owner/demo"),
            repo_block("demo", "OWNER/Demo", "for:atr"),
            repo_block("other", "owner/other", "for:atr"),
        );
        let s = shared(dir.path(), &extra);
        poll_once(&s).await.unwrap();
        assert_eq!(task_keys(&s), vec!["other-12"]);
        let log = gh_log(dir.path());
        assert!(!log.to_ascii_lowercase().contains("owner/demo"), "{log}");
    }

    #[tokio::test]
    async fn poller_polls_the_repo_when_shadow_is_disabled() {
        let dir = tempfile::tempdir().unwrap();
        write_issue(dir.path(), 12);
        let extra = format!(
            "{}{}",
            shadow_block(false, "owner/demo"),
            repo_block("demo", "owner/demo", "for:atr"),
        );
        let s = shared(dir.path(), &extra);
        poll_once(&s).await.unwrap();
        assert_eq!(task_keys(&s), vec!["demo-12"]);
    }

    #[tokio::test]
    async fn poller_with_only_the_shadow_repo_creates_no_task() {
        let dir = tempfile::tempdir().unwrap();
        write_issue(dir.path(), 12);
        let extra = format!(
            "{}{}",
            shadow_block(true, "owner/demo"),
            repo_block("demo", "owner/demo", "for:atr"),
        );
        let s = shared(dir.path(), &extra);
        poll_once(&s).await.unwrap();
        assert!(task_keys(&s).is_empty());
    }

    #[tokio::test]
    async fn poller_keeps_the_shorthand_repo_when_shadow_is_on() {
        let dir = tempfile::tempdir().unwrap();
        write_issue(dir.path(), 12);
        let s = shared(dir.path(), &shadow_block(true, "owner/acme"));
        poll_once(&s).await.unwrap();
        assert_eq!(task_keys(&s), vec!["task-12"]);
    }
}
