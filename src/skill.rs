use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// Embedded atr skill. Installed by `atr init` into each CLI's skill dir.
pub const SKILL_MD: &str = include_str!("../skills/atr/SKILL.md");

/// Relative skill paths under `$HOME` (or `ATR_SKILL_HOME`).
pub fn skill_rel_paths() -> &'static [&'static str] {
    &[
        ".claude/skills/atr/SKILL.md",
        ".cursor/skills/atr/SKILL.md",
        ".codex/skills/atr/SKILL.md",
        ".grok/skills/atr/SKILL.md",
        ".config/opencode/skills/atr/SKILL.md",
    ]
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

pub fn install_into_home(home: &Path) -> Result<String> {
    let mut n = 0;
    for rel in skill_rel_paths() {
        let dest = home.join(rel);
        if let Some(dir) = dest.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        }
        std::fs::write(&dest, SKILL_MD).with_context(|| format!("write {}", dest.display()))?;
        n += 1;
    }
    Ok(format!(
        "installed atr skill ({n} CLI dirs) under {}\n",
        home.display()
    ))
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
        let msg = install_into_home(dir.path()).unwrap();
        assert!(msg.contains("5 CLI"), "{msg}");
        for rel in skill_rel_paths() {
            let p = dir.path().join(rel);
            let text = std::fs::read_to_string(&p).unwrap();
            assert_eq!(text, SKILL_MD);
        }
    }
}
