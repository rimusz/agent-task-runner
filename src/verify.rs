use crate::config::Config;
use crate::db::{now_rfc3339, Db, Task, VerifyCheck};
use anyhow::Result;
use serde_json::Value;
use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};
use tokio::process::Command;
use tokio::time::timeout;

#[derive(Debug, Clone)]
pub struct VerifyRun {
    pub skipped: bool,
    pub passed: bool,
    pub checks: Vec<VerifyCheck>,
    pub tests_modified: Vec<String>,
    pub reason: Option<String>,
}

impl Default for VerifyRun {
    fn default() -> Self {
        Self {
            skipped: true,
            passed: true,
            checks: Vec::new(),
            tests_modified: Vec::new(),
            reason: None,
        }
    }
}

impl VerifyRun {
    pub fn summary(&self) -> String {
        if self.skipped && self.reason.is_none() {
            return String::new();
        }
        let mut out = format_checks(&self.checks);
        if self.reason.as_deref() == Some("tests_modified") && !self.tests_modified.is_empty() {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str("tests_modified: ");
            out.push_str(&self.tests_modified.join(", "));
        }
        out
    }

    pub fn failure_followup(&self, max_bytes: usize) -> String {
        let mut out = String::from(
            "The configured verify checks failed. Fix the failures in this worktree. \
Do not claim the task is done until these pass.\n",
        );
        for c in &self.checks {
            if c.passed {
                continue;
            }
            out.push_str(&format!("\n### `{}`\nexit {}\n", c.command, c.exit_code));
            let trimmed = trim_output(&c.output, max_bytes);
            if !trimmed.is_empty() {
                out.push_str("```\n");
                out.push_str(&trimmed);
                if !trimmed.ends_with('\n') {
                    out.push('\n');
                }
                out.push_str("```\n");
            }
        }
        out
    }
}

pub fn commands_for(cfg: &Config, repo: &Path, worktree: &Path) -> Vec<String> {
    for r in &cfg.verify.repos {
        if path_matches(&r.path, repo) || path_matches(&r.path, worktree) {
            if !r.enabled {
                return Vec::new();
            }
            if !r.commands.is_empty() {
                return r.commands.clone();
            }
            return detect_commands(worktree);
        }
    }
    if !cfg.verify.commands.is_empty() {
        return cfg.verify.commands.clone();
    }
    if cfg.verify.only_listed_repos {
        return Vec::new();
    }
    detect_commands(worktree)
}

/// Optional detection only: scripts that exist in package.json, or Cargo.toml.
/// Nothing is run unless those files actually declare it.
pub fn detect_commands(worktree: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let pkg = worktree.join("package.json");
    if let Ok(text) = std::fs::read_to_string(&pkg) {
        if let Ok(v) = serde_json::from_str::<Value>(&text) {
            if let Some(scripts) = v.get("scripts").and_then(|s| s.as_object()) {
                for name in ["test", "lint", "typecheck"] {
                    if scripts.contains_key(name) {
                        if name == "test" {
                            out.push("npm test".into());
                        } else {
                            out.push(format!("npm run {name}"));
                        }
                    }
                }
            }
        }
    }
    if worktree.join("Cargo.toml").is_file() {
        out.push("cargo test".into());
    }
    out
}

fn path_matches(configured: &Path, actual: &Path) -> bool {
    if configured.as_os_str().is_empty() {
        return false;
    }
    let c = configured
        .canonicalize()
        .unwrap_or_else(|_| configured.to_path_buf());
    let a = actual
        .canonicalize()
        .unwrap_or_else(|_| actual.to_path_buf());
    if c == a {
        return true;
    }
    let cs = configured.to_string_lossy();
    let as_ = actual.to_string_lossy();
    if as_ == cs {
        return true;
    }
    as_.ends_with(cs.as_ref()) || cs.ends_with(as_.as_ref())
}

/// The path `[[verify.repo]]` entries are matched against for this task.
pub fn workspace_repo<'a>(task: &Task, repo: &'a Path, worktree: &'a Path) -> &'a Path {
    if task.workspace_kind == "dir" {
        worktree
    } else {
        repo
    }
}

