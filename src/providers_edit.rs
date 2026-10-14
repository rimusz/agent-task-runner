use crate::config::{is_known_kind, kind_defaults, Config};
use crate::presets::BuiltinPreset;
use crate::util::sighup_daemon;
use anyhow::{bail, Context, Result};
use std::io::IsTerminal;
use std::path::Path;
use toml_edit::{Array, ArrayOfTables, DocumentMut, Item, Table, Value};

#[derive(Debug, Clone)]
pub struct AddOpts<'a> {
    pub kind: Option<&'a str>,
    pub model: Option<&'a str>,
    pub cli: Option<&'a str>,
    pub overrides: &'a [String],
    pub key: Option<&'a str>,
    pub url: Option<&'a str>,
    pub yes: bool,
}

impl AddOpts<'static> {
    pub fn empty() -> Self {
        Self {
            kind: None,
            model: None,
            cli: None,
            overrides: &[],
            key: None,
            url: None,
            yes: false,
        }
    }
}

fn display_model(p: &crate::config::Provider) -> String {
    if !p.model.is_empty() {
        return p.model.clone();
    }
    crate::detect::cli_default_model(&p.kind).unwrap_or_else(|| "-".into())
}

pub fn list_text(cfg: &Config, shared: Option<&crate::app::Shared>) -> String {
    let clock = crate::clock::Clock::system();
    let mut rows: Vec<[String; 6]> = Vec::new();
    for (i, p) in cfg.providers.iter().enumerate() {
        let enabled = if let Some(s) = shared {
            s.db.enabled_override(&p.name)
                .ok()
                .flatten()
                .unwrap_or(p.enabled)
        } else {
            p.enabled
        };
        let usage = if let Some(s) = shared {
            crate::reported::format_provider_usage(
                &crate::reported::provider_usage_view(&s.db, &p.name),
                &cfg.display_timezone,
                clock.now(),
            )
        } else {
            String::new()
        };
        let mut skip: Vec<String> = Vec::new();
        if !enabled {
            skip.push("disabled".into());
        }
        if let Some(s) = shared {
            let (cooling, degraded) = s.db.provider_blocked(&p.name).unwrap_or((false, false));
            if cooling {
                skip.push("cooling".into());
            }
            if degraded {
                skip.push("degraded".into());
            }
            if crate::reported::reported_over(&s.db, p, &clock).unwrap_or(false) {
                skip.push("reported".into());
            }
        }
        let extra = if skip.is_empty() {
            usage
        } else if usage.is_empty() {
            skip.join(", ")
        } else {
            format!("{usage}  ({})", skip.join(", "))
        };
        rows.push([
            (i + 1).to_string(),
            if enabled { "yes".into() } else { "no".into() },
            p.name.clone(),
            p.kind.clone(),
            display_model(p),
            extra,
        ]);
    }
    let headers = ["#", "enabled", "name", "kind", "model", "usage / skip"];
    let widths: Vec<usize> = (0..headers.len())
        .map(|i| {
            rows.iter()
                .map(|r| r[i].chars().count())
                .chain(std::iter::once(headers[i].chars().count()))
                .max()
                .unwrap_or(1)
        })
        .collect();
    let mut out = String::new();
    for (i, h) in headers.iter().enumerate() {
        if i > 0 {
            out.push_str("  ");
        }
        out.push_str(&format!("{:<w$}", h, w = widths[i]));
    }
    out.push('\n');
    for r in &rows {
        for (i, cell) in r.iter().enumerate() {
            if i > 0 {
                out.push_str("  ");
            }
            out.push_str(&format!("{:<w$}", cell, w = widths[i]));
        }
        out.push('\n');
    }
    out
}

