//! `[[repo]]` watch targets: list / add / enable / disable / rm / migrate.
//!
//! Existing Mini configs stay on `repo_path` + `[github].repo` until the user
//! runs `atr config migrate` or `atr repos add` (which migrates first).
//! `atr init` and `atr serve` never rewrite that shorthand.

use crate::config::{
    expand_path, parse_github_slug, repo_short_name, valid_repo_name, Config, DEFAULT_GITHUB_LABEL,
};
use crate::util::sighup_daemon;
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;
use toml_edit::{ArrayOfTables, DocumentMut, Item, Table, Value};

pub fn list_text(cfg: &Config) -> String {
    let repos = cfg.effective_repos();
    if repos.is_empty() {
        return "(no repos)\n".into();
    }
    let headers = [
        "name", "enabled", "github", "label", "path", "max", "source",
    ];
    let mut rows: Vec<[String; 7]> = Vec::new();
    for r in &repos {
        rows.push([
            r.name.clone(),
            if r.enabled { "yes".into() } else { "no".into() },
            if r.github.is_empty() {
                "-".into()
            } else {
                r.github.clone()
            },
            if r.label.is_empty() {
                "-".into()
            } else {
                r.label.clone()
            },
            r.path.display().to_string(),
            if r.max_parallel == 0 {
                "-".into()
            } else {
                r.max_parallel.to_string()
            },
            if r.shorthand { "shorthand" } else { "[[repo]]" }.into(),
        ]);
    }
    let widths: Vec<usize> = (0..headers.len())
        .map(|i| {
            rows.iter()
                .map(|r| r[i].chars().count())
                .chain(std::iter::once(headers[i].chars().count()))
                .max()
                .unwrap_or(1)
        })
        .collect();
    let mut out = String::new();
    for (i, h) in headers.iter().enumerate() {
        if i > 0 {
            out.push_str("  ");
        }
        out.push_str(&format!("{:<w$}", h, w = widths[i]));
    }
    out.push('\n');
    for r in &rows {
        for (i, cell) in r.iter().enumerate() {
            if i > 0 {
                out.push_str("  ");
            }
            out.push_str(&format!("{:<w$}", cell, w = widths[i]));
        }
        out.push('\n');
    }
    if repos.iter().any(|r| r.shorthand) {
        out.push_str(
            "source=shorthand is repo_path + [github].repo. Run `atr config migrate` to write [[repo]].\n",
        );
    }
    out
}

#[derive(Debug, Clone)]
pub struct AddOpts<'a> {
    pub path: &'a Path,
    pub label: Option<&'a str>,
    pub name: Option<&'a str>,
    pub clone_if_missing: bool,
}

pub fn add(config_path: &Path, github: &str, opts: AddOpts<'_>) -> Result<String> {
    let slug = parse_github_slug(github)?;
    let name = match opts.name {
        Some(n) => {
            if !valid_repo_name(n) {
                bail!("repo name {n:?} must be letters, digits, '-' or '_'");
            }
            n.to_string()
        }
        None => {
            let n = repo_short_name(&slug);
            if !valid_repo_name(&n) {
                bail!("could not derive a repo name from {slug}; pass --name");
            }
            n
        }
    };
    let label = opts
        .label
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(DEFAULT_GITHUB_LABEL);
    let dest = expand_path(&opts.path.display().to_string());
    let dest_raw = display_path(&dest);

    if !dest.exists() {
        if !opts.clone_if_missing {
            bail!(
                "{} does not exist; pass a clone path or let add clone it",
                dest.display()
            );
        }
        clone_repo(&slug, &dest)?;
    } else if !dest.join(".git").exists() && !dest.join(".git").is_file() {
        bail!("{} exists and is not a git repo", dest.display());
    }

    let mut notes = String::new();
    if !has_explicit_repos(config_path)? && has_shorthand(config_path)? {
        match migrate(config_path)? {
            MigrateOutcome::Wrote(msg) => notes.push_str(&msg),
            MigrateOutcome::Already | MigrateOutcome::Nothing => {}
        }
    }

    let mut doc = read_doc(config_path)?;
    let arr = repo_array(&mut doc)?;
    for t in arr.iter() {
        if t.get("name").and_then(|v| v.as_str()) == Some(name.as_str()) {
            bail!("repo {name} already exists");
        }
        if t.get("github").and_then(|v| v.as_str()) == Some(slug.as_str()) {
            bail!("repo {slug} already exists");
        }
    }
    let mut table = Table::new();
    table["name"] = value_str(&name);
    table["path"] = value_str(&dest_raw);
    table["github"] = value_str(&slug);
    table["label"] = value_str(label);
    table["enabled"] = Item::Value(Value::from(true));
    crate::config_edit::backup_config(config_path)?;
    arr.push(table);
    write_doc(config_path, &doc)?;
    sighup_ok(config_path);
    notes.push_str(&format!(
        "added repo {name} ({slug}) at {}\n",
        dest.display()
    ));
    Ok(notes)
}