pub async fn run(
    cfg: &Config,
    db: &Db,
    key: &str,
    repo: &Path,
    worktree: &Path,
    run_id: i64,
    task: &Task,
) -> Result<VerifyRun> {
    if !cfg.verify.enabled || cfg.dry_run {
        return Ok(VerifyRun::default());
    }
    // A task with no workspace has nothing to verify: its scratch folder is
    // not the configured repo, so repo checks (e.g. `./verify.sh`) would fail.
    if task.workspace_kind == "none" {
        return Ok(VerifyRun::default());
    }
    // A plain-folder task only matches `[[verify.repo]]` by its own folder,
    // never by the fallback repo it was filed under.
    let repo = workspace_repo(task, repo, worktree);
    let cmds = commands_for(cfg, repo, worktree);
    if cmds.is_empty() {
        return Ok(VerifyRun::default());
    }
    let globs = test_globs_for(cfg, repo, worktree);
    let skip = skip_protect(&cfg.verify, task);
    let changed = if skip {
        Vec::new()
    } else {
        changed_protected_tests(worktree, &cfg.repo_default_branch, globs)
    };
    let (passed, checks, tests_modified, reason) = if changed.is_empty() {
        let (p, c) = run_cmds(
            &cmds,
            worktree,
            cfg.verify.timeout,
            cfg.verify.max_output_bytes,
        )
        .await;
        (p, c, Vec::new(), None)
    } else {
        let guard = TestRestore::apply(worktree, &cfg.repo_default_branch, &changed);
        let (base_ok, base_checks) = run_cmds(
            &cmds,
            worktree,
            cfg.verify.timeout,
            cfg.verify.max_output_bytes,
        )
        .await;
        drop(guard);
        if base_ok {
            (true, base_checks, Vec::new(), None)
        } else {
            let (agent_ok, mut c) = run_cmds(
                &cmds,
                worktree,
                cfg.verify.timeout,
                cfg.verify.max_output_bytes,
            )
            .await;
            if agent_ok {
                c.insert(
                    0,
                    VerifyCheck {
                        command: "(protect_tests)".into(),
                        passed: false,
                        exit_code: 2,
                        duration_ms: 0,
                        output: format!(
                            "reason=tests_modified\nfiles:\n{}\nChecks passed only with the agent's test edits.",
                            changed
                                .iter()
                                .map(|p| format!("- {p}"))
                                .collect::<Vec<_>>()
                                .join("\n")
                        ),
                        recorded: now_rfc3339(),
                    },
                );
                (false, c, changed, Some("tests_modified".into()))
            } else {
                (false, c, Vec::new(), None)
            }
        }
    };
    for check in &checks {
        db.insert_verify_check(run_id, key, check)?;
    }
    Ok(VerifyRun {
        skipped: false,
        passed,
        checks,
        tests_modified,
        reason,
    })
}

async fn run_cmds(
    cmds: &[String],
    cwd: &Path,
    limit: Duration,
    max_output: usize,
) -> (bool, Vec<VerifyCheck>) {
    let mut checks = Vec::with_capacity(cmds.len());
    let mut passed = true;
    for cmd in cmds {
        let check = run_one(cmd, cwd, limit, max_output).await;
        if !check.passed {
            passed = false;
        }
        checks.push(check);
    }
    (passed, checks)
}

pub fn skip_protect(cfg: &crate::config::VerifyConfig, task: &Task) -> bool {
    if !cfg.protect_tests {
        return true;
    }
    if task.protect_tests_skip {
        return true;
    }
    let want = cfg.protect_tests_skip_label.trim();
    if want.is_empty() {
        return false;
    }
    task.labels
        .split([',', '\n'])
        .any(|l| l.trim().eq_ignore_ascii_case(want))
}

pub fn test_globs_for<'a>(cfg: &'a Config, repo: &Path, worktree: &Path) -> &'a [String] {
    for r in &cfg.verify.repos {
        if path_matches(&r.path, repo) || path_matches(&r.path, worktree) {
            if !r.test_globs.is_empty() {
                return &r.test_globs;
            }
            break;
        }
    }
    &cfg.verify.test_globs
}

pub fn glob_match(pat: &str, path: &str) -> bool {
    let path = path.replace('\\', "/");
    let path = path.strip_prefix("./").unwrap_or(&path);
    let pat = pat.replace('\\', "/");
    glob_rec(&pat, path)
}

