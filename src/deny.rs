//! Best-effort deny rules for Telegram-originated (and host) CLI runs.
//!
//! These map onto each vendor's own flags where they exist. They are not a
//! hard guarantee — GitHub branch protection is the backstop for force-push
//! and branch deletion.

use crate::config::{Config, TelegramDeny};
use crate::db::Task;
use crate::providers::CommandSpec;
use anyhow::{Context, Result};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

/// Built-in patterns applied when `[telegram.deny] rules` is empty.
pub const DEFAULT_RULES: &[&str] = &[
    "rm -rf",
    "launchctl bootout",
    "launchctl unload",
    "git push --force",
    "git push -f",
    "git push --delete",
    "git branch -D main",
    "git branch -D develop",
    "git branch -D rimas-develop",
    "sudo",
];

pub fn effective_rules(deny: &TelegramDeny) -> Vec<String> {
    if deny.rules.is_empty() {
        DEFAULT_RULES.iter().map(|s| (*s).to_string()).collect()
    } else {
        deny.rules.clone()
    }
}

/// Telegram-originated runs, and anything whose workspace is the host dir.
pub fn applies(cfg: &Config, task: &Task) -> bool {
    if task.source_kind == "telegram" {
        return true;
    }
    is_host_workspace(cfg, task)
}

pub fn is_host_workspace(cfg: &Config, task: &Task) -> bool {
    let Some(repo) = task.repo.as_deref().filter(|s| !s.is_empty()) else {
        return false;
    };
    let host = crate::config::expand_path(&cfg.telegram.host_dir);
    let repo_path = crate::config::expand_path(repo);
    paths_equal(&host, &repo_path)
}

fn paths_equal(a: &Path, b: &Path) -> bool {
    match (fs::canonicalize(a), fs::canonicalize(b)) {
        (Ok(x), Ok(y)) => x == y,
        _ => a == b,
    }
}

/// Kinds that can take `--agent`.
pub fn agent_supported(kind: &str) -> bool {
    matches!(kind, "grok" | "claude")
}

pub fn agent_unsupported_message(kind: &str, provider: &str) -> String {
    format!("--agent is not supported on {provider} (kind {kind}); only grok and claude accept it")
}

/// Add vendor deny flags. Cursor and OpenCode/spark have no equivalent flag.
pub fn apply_to_spec(kind: &str, spec: &mut CommandSpec, rules: &[String]) {
    match kind {
        "grok" => apply_grok_deny(spec, rules),
        "claude" => apply_claude_deny(spec, rules),
        "codex" => apply_codex_sandbox(spec),
        _ => {}
    }
}

fn apply_grok_deny(spec: &mut CommandSpec, rules: &[String]) {
    for rule in rules {
        if spec
            .args
            .windows(2)
            .any(|w| w[0] == "--deny" && w[1] == *rule)
        {
            continue;
        }
        spec.args.push("--deny".into());
        spec.args.push(rule.clone());
    }
}

fn apply_claude_deny(spec: &mut CommandSpec, rules: &[String]) {
    for rule in rules {
        let pat = claude_bash_pattern(rule);
        if spec
            .args
            .windows(2)
            .any(|w| w[0] == "--disallowedTools" && w[1] == pat)
        {
            continue;
        }
        spec.args.push("--disallowedTools".into());
        spec.args.push(pat);
    }
}

pub fn claude_bash_pattern(rule: &str) -> String {
    format!("Bash({rule}:*)")
}

fn apply_codex_sandbox(spec: &mut CommandSpec) {
    let mut i = 0;
    let mut saw = false;
    while i < spec.args.len() {
        if spec.args[i] == "-s" || spec.args[i] == "--sandbox" {
            saw = true;
            if i + 1 < spec.args.len() {
                spec.args[i + 1] = "workspace-write".into();
            }
        }
        i += 1;
    }
    if !saw {
        spec.args.push("-s".into());
        spec.args.push("workspace-write".into());
    }
}

/// Shared shim dir under the state directory (`deny-bin/`).
pub fn wrapper_dir(state_db: &Path) -> PathBuf {
    state_db
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("deny-bin")
}

pub fn ensure_wrapper_dir(state_db: &Path) -> Result<PathBuf> {
    let dir = wrapper_dir(state_db);
    write_wrappers(&dir)?;
    Ok(dir)
}

