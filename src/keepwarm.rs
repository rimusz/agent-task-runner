//! Claude prompt-cache keep-warm (off by default).
//!
//! Claude Code on a subscription writes its prompt cache with a 1 h lifetime
//! (`cache_creation.ephemeral_1h_input_tokens`). When a task's Claude session
//! sits idle (task `waiting` / `needs_attention`, session kept for a resume)
//! longer than that, the next real resume re-writes the whole prefix at 2x the
//! input price instead of reading it at 0.1x. Keep-warm sends one tiny prompt
//! on a throwaway fork of the session shortly before the cache expires: the
//! provider re-reads the cached prefix, which refreshes its lifetime, and the
//! real session is not touched (`--fork-session --no-session-persistence`).
//!
//! Design borrowed from OpenMausBot `server/cache-keepwarm.ts` and
//! `server/drivers/claude-keepwarm.ts` (itself ported from Cache Tax, MIT).
//!
//! The ping is built by the same [`crate::providers::build_command_opts`] call
//! atr uses for a real resume (same model, permission mode, `extra_args` such
//! as `--mcp-config`, cwd), so the request prefix (tools, system prompt,
//! messages) is byte-identical. Claude Code replays the session's recorded
//! system prompt on resume (`--system-prompt-snapshot`, on by default), which
//! is why atr's resume does not re-pass `--append-system-prompt-file`.

use crate::app::Shared;
use crate::config::{Config, Provider};
use crate::db::Task;
use crate::duration::parse_duration;
use crate::providers::{CommandSpec, ParsedEvent, UsageNumbers};
use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

/// Prompt of the ping. One word in, one word out.
pub const PING_PROMPT: &str = "Reply with the single word: warm";
/// Wall-clock cap for one ping (a resume of a large session plus MCP startup).
pub const PING_TIMEOUT: Duration = Duration::from_secs(120);
/// Cache Tax's rule: a warm ping writes only its own few tokens. A ping that
/// writes more than this share of what it read found the cache already gone.
pub const MAX_WRITE_RATIO: f64 = 0.1;
/// How often the background loop looks for idle sessions.
pub const LOOP_EVERY: Duration = Duration::from_secs(60);
/// Default cache lifetime (Claude Code subscription writes 1 h entries).
pub const DEFAULT_TTL: Duration = Duration::from_secs(3600);
pub const DEFAULT_MAX_PINGS: u32 = 3;
pub const DEFAULT_MIN_TOKENS: u64 = 50_000;
/// Task states where atr expects a later resume of the stored session.
pub const ELIGIBLE_STATUS: &[&str] = &["waiting", "needs_attention"];

/// `[[provider]] keepwarm = "50m"` and friends, parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeepWarm {
    /// Idle time after the last request (run end or ping) before a ping.
    pub interval: Duration,
    /// Cache lifetime: never ping a session idle this long (already cold).
    pub ttl: Duration,
    /// Pings per idle stretch (reset by the next real run).
    pub max_pings: u32,
    /// Skip sessions whose last run read/wrote fewer prompt tokens.
    pub min_tokens: u64,
}

impl KeepWarm {
    /// `keepwarm` empty or `off` = `None` (the default).
    pub fn from_raw(
        kind: &str,
        keepwarm: &str,
        ttl: &str,
        max_pings: Option<u32>,
        min_tokens: Option<u64>,
    ) -> Result<Option<Self>> {
        let raw = keepwarm.trim();
        if raw.is_empty() || raw.eq_ignore_ascii_case("off") {
            return Ok(None);
        }
        if kind != "claude" {
            bail!("keepwarm is only supported for kind = \"claude\" (got {kind:?})");
        }
        let interval = parse_duration(raw).context("keepwarm")?;
        let ttl = if ttl.trim().is_empty() {
            DEFAULT_TTL
        } else {
            parse_duration(ttl.trim()).context("keepwarm_ttl")?
        };
        if interval.is_zero() || interval >= ttl {
            bail!(
                "keepwarm ({raw}) must be > 0 and shorter than keepwarm_ttl ({}s)",
                ttl.as_secs()
            );
        }
        let max_pings = max_pings.unwrap_or(DEFAULT_MAX_PINGS);
        if max_pings == 0 {
            bail!("keepwarm_max_pings must be >= 1 (use keepwarm = \"off\" to disable)");
        }
        Ok(Some(Self {
            interval,
            ttl,
            max_pings,
            min_tokens: min_tokens.unwrap_or(DEFAULT_MIN_TOKENS),
        }))
    }
}

