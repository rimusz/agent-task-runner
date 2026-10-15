use crate::config::{default_atr_dir, expand_path, paths_eq, Config};
use crate::util::current_bin;
use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

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
    render_plist_with(home, bin, LAUNCHD_LABEL, &default_config_file())
}

pub fn render_plist_with(home: &Path, bin: &Path, label: &str, config_path: &Path) -> String {
    let bin_dir = bin.parent().unwrap_or(home);
    PLIST_TEMPLATE
        .replace("__HOME__", &home.display().to_string())
        .replace("__ATR_BIN_DIR__", &bin_dir.display().to_string())
        .replace("__LABEL__", label)
        .replace("__ATR_CONFIG__", &config_path.display().to_string())
}

fn default_config_file() -> PathBuf {
    default_atr_dir().join("config.toml")
}

/// True when this config is `~/.config/atr/config.toml` and ATR_CONFIG is unset.
pub fn config_is_default_path(config_path: &Path) -> bool {
    if std::env::var("ATR_CONFIG")
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false)
    {
        let env = expand_path(&std::env::var("ATR_CONFIG").unwrap_or_default());
        if !paths_eq(&env, &default_config_file()) {
            return false;
        }
    }
    paths_eq(config_path, &default_config_file())
}

/// Derive a LaunchAgent label from a non-default config path so init cannot
/// clobber the user's `dev.atr.serve` job.
pub fn launchd_label_for(config_path: &Path) -> String {
    if config_is_default_path(config_path) {
        return LAUNCHD_LABEL.to_string();
    }
    let canon = config_path
        .canonicalize()
        .unwrap_or_else(|_| config_path.to_path_buf());
    let hex = format!("{:x}", Sha256::digest(canon.to_string_lossy().as_bytes()));
    format!("{LAUNCHD_LABEL}.{}", &hex[..8])
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallDecision {
    Install,
    Skip(String),
}

/// Init must not silently replace a loaded `dev.atr.serve` job, and must not
/// install a service for a non-default config/HOME unless `--force-service`.
pub fn decide_service_install(
    force: bool,
    config_is_default: bool,
    existing_loaded: bool,
    loaded_label: &str,
) -> Result<InstallDecision> {
    if existing_loaded && !force {
        bail!(
            "{loaded_label} is already loaded; pass --force-service to replace it, or --no-service to skip"
        );
    }
    if !config_is_default && !force {
        return Ok(InstallDecision::Skip(format!(
            "skipped user service (config is not {}); pass --force-service to install",
            default_config_file().display()
        )));
    }
    Ok(InstallDecision::Install)
}

pub fn service_loaded(label: &str) -> bool {
    if skip_load() {
        return false;
    }
    if cfg!(target_os = "macos") {
        let uid = users_uid();
        Command::new("launchctl")
            .args(["print", &format!("gui/{uid}/{label}")])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    } else {
        Command::new("systemctl")
            .args(["--user", "is-active", "--quiet", "atr"])
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }
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
    install_with(cfg, load, legacy_label, None)
}

pub fn install_for_init(cfg: Option<&Config>, config_path: &Path, force: bool) -> Result<String> {
    let default = config_is_default_path(config_path);
    let label = launchd_label_for(config_path);
    let loaded_default = service_loaded(LAUNCHD_LABEL);
    let loaded_this = label != LAUNCHD_LABEL && service_loaded(&label);
    match decide_service_install(force, default, loaded_default || loaded_this, LAUNCHD_LABEL)? {
        InstallDecision::Skip(msg) => return Ok(msg),
        InstallDecision::Install => {}
    }
    install_with(cfg, true, None, Some(config_path))
}

fn install_with(
    cfg: Option<&Config>,
    load: bool,
    legacy_label: Option<&str>,
    config_path: Option<&Path>,
) -> Result<String> {
    let bin = current_bin();
    write_launcher(&bin)?;
    let mut notes = Vec::new();
    if let Some(note) = stop_legacy_launchd(legacy_label)? {
        notes.push(note);
    }
    let config_path = config_path
        .map(Path::to_path_buf)
        .unwrap_or_else(default_config_file);
    if cfg!(target_os = "macos") {
        notes.push(install_launchd(&bin, load, &config_path)?);
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

fn install_launchd(bin: &Path, load: bool, config_path: &Path) -> Result<String> {
    let home = home_dir();
    let label = launchd_label_for(config_path);
    let dest = home
        .join("Library/LaunchAgents")
        .join(format!("{label}.plist"));
    if let Some(dir) = dest.parent() {
        fs::create_dir_all(dir)?;
    }
    fs::create_dir_all(home.join("Library/Logs/atr")).ok();
    fs::write(&dest, render_plist_with(&home, bin, &label, config_path))?;
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
            .args(["enable", &format!("gui/{uid}/{label}")])
            .status();
        let _ = Command::new("launchctl")
            .args(["kickstart", "-k", &format!("gui/{uid}/{label}")])
            .status();
    }
    Ok(format!(
        "installed LaunchAgent {} ({label})",
        dest.display()
    ))
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
        assert!(PLIST_TEMPLATE.contains("<string>__LABEL__</string>"));
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

    #[test]
    fn decide_service_install_refuses_loaded_job() {
        let err = decide_service_install(false, true, true, LAUNCHD_LABEL)
            .unwrap_err()
            .to_string();
        assert!(err.contains("already loaded"), "{err}");
        assert!(err.contains("--force-service"), "{err}");
        assert!(matches!(
            decide_service_install(true, true, true, LAUNCHD_LABEL).unwrap(),
            InstallDecision::Install
        ));
        match decide_service_install(false, false, false, LAUNCHD_LABEL).unwrap() {
            InstallDecision::Skip(msg) => {
                assert!(msg.contains("--force-service"), "{msg}");
                assert!(msg.contains("skipped"), "{msg}");
            }
            other => panic!("{other:?}"),
        }
        assert!(matches!(
            decide_service_install(true, false, false, LAUNCHD_LABEL).unwrap(),
            InstallDecision::Install
        ));
    }

    #[test]
    fn non_default_config_gets_distinct_label() {
        let other = PathBuf::from("/tmp/atr-alt/config.toml");
        let label = launchd_label_for(&other);
        assert!(label.starts_with(LAUNCHD_LABEL), "{label}");
        assert_ne!(label, LAUNCHD_LABEL);
        assert_eq!(label, launchd_label_for(&other));
        let rendered = render_plist_with(
            Path::new("/Users/me"),
            Path::new("/opt/atr"),
            &label,
            &other,
        );
        assert!(
            rendered.contains(&format!("<string>{label}</string>")),
            "{rendered}"
        );
        assert!(rendered.contains("/tmp/atr-alt/config.toml"), "{rendered}");
    }
}
