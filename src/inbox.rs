//! Inbox directory source: TOML or JSON task files dropped in a folder.

use crate::app::{Shared, SubmitOpts};
use crate::task::{self, OutputKind, SourceKind, WorkspaceKind};
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

#[derive(Debug, Deserialize, Default)]
struct InboxFile {
    #[serde(default)]
    prompt: String,
    #[serde(default, alias = "body", alias = "text", alias = "message")]
    body: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    key: String,
    #[serde(default)]
    dir: String,
    #[serde(default)]
    repo: String,
    #[serde(default)]
    workspace: String,
    #[serde(default)]
    output: String,
    #[serde(default)]
    webhook: String,
    #[serde(default)]
    patch: String,
    #[serde(default)]
    provider: String,
    #[serde(default)]
    shadow: bool,
}

pub async fn poll_loop(shared: Arc<Shared>) {
    loop {
        let interval = {
            let cfg = shared.config.read().unwrap();
            cfg.inbox_sources()
                .iter()
                .map(|s| s.poll_interval)
                .min()
                .unwrap_or(Duration::from_secs(5))
                .max(Duration::from_secs(1))
        };
        if let Err(e) = poll_once(&shared) {
            tracing::warn!("inbox poll failed: {e:#}");
        }
        tokio::time::sleep(interval).await;
    }
}

pub fn poll_once(shared: &Shared) -> Result<()> {
    if shared.intake_paused.load(Ordering::Relaxed) {
        tracing::debug!("intake paused; skip inbox poll");
        return Ok(());
    }
    let cfg = shared.config.read().unwrap().clone();
    if !cfg.intake_allows(SourceKind::Inbox) {
        return Ok(());
    }
    for src in cfg.inbox_sources() {
        if src.dir.as_os_str().is_empty() {
            continue;
        }
        let max_bytes = if src.max_bytes > 0 {
            src.max_bytes
        } else {
            cfg.max_prompt_bytes
        };
        if let Err(e) = poll_dir(shared, &src.dir, max_bytes) {
            tracing::warn!(dir = %src.dir.display(), "inbox dir: {e:#}");
        }
    }
    Ok(())
}

fn poll_dir(shared: &Shared, dir: &Path, max_bytes: u64) -> Result<()> {
    fs::create_dir_all(dir).with_context(|| format!("create inbox {}", dir.display()))?;
    fs::create_dir_all(dir.join("accepted"))?;
    fs::create_dir_all(dir.join("failed"))?;
    fs::create_dir_all(dir.join("results"))?;
    let mut entries: Vec<PathBuf> = fs::read_dir(dir)
        .with_context(|| format!("read inbox {}", dir.display()))?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_file())
        .filter(|p| {
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
            !name.starts_with('.')
                && (name.ends_with(".toml") || name.ends_with(".json") || name.ends_with(".tml"))
        })
        .collect();
    entries.sort();
    for path in entries {
        match ingest_file(shared, dir, &path, max_bytes) {
            Ok(key) => {
                let dest = unique_dest(&dir.join("accepted"), &path);
                if let Err(e) = fs::rename(&path, &dest) {
                    tracing::warn!(file = %path.display(), "inbox move accepted: {e:#}");
                }
                tracing::info!(task = %key, file = %path.display(), "inbox accepted");
            }
            Err(e) => {
                tracing::warn!(file = %path.display(), "inbox reject: {e:#}");
                let dest = unique_dest(&dir.join("failed"), &path);
                let _ = fs::rename(&path, &dest);
                let _ = fs::write(dest.with_extension("error.txt"), format!("{e:#}\n"));
            }
        }
    }
    Ok(())
}

fn unique_dest(dir: &Path, src: &Path) -> PathBuf {
    let name = src
        .file_name()
        .unwrap_or_else(|| std::ffi::OsStr::new("task"));
    let dest = dir.join(name);
    if !dest.exists() {
        return dest;
    }
    let stem = src.file_stem().and_then(|s| s.to_str()).unwrap_or("task");
    let ext = src.extension().and_then(|s| s.to_str()).unwrap_or("toml");
    for i in 2..1000 {
        let p = dir.join(format!("{stem}-{i}.{ext}"));
        if !p.exists() {
            return p;
        }
    }
    dir.join(format!("{stem}-{}.{}", crate::new_session_uuid(), ext))
}

