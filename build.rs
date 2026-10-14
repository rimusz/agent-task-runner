fn main() {
    let sha = git_sha().unwrap_or_else(|| "unknown".into());
    let ver = release_version();
    println!("cargo:rustc-env=ATR_GIT_SHA={sha}");
    println!("cargo:rustc-env=ATR_VERSION_LONG={ver} ({sha})");
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-env-changed=GITHUB_SHA");
    println!("cargo:rerun-if-env-changed=ATR_RELEASE_VERSION");
}

/// Tag / ATR_RELEASE_VERSION wins so CI can stamp a release without editing Cargo.toml.
fn release_version() -> String {
    if let Ok(v) = std::env::var("ATR_RELEASE_VERSION") {
        let v = v.trim().trim_start_matches('v');
        if !v.is_empty() {
            return v.to_string();
        }
    }
    if let Some(v) = git_exact_tag() {
        return v;
    }
    env!("CARGO_PKG_VERSION").to_string()
}

fn git_exact_tag() -> Option<String> {
    let out = std::process::Command::new("git")
        .args(["describe", "--tags", "--exact-match", "--match", "v[0-9]*"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?;
    let s = s.trim().trim_start_matches('v');
    if s.is_empty() {
        None
    } else {
        Some(s.to_string())
    }
}

fn git_sha() -> Option<String> {
    let out = std::process::Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()?;
    if !out.status.success() {
        return env_sha();
    }
    let s = String::from_utf8(out.stdout).ok()?;
    let s = s.trim();
    if s.is_empty() {
        env_sha()
    } else {
        Some(s.to_string())
    }
}

fn env_sha() -> Option<String> {
    std::env::var("GITHUB_SHA")
        .ok()
        .map(|s| s.chars().take(7).collect())
        .filter(|s: &String| !s.is_empty())
}
