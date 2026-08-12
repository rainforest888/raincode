//! Pure display helpers: token/elapsed formatting and CJK-aware truncation.
//! file-level copy from crates/rc-tui/src/repl/fmt.rs
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

pub fn format_tokens(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}K", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

pub fn format_elapsed(secs: u64) -> String {
    if secs < 1 {
        "<1s".into()
    } else if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m {}s", secs / 60, secs % 60)
    } else {
        format!("{}h {}m", secs / 3600, (secs % 3600) / 60)
    }
}

/// Truncate multi-line text to at most `max_lines` lines, appending a count
/// marker when lines were dropped.
pub fn truncate_output(text: &str, max_lines: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let mut out: Vec<String> = lines.iter().take(max_lines).map(|l| l.to_string()).collect();
    if lines.len() > max_lines {
        out.push(format!("… (+{} more)", lines.len() - max_lines));
    }
    out.join("\n")
}

/// Truncate a single line to at most `width` display columns (CJK = 2),
/// appending an ellipsis when truncation happened.
pub fn truncate_line(text: &str, width: usize) -> String {
    if UnicodeWidthStr::width(text) <= width {
        return text.to_string();
    }
    let mut out = String::new();
    let mut w = 0usize;
    for ch in text.chars() {
        let cw = UnicodeWidthChar::width(ch).unwrap_or(0);
        if w + cw > width.saturating_sub(1) {
            break;
        }
        out.push(ch);
        w += cw;
    }
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_tokens_with_units() {
        assert_eq!(format_tokens(0), "0");
        assert_eq!(format_tokens(999), "999");
        assert_eq!(format_tokens(1_200), "1.2K");
        assert_eq!(format_tokens(128_000), "128.0K");
        assert_eq!(format_tokens(1_500_000), "1.5M");
    }

    #[test]
    fn formats_elapsed_hms() {
        assert_eq!(format_elapsed(0), "<1s");
        assert_eq!(format_elapsed(26), "26s");
        assert_eq!(format_elapsed(63), "1m 3s");
        assert_eq!(format_elapsed(3_700), "1h 1m");
    }

    #[test]
    fn truncates_multiline_output() {
        assert_eq!(truncate_output("a\nb\nc", 5), "a\nb\nc");
        assert_eq!(truncate_output("a\nb\nc", 2), "a\nb\n… (+1 more)");
    }

    #[test]
    fn truncates_line_cjk_aware() {
        assert_eq!(truncate_line("hello", 10), "hello");
        // "hello w" 是 7 列,加省略号后 = 8 ≤ 8。
        assert_eq!(truncate_line("hello world", 8), "hello w…");
        // "中文很" 显示宽度 6,加省略号后 = 7 ≤ 7。
        assert_eq!(truncate_line("中文很长", 7), "中文很…");
    }
}
