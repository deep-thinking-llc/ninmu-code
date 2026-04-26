use std::time::Duration;

use crate::tui::theme::Theme;

/// The kind of thinking indicator to display.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThinkingKind {
    /// Generic processing (model is thinking about what to do next).
    Processing,
    /// Extended reasoning (model is deep-thinking with chain-of-thought).
    Reasoning,
}

/// Generate animated thinking indicator frames (dot-wave).
pub struct ThinkingFrames;

impl ThinkingFrames {
    /// Returns an iterator that cycles through animation frames forever.
    pub fn frames() -> impl Iterator<Item = String> {
        let accent = Theme::ACCENT.to_string();
        let reset = Theme::RESET.to_string();
        [
            format!("{accent}  ▓░░░{reset}"),
            format!("{accent}  ▓▓░░{reset}"),
            format!("{accent}  ▓▓▓░{reset}"),
            format!("{accent}  ▓▓▓▓{reset}"),
            format!("{accent}  ▓▓▓░{reset}"),
            format!("{accent}  ▓▓░░{reset}"),
            format!("{accent}  ▓░░░{reset}"),
            format!("{accent}  ░░░░{reset}"),
        ]
        .into_iter()
        .cycle()
    }

    /// Frame delay for smooth animation.
    pub fn frame_delay() -> Duration {
        Duration::from_millis(120)
    }
}

/// Generate animated reasoning indicator frames (pulsing brain wave).
/// Distinct from the generic ThinkingFrames dot-wave pattern.
pub struct ReasoningFrames;

impl ReasoningFrames {
    /// Returns an iterator that cycles through reasoning animation frames forever.
    pub fn frames() -> impl Iterator<Item = String> {
        let thinking = Theme::THINKING.to_string();
        let reset = Theme::RESET.to_string();
        [
            format!("{thinking}  ◇{reset}"),
            format!("{thinking}  ◆{reset}"),
            format!("{thinking}  ◇{reset}"),
            format!("{thinking}  ◇{reset}"),
            format!("{thinking}  ◆{reset}"),
            format!("{thinking}  ◇{reset}"),
        ]
        .into_iter()
        .cycle()
    }

    /// Frame delay for reasoning animation (slightly faster pulse).
    pub fn frame_delay() -> Duration {
        Duration::from_millis(200)
    }
}

/// Select the correct frames iterator based on thinking kind.
pub fn frames_for_kind(kind: ThinkingKind) -> Box<dyn Iterator<Item = String>> {
    match kind {
        ThinkingKind::Processing => Box::new(ThinkingFrames::frames()),
        ThinkingKind::Reasoning => Box::new(ReasoningFrames::frames()),
    }
}

/// Format the static "Reasoned for X.Xs" line after thinking completes.
pub fn format_thinking_completed(elapsed: Duration) -> String {
    let secs = elapsed.as_secs_f64();
    format!(
        "{}-- reasoned for {secs:.1}s{}",
        Theme::THINKING,
        Theme::RESET
    )
}

/// Render a short inline thinking indicator for non-animated use.
/// Uses a distinct label depending on whether the model is reasoning vs processing.
pub fn render_thinking_inline(char_count: Option<usize>, redacted: bool) -> String {
    let summary = if redacted {
        format!(
            "{}-- thinking block hidden by provider{}",
            Theme::THINKING,
            Theme::RESET
        )
    } else if let Some(char_count) = char_count {
        format!(
            "{}  reasoning ({char_count} chars){}",
            Theme::THINKING,
            Theme::RESET
        )
    } else {
        format!("{}  reasoning{}", Theme::THINKING, Theme::RESET)
    };
    format!("\n{summary}\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_cycles_indefinitely() {
        let frames: Vec<String> = ThinkingFrames::frames().take(16).collect();
        // 8 unique frames, then repeats
        assert_eq!(frames.len(), 16);
        let first = &frames[0];
        assert_eq!(&frames[8], first); // 9th frame = 1st (cycle)
    }

    #[test]
    fn reasoning_frames_cycles_indefinitely() {
        let frames: Vec<String> = ReasoningFrames::frames().take(12).collect();
        assert_eq!(frames.len(), 12);
        let first = &frames[0];
        assert_eq!(&frames[6], first); // 7th frame = 1st (cycle of 6)
    }

    #[test]
    fn reasoning_frames_uses_thinking_color() {
        let frames: Vec<String> = ReasoningFrames::frames().take(6).collect();
        for frame in &frames {
            assert!(frame.contains(Theme::THINKING));
        }
    }

    #[test]
    fn reasoning_frames_different_from_thinking() {
        let thinking: Vec<String> = ThinkingFrames::frames().take(6).collect();
        let reasoning: Vec<String> = ReasoningFrames::frames().take(6).collect();
        // They should not be identical
        assert_ne!(thinking, reasoning);
    }

    #[test]
    fn frames_for_kind_returns_correct_type() {
        let processing_frames: Vec<String> =
            frames_for_kind(ThinkingKind::Processing).take(2).collect();
        let reasoning_frames: Vec<String> =
            frames_for_kind(ThinkingKind::Reasoning).take(2).collect();
        assert_eq!(processing_frames.len(), 2);
        assert_eq!(reasoning_frames.len(), 2);
        assert_ne!(processing_frames, reasoning_frames);
    }

    #[test]
    fn thinking_completed_formats_seconds() {
        let result = format_thinking_completed(Duration::from_secs_f64(3.5));
        assert!(result.contains("reasoned for"));
        assert!(result.contains("3.5s"));
        assert!(result.contains(Theme::THINKING)); // magenta
    }

    #[test]
    fn thinking_inline_with_char_count() {
        let result = render_thinking_inline(Some(42), false);
        assert!(result.contains("reasoning"));
        assert!(result.contains("42 chars"));
        assert!(result.contains(Theme::THINKING));
    }

    #[test]
    fn thinking_inline_redacted() {
        let result = render_thinking_inline(None, true);
        assert!(result.contains("hidden by provider"));
    }

    #[test]
    fn thinking_inline_without_count() {
        let result = render_thinking_inline(None, false);
        assert!(result.contains("reasoning"));
        assert!(!result.contains("chars"));
    }
}
