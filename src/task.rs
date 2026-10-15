//! Generic task model: workspace, output, and intake source.
//!
//! A task is a prompt plus an optional workspace. Issue numbers are optional.
//! GitHub-label intake is one source; it still keys tasks as `{prefix}-{issue}`.

use anyhow::{bail, Result};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkspaceKind {
    /// Git repo: worktree / branch / optional PR.
    Git,
    /// Local directory: edit in place, stay inside the dir.
    Dir,
    /// No files: answer in text only.
    None,
}

impl WorkspaceKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Git => "git",
            Self::Dir => "dir",
            Self::None => "none",
        }
    }

    pub fn parse(raw: &str) -> Result<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "" | "git" | "repo" => Ok(Self::Git),
            "dir" | "directory" | "path" | "local" => Ok(Self::Dir),
            "none" | "no" | "answer" | "text" => Ok(Self::None),
            other => bail!("workspace {other:?} must be git, dir, or none"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputKind {
    /// Reply to the intake source (GitHub comment+PR via the agent prompt,
    /// Telegram message, MCP/CLI/HTTP stored result, inbox result file).
    Reply,
    PullRequest,
    IssueComment,
    Branch,
    Patch,
    Text,
    Webhook,
}

impl OutputKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Reply => "reply",
            Self::PullRequest => "pr",
            Self::IssueComment => "issue_comment",
            Self::Branch => "branch",
            Self::Patch => "patch",
            Self::Text => "text",
            Self::Webhook => "webhook",
        }
    }

    pub fn parse(raw: &str) -> Result<Self> {
        match raw.trim().to_ascii_lowercase().replace('-', "_").as_str() {
            "" | "reply" | "source" | "default" => Ok(Self::Reply),
            "pr" | "pull_request" | "pullrequest" => Ok(Self::PullRequest),
            "issue_comment" | "comment" | "issue" => Ok(Self::IssueComment),
            "branch" | "branch_only" => Ok(Self::Branch),
            "patch" | "patch_file" | "diff" => Ok(Self::Patch),
            "text" | "answer" => Ok(Self::Text),
            "webhook" | "callback" => Ok(Self::Webhook),
            other => bail!(
                "output {other:?} must be reply, pr, issue_comment, branch, patch, text, or webhook"
            ),
        }
    }

    pub fn needs_git(self) -> bool {
        matches!(self, Self::PullRequest | Self::Branch | Self::Patch)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceKind {
    Github,
    Mcp,
    Cli,
    Http,
    Telegram,
    Inbox,
}

impl SourceKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Github => "github",
            Self::Mcp => "mcp",
            Self::Cli => "cli",
            Self::Http => "http",
            Self::Telegram => "telegram",
            Self::Inbox => "inbox",
        }
    }

    pub fn parse(raw: &str) -> Result<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "github" | "github-comment" | "gh" => Ok(Self::Github),
            "mcp" => Ok(Self::Mcp),
            "cli" | "submit" | "add" => Ok(Self::Cli),
            "http" | "api" => Ok(Self::Http),
            "telegram" | "tg" => Ok(Self::Telegram),
            "inbox" | "file" => Ok(Self::Inbox),
            other => bail!("source {other:?} must be github, mcp, cli, http, telegram, or inbox"),
        }
    }

    pub fn from_message_source(raw: &str) -> Self {
        Self::parse(raw).unwrap_or(Self::Cli)
    }
}

pub fn infer_workspace_kind(repo: &str) -> WorkspaceKind {
    if repo.trim().is_empty() {
        WorkspaceKind::None
    } else {
        WorkspaceKind::Git
    }
}

pub fn infer_source_kind(issue: Option<i64>, message_source: &str) -> SourceKind {
    if issue.is_some() && message_source.starts_with("github") {
        return SourceKind::Github;
    }
    SourceKind::from_message_source(message_source)
}

pub fn slugify(title: &str) -> String {
    let mut out = String::new();
    let mut dash = false;
    for c in title.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            dash = false;
        } else if !dash && !out.is_empty() {
            out.push('-');
            dash = true;
        }
    }
    let s = out.trim_matches('-');
    let s: String = s.chars().take(40).collect();
    if s.is_empty() {
        "task".into()
    } else {
        s
    }
}

pub fn allocate_key(db: &crate::db::Db, prefix: &str, title: &str) -> Result<String> {
    let prefix = if prefix.trim().is_empty() {
        "task"
    } else {
        prefix.trim()
    };
    let slug = slugify(title);
    let base = format!("{prefix}-{slug}");
    if db.get_task(&base)?.is_none() {
        return Ok(base);
    }
    for i in 2..100 {
        let k = format!("{base}-{i}");
        if db.get_task(&k)?.is_none() {
            return Ok(k);
        }
    }
    bail!("could not allocate a unique task key")
}

