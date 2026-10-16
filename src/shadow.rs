use crate::app::Shared;
use crate::config::{expand_path, Config};
use crate::db::{now_rfc3339, Db, ShadowResult};
use crate::handoff;
use crate::providers::cache_hit_ratio;
use anyhow::{bail, Context, Result};
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use serde::Deserialize;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;
use tokio::process::Command;

#[derive(Debug, Deserialize)]
struct Label {
    name: String,
}

#[derive(Debug, Deserialize)]
struct Issue {
    number: i64,
    title: String,
    body: Option<String>,
    #[serde(default)]
    labels: Vec<Label>,
}

#[derive(Debug, Deserialize)]
struct Comment {
    id: i64,
    body: Option<String>,
    user: Option<User>,
}

#[derive(Debug, Deserialize)]
struct User {
    login: Option<String>,
    #[serde(rename = "type")]
    kind: Option<String>,
}

pub fn shadow_key(cfg: &Config, issue: i64) -> String {
    let prefix = if cfg.shadow.key_prefix.is_empty() {
        format!("{}-shadow", cfg.task_key_prefix)
    } else {
        cfg.shadow.key_prefix.clone()
    };
    format!("{prefix}-{issue}")
}

pub fn shadow_repo(cfg: &Config) -> String {
    if cfg.shadow.repo.is_empty() {
        cfg.github.repo.clone()
    } else {
        cfg.shadow.repo.clone()
    }
}

pub fn shadow_labels(cfg: &Config) -> Vec<String> {
    if cfg.shadow.labels.is_empty() {
        if cfg.github.label.is_empty() {
            Vec::new()
        } else {
            vec![cfg.github.label.clone()]
        }
    } else {
        cfg.shadow.labels.clone()
    }
}

/// True when `gh` args would write to GitHub (issues, PRs, labels, mutating api).
pub fn gh_is_write(args: &[impl AsRef<str>]) -> bool {
    let args: Vec<&str> = args.iter().map(|s| s.as_ref()).collect();
    for w in args.windows(2) {
        if w[0] == "-X" || w[0] == "--method" {
            let m = w[1].to_ascii_uppercase();
            if matches!(m.as_str(), "POST" | "PUT" | "PATCH" | "DELETE") {
                return true;
            }
        }
    }
    let cmd = args.first().copied().unwrap_or("");
    let sub = args.get(1).copied().unwrap_or("");
    matches!(
        (cmd, sub),
        (
            "issue",
            "comment"
                | "edit"
                | "create"
                | "delete"
                | "close"
                | "reopen"
                | "develop"
                | "transfer"
                | "pin"
                | "unpin"
        ) | (
            "pr",
            "create" | "comment" | "edit" | "merge" | "close" | "ready" | "review"
        ) | ("label", "create" | "delete" | "edit" | "clone")
    )
}

/// Writes plus token-leaking `gh auth` forms the wrapper must refuse.
pub fn gh_is_forbidden(args: &[impl AsRef<str>]) -> bool {
    if gh_is_write(args) {
        return true;
    }
    let args: Vec<&str> = args.iter().map(|s| s.as_ref()).collect();
    let cmd = args.first().copied().unwrap_or("");
    let sub = args.get(1).copied().unwrap_or("");
    if cmd == "auth" && sub == "token" {
        return true;
    }
    if cmd == "auth" && sub == "status" {
        return args.contains(&"--show-token");
    }
    false
}

/// True when `path` is this crate's PATH-shadow `gh` wrapper.
pub fn is_shadow_wrapper(path: &Path) -> bool {
    let s = path.to_string_lossy();
    if s.contains("/.atr/bin/gh") || s.ends_with(".atr/bin/gh") {
        return true;
    }
    match std::fs::read_to_string(path) {
        Ok(text) => text.contains("ATR_SHADOW_GH"),
        Err(_) => false,
    }
}

/// Resolve `github.cli` to an absolute binary that is not the wrapper.
pub fn resolve_github_cli(cli: &str) -> Result<PathBuf> {
    let path = std::env::var("PATH").unwrap_or_default();
    resolve_github_cli_in_path(cli, &path)
}

