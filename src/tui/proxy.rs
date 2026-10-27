//! Provider-CLI proxy helpers: non-interactive controls and native TUI.
//!
//! atr's engine always spawns the CLI headless (Claude `-p` stream-json,
//! Grok `--output-format streaming-json`, Codex `exec --json`, OpenCode
//! `run --format json`). Native slash pickers (Grok `/model`) live in
//! that CLI's own interactive TUI. This module maps what we can without
//! inventing a second catalog:
//!
//! * `/model` is atr-local (see [`collect_models`]): never a chat turn
//! * `/cli` → spawn the provider CLI *without* `-p` / `exec` / json, on
//!   the session worktree, resuming when we have an id

use crate::app::Shared;
use crate::config::Provider;
use crate::tui::slash::ModelChoice;
use crate::tui::{session_model, tui_provider_now, Workspace};
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub(crate) fn models_argv(kind: &str) -> Option<&'static [&'static str]> {
    match kind {
        "grok" | "opencode" => Some(&["models"]),
        _ => None,
    }
}

pub(crate) fn collect_models(shared: &Shared, key: &str, ws: &Workspace) -> Vec<ModelChoice> {
    let Some((kind, cli, pname)) = current_kind_cli(shared, key) else {
        return Vec::new();
    };
    let cfg = shared.config.read().unwrap().clone();
    let current = session_model(&shared.db, key);
    let mut out: Vec<ModelChoice> = Vec::new();
    let mut push = |name: String, tag: &str| {
        let name = name.trim().to_string();
        if name.is_empty() {
            return;
        }
        if let Some(e) = out.iter_mut().find(|m| m.name == name) {
            if !e.tag.contains(tag) {
                e.tag = format!("{}, {tag}", e.tag);
            }
            return;
        }
        let is_cur = current.as_deref() == Some(name.as_str());
        out.push(ModelChoice {
            name,
            tag: tag.to_string(),
            current: is_cur,
        });
    };

    if let Some(m) = &current {
        push(m.clone(), "session");
    }
    if let Some(p) = cfg.provider(&pname) {
        if !p.model.trim().is_empty() {
            push(p.model.clone(), "config");
        }
    }
    if let Some(m) = crate::detect::cli_default_model(&kind) {
        push(m, "detect");
    }
    if let Ok(runs) = shared.db.list_runs(Some(key)) {
        if let Some(m) = runs
            .last()
            .and_then(|r| r.model.clone())
            .filter(|s| !s.is_empty())
        {
            push(m, "last-run");
        }
    }
    for p in &cfg.providers {
        if p.kind == kind && p.name != pname && !p.model.trim().is_empty() {
            push(p.model.clone(), &format!("config:{}", p.name));
        }
    }
    if kind == "opencode" {
        for b in crate::presets::BUILTINS {
            push(b.model.to_string(), "preset");
        }
    }
    if models_argv(&kind).is_some() {
        let cwd = session_cwd(shared, key, ws);
        if let Ok(text) = run_models_list(&cli, &kind, &cwd) {
            for id in parse_cli_model_ids(&text) {
                push(id, "cli");
            }
        }
    }
    if let Some(cur) = &current {
        if let Some(e) = out.iter_mut().find(|m| m.name == *cur) {
            e.current = true;
        }
    } else if let Some(e) = out.iter_mut().find(|m| m.tag.contains("config")) {
        e.current = true;
    }
    out
}

pub(crate) fn parse_cli_model_ids(text: &str) -> Vec<String> {
    let mut out = crate::detect::parse_models(text);
    for line in text.lines() {
        let tok = line.split_whitespace().next().unwrap_or("");
        if !looks_like_model_id(tok) {
            continue;
        }
        if !out.iter().any(|e| e == tok) {
            out.push(tok.to_string());
        }
    }
    out
}

fn looks_like_model_id(s: &str) -> bool {
    if s.len() < 2 || s.starts_with('-') || s.starts_with('#') {
        return false;
    }
    s.chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/' | ':'))
        && s.chars().any(|c| c.is_ascii_alphanumeric())
}

pub(crate) fn run_models_list(cli: &Path, kind: &str, cwd: &Path) -> Result<String> {
    let args = models_argv(kind).context("no models subcommand for this kind")?;
    let out = Command::new(cli)
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("ATR_TUI_SIDE", "1")
        .output()
        .with_context(|| format!("run {} models", cli.display()))?;
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    let err = String::from_utf8_lossy(&out.stderr);
    if !err.trim().is_empty() {
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(err.trim());
        text.push('\n');
    }
    if !out.status.success() && text.trim().is_empty() {
        bail!("{} models exited {}", cli.display(), out.status);
    }
    if text.trim().is_empty() {
        text = "(no models listed)\n".into();
    }
    Ok(text)
}

