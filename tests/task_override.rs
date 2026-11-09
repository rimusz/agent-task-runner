//! Per-task provider + model override: dispatch (`runs.model`, CLI argv,
//! failover) and `atr add` validation.
#[allow(dead_code)]
mod common;

use common::Harness;
use std::fs;
use std::process::Command;

const CONFIG_MODEL: &str = "claude-sonnet-5-5";

async fn harness(grok: &str) -> Harness {
    let h = Harness::with_providers(
        &[
            ("claude", true, "success"),
            ("grok", true, grok),
            ("codex", true, "success"),
        ],
        "127.0.0.1:0",
    )
    .await;
    h.shared
        .config
        .write()
        .unwrap()
        .providers
        .iter_mut()
        .find(|p| p.name == "claude")
        .unwrap()
        .model = CONFIG_MODEL.into();
    h
}

fn submit(h: &Harness, key: &str, provider: Option<&str>, model: Option<&str>) {
    h.shared
        .submit_with_model(
            key,
            "do it",
            "cli",
            None,
            None,
            false,
            false,
            atr::SubmitOpts {
                workspace_kind: Some("none".into()),
                output_kind: Some("text".into()),
                allow_empty_repo: true,
                provider_hint: provider.map(str::to_string),
                ..Default::default()
            },
            model,
        )
        .unwrap();
}

fn runs(h: &Harness, key: &str) -> Vec<(String, String)> {
    h.shared
        .db
        .list_runs(Some(key))
        .unwrap()
        .into_iter()
        .map(|r| (r.provider, r.model.unwrap_or_default()))
        .collect()
}

fn invoked_with(h: &Harness, model: &str) -> usize {
    h.invocations()
        .iter()
        .filter(|i| i.to_string().contains(model))
        .count()
}

#[tokio::test]
async fn override_reaches_the_named_provider_and_runs_model() {
    let h = harness("success").await;
    submit(&h, "demo-ov1", Some("codex"), Some("gpt-5.6-sol"));
    h.wait_status("demo-ov1", &["waiting", "done"], 15000).await;
    assert_eq!(
        runs(&h, "demo-ov1"),
        vec![("codex".to_string(), "gpt-5.6-sol".to_string())]
    );
    assert_eq!(invoked_with(&h, "gpt-5.6-sol"), 1, "{:?}", h.invocations());

    let text = atr::app::task_verbose_text(&h.shared, "demo-ov1").unwrap();
    assert!(text.contains("model\tgpt-5.6-sol\n"), "{text}");
    assert!(text.contains("\tgpt-5.6-sol\n"), "{text}");
    assert!(text.lines().any(|l| l.starts_with("run_model\t")), "{text}");
    assert_eq!(
        h.shared.status_json()["tasks"][0]["model"],
        "gpt-5.6-sol",
        "status JSON shows the override"
    );
    assert!(h.shared.status_json().get("groups").is_some());
}

#[tokio::test]
async fn task_without_override_keeps_the_provider_model() {
    let h = harness("success").await;
    submit(&h, "demo-ov2", Some("claude"), None);
    h.wait_status("demo-ov2", &["waiting", "done"], 15000).await;
    assert_eq!(
        runs(&h, "demo-ov2"),
        vec![("claude".to_string(), CONFIG_MODEL.to_string())]
    );
    let text = atr::app::task_verbose_text(&h.shared, "demo-ov2").unwrap();
    assert!(!text.contains("model\t"), "{text}");
    assert!(h.shared.status_json()["tasks"][0].get("model").is_none());
}

fn pin_reason(reason: &str) -> String {
    format!("model override grok-override-9 pins provider grok: {reason}")
}

fn set_provider(h: &Harness, name: &str, change: impl Fn(&mut atr::config::Provider)) {
    change(
        h.shared
            .config
            .write()
            .unwrap()
            .providers
            .iter_mut()
            .find(|p| p.name == name)
            .unwrap(),
    );
}

fn run_providers(h: &Harness, key: &str) -> Vec<String> {
    runs(h, key).into_iter().map(|(p, _)| p).collect()
}

fn no_compare_side_effects(h: &Harness, key: &str) {
    for r in h.shared.db.list_runs(Some(key)).unwrap() {
        assert_ne!(r.outcome.as_deref(), Some("arm_unavailable"), "{key}");
        let events = h.shared.db.events_for_run(r.id).unwrap();
        assert!(events.iter().all(|e| e.kind != "arm_unavailable"), "{key}");
    }
}