pub fn add(path: &Path, name: &str, opts: AddOpts<'_>) -> Result<()> {
    let cfg = crate::config::load_config(path).ok();
    let plan = resolve_add(cfg.as_ref(), name, opts.kind, opts.model, opts.cli)?;
    if let Some(bp) = plan.builtin {
        apply_builtin_creds(path, name, bp, &plan, &opts)?;
    }
    let kind = plan.kind.as_str();
    let mut doc = read_doc(path)?;
    let arr = provider_array(&mut doc)?;
    for t in arr.iter() {
        if t.get("name").and_then(|v| v.as_str()) == Some(name) {
            bail!("provider {name} already exists");
        }
    }
    let mut table = Table::new();
    table["name"] = value_str(name);
    table["kind"] = value_str(kind);
    table["enabled"] = Item::Value(Value::from(true));
    if let Some(preset) = &plan.preset {
        table["preset"] = value_str(preset);
    }
    if let Some(cli) = &plan.cli {
        table["cli"] = value_str(cli);
    }
    if plan.model_explicit || plan.builtin.is_none() {
        if let Some(model) = &plan.model {
            table["model"] = value_str(model);
        }
    }
    crate::config_edit::backup_config(path)?;
    if !opts.overrides.is_empty() {
        let mut a = Array::new();
        for o in opts.overrides {
            a.push(o.as_str());
        }
        table["config_overrides"] = Item::Value(Value::Array(a));
    } else if kind_defaults(kind).is_some() {
        // kind fills defaults; keep the block short
    }
    arr.push(table);
    write_doc(path, &doc)?;
    sighup_ok(path);
    Ok(())
}

pub fn set_enabled(path: &Path, name: &str, enabled: bool) -> Result<()> {
    crate::config_edit::apply_set(
        path,
        &format!("provider.{name}.enabled"),
        if enabled { "true" } else { "false" },
    )?;
    Ok(())
}

pub fn set_model(path: &Path, name: &str, model: &str) -> Result<()> {
    crate::config_edit::apply_set(path, &format!("provider.{name}.model"), model)?;
    Ok(())
}

/// Store a preset key / URL after init already wrote the `[[provider]]` block.
pub fn ensure_preset_setup(path: &Path, name: &str, yes: bool) -> Result<()> {
    let Some(bp) = crate::presets::builtin(name) else {
        return Ok(());
    };
    let plan = AddPlan {
        kind: bp.kind.into(),
        model: Some(bp.model.into()),
        cli: None,
        preset: Some(name.into()),
        builtin: Some(bp),
        model_explicit: false,
    };
    match apply_builtin_creds(
        path,
        name,
        bp,
        &plan,
        &AddOpts {
            kind: None,
            model: None,
            cli: None,
            overrides: &[],
            key: None,
            url: None,
            yes,
        },
    ) {
        Ok(()) => Ok(()),
        Err(e) if yes => {
            // --yes must not fail init when a key is missing; doctor will warn.
            let msg = e.to_string();
            if msg.contains("needs ") {
                Ok(())
            } else {
                Err(e)
            }
        }
        Err(e) => Err(e),
    }
}

pub fn preview_add(
    path: &Path,
    name: &str,
    kind: Option<&str>,
    model: Option<&str>,
) -> Result<String> {
    let cfg = crate::config::load_config(path).ok();
    if let Some(cfg) = &cfg {
        if cfg.provider(name).is_some() {
            bail!("provider {name} already exists");
        }
    }
    let plan = resolve_add(cfg.as_ref(), name, kind, model, None)?;
    let mut desc = format!(
        "kind={} model={}",
        plan.kind,
        plan.model.as_deref().unwrap_or("-")
    );
    if let Some(preset) = &plan.preset {
        desc = format!(
            "kind={} preset={preset} model={}",
            plan.kind,
            plan.model.as_deref().unwrap_or("-")
        );
    }
    Ok(desc)
}

pub fn move_to(path: &Path, name: &str, to: usize) -> Result<()> {
    if to == 0 {
        bail!("--to is 1-based");
    }
    crate::config_edit::backup_config(path)?;
    let text = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let next = reorder_provider_blocks(&text, name, to)?;
    std::fs::write(path, next).with_context(|| format!("write {}", path.display()))?;
    sighup_ok(path);
    Ok(())
}

