use anyhow::{Context, Result};
use chrono::{Duration as ChronoDuration, Utc};
use rusqlite::{params, Connection, OptionalExtension};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;

/// Strip Grok's `USAGE_PERIOD_TYPE_` prefix so `usage_period_type_weekly`
/// and `weekly` share one `provider_usage` primary key.
pub fn canonical_usage_window(raw: &str) -> String {
    let lower = raw.trim().to_ascii_lowercase();
    lower
        .strip_prefix("usage_period_type_")
        .unwrap_or(&lower)
        .to_string()
}

pub fn now_rfc3339() -> String {
    Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

#[derive(Debug, Clone)]
pub struct Task {
    pub key: String,
    pub repo: Option<String>,
    pub issue: Option<i64>,
    pub status: String,
    pub provider: Option<String>,
    pub session_id: Option<String>,
    pub worktree: Option<String>,
    pub stall_retries: i64,
    pub handoff: Option<String>,
    pub tried: Option<String>,
    pub created: String,
    pub updated: String,
    pub shadow: bool,
    pub verify_retries: i64,
    pub protect_tests_skip: bool,
    pub labels: String,
    pub parent_key: Option<String>,
    pub role: String,
    pub deps: String,
    pub review_rounds: i64,
    pub provider_hint: String,
    pub title: String,
    /// git | dir | none
    pub workspace_kind: String,
    /// reply | pr | issue_comment | branch | patch | text | webhook
    pub output_kind: String,
    /// webhook URL, patch path, telegram chat id, inbox results dir
    pub output_spec: String,
    /// github | mcp | cli | http | telegram | inbox
    pub source_kind: String,
    pub result_text: String,
    pub result_diff: String,
    pub result_pr_url: String,
    /// Comma list of required provider capabilities (browser, desktop).
    pub needs: String,
    /// Compare arm: run only on `provider`, never fail over to another one.
    pub pin_strict: bool,
    /// Compare parent: the judge's decision as JSON (empty until judged).
    pub compare_decision: String,
}

impl Default for Task {
    fn default() -> Self {
        Self {
            key: String::new(),
            repo: None,
            issue: None,
            status: "queued".into(),
            provider: None,
            session_id: None,
            worktree: None,
            stall_retries: 0,
            handoff: None,
            tried: None,
            created: String::new(),
            updated: String::new(),
            shadow: false,
            verify_retries: 0,
            protect_tests_skip: false,
            labels: String::new(),
            parent_key: None,
            role: "worker".into(),
            deps: String::new(),
            review_rounds: 0,
            provider_hint: String::new(),
            title: String::new(),
            workspace_kind: "git".into(),
            output_kind: "reply".into(),
            output_spec: String::new(),
            source_kind: String::new(),
            result_text: String::new(),
            result_diff: String::new(),
            result_pr_url: String::new(),
            needs: String::new(),
            pin_strict: false,
            compare_decision: String::new(),
        }
    }
}

const TASK_SELECT: &str = "key, repo, issue, status, provider, session_id, worktree,
                          stall_retries, handoff, tried, created, updated, shadow,
                          verify_retries, protect_tests_skip, labels, parent_key, role,
                          deps, review_rounds, provider_hint, title,
                          workspace_kind, output_kind, output_spec, source_kind,
                          result_text, result_diff, result_pr_url, needs,
                          pin_strict, compare_decision";

#[derive(Debug, Clone)]
pub struct RunRow {
    pub id: i64,
    pub task_key: String,
    pub provider: String,
    pub model: Option<String>,
    pub pid: Option<i64>,
    pub started: String,
    pub ended: Option<String>,
    pub outcome: Option<String>,
    pub last_event_at: Option<String>,
    pub exit_code: Option<i64>,
    pub session_id: Option<String>,
    pub handoff: Option<String>,
    pub prefix_hash: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ProviderUsageRow {
    pub provider: String,
    pub window: String,
    pub utilization: Option<f64>,
    pub balance: Option<f64>,
    pub credit_usage_percent: Option<f64>,
    pub resets_at: Option<String>,
    pub status: Option<String>,
    pub updated_at: String,
}

#[derive(Debug, Clone, Default)]
pub struct ProviderRunStats {
    pub provider: String,
    pub runs: i64,
    pub successes: i64,
    pub tokens: i64,
    pub cost_usd: Option<f64>,
}

#[derive(Debug, Clone)]
pub struct EventRow {
    pub run_id: i64,
    pub kind: String,
    pub raw: String,
}

#[derive(Debug, Clone)]
pub struct Message {
    pub id: i64,
    pub body: String,
    pub source: String,
}

#[derive(Debug, Clone)]
pub struct UsageDay {
    pub provider: String,
    pub day: String,
    pub input: i64,
    pub output: i64,
    pub cache_read: i64,
    pub cache_write: i64,
    pub cost_usd: f64,
    pub runs: i64,
}

#[derive(Debug, Clone)]
pub struct UsageRow {
    pub run_id: i64,
    pub provider: String,
    pub input: i64,
    pub output: i64,
    pub cache_read: i64,
    pub cache_write: i64,
    pub cost_usd: Option<f64>,
    pub cost_kind: Option<String>,
}

#[derive(Debug, Clone)]
pub struct UsageDetail {
    pub provider: String,
    pub input: i64,
    pub output: i64,
    pub cache_read: i64,
    pub cache_write: i64,
    pub cost_usd: f64,
    pub is_resume: bool,
}

pub struct Db {
    conn: Mutex<Connection>,
}

impl Db {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("create db dir {}", dir.display()))?;
        }
        let conn =
            Connection::open(path).with_context(|| format!("open sqlite {}", path.display()))?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        let db = Db {
            conn: Mutex::new(conn),
        };
        db.migrate()?;
        Ok(db)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn migrate(&self) -> Result<()> {
        let conn = self.lock();
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS tasks (
                key TEXT PRIMARY KEY,
                repo TEXT,
                issue INTEGER,
                status TEXT NOT NULL,
                provider TEXT,
                session_id TEXT,
                worktree TEXT,
                stall_retries INTEGER NOT NULL DEFAULT 0,
                handoff TEXT,
                tried TEXT,
                comment_cursor TEXT,
                created TEXT NOT NULL,
                updated TEXT NOT NULL,
                shadow INTEGER NOT NULL DEFAULT 0
            );
            CREATE TABLE IF NOT EXISTS runs (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                task_key TEXT NOT NULL,
                provider TEXT NOT NULL,
                model TEXT,
                pid INTEGER,
                started TEXT NOT NULL,
                ended TEXT,
                outcome TEXT,
                last_event_at TEXT,
                exit_code INTEGER,
                session_id TEXT,
                FOREIGN KEY(task_key) REFERENCES tasks(key)
            );
            CREATE TABLE IF NOT EXISTS turns (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                run_id INTEGER NOT NULL,
                task_key TEXT NOT NULL,
                started TEXT NOT NULL,
                ended TEXT,
                outcome TEXT,
                FOREIGN KEY(run_id) REFERENCES runs(id)
            );
            CREATE TABLE IF NOT EXISTS messages (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                task_key TEXT NOT NULL,
                body TEXT NOT NULL,
                source TEXT NOT NULL,
                received TEXT NOT NULL,
                consumed_by_run INTEGER,
                FOREIGN KEY(task_key) REFERENCES tasks(key)
            );
            CREATE TABLE IF NOT EXISTS usage (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                run_id INTEGER NOT NULL,
                turn_id INTEGER,
                provider TEXT NOT NULL,
                input INTEGER NOT NULL DEFAULT 0,
                output INTEGER NOT NULL DEFAULT 0,
                cache_read INTEGER NOT NULL DEFAULT 0,
                cache_write INTEGER NOT NULL DEFAULT 0,
                cost_usd REAL,
                cost_kind TEXT,
                raw_json TEXT,
                recorded TEXT NOT NULL,
                FOREIGN KEY(run_id) REFERENCES runs(id)
            );
            CREATE TABLE IF NOT EXISTS provider_state (
                name TEXT PRIMARY KEY,
                cooldown_until TEXT,
                degraded INTEGER NOT NULL DEFAULT 0,
                degraded_until TEXT
            );
            CREATE TABLE IF NOT EXISTS events (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                run_id INTEGER NOT NULL,
                recorded TEXT NOT NULL,
                kind TEXT NOT NULL,
                raw_json TEXT NOT NULL,
                FOREIGN KEY(run_id) REFERENCES runs(id)
            );
            CREATE TABLE IF NOT EXISTS task_sessions (
                task_key TEXT NOT NULL,
                provider TEXT NOT NULL,
                session_id TEXT NOT NULL,
                updated TEXT NOT NULL,
                PRIMARY KEY (task_key, provider)
            );
            CREATE TABLE IF NOT EXISTS shadow_results (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                task_key TEXT NOT NULL,
                run_id INTEGER,
                provider TEXT NOT NULL,
                outcome TEXT NOT NULL,
                duration_ms INTEGER NOT NULL DEFAULT 0,
                input INTEGER NOT NULL DEFAULT 0,
                output INTEGER NOT NULL DEFAULT 0,
                cache_read INTEGER NOT NULL DEFAULT 0,
                cache_write INTEGER NOT NULL DEFAULT 0,
                cost_usd REAL NOT NULL DEFAULT 0,
                cache_hit REAL,
                tests TEXT,
                diff TEXT,
                diffstat TEXT,
                verify TEXT,
                recorded TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS verify_results (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                run_id INTEGER NOT NULL,
                task_key TEXT NOT NULL,
                command TEXT NOT NULL,
                passed INTEGER NOT NULL,
                exit_code INTEGER NOT NULL DEFAULT 0,
                duration_ms INTEGER NOT NULL DEFAULT 0,
                output TEXT,
                recorded TEXT NOT NULL,
                FOREIGN KEY(run_id) REFERENCES runs(id)
            );
            "#,
        )?;
        let _ = conn.execute(
            "ALTER TABLE provider_state ADD COLUMN degraded_until TEXT",
            [],
        );
        let _ = conn.execute("ALTER TABLE runs ADD COLUMN handoff TEXT", []);
        let _ = conn.execute(
            "ALTER TABLE tasks ADD COLUMN shadow INTEGER NOT NULL DEFAULT 0",
            [],
        );
        let _ = conn.execute("ALTER TABLE shadow_results ADD COLUMN diffstat TEXT", []);
        let _ = conn.execute(
            "ALTER TABLE tasks ADD COLUMN verify_retries INTEGER NOT NULL DEFAULT 0",
            [],
        );
        let _ = conn.execute("ALTER TABLE shadow_results ADD COLUMN verify TEXT", []);
        let _ = conn.execute(
            "ALTER TABLE provider_state ADD COLUMN quota_reset_at TEXT",
            [],
        );
        let _ = conn.execute(
            "ALTER TABLE provider_state ADD COLUMN last_limit_at TEXT",
            [],
        );
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS provider_limits (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                provider TEXT NOT NULL,
                recorded TEXT NOT NULL,
                reset_at TEXT,
                message TEXT,
                raw_json TEXT
            );
            CREATE TABLE IF NOT EXISTS meta (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );
            "#,
        )?;
        let _ = conn.execute(
            "ALTER TABLE provider_state ADD COLUMN enabled_override INTEGER",
            [],
        );
        let _ = conn.execute("ALTER TABLE runs ADD COLUMN prefix_hash TEXT", []);
        let _ = conn.execute(
            "ALTER TABLE usage ADD COLUMN is_resume INTEGER NOT NULL DEFAULT 0",
            [],
        );
        let _ = conn.execute(
            "ALTER TABLE tasks ADD COLUMN protect_tests_skip INTEGER NOT NULL DEFAULT 0",
            [],
        );
        let _ = conn.execute("ALTER TABLE tasks ADD COLUMN labels TEXT", []);
        let _ = conn.execute("ALTER TABLE runs ADD COLUMN checklist TEXT", []);
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS provider_usage (
                provider TEXT NOT NULL,
                window TEXT NOT NULL,
                utilization REAL,
                balance REAL,
                credit_usage_percent REAL,
                resets_at TEXT,
                status TEXT,
                updated_at TEXT NOT NULL,
                PRIMARY KEY (provider, window)
            );
            "#,
        )?;
        let _ = conn.execute(
            "ALTER TABLE provider_usage ADD COLUMN credit_usage_percent REAL",
            [],
        );
        let _ = conn.execute(
            "DELETE FROM usage WHERE id NOT IN (SELECT MAX(id) FROM usage GROUP BY run_id)",
            [],
        );
        conn.execute(
            "CREATE UNIQUE INDEX IF NOT EXISTS usage_one_per_run ON usage(run_id)",
            [],
        )?;
        let _ = conn.execute("ALTER TABLE tasks ADD COLUMN parent_key TEXT", []);
        let _ = conn.execute(
            "ALTER TABLE tasks ADD COLUMN role TEXT NOT NULL DEFAULT 'worker'",
            [],
        );
        let _ = conn.execute(
            "ALTER TABLE tasks ADD COLUMN deps TEXT NOT NULL DEFAULT ''",
            [],
        );
        let _ = conn.execute(
            "ALTER TABLE tasks ADD COLUMN review_rounds INTEGER NOT NULL DEFAULT 0",
            [],
        );
        let _ = conn.execute(
            "ALTER TABLE tasks ADD COLUMN provider_hint TEXT NOT NULL DEFAULT ''",
            [],
        );
        let _ = conn.execute(
            "ALTER TABLE tasks ADD COLUMN title TEXT NOT NULL DEFAULT ''",
            [],
        );
        let _ = conn.execute(
            "ALTER TABLE tasks ADD COLUMN needs TEXT NOT NULL DEFAULT ''",
            [],
        );
        let _ = conn.execute(
            "ALTER TABLE tasks ADD COLUMN workspace_kind TEXT NOT NULL DEFAULT ''",
            [],
        );
        let _ = conn.execute(
            "ALTER TABLE tasks ADD COLUMN output_kind TEXT NOT NULL DEFAULT ''",
            [],
        );
        let _ = conn.execute(
            "ALTER TABLE tasks ADD COLUMN output_spec TEXT NOT NULL DEFAULT ''",
            [],
        );
        let _ = conn.execute(
            "ALTER TABLE tasks ADD COLUMN source_kind TEXT NOT NULL DEFAULT ''",
            [],
        );
        let _ = conn.execute(
            "ALTER TABLE tasks ADD COLUMN result_text TEXT NOT NULL DEFAULT ''",
            [],
        );
        let _ = conn.execute(
            "ALTER TABLE tasks ADD COLUMN result_diff TEXT NOT NULL DEFAULT ''",
            [],
        );
        let _ = conn.execute(
            "ALTER TABLE tasks ADD COLUMN result_pr_url TEXT NOT NULL DEFAULT ''",
            [],
        );
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS lessons (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                repo TEXT NOT NULL,
                text TEXT NOT NULL,
                task_key TEXT NOT NULL DEFAULT '',
                run_id INTEGER,
                signal TEXT NOT NULL,
                created TEXT NOT NULL,
                last_hit TEXT,
                hits INTEGER NOT NULL DEFAULT 0,
                pinned INTEGER NOT NULL DEFAULT 0,
                shadow INTEGER NOT NULL DEFAULT 0
            );
            CREATE INDEX IF NOT EXISTS lessons_repo ON lessons(repo);
            "#,
        )?;
        let _ = conn.execute("ALTER TABLE runs ADD COLUMN lessons TEXT", []);
        let _ = conn.execute(
            "ALTER TABLE tasks ADD COLUMN pin_strict INTEGER NOT NULL DEFAULT 0",
            [],
        );
        let _ = conn.execute(
            "ALTER TABLE tasks ADD COLUMN compare_decision TEXT NOT NULL DEFAULT ''",
            [],
        );
        conn.execute_batch(
            r#"
            UPDATE tasks SET workspace_kind = CASE
                WHEN workspace_kind != '' THEN workspace_kind
                WHEN repo IS NULL OR repo = '' THEN 'none'
                ELSE 'git'
            END;
            UPDATE tasks SET output_kind = CASE
                WHEN output_kind != '' THEN output_kind
                ELSE 'reply'
            END;
            UPDATE tasks SET source_kind = CASE
                WHEN source_kind != '' THEN source_kind
                WHEN issue IS NOT NULL THEN 'github'
                ELSE 'cli'
            END;
            "#,
        )?;
        drop(conn);
        self.normalize_legacy_usage()?;
        Ok(())
    }

    /// Old Grok rows stored `prepaidBalance` cents and `creditUsagePercent`
    /// as-is with utilization blank. Convert once: balance/100, used fraction
    /// = percent/100. Empty `cost_kind`: `est` when a dollar figure exists,
    /// otherwise `plan` (cursor / spark / grok without an estimate). Grok
    /// rows that already have (or can recover) a cost stay `est`.
    fn normalize_legacy_usage(&self) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            r#"UPDATE provider_usage
               SET utilization = credit_usage_percent / 100.0,
                   balance = CASE
                       WHEN balance IS NOT NULL THEN balance / 100.0
                       ELSE NULL
                   END
               WHERE utilization IS NULL
                 AND credit_usage_percent IS NOT NULL"#,
            [],
        )?;
        dedupe_provider_usage_windows(&conn)?;
        conn.execute(
            r#"UPDATE usage SET cost_kind = 'est'
               WHERE cost_usd IS NOT NULL
                 AND (cost_kind IS NULL OR cost_kind = '')"#,
            [],
        )?;
        conn.execute(
            r#"UPDATE usage SET cost_kind = 'plan'
               WHERE cost_usd IS NULL
                 AND (cost_kind IS NULL OR cost_kind = '')"#,
            [],
        )?;
        recover_grok_cost_from_raw(&conn)?;
        conn.execute(
            r#"UPDATE usage SET cost_kind = 'est'
               WHERE lower(provider) = 'grok'
                 AND cost_usd IS NOT NULL
                 AND cost_kind = 'plan'"#,
            [],
        )?;
        // `grok usage` reports `costUsdTicks` (1 USD = 1e10 ticks). Older atr
        // divided by 1e6, so those rows are 10,000x too high. Fix them once.
        let grok_ticks_done: Option<String> = conn
            .query_row(
                "SELECT value FROM meta WHERE key = ?1",
                params!["usage_grok_ticks_1e10"],
                |r| r.get(0),
            )
            .optional()?;
        if grok_ticks_done.as_deref() != Some("1") {
            conn.execute(
                r#"UPDATE usage SET cost_usd = cost_usd / 10000.0
                   WHERE cost_usd IS NOT NULL
                     AND raw_json LIKE '%"source":"grok usage"%'"#,
                [],
            )?;
            conn.execute(
                r#"INSERT INTO meta (key, value) VALUES ('usage_grok_ticks_1e10', '1')
                   ON CONFLICT(key) DO UPDATE SET value = excluded.value"#,
                [],
            )?;
        }
        // Codex-kind rows used to store inclusive input (cached ⊂ input).
        // Subtract once so totals and hit% match the live extractor.
        let codex_done: Option<String> = conn
            .query_row(
                "SELECT value FROM meta WHERE key = ?1",
                params!["usage_codex_input_uncached"],
                |r| r.get(0),
            )
            .optional()?;
        if codex_done.as_deref() != Some("1") {
            conn.execute(
                r#"UPDATE usage SET input = input - cache_read
                   WHERE lower(provider) IN ('codex', 'spark')
                     AND cache_read > 0
                     AND input >= cache_read"#,
                [],
            )?;
            conn.execute(
                r#"INSERT INTO meta (key, value) VALUES ('usage_codex_input_uncached', '1')
                   ON CONFLICT(key) DO UPDATE SET value = excluded.value"#,
                [],
            )?;
        }
        Ok(())
    }

    pub fn upsert_task(&self, key: &str, repo: &str, issue: Option<i64>) -> Result<Task> {
        self.upsert_task_spec(key, repo, issue, None, None, None, None)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn upsert_task_spec(
        &self,
        key: &str,
        repo: &str,
        issue: Option<i64>,
        workspace_kind: Option<&str>,
        output_kind: Option<&str>,
        output_spec: Option<&str>,
        source_kind: Option<&str>,
    ) -> Result<Task> {
        let now = now_rfc3339();
        let ws = workspace_kind
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| {
                if repo.trim().is_empty() {
                    "none"
                } else {
                    "git"
                }
            });
        let out = output_kind
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("reply");
        let spec = output_spec.unwrap_or("").trim();
        let src = source_kind
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| if issue.is_some() { "github" } else { "" });
        let conn = self.lock();
        conn.execute(
            r#"
            INSERT INTO tasks (
                key, repo, issue, status, created, updated,
                workspace_kind, output_kind, output_spec, source_kind
            )
            VALUES (?1, ?2, ?3, 'queued', ?4, ?4, ?5, ?6, ?7, ?8)
            ON CONFLICT(key) DO UPDATE SET
                repo = CASE
                    WHEN excluded.repo IS NULL OR excluded.repo = '' THEN tasks.repo
                    ELSE excluded.repo
                END,
                issue = COALESCE(excluded.issue, tasks.issue),
                workspace_kind = CASE
                    WHEN excluded.workspace_kind = '' THEN tasks.workspace_kind
                    ELSE excluded.workspace_kind
                END,
                output_kind = CASE
                    WHEN excluded.output_kind = '' THEN tasks.output_kind
                    ELSE excluded.output_kind
                END,
                output_spec = CASE
                    WHEN excluded.output_spec = '' THEN tasks.output_spec
                    ELSE excluded.output_spec
                END,
                source_kind = CASE
                    WHEN excluded.source_kind = '' THEN tasks.source_kind
                    ELSE excluded.source_kind
                END,
                updated = excluded.updated
            "#,
            params![key, repo, issue, now, ws, out, spec, src],
        )?;
        drop(conn);
        self.get_task(key)?
            .ok_or_else(|| anyhow::anyhow!("task {key} missing after upsert"))
    }

    pub fn get_task(&self, key: &str) -> Result<Option<Task>> {
        let conn = self.lock();
        let t = conn
            .query_row(
                &format!("SELECT {TASK_SELECT} FROM tasks WHERE key = ?1"),
                params![key],
                row_to_task,
            )
            .optional()?;
        Ok(t)
    }

    /// Tasks for display and scheduling. Internal reflect tasks
    /// (`role = 'reflect'`, `<key>-reflect`) are hidden; see [`Db::list_all_tasks`].
    pub fn list_tasks(&self) -> Result<Vec<Task>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(&format!(
            "SELECT {TASK_SELECT} FROM tasks WHERE role != 'reflect' ORDER BY updated DESC"
        ))?;
        let rows = stmt.query_map([], row_to_task)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// Every task row, including internal reflect tasks (for gc).
    pub fn list_all_tasks(&self) -> Result<Vec<Task>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(&format!(
            "SELECT {TASK_SELECT} FROM tasks ORDER BY updated DESC"
        ))?;
        let rows = stmt.query_map([], row_to_task)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn set_status(&self, key: &str, status: &str) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "UPDATE tasks SET status = ?1, updated = ?2 WHERE key = ?3",
            params![status, now_rfc3339(), key],
        )?;
        Ok(())
    }

    /// Queue a task unless it is running, in one statement. A separate read
    /// then `set_status` races the scheduler's `claim_task` and can flip a
    /// task it just claimed back to `queued` while its run is live.
    pub fn queue_unless_running(&self, key: &str) -> Result<bool> {
        let conn = self.lock();
        let n = conn.execute(
            "UPDATE tasks SET status = 'queued', updated = ?1 WHERE key = ?2 AND status != 'running'",
            params![now_rfc3339(), key],
        )?;
        Ok(n == 1)
    }

    pub fn set_parent_key(&self, key: &str, parent: &str) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "UPDATE tasks SET parent_key = ?1, updated = ?2 WHERE key = ?3",
            params![parent, now_rfc3339(), key],
        )?;
        Ok(())
    }

    /// Pin a task strictly to its provider (compare arms): no failover.
    pub fn set_pin_strict(&self, key: &str, strict: bool) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "UPDATE tasks SET pin_strict = ?1, updated = ?2 WHERE key = ?3",
            params![strict as i64, now_rfc3339(), key],
        )?;
        Ok(())
    }

    pub fn set_compare_decision(&self, key: &str, json: &str) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "UPDATE tasks SET compare_decision = ?1, updated = ?2 WHERE key = ?3",
            params![json, now_rfc3339(), key],
        )?;
        Ok(())
    }

    pub fn set_task_role(&self, key: &str, role: &str) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "UPDATE tasks SET role = ?1, updated = ?2 WHERE key = ?3",
            params![role, now_rfc3339(), key],
        )?;
        Ok(())
    }

    pub fn set_task_deps(&self, key: &str, deps: &str) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "UPDATE tasks SET deps = ?1, updated = ?2 WHERE key = ?3",
            params![deps, now_rfc3339(), key],
        )?;
        Ok(())
    }

    pub fn set_review_rounds(&self, key: &str, n: i64) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "UPDATE tasks SET review_rounds = ?1, updated = ?2 WHERE key = ?3",
            params![n, now_rfc3339(), key],
        )?;
        Ok(())
    }

    pub fn set_provider_hint(&self, key: &str, hint: &str) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "UPDATE tasks SET provider_hint = ?1, updated = ?2 WHERE key = ?3",
            params![hint, now_rfc3339(), key],
        )?;
        Ok(())
    }

    pub fn set_task_needs(&self, key: &str, needs: &str) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "UPDATE tasks SET needs = ?1, updated = ?2 WHERE key = ?3",
            params![needs, now_rfc3339(), key],
        )?;
        Ok(())
    }

    pub fn set_task_title(&self, key: &str, title: &str) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "UPDATE tasks SET title = ?1, updated = ?2 WHERE key = ?3",
            params![title, now_rfc3339(), key],
        )?;
        Ok(())
    }

    pub fn set_task_result(
        &self,
        key: &str,
        text: &str,
        diff: &str,
        pr_url: Option<&str>,
    ) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            r#"UPDATE tasks SET result_text = ?1, result_diff = ?2,
                   result_pr_url = CASE WHEN ?3 IS NULL OR ?3 = '' THEN result_pr_url ELSE ?3 END,
                   updated = ?4
               WHERE key = ?5"#,
            params![text, diff, pr_url.unwrap_or(""), now_rfc3339(), key],
        )?;
        Ok(())
    }

    pub fn children_of(&self, parent: &str) -> Result<Vec<Task>> {
        Ok(self
            .list_tasks()?
            .into_iter()
            .filter(|t| t.parent_key.as_deref() == Some(parent))
            .collect())
    }

    pub fn last_run_outcome(&self, key: &str) -> Result<Option<String>> {
        let conn = self.lock();
        let v = conn
            .query_row(
                "SELECT outcome FROM runs WHERE task_key = ?1 AND ended IS NOT NULL ORDER BY id DESC LIMIT 1",
                params![key],
                |r| r.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten();
        Ok(v)
    }

    pub fn last_finished_run(&self, key: &str) -> Result<Option<(String, String)>> {
        let conn = self.lock();
        let row = conn
            .query_row(
                "SELECT provider, ended FROM runs WHERE task_key = ?1 AND ended IS NOT NULL ORDER BY id DESC LIMIT 1",
                params![key],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?)),
            )
            .optional()?;
        Ok(row.and_then(|(p, e)| e.map(|ended| (p, ended))))
    }

    pub fn latest_finished_provider(&self) -> Result<Option<String>> {
        let conn = self.lock();
        let v = conn
            .query_row(
                "SELECT provider FROM runs WHERE ended IS NOT NULL ORDER BY ended DESC, id DESC LIMIT 1",
                [],
                |r| r.get::<_, String>(0),
            )
            .optional()?;
        Ok(v)
    }

    /// Finished (`waiting`/`done`), failed, or cancelled. Running and queued stay.
    pub fn removable_status(status: &str) -> bool {
        matches!(status, "waiting" | "failed" | "done" | "cancelled")
    }

    pub fn has_open_run(&self, key: &str) -> Result<bool> {
        let conn = self.lock();
        let n: i64 = conn.query_row(
            "SELECT COUNT(*) FROM runs WHERE task_key = ?1 AND ended IS NULL",
            params![key],
            |r| r.get(0),
        )?;
        Ok(n > 0)
    }

    /// `None` if this key can be deleted. Running and unfinished rows are refused.
    pub fn delete_refuse_reason(&self, key: &str) -> Result<Option<String>> {
        match self.get_task(key)? {
            None => Ok(Some(format!("unknown task {key}"))),
            Some(t) if t.status == "running" => Ok(Some(format!("task {key} is running"))),
            Some(t) if !Self::removable_status(&t.status) => Ok(Some(format!(
                "task {key} is {} (only finished, failed, or cancelled may be removed)",
                t.status
            ))),
            Some(_) if self.has_open_run(key)? => Ok(Some(format!("task {key} has an open run"))),
            Some(_) => Ok(None),
        }
    }

    /// Delete a finished/failed/cancelled task and its sessions, runs, turns, events, messages.
    pub fn delete_task(&self, key: &str) -> Result<()> {
        if let Some(reason) = self.delete_refuse_reason(key)? {
            anyhow::bail!("{reason}");
        }
        self.delete_task_rows(key)
    }

    fn delete_task_rows(&self, key: &str) -> Result<()> {
        let conn = self.lock();
        conn.pragma_update(None, "foreign_keys", "OFF")?;
        let _ = conn.execute(
            "DELETE FROM events WHERE run_id IN (SELECT id FROM runs WHERE task_key = ?1)",
            params![key],
        );
        let _ = conn.execute(
            "DELETE FROM turns WHERE task_key = ?1 OR run_id IN (SELECT id FROM runs WHERE task_key = ?1)",
            params![key],
        );
        let _ = conn.execute(
            "DELETE FROM verify_results WHERE task_key = ?1 OR run_id IN (SELECT id FROM runs WHERE task_key = ?1)",
            params![key],
        );
        let _ = conn.execute(
            "DELETE FROM shadow_results WHERE task_key = ?1",
            params![key],
        );
        let _ = conn.execute("DELETE FROM messages WHERE task_key = ?1", params![key]);
        let _ = conn.execute(
            "DELETE FROM task_sessions WHERE task_key = ?1",
            params![key],
        );
        let _ = conn.execute("DELETE FROM runs WHERE task_key = ?1", params![key]);
        conn.execute("DELETE FROM tasks WHERE key = ?1", params![key])?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        Ok(())
    }

    pub fn claim_task(&self, key: &str) -> Result<bool> {
        let conn = self.lock();
        let n = conn.execute(
            r#"UPDATE tasks SET status = 'running', updated = ?1
               WHERE key = ?2 AND status IN ('queued', 'waiting')"#,
            params![now_rfc3339(), key],
        )?;
        Ok(n == 1)
    }

    pub fn mark_shadow(&self, key: &str) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "UPDATE tasks SET shadow = 1, updated = ?1 WHERE key = ?2",
            params![now_rfc3339(), key],
        )?;
        Ok(())
    }

    pub fn is_shadow(&self, key: &str) -> Result<bool> {
        Ok(self.get_task(key)?.map(|t| t.shadow).unwrap_or(false))
    }

    pub fn set_worktree(&self, key: &str, path: &str) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "UPDATE tasks SET worktree = ?1, updated = ?2 WHERE key = ?3",
            params![path, now_rfc3339(), key],
        )?;
        Ok(())
    }

    pub fn clear_worktree(&self, key: &str) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "UPDATE tasks SET worktree = NULL, updated = ?1 WHERE key = ?2",
            params![now_rfc3339(), key],
        )?;
        Ok(())
    }

    pub fn set_timestamps(&self, key: &str, created: &str, updated: &str) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "UPDATE tasks SET created = ?1, updated = ?2 WHERE key = ?3",
            params![created, updated, key],
        )?;
        Ok(())
    }

    pub fn set_provider_session(
        &self,
        key: &str,
        provider: Option<&str>,
        session: Option<&str>,
    ) -> Result<()> {
        let now = now_rfc3339();
        let conn = self.lock();
        conn.execute(
            "UPDATE tasks SET provider = ?1, session_id = ?2, updated = ?3 WHERE key = ?4",
            params![provider, session, now, key],
        )?;
        if let (Some(p), Some(s)) = (provider, session) {
            if !p.is_empty() && !s.is_empty() {
                conn.execute(
                    r#"INSERT INTO task_sessions (task_key, provider, session_id, updated)
                       VALUES (?1, ?2, ?3, ?4)
                       ON CONFLICT(task_key, provider) DO UPDATE SET
                         session_id = excluded.session_id,
                         updated = excluded.updated"#,
                    params![key, p, s, now],
                )?;
            }
        } else if let (Some(p), None) = (provider, session) {
            // Forget this provider's session only. Failover passes (None, None)
            // so other providers' ids stay available if we return later.
            if !p.is_empty() {
                conn.execute(
                    "DELETE FROM task_sessions WHERE task_key = ?1 AND provider = ?2",
                    params![key, p],
                )?;
            }
        }
        Ok(())
    }

    /// Session previously recorded for this task on this provider only.
    pub fn task_session(&self, key: &str, provider: &str) -> Result<Option<String>> {
        let conn = self.lock();
        let v = conn
            .query_row(
                "SELECT session_id FROM task_sessions WHERE task_key = ?1 AND provider = ?2",
                params![key, provider],
                |r| r.get::<_, String>(0),
            )
            .optional()?;
        Ok(v)
    }

    pub fn set_handoff(&self, key: &str, handoff: Option<&str>) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "UPDATE tasks SET handoff = ?1, updated = ?2 WHERE key = ?3",
            params![handoff, now_rfc3339(), key],
        )?;
        Ok(())
    }

    pub fn set_tried(&self, key: &str, tried: Option<&str>) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "UPDATE tasks SET tried = ?1, updated = ?2 WHERE key = ?3",
            params![tried, now_rfc3339(), key],
        )?;
        Ok(())
    }

    pub fn set_stall_retries(&self, key: &str, n: i64) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "UPDATE tasks SET stall_retries = ?1, updated = ?2 WHERE key = ?3",
            params![n, now_rfc3339(), key],
        )?;
        Ok(())
    }

    pub fn set_verify_retries(&self, key: &str, n: i64) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "UPDATE tasks SET verify_retries = ?1, updated = ?2 WHERE key = ?3",
            params![n, now_rfc3339(), key],
        )?;
        Ok(())
    }

    pub fn set_protect_tests_skip(&self, key: &str, skip: bool) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "UPDATE tasks SET protect_tests_skip = ?1, updated = ?2 WHERE key = ?3",
            params![if skip { 1 } else { 0 }, now_rfc3339(), key],
        )?;
        Ok(())
    }

    pub fn set_task_labels(&self, key: &str, labels: &str) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "UPDATE tasks SET labels = ?1, updated = ?2 WHERE key = ?3",
            params![labels, now_rfc3339(), key],
        )?;
        Ok(())
    }

    pub fn set_run_checklist(&self, run_id: i64, text: &str) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "UPDATE runs SET checklist = ?1 WHERE id = ?2",
            params![text, run_id],
        )?;
        Ok(())
    }

    pub fn run_checklist(&self, run_id: i64) -> Result<Option<String>> {
        let conn = self.lock();
        let v = conn
            .query_row(
                "SELECT checklist FROM runs WHERE id = ?1",
                params![run_id],
                |r| r.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten();
        Ok(v)
    }

    pub fn set_comment_cursor(&self, key: &str, cursor: &str) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "UPDATE tasks SET comment_cursor = ?1, updated = ?2 WHERE key = ?3",
            params![cursor, now_rfc3339(), key],
        )?;
        Ok(())
    }

    pub fn comment_cursor(&self, key: &str) -> Result<Option<String>> {
        let conn = self.lock();
        let v = conn
            .query_row(
                "SELECT comment_cursor FROM tasks WHERE key = ?1",
                params![key],
                |r| r.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten();
        Ok(v)
    }

    pub fn enqueue_message(&self, key: &str, body: &str, source: &str) -> Result<i64> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO messages (task_key, body, source, received) VALUES (?1, ?2, ?3, ?4)",
            params![key, body, source, now_rfc3339()],
        )?;
        Ok(conn.last_insert_rowid())
    }

    pub fn pending_messages(&self, key: &str) -> Result<Vec<Message>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            r#"SELECT id, body, source FROM messages
               WHERE task_key = ?1 AND consumed_by_run IS NULL
               ORDER BY id ASC"#,
        )?;
        let rows = stmt.query_map(params![key], |r| {
            Ok(Message {
                id: r.get(0)?,
                body: r.get(1)?,
                source: r.get(2)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn mark_consumed(&self, ids: &[i64], run_id: i64) -> Result<()> {
        let conn = self.lock();
        for id in ids {
            conn.execute(
                "UPDATE messages SET consumed_by_run = ?1 WHERE id = ?2",
                params![run_id, id],
            )?;
        }
        Ok(())
    }

    pub fn unconsume_run(&self, run_id: i64) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "UPDATE messages SET consumed_by_run = NULL WHERE consumed_by_run = ?1",
            params![run_id],
        )?;
        Ok(())
    }

    pub fn start_run(&self, task_key: &str, provider: &str, model: &str) -> Result<i64> {
        let now = now_rfc3339();
        let conn = self.lock();
        conn.execute(
            r#"INSERT INTO runs (task_key, provider, model, started, last_event_at)
               VALUES (?1, ?2, ?3, ?4, ?4)"#,
            params![task_key, provider, model, now],
        )?;
        Ok(conn.last_insert_rowid())
    }

    pub fn set_run_pid(&self, run_id: i64, pid: u32) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "UPDATE runs SET pid = ?1 WHERE id = ?2",
            params![pid as i64, run_id],
        )?;
        Ok(())
    }

    pub fn touch_run(&self, run_id: i64) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "UPDATE runs SET last_event_at = ?1 WHERE id = ?2",
            params![now_rfc3339(), run_id],
        )?;
        Ok(())
    }

    pub fn set_run_session(&self, run_id: i64, session: &str) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "UPDATE runs SET session_id = ?1 WHERE id = ?2",
            params![session, run_id],
        )?;
        Ok(())
    }

    pub fn set_run_model(&self, run_id: i64, model: &str) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "UPDATE runs SET model = ?1 WHERE id = ?2 AND (model IS NULL OR model = '')",
            params![model, run_id],
        )?;
        Ok(())
    }

    pub fn set_run_handoff(&self, run_id: i64, handoff: &str) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "UPDATE runs SET handoff = ?1 WHERE id = ?2",
            params![handoff, run_id],
        )?;
        Ok(())
    }

    pub fn set_run_prefix_hash(&self, run_id: i64, hash: &str) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "UPDATE runs SET prefix_hash = ?1 WHERE id = ?2",
            params![hash, run_id],
        )?;
        Ok(())
    }

    pub fn last_prefix_hash(
        &self,
        session: &str,
        provider: &str,
        except_run: i64,
    ) -> Result<Option<String>> {
        let conn = self.lock();
        let v = conn
            .query_row(
                r#"SELECT prefix_hash FROM runs
                   WHERE session_id = ?1 AND provider = ?2 AND id != ?3
                     AND prefix_hash IS NOT NULL AND prefix_hash != ''
                   ORDER BY id DESC LIMIT 1"#,
                params![session, provider, except_run],
                |r| r.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten();
        Ok(v)
    }

    pub fn session_prefix_changed(&self, key: &str) -> Result<bool> {
        let conn = self.lock();
        let n: i64 = conn.query_row(
            r#"SELECT COUNT(*) FROM (
                   SELECT session_id FROM runs
                   WHERE task_key = ?1 AND session_id IS NOT NULL
                     AND prefix_hash IS NOT NULL AND prefix_hash != ''
                   GROUP BY session_id
                   HAVING COUNT(DISTINCT prefix_hash) > 1
               )"#,
            params![key],
            |r| r.get(0),
        )?;
        Ok(n > 0)
    }

    pub fn set_run_times(&self, run_id: i64, started: &str, ended: &str) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "UPDATE runs SET started = ?1, ended = ?2 WHERE id = ?3",
            params![started, ended, run_id],
        )?;
        Ok(())
    }

    pub fn insert_event_at(
        &self,
        run_id: i64,
        kind: &str,
        raw: &str,
        recorded: &str,
    ) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO events (run_id, recorded, kind, raw_json) VALUES (?1, ?2, ?3, ?4)",
            params![run_id, recorded, kind, raw],
        )?;
        Ok(())
    }

    pub fn prune_events(&self, before: &str, dry_run: bool) -> Result<u64> {
        let conn = self.lock();
        let n: i64 = conn.query_row(
            "SELECT COUNT(*) FROM events WHERE recorded < ?1",
            params![before],
            |r| r.get(0),
        )?;
        if !dry_run && n > 0 {
            conn.execute("DELETE FROM events WHERE recorded < ?1", params![before])?;
        }
        Ok(n as u64)
    }

    pub fn prune_runs(&self, before: &str, dry_run: bool) -> Result<u64> {
        let conn = self.lock();
        let n: i64 = conn.query_row(
            r#"SELECT COUNT(*) FROM runs
               WHERE ended IS NOT NULL AND ended < ?1"#,
            params![before],
            |r| r.get(0),
        )?;
        if !dry_run && n > 0 {
            conn.pragma_update(None, "foreign_keys", "OFF")?;
            let _ = conn.execute(
                r#"DELETE FROM events WHERE run_id IN (
                       SELECT id FROM runs WHERE ended IS NOT NULL AND ended < ?1
                   )"#,
                params![before],
            );
            let _ = conn.execute(
                r#"DELETE FROM turns WHERE run_id IN (
                       SELECT id FROM runs WHERE ended IS NOT NULL AND ended < ?1
                   )"#,
                params![before],
            );
            let _ = conn.execute(
                r#"DELETE FROM verify_results WHERE run_id IN (
                       SELECT id FROM runs WHERE ended IS NOT NULL AND ended < ?1
                   )"#,
                params![before],
            );
            conn.execute(
                "DELETE FROM runs WHERE ended IS NOT NULL AND ended < ?1",
                params![before],
            )?;
            conn.pragma_update(None, "foreign_keys", "ON")?;
        }
        Ok(n as u64)
    }

    pub fn insert_event(&self, run_id: i64, kind: &str, raw: &str) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO events (run_id, recorded, kind, raw_json) VALUES (?1, ?2, ?3, ?4)",
            params![run_id, now_rfc3339(), kind, raw],
        )?;
        Ok(())
    }

    pub fn events_for_run(&self, run_id: i64) -> Result<Vec<EventRow>> {
        let conn = self.lock();
        let mut stmt = conn
            .prepare("SELECT run_id, kind, raw_json FROM events WHERE run_id = ?1 ORDER BY id")?;
        let rows = stmt.query_map(params![run_id], row_to_event)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn events_for_task(&self, key: &str) -> Result<Vec<EventRow>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            r#"SELECT e.run_id, e.kind, e.raw_json FROM events e
               JOIN runs r ON r.id = e.run_id
               WHERE r.task_key = ?1
               ORDER BY e.id"#,
        )?;
        let rows = stmt.query_map(params![key], row_to_event)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn all_messages(&self, key: &str) -> Result<Vec<Message>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            r#"SELECT id, body, source FROM messages
               WHERE task_key = ?1 ORDER BY id ASC"#,
        )?;
        let rows = stmt.query_map(params![key], |r| {
            Ok(Message {
                id: r.get(0)?,
                body: r.get(1)?,
                source: r.get(2)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn finish_run(&self, run_id: i64, outcome: &str, exit_code: Option<i32>) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "UPDATE runs SET ended = ?1, outcome = ?2, exit_code = ?3 WHERE id = ?4",
            params![now_rfc3339(), outcome, exit_code, run_id],
        )?;
        Ok(())
    }

    pub fn set_run_outcome(&self, run_id: i64, outcome: &str) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "UPDATE runs SET outcome = ?1 WHERE id = ?2",
            params![outcome, run_id],
        )?;
        Ok(())
    }

    pub fn open_runs(&self) -> Result<Vec<RunRow>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            r#"SELECT id, task_key, provider, model, pid, started, ended, outcome,
                      last_event_at, exit_code, session_id, handoff, prefix_hash
               FROM runs WHERE ended IS NULL"#,
        )?;
        let rows = stmt.query_map([], row_to_run)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn list_runs(&self, key: Option<&str>) -> Result<Vec<RunRow>> {
        let conn = self.lock();
        if let Some(key) = key {
            let mut stmt = conn.prepare(
                r#"SELECT id, task_key, provider, model, pid, started, ended, outcome,
                          last_event_at, exit_code, session_id, handoff, prefix_hash
                   FROM runs WHERE task_key = ?1 ORDER BY id"#,
            )?;
            let rows = stmt.query_map(params![key], row_to_run)?;
            return rows.collect::<Result<Vec<_>, _>>().map_err(Into::into);
        }
        let mut stmt = conn.prepare(
            r#"SELECT id, task_key, provider, model, pid, started, ended, outcome,
                      last_event_at, exit_code, session_id, handoff, prefix_hash
               FROM runs ORDER BY id DESC LIMIT 50"#,
        )?;
        let rows = stmt.query_map([], row_to_run)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn start_turn(&self, run_id: i64, task_key: &str) -> Result<i64> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO turns (run_id, task_key, started) VALUES (?1, ?2, ?3)",
            params![run_id, task_key, now_rfc3339()],
        )?;
        Ok(conn.last_insert_rowid())
    }

    pub fn finish_turn(&self, turn_id: i64, outcome: &str) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "UPDATE turns SET ended = ?1, outcome = ?2 WHERE id = ?3",
            params![now_rfc3339(), outcome, turn_id],
        )?;
        Ok(())
    }

    pub fn insert_usage(
        &self,
        run_id: i64,
        turn_id: Option<i64>,
        provider: &str,
        input: u64,
        output: u64,
        cache_read: u64,
        cache_write: u64,
        cost_usd: Option<f64>,
        cost_kind: Option<&str>,
        raw_json: &str,
    ) -> Result<()> {
        self.insert_usage_at(
            run_id,
            turn_id,
            provider,
            input,
            output,
            cache_read,
            cache_write,
            cost_usd,
            cost_kind,
            raw_json,
            &now_rfc3339(),
        )
    }

    pub fn insert_usage_at(
        &self,
        run_id: i64,
        turn_id: Option<i64>,
        provider: &str,
        input: u64,
        output: u64,
        cache_read: u64,
        cache_write: u64,
        cost_usd: Option<f64>,
        cost_kind: Option<&str>,
        raw_json: &str,
        recorded: &str,
    ) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            r#"INSERT INTO usage (run_id, turn_id, provider, input, output, cache_read,
                   cache_write, cost_usd, cost_kind, raw_json, recorded)
               VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
               ON CONFLICT(run_id) DO UPDATE SET
                   turn_id = excluded.turn_id,
                   provider = excluded.provider,
                   input = excluded.input,
                   output = excluded.output,
                   cache_read = excluded.cache_read,
                   cache_write = excluded.cache_write,
                   cost_usd = excluded.cost_usd,
                   cost_kind = excluded.cost_kind,
                   raw_json = excluded.raw_json,
                   recorded = excluded.recorded"#,
            params![
                run_id,
                turn_id,
                provider,
                input as i64,
                output as i64,
                cache_read as i64,
                cache_write as i64,
                cost_usd,
                cost_kind,
                raw_json,
                recorded
            ],
        )?;
        Ok(())
    }

    pub fn set_usage_resume(&self, run_id: i64, resume: bool) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "UPDATE usage SET is_resume = ?1 WHERE run_id = ?2",
            params![if resume { 1 } else { 0 }, run_id],
        )?;
        Ok(())
    }

    /// Per-provider run stats for runs started at/after `since` (RFC 3339 UTC).
    pub fn provider_run_stats(&self, since: &str) -> Result<Vec<ProviderRunStats>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            r#"SELECT r.provider, COUNT(*),
                      SUM(CASE WHEN r.outcome = 'success' THEN 1 ELSE 0 END),
                      COALESCE(SUM(u.t), 0), SUM(u.c)
               FROM runs r
               LEFT JOIN (SELECT run_id, SUM(input + output + cache_read + cache_write) AS t,
                                 SUM(cost_usd) AS c
                          FROM usage GROUP BY run_id) u ON u.run_id = r.id
               WHERE r.started >= ?1
               GROUP BY r.provider ORDER BY r.provider"#,
        )?;
        let rows = stmt.query_map(params![since], |r| {
            Ok(ProviderRunStats {
                provider: r.get(0)?,
                runs: r.get(1)?,
                successes: r.get::<_, Option<i64>>(2)?.unwrap_or(0),
                tokens: r.get(3)?,
                cost_usd: r.get(4)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// Latest successful run end per provider.
    pub fn last_success_by_provider(&self) -> Result<HashMap<String, String>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT provider, MAX(ended) FROM runs WHERE outcome = 'success' AND ended IS NOT NULL GROUP BY provider",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        rows.collect::<Result<HashMap<_, _>, _>>()
            .map_err(Into::into)
    }

    /// Total tokens and summed cost for one run (cost None when never reported).
    pub fn run_totals(&self, run_id: i64) -> Result<(i64, Option<f64>)> {
        let conn = self.lock();
        conn.query_row(
            "SELECT COALESCE(SUM(input + output + cache_read + cache_write), 0), SUM(cost_usd) FROM usage WHERE run_id = ?1",
            params![run_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .map_err(Into::into)
    }

    /// Most recent event of a run (for a one-line progress hint).
    pub fn last_event_for_run(&self, run_id: i64) -> Result<Option<EventRow>> {
        let conn = self.lock();
        conn.query_row(
            "SELECT run_id, kind, raw_json FROM events WHERE run_id = ?1 ORDER BY id DESC LIMIT 1",
            params![run_id],
            row_to_event,
        )
        .optional()
        .map_err(Into::into)
    }

    /// Prompt for a run: first message that run consumed, else the task's first.
    pub fn run_prompt(&self, key: &str, run_id: i64) -> Result<Option<String>> {
        let conn = self.lock();
        conn.query_row(
            "SELECT body FROM messages WHERE task_key = ?1
             ORDER BY (consumed_by_run = ?2) DESC, id LIMIT 1",
            params![key, run_id],
            |r| r.get(0),
        )
        .optional()
        .map_err(Into::into)
    }

    pub fn list_usage_for_run(&self, run_id: i64) -> Result<Vec<UsageRow>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            r#"SELECT run_id, provider, input, output, cache_read, cache_write,
                      cost_usd, cost_kind
               FROM usage WHERE run_id = ?1 ORDER BY id"#,
        )?;
        let rows = stmt.query_map(params![run_id], |r| {
            Ok(UsageRow {
                run_id: r.get(0)?,
                provider: r.get(1)?,
                input: r.get(2)?,
                output: r.get(3)?,
                cache_read: r.get(4)?,
                cache_write: r.get(5)?,
                cost_usd: r.get(6)?,
                cost_kind: r.get(7)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// `(input, output, cost_usd, run_count)` since `since` (RFC3339).
    pub fn usage_since(&self, provider: &str, since: &str) -> Result<(i64, i64, f64, i64)> {
        let conn = self.lock();
        let (input, output, cost): (i64, i64, f64) = conn.query_row(
            r#"SELECT COALESCE(SUM(input), 0), COALESCE(SUM(output), 0),
                      COALESCE(SUM(cost_usd), 0)
               FROM usage WHERE provider = ?1 AND recorded >= ?2"#,
            params![provider, since],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        let runs: i64 = conn.query_row(
            r#"SELECT COUNT(*) FROM runs
               WHERE provider = ?1 AND started >= ?2 AND ended IS NOT NULL"#,
            params![provider, since],
            |r| r.get(0),
        )?;
        Ok((input, output, cost, runs))
    }

    pub fn usage_oldest(&self, provider: &str, since: &str) -> Result<Option<String>> {
        let conn = self.lock();
        let v = conn
            .query_row(
                r#"SELECT MIN(recorded) FROM usage WHERE provider = ?1 AND recorded >= ?2"#,
                params![provider, since],
                |r| r.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten();
        Ok(v)
    }

    pub fn record_limit(
        &self,
        provider: &str,
        reset_at: Option<&str>,
        message: &str,
        raw: &str,
        recorded: &str,
    ) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            r#"INSERT INTO provider_limits (provider, recorded, reset_at, message, raw_json)
               VALUES (?1, ?2, ?3, ?4, ?5)"#,
            params![provider, recorded, reset_at, message, raw],
        )?;
        conn.execute(
            r#"INSERT INTO provider_state (name, cooldown_until, degraded, last_limit_at, quota_reset_at)
               VALUES (?1, NULL, 0, ?2, ?3)
               ON CONFLICT(name) DO UPDATE SET
                   last_limit_at = excluded.last_limit_at,
                   quota_reset_at = COALESCE(excluded.quota_reset_at, provider_state.quota_reset_at)"#,
            params![provider, recorded, reset_at],
        )?;
        Ok(())
    }

    pub fn upsert_provider_usage(
        &self,
        provider: &str,
        window: &str,
        utilization: Option<f64>,
        balance: Option<f64>,
        credit_usage_percent: Option<f64>,
        resets_at: Option<&str>,
        status: Option<&str>,
        updated_at: &str,
    ) -> Result<()> {
        let window = canonical_usage_window(window);
        let conn = self.lock();
        conn.execute(
            r#"INSERT INTO provider_usage
               (provider, window, utilization, balance, credit_usage_percent, resets_at, status, updated_at)
               VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
               ON CONFLICT(provider, window) DO UPDATE SET
                   utilization = excluded.utilization,
                   balance = excluded.balance,
                   credit_usage_percent = excluded.credit_usage_percent,
                   resets_at = excluded.resets_at,
                   status = excluded.status,
                   updated_at = excluded.updated_at"#,
            params![
                provider,
                window,
                utilization,
                balance,
                credit_usage_percent,
                resets_at,
                status,
                updated_at
            ],
        )?;
        Ok(())
    }

    pub fn list_provider_usage(&self, provider: &str) -> Result<Vec<ProviderUsageRow>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            r#"SELECT provider, window, utilization, balance, credit_usage_percent, resets_at, status, updated_at
               FROM provider_usage WHERE provider = ?1 ORDER BY window"#,
        )?;
        let rows = stmt.query_map(params![provider], |r| {
            Ok(ProviderUsageRow {
                provider: r.get(0)?,
                window: r.get(1)?,
                utilization: r.get(2)?,
                balance: r.get(3)?,
                credit_usage_percent: r.get(4)?,
                resets_at: r.get(5)?,
                status: r.get(6)?,
                updated_at: r.get(7)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn provider_usage_updated(&self, provider: &str) -> Result<Option<String>> {
        let conn = self.lock();
        let v = conn
            .query_row(
                "SELECT MAX(updated_at) FROM provider_usage WHERE provider = ?1",
                params![provider],
                |r| r.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten();
        Ok(v)
    }

    pub fn quota_reset_at(&self, name: &str) -> Result<Option<String>> {
        let conn = self.lock();
        let v = conn
            .query_row(
                "SELECT quota_reset_at FROM provider_state WHERE name = ?1",
                params![name],
                |r| r.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten();
        Ok(v)
    }

    pub fn last_limit_at(&self, name: &str) -> Result<Option<String>> {
        let conn = self.lock();
        let v = conn
            .query_row(
                "SELECT last_limit_at FROM provider_state WHERE name = ?1",
                params![name],
                |r| r.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten();
        Ok(v)
    }

    pub fn runnable_keys(&self) -> Result<Vec<String>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            r#"SELECT DISTINCT t.key FROM tasks t
               WHERE t.status IN ('queued', 'waiting')
                 AND EXISTS (
                   SELECT 1 FROM messages m
                   WHERE m.task_key = t.key AND m.consumed_by_run IS NULL
                 )
               ORDER BY t.updated ASC"#,
        )?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// Unfinished run row — the live CLI (or its post-processing) still owns the task.
    pub fn open_run_for_task(&self, key: &str) -> Result<Option<i64>> {
        let conn = self.lock();
        let id = conn
            .query_row(
                "SELECT id FROM runs WHERE task_key = ?1 AND outcome IS NULL ORDER BY id DESC LIMIT 1",
                params![key],
                |r| r.get::<_, i64>(0),
            )
            .optional()?;
        Ok(id)
    }

    pub fn task_is_running(&self, key: &str) -> Result<bool> {
        let conn = self.lock();
        let status: Option<String> = conn
            .query_row(
                "SELECT status FROM tasks WHERE key = ?1",
                params![key],
                |r| r.get(0),
            )
            .optional()?;
        Ok(matches!(status.as_deref(), Some("running")))
    }

    pub fn first_run_started(&self, key: &str) -> Result<Option<String>> {
        let conn = self.lock();
        let v = conn
            .query_row(
                "SELECT MIN(started) FROM runs WHERE task_key = ?1",
                params![key],
                |r| r.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten();
        Ok(v)
    }

    pub fn set_cooldown(&self, name: &str, until: &str) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            r#"INSERT INTO provider_state (name, cooldown_until, degraded)
               VALUES (?1, ?2, 0)
               ON CONFLICT(name) DO UPDATE SET cooldown_until = excluded.cooldown_until"#,
            params![name, until],
        )?;
        Ok(())
    }

    pub fn set_degraded_until(&self, name: &str, until: Option<&str>) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            r#"INSERT INTO provider_state (name, degraded, degraded_until)
               VALUES (?1, ?2, ?3)
               ON CONFLICT(name) DO UPDATE SET
                 degraded = excluded.degraded,
                 degraded_until = excluded.degraded_until"#,
            params![name, i64::from(until.is_some()), until],
        )?;
        Ok(())
    }

    pub fn run_outcome(&self, run_id: i64) -> Result<Option<String>> {
        let conn = self.lock();
        let v = conn
            .query_row(
                "SELECT outcome FROM runs WHERE id = ?1",
                params![run_id],
                |r| r.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten();
        Ok(v)
    }

    pub fn finished_run_count(&self, key: &str) -> Result<i64> {
        let conn = self.lock();
        let n: i64 = conn.query_row(
            "SELECT COUNT(*) FROM runs WHERE task_key = ?1 AND ended IS NOT NULL",
            params![key],
            |r| r.get(0),
        )?;
        Ok(n)
    }

    /// Successful finished runs for this task+session. Used so a session's
    /// cold first run (including after stalls) does not trip the low-cache warn.
    pub fn successful_session_runs(&self, key: &str, session: &str) -> Result<i64> {
        let conn = self.lock();
        let n: i64 = conn.query_row(
            r#"SELECT COUNT(*) FROM runs
               WHERE task_key = ?1 AND session_id = ?2
                 AND ended IS NOT NULL AND outcome = 'success'"#,
            params![key, session],
            |r| r.get(0),
        )?;
        Ok(n)
    }

    pub fn provider_blocked(&self, name: &str) -> Result<(bool, bool)> {
        let conn = self.lock();
        let row = conn
            .query_row(
                "SELECT cooldown_until, degraded, degraded_until FROM provider_state WHERE name = ?1",
                params![name],
                |r| {
                    Ok((
                        r.get::<_, Option<String>>(0)?,
                        r.get::<_, i64>(1)? != 0,
                        r.get::<_, Option<String>>(2)?,
                    ))
                },
            )
            .optional()?;
        let Some((until, degraded_flag, degraded_until)) = row else {
            return Ok((false, false));
        };
        let cooling = future_ts(until.as_deref());
        let degraded = future_ts(degraded_until.as_deref());
        let stale_flag = degraded_flag && !degraded;
        drop(conn);
        if stale_flag {
            // Window expired, or leftover degraded=1 with a NULL until.
            let _ = self.set_degraded_until(name, None);
        }
        Ok((cooling, degraded))
    }

    pub fn cooldown_until(&self, name: &str) -> Result<Option<String>> {
        let conn = self.lock();
        let v = conn
            .query_row(
                "SELECT cooldown_until FROM provider_state WHERE name = ?1",
                params![name],
                |r| r.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten();
        Ok(v)
    }

    pub fn run_by_id(&self, run_id: i64) -> Result<Option<RunRow>> {
        let conn = self.lock();
        let v = conn
            .query_row(
                r#"SELECT id, task_key, provider, model, pid, started, ended, outcome,
                          last_event_at, exit_code, session_id, handoff, prefix_hash
                   FROM runs WHERE id = ?1"#,
                params![run_id],
                row_to_run,
            )
            .optional()?;
        Ok(v)
    }

    pub fn insert_shadow_result(&self, r: &ShadowResult) -> Result<i64> {
        let conn = self.lock();
        conn.execute(
            r#"INSERT INTO shadow_results (
                   task_key, run_id, provider, outcome, duration_ms, input, output,
                   cache_read, cache_write, cost_usd, cache_hit, tests, diff, diffstat,
                   verify, recorded
               ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)"#,
            params![
                r.task_key,
                r.run_id,
                r.provider,
                r.outcome,
                r.duration_ms,
                r.input,
                r.output,
                r.cache_read,
                r.cache_write,
                r.cost_usd,
                r.cache_hit,
                r.tests,
                r.diff,
                r.diffstat,
                r.verify,
                r.recorded
            ],
        )?;
        Ok(conn.last_insert_rowid())
    }

    pub fn list_shadow_results(&self, since: Option<&str>) -> Result<Vec<ShadowResult>> {
        let conn = self.lock();
        let sql = r#"SELECT task_key, run_id, provider, outcome, duration_ms, input, output,
                            cache_read, cache_write, cost_usd, cache_hit, tests, diff,
                            COALESCE(diffstat, ''), COALESCE(verify, ''), recorded
                     FROM shadow_results
                     WHERE (?1 IS NULL OR recorded >= ?1)
                     ORDER BY id"#;
        let mut stmt = conn.prepare(sql)?;
        let rows = stmt.query_map(params![since], |r| {
            Ok(ShadowResult {
                task_key: r.get(0)?,
                run_id: r.get(1)?,
                provider: r.get(2)?,
                outcome: r.get(3)?,
                duration_ms: r.get(4)?,
                input: r.get(5)?,
                output: r.get(6)?,
                cache_read: r.get(7)?,
                cache_write: r.get(8)?,
                cost_usd: r.get(9)?,
                cache_hit: r.get(10)?,
                tests: r.get(11)?,
                diff: r.get(12)?,
                diffstat: r.get(13)?,
                verify: r.get(14)?,
                recorded: r.get(15)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn insert_verify_check(&self, run_id: i64, key: &str, c: &VerifyCheck) -> Result<i64> {
        let conn = self.lock();
        conn.execute(
            r#"INSERT INTO verify_results (
                   run_id, task_key, command, passed, exit_code, duration_ms, output, recorded
               ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)"#,
            params![
                run_id,
                key,
                c.command,
                if c.passed { 1 } else { 0 },
                c.exit_code,
                c.duration_ms,
                c.output,
                c.recorded
            ],
        )?;
        Ok(conn.last_insert_rowid())
    }

    pub fn verify_checks_for_run(&self, run_id: i64) -> Result<Vec<VerifyCheck>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            r#"SELECT command, passed, exit_code, duration_ms, output, recorded
               FROM verify_results WHERE run_id = ?1 ORDER BY id"#,
        )?;
        let rows = stmt.query_map(params![run_id], |r| {
            Ok(VerifyCheck {
                command: r.get(0)?,
                passed: r.get::<_, i64>(1)? != 0,
                exit_code: r.get(2)?,
                duration_ms: r.get(3)?,
                output: r.get(4)?,
                recorded: r.get(5)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn verify_summary_for_run(&self, run_id: i64) -> Result<String> {
        Ok(crate::verify::format_checks(
            &self.verify_checks_for_run(run_id)?,
        ))
    }

    pub fn usage_by_day(&self, days: u32) -> Result<Vec<UsageDay>> {
        self.usage_by_day_tz(days, &crate::tz::DisplayTz::system())
    }

    pub fn usage_by_day_tz(&self, days: u32, tz: &crate::tz::DisplayTz) -> Result<Vec<UsageDay>> {
        let since = (Utc::now() - ChronoDuration::days(i64::from(days)))
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        let conn = self.lock();
        let mut stmt = conn.prepare(
            r#"SELECT provider, recorded, input, output, cache_read, cache_write,
                      COALESCE(cost_usd, 0)
               FROM usage WHERE recorded >= ?1"#,
        )?;
        let rows = stmt.query_map(params![since], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, i64>(4)?,
                r.get::<_, i64>(5)?,
                r.get::<_, f64>(6)?,
            ))
        })?;
        let mut acc: std::collections::BTreeMap<(std::cmp::Reverse<String>, String), UsageDay> =
            std::collections::BTreeMap::new();
        for row in rows {
            let (provider, recorded, input, output, cr, cw, cost) = row?;
            let day = crate::tz::DisplayTz::parse_utc(&recorded)
                .map(|t| tz.day(t))
                .unwrap_or_else(|| recorded.chars().take(10).collect());
            let e = acc
                .entry((std::cmp::Reverse(day.clone()), provider.clone()))
                .or_insert(UsageDay {
                    provider: provider.clone(),
                    day,
                    input: 0,
                    output: 0,
                    cache_read: 0,
                    cache_write: 0,
                    cost_usd: 0.0,
                    runs: 0,
                });
            e.input += input;
            e.output += output;
            e.cache_read += cr;
            e.cache_write += cw;
            e.cost_usd += cost;
            e.runs += 1;
        }
        Ok(acc.into_values().collect())
    }

    /// Calendar-day aggregates in `tz` for `from_day`..=`to_day` (YYYY-MM-DD, inclusive).
    pub fn usage_by_day_range(
        &self,
        from_day: &str,
        to_day: &str,
        tz: &crate::tz::DisplayTz,
    ) -> Result<Vec<UsageDay>> {
        self.usage_by_day_range_filtered(from_day, to_day, tz, None)
    }

    /// Same as [`Self::usage_by_day_range`], optionally keeping only rows whose
    /// task.repo matches one of `repo_paths` (exact path or trailing name).
    pub fn usage_by_day_range_filtered(
        &self,
        from_day: &str,
        to_day: &str,
        tz: &crate::tz::DisplayTz,
        repo_paths: Option<&[String]>,
    ) -> Result<Vec<UsageDay>> {
        let since = format!("{from_day}T00:00:00Z");
        let until = format!("{to_day}T00:00:00Z");
        let since = crate::tz::DisplayTz::parse_utc(&since)
            .map(|t| t - ChronoDuration::days(1))
            .map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
            .unwrap_or(since);
        let until = crate::tz::DisplayTz::parse_utc(&until)
            .map(|t| t + ChronoDuration::days(2))
            .map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
            .unwrap_or(until);
        let conn = self.lock();
        let mut stmt = conn.prepare(
            r#"SELECT u.provider, u.recorded, u.input, u.output, u.cache_read, u.cache_write,
                      COALESCE(u.cost_usd, 0), t.repo
               FROM usage u
               LEFT JOIN runs r ON r.id = u.run_id
               LEFT JOIN tasks t ON t.key = r.task_key
               WHERE u.recorded >= ?1 AND u.recorded < ?2"#,
        )?;
        let rows = stmt.query_map(params![since, until], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, i64>(4)?,
                r.get::<_, i64>(5)?,
                r.get::<_, f64>(6)?,
                r.get::<_, Option<String>>(7)?,
            ))
        })?;
        let mut acc: std::collections::BTreeMap<(std::cmp::Reverse<String>, String), UsageDay> =
            std::collections::BTreeMap::new();
        for row in rows {
            let (provider, recorded, input, output, cr, cw, cost, task_repo) = row?;
            if let Some(want) = repo_paths {
                let keep = task_repo.as_deref().is_some_and(|p| {
                    want.iter().any(|w| {
                        p == w
                            || std::path::Path::new(p).file_name()
                                == std::path::Path::new(w).file_name()
                    })
                });
                if !keep {
                    continue;
                }
            }
            let day = crate::tz::DisplayTz::parse_utc(&recorded)
                .map(|t| tz.day(t))
                .unwrap_or_else(|| recorded.chars().take(10).collect());
            if day.as_str() < from_day || day.as_str() > to_day {
                continue;
            }
            let e = acc
                .entry((std::cmp::Reverse(day.clone()), provider.clone()))
                .or_insert(UsageDay {
                    provider: provider.clone(),
                    day,
                    input: 0,
                    output: 0,
                    cache_read: 0,
                    cache_write: 0,
                    cost_usd: 0.0,
                    runs: 0,
                });
            e.input += input;
            e.output += output;
            e.cache_read += cr;
            e.cache_write += cw;
            e.cost_usd += cost;
            e.runs += 1;
        }
        Ok(acc.into_values().collect())
    }

    pub fn usage_detail_range(
        &self,
        from_day: &str,
        to_day: &str,
        tz: &crate::tz::DisplayTz,
        repo_paths: Option<&[String]>,
    ) -> Result<Vec<UsageDetail>> {
        let since = format!("{from_day}T00:00:00Z");
        let until = format!("{to_day}T00:00:00Z");
        let since = crate::tz::DisplayTz::parse_utc(&since)
            .map(|t| t - ChronoDuration::days(1))
            .map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
            .unwrap_or(since);
        let until = crate::tz::DisplayTz::parse_utc(&until)
            .map(|t| t + ChronoDuration::days(2))
            .map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
            .unwrap_or(until);
        let conn = self.lock();
        let mut stmt = conn.prepare(
            r#"SELECT u.provider, u.recorded, u.input, u.output, u.cache_read, u.cache_write,
                      COALESCE(u.cost_usd, 0), COALESCE(u.is_resume, 0), t.repo
               FROM usage u
               LEFT JOIN runs r ON r.id = u.run_id
               LEFT JOIN tasks t ON t.key = r.task_key
               WHERE u.recorded >= ?1 AND u.recorded < ?2"#,
        )?;
        let rows = stmt.query_map(params![since, until], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, i64>(4)?,
                r.get::<_, i64>(5)?,
                r.get::<_, f64>(6)?,
                r.get::<_, i64>(7)?,
                r.get::<_, Option<String>>(8)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (provider, recorded, input, output, cr, cw, cost, resume, task_repo) = row?;
            if let Some(want) = repo_paths {
                let keep = task_repo.as_deref().is_some_and(|p| {
                    want.iter().any(|w| {
                        p == w
                            || std::path::Path::new(p).file_name()
                                == std::path::Path::new(w).file_name()
                    })
                });
                if !keep {
                    continue;
                }
            }
            let day = crate::tz::DisplayTz::parse_utc(&recorded)
                .map(|t| tz.day(t))
                .unwrap_or_else(|| recorded.chars().take(10).collect());
            if day.as_str() < from_day || day.as_str() > to_day {
                continue;
            }
            out.push(UsageDetail {
                provider,
                input,
                output,
                cache_read: cr,
                cache_write: cw,
                cost_usd: cost,
                is_resume: resume != 0,
            });
        }
        Ok(out)
    }

    pub fn meta_get(&self, key: &str) -> Result<Option<String>> {
        let conn = self.lock();
        let v = conn
            .query_row("SELECT value FROM meta WHERE key = ?1", params![key], |r| {
                r.get::<_, String>(0)
            })
            .optional()?;
        Ok(v)
    }

    pub fn meta_set(&self, key: &str, value: &str) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            r#"INSERT INTO meta (key, value) VALUES (?1, ?2)
               ON CONFLICT(key) DO UPDATE SET value = excluded.value"#,
            params![key, value],
        )?;
        Ok(())
    }

    pub fn enabled_override(&self, name: &str) -> Result<Option<bool>> {
        let conn = self.lock();
        let v = conn
            .query_row(
                "SELECT enabled_override FROM provider_state WHERE name = ?1",
                params![name],
                |r| r.get::<_, Option<i64>>(0),
            )
            .optional()?
            .flatten();
        Ok(v.map(|n| n != 0))
    }

    pub fn set_enabled_override(&self, name: &str, enabled: bool) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            r#"INSERT INTO provider_state (name, cooldown_until, degraded, enabled_override)
               VALUES (?1, NULL, 0, ?2)
               ON CONFLICT(name) DO UPDATE SET enabled_override = excluded.enabled_override"#,
            params![name, if enabled { 1 } else { 0 }],
        )?;
        Ok(())
    }

    pub fn runs_on_day(&self, tz: &crate::tz::DisplayTz, day: &str) -> Result<Vec<RunRow>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            r#"SELECT id, task_key, provider, model, pid, started, ended, outcome,
                      last_event_at, exit_code, session_id, handoff, prefix_hash
               FROM runs WHERE ended IS NOT NULL"#,
        )?;
        let rows = stmt.query_map([], row_to_run)?;
        let mut out = Vec::new();
        for r in rows {
            let r = r?;
            if let Some(t) = crate::tz::DisplayTz::parse_utc(&r.started) {
                if tz.day(t) == day {
                    out.push(r);
                }
            }
        }
        Ok(out)
    }

    pub fn usage_totals(&self, days: u32) -> Result<Vec<UsageDay>> {
        let since = (Utc::now() - ChronoDuration::days(i64::from(days)))
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        let conn = self.lock();
        let mut stmt = conn.prepare(
            r#"SELECT provider, 'all',
                      SUM(input), SUM(output), SUM(cache_read), SUM(cache_write),
                      COALESCE(SUM(cost_usd), 0), COUNT(*)
               FROM usage
               WHERE recorded >= ?1
               GROUP BY provider
               ORDER BY provider"#,
        )?;
        let rows = stmt.query_map(params![since], |r| {
            Ok(UsageDay {
                provider: r.get(0)?,
                day: r.get(1)?,
                input: r.get(2)?,
                output: r.get(3)?,
                cache_read: r.get(4)?,
                cache_write: r.get(5)?,
                cost_usd: r.get(6)?,
                runs: r.get(7)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }
}

fn row_to_task(r: &rusqlite::Row<'_>) -> rusqlite::Result<Task> {
    Ok(Task {
        key: r.get(0)?,
        repo: r.get(1)?,
        issue: r.get(2)?,
        status: r.get(3)?,
        provider: r.get(4)?,
        session_id: r.get(5)?,
        worktree: r.get(6)?,
        stall_retries: r.get(7)?,
        handoff: r.get(8)?,
        tried: r.get(9)?,
        created: r.get(10)?,
        updated: r.get(11)?,
        shadow: r.get::<_, i64>(12).unwrap_or(0) != 0,
        verify_retries: r.get::<_, i64>(13).unwrap_or(0),
        protect_tests_skip: r.get::<_, i64>(14).unwrap_or(0) != 0,
        labels: r.get::<_, Option<String>>(15)?.unwrap_or_default(),
        parent_key: r.get::<_, Option<String>>(16).ok().flatten(),
        role: r
            .get::<_, Option<String>>(17)?
            .unwrap_or_else(|| "worker".into()),
        deps: r.get::<_, Option<String>>(18)?.unwrap_or_default(),
        review_rounds: r.get::<_, i64>(19).unwrap_or(0),
        provider_hint: r.get::<_, Option<String>>(20)?.unwrap_or_default(),
        title: r.get::<_, Option<String>>(21)?.unwrap_or_default(),
        workspace_kind: r.get::<_, Option<String>>(22)?.unwrap_or_default(),
        output_kind: r.get::<_, Option<String>>(23)?.unwrap_or_default(),
        output_spec: r.get::<_, Option<String>>(24)?.unwrap_or_default(),
        source_kind: r.get::<_, Option<String>>(25)?.unwrap_or_default(),
        result_text: r.get::<_, Option<String>>(26)?.unwrap_or_default(),
        result_diff: r.get::<_, Option<String>>(27)?.unwrap_or_default(),
        result_pr_url: r.get::<_, Option<String>>(28)?.unwrap_or_default(),
        needs: r.get::<_, Option<String>>(29)?.unwrap_or_default(),
        pin_strict: r.get::<_, Option<i64>>(30)?.unwrap_or(0) != 0,
        compare_decision: r.get::<_, Option<String>>(31)?.unwrap_or_default(),
    })
}

#[derive(Debug, Clone)]
pub struct ShadowResult {
    pub task_key: String,
    pub run_id: Option<i64>,
    pub provider: String,
    pub outcome: String,
    pub duration_ms: i64,
    pub input: i64,
    pub output: i64,
    pub cache_read: i64,
    pub cache_write: i64,
    pub cost_usd: f64,
    pub cache_hit: Option<f64>,
    pub tests: String,
    pub diff: String,
    pub diffstat: String,
    pub verify: String,
    pub recorded: String,
}

#[derive(Debug, Clone)]
pub struct VerifyCheck {
    pub command: String,
    pub passed: bool,
    pub exit_code: i32,
    pub duration_ms: i64,
    pub output: String,
    pub recorded: String,
}

/// Merge `usage_period_type_*` rows onto their canonical window. Prefer a
/// row that already has utilization, then the latest `updated_at`.
fn dedupe_provider_usage_windows(conn: &Connection) -> Result<()> {
    let mut stmt = conn.prepare(
        r#"SELECT provider, window, utilization, balance, credit_usage_percent,
                  resets_at, status, updated_at
           FROM provider_usage"#,
    )?;
    let rows = stmt
        .query_map([], |r| {
            Ok(ProviderUsageRow {
                provider: r.get(0)?,
                window: r.get(1)?,
                utilization: r.get(2)?,
                balance: r.get(3)?,
                credit_usage_percent: r.get(4)?,
                resets_at: r.get(5)?,
                status: r.get(6)?,
                updated_at: r.get(7)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    drop(stmt);

    let mut groups: HashMap<(String, String), Vec<ProviderUsageRow>> = HashMap::new();
    for r in rows {
        let canon = canonical_usage_window(&r.window);
        groups
            .entry((r.provider.clone(), canon))
            .or_default()
            .push(r);
    }
    for ((provider, canon), mut members) in groups {
        if members.len() == 1 && members[0].window == canon {
            continue;
        }
        members.sort_by(|a, b| {
            a.utilization
                .is_some()
                .cmp(&b.utilization.is_some())
                .then_with(|| a.updated_at.cmp(&b.updated_at))
        });
        let winner = members.pop().expect("group is non-empty");
        for loser in &members {
            conn.execute(
                "DELETE FROM provider_usage WHERE provider = ?1 AND window = ?2",
                params![loser.provider, loser.window],
            )?;
        }
        if winner.window != canon {
            conn.execute(
                "UPDATE provider_usage SET window = ?1 WHERE provider = ?2 AND window = ?3",
                params![canon, provider, winner.window],
            )?;
        }
    }
    Ok(())
}

/// Old grok usage rows sometimes stored the dollar figure only in `raw_json`
/// (progress last-wins or a backfill that saw `cost_usd` NULL).
fn recover_grok_cost_from_raw(conn: &Connection) -> Result<()> {
    let mut stmt = conn.prepare(
        r#"SELECT id, raw_json FROM usage
           WHERE lower(provider) = 'grok'
             AND cost_usd IS NULL"#,
    )?;
    let rows = stmt
        .query_map([], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, Option<String>>(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    drop(stmt);
    for (id, raw) in rows {
        let Some(raw) = raw.filter(|s| !s.is_empty()) else {
            continue;
        };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) else {
            continue;
        };
        if let Some(cost) = crate::providers::extract_usage("grok", &v).cost_usd {
            conn.execute(
                "UPDATE usage SET cost_usd = ?1, cost_kind = 'est' WHERE id = ?2",
                params![cost, id],
            )?;
        }
    }
    Ok(())
}

fn row_to_run(r: &rusqlite::Row<'_>) -> rusqlite::Result<RunRow> {
    Ok(RunRow {
        id: r.get(0)?,
        task_key: r.get(1)?,
        provider: r.get(2)?,
        model: r.get(3)?,
        pid: r.get(4)?,
        started: r.get(5)?,
        ended: r.get(6)?,
        outcome: r.get(7)?,
        last_event_at: r.get(8)?,
        exit_code: r.get(9)?,
        session_id: r.get(10)?,
        handoff: r.get(11)?,
        prefix_hash: r.get::<_, Option<String>>(12)?,
    })
}

fn row_to_event(r: &rusqlite::Row<'_>) -> rusqlite::Result<EventRow> {
    Ok(EventRow {
        run_id: r.get(0)?,
        kind: r.get(1)?,
        raw: r.get(2)?,
    })
}

pub fn pid_alive(pid: i64) -> bool {
    if pid <= 0 {
        return false;
    }
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

pub fn cooldown_rfc3339(d: std::time::Duration) -> String {
    (Utc::now() + ChronoDuration::from_std(d).unwrap_or(ChronoDuration::minutes(30)))
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

fn future_ts(s: Option<&str>) -> bool {
    ts_in_future(s, Utc::now())
}

pub fn ts_in_future(s: Option<&str>, now: chrono::DateTime<Utc>) -> bool {
    s.and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|t| t.with_timezone(&Utc) > now)
        .unwrap_or(false)
}

/// One stored lesson: a short, evidence-backed note for a repo. `task_key`,
/// `run_id` and `signal` come from atr's own records (or `manual`).
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Lesson {
    pub id: i64,
    pub repo: String,
    pub text: String,
    pub task_key: String,
    pub run_id: Option<i64>,
    pub signal: String,
    pub created: String,
    pub last_hit: Option<String>,
    pub hits: i64,
    pub pinned: bool,
    pub shadow: bool,
}

/// Fields for a new lesson.
#[derive(Debug, Clone, Default)]
pub struct NewLesson<'a> {
    pub repo: &'a str,
    pub text: &'a str,
    pub task_key: &'a str,
    pub run_id: Option<i64>,
    pub signal: &'a str,
    pub shadow: bool,
}

const LESSON_COLS: &str =
    "id, repo, text, task_key, run_id, signal, created, last_hit, hits, pinned, shadow";

fn lesson_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Lesson> {
    Ok(Lesson {
        id: r.get(0)?,
        repo: r.get(1)?,
        text: r.get(2)?,
        task_key: r.get(3)?,
        run_id: r.get(4)?,
        signal: r.get(5)?,
        created: r.get(6)?,
        last_hit: r.get(7)?,
        hits: r.get(8)?,
        pinned: r.get::<_, i64>(9)? != 0,
        shadow: r.get::<_, i64>(10)? != 0,
    })
}

impl Db {
    /// Insert a lesson, then prune the repo to `max_entries` (oldest unpinned
    /// by `max(created, last_hit)` first). Returns the new id.
    pub fn add_lesson(&self, l: &NewLesson<'_>, max_entries: usize) -> Result<i64> {
        let conn = self.lock();
        conn.execute(
            r#"INSERT INTO lessons (repo, text, task_key, run_id, signal, created, shadow)
               VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)"#,
            params![
                l.repo,
                l.text,
                l.task_key,
                l.run_id,
                l.signal,
                now_rfc3339(),
                l.shadow as i64
            ],
        )?;
        let id = conn.last_insert_rowid();
        prune_lessons_locked(&conn, l.repo, max_entries, Some(id))?;
        Ok(id)
    }

    /// Lessons for one repo (or all), pinned first, then newest first.
    pub fn list_lessons(&self, repo: Option<&str>) -> Result<Vec<Lesson>> {
        let conn = self.lock();
        let order =
            "ORDER BY repo, pinned DESC, MAX(created, COALESCE(last_hit, '')) DESC, id DESC";
        let rows = match repo {
            Some(r) => {
                let mut st = conn.prepare(&format!(
                    "SELECT {LESSON_COLS} FROM lessons WHERE repo = ?1 {order}"
                ))?;
                let v = st
                    .query_map(params![r], lesson_from_row)?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                v
            }
            None => {
                let mut st = conn.prepare(&format!("SELECT {LESSON_COLS} FROM lessons {order}"))?;
                let v = st
                    .query_map([], lesson_from_row)?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                v
            }
        };
        Ok(rows)
    }

    pub fn get_lesson(&self, id: i64) -> Result<Option<Lesson>> {
        let conn = self.lock();
        let mut st = conn.prepare(&format!("SELECT {LESSON_COLS} FROM lessons WHERE id = ?1"))?;
        let mut rows = st.query_map(params![id], lesson_from_row)?;
        Ok(rows.next().transpose()?)
    }

    /// Delete one lesson (`id`) or every lesson of `repo`. Returns rows removed.
    pub fn clear_lessons(&self, repo: &str, id: Option<i64>) -> Result<usize> {
        let conn = self.lock();
        let n = match id {
            Some(id) => conn.execute(
                "DELETE FROM lessons WHERE repo = ?1 AND id = ?2",
                params![repo, id],
            )?,
            None => conn.execute("DELETE FROM lessons WHERE repo = ?1", params![repo])?,
        };
        Ok(n)
    }

    /// Pin or unpin. Returns false when the id does not exist.
    pub fn set_lesson_pinned(&self, id: i64, pinned: bool) -> Result<bool> {
        let conn = self.lock();
        let n = conn.execute(
            "UPDATE lessons SET pinned = ?1 WHERE id = ?2",
            params![pinned as i64, id],
        )?;
        Ok(n > 0)
    }

    /// Mark lessons as injected into a run: bump hits/last_hit and record the
    /// ids on the run (`runs.lessons`, comma-separated).
    pub fn record_lessons_injected(&self, run_id: i64, ids: &[i64]) -> Result<()> {
        if ids.is_empty() {
            return Ok(());
        }
        let conn = self.lock();
        let now = now_rfc3339();
        for id in ids {
            conn.execute(
                "UPDATE lessons SET hits = hits + 1, last_hit = ?1 WHERE id = ?2",
                params![now, id],
            )?;
        }
        let list = ids
            .iter()
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join(",");
        conn.execute(
            "UPDATE runs SET lessons = ?1 WHERE id = ?2",
            params![list, run_id],
        )?;
        Ok(())
    }

    /// Lesson ids injected into a run.
    pub fn run_lessons(&self, run_id: i64) -> Result<Vec<i64>> {
        let conn = self.lock();
        let v: Option<String> = conn
            .query_row(
                "SELECT lessons FROM runs WHERE id = ?1",
                params![run_id],
                |r| r.get(0),
            )
            .optional()?
            .flatten();
        Ok(v.unwrap_or_default()
            .split(',')
            .filter_map(|s| s.trim().parse().ok())
            .collect())
    }

    /// A near-duplicate lesson came up again: count it as a hit (refreshes ttl).
    pub fn bump_lesson(&self, id: i64) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "UPDATE lessons SET hits = hits + 1, last_hit = ?1 WHERE id = ?2",
            params![now_rfc3339(), id],
        )?;
        Ok(())
    }

    /// Drop unpinned lessons whose last activity (`max(created, last_hit)`)
    /// is before `cutoff` (RFC3339). Returns rows removed.
    pub fn expire_lessons(&self, cutoff: &str) -> Result<usize> {
        let conn = self.lock();
        let n = conn.execute(
            "DELETE FROM lessons WHERE pinned = 0 AND MAX(created, COALESCE(last_hit, '')) < ?1",
            params![cutoff],
        )?;
        Ok(n)
    }

    /// Per-repo lesson counts (for status/dashboard; never the text).
    pub fn lesson_counts(&self) -> Result<Vec<(String, i64)>> {
        let conn = self.lock();
        let mut st =
            conn.prepare("SELECT repo, COUNT(*) FROM lessons GROUP BY repo ORDER BY repo")?;
        let v = st
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(v)
    }
}

fn prune_lessons_locked(
    conn: &Connection,
    repo: &str,
    max_entries: usize,
    keep: Option<i64>,
) -> Result<()> {
    let total: i64 = conn.query_row(
        "SELECT COUNT(*) FROM lessons WHERE repo = ?1",
        params![repo],
        |r| r.get(0),
    )?;
    let over = total - max_entries.max(1) as i64;
    if over <= 0 {
        return Ok(());
    }
    conn.execute(
        r#"DELETE FROM lessons WHERE id IN (
               SELECT id FROM lessons
               WHERE repo = ?1 AND pinned = 0 AND id != ?3
               ORDER BY MAX(created, COALESCE(last_hit, '')) ASC, id ASC
               LIMIT ?2)"#,
        params![repo, over, keep.unwrap_or(-1)],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn queue_unless_running_never_overwrites_running() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("s.db")).unwrap();
        db.upsert_task("q-1", "/tmp/r", None).unwrap();
        assert!(db.claim_task("q-1").unwrap());
        assert!(!db.queue_unless_running("q-1").unwrap());
        assert_eq!(db.get_task("q-1").unwrap().unwrap().status, "running");
        db.set_status("q-1", "waiting").unwrap();
        assert!(db.queue_unless_running("q-1").unwrap());
        assert_eq!(db.get_task("q-1").unwrap().unwrap().status, "queued");
    }

    use super::*;

    fn new_lesson<'a>(repo: &'a str, text: &'a str) -> NewLesson<'a> {
        NewLesson {
            repo,
            text,
            signal: "manual",
            ..Default::default()
        }
    }

    #[test]
    fn lessons_prune_oldest_unpinned_and_keep_pinned() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("s.db")).unwrap();
        let a = db.add_lesson(&new_lesson("app", "a"), 3).unwrap();
        let b = db.add_lesson(&new_lesson("app", "b"), 3).unwrap();
        let c = db.add_lesson(&new_lesson("app", "c"), 3).unwrap();
        db.add_lesson(&new_lesson("other", "x"), 3).unwrap();
        assert!(db.set_lesson_pinned(a, true).unwrap());
        // b was hit recently, so c (older activity) goes first once over cap.
        {
            let conn = db.lock();
            conn.execute(
                "UPDATE lessons SET created = '2026-01-01T00:00:00Z' WHERE repo = 'app'",
                [],
            )
            .unwrap();
            conn.execute(
                "UPDATE lessons SET last_hit = '2026-06-01T00:00:00Z' WHERE id = ?1",
                params![b],
            )
            .unwrap();
        }
        let d = db.add_lesson(&new_lesson("app", "d"), 3).unwrap();
        let ids: Vec<i64> = db
            .list_lessons(Some("app"))
            .unwrap()
            .iter()
            .map(|l| l.id)
            .collect();
        assert_eq!(ids.len(), 3);
        assert!(ids.contains(&a), "pinned kept");
        assert!(ids.contains(&b), "recently hit kept");
        assert!(ids.contains(&d), "new one kept");
        assert!(!ids.contains(&c), "oldest unpinned pruned");
        assert_eq!(ids[0], a, "pinned listed first");
        assert_eq!(db.list_lessons(Some("other")).unwrap().len(), 1);
        assert_eq!(db.list_lessons(None).unwrap().len(), 4);
        assert_eq!(
            db.lesson_counts().unwrap(),
            vec![("app".into(), 3), ("other".into(), 1)]
        );
    }

    #[test]
    fn lessons_clear_pin_and_run_ids() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("s.db")).unwrap();
        let a = db.add_lesson(&new_lesson("app", "a"), 20).unwrap();
        let b = db.add_lesson(&new_lesson("app", "b"), 20).unwrap();
        assert!(!db.set_lesson_pinned(999, true).unwrap());
        db.upsert_task("app-1", "", None).unwrap();
        let run = db.start_run("app-1", "claude", "").unwrap();
        db.record_lessons_injected(run, &[a, b]).unwrap();
        assert_eq!(db.run_lessons(run).unwrap(), vec![a, b]);
        assert_eq!(db.get_lesson(a).unwrap().unwrap().hits, 1);
        assert_eq!(db.clear_lessons("app", Some(a)).unwrap(), 1);
        assert_eq!(db.clear_lessons("other", None).unwrap(), 0);
        assert_eq!(db.clear_lessons("app", None).unwrap(), 1);
        assert!(db.list_lessons(Some("app")).unwrap().is_empty());
    }
    use chrono::Duration as ChronoDuration;

    #[test]
    fn degraded_flag_clears_when_window_expires() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("s.db")).unwrap();
        let past = (Utc::now() - ChronoDuration::minutes(1))
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        db.set_degraded_until("claude", Some(&past)).unwrap();
        let (cooling, degraded) = db.provider_blocked("claude").unwrap();
        assert!(!cooling);
        assert!(!degraded, "expired degraded_until must not block");
        let conn = db.lock();
        let (flag, until): (i64, Option<String>) = conn
            .query_row(
                "SELECT degraded, degraded_until FROM provider_state WHERE name = 'claude'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(flag, 0);
        assert!(until.is_none());
    }

    #[test]
    fn open_run_for_task_tracks_unfinished_only() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("s.db")).unwrap();
        db.upsert_task("acme-live", "/repo", Some(1)).unwrap();
        assert!(db.open_run_for_task("acme-live").unwrap().is_none());
        let id = db.start_run("acme-live", "claude", "m").unwrap();
        assert_eq!(db.open_run_for_task("acme-live").unwrap(), Some(id));
        db.finish_run(id, "success", Some(0)).unwrap();
        assert!(db.open_run_for_task("acme-live").unwrap().is_none());
    }

    #[test]
    fn leftover_degraded_flag_without_until_clears() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("s.db")).unwrap();
        {
            let conn = db.lock();
            conn.execute(
                "INSERT INTO provider_state (name, degraded, degraded_until) VALUES ('grok', 1, NULL)",
                [],
            )
            .unwrap();
        }
        let (_, degraded) = db.provider_blocked("grok").unwrap();
        assert!(!degraded);
        let conn = db.lock();
        let flag: i64 = conn
            .query_row(
                "SELECT degraded FROM provider_state WHERE name = 'grok'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(flag, 0);
    }

    #[test]
    fn successful_session_runs_ignore_stalls() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("s.db")).unwrap();
        db.upsert_task("acme-1", "/repo", Some(1)).unwrap();
        let stalled = db.start_run("acme-1", "grok", "m").unwrap();
        db.set_run_session(stalled, "sess-a").unwrap();
        db.finish_run(stalled, "stalled", None).unwrap();
        assert_eq!(db.finished_run_count("acme-1").unwrap(), 1);
        assert_eq!(db.successful_session_runs("acme-1", "sess-a").unwrap(), 0);
        let ok = db.start_run("acme-1", "grok", "m").unwrap();
        db.set_run_session(ok, "sess-a").unwrap();
        db.finish_run(ok, "success", Some(0)).unwrap();
        assert_eq!(db.successful_session_runs("acme-1", "sess-a").unwrap(), 1);
        assert_eq!(db.successful_session_runs("acme-1", "other").unwrap(), 0);
    }

    #[test]
    fn parent_child_task_links() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("s.db")).unwrap();
        db.upsert_task("p", "/repo", Some(1)).unwrap();
        db.set_task_role("p", "parent").unwrap();
        db.upsert_task("p-s1", "/repo", Some(1)).unwrap();
        db.set_parent_key("p-s1", "p").unwrap();
        db.set_task_role("p-s1", "worker").unwrap();
        db.set_task_title("p-s1", "one").unwrap();
        db.upsert_task("p-s2", "/repo", Some(1)).unwrap();
        db.set_parent_key("p-s2", "p").unwrap();
        db.set_task_deps("p-s2", "p-s1").unwrap();
        let kids = db.children_of("p").unwrap();
        assert_eq!(kids.len(), 2);
        let t = db.get_task("p-s1").unwrap().unwrap();
        assert_eq!(t.parent_key.as_deref(), Some("p"));
        assert_eq!(t.role, "worker");
        assert_eq!(t.title, "one");
    }

    #[test]
    fn task_sessions_are_isolated_per_provider() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("s.db")).unwrap();
        db.upsert_task("acme-2", "/repo", Some(2)).unwrap();
        db.set_provider_session("acme-2", Some("claude"), Some("claude-sid"))
            .unwrap();
        db.set_provider_session("acme-2", Some("grok"), Some("grok-sid"))
            .unwrap();
        assert_eq!(
            db.task_session("acme-2", "claude").unwrap().as_deref(),
            Some("claude-sid")
        );
        assert_eq!(
            db.task_session("acme-2", "grok").unwrap().as_deref(),
            Some("grok-sid")
        );
        // Failover clears the task pointer, not the per-provider map.
        db.set_provider_session("acme-2", None, None).unwrap();
        let task = db.get_task("acme-2").unwrap().unwrap();
        assert!(task.session_id.is_none());
        assert!(task.provider.is_none());
        assert_eq!(
            db.task_session("acme-2", "claude").unwrap().as_deref(),
            Some("claude-sid")
        );
        assert_eq!(
            db.task_session("acme-2", "grok").unwrap().as_deref(),
            Some("grok-sid")
        );
        // Forgetting one provider leaves the others.
        db.set_provider_session("acme-2", Some("claude"), None)
            .unwrap();
        assert!(db.task_session("acme-2", "claude").unwrap().is_none());
        assert_eq!(
            db.task_session("acme-2", "grok").unwrap().as_deref(),
            Some("grok-sid")
        );
    }

    #[test]
    fn usage_days_follow_display_timezone() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("s.db")).unwrap();
        db.upsert_task("acme-tz", "/repo", None).unwrap();
        let run = db.start_run("acme-tz", "claude", "m").unwrap();
        db.finish_run(run, "success", Some(0)).unwrap();
        let recorded = "2026-10-14T02:00:00Z";
        db.insert_usage_at(
            run,
            None,
            "claude",
            10,
            2,
            0,
            0,
            Some(1.0),
            Some("est"),
            "{}",
            recorded,
        )
        .unwrap();
        let utc = crate::tz::DisplayTz::parse("UTC").unwrap();
        let ny = crate::tz::DisplayTz::parse("America/New_York").unwrap();
        let utc_days = db.usage_by_day_tz(30, &utc).unwrap();
        let ny_days = db.usage_by_day_tz(30, &ny).unwrap();
        assert!(
            utc_days
                .iter()
                .any(|d| d.day == "2026-10-14" && d.provider == "claude"),
            "{utc_days:?}"
        );
        assert!(
            ny_days
                .iter()
                .any(|d| d.day == "2026-10-13" && d.provider == "claude"),
            "{ny_days:?}"
        );
        let range = db
            .usage_by_day_range("2026-10-14", "2026-10-14", &utc)
            .unwrap();
        assert!(
            range
                .iter()
                .any(|d| d.day == "2026-10-14" && d.provider == "claude" && d.input == 10),
            "{range:?}"
        );
        let empty = db
            .usage_by_day_range("2026-10-01", "2026-10-01", &utc)
            .unwrap();
        assert!(empty.is_empty(), "{empty:?}");
        let ny_range = db
            .usage_by_day_range("2026-10-13", "2026-10-13", &ny)
            .unwrap();
        assert!(
            ny_range
                .iter()
                .any(|d| d.day == "2026-10-13" && d.provider == "claude"),
            "{ny_range:?}"
        );
    }

    #[test]
    fn usage_upsert_keeps_one_row_per_run() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("s.db")).unwrap();
        db.upsert_task("acme-u", "/repo", None).unwrap();
        let run = db.start_run("acme-u", "grok", "m").unwrap();
        db.finish_run(run, "success", Some(0)).unwrap();
        db.insert_usage(run, None, "grok", 10, 2, 90, 5, None, Some("plan"), "{}")
            .unwrap();
        db.insert_usage(run, None, "grok", 10, 2, 90, 5, None, Some("plan"), "{}")
            .unwrap();
        db.insert_usage(
            run,
            None,
            "grok",
            10,
            2,
            90,
            5,
            Some(0.004),
            Some("est"),
            "{}",
        )
        .unwrap();
        let rows = db.list_usage_for_run(run).unwrap();
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(rows[0].input, 10);
        assert_eq!(rows[0].output, 2);
        assert!((rows[0].cost_usd.unwrap() - 0.004).abs() < 1e-9);
        assert_eq!(rows[0].cost_kind.as_deref(), Some("est"));
        let days = db.usage_by_day(1).unwrap();
        let grok = days.iter().find(|d| d.provider == "grok").unwrap();
        assert_eq!(grok.input, 10);
        assert_eq!(grok.runs, 1);
        assert!((grok.cost_usd - 0.004).abs() < 1e-9);
    }

    #[test]
    fn migrate_converts_old_grok_units_and_backfills_cost_kind() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.db");
        {
            let db = Db::open(&path).unwrap();
            let reset = "2026-10-16T07:36:00.000+00:00";
            db.upsert_provider_usage(
                "grok",
                "weekly",
                None,
                Some(14032.0),
                Some(1.0),
                Some(reset),
                None,
                "2026-10-13T12:00:00.000+00:00",
            )
            .unwrap();
            db.upsert_task("acme-m", "/repo", None).unwrap();
            let cursor_run = db.start_run("acme-m", "cursor", "m").unwrap();
            db.finish_run(cursor_run, "success", Some(0)).unwrap();
            db.insert_usage(
                cursor_run, None, "cursor", 80, 40, 400, 15, None, None, "{}",
            )
            .unwrap();
            let spark_run = db.start_run("acme-m", "spark", "m").unwrap();
            db.finish_run(spark_run, "success", Some(0)).unwrap();
            db.insert_usage(spark_run, None, "spark", 1, 1, 0, 0, None, Some(""), "{}")
                .unwrap();
            let grok_run = db.start_run("acme-m", "grok", "m").unwrap();
            db.finish_run(grok_run, "success", Some(0)).unwrap();
            db.insert_usage(grok_run, None, "grok", 10, 2, 90, 5, None, None, "{}")
                .unwrap();
            let claude_run = db.start_run("acme-m", "claude", "m").unwrap();
            db.finish_run(claude_run, "success", Some(0)).unwrap();
            db.insert_usage(
                claude_run,
                None,
                "claude",
                100,
                50,
                800,
                20,
                Some(0.0123),
                None,
                "{}",
            )
            .unwrap();
            let grok_est = db.start_run("acme-m", "grok", "m").unwrap();
            db.finish_run(grok_est, "success", Some(0)).unwrap();
            db.insert_usage(
                grok_est,
                None,
                "grok",
                10,
                2,
                90,
                5,
                Some(0.004),
                None,
                "{}",
            )
            .unwrap();
        }
        let db = Db::open(&path).unwrap();
        let grok = db.list_provider_usage("grok").unwrap();
        assert_eq!(grok.len(), 1);
        assert!((grok[0].utilization.unwrap() - 0.01).abs() < 1e-9);
        assert!((grok[0].balance.unwrap() - 140.32).abs() < 1e-9);
        let tz = crate::tz::DisplayTz::parse("UTC").unwrap();
        let now = chrono::DateTime::parse_from_rfc3339("2026-10-13T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(
            crate::reported::format_provider_usage(
                &crate::reported::provider_usage_view(&db, "grok"),
                &tz,
                now,
            ),
            "weekly 1.0% used, resets Oct 16 07:36 UTC; extra credits $140.32"
        );
        let runs = db.list_runs(Some("acme-m")).unwrap();
        let by = |name: &str| {
            let id = runs.iter().find(|r| r.provider == name).unwrap().id;
            db.list_usage_for_run(id).unwrap().remove(0)
        };
        let cursor = by("cursor");
        assert_eq!(cursor.cost_kind.as_deref(), Some("plan"));
        assert!(cursor.cost_usd.is_none());
        let spark = runs
            .iter()
            .find(|r| r.provider == "spark")
            .map(|r| db.list_usage_for_run(r.id).unwrap().remove(0))
            .unwrap();
        assert_eq!(spark.cost_kind.as_deref(), Some("plan"));
        let grok_plan = runs
            .iter()
            .filter(|r| r.provider == "grok")
            .find(|r| db.list_usage_for_run(r.id).unwrap()[0].cost_usd.is_none())
            .map(|r| db.list_usage_for_run(r.id).unwrap().remove(0))
            .unwrap();
        assert_eq!(grok_plan.cost_kind.as_deref(), Some("plan"));
        let grok_est = runs
            .iter()
            .filter(|r| r.provider == "grok")
            .find(|r| db.list_usage_for_run(r.id).unwrap()[0].cost_usd.is_some())
            .map(|r| db.list_usage_for_run(r.id).unwrap().remove(0))
            .unwrap();
        assert_eq!(grok_est.cost_kind.as_deref(), Some("est"));
        let claude = by("claude");
        assert_eq!(claude.cost_kind.as_deref(), Some("est"));
        // Second open must not divide again.
        drop(db);
        let db = Db::open(&path).unwrap();
        let grok = db.list_provider_usage("grok").unwrap();
        assert!((grok[0].utilization.unwrap() - 0.01).abs() < 1e-9);
        assert!((grok[0].balance.unwrap() - 140.32).abs() < 1e-9);
    }

    #[test]
    fn migrate_grok_ticks_divides_old_cost_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.db");
        let raw = r#"{"source":"grok usage","sessionId":"g","usage":{"inputTokens":19090,"cost_usd":126.8336}}"#;
        {
            let db = Db::open(&path).unwrap();
            db.upsert_task("acme-gt", "/repo", None).unwrap();
            let run = db.start_run("acme-gt", "grok", "m").unwrap();
            db.finish_run(run, "success", Some(0)).unwrap();
            db.insert_usage(
                run,
                None,
                "grok",
                1,
                1,
                0,
                0,
                Some(126.8336),
                Some("est"),
                raw,
            )
            .unwrap();
            let other = db.start_run("acme-gt", "claude", "m").unwrap();
            db.finish_run(other, "success", Some(0)).unwrap();
            db.insert_usage(
                other,
                None,
                "claude",
                1,
                1,
                0,
                0,
                Some(0.5),
                Some("est"),
                "{}",
            )
            .unwrap();
            db.meta_set("usage_grok_ticks_1e10", "0").unwrap();
        }
        for _ in 0..2 {
            let db = Db::open(&path).unwrap();
            let runs = db.list_runs(Some("acme-gt")).unwrap();
            let g = runs.iter().find(|r| r.provider == "grok").unwrap();
            let row = db.list_usage_for_run(g.id).unwrap().remove(0);
            assert!((row.cost_usd.unwrap() - 0.01268336).abs() < 1e-9);
            let c = runs.iter().find(|r| r.provider == "claude").unwrap();
            let row = db.list_usage_for_run(c.id).unwrap().remove(0);
            assert_eq!(row.cost_usd, Some(0.5));
        }
    }

    #[test]
    fn migrate_codex_input_subtracts_cached_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.db");
        {
            let db = Db::open(&path).unwrap();
            db.upsert_task("acme-cx", "/repo", None).unwrap();
            let run = db.start_run("acme-cx", "codex", "m").unwrap();
            db.finish_run(run, "success", Some(0)).unwrap();
            db.insert_usage(
                run,
                None,
                "codex",
                200,
                30,
                160,
                10,
                None,
                Some("plan"),
                "{}",
            )
            .unwrap();
            let spark = db.start_run("acme-cx", "spark", "m").unwrap();
            db.finish_run(spark, "success", Some(0)).unwrap();
            db.insert_usage(
                spark,
                None,
                "spark",
                200,
                30,
                160,
                10,
                None,
                Some("plan"),
                "{}",
            )
            .unwrap();
            db.meta_set("usage_codex_input_uncached", "0").unwrap();
        }
        let db = Db::open(&path).unwrap();
        let runs = db.list_runs(Some("acme-cx")).unwrap();
        let codex = runs.iter().find(|r| r.provider == "codex").unwrap();
        let row = db.list_usage_for_run(codex.id).unwrap().remove(0);
        assert_eq!(row.input, 40);
        assert_eq!(row.cache_read, 160);
        let spark = runs.iter().find(|r| r.provider == "spark").unwrap();
        let row = db.list_usage_for_run(spark.id).unwrap().remove(0);
        assert_eq!(row.input, 40);
        drop(db);
        let db = Db::open(&path).unwrap();
        let runs = db.list_runs(Some("acme-cx")).unwrap();
        let codex = runs.iter().find(|r| r.provider == "codex").unwrap();
        let row = db.list_usage_for_run(codex.id).unwrap().remove(0);
        assert_eq!(row.input, 40, "second open must not subtract again");
    }

    #[test]
    fn migrate_renames_and_dedupes_grok_window_aliases() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.db");
        {
            let _db = Db::open(&path).unwrap();
        }
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute(
                r#"INSERT INTO provider_usage
                   (provider, window, utilization, balance, credit_usage_percent, resets_at, status, updated_at)
                   VALUES ('grok', 'usage_period_type_weekly', NULL, 14032.0, 1.0,
                           '2026-10-16T07:36:00.000+00:00', NULL, '2026-10-13T12:00:00.000+00:00')"#,
                [],
            )
            .unwrap();
            conn.execute(
                r#"INSERT INTO provider_usage
                   (provider, window, utilization, balance, credit_usage_percent, resets_at, status, updated_at)
                   VALUES ('grok', 'weekly', 0.01, 140.32, 1.0,
                           '2026-10-16T07:36:00.000+00:00', NULL, '2026-10-14T10:00:00.000+00:00')"#,
                [],
            )
            .unwrap();
        }
        let db = Db::open(&path).unwrap();
        let grok = db.list_provider_usage("grok").unwrap();
        assert_eq!(grok.len(), 1, "{grok:?}");
        assert_eq!(grok[0].window, "weekly");
        assert!((grok[0].utilization.unwrap() - 0.01).abs() < 1e-9);
        assert!((grok[0].balance.unwrap() - 140.32).abs() < 1e-9);
        let tz = crate::tz::DisplayTz::parse("UTC").unwrap();
        let now = chrono::DateTime::parse_from_rfc3339("2026-10-13T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(
            crate::reported::format_provider_usage(
                &crate::reported::provider_usage_view(&db, "grok"),
                &tz,
                now,
            ),
            "weekly 1.0% used, resets Oct 16 07:36 UTC; extra credits $140.32"
        );
        db.upsert_provider_usage(
            "grok",
            "usage_period_type_weekly",
            Some(0.02),
            Some(140.32),
            Some(2.0),
            Some("2026-10-16T07:36:00.000+00:00"),
            None,
            "2026-10-14T11:00:00.000+00:00",
        )
        .unwrap();
        let grok = db.list_provider_usage("grok").unwrap();
        assert_eq!(grok.len(), 1, "upsert must canonicalize onto weekly");
        assert_eq!(grok[0].window, "weekly");
        assert!((grok[0].utilization.unwrap() - 0.02).abs() < 1e-9);
    }

    #[test]
    fn migrate_renames_lone_prefixed_grok_window() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.db");
        {
            let _db = Db::open(&path).unwrap();
        }
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute(
                r#"INSERT INTO provider_usage
                   (provider, window, utilization, balance, credit_usage_percent, resets_at, status, updated_at)
                   VALUES ('grok', 'usage_period_type_weekly', 0.01, 140.32, 1.0,
                           '2026-10-16T07:36:00.000+00:00', NULL, '2026-10-13T12:00:00.000+00:00')"#,
                [],
            )
            .unwrap();
        }
        let db = Db::open(&path).unwrap();
        let grok = db.list_provider_usage("grok").unwrap();
        assert_eq!(grok.len(), 1);
        assert_eq!(grok[0].window, "weekly");
    }

    #[test]
    fn migrate_recovers_grok_cost_kind_from_raw() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.db");
        let grok_raw = r#"{"text":"done","sessionId":"g1","usage":{"input_tokens":10,"cache_read_input_tokens":90,"cache_creation_input_tokens":5,"output_tokens":12},"total_cost_usd":0.004}"#;
        {
            let db = Db::open(&path).unwrap();
            db.upsert_task("acme-gk", "/repo", None).unwrap();
            let from_raw = db.start_run("acme-gk", "grok", "m").unwrap();
            db.finish_run(from_raw, "success", Some(0)).unwrap();
            db.insert_usage(
                from_raw,
                None,
                "grok",
                10,
                12,
                90,
                5,
                None,
                Some("plan"),
                grok_raw,
            )
            .unwrap();
            let already_cost = db.start_run("acme-gk", "grok", "m").unwrap();
            db.finish_run(already_cost, "success", Some(0)).unwrap();
            db.insert_usage(
                already_cost,
                None,
                "grok",
                10,
                12,
                90,
                5,
                Some(0.007),
                Some("plan"),
                "{}",
            )
            .unwrap();
            let no_cost = db.start_run("acme-gk", "grok", "m").unwrap();
            db.finish_run(no_cost, "success", Some(0)).unwrap();
            db.insert_usage(
                no_cost,
                None,
                "grok",
                10,
                12,
                90,
                5,
                None,
                Some("plan"),
                "{}",
            )
            .unwrap();
            let codex = db.start_run("acme-gk", "codex", "m").unwrap();
            db.finish_run(codex, "success", Some(0)).unwrap();
            db.insert_usage(
                codex,
                None,
                "codex",
                40,
                30,
                160,
                10,
                None,
                Some("plan"),
                "{}",
            )
            .unwrap();
        }
        let db = Db::open(&path).unwrap();
        let runs = db.list_runs(Some("acme-gk")).unwrap();
        let grok_usage: Vec<_> = runs
            .iter()
            .filter(|r| r.provider == "grok")
            .map(|r| db.list_usage_for_run(r.id).unwrap().remove(0))
            .collect();
        let recovered = grok_usage
            .iter()
            .find(|u| {
                u.cost_usd
                    .map(|c| (c - 0.004).abs() < 1e-9)
                    .unwrap_or(false)
            })
            .unwrap();
        assert_eq!(recovered.cost_kind.as_deref(), Some("est"));
        let flipped = grok_usage
            .iter()
            .find(|u| {
                u.cost_usd
                    .map(|c| (c - 0.007).abs() < 1e-9)
                    .unwrap_or(false)
            })
            .unwrap();
        assert_eq!(flipped.cost_kind.as_deref(), Some("est"));
        let still_plan = grok_usage.iter().find(|u| u.cost_usd.is_none()).unwrap();
        assert_eq!(still_plan.cost_kind.as_deref(), Some("plan"));
        let codex = runs
            .iter()
            .find(|r| r.provider == "codex")
            .map(|r| db.list_usage_for_run(r.id).unwrap().remove(0))
            .unwrap();
        assert_eq!(codex.cost_kind.as_deref(), Some("plan"));
        assert!(codex.cost_usd.is_none());
    }

    #[test]
    fn empty_repo_on_upsert_keeps_existing() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("s.db")).unwrap();
        db.upsert_task("acme-1", "/shadow/src", Some(1)).unwrap();
        db.upsert_task("acme-1", "", Some(1)).unwrap();
        let t = db.get_task("acme-1").unwrap().unwrap();
        assert_eq!(t.repo.as_deref(), Some("/shadow/src"));
    }

    #[test]
    fn migrate_keeps_existing_tasks_and_backfills_generic_fields() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.db");
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(
                r#"
                CREATE TABLE tasks (
                    key TEXT PRIMARY KEY,
                    repo TEXT,
                    issue INTEGER,
                    status TEXT NOT NULL,
                    provider TEXT,
                    session_id TEXT,
                    worktree TEXT,
                    stall_retries INTEGER NOT NULL DEFAULT 0,
                    handoff TEXT,
                    tried TEXT,
                    comment_cursor TEXT,
                    created TEXT NOT NULL,
                    updated TEXT NOT NULL,
                    shadow INTEGER NOT NULL DEFAULT 0
                );
                INSERT INTO tasks (key, repo, issue, status, created, updated)
                VALUES ('acme-12', '/src/acme', 12, 'waiting', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z');
                INSERT INTO tasks (key, repo, issue, status, created, updated)
                VALUES ('local-1', '/tmp/notes', NULL, 'queued', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z');
                "#,
            )
            .unwrap();
        }
        let db = Db::open(&path).unwrap();
        let gh = db.get_task("acme-12").unwrap().unwrap();
        assert_eq!(gh.issue, Some(12));
        assert_eq!(gh.repo.as_deref(), Some("/src/acme"));
        assert_eq!(gh.status, "waiting");
        assert_eq!(gh.workspace_kind, "git");
        assert_eq!(gh.output_kind, "reply");
        assert_eq!(gh.source_kind, "github");
        let local = db.get_task("local-1").unwrap().unwrap();
        assert_eq!(local.issue, None);
        assert_eq!(local.workspace_kind, "git");
        assert_eq!(local.source_kind, "cli");
        db.upsert_task("answer-1", "", None).unwrap();
        let none = db.get_task("answer-1").unwrap().unwrap();
        assert_eq!(none.workspace_kind, "none");
        assert_eq!(none.issue, None);
    }

    #[test]
    fn prefix_hash_change_is_detected_per_session() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("s.db")).unwrap();
        db.upsert_task("acme-1", "/repo", None).unwrap();
        let a = db.start_run("acme-1", "claude", "m").unwrap();
        db.set_run_session(a, "sid-1").unwrap();
        db.set_run_prefix_hash(a, "aaa").unwrap();
        db.finish_run(a, "success", Some(0)).unwrap();
        assert!(!db.session_prefix_changed("acme-1").unwrap());
        let b = db.start_run("acme-1", "claude", "m").unwrap();
        db.set_run_session(b, "sid-1").unwrap();
        db.set_run_prefix_hash(b, "bbb").unwrap();
        db.finish_run(b, "success", Some(0)).unwrap();
        assert!(db.session_prefix_changed("acme-1").unwrap());
        assert_eq!(
            db.last_prefix_hash("sid-1", "claude", b)
                .unwrap()
                .as_deref(),
            Some("aaa")
        );
    }

    fn count_for(db: &Db, sql: &str, key: &str) -> i64 {
        let conn = db.lock();
        conn.query_row(sql, params![key], |r| r.get(0)).unwrap()
    }

    #[test]
    fn delete_task_removes_children_refuses_running() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("s.db")).unwrap();
        db.upsert_task("keep", "/repo-a", None).unwrap();
        db.set_status("keep", "waiting").unwrap();
        db.upsert_task("gone", "/repo-a", None).unwrap();
        db.set_status("gone", "waiting").unwrap();
        db.set_provider_session("gone", Some("claude"), Some("sid-gone"))
            .unwrap();
        let rid = db.start_run("gone", "claude", "m").unwrap();
        db.start_turn(rid, "gone").unwrap();
        db.finish_run(rid, "success", Some(0)).unwrap();
        db.insert_event(rid, "assistant", "{}").unwrap();
        db.enqueue_message("gone", "hello", "cli").unwrap();
        db.insert_verify_check(
            rid,
            "gone",
            &VerifyCheck {
                command: "true".into(),
                passed: true,
                exit_code: 0,
                duration_ms: 1,
                output: String::new(),
                recorded: now_rfc3339(),
            },
        )
        .unwrap();
        db.insert_shadow_result(&ShadowResult {
            task_key: "gone".into(),
            run_id: Some(rid),
            provider: "claude".into(),
            outcome: "success".into(),
            duration_ms: 1,
            input: 0,
            output: 0,
            cache_read: 0,
            cache_write: 0,
            cost_usd: 0.0,
            cache_hit: None,
            tests: String::new(),
            diff: String::new(),
            diffstat: String::new(),
            verify: String::new(),
            recorded: now_rfc3339(),
        })
        .unwrap();

        db.delete_task("gone").unwrap();
        assert!(db.get_task("gone").unwrap().is_none());
        assert!(db.get_task("keep").unwrap().is_some());
        assert!(db.list_runs(Some("gone")).unwrap().is_empty());
        assert!(db.events_for_task("gone").unwrap().is_empty());
        assert!(db.all_messages("gone").unwrap().is_empty());
        assert!(db.task_session("gone", "claude").unwrap().is_none());
        assert_eq!(
            count_for(
                &db,
                "SELECT COUNT(*) FROM turns WHERE task_key = ?1",
                "gone"
            ),
            0
        );
        assert_eq!(
            count_for(
                &db,
                "SELECT COUNT(*) FROM verify_results WHERE task_key = ?1",
                "gone"
            ),
            0
        );
        assert_eq!(
            count_for(
                &db,
                "SELECT COUNT(*) FROM shadow_results WHERE task_key = ?1",
                "gone"
            ),
            0
        );

        db.upsert_task("live", "/repo-a", None).unwrap();
        db.set_status("live", "running").unwrap();
        let err = db.delete_task("live").unwrap_err().to_string();
        assert!(err.contains("running"), "{err}");
        assert!(db.get_task("live").unwrap().is_some());

        db.upsert_task("queued", "/repo-a", None).unwrap();
        let err = db.delete_task("queued").unwrap_err().to_string();
        assert!(err.contains("queued"), "{err}");

        db.upsert_task("open", "/repo-a", None).unwrap();
        db.set_status("open", "waiting").unwrap();
        db.start_run("open", "claude", "m").unwrap();
        let err = db.delete_task("open").unwrap_err().to_string();
        assert!(err.contains("open run"), "{err}");
        assert!(db.get_task("open").unwrap().is_some());
    }
}
