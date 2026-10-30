use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

pub fn is_git_repo(path: &Path) -> bool {
    let git = path.join(".git");
    git.exists() || git.is_file()
}

/// Answer-only scratch folder (`<worktree_root>/_none/<key>`).
pub fn is_none_workspace(path: &Path) -> bool {
    path.iter().any(|c| c == "_none")
}

fn quiet_git() -> Command {
    crate::util::git_command()
}

fn capture_git() -> Command {
    crate::util::git_capture()
}

/// Work in place in a local directory. Does not create a git worktree.
pub fn ensure_dir_workspace(path: &Path) -> Result<PathBuf> {
    if !path.exists() {
        bail!("directory workspace {} does not exist", path.display());
    }
    if !path.is_dir() {
        bail!("directory workspace {} is not a directory", path.display());
    }
    Ok(path.canonicalize().unwrap_or_else(|_| path.to_path_buf()))
}

/// Scratch folder for answer-only tasks (not a git repo).
pub fn ensure_none_workspace(root: &Path, key: &str) -> Result<PathBuf> {
    let dest = root.join("_none").join(key);
    std::fs::create_dir_all(&dest)
        .with_context(|| format!("create none workspace {}", dest.display()))?;
    Ok(dest)
}

pub fn ensure_worktree(
    repo: &Path,
    root: &Path,
    key: &str,
    branch_prefix: &str,
    main: &str,
) -> Result<PathBuf> {
    let repo_name = repo
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "repo".into());
    let dest = root.join(repo_name).join(key);
    if dest.join(".git").exists() || dest.join(".git").is_file() {
        let _ = ensure_atr_exclude(&dest);
        return Ok(dest);
    }
    if dest.exists()
        && dest
            .read_dir()
            .map(|mut d| d.next().is_none())
            .unwrap_or(false)
    {
        let _ = std::fs::remove_dir(&dest);
    }
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create worktree parent {}", parent.display()))?;
    }
    let branch = format!("{branch_prefix}/{key}");
    let status = quiet_git()
        .args([
            "-C",
            &repo.to_string_lossy(),
            "worktree",
            "add",
            "-B",
            &branch,
            &dest.to_string_lossy(),
            main,
        ])
        .status()
        .context("git worktree add")?;
    if !status.success() {
        if dest.join(".git").exists() {
            return Ok(dest);
        }
        bail!(
            "git worktree add failed for {} -> {} ({status})",
            repo.display(),
            dest.display()
        );
    }
    ensure_atr_exclude(&dest)?;
    Ok(dest)
}

pub fn pool_slot_path(root: &Path, repo: &Path, provider: &str, slot: u32) -> PathBuf {
    let repo_name = repo
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "repo".into());
    let safe: String = provider
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    root.join(repo_name)
        .join(".atr")
        .join("wt")
        .join(format!("{safe}-{slot}"))
}

pub fn is_pool_worktree(path: &Path) -> bool {
    path.components().any(|c| c.as_os_str() == ".atr")
        && path
            .to_string_lossy()
            .split("/.atr/")
            .nth(1)
            .is_some_and(|rest| rest.starts_with("wt/") || rest.starts_with("wt\\"))
}

pub fn current_branch(dest: &Path) -> Option<String> {
    let out = capture_git()
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .current_dir(dest)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() || s == "HEAD" {
        None
    } else {
        Some(s)
    }
}

pub fn slot_from_pool_path(path: &Path, provider: &str) -> Option<u32> {
    let name = path.file_name()?.to_str()?;
    let prefix = format!("{provider}-");
    name.strip_prefix(&prefix)?.parse().ok()
}

