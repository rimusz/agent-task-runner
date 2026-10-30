use crate::config::{Config, Provider};
use sha2::{Digest, Sha256};
use std::path::Path;

pub const VARIABLE_MARKER: &str = "----- atr variable -----";

/// Labels that always appear after the marker, in this order.
pub const TASK_HEADER_TEMPLATE: &str = "\
## Task header
After the marker the fields are always, in this order:
Task key, Worktree, Repo, optional Issue, Shell-command timeout,
optional Shadow mode, optional Provider handoff, then Messages.
";

pub fn load_stable_prefix(system_prompt_file: &Path, repo_path: &Path) -> String {
    let mut prefix = std::fs::read_to_string(system_prompt_file).unwrap_or_else(|_| {
        tracing::warn!(
            path = %system_prompt_file.display(),
            "system prompt file missing; using a short built-in prefix"
        );
        "You are a coding agent. Work only in the given worktree.\n".into()
    });
    if !prefix.ends_with('\n') {
        prefix.push('\n');
    }
    let agents = repo_path.join("AGENTS.md");
    if let Ok(extra) = std::fs::read_to_string(&agents) {
        prefix.push('\n');
        prefix.push_str(&extra);
        if !prefix.ends_with('\n') {
            prefix.push('\n');
        }
    }
    prefix
}

/// Provider tool/skill contract. Same bytes for every task on this provider.
pub fn provider_preamble(p: &Provider) -> String {
    format!(
        "## Provider contract\nkind={}\nname={}\npermission_mode={}\nprompt_mode={}\nresume_flag={}\n",
        p.kind, p.name, p.permission_mode, p.prompt_mode, p.resume_flag
    )
}

/// System prompt + AGENTS.md + provider preamble + task header template.
pub fn assemble_stable(cfg: &Config, provider: &Provider, repo_path: &Path) -> String {
    assemble_stable_with(cfg, provider, repo_path, None)
}

pub fn assemble_stable_with(
    cfg: &Config,
    provider: &Provider,
    repo_path: &Path,
    prompt_file: Option<&Path>,
) -> String {
    let prompt = prompt_file.unwrap_or(&cfg.system_prompt_file);
    let mut out = load_stable_prefix(prompt, repo_path);
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out.push('\n');
    out.push_str(&provider_preamble(provider));
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out.push('\n');
    out.push_str(TASK_HEADER_TEMPLATE);
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out
}