/// Write `git` / `rm` / `launchctl` / `sudo` shims that refuse the default
/// patterns, then exec the real binary found later on PATH.
pub fn write_wrappers(dir: &Path) -> Result<()> {
    fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    write_shim(dir, "git", GIT_SHIM)?;
    write_shim(dir, "rm", RM_SHIM)?;
    write_shim(dir, "launchctl", LAUNCHCTL_SHIM)?;
    write_shim(dir, "sudo", SUDO_SHIM)?;
    Ok(())
}

fn write_shim(dir: &Path, name: &str, body: &str) -> Result<()> {
    let path = dir.join(name);
    fs::write(&path, body).with_context(|| format!("write {}", path.display()))?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755))?;
    Ok(())
}

const GIT_SHIM: &str = concat!(
    "#!/bin/sh\n",
    r#"
find_real() {
  name="$1"
  here=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
  old_ifs=$IFS
  IFS=:
  for d in $PATH; do
    [ -z "$d" ] && continue
    case "$d" in
      "$here") continue ;;
    esac
    if [ -x "$d/$name" ]; then
      IFS=$old_ifs
      printf '%s\n' "$d/$name"
      return 0
    fi
  done
  IFS=$old_ifs
  return 1
}
force=0
delete=0
branch_d=0
branch=
prev=
for a in "$@"; do
  case "$a" in
    --force|--force-with-lease|-f) force=1 ;;
    --delete) delete=1 ;;
    -D) branch_d=1 ;;
  esac
  if [ "$prev" = "-D" ]; then
    branch=$a
  fi
  prev=$a
done
if [ "$force" = 1 ]; then
  echo "atr deny: git push --force is blocked (Telegram/host). GitHub branch protection is the hard backstop." >&2
  exit 1
fi
if [ "$delete" = 1 ]; then
  echo "atr deny: git push --delete is blocked (Telegram/host)." >&2
  exit 1
fi
if [ "$branch_d" = 1 ]; then
  case "$branch" in
    main|develop|rimas-develop)
      echo "atr deny: git branch -D $branch is blocked (Telegram/host)." >&2
      exit 1
      ;;
  esac
fi
real=$(find_real git) || { echo "atr deny: git not found on PATH" >&2; exit 127; }
exec "$real" "$@"
"#
);

const RM_SHIM: &str = concat!(
    "#!/bin/sh\n",
    r#"
find_real() {
  name="$1"
  here=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
  old_ifs=$IFS
  IFS=:
  for d in $PATH; do
    [ -z "$d" ] && continue
    case "$d" in
      "$here") continue ;;
    esac
    if [ -x "$d/$name" ]; then
      IFS=$old_ifs
      printf '%s\n' "$d/$name"
      return 0
    fi
  done
  IFS=$old_ifs
  return 1
}
recur=0
for a in "$@"; do
  case "$a" in
    -r|-R|--recursive|-rf|-fr|-Rf|-fR) recur=1 ;;
  esac
  case "$a" in
    -*r*|-*R*) recur=1 ;;
  esac
done
if [ "$recur" = 1 ]; then
  echo "atr deny: rm -rf is blocked (Telegram/host)." >&2
  exit 1
fi
real=$(find_real rm) || { echo "atr deny: rm not found on PATH" >&2; exit 127; }
exec "$real" "$@"
"#
);

const LAUNCHCTL_SHIM: &str = concat!(
    "#!/bin/sh\n",
    r#"
find_real() {
  name="$1"
  here=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
  old_ifs=$IFS
  IFS=:
  for d in $PATH; do
    [ -z "$d" ] && continue
    case "$d" in
      "$here") continue ;;
    esac
    if [ -x "$d/$name" ]; then
      IFS=$old_ifs
      printf '%s\n' "$d/$name"
      return 0
    fi
  done
  IFS=$old_ifs
  return 1
}
for a in "$@"; do
  case "$a" in
    bootout|unload)
      echo "atr deny: launchctl $a is blocked (Telegram/host)." >&2
      exit 1
      ;;
  esac
done
real=$(find_real launchctl) || { echo "atr deny: launchctl not found on PATH" >&2; exit 127; }
exec "$real" "$@"
"#
);