fn ingest_file(shared: &Shared, inbox: &Path, path: &Path, max_bytes: u64) -> Result<String> {
    let meta = fs::metadata(path).with_context(|| format!("stat {}", path.display()))?;
    if max_bytes > 0 && meta.len() > max_bytes {
        bail!("inbox file is {} bytes; max is {max_bytes}", meta.len());
    }
    let text = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let spec = parse_inbox_file(path, &text)?;
    let prompt = if !spec.prompt.trim().is_empty() {
        spec.prompt.trim().to_string()
    } else {
        spec.body.trim().to_string()
    };
    if prompt.is_empty() {
        bail!("missing prompt");
    }
    let cfg = shared.config.read().unwrap().clone();
    task::check_prompt_bytes(&prompt, cfg.max_prompt_bytes.min(max_bytes))?;
    let title = if spec.title.trim().is_empty() {
        task::title_from_prompt(&prompt)
    } else {
        spec.title.trim().to_string()
    };
    let (workspace_kind, repo) = resolve_workspace(&cfg, &spec)?;
    let output = if spec.output.trim().is_empty() {
        OutputKind::Reply
    } else {
        OutputKind::parse(&spec.output)?
    };
    if output.needs_git() && workspace_kind != WorkspaceKind::Git {
        bail!("output {} needs a git workspace", output.as_str());
    }
    let output_spec = if !spec.webhook.trim().is_empty() {
        spec.webhook.trim().to_string()
    } else if !spec.patch.trim().is_empty() {
        spec.patch.trim().to_string()
    } else {
        inbox.join("results").display().to_string()
    };
    let key = if spec.key.trim().is_empty() {
        task::allocate_key(&shared.db, &cfg.task_key_prefix, &title)?
    } else {
        spec.key.trim().to_string()
    };
    let body = if title.is_empty() {
        prompt.clone()
    } else {
        format!("# {title}\n\n{prompt}")
    };
    let outcome = shared.submit_with(
        &key,
        &body,
        "inbox",
        repo.as_deref(),
        None,
        spec.shadow,
        false,
        SubmitOpts {
            workspace_kind: Some(workspace_kind.as_str().into()),
            output_kind: Some(output.as_str().into()),
            output_spec: Some(output_spec),
            source_kind: Some(SourceKind::Inbox.as_str().into()),
            title: Some(title),
            allow_empty_repo: workspace_kind == WorkspaceKind::None,
            provider_hint: Some(spec.provider.trim().to_string()).filter(|s| !s.is_empty()),
        },
    )?;
    let _ = outcome;
    Ok(key)
}

fn resolve_workspace(
    cfg: &crate::config::Config,
    spec: &InboxFile,
) -> Result<(WorkspaceKind, Option<String>)> {
    if !spec.dir.trim().is_empty() {
        let p = task::resolve_dir(&spec.dir)?;
        return Ok((WorkspaceKind::Dir, Some(p.display().to_string())));
    }
    if !spec.repo.trim().is_empty() {
        if let Some(entry) = cfg.resolve_repo_filter(spec.repo.trim()) {
            return Ok((WorkspaceKind::Git, Some(entry.path.display().to_string())));
        }
        let p = task::resolve_git_repo(&spec.repo)?;
        return Ok((WorkspaceKind::Git, Some(p.display().to_string())));
    }
    let kind = if spec.workspace.trim().is_empty() {
        WorkspaceKind::None
    } else {
        WorkspaceKind::parse(&spec.workspace)?
    };
    match kind {
        WorkspaceKind::None => Ok((WorkspaceKind::None, None)),
        WorkspaceKind::Dir => bail!("workspace dir requires dir = \"...\""),
        WorkspaceKind::Git => {
            if cfg.repo_path.as_os_str().is_empty() {
                bail!("workspace git requires repo = \"...\" or config repo_path");
            }
            Ok((
                WorkspaceKind::Git,
                Some(cfg.repo_path.display().to_string()),
            ))
        }
    }
}

fn parse_inbox_file(path: &Path, text: &str) -> Result<InboxFile> {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    if name.ends_with(".json") {
        serde_json::from_str(text).with_context(|| format!("parse JSON {}", path.display()))
    } else {
        toml::from_str(text).with_context(|| format!("parse TOML {}", path.display()))
    }
}