pub fn set_enabled(config_path: &Path, name: &str, enabled: bool) -> Result<String> {
    ensure_explicit(config_path, name)?;
    crate::config_edit::apply_set(
        config_path,
        &format!("repo.{name}.enabled"),
        if enabled { "true" } else { "false" },
    )?;
    Ok(format!(
        "repo {name} {}\n",
        if enabled { "enabled" } else { "disabled" }
    ))
}

pub fn remove(config_path: &Path, name: &str) -> Result<String> {
    ensure_explicit(config_path, name)?;
    crate::config_edit::backup_config(config_path)?;
    let mut doc = read_doc(config_path)?;
    let arr = repo_array(&mut doc)?;
    let idx = arr
        .iter()
        .position(|t| t.get("name").and_then(|v| v.as_str()) == Some(name))
        .ok_or_else(|| anyhow::anyhow!("unknown repo {name}"))?;
    arr.remove(idx);
    write_doc(config_path, &doc)?;
    sighup_ok(config_path);
    Ok(format!("removed repo {name}\n"))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MigrateOutcome {
    Already,
    Nothing,
    Wrote(String),
}

/// Convert `repo_path` + `[github].repo` into a `[[repo]]` block.
/// Never deletes the shorthand keys (Mini keeps its values).
pub fn migrate(config_path: &Path) -> Result<MigrateOutcome> {
    if has_explicit_repos(config_path)? {
        return Ok(MigrateOutcome::Already);
    }
    if !has_shorthand(config_path)? {
        return Ok(MigrateOutcome::Nothing);
    }
    let text = std::fs::read_to_string(config_path)
        .with_context(|| format!("read {}", config_path.display()))?;
    let cfg = crate::config::parse_config_toml(&text)?;
    let mut doc: DocumentMut = text
        .parse()
        .with_context(|| format!("parse {}", config_path.display()))?;
    let path_raw = doc
        .get("repo_path")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let github = doc
        .get("github")
        .and_then(|t| t.get("repo"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let label = doc
        .get("github")
        .and_then(|t| t.get("label"))
        .and_then(|v| v.as_str())
        .unwrap_or(DEFAULT_GITHUB_LABEL)
        .to_string();
    let name = if valid_repo_name(&cfg.task_key_prefix) {
        cfg.task_key_prefix.clone()
    } else if !github.is_empty() {
        repo_short_name(&github)
    } else {
        "repo".into()
    };
    if !valid_repo_name(&name) {
        bail!(
            "cannot migrate: derive a [[repo]] name (task_key_prefix={:?})",
            cfg.task_key_prefix
        );
    }
    let path_value = if path_raw.is_empty() {
        cfg.repo_path.display().to_string()
    } else {
        path_raw
    };
    let mut table = Table::new();
    table["name"] = value_str(&name);
    table["path"] = value_str(&path_value);
    table["github"] = value_str(&github);
    table["label"] = value_str(&label);
    table["enabled"] = Item::Value(Value::from(true));
    crate::config_edit::backup_config(config_path)?;
    let arr = repo_array(&mut doc)?;
    arr.push(table);
    write_doc(config_path, &doc)?;
    sighup_ok(config_path);
    Ok(MigrateOutcome::Wrote(format!(
        "migrated shorthand to [[repo]] name={name} github={github}\n"
    )))
}

pub fn migrate_text(config_path: &Path) -> Result<String> {
    match migrate(config_path)? {
        MigrateOutcome::Already => Ok("already using [[repo]]; nothing to migrate\n".into()),
        MigrateOutcome::Nothing => Ok("no repo_path / [github].repo shorthand to migrate\n".into()),
        MigrateOutcome::Wrote(msg) => Ok(msg),
    }
}

fn ensure_explicit(config_path: &Path, name: &str) -> Result<()> {
    if has_explicit_repos(config_path)? {
        return Ok(());
    }
    if has_shorthand(config_path)? {
        let cfg = crate::config::load_config(config_path)?;
        let syn = cfg.shorthand_repo();
        if syn.name == name || syn.github == name || repo_short_name(&syn.github) == name {
            migrate(config_path)?;
            return Ok(());
        }
        bail!(
            "no [[repo]] named {name}; this config uses repo_path shorthand. Run `atr config migrate` first"
        );
    }
    Ok(())
}

fn has_explicit_repos(path: &Path) -> Result<bool> {
    let doc = read_doc(path)?;
    Ok(doc
        .get("repo")
        .and_then(|i| i.as_array_of_tables())
        .map(|a| !a.is_empty())
        .unwrap_or(false))
}

fn has_shorthand(path: &Path) -> Result<bool> {
    let doc = read_doc(path)?;
    let repo_path = doc
        .get("repo_path")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();
    let github = doc
        .get("github")
        .and_then(|t| t.get("repo"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();
    Ok(!repo_path.is_empty() || !github.is_empty())
}

fn repo_array(doc: &mut DocumentMut) -> Result<&mut ArrayOfTables> {
    if !doc.contains_key("repo") {
        doc["repo"] = Item::ArrayOfTables(ArrayOfTables::new());
    }
    doc["repo"]
        .as_array_of_tables_mut()
        .ok_or_else(|| anyhow::anyhow!("[[repo]] is not an array of tables"))
}

fn read_doc(path: &Path) -> Result<DocumentMut> {
    let text = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    text.parse()
        .with_context(|| format!("parse {}", path.display()))
}

fn write_doc(path: &Path, doc: &DocumentMut) -> Result<()> {
    std::fs::write(path, doc.to_string()).with_context(|| format!("write {}", path.display()))?;
    crate::config::parse_config_toml(&std::fs::read_to_string(path)?)?;
    Ok(())
}

fn value_str(s: &str) -> Item {
    Item::Value(Value::from(s))
}

fn display_path(path: &Path) -> String {
    if let Some(home) = std::env::var_os("HOME") {
        let home = PathBuf::from(home);
        if let Ok(rest) = path.strip_prefix(&home) {
            return format!("~/{}", rest.display());
        }
    }
    path.display().to_string()
}

fn clone_repo(github: &str, dest: &Path) -> Result<()> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let dest_s = dest.display().to_string();
    let gh = Command::new("gh")
        .args(["repo", "clone", github, &dest_s])
        .status();
    if let Ok(st) = gh {
        if st.success() {
            return Ok(());
        }
    }
    let https = format!("https://github.com/{github}.git");
    let st = Command::new("git")
        .args(["clone", &https, &dest_s])
        .status()
        .with_context(|| format!("git clone {https}"))?;
    if st.success() {
        return Ok(());
    }
    let ssh = format!("git@github.com:{github}.git");
    let st = Command::new("git")
        .args(["clone", &ssh, &dest_s])
        .status()
        .with_context(|| format!("git clone {ssh}"))?;
    if st.success() {
        return Ok(());
    }
    bail!("could not clone {github} into {dest_s} (tried gh, https, ssh)");
}

fn sighup_ok(path: &Path) {
    if let Ok(cfg) = crate::config::load_config(path) {
        match sighup_daemon(&cfg) {
            Ok(()) => eprintln!("sent SIGHUP to atr serve"),
            Err(e) => eprintln!("config saved; SIGHUP skipped ({e})"),
        }
    }
}

pub fn detect_cwd_git() -> Option<(PathBuf, Option<String>)> {
    let out = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let root = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
    if root.as_os_str().is_empty() {
        return None;
    }
    let origin = Command::new("git")
        .args([
            "-C",
            &root.display().to_string(),
            "remote",
            "get-url",
            "origin",
        ])
        .output()
        .ok();
    let github = origin.and_then(|o| {
        if !o.status.success() {
            return None;
        }
        parse_github_slug(std::str::from_utf8(&o.stdout).ok()?.trim()).ok()
    });
    Some((root, github))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn seed() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let mut f = std::fs::File::create(&path).unwrap();
        write!(
            f,
            r#"
listen = "127.0.0.1:0"
repo_path = "~/src/acme"
task_key_prefix = "acme"
worktree_root = "/tmp/wt"
system_prompt_file = "/tmp/p.md"
[github]
enabled = true
repo = "owner/acme"
label = "for:legacy"
[[provider]]
name = "claude"
kind = "claude"
"#
        )
        .unwrap();
        (dir, path)
    }

    #[test]
    fn migrate_writes_repo_keeps_shorthand() {
        let (_dir, path) = seed();
        let before = std::fs::read_to_string(&path).unwrap();
        let out = migrate(&path).unwrap();
        match out {
            MigrateOutcome::Wrote(msg) => assert!(msg.contains("acme"), "{msg}"),
            other => panic!("{other:?}"),
        }
        let after = std::fs::read_to_string(&path).unwrap();
        assert!(after.contains("[[repo]]"), "{after}");
        assert!(after.contains("name = \"acme\""), "{after}");
        assert!(after.contains("github = \"owner/acme\""), "{after}");
        assert!(after.contains("label = \"for:legacy\""), "{after}");
        assert!(after.contains("repo_path = \"~/src/acme\""), "{after}");
        assert!(after.contains("repo = \"owner/acme\""), "{after}");
        assert!(after.contains("task_key_prefix = \"acme\""), "{after}");
        let cfg = crate::config::parse_config_toml(&after).unwrap();
        assert_eq!(cfg.repos.len(), 1);
        assert_eq!(cfg.issue_task_key(&cfg.repos[0], 7), "acme-7");
        assert_eq!(migrate(&path).unwrap(), MigrateOutcome::Already);
        assert!(before.contains("for:legacy"));
    }

    #[test]
    fn add_migrates_shorthand_then_appends() {
        let (dir, path) = seed();
        let extra = dir.path().join("other");
        std::fs::create_dir_all(extra.join(".git")).unwrap();
        add(
            &path,
            "owner/other",
            AddOpts {
                path: &extra,
                label: Some("for:atr"),
                name: None,
                clone_if_missing: false,
            },
        )
        .unwrap();
        let after = std::fs::read_to_string(&path).unwrap();
        assert!(after.contains("name = \"acme\""), "{after}");
        assert!(after.contains("name = \"other\""), "{after}");
        let cfg = crate::config::parse_config_toml(&after).unwrap();
        assert_eq!(cfg.repos.len(), 2);
        assert_eq!(cfg.issue_task_key(&cfg.repos[0], 1), "acme-1");
        assert_eq!(cfg.issue_task_key(&cfg.repos[1], 1), "other-1");
    }

    #[test]
    fn enable_disable_rm() {
        let (dir, path) = seed();
        let extra = dir.path().join("other");
        std::fs::create_dir_all(extra.join(".git")).unwrap();
        add(
            &path,
            "owner/other",
            AddOpts {
                path: &extra,
                label: None,
                name: Some("other"),
                clone_if_missing: false,
            },
        )
        .unwrap();
        set_enabled(&path, "other", false).unwrap();
        let cfg = crate::config::load_config(&path).unwrap();
        assert!(!cfg.repo_by_name("other").unwrap().enabled);
        remove(&path, "other").unwrap();
        let cfg = crate::config::load_config(&path).unwrap();
        assert!(cfg.repo_by_name("other").is_none());
        assert_eq!(cfg.repos.len(), 1);
    }

    #[test]
    fn list_shows_shorthand() {
        let (_dir, path) = seed();
        let cfg = crate::config::load_config(&path).unwrap();
        let text = list_text(&cfg);
        assert!(text.contains("acme"), "{text}");
        assert!(text.contains("shorthand"), "{text}");
        assert!(text.contains("owner/acme"), "{text}");
    }
}
