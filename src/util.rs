use crate::config::Config;
use anyhow::{bail, Context, Result};
use std::fs;
use std::path::{Path, PathBuf};

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
    fs::write(&path, format!("{}", std::process::id()))
        .with_context(|| format!("write {}", path.display()))
}

pub fn read_pid(cfg: &Config) -> Option<i32> {
    let text = fs::read_to_string(pid_path(cfg)).ok()?;
    text.trim().parse().ok()
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

pub fn host_target() -> &'static str {
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    {
        return "aarch64-apple-darwin";
    }
    #[cfg(all(target_os = "linux", target_arch = "x86_64", target_env = "musl"))]
    {
        return "x86_64-unknown-linux-musl";
    }
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    {
        return "x86_64-unknown-linux-gnu";
    }
    #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
    {
        return "aarch64-unknown-linux-gnu";
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
    use super::*;

    #[test]
    fn parse_sha256_line_takes_first_token() {
        let h = "a".repeat(64);
        assert_eq!(
            parse_sha256_line(&format!("{h}  atr-0.1.1.tar.gz")),
            Some(h)
        );
    }
}
