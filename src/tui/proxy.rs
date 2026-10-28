//! Provider-CLI proxy helpers: non-interactive controls and native TUI.
//!
//! atr's engine always spawns the CLI headless (Claude `-p` stream-json,
//! Grok `--output-format streaming-json`, Codex `exec --json`, OpenCode
//! `run --format json`). Native slash pickers (Grok `/model`) live in
//! that CLI's own interactive TUI. This module maps what we can without
//! inventing a second catalog:
//!
//! * `/model` is atr-local (see [`collect_models`]): never a chat turn.
//!   Session model is per-provider. Claude seeds documented full ids
//!   (`claude-sonnet-5-5`, …). Codex reads `~/.codex/models_cache.json`
//!   slugs (never spawn its TUI).
//! * `/cli` → spawn the provider CLI *without* `-p` / `exec` / json, on
//!   the session worktree, resuming when we have an id

use crate::app::Shared;
use crate::config::Provider;
use crate::tui::slash::ModelChoice;
use crate::tui::{last_run_model_for, session_model_for, Workspace};
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

/// Live Rs-MBP: `grok models`, `opencode models`, `cursor-agent models`
/// (and `agent models`) are non-interactive catalogs. `claude models`
/// hangs; `codex models` starts a TUI and refuses `TERM=dumb`.
pub(crate) fn models_argv(kind: &str) -> Option<&'static [&'static str]> {
    match kind {
        "grok" | "opencode" | "cursor" => Some(&["models"]),
        _ => None,
    }
}

/// Claude Code `--model` / `/model` ids (support.claude.com, Oct 2026).
/// `claude models` hangs (exit 124); `/model` help is short aliases only;
/// `~/.claude/settings.json` is a single default, not a catalog. Seed
/// these the same way Codex seeds slugs.
pub(crate) const CLAUDE_MODELS: &[&str] = &[
    "claude-fable-5-1",
    "claude-opus-5-5",
    "claude-sonnet-5-5",
    "claude-haiku-5-5",
    "claude-opus-5",
    "claude-sonnet-5",
    "claude-fable-5",
    "claude-opus-4-8",
    "claude-opus-4-7",
    "claude-sonnet-4-6",
    "claude-opus-4-6",
    "claude-opus-4-5-20251101",
    "claude-haiku-4-5-20251001",
    "claude-sonnet-4-5-20250929",
];

/// Claude `/model` help aliases (Rs-MBP `claude /model`). Secondary
/// rows — the picker leads with [`CLAUDE_MODELS`]. Never `sonnet[1m]`
/// / `opus[1m]` / `fable[1m]` — those are SGR crumbs (`ESC[1m`).
pub(crate) const CLAUDE_ALIASES: &[&str] =
    &["sonnet", "opus", "haiku", "fable", "best", "opusplan"];

/// Last-resort Grok ids when `grok models` has not been cached yet.
/// The strip and picker use a real id from this list — never prose
/// headers (`Available`, `You`, `Default`, `Models`).
pub(crate) const GROK_MODELS: &[&str] =
    &["grok-4.7", "grok-4.7-build-fast", "grok-4.6", "grok-4.5"];

/// Last-resort Codex ids when `~/.codex/models_cache.json` (and the
/// picker catalog) are missing. `codex models` is a TUI — never spawn it.
pub(crate) const CODEX_ALIASES: &[&str] = &[
    "gpt-6.1-sol",
    "gpt-6.1-terra",
    "gpt-6-sol",
    "gpt-6-astra",
    "gpt-6-luna",
    "gpt-5.6-sol",
    "gpt-5.6-terra",
    "gpt-5.6-luna",
    "gpt-5-codex",
];

/// `$CODEX_HOME` if set, else `$HOME/.codex`.
pub(crate) fn codex_catalog_home() -> Option<PathBuf> {
    if let Some(h) = std::env::var_os("CODEX_HOME").filter(|h| !h.is_empty()) {
        return Some(PathBuf::from(h));
    }
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".codex"))
}

