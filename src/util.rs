use crate::config::Config;
use anyhow::{bail, Context, Result};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

/// Git child that never inherits the TTY (TUI composer stays clean).
pub fn git_command() -> Command {
    let mut c = Command::new("git");
    c.stdin(Stdio::null());
    c.stdout(Stdio::null());
    c.stderr(Stdio::null());
    c.env("GIT_TERMINAL_PROMPT", "0");
    c.env("GIT_PAGER", "");
    c
}

/// Git child with captured stdio (never inherit).
pub fn git_capture() -> Command {
    let mut c = Command::new("git");
    c.stdin(Stdio::null());
    c.stdout(Stdio::piped());
    c.stderr(Stdio::piped());
    c.env("GIT_TERMINAL_PROMPT", "0");
    c.env("GIT_PAGER", "");
    c
}

/// Bounded HTTP client for outbound calls (Telegram, webhooks, notify).
/// ureq has no default timeout; without one a hung peer blocks the thread forever.
pub(crate) fn ureq_agent(timeout: Duration) -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout(timeout)
        .user_agent("atr")
        .build()
}

/// Spawning a script right after it was written can fail with ETXTBSY
/// ("Text file busy"): a child forked by another thread at that moment still
/// holds the write fd until it execs. The fd closes within milliseconds, so
/// retry the spawn while (and only while) the error is ExecutableFileBusy.
const EXEC_BUSY_TRIES: u32 = 50;

fn exec_busy_backoff(attempt: u32) -> Duration {
    Duration::from_millis(5u64 << attempt.min(4))
}

pub fn retry_exec_busy<T>(mut f: impl FnMut() -> std::io::Result<T>) -> std::io::Result<T> {
    let mut attempt = 0;
    loop {
        match f() {
            Err(e)
                if e.kind() == std::io::ErrorKind::ExecutableFileBusy
                    && attempt + 1 < EXEC_BUSY_TRIES =>
            {
                std::thread::sleep(exec_busy_backoff(attempt));
                attempt += 1;
            }
            other => return other,
        }
    }
}

/// `tokio::process::Command::output` with [`retry_exec_busy`] on spawn.
/// Captures stdout/stderr; stdin is null (same defaults as `output()`).
pub async fn output_retry_busy(
    cmd: &mut tokio::process::Command,
) -> std::io::Result<std::process::Output> {
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    retry_exec_busy(|| cmd.spawn())?.wait_with_output().await
}

pub fn current_bin() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.canonicalize().ok())
        .unwrap_or_else(|| PathBuf::from("atr"))
}

pub fn pid_path(cfg: &Config) -> PathBuf {
    cfg.state_db
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("atr.pid")
}

pub fn write_pid(cfg: &Config) -> Result<()> {
    let path = pid_path(cfg);
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).ok();
    }
    let owner = crate::owner::current();
    debug_assert!(
        !owner.started.chars().any(char::is_whitespace),
        "owner start token must be whitespace-free: {:?}",
        owner.started
    );
    fs::write(&path, format!("{} {}\n", owner.pid, owner.started))
        .with_context(|| format!("write {}", path.display()))
}

pub fn read_pid(cfg: &Config) -> Option<i32> {
    let text = fs::read_to_string(pid_path(cfg)).ok()?;
    parse_pidfile(&text).map(|(pid, _)| pid as i32)
}

/// `(pid, Some(start_token))`, or `(pid, None)` for a legacy bare-pid file.
/// The token is the whole remainder after the first whitespace so a
/// multi-word `ps` lstart is not truncated.
pub fn parse_pidfile(text: &str) -> Option<(i64, Option<&str>)> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    match text.split_once(char::is_whitespace) {
        Some((pid_s, rest)) => {
            let pid: i64 = pid_s.parse().ok()?;
            let started = rest.trim();
            if started.is_empty() {
                Some((pid, None))
            } else {
                Some((pid, Some(started)))
            }
        }
        None => Some((text.parse().ok()?, None)),
    }
}

/// True when the serve pidfile names a still-live process (pid + start token).
pub fn pidfile_owner_alive(cfg: &Config) -> bool {
    let text = match fs::read_to_string(pid_path(cfg)) {
        Ok(t) => t,
        Err(_) => return false,
    };
    match parse_pidfile(&text) {
        Some((pid, Some(started))) => crate::owner::owner_alive(pid, started),
        Some((pid, None)) => crate::db::pid_alive(pid),
        None => false,
    }
}

pub fn sighup_daemon(cfg: &Config) -> Result<()> {
    let Some(pid) = read_pid(cfg) else {
        bail!("daemon pid file not found; is atr serve running?");
    };
    let rc = unsafe { libc::kill(pid, libc::SIGHUP) };
    if rc != 0 {
        bail!("SIGHUP {pid} failed");
    }
    Ok(())
}

pub fn backup_dir() -> PathBuf {
    crate::config::expand_path("~/.local/share/atr/backup")
}

