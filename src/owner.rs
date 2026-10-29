//! Cross-process owner identity for TUI / serve coexistence.
//!
//! A live owner is a `(pid, start-token)` pair: the pid is still alive and
//! the OS still reports the same start token. That survives PID reuse.
//! `atr serve` registers as [`KIND_SERVE`]; `atr tui` / `atr ask` register as
//! [`KIND_TUI`]. Runs and claimed tasks stamp the current process so a new
//! serve can tell a live TUI turn from a leftover CLI.

use crate::db::pid_alive;
use std::sync::OnceLock;

/// `owners.kind` for `atr serve` / the user service.
pub const KIND_SERVE: &str = "serve";
/// `owners.kind` for `atr tui` / `atr ask`.
pub const KIND_TUI: &str = "tui";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnerId {
    pub pid: i64,
    /// OS start token (Linux `/proc` starttime, elsewhere `ps` lstart).
    pub started: String,
}

impl OwnerId {
    pub fn is_alive(&self) -> bool {
        owner_alive(self.pid, &self.started)
    }
}

/// This process. Cached so a later `/proc` hiccup does not change our stamp.
pub fn current() -> OwnerId {
    static CURRENT: OnceLock<OwnerId> = OnceLock::new();
    CURRENT
        .get_or_init(|| {
            let pid = std::process::id() as i64;
            OwnerId {
                pid,
                started: process_start_token(pid).unwrap_or_default(),
            }
        })
        .clone()
}

/// True when `pid` is alive and still has `started`. Empty/zero is never live
/// (legacy rows and test fixtures for a dead previous serve).
pub fn owner_alive(pid: i64, started: &str) -> bool {
    if pid <= 0 || started.is_empty() {
        return false;
    }
    pid_alive(pid) && process_start_token(pid).as_deref() == Some(started)
}

pub fn process_start_token(pid: i64) -> Option<String> {
    if pid <= 0 {
        return None;
    }
    #[cfg(target_os = "linux")]
    {
        let text = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        parse_linux_starttime(&text).map(|n| n.to_string())
    }
    #[cfg(not(target_os = "linux"))]
    {
        ps_lstart_token(pid)
    }
}

/// Field 22 of `/proc/<pid>/stat` (starttime). `comm` may contain spaces and
/// parentheses, so we take the 20th whitespace field after the last `)`.
pub fn parse_linux_starttime(stat: &str) -> Option<i64> {
    let rest = stat.rsplit_once(')')?.1;
    rest.split_whitespace().nth(19)?.parse().ok()
}

#[cfg(not(target_os = "linux"))]
fn ps_lstart_token(pid: i64) -> Option<String> {
    let out = std::process::Command::new("ps")
        .args(["-o", "lstart=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_process_is_alive_and_stable() {
        let a = current();
        let b = current();
        assert_eq!(a, b);
        assert!(a.pid > 0);
        assert!(!a.started.is_empty(), "start token must be readable");
        assert!(a.is_alive());
        assert!(!owner_alive(0, &a.started));
        assert!(!owner_alive(a.pid, ""));
        assert!(!owner_alive(a.pid, "not-the-start-token"));
        assert!(!owner_alive(1_999_999_999, "1"));
    }

    #[test]
    fn linux_stat_starttime_is_field_22() {
        // pid (comm) + 20 fields after ')'; starttime is the 20th (index 19).
        let stat = "1 (some name) S 0 1 1 0 -1 4194560 0 0 0 0 0 0 0 0 20 0 1 0 12345 0";
        assert_eq!(parse_linux_starttime(stat), Some(12345));
        assert_eq!(parse_linux_starttime("broken"), None);
    }
}