/// Reorder `[[provider]]` text blocks. toml_edit's ArrayOfTables does not
/// reliably persist a rebuilt order, and a text move keeps comments.
fn reorder_provider_blocks(text: &str, name: &str, to: usize) -> Result<String> {
    let (header, mut blocks, trailing) = split_provider_blocks(text);
    let idx = blocks
        .iter()
        .position(|b| block_name(b).as_deref() == Some(name))
        .ok_or_else(|| anyhow::anyhow!("unknown provider {name}"))?;
    let dest = to - 1;
    if dest >= blocks.len() {
        bail!("--to {to} is past the last provider ({})", blocks.len());
    }
    if idx != dest {
        let block = blocks.remove(idx);
        blocks.insert(dest, block);
    }
    let mut out = header;
    for b in blocks {
        out.push_str(&b);
    }
    out.push_str(&trailing);
    Ok(out)
}

fn split_provider_blocks(text: &str) -> (String, Vec<String>, String) {
    let mut header = String::new();
    let mut blocks = Vec::new();
    let mut current: Option<String> = None;
    let mut trailing = String::new();
    let mut seen = false;
    for line in text.lines() {
        if line.trim() == "[[provider]]" {
            if let Some(c) = current.take() {
                blocks.push(c);
            }
            current = Some(format!("{line}\n"));
            seen = true;
            continue;
        }
        if seen && current.is_some() && is_commented_provider_start(line) {
            if let Some(c) = current.take() {
                let (block, blanks) = peel_trailing_blank_lines(&c);
                blocks.push(block);
                trailing.push_str(&blanks);
            }
            trailing.push_str(line);
            trailing.push('\n');
            continue;
        }
        if let Some(ref mut c) = current {
            c.push_str(line);
            c.push('\n');
        } else if seen {
            trailing.push_str(line);
            trailing.push('\n');
        } else {
            header.push_str(line);
            header.push('\n');
        }
    }
    if let Some(c) = current {
        let (block, rest) = peel_commented_provider_table(&c);
        blocks.push(block);
        if !rest.is_empty() {
            trailing = format!("{rest}{trailing}");
        }
    }
    (header, blocks, trailing)
}

fn is_commented_provider_start(line: &str) -> bool {
    let t = line.trim_start();
    t.starts_with('#') && t.contains("[[provider]]")
}

fn peel_trailing_blank_lines(block: &str) -> (String, String) {
    let mut lines: Vec<&str> = block.lines().collect();
    let mut blanks = String::new();
    while matches!(lines.last().copied(), Some(l) if l.trim().is_empty()) {
        blanks.insert(0, '\n');
        lines.pop();
    }
    let mut main = lines.join("\n");
    if !main.is_empty() && !main.ends_with('\n') {
        main.push('\n');
    }
    (main, blanks)
}

fn peel_commented_provider_table(block: &str) -> (String, String) {
    let lines: Vec<&str> = block.lines().collect();
    let cut = lines.iter().position(|l| is_commented_provider_start(l));
    let Some(cut) = cut else {
        return (ensure_trailing_nl(block), String::new());
    };
    let (main, blanks) = peel_trailing_blank_lines(&lines[..cut].join("\n"));
    let mut rest = blanks;
    rest.push_str(&lines[cut..].join("\n"));
    if !rest.is_empty() && !rest.ends_with('\n') {
        rest.push('\n');
    }
    (main, rest)
}

fn ensure_trailing_nl(s: &str) -> String {
    if s.is_empty() || s.ends_with('\n') {
        s.to_string()
    } else {
        format!("{s}\n")
    }
}

fn block_name(block: &str) -> Option<String> {
    for line in block.lines() {
        let t = line.trim();
        if t.starts_with('#') {
            continue;
        }
        let Some(rest) = t.strip_prefix("name") else {
            continue;
        };
        let rest = rest.trim();
        let Some(rest) = rest.strip_prefix('=') else {
            continue;
        };
        let name = rest.trim().trim_matches(|c| c == '"' || c == '\'');
        if !name.is_empty() {
            return Some(name.to_string());
        }
    }
    None
}