pub fn hash_prefix(stable: &str) -> String {
    let digest = Sha256::digest(stable.as_bytes());
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// Split a rendered prompt into (stable prefix, variable tail).
pub fn split_variable(rendered: &str) -> Option<(&str, &str)> {
    rendered.split_once(VARIABLE_MARKER)
}

pub fn render(
    prefix: &str,
    task_key: &str,
    worktree: &Path,
    repo: &str,
    issue: Option<i64>,
    shell_timeout: &str,
    handoff: Option<&str>,
    messages: &[String],
    shadow: bool,
    checklist_on: bool,
    checklist: Option<&str>,
) -> String {
    let mut out = String::new();
    out.push_str(prefix);
    if !prefix.ends_with('\n') {
        out.push('\n');
    }
    out.push('\n');
    out.push_str(VARIABLE_MARKER);
    out.push('\n');
    out.push_str(&format!("Task key: {task_key}\n"));
    out.push_str(&format!("Worktree: {}\n", worktree.display()));
    out.push_str(&format!("Repo: {repo}\n"));
    if let Some(n) = issue {
        out.push_str(&format!("Issue: {n}\n"));
    }
    out.push_str(&format!("Shell-command timeout: {shell_timeout}\n"));
    if shadow {
        out.push_str(
            "\n## Shadow mode\nThis is a shadow run. Do not push, open or comment on pull requests, \
edit issues or labels, or send notifications. Work only in this worktree. \
Do not run `gh issue comment`, `gh pr create`, `gh pr comment`, `gh issue edit`, \
`gh label`, or `git push`.\n",
        );
    }
    if let Some(h) = handoff {
        if !h.is_empty() {
            out.push_str("\n## Provider handoff\n");
            out.push_str(h);
            out.push('\n');
        }
    }
    if checklist_on {
        out.push_str(crate::checklist::INSTRUCTION);
        if let Some(c) = checklist.filter(|s| !s.trim().is_empty()) {
            out.push_str("\n### Current checklist\n");
            out.push_str(c.trim());
            out.push('\n');
        }
    }
    out.push_str("\n## Messages\n");
    if messages.is_empty() {
        out.push_str("(none)\n");
    } else {
        for (i, m) in messages.iter().enumerate() {
            if i > 0 {
                out.push_str("\n---\n\n");
            }
            out.push_str(m);
            if !m.ends_with('\n') {
                out.push('\n');
            }
        }
    }
    out
}

/// Variable tail only (no stable prefix). Used when the CLI has a system-prompt slot.
pub fn render_variable(
    task_key: &str,
    worktree: &Path,
    repo: &str,
    issue: Option<i64>,
    shell_timeout: &str,
    handoff: Option<&str>,
    messages: &[String],
    shadow: bool,
    checklist_on: bool,
    checklist: Option<&str>,
) -> String {
    render(
        "",
        task_key,
        worktree,
        repo,
        issue,
        shell_timeout,
        handoff,
        messages,
        shadow,
        checklist_on,
        checklist,
    )
}

/// Follow-up on an existing session: new messages only, no prefix or task header.
/// First message of the turn after `steer --interrupt` / TUI Esc.
pub const INTERRUPT_PREAMBLE: &str = "Your previous turn was interrupted so you \
could read the message below. Address it, then continue your unfinished task \
from the current state of the workspace (edits you already made are still there).";

pub fn render_resume(messages: &[String], handoff: Option<&str>) -> String {
    let mut out = String::new();
    if let Some(h) = handoff.filter(|s| !s.is_empty()) {
        out.push_str("## Follow-up\n");
        out.push_str(h);
        if !h.ends_with('\n') {
            out.push('\n');
        }
        out.push('\n');
    }
    if messages.is_empty() {
        out.push_str("(none)\n");
        return out;
    }
    for (i, m) in messages.iter().enumerate() {
        if i > 0 {
            out.push_str("\n---\n\n");
        }
        out.push_str(m);
        if !m.ends_with('\n') {
            out.push('\n');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::tempdir;

    #[test]
    fn variable_section_is_after_stable_prefix() {
        let dir = tempdir().unwrap();
        let sys = dir.path().join("sys.md");
        let mut f = std::fs::File::create(&sys).unwrap();
        writeln!(f, "STABLE PREFIX LINE").unwrap();
        let prefix = load_stable_prefix(&sys, dir.path());
        let rendered = render(
            &prefix,
            "acme-1",
            Path::new("/tmp/wt"),
            "/tmp/repo",
            Some(1),
            "15m",
            None,
            &["please fix it".into()],
            false,
            false,
            None,
        );
        let (head, tail) = rendered.split_once(VARIABLE_MARKER).unwrap();
        assert!(head.contains("STABLE PREFIX LINE"));
        assert!(tail.contains("please fix it"));
        assert!(tail.contains("/tmp/wt"));
        assert!(!head.contains("please fix it"));
    }

    #[test]
    fn handoff_is_after_task_header_not_in_prefix() {
        let rendered = render(
            "STABLE\n",
            "acme-1",
            Path::new("/tmp/wt"),
            "/tmp/repo",
            Some(1),
            "15m",
            Some("Previous provider claude ended (stalled).\n### Diffstat against base\n README.md | 1 +"),
            &["please fix it".into()],
            false,
            false,
            None,
        );
        let (head, tail) = rendered.split_once(VARIABLE_MARKER).unwrap();
        assert_eq!(head.trim(), "STABLE");
        let header_at = tail.find("Task key:").unwrap();
        let handoff_at = tail.find("## Provider handoff").unwrap();
        let msgs_at = tail.find("## Messages").unwrap();
        assert!(header_at < handoff_at && handoff_at < msgs_at, "{tail}");
        assert!(tail.contains("README.md | 1 +"));
    }

    #[test]
    fn two_tasks_share_a_byte_identical_prefix() {
        let dir = tempdir().unwrap();
        let sys = dir.path().join("sys.md");
        std::fs::write(&sys, "STABLE SYSTEM\n").unwrap();
        std::fs::write(dir.path().join("AGENTS.md"), "skills here\n").unwrap();
        let cfg_txt = format!(
            r#"
listen = "127.0.0.1:0"
state_db = "{db}"
worktree_root = "{root}"
repo_path = "{root}"
system_prompt_file = "{sys}"
[[provider]]
name = "claude"
kind = "claude"
cli = "/bin/true"
permission_mode = "bypassPermissions"
"#,
            db = dir.path().join("s.db").display(),
            root = dir.path().display(),
            sys = sys.display(),
        );
        let raw: crate::config::RawConfig = toml::from_str(&cfg_txt).unwrap();
        let cfg = crate::config::Config::from_raw(raw).unwrap();
        let p = cfg.provider("claude").unwrap();
        let stable = assemble_stable(&cfg, p, dir.path());
        let a = render(
            &stable,
            "acme-1",
            Path::new("/tmp/wt-a"),
            "/tmp/repo",
            Some(1),
            "15m",
            Some("handoff from grok"),
            &["first task".into()],
            false,
            true,
            Some("- [ ] one"),
        );
        let b = render(
            &stable,
            "acme-99",
            Path::new("/tmp/wt-b"),
            "/tmp/other",
            Some(99),
            "15m",
            Some("different handoff"),
            &["second task".into()],
            true,
            true,
            Some("- [x] other"),
        );
        let (ha, ta) = split_variable(&a).unwrap();
        let (hb, tb) = split_variable(&b).unwrap();
        assert_eq!(ha, hb);
        assert_eq!(ha.trim_end(), stable.trim_end());
        assert!(ha.contains("STABLE SYSTEM"));
        assert!(ha.contains("skills here"));
        assert!(ha.contains("## Provider contract"));
        assert!(ha.contains("## Task header"));
        assert!(!ha.contains("acme-1") && !ha.contains("handoff from grok"));
        assert!(ta.contains("acme-1") && ta.contains("first task"));
        assert!(tb.contains("acme-99") && tb.contains("second task"));
        assert!(ta.contains("## Checklist") && ta.contains("- [ ] one"));
        assert!(tb.contains("## Checklist") && tb.contains("- [x] other"));
        assert!(!ha.contains("Checklist") && !ha.contains("- [ ] one"));
        assert_eq!(hash_prefix(&stable).len(), 64);
        let (ok, text) = crate::app::doctor_cache(&cfg);
        assert!(ok, "{text}");
        assert!(text.contains("claude\tok"));
    }

    #[test]
    fn shadow_rules_are_in_the_variable_section() {
        let rendered = render(
            "STABLE\n",
            "acme-shadow-1",
            Path::new("/tmp/wt"),
            "/tmp/repo",
            Some(1),
            "15m",
            None,
            &["please fix it".into()],
            true,
            false,
            None,
        );
        let (head, tail) = rendered.split_once(VARIABLE_MARKER).unwrap();
        assert_eq!(head.trim(), "STABLE");
        assert!(tail.contains("## Shadow mode"));
        assert!(tail.contains("Do not run `gh issue comment`"));
        assert!(!head.contains("Shadow mode"));
    }

    #[test]
    fn resume_is_delta_only() {
        let delta = render_resume(&["please also add tests".into()], None);
        assert_eq!(delta.trim(), "please also add tests");
        assert!(!delta.contains(VARIABLE_MARKER));
        assert!(!delta.contains("Task key"));
        assert!(!delta.contains("AGENTS.md"));
        let with_h = render_resume(&["next".into()], Some("note"));
        assert!(with_h.contains("Follow-up"));
        assert!(with_h.contains("next"));
    }
}
