#[test]
fn atr_version_prints_semver_and_git_sha() {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_atr"))
        .arg("--version")
        .output()
        .expect("run atr --version");
    assert!(
        out.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains('.') && text.split_whitespace().any(|w| w.split('.').count() == 3),
        "stdout must include a semver:\n{text}"
    );
    assert!(
        text.contains('(') && text.contains(')'),
        "stdout must include a short git sha in parentheses:\n{text}"
    );
}