/// Stable folder per repo+provider+slot. Reset to `main` on a new task.
pub fn acquire_pool_worktree(
    repo: &Path,
    root: &Path,
    provider: &str,
    slot: u32,
    key: &str,
    branch_prefix: &str,
    main: &str,
    reset: bool,
) -> Result<PathBuf> {
    let dest = pool_slot_path(root, repo, provider, slot);
    let branch = format!("{branch_prefix}/{key}");
    if dest.join(".git").exists() || dest.join(".git").is_file() {
        if !reset {
            let _ = ensure_atr_exclude(&dest);
            return Ok(dest);
        }
        detach_branch_elsewhere(repo, &dest, &branch);
        reset_pool_slot(&dest, &branch, main)?;
        ensure_atr_exclude(&dest)?;
        return Ok(dest);
    }
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    if dest.exists()
        && dest
            .read_dir()
            .map(|mut d| d.next().is_none())
            .unwrap_or(false)
    {
        let _ = std::fs::remove_dir(&dest);
    }
    detach_branch_elsewhere(repo, &dest, &branch);
    // --detach so two providers can share a task key (branch name) across slots.
    let status = quiet_git()
        .args([
            "-C",
            &repo.to_string_lossy(),
            "worktree",
            "add",
            "--detach",
            &dest.to_string_lossy(),
            main,
        ])
        .status()
        .context("git worktree add pool")?;
    if !status.success() {
        if dest.join(".git").exists() {
            reset_pool_slot(&dest, &branch, main)?;
            ensure_atr_exclude(&dest)?;
            return Ok(dest);
        }
        bail!(
            "git worktree add failed for pool {} -> {} ({status})",
            repo.display(),
            dest.display()
        );
    }
    reset_pool_slot(&dest, &branch, main)?;
    ensure_atr_exclude(&dest)?;
    Ok(dest)
}

/// `git checkout -B <branch>` fails if another worktree already has it.
fn detach_branch_elsewhere(repo: &Path, keep: &Path, branch: &str) {
    let out = capture_git()
        .args([
            "-C",
            &repo.to_string_lossy(),
            "worktree",
            "list",
            "--porcelain",
        ])
        .output();
    let Ok(out) = out else {
        return;
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let mut current: Option<PathBuf> = None;
    let mut current_branch = String::new();
    let flush = |path: Option<PathBuf>, br: &str| {
        let Some(path) = path else {
            return;
        };
        if path == keep {
            return;
        }
        let short = br.strip_prefix("refs/heads/").unwrap_or(br);
        if short != branch {
            return;
        }
        let _ = quiet_git()
            .args(["checkout", "--detach"])
            .current_dir(&path)
            .status();
    };
    for line in text.lines() {
        if let Some(p) = line.strip_prefix("worktree ") {
            flush(current.take(), &current_branch);
            current = Some(PathBuf::from(p));
            current_branch.clear();
        } else if let Some(b) = line.strip_prefix("branch ") {
            current_branch = b.to_string();
        }
    }
    flush(current, &current_branch);
}

fn reset_pool_slot(dest: &Path, branch: &str, main: &str) -> Result<()> {
    let dest_s = dest.to_string_lossy();
    let run = |args: &[&str]| -> Result<()> {
        let st = quiet_git()
            .args(args)
            .current_dir(dest)
            .status()
            .with_context(|| format!("git {} in {}", args.join(" "), dest_s))?;
        if !st.success() {
            bail!("git {} failed in {} ({st})", args.join(" "), dest_s);
        }
        Ok(())
    };
    // Detach first so -B can recreate the task branch on this slot.
    let _ = quiet_git()
        .args(["checkout", "--detach"])
        .current_dir(dest)
        .status();
    run(&["checkout", "-B", branch, main])?;
    run(&["reset", "--hard", main])?;
    run(&["clean", "-fd"])?;
    // `.atr/` is git-excluded, so `clean -fd` keeps it: drop the previous
    // task's checklist so it never leaks into the next task's prompt.
    let _ = std::fs::remove_file(dest.join(crate::checklist::REL_PATH));
    Ok(())
}

/// Ignore runner files in this worktree only (`.git/info/exclude`).
pub fn ensure_atr_exclude(worktree: &Path) -> Result<()> {
    let out = capture_git()
        .current_dir(worktree)
        .args(["rev-parse", "--git-path", "info/exclude"])
        .output()
        .context("git rev-parse info/exclude")?;
    if !out.status.success() {
        bail!(
            "git rev-parse --git-path info/exclude failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let rel = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if rel.is_empty() {
        bail!("empty git info/exclude path");
    }
    let path = {
        let p = PathBuf::from(&rel);
        if p.is_absolute() {
            p
        } else {
            worktree.join(p)
        }
    };
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    }
    let mut text = std::fs::read_to_string(&path).unwrap_or_default();
    let has = text.lines().any(|l| {
        let t = l.trim();
        t == ".atr/" || t == ".atr" || t == "/.atr/"
    });
    if !has {
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(".atr/\n");
        std::fs::write(&path, text).with_context(|| format!("write {}", path.display()))?;
    }
    Ok(())
}

/// Block `git push` from a shadow worktree: local pre-push hook + pushurl.
pub fn ensure_shadow_no_push(worktree: &Path) -> Result<()> {
    let hooks = worktree.join(".atr").join("hooks");
    std::fs::create_dir_all(&hooks).with_context(|| format!("create {}", hooks.display()))?;
    let hook = hooks.join("pre-push");
    std::fs::write(
        &hook,
        "#!/bin/sh\necho \"atr shadow: git push is disabled in this worktree\" >&2\nexit 1\n",
    )
    .with_context(|| format!("write {}", hook.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755))?;
    }
    let hooks_rel = ".atr/hooks";
    let st = quiet_git()
        .current_dir(worktree)
        .args(["config", "--local", "core.hooksPath", hooks_rel])
        .status()
        .context("git config core.hooksPath")?;
    if !st.success() {
        bail!(
            "git config core.hooksPath failed for {}",
            worktree.display()
        );
    }
    let remotes = capture_git()
        .current_dir(worktree)
        .args(["remote"])
        .output()
        .context("git remote")?;
    let mut names: Vec<String> = String::from_utf8_lossy(&remotes.stdout)
        .lines()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if !names.iter().any(|n| n == "origin") {
        names.push("origin".into());
    }
    for remote in names {
        let key = format!("remote.{remote}.pushurl");
        let st = quiet_git()
            .current_dir(worktree)
            .args(["config", "--local", &key, "no_push"])
            .status()
            .with_context(|| format!("git config {key}"))?;
        if !st.success() {
            tracing::debug!(remote, "shadow pushurl config failed");
        }
    }
    Ok(())
}