/// What atr knows about one task's idle Claude session.
#[derive(Debug, Clone, Default)]
pub struct Candidate {
    pub status: String,
    pub shadow: bool,
    pub tui: bool,
    /// A live run, an open run row, or unconsumed messages (a run is coming).
    pub busy: bool,
    /// Stored session for (task, this provider).
    pub session: Option<String>,
    /// Provider of the task's last finished run.
    pub last_provider: Option<String>,
    pub last_ended: Option<DateTime<Utc>>,
    /// input + cache_read + cache_write of that run.
    pub last_prompt_tokens: u64,
    /// Pings for this session since `last_ended`.
    pub pings: u32,
    pub last_ping_at: Option<DateTime<Utc>>,
    /// The latest ping found the cache cold (or failed): stop for this stretch.
    pub last_ping_cold: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Skip(&'static str),
    /// Not yet; check again after this long.
    Wait(Duration),
    Ping,
}

pub fn decide(kw: &KeepWarm, provider: &str, c: &Candidate, now: DateTime<Utc>) -> Decision {
    if !ELIGIBLE_STATUS.contains(&c.status.as_str()) {
        return Decision::Skip("status");
    }
    if c.shadow || c.tui {
        return Decision::Skip("shadow or tui task");
    }
    if c.busy {
        return Decision::Skip("busy");
    }
    if c.session.as_deref().is_none_or(str::is_empty) {
        return Decision::Skip("no session");
    }
    if c.last_provider.as_deref() != Some(provider) {
        return Decision::Skip("last run on another provider");
    }
    let Some(ended) = c.last_ended else {
        return Decision::Skip("no finished run");
    };
    if c.last_prompt_tokens < kw.min_tokens {
        return Decision::Skip("session below keepwarm_min_tokens");
    }
    if c.last_ping_cold {
        return Decision::Skip("last ping found the cache cold");
    }
    if c.pings >= kw.max_pings {
        return Decision::Skip("keepwarm_max_pings reached");
    }
    let warm_at = c.last_ping_at.map_or(ended, |p| p.max(ended));
    let idle = now
        .signed_duration_since(warm_at)
        .to_std()
        .unwrap_or_default();
    if idle >= kw.ttl {
        return Decision::Skip("cache already expired");
    }
    if idle < kw.interval {
        return Decision::Wait(kw.interval - idle);
    }
    Decision::Ping
}

/// The ping command: atr's own resume command for `session`, plus
/// `--fork-session` (the original transcript is never written),
/// `--no-session-persistence` (the fork stays off disk) and `--max-turns 1`
/// (one model turn, so no tool loop can start).
pub fn build_ping(
    provider: &Provider,
    worktree: &Path,
    prompt_path: &Path,
    session: &str,
    dir_workspace: bool,
) -> CommandSpec {
    let mut spec = crate::providers::build_command_opts(
        provider,
        worktree,
        prompt_path,
        PING_PROMPT,
        Some(session),
        false,
        None,
        None,
    );
    if dir_workspace {
        crate::providers::apply_dir_confinement(provider, &mut spec, worktree);
    }
    for f in ["--fork-session", "--no-session-persistence"] {
        if !spec.args.iter().any(|a| a == f) {
            spec.args.push(f.into());
        }
    }
    if !spec.args.iter().any(|a| a == "--max-turns") {
        spec.args.push("--max-turns".into());
        spec.args.push("1".into());
    }
    spec
}

/// A ping that read the prefix and wrote little: the cache was warm.
pub fn ping_was_warm(u: &UsageNumbers) -> bool {
    u.cache_read > 0 && (u.cache_write as f64) < MAX_WRITE_RATIO * u.cache_read as f64
}

/// Usage from the ping's stdout (Claude `stream-json` or `json`).
pub fn ping_usage(kind: &str, stdout: &str) -> Option<(UsageNumbers, bool)> {
    crate::providers::parse_blob(kind, stdout)
        .into_iter()
        .rev()
        .find_map(|e| match e {
            ParsedEvent::Usage {
                usage, is_error, ..
            } => Some((usage, is_error)),
            _ => None,
        })
}

#[derive(Debug, Clone, Default)]
pub struct PingResult {
    pub ok: bool,
    pub warm: bool,
    pub usage: UsageNumbers,
    pub note: String,
}

/// Run one ping. Never touches the task's runs, turns or usage rows.
pub async fn run_ping(cfg: &Config, provider: &Provider, task: &Task, session: &str) -> PingResult {
    let fail = |note: String| PingResult {
        note,
        ..Default::default()
    };
    let Some(wt) = task.worktree.as_deref().filter(|w| !w.is_empty()) else {
        return fail("task has no worktree".into());
    };
    let wt = Path::new(wt);
    if !wt.is_dir() {
        return fail(format!("worktree missing: {}", wt.display()));
    }
    let dir = cfg
        .state_db
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("keepwarm");
    if let Err(e) = std::fs::create_dir_all(&dir) {
        return fail(format!("keepwarm dir: {e}"));
    }
    let prompt_path = dir.join("prompt.md");
    if let Err(e) = std::fs::write(&prompt_path, PING_PROMPT) {
        return fail(format!("keepwarm prompt: {e}"));
    }
    let spec = build_ping(
        provider,
        wt,
        &prompt_path,
        session,
        task.workspace_kind == "dir",
    );
    let mut cmd = crate::runner::agent_command(&spec);
    cmd.env("ATR_PROVIDER", &provider.name);
    cmd.env("ATR_KEEPWARM", "1");
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return fail(format!("spawn: {e}")),
    };
    let pid = child.id().unwrap_or(0);
    if let Some(mut stdin) = child.stdin.take() {
        use tokio::io::AsyncWriteExt;
        if let Some(text) = &spec.stdin_text {
            let _ = stdin.write_all(text.as_bytes()).await;
        }
    }
    let out = match tokio::time::timeout(PING_TIMEOUT, child.wait_with_output()).await {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => return fail(format!("wait: {e}")),
        Err(_) => {
            crate::runner::terminate_tree_now(pid);
            return fail("timed out".into());
        }
    };
    // MCP servers (e.g. Playwright via npx) may outlive the CLI.
    crate::runner::terminate_tree_now(pid);
    let stdout = String::from_utf8_lossy(&out.stdout);
    match ping_usage(&provider.kind, &stdout) {
        Some((usage, false)) => PingResult {
            ok: true,
            warm: ping_was_warm(&usage),
            usage,
            note: String::new(),
        },
        Some((usage, true)) => PingResult {
            usage,
            note: "CLI reported an error".into(),
            ..Default::default()
        },
        None => {
            let err = String::from_utf8_lossy(&out.stderr);
            let tail = err.trim().lines().last().unwrap_or("").to_string();
            fail(format!(
                "no usage (exit {:?}){}",
                out.status.code(),
                if tail.is_empty() {
                    String::new()
                } else {
                    format!(": {}", tail.chars().take(200).collect::<String>())
                }
            ))
        }
    }
}

