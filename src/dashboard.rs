use crate::app::Shared;
use crate::config::Config;
use crate::providers::hit_ratio_from_sums;
use axum::http::{header, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};

const PAGE: &str = include_str!("dashboard.html");
const LOG_LINES: usize = 50;

pub fn extra_bind(listen: &str, bind: &str) -> Option<String> {
    let bind = bind.trim();
    if bind.is_empty() || is_loopback_host(bind) {
        return None;
    }
    let (host, port) = split_host_port(listen)?;
    if host == bind {
        return None;
    }
    Some(format!("{bind}:{port}"))
}

fn is_loopback_host(host: &str) -> bool {
    matches!(host, "127.0.0.1" | "localhost" | "::1" | "[::1]")
}

fn split_host_port(listen: &str) -> Option<(&str, &str)> {
    let listen = listen.trim();
    if let Some(rest) = listen.strip_prefix('[') {
        let (h, p) = rest.split_once("]:")?;
        return Some((h, p));
    }
    listen.rsplit_once(':')
}

pub async fn page(shared: &Shared) -> Response {
    if !shared.config.read().unwrap().dashboard.enabled {
        return (StatusCode::NOT_FOUND, "dashboard disabled").into_response();
    }
    (
        StatusCode::OK,
        [(header::CACHE_CONTROL, "no-store")],
        Html(PAGE),
    )
        .into_response()
}

pub async fn json(shared: &Shared, query: Option<&str>) -> Response {
    if !shared.config.read().unwrap().dashboard.enabled {
        return (StatusCode::NOT_FOUND, Json(json!({"ok": false}))).into_response();
    }
    Json(snapshot_period(
        shared,
        &crate::usage::period_from_query(query),
        crate::usage::repo_from_query(query).as_deref(),
    ))
    .into_response()
}

pub fn snapshot(shared: &Shared) -> Value {
    snapshot_period(shared, &crate::usage::Period::Today, None)
}

