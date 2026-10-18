//! CLI equivalents of Telegram-only commands: `atr config get/set`,
//! `atr pause` / `atr resume` and `atr retry` (no running service: they fall
//! back to the config file / state database).
use std::fs;
use std::process::Command;

fn setup() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    fs::create_dir_all(&home).unwrap();
    fs::write(dir.path().join("p.md"), "p\n").unwrap();
    let cfg = dir.path().join("config.toml");
    fs::write(
        &cfg,
        format!(
            r#"# top comment
listen = "127.0.0.1:9"
repo_path = "{root}"
system_prompt_file = "{root}/p.md"
state_db = "{root}/state.db"
[telegram]
enabled = false # keep me
bot_token_env = "ATR_TELEGRAM_TOKEN"
[[provider]]
name = "claude"
kind = "claude"
cli = "/bin/true"
"#,
            root = dir.path().display()
        ),
    )
    .unwrap();
    (dir, cfg)
}

fn atr(dir: &tempfile::TempDir, cfg: &std::path::Path, args: &[&str]) -> (bool, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_atr"))
        .args(args)
        .env("ATR_CONFIG", cfg)
        .env("HOME", dir.path().join("home"))
        .output()
        .unwrap();
    (
        out.status.success(),
        format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ),
    )
}

#[test]
fn config_get_set_keeps_comments_and_validates() {
    let (dir, cfg) = setup();
    let (ok, out) = atr(&dir, &cfg, &["config", "get", "telegram.enabled"]);
    assert!(ok, "{out}");
    assert!(out.contains("telegram.enabled = false"), "{out}");
    let (ok, out) = atr(&dir, &cfg, &["config", "set", "telegram.enabled", "true"]);
    assert!(ok, "{out}");
    assert!(
        out.contains("set telegram.enabled false") && out.contains("→ true"),
        "{out}"
    );
    let text = fs::read_to_string(&cfg).unwrap();
    assert!(text.contains("# top comment"), "{text}");
    assert!(text.contains("enabled = true"), "{text}");
    let (ok, out) = atr(&dir, &cfg, &["config", "get", "no_such_key_xyz"]);
    assert!(!ok, "{out}");
    let (ok, out) = atr(&dir, &cfg, &["config", "set", "telegram.enabled", "lots"]);
    assert!(!ok, "invalid value must be refused: {out}");
    assert!(fs::read_to_string(&cfg).unwrap().contains("enabled = true"));
}

#[test]
fn pause_resume_and_retry_without_service() {
    let (dir, cfg) = setup();
    let (ok, out) = atr(&dir, &cfg, &["pause"]);
    assert!(ok, "{out}");
    assert!(out.contains("intake paused"), "{out}");
    assert!(out.contains("not reachable"), "{out}");
    let (ok, out) = atr(&dir, &cfg, &["status", "--json"]);
    assert!(ok, "{out}");
    assert!(
        out.contains("\"intake_paused\": true") || out.contains("\"intake_paused\":true"),
        "{out}"
    );
    let (ok, out) = atr(&dir, &cfg, &["resume"]);
    assert!(ok, "{out}");
    assert!(out.contains("intake resumed"), "{out}");
    let (ok, out) = atr(&dir, &cfg, &["retry", "nope-1"]);
    assert!(!ok, "{out}");
    assert!(out.contains("unknown task"), "{out}");
}

#[test]
fn lessons_round_trip_without_service() {
    let (dir, cfg) = setup();
    let root = dir.path().display().to_string();
    let (ok, out) = atr(&dir, &cfg, &["lessons", "list"]);
    assert!(ok, "{out}");
    assert!(out.contains("no lessons"), "{out}");
    let (ok, out) = atr(
        &dir,
        &cfg,
        &[
            "lessons",
            "add",
            "--repo",
            &root,
            "Run npm ci before the tests.",
        ],
    );
    assert!(ok, "{out}");
    assert!(out.contains("added lesson #1"), "{out}");
    assert!(out.contains("lessons are off"), "{out}");
    let value = format!("{}{}", "abcdefgh", "12345678");
    let secret = format!("export MY_TOKEN={value}");
    let (ok, out) = atr(&dir, &cfg, &["lessons", "add", "--repo", &root, &secret]);
    assert!(!ok, "secret must be refused: {out}");
    assert!(!out.contains(&value), "{out}");
    let (ok, out) = atr(&dir, &cfg, &["lessons", "add", "--repo", "nope", "x"]);
    assert!(!ok, "{out}");
    let (ok, out) = atr(&dir, &cfg, &["lessons", "pin", "1"]);
    assert!(ok && out.contains("pinned lesson #1"), "{out}");
    let (ok, out) = atr(&dir, &cfg, &["lessons", "pin", "99"]);
    assert!(!ok, "{out}");
    let (ok, out) = atr(&dir, &cfg, &["lessons", "list", "--json"]);
    assert!(ok, "{out}");
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v[0]["text"], "Run npm ci before the tests.");
    assert_eq!(v[0]["signal"], "manual");
    assert_eq!(v[0]["pinned"], true);
    let (ok, out) = atr(&dir, &cfg, &["lessons", "export", "--repo", &root]);
    assert!(ok, "{out}");
    assert!(
        out.contains("- Run npm ci before the tests. (pinned)"),
        "{out}"
    );
    let (ok, out) = atr(&dir, &cfg, &["lessons", "unpin", "1"]);
    assert!(ok && out.contains("unpinned"), "{out}");
    let (ok, out) = atr(&dir, &cfg, &["lessons", "clear", "--repo", &root]);
    assert!(ok && out.contains("removed 1 lesson(s)"), "{out}");
    let (ok, out) = atr(&dir, &cfg, &["lessons", "list"]);
    assert!(ok && out.contains("no lessons"), "{out}");
}