fn parse_ts(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|t| t.with_timezone(&Utc))
}

/// Everything [`decide`] needs for one task, read from the DB.
pub fn candidate(shared: &Shared, task: &Task, provider: &Provider) -> Result<Candidate> {
    let db = &shared.db;
    let busy = shared.inflight.lock().unwrap().contains_key(&task.key)
        || db.open_run_for_task(&task.key)?.is_some()
        || !db.pending_messages(&task.key)?.is_empty();
    let session = db.task_session(&task.key, &provider.name)?;
    let last = db.keepwarm_last_run(&task.key)?;
    let mut c = Candidate {
        status: task.status.clone(),
        shadow: task.shadow,
        tui: crate::tui::is_tui_key(&task.key),
        busy,
        session: session.clone(),
        ..Default::default()
    };
    if let Some((prov, ended, tokens)) = last {
        c.last_provider = Some(prov);
        c.last_ended = parse_ts(&ended);
        c.last_prompt_tokens = tokens;
    }
    if let (Some(sid), Some(ended)) = (session.as_deref(), c.last_ended) {
        let s = db.keepwarm_since(&task.key, &provider.name, sid, &ended.to_rfc3339())?;
        c.pings = s.count;
        c.last_ping_at = s.last_at.as_deref().and_then(parse_ts);
        c.last_ping_cold = s.last_cold;
    }
    Ok(c)
}

