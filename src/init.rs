use crate::config::{default_atr_dir, expand_path, kind_defaults};
use crate::detect::{self, Discovery, ProviderPick};
use crate::service;
use anyhow::{bail, Context, Result};
use std::fs;
use std::io::{self, BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};

pub use crate::detect::first_version_token;

const EXAMPLE_TOML: &str = include_str!("../config/example.toml");
const SYSTEM_PROMPT: &str = include_str!("../config/system-prompt.md");
const ISSUE_TEMPLATE: &str = include_str!("../contrib/.github/ISSUE_TEMPLATE/atr-task.md");
const ISSUE_TEMPLATE_REL: &str = ".github/ISSUE_TEMPLATE/atr-task.md";

#[derive(Debug, Clone)]
pub struct InitOpts {
    pub yes: bool,
    pub force: bool,
    pub no_service: bool,
    pub force_service: bool,
    pub config_path: PathBuf,
}

pub fn detect_clis() -> Discovery {
    detect::discover(&detect::current_env())
}

pub(crate) fn prompt_line(label: &str, default: &str, yes: bool) -> Result<String> {
    if yes {
        return Ok(default.to_string());
    }
    if !std::io::stdin().is_terminal() {
        bail!("stdin is not a terminal; pass --yes for defaults");
    }
    let mut stdout = io::stdout();
    if default.is_empty() {
        write!(stdout, "{label}: ")?;
    } else {
        write!(stdout, "{label} [{default}]: ")?;
    }
    stdout.flush()?;
    let mut line = String::new();
    io::stdin().lock().read_line(&mut line)?;
    let t = line.trim();
    if t.is_empty() {
        Ok(default.to_string())
    } else {
        Ok(t.to_string())
    }
}

pub(crate) fn yn(label: &str, default: bool, yes: bool) -> Result<bool> {
    let def = if default { "Y/n" } else { "y/N" };
    let ans = prompt_line(&format!("{label} ({def})"), "", yes)?;
    if ans.is_empty() {
        return Ok(default);
    }
    Ok(matches!(ans.to_ascii_lowercase().as_str(), "y" | "yes"))
}