#[tokio::test]
async fn pin_fails_with_a_clear_reason_when_the_provider_cannot_run() {
    let h = harness("success").await;
    let missing = h.dir.path().join("no-such-cli");
    let cases: [(&str, &str); 2] = [
        ("demo-ov4", "provider grok is disabled"),
        ("demo-ov5", "provider grok CLI is missing"),
    ];
    for (key, reason) in cases {
        let original = h.shared.config.read().unwrap().clone();
        if key == "demo-ov4" {
            set_provider(&h, "grok", |p| p.enabled = false);
        } else {
            set_provider(&h, "grok", |p| p.cli = missing.clone());
        }
        submit(&h, key, Some("grok"), Some("grok-override-9"));
        h.wait_status(key, &["failed"], 15000).await;
        let t = h.shared.db.get_task(key).unwrap().unwrap();
        let want = pin_reason(reason);
        assert_eq!(t.result_text, want);
        let text = atr::app::task_verbose_text(&h.shared, key).unwrap();
        assert!(text.contains(&want), "{text}");
        // Only the marker run: nothing was spawned, least of all elsewhere.
        let rows = h.shared.db.list_runs(Some(key)).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].outcome.as_deref(), Some("model_pin_unavailable"));
        no_compare_side_effects(&h, key);
        *h.shared.config.write().unwrap() = original;
    }
    assert!(h.invocations().is_empty(), "{:?}", h.invocations());
}

#[tokio::test]
async fn pin_fails_when_the_provider_is_gone_or_lacks_a_capability() {
    let h = harness("success").await;
    h.shared
        .submit_with_model(
            "demo-ov6",
            "do it",
            "cli",
            None,
            None,
            false,
            false,
            atr::SubmitOpts {
                workspace_kind: Some("none".into()),
                output_kind: Some("text".into()),
                allow_empty_repo: true,
                provider_hint: Some("grok".into()),
                needs: vec!["browser".into()],
                ..Default::default()
            },
            Some("grok-override-9"),
        )
        .unwrap();
    h.wait_status("demo-ov6", &["failed"], 15000).await;
    let t = h.shared.db.get_task("demo-ov6").unwrap().unwrap();
    assert_eq!(t.result_text, pin_reason("provider grok lacks browser"));

    h.shared
        .config
        .write()
        .unwrap()
        .providers
        .retain(|p| p.name != "grok");
    submit(&h, "demo-ov7", Some("grok"), Some("grok-override-9"));
    h.wait_status("demo-ov7", &["failed"], 15000).await;
    let t = h.shared.db.get_task("demo-ov7").unwrap().unwrap();
    assert_eq!(t.result_text, pin_reason("provider grok is not configured"));
    assert!(h.invocations().is_empty(), "{:?}", h.invocations());
}

#[tokio::test]
async fn pin_failure_on_the_provider_does_not_fail_over() {
    let h = harness("auth").await;
    submit(&h, "demo-ov8", Some("grok"), Some("grok-override-9"));
    h.wait_status("demo-ov8", &["failed"], 15000).await;
    assert_eq!(run_providers(&h, "demo-ov8"), vec!["grok".to_string()]);
    assert!(
        h.invocations().iter().all(|i| i["kind"] == "grok"),
        "{:?}",
        h.invocations()
    );
    let text = atr::app::task_verbose_text(&h.shared, "demo-ov8").unwrap();
    assert!(
        text.contains("model override grok-override-9 pins provider grok: grok ended with auth"),
        "{text}"
    );
    no_compare_side_effects(&h, "demo-ov8");
}

#[tokio::test]
async fn pin_transient_failure_retries_on_the_same_provider_only() {
    let h = harness("rate_limit").await;
    submit(&h, "demo-ov9", Some("grok"), Some("grok-override-9"));
    h.wait_until("demo-ov9", 15000, |h| {
        h.shared
            .db
            .get_task("demo-ov9")
            .unwrap()
            .is_some_and(|t| t.retry_attempt >= 1)
    })
    .await;
    let t = h.shared.db.get_task("demo-ov9").unwrap().unwrap();
    assert_eq!(t.status, "queued", "waits for its own provider");
    assert_eq!(run_providers(&h, "demo-ov9"), vec!["grok".to_string()]);
    assert_eq!(t.tried, None);
}

#[tokio::test]
async fn pin_with_retries_off_fails_instead_of_moving() {
    let h = harness("rate_limit").await;
    h.shared.config.write().unwrap().retry.enabled = false;
    submit(&h, "demo-ov10", Some("grok"), Some("grok-override-9"));
    h.wait_status("demo-ov10", &["failed"], 15000).await;
    assert_eq!(run_providers(&h, "demo-ov10"), vec!["grok".to_string()]);
}

