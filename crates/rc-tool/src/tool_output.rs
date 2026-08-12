//! 工具输出有界化:>50KB 持久化到托管目录,模型可见替换为 head + marker + tail。
//! 对齐 opencode tool-output-store(MAX_LINES 2000 / MAX_BYTES 50KB / 7 天清理)。
use std::path::PathBuf;

pub const MAX_INLINE_BYTES: usize = 50 * 1024;
const PREVIEW_BYTES: usize = 1024;
const MARKER: &str = "\n... output truncated; full content saved to ";

#[derive(Debug, Clone)]
pub struct BoundOutput {
    /// 模型可见文本(小)。
    pub text: String,
    /// 完整输出落盘路径(大输出才有)。
    pub path: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ToolOutputStore {
    dir: PathBuf,
}

impl ToolOutputStore {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    /// 有界化:小输出原样返回;大输出写盘 + 替换为 head/marker/path/tail。
    pub fn bound(&self, call_id: &str, output: &str) -> BoundOutput {
        if output.len() <= MAX_INLINE_BYTES {
            return BoundOutput {
                text: output.to_string(),
                path: None,
            };
        }
        std::fs::create_dir_all(&self.dir).ok();
        let file_name = format!("tool_{}.txt", sanitize(call_id));
        let path = self.dir.join(&file_name);
        let write_ok = std::fs::write(&path, output).is_ok();
        if !write_ok {
            // 落盘失败降级:截断内联,仍不把 60KB+ 塞进模型上下文。
            let mut t: String = output.chars().take(PREVIEW_BYTES).collect();
            t.push_str("\n... output truncated (failed to persist) ...");
            return BoundOutput { text: t, path: None };
        }
        let head: String = output.chars().take(PREVIEW_BYTES).collect();
        let tail: String = output
            .chars()
            .rev()
            .take(PREVIEW_BYTES)
            .collect::<String>()
            .chars()
            .rev()
            .collect();
        let text = format!(
            "{head}{MARKER}{}{} ...{}",
            path.to_string_lossy(),
            "\n...",
            tail
        );
        BoundOutput {
            text,
            path: Some(path.to_string_lossy().to_string()),
        }
    }

    /// 清理超过 `days` 天的输出文件,返回删除数。
    pub fn cleanup_older_than(&self, days: u64) -> usize {
        let cutoff = std::time::SystemTime::now()
            .checked_sub(std::time::Duration::from_secs(days * 86400));
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return 0;
        };
        let mut removed = 0;
        for entry in entries.flatten() {
            if let Ok(meta) = entry.metadata() {
                if let Some(cutoff) = cutoff {
                    if let Ok(mtime) = meta.modified() {
                        if mtime < cutoff {
                            let _ = std::fs::remove_file(entry.path());
                            removed += 1;
                        }
                    }
                }
            }
        }
        removed
    }
}

/// call_id 可含任意字符,文件名只留字母数字下划线连字符,防路径穿越。
fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_alphanumeric() || c == '_' || c == '-' { c } else { '_' })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_output_stays_inline() {
        let dir = tempfile::tempdir().unwrap();
        let store = ToolOutputStore::new(dir.path().to_path_buf());
        let bound = store.bound("t1", "small");
        assert_eq!(bound.text, "small");
        assert!(bound.path.is_none());
    }

    #[test]
    fn large_output_persists_and_previews() {
        let dir = tempfile::tempdir().unwrap();
        let store = ToolOutputStore::new(dir.path().to_path_buf());
        let big = "x".repeat(60 * 1024);
        let bound = store.bound("t-big", &big);
        let path = bound.path.expect("large output must persist");
        let on_disk = std::fs::read_to_string(&path).unwrap();
        assert_eq!(on_disk, big, "full output on disk");
        // 模型可见文本 = head + marker + tail,且 < 50KB。
        assert!(bound.text.len() < 50 * 1024);
        assert!(bound.text.contains("truncated; full content saved to"));
        assert!(bound.text.starts_with(&"x".repeat(1024)));
        assert!(bound.text.ends_with(&"x".repeat(1024)));
    }

    #[test]
    fn cleanup_removes_files_older_than_days() {
        let dir = tempfile::tempdir().unwrap();
        let store = ToolOutputStore::new(dir.path().to_path_buf());
        let big = "y".repeat(60 * 1024);
        let bound = store.bound("t-old", &big);
        let path = PathBuf::from(bound.path.unwrap());
        // 把文件 mtime 改到 8 天前。
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(8 * 86400);
        let ft = filetime::FileTime::from_system_time(old);
        filetime::set_file_mtime(&path, ft).unwrap();
        let removed = store.cleanup_older_than(7);
        assert_eq!(removed, 1);
        assert!(!path.exists());
    }
}
