use crate::duration::parse_duration;
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawConfig {
    #[serde(default = "default_listen")]
    pub listen: String,
    #[serde(default = "default_state_db")]
    pub state_db: String,
    #[serde(default = "default_worktree_root")]
    pub worktree_root: String,
    #[serde(default)]
    pub repo_path: String,
    #[serde(default = "default_branch")]
    pub repo_default_branch: String,
    #[serde(default = "default_branch_prefix")]
    pub worktree_branch_prefix: String,
    #[serde(default = "default_system_prompt")]
    pub system_prompt_file: String,
    #[serde(default = "default_cache_warn")]
    pub cache_hit_warn_threshold: f64,
    #[serde(default = "default_shell_timeout")]
    pub shell_command_timeout: String,
    #[serde(default = "default_task_timeout")]
    pub task_timeout: String,
    #[serde(default = "default_kill_grace")]
    pub kill_grace: String,
    #[serde(default)]
    pub dry_run: bool,
    #[serde(default = "default_task_prefix")]
    pub task_key_prefix: String,
    /// IANA zone or empty/`local` for the system local zone.
    #[serde(default)]
    pub display_timezone: String,
    #[serde(default)]
    pub github: GithubConfig,
    #[serde(default)]
    pub telegram: RawTelegram,
    #[serde(default)]
    pub dashboard: RawDashboard,
    #[serde(default)]
    pub handoff: RawHandoff,
    #[serde(default)]
    pub shadow: RawShadow,
    #[serde(default)]
    pub verify: RawVerify,
    #[serde(default)]
    pub checklist: RawChecklist,
    #[serde(default)]
    pub log: RawLog,
    #[serde(default)]
    pub gc: RawGc,
    #[serde(default)]
    pub update: RawUpdate,
    /// Optional custom name → kind + model. Built-ins live in `presets.rs`.
    /// Never holds API keys.
    #[serde(rename = "preset", default)]
    pub presets: Vec<RawPreset>,
    #[serde(rename = "provider", default)]
    pub providers: Vec<RawProvider>,
    /// Repeatable watch targets. Empty = use top-level `repo_path` + `[github].repo`.
    #[serde(rename = "repo", default)]
    pub repos: Vec<RawRepo>,
    /// Planning/review provider. Empty = grok if enabled, else first enabled.
    #[serde(default)]
    pub driver: String,
    #[serde(default)]
    pub planner: RawPlannerIn,
    #[serde(default)]
    pub review: RawReview,
    #[serde(default)]
    pub plugin: RawPlugin,
    #[serde(default)]
    pub mcp: RawMcp,
    #[serde(default)]
    pub cache: RawCache,
    /// Repeatable intake sources. Empty = current defaults (GitHub if `[github]
    /// enabled`, MCP/CLI/HTTP always, Telegram if enabled, inbox off).
    #[serde(rename = "source", default)]
    pub sources: Vec<RawSource>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawCache {
    /// Reuse `.atr/wt/<provider>-<n>` folders so cwd stays cacheable.
    #[serde(default = "default_true")]
    pub worktree_pool: bool,
    /// Headline hit% ignores runs below this prompt-token count.
    #[serde(default = "default_headline_min_input")]
    pub headline_min_input: i64,
}

impl Default for RawCache {
    fn default() -> Self {
        Self {
            worktree_pool: true,
            headline_min_input: default_headline_min_input(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawMcp {
    /// HTTP `/mcp` on the existing listener. `atr mcp` stdio is always available.
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_mcp_token_env")]
    pub token_env: String,
    #[serde(default = "default_mcp_token_file")]
    pub token_file: String,
}

impl Default for RawMcp {
    fn default() -> Self {
        Self {
            enabled: true,
            token_env: default_mcp_token_env(),
            token_file: default_mcp_token_file(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum RawPlannerIn {
    Mode(String),
    Table(RawPlanner),
}

impl Default for RawPlannerIn {
    fn default() -> Self {
        Self::Mode(default_planner_mode())
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawPlanner {
    #[serde(default = "default_planner_mode")]
    pub mode: String,
    #[serde(default = "default_planner_chars")]
    pub auto_chars: usize,
    #[serde(default = "default_planner_turns")]
    pub max_turns: u32,
    #[serde(default = "default_planner_subtasks")]
    pub max_subtasks: u32,
}

impl Default for RawPlanner {
    fn default() -> Self {
        Self {
            mode: default_planner_mode(),
            auto_chars: default_planner_chars(),
            max_turns: default_planner_turns(),
            max_subtasks: default_planner_subtasks(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawReview {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_review_rounds")]
    pub max_rounds: u32,
    #[serde(default = "default_review_turns")]
    pub max_turns: u32,
}

impl Default for RawReview {
    fn default() -> Self {
        Self {
            enabled: false,
            max_rounds: default_review_rounds(),
            max_turns: default_review_turns(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct RawPlugin {
    /// Stop hook runs this command and blocks until it passes. Empty = off.
    #[serde(default)]
    pub test_command: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawSource {
    pub kind: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Inbox directory (`kind = "inbox"`).
    #[serde(default)]
    pub dir: String,
    #[serde(default)]
    pub poll_interval: String,
}

#[derive(Debug, Clone)]
pub struct SourceEntry {
    pub kind: crate::task::SourceKind,
    pub enabled: bool,
    pub dir: PathBuf,
    pub poll_interval: Duration,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawRepo {
    pub name: String,
    pub path: String,
    pub github: String,
    #[serde(default = "default_gh_label")]
    pub label: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub branch_prefix: String,
    #[serde(default)]
    pub base: String,
    #[serde(default)]
    pub system_prompt_file: String,
    /// 0 / omitted = no per-repo cap (global + provider limits still apply).
    #[serde(default)]
    pub max_parallel: u32,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawPreset {
    pub name: String,
    pub kind: String,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub cli: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawUpdate {
    #[serde(default = "default_update_repo")]
    pub github_repo: String,
}

impl Default for RawUpdate {
    fn default() -> Self {
        Self {
            github_repo: default_update_repo(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawHandoff {
    #[serde(default = "default_handoff_bytes")]
    pub max_bytes: usize,
    #[serde(default = "default_true")]
    pub include_diff: bool,
}

impl Default for RawHandoff {
    fn default() -> Self {
        Self {
            max_bytes: default_handoff_bytes(),
            include_diff: true,
        }
    }
}

#[derive(Debug, Clone)]
pub struct HandoffConfig {
    pub max_bytes: usize,
    pub include_diff: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawShadow {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub repo: String,
    #[serde(default)]
    pub labels: Vec<String>,
    #[serde(default = "default_poll")]
    pub poll_interval: String,
    #[serde(default = "default_shadow_parallel")]
    pub max_parallel: u32,
    #[serde(default)]
    pub key_prefix: String,
    /// Local clone for shadow worktrees. Empty = `{worktree_root}/_src/<repo>`.
    #[serde(default)]
    pub repo_path: String,
    /// Env var with a read-only token used only by the poller/clone, never the agent.
    #[serde(default = "default_shadow_token_env")]
    pub gh_token_env: String,
    /// `record` stores verify results without a follow-up; `retry` sends one.
    #[serde(default = "default_shadow_verify_mode")]
    pub verify_mode: String,
}

impl Default for RawShadow {
    fn default() -> Self {
        Self {
            enabled: false,
            repo: String::new(),
            labels: Vec::new(),
            poll_interval: default_poll(),
            max_parallel: default_shadow_parallel(),
            key_prefix: String::new(),
            repo_path: String::new(),
            gh_token_env: default_shadow_token_env(),
            verify_mode: default_shadow_verify_mode(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ShadowConfig {
    pub enabled: bool,
    pub repo: String,
    pub labels: Vec<String>,
    pub poll_interval: Duration,
    pub max_parallel: u32,
    pub key_prefix: String,
    pub repo_path: PathBuf,
    pub gh_token_env: String,
    pub verify_mode: ShadowVerifyMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShadowVerifyMode {
    Record,
    Retry,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GithubConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_gh")]
    pub cli: String,
    #[serde(default)]
    pub repo: String,
    #[serde(default = "default_gh_label")]
    pub label: String,
    #[serde(default = "default_poll")]
    pub poll_interval: String,
}

impl Default for GithubConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            cli: default_gh(),
            repo: String::new(),
            label: default_gh_label(),
            poll_interval: default_poll(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct RawTelegram {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_tg_token_env")]
    pub bot_token_env: String,
    #[serde(default)]
    pub allowed_chat_ids: Vec<i64>,
    #[serde(default)]
    pub token_file: String,
    #[serde(default)]
    pub chat_id_file: String,
    #[serde(default = "default_tg_prefix")]
    pub prefix: String,
    #[serde(default = "default_tg_notify")]
    pub notify_interval: String,
    /// Local `HH:MM` for the daily summary. Empty disables it.
    #[serde(default)]
    pub daily_summary: String,
    /// Override API host (tests). Empty = `https://api.telegram.org`.
    #[serde(default)]
    pub api_base: String,
    #[serde(default)]
    pub assistant: RawTelegramAssistant,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct RawTelegramAssistant {
    #[serde(default)]
    pub enabled: bool,
    /// Empty = first available provider in order.
    #[serde(default)]
    pub provider: String,
    #[serde(default = "default_assistant_timeout")]
    pub timeout: String,
}

#[derive(Debug, Clone)]
pub struct TelegramAssistant {
    pub enabled: bool,
    pub provider: String,
    pub timeout: Duration,
}

impl Default for TelegramAssistant {
    fn default() -> Self {
        Self {
            enabled: false,
            provider: String::new(),
            timeout: Duration::from_secs(600),
        }
    }
}

#[derive(Debug, Clone)]
pub struct TelegramConfig {
    pub enabled: bool,
    pub bot_token_env: String,
    pub allowed_chat_ids: Vec<i64>,
    pub token_file: String,
    pub chat_id_file: String,
    pub prefix: String,
    pub notify_interval: Duration,
    pub daily_summary: String,
    pub api_base: String,
    pub assistant: TelegramAssistant,
}

impl Default for TelegramConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            bot_token_env: default_tg_token_env(),
            allowed_chat_ids: Vec::new(),
            token_file: String::new(),
            chat_id_file: String::new(),
            prefix: default_tg_prefix(),
            notify_interval: Duration::from_secs(20),
            daily_summary: String::new(),
            api_base: String::new(),
            assistant: TelegramAssistant::default(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawProvider {
    pub name: String,
    pub kind: String,
    /// Built-in or config `[[preset]]` name. Fills default model when omitted.
    #[serde(default)]
    pub preset: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub cli: String,
    #[serde(default)]
    pub expected_version: Option<String>,
    #[serde(default = "default_version_args")]
    pub version_args: Vec<String>,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub model_flag: String,
    #[serde(default = "default_parallel")]
    pub max_parallel: u32,
    #[serde(default = "default_cooldown")]
    pub cooldown_after_limit: String,
    #[serde(default = "default_degraded_stall")]
    pub degraded_after_stall: String,
    #[serde(default = "default_heartbeat")]
    pub heartbeat_timeout: String,
    #[serde(default = "default_run_timeout")]
    pub run_timeout: String,
    #[serde(default)]
    pub resume_flag: String,
    /// When set (Grok: `--session-id`), a new session is started with a
    /// generated UUID so a stall can resume even if the CLI never emitted an id.
    #[serde(default)]
    pub session_id_flag: String,
    #[serde(default)]
    pub cwd_flag: String,
    /// Headless trust, e.g. Claude `bypassPermissions`. Empty omits the flag.
    #[serde(default)]
    pub permission_mode: String,
    #[serde(default = "default_permission_mode_flag")]
    pub permission_mode_flag: String,
    #[serde(default = "default_prompt_mode")]
    pub prompt_mode: String,
    #[serde(default = "default_at")]
    pub prompt_file_prefix: String,
    #[serde(default)]
    pub profile: String,
    #[serde(default)]
    pub sandbox: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub extra_args: Vec<String>,
    /// `None` = fill from kind. `Some([])` = no overrides.
    #[serde(default, alias = "extra_config")]
    pub config_overrides: Option<Vec<String>>,
    #[serde(default)]
    pub quota: Vec<RawQuota>,
    /// `claude_stream`, `grok_billing`, or empty/`none`.
    #[serde(default)]
    pub reported_usage: String,
    /// Grok only: use `grok agent --plugin-dir … stdio` instead of `-p`.
    #[serde(default)]
    pub plugin: bool,
    /// `--max-turns` for this provider. 0 = role default (worker 30).
    #[serde(default)]
    pub max_turns: u32,
    #[serde(default = "default_skip_at")]
    pub skip_at: f64,
    #[serde(default = "default_reported_interval")]
    pub reported_usage_interval: String,
    #[serde(default = "default_reported_timeout")]
    pub reported_usage_timeout: String,
    #[serde(default)]
    pub health: RawHealth,
    /// How long this provider's prompt cache stays warm (Claude ~5m).
    #[serde(default)]
    pub cache_window: String,
    /// CLI flag that takes a *file path* for the stable prefix. Empty = none.
    #[serde(default)]
    pub system_prompt_flag: String,
    /// OpenCode/Ollama `keep_alive`. Empty = kind default (`24h` for Ollama).
    #[serde(default)]
    pub keep_alive: String,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct RawHealth {
    #[serde(default)]
    pub startup_timeout: String,
    #[serde(default)]
    pub heartbeat_timeout: String,
    #[serde(default)]
    pub stall_resumes: Option<u32>,
    #[serde(default)]
    pub unhealthy_cooldown: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawQuota {
    pub window: String,
    pub budget: f64,
    #[serde(default = "default_quota_unit")]
    pub unit: String,
    #[serde(default = "default_quota_threshold")]
    pub threshold: f64,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub listen: String,
    pub state_db: PathBuf,
    pub worktree_root: PathBuf,
    pub repo_path: PathBuf,
    pub repo_default_branch: String,
    pub worktree_branch_prefix: String,
    pub system_prompt_file: PathBuf,
    pub cache_hit_warn_threshold: f64,
    pub shell_command_timeout: Duration,
    pub shell_command_timeout_raw: String,
    pub task_timeout: Duration,
    pub kill_grace: Duration,
    pub dry_run: bool,
    pub task_key_prefix: String,
    pub display_timezone: crate::tz::DisplayTz,
    pub github: GithubConfig,
    pub github_poll_interval: Duration,
    pub telegram: TelegramConfig,
    pub dashboard: DashboardConfig,
    pub handoff: HandoffConfig,
    pub shadow: ShadowConfig,
    pub verify: VerifyConfig,
    pub checklist: ChecklistConfig,
    pub log: LogConfig,
    pub gc: GcConfig,
    pub update: UpdateConfig,
    pub presets: Vec<Preset>,
    pub providers: Vec<Provider>,
    /// Explicit `[[repo]]` entries only. Use [`Config::effective_repos`] for the
    /// watch list (falls back to `repo_path` + `[github].repo`).
    pub repos: Vec<RepoEntry>,
    pub driver: String,
    pub planner: PlannerConfig,
    pub review: ReviewConfig,
    pub plugin: PluginConfig,
    pub mcp: McpConfig,
    pub cache: CacheConfig,
    /// Explicit `[[source]]` entries. Empty means built-in defaults.
    pub sources: Vec<SourceEntry>,
}

#[derive(Debug, Clone)]
pub struct CacheConfig {
    pub worktree_pool: bool,
    pub headline_min_input: i64,
}

#[derive(Debug, Clone)]
pub struct McpConfig {
    pub enabled: bool,
    pub token_env: String,
    pub token_file: PathBuf,
}

#[derive(Debug, Clone)]
pub struct PlannerConfig {
    pub mode: PlannerMode,
    pub auto_chars: usize,
    pub max_turns: u32,
    pub max_subtasks: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlannerMode {
    Auto,
    Always,
    Off,
}

impl PlannerMode {
    fn parse(raw: &str) -> Result<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "" | "auto" => Ok(Self::Auto),
            "always" | "on" | "true" => Ok(Self::Always),
            "off" | "false" | "disabled" => Ok(Self::Off),
            other => bail!("planner mode {other:?} must be auto, always, or off"),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Always => "always",
            Self::Off => "off",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ReviewConfig {
    pub enabled: bool,
    pub max_rounds: u32,
    pub max_turns: u32,
}

#[derive(Debug, Clone)]
pub struct PluginConfig {
    pub test_command: String,
}

#[derive(Debug, Clone)]
pub struct RepoEntry {
    pub name: String,
    pub path: PathBuf,
    pub github: String,
    pub label: String,
    pub enabled: bool,
    pub branch_prefix: String,
    pub base: String,
    pub system_prompt_file: PathBuf,
    pub max_parallel: u32,
    /// True when synthesized from top-level `repo_path` + `[github]` (no `[[repo]]`).
    pub shorthand: bool,
}

#[derive(Debug, Clone)]
pub struct Preset {
    pub name: String,
    pub kind: String,
    pub model: String,
    pub cli: String,
}

#[derive(Debug, Clone)]
pub struct UpdateConfig {
    pub github_repo: String,
}

#[derive(Debug, Clone)]
pub struct Provider {
    pub name: String,
    pub kind: String,
    pub preset: String,
    pub enabled: bool,
    pub cli: PathBuf,
    pub expected_version: Option<String>,
    pub version_args: Vec<String>,
    pub model: String,
    pub model_flag: String,
    pub max_parallel: u32,
    pub cooldown_after_limit: Duration,
    pub degraded_after_stall: Duration,
    pub heartbeat_timeout: Duration,
    pub run_timeout: Duration,
    pub resume_flag: String,
    pub session_id_flag: String,
    pub cwd_flag: String,
    pub permission_mode: String,
    pub permission_mode_flag: String,
    pub prompt_mode: String,
    pub prompt_file_prefix: String,
    pub profile: String,
    pub sandbox: String,
    pub args: Vec<String>,
    pub extra_args: Vec<String>,
    pub config_overrides: Vec<String>,
    pub quota: Vec<QuotaWindow>,
    pub reported_usage: String,
    pub skip_at: f64,
    pub reported_usage_interval: Duration,
    pub reported_usage_timeout: Duration,
    pub health: ProviderHealth,
    pub plugin: bool,
    pub max_turns: u32,
    pub cache_window: Duration,
    pub system_prompt_flag: String,
    pub keep_alive: String,
}

impl Provider {
    pub fn is_ollama(&self) -> bool {
        is_ollama_model(&self.preset, &self.model)
    }
}

#[derive(Debug, Clone)]
pub struct ProviderHealth {
    pub startup_timeout: Duration,
    pub heartbeat_timeout: Duration,
    pub stall_resumes: u32,
    pub unhealthy_cooldown: Duration,
}

impl Default for ProviderHealth {
    fn default() -> Self {
        Self {
            startup_timeout: Duration::from_secs(90),
            heartbeat_timeout: Duration::from_secs(600),
            stall_resumes: 1,
            unhealthy_cooldown: Duration::from_secs(600),
        }
    }
}

impl Provider {
    #[cfg(test)]
    pub fn test_stub(name: &str) -> Self {
        Self {
            name: name.into(),
            kind: name.into(),
            preset: String::new(),
            enabled: true,
            cli: PathBuf::from(name),
            expected_version: None,
            version_args: vec!["--version".into()],
            model: "m".into(),
            model_flag: "--model".into(),
            max_parallel: 1,
            cooldown_after_limit: Duration::from_secs(1),
            degraded_after_stall: Duration::from_secs(2),
            heartbeat_timeout: Duration::from_secs(1),
            run_timeout: Duration::from_secs(1),
            resume_flag: "--resume".into(),
            session_id_flag: String::new(),
            cwd_flag: String::new(),
            permission_mode: String::new(),
            permission_mode_flag: "--permission-mode".into(),
            prompt_mode: "arg".into(),
            prompt_file_prefix: "@".into(),
            profile: String::new(),
            sandbox: String::new(),
            args: vec![],
            extra_args: vec![],
            config_overrides: vec![],
            quota: vec![],
            reported_usage: String::new(),
            skip_at: 0.9,
            reported_usage_interval: Duration::from_secs(600),
            reported_usage_timeout: Duration::from_secs(20),
            health: ProviderHealth::default(),
            plugin: false,
            max_turns: 0,
            cache_window: Duration::from_secs(3600),
            system_prompt_flag: String::new(),
            keep_alive: String::new(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct QuotaWindow {
    pub window: Duration,
    pub window_raw: String,
    pub budget: f64,
    pub unit: QuotaUnit,
    pub threshold: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuotaUnit {
    Usd,
    Tokens,
    Runs,
}

impl QuotaUnit {
    pub fn as_str(self) -> &'static str {
        match self {
            QuotaUnit::Usd => "usd",
            QuotaUnit::Tokens => "tokens",
            QuotaUnit::Runs => "runs",
        }
    }

    fn parse(raw: &str) -> Result<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "usd" | "dollars" | "dollar" | "cost" => Ok(QuotaUnit::Usd),
            "tokens" | "token" => Ok(QuotaUnit::Tokens),
            "runs" | "run" => Ok(QuotaUnit::Runs),
            other => bail!("quota unit {other:?} must be usd, tokens, or runs"),
        }
    }
}

fn default_listen() -> String {
    "127.0.0.1:7420".into()
}
fn default_state_db() -> String {
    "~/.local/share/atr/state.db".into()
}
fn default_worktree_root() -> String {
    "~/atr-work".into()
}
fn default_system_prompt() -> String {
    "~/.config/atr/system-prompt.md".into()
}
fn default_update_repo() -> String {
    "rimusz/agent-task-runner".into()
}
fn default_branch() -> String {
    "main".into()
}
fn default_branch_prefix() -> String {
    "atr".into()
}
fn default_cache_warn() -> f64 {
    0.70
}
fn default_shell_timeout() -> String {
    "15m".into()
}
fn default_task_timeout() -> String {
    "4h".into()
}
fn default_kill_grace() -> String {
    "30s".into()
}
fn default_task_prefix() -> String {
    "task".into()
}
fn default_gh() -> String {
    "gh".into()
}
/// Default `[github] label`. Existing configs keep whatever they already set.
pub const DEFAULT_GITHUB_LABEL: &str = "for:atr";
fn default_gh_label() -> String {
    DEFAULT_GITHUB_LABEL.into()
}
fn default_poll() -> String {
    "60s".into()
}
fn default_tg_prefix() -> String {
    "atr:".into()
}
fn default_tg_token_env() -> String {
    "ATR_TELEGRAM_TOKEN".into()
}
fn default_tg_notify() -> String {
    "20s".into()
}
fn default_dashboard_bind() -> String {
    "127.0.0.1".into()
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawDashboard {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_dashboard_bind")]
    pub bind: String,
}

impl Default for RawDashboard {
    fn default() -> Self {
        Self {
            enabled: true,
            bind: default_dashboard_bind(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct DashboardConfig {
    pub enabled: bool,
    pub bind: String,
}

impl Default for DashboardConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            bind: default_dashboard_bind(),
        }
    }
}
fn default_assistant_timeout() -> String {
    "10m".into()
}
fn default_true() -> bool {
    true
}

fn default_planner_mode() -> String {
    "auto".into()
}

fn default_planner_chars() -> usize {
    4000
}

fn default_planner_turns() -> u32 {
    8
}

fn default_planner_subtasks() -> u32 {
    8
}

fn default_review_rounds() -> u32 {
    2
}

fn default_review_turns() -> u32 {
    15
}

fn default_mcp_token_env() -> String {
    "ATR_MCP_TOKEN".into()
}

fn default_mcp_token_file() -> String {
    "~/.config/atr/mcp.token".into()
}

fn default_headline_min_input() -> i64 {
    40_000
}

pub fn is_ollama_model(preset: &str, model: &str) -> bool {
    preset.eq_ignore_ascii_case("ollama")
        || model.to_ascii_lowercase().starts_with("ollama/")
        || model.to_ascii_lowercase().contains("/ollama")
}

pub fn default_cache_window(kind: &str) -> Duration {
    match kind {
        "claude" => Duration::from_secs(5 * 60),
        _ => Duration::from_secs(60 * 60),
    }
}

pub fn default_system_prompt_flag(kind: &str) -> &'static str {
    match kind {
        "claude" => "--append-system-prompt-file",
        "grok" => "--rules",
        "opencode" => "--system",
        _ => "",
    }
}

pub fn default_keep_alive(preset: &str, model: &str) -> String {
    if is_ollama_model(preset, model) {
        "24h".into()
    } else {
        String::new()
    }
}
fn default_version_args() -> Vec<String> {
    vec!["--version".into()]
}
fn default_parallel() -> u32 {
    1
}
fn default_cooldown() -> String {
    "30m".into()
}
fn default_degraded_stall() -> String {
    "2m".into()
}
fn default_heartbeat() -> String {
    "10m".into()
}
fn default_run_timeout() -> String {
    "90m".into()
}
fn default_prompt_mode() -> String {
    String::new()
}
fn default_at() -> String {
    String::new()
}
fn default_permission_mode_flag() -> String {
    "--permission-mode".into()
}
fn default_handoff_bytes() -> usize {
    16_384
}
fn default_shadow_parallel() -> u32 {
    1
}
fn default_shadow_token_env() -> String {
    "ATR_SHADOW_GH_TOKEN".into()
}
fn default_verify_timeout() -> String {
    "15m".into()
}
fn default_verify_retries() -> u32 {
    1
}
fn default_verify_output() -> usize {
    4000
}
fn default_protect_skip_label() -> String {
    "atr:tests-ok".into()
}
pub fn default_test_globs() -> Vec<String> {
    vec![
        "**/*.test.*".into(),
        "**/*.spec.*".into(),
        "**/__tests__/**".into(),
        "tests/**".into(),
    ]
}
fn default_checklist_bytes() -> usize {
    4096
}
fn default_shadow_verify_mode() -> String {
    "record".into()
}
fn default_quota_unit() -> String {
    "usd".into()
}
fn default_quota_threshold() -> f64 {
    0.85
}
fn default_skip_at() -> f64 {
    0.9
}
fn default_reported_interval() -> String {
    "10m".into()
}
fn default_reported_timeout() -> String {
    "20s".into()
}
fn default_log_bytes() -> String {
    "5m".into()
}
fn default_log_keep() -> u32 {
    5
}
fn default_gc_worktree() -> String {
    "14d".into()
}
fn default_gc_shadow() -> String {
    "7d".into()
}
fn default_gc_events() -> String {
    "30d".into()
}
fn default_gc_runs() -> String {
    "30d".into()
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawLog {
    #[serde(default)]
    pub path: String,
    #[serde(default = "default_log_bytes")]
    pub max_bytes: String,
    #[serde(default = "default_log_keep")]
    pub keep: u32,
}

impl Default for RawLog {
    fn default() -> Self {
        Self {
            path: String::new(),
            max_bytes: default_log_bytes(),
            keep: default_log_keep(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct LogConfig {
    pub path: PathBuf,
    pub max_bytes: u64,
    pub keep: u32,
}

impl Default for LogConfig {
    fn default() -> Self {
        Self {
            path: PathBuf::new(),
            max_bytes: 5 * 1024 * 1024,
            keep: default_log_keep(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawGc {
    /// Local `HH:MM` in display_timezone. Empty disables automatic gc.
    #[serde(default)]
    pub daily: String,
    #[serde(default = "default_gc_worktree")]
    pub worktree_retention: String,
    #[serde(default = "default_gc_shadow")]
    pub shadow_min_age: String,
    #[serde(default = "default_gc_events")]
    pub event_retention: String,
    #[serde(default = "default_gc_runs")]
    pub run_retention: String,
    /// Delete finished CLI session files from known paths only.
    #[serde(default)]
    pub session_files: bool,
}

impl Default for RawGc {
    fn default() -> Self {
        Self {
            daily: String::new(),
            worktree_retention: default_gc_worktree(),
            shadow_min_age: default_gc_shadow(),
            event_retention: default_gc_events(),
            run_retention: default_gc_runs(),
            session_files: false,
        }
    }
}

#[derive(Debug, Clone)]
pub struct GcConfig {
    pub daily: String,
    pub worktree_retention: Duration,
    pub shadow_min_age: Duration,
    pub event_retention: Duration,
    pub run_retention: Duration,
    pub session_files: bool,
}

impl Default for GcConfig {
    fn default() -> Self {
        Self {
            daily: String::new(),
            worktree_retention: Duration::from_secs(14 * 86400),
            shadow_min_age: Duration::from_secs(7 * 86400),
            event_retention: Duration::from_secs(30 * 86400),
            run_retention: Duration::from_secs(30 * 86400),
            session_files: false,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawVerify {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_verify_timeout")]
    pub timeout: String,
    #[serde(default = "default_verify_retries")]
    pub max_retries: u32,
    #[serde(default)]
    pub commands: Vec<String>,
    #[serde(default = "default_verify_output")]
    pub max_output_bytes: usize,
    /// When true (default), auto-detection runs only for listed `[[verify.repo]]` entries.
    #[serde(default = "default_true")]
    pub only_listed_repos: bool,
    /// Restore base versions of matching tests before verify.
    #[serde(default = "default_true")]
    pub protect_tests: bool,
    #[serde(default = "default_protect_skip_label")]
    pub protect_tests_skip_label: String,
    #[serde(default)]
    pub test_globs: Vec<String>,
    #[serde(default, rename = "repo")]
    pub repos: Vec<RawVerifyRepo>,
}

impl Default for RawVerify {
    fn default() -> Self {
        Self {
            enabled: false,
            timeout: default_verify_timeout(),
            max_retries: default_verify_retries(),
            commands: Vec::new(),
            max_output_bytes: default_verify_output(),
            only_listed_repos: true,
            protect_tests: true,
            protect_tests_skip_label: default_protect_skip_label(),
            test_globs: Vec::new(),
            repos: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawVerifyRepo {
    #[serde(default)]
    pub path: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub commands: Vec<String>,
    #[serde(default)]
    pub test_globs: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct VerifyConfig {
    pub enabled: bool,
    pub timeout: Duration,
    pub max_retries: u32,
    pub commands: Vec<String>,
    pub max_output_bytes: usize,
    pub only_listed_repos: bool,
    pub protect_tests: bool,
    pub protect_tests_skip_label: String,
    pub test_globs: Vec<String>,
    pub repos: Vec<VerifyRepo>,
}

#[derive(Debug, Clone)]
pub struct VerifyRepo {
    pub path: PathBuf,
    pub enabled: bool,
    pub commands: Vec<String>,
    pub test_globs: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawChecklist {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_checklist_bytes")]
    pub max_bytes: usize,
}

impl Default for RawChecklist {
    fn default() -> Self {
        Self {
            enabled: true,
            max_bytes: default_checklist_bytes(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ChecklistConfig {
    pub enabled: bool,
    pub max_bytes: usize,
}

/// Per-kind defaults so a `[[provider]]` can be just `name` + `kind`.
pub struct KindDefaults {
    pub cli_names: &'static [&'static str],
    pub model_flag: &'static str,
    pub args: &'static [&'static str],
    pub resume_flag: &'static str,
    pub session_id_flag: &'static str,
    pub cwd_flag: &'static str,
    pub permission_mode: &'static str,
    pub permission_mode_flag: &'static str,
    pub prompt_mode: &'static str,
    pub prompt_file_prefix: &'static str,
    pub reported_usage: &'static str,
    pub sandbox: &'static str,
    pub config_overrides: &'static [&'static str],
}

pub fn kind_defaults(kind: &str) -> Option<KindDefaults> {
    match kind {
        "claude" => Some(KindDefaults {
            cli_names: &["claude"],
            model_flag: "--model",
            args: &["-p", "--output-format", "stream-json", "--verbose"],
            resume_flag: "--resume",
            session_id_flag: "",
            cwd_flag: "",
            permission_mode: "bypassPermissions",
            permission_mode_flag: "--permission-mode",
            prompt_mode: "stdin",
            prompt_file_prefix: "@",
            reported_usage: "claude_stream",
            sandbox: "",
            config_overrides: &[],
        }),
        "grok" => Some(KindDefaults {
            cli_names: &["grok"],
            model_flag: "--model",
            args: &["--output-format", "streaming-json", "--always-approve"],
            resume_flag: "--resume",
            session_id_flag: "--session-id",
            cwd_flag: "--cwd",
            permission_mode: "",
            permission_mode_flag: "--permission-mode",
            prompt_mode: "file",
            prompt_file_prefix: "--prompt-file=",
            reported_usage: "grok_billing",
            sandbox: "",
            config_overrides: &[],
        }),
        "cursor" => Some(KindDefaults {
            cli_names: &["cursor-agent"],
            model_flag: "--model",
            args: &["-p", "--force", "--output-format", "stream-json"],
            resume_flag: "--resume",
            session_id_flag: "",
            cwd_flag: "",
            permission_mode: "",
            permission_mode_flag: "--permission-mode",
            prompt_mode: "stdin",
            prompt_file_prefix: "@",
            reported_usage: "none",
            sandbox: "",
            config_overrides: &[],
        }),
        "opencode" => Some(KindDefaults {
            cli_names: &["opencode"],
            model_flag: "--model",
            args: &[],
            resume_flag: "--session",
            session_id_flag: "",
            cwd_flag: "--dir",
            permission_mode: "",
            permission_mode_flag: "--permission-mode",
            prompt_mode: "stdin",
            prompt_file_prefix: "@",
            reported_usage: "none",
            sandbox: "",
            config_overrides: &[],
        }),
        "codex" => Some(KindDefaults {
            cli_names: &["codex"],
            model_flag: "-m",
            args: &[],
            resume_flag: "",
            session_id_flag: "",
            cwd_flag: "",
            permission_mode: "",
            permission_mode_flag: "--permission-mode",
            prompt_mode: "stdin",
            prompt_file_prefix: "@",
            reported_usage: "none",
            sandbox: "workspace-write",
            config_overrides: &["model_provider=\"openai\""],
        }),
        _ => None,
    }
}

pub fn is_known_kind(kind: &str) -> bool {
    matches!(kind, "claude" | "grok" | "cursor" | "codex" | "opencode")
}

fn fill_string(configured: String, kind_val: &str) -> String {
    if configured.trim().is_empty() {
        kind_val.to_string()
    } else {
        configured
    }
}

pub fn find_in_path(name: &str) -> Option<PathBuf> {
    if name.is_empty() {
        return None;
    }
    let p = Path::new(name);
    if p.is_absolute() && p.exists() {
        return Some(p.to_path_buf());
    }
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let cand = dir.join(name);
        if cand.is_file() {
            return Some(cand);
        }
    }
    None
}

pub fn resolve_cli(kind: &str, configured: &str) -> PathBuf {
    if !configured.trim().is_empty() {
        return expand_path(configured);
    }
    if let Some(d) = kind_defaults(kind) {
        for name in d.cli_names {
            if let Some(p) = find_in_path(name) {
                return p;
            }
        }
        return PathBuf::from(d.cli_names.first().copied().unwrap_or(kind));
    }
    PathBuf::from(kind)
}

pub fn expand_path(p: &str) -> PathBuf {
    PathBuf::from(shellexpand::tilde(p.trim()).into_owned())
}

/// Always `~/.config/atr` on every OS. `dirs::config_dir()` is not used:
/// on macOS it would be `~/Library/Application Support`.
pub fn default_atr_dir() -> PathBuf {
    expand_path("~/.config/atr")
}

pub fn default_config_path() -> PathBuf {
    if let Ok(p) = std::env::var("ATR_CONFIG") {
        if !p.is_empty() {
            return expand_path(&p);
        }
    }
    default_atr_dir().join("config.toml")
}

/// SQLite lives under XDG data, not next to config. A Mini upgrade that
/// pointed `state_db` at `~/.config/atr/state.db` would otherwise open a
/// brand-new empty database and drop the live task/run history.
pub fn default_state_db_path() -> PathBuf {
    expand_path("~/.local/share/atr/state.db")
}

/// If the configured path is the default and does not exist yet, reuse an
/// existing `~/.config/atr/state.db` instead of starting a silent empty DB.
pub fn resolve_state_db(configured: PathBuf) -> PathBuf {
    resolve_state_db_paths(
        configured,
        expand_path("~/.config/atr/state.db"),
        default_state_db_path(),
    )
}

pub fn resolve_state_db_paths(
    configured: PathBuf,
    legacy: PathBuf,
    default_new: PathBuf,
) -> PathBuf {
    if configured.exists() {
        return configured;
    }
    if configured == default_new && legacy.exists() {
        tracing::warn!(
            configured = %configured.display(),
            existing = %legacy.display(),
            "configured state_db does not exist; using existing ~/.config/atr/state.db so an upgrade does not start a fresh database"
        );
        return legacy;
    }
    configured
}

pub fn load_config(path: &Path) -> Result<Config> {
    if !path.exists() {
        bail!("config not found at {}; run atr init", path.display());
    }
    let text =
        std::fs::read_to_string(path).with_context(|| format!("read config {}", path.display()))?;
    parse_config_toml(&text)
}

pub fn parse_config_toml(text: &str) -> Result<Config> {
    match toml::from_str::<RawConfig>(text) {
        Ok(raw) => Config::from_raw(raw),
        Err(e) => Err(unknown_key_error(e)),
    }
}

pub fn valid_repo_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// `owner/name` from a slug, HTTPS URL, or `git@github.com:` URL.
pub fn parse_github_slug(raw: &str) -> Result<String> {
    let s = raw.trim();
    let s = s.trim_end_matches('/');
    let s = s.trim_end_matches(".git");
    let s = s.strip_prefix("https://github.com/").unwrap_or(s);
    let s = s.strip_prefix("http://github.com/").unwrap_or(s);
    let s = s.strip_prefix("git@github.com:").unwrap_or(s);
    let s = s.strip_prefix("ssh://git@github.com/").unwrap_or(s);
    let s = s.trim_start_matches('/');
    let parts: Vec<_> = s.split('/').filter(|p| !p.is_empty()).collect();
    if parts.len() != 2
        || parts
            .iter()
            .any(|p| p.chars().any(|c| c.is_whitespace() || c == ':'))
    {
        bail!("github repo must be owner/name, got {raw:?}");
    }
    Ok(format!("{}/{}", parts[0], parts[1]))
}

pub fn repo_short_name(github: &str) -> String {
    github
        .rsplit('/')
        .next()
        .unwrap_or(github)
        .trim()
        .to_string()
}

pub fn short_repo_name(repo: Option<&str>) -> String {
    let Some(r) = repo.filter(|s| !s.is_empty()) else {
        return "-".into();
    };
    Path::new(r)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(r)
        .to_string()
}

pub fn paths_eq(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(x), Ok(y)) => x == y,
        _ => a.as_os_str() == b.as_os_str(),
    }
}

fn unknown_key_error(err: toml::de::Error) -> anyhow::Error {
    let msg = err.to_string();
    if let Some(key) = unknown_field_name(&msg) {
        let known = expected_fields(&msg);
        if let Some(near) = nearest_key(&key, &known) {
            return anyhow::anyhow!("unknown config key `{key}`; nearest valid key is `{near}`");
        }
        return anyhow::anyhow!("unknown config key `{key}`");
    }
    anyhow::Error::new(err).context("parse config toml")
}

fn unknown_field_name(msg: &str) -> Option<String> {
    let after = msg.split("unknown field `").nth(1)?;
    let key = after.split('`').next()?;
    if key.is_empty() {
        None
    } else {
        Some(key.to_string())
    }
}

fn expected_fields(msg: &str) -> Vec<String> {
    let after = match msg.split("expected one of ").nth(1) {
        Some(s) => s,
        None => return Vec::new(),
    };
    after
        .split('`')
        .enumerate()
        .filter_map(|(i, s)| {
            if i % 2 == 1 && !s.is_empty() {
                Some(s.to_string())
            } else {
                None
            }
        })
        .collect()
}

fn nearest_key(got: &str, known: &[String]) -> Option<String> {
    known
        .iter()
        .map(|k| (levenshtein(got, k), k.clone()))
        .min_by_key(|(d, _)| *d)
        .filter(|(d, k)| *d <= 3 || *d * 2 <= k.len().max(got.len()))
        .map(|(_, k)| k)
}

fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.iter().enumerate() {
        let mut cur = vec![i + 1];
        for (j, cb) in b.iter().enumerate() {
            let cost = if ca == cb { 0 } else { 1 };
            cur.push((prev[j + 1] + 1).min(cur[j] + 1).min(prev[j] + cost));
        }
        prev = cur;
    }
    prev[b.len()]
}

fn fill_model_from_preset(configured: String, preset: &str, presets: &[Preset]) -> String {
    if !configured.trim().is_empty() {
        return configured;
    }
    if preset.is_empty() {
        return String::new();
    }
    if let Some(b) = crate::presets::builtin(preset) {
        return b.model.to_string();
    }
    presets
        .iter()
        .find(|p| p.name == preset)
        .map(|p| p.model.clone())
        .unwrap_or_default()
}

impl Config {
    pub fn from_raw(raw: RawConfig) -> Result<Self> {
        if raw.providers.is_empty() {
            bail!("config has no [[provider]] entries; order must be set in TOML");
        }
        let mut presets = Vec::with_capacity(raw.presets.len());
        let mut preset_names = std::collections::HashSet::new();
        for pr in raw.presets {
            if !preset_names.insert(pr.name.clone()) {
                bail!("duplicate preset name {:?}", pr.name);
            }
            if !is_known_kind(&pr.kind) {
                bail!(
                    "preset {:?} has unknown kind {}; use claude, grok, cursor, codex, or opencode",
                    pr.name,
                    pr.kind
                );
            }
            presets.push(Preset {
                name: pr.name,
                kind: pr.kind,
                model: pr.model,
                cli: pr.cli,
            });
        }
        let mut providers = Vec::with_capacity(raw.providers.len());
        let mut seen = std::collections::HashSet::new();
        for p in raw.providers {
            if !seen.insert(p.name.clone()) {
                bail!("duplicate provider name {:?}", p.name);
            }
            if !is_known_kind(&p.kind) {
                bail!(
                    "provider {:?} has unknown kind {}; use claude, grok, cursor, codex, or opencode",
                    p.name,
                    p.kind
                );
            }
            let kd = kind_defaults(&p.kind);
            let kd_ref = kd.as_ref();
            let reported_raw = if p.reported_usage.trim().is_empty() {
                kd_ref
                    .map(|k| k.reported_usage.to_string())
                    .unwrap_or_else(|| "none".into())
            } else {
                p.reported_usage.trim().to_ascii_lowercase()
            };
            let reported_usage = match reported_raw.as_str() {
                "" | "none" => "none".into(),
                "claude_stream" | "grok_billing" => reported_raw,
                other => bail!(
                    "provider {:?} reported_usage {other:?} must be claude_stream, grok_billing, or none",
                    p.name
                ),
            };
            if !(0.0..=1.0).contains(&p.skip_at) {
                bail!(
                    "provider {:?} skip_at {} must be between 0 and 1",
                    p.name,
                    p.skip_at
                );
            }
            let args = if p.args.is_empty() {
                kd_ref
                    .map(|k| k.args.iter().map(|s| (*s).to_string()).collect())
                    .unwrap_or_default()
            } else {
                p.args
            };
            let config_overrides = match p.config_overrides {
                Some(v) => v,
                None => kd_ref
                    .map(|k| {
                        k.config_overrides
                            .iter()
                            .map(|s| (*s).to_string())
                            .collect()
                    })
                    .unwrap_or_default(),
            };
            let heartbeat_raw = if !p.health.heartbeat_timeout.trim().is_empty() {
                p.health.heartbeat_timeout.clone()
            } else if !p.heartbeat_timeout.trim().is_empty() {
                p.heartbeat_timeout.clone()
            } else {
                default_heartbeat()
            };
            let model = fill_model_from_preset(p.model, &p.preset, &presets);
            let keep_alive = if p.keep_alive.trim().is_empty() {
                default_keep_alive(&p.preset, &model)
            } else {
                p.keep_alive.trim().to_string()
            };
            providers.push(Provider {
                name: p.name,
                kind: p.kind.clone(),
                preset: p.preset.clone(),
                enabled: p.enabled,
                cli: resolve_cli(&p.kind, &p.cli),
                expected_version: p.expected_version.filter(|s| !s.is_empty()),
                version_args: p.version_args,
                model,
                model_flag: fill_string(
                    p.model_flag,
                    kd_ref.map(|k| k.model_flag).unwrap_or("--model"),
                ),
                max_parallel: p.max_parallel.max(1),
                cooldown_after_limit: parse_duration(&p.cooldown_after_limit)?,
                degraded_after_stall: parse_duration(&p.degraded_after_stall)?,
                heartbeat_timeout: parse_duration(&heartbeat_raw)?,
                run_timeout: parse_duration(&p.run_timeout)?,
                resume_flag: fill_string(
                    p.resume_flag,
                    kd_ref.map(|k| k.resume_flag).unwrap_or(""),
                ),
                session_id_flag: fill_string(
                    p.session_id_flag,
                    kd_ref.map(|k| k.session_id_flag).unwrap_or(""),
                ),
                cwd_flag: fill_string(p.cwd_flag, kd_ref.map(|k| k.cwd_flag).unwrap_or("")),
                permission_mode: fill_string(
                    p.permission_mode,
                    kd_ref.map(|k| k.permission_mode).unwrap_or(""),
                ),
                permission_mode_flag: fill_string(
                    p.permission_mode_flag,
                    kd_ref
                        .map(|k| k.permission_mode_flag)
                        .unwrap_or("--permission-mode"),
                ),
                prompt_mode: fill_string(
                    p.prompt_mode,
                    kd_ref.map(|k| k.prompt_mode).unwrap_or("file"),
                ),
                prompt_file_prefix: fill_string(
                    p.prompt_file_prefix,
                    kd_ref.map(|k| k.prompt_file_prefix).unwrap_or("@"),
                ),
                profile: p.profile,
                sandbox: fill_string(p.sandbox, kd_ref.map(|k| k.sandbox).unwrap_or("")),
                args,
                extra_args: p.extra_args,
                config_overrides,
                quota: {
                    let mut q = Vec::new();
                    for w in p.quota {
                        if w.budget <= 0.0 {
                            continue;
                        }
                        let threshold = if w.threshold <= 0.0 {
                            default_quota_threshold()
                        } else {
                            w.threshold.min(1.0)
                        };
                        q.push(QuotaWindow {
                            window: parse_duration(&w.window)?,
                            window_raw: w.window,
                            budget: w.budget,
                            unit: QuotaUnit::parse(&w.unit)?,
                            threshold,
                        });
                    }
                    q
                },
                reported_usage,
                skip_at: p.skip_at,
                reported_usage_interval: parse_duration(
                    if p.reported_usage_interval.trim().is_empty() {
                        "10m"
                    } else {
                        &p.reported_usage_interval
                    },
                )?,
                reported_usage_timeout: parse_duration(
                    if p.reported_usage_timeout.trim().is_empty() {
                        "20s"
                    } else {
                        &p.reported_usage_timeout
                    },
                )?,
                health: ProviderHealth {
                    startup_timeout: parse_duration(
                        if p.health.startup_timeout.trim().is_empty() {
                            "90s"
                        } else {
                            &p.health.startup_timeout
                        },
                    )?,
                    heartbeat_timeout: parse_duration(&heartbeat_raw)?,
                    stall_resumes: p.health.stall_resumes.unwrap_or(1),
                    unhealthy_cooldown: parse_duration(
                        if p.health.unhealthy_cooldown.trim().is_empty() {
                            "10m"
                        } else {
                            &p.health.unhealthy_cooldown
                        },
                    )?,
                },
                plugin: p.plugin,
                max_turns: p.max_turns,
                cache_window: if p.cache_window.trim().is_empty() {
                    default_cache_window(&p.kind)
                } else {
                    parse_duration(&p.cache_window)?
                },
                system_prompt_flag: if p.system_prompt_flag.trim().is_empty() {
                    default_system_prompt_flag(&p.kind).to_string()
                } else {
                    p.system_prompt_flag.trim().to_string()
                },
                keep_alive,
            });
        }
        Ok(Config {
            listen: raw.listen,
            state_db: resolve_state_db(expand_path(&raw.state_db)),
            worktree_root: expand_path(&raw.worktree_root),
            repo_path: expand_path(&raw.repo_path),
            repo_default_branch: raw.repo_default_branch,
            worktree_branch_prefix: raw.worktree_branch_prefix,
            system_prompt_file: expand_path(&raw.system_prompt_file),
            cache_hit_warn_threshold: raw.cache_hit_warn_threshold,
            shell_command_timeout: parse_duration(&raw.shell_command_timeout)?,
            shell_command_timeout_raw: raw.shell_command_timeout,
            task_timeout: parse_duration(&raw.task_timeout)?,
            kill_grace: parse_duration(&raw.kill_grace)?,
            dry_run: raw.dry_run,
            task_key_prefix: raw.task_key_prefix,
            display_timezone: crate::tz::DisplayTz::parse(&raw.display_timezone)?,
            github_poll_interval: parse_duration(if raw.github.poll_interval.trim().is_empty() {
                "60s"
            } else {
                &raw.github.poll_interval
            })?,
            github: raw.github,
            dashboard: DashboardConfig {
                enabled: raw.dashboard.enabled,
                bind: if raw.dashboard.bind.trim().is_empty() {
                    default_dashboard_bind()
                } else {
                    raw.dashboard.bind.trim().to_string()
                },
            },
            telegram: {
                let t = raw.telegram;
                TelegramConfig {
                    enabled: t.enabled,
                    bot_token_env: if t.bot_token_env.trim().is_empty() {
                        default_tg_token_env()
                    } else {
                        t.bot_token_env
                    },
                    allowed_chat_ids: t.allowed_chat_ids,
                    token_file: t.token_file,
                    chat_id_file: t.chat_id_file,
                    prefix: if t.prefix.is_empty() {
                        default_tg_prefix()
                    } else {
                        t.prefix
                    },
                    notify_interval: parse_duration(&t.notify_interval)
                        .unwrap_or(Duration::from_secs(20)),
                    daily_summary: t.daily_summary,
                    api_base: t.api_base,
                    assistant: TelegramAssistant {
                        enabled: t.assistant.enabled,
                        provider: t.assistant.provider,
                        timeout: parse_duration(&t.assistant.timeout)
                            .unwrap_or(Duration::from_secs(600)),
                    },
                }
            },
            handoff: HandoffConfig {
                max_bytes: raw.handoff.max_bytes.max(256),
                include_diff: raw.handoff.include_diff,
            },
            shadow: ShadowConfig {
                enabled: raw.shadow.enabled,
                repo: raw.shadow.repo,
                labels: raw.shadow.labels,
                poll_interval: parse_duration(&raw.shadow.poll_interval)?,
                max_parallel: raw.shadow.max_parallel.max(1),
                key_prefix: raw.shadow.key_prefix,
                repo_path: if raw.shadow.repo_path.trim().is_empty() {
                    PathBuf::new()
                } else {
                    expand_path(&raw.shadow.repo_path)
                },
                gh_token_env: if raw.shadow.gh_token_env.trim().is_empty() {
                    default_shadow_token_env()
                } else {
                    raw.shadow.gh_token_env
                },
                verify_mode: match raw.shadow.verify_mode.trim().to_ascii_lowercase().as_str() {
                    "" | "record" => ShadowVerifyMode::Record,
                    "retry" => ShadowVerifyMode::Retry,
                    other => bail!("shadow.verify_mode {other:?} must be record or retry"),
                },
            },
            verify: VerifyConfig {
                enabled: raw.verify.enabled,
                timeout: parse_duration(&raw.verify.timeout)?,
                max_retries: raw.verify.max_retries,
                commands: raw.verify.commands,
                max_output_bytes: raw.verify.max_output_bytes.max(256),
                only_listed_repos: raw.verify.only_listed_repos,
                protect_tests: raw.verify.protect_tests,
                protect_tests_skip_label: if raw.verify.protect_tests_skip_label.trim().is_empty() {
                    default_protect_skip_label()
                } else {
                    raw.verify.protect_tests_skip_label
                },
                test_globs: if raw.verify.test_globs.is_empty() {
                    default_test_globs()
                } else {
                    raw.verify.test_globs
                },
                repos: raw
                    .verify
                    .repos
                    .into_iter()
                    .map(|r| VerifyRepo {
                        path: expand_path(&r.path),
                        enabled: r.enabled,
                        commands: r.commands,
                        test_globs: r.test_globs,
                    })
                    .collect(),
            },
            checklist: ChecklistConfig {
                enabled: raw.checklist.enabled,
                max_bytes: raw.checklist.max_bytes.max(256),
            },
            log: LogConfig {
                path: if raw.log.path.trim().is_empty() {
                    PathBuf::new()
                } else {
                    expand_path(&raw.log.path)
                },
                max_bytes: crate::duration::parse_bytes(if raw.log.max_bytes.trim().is_empty() {
                    "5m"
                } else {
                    &raw.log.max_bytes
                })
                .unwrap_or(5 * 1024 * 1024),
                keep: raw.log.keep.max(1),
            },
            update: UpdateConfig {
                github_repo: if raw.update.github_repo.trim().is_empty() {
                    default_update_repo()
                } else {
                    raw.update.github_repo
                },
            },
            repos: {
                let mut seen = std::collections::BTreeSet::new();
                raw.repos
                    .into_iter()
                    .map(|r| {
                        let name = r.name.trim().to_string();
                        if !valid_repo_name(&name) {
                            bail!("[[repo]] name {name:?} must be letters, digits, '-' or '_'");
                        }
                        if !seen.insert(name.clone()) {
                            bail!("duplicate [[repo]] name {name}");
                        }
                        Ok(RepoEntry {
                            name,
                            path: expand_path(&r.path),
                            github: r.github.trim().to_string(),
                            label: if r.label.trim().is_empty() {
                                default_gh_label()
                            } else {
                                r.label
                            },
                            enabled: r.enabled,
                            branch_prefix: r.branch_prefix.trim().to_string(),
                            base: r.base.trim().to_string(),
                            system_prompt_file: if r.system_prompt_file.trim().is_empty() {
                                PathBuf::new()
                            } else {
                                expand_path(&r.system_prompt_file)
                            },
                            max_parallel: r.max_parallel,
                            shorthand: false,
                        })
                    })
                    .collect::<Result<Vec<_>>>()?
            },
            gc: GcConfig {
                daily: raw.gc.daily,
                worktree_retention: parse_duration(
                    if raw.gc.worktree_retention.trim().is_empty() {
                        "14d"
                    } else {
                        &raw.gc.worktree_retention
                    },
                )?,
                shadow_min_age: parse_duration(if raw.gc.shadow_min_age.trim().is_empty() {
                    "7d"
                } else {
                    &raw.gc.shadow_min_age
                })?,
                event_retention: parse_duration(if raw.gc.event_retention.trim().is_empty() {
                    "30d"
                } else {
                    &raw.gc.event_retention
                })?,
                run_retention: parse_duration(if raw.gc.run_retention.trim().is_empty() {
                    "30d"
                } else {
                    &raw.gc.run_retention
                })?,
                session_files: raw.gc.session_files,
            },
            presets,
            providers,
            driver: raw.driver.trim().to_string(),
            planner: {
                let p = match raw.planner {
                    RawPlannerIn::Mode(mode) => RawPlanner {
                        mode,
                        ..RawPlanner::default()
                    },
                    RawPlannerIn::Table(t) => t,
                };
                PlannerConfig {
                    mode: PlannerMode::parse(&p.mode)?,
                    auto_chars: p.auto_chars.max(1),
                    max_turns: p.max_turns.max(1),
                    max_subtasks: p.max_subtasks.max(1),
                }
            },
            review: ReviewConfig {
                enabled: raw.review.enabled,
                max_rounds: raw.review.max_rounds.max(1),
                max_turns: raw.review.max_turns.max(1),
            },
            plugin: PluginConfig {
                test_command: raw.plugin.test_command.trim().to_string(),
            },
            mcp: {
                let token_file = if raw.mcp.token_file.trim().is_empty() {
                    default_mcp_token_file()
                } else {
                    raw.mcp.token_file.trim().to_string()
                };
                McpConfig {
                    enabled: raw.mcp.enabled,
                    token_env: if raw.mcp.token_env.trim().is_empty() {
                        default_mcp_token_env()
                    } else {
                        raw.mcp.token_env.trim().to_string()
                    },
                    token_file: expand_path(&token_file),
                }
            },
            cache: CacheConfig {
                worktree_pool: raw.cache.worktree_pool,
                headline_min_input: raw.cache.headline_min_input.max(0),
            },
            sources: {
                let mut out = Vec::new();
                for s in raw.sources {
                    let kind = crate::task::SourceKind::parse(&s.kind)?;
                    if kind == crate::task::SourceKind::Inbox
                        && s.dir.trim().is_empty()
                        && s.enabled
                    {
                        bail!("[[source]] kind = \"inbox\" needs dir");
                    }
                    out.push(SourceEntry {
                        kind,
                        enabled: s.enabled,
                        dir: if s.dir.trim().is_empty() {
                            PathBuf::new()
                        } else {
                            expand_path(&s.dir)
                        },
                        poll_interval: parse_duration(if s.poll_interval.trim().is_empty() {
                            "5s"
                        } else {
                            &s.poll_interval
                        })?,
                    });
                }
                out
            },
        })
    }

    pub fn preset(&self, name: &str) -> Option<&Preset> {
        self.presets.iter().find(|p| p.name == name)
    }

    pub fn provider(&self, name: &str) -> Option<&Provider> {
        self.providers.iter().find(|p| p.name == name)
    }

    /// Configured driver name, or grok when enabled, else the first enabled
    /// provider. Empty when nothing is enabled.
    pub fn driver_name(&self) -> Option<String> {
        let want = self.driver.trim();
        if !want.is_empty() {
            return Some(want.to_string());
        }
        if self.provider("grok").is_some_and(|p| p.enabled) {
            return Some("grok".into());
        }
        self.providers
            .iter()
            .find(|p| p.enabled)
            .map(|p| p.name.clone())
    }

    pub fn driver(&self) -> Option<&Provider> {
        self.driver_name().and_then(|n| self.provider(&n))
    }

    pub fn worker_max_turns(provider: &Provider) -> u32 {
        if provider.max_turns > 0 {
            provider.max_turns
        } else {
            30
        }
    }

    pub fn has_explicit_repos(&self) -> bool {
        !self.repos.is_empty()
    }

    /// Watch targets: explicit `[[repo]]` when present, otherwise one entry
    /// synthesized from `repo_path` + `[github].repo` / `label`.
    pub fn effective_repos(&self) -> Vec<RepoEntry> {
        if !self.repos.is_empty() {
            return self.repos.clone();
        }
        vec![self.shorthand_repo()]
    }

    pub fn shorthand_repo(&self) -> RepoEntry {
        RepoEntry {
            name: self.task_key_prefix.clone(),
            path: self.repo_path.clone(),
            github: self.github.repo.clone(),
            label: self.github.label.clone(),
            enabled: true,
            branch_prefix: String::new(),
            base: String::new(),
            system_prompt_file: PathBuf::new(),
            max_parallel: 0,
            shorthand: true,
        }
    }

    pub fn enabled_watch_repos(&self) -> Vec<RepoEntry> {
        self.effective_repos()
            .into_iter()
            .filter(|r| r.enabled)
            .collect()
    }

    pub fn repo_by_name(&self, name: &str) -> Option<RepoEntry> {
        self.effective_repos().into_iter().find(|r| r.name == name)
    }

    pub fn resolve_repo_filter(&self, filter: &str) -> Option<RepoEntry> {
        let f = filter.trim();
        if f.is_empty() {
            return None;
        }
        self.effective_repos().into_iter().find(|r| {
            r.name == f
                || r.github == f
                || r.path.as_os_str() == std::ffi::OsStr::new(f)
                || r.path.to_string_lossy() == f
                || r.path.file_name().and_then(|n| n.to_str()) == Some(f)
                || r.github.rsplit('/').next() == Some(f)
        })
    }

    pub fn repo_for_path(&self, path: &Path) -> Option<RepoEntry> {
        self.effective_repos()
            .into_iter()
            .find(|r| paths_eq(&r.path, path))
    }

    pub fn repo_for_task(&self, task: &crate::db::Task) -> Option<RepoEntry> {
        let repos = self.effective_repos();
        if let Some(path) = task.repo.as_deref() {
            if let Some(r) = repos.iter().find(|r| paths_eq(&r.path, Path::new(path))) {
                return Some(r.clone());
            }
        }
        if let Some(r) = repos
            .iter()
            .find(|r| task.key == r.name || task.key.starts_with(&format!("{}-", r.name)))
        {
            return Some(r.clone());
        }
        None
    }

    pub fn task_repo_name(&self, task: &crate::db::Task) -> String {
        if let Some(r) = self.repo_for_task(task) {
            return r.name;
        }
        short_repo_name(task.repo.as_deref())
    }

    pub fn task_matches_repo(&self, task: &crate::db::Task, filter: &str) -> bool {
        let f = filter.trim();
        if f.is_empty() {
            return true;
        }
        if let Some(r) = self.resolve_repo_filter(f) {
            if let Some(p) = task.repo.as_deref() {
                if paths_eq(Path::new(p), &r.path) {
                    return true;
                }
            }
            return self.task_repo_name(task) == r.name
                || task.key.starts_with(&format!("{}-", r.name));
        }
        task.repo
            .as_deref()
            .map(|p| p == f || Path::new(p).file_name().and_then(|n| n.to_str()) == Some(f))
            .unwrap_or(false)
            || self.task_repo_name(task) == f
    }

    pub fn issue_task_key(&self, repo: &RepoEntry, issue: i64) -> String {
        if repo.shorthand {
            format!("{}-{}", self.task_key_prefix, issue)
        } else {
            format!("{}-{}", repo.name, issue)
        }
    }

    pub fn listen_is_loopback(&self) -> bool {
        self.listen.starts_with("127.0.0.1:")
            || self.listen.starts_with("localhost:")
            || self.listen.starts_with("[::1]:")
    }

    /// Required settings that `atr serve` will not start without.
    pub fn missing_required(&self) -> Vec<&'static str> {
        let mut missing = Vec::new();
        if self.listen.trim().is_empty() {
            missing.push("listen");
        }
        if self.worktree_root.as_os_str().is_empty() {
            missing.push("worktree_root");
        }
        if self.intake_allows(crate::task::SourceKind::Github) {
            if self.repos.is_empty() {
                if self.repo_path.as_os_str().is_empty() {
                    missing.push("repo_path");
                }
            } else if !self.repos.iter().any(|r| !r.path.as_os_str().is_empty()) {
                missing.push("[[repo]].path");
            }
        } else if !self.repos.is_empty()
            && !self.repos.iter().any(|r| !r.path.as_os_str().is_empty())
        {
            missing.push("[[repo]].path");
        }
        if self.system_prompt_file.as_os_str().is_empty() {
            missing.push("system_prompt_file");
        }
        if self.providers.is_empty() {
            missing.push("[[provider]]");
        }
        if self.intake_allows(crate::task::SourceKind::Github) {
            let watch = self.effective_repos();
            let any = watch
                .iter()
                .any(|r| r.enabled && !r.github.trim().is_empty() && !r.label.trim().is_empty());
            if !any {
                if self.repos.is_empty() {
                    if self.github.repo.trim().is_empty() {
                        missing.push("github.repo");
                    }
                    if self.github.label.trim().is_empty() {
                        missing.push("github.label");
                    }
                } else {
                    missing.push("[[repo]].github");
                }
            }
        }
        if self.telegram.enabled && self.telegram.allowed_chat_ids.is_empty() {
            missing.push("telegram.allowed_chat_ids");
        }
        missing
    }

    pub fn validate_for_serve(&self) -> Result<()> {
        let missing = self.missing_required();
        if missing.is_empty() {
            return Ok(());
        }
        bail!(
            "cannot start atr serve; set these required settings: {}",
            missing.join(", ")
        )
    }

    /// Whether intake of this kind is on. An explicit `[[source]]` for that
    /// kind wins. Otherwise: GitHub follows `[github].enabled`, Telegram
    /// follows `[telegram].enabled`, inbox is off, MCP/CLI/HTTP are on.
    pub fn intake_allows(&self, kind: crate::task::SourceKind) -> bool {
        if let Some(s) = self.sources.iter().find(|s| s.kind == kind) {
            return s.enabled;
        }
        match kind {
            crate::task::SourceKind::Github => self.github.enabled,
            crate::task::SourceKind::Telegram => self.telegram.enabled,
            crate::task::SourceKind::Inbox => false,
            crate::task::SourceKind::Mcp
            | crate::task::SourceKind::Cli
            | crate::task::SourceKind::Http => true,
        }
    }

    pub fn inbox_sources(&self) -> Vec<&SourceEntry> {
        self.sources
            .iter()
            .filter(|s| s.kind == crate::task::SourceKind::Inbox && s.enabled)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn example_config_parses() {
        let text = std::fs::read_to_string("config/example.toml").unwrap();
        let cfg = parse_config_toml(&text).unwrap();
        let names: Vec<_> = cfg.providers.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["claude", "grok", "cursor", "codex"]);
        let joined = text.to_ascii_lowercase();
        for banned in ["acme", "com.rimas", "/opt/homebrew", "for:legacy"] {
            assert!(
                !joined.contains(banned),
                "example must stay neutral, found {banned}"
            );
        }
        let grok = cfg.provider("grok").unwrap();
        assert!(!grok.args.iter().any(|a| a == "-p"));
        assert_eq!(grok.prompt_file_prefix, "--prompt-file=");
        assert_eq!(grok.prompt_mode, "file");
        assert_eq!(grok.session_id_flag, "--session-id");
        assert_eq!(grok.resume_flag, "--resume");
        assert_eq!(grok.reported_usage, "grok_billing");
        assert!(cfg.cache.worktree_pool);
        assert_eq!(cfg.cache.headline_min_input, 40_000);
        let claude = cfg.provider("claude").unwrap();
        assert_eq!(claude.permission_mode, "bypassPermissions");
        assert_eq!(claude.permission_mode_flag, "--permission-mode");
        assert_eq!(claude.cache_window, Duration::from_secs(5 * 60));
        assert_eq!(claude.system_prompt_flag, "--append-system-prompt-file");
        assert_eq!(grok.system_prompt_flag, "--rules");
        assert_eq!(grok.cache_window, Duration::from_secs(60 * 60));
        assert_eq!(claude.reported_usage, "claude_stream");
        assert_eq!(claude.prompt_mode, "stdin");
        assert!(
            cfg.state_db
                .to_string_lossy()
                .ends_with(".local/share/atr/state.db")
                || cfg.state_db.exists()
                || cfg
                    .state_db
                    .to_string_lossy()
                    .ends_with(".config/atr/state.db"),
            "example must keep the data-dir default (or migrate), got {}",
            cfg.state_db.display()
        );
        let codex = cfg.provider("codex").unwrap();
        assert!(codex
            .config_overrides
            .iter()
            .any(|c| c.contains("model_provider")));
        assert!(codex.model.is_empty(), "codex model is optional");
        assert!(cfg.handoff.max_bytes >= 256);
        assert!(!cfg.shadow.enabled);
        assert_eq!(cfg.github.label, DEFAULT_GITHUB_LABEL);
        assert!(cfg.shadow.labels.is_empty());
        assert!(
            cfg.worktree_root.to_string_lossy().ends_with("atr-work"),
            "{}",
            cfg.worktree_root.display()
        );
        assert!(cfg.shadow.repo_path.as_os_str().is_empty());
        assert_eq!(cfg.task_key_prefix, "task");
        assert_eq!(cfg.worktree_branch_prefix, "atr");
        assert_eq!(cfg.update.github_repo, "rimusz/agent-task-runner");
        assert_eq!(cfg.shadow.verify_mode, ShadowVerifyMode::Record);
        assert_eq!(cfg.provider("cursor").unwrap().reported_usage, "none");
        assert_eq!(cfg.log.keep, 5);
        assert!(
            cfg.log
                .path
                .to_string_lossy()
                .ends_with(".local/share/atr/atr.log"),
            "{}",
            cfg.log.path.display()
        );
        assert!(!cfg.gc.session_files);
        assert!(cfg.mcp.enabled);
        assert_eq!(cfg.mcp.token_env, "ATR_MCP_TOKEN");
        assert!(text.contains("name = \"spark\""));
        assert!(text.contains("kind = \"codex\""));
        assert!(text.contains("kind = \"opencode\""));
        assert!(text.contains("preset = \"spark\""));
        let kimi = crate::presets::builtin("kimi").unwrap();
        assert_eq!(kimi.kind, "opencode");
        assert_eq!(kimi.model, "moonshotai/kimi-k2.6");
        assert!(crate::presets::builtin("ollama").is_some());
        assert_eq!(crate::presets::builtin("spark").unwrap().kind, "opencode");
        for p in &cfg.presets {
            assert!(
                p.cli.is_empty() || !p.cli.to_ascii_lowercase().contains("key"),
                "preset must not store keys"
            );
        }
    }

    #[test]
    fn provider_preset_fills_default_model() {
        let cfg = parse_config_toml(
            r#"
repo_path = "/tmp"
[[provider]]
name = "kimi"
kind = "opencode"
preset = "kimi"
"#,
        )
        .unwrap();
        let k = cfg.provider("kimi").unwrap();
        assert_eq!(k.preset, "kimi");
        assert_eq!(k.model, "moonshotai/kimi-k2.6");
        assert_eq!(k.kind, "opencode");
        assert_eq!(k.reported_usage, "none");
    }

    #[test]
    fn preset_rejects_api_key_field() {
        let err = parse_config_toml(
            r#"
repo_path = "/tmp"
[[preset]]
name = "kimi"
kind = "opencode"
model = "moonshotai/kimi-k2.6"
api_key = "secret"
[[provider]]
name = "claude"
kind = "claude"
"#,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("api_key"), "{err}");
    }

    #[test]
    fn unknown_config_key_names_nearest() {
        let err = parse_config_toml(
            r#"
listen = "127.0.0.1:0"
repo_path = "/tmp/repo"
worktree_root = "/tmp/wt"
system_prompt_file = "/tmp/p.md"
repoo_path = "/oops"
[[provider]]
name = "claude"
kind = "claude"
"#,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("repoo_path"), "{err}");
        assert!(err.contains("repo_path"), "{err}");
    }

    #[test]
    fn short_provider_block_fills_kind_defaults() {
        let cfg = parse_config_toml(
            r#"
repo_path = "/tmp/repo"
[[provider]]
name = "claude"
kind = "claude"
[[provider]]
name = "spark"
kind = "codex"
"#,
        )
        .unwrap();
        let c = cfg.provider("claude").unwrap();
        assert_eq!(c.reported_usage, "claude_stream");
        assert_eq!(c.permission_mode, "bypassPermissions");
        assert!(c.args.iter().any(|a| a == "-p"));
        let s = cfg.provider("spark").unwrap();
        assert_eq!(s.kind, "codex");
        assert!(s
            .config_overrides
            .iter()
            .any(|o| o.contains("model_provider")));
    }

    #[test]
    fn mini_shaped_verbose_config_still_parses() {
        let cfg = parse_config_toml(
            r#"
listen = "127.0.0.1:7420"
state_db = "~/.local/share/atr/state.db"
worktree_root = "~/atr-work"
repo_path = "~/src/acme"
worktree_branch_prefix = "otto"
task_key_prefix = "acme"
system_prompt_file = "~/.config/atr/system-prompt.md"
[github]
enabled = true
repo = "owner/acme"
label = "for:legacy"
[[provider]]
name = "claude"
kind = "claude"
enabled = true
cli = "/opt/homebrew/bin/claude"
expected_version = "2.1.287"
model = "opus"
heartbeat_timeout = "10m"
[provider.health]
heartbeat_timeout = "10m"
stall_resumes = 1
"#,
        )
        .unwrap();
        assert_eq!(cfg.task_key_prefix, "acme");
        assert_eq!(cfg.worktree_branch_prefix, "otto");
        assert_eq!(cfg.github.label, "for:legacy");
        assert!(
            cfg.repos.is_empty(),
            "Mini shorthand must not invent [[repo]]"
        );
        let watch = cfg.effective_repos();
        assert_eq!(watch.len(), 1);
        assert!(watch[0].shorthand);
        assert_eq!(watch[0].name, "acme");
        assert_eq!(watch[0].github, "owner/acme");
        assert_eq!(cfg.issue_task_key(&watch[0], 123), "acme-123");
        let omitted = parse_config_toml(
            r#"
listen = "127.0.0.1:7420"
repo_path = "~/src/acme"
system_prompt_file = "~/.config/atr/system-prompt.md"
[github]
enabled = true
repo = "owner/acme"
[[provider]]
name = "claude"
kind = "claude"
"#,
        )
        .unwrap();
        assert_eq!(omitted.github.label, DEFAULT_GITHUB_LABEL);
        let c = cfg.provider("claude").unwrap();
        assert_eq!(c.model, "opus");
        assert_eq!(c.cli, PathBuf::from("/opt/homebrew/bin/claude"));
        assert_eq!(c.health.stall_resumes, 1);
    }

    #[test]
    fn missing_config_tells_you_to_run_init() {
        let err = load_config(Path::new("/no/such/atr-config.toml"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("run atr init"), "{err}");
    }

    #[test]
    fn serve_names_empty_required_settings() {
        let cfg = parse_config_toml(
            r#"
repo_path = ""
[[provider]]
name = "claude"
kind = "claude"
"#,
        )
        .unwrap();
        cfg.validate_for_serve()
            .expect("github off: empty repo_path is allowed");
        let gh = parse_config_toml(
            r#"
repo_path = ""
[github]
enabled = true
[[provider]]
name = "claude"
kind = "claude"
"#,
        )
        .unwrap();
        let err = gh.validate_for_serve().unwrap_err().to_string();
        assert!(err.contains("repo_path"), "{err}");
    }

    #[test]
    fn source_table_overrides_and_defaults() {
        let none = parse_config_toml(
            r#"
repo_path = "/tmp/repo"
[[provider]]
name = "claude"
kind = "claude"
"#,
        )
        .unwrap();
        assert!(none.sources.is_empty());
        assert!(!none.intake_allows(crate::task::SourceKind::Github));
        assert!(none.intake_allows(crate::task::SourceKind::Mcp));
        assert!(none.intake_allows(crate::task::SourceKind::Cli));
        assert!(!none.intake_allows(crate::task::SourceKind::Inbox));
        let with = parse_config_toml(
            r#"
repo_path = "/tmp/repo"
[[source]]
kind = "inbox"
dir = "/tmp/atr-inbox"
[[source]]
kind = "github"
enabled = false
[[provider]]
name = "claude"
kind = "claude"
"#,
        )
        .unwrap();
        assert!(with.intake_allows(crate::task::SourceKind::Inbox));
        assert!(!with.intake_allows(crate::task::SourceKind::Github));
        assert!(with.intake_allows(crate::task::SourceKind::Mcp));
        assert_eq!(with.inbox_sources()[0].dir, PathBuf::from("/tmp/atr-inbox"));
    }

    #[test]
    fn telegram_chat_id_comes_only_from_allowed_chat_ids() {
        std::env::set_var("ATR_TELEGRAM_CHAT_ID", "99");
        let cfg = parse_config_toml(
            r#"
repo_path = "/tmp/repo"
[telegram]
enabled = false
allowed_chat_ids = []
[[provider]]
name = "claude"
kind = "claude"
"#,
        )
        .unwrap();
        assert!(
            cfg.telegram.allowed_chat_ids.is_empty(),
            "ATR_TELEGRAM_CHAT_ID must not fill allowed_chat_ids"
        );
        let cfg = parse_config_toml(
            r#"
repo_path = "/tmp/repo"
[telegram]
enabled = true
allowed_chat_ids = [42]
[[provider]]
name = "claude"
kind = "claude"
"#,
        )
        .unwrap();
        assert_eq!(cfg.telegram.allowed_chat_ids, vec![42]);
        assert!(!cfg.telegram.assistant.enabled);
        assert!(cfg.telegram.assistant.provider.is_empty());
        assert_eq!(cfg.telegram.assistant.timeout, Duration::from_secs(600));

        let cfg = parse_config_toml(
            r#"
repo_path = "/tmp/repo"
[telegram]
enabled = true
allowed_chat_ids = [42]
[telegram.assistant]
enabled = true
provider = "claude"
timeout = "2m"
[[provider]]
name = "claude"
kind = "claude"
"#,
        )
        .unwrap();
        assert!(cfg.telegram.assistant.enabled);
        assert_eq!(cfg.telegram.assistant.provider, "claude");
        assert_eq!(cfg.telegram.assistant.timeout, Duration::from_secs(120));
        assert!(cfg.dashboard.enabled);
        assert_eq!(cfg.dashboard.bind, "127.0.0.1");

        let cfg = parse_config_toml(
            r#"
repo_path = "/tmp/repo"
[dashboard]
enabled = false
bind = "100.64.0.10"
[[provider]]
name = "claude"
kind = "claude"
"#,
        )
        .unwrap();
        assert!(!cfg.dashboard.enabled);
        assert_eq!(cfg.dashboard.bind, "100.64.0.10");
    }

    #[test]
    fn default_paths_use_dot_config_on_every_os() {
        let dir = default_atr_dir();
        let dir_s = dir.to_string_lossy();
        assert!(
            dir_s.ends_with(".config/atr") || dir_s.ends_with(".config/atr/"),
            "{dir_s}"
        );
        assert!(
            !dir_s.contains("Application Support"),
            "must not use macOS Application Support: {dir_s}"
        );
        let db = default_state_db_path();
        assert!(
            db.to_string_lossy().ends_with(".local/share/atr/state.db"),
            "{}",
            db.display()
        );
    }

    #[test]
    fn resolve_state_db_reuses_legacy_when_default_is_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let default_new = tmp.path().join("share").join("state.db");
        let legacy = tmp.path().join("config").join("state.db");
        std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        std::fs::write(&legacy, "legacy-db").unwrap();

        let resolved =
            resolve_state_db_paths(default_new.clone(), legacy.clone(), default_new.clone());
        assert_eq!(resolved, legacy, "must not open a fresh default database");

        let custom = tmp.path().join("custom").join("state.db");
        let resolved = resolve_state_db_paths(custom.clone(), legacy.clone(), default_new.clone());
        assert_eq!(
            resolved, custom,
            "a non-default path that does not exist yet is created, not redirected"
        );

        std::fs::create_dir_all(default_new.parent().unwrap()).unwrap();
        std::fs::write(&default_new, "new-db").unwrap();
        let resolved = resolve_state_db_paths(default_new.clone(), legacy, default_new.clone());
        assert_eq!(resolved, default_new, "existing configured path wins");
    }

    #[test]
    fn explicit_repos_win_over_shorthand() {
        let cfg = parse_config_toml(
            r#"
repo_path = "/tmp/old"
task_key_prefix = "old"
[github]
enabled = true
repo = "owner/old"
label = "for:atr"
[[repo]]
name = "demo-1"
path = "/tmp/demo-1"
github = "owner/demo-1"
[[repo]]
name = "demo-2"
path = "/tmp/demo-2"
github = "owner/demo-2"
label = "for:team"
enabled = false
max_parallel = 1
[[provider]]
name = "claude"
kind = "claude"
"#,
        )
        .unwrap();
        assert_eq!(cfg.repos.len(), 2);
        let watch = cfg.effective_repos();
        assert_eq!(watch.len(), 2);
        assert!(!watch[0].shorthand);
        assert_eq!(watch[0].name, "demo-1");
        assert_eq!(watch[0].label, DEFAULT_GITHUB_LABEL);
        assert_eq!(cfg.issue_task_key(&watch[0], 9), "demo-1-9");
        assert!(!watch[1].enabled);
        assert_eq!(watch[1].label, "for:team");
        assert_eq!(watch[1].max_parallel, 1);
        assert_eq!(cfg.enabled_watch_repos().len(), 1);
        assert!(cfg.missing_required().is_empty());
        assert_eq!(
            cfg.resolve_repo_filter("demo-1").map(|r| r.name),
            Some("demo-1".into())
        );
        assert_eq!(
            cfg.resolve_repo_filter("owner/demo-2").map(|r| r.name),
            Some("demo-2".into())
        );
    }

    #[test]
    fn serve_allows_empty_repo_path_when_repos_present() {
        let cfg = parse_config_toml(
            r#"
repo_path = ""
[github]
enabled = true
[[repo]]
name = "demo-1"
path = "/tmp/demo-1"
github = "owner/demo-1"
[[provider]]
name = "claude"
kind = "claude"
"#,
        )
        .unwrap();
        assert!(
            cfg.missing_required().is_empty(),
            "{:?}",
            cfg.missing_required()
        );
        cfg.validate_for_serve().unwrap();
    }

    #[test]
    fn driver_defaults_to_grok_then_first_enabled() {
        let grok_first = parse_config_toml(
            r#"
repo_path = "/tmp"
[[provider]]
name = "claude"
kind = "claude"
[[provider]]
name = "grok"
kind = "grok"
"#,
        )
        .unwrap();
        assert_eq!(grok_first.driver_name().as_deref(), Some("grok"));
        assert_eq!(grok_first.planner.mode, PlannerMode::Auto);
        assert!(!grok_first.review.enabled);

        let named = parse_config_toml(
            r#"
repo_path = "/tmp"
driver = "claude"
planner = "always"
[[provider]]
name = "claude"
kind = "claude"
[[provider]]
name = "grok"
kind = "grok"
"#,
        )
        .unwrap();
        assert_eq!(named.driver_name().as_deref(), Some("claude"));
        assert_eq!(named.planner.mode, PlannerMode::Always);

        let no_grok = parse_config_toml(
            r#"
repo_path = "/tmp"
[[provider]]
name = "claude"
kind = "claude"
enabled = true
[[provider]]
name = "grok"
kind = "grok"
enabled = false
"#,
        )
        .unwrap();
        assert_eq!(no_grok.driver_name().as_deref(), Some("claude"));
    }

    #[test]
    fn mcp_defaults_without_table() {
        let cfg = parse_config_toml(
            r#"
repo_path = "/tmp"
[[provider]]
name = "claude"
kind = "claude"
"#,
        )
        .unwrap();
        assert!(cfg.mcp.enabled);
        assert_eq!(cfg.mcp.token_env, "ATR_MCP_TOKEN");
        assert!(cfg
            .mcp
            .token_file
            .to_string_lossy()
            .ends_with(".config/atr/mcp.token"));
    }

    #[test]
    fn parse_github_slug_accepts_urls() {
        assert_eq!(parse_github_slug("owner/name").unwrap(), "owner/name");
        assert_eq!(
            parse_github_slug("https://github.com/owner/name.git").unwrap(),
            "owner/name"
        );
        assert_eq!(
            parse_github_slug("git@github.com:owner/name.git").unwrap(),
            "owner/name"
        );
        assert!(parse_github_slug("not-a-slug").is_err());
    }
}
