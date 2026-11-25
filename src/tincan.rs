//! `[notify.tincan]`: push one task-status ping to an Agent Tincan teammate.
//!
//! Fire-and-forget. A hung or missing `tincan` binary never fails a run.

use crate::config::Config;
use crate::db::Task;
use crate::github_auth::redact_secrets;
use crate::notify::Event;
use crate::util::{git_capture, retry_exec_busy};
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

const WARN_EVERY: Duration = Duration::from_secs(60);
static LAST_WARN: Mutex<Option<Instant>> = Mutex::new(None);

/// Fields copied off a task for one ping. Tests build this directly.
#[derive(Debug, Clone, Default)]
pub struct TincanTask {
    pub key: String,
    pub labels: String,
    pub repo: Option<String>,
    pub issue: Option<i64>,
    pub status: String,
    pub worktree: Option<String>,
    pub result_pr_url: String,
    /// Task-level owner. Wins over `[labels.*] notify_agent`.
    pub notify_agent: String,
    /// Tincan request id for `--about`. Empty omits the flag.
    pub about: String,
}

impl TincanTask {
    pub fn from_task(t: &Task) -> Self {
        // Task rows have no tincan-owner / request-id columns yet. Routing
        // uses `[labels.*] notify_agent` then `default_agent`. `--about` is
        // omitted until intake stores a request id on the task.
        Self {
            key: t.key.clone(),
            labels: t.labels.clone(),
            repo: t.repo.clone(),
            issue: t.issue,
            status: t.status.clone(),
            worktree: t.worktree.clone(),
            result_pr_url: t.result_pr_url.clone(),
            notify_agent: String::new(),
            about: String::new(),
        }
    }
}

pub fn notify(cfg: &Config, event: Event, body: &str, shadow: bool, task: Option<&Task>) {
    let ctx = task.map(TincanTask::from_task);
    notify_ctx(cfg, event, body, shadow, ctx.as_ref());
}

pub fn notify_ctx(cfg: &Config, event: Event, body: &str, shadow: bool, task: Option<&TincanTask>) {
    if shadow {
        tracing::debug!("shadow task; skip tincan");
        return;
    }
    if !cfg.notify.tincan.enabled {
        return;
    }
    if !crate::notify::tincan_allows(&cfg.notify.tincan, event) {
        tracing::debug!(event = event.as_str(), "tincan event filtered");
        return;
    }
    let Some(agent) = route_agent(cfg, task) else {
        tracing::debug!("tincan: no agent; skip");
        return;
    };
    let cfg = cfg.clone();
    let task = task.cloned();
    let body = body.to_string();
    std::thread::spawn(move || {
        let budget = cfg.notify.tincan.timeout;
        let started = Instant::now();
        let mut ping = Ping {
            bin: cfg.notify.tincan.bin.clone(),
            agent,
            about: task
                .as_ref()
                .map(|t| t.about.trim().to_string())
                .filter(|s| !s.is_empty()),
            urgent: cfg.notify.tincan.urgent && urgent_event(event),
            timeout: budget,
            message: format_message(&cfg, event, &body, task.as_ref()),
            scrub: scrub_keys(&cfg),
        };
        let rest = budget.saturating_sub(started.elapsed());
        if rest < Duration::from_millis(100) {
            warn_fail(&"tincan notify timed out building the message");
            return;
        }
        ping.timeout = rest;
        if let Err(e) = deliver(&ping) {
            warn_fail(&e);
        }
    });
}

fn urgent_event(event: Event) -> bool {
    matches!(
        event,
        Event::Failed | Event::NeedsAttention | Event::CapReached
    )
}

/// Owning agent: task-level `notify_agent`, else first matching label, else
/// `default_agent`. One name, lowercase.
pub fn route_agent(cfg: &Config, task: Option<&TincanTask>) -> Option<String> {
    if let Some(t) = task {
        let direct = t.notify_agent.trim();
        if !direct.is_empty() {
            return Some(normalize_agent(direct));
        }
        if let Some(a) = cfg.label_policy(&t.labels).notify_agent {
            if !a.trim().is_empty() {
                return Some(normalize_agent(&a));
            }
        }
    }
    let d = cfg.notify.tincan.default_agent.trim();
    if d.is_empty() {
        None
    } else {
        Some(normalize_agent(d))
    }
}

