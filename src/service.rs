use crate::config::{default_atr_dir, expand_path, Config};
use crate::util::current_bin;
use anyhow::{bail, Context, Result};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

pub const LAUNCHD_LABEL: &str = "dev.atr.serve";
const PLIST_TEMPLATE: &str = include_str!("../contrib/dev.atr.serve.plist");
const UNIT_TEMPLATE: &str = include_str!("../contrib/atr.service");
const SERVE_SH_TEMPLATE: &str = include_str!("../contrib/atr-serve.sh");

pub fn home_dir() -> PathBuf {
    expand_path("~")
}

pub fn plist_path() -> PathBuf {
    home_dir().join("Library/LaunchAgents/dev.atr.serve.plist")
}

pub fn unit_path() -> PathBuf {
    home_dir().join(".config/systemd/user/atr.service")
}

pub fn serve_sh_path() -> PathBuf {
    default_atr_dir().join("atr-serve.sh")
}

pub fn render_serve_sh(bin: &Path) -> String {
    SERVE_SH_TEMPLATE.replace("__ATR_BIN__", &bin.display().to_string())
}

pub fn render_plist(home: &Path, bin: &Path) -> String {
    let bin_dir = bin.parent().unwrap_or(home);
    PLIST_TEMPLATE
        .replace("__HOME__", &home.display().to_string())
        .replace("__ATR_BIN_DIR__", &bin_dir.display().to_string())
}

pub fn write_launcher(bin: &Path) -> Result<PathBuf> {
    let path = serve_sh_path();
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    fs::write(&path, render_serve_sh(bin))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755))?;
    }
    Ok(path)
}

fn skip_load() -> bool {
    matches!(
        std::env::var("ATR_INIT_SKIP_LOAD").as_deref(),
        Ok("1") | Ok("true")
    )
}

pub fn install(cfg: Option<&Config>, load: bool, legacy_label: Option<&str>) -> Result<String> {
    let bin = current_bin();
    write_launcher(&bin)?;
    let mut notes = Vec::new();
    if let Some(note) = stop_legacy_launchd(legacy_label)? {
        notes.push(note);
    }
    if cfg!(target_os = "macos") {
        notes.push(install_launchd(&bin, load)?);
        notes.push(
            "Cursor keychain: atr serve must run in the desktop (Aqua) session, not over SSH."
                .into(),
        );
    } else {
        notes.push(install_systemd(load)?);
    }
    if let Some(cfg) = cfg {
        if cfg.log.path.as_os_str().is_empty() {
            notes.push("set [log] path so `atr logs` works".into());
        }
    }
    Ok(notes.join("\n"))
}

fn install_launchd(bin: &Path, load: bool) -> Result<String> {
    let home = home_dir();
    let dest = plist_path();
    if let Some(dir) = dest.parent() {
        fs::create_dir_all(dir)?;
    }
    fs::create_dir_all(home.join("Library/Logs/atr")).ok();
    fs::write(&dest, render_plist(&home, bin))?;
    if load && !skip_load() {
        let uid = users_uid();
        let _ = Command::new("launchctl")
            .args([
                "bootout",
                &format!("gui/{uid}"),
                &dest.display().to_string(),
            ])
            .status();
        let st = Command::new("launchctl")
            .args([
                "bootstrap",
                &format!("gui/{uid}"),
                &dest.display().to_string(),
            ])
            .status()
            .with_context(|| "launchctl bootstrap")?;
        if !st.success() {
            bail!("launchctl bootstrap failed; run from a desktop Terminal, not SSH");
        }
        let _ = Command::new("launchctl")
            .args(["enable", &format!("gui/{uid}/{LAUNCHD_LABEL}")])
            .status();
        let _ = Command::new("launchctl")
            .args(["kickstart", "-k", &format!("gui/{uid}/{LAUNCHD_LABEL}")])
            .status();
    }
    Ok(format!("installed LaunchAgent {}", dest.display()))
}

fn install_systemd(load: bool) -> Result<String> {
    let dest = unit_path();
    if let Some(dir) = dest.parent() {
        fs::create_dir_all(dir)?;
    }
    fs::write(&dest, UNIT_TEMPLATE)?;
    if load && !skip_load() {
        let _ = Command::new("systemctl")
            .args(["--user", "daemon-reload"])
            .status();
        let st = Command::new("systemctl")
            .args(["--user", "enable", "--now", "atr"])
            .status();
        if !st.map(|s| s.success()).unwrap_or(false) {
            bail!("systemctl --user enable --now atr failed");
        }
    }
    Ok(format!("installed systemd user unit {}", dest.display()))
}

