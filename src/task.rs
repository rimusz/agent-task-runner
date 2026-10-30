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

/// Default cap for prompts and inbox task files (256 KiB).
pub const DEFAULT_MAX_PROMPT_BYTES: u64 = 256 * 1024;

pub fn title_from_prompt(prompt: &str) -> String {
    prompt
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("task")
        .chars()
        .take(80)
        .collect()
}

pub fn check_prompt_bytes(text: &str, max: u64) -> Result<()> {
    let n = text.len() as u64;
    if max > 0 && n > max {
        anyhow::bail!("prompt is {n} bytes; max is {max}");
    }
    Ok(())
}

/// Resolve workspace from intake fields. `dir` wins; otherwise `workspace`
/// is honored even when the path was sent as `repo` (CLI HTTP client).
pub fn resolve_intake_workspace(
    dir: Option<&str>,
    repo: Option<&str>,
    workspace: Option<&str>,
    resolve_git: impl Fn(&str) -> Option<String>,
) -> Result<(WorkspaceKind, Option<String>, bool)> {
    let dir = dir.map(str::trim).filter(|s| !s.is_empty());
    let repo = repo.map(str::trim).filter(|s| !s.is_empty());
    let kind = match workspace.map(str::trim).filter(|s| !s.is_empty()) {
        Some(w) => Some(WorkspaceKind::parse(w)?),
        None => None,
    };
    if let Some(d) = dir {
        let p = resolve_dir(d)?;
        return Ok((WorkspaceKind::Dir, Some(p.display().to_string()), false));
    }
    if let Some(r) = repo {
        let kind = kind.unwrap_or(WorkspaceKind::Git);
        return match kind {
            WorkspaceKind::Dir => {
                let p = resolve_dir(r)?;
                Ok((WorkspaceKind::Dir, Some(p.display().to_string()), false))
            }
            WorkspaceKind::Git => {
                let path = resolve_git(r).unwrap_or_else(|| r.to_string());
                Ok((WorkspaceKind::Git, Some(path), false))
            }
            WorkspaceKind::None => Ok((WorkspaceKind::None, None, true)),
        };
    }
    match kind {
        None | Some(WorkspaceKind::None) => Ok((WorkspaceKind::None, None, true)),
        Some(WorkspaceKind::Dir) => anyhow::bail!("workspace dir requires dir"),
        Some(WorkspaceKind::Git) => Ok((WorkspaceKind::Git, None, false)),
    }
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
    let canon = p.canonicalize().unwrap_or(p);
    check_dir_allowed(&canon)?;
    Ok(canon)
}

/// Refuse dir workspaces that would hand an always-approve agent the whole
/// machine or a secret store. `canon` must already be canonical (symlinks
/// resolved), so a link to ~/.ssh is caught too.
pub fn check_dir_allowed(canon: &Path) -> Result<()> {
    let home = dirs_home();
    if canon.parent().is_none() {
        bail!(
            "refusing dir workspace {}: filesystem root",
            canon.display()
        );
    }
    if let Some(home) = &home {
        if canon == home.as_path() || home.starts_with(canon) {
            bail!(
                "refusing dir workspace {}: home directory or its parent; pick a project folder",
                canon.display()
            );
        }
        for rel in [
            ".ssh",
            ".gnupg",
            ".aws",
            ".config/atr",
            ".config/gh",
            ".kube",
            ".docker",
        ] {
            let s = home.join(rel);
            let s = s.canonicalize().unwrap_or(s);
            if canon.starts_with(&s) {
                bail!(
                    "refusing dir workspace {}: secrets/config directory",
                    canon.display()
                );
            }
        }
    }
    for sys in [
        "/etc",
        "/usr",
        "/bin",
        "/sbin",
        "/lib",
        "/boot",
        "/proc",
        "/sys",
        "/dev",
        "/var",
        "/System",
        "/Library",
        "/private/etc",
    ] {
        let sp = Path::new(sys);
        if canon == sp
            || canon.starts_with(sp)
                && !canon.starts_with("/var/folders")
                && !canon.starts_with("/var/tmp")
        {
            bail!(
                "refusing dir workspace {}: system directory",
                canon.display()
            );
        }
    }
    Ok(())
}