pub fn normalize_agent(raw: &str) -> String {
    raw.trim().to_ascii_lowercase()
}

/// Agents doctor should ping: default plus unique per-label owners.
pub fn configured_agents(cfg: &Config) -> Vec<String> {
    let mut out = Vec::new();
    let push = |out: &mut Vec<String>, raw: &str| {
        let a = normalize_agent(raw);
        if !a.is_empty() && !out.iter().any(|e| e == &a) {
            out.push(a);
        }
    };
    push(&mut out, &cfg.notify.tincan.default_agent);
    for p in cfg.labels.values() {
        if let Some(a) = &p.notify_agent {
            push(&mut out, a);
        }
    }
    out
}

pub fn format_message(cfg: &Config, event: Event, body: &str, task: Option<&TincanTask>) -> String {
    let key = task
        .map(|t| t.key.as_str())
        .filter(|k| !k.is_empty())
        .unwrap_or("-");
    let mut lines = vec![format!("atr {} {key}", event.as_str())];
    let mut fields: Vec<String> = vec![format!("task={key}"), format!("event={}", event.as_str())];
    if let Some(t) = task {
        if !t.status.trim().is_empty() {
            fields.push(format!("status={}", t.status.trim()));
        }
        let labels = crate::config::parse_labels(&t.labels).join(",");
        if !labels.is_empty() {
            fields.push(format!("label={labels}"));
        }
        if let Some(repo) = t.repo.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            fields.push(format!("repo={repo}"));
            if let Some(n) = t.issue {
                if let Some(url) = issue_url(repo, n) {
                    fields.push(format!("issue={url}"));
                } else {
                    fields.push(format!("issue={n}"));
                }
            }
        } else if let Some(n) = t.issue {
            fields.push(format!("issue={n}"));
        }
        let pr = t.result_pr_url.trim();
        if !pr.is_empty() {
            fields.push(format!("pr={pr}"));
        }
        if let Some(wt) = t
            .worktree
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            let (branch, sha) = git_head(wt);
            if let Some(b) = branch {
                fields.push(format!("branch={b}"));
            }
            if let Some(s) = sha {
                fields.push(format!("sha={s}"));
            }
        }
    }
    // public_url or the local listen URL. Never `tailscale serve status`
    // (that has no timeout and would leak this worker).
    let dash = dashboard_link(cfg);
    if !dash.trim().is_empty() {
        fields.push(format!("dashboard={dash}"));
    }
    fields.push(format!("time={}", crate::db::now_rfc3339()));
    lines.push(fields.join(" "));
    let note = first_line(body);
    if !note.is_empty() {
        let head = lines[0].as_str();
        if note != head && !note.starts_with(head) {
            lines.push(note);
        }
    }
    redact_secrets(&lines.join("\n"))
}

fn first_line(body: &str) -> String {
    let line = body.lines().next().unwrap_or("").trim();
    if line.is_empty() {
        return String::new();
    }
    let s: String = line.chars().take(200).collect();
    s
}

fn issue_url(repo: &str, n: i64) -> Option<String> {
    crate::config::parse_github_slug(repo)
        .ok()
        .map(|slug| format!("https://github.com/{slug}/issues/{n}"))
}

fn dashboard_link(cfg: &Config) -> String {
    let p = cfg.dashboard.public_url.trim();
    if !p.is_empty() {
        return p.to_string();
    }
    crate::ui::dashboard_url(&cfg.listen)
}

fn git_head(worktree: &str) -> (Option<String>, Option<String>) {
    let p = Path::new(worktree);
    if !p.exists() {
        return (None, None);
    }
    let branch = git_one(p, &["rev-parse", "--abbrev-ref", "HEAD"]);
    let sha = git_one(p, &["rev-parse", "--short", "HEAD"]);
    (branch, sha)
}

const GIT_HEAD_TIMEOUT: Duration = Duration::from_secs(2);

fn git_one(cwd: &Path, args: &[&str]) -> Option<String> {
    let mut cmd = git_capture();
    cmd.current_dir(cwd).args(args);
    let child = retry_exec_busy(|| cmd.spawn()).ok()?;
    let (mut child, st) = wait_child(child, GIT_HEAD_TIMEOUT).ok()?;
    if !st.success() {
        return None;
    }
    let mut buf = Vec::new();
    if let Some(mut out) = child.stdout.take() {
        let _ = std::io::Read::read_to_end(&mut out, &mut buf);
    }
    let s = String::from_utf8_lossy(&buf).trim().to_string();
    if s.is_empty() || s == "HEAD" {
        None
    } else {
        Some(s)
    }
}