pub fn uninstall() -> Result<String> {
    if cfg!(target_os = "macos") {
        let uid = users_uid();
        let dest = plist_path();
        let _ = Command::new("launchctl")
            .args(["bootout", &format!("gui/{uid}/{LAUNCHD_LABEL}")])
            .status();
        if dest.exists() {
            fs::remove_file(&dest)?;
        }
        Ok(format!("removed {LAUNCHD_LABEL}"))
    } else {
        let _ = Command::new("systemctl")
            .args(["--user", "disable", "--now", "atr"])
            .status();
        let dest = unit_path();
        if dest.exists() {
            fs::remove_file(&dest)?;
        }
        Ok("removed atr.service".into())
    }
}

pub fn restart() -> Result<String> {
    if skip_load() {
        return Ok("service load skipped (ATR_INIT_SKIP_LOAD)".into());
    }
    if cfg!(target_os = "macos") {
        let uid = users_uid();
        let st = Command::new("launchctl")
            .args(["kickstart", "-k", &format!("gui/{uid}/{LAUNCHD_LABEL}")])
            .status()
            .with_context(|| "launchctl kickstart")?;
        if !st.success() {
            bail!("launchctl kickstart failed");
        }
        Ok(format!("restarted {LAUNCHD_LABEL}"))
    } else {
        let st = Command::new("systemctl")
            .args(["--user", "restart", "atr"])
            .status()
            .with_context(|| "systemctl restart")?;
        if !st.success() {
            bail!("systemctl --user restart atr failed");
        }
        Ok("restarted atr.service".into())
    }
}

fn users_uid() -> u32 {
    unsafe { libc::getuid() }
}

/// Reverse-DNS LaunchAgent identifiers only. Rejects paths and the current label.
pub fn valid_legacy_label(label: &str) -> bool {
    let label = label.trim();
    !label.is_empty()
        && label.len() <= 128
        && label != LAUNCHD_LABEL
        && label
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
}

/// Boot out `--legacy-label` if the caller passed one. Any old label works;
/// nothing in-tree hardcodes a previous product name.
pub fn stop_legacy_launchd(legacy_label: Option<&str>) -> Result<Option<String>> {
    let Some(raw) = legacy_label else {
        return Ok(None);
    };
    let label = raw.trim();
    if label.is_empty() {
        return Ok(None);
    }
    if !valid_legacy_label(label) {
        bail!("invalid --legacy-label {label:?}: use a launchd identifier, not a path");
    }
    if !cfg!(target_os = "macos") {
        return Ok(Some(format!(
            "ignored --legacy-label {label} (LaunchAgents are macOS only)"
        )));
    }
    let uid = users_uid();
    let _ = Command::new("launchctl")
        .args(["bootout", &format!("gui/{uid}/{label}")])
        .status();
    let old_plist = home_dir()
        .join("Library/LaunchAgents")
        .join(format!("{label}.plist"));
    if old_plist.exists() {
        fs::remove_file(&old_plist)
            .with_context(|| format!("remove leftover {}", old_plist.display()))?;
    }
    Ok(Some(format!("stopped leftover LaunchAgent {label}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plist_label_is_neutral() {
        assert!(PLIST_TEMPLATE.contains(&format!("<string>{LAUNCHD_LABEL}</string>")));
        assert!(PLIST_TEMPLATE.contains("__HOME__/.config/atr/atr-serve.sh"));
        assert!(PLIST_TEMPLATE.contains("/dev/null"));
    }

    #[test]
    fn legacy_label_accepts_any_reverse_dns() {
        assert!(valid_legacy_label("com.example.old-atr"));
        assert!(valid_legacy_label("dev.old.serve"));
        assert!(!valid_legacy_label(LAUNCHD_LABEL));
        assert!(!valid_legacy_label("../evil"));
        assert!(!valid_legacy_label("gui/501/x"));
        assert!(!valid_legacy_label(""));
        assert!(stop_legacy_launchd(None).unwrap().is_none());
        assert!(stop_legacy_launchd(Some("")).unwrap().is_none());
        assert!(stop_legacy_launchd(Some(LAUNCHD_LABEL)).is_err());
        assert!(stop_legacy_launchd(Some("../evil")).is_err());
        if !cfg!(target_os = "macos") {
            let note = stop_legacy_launchd(Some("com.example.old-atr"))
                .unwrap()
                .expect("linux notes that the flag is macOS-only");
            assert!(note.contains("com.example.old-atr"), "{note}");
            assert!(note.contains("macOS"), "{note}");
        }
    }

    #[test]
    fn serve_sh_falls_back_to_atr() {
        assert!(SERVE_SH_TEMPLATE.contains("exec atr serve"));
        let rendered = render_serve_sh(Path::new("/opt/atr/bin/atr"));
        assert!(rendered.contains("/opt/atr/bin/atr"));
    }
}
