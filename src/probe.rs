//! `atr doctor --probe`: a real round trip through each provider CLI.
//!
//! Turn 1 asks the CLI to remember a random code word and reply `READY`.
//! Turn 2 resumes that same session and asks for the code word back. The
//! second prompt never contains the word, so finding it in the output proves
//! the reply came from the resumed session's memory.
//!
//! The probe builds the same argv the runner uses (`build_command_opts`), but
//! in a throwaway temp directory, with no system prompt and no state DB: it
//! never creates tasks, runs, sessions, or usage rows. Each turn has a hard
//! timeout; a turn that runs past it has its process group killed. It does
//! spend two model turns per provider, which is why it is opt-in.

use crate::config::{Config, Provider};
use crate::doctor::{Check, Status};
use crate::providers::{
    build_command_opts, classify_error_value, new_session_uuid, parse_line, session_id,
    supports_resume, ErrorClass, ParsedEvent,
};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

/// Default per-turn timeout for `--probe`.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Debug, Clone)]
pub struct ProbeOpts {
    /// Provider names to probe. Empty = every enabled provider.
    pub providers: Vec<String>,
    /// Hard cap for each of the two turns.
    pub timeout: Duration,
}

impl Default for ProbeOpts {
    fn default() -> Self {
        Self {
            providers: Vec::new(),
            timeout: DEFAULT_TIMEOUT,
        }
    }
}

/// What one probe turn produced.
#[derive(Debug, Clone, Default)]
pub struct TurnOut {
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub spawn_error: Option<String>,
    pub session_id: Option<String>,
    pub stdout: String,
    pub stderr: String,
    pub is_error: bool,
    pub classified: Option<ErrorClass>,
    pub failed_text: Option<String>,
    pub elapsed: Duration,
}

impl TurnOut {
    pub fn ok(&self) -> bool {
        self.spawn_error.is_none() && !self.timed_out && self.exit_code == Some(0) && !self.is_error
    }

    /// One short, secret-free reason for a failed turn.
    pub fn reason(&self, timeout: Duration) -> String {
        if let Some(e) = &self.spawn_error {
            return format!("could not start CLI: {}", one_line(e, 160));
        }
        if self.timed_out {
            return format!(
                "no finished reply within {} (process killed)",
                crate::duration::format_duration(timeout)
            );
        }
        let class = match self.classified {
            Some(ErrorClass::Auth) => Some("signed out / auth error"),
            Some(ErrorClass::Limit) => Some("usage limit / 429"),
            Some(ErrorClass::Server) => Some("provider server error"),
            Some(ErrorClass::Unavailable) => Some("provider unavailable"),
            _ => None,
        };
        let detail = self
            .failed_text
            .clone()
            .filter(|s| !s.trim().is_empty())
            .or_else(|| first_nonempty(&self.stderr))
            .map(|s| one_line(&s, 160))
            .unwrap_or_default();
        let code = match self.exit_code {
            Some(c) => format!("exit {c}"),
            None => "killed by signal".into(),
        };
        let mut out = code;
        if self.is_error && self.exit_code == Some(0) {
            out = "error result".into();
        }
        if let Some(c) = class {
            out.push_str(&format!(", {c}"));
        }
        if !detail.is_empty() {
            out.push_str(&format!(": {detail}"));
        }
        out
    }
}

fn first_nonempty(text: &str) -> Option<String> {
    text.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(str::to_string)
}

fn one_line(text: &str, max: usize) -> String {
    let flat: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        flat
    } else {
        let cut: String = flat.chars().take(max).collect();
        format!("{cut}…")
    }
}

fn short_sid(sid: &str) -> String {
    let s: String = sid.chars().take(8).collect();
    if sid.chars().count() > 8 {
        format!("{s}…")
    } else {
        s
    }
}

/// Random code word; never a secret, but unguessable by a model.
pub fn code_word() -> String {
    let hex: String = new_session_uuid()
        .chars()
        .filter(|c| c.is_ascii_hexdigit())
        .take(10)
        .collect();
    format!("ATRPROBE{}", hex.to_ascii_uppercase())
}

pub fn first_prompt(word: &str) -> String {
    format!(
        "This is an `atr doctor --probe` connectivity check. Remember this code word for \
         the next turn: {word}\nDo not call any tools, do not read or edit files. \
         Reply with only: READY"
    )
}

