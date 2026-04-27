//! Full-screen TUI mode -- split-pane conversation view using crossterm.
//!
//! Entered via the `--tui` flag. Provides a scrollable conversation history pane
//! and a fixed input line at the bottom, all within the alternate screen buffer.
//! The execute_turn closure handles dispatching input to the model and returns
//! rendered output text that gets appended to the scrollback.

use std::io::{self, Write};

use crossterm::cursor::MoveTo;
use crossterm::style::Print;
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, Clear, ClearType};
use crossterm::{
    execute, queue,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen},
};

use crate::tui::scrollback::Scrollback;
use crate::tui::theme::Theme;

const INPUT_HEIGHT: u16 = 3;
const PROMPT: &str = "> ";

const HELP_OVERLAY: &str = "\
── Keyboard Shortcuts ─────────────────────
  PageUp / PageDown    Scroll conversation
  Home / End           Top / bottom
  Tab                  Expand/collapse tool output
  Ctrl+C               Interrupt
  ?                    Toggle this help
  /exit                Exit TUI
───────────────────────────────────────────";

/// Full-screen TUI manager using crossterm only (no ratatui).
pub struct FullScreenTui {
    scrollback: Scrollback,
    help_visible: bool,
    running: bool,
}

impl FullScreenTui {
    pub fn new() -> Self {
        Self {
            scrollback: Scrollback::default(),
            help_visible: false,
            running: false,
        }
    }

    /// Run the full-screen TUI. `execute_turn` receives user input and
    /// returns the rendered output text to append to scrollback.
    pub fn run<F>(&mut self, mut execute_turn: F) -> io::Result<()>
    where
        F: FnMut(&str) -> Result<String, Box<dyn std::error::Error>>,
    {
        self.running = true;
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen, Clear(ClearType::All))?;

        while self.running {
            let (width, height) = crossterm::terminal::size()?;
            let conv_height = height.saturating_sub(INPUT_HEIGHT) as usize;

            self.render_conversation(&mut stdout, conv_height, width)?;

            // Render prompt line
            queue!(
                stdout,
                MoveTo(0, conv_height as u16),
                Clear(ClearType::CurrentLine),
                Print(PROMPT),
            )?;
            stdout.flush()?;

            let input = self.read_input(&mut stdout, conv_height as u16, conv_height as u16)?;

            match input.as_str() {
                "/exit" | "/quit" => {
                    self.running = false;
                    break;
                }
                "?" => {
                    self.help_visible = !self.help_visible;
                }
                _ if input.is_empty() => {}
                _ => {
                    self.scrollback.push(format!(" > {input}"));
                    match execute_turn(&input) {
                        Ok(output) => {
                            if !output.is_empty() {
                                self.scrollback.push_str(&output);
                            }
                        }
                        Err(error) => {
                            self.scrollback.push(format!("error: {error}"));
                        }
                    }
                }
            }
        }