pub fn resolve_dir(raw: &str) -> Result<PathBuf> {
    let p = crate::config::expand_path(raw);
    if p.as_os_str().is_empty() {
        bail!("directory workspace path is empty");
    }
    if !p.exists() {
        bail!("directory {} does not exist", p.display());
    }
    if !p.is_dir() {
        bail!("{} is not a directory", p.display());
    }
    Ok(p.canonicalize().unwrap_or(p))
}

pub fn resolve_git_repo(raw: &str) -> Result<PathBuf> {
    let p = crate::config::expand_path(raw);
    if p.as_os_str().is_empty() {
        bail!("git workspace path is empty");
    }
    if !crate::worktree::is_git_repo(&p) {
        bail!("{} is not a git repository", p.display());
    }
    Ok(p)
}

pub fn path_inside(root: &Path, child: &Path) -> bool {
    let Ok(root) = root.canonicalize() else {
        return false;
    };
    let child = child.canonicalize().unwrap_or_else(|_| child.to_path_buf());
    child == root || child.starts_with(&root)
}

pub fn workspace_note(kind: WorkspaceKind, path: &str, output: OutputKind) -> Option<String> {
    let mut parts = Vec::new();
    match kind {
        WorkspaceKind::Git => {}
        WorkspaceKind::Dir => {
            parts.push(format!(
                "## Workspace\nThis is a local directory, not a git worktree. You may edit files. \
Stay inside {path}. Do not read or write files outside that directory. Do not `cd` elsewhere.\n"
            ));
        }
        WorkspaceKind::None => {
            parts.push(
                "## Workspace\nThis task has no workspace. Answer in text only. \
Do not create, edit, or delete files.\n"
                    .into(),
            );
        }
    }
    match output {
        OutputKind::Reply => {}
        OutputKind::Text => {
            parts.push(
                "## Output\nReturn a text answer. Do not open a pull request or comment on GitHub \
unless the messages ask you to.\n"
                    .into(),
            );
        }
        OutputKind::PullRequest => {
            parts.push(
                "## Output\nFinish by leaving a reviewable branch. atr will open the pull request.\n"
                    .into(),
            );
        }
        OutputKind::IssueComment => {
            parts.push(
                "## Output\natr will post the result as a GitHub issue comment. Do not run `gh issue comment` yourself.\n"
                    .into(),
            );
        }
        OutputKind::Branch => {
            parts.push(
                "## Output\nCommit on the task branch. atr will push it. Do not open a pull request.\n"
                    .into(),
            );
        }
        OutputKind::Patch => {
            parts.push(
                "## Output\nEdit files as needed. atr will write a patch file of the diff. Do not `git push`.\n"
                    .into(),
            );
        }
        OutputKind::Webhook => {
            parts.push(
                "## Output\nComplete the work. atr will POST the result to the configured webhook.\n"
                    .into(),
            );
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("\n"))
    }
}

pub fn insert_before_messages(rendered: &str, note: &str) -> String {
    let note = note.trim();
    if note.is_empty() {
        return rendered.to_string();
    }
    if let Some(idx) = rendered.find("\n## Messages\n") {
        format!("{}\n{}\n{}", &rendered[..idx], note, &rendered[idx + 1..])
    } else {
        format!("{rendered}\n{note}\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_aliases() {
        assert_eq!(WorkspaceKind::parse("DIR").unwrap(), WorkspaceKind::Dir);
        assert_eq!(WorkspaceKind::parse("none").unwrap(), WorkspaceKind::None);
        assert_eq!(
            OutputKind::parse("pull-request").unwrap(),
            OutputKind::PullRequest
        );
        assert_eq!(
            OutputKind::parse("issue-comment").unwrap(),
            OutputKind::IssueComment
        );
        assert_eq!(
            SourceKind::parse("github-comment").unwrap(),
            SourceKind::Github
        );
        assert!(OutputKind::PullRequest.needs_git());
        assert!(!OutputKind::Text.needs_git());
    }

    #[test]
    fn slug_and_notes() {
        assert_eq!(slugify("Fix Login!"), "fix-login");
        assert_eq!(slugify("..."), "task");
        let n = workspace_note(WorkspaceKind::None, "", OutputKind::Text).unwrap();
        assert!(n.contains("no workspace"));
        let rendered = "head\n## Messages\nbody\n";
        let out = insert_before_messages(rendered, "## Workspace\nstay in /tmp\n");
        assert!(out.contains("stay in /tmp"));
        assert!(out.find("Workspace").unwrap() < out.find("Messages").unwrap());
    }
}