fn dirs_home() -> Option<PathBuf> {
    let h = std::env::var_os("HOME").filter(|h| !h.is_empty())?;
    let h = PathBuf::from(h);
    Some(h.canonicalize().unwrap_or(h))
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
Stay inside {path}. Do not read or write files outside that directory. Do not `cd` elsewhere. \
Do not create a `.atr/` folder here.\n\
atr asks the CLI to confine you where it can (Claude `--add-dir`, Codex `-s workspace-write`, \
Grok `--cwd`, OpenCode runs in the folder). That is best-effort: some CLIs can still write outside, \
and atr does not snapshot-detect outside writes.\n"
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
    // Search after the variable marker so a "## Messages" line inside the
    // stable prefix (e.g. AGENTS.md) can never pull the note into it.
    let from = rendered.find(crate::prompt::VARIABLE_MARKER).unwrap_or(0);
    if let Some(idx) = rendered[from..].find("\n## Messages\n").map(|i| i + from) {
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
        let d = workspace_note(WorkspaceKind::Dir, "/notes", OutputKind::Reply).unwrap();
        assert!(d.contains("Stay inside /notes"));
        assert!(d.contains("best-effort"));
        assert!(d.contains("`.atr/`"));
        let rendered = "head\n## Messages\nbody\n";
        let out = insert_before_messages(rendered, "## Workspace\nstay in /tmp\n");
        assert!(out.contains("stay in /tmp"));
        assert!(out.find("Workspace").unwrap() < out.find("Messages").unwrap());
    }

    #[test]
    fn title_and_prompt_size() {
        assert_eq!(title_from_prompt("hello\nworld"), "hello");
        assert_eq!(title_from_prompt("\n  \nFix typo"), "Fix typo");
        check_prompt_bytes("ok", 2).unwrap();
        let err = check_prompt_bytes("abcd", 3).unwrap_err().to_string();
        assert!(err.contains("4 bytes"), "{err}");
        assert!(err.contains("max is 3"), "{err}");
    }

    #[test]
    fn intake_honors_workspace_when_path_is_repo() {
        let dir = tempfile::tempdir().unwrap();
        let notes = dir.path().join("notes");
        std::fs::create_dir_all(&notes).unwrap();
        let notes_s = notes.display().to_string();

        let (k, p, empty) =
            resolve_intake_workspace(None, Some(&notes_s), Some("dir"), |_| None).unwrap();
        assert_eq!(k, WorkspaceKind::Dir);
        assert!(!empty);
        assert_eq!(
            p.as_deref(),
            Some(notes.canonicalize().unwrap().display().to_string()).as_deref()
        );

        let (k, p, empty) =
            resolve_intake_workspace(Some(&notes_s), Some("/etc"), Some("git"), |_| None).unwrap();
        assert_eq!(k, WorkspaceKind::Dir);
        assert!(!empty);
        assert!(p.unwrap().contains("notes"));

        let (k, p, empty) = resolve_intake_workspace(None, Some("my-repo"), Some("git"), |r| {
            if r == "my-repo" {
                Some("/src/my-repo".into())
            } else {
                None
            }
        })
        .unwrap();
        assert_eq!(k, WorkspaceKind::Git);
        assert_eq!(p.as_deref(), Some("/src/my-repo"));
        assert!(!empty);

        let (k, p, empty) = resolve_intake_workspace(None, None, None, |_| None).unwrap();
        assert_eq!(k, WorkspaceKind::None);
        assert!(p.is_none());
        assert!(empty);

        let (k, _, empty) =
            resolve_intake_workspace(None, Some("/src/repo"), Some("none"), |_| None).unwrap();
        assert_eq!(k, WorkspaceKind::None);
        assert!(empty);
    }
}

#[cfg(test)]
mod dir_guard_tests {
    use super::*;

