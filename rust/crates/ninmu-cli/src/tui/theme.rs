/// Named semantic color tokens for the TUI.
///
/// All TUI modules should reference these constants instead of
/// hard-coding color values. This makes theme switching
/// a single-point change.
use ratatui::style::Color;

pub struct Theme;

/// ── Base palette ──────────────────────────────────────────────────────────
impl Theme {
    /// ── Ratatui semantic tokens ────────────────────────────────────────────

    pub const BG: Color = Color::Rgb(7, 8, 13);
    pub const SURFACE: Color = Color::Rgb(16, 19, 28);
    pub const SURFACE_RAISED: Color = Color::Rgb(22, 26, 37);
    pub const BORDER: Color = Color::Rgb(38, 48, 66);
    pub const BORDER_BRIGHT_COLOR: Color = Color::Rgb(67, 82, 112);
    pub const TEXT_COLOR: Color = Color::Rgb(231, 237, 247);
    pub const TEXT_SECONDARY_COLOR: Color = Color::Rgb(154, 166, 184);
    pub const MUTED_COLOR: Color = Color::Rgb(88, 99, 119);
    pub const ACCENT_COLOR: Color = Color::Rgb(255, 107, 53);
    pub const FOCUS: Color = Color::Rgb(0, 217, 255);
    pub const FOCUS_TEXT: Color = Color::Rgb(2, 16, 24);
    pub const FOCUS_MUTED: Color = Color::Rgb(61, 91, 103);
    pub const WARNING_COLOR: Color = Color::Rgb(255, 184, 77);
    pub const WARNING_BG: Color = Color::Rgb(80, 52, 34);
    pub const ERROR_COLOR: Color = Color::Rgb(203, 80, 80);
    pub const SUCCESS_COLOR: Color = Color::Rgb(70, 205, 120);
    pub const THINKING_COLOR: Color = Color::Rgb(210, 92, 255);
    pub const USER_COLOR: Color = Color::Rgb(90, 235, 145);
    pub const USER_COLOR_DIM: Color = Color::Rgb(38, 107, 70);
    pub const ASSISTANT_COLOR: Color = Color::Rgb(204, 222, 255);
    pub const CODE_BG: Color = Color::Rgb(20, 24, 36);
    pub const CODE_FG: Color = Color::Rgb(184, 224, 255);
    pub const INLINE_CODE_BG: Color = Color::Rgb(30, 35, 49);
    pub const PASTE_FLASH_A: Color = Color::Rgb(255, 160, 40);
    pub const PASTE_FLASH_B: Color = Color::Rgb(255, 120, 20);
    pub const ACCENT_ON_COLOR: Color = Color::Rgb(255, 218, 190);

    /// Dark grey/dim for secondary info (status bar, truncation notices).
    pub const DIM: &'static str = "\x1b[2m";
    /// Reset all attributes.
    pub const RESET: &'static str = "\x1b[0m";

    /// ── 256-color semantic tokens ──────────────────────────────────────────

    /// Accent orange (#ff6b35): product branding, success, cost display.
    pub const ACCENT: &'static str = "\x1b[38;2;255;107;53m";
    /// Bright white (#e8e8e8): primary text, tool names.
    pub const TEXT: &'static str = "\x1b[38;5;254m";
    /// Secondary text (#888888): secondary labels, status bar base.
    pub const TEXT_SECONDARY: &'static str = "\x1b[38;5;102m";
    /// Bright border (rgba(255,255,255,0.12)): prominent separators.
    pub const BORDER_BRIGHT: &'static str = "\x1b[38;5;241m";

    /// Green: success indicators, additions.
    pub const SUCCESS: &'static str = "\x1b[38;5;70m";
    /// Red: error indicators, deletions.
    pub const ERROR: &'static str = "\x1b[38;5;203m";
    /// Bright red: critical errors.
    pub const ERROR_BRIGHT: &'static str = "\x1b[1;31m";
    /// Cyan: highlight (tool names, hunk headers).
    pub const HIGHLIGHT: &'static str = "\x1b[1;36m";
    /// Magenta: thinking/reasoning indicators.
    pub const THINKING: &'static str = "\x1b[38;5;13m";
    /// Yellow: warning, permission prompts.
    pub const WARNING: &'static str = "\x1b[1;33m";
    /// Grey (#555555): borders, secondary labels, file headers.
    pub const MUTED: &'static str = "\x1b[38;5;59m";
    /// White on dark grey background: command display (bash inline).
    pub const COMMAND_BG: &'static str = "\x1b[48;5;236;38;5;255m";
    /// Bold green: file write/create.
    pub const SUCCESS_BOLD: &'static str = "\x1b[1;32m";
    /// Bold yellow: file edit.
    pub const EDIT: &'static str = "\x1b[1;33m";

    /// ── Composite styles ───────────────────────────────────────────────────

    /// Truncation notice suffix.
    pub fn truncation_notice() -> String {
        format!(
            "{}… output truncated for display; full result preserved in session.{}",
            Self::DIM,
            Self::RESET
        )
    }

    /// Status bar foreground.
    pub fn status_bar_fg() -> &'static str {
        Self::TEXT_SECONDARY
    }

    /// Permission prompt border.
    pub fn permission_border() -> String {
        format!(
            "{}────────────────────────────────────────────────{}",
            Self::BORDER_BRIGHT,
            Self::RESET
        )
    }
}

/// ── Unit tests ────────────────────────────────────────────────────────────
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn theme_constants_are_non_empty() {
        assert!(!Theme::DIM.is_empty());
        assert!(!Theme::RESET.is_empty());
        assert!(!Theme::SUCCESS.is_empty());
        assert!(!Theme::ERROR.is_empty());
        assert!(!Theme::HIGHLIGHT.is_empty());
        assert!(!Theme::THINKING.is_empty());
        assert!(!Theme::WARNING.is_empty());
        assert!(!Theme::MUTED.is_empty());
        assert!(!Theme::EDIT.is_empty());
        assert!(!Theme::ACCENT.is_empty());
        assert!(!Theme::TEXT.is_empty());
        assert!(!Theme::TEXT_SECONDARY.is_empty());
        assert!(!Theme::BORDER_BRIGHT.is_empty());
    }

    #[test]
    fn ratatui_palette_has_distinct_neon_roles() {
        assert_ne!(Theme::FOCUS, Theme::ACCENT_COLOR);
        assert_ne!(Theme::THINKING_COLOR, Theme::ACCENT_COLOR);
        assert_ne!(Theme::WARNING_COLOR, Theme::ERROR_COLOR);
        assert_ne!(Theme::BG, Theme::SURFACE);
    }

    #[test]
    fn truncation_notice_contains_dim_and_reset() {
        let notice = Theme::truncation_notice();
        assert!(notice.contains(Theme::DIM));
        assert!(notice.contains(Theme::RESET));
        assert!(notice.contains("truncated for display"));
    }
}
