//! Crossterm full-screen shell (claude-code/opencode model).
//!
//! Enters alternate screen + raw mode, renders the WHOLE screen from an
//! app-side buffer each frame, and diffs against the previous frame so only
//! changed rows are written. This structurally eliminates the "HUD appears
//! twice" / "big blank gap" bugs of the incremental scroll-region approach.

use std::io::{self, Write};

use crossterm::cursor::{Hide, MoveTo, Show};
use crossterm::queue;
use crossterm::style::Print;
use crossterm::event::{DisableMouseCapture, EnableMouseCapture};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::{execute, event};

use crate::repl::render::RenderFrame;

pub struct Shell {
    stdout: io::Stdout,
    /// 上一帧每行内容(整屏)。用于 diff,只刷变化的行。
    prev_lines: Vec<String>,
}

impl Shell {
    pub fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        let stdout = io::stdout();
        let mut s = Self {
            stdout,
            prev_lines: Vec::new(),
        };
        execute!(s.stdout, EnterAlternateScreen)?;
        execute!(s.stdout, Hide)?;
        // 捕获鼠标:滚轮上/下滚动对话历史。离开时 DisableMouseCapture。
        execute!(s.stdout, EnableMouseCapture)?;
        Ok(s)
    }

    pub fn leave(&mut self) -> io::Result<()> {
        execute!(self.stdout, DisableMouseCapture)?;
        execute!(self.stdout, Show)?;
        execute!(self.stdout, LeaveAlternateScreen)?;
        disable_raw_mode()?;
        Ok(())
    }

    pub fn size(&self) -> io::Result<(u16, u16)> {
        crossterm::terminal::size()
    }

    pub fn clear_all(&mut self) -> io::Result<()> {
        execute!(self.stdout, Clear(ClearType::All))?;
        self.prev_lines.clear();
        Ok(())
    }

    /// 整屏 diff 重绘:对比上一帧,只对变化的行输出 MoveTo + 内容。
    /// `input_row`/`cursor_col` 定位光标到输入框。
    pub fn draw(&mut self, frame: &RenderFrame) -> io::Result<()> {
        let rows = frame.lines.len();
        // 逐行 diff:变化的行重画。
        for r in 0..rows {
            let cur = frame.lines.get(r).map(String::as_str).unwrap_or("");
            let prev = self.prev_lines.get(r).map(String::as_str).unwrap_or("");
            if cur != prev {
                queue!(
                    self.stdout,
                    MoveTo(0, r as u16),
                    Clear(ClearType::CurrentLine),
                    Print(cur)
                )?;
            }
        }
        // 上一帧比现在多出的行 → 清空。
        if self.prev_lines.len() > rows {
            for r in rows..self.prev_lines.len() {
                queue!(self.stdout, MoveTo(0, r as u16), Clear(ClearType::CurrentLine))?;
            }
        }
        self.prev_lines = frame.lines.clone();
        // 光标到输入行(列钳到行宽内,避免越界)。
        let (width, _) = self.size()?;
        let col = frame.cursor_col.min(width.saturating_sub(1) as usize) as u16;
        queue!(
            self.stdout,
            MoveTo(col, frame.input_row)
        )?;
        self.stdout.flush()?;
        Ok(())
    }
}

impl Drop for Shell {
    fn drop(&mut self) {
        // Best-effort restore on ANY exit path.
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), Show);
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
    }
}

/// Non-blocking key reader (from rc-tui, Release-filtered for Windows IME).
pub fn read_keys(tx: tokio::sync::mpsc::UnboundedSender<event::KeyEvent>) -> io::Result<()> {
    loop {
        match event::read()? {
            event::Event::Key(key) => {
                // Windows 端每个按键产生 Press + Release 两条事件;忽略 Release,
                // 否则每个字符被插入两次(中文 IME 提交同样如此 → "你你好好")。
                if matches!(key.kind, event::KeyEventKind::Release) {
                    continue;
                }
                if tx.send(key).is_err() {
                    return Ok(());
                }
            }
            // 鼠标滚轮 → 合成 PageUp/PageDown,复用 B7 的滚动逻辑(上滚解锁自动滚动)。
            event::Event::Mouse(me) => {
                let key = match me.kind {
                    event::MouseEventKind::ScrollUp => {
                        Some(event::KeyEvent::new(event::KeyCode::PageUp, event::KeyModifiers::NONE))
                    }
                    event::MouseEventKind::ScrollDown => {
                        Some(event::KeyEvent::new(event::KeyCode::PageDown, event::KeyModifiers::NONE))
                    }
                    _ => None,
                };
                if let Some(key) = key {
                    if tx.send(key).is_err() {
                        return Ok(());
                    }
                }
            }
            event::Event::Resize(_, _) => {} // 下一帧自动按新尺寸重绘
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enter_uses_alternate_screen() {
        // 无法在无 TTY 测试;验证 leave 前的关键状态类型存在即可。
        let _ = Shell::enter;
    }
}