struct AddPlan {
    kind: String,
    model: Option<String>,
    cli: Option<String>,
    preset: Option<String>,
    builtin: Option<&'static BuiltinPreset>,
    /// True when the caller passed `--model` / a Telegram model override.
    model_explicit: bool,
}

fn resolve_add(
    cfg: Option<&Config>,
    name: &str,
    kind: Option<&str>,
    model: Option<&str>,
    cli: Option<&str>,
) -> Result<AddPlan> {
    let kind_explicit = kind.filter(|s| !s.is_empty());
    if let Some(kind) = kind_explicit {
        if !is_known_kind(kind) {
            bail!("unknown kind {kind}; use claude, grok, cursor, codex, or opencode");
        }
        // `atr providers add spark --kind codex` keeps the old Codex path.
        if kind != "opencode" {
            let model_explicit = model.map(str::trim).filter(|s| !s.is_empty()).is_some();
            let model = model
                .map(str::to_string)
                .filter(|s| !s.is_empty())
                .or_else(|| {
                    cfg.and_then(|c| c.preset(name))
                        .map(|p| p.model.clone())
                        .filter(|s| !s.is_empty())
                });
            let cli = cli
                .map(str::to_string)
                .filter(|s| !s.is_empty())
                .or_else(|| {
                    cfg.and_then(|c| c.preset(name))
                        .map(|p| p.cli.clone())
                        .filter(|s| !s.is_empty())
                });
            return Ok(AddPlan {
                kind: kind.to_string(),
                model,
                cli,
                preset: None,
                builtin: None,
                model_explicit,
            });
        }
    }
    if let Some(bp) = crate::presets::builtin(name) {
        if kind_explicit.is_none() || kind_explicit == Some("opencode") {
            let model_explicit = model.map(str::trim).filter(|s| !s.is_empty()).is_some();
            let model = model
                .map(str::to_string)
                .filter(|s| !s.is_empty())
                .or_else(|| Some(bp.model.to_string()));
            let cli = cli.map(str::to_string).filter(|s| !s.is_empty());
            return Ok(AddPlan {
                kind: "opencode".into(),
                model,
                cli,
                preset: Some(bp.name.to_string()),
                builtin: Some(bp),
                model_explicit,
            });
        }
    }
    if let Some(kind) = kind_explicit {
        let model_explicit = model.map(str::trim).filter(|s| !s.is_empty()).is_some();
        let model = model
            .map(str::to_string)
            .filter(|s| !s.is_empty())
            .or_else(|| {
                cfg.and_then(|c| c.preset(name))
                    .map(|p| p.model.clone())
                    .filter(|s| !s.is_empty())
            });
        let cli = cli
            .map(str::to_string)
            .filter(|s| !s.is_empty())
            .or_else(|| {
                cfg.and_then(|c| c.preset(name))
                    .map(|p| p.cli.clone())
                    .filter(|s| !s.is_empty())
            });
        return Ok(AddPlan {
            kind: kind.to_string(),
            model,
            cli,
            preset: None,
            builtin: None,
            model_explicit,
        });
    }
    let Some(preset) = cfg.and_then(|c| c.preset(name)) else {
        bail!(
            "unknown name {name}: pass --kind or use a built-in preset ({}) or a [[preset]]",
            crate::presets::builtin_names()
        );
    };
    let r = crate::presets::apply(preset, None, model, cli)?;
    Ok(AddPlan {
        kind: r.kind,
        model: if r.model.is_empty() {
            None
        } else {
            Some(r.model)
        },
        cli: r.cli,
        preset: Some(name.to_string()),
        builtin: None,
        model_explicit: model.map(str::trim).filter(|s| !s.is_empty()).is_some(),
    })
}