    #[test]
    fn refuses_root_home_and_secrets() {
        assert!(check_dir_allowed(Path::new("/")).is_err());
        assert!(check_dir_allowed(Path::new("/etc")).is_err());
        if let Some(h) = dirs_home() {
            assert!(check_dir_allowed(&h).is_err());
            assert!(check_dir_allowed(&h.join(".ssh")).is_err());
            if let Some(parent) = h.parent() {
                assert!(check_dir_allowed(parent).is_err());
            }
        }
        let t = tempfile::tempdir().unwrap();
        let c = t.path().canonicalize().unwrap();
        assert!(check_dir_allowed(&c).is_ok(), "{}", c.display());
    }
}

/// Capabilities a task can require from its provider (`needs`).
pub const KNOWN_NEEDS: &[&str] = &["browser", "desktop"];

/// Parse a comma/space separated `needs` list. Lowercases, dedupes, and
/// rejects unknown capabilities. Empty input yields an empty list.
pub fn parse_needs<S: AsRef<str>>(items: &[S]) -> Result<Vec<String>> {
    let mut out: Vec<String> = Vec::new();
    for item in items {
        for raw in item.as_ref().split(|c: char| c == ',' || c.is_whitespace()) {
            let n = raw.trim().to_ascii_lowercase();
            if n.is_empty() {
                continue;
            }
            if !KNOWN_NEEDS.contains(&n.as_str()) {
                bail!("need {n:?} must be one of: {}", KNOWN_NEEDS.join(", "));
            }
            if !out.contains(&n) {
                out.push(n);
            }
        }
    }
    Ok(out)
}

/// `needs:browser` / `needs:desktop` GitHub labels → needs. Unknown
/// `needs:*` labels are ignored so other label schemes keep working.
pub fn needs_from_labels<S: AsRef<str>>(labels: &[S]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for l in labels {
        let l = l.as_ref().trim().to_ascii_lowercase();
        if let Some(n) = l.strip_prefix("needs:").map(str::trim) {
            if KNOWN_NEEDS.contains(&n) && !out.iter().any(|o| o == n) {
                out.push(n.to_string());
            }
        }
    }
    out
}

/// Stored needs plus any `needs:*` labels, deduped.
pub fn task_needs(stored: &str, labels: &str) -> Vec<String> {
    let mut out = parse_needs(&[stored]).unwrap_or_default();
    let ls: Vec<&str> = labels.split(',').collect();
    for n in needs_from_labels(&ls) {
        if !out.contains(&n) {
            out.push(n);
        }
    }
    out
}

/// Serde helper: accept `needs = "browser"`, `"browser,desktop"`, or a list.
pub fn de_needs<'de, D>(d: D) -> std::result::Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(serde::Deserialize)]
    #[serde(untagged)]
    enum OneOrMany {
        One(String),
        Many(Vec<String>),
    }
    let v: Option<OneOrMany> = serde::Deserialize::deserialize(d)?;
    let items = match v {
        None => Vec::new(),
        Some(OneOrMany::One(s)) => vec![s],
        Some(OneOrMany::Many(v)) => v,
    };
    parse_needs(&items).map_err(serde::de::Error::custom)
}

#[cfg(test)]
mod needs_tests {
    use super::*;

    #[test]
    fn parse_needs_normalises_and_rejects_unknown() {
        assert_eq!(
            parse_needs(&["Browser, desktop browser"]).unwrap(),
            vec!["browser", "desktop"]
        );
        assert!(parse_needs(&[""]).unwrap().is_empty());
        assert!(parse_needs(&["gpu"]).is_err());
    }

    #[test]
    fn labels_map_to_needs() {
        assert_eq!(
            needs_from_labels(&["for:atr", "needs:browser", "needs:gpu", "Needs:Desktop"]),
            vec!["browser", "desktop"]
        );
        assert_eq!(
            task_needs("desktop", "needs:browser,needs:desktop"),
            vec!["desktop", "browser"]
        );
    }
}