pub const SECOND_PROMPT: &str = "What was the code word I asked you to remember in the \
previous turn? Do not call any tools. Reply with only the code word.";

/// Run one CLI turn in `dir` with a hard timeout. No DB, no events.
pub async fn run_turn(
    p: &Provider,
    dir: &Path,
    prompt: &str,
    session: Option<&str>,
    new_session: bool,
    timeout: Duration,
) -> TurnOut {
    let started = Instant::now();
    let mut out = TurnOut::default();
    let prompt_path = dir.join(".atr-probe-prompt.md");
    if let Err(e) = std::fs::write(&prompt_path, prompt) {
        out.spawn_error = Some(format!("write prompt: {e}"));
        return out;
    }
    let spec = build_command_opts(
        p,
        dir,
        &prompt_path,
        prompt,
        session,
        new_session,
        None,
        None,
    );
    let mut cmd = Command::new(&spec.program);
    cmd.args(&spec.args)
        .current_dir(&spec.cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .env("ATR_PROVIDER", &p.name)
        .env("ATR_PROBE", "1");
    #[cfg(unix)]
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = match crate::util::retry_exec_busy(|| cmd.spawn()) {
        Ok(c) => c,
        Err(e) => {
            out.spawn_error = Some(format!("{}: {e}", spec.program.display()));
            return out;
        }
    };
    let pid = child.id().unwrap_or(0);
    if let Some(mut stdin) = child.stdin.take() {
        if let Some(text) = &spec.stdin_text {
            let _ = stdin.write_all(text.as_bytes()).await;
        }
        drop(stdin);
    }
    let mut stdout = child.stdout.take();
    let mut stderr = child.stderr.take();
    let io = async {
        let mut o = String::new();
        let mut e = String::new();
        let read_o = async {
            if let Some(s) = stdout.as_mut() {
                let _ = s.read_to_string(&mut o).await;
            }
        };
        let read_e = async {
            if let Some(s) = stderr.as_mut() {
                let _ = s.read_to_string(&mut e).await;
            }
        };
        tokio::join!(read_o, read_e);
        let status = child.wait().await;
        (o, e, status)
    };
    match tokio::time::timeout(timeout, io).await {
        Ok((o, e, status)) => {
            out.stdout = o;
            out.stderr = e;
            out.exit_code = status.ok().and_then(|s| s.code());
        }
        Err(_) => {
            out.timed_out = true;
            crate::runner::terminate_tree_now(pid);
        }
    }
    out.elapsed = started.elapsed();
    out.session_id = session.map(str::to_string);
    for line in out.stdout.lines() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
            continue;
        };
        if let Some(id) = session_id(&v) {
            out.session_id = Some(id);
        }
        if let Some(c) = classify_error_value(&v) {
            out.classified = Some(c);
        }
        match parse_line(&p.kind, line) {
            Some(ParsedEvent::Usage { is_error, .. }) if is_error => out.is_error = true,
            Some(ParsedEvent::Failed(t)) => {
                out.is_error = true;
                out.failed_text = Some(t);
            }
            Some(ParsedEvent::Session(id)) => out.session_id = Some(id),
            _ => {}
        }
    }
    out
}

fn probe_dir() -> std::io::Result<PathBuf> {
    let dir = std::env::temp_dir().join(format!("atr-probe-{}", new_session_uuid()));
    std::fs::create_dir_all(&dir)?;
    // Codex refuses to run outside a git repo; an empty repo is harmless.
    let _ = crate::util::git_capture()
        .args(["init", "-q"])
        .current_dir(&dir)
        .output();
    Ok(dir)
}