/// One pass: ping every due session (sequentially; pings are rare).
pub async fn tick(shared: &Arc<Shared>) -> Result<u32> {
    let cfg = shared.config.read().unwrap().clone();
    let providers: Vec<Provider> = cfg
        .providers
        .iter()
        .filter(|p| p.enabled && p.keepwarm.is_some())
        .cloned()
        .collect();
    if providers.is_empty() {
        return Ok(0);
    }
    let mut sent = 0;
    for task in shared.db.list_tasks()? {
        if !ELIGIBLE_STATUS.contains(&task.status.as_str()) {
            continue;
        }
        for p in &providers {
            let kw = p.keepwarm.as_ref().expect("filtered");
            let c = candidate(shared, &task, p)?;
            if decide(kw, &p.name, &c, Utc::now()) != Decision::Ping {
                continue;
            }
            let sid = c.session.clone().unwrap_or_default();
            let r = run_ping(&cfg, p, &task, &sid).await;
            tracing::info!(
                task = %task.key,
                provider = %p.name,
                ok = r.ok,
                warm = r.warm,
                cache_read = r.usage.cache_read,
                cache_write = r.usage.cache_write,
                input = r.usage.input,
                output = r.usage.output,
                note = %r.note,
                "keepwarm ping"
            );
            shared.db.insert_keepwarm_ping(
                &task.key,
                &p.name,
                &sid,
                r.ok,
                r.ok && r.warm,
                &r.usage,
                &r.note,
            )?;
            sent += 1;
        }
    }
    Ok(sent)
}

