use anyhow::{bail, Context, Result};
use atr::{app, config::default_config_path, load_config, Shared, SubmitOutcome, EX_TEMPFAIL};
use clap::{Parser, Subcommand};
use std::io::{IsTerminal, Read};
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Parser, Debug)]
#[command(
    name = "atr",
    version = atr::VERSION,
    about = "Agent task runner for official CLI subscriptions"
)]
struct Cli {
    #[arg(long, global = true, env = "ATR_CONFIG")]
    config: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Write config, prompt, launcher, and install the user service.
    Init {
        /// Accept defaults (non-interactive).
        #[arg(long)]
        yes: bool,
        /// Overwrite existing files.
        #[arg(long)]
        force: bool,
        /// Write files but do not load launchd/systemd.
        #[arg(long)]
        no_service: bool,
        /// Replace a loaded user service, or install one for a non-default config.
        #[arg(long)]
        force_service: bool,
    },
    /// Run the local HTTP server, scheduler, and GitHub poller.
    Serve,
    /// MCP server on stdio (local agents). HTTP is POST /mcp on `atr serve`.
    Mcp,
    /// Enqueue a message for a task key (queued if the task is already running).
    Submit {
        key: String,
        #[arg(long)]
        body: Option<String>,
        #[arg(long)]
        body_file: Option<PathBuf>,
        /// Who submitted this message (cli, github, telegram, …).
        #[arg(long, default_value = "cli")]
        source: String,
        /// Git worktree source repo (default: config repo_path).
        #[arg(long)]
        repo: Option<String>,
        /// Linked GitHub issue number, if any.
        #[arg(long)]
        issue: Option<i64>,
        /// Mark the task as shadow (local only; no GitHub writes or notifications).
        #[arg(long)]
        shadow: bool,
        /// Allow verify to use the agent's test-file edits (`[verify] protect_tests`).
        #[arg(long)]
        tests_ok: bool,
    },
    /// Enqueue a generic task (prompt + optional workspace). No issue number.
    Add {
        /// Prompt text (or use --body / --body-file / stdin).
        prompt: Option<String>,
        #[arg(long)]
        body: Option<String>,
        #[arg(long)]
        body_file: Option<PathBuf>,
        /// Local directory workspace (edit in place; stay inside).
        #[arg(long, conflicts_with = "repo")]
        dir: Option<PathBuf>,
        /// Git repository workspace (worktree / branch). Name, owner/name, or path.
        #[arg(long)]
        repo: Option<String>,
        /// reply (default), pr, issue-comment, branch, patch, text, webhook
        #[arg(long)]
        output: Option<String>,
        #[arg(long)]
        webhook: Option<String>,
        #[arg(long)]
        patch: Option<PathBuf>,
        #[arg(long)]
        key: Option<String>,
        #[arg(long)]
        provider: Option<String>,
        #[arg(long)]
        title: Option<String>,
        /// Required provider capabilities: browser, desktop (comma-separated or repeated).
        #[arg(long, value_delimiter = ',')]
        needs: Vec<String>,
        /// Run on two providers (e.g. grok,claude) and keep the arm that passes
        /// verify with the smallest diff. Needs --repo and verify commands.
        #[arg(long, conflicts_with_all = ["provider", "dir", "needs"])]
        compare: Option<String>,
    },
    /// Interactive terminal session (in-process; `atr serve` is not required).
    /// With a prompt argument, run one turn, print the answer, and exit.
    Tui {
        /// One-shot prompt. Omit for the interactive transcript + status footer.
        prompt: Option<String>,
        /// Preferred provider for this session.
        #[arg(long)]
        provider: Option<String>,
        /// Session name (task key `cli-<name>`, default `default`).
        #[arg(long)]
        session: Option<String>,
        /// Local directory workspace (edit in place).
        #[arg(long, conflicts_with = "repo")]
        dir: Option<PathBuf>,
        /// Git repository workspace (worktree / branch).
        #[arg(long)]
        repo: Option<String>,
        /// Drop the previous `cli-<name>` task and start a fresh session.
        #[arg(long)]
        new: bool,
    },
    /// Show providers (with order numbers), inflight tasks, and usage.
    Status {
        /// Print the machine-readable JSON snapshot.
        #[arg(long)]
        json: bool,
        /// Limit tasks to this watch repo (name, owner/name, or path).
        #[arg(long)]
        repo: Option<String>,
    },
    /// List or edit watched git repos (preserves comments; SIGHUP the daemon).
    Repos {
        #[command(subcommand)]
        cmd: Option<ReposCmd>,
    },
    /// Convert shorthand repo_path + [github].repo into [[repo]] (never automatic).
    Config {
        #[command(subcommand)]
        cmd: ConfigCmd,
    },
    /// List or edit providers (preserves comments; SIGHUP the daemon).
    Providers {
        #[command(subcommand)]
        cmd: Option<ProvidersCmd>,
    },
    /// Show or set the driver (planning/review provider).
    Driver {
        #[command(subcommand)]
        cmd: Option<DriverCmd>,
    },
    /// List tasks, show one, or remove finished ones.
    #[command(subcommand_negates_reqs = true)]
    Tasks {
        #[command(subcommand)]
        cmd: Option<TasksCmd>,
        /// Task key (omit to list).
        key: Option<String>,
        /// Include per-run handoff.
        #[arg(long)]
        verbose: bool,
        /// Limit the list to this watch repo (name, owner/name, or path).
        #[arg(long)]
        repo: Option<String>,
    },
    /// Per-provider and per-day usage, including cache-hit ratio and token savings.
    Usage {
        /// Rolling-style day count when `--period` is omitted (calendar days, default 7).
        #[arg(long, default_value_t = 7)]
        days: u32,
        /// `today`, `7d`, `30d`, `month`, or `YYYY-MM-DD..YYYY-MM-DD`.
        #[arg(long)]
        period: Option<String>,
        /// Inclusive start date (`YYYY-MM-DD`); requires `--to`.
        #[arg(long)]
        from: Option<String>,
        /// Inclusive end date (`YYYY-MM-DD`); requires `--from`.
        #[arg(long)]
        to: Option<String>,
        /// Group rows by `provider` or `day`.
        #[arg(long, default_value = "day")]
        by: String,
        /// Limit usage to this watch repo (name, owner/name, or path).
        #[arg(long)]
        repo: Option<String>,
    },
    /// One ok/warn/FAIL line per check. Missing config: run atr init.
    Doctor {
        /// Render two prompts per provider and assert the cache prefix is stable.
        #[arg(long)]
        cache: bool,
        /// Print the report as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Prune old worktrees, events, runs, and (opt-in) task rows or CLI session files.
    Gc {
        #[arg(long)]
        dry_run: bool,
        /// Remove dirty worktrees too.
        #[arg(long)]
        force: bool,
        /// Override worktree retention for this run (`7d`, `48h`).
        #[arg(long, value_name = "DUR")]
        older_than: Option<String>,
        /// Override event and run retention for this run (`7d`, `48h`).
        #[arg(long, value_name = "DUR")]
        history_older_than: Option<String>,
        /// Also delete finished, failed, or cancelled task rows (and their history).
        #[arg(long)]
        tasks: bool,
        /// Limit `--tasks` to this repo path.
        #[arg(long, requires = "tasks")]
        repo: Option<String>,
        /// Limit `--tasks` to `failed` (failed|cancelled) or `done` (waiting|done).
        #[arg(long, requires = "tasks", value_parser = ["failed", "done"])]
        status: Option<String>,
    },
    /// Cancel a task. A live run is only killed with --force.
    Cancel {
        key: String,
        #[arg(long)]
        force: bool,
    },
    /// Re-queue a finished, failed or cancelled task as a fresh attempt (same as
    /// Telegram /retry): last prompt is unconsumed, wall clock restarts,
    /// provider re-picked from config.
    Retry { key: String },
    /// Pause intake: GitHub, shadow and inbox pollers stop picking up work (same as Telegram /pause).
    Pause,
    /// Resume intake (same as Telegram /resume).
    Resume,
    /// Open the dashboard in a browser (prints the URL when headless/SSH).
    Ui {
        /// Only print the URL.
        #[arg(long)]
        print: bool,
    },
    /// Install the atr agent skill for installed CLIs only.
    Skills {
        #[command(subcommand)]
        cmd: SkillsCmd,
    },
    /// Download a GitHub release, verify sha256, swap the binary, restart.
    /// Without a version, does nothing if already on the latest release.
    Upgrade {
        version: Option<String>,
        /// Reinstall even if already on the latest release.
        #[arg(long)]
        force: bool,
    },
    /// Restore the previous binary, config, and database.
    Rollback,
    /// Full reset: stop and remove the service, delete atr's config, data, logs, and skills.
    Reset {
        /// Skip the confirmation prompt.
        #[arg(short, long)]
        yes: bool,
        /// Show what would be removed and change nothing.
        #[arg(long)]
        dry_run: bool,
    },
    /// Install, uninstall, or restart the user service.
    Service {
        #[command(subcommand)]
        cmd: ServiceCmd,
    },
    /// Print the configured log file.
    Logs {
        /// Follow new lines (`tail -f`).
        #[arg(short, long)]
        follow: bool,
        /// Only lines that contain this task key.
        #[arg(long)]
        task: Option<String>,
    },
    /// GitHub helpers (per-owner tokens).
    Github {
        #[command(subcommand)]
        cmd: GithubCmd,
    },
    /// Store the bot token, discover chat id, or send a test message.
    Telegram {
        #[command(subcommand)]
        cmd: TelegramCmd,
    },
    /// Shadow-mode commands (side-by-side with an existing runner, no GitHub writes).
    Shadow {
        #[command(subcommand)]
        cmd: ShadowCmd,
    },
    /// Per-repo lessons injected into first-run prompts (`[lessons]`, off by default).
    Lessons {
        #[command(subcommand)]
        cmd: LessonsCmd,
    },
    /// Compare tasks (`atr add --compare a,b`): arms, outcomes, decision.
    Compare {
        #[command(subcommand)]
        cmd: CompareCmd,
    },
}

#[derive(Subcommand, Debug)]
enum CompareCmd {
    /// Show a compare task's arms, outcomes, diff sizes, decision and PR URL.
    Show {
        key: String,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand, Debug)]
enum LessonsCmd {
    /// List stored lessons (all repos, or one).
    List {
        #[arg(long)]
        repo: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Add a manual lesson (scrubbed for secrets; signal `manual`).
    Add {
        #[arg(long)]
        repo: String,
        text: String,
    },
    /// Delete one lesson (`--id`) or every lesson of a repo.
    Clear {
        #[arg(long)]
        repo: String,
        #[arg(long)]
        id: Option<i64>,
    },
    /// Pin a lesson so pruning never drops it.
    Pin { id: i64 },
    /// Unpin a lesson.
    Unpin { id: i64 },
    /// Print a repo's lessons as markdown (to promote into AGENTS.md by hand).
    Export {
        #[arg(long)]
        repo: String,
    },
}

#[derive(Subcommand, Debug)]
enum SkillsCmd {
    /// Write skills/atr/SKILL.md for each installed CLI (claude, cursor-agent,
    /// codex, grok, opencode). Missing CLIs are skipped. Configs are untouched.
    Install,
}

#[derive(Subcommand, Debug)]
enum ReposCmd {
    /// List watch targets (shorthand or [[repo]]).
    List,
    /// Add owner/name. Clones into --path when the directory is missing.
    Add {
        github: String,
        #[arg(long)]
        path: PathBuf,
        #[arg(long)]
        label: Option<String>,
        #[arg(long)]
        name: Option<String>,
    },
    Enable {
        name: String,
    },
    Disable {
        name: String,
    },
    Rm {
        name: String,
    },
}

#[derive(Subcommand, Debug)]
enum ConfigCmd {
    /// Write a [[repo]] block from repo_path + [github].repo. Leaves those keys in place.
    Migrate,
    /// Print one config key (dotted, e.g. `telegram.assistant.enabled`). Secret keys are refused.
    Get { key: String },
    /// Set one config key (same validation as Telegram /config set), keep comments, back up, SIGHUP the service.
    Set {
        key: String,
        /// Value; several words are joined with spaces.
        #[arg(required = true, num_args = 1.., allow_hyphen_values = true)]
        value: Vec<String>,
    },
}

#[derive(Subcommand, Debug)]
enum TasksCmd {
    /// Delete finished, failed, or cancelled tasks (and their history).
    Rm {
        /// Task keys to remove.
        #[arg(required = true, num_args = 1..)]
        keys: Vec<String>,
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(Subcommand, Debug)]
enum DriverCmd {
    /// Use this [[provider]] as driver; `--model` also sets its model.
    Set {
        provider: String,
        #[arg(long)]
        model: Option<String>,
    },
    /// Clear `driver` (default: grok if enabled, else first enabled).
    Clear,
}

#[derive(Subcommand, Debug)]
enum ProvidersCmd {
    /// Add a provider. `--kind` optional when `name` matches a built-in or `[[preset]]`.
    Add {
        name: String,
        #[arg(long)]
        kind: Option<String>,
        #[arg(long)]
        model: Option<String>,
        #[arg(long)]
        cli: Option<String>,
        #[arg(long = "override")]
        overrides: Vec<String>,
        /// API key for OpenCode presets (stored in shadow.env, never config).
        #[arg(long)]
        key: Option<String>,
        /// Base URL for ollama/spark (or to override a built-in).
        #[arg(long)]
        url: Option<String>,
        /// Non-interactive: use defaults / require `--key` or an existing env/shadow key.
        #[arg(long)]
        yes: bool,
    },
    Enable {
        name: String,
    },
    Disable {
        name: String,
    },
    /// Move a provider to 1-based position N.
    Move {
        name: String,
        #[arg(long)]
        to: usize,
    },
    /// Set [[provider]] model.
    Model {
        name: String,
        model: String,
    },
    /// Find installed CLIs, versions, and signed-in status (same as `atr init`).
    Detect,
}

#[derive(Subcommand, Debug)]
enum ServiceCmd {
    /// Write and load the user service (`dev.atr.serve` / `atr.service`).
    Install {
        /// Boot out this older LaunchAgent label before installing the current one.
        #[arg(long)]
        legacy_label: Option<String>,
    },
    Uninstall,
    Restart,
}

#[derive(Subcommand, Debug)]
enum GithubCmd {
    /// Per-owner GitHub tokens (values live only in shadow.env).
    Token {
        #[command(subcommand)]
        cmd: GithubTokenCmd,
    },
}

#[derive(Subcommand, Debug)]
enum GithubTokenCmd {
    /// Store a token for <owner> (hidden prompt, or stdin when piped), map it in [github.tokens], validate.
    Set {
        owner: String,
        /// Env var name (default ATR_GH_TOKEN_<OWNER>).
        #[arg(long)]
        env_var: Option<String>,
    },
    /// Owners, env var names, status and days left (never values).
    List,
    /// Remove <owner>'s mapping and its value from shadow.env.
    Remove { owner: String },
}

#[derive(Subcommand, Debug)]
enum TelegramCmd {
    /// Token → shadow.env, discover chat via /start, write allowed_chat_ids.
    Setup,
    /// Send a test message to allowed_chat_ids.
    Test,
}

#[derive(Subcommand, Debug)]
enum ShadowCmd {
    /// Per-task table plus totals. `--since` is a duration (`7d`) or RFC3339.
    Report {
        #[arg(long)]
        since: Option<String>,
        /// Print JSON instead of the table.
        #[arg(long)]
        json: bool,
        /// Write JSON to this path (implies `--json`).
        #[arg(long)]
        output: Option<PathBuf>,
    },
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli).await {
        Ok(code) => code,
        Err(e) => {
            eprintln!("{e:#}");
            ExitCode::from(1)
        }
    }
}

async fn run(cli: Cli) -> Result<ExitCode> {
    let path = cli.config.unwrap_or_else(default_config_path);
    let cmd = match cli.cmd {
        Some(c) => c,
        None => Cmd::Tui {
            prompt: None,
            provider: None,
            session: None,
            dir: None,
            repo: None,
            new: false,
        },
    };
    if !matches!(cmd, Cmd::Serve) {
        atr::logrotate::init_tracing(None);
    }
    match cmd {
        Cmd::Init {
            yes,
            force,
            no_service,
            force_service,
        } => {
            let text = atr::init::run_and_doctor(atr::init::InitOpts {
                yes,
                force,
                no_service,
                force_service,
                config_path: path,
            })
            .await?;
            print!("{text}");
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Serve => {
            let shared = Shared::open(path)?;
            {
                let log = shared.config.read().unwrap().log.clone();
                atr::logrotate::init_tracing(Some(&log));
            }
            app::serve(shared).await?;
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Mcp => {
            let shared = Shared::open(path)?;
            atr::mcp::serve_stdio(shared).await?;
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Submit {
            key,
            body,
            body_file,
            source,
            repo,
            issue,
            shadow,
            tests_ok,
        } => {
            let text = read_body(body, body_file)?;
            let cfg = load_config(&path)?;
            let auth = atr::mcp::auth_header(&cfg.mcp);
            if let Ok(status) = http_submit(
                &cfg.listen,
                auth.as_deref(),
                &key,
                &text,
                &source,
                repo.as_deref(),
                issue,
                shadow,
                tests_ok,
            ) {
                return Ok(match status {
                    200 => ExitCode::SUCCESS,
                    202 => ExitCode::from(EX_TEMPFAIL as u8),
                    _ => ExitCode::from(1),
                });
            }
            let shared = Shared::open(path)?;
            match shared.submit(
                &key,
                &text,
                &source,
                repo.as_deref(),
                issue,
                shadow,
                tests_ok,
            )? {
                SubmitOutcome::Accepted => Ok(ExitCode::SUCCESS),
                SubmitOutcome::Queued => Ok(ExitCode::from(EX_TEMPFAIL as u8)),
            }
        }
        Cmd::Add {
            prompt,
            body,
            body_file,
            dir,
            repo,
            output,
            webhook,
            patch,
            key,
            provider,
            title,
            needs,
            compare,
        } => {
            let needs = atr::task::parse_needs(&needs)?;
            let text = if let Some(p) = prompt.filter(|s| !s.trim().is_empty()) {
                if body.is_some() || body_file.is_some() {
                    anyhow::bail!("pass the prompt as an argument or --body, not both");
                }
                p
            } else {
                read_body(body, body_file)?
            };
            let shared = Shared::open(path)?;
            let cfg = shared.config.read().unwrap().clone();
            atr::task::check_prompt_bytes(&text, cfg.max_prompt_bytes)?;
            let title = title
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| atr::task::title_from_prompt(&text));
            let (workspace_kind, repo_path, allow_empty) = if let Some(d) = dir {
                let p = atr::task::resolve_dir(&d.display().to_string())?;
                (
                    atr::task::WorkspaceKind::Dir,
                    Some(p.display().to_string()),
                    false,
                )
            } else if let Some(r) = repo.as_deref() {
                if let Some(entry) = cfg.resolve_repo_filter(r) {
                    (
                        atr::task::WorkspaceKind::Git,
                        Some(entry.path.display().to_string()),
                        false,
                    )
                } else {
                    let p = atr::task::resolve_git_repo(r)?;
                    (
                        atr::task::WorkspaceKind::Git,
                        Some(p.display().to_string()),
                        false,
                    )
                }
            } else {
                (atr::task::WorkspaceKind::None, None, true)
            };
            let output_kind = match output.as_deref() {
                Some(o) => atr::task::OutputKind::parse(o)?,
                None => atr::task::OutputKind::Reply,
            };
            if output_kind.needs_git() && workspace_kind != atr::task::WorkspaceKind::Git {
                anyhow::bail!("output {} needs --repo", output_kind.as_str());
            }
            let output_spec = webhook.or_else(|| patch.map(|p| p.display().to_string()));
            let key = match key {
                Some(k) => k,
                None => atr::task::allocate_key(&shared.db, &cfg.task_key_prefix, &title)?,
            };
            let body = format!("# {title}\n\n{text}");
            if let Some(arms) = compare.as_deref() {
                if workspace_kind != atr::task::WorkspaceKind::Git {
                    anyhow::bail!("--compare needs --repo (arms are judged on their git diff)");
                }
                let providers = atr::compare::parse_arms(&cfg, arms)?;
                let keys = atr::compare::add(
                    &shared,
                    &cfg,
                    &atr::compare::CompareSpec {
                        key: key.clone(),
                        repo: repo_path.clone().unwrap_or_default(),
                        issue: None,
                        message: body,
                        output_kind: output_kind.as_str().into(),
                        output_spec: output_spec.unwrap_or_default(),
                        title: title.clone(),
                        labels: String::new(),
                        providers,
                    },
                )?;
                println!("{key}");
                eprintln!("arms: {}", keys.join(", "));
                return Ok(ExitCode::SUCCESS);
            }
            let auth = atr::mcp::auth_header(&cfg.mcp);
            if let Ok(status) = http_create(
                &cfg.listen,
                auth.as_deref(),
                &key,
                &text,
                repo_path.as_deref(),
                workspace_kind.as_str(),
                output_kind.as_str(),
                output_spec.as_deref(),
                title.as_str(),
                provider.as_deref(),
                matches!(workspace_kind, atr::task::WorkspaceKind::Dir),
                &needs,
            ) {
                println!("{key}");
                return Ok(match status {
                    200 => ExitCode::SUCCESS,
                    202 => ExitCode::from(EX_TEMPFAIL as u8),
                    _ => ExitCode::from(1),
                });
            }
            drop(cfg);
            let outcome = shared.submit_with(
                &key,
                &body,
                "cli",
                repo_path.as_deref(),
                None,
                false,
                false,
                atr::SubmitOpts {
                    workspace_kind: Some(workspace_kind.as_str().into()),
                    output_kind: Some(output_kind.as_str().into()),
                    output_spec,
                    source_kind: Some("cli".into()),
                    title: Some(title),
                    allow_empty_repo: allow_empty,
                    provider_hint: provider.clone(),
                    needs: needs.clone(),
                },
            )?;
            println!("{key}");
            match outcome {
                SubmitOutcome::Accepted => Ok(ExitCode::SUCCESS),
                SubmitOutcome::Queued => Ok(ExitCode::from(EX_TEMPFAIL as u8)),
            }
        }
        Cmd::Tui {
            prompt,
            provider,
            session,
            dir,
            repo,
            new,
        } => {
            let shared = Shared::open(path)?;
            atr::tui::run(
                shared,
                atr::tui::TuiOpts {
                    prompt,
                    provider,
                    session,
                    dir,
                    repo,
                    new,
                },
            )
            .await
        }
        Cmd::Status { json, repo } => {
            let shared = Shared::open(path)?;
            app::print_status(&shared, json, repo.as_deref());
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Repos { cmd } => {
            match cmd.unwrap_or(ReposCmd::List) {
                ReposCmd::List => {
                    let cfg = load_config(&path)?;
                    print!("{}", atr::repos::list_text(&cfg));
                }
                ReposCmd::Add {
                    github,
                    path: repo_path,
                    label,
                    name,
                } => {
                    let msg = atr::repos::add(
                        &path,
                        &github,
                        atr::repos::AddOpts {
                            path: &repo_path,
                            label: label.as_deref(),
                            name: name.as_deref(),
                            clone_if_missing: true,
                        },
                    )?;
                    print!("{msg}");
                }
                ReposCmd::Enable { name } => {
                    print!("{}", atr::repos::set_enabled(&path, &name, true)?)
                }
                ReposCmd::Disable { name } => {
                    print!("{}", atr::repos::set_enabled(&path, &name, false)?)
                }
                ReposCmd::Rm { name } => print!("{}", atr::repos::remove(&path, &name)?),
            }
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Config { cmd } => match cmd {
            ConfigCmd::Migrate => {
                print!("{}", atr::repos::migrate_text(&path)?);
                Ok(ExitCode::SUCCESS)
            }
            ConfigCmd::Get { key } => {
                println!("{key} = {}", atr::config_edit::get_value(&path, &key)?);
                Ok(ExitCode::SUCCESS)
            }
            ConfigCmd::Set { key, value } => {
                let c = atr::config_edit::apply_set(&path, &key, &value.join(" "))?;
                println!("set {} {} → {}", c.key, c.old, c.new);
                Ok(ExitCode::SUCCESS)
            }
        },
        Cmd::Driver { cmd } => {
            match cmd {
                Some(DriverCmd::Set { provider, model }) => {
                    atr::providers_edit::set_driver(&path, Some(&provider), model.as_deref())?
                }
                Some(DriverCmd::Clear) => atr::providers_edit::set_driver(&path, None, None)?,
                None => {}
            }
            print!("{}", atr::providers_edit::driver_text(&load_config(&path)?));
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Providers { cmd } => {
            match cmd {
                None => {
                    let cfg = load_config(&path)?;
                    let shared = Shared::open(path).ok();
                    print!(
                        "{}",
                        atr::providers_edit::list_text(&cfg, shared.as_deref())
                    );
                }
                Some(ProvidersCmd::Add {
                    name,
                    kind,
                    model,
                    cli,
                    overrides,
                    key,
                    url,
                    yes,
                }) => atr::providers_edit::add(
                    &path,
                    &name,
                    atr::providers_edit::AddOpts {
                        kind: kind.as_deref(),
                        model: model.as_deref(),
                        cli: cli.as_deref(),
                        overrides: &overrides,
                        key: key.as_deref(),
                        url: url.as_deref(),
                        yes,
                    },
                )?,
                Some(ProvidersCmd::Enable { name }) => {
                    atr::providers_edit::set_enabled(&path, &name, true)?
                }
                Some(ProvidersCmd::Disable { name }) => {
                    atr::providers_edit::set_enabled(&path, &name, false)?
                }
                Some(ProvidersCmd::Move { name, to }) => {
                    atr::providers_edit::move_to(&path, &name, to)?
                }
                Some(ProvidersCmd::Model { name, model }) => {
                    atr::providers_edit::set_model(&path, &name, &model)?
                }
                Some(ProvidersCmd::Detect) => {
                    let d = atr::detect::discover(&atr::detect::current_env());
                    let cfg = load_config(&path).ok();
                    print!("{}", atr::detect::format_report(&d, cfg.as_ref()));
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Tasks {
            key,
            verbose,
            cmd,
            repo,
        } => {
            let shared = Shared::open(path)?;
            match cmd {
                Some(TasksCmd::Rm { keys, dry_run }) => {
                    app::remove_tasks(&shared, &keys, dry_run)?;
                }
                None => {
                    app::print_tasks(&shared, key.as_deref(), verbose, repo.as_deref())?;
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Usage {
            days,
            period,
            from,
            to,
            by,
            repo,
        } => {
            let shared = Shared::open(path)?;
            let period =
                atr::usage::resolve(period.as_deref(), days, from.as_deref(), to.as_deref())?;
            let by = atr::usage::parse_group(&by)?;
            app::print_usage(&shared, &period, by, repo.as_deref())?;
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Doctor { cache, json } => {
            let cfg = load_config(&path)?;
            let report = atr::doctor::run(&cfg, &path, cache).await;
            if json {
                println!("{}", report.to_json());
            } else {
                print!("{}", report.format());
            }
            if report.ok() {
                Ok(ExitCode::SUCCESS)
            } else {
                Ok(ExitCode::from(1))
            }
        }
        Cmd::Gc {
            dry_run,
            force,
            older_than,
            history_older_than,
            tasks,
            repo,
            status,
        } => {
            let older = older_than
                .as_deref()
                .map(atr::duration::parse_duration)
                .transpose()?;
            let history = history_older_than
                .as_deref()
                .map(atr::duration::parse_duration)
                .transpose()?;
            let shared = Shared::open(path)?;
            let filter = tasks.then_some(atr::gc::TaskGc { repo, status });
            let report = atr::gc::run(&shared, dry_run, force, older, history, filter.as_ref())?;
            print!("{}", report.text(dry_run));
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Compare { cmd } => {
            let shared = Shared::open(path.clone())?;
            let cfg = shared.config.read().unwrap().clone();
            match cmd {
                CompareCmd::Show { key, json } => {
                    if json {
                        let v = atr::compare::show_json(&cfg, &shared.db, &key)?;
                        println!("{}", serde_json::to_string_pretty(&v)?);
                    } else {
                        print!("{}", atr::compare::show_text(&cfg, &shared.db, &key)?);
                    }
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Lessons { cmd } => {
            let shared = Shared::open(path.clone())?;
            let cfg = shared.config.read().unwrap().clone();
            let repo_name = |r: &str| -> String {
                cfg.resolve_repo_filter(r)
                    .map(|e| e.name)
                    .unwrap_or_else(|| r.trim().to_string())
            };
            match cmd {
                LessonsCmd::List { repo, json } => {
                    let repo = repo.as_deref().map(repo_name);
                    let rows = shared.db.list_lessons(repo.as_deref())?;
                    if json {
                        println!("{}", serde_json::to_string_pretty(&rows)?);
                    } else {
                        print!("{}", atr::lessons::list_text(&rows));
                    }
                }
                LessonsCmd::Add { repo, text } => {
                    let Some(entry) = cfg.resolve_repo_filter(&repo) else {
                        let names: Vec<String> =
                            cfg.effective_repos().into_iter().map(|r| r.name).collect();
                        bail!("unknown repo {repo:?}; configured: {}", names.join(", "));
                    };
                    let env = atr::lessons::shadow_env_values(&path);
                    let id = atr::lessons::add(
                        &shared.db,
                        &cfg,
                        &env,
                        &atr::db::NewLesson {
                            repo: &entry.name,
                            text: &text,
                            signal: "manual",
                            ..Default::default()
                        },
                        atr::lessons::MAX_MANUAL_CHARS,
                    )?;
                    println!("added lesson #{id} for {}", entry.name);
                    if !entry.lessons.unwrap_or(cfg.lessons.enabled) {
                        println!(
                            "note: lessons are off for {}; turn on with `atr config set lessons.enabled true` or `lessons = true` in its [[repo]]",
                            entry.name
                        );
                    }
                }
                LessonsCmd::Clear { repo, id } => {
                    let name = repo_name(&repo);
                    let n = shared.db.clear_lessons(&name, id)?;
                    println!("removed {n} lesson(s) from {name}");
                }
                LessonsCmd::Pin { id } | LessonsCmd::Unpin { id } => {
                    let pin = matches!(cmd, LessonsCmd::Pin { .. });
                    if !shared.db.set_lesson_pinned(id, pin)? {
                        bail!("no lesson #{id}");
                    }
                    println!("{} lesson #{id}", if pin { "pinned" } else { "unpinned" });
                }
                LessonsCmd::Export { repo } => {
                    let name = repo_name(&repo);
                    let rows = shared.db.list_lessons(Some(&name))?;
                    print!("{}", atr::lessons::export_markdown(&name, &rows));
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Shadow { cmd } => match cmd {
            ShadowCmd::Report {
                since,
                json,
                output,
            } => {
                let shared = Shared::open(path)?;
                let since_ts = match since.as_deref() {
                    Some(s) => Some(atr::shadow::parse_since(s)?),
                    None => None,
                };
                let as_json = json || output.is_some();
                let cfg = shared.config.read().unwrap().clone();
                let names: Vec<&str> = cfg.providers.iter().map(|p| p.name.as_str()).collect();
                let tz = &cfg.display_timezone;
                let text = if as_json {
                    atr::shadow::report_json_tz_usage(&shared.db, since_ts.as_deref(), tz, &names)?
                } else {
                    atr::shadow::report_text_tz_usage(&shared.db, since_ts.as_deref(), tz, &names)?
                };
                if let Some(p) = output {
                    std::fs::write(&p, &text).with_context(|| format!("write {}", p.display()))?;
                } else {
                    print!("{text}");
                }
                Ok(ExitCode::SUCCESS)
            }
        },
        Cmd::Cancel { key, force } => {
            if let Ok(cfg) = load_config(&path) {
                match http_cancel(
                    &cfg.listen,
                    atr::mcp::auth_header(&cfg.mcp).as_deref(),
                    &key,
                    force,
                ) {
                    Ok(()) => {
                        println!("{}", atr::app::Shared::cancel_ok_line(&key, force));
                        return Ok(ExitCode::SUCCESS);
                    }
                    Err(e) => {
                        let msg = e.to_string();
                        if msg.contains("--force") || msg.contains("unknown task") {
                            return Err(e);
                        }
                    }
                }
            }
            let shared = Shared::open(path)?;
            shared.cancel(&key, force)?;
            println!("{}", atr::app::Shared::cancel_ok_line(&key, force));
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Retry { key } => {
            let cfg = load_config(&path)?;
            match http_post_json(&cfg, &format!("tasks/{key}/retry")) {
                Ok(_) => {}
                Err(PostErr::Unreachable(e)) => {
                    // No running service: queue it in the database; the next `atr serve` picks it up.
                    Shared::open(path)?.retry(&key)?;
                    eprintln!(
                        "atr serve not reachable ({e}); queued in the database for the next start"
                    );
                }
                Err(PostErr::Refused(e)) => return Err(e),
            }
            println!("retried {key}");
            Ok(ExitCode::SUCCESS)
        }
        c @ (Cmd::Pause | Cmd::Resume) => {
            let pause = matches!(c, Cmd::Pause);
            let cfg = load_config(&path)?;
            let route = if pause {
                "intake/pause"
            } else {
                "intake/resume"
            };
            match http_post_json(&cfg, route) {
                Ok(_) => {}
                Err(PostErr::Unreachable(e)) => {
                    // Persisted in the db meta table; `atr serve` reads it at start.
                    Shared::open(path)?.set_intake_paused(pause);
                    eprintln!("atr serve not reachable ({e}); saved for the next start");
                }
                Err(PostErr::Refused(e)) => return Err(e),
            }
            println!("intake {}", if pause { "paused" } else { "resumed" });
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Ui { print } => {
            let listen = load_config(&path)
                .map(|c| c.listen)
                .unwrap_or_else(|_| "127.0.0.1:7420".into());
            println!("{}", atr::ui::run(&listen, print));
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Skills {
            cmd: SkillsCmd::Install,
        } => {
            match atr::skill::install_default()? {
                Some(msg) => print!("{msg}"),
                None => anyhow::bail!("HOME not set; set ATR_SKILL_HOME"),
            }
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Upgrade { version, force } => {
            let msg = atr::upgrade::upgrade(version.as_deref(), force, &path)?;
            println!("{msg}");
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Rollback => {
            let msg = atr::upgrade::rollback(&path)?;
            println!("{msg}");
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Reset { yes, dry_run } => {
            let cfg = load_config(&path).ok();
            let home = atr::service::home_dir();
            let skill_home = atr::skill::skill_home().unwrap_or_else(|| home.clone());
            let plan = atr::reset::plan(
                &home,
                &skill_home,
                &atr::service::service_file_path(),
                &path,
                cfg.as_ref(),
            );
            print!("{}", atr::reset::render(&plan));
            if dry_run {
                println!("dry run: nothing changed");
                return Ok(ExitCode::SUCCESS);
            }
            let tty = std::io::stdin().is_terminal();
            if atr::reset::confirm_mode(yes, dry_run, tty)? == atr::reset::Confirm::Ask {
                use std::io::{BufRead, Write};
                print!("Proceed with full reset? [y/N] ");
                std::io::stdout().flush()?;
                let mut line = String::new();
                std::io::stdin().lock().read_line(&mut line)?;
                if !atr::reset::answer_is_yes(&line) {
                    println!("aborted; nothing changed");
                    return Ok(ExitCode::FAILURE);
                }
            }
            println!("{}", atr::reset::execute(&plan, atr::service::uninstall)?);
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Service { cmd } => {
            let cfg = load_config(&path).ok();
            let msg = match cmd {
                ServiceCmd::Install { legacy_label } => {
                    atr::service::install(cfg.as_ref(), true, legacy_label.as_deref())?
                }
                ServiceCmd::Uninstall => atr::service::uninstall()?,
                ServiceCmd::Restart => atr::service::restart()?,
            };
            println!("{msg}");
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Logs { follow, task } => {
            let cfg = load_config(&path)?;
            atr::upgrade::follow_logs(&cfg, follow, task.as_deref())?;
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Github {
            cmd: GithubCmd::Token { cmd },
        } => {
            let text = match cmd {
                GithubTokenCmd::Set { owner, env_var } => {
                    let tok =
                        atr::gh_token_cli::read_secret(&format!("GitHub token for {owner}: "))?;
                    atr::gh_token_cli::set(&path, &owner, env_var.as_deref(), &tok)?
                }
                GithubTokenCmd::List => atr::gh_token_cli::list(&path)?,
                GithubTokenCmd::Remove { owner } => atr::gh_token_cli::remove(&path, &owner)?,
            };
            print!("{text}");
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Telegram { cmd } => {
            let text = match cmd {
                TelegramCmd::Setup => atr::telegram_setup::setup_interactive(&path)?,
                TelegramCmd::Test => atr::telegram_setup::test(&path)?,
            };
            print!("{text}");
            Ok(ExitCode::SUCCESS)
        }
    }
}

fn read_body(body: Option<String>, file: Option<PathBuf>) -> Result<String> {
    if let Some(b) = body {
        return Ok(b);
    }
    if let Some(p) = file {
        return std::fs::read_to_string(p).map_err(Into::into);
    }
    if !std::io::stdin().is_terminal() {
        let mut s = String::new();
        std::io::stdin().read_to_string(&mut s)?;
        if !s.trim().is_empty() {
            return Ok(s);
        }
    }
    bail!("provide --body, --body-file, or pipe stdin")
}

#[allow(clippy::too_many_arguments)]
fn http_submit(
    listen: &str,
    auth: Option<&str>,
    key: &str,
    body: &str,
    source: &str,
    repo: Option<&str>,
    issue: Option<i64>,
    shadow: bool,
    tests_ok: bool,
) -> Result<u16> {
    let url = format!("http://{listen}/tasks/{key}/messages");
    let mut req = ureq::post(&url).set("User-Agent", "atr");
    if let Some(a) = auth {
        req = req.set("Authorization", a);
    }
    let resp = req.send_json(serde_json::json!({
        "body": body,
        "source": source,
        "repo": repo,
        "issue": issue,
        "shadow": shadow,
        "tests_ok": tests_ok,
    }))?;
    Ok(resp.status())
}

#[allow(clippy::too_many_arguments)]
fn http_create(
    listen: &str,
    auth: Option<&str>,
    key: &str,
    body: &str,
    repo: Option<&str>,
    workspace: &str,
    output: &str,
    output_spec: Option<&str>,
    title: &str,
    provider: Option<&str>,
    as_dir: bool,
    needs: &[String],
) -> Result<u16> {
    let url = format!("http://{listen}/tasks");
    let mut payload = serde_json::json!({
        "key": key,
        "body": body,
        "title": title,
        "source": "cli",
        "workspace": workspace,
        "output": output,
    });
    if as_dir {
        if let Some(d) = repo {
            payload["dir"] = serde_json::Value::String(d.into());
        }
    } else if let Some(r) = repo {
        payload["repo"] = serde_json::Value::String(r.into());
    }
    if let Some(p) = provider {
        payload["provider"] = serde_json::Value::String(p.into());
    }
    if !needs.is_empty() {
        payload["needs"] = serde_json::json!(needs);
    }
    if let Some(spec) = output_spec {
        if output == "webhook" {
            payload["webhook"] = serde_json::Value::String(spec.into());
        } else if output == "patch" {
            payload["patch"] = serde_json::Value::String(spec.into());
        }
    }
    let mut req = ureq::post(&url).set("User-Agent", "atr");
    if let Some(a) = auth {
        req = req.set("Authorization", a);
    }
    let resp = req.send_json(payload)?;
    Ok(resp.status())
}

enum PostErr {
    /// Could not connect (service not running / wrong listen).
    Unreachable(anyhow::Error),
    /// The service answered with an error (auth, unknown task, running task…).
    Refused(anyhow::Error),
}

/// POST `{}` to a write route on the running `atr serve`, with the MCP bearer token.
fn http_post_json(cfg: &atr::config::Config, route: &str) -> Result<serde_json::Value, PostErr> {
    let url = format!("http://{}/{route}", cfg.listen);
    let mut req = ureq::post(&url).set("User-Agent", "atr");
    if let Some(a) = atr::mcp::auth_header(&cfg.mcp) {
        req = req.set("Authorization", &a);
    }
    match req.send_json(serde_json::json!({})) {
        Ok(resp) => Ok(resp.into_json().unwrap_or(serde_json::Value::Null)),
        Err(ureq::Error::Status(code, resp)) => {
            let msg = resp
                .into_string()
                .ok()
                .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
                .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(String::from))
                .unwrap_or_else(|| format!("HTTP {code}"));
            Err(PostErr::Refused(anyhow::anyhow!("{msg}")))
        }
        Err(e) => Err(PostErr::Unreachable(anyhow::anyhow!("{e}"))),
    }
}

fn http_cancel(listen: &str, auth: Option<&str>, key: &str, force: bool) -> Result<()> {
    let url = format!("http://{listen}/tasks/{key}/cancel");
    let mut req = ureq::post(&url).set("User-Agent", "atr");
    if let Some(a) = auth {
        req = req.set("Authorization", a);
    }
    let resp = req.send_json(serde_json::json!({ "force": force }))?;
    if resp.status() >= 400 {
        let msg = resp
            .into_string()
            .ok()
            .and_then(|s| {
                serde_json::from_str::<serde_json::Value>(&s)
                    .ok()
                    .and_then(|v| {
                        v.get("error")
                            .and_then(|e| e.as_str())
                            .map(|s| s.to_string())
                    })
            })
            .unwrap_or_else(|| "cancel failed".to_string());
        bail!("{msg}");
    }
    Ok(())
}
