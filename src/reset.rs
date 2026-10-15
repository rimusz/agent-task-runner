//! `atr reset`: stop and remove the user service, then delete only the
//! files and directories atr itself owns. The `atr` binary, provider
//! configs, and user repos are never touched.

use crate::config::Config;
use anyhow::{bail, Context, Result};
use std::fs;
use std::path::{Path, PathBuf};

pub const DONE_MSG: &str = "atr reset complete. Run `atr init` to set up again.";

/// Provider skill folders `atr init` installs (each holds only atr's SKILL.md).
pub const SKILL_DIRS: &[&str] = &[
    ".claude/skills/atr",
    ".cursor/skills/atr",
    ".codex/skills/atr",
    ".grok/skills/atr",
    ".config/opencode/skills/atr",
];

/// What `atr reset` will do, computed without touching anything.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ResetPlan {
    /// Service definition to stop and remove (unit or plist), if installed.
    pub service_file: Option<PathBuf>,
    /// atr-owned paths that exist and will be removed.
    pub remove: Vec<PathBuf>,
    /// Configured paths outside atr's own dirs that are left alone.
    pub kept: Vec<(PathBuf, String)>,
}

/// atr's own roots under `home`. Everything reset deletes is one of these
/// (or a configured path inside one of them).
pub fn atr_roots(home: &Path, skill_home: &Path) -> Vec<PathBuf> {
    let mut v = vec![
        home.join(".config/atr"),
        home.join(".local/share/atr"),
        home.join("atr-work"),
        home.join("Library/Logs/atr"),
        home.join("Library/Logs/atr.boot.err"),
    ];
    v.extend(SKILL_DIRS.iter().map(|d| skill_home.join(d)));
    v
}

fn inside(p: &Path, root: &Path) -> bool {
    p.starts_with(root)
}

/// Build the plan. `cfg` adds configured paths (state_db, log, worktree_root,
/// config file); those outside atr's roots are reported as kept, never removed.
/// A root that contains a watched repo is kept too.
pub fn plan(
    home: &Path,
    skill_home: &Path,
    service_file: &Path,
    config_path: &Path,
    cfg: Option<&Config>,
) -> ResetPlan {
    let roots = atr_roots(home, skill_home);
    let repos: Vec<PathBuf> = cfg
        .map(|c| {
            let mut r: Vec<PathBuf> = c.effective_repos().into_iter().map(|e| e.path).collect();
            if !c.repo_path.as_os_str().is_empty() {
                r.push(c.repo_path.clone());
            }
            r
        })
        .unwrap_or_default();

    let mut out = ResetPlan {
        service_file: service_file.exists().then(|| service_file.to_path_buf()),
        ..Default::default()
    };
    for root in &roots {
        if !root.exists() {
            continue;
        }
        if let Some(repo) = repos.iter().find(|r| inside(r, root)) {
            out.kept.push((
                root.clone(),
                format!("contains watched repo {}", repo.display()),
            ));
            continue;
        }
        out.remove.push(root.clone());
    }

    let mut extra: Vec<(PathBuf, &str)> = vec![(config_path.to_path_buf(), "config file")];
    if let Some(c) = cfg {
        extra.push((c.state_db.clone(), "task database"));
        extra.push((c.log.path.clone(), "log file"));
        extra.push((c.worktree_root.clone(), "worktree root"));
    }
    for (p, what) in extra {
        if p.as_os_str().is_empty() || !p.exists() {
            continue;
        }
        if roots.iter().any(|r| inside(&p, r)) {
            continue; // already covered (or deliberately kept) via its root
        }
        out.kept
            .push((p, format!("{what} outside atr's own dirs; not removed")));
    }
    out
}

pub fn render(plan: &ResetPlan) -> String {
    let mut s = String::new();
    match &plan.service_file {
        Some(f) => s.push_str(&format!("stop and uninstall service: {}\n", f.display())),
        None => s.push_str("service: not installed\n"),
    }
    if plan.remove.is_empty() {
        s.push_str("remove: nothing (no atr files found)\n");
    } else {
        s.push_str("remove:\n");
        for p in &plan.remove {
            s.push_str(&format!("  {}\n", p.display()));
        }
    }
    for (p, why) in &plan.kept {
        s.push_str(&format!("keep: {} ({why})\n", p.display()));
    }
    s.push_str("keep: the atr binary, provider configs, and your repos\n");
    s
}

/// Carry out the plan. `uninstall` is the service uninstaller (injected so
/// tests never call systemctl or launchctl).
pub fn execute<F>(plan: &ResetPlan, uninstall: F) -> Result<String>
where
    F: FnOnce() -> Result<String>,
{
    let mut notes = Vec::new();
    if plan.service_file.is_some() {
        notes.push(uninstall().context("uninstall service")?);
    }
    for p in &plan.remove {
        let meta = match fs::symlink_metadata(p) {
            Ok(m) => m,
            Err(_) => continue,
        };
        if meta.is_dir() {
            fs::remove_dir_all(p).with_context(|| format!("remove {}", p.display()))?;
        } else {
            fs::remove_file(p).with_context(|| format!("remove {}", p.display()))?;
        }
        notes.push(format!("removed {}", p.display()));
    }
    notes.push(DONE_MSG.to_string());
    Ok(notes.join("\n"))
}

/// How the confirmation step should go.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Confirm {
    Skip,
    Ask,
}