struct DirGuard(PathBuf);
impl Drop for DirGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Probe one provider: reply turn, then resume + recall turn.
pub async fn probe_provider(p: &Provider, timeout: Duration) -> Check {
    let name = format!("probe {}", p.name);
    let fail = |detail: String, hint: &str| Check {
        name: name.clone(),
        status: Status::Fail,
        detail,
        hint: Some(hint.to_string()),
    };
    let dir = match probe_dir() {
        Ok(d) => d,
        Err(e) => return fail(format!("temp dir: {e}"), "check TMPDIR is writable"),
    };
    let _guard = DirGuard(dir.clone());
    let word = code_word();
    let (session, new_session) = if p.session_id_flag.is_empty() {
        (None, false)
    } else {
        (Some(new_session_uuid()), true)
    };
    let t1 = run_turn(
        p,
        &dir,
        &first_prompt(&word),
        session.as_deref(),
        new_session,
        timeout,
    )
    .await;
    if !t1.ok() {
        return fail(
            format!("turn 1 (reply): {}", t1.reason(timeout)),
            &format!(
                "run `{}` by hand and sign in / fix it, then retry",
                p.cli.display()
            ),
        );
    }
    if !supports_resume(p) {
        return Check {
            name,
            status: Status::Warn,
            detail: format!(
                "reply ok ({:.1}s); no resume flag, so follow-ups start fresh",
                t1.elapsed.as_secs_f64()
            ),
            hint: Some("set resume_flag for this provider if its CLI supports resume".into()),
        };
    }
    let Some(sid) = t1.session_id.clone().filter(|s| !s.is_empty()) else {
        return fail(
            "turn 1 replied but reported no session id; resume is impossible".into(),
            "check the CLI's JSON output flags (stream-json / --json)",
        );
    };
    let t2 = run_turn(p, &dir, SECOND_PROMPT, Some(&sid), false, timeout).await;
    if !t2.ok() {
        return fail(
            format!(
                "turn 2 (resume {}): {}",
                short_sid(&sid),
                t2.reason(timeout)
            ),
            "resume is broken: follow-ups and retries will lose context",
        );
    }
    if !t2.stdout.contains(&word) {
        return fail(
            format!(
                "turn 2 resumed {} but did not recall the code word",
                short_sid(&sid)
            ),
            "resume is not restoring the conversation; follow-ups will lose context",
        );
    }
    let moved = t2
        .session_id
        .as_deref()
        .filter(|s| !s.is_empty() && *s != sid)
        .map(|s| format!(", resumed as {}", short_sid(s)))
        .unwrap_or_default();
    Check {
        name,
        status: Status::Ok,
        detail: format!(
            "reply + resume recall ok (session {}{moved}; {:.1}s + {:.1}s)",
            short_sid(&sid),
            t1.elapsed.as_secs_f64(),
            t2.elapsed.as_secs_f64()
        ),
        hint: None,
    }
}

/// Probe the selected (or every enabled) provider, one at a time.
pub async fn run(cfg: &Config, opts: &ProbeOpts) -> Vec<Check> {
    let mut checks = Vec::new();
    let selected: Vec<&Provider> = if opts.providers.is_empty() {
        cfg.providers.iter().filter(|p| p.enabled).collect()
    } else {
        let mut v = Vec::new();
        for want in &opts.providers {
            match cfg.providers.iter().find(|p| &p.name == want) {
                Some(p) => v.push(p),
                None => checks.push(Check {
                    name: format!("probe {want}"),
                    status: Status::Fail,
                    detail: "no provider with that name in config".into(),
                    hint: Some("see `atr providers`".into()),
                }),
            }
        }
        v
    };
    if selected.is_empty() && checks.is_empty() {
        checks.push(Check {
            name: "probe".into(),
            status: Status::Warn,
            detail: "no enabled providers to probe".into(),
            hint: Some("enable one with `atr providers enable <name>`".into()),
        });
    }
    for p in selected {
        checks.push(probe_provider(p, opts.timeout).await);
    }
    checks
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fake CLI: remembers the code word per session under $ATR_FAKE_PROBE_DIR
    /// and answers like Claude stream-json. Behaviour via ATR_FAKE_PROBE.
    const FAKE: &str = r#"#!/bin/sh
mode="${ATR_FAKE_PROBE:-good}"
store="${ATR_FAKE_PROBE_DIR:?}"
sid=""
resume=""
while [ $# -gt 0 ]; do
  case "$1" in
    --session-id) sid="$2"; shift ;;
    --resume) resume="$2"; shift ;;
  esac
  shift
done
prompt=$(cat)
if [ "$mode" = "hang" ]; then sleep 30; fi
if [ "$mode" = "auth" ]; then
  echo '{"type":"result","subtype":"error","is_error":true,"result":"Invalid API key · Please run /login"}'
  exit 1