pub async fn keepwarm_loop(shared: Arc<Shared>) {
    let mut every = tokio::time::interval(LOOP_EVERY);
    every.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        every.tick().await;
        if let Err(e) = tick(&shared).await {
            tracing::warn!("keepwarm: {e:#}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn kw() -> KeepWarm {
        KeepWarm::from_raw("claude", "50m", "", None, None)
            .unwrap()
            .unwrap()
    }

    fn t(min: i64) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-11-03T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
            + chrono::Duration::minutes(min)
    }

    fn idle_session() -> Candidate {
        Candidate {
            status: "waiting".into(),
            session: Some("s1".into()),
            last_provider: Some("claude".into()),
            last_ended: Some(t(0)),
            last_prompt_tokens: 80_000,
            ..Default::default()
        }
    }

    #[test]
    fn off_by_default_and_parses() {
        for raw in ["", "off", "OFF", "  "] {
            assert_eq!(
                KeepWarm::from_raw("claude", raw, "", None, None).unwrap(),
                None
            );
        }
        let k = kw();
        assert_eq!(k.interval, Duration::from_secs(50 * 60));
        assert_eq!(k.ttl, DEFAULT_TTL);
        assert_eq!(k.max_pings, DEFAULT_MAX_PINGS);
        assert_eq!(k.min_tokens, DEFAULT_MIN_TOKENS);
    }

    #[test]
    fn rejects_bad_config() {
        assert!(KeepWarm::from_raw("grok", "50m", "", None, None).is_err());
        assert!(KeepWarm::from_raw("claude", "1h", "1h", None, None).is_err());
        assert!(KeepWarm::from_raw("claude", "4m", "5m", Some(0), None).is_err());
        assert!(KeepWarm::from_raw("claude", "4m", "5m", Some(2), Some(1)).is_ok());
    }

    #[test]
    fn waits_until_interval_then_pings() {
        let k = kw();
        let c = idle_session();
        assert_eq!(
            decide(&k, "claude", &c, t(10)),
            Decision::Wait(Duration::from_secs(40 * 60))
        );
        assert_eq!(decide(&k, "claude", &c, t(50)), Decision::Ping);
        assert_eq!(decide(&k, "claude", &c, t(59)), Decision::Ping);
        assert_eq!(
            decide(&k, "claude", &c, t(60)),
            Decision::Skip("cache already expired")
        );
    }

    #[test]
    fn a_warm_ping_restarts_the_clock_until_the_cap() {
        let k = kw();
        let mut c = idle_session();
        c.pings = 1;
        c.last_ping_at = Some(t(50));
        assert_eq!(
            decide(&k, "claude", &c, t(70)),
            Decision::Wait(Duration::from_secs(30 * 60))
        );
        assert_eq!(decide(&k, "claude", &c, t(100)), Decision::Ping);
        assert_eq!(
            decide(&k, "claude", &c, t(115)),
            Decision::Skip("cache already expired")
        );
        c.pings = 3;
        assert_eq!(
            decide(&k, "claude", &c, t(100)),
            Decision::Skip("keepwarm_max_pings reached")
        );
    }

    type Mutate = Box<dyn Fn(&mut Candidate)>;

    #[test]
    fn skips_ineligible_sessions() {
        let k = kw();
        let now = t(55);
        let cases: Vec<(Mutate, &str)> = vec![
            (Box::new(|c| c.status = "done".into()), "status"),
            (Box::new(|c| c.status = "running".into()), "status"),
            (Box::new(|c| c.shadow = true), "shadow or tui task"),
            (Box::new(|c| c.tui = true), "shadow or tui task"),
            (Box::new(|c| c.busy = true), "busy"),
            (Box::new(|c| c.session = None), "no session"),
            (
                Box::new(|c| c.last_provider = Some("grok".into())),
                "last run on another provider",
            ),
            (Box::new(|c| c.last_ended = None), "no finished run"),
            (
                Box::new(|c| c.last_prompt_tokens = 10_000),
                "session below keepwarm_min_tokens",
            ),
            (
                Box::new(|c| c.last_ping_cold = true),
                "last ping found the cache cold",
            ),
        ];
        for (f, why) in cases {
            let mut c = idle_session();
            f(&mut c);
            assert_eq!(decide(&k, "claude", &c, now), Decision::Skip(why));
        }
        let mut c = idle_session();
        c.status = "needs_attention".into();
        assert_eq!(decide(&k, "claude", &c, now), Decision::Ping);
    }

    fn claude() -> Provider {
        let mut p = Provider::test_stub("claude");
        p.cli = PathBuf::from("/opt/homebrew/bin/claude");
        p.model = "opus".into();
        p.args = vec![
            "-p".into(),
            "--output-format".into(),
            "stream-json".into(),
            "--verbose".into(),
        ];
        p.permission_mode = "bypassPermissions".into();
        p.prompt_mode = "stdin".into();
        p.system_prompt_flag = "--append-system-prompt-file".into();
        p.extra_args = vec![
            "--mcp-config=/x/claude-mcp.json".into(),
            "--strict-mcp-config".into(),
        ];
        p
    }

    #[test]
    fn ping_is_atr_resume_plus_fork_no_persistence() {
        let p = claude();
        let wt = Path::new("/w/claude-0");
        let prompt = Path::new("/s/keepwarm/prompt.md");
        let resume = crate::providers::build_command_opts(
            &p,
            wt,
            prompt,
            PING_PROMPT,
            Some("s1"),
            false,
            None,
            None,
        );
        let ping = build_ping(&p, wt, prompt, "s1", false);
        // Same launch contract as the real resume (prefix must match) ...
        assert_eq!(&ping.args[..resume.args.len()], &resume.args[..]);
        assert_eq!(ping.cwd, resume.cwd);
        assert_eq!(ping.program, resume.program);
        // ... plus the fork flags, once.
        assert_eq!(
            &ping.args[resume.args.len()..],
            &[
                "--fork-session",
                "--no-session-persistence",
                "--max-turns",
                "1"
            ]
        );
        let has = |f: &str| ping.args.iter().any(|a| a == f);
        assert!(has("--resume") && has("s1") && has("--model") && has("opus"));
        assert!(has("--mcp-config=/x/claude-mcp.json") && has("--strict-mcp-config"));
        // atr's resume never re-passes the system prompt file (snapshot replay).
        assert!(!has("--append-system-prompt-file"));
        assert_eq!(ping.stdin_text.as_deref(), Some(PING_PROMPT));
    }

    #[test]
    fn dir_workspace_ping_keeps_add_dir() {
        let p = claude();
        let wt = Path::new("/w/dir");
        let ping = build_ping(&p, wt, Path::new("/s/p.md"), "s1", true);
        assert!(ping
            .args
            .windows(2)
            .any(|w| w[0] == "--add-dir" && w[1] == "/w/dir"));
    }

    #[test]
    fn warm_rule_and_usage_parse() {
        let warm = UsageNumbers {
            cache_read: 80_000,
            cache_write: 20,
            ..Default::default()
        };
        let cold = UsageNumbers {
            cache_read: 10_000,
            cache_write: 70_000,
            ..Default::default()
        };
        assert!(ping_was_warm(&warm));
        assert!(!ping_was_warm(&cold));
        assert!(!ping_was_warm(&UsageNumbers::default()));
        let out = r#"{"type":"result","subtype":"success","is_error":false,"result":"warm","session_id":"f1","total_cost_usd":0.05,"usage":{"input_tokens":3,"cache_creation_input_tokens":25,"cache_read_input_tokens":80281,"output_tokens":5}}"#;
        let (u, err) = ping_usage("claude", out).unwrap();
        assert!(!err);
        assert_eq!(
            (u.input, u.cache_read, u.cache_write, u.output),
            (3, 80281, 25, 5)
        );
        assert!(ping_was_warm(&u));
    }

    #[test]
    fn db_tracks_last_run_and_pings_per_stretch() {
        let dir = tempfile::tempdir().unwrap();
        let db = crate::db::Db::open(&dir.path().join("s.db")).unwrap();
        db.upsert_task("kw-1", "/repo", None).unwrap();
        assert_eq!(db.keepwarm_last_run("kw-1").unwrap(), None);
        let run = db.start_run("kw-1", "claude", "opus").unwrap();
        db.insert_usage(run, None, "claude", 10, 5, 70_000, 2_000, None, None, "{}")
            .unwrap();
        db.finish_run(run, "success", Some(0)).unwrap();
        let (prov, ended, tokens) = db.keepwarm_last_run("kw-1").unwrap().unwrap();
        assert_eq!((prov.as_str(), tokens), ("claude", 72_010));
        let none = db.keepwarm_since("kw-1", "claude", "s1", &ended).unwrap();
        assert_eq!(none, crate::db::KeepWarmSince::default());
        let warm = UsageNumbers {
            cache_read: 72_000,
            cache_write: 20,
            ..Default::default()
        };
        db.insert_keepwarm_ping("kw-1", "claude", "s1", true, true, &warm, "")
            .unwrap();
        let s = db.keepwarm_since("kw-1", "claude", "s1", &ended).unwrap();
        assert_eq!(s.count, 1);
        assert!(!s.last_cold && s.last_at.is_some());
        db.insert_keepwarm_ping("kw-1", "claude", "s1", true, false, &warm, "")
            .unwrap();
        assert!(
            db.keepwarm_since("kw-1", "claude", "s1", &ended)
                .unwrap()
                .last_cold
        );
        // Another session, or pings before the last real run, do not count.
        assert_eq!(
            db.keepwarm_since("kw-1", "claude", "s2", &ended)
                .unwrap()
                .count,
            0
        );
        assert_eq!(
            db.keepwarm_since("kw-1", "claude", "s1", "2999-01-01T00:00:00Z")
                .unwrap()
                .count,
            0
        );
    }
}