pub fn run(opts: InitOpts) -> Result<String> {
    let dir = opts
        .config_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(default_atr_dir);
    fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;

    let discovered = detect_clis();
    let existing = crate::config::load_config(&opts.config_path).ok();
    let report = detect::format_report(&discovered, existing.as_ref());
    print!("{report}");

    let gh_guide = github_auth_guide(&opts.config_path);
    print!("{gh_guide}");

    let picks = if opts.yes {
        detect::yes_picks(&discovered)
    } else {
        choose_picks(&discovered)?
    };

    let cwd_git = crate::repos::detect_cwd_git();
    if !opts.yes {
        if let Some((root, gh)) = &cwd_git {
            eprintln!(
                "current git repo: {}{}",
                root.display(),
                gh.as_deref().map(|s| format!(" ({s})")).unwrap_or_default()
            );
        }
    }
    let default_repo = expand_path("~/src");
    let repo_default = if let Some((root, _)) = &cwd_git {
        compact_home(root)
    } else if default_repo.exists() {
        "~/src".to_string()
    } else {
        "~/src/my-repo".to_string()
    };
    let repo_path = prompt_line("repo_path", &repo_default, opts.yes)?;
    let gh_label = prompt_line(
        "GitHub issue label (empty disables poller)",
        crate::config::DEFAULT_GITHUB_LABEL,
        opts.yes,
    )?;
    let tz = prompt_line("display_timezone (empty = local)", "", opts.yes)?;
    let driver_default = if picks.iter().any(|p| p.name == "grok") {
        "grok"
    } else {
        picks
            .iter()
            .find(|p| p.enabled)
            .map(|p| p.name.as_str())
            .unwrap_or("grok")
    };
    let driver = prompt_line("driver (planning/review)", driver_default, opts.yes)?;

    let detected_github = cwd_git.as_ref().and_then(|(_, g)| g.clone());
    let toml = render_config(
        &picks,
        &repo_path,
        &gh_label,
        &tz,
        detected_github.as_deref(),
        &driver,
    );
    // A non-default config dir keeps its own system prompt (init writes it
    // next to config.toml), so point the config at that copy. Otherwise
    // doctor fails right after `atr init --config <dir>/config.toml`.
    let custom_dir = !same_dir(&dir, &crate::config::default_atr_dir());
    let toml = if custom_dir {
        replace_assign(
            &toml,
            "system_prompt_file",
            &quote(&compact_home(&dir.join("system-prompt.md"))),
        )
    } else {
        toml
    };
    write_file(&opts.config_path, &toml, opts.force, true)?;
    write_file(
        &dir.join("system-prompt.md"),
        SYSTEM_PROMPT,
        opts.force,
        false,
    )?;
    let bin = crate::util::current_bin();
    write_file(
        &dir.join("atr-serve.sh"),
        &service::render_serve_sh(&bin),
        opts.force,
        false,
    )?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let sh = dir.join("atr-serve.sh");
        fs::set_permissions(&sh, fs::Permissions::from_mode(0o755)).ok();
    }

    let mut out = format!("wrote {}\n", opts.config_path.display());
    match ensure_mcp_token(&opts.config_path, opts.yes) {
        Ok(Some(msg)) => out.push_str(&msg),
        Ok(None) => {}
        Err(e) => out.push_str(&format!("mcp token: {e:#}\n")),
    }
    out.push_str(&report);
    out.push_str(&gh_guide);
    // Skills go into the user's real CLI dirs (~/.claude, ~/.codex, ...).
    // A test/side config must not touch those.
    let skill_home_set = std::env::var_os("ATR_SKILL_HOME").is_some_and(|v| !v.is_empty());
    if custom_dir && !skill_home_set {
        out.push_str(
            "skill install skipped (non-default config dir; set ATR_SKILL_HOME to install)\n",
        );
    } else {
        match crate::skill::install_default() {
            Ok(Some(msg)) => out.push_str(&msg),
            Ok(None) => {}
            Err(e) => out.push_str(&format!("skill install: {e:#}\n")),
        }
    }
    match store_opencode_creds(&opts.config_path, &picks, opts.yes) {
        Ok(Some(msg)) => out.push_str(&msg),
        Ok(None) => {}
        Err(e) => out.push_str(&format!("opencode presets: {e:#}\n")),
    }
    if !opts.yes {
        match maybe_telegram_setup(&opts.config_path) {
            Ok(Some(msg)) => out.push_str(&msg),
            Ok(None) => {}
            Err(e) => out.push_str(&format!("telegram setup: {e:#}\n")),
        }
        match maybe_issue_template(&repo_path) {
            Ok(Some(msg)) => out.push_str(&msg),
            Ok(None) => {}
            Err(e) => out.push_str(&format!("issue template: {e:#}\n")),
        }
        match maybe_add_cwd_repo(&opts.config_path, existing.as_ref(), &cwd_git, &repo_path) {
            Ok(Some(msg)) => out.push_str(&msg),
            Ok(None) => {}
            Err(e) => out.push_str(&format!("add current repo: {e:#}\n")),
        }
    }
    if offer_service_install(opts.yes, opts.no_service)? {
        match crate::config::load_config(&opts.config_path) {
            Ok(cfg) => {
                match service::install_for_init(Some(&cfg), &opts.config_path, opts.force_service) {
                    Ok(msg) => {
                        out.push_str(&msg);
                        out.push('\n');
                    }
                    Err(e) => out.push_str(&format!("service install: {e:#}\n")),
                }
            }
            Err(e) => out.push_str(&format!("config load after init: {e:#}\n")),
        }
    } else {
        out.push_str(SERVICE_SKIPPED_HINT);
        out.push('\n');
    }
    Ok(out)
}

/// Printed when init skips the user service so “no” is not a dead end.
pub const SERVICE_SKIPPED_HINT: &str =
    "skipped user service. Bare `atr` / `atr tui` / `atr ask` still work in-process. Telegram, GitHub watching, and queued `atr add` need `atr service install` or `atr serve` later.";

/// What the user service is for, plus platform install notes.
pub fn service_offer_explainer() -> String {
    let mut s = String::from(
        "The user service runs `atr serve` in the background so Telegram, GitHub watching, and queued `atr add` keep working when you are not in the TUI.\n",
    );
    if cfg!(target_os = "macos") {
        s.push_str(
            "This installs LaunchAgent `dev.atr.serve` (load it from the desktop session, not SSH).\n",
        );
    } else {
        s.push_str(
            "This installs systemd user unit `atr.service`. On a headless Linux host also run `sudo loginctl enable-linger \"$USER\"` so it survives logout.\n",
        );
    }
    s.push_str("Say no to skip; you can run `atr service install` or `atr serve` later.\n");
    s
}

/// `--no-service` skips; `--yes` installs (existing convention); otherwise
/// explain and ask on a TTY (`yn` refuses without a terminal).
pub fn offer_service_install(yes: bool, no_service: bool) -> Result<bool> {
    if no_service {
        return Ok(false);
    }
    if yes {
        return Ok(true);
    }
    eprint!("{}", service_offer_explainer());
    yn("Install the user service now", true, false)
}