fi
if [ -n "$resume" ]; then
  sid="$resume"
  if [ "$mode" = "forget" ]; then word="no idea"; else word=$(cat "$store/$sid" 2>/dev/null); fi
  printf '{"type":"result","subtype":"success","is_error":false,"result":"%s","session_id":"%s","usage":{"input_tokens":1,"output_tokens":1}}\n' "$word" "$sid"
  exit 0
fi
[ -n "$sid" ] || sid="sess-$$"
echo "$prompt" | grep -o 'ATRPROBE[0-9A-F]*' > "$store/$sid"
if [ "$mode" = "nosid" ]; then
  echo '{"type":"result","subtype":"success","is_error":false,"result":"READY","usage":{"input_tokens":1,"output_tokens":1}}'
  exit 0
fi
printf '{"type":"system","subtype":"init","session_id":"%s"}\n' "$sid"
printf '{"type":"result","subtype":"success","is_error":false,"result":"READY","session_id":"%s","usage":{"input_tokens":1,"output_tokens":1}}\n' "$sid"
"#;

    fn fake_provider(dir: &Path, name: &str) -> Provider {
        let cli = dir.join(format!("fake-{name}"));
        std::fs::write(&cli, FAKE).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let mut p = crate::providers::tests::stub("claude");
        p.name = name.into();
        p.cli = cli;
        p.prompt_mode = "stdin".into();
        p
    }

    // The fake reads its mode from the environment, and env is process-wide,
    // so the cases run in one test, one after another.
    #[tokio::test]
    async fn probe_cases() {
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("ATR_FAKE_PROBE_DIR", tmp.path());
        let t = Duration::from_secs(10);

        std::env::set_var("ATR_FAKE_PROBE", "good");
        let c = probe_provider(&fake_provider(tmp.path(), "good"), t).await;
        assert_eq!(c.status, Status::Ok, "{c:?}");
        assert!(c.detail.contains("resume recall ok"), "{}", c.detail);

        // Same thing with an atr-chosen session id (Grok-style --session-id).
        let mut p = fake_provider(tmp.path(), "sidflag");
        p.session_id_flag = "--session-id".into();
        let c = probe_provider(&p, t).await;
        assert_eq!(c.status, Status::Ok, "{c:?}");

        std::env::set_var("ATR_FAKE_PROBE", "forget");
        let c = probe_provider(&fake_provider(tmp.path(), "forget"), t).await;
        assert_eq!(c.status, Status::Fail);
        assert!(c.detail.contains("did not recall"), "{}", c.detail);

        std::env::set_var("ATR_FAKE_PROBE", "auth");
        let c = probe_provider(&fake_provider(tmp.path(), "auth"), t).await;
        assert_eq!(c.status, Status::Fail);
        assert!(c.detail.starts_with("turn 1"), "{}", c.detail);

        std::env::set_var("ATR_FAKE_PROBE", "nosid");
        let c = probe_provider(&fake_provider(tmp.path(), "nosid"), t).await;
        assert_eq!(c.status, Status::Fail);
        assert!(c.detail.contains("no session id"), "{}", c.detail);

        std::env::set_var("ATR_FAKE_PROBE", "good");
        let mut p = fake_provider(tmp.path(), "noresume");
        p.resume_flag.clear();
        let c = probe_provider(&p, t).await;
        assert_eq!(c.status, Status::Warn, "{c:?}");

        std::env::set_var("ATR_FAKE_PROBE", "hang");
        let started = Instant::now();
        let c = probe_provider(
            &fake_provider(tmp.path(), "hang"),
            Duration::from_millis(500),
        )
        .await;
        assert_eq!(c.status, Status::Fail);
        assert!(c.detail.contains("process killed"), "{}", c.detail);
        assert!(started.elapsed() < Duration::from_secs(10));

        let missing = Provider {
            cli: tmp.path().join("does-not-exist"),
            ..fake_provider(tmp.path(), "missing")
        };
        let c = probe_provider(&missing, t).await;
        assert_eq!(c.status, Status::Fail);
        assert!(c.detail.contains("could not start CLI"), "{}", c.detail);
    }

    #[test]
    fn second_prompt_never_contains_the_word() {
        let w = code_word();
        assert!(w.starts_with("ATRPROBE") && w.len() == 18, "{w}");
        assert!(first_prompt(&w).contains(&w));
        assert!(!SECOND_PROMPT.contains("ATRPROBE"));
        assert_ne!(code_word(), code_word());
    }
}