pub fn write_result_file(inbox_or_results: &Path, key: &str, payload: &serde_json::Value) {
    let dir = if inbox_or_results.ends_with("results") {
        inbox_or_results.to_path_buf()
    } else {
        inbox_or_results.join("results")
    };
    let _ = fs::create_dir_all(&dir);
    let path = dir.join(format!("{key}.json"));
    if let Ok(text) = serde_json::to_string_pretty(payload) {
        let _ = fs::write(path, text);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::load_config;
    use std::sync::Arc;

    fn shared(dir: &Path) -> Arc<Shared> {
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
repo_path = ""
system_prompt_file = "{prompt}"
display_timezone = "UTC"
[[source]]
kind = "inbox"
enabled = true
dir = "{inbox}"
[[provider]]
name = "claude"
kind = "claude"
cli = "/bin/true"
"#,
                db = dir.join("s.db").display(),
                root = dir.display(),
                prompt = prompt.display(),
                inbox = dir.join("inbox").display(),
            ),
        )
        .unwrap();
        let cfg = load_config(&cfg_path).unwrap();
        Shared::from_config(cfg_path, cfg).unwrap()
    }

    #[test]
    fn inbox_provider_is_saved_as_hint() {
        // Regression (#5 item 7) for inbox files.
        let tmp = tempfile::tempdir().unwrap();
        let s = shared(tmp.path());
        let inbox = tmp.path().join("inbox");
        std::fs::create_dir_all(&inbox).unwrap();
        std::fs::write(
            inbox.join("hinted.toml"),
            "prompt = \"hi\"\nprovider = \"grok\"\n",
        )
        .unwrap();
        std::fs::write(
            inbox.join("plain.toml"),
            "prompt = \"hi too\"\nprovider = \"  \"\n",
        )
        .unwrap();
        poll_once(&s).unwrap();
        let tasks = s.db.list_tasks().unwrap();
        assert_eq!(tasks.len(), 2, "{tasks:?}");
        let hints: Vec<_> = tasks.iter().map(|t| t.provider_hint.clone()).collect();
        assert!(hints.contains(&"grok".to_string()), "{hints:?}");
        assert!(
            hints.contains(&String::new()),
            "blank provider must not pin: {hints:?}"
        );
    }

    #[test]
    fn picks_up_toml_and_json_without_issue() {
        let tmp = tempfile::tempdir().unwrap();
        let s = shared(tmp.path());
        let inbox = tmp.path().join("inbox");
        std::fs::create_dir_all(&inbox).unwrap();
        std::fs::write(
            inbox.join("one.toml"),
            "prompt = \"what is 2+2?\"\noutput = \"text\"\n",
        )
        .unwrap();
        std::fs::write(
            inbox.join("two.json"),
            r#"{"prompt":"name the colors","title":"colors"}"#,
        )
        .unwrap();
        poll_once(&s).unwrap();
        let tasks = s.db.list_tasks().unwrap();
        assert_eq!(tasks.len(), 2, "{tasks:?}");
        assert!(tasks.iter().all(|t| t.issue.is_none()));
        assert!(tasks.iter().all(|t| t.workspace_kind == "none"));
        assert!(tasks.iter().all(|t| t.source_kind == "inbox"));
        assert!(!inbox.join("one.toml").exists());
        assert!(inbox.join("accepted").join("one.toml").exists());
    }

    #[test]
    fn dir_workspace_stays_inside_path() {
        let tmp = tempfile::tempdir().unwrap();
        let s = shared(tmp.path());
        let inbox = tmp.path().join("inbox");
        let work = tmp.path().join("notes");
        std::fs::create_dir_all(&inbox).unwrap();
        std::fs::create_dir_all(&work).unwrap();
        std::fs::write(
            inbox.join("edit.toml"),
            format!("prompt = \"fix the typo\"\ndir = \"{}\"\n", work.display()),
        )
        .unwrap();
        poll_once(&s).unwrap();
        let t = s.db.list_tasks().unwrap().pop().unwrap();
        assert_eq!(t.workspace_kind, "dir");
        assert_eq!(t.issue, None);
        let repo = t.repo.unwrap();
        assert!(task::path_inside(&work, Path::new(&repo)) || repo == work.display().to_string());
    }

    #[test]
    fn oversized_file_moves_to_failed() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg_path = tmp.path().join("config.toml");
        let prompt = tmp.path().join("p.md");
        std::fs::write(&prompt, "p\n").unwrap();
        std::fs::write(
            &cfg_path,
            format!(
                r#"
listen = "127.0.0.1:0"
state_db = "{db}"
worktree_root = "{root}"
repo_path = ""
system_prompt_file = "{prompt}"
display_timezone = "UTC"
max_prompt_bytes = "64"
[[source]]
kind = "inbox"
enabled = true
dir = "{inbox}"
max_bytes = "64"
[[provider]]
name = "claude"
kind = "claude"
cli = "/bin/true"
"#,
                db = tmp.path().join("s.db").display(),
                root = tmp.path().display(),
                prompt = prompt.display(),
                inbox = tmp.path().join("inbox").display(),
            ),
        )
        .unwrap();
        let cfg = load_config(&cfg_path).unwrap();
        let s = Shared::from_config(cfg_path, cfg).unwrap();
        let inbox = tmp.path().join("inbox");
        std::fs::create_dir_all(&inbox).unwrap();
        let body = format!("prompt = \"{}\"\n", "x".repeat(200));
        std::fs::write(inbox.join("huge.toml"), body).unwrap();
        poll_once(&s).unwrap();
        assert!(s.db.list_tasks().unwrap().is_empty());
        assert!(!inbox.join("huge.toml").exists());
        assert!(inbox.join("failed").join("huge.toml").exists());
        let err = std::fs::read_to_string(inbox.join("failed").join("huge.error.txt")).unwrap();
        assert!(err.contains("bytes"), "{err}");
    }
}
