//! `atr providers detect` against fake CLIs on PATH.
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;

fn atr() -> Command {
    Command::new(env!("CARGO_BIN_EXE_atr"))
}

fn install_fakes(bin: &std::path::Path) {
    let src = format!("{}/tests/fixtures/fake-cli.sh", env!("CARGO_MANIFEST_DIR"));
    fs::create_dir_all(bin).unwrap();
    for name in [
        "claude",
        "grok",
        "cursor-agent",
        "codex",
        "opencode",
        "kimi",
    ] {
        let dest = bin.join(name);
        fs::copy(&src, &dest).unwrap();
        fs::set_permissions(&dest, fs::Permissions::from_mode(0o755)).unwrap();
    }
}

#[test]
fn providers_detect_lists_version_auth_and_login() {
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("bin");
    let home = dir.path().join("home");
    fs::create_dir_all(&home).unwrap();
    install_fakes(&bin);
    let path = format!("{}:/bin:/usr/bin", bin.display());
    let out = atr()
        .arg("providers")
        .arg("detect")
        .env("PATH", &path)
        .env("HOME", &home)
        .env("ATR_FAKE_SIGNED_claude", "1")
        .env("ATR_FAKE_SIGNED_opencode", "1")
        .env("ATR_FAKE_SIGNED_kimi", "0")
        .env("ATR_FAKE_SIGNED_grok", "0")
        .env("ATR_OPENCODE_CONFIG", home.join("opencode.json"))
        .output()
        .expect("atr providers detect");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.status.success(), "{text}");
    assert!(text.contains("detected CLIs"), "{text}");
    assert!(text.contains("claude"), "{text}");
    assert!(text.contains("1.2.3"), "{text}");
    assert!(text.contains("signed in"), "{text}");
    assert!(text.contains("login: grok"), "{text}");
    assert!(
        text.contains("opencode auth login") || text.contains("moonshotai"),
        "{text}"
    );
    assert!(text.contains("atr providers add kimi"), "{text}");
    assert!(!text.contains("sk-"), "{text}");
    assert!(!text.contains("gho_"), "{text}");
}

#[test]
fn providers_detect_keychain_and_path_mismatch() {
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("bin");
    let home = dir.path().join("home");
    let local = home.join(".local/bin");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&local).unwrap();
    install_fakes(&bin);
    fs::copy(bin.join("grok"), local.join("grok")).unwrap();
    fs::set_permissions(local.join("grok"), fs::Permissions::from_mode(0o755)).unwrap();
    let cfg_path = dir.path().join("config.toml");
    fs::write(
        &cfg_path,
        format!(
            r#"
listen = "127.0.0.1:0"
repo_path = "{root}"
system_prompt_file = "{root}/p.md"
[[provider]]
name = "grok"
kind = "grok"
cli = "{local_grok}"
"#,
            root = dir.path().display(),
            local_grok = local.join("grok").display()
        ),
    )
    .unwrap();
    fs::write(dir.path().join("p.md"), "p\n").unwrap();
    let path = format!("{}:/bin:/usr/bin", bin.display());
    let out = atr()
        .arg("providers")
        .arg("detect")
        .env("PATH", &path)
        .env("HOME", &home)
        .env("ATR_CONFIG", &cfg_path)
        .env("ATR_FAKE_KEYCHAIN_cursor_agent", "1")
        .env("ATR_FAKE_SIGNED_grok", "1")
        .env("ATR_OPENCODE_CONFIG", home.join("opencode.json"))
        .output()
        .expect("atr providers detect");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.status.success(), "{text}");
    assert!(text.contains("unverified (keychain locked)"), "{text}");
    let cursor_line = text
        .lines()
        .find(|l| l.contains("cursor") && !l.contains("login"))
        .unwrap_or("");
    assert!(
        cursor_line.contains("unverified (keychain locked)"),
        "{cursor_line}\n{text}"
    );
    assert!(!cursor_line.contains("not signed in"), "{cursor_line}");
    assert!(
        !text.contains('\u{1b}') && !text.contains("locked."),
        "ANSI/version leak: {text:?}"
    );
    assert!(text.contains("warning: config uses"), "{text}");
    assert!(
        text.contains(&local.join("grok").display().to_string()),
        "{text}"
    );
    assert!(
        text.contains(&bin.join("grok").display().to_string()),
        "{text}"
    );
}