/// Compare-arm variant of [`ensure_shadow_no_push`] for a linked worktree of
/// the user's own repo. `git config --local` there would write the shared
/// repo config and block pushes for every task, so the hook path and
/// `pushurl = no_push` go into this worktree's own `config.worktree`
/// (`extensions.worktreeConfig`). Undo with [`clear_worktree_no_push`].
pub fn ensure_worktree_no_push(worktree: &Path) -> Result<()> {
    write_no_push_hook(worktree)?;
    git_ok(worktree, &["config", "extensions.worktreeConfig", "true"])?;
    git_ok(
        worktree,
        &["config", "--worktree", "core.hooksPath", ".atr/hooks"],
    )?;
    for remote in remote_names(worktree)? {
        let key = format!("remote.{remote}.pushurl");
        git_ok(worktree, &["config", "--worktree", &key, "no_push"])?;
    }
    Ok(())
}

/// Remove what [`ensure_worktree_no_push`] added (winner promotion).
pub fn clear_worktree_no_push(worktree: &Path) -> Result<()> {
    let _ = quiet_git()
        .current_dir(worktree)
        .args(["config", "--worktree", "--unset", "core.hooksPath"])
        .status();
    for remote in remote_names(worktree)? {
        let key = format!("remote.{remote}.pushurl");
        let _ = quiet_git()
            .current_dir(worktree)
            .args(["config", "--worktree", "--unset-all", &key])
            .status();
    }
    let _ = std::fs::remove_file(worktree.join(".atr").join("hooks").join("pre-push"));
    let _ = std::fs::remove_file(worktree.join(".atr").join("bin").join("gh"));
    Ok(())
}