const SUDO_SHIM: &str = concat!(
    "#!/bin/sh\n",
    "echo \"atr deny: sudo is blocked (Telegram/host).\" >&2\n",
    "exit 1\n",
);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::CommandSpec;

    fn spec() -> CommandSpec {
        CommandSpec {
            program: PathBuf::from("cli"),
            args: vec!["--output-format".into(), "json".into()],
            cwd: PathBuf::from("/tmp"),
            stdin_text: None,
            env: Vec::new(),
        }
    }

    #[test]
    fn grok_gets_deny_flags() {
        let mut s = spec();
        apply_to_spec("grok", &mut s, &["rm -rf".into(), "sudo".into()]);
        assert!(s.args.windows(2).any(|w| w == ["--deny", "rm -rf"]));
        assert!(s.args.windows(2).any(|w| w == ["--deny", "sudo"]));
    }

    #[test]
    fn claude_gets_disallowed_tools() {
        let mut s = spec();
        apply_to_spec("claude", &mut s, &["rm -rf".into()]);
        assert!(s
            .args
            .windows(2)
            .any(|w| w == ["--disallowedTools", "Bash(rm -rf:*)"]));
    }

    #[test]
    fn codex_forced_to_workspace_write() {
        let mut s = spec();
        s.args.extend(["-s".into(), "danger-full-access".into()]);
        apply_to_spec("codex", &mut s, &[]);
        assert!(s.args.windows(2).any(|w| w == ["-s", "workspace-write"]));
        assert!(!s.args.iter().any(|a| a == "danger-full-access"));
        let mut empty = spec();
        apply_to_spec("codex", &mut empty, &[]);
        assert!(empty
            .args
            .windows(2)
            .any(|w| w == ["-s", "workspace-write"]));
    }

    #[test]
    fn cursor_and_opencode_have_no_flag() {
        let mut s = spec();
        apply_to_spec("cursor", &mut s, &["sudo".into()]);
        apply_to_spec("opencode", &mut s, &["sudo".into()]);
        assert!(!s
            .args
            .iter()
            .any(|a| a.contains("deny") || a.contains("disallowed")));
    }

    #[test]
    fn wrapper_scripts_refuse_patterns() {
        let dir = tempfile::tempdir().unwrap();
        write_wrappers(dir.path()).unwrap();
        for name in ["git", "rm", "launchctl", "sudo"] {
            let p = dir.path().join(name);
            assert!(p.exists(), "{name}");
            let mode = fs::metadata(&p).unwrap().permissions().mode() & 0o111;
            assert_ne!(mode, 0, "{name} not executable");
        }
        let git = fs::read_to_string(dir.path().join("git")).unwrap();
        assert!(git.contains("git push --force"));
        assert!(git.contains("rimas-develop"));
        let out = std::process::Command::new(dir.path().join("sudo"))
            .output()
            .unwrap();
        assert!(!out.status.success());
        let msg = String::from_utf8_lossy(&out.stderr);
        assert!(msg.contains("sudo is blocked"), "{msg}");
    }

    #[test]
    fn applies_to_telegram_and_host() {
        let cfg = crate::config::Config::from_raw(
            toml::from_str(
                r#"
listen = "127.0.0.1:0"
state_db = "/tmp/x.db"
worktree_root = "/tmp"
repo_path = "/tmp"
system_prompt_file = "/tmp/p.md"
[telegram]
host_dir = "/tmp/atr-ops-test"
[[provider]]
name = "claude"
kind = "claude"
cli = "/bin/true"
"#,
            )
            .unwrap(),
        )
        .unwrap();
        let telegram = Task {
            source_kind: "telegram".into(),
            ..Task::default()
        };
        assert!(applies(&cfg, &telegram));
        let other = Task {
            source_kind: "cli".into(),
            repo: Some("/tmp/other".into()),
            ..Task::default()
        };
        assert!(!applies(&cfg, &other));
        let host = Task {
            source_kind: "cli".into(),
            repo: Some("/tmp/atr-ops-test".into()),
            workspace_kind: "dir".into(),
            ..Task::default()
        };
        assert!(applies(&cfg, &host));
        let alias = crate::config::Config::from_raw(
            toml::from_str(
                r#"
listen = "127.0.0.1:0"
state_db = "/tmp/x.db"
worktree_root = "/tmp"
repo_path = "/tmp"
system_prompt_file = "/tmp/p.md"
[telegram]
mini_dir = "/tmp/atr-ops-test"
[[provider]]
name = "claude"
kind = "claude"
cli = "/bin/true"
"#,
            )
            .unwrap(),
        )
        .unwrap();
        assert!(applies(&alias, &host));
    }

    #[test]
    fn empty_rules_use_defaults() {
        let rules = effective_rules(&TelegramDeny {
            rules: Vec::new(),
            wrapper: false,
        });
        assert!(rules.iter().any(|r| r == "sudo"));
        assert!(rules.iter().any(|r| r.contains("rimas-develop")));
    }
}
