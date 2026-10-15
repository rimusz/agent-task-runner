//! README commands must exist on the CLI and match `--help`.
use std::collections::BTreeSet;
use std::process::Command;

fn atr() -> Command {
    Command::new(env!("CARGO_BIN_EXE_atr"))
}

fn help(args: &[&str]) -> String {
    let mut cmd = atr();
    cmd.args(args).arg("--help");
    let out = cmd.output().expect("atr --help");
    assert!(
        out.status.success(),
        "atr {} --help failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn section_commands(help: &str, heading: &str) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    let mut in_section = false;
    for line in help.lines() {
        let t = line.trim_end();
        if t.starts_with("Commands:") || t.starts_with("Arguments:") || t.starts_with("Options:") {
            in_section = t.starts_with(heading);
            continue;
        }
        if !in_section || t.is_empty() {
            continue;
        }
        if heading == "Commands:" {
            if let Some(name) = t.split_whitespace().next() {
                if name.chars().all(|c| c.is_ascii_lowercase() || c == '-') {
                    names.insert(name.to_string());
                }
            }
        }
        if heading == "Options:" {
            for tok in t.split_whitespace() {
                let tok = tok.trim_end_matches(',');
                if let Some(rest) = tok.strip_prefix("--") {
                    if !rest.is_empty()
                        && rest.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
                    {
                        names.insert(format!("--{rest}"));
                    }
                } else if tok.starts_with('-') && tok.len() == 2 {
                    names.insert(tok.to_string());
                }
            }
        }
    }
    names
}

fn is_placeholder(tok: &str) -> bool {
    tok.starts_with('<') && tok.ends_with('>')
        || tok.starts_with('[') && tok.ends_with(']')
        || tok == "…"
        || tok == "..."
}

fn tokenize(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote = None::<char>;
    for c in line.chars() {
        if let Some(q) = quote {
            cur.push(c);
            if c == q {
                quote = None;
                out.push(std::mem::take(&mut cur));
            }
            continue;
        }
        if c == '\'' || c == '"' {
            quote = Some(c);
            cur.push(c);
            continue;
        }
        if c.is_whitespace() {
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
            continue;
        }
        cur.push(c);
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

fn invocations_from_line(line: &str) -> Vec<Vec<String>> {
    let mut found = Vec::new();
    let mut rest = line;
    if let Some((code, comment)) = line.split_once('#') {
        rest = code;
        if let Some(idx) = comment.find("atr ") {
            found.extend(invocations_from_line(&comment[idx..]));
        }
    }
    let rest = rest.trim();
    if rest.is_empty() || rest.starts_with("curl") || rest.contains("releases/download") {
        return found;
    }
    let Some(idx) = rest.find("atr") else {
        return found;
    };
    if idx > 0 {
        let before = rest.as_bytes()[idx - 1];
        if before.is_ascii_alphanumeric() || before == b'-' || before == b'_' || before == b'/' {
            return found;
        }
    }
    let slice = &rest[idx..];
    let stop = slice
        .find('|')
        .or_else(|| slice.find("&&"))
        .unwrap_or(slice.len());
    let tokens = tokenize(slice[..stop].trim());
    if tokens.first().is_some_and(|t| t == "atr") {
        found.push(tokens);
    }
    found
}

fn extract_atr_invocations(text: &str) -> Vec<Vec<String>> {
    let mut out = Vec::new();
    let mut in_fence = false;
    for line in text.lines() {
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            out.extend(invocations_from_line(line));
            continue;
        }
        let mut buf = line;
        while let Some(start) = buf.find('`') {
            let rest = &buf[start + 1..];
            if let Some(end) = rest.find('`') {
                let inner = &rest[..end];
                if inner.starts_with("atr") {
                    out.extend(invocations_from_line(inner));
                }
                buf = &rest[end + 1..];
            } else {
                break;
            }
        }
    }
    out
}

fn extract_slash_commands(text: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let mut buf = text;
    while let Some(start) = buf.find('`') {
        let rest = &buf[start + 1..];
        if let Some(end) = rest.find('`') {
            let inner = rest[..end].trim();
            if let Some(cmd) = inner.strip_prefix('/') {
                let name = cmd
                    .split_whitespace()
                    .next()
                    .unwrap_or("")
                    .trim_end_matches(|c: char| !c.is_ascii_alphanumeric());
                if !name.is_empty() && name.chars().all(|c| c.is_ascii_lowercase()) {
                    out.insert(format!("/{name}"));
                }
            }
            buf = &rest[end + 1..];
        } else {
            break;
        }
    }
    out
}

const TELEGRAM: &[&str] = &[
    "/help",
    "/start",
    "/status",
    "/tasks",
    "/task",
    "/add",
    "/cancel",
    "/retry",
    "/usage",
    "/shadow",
    "/pause",
    "/resume",
    "/providers",
    "/enable",
    "/disable",
    "/config",
    "/repos",
];

fn validate_invocation(tokens: &[String], top: &BTreeSet<String>, global_flags: &BTreeSet<String>) {
    let shown = tokens.join(" ");
    let mut i = 0;
    while i < tokens.len() && tokens[i].contains('=') && !tokens[i].starts_with('-') {
        i += 1;
    }
    assert_eq!(tokens.get(i).map(String::as_str), Some("atr"), "{shown}");
    i += 1;
    let mut help_args: Vec<&str> = Vec::new();
    let mut flags = global_flags.clone();
    let mut commands = top.clone();
    while i < tokens.len() {
        let tok = &tokens[i];
        if tok.starts_with('-') {
            let flag = tok.split('=').next().unwrap();
            assert!(
                flags.contains(flag),
                "README `{shown}`: flag {flag} is not in `atr {} --help` ({flags:?})",
                help_args.join(" ")
            );
            i += 1;
            continue;
        }
        if is_placeholder(tok) {
            i += 1;
            continue;
        }
        if commands.contains(tok.as_str()) {
            help_args.push(tok.as_str());
            let text = help(&help_args);
            commands = section_commands(&text, "Commands:");
            flags = section_commands(&text, "Options:");
            flags.extend(global_flags.iter().cloned());
            i += 1;
            continue;
        }
        // positional (task key, provider name, version, …)
        i += 1;
    }
}

#[test]
fn readme_headings_and_commands_match_help() {
    let readme = std::fs::read_to_string(format!("{}/README.md", env!("CARGO_MANIFEST_DIR")))
        .expect("README.md");
    // The short README links out; the long-form usage lives in docs/.
    let doc = |name: &str| {
        std::fs::read_to_string(format!("{}/docs/{name}", env!("CARGO_MANIFEST_DIR")))
            .unwrap_or_else(|_| panic!("docs/{name}"))
    };
    for heading in [
        "## Install",
        "## Setup",
        "## Your first task",
        "## What you can use it for",
        "## Providers",
        "## More docs",
        "## Trademarks and license",
    ] {
        assert!(
            readme.contains(heading),
            "README must contain heading {heading}"
        );
    }
    for link in ["docs/INSTALL.md", "docs/USE-CASES.md", "docs/USAGE.md"] {
        assert!(readme.contains(link), "README must link {link}");
    }
    let usage = doc("USAGE.md");
    for heading in [
        "### Giving it work",
        "### Hand work to atr",
        "### Watching repos",
        "### Watching and steering",
        "### Using atr from other agents",
        "### Maintenance",
        "## Writing a good task issue",
        "## Telegram",
        "## Dashboard",
        "## Troubleshooting",
    ] {
        assert!(
            usage.contains(heading),
            "docs/USAGE.md must contain heading {heading}"
        );
    }
    assert!(
        doc("INSTALL.md").contains("## Full agent deploy prompt"),
        "docs/INSTALL.md must keep the full deploy prompt"
    );
    let readme = format!(
        "{readme}\n{usage}\n{}\n{}",
        doc("INSTALL.md"),
        doc("USE-CASES.md")
    );
    assert!(
        readme.contains("docs/DESIGN.md"),
        "internals must point at DESIGN.md"
    );
    assert!(
        readme.contains("docs/HOW-IT-WORKS.md"),
        "README must link HOW-IT-WORKS.md"
    );
    assert!(
        readme.contains("docs/CONFIG.md"),
        "README must link CONFIG.md"
    );
    assert!(
        readme.contains("docs/GITHUB.md"),
        "README must link GITHUB.md"
    );
    assert!(
        readme.contains("for:atr"),
        "GitHub watch label must be for:atr"
    );
    for needle in [
        "Using atr from other agents",
        "Claude Code",
        "Codex",
        "Grok Build",
        "atr mcp",
        "GitHub-label method",
    ] {
        assert!(
            readme.contains(needle),
            "README MCP section must mention {needle}"
        );
    }
    let how = std::fs::read_to_string(format!(
        "{}/docs/HOW-IT-WORKS.md",
        env!("CARGO_MANIFEST_DIR")
    ))
    .expect("docs/HOW-IT-WORKS.md");
    for heading in [
        "## Providers",
        "## Token caching",
        "## Driver, planner, and review",
        "## MCP tool reference",
        "## CI usage",
    ] {
        assert!(
            how.contains(heading),
            "HOW-IT-WORKS must contain heading {heading}"
        );
    }
    for needle in ["ATR_MCP_TOKEN", "POST /mcp", "Authorization", "keepAlive"] {
        assert!(how.contains(needle), "HOW-IT-WORKS must mention {needle}");
    }
    let config = std::fs::read_to_string(format!("{}/docs/CONFIG.md", env!("CARGO_MANIFEST_DIR")))
        .expect("docs/CONFIG.md");
    assert!(config.contains("config.toml"), "{config}");
    assert!(config.contains("[cache]"), "{config}");
    assert!(
        config.contains("--legacy-label"),
        "CONFIG.md must document --legacy-label instead of a hardcoded old LaunchAgent name"
    );
    assert!(
        !config.to_ascii_lowercase().contains("com.rimas"),
        "CONFIG.md must not name a personal legacy LaunchAgent label"
    );
    let github = std::fs::read_to_string(format!("{}/docs/GITHUB.md", env!("CARGO_MANIFEST_DIR")))
        .expect("docs/GITHUB.md");
    let release = std::fs::read_to_string(format!(
        "{}/.github/workflows/release.yml",
        env!("CARGO_MANIFEST_DIR")
    ))
    .expect("release.yml");
    let ci = std::fs::read_to_string(format!(
        "{}/.github/workflows/ci.yml",
        env!("CARGO_MANIFEST_DIR")
    ))
    .expect("ci.yml");
    assert!(
        ci.contains("gitleaks detect") && ci.contains(".gitleaks.toml"),
        "CI must run gitleaks against .gitleaks.toml"
    );
    assert!(
        !release.contains("x86_64-apple-darwin"),
        "macOS releases stay Apple Silicon only"
    );
    assert!(
        release.contains("aarch64-apple-darwin"),
        "macOS Apple Silicon target must stay in release.yml"
    );
    for needle in [
        "gh auth login",
        "GH_TOKEN",
        "Contents",
        "Issues",
        "Pull requests",
        "Metadata",
        "ssh-keygen",
        "ssh -T git@github.com",
        "ATR_SHADOW_GH_TOKEN",
        "never printed",
    ] {
        assert!(
            github.contains(needle),
            "docs/GITHUB.md must mention {needle}"
        );
    }
    assert!(
        how.contains("does **not** run in GitHub Actions"),
        "CI detail belongs in HOW-IT-WORKS"
    );

    let install_help = help(&["service", "install"]);
    assert!(
        install_help.contains("--legacy-label"),
        "atr service install must accept any old LaunchAgent label"
    );

    let top_help = help(&[]);
    let top = section_commands(&top_help, "Commands:");
    let global = section_commands(&top_help, "Options:");
    assert!(top.contains("init"), "{top:?}");
    assert!(top.contains("submit"), "{top:?}");
    assert!(top.contains("add"), "{top:?}");
    assert!(top.contains("providers"), "{top:?}");

    let inv = extract_atr_invocations(&readme);
    assert!(inv.len() >= 12, "expected many atr examples, got {inv:?}");
    for tokens in &inv {
        validate_invocation(tokens, &top, &global);
    }

    const IGNORE_SLASH: &[&str] = &["/command", "/newbot", "/connect", "/review", "/mcp"];
    for cmd in extract_slash_commands(&readme) {
        if IGNORE_SLASH.contains(&cmd.as_str()) {
            continue;
        }
        assert!(
            TELEGRAM.contains(&cmd.as_str()),
            "README documents unknown Telegram command {cmd}"
        );
    }
}

#[test]
fn extract_covers_subcommand_and_flags() {
    let sample = r#"
```bash
atr init --yes
atr providers move claude --to 1
atr usage --period month --by provider
```
Use `atr cancel my-task --force` or `atr --config ~/.config/atr/config.toml status`.
"#;
    let inv = extract_atr_invocations(sample);
    let joined: Vec<String> = inv.iter().map(|t| t.join(" ")).collect();
    assert!(joined.iter().any(|s| s == "atr init --yes"), "{joined:?}");
    assert!(
        joined
            .iter()
            .any(|s| s.contains("providers move") && s.contains("--to")),
        "{joined:?}"
    );
    assert!(
        joined
            .iter()
            .any(|s| s.contains("cancel") && s.contains("--force")),
        "{joined:?}"
    );
}
