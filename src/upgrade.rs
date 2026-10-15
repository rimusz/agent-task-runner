use crate::config::{load_config, Config};
use crate::util::{backup_dir, copy_file, current_bin, host_asset, parse_sha256_line, sha256_file};
use anyhow::{bail, Context, Result};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

pub fn upgrade(version: Option<&str>, config_path: &Path) -> Result<String> {
    let cfg = load_config(config_path).ok();
    let repo = cfg
        .as_ref()
        .map(|c| c.update.github_repo.clone())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "rimusz/agent-task-runner".into());
    let asset = host_asset();
    if asset == "unknown" {
        bail!("unsupported platform for atr upgrade");
    }
    let ver = match version {
        Some(v) => v.trim().trim_start_matches('v').to_string(),
        None => latest_version(&repo)?,
    };
    let tag = format!("v{ver}");
    let tmp = tempfile_dir()?;
    let name = format!("atr-{ver}-{asset}.tar.gz");
    let url = format!("https://github.com/{repo}/releases/download/{tag}/{name}");
    let tgz = tmp.join(&name);
    download(&url, &tgz)?;
    let sha_url = format!("{url}.sha256");
    let sha_file = tmp.join(format!("{name}.sha256"));
    download(&sha_url, &sha_file)?;
    let expected = parse_sha256_line(&fs::read_to_string(&sha_file)?)
        .ok_or_else(|| anyhow::anyhow!("could not parse sha256 file"))?;
    let got = sha256_file(&tgz)?;
    if got != expected {
        bail!("sha256 mismatch: got {got}, expected {expected}");
    }
    let extracted = tmp.join("atr");
    extract_atr(&tgz, &extracted)?;
    backup_current(cfg.as_ref(), config_path)?;
    let dest = current_bin();
    replace_binary(&extracted, &dest)?;
    if let Err(e) = crate::service::restart() {
        eprintln!("restart after upgrade: {e:#}");
    }
    Ok(format!("upgraded to {tag} ({asset}) -> {}", dest.display()))
}

pub fn rollback(config_path: &Path) -> Result<String> {
    let dir = backup_dir();
    let bin_bak = dir.join("atr");
    if !bin_bak.exists() {
        bail!("no backup at {}", bin_bak.display());
    }
    replace_binary(&bin_bak, &current_bin())?;
    let cfg_bak = dir.join("config.toml");
    if cfg_bak.exists() && config_path.exists() {
        copy_file(&cfg_bak, config_path)?;
    }
    if let Ok(cfg) = load_config(config_path) {
        let db_bak = dir.join("state.db");
        if db_bak.exists() {
            copy_file(&db_bak, &cfg.state_db)?;
            for suf in ["-wal", "-shm"] {
                let src = dir.join(format!("state.db{suf}"));
                if src.exists() {
                    let mut dest = cfg.state_db.as_os_str().to_os_string();
                    dest.push(suf);
                    copy_file(&src, Path::new(&dest))?;
                }
            }
        }
    }
    if let Err(e) = crate::service::restart() {
        eprintln!("restart after rollback: {e:#}");
    }
    Ok(format!("restored backup from {}", dir.display()))
}

fn backup_current(cfg: Option<&Config>, config_path: &Path) -> Result<()> {
    let dir = backup_dir();
    fs::create_dir_all(&dir)?;
    let bin = current_bin();
    if bin.exists() {
        copy_file(&bin, &dir.join("atr"))?;
    }
    if config_path.exists() {
        copy_file(config_path, &dir.join("config.toml"))?;
    }
    if let Some(cfg) = cfg {
        if cfg.state_db.exists() {
            copy_file(&cfg.state_db, &dir.join("state.db"))?;
            for suf in ["-wal", "-shm"] {
                let mut src = cfg.state_db.as_os_str().to_os_string();
                src.push(suf);
                let src = PathBuf::from(src);
                if src.exists() {
                    copy_file(&src, &dir.join(format!("state.db{suf}")))?;
                }
            }
        }
    }
    Ok(())
}