struct Ping {
    bin: String,
    agent: String,
    about: Option<String>,
    urgent: bool,
    timeout: Duration,
    message: String,
    scrub: Vec<String>,
}

fn deliver(ping: &Ping) -> Result<()> {
    run_tincan(
        &ping.bin,
        &ask_args(
            &ping.agent,
            &ping.message,
            ping.about.as_deref(),
            ping.urgent,
        ),
        ping.timeout,
        &ping.scrub,
    )
    .map(|_| ())
}

pub fn ask_args(agent: &str, message: &str, about: Option<&str>, urgent: bool) -> Vec<String> {
    let mut args = vec![
        "ask".into(),
        agent.to_string(),
        message.to_string(),
        "--notify".into(),
        "--json".into(),
    ];
    if let Some(id) = about.map(str::trim).filter(|s| !s.is_empty()) {
        args.push("--about".into());
        args.push(id.to_string());
    }
    if urgent {
        args.push("--urgent".into());
    }
    args
}

pub fn ping_args(agent: &str) -> Vec<String> {
    vec!["ping".into(), agent.to_string()]
}

/// Resolve `[notify.tincan] bin` (PATH name or absolute/relative path).
pub fn resolve_bin(bin: &str) -> Option<PathBuf> {
    let bin = bin.trim();
    if bin.is_empty() {
        return None;
    }
    let p = Path::new(bin);
    if p.is_absolute() || bin.contains('/') {
        return p.is_file().then(|| p.to_path_buf());
    }
    crate::detect::find_binary(bin, &crate::detect::current_env())
}

pub fn run_tincan(bin: &str, args: &[String], timeout: Duration, scrub: &[String]) -> Result<i32> {
    let mut cmd = Command::new(bin);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    for k in scrub {
        cmd.env_remove(k);
    }
    let child = retry_exec_busy(|| cmd.spawn()).with_context(|| format!("spawn {bin}"))?;
    let limit = timeout.max(Duration::from_millis(100));
    match wait_child(child, limit) {
        Ok((_child, st)) => {
            if !st.success() {
                bail!("{bin} exited {st}");
            }
            Ok(st.code().unwrap_or(0))
        }
        Err(e) => bail!("{bin} {e}"),
    }
}

fn wait_child(
    mut child: std::process::Child,
    timeout: Duration,
) -> Result<(std::process::Child, std::process::ExitStatus)> {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(st)) => return Ok((child, st)),
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                if reap_until(&mut child, Duration::from_millis(500)).is_none() {
                    // Still running; leaking Child avoids Drop's blocking wait.
                    std::mem::forget(child);
                }
                bail!(
                    "timed out after {}",
                    crate::duration::format_duration(timeout)
                );
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(e) => bail!("wait: {e}"),
        }
    }
}

fn reap_until(
    child: &mut std::process::Child,
    limit: Duration,
) -> Option<std::process::ExitStatus> {
    let deadline = Instant::now() + limit;
    loop {
        match child.try_wait() {
            Ok(Some(st)) => return Some(st),
            Err(_) => return None,
            Ok(None) if Instant::now() >= deadline => return None,
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
        }
    }
}

pub fn scrub_keys(cfg: &Config) -> Vec<String> {
    std::env::vars_os()
        .filter_map(|(k, _)| k.into_string().ok())
        .filter(|k| {
            k.starts_with("ATR_")
                || crate::gh_token::is_token_var(k)
                || *k == cfg.shadow.gh_token_env
                || *k == cfg.telegram.bot_token_env
                || *k == cfg.mcp.token_env
        })
        .collect()
}