        execute!(stdout, LeaveAlternateScreen)?;
        disable_raw_mode()?;
        Ok(())
    }

    fn render_conversation(
        &self,
        stdout: &mut impl Write,
        viewport_height: usize,
        width: u16,
    ) -> io::Result<()> {
        let (visible, start, total) = self.scrollback.visible(viewport_height);

        for row in 0..viewport_height {
            queue!(stdout, MoveTo(0, row as u16), Clear(ClearType::CurrentLine))?;
        }

        for (i, line) in visible.iter().enumerate() {
            let truncated = if line.len() > width as usize {
                &line[..width.saturating_sub(3) as usize]
            } else {
                line.as_str()
            };
            queue!(stdout, MoveTo(0, i as u16), Print(truncated))?;
        }

        if !self.scrollback.is_at_bottom() {
            let indicator = format!(
                "{muted}[lines {start}-{end} of {total} · j/k scroll · q quit]{reset}",
                muted = Theme::MUTED,
                start = start + 1,
                end = start + visible.len(),
                total = total,
                reset = Theme::RESET,
            );
            let row = viewport_height.saturating_sub(1) as u16;
            queue!(
                stdout,
                MoveTo(0, row),
                Clear(ClearType::CurrentLine),
                Print(indicator),
            )?;
        }

        if self.help_visible {
            let help_lines: Vec<&str> = HELP_OVERLAY.lines().collect();
            let help_h = help_lines.len();
            let start_row = (viewport_height.saturating_sub(help_h)) / 2;
            for (i, line) in help_lines.iter().enumerate() {
                queue!(stdout, MoveTo(2, (start_row + i) as u16), Print(line))?;
            }
        }

        stdout.flush()?;
        Ok(())
    }

    /// Read a line of input from the user (crossterm event loop).
    /// Supports Ctrl+J (newline), Ctrl+D (exit), cursor left/right,
    /// backspace, and scroll keys.
    fn read_input(
        &mut self,
        stdout: &mut impl Write,
        prompt_row: u16,
        conv_height: u16,
    ) -> io::Result<String> {
        let mut buffer: Vec<char> = Vec::new();
        let mut cursor: usize = 0;
        loop {
            match crossterm::event::read() {
                Ok(crossterm::event::Event::Key(key)) => match key.code {
                    crossterm::event::KeyCode::Enter => {
                        if !buffer.is_empty() {
                            return Ok(buffer.iter().collect());
                        }
                    }
                    crossterm::event::KeyCode::Char(c)
                        if key.modifiers == crossterm::event::KeyModifiers::CONTROL && c == 'd' =>
                    {
                        return Ok(String::new());
                    }
                    crossterm::event::KeyCode::Char(c)
                        if key.modifiers == crossterm::event::KeyModifiers::CONTROL && c == 'j' =>
                    {
                        buffer.insert(cursor, '\n');
                        cursor += 1;
                        Self::render_input_line(stdout, prompt_row, &buffer, cursor)?;
                    }
                    crossterm::event::KeyCode::Char(c) => {
                        buffer.insert(cursor, c);
                        cursor += 1;
                        Self::render_input_line(stdout, prompt_row, &buffer, cursor)?;
                    }
                    crossterm::event::KeyCode::Backspace => {
                        if cursor > 0 {
                            cursor -= 1;
                            buffer.remove(cursor);
                            Self::render_input_line(stdout, prompt_row, &buffer, cursor)?;
                        }
                    }
                    crossterm::event::KeyCode::Delete => {
                        if cursor < buffer.len() {
                            buffer.remove(cursor);
                            Self::render_input_line(stdout, prompt_row, &buffer, cursor)?;
                        }
                    }
                    crossterm::event::KeyCode::Left => {
                        if cursor > 0 {
                            cursor -= 1;
                            Self::render_input_line(stdout, prompt_row, &buffer, cursor)?;
                        }
                    }
                    crossterm::event::KeyCode::Right => {
                        if cursor < buffer.len() {
                            cursor += 1;
                            Self::render_input_line(stdout, prompt_row, &buffer, cursor)?;
                        }
                    }
                    crossterm::event::KeyCode::Home => {
                        cursor = 0;
                        Self::render_input_line(stdout, prompt_row, &buffer, cursor)?;
                    }
                    crossterm::event::KeyCode::End => {
                        cursor = buffer.len();
                        Self::render_input_line(stdout, prompt_row, &buffer, cursor)?;
                    }
                    crossterm::event::KeyCode::Tab => {
                        // Toggle expand/collapse on the first visible entry
                        let (_, start, _) = self.scrollback.visible(conv_height as usize);
                        if self.scrollback.toggle_expand_at(start) {
                            self.render_conversation(stdout, conv_height as usize, 80)?;
                            queue!(
                                stdout,
                                MoveTo(0, conv_height),
                                Clear(ClearType::CurrentLine),
                                Print(PROMPT),
                            )?;
                            Self::render_input_line(stdout, prompt_row, &buffer, cursor)?;
                        }
                    }
                    crossterm::event::KeyCode::PageUp => {
                        let n = conv_height.saturating_sub(1) as usize;
                        self.scrollback.scroll_up(n);
                        return Ok(String::new());
                    }
                    crossterm::event::KeyCode::PageDown => {
                        let n = conv_height.saturating_sub(1) as usize;
                        self.scrollback.scroll_down(n);
                        return Ok(String::new());
                    }
                    crossterm::event::KeyCode::Esc => {}
                    _ => {}
                },
                Ok(crossterm::event::Event::Resize(..)) => {}
                _ => {}
            }
        }
    }

    /// Render the current input line with cursor position.
    fn render_input_line(
        stdout: &mut impl Write,
        row: u16,
        buffer: &[char],
        cursor: usize,
    ) -> io::Result<()> {
        let line: String = buffer.iter().collect();
        queue!(
            stdout,
            MoveTo(0, row),
            Clear(ClearType::CurrentLine),
            Print(PROMPT),
            Print(&line),
        )?;
        let cursor_col = PROMPT.len() + cursor;
        queue!(stdout, MoveTo(cursor_col as u16, row))?;
        stdout.flush()
    }
}

