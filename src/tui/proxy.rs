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
use crate::tui::{session_model, Workspace};
use anyhow::{bail, Context, Result};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// How `/model` may touch a provider `models` CLI.
///
/// [`CliFetch::Cached`] never spawns — TUI picker open / overlay refresh.
/// [`CliFetch::Blocking`] fills the cache on miss (line-mode list, tests).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CliFetch {
    Cached,
    Blocking,
}

const CLI_MODELS_TTL: Duration = Duration::from_secs(5 * 60);

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
struct CacheKey {
    kind: String,
    cli: PathBuf,
}

#[derive(Clone, Debug)]
struct CacheEntry {
    ids: Vec<String>,
    at: Instant,
}

#[derive(Debug, Default)]
struct ModelsCache {
    entries: HashMap<CacheKey, CacheEntry>,
    inflight: HashSet<CacheKey>,
}

fn models_cache() -> &'static Mutex<ModelsCache> {
    static CACHE: OnceLock<Mutex<ModelsCache>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(ModelsCache::default()))
}

fn cache_key(kind: &str, cli: &Path) -> CacheKey {
    CacheKey {
        kind: kind.to_string(),
        cli: cli.to_path_buf(),
    }
}

pub(crate) fn models_argv(kind: &str) -> Option<&'static [&'static str]> {
    match kind {
        "grok" | "opencode" => Some(&["models"]),
        _ => None,
    }
}

