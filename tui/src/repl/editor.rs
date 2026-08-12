//! Pure text editor for the REPL input line (no terminal I/O).
//! file-level copy from crates/rc-tui/src/repl/editor.rs
#[derive(Clone)]
pub struct InputEditor {
    pub text: String,
    pub cursor: usize, // byte index into `text`
    pub history: Vec<String>,
    pub history_idx: Option<usize>,
}

impl InputEditor {
    pub fn new() -> Self {
        Self {
            text: String::new(),
            cursor: 0,
            history: Vec::new(),
            history_idx: None,
        }
    }
}

impl Default for InputEditor {
    fn default() -> Self {
        Self::new()
    }
}

impl InputEditor {
    pub fn insert_char(&mut self, c: char) {
        self.text.insert(self.cursor, c);
        self.cursor += c.len_utf8();
    }

    pub fn delete_before(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let prev = self.text[..self.cursor].chars().next_back().unwrap();
        self.cursor -= prev.len_utf8();
        self.text.remove(self.cursor);
    }

    pub fn delete_after(&mut self) {
        if self.cursor >= self.text.len() {
            return;
        }
        self.text.remove(self.cursor);
    }

    pub fn move_cursor(&mut self, delta: i32) {
        if delta > 0 {
            let mut to = self.cursor;
            let mut n = delta;
            while n > 0 {
                if to >= self.text.len() {
                    break;
                }
                to += self.text[to..].chars().next().unwrap().len_utf8();
                n -= 1;
            }
            self.cursor = to;
        } else {
            let mut to = self.cursor;
            let mut n = -delta;
            while n > 0 {
                if to == 0 {
                    break;
                }
                to -= self.text[..to].chars().next_back().unwrap().len_utf8();
                n -= 1;
            }
            self.cursor = to;
        }
    }

    pub fn move_to_start(&mut self) {
        self.cursor = 0;
    }

    pub fn move_to_end(&mut self) {
        self.cursor = self.text.len();
    }

    pub fn insert_newline(&mut self) {
        self.insert_char('\n');
    }

    /// Commit the current line to history and return it (clearing the buffer).
    pub fn submit(&mut self) -> Option<String> {
        let t = self.text.trim().to_string();
        if t.is_empty() {
            self.text.clear();
            self.cursor = 0;
            return None;
        }
        self.history.push(t.clone());
        if self.history.len() > 200 {
            self.history.remove(0);
        }
        self.history_idx = None;
        self.text.clear();
        self.cursor = 0;
        Some(t)
    }

    pub fn history_prev(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let idx = match self.history_idx {
            Some(i) if i > 0 => i - 1,
            _ => self.history.len() - 1,
        };
        self.history_idx = Some(idx);
        self.text = self.history[idx].clone();
        self.cursor = self.text.len();
    }

    pub fn history_next(&mut self) {
        let Some(i) = self.history_idx else { return };
        if i + 1 < self.history.len() {
            let idx = i + 1;
            self.history_idx = Some(idx);
            self.text = self.history[idx].clone();
            self.cursor = self.text.len();
        } else {
            self.history_idx = None;
            self.text.clear();
            self.cursor = 0;
        }
    }

    /// 从最近往回搜索包含 `query` 的历史项（codex Ctrl+R 式 reverse-i-search）。
    pub fn history_search(&self, query: &str) -> Option<String> {
        self.history.iter().rev().find(|h| h.contains(query)).cloned()
    }

    /// 追加最近一条历史到 JSONL 文件（幂等：只写本次 submit 的那条）。
    pub fn append_history(&self, path: &std::path::Path) -> std::io::Result<()> {
        use std::io::Write;
        if let Some(last) = self.history.last() {
            let mut f = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)?;
            writeln!(f, "{}", serde_json::to_string(last).unwrap_or_default())?;
        }
        Ok(())
    }

    /// 从 JSONL 文件加载历史到输入框（会话重启后仍能看到本会话的输入记录）。
    /// 文件不存在 → 空;坏行跳过。
    pub fn load_history(&mut self, path: &std::path::Path) {
        let Ok(content) = std::fs::read_to_string(path) else {
            return;
        };
        for line in content.lines() {
            if let Ok(s) = serde_json::from_str::<String>(line) {
                if !s.trim().is_empty() && !self.history.contains(&s) {
                    self.history.push(s);
                }
            }
        }
        if self.history.len() > 200 {
            let start = self.history.len() - 200;
            self.history.drain(..start);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inserts_deletes_and_moves() {
        let mut e = InputEditor::new();
        e.insert_char('a');
        e.insert_char('中');
        e.insert_char('b');
        assert_eq!(e.text, "a中b");
        e.move_cursor(-1);
        e.delete_before();
        assert_eq!(e.text, "ab");
        assert_eq!(e.cursor, 1);
        e.delete_after();
        assert_eq!(e.text, "a");
        e.move_to_end();
        e.insert_char('x');
        assert_eq!(e.text, "ax");
    }

    #[test]
    fn submit_trims_and_clears() {
        let mut e = InputEditor::new();
        e.insert_char('h');
        e.insert_char('i');
        assert_eq!(e.submit(), Some("hi".into()));
        assert_eq!(e.text, "");
        assert_eq!(e.submit(), None); // empty → None
    }

    #[test]
    fn history_navigates() {
        let mut e = InputEditor::new();
        for s in ["one", "two"] {
            for ch in s.chars() {
                e.insert_char(ch);
            }
            e.submit();
        }
        e.history_prev();
        assert_eq!(e.text, "two");
        e.history_prev();
        assert_eq!(e.text, "one");
        e.history_next();
        assert_eq!(e.text, "two");
        e.history_next();
        assert_eq!(e.text, "");
    }

    #[test]
    fn multiline_keeps_newline() {
        let mut e = InputEditor::new();
        e.insert_char('a');
        e.insert_newline();
        e.insert_char('b');
        assert_eq!(e.text, "a\nb");
        assert_eq!(e.submit(), Some("a\nb".into()));
    }

    #[test]
    fn cursor_respects_utf8_boundaries() {
        let mut e = InputEditor::new();
        e.insert_char('a');
        e.insert_char('中');
        e.insert_char('c');
        e.move_cursor(-1);
        assert_eq!(&e.text[e.cursor..], "c"); // 不能落在中文字节中间
        e.insert_char('x');
        assert_eq!(e.text, "a中xc");
    }

    #[test]
    fn history_search_finds_oldest_match_from_recent() {
        let mut e = InputEditor::new();
        e.history = vec!["cargo test".into(), "git status".into(), "cargo build".into()];
        assert_eq!(e.history_search("cargo"), Some("cargo build".into()));
        assert_eq!(e.history_search("git"), Some("git status".into()));
        assert_eq!(e.history_search("zzz"), None);
    }

    #[test]
    fn history_persists_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hist.jsonl");
        let mut e = InputEditor::new();
        e.history = vec!["hi".into()];
        e.append_history(&path).unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        assert_eq!(content.trim(), serde_json::to_string("hi").unwrap());
    }
}