fn glob_rec(pat: &str, path: &str) -> bool {
    if pat == "**" {
        return true;
    }
    if let Some(rest) = pat.strip_prefix("**/") {
        if glob_rec(rest, path) {
            return true;
        }
        return match path.split_once('/') {
            Some((_, tail)) => glob_rec(pat, tail),
            None => false,
        };
    }
    if pat.is_empty() {
        return path.is_empty();
    }
    let (pseg, prest) = match pat.split_once('/') {
        Some((a, b)) => (a, Some(b)),
        None => (pat, None),
    };
    if path.is_empty() {
        return pseg == "**" && prest.map(|p| glob_rec(p, "")).unwrap_or(true);
    }
    let (seg, rest) = match path.split_once('/') {
        Some((a, b)) => (a, Some(b)),
        None => (path, None),
    };
    if pseg == "**" {
        if let Some(p) = prest {
            return glob_rec(&format!("**/{p}"), path) || glob_rec(p, path);
        }
        return true;
    }
    if !seg_match(pseg, seg) {
        return false;
    }
    match (prest, rest) {
        (Some(p), Some(r)) => glob_rec(p, r),
        (Some(p), None) => glob_rec(p, ""),
        (None, None) => true,
        (None, Some(_)) => false,
    }
}

fn seg_match(pat: &str, seg: &str) -> bool {
    wildcard(pat.as_bytes(), seg.as_bytes())
}

fn wildcard(pat: &[u8], text: &[u8]) -> bool {
    match (pat.first(), text.first()) {
        (None, None) => true,
        (Some(b'*'), _) => {
            wildcard(&pat[1..], text) || (!text.is_empty() && wildcard(pat, &text[1..]))
        }
        (Some(p), Some(t)) if p == t => wildcard(&pat[1..], &text[1..]),
        _ => false,
    }
}

pub fn changed_protected_tests(worktree: &Path, base: &str, globs: &[String]) -> Vec<String> {
    let out = match std::process::Command::new("git")
        .current_dir(worktree)
        .args(["diff", "--name-status", "--no-renames", base, "--"])
        .output()
    {
        Ok(o) if o.status.success() => o,
        _ => return Vec::new(),
    };
    let mut files = Vec::new();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let Some((status, path)) = line.split_once('\t') else {
            continue;
        };
        let status = status.trim();
        if status != "M" && status != "D" {
            continue;
        }
        let path = path.trim().replace('\\', "/");
        if globs.iter().any(|g| glob_match(g, &path)) {
            files.push(path);
        }
    }
    files.sort();
    files.dedup();
    files
}

struct TestRestore {
    worktree: std::path::PathBuf,
    saved: Vec<(String, Option<Vec<u8>>)>,
}

impl TestRestore {
    fn apply(worktree: &Path, base: &str, files: &[String]) -> Self {
        let mut saved = Vec::new();
        for rel in files {
            let p = worktree.join(rel);
            let bytes = std::fs::read(&p).ok();
            saved.push((rel.clone(), bytes));
            let _ = std::process::Command::new("git")
                .current_dir(worktree)
                .args(["checkout", base, "--", rel])
                .status();
        }
        Self {
            worktree: worktree.to_path_buf(),
            saved,
        }
    }

    fn restore(&self) {
        for (rel, bytes) in &self.saved {
            let p = self.worktree.join(rel);
            match bytes {
                Some(b) => {
                    if let Some(dir) = p.parent() {
                        let _ = std::fs::create_dir_all(dir);
                    }
                    let _ = std::fs::write(&p, b);
                }
                None => {
                    let _ = std::fs::remove_file(&p);
                }
            }
        }
    }
}

impl Drop for TestRestore {
    fn drop(&mut self) {
        self.restore();
    }
}

