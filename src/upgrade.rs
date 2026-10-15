use crate::config::{load_config, Config};
use crate::util::{
    backup_dir, copy_file, current_bin, host_asset, host_target, parse_sha256_line, sha256_file,
};
use anyhow::{bail, Context, Result};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

/// Where `upgrade`/`rollback` read and write. Tests point this at a mock
/// release server and temp dirs.
pub(crate) struct Env {
    /// `https://github.com` in production.
    pub base: String,
    pub bin: PathBuf,
    pub backup: PathBuf,
    pub tmp: PathBuf,
}

impl Env {
    fn real() -> Result<Self> {
        Ok(Env {
            base: "https://github.com".into(),
            bin: current_bin(),
            backup: backup_dir(),
            tmp: tempfile_dir()?,
        })
    }
}

pub fn upgrade(version: Option<&str>, config_path: &Path) -> Result<String> {
    let env = Env::real()?;
    let msg = upgrade_with(version, config_path, &env)?;
    if let Err(e) = crate::service::restart() {
        eprintln!("restart after upgrade: {e:#}");
    }
    Ok(msg)
}

pub(crate) fn upgrade_with(version: Option<&str>, config_path: &Path, env: &Env) -> Result<String> {
    let cfg = load_config(config_path).ok();
    let repo = cfg
        .as_ref()
        .map(|c| c.update.github_repo.clone())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "rimusz/agent-task-runner".into());
    let target = host_target();
    let asset = host_asset();
    if target == "unknown" || asset == "unknown" {
        bail!("unsupported platform for atr upgrade");
    }
    let ver = match version {
        Some(v) => v.trim().trim_start_matches('v').to_string(),
        None => latest_version(&env.base, &repo)?,
    };
    let tag = format!("v{ver}");
    let tmp = &env.tmp;
    fs::create_dir_all(tmp)?;
    // Friendly name first (atr-<ver>-linux-x86_64.tar.gz); fall back to the
    // Rust-triple name used by releases published before the rename.
    let mut fetched = None;
    let mut last_err = None;
    for plat in [asset, target] {
        let name = format!("atr-{ver}-{plat}.tar.gz");
        let url = format!("{}/{repo}/releases/download/{tag}/{name}", env.base);
        let tgz = tmp.join(&name);
        match download(&url, &tgz) {
            Ok(()) => {
                fetched = Some((name, url, tgz));
                break;
            }
            Err(e) => last_err = Some(e),
        }
    }
    let (name, url, tgz) = match fetched {
        Some(f) => f,
        None => return Err(last_err.unwrap_or_else(|| anyhow::anyhow!("download failed"))),
    };
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
    backup_current(cfg.as_ref(), config_path, &env.bin, &env.backup)?;
    replace_binary(&extracted, &env.bin)?;
    Ok(format!(
        "upgraded to {tag} ({asset}) -> {}",
        env.bin.display()
    ))
}

pub fn rollback(config_path: &Path) -> Result<String> {
    let msg = rollback_with(config_path, &current_bin(), &backup_dir())?;
    if let Err(e) = crate::service::restart() {
        eprintln!("restart after rollback: {e:#}");
    }
    Ok(msg)
}

pub(crate) fn rollback_with(config_path: &Path, bin: &Path, dir: &Path) -> Result<String> {
    let bin_bak = dir.join("atr");
    if !bin_bak.exists() {
        bail!("no backup at {}", bin_bak.display());
    }
    replace_binary(&bin_bak, bin)?;
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
    Ok(format!("restored backup from {}", dir.display()))
}

