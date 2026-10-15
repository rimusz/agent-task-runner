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
    cmd: Cmd,
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
    /// Open the dashboard in a browser (prints the URL when headless/SSH).
    Ui {
        /// Only print the URL.
        #[arg(long)]
        print: bool,
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
    if !matches!(cli.cmd, Cmd::Serve) {
        atr::logrotate::init_tracing(None);
    }
    match cli.cmd {
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
            if let Ok(status) = http_submit(
                &cfg.listen,
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
        } => {
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
            if let Ok(status) = http_create(
                &cfg.listen,
                &key,
                &text,
                repo_path.as_deref(),
                workspace_kind.as_str(),
                output_kind.as_str(),
                output_spec.as_deref(),
                title.as_str(),
                provider.as_deref(),
                matches!(workspace_kind, atr::task::WorkspaceKind::Dir),
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
                },
            )?;
            println!("{key}");
            match outcome {
                SubmitOutcome::Accepted => Ok(ExitCode::SUCCESS),
                SubmitOutcome::Queued => Ok(ExitCode::from(EX_TEMPFAIL as u8)),
            }
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
        },
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
                match http_cancel(&cfg.listen, &key, force) {
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
        Cmd::Ui { print } => {
            let listen = load_config(&path)
                .map(|c| c.listen)
                .unwrap_or_else(|_| "127.0.0.1:7420".into());
            println!("{}", atr::ui::run(&listen, print));
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
    key: &str,
    body: &str,
    source: &str,
    repo: Option<&str>,
    issue: Option<i64>,
    shadow: bool,
    tests_ok: bool,
) -> Result<u16> {
    let url = format!("http://{listen}/tasks/{key}/messages");
    let resp = ureq::post(&url)
        .set("User-Agent", "atr")
        .send_json(serde_json::json!({
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
    key: &str,
    body: &str,
    repo: Option<&str>,
    workspace: &str,
    output: &str,
    output_spec: Option<&str>,
    title: &str,
    provider: Option<&str>,
    as_dir: bool,
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
    if let Some(spec) = output_spec {
        if output == "webhook" {
            payload["webhook"] = serde_json::Value::String(spec.into());
        } else if output == "patch" {
            payload["patch"] = serde_json::Value::String(spec.into());
        }
    }
    let resp = ureq::post(&url)
        .set("User-Agent", "atr")
        .send_json(payload)?;
    Ok(resp.status())
}

fn http_cancel(listen: &str, key: &str, force: bool) -> Result<()> {
    let url = format!("http://{listen}/tasks/{key}/cancel");
    let resp = ureq::post(&url)
        .set("User-Agent", "atr")
        .send_json(serde_json::json!({ "force": force }))?;
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
