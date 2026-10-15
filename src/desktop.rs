//! Graphical-session detection for `needs = ["desktop"]` tasks.
//!
//! A desktop task may only start when the user running atr has a logged-in,
//! local, unlocked graphical session. Otherwise it stays queued and is retried
//! on later scheduler ticks.
//!
//! `desktop_check` in the config overrides the built-in detection: it is run
//! through `sh -c` and exit 0 means "ready". The built-in detection uses
//! `loginctl` on Linux and the console owner + `ioreg` lock flag on macOS.

use std::collections::HashMap;
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

const CACHE_TTL: Duration = Duration::from_secs(10);

static CACHE: Mutex<Option<HashMap<String, (Instant, bool)>>> = Mutex::new(None);

/// True when a desktop task can run now. Results are cached briefly so the
/// scheduler loop does not spawn a process per tick per task.
pub fn session_ready(check: &str) -> bool {
    let now = Instant::now();
    if let Some((at, ok)) = CACHE
        .lock()
        .unwrap()
        .get_or_insert_with(HashMap::new)
        .get(check)
        .copied()
    {
        if now.duration_since(at) < CACHE_TTL {
            return ok;
        }
    }
    let ok = probe(check);
    CACHE
        .lock()
        .unwrap()
        .get_or_insert_with(HashMap::new)
        .insert(check.to_string(), (now, ok));
    ok
}

fn probe(check: &str) -> bool {
    if !check.trim().is_empty() {
        return Command::new("sh")
            .arg("-c")
            .arg(check)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
    }
    if cfg!(target_os = "macos") {
        macos_ready()
    } else {
        linux_ready()
    }
}

fn output(cmd: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(cmd)
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

fn current_uid() -> Option<String> {
    output("id", &["-u"]).map(|s| s.trim().to_string())
}

fn linux_ready() -> bool {
    let Some(uid) = current_uid() else {
        return false;
    };
    let Some(list) = output("loginctl", &["list-sessions", "--no-legend"]) else {
        return false;
    };
    for id in session_ids_for_uid(&list, &uid) {
        if let Some(props) = output(
            "loginctl",
            &[
                "show-session",
                &id,
                "-p",
                "Type",
                "-p",
                "Active",
                "-p",
                "Remote",
                "-p",
                "LockedHint",
            ],
        ) {
            if linux_session_usable(&props) {
                return true;
            }
        }
    }
    false
}

/// Session ids owned by `uid` from `loginctl list-sessions --no-legend`.
pub fn session_ids_for_uid(list: &str, uid: &str) -> Vec<String> {
    list.lines()
        .filter_map(|l| {
            let mut f = l.split_whitespace();
            let id = f.next()?;
            let u = f.next()?;
            (u == uid).then(|| id.to_string())
        })
        .collect()
}

/// `loginctl show-session -p Type -p Active -p Remote -p LockedHint` output →
/// usable when it is a local, active, unlocked x11/wayland session.
pub fn linux_session_usable(props: &str) -> bool {
    let get = |k: &str| {
        props
            .lines()
            .find_map(|l| l.strip_prefix(&format!("{k}=")).map(str::trim))
            .unwrap_or("")
    };
    matches!(get("Type"), "x11" | "wayland" | "mir")
        && get("Active") == "yes"
        && get("Remote") != "yes"
        && get("LockedHint") != "yes"
}

fn macos_ready() -> bool {
    let me = std::env::var("USER").unwrap_or_default();
    let Some(owner) = output("stat", &["-f", "%Su", "/dev/console"]) else {
        return false;
    };
    let Some(ioreg) = output("ioreg", &["-n", "Root", "-d1"]) else {
        return false;
    };
    macos_session_usable(owner.trim(), &me, &ioreg)
}

/// Console owned by us, not at the login window, and the screen not locked.
pub fn macos_session_usable(console_owner: &str, me: &str, ioreg: &str) -> bool {
    !me.is_empty()
        && console_owner == me
        && console_owner != "root"
        && !ioreg.contains("\"CGSSessionScreenIsLocked\"=Yes")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linux_parsing() {
        let list = "  2 1000 rimas seat0 tty2\n  5 1001 bob - -\n  7 1000 rimas - pts/1\n";
        assert_eq!(session_ids_for_uid(list, "1000"), vec!["2", "7"]);
        assert!(linux_session_usable(
            "Type=wayland\nActive=yes\nRemote=no\nLockedHint=no\n"
        ));
        assert!(!linux_session_usable(
            "Type=wayland\nActive=yes\nRemote=no\nLockedHint=yes\n"
        ));
        assert!(!linux_session_usable(
            "Type=tty\nActive=yes\nRemote=no\nLockedHint=no\n"
        ));
        assert!(!linux_session_usable(
            "Type=x11\nActive=no\nRemote=no\nLockedHint=no\n"
        ));
        assert!(!linux_session_usable(
            "Type=x11\nActive=yes\nRemote=yes\nLockedHint=no\n"
        ));
    }

    #[test]
    fn macos_parsing() {
        assert!(macos_session_usable("tom", "tom", "| {}"));
        assert!(!macos_session_usable("root", "tom", ""));
        assert!(!macos_session_usable(
            "tom",
            "tom",
            "\"CGSSessionScreenIsLocked\"=Yes"
        ));
    }

    #[test]
    fn custom_check_uses_exit_status() {
        let dir = tempfile::tempdir().unwrap();
        let flag = dir.path().join("unlocked");
        let check = format!("test -e {}", flag.display());
        assert!(!probe(&check));
        std::fs::write(&flag, "").unwrap();
        assert!(probe(&check));
    }
}
