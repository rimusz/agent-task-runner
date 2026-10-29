use atr::{serve_background, Shared};
use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tempfile::TempDir;

/// Bearer token for the harness HTTP write routes.
#[allow(dead_code)]
pub const HTTP_TOKEN: &str = "e2e-http-write-token";

struct Watchdog(Arc<AtomicBool>);

impl Drop for Watchdog {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

pub struct Harness {
    #[allow(dead_code)]
    pub dir: TempDir,
    pub shared: Arc<Shared>,
    pub addr: SocketAddr,
    pub fake_dir: PathBuf,
    pub repo: PathBuf,
    _watchdog: Watchdog,
}

impl Harness {
    pub async fn with_providers(providers: &[(&str, bool, &str)], listen: &str) -> Self {
        let dir = TempDir::new().unwrap();
        let fake_dir = dir.path().join("fake");
        fs::create_dir_all(&fake_dir).unwrap();
        let bin = dir.path().join("bin");
        fs::create_dir_all(&bin).unwrap();
        let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake-agent.sh");
        for name in ["claude", "grok", "cursor-agent", "codex", "opencode"] {
            let dest = bin.join(name);
            fs::copy(&script, &dest).unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&dest, fs::Permissions::from_mode(0o755)).unwrap();
            }
        }
        let gh_script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake-gh.sh");
        let gh_dest = bin.join("gh");
        fs::copy(&gh_script, &gh_dest).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&gh_dest, fs::Permissions::from_mode(0o755)).unwrap();
        }
        for (name, _, beh) in providers {
            let cli = match *name {
                "cursor" => "cursor-agent",
                other => other,
            };
            fs::write(fake_dir.join(format!("{cli}.behavior")), beh).unwrap();
        }

        let repo = dir.path().join("repo");
        atr::worktree::init_sample_repo(&repo).unwrap();
        fs::write(repo.join("AGENTS.md"), "repo agents\n").unwrap();

        let prompt = dir.path().join("system-prompt.md");
        fs::write(&prompt, "STABLE PREFIX\n").unwrap();

        let db = dir.path().join("state.db");
        let wt = dir.path().join("work");
        let cfg_path = dir.path().join("config.toml");
        fs::write(
            &cfg_path,
            render_config(listen, &db, &wt, &repo, &prompt, &bin, providers),
        )
        .unwrap();

        atr::mcp::write_token_file(&db.with_file_name("http.token"), HTTP_TOKEN).unwrap();
        let cfg = atr::load_config(&cfg_path).unwrap();
        let shared = Shared::from_config(cfg_path, cfg).unwrap();
        let addr = serve_background(shared.clone()).await.unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        std::thread::spawn(move || {
            for _ in 0..90 {
                if flag.load(Ordering::Relaxed) {
                    return;
                }
                std::thread::sleep(std::time::Duration::from_secs(1));
            }
            if !flag.load(Ordering::Relaxed) {
                eprintln!("e2e harness watchdog: test exceeded 90s");
                std::process::abort();
            }
        });
        Harness {
            dir,
            shared,
            addr,
            fake_dir,
            repo,
            _watchdog: Watchdog(stop),
        }
    }

    pub fn submit(&self, key: &str, body: &str) -> atr::SubmitOutcome {
        self.shared
            .submit(
                key,
                body,
                "test",
                Some(&self.repo.display().to_string()),
                Some(1),
                false,
                false,
            )
            .unwrap()
    }

    pub fn submit_tests_ok(&self, key: &str, body: &str) -> atr::SubmitOutcome {
        self.shared
            .submit(
                key,
                body,
                "test",
                Some(&self.repo.display().to_string()),
                Some(1),
                false,
                true,
            )
            .unwrap()
    }

    pub fn submit_shadow(&self, key: &str, body: &str) -> atr::SubmitOutcome {
        self.shared
            .submit(
                key,
                body,
                "test",
                Some(&self.repo.display().to_string()),
                Some(1),
                true,
                false,
            )
            .unwrap()
    }

    pub fn install_gh_stub(&self) -> PathBuf {
        let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake-gh.sh");
        let dest = self.dir.path().join("bin").join("gh");
        fs::copy(&script, &dest).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&dest, fs::Permissions::from_mode(0o755)).unwrap();
        }
        dest
    }

    pub fn enable_shadow(&self, max_parallel: u32, gh: &Path) {
        fs::write(
            self.fake_dir.join("clone-src"),
            self.repo.display().to_string(),
        )
        .unwrap();
        let mut cfg = self.shared.config.write().unwrap();
        cfg.shadow.enabled = true;
        cfg.shadow.repo = "example/demo-1".into();
        cfg.shadow.labels = vec!["for:atr".into()];
        cfg.shadow.max_parallel = max_parallel.max(1);
        cfg.shadow.repo_path = self.dir.path().join("shadow-src");
        cfg.github.cli = gh.display().to_string();
    }

    pub fn invocations(&self) -> Vec<serde_json::Value> {
        let path = self.fake_dir.join("invocations.jsonl");
        let Ok(text) = fs::read_to_string(path) else {
            return vec![];
        };
        text.lines()
            .filter(|l| !l.is_empty())
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    pub async fn wait_status(&self, key: &str, want: &[&str], ms: u64) -> String {
        let start = std::time::Instant::now();
        loop {
            if let Ok(Some(t)) = self.shared.db.get_task(key) {
                if want.iter().any(|w| t.status == *w) {
                    return t.status;
                }
            }
            if start.elapsed() > std::time::Duration::from_millis(ms) {
                let st = self
                    .shared
                    .db
                    .get_task(key)
                    .ok()
                    .flatten()
                    .map(|t| t.status)
                    .unwrap_or_else(|| "missing".into());
                panic!("timeout waiting for {key} in {want:?}, last={st}");
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }

    pub async fn wait_until(&self, key: &str, ms: u64, pred: impl Fn(&Self) -> bool) {
        let start = std::time::Instant::now();
        loop {
            if pred(self) {
                return;
            }
            if start.elapsed() > std::time::Duration::from_millis(ms) {
                let st = self
                    .shared
                    .db
                    .get_task(key)
                    .ok()
                    .flatten()
                    .map(|t| t.status)
                    .unwrap_or_else(|| "missing".into());
                let runs = self.shared.db.list_runs(Some(key)).unwrap_or_default();
                let runs: Vec<String> = runs
                    .iter()
                    .map(|r| format!("{}:{:?}", r.provider, r.outcome))
                    .collect();
                let task = self.shared.db.get_task(key).ok().flatten();
                panic!(
                    "timeout waiting for {key} predicate, last={st}, runs={runs:?}, provider={:?}, tried={:?}, inflight={:?}",
                    task.as_ref().and_then(|t| t.provider.clone()),
                    task.as_ref().and_then(|t| t.tried.clone()),
                    self.shared.inflight.lock().unwrap().keys().collect::<Vec<_>>()
                );
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }
}

fn render_config(
    listen: &str,
    db: &Path,
    wt: &Path,
    repo: &Path,
    prompt: &Path,
    bin: &Path,
    providers: &[(&str, bool, &str)],
) -> String {
    let mut out = format!(
        r#"
listen = "{listen}"
state_db = "{db}"
worktree_root = "{wt}"
repo_path = "{repo}"
repo_default_branch = "main"
worktree_branch_prefix = "atr"
system_prompt_file = "{prompt}"
cache_hit_warn_threshold = 0.70
shell_command_timeout = "15m"
task_timeout = "4h"
kill_grace = "200ms"
dry_run = false
task_key_prefix = "demo"
display_timezone = "UTC"

[github]
enabled = false
cli = "{gh}"
repo = ""
label = "for:atr"
poll_interval = "60s"

[telegram]
enabled = false
bot_token_env = "ATR_TELEGRAM_TOKEN"
allowed_chat_ids = []
token_file = ""
chat_id_file = ""
prefix = "atr:"
notify_interval = "20s"
daily_summary = ""

[dashboard]
enabled = true
bind = "127.0.0.1"

[mcp]
enabled = true
token_env = "ATR_E2E_HTTP_TOKEN_UNSET"
token_file = "{token}"

[handoff]
max_bytes = 8192
include_diff = true

[host]
enabled = false

[shadow]
enabled = false
repo = ""
labels = []
poll_interval = "60s"
max_parallel = 1
key_prefix = ""
"#,
        listen = listen,
        db = db.display(),
        wt = wt.display(),
        repo = repo.display(),
        prompt = prompt.display(),
        gh = bin.join("gh").display(),
        token = db.with_file_name("http.token").display(),
    );
    for (name, enabled, _) in providers {
        out.push_str(&provider_block(name, *enabled, bin));
    }
    out
}

fn provider_block(name: &str, enabled: bool, bin: &Path) -> String {
    let (kind, cli_name, expected, extra) = match name {
        "claude" => (
            "claude",
            "claude",
            "2.1.287",
            r#"args = ["-p", "--output-format", "stream-json"]
resume_flag = "--resume"
model_flag = "--model"
prompt_mode = "stdin"
cwd_flag = ""
permission_mode = "bypassPermissions"
permission_mode_flag = "--permission-mode"
config_overrides = []
reported_usage = "claude_stream"
"#,
        ),
        "grok" => (
            "grok",
            "grok",
            "1.0.52",
            r#"args = ["--output-format", "streaming-json", "--always-approve"]
resume_flag = "--resume"
session_id_flag = "--session-id"
model_flag = "--model"
prompt_mode = "file"
prompt_file_prefix = "--prompt-file="
cwd_flag = "--cwd"
config_overrides = []
reported_usage = "grok_billing"
"#,
        ),
        "cursor" => (
            "cursor",
            "cursor-agent",
            "2026.10.01",
            r#"args = ["-p", "--output-format", "stream-json"]
resume_flag = "--resume"
model_flag = "--model"
prompt_mode = "stdin"
cwd_flag = ""
config_overrides = []
reported_usage = "none"
"#,
        ),
        "codex" => (
            "codex",
            "codex",
            "0.162.1",
            r#"args = []
resume_flag = ""
model_flag = "-m"
prompt_mode = "stdin"
cwd_flag = ""
sandbox = "workspace-write"
config_overrides = ["model_provider=\"openai\""]
"#,
        ),
        "opencode" => (
            "opencode",
            "opencode",
            "1.0.132",
            r#"args = []
resume_flag = "--session"
model_flag = "--model"
prompt_mode = "stdin"
cwd_flag = "--dir"
config_overrides = []
reported_usage = "none"
"#,
        ),
        _ => panic!("unknown {name}"),
    };
    format!(
        r#"
[[provider]]
name = "{name}"
kind = "{kind}"
enabled = {enabled}
cli = "{cli}"
expected_version = "{expected}"
version_args = ["--version"]
model = "test-model"
max_parallel = 1
cooldown_after_limit = "2s"
degraded_after_stall = "30s"
heartbeat_timeout = "2s"
run_timeout = "8s"
{extra}
[provider.health]
startup_timeout = "2s"
heartbeat_timeout = "2s"
stall_resumes = 1
unhealthy_cooldown = "20s"
"#,
        name = name,
        kind = kind,
        enabled = enabled,
        cli = bin.join(cli_name).display(),
        expected = expected,
        extra = extra,
    )
}
