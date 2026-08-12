//! 7 色 agent 轮换 + 对比度感知前景。对齐 opencode A5 / 99-synthesis Part A。
use crate::repl::palette::*;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

/// 7 色轮换(opencode 默认:secondary/accent/success/warning/primary/error/info;
/// accent≈info,故第 7 位用 BRIGHT_YELLOW 保证 7 个色码互不相同)。
pub const AGENT_COLORS: [&str; 7] = [
    SECONDARY, INFO, SUCCESS, WARNING, PRIMARY, ERROR, BRIGHT_YELLOW,
];

/// 按 agent 名取稳定颜色(哈希取模 7)。index_hint 若提供则优先(可见 agent 下标取模)。
pub fn agent_color(name: &str, index_hint: Option<usize>) -> &'static str {
    if let Some(i) = index_hint {
        return AGENT_COLORS[i % AGENT_COLORS.len()];
    }
    let mut h = DefaultHasher::new();
    name.hash(&mut h);
    AGENT_COLORS[(h.finish() as usize) % AGENT_COLORS.len()]
}

/// 对比度感知前景:按 bg 亮度选黑/白(RGB → 0.299r+0.587g+0.114b > 0.5 → 黑)。
pub fn contrast_fg(bg_rgb: (u8, u8, u8)) -> &'static str {
    let (r, g, b) = bg_rgb;
    let lum = (r as f64 * 0.299 + g as f64 * 0.587 + b as f64 * 0.114) / 255.0;
    if lum > 0.5 { "\x1b[30m" } else { "\x1b[97m" }
}

/// 从 `\x1b[38;2;R;G;Bm` 前景码提取 RGB;非 truecolor 码返回 None。
fn parse_fg_rgb(code: &str) -> Option<(u8, u8, u8)> {
    let inner = code.strip_prefix("\x1b[38;2;")?.strip_suffix('m')?;
    let mut it = inner.splitn(3, ';');
    Some((
        it.next()?.parse().ok()?,
        it.next()?.parse().ok()?,
        it.next()?.parse().ok()?,
    ))
}

/// QUEUED 徽标:以 agent 色(38;2 前景码)为背景 + 亮度对比前景(白底黑字/黑底白字)。
/// 解析失败回退 SECONDARY 蓝(agent 色之一)。
pub fn queued_badge(color: &str) -> String {
    let (r, g, b) = parse_fg_rgb(color).unwrap_or((92, 156, 245)); // SECONDARY #5c9cf5
    let fg = contrast_fg((r, g, b));
    format!("\x1b[48;2;{r};{g};{b}m{fg} QUEUED {RESET}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_color_is_stable_per_name() {
        assert_eq!(agent_color("a1", None), agent_color("a1", None));
        assert_ne!(agent_color("a1", None), agent_color("a2", None));
    }

    #[test]
    fn agent_color_honors_index_hint() {
        assert_eq!(agent_color("anything", Some(0)), AGENT_COLORS[0]);
        assert_eq!(agent_color("anything", Some(7)), AGENT_COLORS[0]); // 取模 7
    }

    #[test]
    fn contrast_fg_uses_luminance() {
        assert_eq!(contrast_fg((255, 255, 255)), "\x1b[30m"); // 白底 → 黑字
        assert_eq!(contrast_fg((0, 0, 0)), "\x1b[97m"); // 黑底 → 白字
    }

    #[test]
    fn seven_colors_exhaustive() {
        // 7 个不同色码,与 palette 语义色对应。
        let colors = AGENT_COLORS;
        assert_eq!(colors.len(), 7);
        let mut uniq = std::collections::HashSet::new();
        for c in colors {
            assert!(uniq.insert(c));
        }
    }
}