/// Slugs from a Codex on-disk catalog (`models[].slug` only).
pub(crate) fn parse_codex_catalog(text: &str) -> Vec<String> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(text) else {
        return Vec::new();
    };
    let Some(models) = v.get("models").and_then(|m| m.as_array()) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for m in models {
        let Some(slug) = m.get("slug").and_then(|s| s.as_str()) else {
            continue;
        };
        let slug = slug.trim();
        if accept_model_name("codex", slug) {
            push_id(&mut out, slug);
        }
    }
    out
}

/// Preferred: `home/models_cache.json`. Fallback:
/// `home/model-catalogs/codex_picker_catalog.json`.
pub(crate) fn codex_catalog_slugs_in(home: &Path) -> Vec<String> {
    let cache = home.join("models_cache.json");
    if let Ok(text) = std::fs::read_to_string(&cache) {
        let slugs = parse_codex_catalog(&text);
        if !slugs.is_empty() {
            return slugs;
        }
    }
    let picker = home
        .join("model-catalogs")
        .join("codex_picker_catalog.json");
    if let Ok(text) = std::fs::read_to_string(&picker) {
        let slugs = parse_codex_catalog(&text);
        if !slugs.is_empty() {
            return slugs;
        }
    }
    Vec::new()
}

fn codex_catalog_slugs() -> Vec<String> {
    codex_catalog_home()
        .map(|h| codex_catalog_slugs_in(&h))
        .unwrap_or_default()
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
    let current =
        session_model_for(&shared.db, key, &pname).filter(|n| accept_model_name(&kind, n));
    let mut out: Vec<ModelChoice> = Vec::new();
    let mut push = |name: String, tag: &str| {
        let name = name.trim().to_string();
        if !accept_model_name(&kind, &name) {
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
    if let Some(m) = last_run_model_for(shared, key, &pname) {
        push(m, "last-run");
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
    if kind == "grok" {
        for id in GROK_MODELS {
            push((*id).to_string(), "catalog");
        }
    }
    if kind == "claude" {
        for id in CLAUDE_MODELS {
            push((*id).to_string(), "catalog");
        }
        for a in CLAUDE_ALIASES {
            push((*a).to_string(), "alias");
        }
    }
    if kind == "codex" {
        let slugs = codex_catalog_slugs();
        if slugs.is_empty() {
            for a in CODEX_ALIASES {
                push((*a).to_string(), "alias");
            }
        } else {
            for s in slugs {
                push(s, "catalog");
            }
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
    if kind == "claude" {
        out = primary_then_aliases(out);
    }
    out
}

/// Full Claude ids stay ahead of short aliases, even when the session
/// / config current is `opus` / `sonnet` / …
fn primary_then_aliases(list: Vec<ModelChoice>) -> Vec<ModelChoice> {
    let (primary, secondary): (Vec<_>, Vec<_>) = list.into_iter().partition(|m| {
        !CLAUDE_ALIASES
            .iter()
            .any(|a| a.eq_ignore_ascii_case(&m.name))
    });
    let mut out = primary;
    out.extend(secondary);
    out
}

/// Strip / footer model: first real id among session, last-run, config,
/// detect, then this kind's catalog seed. Never prose headers.
pub(crate) fn resolve_display_model(
    kind: &str,
    cli: Option<&Path>,
    candidates: &[Option<String>],
) -> String {
    for c in candidates {
        let Some(raw) = c.as_deref() else {
            continue;
        };
        let name = crate::detect::strip_ansi(raw);
        let name = name.trim();
        if accept_model_name(kind, name) {
            return name.to_string();
        }
    }
    if let Some(cli) = cli {
        if let Some(id) = cached_cli_model_ids(kind, cli)
            .into_iter()
            .find(|n| accept_model_name(kind, n))
        {
            return id;
        }
    }
    for id in kind_seed_models(kind) {
        if accept_model_name(kind, id) {
            return (*id).to_string();
        }
    }
    "—".into()
}

fn kind_seed_models(kind: &str) -> &'static [&'static str] {
    match kind {
        "grok" => GROK_MODELS,
        "claude" => CLAUDE_MODELS,
        "codex" => CODEX_ALIASES,
        _ => &[],
    }
}

/// Model ids from a provider `models` CLI (or help text we must not trust).
///
/// Shapes (Rs-MBP live captures):
/// * Grok: login banner + `Default model: id` + `Available models:` + bullets
/// * Cursor / agent: header `Available models` then `id - Display Name`
/// * OpenCode: one `provider/model` per line
/// * Claude `/model` help and Codex TUI errors are noise — no junk rows
pub(crate) fn parse_cli_model_ids(text: &str) -> Vec<String> {
    let text = crate::detect::strip_ansi(text);
    let mut out = crate::detect::parse_models(&text);
    for line in text.lines() {
        if is_help_or_noise_line(line) {
            continue;
        }
        let line = line.trim();
        if let Some(id) = cursor_id_from_label_line(line) {
            push_id(&mut out, &id);
            continue;
        }
        if let Some(id) = labeled_default_model(line) {
            push_id(&mut out, &id);
            continue;
        }
        let line = line
            .trim_start_matches(['*', '-', '•', '·', '–', '—'])
            .trim();
        for raw in line.split_whitespace() {
            let tok = normalize_model_token(raw);
            if crate::detect::has_csi_leftover(tok) {
                continue;
            }
            if accept_model_name("", tok) {
                push_id(&mut out, tok);
            }
        }
    }
    out
}

fn push_id(out: &mut Vec<String>, id: &str) {
    if !out.iter().any(|e| e == id) {
        out.push(id.to_string());
    }
}

/// Cursor / agent: `composer-2.5 - Composer 2.5` → `composer-2.5`.
fn cursor_id_from_label_line(line: &str) -> Option<String> {
    let (id, label) = line.split_once(" - ")?;
    if label.trim().is_empty() {
        return None;
    }
    let id = normalize_model_token(id.trim());
    accept_model_name("cursor", id).then(|| id.to_string())
}

/// `Default model: grok-4.7`
fn labeled_default_model(line: &str) -> Option<String> {
    let (left, right) = line.split_once(':')?;
    let left = left.trim().to_ascii_lowercase();
    if left != "default model" && left != "current model" {
        return None;
    }
    let tok = normalize_model_token(right.split_whitespace().next().unwrap_or(""));
    accept_model_name("", tok).then(|| tok.to_string())
}

fn is_help_or_noise_line(line: &str) -> bool {
    let t = line.trim();
    if t.is_empty() || t.starts_with('#') || t.starts_with("===") {
        return true;
    }
    let lower = t.to_ascii_lowercase();
    lower.starts_with("available models")
        || lower.starts_with("usage:")
        || lower.starts_with("usage ")
        || lower.starts_with("description")
        || lower.starts_with("arguments")
        || lower.starts_with("flags")
        || lower.starts_with("global flags")
        || lower.starts_with("subcommands")
        || lower.starts_with("error")
        || lower.starts_with("(exit")
        || lower.starts_with("ai stack")
        || lower.contains("bash_profile")
        || lower.contains("refusing to start")
        || lower.contains("unexpected positional")
}

fn normalize_model_token(s: &str) -> &str {
    s.trim_matches(|c: char| {
        !c.is_ascii_alphanumeric() && !matches!(c, '-' | '_' | '.' | '/' | ':')
    })
}

pub(crate) fn accept_model_name(kind: &str, name: &str) -> bool {
    let stripped = crate::detect::strip_ansi(name);
    let name = stripped.trim();
    if name.is_empty() || crate::detect::has_csi_leftover(name) {
        return false;
    }
    if kind == "codex" && is_codex_non_chat_id(name) {
        return false;
    }
    if is_placeholder_model(name) || is_junk_model_label(name) {
        return is_kind_alias(kind, name);
    }
    is_kind_alias(kind, name) || looks_like_cli_model_id(name)
}

/// Codex slugs that are not interactive chat models (auto-review,
/// tool-only, non-interactive). Keep `gpt-*` / chat ids.
fn is_codex_non_chat_id(name: &str) -> bool {
    let t = name.trim().to_ascii_lowercase();
    t.contains("auto-review")
        || t.contains("auto_review")
        || t.contains("tool-only")
        || t.contains("tool_only")
        || t.contains("non-interactive")
        || t.contains("noninteractive")
}

fn is_placeholder_model(name: &str) -> bool {
    matches!(name.trim(), "" | "-" | "—" | "*" | "–")
}

fn is_junk_model_label(name: &str) -> bool {
    let t = name.trim().trim_end_matches(':').to_ascii_lowercase();
    if t.is_empty() || t.contains(char::is_whitespace) {
        return true;
    }
    matches!(
        t.as_str(),
        "you"
            | "are"
            | "logged"
            | "in"
            | "with"
            | "default"
            | "available"
            | "models"
            | "model"
            | "usage"
            | "current"
            | "auto-approve"
            | "directory"
    )
}

fn is_kind_alias(kind: &str, name: &str) -> bool {
    match kind {
        "claude" => CLAUDE_MODELS
            .iter()
            .chain(CLAUDE_ALIASES)
            .any(|a| a.eq_ignore_ascii_case(name)),
        "cursor" => name.eq_ignore_ascii_case("auto"),
        _ => name.eq_ignore_ascii_case("auto"),
    }
}

fn looks_like_cli_model_id(s: &str) -> bool {
    if s.len() < 2 || s.starts_with('/') || crate::detect::has_csi_leftover(s) {
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
    // Lone versions (`5.5` from `Opus 5.5`) are not ids.
    if s.chars().all(|c| c.is_ascii_digit() || c == '.') {
        return false;
    }
    s.contains('-') || s.contains('/') || s.chars().any(|c| c.is_ascii_digit())
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
    if let Some(m) = session_model_for(&shared.db, key, &pname) {
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
        assert_eq!(models_argv("cursor"), Some(&["models"][..]));
        assert_eq!(models_argv("claude"), None);
        assert_eq!(models_argv("codex"), None);
    }

    fn assert_no_junk(ids: &[String]) {
        for bad in [
            "Available",
            "models",
            "Available models",
            "You",
            "Default",
            "Composer 2.5",
            "Auto (default)",
            "Usage",
            "-",
        ] {
            assert!(
                !ids.iter().any(|s| s.eq_ignore_ascii_case(bad)),
                "junk {bad:?} leaked into {ids:?}"
            );
        }
        assert!(
            !ids.iter().any(|s| s.contains(' ')),
            "display phrase leaked into {ids:?}"
        );
    }

    #[test]
    fn parse_cursor_agent_models_live_sample() {
        let ids = parse_cli_model_ids(include_str!(
            "../../tests/fixtures/models/cursor-agent_models.txt"
        ));
        assert!(ids.iter().any(|s| s == "composer-2.5"), "{ids:?}");
        assert!(ids.iter().any(|s| s == "auto"), "{ids:?}");
        assert!(ids.iter().any(|s| s == "gpt-5.6-sol-high"), "{ids:?}");
        assert!(ids.iter().any(|s| s == "composer-2.5-fast"), "{ids:?}");
        assert_no_junk(&ids);
        let agent =
            parse_cli_model_ids(include_str!("../../tests/fixtures/models/agent_models.txt"));
        assert!(agent.iter().any(|s| s == "auto"), "{agent:?}");
        assert!(agent.iter().any(|s| s == "composer-2.5"), "{agent:?}");
        assert_no_junk(&agent);
    }

    #[test]
    fn parse_live_grok_and_opencode_samples() {
        let grok = parse_cli_model_ids(include_str!("../../tests/fixtures/models/grok_models.txt"));
        assert!(grok.iter().any(|s| s == "grok-4.7"), "{grok:?}");
        assert!(grok.iter().any(|s| s == "grok-4.7-build-fast"), "{grok:?}");
        assert_no_junk(&grok);
        let oc = parse_cli_model_ids(include_str!(
            "../../tests/fixtures/models/opencode_models.txt"
        ));
        assert!(oc.iter().any(|s| s == "cursorapi/composer-2.5"), "{oc:?}");
        assert!(oc.iter().any(|s| s == "openai/gpt-5.6"), "{oc:?}");
        assert_no_junk(&oc);
    }

    #[test]
    fn parse_claude_help_and_codex_tui_are_not_catalogs() {
        let claude = parse_cli_model_ids(include_str!(
            "../../tests/fixtures/models/claude__model.txt"
        ));
        assert_no_junk(&claude);
        assert!(
            claude.is_empty(),
            "claude /model help must not become picker rows: {claude:?}"
        );
        let hung = parse_cli_model_ids(include_str!(
            "../../tests/fixtures/models/claude_models.txt"
        ));
        assert!(hung.is_empty(), "{hung:?}");
        let codex =
            parse_cli_model_ids(include_str!("../../tests/fixtures/models/codex_models.txt"));
        assert!(
            codex.is_empty(),
            "codex TUI error is not a catalog: {codex:?}"
        );
        assert_no_junk(&codex);
    }

    #[test]
    fn claude_catalog_is_full_ids_not_only_aliases() {
        let ids: Vec<&str> = include_str!("../../tests/fixtures/models/claude_ids.txt")
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .collect();
        assert_eq!(ids, CLAUDE_MODELS);
        assert!(ids.len() > 1, "{ids:?}");
        for id in CLAUDE_MODELS {
            assert!(
                id.starts_with("claude-") && id.contains('-'),
                "expected a full Claude id, got {id}"
            );
            assert!(accept_model_name("claude", id), "{id}");
            assert!(
                !CLAUDE_ALIASES.iter().any(|a| a.eq_ignore_ascii_case(id)),
                "full id {id} must not be a short alias"
            );
        }
        assert!(CLAUDE_MODELS.contains(&"claude-sonnet-5-5"));
        assert!(CLAUDE_MODELS.contains(&"claude-opus-5-5"));
        assert!(CLAUDE_MODELS.contains(&"claude-haiku-5-5"));
        assert!(CLAUDE_MODELS.contains(&"claude-fable-5-1"));
    }

    #[test]
    fn codex_fallback_is_more_than_config_placeholder() {
        let ids: Vec<&str> = include_str!("../../tests/fixtures/models/codex_aliases.txt")
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .collect();
        assert!(ids.len() > 1, "{ids:?}");
        assert!(ids.contains(&"gpt-6.1-sol"), "{ids:?}");
        assert!(ids.contains(&"gpt-6-luna"), "{ids:?}");
        assert_eq!(ids, CODEX_ALIASES);
        for id in CODEX_ALIASES {
            assert!(accept_model_name("codex", id), "{id}");
        }
        assert!(CODEX_ALIASES.len() > 1);
    }

    #[test]
    fn parse_codex_models_cache_slugs_only() {
        let ids = parse_codex_catalog(include_str!(
            "../../tests/fixtures/models/codex_models_cache.json"
        ));
        assert!(ids.len() >= 5, "{ids:?}");
        assert!(ids.len() > 1, "must not be a single config row: {ids:?}");
        for want in [
            "gpt-6.1-sol",
            "gpt-6-astra",
            "gpt-6.1-terra",
            "gpt-6-sol",
            "gpt-6-luna",
            "gpt-5.6-sol",
            "gpt-5.6-terra",
            "gpt-5.6-luna",
            "gpt-5.5",
        ] {
            assert!(ids.iter().any(|s| s == want), "missing {want} in {ids:?}");
        }
        assert!(
            !ids.iter().any(|s| s == "codex-auto-review"),
            "auto-review is not a chat model: {ids:?}"
        );
        for bad in [
            "GPT-6.1-Sol",
            "GPT-6.1 Terra",
            "GPT-6-Astra",
            "Codex Auto Review",
        ] {
            assert!(
                !ids.iter().any(|s| s == bad),
                "display_name leaked into {ids:?}"
            );
        }
        assert_no_junk(&ids);
        assert!(parse_codex_catalog("not json").is_empty());
        assert!(parse_codex_catalog("{}").is_empty());
        assert!(parse_codex_catalog(r#"{"models":[]}"#).is_empty());
    }

    #[test]
    fn parse_codex_picker_catalog_slugs() {
        let ids = parse_codex_catalog(include_str!(
            "../../tests/fixtures/models/codex_picker_catalog.json"
        ));
        assert!(ids.len() >= 5, "{ids:?}");
        assert!(ids.iter().any(|s| s == "gpt-5.5"), "{ids:?}");
        assert!(ids.iter().any(|s| s == "gpt-5.3-codex-spark"), "{ids:?}");
        assert!(
            ids.iter().any(|s| s == "clinepass/cline-pass-glm-5.2"),
            "{ids:?}"
        );
        assert!(
            !ids.iter().any(|s| s == "codex-auto-review"),
            "auto-review is not a chat model: {ids:?}"
        );
        assert_no_junk(&ids);
    }

    #[test]
    fn codex_catalog_slugs_in_prefers_cache_then_picker() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        std::fs::write(
            home.join("models_cache.json"),
            include_str!("../../tests/fixtures/models/codex_models_cache.json"),
        )
        .unwrap();
        std::fs::create_dir_all(home.join("model-catalogs")).unwrap();
        std::fs::write(
            home.join("model-catalogs")
                .join("codex_picker_catalog.json"),
            include_str!("../../tests/fixtures/models/codex_picker_catalog.json"),
        )
        .unwrap();
        let ids = codex_catalog_slugs_in(home);
        assert!(ids.len() >= 5, "{ids:?}");
        assert!(ids.iter().any(|s| s == "gpt-6.1-sol"), "{ids:?}");
        assert!(
            !ids.iter().any(|s| s == "gpt-5.4"),
            "cache must win over picker: {ids:?}"
        );
        assert!(!ids.iter().any(|s| s.contains(' ')), "{ids:?}");

        let only_picker = tempfile::tempdir().unwrap();
        let ph = only_picker.path();
        std::fs::create_dir_all(ph.join("model-catalogs")).unwrap();
        std::fs::write(
            ph.join("model-catalogs").join("codex_picker_catalog.json"),
            include_str!("../../tests/fixtures/models/codex_picker_catalog.json"),
        )
        .unwrap();
        let fallback = codex_catalog_slugs_in(ph);
        assert!(fallback.iter().any(|s| s == "gpt-5.5"), "{fallback:?}");
        assert!(fallback.len() > 1, "{fallback:?}");

        let empty = tempfile::tempdir().unwrap();
        assert!(codex_catalog_slugs_in(empty.path()).is_empty());
    }

    #[test]
    fn reject_placeholder_config_model() {
        assert!(!accept_model_name("cursor", "-"));
        assert!(!accept_model_name("claude", ""));
        assert!(!accept_model_name("grok", "Available"));
        assert!(!accept_model_name("grok", "Available models"));
        assert!(!accept_model_name("grok", "You"));
        assert!(!accept_model_name("grok", "Default"));
        assert!(!accept_model_name("grok", "Models"));
        assert!(!accept_model_name("codex", "codex-auto-review"));
        assert!(!accept_model_name("codex", "gpt-5.5-auto-review"));
        assert!(accept_model_name("codex", "gpt-6.1-sol"));
        assert!(accept_model_name("cursor", "auto"));
        assert!(accept_model_name("cursor", "composer-2.5"));
        assert!(accept_model_name("claude", "sonnet"));
        assert!(
            !accept_model_name("claude", "sonnet[1m]"),
            "SGR crumb must never be a picker row"
        );
        assert!(!accept_model_name("claude", "opus[1m]"));
        assert!(!accept_model_name("claude", "fable[1m]"));
        assert!(accept_model_name("claude", "claude-sonnet-5-5"));
        assert!(accept_model_name("claude", "claude-opus-5-5"));
        assert!(accept_model_name("opencode", "moonshotai/kimi-k2.6"));
    }

    #[test]
    fn parse_cli_model_ids_strips_ansi_and_rejects_sgr_crumbs() {
        let ids = parse_cli_model_ids(
            "\u{1b}[1mAvailable models:\u{1b}[0m\n  * \u{1b}[1mgrok-4.7\u{1b}[0m (default)\n  - sonnet[1m]\n  - opus[1m]\n  - fable[1m]\n  - grok-4.6\n",
        );
        assert!(ids.iter().any(|s| s == "grok-4.7"), "{ids:?}");
        assert!(ids.iter().any(|s| s == "grok-4.6"), "{ids:?}");
        for bad in ["Available", "sonnet[1m]", "opus[1m]", "fable[1m]", "You"] {
            assert!(
                !ids.iter().any(|s| s.eq_ignore_ascii_case(bad)),
                "junk {bad:?} leaked into {ids:?}"
            );
        }
        assert!(
            !ids.iter()
                .any(|s| s.contains("[1m]") || s.contains('\u{1b}')),
            "{ids:?}"
        );
    }

    #[test]
    fn resolve_display_model_never_uses_prose_headers() {
        let grok = resolve_display_model(
            "grok",
            None,
            &[
                Some("Available".into()),
                Some("You".into()),
                Some("Default".into()),
                Some("Models".into()),
                Some("".into()),
            ],
        );
        assert_eq!(grok, "grok-4.7");
        assert_ne!(grok.to_ascii_lowercase(), "available");
        let from_session = resolve_display_model(
            "grok",
            None,
            &[Some("grok-4.7-build-fast".into()), Some("Available".into())],
        );
        assert_eq!(from_session, "grok-4.7-build-fast");
        let claude = resolve_display_model("claude", None, &[Some("Available".into())]);
        assert!(claude.starts_with("claude-"), "{claude}");
    }

    #[test]
    fn claude_aliases_are_secondary_and_exclude_sgr_crumbs() {
        for crumb in ["sonnet[1m]", "opus[1m]", "fable[1m]"] {
            assert!(
                !CLAUDE_ALIASES.iter().any(|a| a.eq_ignore_ascii_case(crumb)),
                "{crumb} must not be a Claude alias"
            );
        }
        let mixed = vec![
            ModelChoice {
                name: "opus".into(),
                tag: "session".into(),
                current: true,
            },
            ModelChoice {
                name: "claude-sonnet-5-5".into(),
                tag: "catalog".into(),
                current: false,
            },
            ModelChoice {
                name: "sonnet".into(),
                tag: "alias".into(),
                current: false,
            },
            ModelChoice {
                name: "claude-opus-5-5".into(),
                tag: "catalog".into(),
                current: false,
            },
        ];
        let ordered = primary_then_aliases(mixed);
        let names: Vec<&str> = ordered.iter().map(|m| m.name.as_str()).collect();
        let first_alias = names
            .iter()
            .position(|n| CLAUDE_ALIASES.iter().any(|a| a.eq_ignore_ascii_case(n)))
            .unwrap();
        let last_full = names
            .iter()
            .rposition(|n| n.starts_with("claude-"))
            .unwrap();
        assert!(
            last_full < first_alias,
            "full ids must precede aliases: {names:?}"
        );
        assert_eq!(names[0], "claude-sonnet-5-5");
        assert!(names.contains(&"opus"));
        assert!(ordered.iter().any(|m| m.name == "opus" && m.current));
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