fn apply_builtin_creds(
    config_path: &Path,
    name: &str,
    bp: &BuiltinPreset,
    plan: &AddPlan,
    opts: &AddOpts<'_>,
) -> Result<()> {
    let shadow = crate::telegram_setup::shadow_env_path(config_path);
    let mut key_to_store: Option<String> = None;
    if let Some(env_name) = bp.env_key {
        if let Some(k) = opts.key.map(str::trim).filter(|s| !s.is_empty()) {
            key_to_store = Some(k.to_string());
        } else if let Ok(k) = std::env::var(env_name) {
            if !k.trim().is_empty()
                && crate::telegram_setup::read_shadow_env_var(&shadow, env_name).is_none()
            {
                key_to_store = Some(k);
            }
        } else if crate::telegram_setup::read_shadow_env_var(&shadow, env_name).is_some() {
            // already in shadow.env
        } else if opts.yes || !std::io::stdin().is_terminal() {
            bail!(
                "preset {name} needs {env_name}: pass --key or put it in {}",
                shadow.display()
            );
        } else {
            let k = crate::init::prompt_line(
                &format!("{env_name} (stored in shadow.env, never config)"),
                "",
                false,
            )?;
            if k.trim().is_empty() {
                bail!("preset {name} needs {env_name}");
            }
            key_to_store = Some(k);
        }
        if let Some(k) = &key_to_store {
            crate::telegram_setup::upsert_shadow_env(&shadow, env_name, k.trim())?;
        }
    }
    let url = if let Some(u) = opts.url.map(str::trim).filter(|s| !s.is_empty()) {
        crate::opencode_cfg::normalize_base_url(u, bp.base_url)
    } else if bp.needs_url && !opts.yes && std::io::stdin().is_terminal() {
        let u = crate::init::prompt_line("base URL", bp.base_url, false)?;
        crate::opencode_cfg::normalize_base_url(&u, bp.base_url)
    } else {
        bp.base_url.to_string()
    };
    let model = plan.model.as_deref().unwrap_or(bp.model);
    crate::opencode_cfg::merge_provider(
        &crate::opencode_cfg::config_path(),
        bp.oc_provider,
        bp.display,
        &url,
        bp.env_key,
        crate::presets::model_leaf(model),
    )?;
    Ok(())
}

fn provider_array(doc: &mut DocumentMut) -> Result<&mut ArrayOfTables> {
    if !doc.contains_key("provider") {
        doc["provider"] = Item::ArrayOfTables(ArrayOfTables::new());
    }
    doc["provider"]
        .as_array_of_tables_mut()
        .ok_or_else(|| anyhow::anyhow!("[[provider]] is not an array of tables"))
}

fn read_doc(path: &Path) -> Result<DocumentMut> {
    let text = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    text.parse()
        .with_context(|| format!("parse {}", path.display()))
}

fn write_doc(path: &Path, doc: &DocumentMut) -> Result<()> {
    std::fs::write(path, doc.to_string()).with_context(|| format!("write {}", path.display()))
}

fn value_str(s: &str) -> Item {
    Item::Value(Value::from(s))
}