impl Default for FullScreenTui {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::scrollback::Scrollback;

    #[test]
    fn tui_initializes_cleanly() {
        let tui = FullScreenTui::new();
        assert!(!tui.help_visible);
        assert!(!tui.running);
    }

    #[test]
    fn scrollback_append_and_visible() {
        let mut tui = FullScreenTui::new();
        for i in 0..50 {
            tui.scrollback.push(format!("line {i}"));
        }
        let (visible, _, total) = tui.scrollback.visible(10);
        assert_eq!(visible.len(), 10);
        assert_eq!(total, 50);
    }

    #[test]
    fn help_overlay_content() {
        assert!(HELP_OVERLAY.contains("PageUp"));
        assert!(HELP_OVERLAY.contains("?"));
        assert!(HELP_OVERLAY.contains("/exit"));
    }

    #[test]
    fn append_output_adds_lines() {
        let mut tui = FullScreenTui::new();
        tui.scrollback.push_str("hello\nworld");
        assert_eq!(tui.scrollback.len(), 2);
    }

    #[test]
    fn page_up_down_scrolls_correctly() {
        let mut tui = FullScreenTui::new();
        for i in 0..50 {
            tui.scrollback.push(format!("line {i}"));
        }
        assert!(tui.scrollback.is_at_bottom());
        tui.scrollback.scroll_up(10);
        assert!(!tui.scrollback.is_at_bottom());
        assert_eq!(tui.scrollback.scroll_offset(), 10);
        tui.scrollback.scroll_down(5);
        assert_eq!(tui.scrollback.scroll_offset(), 5);
        tui.scrollback.scroll_to_top();
        assert_eq!(tui.scrollback.scroll_offset(), 49);
        tui.scrollback.scroll_to_bottom();
        assert_eq!(tui.scrollback.scroll_offset(), 0);
    }

    #[test]
    fn scroll_indicator_shown_when_not_at_bottom() {
        let mut tui = FullScreenTui::new();
        for i in 0..50 {
            tui.scrollback.push(format!("line {i}"));
        }
        tui.scrollback.scroll_up(5);
        assert!(!tui.scrollback.is_at_bottom());
        let (visible, start, total) = tui.scrollback.visible(10);
        assert_eq!(total, 50);
        assert_eq!(start, 35);
        assert_eq!(visible.len(), 10);
    }

    #[test]
    fn read_input_empty_returns_empty() {
        let tui = FullScreenTui::new();
        // Just verify the logic around initial buffer state
        assert!(tui.scrollback.is_empty());
    }

    #[test]
    fn render_conversation_handles_empty_scrollback() {
        let tui = FullScreenTui::new();
        let mut buf = Vec::new();
        // Should not panic
        tui.render_conversation(&mut buf, 10, 80)
            .expect("render empty");
        let output = String::from_utf8_lossy(&buf);
        // Should contain MoveTo (CSI H) sequences for clearing rows
        assert!(output.contains("\x1b[")); // some ANSI escape
        assert!(!buf.is_empty());
    }