fn write_no_push_hook(worktree: &Path) -> Result<()> {
    let hooks = worktree.join(".atr").join("hooks");
    std::fs::create_dir_all(&hooks).with_context(|| format!("create {}", hooks.display()))?;
    let hook = hooks.join("pre-push");
    std::fs::write(
        &hook,
        "#!/bin/sh\necho \"atr: git push is disabled in this worktree\" >&2\nexit 1\n",
    )
    .with_context(|| format!("write {}", hook.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755))?;
    }
    Ok(())
}

fn remote_names(worktree: &Path) -> Result<Vec<String>> {
    let remotes = capture_git()
        .current_dir(worktree)
        .args(["remote"])
        .output()
        .context("git remote")?;
    let mut names: Vec<String> = String::from_utf8_lossy(&remotes.stdout)
        .lines()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if !names.iter().any(|n| n == "origin") {
        names.push("origin".into());
    }
    Ok(names)
}

fn git_ok(worktree: &Path, args: &[&str]) -> Result<()> {
    let st = quiet_git()
        .current_dir(worktree)
        .args(args)
        .status()
        .with_context(|| format!("git {}", args.join(" ")))?;
    if !st.success() {
        bail!("git {} failed in {}", args.join(" "), worktree.display());
    }
    Ok(())
}

/// Remove a git worktree. Refuses a dirty tree unless `force`.
pub fn remove_worktree(repo: &Path, dest: &Path, force: bool) -> Result<()> {
    if dest.exists() {
        let mut args = vec![
            "-C".into(),
            repo.to_string_lossy().into_owned(),
            "worktree".into(),
            "remove".into(),
        ];
        if force {
            args.push("--force".into());
        }
        args.push(dest.to_string_lossy().into_owned());
        let strs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        let st = quiet_git()
            .args(&strs)
            .status()
            .context("git worktree remove")?;
        if st.success() {
            return Ok(());
        }
        if force && dest.exists() {
            let _ = std::fs::remove_dir_all(dest);
            let _ = quiet_git()
                .args(["-C", &repo.to_string_lossy(), "worktree", "prune"])
                .status();
            return Ok(());
        }
        bail!("git worktree remove failed for {} ({st})", dest.display());
    }
    Ok(())
}

