//! Embedded atr Grok plugin. Written under the atr data dir — never ~/.grok.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

const PLUGIN_JSON: &str = r#"{
  "name": "atr",
  "version": "1.0.0",
  "description": "atr hooks and skill for live progress"
}
"#;

const POST_SH: &str = r#"#!/bin/sh
url="${ATR_HOOK_URL:-http://127.0.0.1:7420/hooks}"
exec curl -sS --max-time 5 -X POST -H "Content-Type: application/json" --data-binary @- "$url"
"#;

const STOP_SH: &str = r#"#!/bin/sh
if [ -n "$ATR_PLUGIN_TEST" ]; then
  sh -c "$ATR_PLUGIN_TEST" || exit 2
fi
url="${ATR_HOOK_URL:-http://127.0.0.1:7420/hooks}"
exec curl -sS --max-time 5 -X POST -H "Content-Type: application/json" --data-binary @- "$url"
"#;

/// Directory that holds `plugin.json` + `hooks/` (passed to `--plugin-dir`).
pub fn plugin_dir(state_db: &Path) -> PathBuf {
    state_db
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("plugin")
        .join("atr")
}

pub fn ensure_plugin(state_db: &Path, test_command: &str) -> Result<PathBuf> {
    let dir = plugin_dir(state_db);
    std::fs::create_dir_all(dir.join("hooks")).context("create atr plugin dir")?;
    std::fs::write(dir.join("plugin.json"), PLUGIN_JSON)?;
    let post = dir.join("hooks").join("post.sh");
    std::fs::write(&post, POST_SH)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&post, std::fs::Permissions::from_mode(0o755))?;
    }
    let stop = dir.join("hooks").join("stop.sh");
    std::fs::write(&stop, STOP_SH)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&stop, std::fs::Permissions::from_mode(0o755))?;
    }
    let _ = test_command;
    let hooks = hooks_json(&dir, !test_command.trim().is_empty());
    std::fs::write(dir.join("hooks").join("hooks.json"), hooks)?;
    let skill_dir = dir.join("skills").join("atr");
    std::fs::create_dir_all(&skill_dir)?;
    std::fs::write(skill_dir.join("SKILL.md"), crate::skill::SKILL_MD)?;
    Ok(dir)
}

fn hooks_json(dir: &Path, stop_test: bool) -> String {
    let post = dir.join("hooks").join("post.sh").display().to_string();
    let stop = dir.join("hooks").join("stop.sh").display().to_string();
    let stop_cmd = if stop_test { &stop } else { &post };
    format!(
        r#"{{
  "hooks": {{
    "PostToolUse": [{{"hooks": [{{"type": "command", "command": {post}}}]}}],
    "SubagentStart": [{{"hooks": [{{"type": "command", "command": {post}}}]}}],
    "SubagentStop": [{{"hooks": [{{"type": "command", "command": {post}}}]}}],
    "Stop": [{{"hooks": [{{"type": "command", "command": {stop}}}]}}],
    "StopFailure": [{{"hooks": [{{"type": "command", "command": {post}}}]}}]
  }}
}}
"#,
        post = serde_json::to_string(&post).unwrap_or_else(|_| format!("\"{post}\"")),
        stop = serde_json::to_string(stop_cmd).unwrap_or_else(|_| format!("\"{stop_cmd}\"")),
    )
}

pub fn never_touches_grok_home(home: &Path, plugin: &Path) -> bool {
    !plugin.starts_with(home.join(".grok"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_plugin_away_from_grok_home() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        std::fs::create_dir_all(home.join(".grok")).unwrap();
        let db = tmp.path().join("data").join("state.db");
        std::fs::create_dir_all(db.parent().unwrap()).unwrap();
        let dir = ensure_plugin(&db, "true").unwrap();
        assert!(dir.join("plugin.json").is_file());
        assert!(dir.join("hooks/hooks.json").is_file());
        assert!(dir.join("hooks/post.sh").is_file());
        assert!(dir.join("skills/atr/SKILL.md").is_file());
        assert!(never_touches_grok_home(&home, &dir));
        let text = std::fs::read_to_string(dir.join("plugin.json")).unwrap();
        assert!(text.contains("\"name\": \"atr\""));
        let hooks = std::fs::read_to_string(dir.join("hooks/hooks.json")).unwrap();
        assert!(hooks.contains("PostToolUse"));
        assert!(hooks.contains("SubagentStart"));
        assert!(hooks.contains("StopFailure"));
    }
}