async fn run_one(command: &str, cwd: &Path, limit: Duration, max_output: usize) -> VerifyCheck {
    let started = Instant::now();
    let mut cmd = Command::new("sh");
    cmd.arg("-c")
        .arg(command)
        .current_dir(cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .stdin(Stdio::null());
    let child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            return VerifyCheck {
                command: command.into(),
                passed: false,
                exit_code: -1,
                duration_ms: started.elapsed().as_millis() as i64,
                output: format!("spawn failed: {e}"),
                recorded: now_rfc3339(),
            };
        }
    };
    match timeout(limit, child.wait_with_output()).await {
        Ok(Ok(out)) => {
            let mut text = String::new();
            text.push_str(&String::from_utf8_lossy(&out.stdout));
            if !out.stderr.is_empty() {
                if !text.is_empty() && !text.ends_with('\n') {
                    text.push('\n');
                }
                text.push_str(&String::from_utf8_lossy(&out.stderr));
            }
            let code = out.status.code().unwrap_or(-1);
            VerifyCheck {
                command: command.into(),
                passed: out.status.success(),
                exit_code: code,
                duration_ms: started.elapsed().as_millis() as i64,
                output: trim_output(&text, max_output),
                recorded: now_rfc3339(),
            }
        }
        Ok(Err(e)) => VerifyCheck {
            command: command.into(),
            passed: false,
            exit_code: -1,
            duration_ms: started.elapsed().as_millis() as i64,
            output: format!("wait failed: {e}"),
            recorded: now_rfc3339(),
        },
        Err(_) => VerifyCheck {
            command: command.into(),
            passed: false,
            exit_code: -1,
            duration_ms: started.elapsed().as_millis() as i64,
            output: format!(
                "timed out after {}",
                crate::duration::format_duration(limit)
            ),
            recorded: now_rfc3339(),
        },
    }
}

pub fn format_checks(checks: &[VerifyCheck]) -> String {
    if checks.is_empty() {
        return String::new();
    }
    let mut out = String::new();
    for c in checks {
        if !out.is_empty() {
            out.push('\n');
        }
        let mark = if c.passed { "pass" } else { "FAIL" };
        out.push_str(&format!("[{mark}] {} (exit {})", c.command, c.exit_code));
        if !c.passed && !c.output.trim().is_empty() {
            out.push('\n');
            out.push_str(c.output.trim());
        }
    }
    out
}