fn sighup_ok(path: &Path) {
    if let Ok(cfg) = crate::config::load_config(path) {
        match sighup_daemon(&cfg) {
            Ok(()) => eprintln!("sent SIGHUP to atr serve"),
            Err(e) => eprintln!("config saved; SIGHUP skipped ({e})"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn seed() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let mut f = std::fs::File::create(&path).unwrap();
        write!(
            f,
            r#"
# keep this comment
listen = "127.0.0.1:0"
repo_path = "{repo}"
worktree_root = "{root}"
system_prompt_file = "{root}/p.md"

[[provider]]
name = "claude"
kind = "claude"
enabled = true

[[provider]]
name = "grok"
kind = "grok"
enabled = false
"#,
            repo = dir.path().display(),
            root = dir.path().display()
        )
        .unwrap();
        std::fs::write(dir.path().join("p.md"), "p\n").unwrap();
        (dir, path)
    }

    #[test]
    fn add_enable_move_preserve_comment() {
        let (_dir, path) = seed();
        add(
            &path,
            "codex",
            AddOpts {
                kind: Some("codex"),
                model: Some("gpt-5"),
                ..AddOpts::empty()
            },
        )
        .unwrap();
        set_enabled(&path, "grok", true).unwrap();
        move_to(&path, "codex", 1).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("# keep this comment"), "{text}");
        let cfg = crate::config::load_config(&path).unwrap();
        let names: Vec<_> = cfg.providers.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["codex", "claude", "grok"], "{text}");
        assert!(cfg.provider("grok").unwrap().enabled);
        assert_eq!(cfg.provider("codex").unwrap().model, "gpt-5");
        set_model(&path, "codex", "gpt-5.1").unwrap();
        let cfg = crate::config::load_config(&path).unwrap();
        assert_eq!(cfg.provider("codex").unwrap().model, "gpt-5.1");
        assert!(path.with_file_name("config.toml.bak").exists());
    }

    #[test]
    fn reorder_provider_blocks_moves_text() {
        let text = r#"# keep
listen = "127.0.0.1:0"

[[provider]]
name = "claude"
kind = "claude"

[[provider]]
name = "grok"
kind = "grok"

# [[provider]]
# name = "spark"
"#;
        let out = reorder_provider_blocks(text, "grok", 1).unwrap();
        assert!(out.starts_with("# keep"), "{out}");
        assert!(out.contains("# name = \"spark\""), "{out}");
        let names: Vec<_> = out
            .lines()
            .filter_map(|l| {
                let t = l.trim();
                t.strip_prefix("name = \"")
                    .and_then(|s| s.strip_suffix('"'))
                    .map(|s| s.to_string())
            })
            .collect();
        assert_eq!(names, ["grok", "claude"], "{out}");
        let live = r#"
repo_path = "/tmp"
[[provider]]
name = "claude"
kind = "claude"
[[provider]]
name = "grok"
kind = "grok"
"#;
        let moved = reorder_provider_blocks(live, "grok", 1).unwrap();
        let cfg = crate::config::parse_config_toml(&moved).unwrap();
        let names: Vec<_> = cfg.providers.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["grok", "claude"]);
    }

    #[test]
    fn add_builtin_kimi_stores_key_in_shadow_env() {
        let (dir, path) = seed();
        let oc = dir.path().join("opencode.json");
        crate::opencode_cfg::set_test_path(Some(oc.clone()));
        add(
            &path,
            "kimi",
            AddOpts {
                key: Some("sk-moon-test"),
                yes: true,
                ..AddOpts::empty()
            },
        )
        .unwrap();
        crate::opencode_cfg::set_test_path(None);
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(
            !written.contains("sk-moon-test"),
            "key must not land in config: {written}"
        );
        assert!(
            !written.to_ascii_lowercase().contains("api_key"),
            "{written}"
        );
        assert!(written.contains("preset = \"kimi\""), "{written}");
        assert!(written.contains("kind = \"opencode\""), "{written}");
        let shadow = std::fs::read_to_string(dir.path().join("shadow.env")).unwrap();
        assert!(shadow.contains("MOONSHOT_API_KEY=sk-moon-test"), "{shadow}");
        let oc_text = std::fs::read_to_string(&oc).unwrap();
        assert!(oc_text.contains("{env:MOONSHOT_API_KEY}"), "{oc_text}");
        assert!(!oc_text.contains("sk-moon-test"), "{oc_text}");
        let cfg = crate::config::load_config(&path).unwrap();
        let k = cfg.provider("kimi").unwrap();
        assert_eq!(k.kind, "opencode");
        assert_eq!(k.preset, "kimi");
        assert_eq!(k.model, "moonshotai/kimi-k2.6");
    }

    #[test]
    fn add_spark_opencode_preset_and_codex_kind() {
        let (dir, path) = seed();
        let oc = dir.path().join("opencode.json");
        crate::opencode_cfg::set_test_path(Some(oc.clone()));
        add(
            &path,
            "spark",
            AddOpts {
                yes: true,
                ..AddOpts::empty()
            },
        )
        .unwrap();
        crate::opencode_cfg::set_test_path(None);
        let cfg = crate::config::load_config(&path).unwrap();
        let s = cfg.provider("spark").unwrap();
        assert_eq!(s.kind, "opencode");
        assert_eq!(s.preset, "spark");
        assert_eq!(s.model, "spark/deepseek-v4-flash-0731");
        let oc_text = std::fs::read_to_string(&oc).unwrap();
        assert!(oc_text.contains("127.0.0.1:8091"), "{oc_text}");
        assert!(!oc_text.contains("apiKey"), "{oc_text}");

        let overrides = vec![r#"model_provider="openai""#.to_string()];
        add(
            &path,
            "spark-codex",
            AddOpts {
                kind: Some("codex"),
                overrides: &overrides,
                yes: true,
                ..AddOpts::empty()
            },
        )
        .unwrap();
        let cfg = crate::config::load_config(&path).unwrap();
        let c = cfg.provider("spark-codex").unwrap();
        assert_eq!(c.kind, "codex");
        assert!(c.preset.is_empty());
        assert!(c.config_overrides.iter().any(|o| o.contains("openai")));
    }

    #[test]
    fn add_from_config_preset_custom_name() {
        let (_dir, path) = seed();
        let mut text = std::fs::read_to_string(&path).unwrap();
        text.push_str(
            r#"
[[preset]]
name = "custom-oc"
kind = "opencode"
model = "moonshotai/kimi-k2.6"
"#,
        );
        std::fs::write(&path, text).unwrap();
        add(&path, "custom-oc", AddOpts::empty()).unwrap();
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(
            !written.to_ascii_lowercase().contains("api_key"),
            "{written}"
        );
        let cfg = crate::config::load_config(&path).unwrap();
        let k = cfg.provider("custom-oc").unwrap();
        assert_eq!(k.kind, "opencode");
        assert_eq!(k.model, "moonshotai/kimi-k2.6");
    }

    #[test]
    fn list_text_cli_default_model_and_dynamic_width() {
        let (dir, path) = seed();
        let home = dir.path().join("home");
        std::fs::create_dir_all(home.join(".codex")).unwrap();
        std::fs::write(home.join(".codex/config.toml"), "model = \"gpt-5.4\"\n").unwrap();
        crate::detect::set_test_env(Some(crate::detect::DetectEnv {
            path: "/nonexistent".into(),
            home,
            search_system: false,
        }));
        let oc = dir.path().join("opencode.json");
        crate::opencode_cfg::set_test_path(Some(oc));
        add(
            &path,
            "codex",
            AddOpts {
                kind: Some("codex"),
                ..AddOpts::empty()
            },
        )
        .unwrap();
        add(
            &path,
            "spark",
            AddOpts {
                yes: true,
                ..AddOpts::empty()
            },
        )
        .unwrap();
        crate::opencode_cfg::set_test_path(None);
        let cfg = crate::config::load_config(&path).unwrap();
        let text = list_text(&cfg, None);
        crate::detect::set_test_env(None);
        assert!(text.contains("gpt-5.4"), "{text}");
        assert!(text.contains("deepseek-v4-flash-0731"), "{text}");
        let lines: Vec<&str> = text.lines().collect();
        let header = lines[0];
        let model_at = header.find("model").expect(header);
        let usage_at = header.find("usage").expect(header);
        assert!(usage_at > model_at, "{header}");
        let spark = lines
            .iter()
            .find(|l| l.contains("deepseek-v4-flash-0731"))
            .expect(&text);
        let before_usage: String = spark.chars().take(usage_at).collect();
        assert!(
            before_usage.contains("deepseek-v4-flash-0731"),
            "long model overflowed usage column:\n{spark}\n{header}"
        );
        assert!(
            usage_at >= "spark/deepseek-v4-flash-0731".len(),
            "model column too narrow:\n{header}"
        );
    }
}
