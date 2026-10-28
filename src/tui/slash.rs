//! Slash routing for the TUI host.
//!
//! atr-local: `/provider`, `/model`, `/effort`, `/context`, `/repo`, `/new`,
//! `/quit`, `/status`, `/help`, `/cli`. Every other `/…` is forwarded to
//! the selected provider CLI. `/model`, `/effort`, `/context`, and `/repo`
//! never become a chat turn.

use crate::app::Shared;
use crate::config::Config;
use crate::tui::tui_provider_now;
use crate::worktree;
use std::path::{Path, PathBuf};

/// Atr-local host commands shown in the `/` autocomplete menu.
/// Order is the display order (Grok-style).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct HostCommand {
    pub name: &'static str,
    pub aliases: &'static [&'static str],
    pub hint: &'static str,
}

pub(crate) const HOST_COMMANDS: &[HostCommand] = &[
    HostCommand {
        name: "/provider",
        aliases: &[],
        hint: "list / pick atr providers",
    },
    HostCommand {
        name: "/model",
        aliases: &["/m"],
        hint: "session model (next --model)",
    },
    HostCommand {
        name: "/effort",
        aliases: &[],
        hint: "session effort (next spawn)",
    },
    HostCommand {
        name: "/context",
        aliases: &[],
        hint: "session context (next spawn)",
    },
    HostCommand {
        name: "/repo",
        aliases: &[],
        hint: "git workspace / answer-only",
    },
    HostCommand {
        name: "/status",
        aliases: &[],
        hint: "session · provider · tokens",
    },
    HostCommand {
        name: "/new",
        aliases: &[],
        hint: "fresh atr session",
    },
    HostCommand {
        name: "/cli",
        aliases: &["/native"],
        hint: "open the provider's own TUI",
    },
    HostCommand {
        name: "/help",
        aliases: &["/?"],
        hint: "atr host commands",
    },
    HostCommand {
        name: "/quit",
        aliases: &["/q", "/exit"],
        hint: "leave atr",
    },
];

pub(crate) fn is_bare_slash(line: &str) -> bool {
    line.trim() == "/"
}

/// `/provider`, `/model`, and `/repo` stay in overlays / the under-input
/// strip. Do not append them (or their confirmations) to the chat transcript.
pub(crate) fn quiet_transcript(line: &str) -> bool {
    matches!(
        parse_slash(line),
        Some(Slash::Provider(_))
            | Some(Slash::Model(_))
            | Some(Slash::Effort(_))
            | Some(Slash::Context(_))
            | Some(Slash::Repo(_))
    )
}

/// Prefix + fuzzy filter for the `/` menu. Empty when the line is not
/// an in-progress atr-local command (has args, or is a provider slash).
pub(crate) fn filter_host_commands(typed: &str) -> Vec<&'static HostCommand> {
    let q = typed.trim();
    if !q.starts_with('/') || q.contains(char::is_whitespace) {
        return Vec::new();
    }
    let mut hits: Vec<&HostCommand> = HOST_COMMANDS.iter().filter(|c| cmd_matches(c, q)).collect();
    hits.sort_by_key(|c| {
        let prefix = c.name.starts_with(q) || c.aliases.iter().any(|a| a.starts_with(q));
        (!prefix, c.name)
    });
    hits
}

fn cmd_matches(c: &HostCommand, q: &str) -> bool {
    if q == "/" {
        return true;
    }
    std::iter::once(c.name)
        .chain(c.aliases.iter().copied())
        .any(|n| n.starts_with(q) || fuzzy_subseq(n, q))
}

pub(crate) fn fuzzy_subseq(name: &str, q: &str) -> bool {
    let mut it = name.chars();
    for qc in q.chars() {
        loop {
            match it.next() {
                Some(c) if c.eq_ignore_ascii_case(&qc) => break,
                Some(_) => continue,
                None => return false,
            }
        }
    }
    true
}