pub fn resolve_github_cli_in_path(cli: &str, path: &str) -> Result<PathBuf> {
    let p = expand_path(cli);
    if p.is_absolute() {
        if !p.exists() {
            bail!("github.cli {} does not exist", p.display());
        }
        if is_shadow_wrapper(&p) {
            bail!(
                "github.cli {} is the atr shadow gh wrapper; set [github] cli to the real gh binary",
                p.display()
            );
        }
        return Ok(p.canonicalize().unwrap_or(p));
    }
    for dir in path.split(':') {
        if dir.is_empty() {
            continue;
        }
        let cand = PathBuf::from(dir).join(&p);
        if !cand.is_file() {
            continue;
        }
        if is_shadow_wrapper(&cand) {
            continue;
        }
        return Ok(cand.canonicalize().unwrap_or(cand));
    }
    bail!("could not resolve github.cli {cli:?} to a real gh outside the shadow wrapper");
}

pub fn validate_github_cli(cli: &str) -> Result<PathBuf> {
    resolve_github_cli(cli)
}

/// Token used by shadow `gh` calls: the shared atr GitHub token
/// (`ATR_GH_TOKEN`, then `[shadow] gh_token_env`, then `GH_TOKEN`).
pub fn shadow_token(cfg: &Config) -> Option<String> {
    crate::gh_token::resolve_for(cfg, None, Some(&shadow_repo(cfg))).map(|(_, v)| v)
}

fn apply_shadow_gh_env(cmd: &mut Command, cfg: &Config, repo: &str) {
    crate::gh_token::apply(cmd, cfg, None, Some(repo));
}

fn apply_shadow_gh_env_std(cmd: &mut std::process::Command, cfg: &Config, repo: &str) {
    crate::gh_token::apply_std(cmd, cfg, None, Some(repo));
}

pub fn shadow_repo_path(cfg: &Config) -> PathBuf {
    if !cfg.shadow.repo_path.as_os_str().is_empty() {
        return cfg.shadow.repo_path.clone();
    }
    let repo = shadow_repo(cfg);
    let name = repo
        .rsplit('/')
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or("shadow-src");
    cfg.worktree_root.join("_src").join(name)
}

/// Create or fetch the per-shadow local clone using the read-only poller token.
pub fn ensure_shadow_repo(cfg: &Config) -> Result<PathBuf> {
    let dest = shadow_repo_path(cfg);
    if dest.join(".git").exists() || dest.join(".git").is_file() {
        fetch_shadow_repo(cfg, &dest);
        return Ok(dest);
    }
    let repo = shadow_repo(cfg);
    if repo.is_empty() {
        bail!("shadow.repo is empty; cannot clone a local shadow source");
    }
    clone_shadow_repo(cfg, &repo, &dest)?;
    Ok(dest)
}

fn clone_shadow_repo(cfg: &Config, repo: &str, dest: &Path) -> Result<()> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    if let Ok(gh) = resolve_github_cli(&cfg.github.cli) {
        let dest_s = dest.display().to_string();
        let mut cmd = std::process::Command::new(&gh);
        cmd.args(["repo", "clone", repo, &dest_s]);
        apply_shadow_gh_env_std(&mut cmd, cfg, repo);
        if cmd.status().map(|s| s.success()).unwrap_or(false)
            && (dest.join(".git").exists() || dest.join(".git").is_file())
        {
            return Ok(());
        }
    }
    git_clone_github(cfg, repo, dest)?;
    if dest.join(".git").exists() || dest.join(".git").is_file() {
        Ok(())
    } else {
        bail!(
            "shadow clone did not produce a git repo at {}",
            dest.display()
        )
    }
}

fn git_clone_github(cfg: &Config, repo: &str, dest: &Path) -> Result<()> {
    let url = if repo.starts_with("http://")
        || repo.starts_with("https://")
        || repo.starts_with("git@")
    {
        repo.to_string()
    } else {
        format!("https://github.com/{repo}.git")
    };
    let mut cmd = std::process::Command::new("git");
    if let Some(tok) = shadow_token(cfg) {
        cmd.arg("-c")
            .arg(format!("http.extraHeader=Authorization: Bearer {tok}"));
    }
    cmd.args(["clone", "--", &url, &dest.display().to_string()]);
    let st = cmd.status().context("git clone")?;
    if !st.success() {
        bail!("git clone {url} failed ({st})");
    }
    Ok(())
}