fn ensure_mcp_token(config_path: &Path, yes: bool) -> Result<Option<String>> {
    let dest = config_path
        .parent()
        .map(|p| p.join("mcp.token"))
        .unwrap_or_else(|| crate::config::default_atr_dir().join("mcp.token"));
    if dest.exists() {
        point_config_at_token_file(config_path, &dest)?;
        return Ok(Some(format!(
            "mcp token file exists ({}, not shown)\n",
            dest.display()
        )));
    }
    let make = yn(
        "generate an MCP HTTP token (0600 file; never printed)",
        true,
        yes,
    )?;
    if !make {
        return Ok(None);
    }
    let token = crate::mcp::generate_token();
    crate::mcp::write_token_file(&dest, &token)?;
    point_config_at_token_file(config_path, &dest)?;
    Ok(Some(format!(
        "mcp token written to {} (mode 0600; atr never prints it). HTTP clients send Authorization: Bearer $ATR_MCP_TOKEN. `atr mcp` stdio needs no token.\n",
        dest.display()
    )))
}

fn same_dir(a: &Path, b: &Path) -> bool {
    let ca = fs::canonicalize(a).unwrap_or_else(|_| a.to_path_buf());
    let cb = fs::canonicalize(b).unwrap_or_else(|_| b.to_path_buf());
    ca == cb
}

fn point_config_at_token_file(config_path: &Path, dest: &Path) -> Result<()> {
    if !config_path.exists() {
        return Ok(());
    }
    let raw = fs::read_to_string(config_path)?;
    let mut doc = raw
        .parse::<toml_edit::DocumentMut>()
        .context("parse config to set mcp.token_file")?;
    let already = doc
        .get("mcp")
        .and_then(|i| i.get("token_file"))
        .and_then(|i| i.as_str())
        .is_some_and(|s| crate::config::expand_path(s) == dest);
    if already {
        return Ok(());
    }
    if !doc.contains_key("mcp") {
        doc["mcp"] = toml_edit::table();
    }
    doc["mcp"]["token_file"] = toml_edit::value(compact_home(dest));
    fs::write(config_path, doc.to_string())?;
    Ok(())
}

fn write_file(path: &Path, contents: &str, force: bool, mergeable: bool) -> Result<()> {
    if path.exists() && !force {
        if mergeable {
            let existing = fs::read_to_string(path)?;
            let merged = merge_toml(&existing, contents)?;
            if merged != existing {
                fs::write(path, merged)?;
                eprintln!("merged new keys into {}", path.display());
            } else {
                eprintln!(
                    "{} exists; left unchanged (pass --force to overwrite)",
                    path.display()
                );
            }
            return Ok(());
        }
        eprintln!(
            "{} exists; left unchanged (pass --force to overwrite)",
            path.display()
        );
        return Ok(());
    }
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    fs::write(path, contents).with_context(|| format!("write {}", path.display()))
}

fn merge_toml(existing: &str, template: &str) -> Result<String> {
    let mut dest = existing
        .parse::<toml_edit::DocumentMut>()
        .context("parse existing config")?;
    let src = template
        .parse::<toml_edit::DocumentMut>()
        .context("parse template")?;
    merge_item(dest.as_table_mut(), src.as_table());
    Ok(dest.to_string())
}

fn merge_item(dest: &mut toml_edit::Table, src: &toml_edit::Table) {
    for (key, val) in src.iter() {
        if key == "provider" || key == "repo" {
            continue;
        }
        if dest.contains_key(key) {
            if let (Some(dt), Some(st)) = (dest[key].as_table_mut(), val.as_table()) {
                merge_item(dt, st);
            }
            continue;
        }
        dest.insert(key, val.clone());
    }
}

fn compact_home(path: &Path) -> String {
    if let Some(home) = std::env::var_os("HOME") {
        if let Ok(rest) = path.strip_prefix(home) {
            return format!("~/{}", rest.display());
        }
    }
    path.display().to_string()
}