pub(crate) fn filter_names(names: &[String], q: &str) -> Vec<String> {
    if q.is_empty() {
        return names.to_vec();
    }
    let ql = q.to_ascii_lowercase();
    names
        .iter()
        .filter(|n| {
            let nl = n.to_ascii_lowercase();
            nl.contains(&ql) || fuzzy_subseq(&nl, &ql)
        })
        .cloned()
        .collect()
}

/// Repo picker type-to-find. A lone `/` or `~` is a path prefix, not a
/// token query — keep catalog rows. A filter miss still keeps the typed
/// text so Enter can bind an absolute / `~/` / relative git path.
pub(crate) fn filter_repo_items(all: &[String], q: &str) -> Vec<String> {
    let q = q.trim();
    if q.is_empty() {
        return all.to_vec();
    }
    let prefix_only = q == "/" || q == "~" || q == "~/";
    let mut items = if prefix_only {
        all.to_vec()
    } else {
        filter_names(all, q)
    };
    let pathish = prefix_only
        || q.starts_with('/')
        || q.starts_with('~')
        || q.contains('/')
        || q.contains('\\');
    if (pathish || items.is_empty()) && !items.iter().any(|s| s == q) {
        items.push(q.to_string());
    }
    items
}

/// Overlay title suffix: do not paint `//` when the filter already starts
/// with `/` (that looked like a dead search).
pub(crate) fn picker_filter_title(title: &str, filter: &str) -> String {
    if filter.is_empty() {
        return title.to_string();
    }
    if filter.starts_with('/') || filter.starts_with('~') {
        format!("{title}  {filter}")
    } else {
        format!("{title}  /{filter}")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Slash {
    Help,
    Quit,
    Status,
    New,
    Provider(Option<String>),
    Model(Option<String>),
    Effort(Option<String>),
    Context(Option<String>),
    Repo(Option<String>),
    Cli,
    /// Full line (`/compact`, `/plan`, …) for the provider proxy path.
    Proxy(String),
}

pub(crate) fn parse_slash(line: &str) -> Option<Slash> {
    let raw = line.trim();
    if !raw.starts_with('/') {
        return None;
    }
    let t = strip_atr_ns(raw);
    let mut parts = t.split_whitespace();
    let cmd = parts.next()?;
    match cmd {
        "/help" | "/?" => Some(Slash::Help),
        "/quit" | "/exit" | "/q" => Some(Slash::Quit),
        "/status" => Some(Slash::Status),
        "/new" => Some(Slash::New),
        "/provider" => Some(Slash::Provider(parts.next().map(str::to_string))),
        "/repo" => {
            let rest: Vec<&str> = parts.collect();
            Some(Slash::Repo(if rest.is_empty() {
                None
            } else {
                Some(rest.join(" "))
            }))
        }
        "/model" | "/m" => {
            let rest: Vec<&str> = parts.collect();
            Some(Slash::Model(if rest.is_empty() {
                None
            } else {
                Some(rest.join(" "))
            }))
        }
        "/effort" => {
            let rest: Vec<&str> = parts.collect();
            Some(Slash::Effort(if rest.is_empty() {
                None
            } else {
                Some(rest.join(" "))
            }))
        }
        "/context" => {
            let rest: Vec<&str> = parts.collect();
            Some(Slash::Context(if rest.is_empty() {
                None
            } else {
                Some(rest.join(" "))
            }))
        }
        "/cli" | "/native" => Some(Slash::Cli),
        _ => Some(Slash::Proxy(t.to_string())),
    }
}

/// `/atr provider grok` and `/atr /provider grok` are the namespaced forms.
fn strip_atr_ns(raw: &str) -> String {
    let Some(rest) = raw.strip_prefix("/atr") else {
        return raw.to_string();
    };
    let rest = rest.trim();
    if rest.is_empty() {
        return "/help".into();
    }
    if rest.starts_with('/') {
        rest.to_string()
    } else {
        format!("/{rest}")
    }
}

pub(crate) fn slash_help() -> &'static str {
    "\
atr tui — interactive session host

Chat
  Type a message and press Enter. Each turn runs the selected provider
  CLI headless (same engine as `atr ask`): stream-json / -p / exec, not
  that CLI's own full-screen TUI.

atr (this host)
  /provider [name]   list / pick atr providers (enabled · current)
  /model [id]        list / pick the session model (next spawn --model)
  /effort [level]    session effort for this provider (next spawn; kind-specific)
  /context [size]    session context for this provider (next spawn; kind-specific)
  /repo [name|path]  list / pick a git workspace (config [[repo]]; serve optional)
  /repo .            bind the current directory's git root
  /repo none         answer-only (no workspace)
  /status            session, provider, model, tokens, workspace
  /new               fresh atr session (same key)
  /cli               open the provider's own interactive TUI
  /quit              leave atr  (/exit /q)
  Ctrl+C             leave atr
  Esc                dismiss menus / pickers (never quits)

  `/model` `/effort` `/context` are atr-local. They are never sent as chat.
  `/effort` and `/context` only offer values that kind can pass on argv.
  `/repo` is atr-local. It is never a provider chat turn.
  `/cli` is the full native experience (Grok/Claude's own /model picker).
  `/atr …` namespaces host commands if a provider slash collides.
  `atr tui --repo …` / `--dir …` set the launch workspace; omit for answer-only.

Provider CLI slashes
  Other /commands (/compact, /plan, skills, …) go to the current
  provider on the next turn. Not /model.
"
}

#[derive(Debug, Clone)]
pub(crate) struct ProviderChoice {
    pub name: String,
    pub kind: String,
    pub enabled: bool,
    pub current: bool,
}

pub(crate) fn list_providers(shared: &Shared, key: &str) -> Vec<ProviderChoice> {
    let cfg = shared.config.read().unwrap().clone();
    let current = tui_provider_now(shared, key).or_else(|| {
        cfg.providers
            .iter()
            .find(|p| p.enabled)
            .map(|p| p.name.clone())
    });
    cfg.providers
        .iter()
        .map(|p| ProviderChoice {
            name: p.name.clone(),
            kind: p.kind.clone(),
            enabled: p.enabled,
            current: current.as_deref() == Some(p.name.as_str()),
        })
        .collect()
}

pub(crate) fn format_provider_list(list: &[ProviderChoice]) -> String {
    if list.is_empty() {
        return "no providers in config. atr init / atr providers detect".into();
    }
    let mut lines = vec!["Providers  (* current)".to_string()];
    for (i, p) in list.iter().enumerate() {
        let mark = if p.current { "*" } else { " " };
        let state = if p.enabled { "enabled" } else { "disabled" };
        lines.push(format!(
            "  {:>2}  {:<16} {state:<8} {mark}  {}",
            i + 1,
            p.name,
            p.kind
        ));
    }
    lines.join("\n")
}

pub(crate) fn pick_choice(names: &[String], input: &str) -> Option<String> {
    let t = input.trim();
    if t.is_empty() {
        return None;
    }
    if let Ok(n) = t.parse::<usize>() {
        if n >= 1 && n <= names.len() {
            return Some(names[n - 1].clone());
        }
    }
    if let Some(exact) = names.iter().find(|n| n.eq_ignore_ascii_case(t)) {
        return Some(exact.clone());
    }
    let lower = t.to_ascii_lowercase();
    let hits: Vec<&String> = names
        .iter()
        .filter(|n| n.to_ascii_lowercase().starts_with(&lower))
        .collect();
    if hits.len() == 1 {
        return Some(hits[0].clone());
    }
    None
}

pub(crate) fn provider_pick_usage(list: &[ProviderChoice]) -> String {
    format!(
        "{}\n\nusage: /provider <name or number>",
        format_provider_list(list)
    )
}

#[derive(Debug, Clone)]
pub(crate) struct ModelChoice {
    pub name: String,
    pub tag: String,
    pub current: bool,
}

pub(crate) fn format_model_list(provider: &str, kind: &str, list: &[ModelChoice]) -> String {
    if list.is_empty() {
        return model_empty_hint(provider, kind);
    }
    let mut lines = vec![format!("Models for {provider} ({kind})  (* current)")];
    for (i, m) in list.iter().enumerate() {
        let mark = if m.current { "*" } else { " " };
        lines.push(format!("  {:>2}  {:<28} {mark}  {}", i + 1, m.name, m.tag));
    }
    lines.join("\n")
}

pub(crate) fn model_pick_usage(provider: &str, kind: &str, list: &[ModelChoice]) -> String {
    format!(
        "{}\n\nusage: /model <id or number>\n       atr providers model {provider} <id>\n       /cli opens {kind}'s own TUI",
        format_model_list(provider, kind, list)
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RepoSection {
    None,
    Cwd,
    Config,
    Folder,
    Recent,
}

/// One-level scan of a shorthand `repo_path` mega-folder. Keep this small:
/// `~/src` can be wide, and the overlay is a picker not a file browser.
pub(crate) const MEGA_CHILD_CAP: usize = 48;

#[derive(Debug, Clone)]
pub(crate) struct RepoChoice {
    pub token: String,
    pub label: String,
    pub section: RepoSection,
    pub current: bool,
}

/// Config `[[repo]]` / `effective_repos()` that are real git roots, plus
/// cwd git root (`.`), one-level git children of a shorthand mega-folder
/// `repo_path` (not the folder itself), and recent typed paths.
pub(crate) fn list_repos(
    cfg: &Config,
    current: Option<&Path>,
    recent: &[String],
) -> Vec<RepoChoice> {
    let mut out = vec![RepoChoice {
        token: "none".into(),
        label: "answer-only".into(),
        section: RepoSection::None,
        current: current.is_none(),
    }];
    let cwd_root = cwd_git_root();
    let config_roots = config_git_repos(cfg);
    let folder_kids = mega_folder_git_children(cfg);
    if let Some(root) = cwd_root.as_ref() {
        let already = config_roots
            .iter()
            .any(|(p, _)| crate::config::paths_eq(p, root))
            || folder_kids
                .iter()
                .any(|(p, _)| crate::config::paths_eq(p, root));
        if !already {
            out.push(RepoChoice {
                token: ".".into(),
                label: format!("cwd {}", shorten_choice(root)),
                section: RepoSection::Cwd,
                current: current.is_some_and(|c| crate::config::paths_eq(c, root)),
            });
        }
    }
    for (path, name) in &config_roots {
        let token = if !name.is_empty() {
            name.clone()
        } else {
            path.display().to_string()
        };
        let watched = " (watched)";
        out.push(RepoChoice {
            token,
            label: format!("{}{watched}", shorten_choice(path)),
            section: RepoSection::Config,
            current: current.is_some_and(|c| crate::config::paths_eq(c, path)),
        });
    }
    let mut used: std::collections::HashSet<String> = out.iter().map(|c| c.token.clone()).collect();
    for (path, name) in &folder_kids {
        if config_roots
            .iter()
            .any(|(c, _)| crate::config::paths_eq(c, path))
        {
            continue;
        }
        if cwd_root
            .as_ref()
            .is_some_and(|c| crate::config::paths_eq(c, path))
        {
            continue;
        }
        let token = unique_repo_token(&mut used, name, path);
        out.push(RepoChoice {
            token,
            label: shorten_choice(path),
            section: RepoSection::Folder,
            current: current.is_some_and(|c| crate::config::paths_eq(c, path)),
        });
    }
    for raw in recent {
        let p = crate::config::expand_path(raw);
        if !worktree::is_git_repo(&p) {
            continue;
        }
        if config_roots
            .iter()
            .any(|(c, _)| crate::config::paths_eq(c, &p))
        {
            continue;
        }
        if cwd_root
            .as_ref()
            .is_some_and(|c| crate::config::paths_eq(c, &p))
        {
            continue;
        }
        if folder_kids
            .iter()
            .any(|(c, _)| crate::config::paths_eq(c, &p))
        {
            continue;
        }
        if out
            .iter()
            .any(|c| crate::config::paths_eq(Path::new(&c.token), &p))
        {
            continue;
        }
        out.push(RepoChoice {
            token: p.display().to_string(),
            label: format!("recent {}", shorten_choice(&p)),
            section: RepoSection::Recent,
            current: current.is_some_and(|c| crate::config::paths_eq(c, &p)),
        });
    }
    out
}

pub(crate) fn config_git_repos(cfg: &Config) -> Vec<(PathBuf, String)> {
    cfg.effective_repos()
        .into_iter()
        .filter(|r| worktree::is_git_repo(&r.path))
        .map(|r| (r.path, r.name))
        .collect()
}

/// One-level git children of shorthand `repo_path` when that path is a
/// directory but not itself a git root (`~/src` is not one mega-repo).
pub(crate) fn mega_folder_git_children(cfg: &Config) -> Vec<(PathBuf, String)> {
    let root = &cfg.repo_path;
    if root.as_os_str().is_empty() || worktree::is_git_repo(root) || !root.is_dir() {
        return Vec::new();
    }
    one_level_git_children(root, MEGA_CHILD_CAP)
}

pub(crate) fn mega_folder_child(cfg: &Config, spec: &str) -> Option<PathBuf> {
    let spec = spec.trim();
    if spec.is_empty() {
        return None;
    }
    let want = crate::config::expand_path(spec);
    mega_folder_git_children(cfg)
        .into_iter()
        .find_map(|(p, name)| {
            if name == spec
                || crate::config::paths_eq(&p, &want)
                || p.file_name().and_then(|n| n.to_str()) == Some(spec)
            {
                Some(p)
            } else {
                None
            }
        })
}

fn one_level_git_children(dir: &Path, cap: usize) -> Vec<(PathBuf, String)> {
    let mut kids = Vec::new();
    let Ok(rd) = std::fs::read_dir(dir) else {
        return kids;
    };
    let mut ents: Vec<_> = rd.filter_map(|e| e.ok()).collect();
    ents.sort_by_key(|a| a.file_name());
    for ent in ents {
        if kids.len() >= cap {
            break;
        }
        let name = ent.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if name.starts_with('.') {
            continue;
        }
        let path = ent.path();
        if !path.is_dir() || !worktree::is_git_repo(&path) {
            continue;
        }
        kids.push((path, name.to_string()));
    }
    kids
}

fn unique_repo_token(
    used: &mut std::collections::HashSet<String>,
    name: &str,
    path: &Path,
) -> String {
    if !name.is_empty() && !used.contains(name) && name != "none" && name != "." {
        used.insert(name.to_string());
        return name.to_string();
    }
    let t = path.display().to_string();
    used.insert(t.clone());
    t
}

pub(crate) fn cwd_git_root() -> Option<PathBuf> {
    git_root_from(&std::env::current_dir().ok()?)
}

pub(crate) fn git_root_from(start: &Path) -> Option<PathBuf> {
    let mut p = start.canonicalize().unwrap_or_else(|_| start.to_path_buf());
    loop {
        if worktree::is_git_repo(&p) {
            return Some(p);
        }
        if !p.pop() {
            return None;
        }
    }
}

fn shorten_choice(path: &Path) -> String {
    crate::tui::composer_path_line(path)
}

pub(crate) fn format_repo_list(list: &[RepoChoice]) -> String {
    if list.is_empty() {
        return "no git workspaces. add [[repo]] in config (atr serve optional) or /repo <path>"
            .into();
    }
    let mut lines = vec!["Workspace  (* current)  — config [[repo]] (serve optional)".to_string()];
    let mut last = None;
    for (i, r) in list.iter().enumerate() {
        if last != Some(r.section) {
            let header = match r.section {
                RepoSection::None => None,
                RepoSection::Cwd => Some("cwd"),
                RepoSection::Config => Some("config"),
                RepoSection::Folder => Some("folder"),
                RepoSection::Recent => Some("recent"),
            };
            if let Some(h) = header {
                lines.push(format!("  [{h}]"));
            }
            last = Some(r.section);
        }
        let mark = if r.current { "*" } else { " " };
        lines.push(format!(
            "  {:>2}  {:<16} {mark}  {}",
            i + 1,
            r.token,
            r.label
        ));
    }
    lines.join("\n")
}

pub(crate) fn repo_pick_usage(list: &[RepoChoice]) -> String {
    format!(
        "{}\n\nusage: /repo <name|path|.|none>\n       atr tui --repo <name|path>   atr tui --dir <path>",
        format_repo_list(list)
    )
}

pub(crate) fn model_empty_hint(provider: &str, kind: &str) -> String {
    format!(
        "no models parsed — try /cli\n\
         {provider} ({kind}): atr does not invent a catalog.\n\
         Set this session: /model <id>\n\
         Persist in config: atr providers model {provider} <id>\n\
         Or /cli for {kind}'s own TUI picker."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_slashes_never_unknown() {
        assert_eq!(parse_slash("hello"), None);
        assert_eq!(parse_slash("/quit"), Some(Slash::Quit));
        assert_eq!(parse_slash("/q"), Some(Slash::Quit));
        assert_eq!(parse_slash("/help"), Some(Slash::Help));
        assert_eq!(parse_slash("/provider"), Some(Slash::Provider(None)));
        assert_eq!(
            parse_slash("/provider grok"),
            Some(Slash::Provider(Some("grok".into())))
        );
        assert_eq!(parse_slash("/repo"), Some(Slash::Repo(None)));
        assert_eq!(
            parse_slash("/repo none"),
            Some(Slash::Repo(Some("none".into())))
        );
        assert_eq!(parse_slash("/repo ."), Some(Slash::Repo(Some(".".into()))));
        assert_eq!(
            parse_slash("/repo ~/src/demo"),
            Some(Slash::Repo(Some("~/src/demo".into())))
        );
        assert!(quiet_transcript("/provider"));
        assert!(quiet_transcript("/provider codex"));
        assert!(quiet_transcript("/model"));
        assert!(quiet_transcript("/model gpt-6.1-sol"));
        assert!(quiet_transcript("/m"));
        assert!(quiet_transcript("/effort"));
        assert!(quiet_transcript("/effort high"));
        assert!(quiet_transcript("/context"));
        assert!(quiet_transcript("/context 200k"));
        assert!(quiet_transcript("/atr effort high"));
        assert!(quiet_transcript("/repo"));
        assert!(quiet_transcript("/repo none"));
        assert!(quiet_transcript("/repo ."));
        assert!(quiet_transcript("/atr repo"));
        assert!(quiet_transcript("/atr /repo none"));
        assert!(quiet_transcript("/atr provider"));
        assert!(quiet_transcript("/atr /provider grok"));
        assert!(quiet_transcript("/atr model"));
        assert!(quiet_transcript("/atr /model gpt-6.1-sol"));
        assert!(!quiet_transcript("/help"));
        assert!(!quiet_transcript("/status"));
        assert!(!quiet_transcript("/cli"));
        assert!(!quiet_transcript("hello"));
        assert_eq!(parse_slash("/model"), Some(Slash::Model(None)));
        assert_eq!(
            parse_slash("/model grok-code"),
            Some(Slash::Model(Some("grok-code".into())))
        );
        assert_eq!(parse_slash("/m"), Some(Slash::Model(None)));
        assert_eq!(parse_slash("/effort"), Some(Slash::Effort(None)));
        assert_eq!(
            parse_slash("/effort high"),
            Some(Slash::Effort(Some("high".into())))
        );
        assert_eq!(parse_slash("/context"), Some(Slash::Context(None)));
        assert_eq!(
            parse_slash("/context 1m"),
            Some(Slash::Context(Some("1m".into())))
        );
        assert_eq!(
            parse_slash("/compact"),
            Some(Slash::Proxy("/compact".into()))
        );
        assert_eq!(parse_slash("/nope"), Some(Slash::Proxy("/nope".into())));
        assert_eq!(
            parse_slash("/atr provider grok"),
            Some(Slash::Provider(Some("grok".into())))
        );
    }

    #[test]
    fn model_slash_does_not_eat_memory() {
        assert_eq!(parse_slash("/memory"), Some(Slash::Proxy("/memory".into())));
        assert_eq!(parse_slash("/mcps"), Some(Slash::Proxy("/mcps".into())));
        assert_eq!(
            parse_slash("/compact"),
            Some(Slash::Proxy("/compact".into()))
        );
    }

    #[test]
    fn help_has_host_and_proxy_sections() {
        let h = slash_help();
        assert!(h.contains("Chat"), "{h}");
        assert!(h.contains("Provider CLI"), "{h}");
        assert!(h.contains("/provider"), "{h}");
        assert!(h.contains("/model"), "{h}");
        assert!(h.contains("/effort"), "{h}");
        assert!(h.contains("/context"), "{h}");
        assert!(h.contains("/repo"), "{h}");
        assert!(h.contains("/repo none"), "{h}");
        assert!(h.contains("/repo ."), "{h}");
        assert!(h.contains("atr tui --repo"), "{h}");
        assert!(h.contains("never sent as chat"), "{h}");
        assert!(h.contains("/cli"), "{h}");
        assert!(h.contains("Ctrl+C"), "{h}");
        assert!(!h.contains("unknown command"), "{h}");
        assert!(!h.contains("Claude's `/model` works in print mode"), "{h}");
    }

    #[test]
    fn slash_menu_filters_atr_local_only() {
        let all = filter_host_commands("/");
        assert_eq!(all.len(), HOST_COMMANDS.len(), "{all:?}");
        assert!(all.iter().any(|c| c.name == "/model"));
        assert!(all.iter().any(|c| c.name == "/effort"));
        assert!(all.iter().any(|c| c.name == "/context"));
        assert!(all.iter().any(|c| c.name == "/provider"));
        assert!(all.iter().any(|c| c.name == "/repo"));
        assert!(all.iter().any(|c| c.name == "/quit"));

        let m = filter_host_commands("/m");
        assert!(m.iter().any(|c| c.name == "/model"), "{m:?}");
        assert!(m.iter().all(|c| c.name == "/model"), "{m:?}");

        let e = filter_host_commands("/eff");
        assert!(e.iter().any(|c| c.name == "/effort"), "{e:?}");
        assert!(e.iter().all(|c| c.name == "/effort"), "{e:?}");

        let q = filter_host_commands("/q");
        assert!(q.iter().any(|c| c.name == "/quit"), "{q:?}");

        let fuzzy = filter_host_commands("/prv");
        assert!(fuzzy.iter().any(|c| c.name == "/provider"), "{fuzzy:?}");

        assert!(filter_host_commands("/compact").is_empty());
        assert!(filter_host_commands("/model grok").is_empty());
        assert!(filter_host_commands("hello").is_empty());
        assert!(is_bare_slash(" / "));
        assert!(!is_bare_slash("/model"));
    }

    #[test]
    fn filter_names_type_to_find() {
        let names = vec![
            "grok-4.7".into(),
            "grok-4.7-build-fast".into(),
            "sonnet".into(),
        ];
        assert_eq!(filter_names(&names, "").len(), 3);
        let grok = filter_names(&names, "grok-4.7");
        assert!(grok.iter().any(|s| s == "grok-4.7"));
        assert!(grok.iter().any(|s| s == "grok-4.7-build-fast"));
        assert!(!grok.iter().any(|s| s == "sonnet"));
        assert_eq!(filter_names(&names, "son").as_slice(), ["sonnet"]);
    }

    #[test]
    fn pick_choice_number_and_name() {
        let names = vec!["claude".into(), "grok".into()];
        assert_eq!(pick_choice(&names, "2").as_deref(), Some("grok"));
        assert_eq!(pick_choice(&names, "GROK").as_deref(), Some("grok"));
        assert_eq!(pick_choice(&names, "cl").as_deref(), Some("claude"));
        assert_eq!(pick_choice(&names, "9"), None);
        assert_eq!(pick_choice(&names, ""), None);
    }

    #[test]
    fn repo_filter_slash_keeps_rows_and_typed_path() {
        let all = vec!["none".into(), ".".into(), "atr-pr65".into()];
        let slash = filter_repo_items(&all, "/");
        assert!(slash.iter().any(|s| s == "none"), "{slash:?}");
        assert!(slash.iter().any(|s| s == "atr-pr65"), "{slash:?}");
        assert!(
            slash.iter().any(|s| s == "/"),
            "typed / stays pickable: {slash:?}"
        );
        assert!(!slash.is_empty());

        let tilde = filter_repo_items(&all, "~");
        assert!(tilde.iter().any(|s| s == "none"), "{tilde:?}");
        assert!(tilde.iter().any(|s| s == "~"), "{tilde:?}");

        let miss = filter_repo_items(&all, "zzz");
        assert!(
            miss.iter().any(|s| s == "zzz"),
            "filter miss keeps typed text: {miss:?}"
        );
        assert!(!miss.is_empty());

        let hit = filter_repo_items(&all, "atr");
        assert_eq!(hit.as_slice(), ["atr-pr65"]);

        let path = filter_repo_items(&all, "~/src/atr-pr65");
        assert!(path.iter().any(|s| s == "~/src/atr-pr65"), "{path:?}");

        assert_eq!(
            picker_filter_title("Repo  (Enter / type to find · Esc)", "/"),
            "Repo  (Enter / type to find · Esc)  /"
        );
        assert!(
            !picker_filter_title("Repo", "/").contains("//"),
            "lone / must not paint //"
        );
        assert_eq!(picker_filter_title("Repo", "atr"), "Repo  /atr");
    }

    #[test]
    fn list_repos_includes_mega_folder_git_children() {
        let tmp = tempfile::TempDir::new().unwrap();
        let src = tmp.path().join("src");
        std::fs::create_dir_all(src.join("notes")).unwrap();
        crate::worktree::init_sample_repo(&src.join("atr-pr65")).unwrap();
        crate::worktree::init_sample_repo(&src.join("other")).unwrap();
        let cfg = crate::config::parse_config_toml(&format!(
            r#"
repo_path = "{src}"
task_key_prefix = "demo"
[planner]
mode = "off"
[github]
enabled = false
[[provider]]
name = "claude"
kind = "claude"
enabled = true
"#,
            src = src.display()
        ))
        .unwrap();
        let list = list_repos(&cfg, None, &[]);
        assert!(list.iter().any(|c| c.token == "none"), "{list:?}");
        assert!(
            list.iter()
                .any(|c| c.token == "atr-pr65" && c.section == RepoSection::Folder),
            "git child name must be pickable: {list:?}"
        );
        assert!(
            list.iter().any(|c| c.token == "other"),
            "second git child: {list:?}"
        );
        assert!(
            !list
                .iter()
                .any(|c| c.token == "notes" || c.label.contains("notes")),
            "non-git dir must be skipped: {list:?}"
        );
        assert!(
            !list
                .iter()
                .any(|c| crate::config::paths_eq(Path::new(&c.token), &src)
                    || c.label == shorten_choice(&src)),
            "mega-folder itself is not one repo: {list:?}"
        );
        assert!(
            mega_folder_child(&cfg, "atr-pr65").is_some_and(|p| p.ends_with("atr-pr65")),
            "child name resolves"
        );
        assert!(mega_folder_child(&cfg, "notes").is_none());
    }
}
