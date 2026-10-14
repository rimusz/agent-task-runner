use std::path::Path;

pub const REL_PATH: &str = ".atr/checklist.md";

pub const INSTRUCTION: &str = "\n\
## Checklist\n\
Keep a markdown checklist at `.atr/checklist.md` in this worktree (already gitignored). \
Use `- [ ]` / `- [x]` items and update them as you finish work. \
The current file is included on follow-up turns and provider switches.\n";

pub fn load(worktree: &Path, max_bytes: usize) -> Option<String> {
    let text = std::fs::read_to_string(worktree.join(".atr").join("checklist.md")).ok()?;
    let t = text.trim();
    if t.is_empty() {
        return None;
    }
    Some(truncate(t, max_bytes))
}

pub fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let keep = max.saturating_sub(16);
    let mut end = keep;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…[truncated]", &s[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_empty_or_missing_is_none() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load(dir.path(), 4096).is_none());
        std::fs::create_dir_all(dir.path().join(".atr")).unwrap();
        std::fs::write(dir.path().join(".atr/checklist.md"), "  \n").unwrap();
        assert!(load(dir.path(), 4096).is_none());
    }

    #[test]
    fn load_truncates() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".atr")).unwrap();
        std::fs::write(dir.path().join(".atr/checklist.md"), "- [x] a\n- [ ] b\n").unwrap();
        assert_eq!(load(dir.path(), 4096).as_deref(), Some("- [x] a\n- [ ] b"));
        let long = "x".repeat(80);
        std::fs::write(dir.path().join(".atr/checklist.md"), &long).unwrap();
        let got = load(dir.path(), 32).unwrap();
        assert!(got.len() <= 32, "{got}");
        assert!(got.ends_with("…[truncated]"));
    }
}
