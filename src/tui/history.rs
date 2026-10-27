//! Composer input history (Grok / shell ↑↓).

const MAX_ENTRIES: usize = 200;

/// Recently submitted prompts and slash commands.
///
/// `index` is `None` while editing the live draft. ↑ walks toward older
/// entries; ↓ returns toward the draft. At either edge the current line
/// stays put — history never steals transcript scroll.
#[derive(Debug, Default, Clone)]
pub struct InputHistory {
    entries: Vec<String>,
    index: Option<usize>,
    draft: String,
}

impl InputHistory {
    pub fn push(&mut self, line: &str) {
        let t = line.trim();
        if t.is_empty() {
            return;
        }
        if self.entries.last().map(String::as_str) != Some(t) {
            self.entries.push(t.to_string());
            if self.entries.len() > MAX_ENTRIES {
                self.entries.remove(0);
            }
        }
        self.index = None;
        self.draft.clear();
    }

    /// Older entry, or the oldest again when already there.
    pub fn up(&mut self, current: &str) -> Option<String> {
        if self.entries.is_empty() {
            return None;
        }
        match self.index {
            None => {
                self.draft = current.to_string();
                self.index = Some(self.entries.len() - 1);
            }
            Some(0) => {}
            Some(i) => self.index = Some(i - 1),
        }
        Some(self.entries[self.index.unwrap()].clone())
    }

    /// Newer entry, or the saved draft after the newest.
    pub fn down(&mut self) -> Option<String> {
        match self.index {
            None => None,
            Some(i) if i + 1 >= self.entries.len() => {
                self.index = None;
                Some(std::mem::take(&mut self.draft))
            }
            Some(i) => {
                self.index = Some(i + 1);
                Some(self.entries[i + 1].clone())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn up_down_walks_and_restores_draft() {
        let mut h = InputHistory::default();
        assert!(h.up("").is_none());
        assert!(h.down().is_none());
        h.push("hello");
        h.push("/model");
        h.push("world");
        h.push("world");
        assert_eq!(h.entries.len(), 3);

        assert_eq!(h.up("draft").as_deref(), Some("world"));
        assert_eq!(h.up("").as_deref(), Some("/model"));
        assert_eq!(h.up("").as_deref(), Some("hello"));
        assert_eq!(h.up("").as_deref(), Some("hello"));
        assert_eq!(h.down().as_deref(), Some("/model"));
        assert_eq!(h.down().as_deref(), Some("world"));
        assert_eq!(h.down().as_deref(), Some("draft"));
        assert!(h.down().is_none());
    }

    #[test]
    fn empty_and_whitespace_are_ignored() {
        let mut h = InputHistory::default();
        h.push("  ");
        h.push("");
        assert!(h.entries.is_empty());
        h.push("/help");
        assert_eq!(h.up("").as_deref(), Some("/help"));
    }
}