fn backup_current(cfg: Option<&Config>, config_path: &Path, bin: &Path, dir: &Path) -> Result<()> {
    fs::create_dir_all(dir)?;
    if bin.exists() {
        copy_file(bin, &dir.join("atr"))?;
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

fn latest_version(base: &str, repo: &str) -> Result<String> {
    let url = format!("{base}/{repo}/releases/latest");
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

    use std::collections::HashMap;
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;

    type Routes = HashMap<String, (u16, Vec<(String, String)>, Vec<u8>)>;

    /// Tiny HTTP/1.1 server: path -> (status, extra headers, body).
    fn serve(routes: Routes) -> String {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        std::thread::spawn(move || {
            for stream in l.incoming() {
                let Ok(mut s) = stream else { break };
                let mut r = BufReader::new(s.try_clone().unwrap());
                let mut first = String::new();
                if r.read_line(&mut first).is_err() {
                    continue;
                }
                loop {
                    let mut h = String::new();
                    if r.read_line(&mut h).unwrap_or(0) == 0 || h == "\r\n" {
                        break;
                    }
                }
                let path = first.split_whitespace().nth(1).unwrap_or("/").to_string();
                let (code, hdrs, body) =
                    routes
                        .get(&path)
                        .cloned()
                        .unwrap_or((404, vec![], b"nope".to_vec()));
                let mut head = format!(
                    "HTTP/1.1 {code} X\r\nContent-Length: {}\r\nConnection: close\r\n",
                    body.len()
                );
                for (k, v) in hdrs {
                    head.push_str(&format!("{k}: {v}\r\n"));
                }
                head.push_str("\r\n");
                let _ = s.write_all(head.as_bytes());
                let _ = s.write_all(&body);
            }
        });
        format!("http://{addr}")
    }

    fn tarball(dir: &Path, content: &[u8]) -> Vec<u8> {
        let stage = dir.join("stage");
        fs::create_dir_all(&stage).unwrap();
        fs::write(stage.join("atr"), content).unwrap();
        let out = dir.join("pkg.tar.gz");
        let st = std::process::Command::new("tar")
            .args([
                "-czf",
                &out.display().to_string(),
                "-C",
                &stage.display().to_string(),
                "atr",
            ])
            .status()
            .unwrap();
        assert!(st.success());
        fs::read(out).unwrap()
    }

    fn sha_hex(data: &[u8]) -> String {
        use sha2::{Digest, Sha256};
        format!("{:x}", Sha256::digest(data))
    }

    fn release(dir: &Path, ver: &str, content: &[u8], sha_override: Option<&str>) -> Routes {
        let tgz = tarball(dir, content);
        let sha = sha_override
            .map(str::to_string)
            .unwrap_or_else(|| sha_hex(&tgz));
        let repo = "rimusz/agent-task-runner";
        let name = format!("atr-{ver}-{}.tar.gz", host_asset());
        let mut m = HashMap::new();
        m.insert(
            format!("/{repo}/releases/latest"),
            (
                302,
                vec![("Location".into(), format!("/{repo}/releases/tag/v{ver}"))],
                vec![],
            ),
        );
        m.insert(
            format!("/{repo}/releases/tag/v{ver}"),
            (200, vec![], b"ok".to_vec()),
        );
        m.insert(
            format!("/{repo}/releases/download/v{ver}/{name}"),
            (200, vec![], tgz),
        );
        m.insert(
            format!("/{repo}/releases/download/v{ver}/{name}.sha256"),
            (200, vec![], format!("{sha}  {name}\n").into_bytes()),
        );
        m
    }

    fn env_in(dir: &Path, base: String) -> Env {
        let bin = dir.join("bin").join("atr");
        fs::create_dir_all(bin.parent().unwrap()).unwrap();
        fs::write(&bin, b"OLD-BINARY").unwrap();
        Env {
            base,
            bin,
            backup: dir.join("backup"),
            tmp: dir.join("tmp"),
        }
    }

    fn write_cfg(dir: &Path) -> PathBuf {
        let cfg = dir.join("config.toml");
        fs::write(dir.join("p.md"), "p\n").unwrap();
        fs::write(
            &cfg,
            format!(
                "listen = \"127.0.0.1:0\"\nstate_db = \"{d}/state.db\"\nworktree_root = \"{d}/wt\"\nrepo_path = \"{d}\"\nsystem_prompt_file = \"{d}/p.md\"\n[[provider]]\nname = \"claude\"\nkind = \"claude\"\ncli = \"/bin/true\"\n",
                d = dir.display()
            ),
        )
        .unwrap();
        fs::write(dir.join("state.db"), b"DB-V1").unwrap();
        cfg
    }

    #[test]
    fn upgrade_latest_from_mock_release_then_rollback() {
        if host_asset() == "unknown" {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let base = serve(release(dir.path(), "9.9.9", b"NEW-BINARY", None));
        let env = env_in(dir.path(), base);
        let cfg = write_cfg(dir.path());
        let msg = upgrade_with(None, &cfg, &env).unwrap();
        assert!(msg.contains("v9.9.9"), "{msg}");
        assert_eq!(fs::read(&env.bin).unwrap(), b"NEW-BINARY");
        assert_eq!(fs::read(env.backup.join("atr")).unwrap(), b"OLD-BINARY");
        assert_eq!(fs::read(env.backup.join("state.db")).unwrap(), b"DB-V1");
        assert!(env.backup.join("config.toml").exists());

        // State changes after the upgrade; rollback restores binary + db + config.
        fs::write(dir.path().join("state.db"), b"DB-V2").unwrap();
        let orig_cfg = fs::read_to_string(&cfg).unwrap();
        fs::write(&cfg, format!("{orig_cfg}\n# edited\n")).unwrap();
        let msg = rollback_with(&cfg, &env.bin, &env.backup).unwrap();
        assert!(msg.contains("restored"), "{msg}");
        assert_eq!(fs::read(&env.bin).unwrap(), b"OLD-BINARY");
        assert_eq!(fs::read(dir.path().join("state.db")).unwrap(), b"DB-V1");
        assert_eq!(fs::read_to_string(&cfg).unwrap(), orig_cfg);
    }

    #[test]
    fn upgrade_explicit_version_strips_v() {
        if host_asset() == "unknown" {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let base = serve(release(dir.path(), "1.2.3", b"V123", None));
        let env = env_in(dir.path(), base);
        let msg = upgrade_with(Some("v1.2.3"), &dir.path().join("missing.toml"), &env).unwrap();
        assert!(msg.contains("v1.2.3"), "{msg}");
        assert_eq!(fs::read(&env.bin).unwrap(), b"V123");
    }

    #[test]
    fn upgrade_refuses_sha_mismatch_and_keeps_binary() {
        if host_asset() == "unknown" {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let bad = "0".repeat(64);
        let base = serve(release(dir.path(), "9.9.9", b"EVIL", Some(&bad)));
        let env = env_in(dir.path(), base);
        let err = upgrade_with(None, &dir.path().join("missing.toml"), &env).unwrap_err();
        assert!(format!("{err:#}").contains("sha256 mismatch"), "{err:#}");
        assert_eq!(fs::read(&env.bin).unwrap(), b"OLD-BINARY");
        assert!(!env.backup.join("atr").exists());
    }

    #[test]
    fn upgrade_garbled_sha_file_errors() {
        if host_asset() == "unknown" {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let base = serve(release(dir.path(), "9.9.9", b"X", Some("not-a-hash")));
        let env = env_in(dir.path(), base);
        let err = upgrade_with(None, &dir.path().join("missing.toml"), &env).unwrap_err();
        assert!(format!("{err:#}").contains("sha256"), "{err:#}");
        assert_eq!(fs::read(&env.bin).unwrap(), b"OLD-BINARY");
    }

    #[test]
    fn upgrade_missing_release_errors_and_keeps_binary() {
        let dir = tempfile::tempdir().unwrap();
        let base = serve(HashMap::new());
        let env = env_in(dir.path(), base);
        assert!(upgrade_with(Some("7.7.7"), &dir.path().join("missing.toml"), &env).is_err());
        assert!(upgrade_with(None, &dir.path().join("missing.toml"), &env).is_err());
        assert_eq!(fs::read(&env.bin).unwrap(), b"OLD-BINARY");
    }

    #[test]
    fn rollback_without_backup_errors() {
        let dir = tempfile::tempdir().unwrap();
        let err = rollback_with(
            &dir.path().join("c.toml"),
            &dir.path().join("atr"),
            &dir.path().join("nobackup"),
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("no backup"), "{err:#}");
    }

    #[test]
    fn tarball_without_atr_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let stage = dir.path().join("s");
        fs::create_dir_all(&stage).unwrap();
        fs::write(stage.join("other"), b"x").unwrap();
        let tgz = dir.path().join("t.tar.gz");
        std::process::Command::new("tar")
            .args([
                "-czf",
                &tgz.display().to_string(),
                "-C",
                &stage.display().to_string(),
                "other",
            ])
            .status()
            .unwrap();
        let err = extract_atr(&tgz, &dir.path().join("out")).unwrap_err();
        assert!(format!("{err:#}").contains("did not contain atr"));
    }

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