pub(crate) fn current_kind_cli(shared: &Shared, key: &str) -> Option<(String, PathBuf, String)> {
    let cfg = shared.config.read().unwrap().clone();
    let name = tui_provider_now(shared, key).or_else(|| {
        cfg.providers
            .iter()
            .find(|p| p.enabled)
            .map(|p| p.name.clone())
    })?;
    let p = cfg.provider(&name)?;
    Some((p.kind.clone(), p.cli.clone(), p.name.clone()))
}

pub(crate) fn session_cwd(shared: &Shared, key: &str, ws: &Workspace) -> PathBuf {
    if let Ok(Some(t)) = shared.db.get_task(key) {
        if let Some(wt) = t
            .worktree
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            let p = PathBuf::from(wt);
            if p.is_dir() {
                return p;
            }
        }
        if let Some(r) = t.repo.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            let p = PathBuf::from(r);
            if p.is_dir() {
                return p;
            }
        }
    }
    if let Some(r) = ws.repo.as_deref() {
        let p = PathBuf::from(r);
        if p.is_dir() {
            return p;
        }
    }
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

pub(crate) fn native_argv(p: &Provider, session: Option<&str>, cwd: &Path) -> Vec<String> {
    let mut args = Vec::new();
    match p.kind.as_str() {
        "codex" => {
            if let Some(sid) = session.filter(|s| !s.is_empty()) {
                args.push("resume".into());
                args.push(sid.to_string());
            }
        }
        "opencode" => {
            if let Some(sid) = session.filter(|s| !s.is_empty()) {
                let flag = if p.resume_flag.is_empty() {
                    "--session"
                } else {
                    p.resume_flag.as_str()
                };
                args.push(flag.into());
                args.push(sid.to_string());
            }
        }
        _ => {
            if let Some(sid) = session.filter(|s| !s.is_empty()) {
                if !p.resume_flag.is_empty() {
                    args.push(p.resume_flag.clone());
                    args.push(sid.to_string());
                }
            }
            if !p.cwd_flag.is_empty() {
                args.push(p.cwd_flag.clone());
                args.push(cwd.display().to_string());
            }
            let model = if p.model.trim().is_empty() {
                None
            } else {
                Some(p.model.clone())
            };
            if let Some(m) = model {
                if !p.model_flag.is_empty() {
                    args.push(p.model_flag.clone());
                    args.push(m);
                }
            }
        }
    }
    args
}

pub(crate) fn run_native_cli(shared: &Shared, key: &str, ws: &Workspace) -> Result<String> {
    let cfg = shared.config.read().unwrap().clone();
    let Some((kind, cli, pname)) = current_kind_cli(shared, key) else {
        bail!("no provider to open. /provider first");
    };
    let Some(p) = cfg.provider(&pname) else {
        bail!("provider {pname:?} is not in config");
    };
    let cwd = session_cwd(shared, key, ws);
    let sid = shared
        .db
        .task_session(key, &pname)
        .ok()
        .flatten()
        .or_else(|| {
            shared
                .db
                .get_task(key)
                .ok()
                .flatten()
                .and_then(|t| t.session_id)
        });
    let mut p = p.clone();
    if let Some(m) = session_model(&shared.db, key) {
        p.model = m;
    }
    let args = native_argv(&p, sid.as_deref(), &cwd);
    let status = Command::new(&cli)
        .args(&args)
        .current_dir(&cwd)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .with_context(|| format!("spawn {} ({kind})", cli.display()))?;
    Ok(format!(
        "opened {pname} ({kind}) interactive{} — back in atr (exit {}).",
        if sid.is_some() { " · resumed" } else { "" },
        status.code().unwrap_or(-1)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Provider;

    #[test]
    fn parse_cli_model_ids_accepts_simple_and_slash_ids() {
        let ids = parse_cli_model_ids("test-model\ntest-model-2\nmoonshotai/kimi-k2.6\n# skip\n");
        assert!(ids.iter().any(|s| s == "test-model"), "{ids:?}");
        assert!(ids.iter().any(|s| s == "test-model-2"), "{ids:?}");
        assert!(ids.iter().any(|s| s == "moonshotai/kimi-k2.6"), "{ids:?}");
    }

    #[test]
    fn models_argv_only_for_clis_that_have_it() {
        assert_eq!(models_argv("grok"), Some(&["models"][..]));
        assert_eq!(models_argv("opencode"), Some(&["models"][..]));
        assert_eq!(models_argv("claude"), None);
        assert_eq!(models_argv("cursor"), None);
        assert_eq!(models_argv("codex"), None);
    }

    #[test]
    fn native_argv_skips_print_flags() {
        let mut grok = Provider::test_stub("grok");
        grok.kind = "grok".into();
        grok.resume_flag = "--resume".into();
        grok.cwd_flag = "--cwd".into();
        grok.model_flag = "--model".into();
        grok.model = "grok-code".into();
        let args = native_argv(&grok, Some("sid-1"), Path::new("/tmp"));
        assert!(args.contains(&"--resume".into()), "{args:?}");
        assert!(args.contains(&"sid-1".into()), "{args:?}");
        assert!(
            !args.iter().any(|a| a == "-p" || a.contains("json")),
            "{args:?}"
        );
    }
}