pub fn confirm_mode(yes: bool, dry_run: bool, tty: bool) -> Result<Confirm> {
    if yes || dry_run {
        return Ok(Confirm::Skip);
    }
    if !tty {
        bail!("atr reset needs confirmation; re-run in a terminal or pass --yes");
    }
    Ok(Confirm::Ask)
}

pub fn answer_is_yes(line: &str) -> bool {
    matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn touch(p: &Path) {
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, "x").unwrap();
    }

    #[test]
    fn confirm_rules() {
        assert_eq!(confirm_mode(true, false, false).unwrap(), Confirm::Skip);
        assert_eq!(confirm_mode(false, true, false).unwrap(), Confirm::Skip);
        assert_eq!(confirm_mode(false, false, true).unwrap(), Confirm::Ask);
        assert!(confirm_mode(false, false, false).is_err());
        assert!(answer_is_yes(" Y\n") && answer_is_yes("yes") && !answer_is_yes("n"));
    }

    #[test]
    fn removes_only_atr_paths_and_uses_fake_uninstall() {
        let t = tempfile::tempdir().unwrap();
        let h = t.path();
        let unit = h.join(".config/systemd/user/atr.service");
        touch(&unit);
        touch(&h.join(".config/atr/config.toml"));
        touch(&h.join(".local/share/atr/state.db"));
        touch(&h.join(".local/share/atr/atr.log"));
        touch(&h.join("atr-work/wt1/file"));
        touch(&h.join(".claude/skills/atr/SKILL.md"));
        touch(&h.join(".grok/skills/atr/SKILL.md"));
        // must survive
        touch(&h.join(".claude/settings.json"));
        touch(&h.join(".claude/skills/other/SKILL.md"));
        touch(&h.join(".grok/config.toml"));
        touch(&h.join("code/myrepo/README"));
        touch(&h.join(".local/bin/atr"));

        let p = plan(h, h, &unit, &h.join(".config/atr/config.toml"), None);
        assert_eq!(p.service_file.as_deref(), Some(unit.as_path()));
        assert_eq!(p.remove.len(), 5, "{p:?}");
        let text = render(&p);
        assert!(
            text.contains(".config/atr") && text.contains("skills/atr"),
            "{text}"
        );

        let called = Cell::new(0);
        let out = execute(&p, || {
            called.set(called.get() + 1);
            fs::remove_file(&unit).unwrap();
            Ok("removed atr.service".into())
        })
        .unwrap();
        assert_eq!(called.get(), 1);
        assert!(out.ends_with(DONE_MSG), "{out}");
        for gone in [
            ".config/atr",
            ".local/share/atr",
            "atr-work",
            ".claude/skills/atr",
            ".grok/skills/atr",
        ] {
            assert!(!h.join(gone).exists(), "{gone}");
        }
        for kept in [
            ".claude/settings.json",
            ".claude/skills/other/SKILL.md",
            ".grok/config.toml",
            "code/myrepo/README",
            ".local/bin/atr",
        ] {
            assert!(h.join(kept).exists(), "{kept}");
        }
    }

    #[test]
    fn no_service_means_no_uninstall_call() {
        let t = tempfile::tempdir().unwrap();
        let h = t.path();
        touch(&h.join(".config/atr/config.toml"));
        let p = plan(
            h,
            h,
            &h.join("nope.service"),
            &h.join(".config/atr/config.toml"),
            None,
        );
        assert!(p.service_file.is_none());
        execute(&p, || panic!("must not uninstall")).unwrap();
        assert!(!h.join(".config/atr").exists());
    }

    #[test]
    fn configured_paths_outside_atr_dirs_are_kept() {
        let t = tempfile::tempdir().unwrap();
        let h = t.path();
        touch(&h.join(".config/atr/config.toml"));
        touch(&h.join("custom/atr.toml"));
        touch(&h.join("atr-work/repo/.git/HEAD"));
        let toml = format!(
            "repo_path = \"{}\"\nstate_db = \"{}\"\n[[provider]]\nname = \"grok\"\nkind = \"grok\"\n",
            h.join("atr-work/repo").display(),
            h.join("elsewhere/state.db").display()
        );
        let cfg = crate::config::parse_config_toml(&toml).unwrap();
        touch(&cfg.state_db);
        let p = plan(h, h, &h.join("x"), &h.join("custom/atr.toml"), Some(&cfg));
        assert!(!p.remove.iter().any(|r| r.ends_with("atr-work")), "{p:?}");
        assert!(!p.remove.iter().any(|r| r.starts_with(h.join("custom"))));
        assert!(!p.remove.iter().any(|r| r.starts_with(h.join("elsewhere"))));
        let kept: Vec<_> = p.kept.iter().map(|k| k.0.clone()).collect();
        assert!(kept.contains(&h.join("atr-work")));
        assert!(kept.contains(&h.join("custom/atr.toml")));
        assert!(kept.contains(&h.join("elsewhere/state.db")));
        execute(&p, || unreachable!()).unwrap();
        assert!(h.join("atr-work/repo/.git/HEAD").exists());
        assert!(h.join("custom/atr.toml").exists());
    }

    #[test]
    fn dry_run_plan_changes_nothing() {
        let t = tempfile::tempdir().unwrap();
        let h = t.path();
        touch(&h.join(".config/atr/config.toml"));
        let p = plan(h, h, &h.join("x"), &h.join(".config/atr/config.toml"), None);
        let _ = render(&p);
        assert!(h.join(".config/atr/config.toml").exists());
    }
}