pub fn copy_file(from: &Path, to: &Path) -> Result<()> {
    if let Some(dir) = to.parent() {
        fs::create_dir_all(dir)?;
    }
    fs::copy(from, to).with_context(|| format!("copy {} -> {}", from.display(), to.display()))?;
    Ok(())
}

/// Release asset platform name for this host (`atr-<ver>-<asset>.tar.gz`):
/// macos-arm64, linux-x86_64, linux-arm64 or linux-x86_64-musl.
pub fn host_asset() -> &'static str {
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    {
        return "macos-arm64";
    }
    #[cfg(all(target_os = "linux", target_arch = "x86_64", target_env = "musl"))]
    {
        return "linux-x86_64-musl";
    }
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    {
        return "linux-x86_64";
    }
    #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
    {
        return "linux-arm64";
    }
    #[allow(unreachable_code)]
    "unknown"
}

pub fn sha256_file(path: &Path) -> Result<String> {
    use sha2::{Digest, Sha256};
    let data = fs::read(path).with_context(|| format!("read {}", path.display()))?;
    let mut h = Sha256::new();
    h.update(&data);
    Ok(format!("{:x}", h.finalize()))
}

pub fn parse_sha256_line(text: &str) -> Option<String> {
    let first = text.split_whitespace().next()?;
    let hex = first.trim().to_ascii_lowercase();
    if hex.len() == 64 && hex.chars().all(|c| c.is_ascii_hexdigit()) {
        Some(hex)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn retry_exec_busy_retries_only_busy() {
        let mut n = 0;
        let r = super::retry_exec_busy(|| {
            n += 1;
            if n < 3 {
                Err(std::io::Error::from(std::io::ErrorKind::ExecutableFileBusy))
            } else {
                Ok(n)
            }
        });
        assert_eq!(r.unwrap(), 3);
        let mut m = 0;
        let e = super::retry_exec_busy::<()>(|| {
            m += 1;
            Err(std::io::Error::from(std::io::ErrorKind::NotFound))
        });
        assert!(e.is_err());
        assert_eq!(m, 1, "other errors are not retried");
    }

    use super::*;

    #[test]
    fn host_asset_is_a_friendly_release_name() {
        let a = host_asset();
        assert!(
            [
                "macos-arm64",
                "linux-x86_64",
                "linux-arm64",
                "linux-x86_64-musl",
                "unknown"
            ]
            .contains(&a),
            "{a}"
        );
        assert!(!a.contains("unknown-linux") && !a.contains("apple-darwin"));
    }

    #[test]
    fn parse_sha256_line_takes_first_token() {
        let h = "a".repeat(64);
        assert_eq!(
            parse_sha256_line(&format!("{h}  atr-0.1.1.tar.gz")),
            Some(h)
        );
    }

    #[test]
    fn parse_pidfile_keeps_multi_word_start_token() {
        // macOS `ps -o lstart=` used to be stored raw. Linux CI must catch a
        // split_whitespace parser that only keeps "Thu".
        assert_eq!(
            parse_pidfile("42 Thu Oct 29 10:17:41 2026\n"),
            Some((42, Some("Thu Oct 29 10:17:41 2026")))
        );
        assert_eq!(
            parse_pidfile("  99\tThu Oct  9 01:02:03 2026  "),
            Some((99, Some("Thu Oct  9 01:02:03 2026")))
        );
        assert_eq!(
            parse_pidfile("7 1761733061\n"),
            Some((7, Some("1761733061")))
        );
        assert_eq!(parse_pidfile("12345\n"), Some((12345, None)));
        assert_eq!(parse_pidfile(""), None);
        assert_eq!(parse_pidfile("not-a-pid leftover"), None);
    }

    #[test]
    fn write_pid_round_trips_current_owner() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("p.md"), "p\n").unwrap();
        let cli = if std::path::Path::new("/bin/true").exists() {
            "/bin/true"
        } else {
            "/usr/bin/true"
        };
        let text = format!(
            r#"
listen = "127.0.0.1:0"
state_db = "{db}"
worktree_root = "{root}"
repo_path = "{root}"
system_prompt_file = "{root}/p.md"
[[provider]]
name = "claude"
kind = "claude"
cli = "{cli}"
"#,
            db = dir.path().join("s.db").display(),
            root = dir.path().display(),
        );
        let raw: crate::config::RawConfig = toml::from_str(&text).unwrap();
        let cfg = crate::config::Config::from_raw(raw).unwrap();
        write_pid(&cfg).unwrap();
        let body = std::fs::read_to_string(pid_path(&cfg)).unwrap();
        let owner = crate::owner::current();
        assert_eq!(
            parse_pidfile(&body),
            Some((owner.pid, Some(owner.started.as_str())))
        );
        assert!(
            !owner.started.chars().any(char::is_whitespace),
            "{}",
            owner.started
        );
        assert!(pidfile_owner_alive(&cfg));
        // Legacy bare-pid files still count as live.
        std::fs::write(pid_path(&cfg), format!("{}\n", owner.pid)).unwrap();
        assert!(pidfile_owner_alive(&cfg));
    }
}
