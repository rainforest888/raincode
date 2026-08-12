//! Semantic ANSI colors (opencode dark theme).
//!
//! Borrowed from opencode's default dark theme (`theme/assets/opencode.json`):
//! primary #fab283 (peach), secondary #5c9cf5 (blue), accent #9d7cd8 (purple),
//! error #e06c75, warning #f5a742, success #7fd88f, info #56b6c2.
pub const RESET: &str = "\x1b[0m";
pub const PRIMARY: &str = "\x1b[38;2;250;178;131m";
pub const SECONDARY: &str = "\x1b[38;2;92;156;245m";
pub const ERROR: &str = "\x1b[38;2;224;108;117m";
pub const WARNING: &str = "\x1b[38;2;245;167;66m";
pub const SUCCESS: &str = "\x1b[38;2;127;216;143m";
pub const INFO: &str = "\x1b[38;2;86;182;194m";
pub const BRIGHT_YELLOW: &str = "\x1b[93m";
pub const DIM: &str = "\x1b[2m";
pub const BOLD: &str = "\x1b[1m";
/// 纯红(监督 agent 输出用,区别于 Error 的柔红 #e06c75)。
pub const RED: &str = "\x1b[31m";
/// 删除线(denied 工具行)。
pub const STRIKE: &str = "\x1b[9m";

pub fn fg(code: &str, text: &str) -> String {
    format!("{code}{text}{RESET}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fg_wraps_and_resets() {
        assert_eq!(fg(SUCCESS, "ok"), "\x1b[38;2;127;216;143mok\x1b[0m");
    }
}
