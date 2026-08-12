//! Startup splash: a clean RAINCODE wordmark (light-blue half-block glyphs),
//! centered vertically, nothing else. `frame()` is pure and testable; `play()`
//! shows it briefly then clears so the REPL starts clean.

use std::io::{self, Write};

use crossterm::cursor::MoveTo;
use crossterm::queue;
use crossterm::style::Print;
use crossterm::terminal::{Clear, ClearType};

const RESET: &str = "\x1b[0m";
const DIM: &str = "\x1b[2m";
const TITLE_BLUE: &str = "\x1b[38;2;120;180;255m"; // 淡蓝标题

/// RAINCODE 标准 5×7 点阵,半块字符(▀▄█)渲染。每字母 5 宽,4 行高。
const TITLE: &[&str] = &[
    "█▀▀▀▄ ▄▀▀▀▄ ▀▀█▀▀ █▄  █ ▄▀▀▀▀ ▄▀▀▀▄ █▀▀▀▄ █▀▀▀▀",
    "█▄▄▄▀ █▄▄▄█   █   █ ▀▄█ █     █   █ █   █ █▄▄▄ ",
    "█ ▀▄  █   █   █   █   █ █     █   █ █   █ █    ",
    "▀   ▀ ▀   ▀ ▀▀▀▀▀ ▀   ▀  ▀▀▀▀  ▀▀▀  ▀▀▀▀  ▀▀▀▀▀",
];

/// 一帧 splash:标题垂直居中(根据终端高度),左对齐 2 格。
pub fn frame(width: usize, height: usize) -> String {
    let width = width.max(34);
    let mut out = String::new();
    // 垂直居中:标题上方补空行。
    let pad_top = height.saturating_sub(TITLE.len() + 1) / 2;
    for _ in 0..pad_top {
        out.push_str("\r\n");
    }
    for line in TITLE {
        out.push_str(&format!("{TITLE_BLUE}{}{RESET}\r\n", indent(line)));
    }
    out.push_str(&format!("{DIM}  raincode — terminal coding agent{RESET}\r\n"));
    let _ = width;
    out
}

fn indent(line: &str) -> String {
    format!("  {line}")
}

/// Play the splash (brief), then clear to a blank line so the REPL starts clean.
/// 光标由 Shell 管理(enter 隐藏 / leave 显示),这里不碰。
pub fn play(stdout: &mut io::Stdout) -> io::Result<()> {
    let (w, h) = crossterm::terminal::size()?;
    let f = frame(w as usize, h as usize);
    queue!(stdout, MoveTo(0, 0), Clear(ClearType::All), Print(f))?;
    stdout.flush()?;
    std::thread::sleep(std::time::Duration::from_millis(1200));
    queue!(stdout, MoveTo(0, 0), Clear(ClearType::All))?;
    stdout.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_contains_title() {
        let f = frame(80, 24);
        assert!(f.contains("█▀▀▀▄")); // 标题字形
        assert!(f.contains("█▄▄▄▀"));
    }

    #[test]
    fn title_uses_light_blue() {
        let f = frame(80, 24);
        assert!(f.contains("\x1b[38;2;120;180;255m"));
    }

    #[test]
    fn frame_centers_vertically() {
        // 高屏顶部有空行(居中)。
        assert!(frame(80, 40).starts_with("\r\n"));
        // 高度不足标题+footer(5 行)时不补空行。
        assert!(!frame(80, 5).starts_with("\r\n"));
    }

    #[test]
    fn no_forest_art() {
        let f = frame(80, 24);
        // 标题用半块字形 ▀▄█,不应再有像素树林的实心块/纹理。
        assert!(!f.contains('▓'));
        assert!(!f.contains('▒'));
        assert!(!f.contains("████"));
    }
}