pub fn snapshot_period(
    shared: &Shared,
    period: &crate::usage::Period,
    repo: Option<&str>,
) -> Value {
    let cfg = shared.config.read().unwrap().clone();
    let tz = &cfg.display_timezone;
    let inflight = shared.inflight.lock().unwrap().clone();
    let tasks = shared.db.list_tasks().unwrap_or_default();
    let runs = shared.db.list_runs(None).unwrap_or_default();
    let secrets = collect_secrets(&cfg);
    let now = crate::clock::Clock::system().now();

    let mut running = Vec::new();
    let mut queued = Vec::new();
    for t in &tasks {
        if let Some(f) = repo {
            if !cfg.task_matches_repo(t, f) {
                continue;
            }
        }
        let live = inflight.contains_key(&t.key) || t.status == "running";
        if live {
            running.push(task_json(shared, &cfg, t, "running"));
        } else if t.status == "queued" {
            queued.push(task_json(shared, &cfg, t, "queued"));
        }
    }

    // Hide a task's finished runs only while it has a run that is still open.
    // The scheduler keeps its in-flight slot until after the status flips to
    // waiting/queued, so `inflight` alone would briefly hide a just-finished run.
    let open: std::collections::HashSet<&str> = runs
        .iter()
        .filter(|r| r.ended.is_none())
        .map(|r| r.task_key.as_str())
        .collect();
    let mut finished = Vec::new();
    for r in &runs {
        if r.ended.is_none() {
            continue;
        }
        if open.contains(r.task_key.as_str()) {
            continue;
        }
        let task = tasks.iter().find(|t| t.key == r.task_key);
        if let Some(f) = repo {
            match task {
                Some(t) if cfg.task_matches_repo(t, f) => {}
                _ => continue,
            }
        }
        let result = r.outcome.as_deref().unwrap_or("-");
        let reason = if result == "success" {
            String::new()
        } else {
            result.to_string()
        };
        finished.push(task_json_run(&cfg, task, r, result, &reason));
        if finished.len() >= 20 {
            break;
        }
    }

    let providers: Vec<Value> = cfg
        .providers
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let enabled = shared
                .db
                .enabled_override(&p.name)
                .ok()
                .flatten()
                .unwrap_or(p.enabled);
            let (cooling, _) = shared
                .db
                .provider_blocked(&p.name)
                .unwrap_or((false, false));
            let usage = crate::reported::format_provider_usage(
                &crate::reported::provider_usage_view(&shared.db, &p.name),
                tz,
                now,
            );
            let refs: Vec<&str> = secrets.iter().map(String::as_str).collect();
            json!({
                "order": i + 1,
                "name": p.name,
                "enabled": enabled,
                "cooling": cooling,
                "usage": redact(&usage, &refs),
            })
        })
        .collect();

    let today_day = tz.today();
    let today: Vec<Value> = shared
        .db
        .usage_by_day_tz(1, tz)
        .unwrap_or_default()
        .into_iter()
        .filter(|row| row.day == today_day)
        .map(|row| {
            let kind = cfg
                .provider(&row.provider)
                .map(|p| p.kind.as_str())
                .unwrap_or(row.provider.as_str());
            let hit = hit_ratio_from_sums(kind, row.input, row.cache_read, row.cache_write)
                .map(|h| format!("{:.0}%", h * 100.0))
                .unwrap_or_else(|| "-".into());
            json!({
                "provider": row.provider,
                "day": row.day,
                "input": row.input,
                "output": row.output,
                "hit": hit,
            })
        })
        .collect();

    let savings = crate::usage::report_filtered(shared, period, repo)
        .map(|r| r.to_json())
        .unwrap_or_else(|_| json!({"period": period.label(), "by_provider": [], "by_day": []}));
    let repos: Vec<Value> = cfg
        .effective_repos()
        .iter()
        .map(|r| {
            json!({
                "name": r.name,
                "enabled": r.enabled,
                "github": r.github,
                "label": r.label,
                "path": r.path.display().to_string(),
                "shorthand": r.shorthand,
            })
        })
        .collect();

    json!({
        "ok": true,
        "now": tz.format(chrono::Utc::now()),
        "timezone": tz.label,
        "bind": cfg.dashboard.bind,
        "listen": cfg.listen,
        "intake_paused": shared.intake_paused.load(std::sync::atomic::Ordering::Relaxed),
        "driver": cfg.driver_name(),
        "repos": repos,
        "running": running,
        "queued": queued,
        "finished": finished,
        "providers": providers,
        "today": today,
        "savings": savings,
        "logs": tail_logs(&cfg, &secrets),
    })
}

fn task_json(shared: &Shared, cfg: &Config, t: &crate::db::Task, state: &str) -> Value {
    let last = shared
        .db
        .list_runs(Some(&t.key))
        .ok()
        .and_then(|rs| rs.into_iter().last());
    let model = last
        .as_ref()
        .and_then(|r| r.model.clone())
        .filter(|s| !s.is_empty())
        .or_else(|| {
            cfg.provider(t.provider.as_deref().unwrap_or(""))
                .map(|p| p.model.clone())
        })
        .unwrap_or_default();
    let started = last
        .as_ref()
        .map(|r| r.started.as_str())
        .unwrap_or(t.updated.as_str());
    json!({
        "key": t.key,
        "repo": short_repo(t.repo.as_deref()),
        "provider": t.provider.clone().unwrap_or_else(|| "-".into()),
        "model": if model.is_empty() { "-".into() } else { model },
        "elapsed": format_elapsed(started),
        "state": state,
        "result": Value::Null,
        "reason": Value::Null,
        "parent": t.parent_key,
        "role": t.role,
        "title": t.title,
    })
}