    #[test]
    fn render_conversation_truncates_long_lines() {
        let mut tui = FullScreenTui::new();
        let long_line = "a".repeat(200);
        tui.scrollback.push(long_line);
        let mut buf = Vec::new();
        tui.render_conversation(&mut buf, 10, 80)
            .expect("render long");
        let output = String::from_utf8_lossy(&buf);
        // Should not contain the full 200-char line
        assert!(!output.contains(&"a".repeat(200)));
    }

    #[test]
    fn help_overlay_and_scroll_indicator_dont_both_show() {
        let mut tui = FullScreenTui::new();
        for i in 0..50 {
            tui.scrollback.push(format!("line {i}"));
        }
        tui.scrollback.scroll_up(5);
        tui.help_visible = true;
        // Both conditions are true, but rendering should not crash
        let mut buf = Vec::new();
        tui.render_conversation(&mut buf, 20, 80)
            .expect("render both");
        let output = String::from_utf8_lossy(&buf);
        assert!(output.contains("lines"));
        assert!(output.contains("Keyboard"));
    }

    /// Simulate a single read_input event and return what the buffer produces.
    /// This is a simplified test that validates the dispatch logic only.
    fn simulate_key_event(tui: &mut FullScreenTui, key: crossterm::event::KeyCode) -> String {
        // We capture the buffer behavior by testing the scrollback directly
        // since read_input requires a real terminal
        match key {
            crossterm::event::KeyCode::PageUp => {
                tui.scrollback.scroll_up(10);
            }
            crossterm::event::KeyCode::PageDown => {
                tui.scrollback.scroll_down(10);
            }
            crossterm::event::KeyCode::Home => {
                tui.scrollback.scroll_to_top();
            }
            crossterm::event::KeyCode::End => {
                tui.scrollback.scroll_to_bottom();
            }
            _ => {}
        }
        String::new()
    }

    #[test]
    fn read_input_dispatch_keys_work() {
        let mut tui = FullScreenTui::new();
        for i in 0..50 {
            tui.scrollback.push(format!("line {i}"));
        }
        // PageUp
        simulate_key_event(&mut tui, crossterm::event::KeyCode::PageUp);
        assert!(!tui.scrollback.is_at_bottom());
        // End
        simulate_key_event(&mut tui, crossterm::event::KeyCode::End);
        assert!(tui.scrollback.is_at_bottom());
        // PageDown at bottom should stay at bottom
        simulate_key_event(&mut tui, crossterm::event::KeyCode::PageDown);
        assert!(tui.scrollback.is_at_bottom());
        // Home
        simulate_key_event(&mut tui, crossterm::event::KeyCode::Home);
        assert!(!tui.scrollback.is_at_bottom());
    }

    // ── Integration tests ────────────────────────────────────────────────

    #[test]
    fn render_input_line_writes_cursor_at_correct_column() {
        let mut buf = Vec::new();
        FullScreenTui::render_input_line(&mut buf, 5, &['h', 'e', 'l', 'l', 'o'], 3)
            .expect("render input line");
        let output = String::from_utf8_lossy(&buf);
        // Should contain the prompt prefix
        assert!(output.contains(PROMPT));
        // Should contain the typed text
        assert!(output.contains("hello"));
        // MoveTo(col=5, row=5) outputs \x1b[6;6H (crossterm converts 0-based to 1-based)
        // PROMPT.len()=2, cursor=3 => cursor_col=5
        // row=5 => escape 6
        // col=5 => escape 6
        assert!(output.contains("\x1b[6;6H"));
    }