pub fn init_sample_repo(path: &Path) -> Result<()> {
    std::fs::create_dir_all(path)?;
    let git = |args: &[&str]| -> Result<()> {
        let st = quiet_git()
            .current_dir(path)
            .args(args)
            .status()
            .with_context(|| format!("git {}", args.join(" ")))?;
        if !st.success() {
            bail!("git {} failed: {st}", args.join(" "));
        }
        Ok(())
    };
    git(&["init", "-b", "main"])?;
    git(&["config", "user.email", "atr@example.test"])?;
    git(&["config", "user.name", "atr"])?;
    git(&["config", "commit.gpgsign", "false"])?;
    std::fs::write(path.join("README.md"), "test repo\n")?;
    git(&["add", "README.md"])?;
    git(&["commit", "-m", "init"])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn none_workspace_is_the_scratch_folder() {
        assert!(is_none_workspace(Path::new("/tmp/work/_none/cli-default")));
        assert!(!is_none_workspace(Path::new(
            "/tmp/work/nixer-app/cli-default"
        )));
    }

    #[test]
    fn checkout_noise_is_captured() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        init_sample_repo(&repo).unwrap();
        let out = capture_git()
            .current_dir(&repo)
            .args(["checkout", "-B", "tmp", "main"])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(
            err.contains("HEAD is now at") || err.is_empty() || out.status.success(),
            "checkout stderr must be captured (not inherited): {err:?}"
        );
        let leaked = quiet_git()
            .current_dir(&repo)
            .args(["checkout", "-B", "tmp2", "main"])
            .output();
        assert!(
            leaked.is_ok(),
            "quiet git still runs when output() is requested after null stdio"
        );
    }

    #[test]
    fn worktree_excludes_atr_dir() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        init_sample_repo(&repo).unwrap();
        let wt =
            ensure_worktree(&repo, &dir.path().join("work"), "acme-1", "otto", "main").unwrap();
        std::fs::create_dir_all(wt.join(".atr")).unwrap();
        std::fs::write(wt.join(".atr/prompt.md"), "secret\n").unwrap();
        std::fs::write(wt.join("visible.md"), "hi\n").unwrap();
        let ignore = Command::new("git")
            .current_dir(&wt)
            .args(["check-ignore", "-q", ".atr/prompt.md"])
            .status()
            .unwrap();
        assert!(
            ignore.success(),
            ".atr/ must be in this worktree's info/exclude"
        );
        let status = Command::new("git")
            .current_dir(&wt)
            .args(["status", "--short"])
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&status.stdout);
        assert!(
            !text.contains(".atr"),
            ".atr/ must not appear in git status:\n{text}"
        );
        assert!(text.contains("visible.md"), "{text}");
    }

    #[test]
    fn shadow_worktree_blocks_push() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        init_sample_repo(&repo).unwrap();
        let st = Command::new("git")
            .current_dir(&repo)
            .args(["remote", "add", "origin", "https://example.test/o/r.git"])
            .status()
            .unwrap();
        assert!(st.success());
        let wt =
            ensure_worktree(&repo, &dir.path().join("work"), "acme-1", "otto", "main").unwrap();
        ensure_shadow_no_push(&wt).unwrap();
        let pushurl = Command::new("git")
            .current_dir(&wt)
            .args(["config", "--get", "remote.origin.pushurl"])
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&pushurl.stdout).trim(),
            "no_push",
            "shadow worktree must set pushurl=no_push"
        );
        let push = Command::new("git")
            .current_dir(&wt)
            .args(["push", "origin", "HEAD"])
            .output()
            .unwrap();
        assert!(
            !push.status.success(),
            "git push must fail in a shadow worktree: {}",
            String::from_utf8_lossy(&push.stderr)
        );
    }

    #[test]
    fn pool_reuses_path_and_resets_between_tasks() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        init_sample_repo(&repo).unwrap();
        let root = dir.path().join("work");
        let a = acquire_pool_worktree(&repo, &root, "claude", 0, "task-a", "atr", "main", true)
            .unwrap();
        assert!(a.ends_with(".atr/wt/claude-0"), "{}", a.display());
        assert!(is_pool_worktree(&a));
        assert_eq!(slot_from_pool_path(&a, "claude"), Some(0));
        std::fs::write(a.join("leftover.txt"), "old\n").unwrap();
        std::fs::create_dir_all(a.join(".atr")).unwrap();
        std::fs::write(a.join(crate::checklist::REL_PATH), "- [x] task-a step\n").unwrap();
        let b = acquire_pool_worktree(&repo, &root, "claude", 0, "task-b", "atr", "main", true)
            .unwrap();
        assert_eq!(a, b);
        assert!(
            !b.join("leftover.txt").exists(),
            "reset must clean leftovers"
        );
        assert!(
            !b.join(crate::checklist::REL_PATH).exists(),
            "a new task must not inherit the last task's checklist"
        );
        assert_eq!(current_branch(&b).as_deref(), Some("atr/task-b"));
        let same = acquire_pool_worktree(&repo, &root, "claude", 0, "task-b", "atr", "main", false)
            .unwrap();
        assert_eq!(same, b);
        let grok =
            acquire_pool_worktree(&repo, &root, "grok", 0, "task-b", "atr", "main", true).unwrap();
        assert!(grok.ends_with(".atr/wt/grok-0"), "{}", grok.display());
        assert_eq!(current_branch(&grok).as_deref(), Some("atr/task-b"));
        assert_ne!(
            current_branch(&b).as_deref(),
            Some("atr/task-b"),
            "claude slot must detach so grok can take the task branch"
        );
    }
}