fn task_json_run(
    cfg: &Config,
    task: Option<&crate::db::Task>,
    r: &crate::db::RunRow,
    result: &str,
    reason: &str,
) -> Value {
    let model = r
        .model
        .as_deref()
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| {
            task.and_then(|t| t.provider.as_deref())
                .and_then(|n| cfg.provider(n))
                .map(|p| p.model.clone())
        })
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "-".into());
    let elapsed = match (&r.started, &r.ended) {
        (s, Some(e)) => format_span(s, e),
        (s, None) => format_elapsed(s),
    };
    json!({
        "key": r.task_key,
        "repo": short_repo(task.and_then(|t| t.repo.as_deref())),
        "provider": r.provider,
        "model": model,
        "elapsed": elapsed,
        "state": result,
        "result": result,
        "reason": reason,
        "parent": task.and_then(|t| t.parent_key.clone()),
        "role": task.map(|t| t.role.as_str()).unwrap_or("worker"),
        "title": task.map(|t| t.title.as_str()).unwrap_or(""),
    })
}

fn short_repo(repo: Option<&str>) -> String {
    let Some(r) = repo.filter(|s| !s.is_empty()) else {
        return "-".into();
    };
    std::path::Path::new(r)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(r)
        .to_string()
}

fn format_elapsed(started: &str) -> String {
    let Ok(ts) = chrono::DateTime::parse_from_rfc3339(started) else {
        return "-".into();
    };
    secs_human(
        (chrono::Utc::now() - ts.with_timezone(&chrono::Utc))
            .num_seconds()
            .max(0) as u64,
    )
}

fn format_span(start: &str, end: &str) -> String {
    let Ok(a) = chrono::DateTime::parse_from_rfc3339(start) else {
        return "-".into();
    };
    let Ok(b) = chrono::DateTime::parse_from_rfc3339(end) else {
        return format_elapsed(start);
    };
    secs_human((b - a).num_seconds().unsigned_abs())
}

fn secs_human(secs: u64) -> String {
    if secs < 60 {
        return format!("{secs}s");
    }
    let m = secs / 60;
    let s = secs % 60;
    if m < 60 {
        return if s == 0 {
            format!("{m}m")
        } else {
            format!("{m}m {s}s")
        };
    }
    let h = m / 60;
    let m = m % 60;
    if m == 0 {
        format!("{h}h")
    } else {
        format!("{h}h {m}m")
    }
}

fn tail_logs(cfg: &Config, secrets: &[String]) -> Vec<String> {
    if cfg.log.path.as_os_str().is_empty() || !cfg.log.path.exists() {
        return Vec::new();
    }
    let Ok(text) = std::fs::read_to_string(&cfg.log.path) else {
        return Vec::new();
    };
    let refs: Vec<&str> = secrets.iter().map(String::as_str).collect();
    text.lines()
        .rev()
        .take(LOG_LINES)
        .map(|l| redact(l, &refs))
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect()
}

fn collect_secrets(cfg: &Config) -> Vec<String> {
    let mut s = Vec::new();
    if let Some(t) = crate::telegram::bot_token(&cfg.telegram) {
        s.push(t);
    }
    for name in [
        cfg.telegram.bot_token_env.as_str(),
        "ATR_TELEGRAM_TOKEN",
        "GH_TOKEN",
        "GITHUB_TOKEN",
        "ATR_SHADOW_GH_TOKEN",
        "MOONSHOT_API_KEY",
        "DASHSCOPE_API_KEY",
        "MIMO_API_KEY",
    ] {
        if name.is_empty() {
            continue;
        }
        if let Ok(v) = std::env::var(name) {
            if !v.trim().is_empty() {
                s.push(v);
            }
        }
    }
    s
}