pub(crate) fn collect_models(
    shared: &Shared,
    key: &str,
    ws: &Workspace,
    fetch: CliFetch,
) -> Vec<ModelChoice> {
    let Some((kind, cli, pname)) = current_kind_cli(shared, key) else {
        return Vec::new();
    };
    let cfg = shared.config.read().unwrap().clone();
    let current = session_model(&shared.db, key);
    let mut out: Vec<ModelChoice> = Vec::new();
    let mut push = |name: String, tag: &str| {
        let name = name.trim().to_string();
        if name.is_empty() || is_banner_prose(&name) {
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
        for id in cli_catalog_ids(&kind, &cli, &cwd, fetch) {
            push(id, "cli");
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

/// Model ids from `grok models` / `opencode models` (and similar).
///
/// Grok 1.0.70 prints a login banner plus bulleted ids:
/// `Default model: grok-4.7` / `  * grok-4.7 (default)` / `  - grok-4.6`.
/// First-whitespace-token parsing would keep `You` / `Default` / `Available`
/// and drop the real ids (the first token is `*` or `-`).
pub(crate) fn parse_cli_model_ids(text: &str) -> Vec<String> {
    let text = crate::detect::strip_ansi(text);
    let mut out = crate::detect::parse_models(&text);
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line
            .trim_start_matches(['*', '-', '•', '·', '–', '—'])
            .trim();
        for raw in line.split_whitespace() {
            let tok = normalize_model_token(raw);
            if looks_like_cli_model_id(tok) && !out.iter().any(|e| e == tok) {
                out.push(tok.to_string());
            }
        }
    }
    out
}

fn normalize_model_token(s: &str) -> &str {
    s.trim_matches(|c: char| {
        !c.is_ascii_alphanumeric() && !matches!(c, '-' | '_' | '.' | '/' | ':')
    })
}

fn looks_like_cli_model_id(s: &str) -> bool {
    if s.len() < 2 {
        return false;
    }
    if !s
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/' | ':'))
    {
        return false;
    }
    if !s.chars().any(|c| c.is_ascii_alphanumeric()) {
        return false;
    }
    // Banner words (`You`, `Default`, `Available`) are alphabetic only.
    // Vendor ids have a hyphen, a slash (`provider/model`), or a digit.
    s.contains('-') || s.contains('/') || s.chars().any(|c| c.is_ascii_digit())
}

fn is_banner_prose(s: &str) -> bool {
    matches!(
        s.trim().to_ascii_lowercase().as_str(),
        "you" | "are" | "logged" | "in" | "with" | "default" | "available" | "models" | "model"
    )
}

fn fresh_cached_ids(kind: &str, cli: &Path) -> Option<Vec<String>> {
    let key = cache_key(kind, cli);
    let c = models_cache().lock().unwrap();
    c.entries
        .get(&key)
        .and_then(|e| (e.at.elapsed() < CLI_MODELS_TTL).then(|| e.ids.clone()))
}

pub(crate) fn cached_cli_model_ids(kind: &str, cli: &Path) -> Vec<String> {
    let key = cache_key(kind, cli);
    let c = models_cache().lock().unwrap();
    c.entries
        .get(&key)
        .map(|e| e.ids.clone())
        .unwrap_or_default()
}

fn store_cli_model_ids(kind: &str, cli: &Path, ids: Vec<String>) {
    let key = cache_key(kind, cli);
    let mut c = models_cache().lock().unwrap();
    c.entries.insert(
        key,
        CacheEntry {
            ids,
            at: Instant::now(),
        },
    );
}

pub(crate) fn invalidate_cli_models(kind: &str, cli: &Path) {
    let key = cache_key(kind, cli);
    let mut c = models_cache().lock().unwrap();
    c.entries.remove(&key);
    c.inflight.remove(&key);
}

fn fetch_and_store(kind: &str, cli: &Path, cwd: &Path) -> Vec<String> {
    let ids = run_models_list(cli, kind, cwd)
        .map(|text| parse_cli_model_ids(&text))
        .unwrap_or_default();
    store_cli_model_ids(kind, cli, ids.clone());
    ids
}

fn cli_catalog_ids(kind: &str, cli: &Path, cwd: &Path, fetch: CliFetch) -> Vec<String> {
    match fetch {
        CliFetch::Cached => cached_cli_model_ids(kind, cli),
        CliFetch::Blocking => {
            if let Some(ids) = fresh_cached_ids(kind, cli) {
                return ids;
            }
            fetch_and_store(kind, cli, cwd)
        }
    }
}

/// Spawn `kind models` off the UI thread. No-op when the kind has no
/// list subcommand, a fresh cache entry exists, or a fetch is in flight.
pub(crate) fn begin_cli_models_refresh(kind: &str, cli: &Path, cwd: &Path) {
    if models_argv(kind).is_none() {
        return;
    }
    let key = cache_key(kind, cli);
    {
        let mut c = models_cache().lock().unwrap();
        if let Some(e) = c.entries.get(&key) {
            if e.at.elapsed() < CLI_MODELS_TTL {
                return;
            }
        }
        if !c.inflight.insert(key.clone()) {
            return;
        }
    }
    let kind = kind.to_string();
    let cli = cli.to_path_buf();
    let cwd = cwd.to_path_buf();
    let _ = std::thread::Builder::new()
        .name("atr-models".into())
        .spawn(move || {
            let _ = fetch_and_store(&kind, &cli, &cwd);
            if let Ok(mut c) = models_cache().lock() {
                c.inflight.remove(&CacheKey { kind, cli });
            }
        });
}

pub(crate) fn begin_session_models_refresh(shared: &Shared, key: &str, ws: &Workspace) {
    let Some((kind, cli, _)) = current_kind_cli(shared, key) else {
        return;
    };
    let cwd = session_cwd(shared, key, ws);
    begin_cli_models_refresh(&kind, &cli, &cwd);
}

pub(crate) fn run_models_list(cli: &Path, kind: &str, cwd: &Path) -> Result<String> {
    let args = models_argv(kind).context("no models subcommand for this kind")?;
    let out = crate::util::retry_exec_busy(|| {
        Command::new(cli)
            .args(args)
            .current_dir(cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env("ATR_TUI_SIDE", "1")
            .output()
    })
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
    let name = crate::tui::session_provider_now(shared, key)?;
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
    use std::time::{Duration, Instant};

    const GROK_MODELS_SAMPLE: &str = "\
You are logged in with grok.com.

Default model: grok-4.7

Available models:
  * grok-4.7 (default)
  - grok-4.7-build-fast
  - grok-4.6
  - grok-4.5
  - cline-pass-glm-5.2
  - cline-pass-deepseek-v4-pro
  - cline-pass-mimo-v2.5-pro
  - cline-pass-kimi-k3
  - cline-pass-qwen3.8-max
  - cline-pass-minimax-m3
  - cursor-composer-2.5
  - deepseek-v4-flash
  - cursor-claude-opus-5-5
  - cursor-gpt-5.6-sol
  - cursor-grok-4.7
";

    #[test]
    fn parse_cli_model_ids_accepts_simple_and_slash_ids() {
        let ids = parse_cli_model_ids("test-model\ntest-model-2\nmoonshotai/kimi-k2.6\n# skip\n");
        assert!(ids.iter().any(|s| s == "test-model"), "{ids:?}");
        assert!(ids.iter().any(|s| s == "test-model-2"), "{ids:?}");
        assert!(ids.iter().any(|s| s == "moonshotai/kimi-k2.6"), "{ids:?}");
    }

    #[test]
    fn parse_grok_models_prose_and_bullets() {
        let ids = parse_cli_model_ids(GROK_MODELS_SAMPLE);
        for want in [
            "grok-4.7",
            "grok-4.7-build-fast",
            "grok-4.6",
            "grok-4.5",
            "cline-pass-glm-5.2",
            "cline-pass-deepseek-v4-pro",
            "cline-pass-mimo-v2.5-pro",
            "cline-pass-kimi-k3",
            "cline-pass-qwen3.8-max",
            "cline-pass-minimax-m3",
            "cursor-composer-2.5",
            "deepseek-v4-flash",
            "cursor-claude-opus-5-5",
            "cursor-gpt-5.6-sol",
            "cursor-grok-4.7",
        ] {
            assert!(ids.iter().any(|s| s == want), "missing {want} in {ids:?}");
        }
        for bad in [
            "You",
            "are",
            "logged",
            "in",
            "with",
            "Default",
            "Available",
            "models",
            "default",
            "grok.com",
        ] {
            assert!(
                !ids.iter().any(|s| s.eq_ignore_ascii_case(bad)),
                "prose token {bad:?} leaked into {ids:?}"
            );
        }
        assert_eq!(
            ids.iter().filter(|s| *s == "grok-4.7").count(),
            1,
            "{ids:?}"
        );
    }

    #[test]
    fn models_argv_only_for_clis_that_have_it() {
        assert_eq!(models_argv("grok"), Some(&["models"][..]));
        assert_eq!(models_argv("opencode"), Some(&["models"][..]));
        assert_eq!(models_argv("claude"), None);
        assert_eq!(models_argv("cursor"), None);
        assert_eq!(models_argv("codex"), None);
    }

    fn write_models_cli(dir: &Path, name: &str, log: &Path, body: &str) -> PathBuf {
        let cli = dir.join(name);
        std::fs::write(
            &cli,
            format!(
                "#!/bin/sh\nprintf . >> '{}'\nif [ \"$1\" = models ]; then\n{body}\nexit 0\nfi\nexit 1\n",
                log.display()
            ),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        cli
    }

    fn spawn_count(log: &Path) -> usize {
        std::fs::read_to_string(log).unwrap_or_default().len()
    }

    #[test]
    fn cli_models_cache_hit_skips_spawn() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("spawns");
        let cwd = dir.path();
        let grok = write_models_cli(
            dir.path(),
            "grok",
            &log,
            "echo '  * grok-4.7 (default)'\necho '  - grok-4.7-build-fast'\n",
        );
        invalidate_cli_models("grok", &grok);

        let cold = cli_catalog_ids("grok", &grok, cwd, CliFetch::Cached);
        assert!(cold.is_empty(), "{cold:?}");
        assert_eq!(spawn_count(&log), 0, "Cached must never spawn");

        let first = cli_catalog_ids("grok", &grok, cwd, CliFetch::Blocking);
        assert!(first.iter().any(|s| s == "grok-4.7"), "{first:?}");
        assert!(
            first.iter().any(|s| s == "grok-4.7-build-fast"),
            "{first:?}"
        );
        assert_eq!(spawn_count(&log), 1);

        let hit = cli_catalog_ids("grok", &grok, cwd, CliFetch::Blocking);
        assert_eq!(hit, first);
        assert_eq!(spawn_count(&log), 1, "warm Blocking must not spawn");

        let cached = cli_catalog_ids("grok", &grok, cwd, CliFetch::Cached);
        assert_eq!(cached, first);
        assert_eq!(spawn_count(&log), 1);

        let oc = write_models_cli(dir.path(), "opencode", &log, "echo moonshotai/kimi-k2.6\n");
        invalidate_cli_models("opencode", &oc);
        let oc_cached = cli_catalog_ids("opencode", &oc, cwd, CliFetch::Cached);
        assert!(oc_cached.is_empty(), "kinds must not share a catalog");
        assert_eq!(spawn_count(&log), 1);
        let oc_ids = cli_catalog_ids("opencode", &oc, cwd, CliFetch::Blocking);
        assert!(
            oc_ids.iter().any(|s| s == "moonshotai/kimi-k2.6"),
            "{oc_ids:?}"
        );
        assert_eq!(spawn_count(&log), 2);

        invalidate_cli_models("grok", &grok);
        assert!(cached_cli_model_ids("grok", &grok).is_empty());
        let again = cli_catalog_ids("grok", &grok, cwd, CliFetch::Blocking);
        assert!(again.iter().any(|s| s == "grok-4.7"), "{again:?}");
        assert_eq!(spawn_count(&log), 3);
    }

    #[test]
    fn begin_cli_models_refresh_fills_cache_off_caller() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("spawns");
        let grok = write_models_cli(dir.path(), "grok", &log, "echo '  * grok-4.7 (default)'\n");
        invalidate_cli_models("grok", &grok);
        begin_cli_models_refresh("grok", &grok, dir.path());
        let deadline = Instant::now() + Duration::from_secs(5);
        while cached_cli_model_ids("grok", &grok).is_empty() {
            assert!(
                Instant::now() < deadline,
                "background refresh did not store ids"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(cached_cli_model_ids("grok", &grok)
            .iter()
            .any(|s| s == "grok-4.7"));
        let n = spawn_count(&log);
        begin_cli_models_refresh("grok", &grok, dir.path());
        assert_eq!(
            spawn_count(&log),
            n,
            "fresh cache must not start another fetch"
        );
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