fn maybe_add_cwd_repo(
    config_path: &Path,
    existing: Option<&crate::config::Config>,
    cwd_git: &Option<(PathBuf, Option<String>)>,
    written_repo_path: &str,
) -> Result<Option<String>> {
    let Some((root, Some(github))) = cwd_git else {
        return Ok(None);
    };
    let written = expand_path(written_repo_path);
    if crate::config::paths_eq(&written, root) {
        return Ok(None);
    }
    let Some(existing) = existing else {
        return Ok(None);
    };
    if existing.has_explicit_repos() {
        if existing.repo_for_path(root).is_some()
            || existing
                .effective_repos()
                .iter()
                .any(|r| r.github == *github)
        {
            return Ok(None);
        }
    } else if crate::config::paths_eq(&existing.repo_path, root) || existing.github.repo == *github
    {
        return Ok(None);
    }
    if !yn(
        &format!(
            "Add current git repo {github} at {} as another watch target?",
            root.display()
        ),
        true,
        false,
    )? {
        return Ok(None);
    }
    let msg = crate::repos::add(
        config_path,
        github,
        crate::repos::AddOpts {
            path: root,
            label: None,
            name: None,
            clone_if_missing: false,
        },
    )?;
    Ok(Some(msg))
}

fn render_config(
    picks: &[ProviderPick],
    repo_path: &str,
    gh_label: &str,
    tz: &str,
    github_repo: Option<&str>,
    driver: &str,
) -> String {
    let mut out = EXAMPLE_TOML.to_string();
    out = replace_assign(&out, "repo_path", &quote(repo_path));
    out = replace_assign(&out, "display_timezone", &quote(tz));
    if !driver.trim().is_empty() {
        out = replace_assign(&out, "driver", &quote(driver.trim()));
    }
    if let Some(gh) = github_repo {
        out = set_table_assign(&out, "[github]", "repo", &quote(gh));
    }
    if gh_label.is_empty() {
        out = replace_assign(&out, "label", "\"\"");
    } else {
        out = replace_assign(&out, "label", &quote(gh_label));
        // Default `for:atr` stays in the template with the poller off. A
        // custom label still turns the poller on (same as before).
        if gh_label != crate::config::DEFAULT_GITHUB_LABEL {
            out = set_table_bool(&out, "[github]", "enabled", true);
        }
    }
    if cfg!(target_os = "macos") {
        out = replace_assign(&out, "path", &quote("~/Library/Logs/atr/atr.log"));
    }

    // Rebuild provider blocks in requested order.
    if let Some(idx) = out.find("[[provider]]") {
        out.truncate(idx);
    }
    out.push('\n');
    for p in picks {
        out.push_str("[[provider]]\n");
        out.push_str(&format!("name = \"{}\"\n", escape(&p.name)));
        out.push_str(&format!("kind = \"{}\"\n", escape(&p.kind)));
        out.push_str(&format!(
            "enabled = {}\n",
            if p.enabled { "true" } else { "false" }
        ));
        if let Some(preset) = &p.preset {
            out.push_str(&format!("preset = \"{}\"\n", escape(preset)));
        }
        if let Some(model) = &p.model {
            if p.preset.is_none() {
                out.push_str(&format!("model = \"{}\"\n", escape(model)));
            }
        }
        if let Some(cli) = &p.cli {
            out.push_str(&format!(
                "cli = \"{}\"\n",
                escape(&cli.display().to_string())
            ));
        } else if p.kind != "opencode" {
            if let Some(kd) = kind_defaults(&p.kind) {
                if let Some(name) = kd.cli_names.first() {
                    out.push_str(&format!(
                        "# cli not found on PATH; doctor will warn\ncli = \"{name}\"\n"
                    ));
                }
            }
        }
        if p.kind == "codex" {
            out.push_str("# config_overrides = [\"model_provider=\\\"openai\\\"\"]\n");
        }
        out.push('\n');
    }
    out.push_str("# OpenCode spark (atr providers add spark):\n# [[provider]]\n# name = \"spark\"\n# kind = \"opencode\"\n# preset = \"spark\"\n");
    out.push_str("# Older Spark-via-Codex path:\n# [[provider]]\n# name = \"spark\"\n# kind = \"codex\"\n# config_overrides = [\"model_provider=\\\"openai\\\"\"]\n");
    out
}

