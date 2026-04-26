//! Tool execution progress indicator.
//!
//! Renders an inline progress line that updates periodically
//! while a tool is executing, showing tool name and elapsed time.

use std::io::Write;
use std::time::Instant;

use crate::tui::theme::Theme;

/// Renders an inline tool execution progress indicator.
pub struct ToolProgress {
    tool_name: String,
    started_at: Instant,
    frame_index: usize,
}

/// Animation frames for the progress bar.
const BAR_FRAMES: [&str; 4] = ["·  ", "·· ", "···", "·· "];

impl ToolProgress {
    /// Create a new progress indicator for the given tool.
    pub fn new(tool_name: &str) -> Self {
        Self {
            tool_name: tool_name.to_string(),
            started_at: Instant::now(),
            frame_index: 0,
        }
    }

    /// Render a progress tick. Uses save/restore cursor for in-place updates.
    pub fn tick(&mut self, out: &mut dyn Write) -> std::io::Result<()> {
        let elapsed = self.started_at.elapsed().as_secs_f64();
        let frame = BAR_FRAMES[self.frame_index % BAR_FRAMES.len()];
        self.frame_index += 1;

        write!(
            out,
            "\x1b7\x1b[0G\x1b[2K{}-- {}{} {}· running · {:.1}s {}{}\x1b8",
            Theme::MUTED,
            self.tool_name,
            Theme::RESET,
            Theme::ACCENT,
            elapsed,
            frame,
            Theme::RESET,
        )?;
        out.flush()
    }

    /// Clear the progress line (call when tool completes).
    pub fn clear(out: &mut dyn Write) -> std::io::Result<()> {
        write!(out, "\x1b7\x1b[0G\x1b[2K\x1b8")?;
        out.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_tick_writes_tool_name() {
        let mut progress = ToolProgress::new("bash");
        let mut buf = Vec::new();
        progress.tick(&mut buf).expect("tick should succeed");
        let output = String::from_utf8_lossy(&buf);
        assert!(output.contains("bash"));
        assert!(output.contains("running"));
    }

    #[test]
    fn progress_tick_shows_elapsed_time() {
        let mut progress = ToolProgress::new("read_file");
        let mut buf = Vec::new();
        progress.tick(&mut buf).expect("tick should succeed");
        let output = String::from_utf8_lossy(&buf);
        assert!(output.contains(".0s") || output.contains(".1s"));
    }

    #[test]
    fn progress_tick_uses_accent_color() {
        let mut progress = ToolProgress::new("bash");
        let mut buf = Vec::new();
        progress.tick(&mut buf).expect("tick should succeed");
        let output = String::from_utf8_lossy(&buf);
        assert!(output.contains(Theme::ACCENT));
    }

    #[test]
    fn clear_writes_cursor_commands() {
        let mut buf = Vec::new();
        ToolProgress::clear(&mut buf).expect("clear should succeed");
        assert!(!buf.is_empty());
    }
}