fn trim_output(s: &str, max: usize) -> String {
    let t = s.trim_end();
    if t.len() <= max {
        return t.to_string();
    }
    let keep = max.saturating_sub(14);
    let mut end = keep;
    while end > 0 && !t.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…[truncated]", &t[..end])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn detect_package_json_scripts_only_when_present() {
        let dir = tempfile::tempdir().unwrap();
        assert!(detect_commands(dir.path()).is_empty());
        std::fs::write(
            dir.path().join("package.json"),
            r#"{"scripts":{"lint":"eslint .","build":"tsc"}}"#,
        )
        .unwrap();
        assert_eq!(
            detect_commands(dir.path()),
            vec!["npm run lint".to_string()]
        );
    }

    #[test]
    fn detect_cargo_when_toml_exists() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "[package]\nname=\"x\"\n").unwrap();
        assert_eq!(detect_commands(dir.path()), vec!["cargo test".to_string()]);
    }

    #[test]
    fn per_repo_override_wins() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::write(repo.join("Cargo.toml"), "[package]\nname=\"x\"\n").unwrap();
        let cfg_path = dir.path().join("cfg.toml");
        let mut f = std::fs::File::create(&cfg_path).unwrap();
        write!(
            f,
            r#"
listen = "127.0.0.1:0"
state_db = "{db}"
worktree_root = "{root}"
repo_path = "{repo}"
system_prompt_file = "{root}/p.md"
[github]
poll_interval = "60s"
[verify]
enabled = true
commands = ["echo global"]
[[verify.repo]]
path = "{repo}"
commands = ["echo override"]
[[provider]]
name = "claude"
kind = "claude"
cli = "/bin/true"
"#,
            db = dir.path().join("s.db").display(),
            root = dir.path().display(),
            repo = repo.display(),
        )
        .unwrap();
        std::fs::write(dir.path().join("p.md"), "p\n").unwrap();
        let cfg = crate::config::load_config(&cfg_path).unwrap();
        assert_eq!(
            commands_for(&cfg, &repo, &repo),
            vec!["echo override".to_string()]
        );
    }

    #[test]
    fn only_listed_repos_skips_global_detect() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::write(repo.join("Cargo.toml"), "[package]\nname=\"x\"\n").unwrap();
        let cfg_path = dir.path().join("cfg.toml");
        let mut f = std::fs::File::create(&cfg_path).unwrap();
        write!(
            f,
            r#"
listen = "127.0.0.1:0"
state_db = "{db}"
worktree_root = "{root}"
repo_path = "{repo}"
system_prompt_file = "{root}/p.md"
[github]
poll_interval = "60s"
[verify]
enabled = true
only_listed_repos = true
[[provider]]
name = "claude"
kind = "claude"
cli = "/bin/true"
"#,
            db = dir.path().join("s.db").display(),
            root = dir.path().display(),
            repo = repo.display(),
        )
        .unwrap();
        std::fs::write(dir.path().join("p.md"), "p\n").unwrap();
        let cfg = crate::config::load_config(&cfg_path).unwrap();
        assert!(cfg.verify.only_listed_repos, "default-on only_listed_repos");
        assert!(
            commands_for(&cfg, &repo, &repo).is_empty(),
            "unlisted repo must not auto-detect"
        );
    }

    #[test]
    fn listed_repo_detects_when_enabled() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::write(repo.join("Cargo.toml"), "[package]\nname=\"x\"\n").unwrap();
        let cfg_path = dir.path().join("cfg.toml");
        let mut f = std::fs::File::create(&cfg_path).unwrap();
        write!(
            f,
            r#"
listen = "127.0.0.1:0"
state_db = "{db}"
worktree_root = "{root}"
repo_path = "{repo}"
system_prompt_file = "{root}/p.md"
[github]
poll_interval = "60s"
[verify]
enabled = true
only_listed_repos = true
[[verify.repo]]
path = "{repo}"
enabled = true
[[provider]]
name = "claude"
kind = "claude"
cli = "/bin/true"
"#,
            db = dir.path().join("s.db").display(),
            root = dir.path().display(),
            repo = repo.display(),
        )
        .unwrap();
        std::fs::write(dir.path().join("p.md"), "p\n").unwrap();
        let cfg = crate::config::load_config(&cfg_path).unwrap();
        assert_eq!(
            commands_for(&cfg, &repo, &repo),
            vec!["cargo test".to_string()]
        );
    }

    #[test]
    fn listed_repo_disabled_skips() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let cfg_path = dir.path().join("cfg.toml");
        let mut f = std::fs::File::create(&cfg_path).unwrap();
        write!(
            f,
            r#"
listen = "127.0.0.1:0"
state_db = "{db}"
worktree_root = "{root}"
repo_path = "{repo}"
system_prompt_file = "{root}/p.md"
[github]
poll_interval = "60s"
[verify]
enabled = true
[[verify.repo]]
path = "{repo}"
enabled = false
commands = ["echo should-not-run"]
[[provider]]
name = "claude"
kind = "claude"
cli = "/bin/true"
"#,
            db = dir.path().join("s.db").display(),
            root = dir.path().display(),
            repo = repo.display(),
        )
        .unwrap();
        std::fs::write(dir.path().join("p.md"), "p\n").unwrap();
        let cfg = crate::config::load_config(&cfg_path).unwrap();
        assert!(commands_for(&cfg, &repo, &repo).is_empty());
    }

    fn scratch_cfg(dir: &Path, repo: &Path) -> crate::config::Config {
        let cfg_path = dir.join("cfg.toml");
        std::fs::write(
            &cfg_path,
            format!(
                r#"
listen = "127.0.0.1:0"
state_db = "{db}"
worktree_root = "{root}"
repo_path = "{repo}"
system_prompt_file = "{root}/p.md"
[github]
poll_interval = "60s"
[verify]
enabled = true
[[verify.repo]]
path = "{repo}"
commands = ["./verify.sh"]
[[provider]]
name = "claude"
kind = "claude"
cli = "/bin/true"
"#,
                db = dir.join("s.db").display(),
                root = dir.display(),
                repo = repo.display(),
            ),
        )
        .unwrap();
        std::fs::write(dir.join("p.md"), "p\n").unwrap();
        crate::config::load_config(&cfg_path).unwrap()
    }

    fn ws_task(kind: &str) -> crate::db::Task {
        crate::db::Task {
            key: "scratch-x".into(),
            status: "running".into(),
            workspace_kind: kind.into(),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn no_workspace_task_skips_repo_verify() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("atr-scratch");
        let wt = dir.path().join("_none").join("scratch-x");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::create_dir_all(&wt).unwrap();
        let cfg = scratch_cfg(dir.path(), &repo);
        let db = Db::open(&dir.path().join("s.db")).unwrap();
        let v = run(&cfg, &db, "scratch-x", &repo, &wt, 1, &ws_task("none"))
            .await
            .unwrap();
        assert!(v.skipped && v.passed && v.checks.is_empty());
    }

    #[tokio::test]
    async fn dir_task_matches_verify_repo_by_its_own_folder_only() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("atr-scratch");
        let folder = dir.path().join("plain");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::create_dir_all(&folder).unwrap();
        let cfg = scratch_cfg(dir.path(), &repo);
        let db = Db::open(&dir.path().join("s.db")).unwrap();
        let v = run(&cfg, &db, "scratch-x", &repo, &folder, 1, &ws_task("dir"))
            .await
            .unwrap();
        assert!(v.skipped && v.checks.is_empty());
        // A git task in the scratch repo still gets its checks.
        assert_eq!(
            commands_for(
                &cfg,
                workspace_repo(&ws_task("git"), &repo, &folder),
                &folder
            ),
            vec!["./verify.sh".to_string()]
        );
    }

    fn dummy_task(skip: bool, labels: &str) -> crate::db::Task {
        crate::db::Task {
            key: "acme-1".into(),
            issue: Some(1),
            status: "running".into(),
            protect_tests_skip: skip,
            labels: labels.into(),
            ..Default::default()
        }
    }

    #[test]
    fn glob_match_defaults() {
        assert!(glob_match("**/*.test.*", "src/foo.test.ts"));
        assert!(glob_match("**/*.spec.*", "a/b.spec.js"));
        assert!(glob_match("**/__tests__/**", "src/__tests__/a.ts"));
        assert!(glob_match("tests/**", "tests/check.sh"));
        assert!(glob_match("**/*.test.*", "foo.test.rs"));
        assert!(!glob_match("**/*.test.*", "src/foo.ts"));
        assert!(!glob_match("tests/**", "src/tests.rs"));
    }

    #[test]
    fn skip_protect_flag_and_label() {
        let mut cfg = crate::config::VerifyConfig {
            enabled: true,
            timeout: Duration::from_secs(1),
            max_retries: 1,
            commands: Vec::new(),
            max_output_bytes: 256,
            only_listed_repos: true,
            protect_tests: true,
            protect_tests_skip_label: "atr:tests-ok".into(),
            test_globs: crate::config::default_test_globs(),
            repos: Vec::new(),
        };
        assert!(!skip_protect(&cfg, &dummy_task(false, "")));
        assert!(skip_protect(&cfg, &dummy_task(true, "")));
        assert!(skip_protect(
            &cfg,
            &dummy_task(false, "for:legacy,atr:tests-ok")
        ));
        assert!(!skip_protect(&cfg, &dummy_task(false, "for:legacy")));
        cfg.protect_tests = false;
        assert!(skip_protect(&cfg, &dummy_task(false, "")));
    }

    #[test]
    fn restore_keeps_agent_edits_and_new_tests() {
        let dir = tempfile::tempdir().unwrap();
        crate::worktree::init_sample_repo(dir.path()).unwrap();
        std::fs::create_dir_all(dir.path().join("tests")).unwrap();
        std::fs::write(dir.path().join("tests/check.sh"), "#!/bin/sh\nexit 1\n").unwrap();
        std::process::Command::new("git")
            .current_dir(dir.path())
            .args(["add", "tests/check.sh"])
            .status()
            .unwrap();
        std::process::Command::new("git")
            .current_dir(dir.path())
            .args(["commit", "-m", "tests"])
            .status()
            .unwrap();
        std::fs::write(
            dir.path().join("tests/check.sh"),
            "#!/bin/sh\n# AGENT\nexit 0\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("tests/new.spec.ts"), "new\n").unwrap();
        let globs = crate::config::default_test_globs();
        let changed = changed_protected_tests(dir.path(), "main", &globs);
        assert_eq!(changed, vec!["tests/check.sh".to_string()], "{changed:?}");
        {
            let guard = TestRestore::apply(dir.path(), "main", &changed);
            let now = std::fs::read_to_string(dir.path().join("tests/check.sh")).unwrap();
            assert!(now.contains("exit 1"), "{now}");
            assert!(!now.contains("AGENT"), "{now}");
            assert!(dir.path().join("tests/new.spec.ts").is_file());
            drop(guard);
        }
        let after = std::fs::read_to_string(dir.path().join("tests/check.sh")).unwrap();
        assert!(after.contains("AGENT"), "{after}");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("tests/new.spec.ts")).unwrap(),
            "new\n"
        );
    }
}