fn redact(text: &str, secrets: &[&str]) -> String {
    crate::telegram::sanitize(text, secrets)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::Shared;
    use std::sync::Arc;

    fn harness() -> (Arc<Shared>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let dbp = dir.path().join("s.db");
        let logp = dir.path().join("atr.log");
        std::fs::write(&logp, "hello tok-secret line\nkeep this\n").unwrap();
        std::fs::write(dir.path().join("p.md"), "p\n").unwrap();
        let cfg = format!(
            r#"
listen = "127.0.0.1:0"
state_db = "{db}"
worktree_root = "{root}"
repo_path = "{root}"
system_prompt_file = "{root}/p.md"
display_timezone = "UTC"
[github]
poll_interval = "60s"
[telegram]
enabled = true
bot_token_env = "ATR_TEST_DASH_TOKEN"
allowed_chat_ids = [1]
[dashboard]
enabled = true
bind = "127.0.0.1"
[log]
path = "{log}"
[[provider]]
name = "claude"
kind = "claude"
cli = "/bin/true"
"#,
            db = dbp.display(),
            root = dir.path().display(),
            log = logp.display(),
        );
        let path = dir.path().join("c.toml");
        std::fs::write(&path, &cfg).unwrap();
        let raw: crate::config::RawConfig = toml::from_str(&cfg).unwrap();
        let parsed = crate::config::Config::from_raw(raw).unwrap();
        (Shared::from_config(path, parsed).unwrap(), dir)
    }

    #[test]
    fn extra_bind_only_when_remote() {
        assert_eq!(extra_bind("127.0.0.1:7420", "127.0.0.1"), None);
        assert_eq!(extra_bind("127.0.0.1:7420", "localhost"), None);
        assert_eq!(
            extra_bind("127.0.0.1:7420", "<tailscale-ip>"),
            Some("<tailscale-ip>:7420".into())
        );
        assert_eq!(extra_bind("<tailscale-ip>:7420", "<tailscale-ip>"), None);
    }

    #[test]
    fn finished_run_visible_while_inflight_slot_is_still_held() {
        let (shared, _dir) = harness();
        shared.db.upsert_task("race-1", "/tmp/my-repo", None).unwrap();
        let run = shared.db.start_run("race-1", "claude", "opus").unwrap();
        shared.db.finish_run(run, "success", Some(0)).unwrap();
        shared.db.set_status("race-1", "waiting").unwrap();
        // Scheduler drops its in-flight slot only after the status flip.
        shared.inflight.lock().unwrap().insert(
            "race-1".into(),
            crate::app::InFlight {
                provider: "claude".into(),
                pid: 0,
                run_id: run,
                kill: Default::default(),
                shadow: false,
            },
        );
        let v = snapshot(&shared);
        let finished = v["finished"].as_array().unwrap();
        assert!(
            finished
                .iter()
                .any(|t| t["key"] == "race-1" && t["result"] == "success"),
            "{v}"
        );
    }

    #[test]
    fn snapshot_redacts_secrets_and_lists_tasks() {
        std::env::set_var("ATR_TEST_DASH_TOKEN", "tok-secret");
        let (shared, _dir) = harness();
        shared
            .db
            .upsert_task("acme-1", "/tmp/my-repo", None)
            .unwrap();
        shared.db.set_status("acme-1", "queued").unwrap();
        shared
            .db
            .upsert_task("done-1", "/tmp/my-repo", None)
            .unwrap();
        let run = shared.db.start_run("done-1", "claude", "opus").unwrap();
        shared.db.finish_run(run, "success", Some(0)).unwrap();

        let v = snapshot(&shared);
        assert_eq!(v["ok"], true);
        assert_eq!(v["bind"], "127.0.0.1");
        let queued = v["queued"].as_array().unwrap();
        assert!(queued.iter().any(|t| t["key"] == "acme-1"), "{v}");
        assert_eq!(queued[0]["repo"], "my-repo");
        assert!(v["repos"].as_array().is_some_and(|a| !a.is_empty()), "{v}");
        let finished = v["finished"].as_array().unwrap();
        assert!(
            finished
                .iter()
                .any(|t| t["key"] == "done-1" && t["result"] == "success"),
            "{v}"
        );
        let usage = v["providers"][0]["usage"].as_str().unwrap();
        assert!(usage.contains("usage:"), "{usage}");
        let logs = v["logs"].as_array().unwrap();
        let joined = logs
            .iter()
            .map(|x| x.as_str().unwrap_or(""))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(joined.contains("keep this"), "{joined}");
        assert!(!joined.contains("tok-secret"), "{joined}");
        assert!(!serde_json::to_string(&v).unwrap().contains("tok-secret"));
        assert!(v["savings"]["period"] == "today", "{v}");
        assert!(v["savings"]["by_provider"].is_array(), "{v}");
        assert!(v["savings"]["by_day"].is_array(), "{v}");
        assert!(v["savings"]["headline"].is_object(), "{v}");
        assert!(v["savings"]["first_run"].is_object(), "{v}");
        assert!(v["savings"]["resume"].is_object(), "{v}");
        assert!(v["savings"]["short"].is_object(), "{v}");
    }

    #[test]
    fn snapshot_savings_month_and_custom() {
        std::env::set_var("ATR_TEST_DASH_TOKEN", "tok-secret");
        let (shared, _dir) = harness();
        shared
            .db
            .upsert_task("acme-s", "/tmp/my-repo", None)
            .unwrap();
        let run = shared.db.start_run("acme-s", "claude", "opus").unwrap();
        shared.db.finish_run(run, "success", Some(0)).unwrap();
        shared
            .db
            .insert_usage_at(
                run,
                None,
                "claude",
                100,
                50,
                800,
                20,
                Some(2.05),
                Some("est"),
                "{}",
                "2026-10-02T12:00:00Z",
            )
            .unwrap();
        let month = snapshot_period(&shared, &crate::usage::Period::Month, None);
        let sav = &month["savings"];
        assert_eq!(sav["period"], "month");
        let prov = sav["by_provider"].as_array().unwrap();
        if sav["from"].as_str().unwrap() <= "2026-10-02"
            && sav["to"].as_str().unwrap() >= "2026-10-02"
        {
            let claude = prov.iter().find(|p| p["provider"] == "claude").unwrap();
            assert_eq!(claude["cached"], 800);
            assert_eq!(claude["tokens_saved"], 800);
            assert!(claude["saved_usd"].as_f64().unwrap() > 0.0, "{claude}");
            assert_eq!(claude["hit"], "87%");
        }
        let custom = snapshot_period(
            &shared,
            &crate::usage::Period::Custom {
                from: crate::usage::parse_ymd("2026-10-01").unwrap(),
                to: crate::usage::parse_ymd("2026-10-03").unwrap(),
            },
            None,
        );
        let sav = &custom["savings"];
        assert_eq!(sav["from"], "2026-10-01");
        assert_eq!(sav["to"], "2026-10-03");
        assert_eq!(sav["by_day"].as_array().unwrap().len(), 3);
        let claude = sav["by_provider"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["provider"] == "claude")
            .unwrap();
        assert_eq!(claude["tokens_saved"], 800);
        assert_eq!(sav["total"]["tokens_saved"], 800);
    }

    #[test]
    fn page_has_no_buttons() {
        assert!(!PAGE.contains("<button"));
        assert!(!PAGE.contains("<form"));
        assert!(PAGE.contains("dashboard.json"));
        assert!(PAGE.contains("5000"));
        assert!(PAGE.contains("Token savings"));
        assert!(PAGE.contains("id=\"period\""));
        assert!(PAGE.contains("<svg"));
    }

    #[tokio::test]
    async fn disabled_returns_404() {
        let (shared, _dir) = harness();
        shared.config.write().unwrap().dashboard.enabled = false;
        let resp = page(&shared).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        let resp = json(&shared, None).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn secs_human_formats() {
        assert_eq!(secs_human(12), "12s");
        assert_eq!(secs_human(75), "1m 15s");
        assert_eq!(secs_human(3600), "1h");
    }
}
