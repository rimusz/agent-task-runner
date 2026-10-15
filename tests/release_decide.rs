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

fn decide(dir: &Path, vars: &[(&str, &str)]) -> String {
    let script = format!("{}/contrib/release-decide.sh", env!("CARGO_MANIFEST_DIR"));
    let mut cmd = Command::new("sh");
    cmd.arg(&script).current_dir(dir);
    for k in ["TAG", "VERSION", "BUMP"] {
        cmd.env_remove(k);
    }
    for (k, v) in vars {
        cmd.env(k, v);
    }
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
    fs::create_dir_all(tmp.path().join("src")).unwrap();
    fs::write(tmp.path().join("src/app.rs"), "fn main() {}\n").unwrap();
    sh(tmp.path(), "git add src/app.rs && git commit -qm first");
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
fn first_release_is_v0_1_1() {
    let repo = init_repo();
    let out = decide(repo.path(), &[]);
    assert_eq!(field(&out, "skip"), "no");
    assert_eq!(field(&out, "tag"), "v0.1.1");
}

#[test]
fn already_tagged_commit_skips() {
    let repo = init_repo();
    sh(repo.path(), "git tag v0.1.1");
    let out = decide(repo.path(), &[]);
    assert_eq!(field(&out, "skip"), "yes");
    assert_eq!(field(&out, "reason"), "tagged");
}

#[test]
fn manual_run_bumps_patch_even_for_docs() {
    let repo = init_repo();
    sh(repo.path(), "git tag v0.1.1");
    fs::write(repo.path().join("README.md"), "docs\n").unwrap();
    sh(repo.path(), "git add README.md && git commit -qm docs");
    let out = decide(repo.path(), &[]);
    assert_eq!(field(&out, "skip"), "no");
    assert_eq!(field(&out, "tag"), "v0.1.2");
    assert_eq!(field(&out, "bump"), "patch");
    let out = decide(repo.path(), &[("BUMP", "minor")]);
    assert_eq!(field(&out, "tag"), "v0.2.0");
}

#[test]
fn explicit_version_and_tag_push() {
    let repo = init_repo();
    sh(repo.path(), "git tag v0.1.1");
    let out = decide(repo.path(), &[("VERSION", "v1.2.3")]);
    assert_eq!(field(&out, "skip"), "no");
    assert_eq!(field(&out, "ver"), "1.2.3");
    let out = decide(repo.path(), &[("VERSION", "0.1.1")]);
    assert_eq!(field(&out, "skip"), "yes");
    assert_eq!(field(&out, "reason"), "exists");
    let out = decide(repo.path(), &[("TAG", "v0.1.1")]);
    assert_eq!(field(&out, "skip"), "no");
    assert_eq!(field(&out, "reason"), "tag");
    assert_eq!(field(&out, "ver"), "0.1.1");
}