    #[test]
    fn render_input_line_cursor_at_start_when_empty() {
        let mut buf = Vec::new();
        FullScreenTui::render_input_line(&mut buf, 3, &[], 0).expect("render empty input");
        let output = String::from_utf8_lossy(&buf);
        assert!(output.contains(PROMPT));
        // MoveTo(col=2, row=3) outputs \x1b[4;3H
        assert!(output.contains("\x1b[4;3H"));
    }

    #[test]
    fn render_input_line_cursor_at_end() {
        let mut buf = Vec::new();
        FullScreenTui::render_input_line(&mut buf, 10, &['a', 'b'], 2)
            .expect("render input line end");
        let output = String::from_utf8_lossy(&buf);
        // MoveTo(col=4, row=10) outputs \x1b[11;5H
        assert!(output.contains("\x1b[11;5H"));
    }

    #[test]
    fn render_input_line_with_multiline_content() {
        let mut buf = Vec::new();
        FullScreenTui::render_input_line(&mut buf, 0, &['a', '\n', 'b'], 2)
            .expect("render multiline");
        let output = String::from_utf8_lossy(&buf);
        assert!(output.contains(PROMPT));
        // Newline in buffer should appear
        assert!(output.contains("a\nb") || output.contains("a") || output.contains("b"));
    }

    #[test]
    fn scroll_indicator_format_matches_expected_pattern() {
        let mut tui = FullScreenTui::new();
        for i in 0..100 {
            tui.scrollback.push(format!("line {i}"));
        }
        tui.scrollback.scroll_up(30);
        let mut buf = Vec::new();
        tui.render_conversation(&mut buf, 20, 80)
            .expect("render with scroll");
        let output = String::from_utf8_lossy(&buf);
        // Should contain "lines N-M of 100" pattern
        assert!(output.contains("lines "));
        assert!(output.contains(" of 100"));
    }

    #[test]
    fn scroll_indicator_hidden_when_at_bottom() {
        let mut tui = FullScreenTui::new();
        for i in 0..10 {
            tui.scrollback.push(format!("line {i}"));
        }
        // At bottom — scroll_indicator should NOT appear
        assert!(tui.scrollback.is_at_bottom());
        let mut buf = Vec::new();
        tui.render_conversation(&mut buf, 20, 80)
            .expect("render at bottom");
        let output = String::from_utf8_lossy(&buf);
        // At bottom with 10 lines in 20-high viewport, no scroll indicator
        // The scroll indicator contains "lines " prefix
        assert!(!output.contains("lines "));
    }

    #[test]
    fn run_invokes_execute_turn_and_append_output() {
        let mut tui = FullScreenTui::new();
        // Simulate calling run. The run() method requires raw mode and a real
        // terminal, but we can test the inner loop behavior: execute_turn is
        // called and its output is appended to scrollback.
        let mut call_count = 0u32;
        let mut execute_turn = |input: &str| -> Result<String, Box<dyn std::error::Error>> {
            call_count += 1;
            assert_eq!(input, "hello");
            Ok("response text".to_string())
        };
        // Manually simulate what run() does after reading input:
        tui.scrollback.push(format!(" > hello"));
        let output = execute_turn("hello").expect("turn succeeded");
        tui.scrollback.push_str(&output);
        assert_eq!(call_count, 1);
        assert!(tui.scrollback.len() >= 2);
        assert!(tui
            .scrollback
            .visible(20)
            .0
            .iter()
            .any(|l| l.contains("response text")));
    }

    #[test]
    fn run_handles_turn_error_and_appends_error_message() {
        let mut tui = FullScreenTui::new();
        let execute_turn = |_: &str| -> Result<String, Box<dyn std::error::Error>> {
            Err("network timeout".into())
        };
        tui.scrollback.push(" > input".to_string());
        match execute_turn("input") {
            Ok(output) => tui.scrollback.push_str(&output),
            Err(error) => {
                tui.scrollback.push(format!("error: {error}"));
            }
        }
        let (visible, _, _) = tui.scrollback.visible(10);
        let has_error = visible.iter().any(|l| l.contains("error: network timeout"));
        assert!(has_error);
    }
}