fn choose_picks(d: &Discovery) -> Result<Vec<ProviderPick>> {
    let mut picks = detect::default_subscription_picks(d);
    for p in picks.iter_mut() {
        let extra = match (p.cli.is_some(), p.enabled) {
            (true, true) => "signed in",
            (true, false) => "installed, not signed in",
            _ => "not found",
        };
        p.enabled = yn(&format!("enable {} ({extra})", p.name), p.enabled, false)?;
    }
    if let Some(oc) = &d.opencode {
        let state = if oc.providers.is_empty() {
            "installed, not signed in"
        } else {
            "signed in"
        };
        if yn(&format!("enable opencode ({state})"), false, false)? {
            if oc.models.is_empty() {
                println!("  no OpenCode models available yet.");
            } else {
                println!("  OpenCode models available to you:");
                for (i, m) in oc.models.iter().enumerate() {
                    println!("    {:>2}) {m}", i + 1);
                }
            }
            println!("  (more providers can be connected with `opencode auth login`)");
            if !oc.models.is_empty() {
                let ans = prompt_line(
                    "add which models? (numbers, comma-separated; empty = none)",
                    "",
                    false,
                )?;
                for p in opencode_choices(&ans, &oc.models) {
                    if !picks.iter().any(|e| e.name == p.name) {
                        picks.push(p);
                    }
                }
            }
        }
    }
    let enabled = picks.iter().filter(|p| p.enabled).count();
    let order = if enabled > 1 {
        prompt_line(
            "order to try enabled providers (comma-separated, first = preferred)",
            &detect::default_order_csv(&picks),
            false,
        )?
    } else {
        String::new()
    };
    Ok(detect::reorder_picks(picks, &order))
}

/// Map "1,3" (or model ids) against the listed OpenCode models.
fn opencode_choices(ans: &str, models: &[String]) -> Vec<ProviderPick> {
    ans.split(',')
        .filter_map(|t| {
            let t = t.trim();
            let tok = match t.parse::<usize>() {
                Ok(n) if n >= 1 && n <= models.len() => models[n - 1].as_str(),
                Ok(_) => return None,
                Err(_) => t,
            };
            detect::pick_from_token(tok)
        })
        .collect()
}

fn store_opencode_creds(
    config_path: &Path,
    picks: &[ProviderPick],
    yes: bool,
) -> Result<Option<String>> {
    let mut out = String::new();
    for p in picks {
        if p.kind != "opencode" {
            continue;
        }
        let Some(preset) = &p.preset else {
            continue;
        };
        match crate::providers_edit::ensure_preset_setup(config_path, preset, yes) {
            Ok(()) => {
                if !yes {
                    out.push_str(&format!("opencode preset {preset} ready\n"));
                }
            }
            Err(e) => out.push_str(&format!("skip {preset}: {e:#}\n")),
        }
    }
    if out.is_empty() {
        Ok(None)
    } else {
        Ok(Some(out))
    }
}

fn quote(s: &str) -> String {
    format!("\"{}\"", escape(s))
}

fn escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