#[tokio::test]
async fn retry_after_a_pin_failure_runs_on_the_named_provider() {
    let h = harness("success").await;
    set_provider(&h, "grok", |p| p.enabled = false);
    submit(&h, "demo-ov11", Some("grok"), Some("grok-override-9"));
    h.wait_status("demo-ov11", &["failed"], 15000).await;
    set_provider(&h, "grok", |p| p.enabled = true);
    h.shared.retry("demo-ov11").unwrap();
    h.wait_status("demo-ov11", &["waiting", "done"], 15000)
        .await;
    assert_eq!(
        runs(&h, "demo-ov11").last().unwrap(),
        &("grok".to_string(), "grok-override-9".to_string())
    );
    assert_eq!(invoked_with(&h, "grok-override-9"), 1);
}

#[tokio::test]
async fn provider_without_a_model_still_fails_over() {
    let h = harness("rate_limit").await;
    submit(&h, "demo-ov12", Some("grok"), None);
    h.wait_status("demo-ov12", &["waiting", "done"], 15000)
        .await;
    assert_eq!(
        run_providers(&h, "demo-ov12"),
        vec!["grok".to_string(), "claude".to_string()]
    );
}

#[tokio::test]
async fn provider_without_a_model_falls_back_when_unusable() {
    let h = harness("success").await;
    set_provider(&h, "grok", |p| p.enabled = false);
    submit(&h, "demo-ov13", Some("grok"), None);
    h.wait_status("demo-ov13", &["waiting", "done"], 15000)
        .await;
    assert_eq!(run_providers(&h, "demo-ov13"), vec!["claude".to_string()]);
}

fn cli_setup() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join("home")).unwrap();
    fs::write(dir.path().join("p.md"), "p\n").unwrap();
    let cfg = dir.path().join("config.toml");
    fs::write(
        &cfg,
        format!(
            r#"
listen = "127.0.0.1:9"
repo_path = "{root}"
system_prompt_file = "{root}/p.md"
state_db = "{root}/state.db"
[[provider]]
name = "claude"
kind = "claude"
cli = "/bin/true"
capabilities = ["browser"]
[[provider]]
name = "codex"
kind = "codex"
cli = "/bin/true"
"#,
            root = dir.path().display()
        ),
    )
    .unwrap();
    (dir, cfg)
}

fn atr_add(dir: &tempfile::TempDir, cfg: &std::path::Path, args: &[&str]) -> (bool, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_atr"))
        .arg("add")
        .args(args)
        .arg("do the thing")
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
fn cli_model_requires_provider() {
    let (dir, cfg) = cli_setup();
    let (ok, out) = atr_add(&dir, &cfg, &["--model", "gpt-5.6-sol"]);
    assert!(!ok, "{out}");
    assert!(out.contains("--provider"), "{out}");
}

#[test]
fn cli_rejects_invalid_overrides_before_creating_a_task() {
    let (dir, cfg) = cli_setup();
    let cases: &[(&[&str], &str)] = &[
        (
            &["--provider", "nope"],
            "unknown provider nope; configured: claude, codex",
        ),
        (
            &["--provider", "claude", "--model", "opus"],
            "model opus is not valid for provider claude",
        ),
        (
            &["--provider", "codex", "--needs", "browser"],
            "provider codex lacks capability browser; providers with browser: claude",
        ),
    ];
    for (args, want) in cases {
        let (ok, out) = atr_add(&dir, &cfg, args);
        assert!(!ok, "{args:?}: {out}");
        assert!(out.contains(want), "{args:?}: {out}");
    }
    let db = atr::db::Db::open(&dir.path().join("state.db")).unwrap();
    assert!(db.list_tasks().unwrap().is_empty());
}

#[test]
fn cli_stores_a_valid_override() {
    let (dir, cfg) = cli_setup();
    let (ok, out) = atr_add(
        &dir,
        &cfg,
        &[
            "--key",
            "ov-cli",
            "--provider",
            "codex",
            "--model",
            "gpt-5.6-sol",
        ],
    );
    assert!(ok, "{out}");
    let db = atr::db::Db::open(&dir.path().join("state.db")).unwrap();
    let t = db.get_task("ov-cli").unwrap().unwrap();
    assert_eq!(t.provider_hint, "codex");
    assert_eq!(t.model_override, "gpt-5.6-sol");
}

#[test]
fn needs_doc_codex_playwright_example_parses() {
    let doc = fs::read_to_string(format!("{}/docs/NEEDS.md", env!("CARGO_MANIFEST_DIR"))).unwrap();
    let snippet = doc
        .split("```toml\n")
        .filter_map(|b| b.split("```").next())
        .find(|b| b.contains("playwright"))
        .expect("codex Playwright example");
    let parsed: toml::Value = toml::from_str(snippet).unwrap();
    let p = &parsed["provider"][0];
    assert_eq!(p["kind"].as_str(), Some("codex"));
    assert_eq!(p["capabilities"][0].as_str(), Some("browser"));
    assert_eq!(p["config_overrides"].as_array().unwrap().len(), 2);
}
