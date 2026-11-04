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

#[tokio::test]
async fn failover_provider_does_not_get_the_override() {
    let h = harness("rate_limit").await;
    submit(&h, "demo-ov3", Some("grok"), Some("grok-override-9"));
    h.wait_status("demo-ov3", &["waiting", "done"], 15000).await;
    assert_eq!(
        runs(&h, "demo-ov3"),
        vec![
            ("grok".to_string(), "grok-override-9".to_string()),
            ("claude".to_string(), CONFIG_MODEL.to_string()),
        ]
    );
    assert_eq!(
        invoked_with(&h, "grok-override-9"),
        h.invocations()
            .iter()
            .filter(|i| i["kind"] == "grok" && i.to_string().contains("grok-override-9"))
            .count(),
        "only grok is spawned with the override: {:?}",
        h.invocations()
    );
    assert!(
        h.invocations()
            .iter()
            .any(|i| i.to_string().contains(CONFIG_MODEL)),
        "claude keeps its configured model: {:?}",
        h.invocations()
    );
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
