use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// Embedded atr skill. Installed by `atr init` into each CLI's skill dir.
pub const SKILL_MD: &str = include_str!("../skills/atr/SKILL.md");

/// Skill targets: (provider id, CLI binaries, skill path under `$HOME`).
/// A skill is only written when one of the provider's CLIs is installed.
pub const SKILL_TARGETS: &[(&str, &[&str], &str)] = &[
    ("claude", &["claude"], ".claude/skills/atr/SKILL.md"),
    ("cursor", &["cursor-agent"], ".cursor/skills/atr/SKILL.md"),
    ("codex", &["codex"], ".codex/skills/atr/SKILL.md"),
    ("grok", &["grok"], ".grok/skills/atr/SKILL.md"),
    (
        "opencode",
        &["opencode"],
        ".config/opencode/skills/atr/SKILL.md",
    ),
];

/// Relative skill paths under `$HOME` (or `ATR_SKILL_HOME`).
pub fn skill_rel_paths() -> Vec<&'static str> {
    SKILL_TARGETS.iter().map(|t| t.2).collect()
}

pub fn skill_home() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("ATR_SKILL_HOME") {
        if !p.trim().is_empty() {
            return Some(PathBuf::from(p));
        }
    }
    std::env::var("HOME")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(PathBuf::from)
}

/// Write the skill for each provider whose CLI is found by `installed`.
/// Providers without a CLI are skipped (no dirs created).
pub fn install_into_home_with(home: &Path, installed: impl Fn(&str) -> bool) -> Result<String> {
    let mut done = Vec::new();
    let mut skipped = Vec::new();
    for (id, bins, rel) in SKILL_TARGETS {
        if !bins.iter().any(|b| installed(b)) {
            skipped.push(*id);
            continue;
        }
        let dest = home.join(rel);
        if let Some(dir) = dest.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        }
        std::fs::write(&dest, SKILL_MD).with_context(|| format!("write {}", dest.display()))?;
        done.push(*id);
    }
    let mut out = format!(
        "installed atr skill ({} CLI dirs) under {}",
        done.len(),
        home.display()
    );
    if !done.is_empty() {
        out.push_str(&format!(": {}", done.join(", ")));
    }
    out.push('\n');
    if !skipped.is_empty() {
        out.push_str(&format!(
            "skill skipped (CLI not installed): {}\n",
            skipped.join(", ")
        ));
    }
    Ok(out)
}

/// Install for the CLIs detected on this machine (PATH + common dirs).
pub fn install_into_home(home: &Path) -> Result<String> {
    let env = crate::detect::current_env();
    install_into_home_with(home, |b| crate::detect::find_binary(b, &env).is_some())
}

pub fn install_default() -> Result<Option<String>> {
    match skill_home() {
        Some(h) => Ok(Some(install_into_home(&h)?)),
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn operator_plugin_skill_matches_repo_skill() {
        let plugin = std::fs::read_to_string(format!(
            "{}/plugins/atr-operator/skills/atr/SKILL.md",
            env!("CARGO_MANIFEST_DIR")
        ))
        .expect("plugins/atr-operator/skills/atr/SKILL.md");
        assert_eq!(
            plugin, SKILL_MD,
            "plugins/atr-operator/skills/atr/SKILL.md must stay a copy of skills/atr/SKILL.md"
        );
    }

    #[test]
    fn operator_plugin_manifest_is_stdio_atr_mcp() {
        let root = env!("CARGO_MANIFEST_DIR");
        let plugin: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(format!("{root}/plugins/atr-operator/plugin.json"))
                .expect("plugin.json"),
        )
        .expect("plugin.json json");
        assert_eq!(
            plugin["$schema"],
            "https://agent-plugins.org/schemas/1.0.0/plugin.schema.json"
        );
        assert_eq!(plugin["name"], "atr-operator");
        assert_eq!(plugin["license"], "MIT OR Apache-2.0");
        assert_eq!(
            plugin["homepage"],
            "https://github.com/rimusz/agent-task-runner"
        );
        assert_eq!(
            plugin["repository"],
            "https://github.com/rimusz/agent-task-runner"
        );

        let mcp: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(format!("{root}/plugins/atr-operator/mcp.json"))
                .expect("mcp.json"),
        )
        .expect("mcp.json json");
        assert_eq!(
            mcp["$schema"],
            "https://agent-plugins.org/schemas/1.0.0/mcp.schema.json"
        );
        let atr = &mcp["mcpServers"]["atr"];
        assert_eq!(atr["type"], "stdio");
        assert_eq!(atr["command"], "atr");
        assert_eq!(atr["args"], serde_json::json!(["mcp"]));
        assert!(atr.get("env").is_none(), "mcp.json must not carry secrets");
        assert!(
            atr.get("headers").is_none(),
            "stdio mcp.json must not carry HTTP headers"
        );
    }

    #[test]
    fn skill_mentions_readme_topics() {
        for needle in [
            "atr init",
            "providers add",
            "opencode",
            "kimi",
            "ollama",
            "spark",
            "Telegram",
            "atr doctor",
            "atr upgrade",
            "atr mcp",
            "Troubleshooting",
            "ATR:",
        ] {
            assert!(
                SKILL_MD.contains(needle),
                "SKILL.md must mention {needle:?}"
            );
        }
    }

    #[test]
    fn install_writes_cli_skill_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let msg = install_into_home_with(dir.path(), |_| true).unwrap();
        assert!(msg.contains("5 CLI"), "{msg}");
        for rel in skill_rel_paths() {
            let p = dir.path().join(rel);
            let text = std::fs::read_to_string(&p).unwrap();
            assert_eq!(text, SKILL_MD);
        }
    }

    #[test]
    fn install_skips_providers_not_installed() {
        let dir = tempfile::tempdir().unwrap();
        let msg = install_into_home_with(dir.path(), |b| b == "claude" || b == "grok").unwrap();
        assert!(msg.contains("2 CLI"), "{msg}");
        assert!(msg.contains("skipped"), "{msg}");
        assert!(dir.path().join(".claude/skills/atr/SKILL.md").is_file());
        assert!(dir.path().join(".grok/skills/atr/SKILL.md").is_file());
        for d in [".cursor", ".codex", ".config/opencode"] {
            assert!(!dir.path().join(d).exists(), "{d} must not be created");
        }
    }

    #[cfg(unix)]
    #[test]
    fn install_detects_fake_clis() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let bin = tmp.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        for name in ["codex", "cursor-agent"] {
            let p = bin.join(name);
            std::fs::write(&p, "#!/bin/sh\nexit 0\n").unwrap();
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let env = crate::detect::DetectEnv {
            path: bin.as_os_str().to_owned(),
            home: home.clone(),
            search_system: false,
        };
        let msg = install_into_home_with(&home, |b| crate::detect::find_binary(b, &env).is_some())
            .unwrap();
        assert!(
            msg.contains("codex, cursor") || msg.contains("cursor, codex"),
            "{msg}"
        );
        assert!(home.join(".codex/skills/atr/SKILL.md").is_file());
        assert!(home.join(".cursor/skills/atr/SKILL.md").is_file());
        assert!(!home.join(".claude").exists());
        assert!(!home.join(".grok").exists());
    }
}
