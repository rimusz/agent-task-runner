#![allow(clippy::too_many_arguments)]

pub mod app;
pub mod checklist;
pub mod clock;
pub mod compare;
pub mod config;
pub mod config_edit;
pub mod dashboard;
pub mod db;
pub mod deps;
pub mod desktop;
pub mod detect;
pub mod doctor;
pub mod duration;
pub mod gc;
pub mod gh_token;
pub mod gh_token_cli;
pub mod github;
pub mod github_auth;
pub mod grok_stdio;
pub mod handoff;
pub mod host;
pub mod http;
pub mod inbox;
pub mod init;
pub mod intake;
pub mod lessons;
pub mod logrotate;
pub mod mcp;
pub mod notify;
pub mod opencode_cfg;
pub mod output;
pub mod owner;
pub mod plan;
pub mod plugin;
pub mod presets;
pub mod prompt;
pub mod providers;
pub mod providers_edit;
pub mod quota;
pub mod reported;
pub mod repos;
pub mod reset;
pub mod retry;
pub mod review;
pub mod runner;
pub mod scheduler;
pub mod service;
pub mod shadow;
pub mod skill;
pub mod task;
pub mod telegram;
pub mod telegram_health;
pub mod telegram_setup;
pub mod tui;
pub mod tz;
pub mod ui;
pub mod upgrade;
pub mod usage;
pub mod util;
pub mod verify;
pub mod worktree;

pub use app::{serve, serve_background, Shared, SubmitOpts, SubmitOutcome, EX_TEMPFAIL};
pub use config::{default_config_path, load_config, Config};
pub use providers::new_session_uuid;

/// Release or Cargo version plus short git sha, e.g. `0.1.1 (abc1234)`.
pub const VERSION: &str = env!("ATR_VERSION_LONG");

#[cfg(test)]
mod version_tests {
    #[test]
    fn version_includes_semver_and_git_sha() {
        let v = crate::VERSION;
        let semver = v.split_whitespace().next().unwrap_or("");
        assert!(
            semver.split('.').count() == 3
                && semver
                    .split('.')
                    .all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit())),
            "version {v:?} must start with semver"
        );
        let sha = v
            .rsplit_once('(')
            .and_then(|(_, rest)| rest.strip_suffix(')'))
            .unwrap_or("");
        assert!(
            sha == "unknown" || sha.chars().all(|c| c.is_ascii_hexdigit()),
            "short git sha in {v:?}"
        );
        assert!(!sha.is_empty(), "{v:?}");
    }
}