fn latest_version(repo: &str) -> Result<String> {
    let url = format!("https://github.com/{repo}/releases/latest");
    let resp = ureq::get(&url)
        .timeout(std::time::Duration::from_secs(20))
        .call()
        .with_context(|| format!("GET {url}"))?;
    let loc = resp.get_url().to_string();
    loc.rsplit('/')
        .next()
        .map(|s| s.trim().trim_start_matches('v').to_string())
        .filter(|s| !s.is_empty() && s != "latest")
        .ok_or_else(|| anyhow::anyhow!("could not parse latest tag from {loc}"))
}

fn download(url: &str, dest: &Path) -> Result<()> {
    let resp = ureq::get(url)
        .timeout(std::time::Duration::from_secs(120))
        .call()
        .with_context(|| format!("GET {url}"))?;
    let mut reader = resp.into_reader();
    let mut buf = Vec::new();
    reader.read_to_end(&mut buf)?;
    if let Some(dir) = dest.parent() {
        fs::create_dir_all(dir)?;
    }
    fs::write(dest, buf).with_context(|| format!("write {}", dest.display()))
}

fn extract_atr(tgz: &Path, dest: &Path) -> Result<()> {
    let tmp = tgz.parent().unwrap_or(Path::new(".")).join("extract");
    fs::create_dir_all(&tmp)?;
    let st = std::process::Command::new("tar")
        .args([
            "-xzf",
            &tgz.display().to_string(),
            "-C",
            &tmp.display().to_string(),
        ])
        .status()
        .with_context(|| "tar")?;
    if !st.success() {
        bail!("tar extract failed");
    }
    let cand = tmp.join("atr");
    if cand.exists() {
        fs::copy(&cand, dest)?;
        return Ok(());
    }
    for entry in fs::read_dir(&tmp)? {
        let p = entry?.path();
        if p.file_name().and_then(|n| n.to_str()) == Some("atr") {
            fs::copy(&p, dest)?;
            return Ok(());
        }
    }
    bail!("tarball did not contain atr")
}

fn replace_binary(src: &Path, dest: &Path) -> Result<()> {
    if let Some(dir) = dest.parent() {
        fs::create_dir_all(dir)?;
    }
    let tmp = dest.with_extension("new");
    fs::copy(src, &tmp).with_context(|| format!("copy {}", src.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o755))?;
    }
    fs::rename(&tmp, dest).with_context(|| format!("replace {}", dest.display()))?;
    Ok(())
}

fn tempfile_dir() -> Result<PathBuf> {
    let base = std::env::temp_dir().join(format!("atr-upgrade-{}", std::process::id()));
    fs::create_dir_all(&base)?;
    Ok(base)
}

pub fn follow_logs(cfg: &Config, follow: bool, task: Option<&str>) -> Result<()> {
    if cfg.log.path.as_os_str().is_empty() {
        bail!("[log] path is empty; set it in config.toml");
    }
    if !cfg.log.path.exists() {
        bail!("log file {} does not exist yet", cfg.log.path.display());
    }
    if follow {
        let mut cmd = std::process::Command::new("tail");
        cmd.arg("-n").arg("50").arg("-f").arg(&cfg.log.path);
        if let Some(key) = task {
            // tail then grep -F --line-buffered
            let mut tail = cmd
                .stdout(std::process::Stdio::piped())
                .spawn()
                .context("tail")?;
            let grep = std::process::Command::new("grep")
                .arg("-F")
                .arg("--line-buffered")
                .arg(key)
                .stdin(tail.stdout.take().unwrap())
                .status()
                .context("grep")?;
            let _ = tail.kill();
            if !grep.success() && grep.code() != Some(1) {
                bail!("grep failed");
            }
            return Ok(());
        }
        let st = cmd.status().context("tail")?;
        if !st.success() {
            bail!("tail failed");
        }
        return Ok(());
    }
    let text = fs::read_to_string(&cfg.log.path)?;
    if let Some(key) = task {
        for line in text.lines() {
            if line.contains(key) {
                println!("{line}");
            }
        }
    } else {
        print!("{text}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backup_and_restore_files() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("atr");
        fs::write(&src, b"new").unwrap();
        let dest = dir.path().join("bin").join("atr");
        fs::create_dir_all(dest.parent().unwrap()).unwrap();
        fs::write(&dest, b"old").unwrap();
        replace_binary(&src, &dest).unwrap();
        assert_eq!(fs::read(&dest).unwrap(), b"new");
    }
}