fn fetch_shadow_repo(cfg: &Config, dest: &Path) {
    let mut cmd = std::process::Command::new("git");
    cmd.current_dir(dest);
    if let Some(tok) = shadow_token(cfg) {
        cmd.arg("-c")
            .arg(format!("http.extraHeader=Authorization: Bearer {tok}"));
    }
    cmd.args(["fetch", "--all", "--prune"]);
    let _ = cmd.status();
}

pub fn write_gh_wrapper(dir: &Path) -> Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join("gh");
    std::fs::write(&path, GH_WRAPPER)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))?;
    }
    Ok(path)
}

const GH_WRAPPER: &str = r#"#!/bin/sh
set -eu
if [ -n "${ATR_SHADOW_GH_NESTED:-}" ]; then
  echo "atr shadow: gh wrapper recursion (ATR_SHADOW_GH_NESTED)" >&2
  exit 3
fi
real="${ATR_SHADOW_GH_REAL:-}"
if [ -z "$real" ]; then
  echo "atr shadow: ATR_SHADOW_GH_REAL is unset" >&2
  exit 3
fi
case "$real" in
  /*) ;;
  *)
    echo "atr shadow: ATR_SHADOW_GH_REAL must be an absolute path (got $real)" >&2
    exit 3
    ;;
esac
self=$(cd "$(dirname "$0")" && pwd)/$(basename "$0")
if [ "$real" = "$self" ]; then
  echo "atr shadow: refusing to exec the wrapper as gh" >&2
  exit 3
fi
export ATR_SHADOW_GH_NESTED=1
log="${ATR_SHADOW_GH_LOG:-}"
if [ -n "$log" ]; then
  mkdir -p "$(dirname "$log")"
  printf '%s\n' "$*" >> "$log"
fi
forbid=0
prev=""
for a in "$@"; do
  ua=$(printf '%s' "$a" | tr '[:lower:]' '[:upper:]')
  if [ "$prev" = "-X" ] || [ "$prev" = "--method" ]; then
    case "$ua" in POST|PUT|PATCH|DELETE) forbid=1 ;; esac
  fi
  prev="$a"
done
cmd="${1:-}"
sub="${2:-}"
case "$cmd $sub" in
  "issue comment"|"issue edit"|"issue create"|"issue delete"|"issue close"|"issue reopen"|"issue develop"|"issue transfer"|"issue pin"|"issue unpin"|"pr create"|"pr comment"|"pr edit"|"pr merge"|"pr close"|"pr ready"|"pr review"|"label create"|"label delete"|"label edit"|"label clone"|"auth token")
    forbid=1 ;;
esac
if [ "$cmd" = "auth" ] && [ "$sub" = "status" ]; then
  for a in "$@"; do
    if [ "$a" = "--show-token" ]; then
      forbid=1
    fi
  done
fi
if [ "$forbid" -eq 1 ]; then
  echo "atr shadow: refusing GitHub write or token leak: gh $*" >&2
  exit 2
fi
exec "$real" "$@"
"#;

pub async fn poll_loop(shared: Arc<Shared>) {
    loop {
        let (enabled, interval) = {
            let cfg = shared.config.read().unwrap();
            (cfg.shadow.enabled, cfg.shadow.poll_interval)
        };
        if enabled {
            if let Err(e) = poll_once(&shared).await {
                tracing::warn!("shadow poll failed: {e:#}");
            }
        }
        tokio::time::sleep(interval.max(Duration::from_secs(5))).await;
    }
}

pub async fn poll_once(shared: &Shared) -> Result<()> {
    if shared.intake_paused.load(Ordering::Relaxed) {
        tracing::debug!("intake paused; skip shadow poll");
        return Ok(());
    }
    let cfg = shared.config.read().unwrap().clone();
    if !cfg.shadow.enabled {
        return Ok(());
    }
    let repo = shadow_repo(&cfg);
    let labels = shadow_labels(&cfg);
    if repo.is_empty() || labels.is_empty() {
        return Ok(());
    }
    let gh = resolve_github_cli(&cfg.github.cli)
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| cfg.github.cli.clone());
    let repo_path = ensure_shadow_repo(&cfg)?.display().to_string();
    let mut seen = std::collections::HashSet::new();
    for label in &labels {
        let list_args = [
            "issue",
            "list",
            "--repo",
            repo.as_str(),
            "--label",
            label.as_str(),
            "--state",
            "open",
            "--limit",
            "50",
            "--json",
            "number,title,body,labels",
        ];
        if gh_is_forbidden(&list_args) {
            anyhow::bail!("shadow poller refused a write-shaped gh invocation");
        }
        let mut cmd = Command::new(&gh);
        cmd.args(list_args);
        apply_shadow_gh_env(&mut cmd, &cfg, &repo);
        let out = cmd.output().await?;
        crate::gh_token::observe_gh(
            &cfg,
            Some(&shared.config_path),
            Some(&repo),
            out.status.success(),
            &String::from_utf8_lossy(&out.stderr),
            "shadow issue list",
        );
        if !out.status.success() {
            anyhow::bail!(
                "gh issue list failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        let issues: Vec<Issue> = serde_json::from_slice(&out.stdout).unwrap_or_default();
        for issue in issues {
            if !seen.insert(issue.number) {
                continue;
            }
            let key = shadow_key(&cfg, issue.number);
            let existed = shared.db.get_task(&key)?.is_some();
            if !existed {
                let body = format!(
                    "# {}\n\n{}\n\nSource: GitHub {repo}#{} (shadow)",
                    issue.title,
                    issue.body.unwrap_or_default(),
                    issue.number
                );
                shared.submit(
                    &key,
                    &body,
                    "shadow",
                    Some(&repo_path),
                    Some(issue.number),
                    true,
                    false,
                )?;
            } else if !shared.db.is_shadow(&key)? {
                shared.db.mark_shadow(&key)?;
            }
            let names: Vec<_> = issue.labels.iter().map(|l| l.name.as_str()).collect();
            if !names.is_empty() {
                let _ = shared.db.set_task_labels(&key, &names.join(","));
            }
            if let Err(e) =
                ingest_comments(shared, &cfg, &gh, &repo, &repo_path, &key, issue.number).await
            {
                tracing::debug!(task = %key, "shadow comment poll: {e:#}");
            }
        }
    }
    Ok(())
}

async fn ingest_comments(
    shared: &Shared,
    cfg: &Config,
    gh: &str,
    repo: &str,
    repo_path: &str,
    key: &str,
    number: i64,
) -> Result<()> {
    let path = format!("repos/{repo}/issues/{number}/comments");
    let args = ["api", path.as_str(), "--paginate"];
    if gh_is_forbidden(&args) {
        return Ok(());
    }
    let mut cmd = Command::new(gh);
    cmd.args(args);
    apply_shadow_gh_env(&mut cmd, cfg, repo);
    let out = cmd.output().await?;
    if !out.status.success() {
        return Ok(());
    }
    let comments: Vec<Comment> = serde_json::from_slice(&out.stdout).unwrap_or_default();
    let cursor = shared
        .db
        .comment_cursor(key)?
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(0);
    let mut max_id = cursor;
    for c in comments {
        if c.id <= cursor {
            continue;
        }
        max_id = max_id.max(c.id);
        if c.user
            .as_ref()
            .and_then(|u| u.kind.as_deref())
            .is_some_and(|t| t.eq_ignore_ascii_case("bot"))
        {
            continue;
        }
        let login = c
            .user
            .as_ref()
            .and_then(|u| u.login.clone())
            .unwrap_or_else(|| "unknown".into());
        let body = c.body.unwrap_or_default();
        if body.trim().is_empty() {
            continue;
        }
        let text = format!("GitHub comment by {login}:\n{body}");
        shared.submit(
            key,
            &text,
            "shadow-comment",
            Some(repo_path),
            Some(number),
            true,
            false,
        )?;
    }
    if max_id > cursor {
        shared.db.set_comment_cursor(key, &max_id.to_string())?;
    }
    Ok(())
}

pub fn record_after_run(
    cfg: &Config,
    db: &Db,
    key: &str,
    worktree: &Path,
    provider: &str,
    run_id: i64,
    outcome: &str,
    usage: &crate::providers::UsageNumbers,
) -> Result<()> {
    let run = db.run_by_id(run_id)?;
    let duration_ms = run
        .as_ref()
        .and_then(|r| duration_ms(&r.started, r.ended.as_deref()))
        .unwrap_or(0);
    let events = db.events_for_run(run_id).unwrap_or_default();
    let tests = tests_summary(&events);
    let (_, stat, mut diff) = handoff::git_snapshot(worktree, &cfg.repo_default_branch);
    let diffstat = short_diffstat(&stat);
    if diff.len() > cfg.handoff.max_bytes {
        diff.truncate(cfg.handoff.max_bytes);
        diff.push_str("…[truncated]");
    }
    let hit = cache_hit_ratio(
        cfg.provider(provider)
            .map(|p| p.kind.as_str())
            .unwrap_or(provider),
        usage,
    );
    db.insert_shadow_result(&ShadowResult {
        task_key: key.to_string(),
        run_id: Some(run_id),
        provider: provider.to_string(),
        outcome: outcome.to_string(),
        duration_ms,
        input: usage.input as i64,
        output: usage.output as i64,
        cache_read: usage.cache_read as i64,
        cache_write: usage.cache_write as i64,
        cost_usd: usage.cost_usd.unwrap_or(0.0),
        cache_hit: hit,
        tests,
        diff,
        diffstat,
        verify: db.verify_summary_for_run(run_id).unwrap_or_default(),
        recorded: now_rfc3339(),
    })?;
    Ok(())
}

fn duration_ms(started: &str, ended: Option<&str>) -> Option<i64> {
    let start = DateTime::parse_from_rfc3339(started).ok()?;
    let end = ended
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .unwrap_or_else(|| Utc::now().into());
    Some(end.signed_duration_since(start).num_milliseconds().max(0))
}

fn tests_summary(events: &[crate::db::EventRow]) -> String {
    let cmds: Vec<String> = handoff::commands_from_events(events)
        .into_iter()
        .filter(|c| looks_like_test(c))
        .collect();
    let output = handoff::test_output_from_events(events);
    let mut s = String::new();
    if !cmds.is_empty() {
        s.push_str(&cmds.join("; "));
    }
    if let Some(o) = output {
        if !s.is_empty() {
            s.push('\n');
        }
        s.push_str(o.trim());
    }
    if s.is_empty() {
        "(none recorded)".into()
    } else if s.len() > 2000 {
        format!("{}…", &s[..2000])
    } else {
        s
    }
}

fn looks_like_test(cmd: &str) -> bool {
    let c = cmd.to_ascii_lowercase();
    c.contains("test")
        || c.contains("pytest")
        || c.contains("cargo t")
        || c.contains("npm test")
        || c.contains("go test")
}

pub fn parse_since(raw: &str) -> Result<String> {
    let s = raw.trim();
    if let Ok(d) = crate::duration::parse_duration(s) {
        let ts = Utc::now() - ChronoDuration::from_std(d).unwrap_or(ChronoDuration::seconds(0));
        return Ok(ts.to_rfc3339_opts(chrono::SecondsFormat::Millis, true));
    }
    DateTime::parse_from_rfc3339(s).map_err(|e| anyhow::anyhow!("--since: {e}"))?;
    Ok(s.to_string())
}

pub fn report_text(db: &Db, since: Option<&str>) -> Result<String> {
    report_text_tz(db, since, &crate::tz::DisplayTz::system())
}

pub fn report_text_tz(db: &Db, since: Option<&str>, tz: &crate::tz::DisplayTz) -> Result<String> {
    report_text_tz_usage(db, since, tz, &[])
}

pub fn report_text_tz_usage(
    db: &Db,
    since: Option<&str>,
    tz: &crate::tz::DisplayTz,
    providers: &[&str],
) -> Result<String> {
    let rows = db.list_shadow_results(since)?;
    let mut out = String::new();
    out.push_str(&format!("timezone {}\n", tz.label));
    if !providers.is_empty() {
        out.push_str(&crate::reported::format_providers_usage(
            db,
            providers,
            tz,
            crate::clock::Clock::system().now(),
        ));
    }
    out.push_str(&format!(
        "{:<22} {:<10} {:>8} {:>8} {:>8} {:>8} {:>8} {:>6} {:<10} {:<18} {:<10} {}\n",
        "task",
        "provider",
        "dur",
        "in",
        "out",
        "cache",
        "cost",
        "hit%",
        "outcome",
        "diffstat",
        "verify",
        "tests"
    ));
    let latest = latest_per_task(&rows);
    let mut tot_ms = 0i64;
    let mut tot_in = 0i64;
    let mut tot_out = 0i64;
    let mut tot_cr = 0i64;
    let mut tot_cw = 0i64;
    let mut tot_cost = 0.0;
    for r in &rows {
        tot_ms += r.duration_ms;
        tot_in += r.input;
        tot_out += r.output;
        tot_cr += r.cache_read;
        tot_cw += r.cache_write;
        tot_cost += r.cost_usd;
    }
    for r in &latest {
        let hit = r
            .cache_hit
            .map(|h| format!("{:.0}%", h * 100.0))
            .unwrap_or_else(|| "-".into());
        let tests = first_line(&r.tests);
        out.push_str(&format!(
            "{} {} {:>8} {:>8} {:>8} {:>8} {:>8.4} {:>6} {} {} {} {}\n",
            cell(&r.task_key, 22),
            cell(&r.provider, 10),
            fmt_ms(r.duration_ms),
            r.input,
            r.output,
            r.cache_read,
            r.cost_usd,
            hit,
            cell(&r.outcome, 10),
            cell(&r.diffstat, 18),
            cell(&first_line(&r.verify), 10),
            cell(&tests, 16),
        ));
    }
    let hit = cache_hit_ratio(
        "claude",
        &crate::providers::UsageNumbers {
            input: tot_in.max(0) as u64,
            output: tot_out.max(0) as u64,
            cache_read: tot_cr.max(0) as u64,
            cache_write: tot_cw.max(0) as u64,
            cost_usd: Some(tot_cost),
        },
    )
    .map(|h| format!("{:.0}%", h * 100.0))
    .unwrap_or_else(|| "-".into());
    out.push_str(&format!(
        "{} {} {:>8} {:>8} {:>8} {:>8} {:>8.4} {:>6} {} {} {} {}\n",
        cell("TOTALS", 22),
        cell(&format!("{} runs", rows.len()), 10),
        fmt_ms(tot_ms),
        tot_in,
        tot_out,
        tot_cr,
        tot_cost,
        hit,
        cell("", 10),
        cell("", 18),
        cell("", 10),
        cell("", 16),
    ));
    for r in &latest {
        if db.session_prefix_changed(&r.task_key).unwrap_or(false) {
            out.push_str(&format!("cache-prefix hash changed\t{}\n", r.task_key));
        }
    }
    Ok(out)
}

pub fn report_json(db: &Db, since: Option<&str>) -> Result<String> {
    report_json_tz(db, since, &crate::tz::DisplayTz::system())
}

pub fn report_json_tz(db: &Db, since: Option<&str>, tz: &crate::tz::DisplayTz) -> Result<String> {
    report_json_tz_usage(db, since, tz, &[])
}

pub fn report_json_tz_usage(
    db: &Db,
    since: Option<&str>,
    tz: &crate::tz::DisplayTz,
    providers: &[&str],
) -> Result<String> {
    let rows = db.list_shadow_results(since)?;
    let latest = latest_per_task(&rows);
    let mut tot_ms = 0i64;
    let mut tot_in = 0i64;
    let mut tot_out = 0i64;
    let mut tot_cr = 0i64;
    let mut tot_cw = 0i64;
    let mut tot_cost = 0.0;
    for r in &rows {
        tot_ms += r.duration_ms;
        tot_in += r.input;
        tot_out += r.output;
        tot_cr += r.cache_read;
        tot_cw += r.cache_write;
        tot_cost += r.cost_usd;
    }
    let usage: Vec<Value> = providers
        .iter()
        .map(|name| {
            json!({
                "provider": name,
                "usage": crate::reported::format_provider_usage(
                    &crate::reported::provider_usage_view(db, name),
                    tz,
                    crate::clock::Clock::system().now(),
                ),
            })
        })
        .collect();
    let v = json!({
        "timezone": tz.label,
        "since": since,
        "usage": usage,
        "tasks": latest.iter().map(|r| json!({
            "task": r.task_key,
            "run_id": r.run_id,
            "provider": r.provider,
            "outcome": r.outcome,
            "duration_ms": r.duration_ms,
            "input": r.input,
            "output": r.output,
            "cache_read": r.cache_read,
            "cache_write": r.cache_write,
            "cost_usd": r.cost_usd,
            "cache_hit": r.cache_hit,
            "tests": r.tests,
            "diff": r.diff,
            "diffstat": r.diffstat,
            "verify": r.verify,
            "recorded": r.recorded,
            "recorded_local": tz.format_str(&r.recorded),
        })).collect::<Vec<_>>(),
        "totals": {
            "runs": rows.len(),
            "duration_ms": tot_ms,
            "input": tot_in,
            "output": tot_out,
            "cache_read": tot_cr,
            "cache_write": tot_cw,
            "cost_usd": tot_cost,
        }
    });
    Ok(serde_json::to_string_pretty(&v)?)
}

fn latest_per_task(rows: &[ShadowResult]) -> Vec<ShadowResult> {
    let mut map = std::collections::BTreeMap::new();
    for r in rows {
        map.insert(r.task_key.clone(), r.clone());
    }
    map.into_values().collect()
}

fn first_line(s: &str) -> String {
    s.lines().next().unwrap_or("").to_string()
}

/// Pad or truncate to `width` chars, using an ellipsis when trimmed.
pub fn cell(s: &str, width: usize) -> String {
    let n = s.chars().count();
    if n <= width {
        format!("{s:<width$}")
    } else if width == 0 {
        String::new()
    } else if width == 1 {
        "…".into()
    } else {
        let take = width - 1;
        let mut out: String = s.chars().take(take).collect();
        out.push('…');
        out
    }
}

/// Compact list of every path in a `git diff --stat` (including untracked).
pub fn short_diffstat(stat: &str) -> String {
    let mut files = Vec::new();
    let mut summary = None;
    for line in stat.lines() {
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        if t.contains("changed")
            && (t.contains("file") || t.contains("insertion") || t.contains("deletion"))
        {
            summary = Some(t.to_string());
            continue;
        }
        let name = t.split('|').next().unwrap_or(t).trim();
        if name.is_empty() {
            continue;
        }
        let label = if t.contains("untracked") {
            format!("{name} (untracked)")
        } else {
            name.to_string()
        };
        if !files.iter().any(|f| f == &label || f == name) {
            files.push(label);
        }
    }
    if files.is_empty() && summary.is_none() {
        return "-".into();
    }
    let mut out = files.join(", ");
    if let Some(s) = summary {
        if !out.is_empty() {
            out.push_str(" | ");
        }
        out.push_str(&s);
    }
    out
}

fn fmt_ms(ms: i64) -> String {
    crate::duration::format_duration(Duration::from_millis(ms.max(0) as u64))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gh_write_detection() {
        assert!(!gh_is_write(&[
            "issue", "list", "--repo", "o/r", "--label", "x"
        ]));
        assert!(!gh_is_write(&["issue", "view", "1"]));
        assert!(!gh_is_write(&[
            "api",
            "repos/o/r/issues/1/comments",
            "--paginate"
        ]));
        assert!(gh_is_write(&["issue", "comment", "1", "--body", "hi"]));
        assert!(gh_is_write(&["pr", "create", "--title", "x"]));
        assert!(gh_is_write(&["pr", "comment", "1", "--body", "x"]));
        assert!(gh_is_write(&["issue", "edit", "1", "--add-label", "x"]));
        assert!(gh_is_write(&["label", "create", "x"]));
        assert!(gh_is_write(&[
            "api",
            "repos/o/r/issues/1/comments",
            "-X",
            "POST"
        ]));
        assert!(!gh_is_write(&[
            "api",
            "repos/o/r/issues/1",
            "--method",
            "GET"
        ]));
        assert!(gh_is_forbidden(&["auth", "token"]));
        assert!(gh_is_forbidden(&["auth", "status", "--show-token"]));
        assert!(!gh_is_forbidden(&["auth", "status"]));
        assert!(!gh_is_forbidden(&["issue", "view", "1"]));
    }

    #[test]
    fn wrapper_refuses_writes() {
        let dir = tempfile::tempdir().unwrap();
        let gh = write_gh_wrapper(dir.path()).unwrap();
        let log = dir.path().join("gh.log");
        let st = std::process::Command::new(&gh)
            .args(["issue", "comment", "1", "--body", "nope"])
            .env("ATR_SHADOW_GH_LOG", &log)
            .env("ATR_SHADOW_GH_REAL", "/bin/true")
            .env_remove("ATR_SHADOW_GH_NESTED")
            .status()
            .unwrap();
        assert!(!st.success());
        let logged = std::fs::read_to_string(&log).unwrap();
        assert!(logged.contains("issue comment"), "{logged}");
    }

    #[test]
    fn wrapper_refuses_auth_token() {
        let dir = tempfile::tempdir().unwrap();
        let gh = write_gh_wrapper(dir.path()).unwrap();
        for args in [
            vec!["auth", "token"],
            vec!["auth", "status", "--show-token"],
        ] {
            let st = std::process::Command::new(&gh)
                .args(&args)
                .env("ATR_SHADOW_GH_REAL", "/bin/true")
                .env_remove("ATR_SHADOW_GH_NESTED")
                .output()
                .unwrap();
            assert!(!st.status.success(), "{args:?}");
            let err = String::from_utf8_lossy(&st.stderr);
            assert!(err.contains("token") || err.contains("refusing"), "{err}");
        }
    }

    #[test]
    fn wrapper_nested_guard_and_self_real() {
        let dir = tempfile::tempdir().unwrap();
        let gh = write_gh_wrapper(dir.path()).unwrap();
        let nested = std::process::Command::new(&gh)
            .args(["issue", "list"])
            .env("ATR_SHADOW_GH_REAL", "/bin/true")
            .env("ATR_SHADOW_GH_NESTED", "1")
            .output()
            .unwrap();
        assert!(!nested.status.success());
        assert!(
            String::from_utf8_lossy(&nested.stderr).contains("NESTED"),
            "{}",
            String::from_utf8_lossy(&nested.stderr)
        );

        let self_real = std::process::Command::new(&gh)
            .args(["issue", "list"])
            .env("ATR_SHADOW_GH_REAL", gh.display().to_string())
            .env_remove("ATR_SHADOW_GH_NESTED")
            .output()
            .unwrap();
        assert!(!self_real.status.success());

        let relative = std::process::Command::new(&gh)
            .args(["issue", "list"])
            .env("PATH", format!("{}:/usr/bin:/bin", dir.path().display()))
            .env("ATR_SHADOW_GH_REAL", "gh")
            .env_remove("ATR_SHADOW_GH_NESTED")
            .output()
            .unwrap();
        assert!(
            !relative.status.success(),
            "relative REAL=gh must not recurse"
        );
        assert!(
            String::from_utf8_lossy(&relative.stderr).contains("absolute"),
            "{}",
            String::from_utf8_lossy(&relative.stderr)
        );
    }

    #[test]
    fn resolve_skips_wrapper_on_path() {
        let dir = tempfile::tempdir().unwrap();
        let wrap = dir.path().join("wrap");
        let real_dir = dir.path().join("real");
        std::fs::create_dir_all(&wrap).unwrap();
        std::fs::create_dir_all(&real_dir).unwrap();
        write_gh_wrapper(&wrap).unwrap();
        let real = real_dir.join("gh");
        std::fs::write(&real, "#!/bin/sh\necho real\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let path = format!("{}:{}", wrap.display(), real_dir.display());
        let got = resolve_github_cli_in_path("gh", &path).unwrap();
        assert_eq!(got, real.canonicalize().unwrap());
    }

    #[test]
    fn resolve_fails_when_only_wrapper() {
        let dir = tempfile::tempdir().unwrap();
        write_gh_wrapper(dir.path()).unwrap();
        let err = resolve_github_cli_in_path("gh", &dir.path().display().to_string()).unwrap_err();
        assert!(
            err.to_string().contains("wrapper") || err.to_string().contains("resolve"),
            "{err:#}"
        );
        assert!(is_shadow_wrapper(&dir.path().join("gh")));
    }

    #[test]
    fn short_diffstat_lists_every_untracked_file() {
        assert_eq!(
            short_diffstat(" README.md | 2 +\n 1 file changed, 2 insertions(+)\n"),
            "README.md | 1 file changed, 2 insertions(+)"
        );
        assert_eq!(
            short_diffstat(" ATR_A.md | untracked\n ATR_B.md | untracked\n extra.rs | untracked\n"),
            "ATR_A.md (untracked), ATR_B.md (untracked), extra.rs (untracked)"
        );
        assert_eq!(cell("abcdefghijklmnopqrstuvwxyz", 8), "abcdefg…");
        assert_eq!(cell("short", 8), "short   ");
        assert_eq!(short_diffstat("   \n"), "-");
    }
}
