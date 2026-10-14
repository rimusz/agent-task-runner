use std::fs;
use std::path::Path;
use std::process::Command;

fn sh(dir: &Path, script: &str) {
    let st = Command::new("sh")
        .arg("-c")
        .arg(script)
        .current_dir(dir)
        .status()
        .unwrap();
    assert!(st.success(), "{script}");
}

fn decide(dir: &Path, before: Option<&str>, labels: &str) -> String {
    let script = format!("{}/contrib/release-decide.sh", env!("CARGO_MANIFEST_DIR"));
    let mut cmd = Command::new("sh");
    cmd.arg(&script).current_dir(dir);
    if let Some(b) = before {
        cmd.env("BEFORE", b);
    }
    cmd.env("LABELS", labels);
    let out = cmd.output().expect("release-decide");
    assert!(
        out.status.success(),
        "status={} stdout={} stderr={}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn field(out: &str, key: &str) -> String {
    out.lines()
        .filter_map(|l| l.strip_prefix(&format!("{key}=")))
        .next_back()
        .unwrap_or("")
        .to_string()
}

fn init_repo() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    sh(
        tmp.path(),
        "git init -q && git config user.email t@t && git config user.name t",
    );
    fs::write(tmp.path().join("app.rs"), "fn main() {}\n").unwrap();
    sh(tmp.path(), "git add app.rs && git commit -qm first");
    tmp
}

#[test]
fn next_version_script() {
    let script = format!("{}/contrib/next-version.sh", env!("CARGO_MANIFEST_DIR"));
    let run = |args: &[&str]| {
        let out = Command::new("sh").arg(&script).args(args).output().unwrap();
        assert!(out.status.success());
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    };
    assert_eq!(run(&[]), "0.1.1");
    assert_eq!(run(&["v0.1.1", "patch"]), "0.1.2");
    assert_eq!(run(&["v0.1.1", "minor"]), "0.2.0");
    assert_eq!(run(&["0.2.3", "major"]), "1.0.0");
}

#[test]
fn first_release_is_v0_1_1_even_for_docs() {
    let repo = init_repo();
    fs::write(repo.path().join("README.md"), "docs\n").unwrap();
    sh(repo.path(), "git add README.md && git commit -qm docs");
    let out = decide(repo.path(), None, "");
    assert_eq!(field(&out, "skip"), "no");
    assert_eq!(field(&out, "tag"), "v0.1.1");
}

#[test]
fn already_tagged_commit_skips() {
    let repo = init_repo();
    sh(repo.path(), "git tag v0.1.1");
    let out = decide(repo.path(), None, "");
    assert_eq!(field(&out, "skip"), "yes");
    assert_eq!(field(&out, "reason"), "tagged");
}

#[test]
fn later_push_is_patch() {
    let repo = init_repo();
    sh(repo.path(), "git tag v0.1.1");
    let before = String::from_utf8(
        Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(repo.path())
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();
    let before = before.trim().to_string();
    fs::write(repo.path().join("app.rs"), "fn main() { /* 2 */ }\n").unwrap();
    sh(repo.path(), "git add app.rs && git commit -qm code");
    let out = decide(repo.path(), Some(&before), "");
    assert_eq!(field(&out, "skip"), "no");
    assert_eq!(field(&out, "tag"), "v0.1.2");
    assert_eq!(field(&out, "bump"), "patch");
}

#[test]
fn docs_only_after_first_release_skips() {
    let repo = init_repo();
    sh(repo.path(), "git tag v0.1.1");
    let before = String::from_utf8(
        Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(repo.path())
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();
    let before = before.trim().to_string();
    fs::write(repo.path().join("README.md"), "only docs\n").unwrap();
    sh(repo.path(), "git add README.md && git commit -qm docs");
    let out = decide(repo.path(), Some(&before), "");
    assert_eq!(field(&out, "skip"), "yes");
    assert_eq!(field(&out, "reason"), "docs");
}

#[test]
fn minor_label_and_major_commit() {
    let repo = init_repo();
    sh(repo.path(), "git tag v0.1.1");
    let before = String::from_utf8(
        Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(repo.path())
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();
    let before = before.trim().to_string();
    fs::write(repo.path().join("app.rs"), "fn main() { /* 3 */ }\n").unwrap();
    sh(
        repo.path(),
        "git add app.rs && git commit -qm 'feat release:minor'",
    );
    let out = decide(repo.path(), Some(&before), "");
    assert_eq!(field(&out, "tag"), "v0.2.0");
    assert_eq!(field(&out, "bump"), "minor");

    fs::write(repo.path().join("app.rs"), "fn main() { /* 4 */ }\n").unwrap();
    sh(
        repo.path(),
        "git add app.rs && git commit -qm 'break release:major'",
    );
    let before2 = String::from_utf8(
        Command::new("git")
            .args(["rev-parse", "HEAD~1"])
            .current_dir(repo.path())
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();
    let out = decide(repo.path(), Some(before2.trim()), "release:minor");
    assert_eq!(field(&out, "tag"), "v1.0.0");
    assert_eq!(field(&out, "bump"), "major");
}