fn replace_assign(text: &str, key: &str, value: &str) -> String {
    let mut out = String::new();
    let prefix = format!("{key} = ");
    let mut done = false;
    for line in text.lines() {
        if !done && line.starts_with(&prefix) {
            out.push_str(&prefix);
            out.push_str(value);
            out.push('\n');
            done = true;
        } else {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

fn set_table_assign(text: &str, header: &str, key: &str, value: &str) -> String {
    let mut out = String::new();
    let mut in_table = false;
    let prefix = format!("{key} = ");
    for line in text.lines() {
        if line.trim() == header {
            in_table = true;
            out.push_str(line);
            out.push('\n');
            continue;
        }
        if in_table && line.starts_with('[') {
            in_table = false;
        }
        if in_table && line.starts_with(&prefix) {
            out.push_str(&format!("{key} = {value}\n"));
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

fn set_table_bool(text: &str, header: &str, key: &str, val: bool) -> String {
    let mut out = String::new();
    let mut in_table = false;
    let prefix = format!("{key} = ");
    for line in text.lines() {
        if line.trim() == header {
            in_table = true;
            out.push_str(line);
            out.push('\n');
            continue;
        }
        if in_table && line.starts_with('[') {
            in_table = false;
        }
        if in_table && line.starts_with(&prefix) {
            out.push_str(&format!("{key} = {val}\n"));
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

fn github_auth_guide(config_path: &Path) -> String {
    let snap = crate::github_auth::probe(crate::github_auth::ProbeOpts {
        config_path,
        gh_cli: "gh",
        shadow_token_env: "ATR_SHADOW_GH_TOKEN",
        probe_ssh: true,
        probe_scopes: false,
    });
    crate::github_auth::format_init_guide(&snap)
}

fn maybe_telegram_setup(config_path: &Path) -> Result<Option<String>> {
    if !yn("Set up Telegram (notifications and control)", false, false)? {
        return Ok(None);
    }
    Ok(Some(crate::telegram_setup::setup_interactive(config_path)?))
}

/// Copy the shipped issue template into `repo/.github/ISSUE_TEMPLATE/`.
/// Local file only — never `git add` or push.
pub fn install_issue_template(repo: &Path) -> Result<String> {
    if !repo.is_dir() {
        bail!("{} is not a directory", repo.display());
    }
    let dest = repo.join(ISSUE_TEMPLATE_REL);
    if dest.exists() {
        return Ok(format!("{} exists; left unchanged\n", dest.display()));
    }
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    fs::write(&dest, ISSUE_TEMPLATE).with_context(|| format!("write {}", dest.display()))?;
    Ok(format!(
        "wrote {} (local file only; atr does not commit or push)\n",
        dest.display()
    ))
}

fn maybe_issue_template(repo_path: &str) -> Result<Option<String>> {
    let repo = crate::config::expand_path(repo_path);
    if !repo.is_dir() {
        return Ok(None);
    }
    let dest = repo.join(ISSUE_TEMPLATE_REL);
    if !yn(
        &format!(
            "Copy atr issue template into {}? (local file only; never pushed)",
            dest.display()
        ),
        false,
        false,
    )? {
        return Ok(None);
    }
    Ok(Some(install_issue_template(&repo)?))
}

pub async fn run_and_doctor(opts: InitOpts) -> Result<String> {
    let path = opts.config_path.clone();
    let mut text = run(opts)?;
    match crate::config::load_config(&path) {
        Ok(cfg) => {
            let report = crate::doctor::run(&cfg, &path, false).await;
            text.push('\n');
            text.push_str(&report.format());
            if report.problems() > 0 {
                text.push_str(&format!(
                    "\ndoctor found {} problem(s); fix those and re-run atr doctor\n",
                    report.problems()
                ));
            }
        }
        Err(e) => text.push_str(&format!("\ndoctor skipped: {e:#}\n")),
    }
    Ok(text)
}

/// Hint for scripts / CI when config is missing (never hangs on prompts).
pub fn config_missing_hint(path: &Path) -> String {
    format!(
        "config not found at {}. Run `atr init` first, or `atr init --yes` for defaults.",
        path.display()
    )
}

fn interactive_stdio() -> bool {
    std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
}

/// Options for [`ensure_config_with`]. Production uses [`ensure_config`].
#[derive(Debug, Clone)]
pub struct EnsureConfigOpts {
    pub config_path: PathBuf,
    /// `None` detects stdin+stdout TTY. Tests pass `Some(true/false)`.
    pub tty: Option<bool>,
    /// Skip the y/N confirm and run `atr init --yes` (tests).
    pub assume_yes: bool,
    pub no_service: bool,
}

/// If config is missing: on a TTY, offer and run the normal `atr init`;
/// otherwise fail with [`config_missing_hint`]. After a successful init
/// the caller continues the original command.
pub async fn ensure_config(path: &Path) -> Result<()> {
    ensure_config_with(EnsureConfigOpts {
        config_path: path.to_path_buf(),
        tty: None,
        assume_yes: false,
        no_service: false,
    })
    .await
}

pub async fn ensure_config_with(opts: EnsureConfigOpts) -> Result<()> {
    if opts.config_path.exists() {
        return Ok(());
    }
    let tty = opts.tty.unwrap_or_else(interactive_stdio);
    if !tty {
        bail!("{}", config_missing_hint(&opts.config_path));
    }
    eprintln!("No atr config at {}.", opts.config_path.display());
    eprintln!(
        "atr needs a config before it can run (providers, workspace). This is the normal `atr init`."
    );
    let go = if opts.assume_yes {
        true
    } else {
        yn("Run atr init now", true, false)?
    };
    if !go {
        bail!("{}", config_missing_hint(&opts.config_path));
    }
    let init = InitOpts {
        yes: opts.assume_yes,
        force: false,
        no_service: opts.no_service,
        force_service: false,
        config_path: opts.config_path.clone(),
    };
    let text = if opts.assume_yes {
        run(init)?
    } else {
        run_and_doctor(init).await?
    };
    print!("{text}");
    if !opts.config_path.exists() {
        bail!(
            "init finished but config is still missing at {}",
            opts.config_path.display()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_version_token_reads_semver() {
        assert_eq!(
            first_version_token("claude 2.1.287 (built)"),
            Some("2.1.287".into())
        );
    }

    #[test]
    fn merge_adds_missing_top_level_key() {
        let existing = "listen = \"127.0.0.1:1\"\nrepo_path = \"/r\"\n";
        let template = "listen = \"127.0.0.1:7420\"\nworktree_root = \"~/atr-work\"\n";
        let merged = merge_toml(existing, template).unwrap();
        assert!(merged.contains("repo_path"));
        assert!(merged.contains("127.0.0.1:1"), "must keep existing listen");
        assert!(merged.contains("worktree_root"));
    }

    #[test]
    fn merge_adds_mcp_table() {
        let existing = "listen = \"127.0.0.1:1\"\nrepo_path = \"/r\"\n";
        let merged = merge_toml(existing, EXAMPLE_TOML).unwrap();
        assert!(merged.contains("[mcp]"), "{merged}");
        assert!(merged.contains("ATR_MCP_TOKEN"), "{merged}");
        assert!(merged.contains("127.0.0.1:1"), "{merged}");
    }

    #[test]
    fn merge_keeps_existing_github_label() {
        let existing = "[github]\nenabled = true\nlabel = \"for:atr\"\n";
        let template = "[github]\nenabled = false\nlabel = \"for:example\"\n";
        let merged = merge_toml(existing, template).unwrap();
        assert!(merged.contains("for:atr"), "{merged}");
        assert!(
            !merged.contains("for:example"),
            "must not rewrite existing label\n{merged}"
        );
        assert!(merged.contains("enabled = true"), "{merged}");
    }

    #[test]
    fn issue_template_has_label_and_headings() {
        assert!(
            ISSUE_TEMPLATE.contains("labels: for:atr"),
            "{ISSUE_TEMPLATE}"
        );
        for h in [
            "## Goal",
            "## Where in the code, if known",
            "## Done when",
            "## Notes",
        ] {
            assert!(ISSUE_TEMPLATE.contains(h), "missing {h}");
        }
    }

    #[test]
    fn install_issue_template_writes_once_and_never_touches_git() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        let msg = install_issue_template(repo).unwrap();
        let dest = repo.join(ISSUE_TEMPLATE_REL);
        assert!(dest.exists(), "{msg}");
        assert!(msg.contains("does not commit or push"), "{msg}");
        let text = std::fs::read_to_string(&dest).unwrap();
        assert_eq!(text, ISSUE_TEMPLATE);
        assert!(!repo.join(".git").exists());
        std::fs::write(&dest, "keep\n").unwrap();
        let again = install_issue_template(repo).unwrap();
        assert!(again.contains("left unchanged"), "{again}");
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "keep\n");
    }

    #[test]
    fn init_yes_skips_telegram() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let msg = run(InitOpts {
            yes: true,
            force: true,
            no_service: true,
            force_service: false,
            config_path: path.clone(),
        })
        .unwrap();
        let toml = std::fs::read_to_string(&path).unwrap();
        assert!(toml.contains("enabled = false"), "{toml}");
        assert!(toml.contains("allowed_chat_ids = []"), "{toml}");
        assert!(!dir.path().join("shadow.env").exists());
        assert!(!msg.to_ascii_lowercase().contains("telegram enabled"));
        assert!(
            msg.contains("atr service install") && msg.contains("in-process"),
            "skipping the service must say how to install later: {msg}"
        );
        assert!(
            !dir.path().join(".github").exists(),
            "--yes must not copy the issue template without confirmation"
        );
        assert!(
            !msg.contains("ISSUE_TEMPLATE"),
            "--yes must not offer the issue template: {msg}"
        );
        assert!(
            msg.contains("GitHub authentication"),
            "init must detect GitHub auth and guide: {msg}"
        );
        assert!(
            !crate::github_auth::looks_like_secret(&msg),
            "init must never print tokens: {msg}"
        );
        assert!(msg.contains("detected CLIs"), "{msg}");
    }

    #[test]
    fn opencode_choices_by_number() {
        let models = vec![
            "opencode/big-pickle".to_string(),
            "opencode/other".to_string(),
        ];
        assert!(opencode_choices("", &models).is_empty());
        assert!(opencode_choices("9", &models).is_empty());
        let p = opencode_choices("2", &models);
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].kind, "opencode");
        assert_eq!(p[0].model.as_deref(), Some("opencode/other"));
    }

    #[test]
    fn init_yes_enables_signed_in_then_opencode() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let bin = dir.path().join("bin");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&bin).unwrap();
        let script = r#"#!/bin/sh
name=$(basename "$0")
if [ "$1" = "--version" ]; then echo "$name 9.9.9"; exit 0; fi
if [ "$1" = "models" ]; then echo "moonshotai/kimi-k2.6"; exit 0; fi
if [ "$1" = "auth" ]; then
  if [ "$name" = "opencode" ] || [ "$name" = "claude" ]; then
    if [ "$2" = "list" ]; then echo "moonshotai"; exit 0; fi
    echo "logged in"; exit 0
  fi
  echo "not logged in"; exit 1
fi
exit 0
"#;
        for name in ["claude", "grok", "opencode"] {
            let p = bin.join(name);
            std::fs::write(&p, script).unwrap();
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        struct Clear;
        impl Drop for Clear {
            fn drop(&mut self) {
                crate::detect::set_test_env(None);
                crate::opencode_cfg::set_test_path(None);
            }
        }
        let _clear = Clear;
        crate::detect::set_test_env(Some(crate::detect::DetectEnv {
            path: bin.as_os_str().to_owned(),
            home: home.clone(),
            search_system: false,
        }));
        crate::opencode_cfg::set_test_path(Some(home.join("opencode.json")));
        std::env::set_var("ATR_SKILL_HOME", dir.path());
        let path = dir.path().join("config.toml");
        let msg = run(InitOpts {
            yes: true,
            force: true,
            no_service: true,
            force_service: false,
            config_path: path.clone(),
        })
        .unwrap();
        assert!(msg.contains("login: grok"), "{msg}");
        assert!(msg.contains("signed in"), "{msg}");
        let cfg = crate::config::load_config(&path).unwrap();
        let names: Vec<_> = cfg.providers.iter().map(|p| p.name.as_str()).collect();
        assert!(cfg.provider("claude").unwrap().enabled, "{names:?}");
        assert!(!cfg.provider("grok").unwrap().enabled, "{names:?}");
        assert!(!cfg.provider("cursor").unwrap().enabled, "{names:?}");
        let kimi = cfg.provider("kimi").expect("kimi from opencode");
        assert!(kimi.enabled);
        assert_eq!(kimi.kind, "opencode");
        assert_eq!(kimi.preset, "kimi");
        let c = names.iter().position(|n| *n == "claude").unwrap();
        let k = names.iter().position(|n| *n == "kimi").unwrap();
        assert!(c < k, "subscriptions first: {names:?}");
        assert!(
            !std::fs::read_to_string(&path)
                .unwrap()
                .contains("expected_version"),
            "init must not write expected_version"
        );
    }

    #[test]
    fn init_yes_skips_service_for_non_default_config() {
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("ATR_INIT_SKIP_LOAD", "1");
        std::env::set_var("ATR_SKILL_HOME", dir.path());
        let path = dir.path().join("config.toml");
        let msg = run(InitOpts {
            yes: true,
            force: true,
            no_service: false,
            force_service: false,
            config_path: path,
        })
        .unwrap();
        assert!(
            msg.contains("skipped user service") || msg.contains("--force-service"),
            "{msg}"
        );
    }

    #[tokio::test]
    async fn ensure_config_noop_when_present() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, "listen = \"127.0.0.1:0\"\n").unwrap();
        ensure_config_with(EnsureConfigOpts {
            config_path: path.clone(),
            tty: Some(false),
            assume_yes: false,
            no_service: true,
        })
        .await
        .unwrap();
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "listen = \"127.0.0.1:0\"\n"
        );
    }

    #[tokio::test]
    async fn ensure_config_non_tty_fails_without_writing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing").join("config.toml");
        let err = ensure_config_with(EnsureConfigOpts {
            config_path: path.clone(),
            tty: Some(false),
            assume_yes: true,
            no_service: true,
        })
        .await
        .unwrap_err()
        .to_string();
        assert!(!path.exists(), "non-TTY must not run init");
        assert!(err.contains("atr init"), "{err}");
        assert!(err.contains("--yes"), "{err}");
    }

    #[test]
    fn offer_service_install_follows_flags_and_refuses_non_tty() {
        assert!(offer_service_install(true, false).unwrap());
        assert!(!offer_service_install(true, true).unwrap());
        assert!(!offer_service_install(false, true).unwrap());
        let err = offer_service_install(false, false).unwrap_err().to_string();
        assert!(
            err.contains("not a terminal") || err.contains("--yes"),
            "{err}"
        );
        let explain = service_offer_explainer();
        assert!(explain.contains("atr serve"), "{explain}");
        assert!(explain.contains("Telegram"), "{explain}");
        assert!(
            explain.contains("atr service install") && explain.contains("atr serve"),
            "{explain}"
        );
        assert!(SERVICE_SKIPPED_HINT.contains("atr service install"),);
        assert!(SERVICE_SKIPPED_HINT.contains("in-process"),);
    }

    #[tokio::test]
    async fn ensure_config_tty_assume_yes_runs_init() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        ensure_config_with(EnsureConfigOpts {
            config_path: path.clone(),
            tty: Some(true),
            assume_yes: true,
            no_service: true,
        })
        .await
        .unwrap();
        assert!(path.exists());
        crate::config::load_config(&path).unwrap();
    }
}