fn warn_fail(err: &impl std::fmt::Display) {
    let text = redact_secrets(&err.to_string());
    let mut g = LAST_WARN.lock().unwrap_or_else(|e| e.into_inner());
    let now = Instant::now();
    if g.map(|t| now.saturating_duration_since(t) >= WARN_EVERY)
        .unwrap_or(true)
    {
        tracing::warn!("tincan notify failed: {text}");
        *g = Some(now);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notify::{self, Event};
    use std::os::unix::fs::PermissionsExt;

    fn fixture_bin() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake-tincan.sh")
    }

    fn parse(extra: &str) -> Config {
        crate::config::parse_config_toml(&format!(
            r#"
repo_path = "/tmp/repo"
{extra}
[[provider]]
name = "claude"
kind = "claude"
cli = "/bin/true"
"#
        ))
        .unwrap()
    }

    fn tincan_cfg(extra: &str) -> Config {
        parse(&format!(
            r#"
[notify.tincan]
enabled = true
bin = "{bin}"
default_agent = "nixer"
{extra}
"#,
            bin = fixture_bin().display()
        ))
    }

    #[test]
    fn empty_events_become_default_set() {
        let cfg = tincan_cfg("");
        assert_eq!(
            cfg.notify.tincan.events,
            notify::default_tincan_event_names()
        );
        for e in notify::TINCAN_DEFAULT_EVENTS {
            assert!(
                notify::tincan_allows(&cfg.notify.tincan, *e),
                "{}",
                e.as_str()
            );
        }
        assert!(!cfg
            .notify
            .tincan
            .events
            .iter()
            .any(|e| e == Event::Start.as_str()));
    }

    #[test]
    fn default_set_filters_start() {
        let cfg = tincan_cfg("");
        assert!(notify::tincan_allows(&cfg.notify.tincan, Event::Failed));
        assert!(notify::tincan_allows(&cfg.notify.tincan, Event::Finished));
        assert!(notify::tincan_allows(&cfg.notify.tincan, Event::CapReached));
        assert!(!notify::tincan_allows(&cfg.notify.tincan, Event::Start));
        assert!(!notify::tincan_allows(&cfg.notify.tincan, Event::Hook));
    }

    #[test]
    fn explicit_events_filter_and_stalled_admits_cap() {
        let cfg = tincan_cfg(r#"events = ["failed", "stalled"]"#);
        assert!(notify::tincan_allows(&cfg.notify.tincan, Event::Failed));
        assert!(notify::tincan_allows(&cfg.notify.tincan, Event::Stalled));
        assert!(notify::tincan_allows(&cfg.notify.tincan, Event::CapReached));
        assert!(!notify::tincan_allows(&cfg.notify.tincan, Event::Finished));
    }

    #[test]
    fn route_prefers_task_then_label_then_default() {
        let cfg = parse(
            r#"
[notify.tincan]
enabled = true
default_agent = "nixer"
[labels.proof]
notify_agent = "proofy"
[labels.other]
notify_agent = "other"
"#,
        );
        assert_eq!(route_agent(&cfg, None).as_deref(), Some("nixer"));
        let labeled = TincanTask {
            labels: "for:atr,PROOF".into(),
            ..Default::default()
        };
        assert_eq!(route_agent(&cfg, Some(&labeled)).as_deref(), Some("proofy"));
        let both = TincanTask {
            labels: "proof".into(),
            notify_agent: "Direct".into(),
            ..Default::default()
        };
        assert_eq!(route_agent(&cfg, Some(&both)).as_deref(), Some("direct"));
        let unlabeled = TincanTask {
            labels: "unrelated".into(),
            ..Default::default()
        };
        assert_eq!(
            route_agent(&cfg, Some(&unlabeled)).as_deref(),
            Some("nixer")
        );
    }

    #[test]
    fn message_has_fields_and_redacts_secrets() {
        let cfg = tincan_cfg("urgent = true");
        let task = TincanTask {
            key: "acme-12".into(),
            labels: "proof".into(),
            repo: Some("rimusz/agent-task-runner".into()),
            issue: Some(104),
            status: "failed".into(),
            result_pr_url: "https://github.com/rimusz/agent-task-runner/pull/9".into(),
            ..Default::default()
        };
        let msg = format_message(
            &cfg,
            Event::Failed,
            "failed acme-12 (task timeout) ghp_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            Some(&task),
        );
        assert!(msg.starts_with("atr failed acme-12\n"), "{msg}");
        assert!(msg.contains("task=acme-12"), "{msg}");
        assert!(msg.contains("event=failed"), "{msg}");
        assert!(msg.contains("status=failed"), "{msg}");
        assert!(msg.contains("label=proof"), "{msg}");
        assert!(msg.contains("repo=rimusz/agent-task-runner"), "{msg}");
        assert!(
            msg.contains("issue=https://github.com/rimusz/agent-task-runner/issues/104"),
            "{msg}"
        );
        assert!(
            msg.contains("pr=https://github.com/rimusz/agent-task-runner/pull/9"),
            "{msg}"
        );
        assert!(msg.contains("dashboard="), "{msg}");
        assert!(msg.contains("time="), "{msg}");
        assert!(!msg.contains("ghp_"), "{msg}");
    }

    #[test]
    fn message_includes_branch_and_sha() {
        let dir = tempfile::tempdir().unwrap();
        let wt = dir.path();
        let git = |args: &[&str]| {
            let st = std::process::Command::new("git")
                .args(args)
                .current_dir(wt)
                .env("GIT_TERMINAL_PROMPT", "0")
                .status()
                .unwrap();
            assert!(st.success(), "git {args:?}");
        };
        git(&["init", "-q"]);
        git(&["config", "user.email", "t@t"]);
        git(&["config", "user.name", "t"]);
        std::fs::write(wt.join("f"), "x\n").unwrap();
        git(&["add", "f"]);
        git(&["commit", "-q", "-m", "init"]);
        git(&["checkout", "-q", "-b", "feat-tincan"]);
        let cfg = tincan_cfg("");
        let task = TincanTask {
            key: "acme-1".into(),
            worktree: Some(wt.display().to_string()),
            status: "waiting".into(),
            ..Default::default()
        };
        let msg = format_message(&cfg, Event::Finished, "finished acme-1", Some(&task));
        assert!(msg.contains("branch=feat-tincan"), "{msg}");
        assert!(msg.contains("sha="), "{msg}");
    }

    #[test]
    fn ask_argv_notify_json_about_urgent() {
        let args = ask_args("nixer", "hello", Some("req-1"), true);
        assert_eq!(
            args,
            vec!["ask", "nixer", "hello", "--notify", "--json", "--about", "req-1", "--urgent",]
        );
        let quiet = ask_args("nixer", "hello", None, false);
        assert_eq!(quiet, vec!["ask", "nixer", "hello", "--notify", "--json"]);
    }

    fn chmod_x(p: &Path) {
        let mut perms = std::fs::metadata(p).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(p, perms).unwrap();
    }

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn wait_log(p: &Path, needle: &str, timeout: Duration) -> String {
        let t0 = Instant::now();
        loop {
            if let Ok(s) = std::fs::read_to_string(p) {
                if s.contains(needle) {
                    return s;
                }
            }
            if t0.elapsed() > timeout {
                return std::fs::read_to_string(p).unwrap_or_default();
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn deliver_records_argv_and_scrubs_atr_env() {
        let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("tincan");
        std::fs::copy(fixture_bin(), &bin).unwrap();
        chmod_x(&bin);
        std::env::set_var("ATR_TINCAN_SECRET", "nope");
        std::env::set_var("TINC_MARKER", "keep");
        let cfg = parse(&format!(
            r#"
[notify.tincan]
enabled = true
bin = "{}"
default_agent = "nixer"
urgent = true
"#,
            bin.display()
        ));
        let task = TincanTask {
            key: "t1".into(),
            status: "failed".into(),
            about: "req-9".into(),
            ..Default::default()
        };
        let ping = Ping {
            bin: cfg.notify.tincan.bin.clone(),
            agent: route_agent(&cfg, Some(&task)).unwrap(),
            about: Some(task.about.clone()),
            urgent: cfg.notify.tincan.urgent && urgent_event(Event::Failed),
            timeout: Duration::from_secs(5),
            message: format_message(&cfg, Event::Failed, "failed t1", Some(&task)),
            scrub: scrub_keys(&cfg),
        };
        assert!(scrub_keys(&cfg).iter().any(|k| k == "ATR_TINCAN_SECRET"));
        deliver(&ping).unwrap();
        let recorded = std::fs::read_to_string(format!("{}.log", bin.display())).unwrap();
        assert!(recorded.contains("ask\nnixer\n"), "{recorded}");
        assert!(recorded.contains("--notify"), "{recorded}");
        assert!(recorded.contains("--json"), "{recorded}");
        assert!(recorded.contains("--about\nreq-9"), "{recorded}");
        assert!(recorded.contains("--urgent"), "{recorded}");
        assert!(recorded.contains("atr failed t1"), "{recorded}");
        assert!(recorded.contains("NO_ATR_TINCAN_SECRET"), "{recorded}");
        assert!(recorded.contains("HAS_TINC_MARKER"), "{recorded}");
        std::env::remove_var("ATR_TINCAN_SECRET");
        std::env::remove_var("TINC_MARKER");
    }

    #[test]
    fn timeout_and_failure_do_not_panic() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("tincan");
        std::fs::copy(fixture_bin(), &bin).unwrap();
        chmod_x(&bin);
        std::fs::write(format!("{}.sleep", bin.display()), "5\n").unwrap();
        let t0 = Instant::now();
        let err = run_tincan(
            bin.to_str().unwrap(),
            &ask_args("nixer", "x", None, false),
            Duration::from_millis(200),
            &[],
        )
        .unwrap_err()
        .to_string();
        assert!(t0.elapsed() < Duration::from_secs(3), "{:?}", t0.elapsed());
        assert!(err.contains("timed out"), "{err}");
        let _ = std::fs::remove_file(format!("{}.sleep", bin.display()));
        std::fs::write(format!("{}.fail", bin.display()), "").unwrap();
        let err = run_tincan(
            bin.to_str().unwrap(),
            &ping_args("nixer"),
            Duration::from_secs(2),
            &[],
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("exited"), "{err}");
    }

    #[test]
    fn notify_ctx_returns_before_slow_child() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("tincan");
        std::fs::copy(fixture_bin(), &bin).unwrap();
        chmod_x(&bin);
        std::fs::write(format!("{}.sleep", bin.display()), "3\n").unwrap();
        let mut cfg = parse(&format!(
            r#"
[notify.tincan]
enabled = true
bin = "{}"
default_agent = "nixer"
timeout = "2s"
"#,
            bin.display()
        ));
        cfg.notify.tincan.timeout = Duration::from_secs(2);
        let t0 = Instant::now();
        notify_ctx(&cfg, Event::Failed, "failed t", false, None);
        assert!(
            t0.elapsed() < Duration::from_millis(500),
            "{:?}",
            t0.elapsed()
        );
        let log = wait_log(
            &PathBuf::from(format!("{}.log", bin.display())),
            "ask",
            Duration::from_secs(2),
        );
        assert!(log.contains("ask\nnixer\n"), "{log}");
    }

    fn skip_cfg(bin: &Path, extra: &str) -> Config {
        parse(&format!(
            r#"
[notify.tincan]
enabled = true
bin = "{}"
default_agent = "nixer"
{extra}
"#,
            bin.display()
        ))
    }

    fn copy_bin() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("tincan");
        std::fs::copy(fixture_bin(), &bin).unwrap();
        chmod_x(&bin);
        (dir, bin)
    }

    fn log_of(bin: &Path) -> PathBuf {
        PathBuf::from(format!("{}.log", bin.display()))
    }

    #[test]
    fn skip_shadow_disabled_and_no_agent() {
        let (_dir, bin) = copy_bin();
        let cfg = skip_cfg(&bin, "");
        notify_ctx(&cfg, Event::Failed, "failed t", true, None);
        assert!(
            !log_of(&bin).exists(),
            "shadow must not spawn tincan: {}",
            std::fs::read_to_string(log_of(&bin)).unwrap_or_default()
        );

        let mut off = skip_cfg(&bin, "");
        off.notify.tincan.enabled = false;
        notify_ctx(&off, Event::Failed, "failed t", false, None);
        assert!(!log_of(&bin).exists(), "disabled must not spawn");

        let mut none = skip_cfg(&bin, "");
        none.notify.tincan.default_agent.clear();
        notify_ctx(&none, Event::Failed, "failed t", false, None);
        assert!(!log_of(&bin).exists(), "no agent must not spawn");
    }

    #[test]
    fn from_task_has_no_about_or_direct_owner() {
        let t = Task {
            key: "k".into(),
            labels: "proof".into(),
            status: "failed".into(),
            ..Default::default()
        };
        let ctx = TincanTask::from_task(&t);
        assert!(ctx.notify_agent.is_empty());
        assert!(ctx.about.is_empty());
        assert_eq!(ctx.status, "failed");
        assert_eq!(ctx.key, "k");
    }
}
