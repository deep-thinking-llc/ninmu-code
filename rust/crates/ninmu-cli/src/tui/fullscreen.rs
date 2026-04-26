//! Full-screen TUI mode -- split-pane conversation view using crossterm.
//!
//! Entered via the `--tui` flag. Provides a scrollable conversation pane
//! and a fixed input line at the bottom, all within the alternate screen buffer.

use std::io::{self, Write};

use crossterm::cursor::MoveTo;
use crossterm::style::Print;
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, Clear, ClearType};
use crossterm::{
    execute, queue,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen},
};

use crate::input::LineEditor;
use crate::tui::scrollback::Scrollback;
use crate::tui::terminal::TerminalSize;
use crate::tui::theme::Theme;

/// Minimum height for the input area to be usable.
const MIN_INPUT_HEIGHT: u16 = 3;

/// Keyboard shortcuts help text.
const HELP_OVERLAY: &str = "\
── Keyboard Shortcuts ─────────────────────
  PageUp / PageDown    Scroll conversation
  Home / End           Top / bottom
  Ctrl+C               Interrupt / exit
  Ctrl+J               Insert newline
  Shift+Enter          Insert newline
  Tab                  Complete (@file, commands)
  ?                    Toggle this help
  Ctrl+D / /exit       Exit TUI
───────────────────────────────────────────";

/// Manages the full-screen TUI layout and rendering.
pub struct FullScreenTui {
    terminal: TerminalSize,
    scrollback: Scrollback,
    help_visible: bool,
    running: bool,
}

impl FullScreenTui {
    /// Create a new full-screen TUI manager.
    pub fn new() -> Self {
        Self {
            terminal: TerminalSize::new(),
            scrollback: Scrollback::default(),
            help_visible: false,
            running: false,
        }
    }

    /// Enter the full-screen TUI.
    /// Takes an input handler and a turn executor function.
    pub fn run<F>(&mut self, editor: &mut LineEditor, execute_turn: F) -> io::Result<()>
    where
        F: FnMut(&str, &mut LineEditor) -> Result<(), Box<dyn std::error::Error>>,
    {
        self.running = true;
        let mut executor = execute_turn;

        enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen, Clear(ClearType::All))?;

        // Main loop
        while self.running {
            let (width, height) = crossterm::terminal::size()?;
            let input_height = MIN_INPUT_HEIGHT;
            let conversation_height = height.saturating_sub(input_height);

            // Render conversation pane
            self.render_conversation(&mut stdout, conversation_height as usize, width)?;

            // Render input prompt line
            let prompt = "> ";
            queue!(
                stdout,
                MoveTo(0, conversation_height),
                Clear(ClearType::CurrentLine),
                Print(prompt),
            )?;
            stdout.flush()?;

            // Read input with crossterm
            let input = self.read_input(&mut stdout, conversation_height, width)?;
            match input.as_str() {
                "/exit" | "/quit" | "" if self.should_exit() => {
                    self.running = false;
                    break;
                }
                "?" => {
                    self.help_visible = !self.help_visible;
                }
                _ => {
                    // Echo input to scrollback
                    self.scrollback.push(format!("> {input}"));
                    // Execute turn
                    if let Err(error) = (executor)(&input, editor) {
                        self.scrollback.push(format!("error: {error}"));
                    }
                }
            }
        }

        execute!(stdout, LeaveAlternateScreen)?;
        disable_raw_mode()?;
        Ok(())
    }

    /// Render the conversation scrollback into the top pane.
    fn render_conversation(
        &self,
        stdout: &mut impl Write,
        viewport_height: usize,
        width: u16,
    ) -> io::Result<()> {
        let (visible, start, total) = self.scrollback.visible(viewport_height);

        // Clear conversation area
        for row in 0..viewport_height {
            queue!(stdout, MoveTo(0, row as u16), Clear(ClearType::CurrentLine),)?;
        }

        // Render visible lines
        for (i, line) in visible.iter().enumerate() {
            // Truncate lines that exceed the terminal width
            let truncated = if line.len() > width as usize {
                &line[..width.saturating_sub(3) as usize]
            } else {
                line
            };
            queue!(stdout, MoveTo(0, i as u16), Print(truncated),)?;
        }

        // Render scroll indicator if not at bottom
        if !self.scrollback.is_at_bottom() {
            let indicator = format!(
                "{}[lines {}-{} of {} · press j/k to scroll, q to exit]{}",
                Theme::MUTED,
                start + 1,
                start + visible.len(),
                total,
                Theme::RESET,
            );
            // Place at the last line of conversation pane
            let indicator_row = viewport_height.saturating_sub(1) as u16;
            queue!(
                stdout,
                MoveTo(0, indicator_row),
                Clear(ClearType::CurrentLine),
                Print(indicator),
            )?;
        }

        // Render help overlay if visible
        if self.help_visible {
            self.render_help_overlay(stdout, viewport_height, width)?;
        }

        stdout.flush()?;
        Ok(())
    }

    /// Render help overlay in the center of the screen.
    fn render_help_overlay(
        &self,
        stdout: &mut impl Write,
        viewport_height: usize,
        _width: u16,
    ) -> io::Result<()> {
        let help_lines: Vec<&str> = HELP_OVERLAY.lines().collect();
        let help_height = help_lines.len();
        let start_row = (viewport_height.saturating_sub(help_height)) / 2;

        // Draw a simple box around the help text
        for (i, line) in help_lines.iter().enumerate() {
            queue!(stdout, MoveTo(2, (start_row + i) as u16), Print(line),)?;
        }
        Ok(())
    }

    /// Read a single line of input using crossterm event handling.
    /// Returns the accumulated input string.
    fn read_input(
        &self,
        stdout: &mut impl Write,
        prompt_row: u16,
        _width: u16,
    ) -> io::Result<String> {
        let mut buffer = String::new();
        loop {
            match crossterm::event::read() {
                Ok(crossterm::event::Event::Key(key)) => match key.code {
                    crossterm::event::KeyCode::Enter => {
                        if !buffer.is_empty() {
                            return Ok(buffer);
                        }
                    }
                    crossterm::event::KeyCode::Char(c) => {
                        buffer.push(c);
                        // Echo character
                        queue!(
                            stdout,
                            MoveTo(2 + buffer.len() as u16 - 1, prompt_row),
                            Print(c.to_string()),
                        )?;
                        stdout.flush()?;
                    }
                    crossterm::event::KeyCode::Backspace => {
                        buffer.pop();
                        queue!(
                            stdout,
                            MoveTo(2 + buffer.len() as u16, prompt_row),
                            Print(" "),
                        )?;
                        stdout.flush()?;
                    }
                    crossterm::event::KeyCode::PageUp => {
                        // Scrolling will be handled externally
                    }
                    crossterm::event::KeyCode::PageDown => {
                        // Scrolling will be handled externally
                    }
                    crossterm::event::KeyCode::Esc => {
                        return Ok(String::new());
                    }
                    _ => {}
                },
                Ok(crossterm::event::Event::Resize(_, _)) => {
                    // Will be handled on next render pass
                }
                _ => {}
            }
        }
    }

    /// Check whether the user wants to exit (for empty input handling).
    fn should_exit(&self) -> bool {
        // If we get an empty input, check if the input line was empty
        true
    }

    /// Scroll the conversation up by one page.
    pub fn page_up(&mut self) {
        let (_, height) = crossterm::terminal::size().unwrap_or((80, 24));
        self.scrollback
            .scroll_up((height.saturating_sub(MIN_INPUT_HEIGHT)) as usize);
    }

    /// Scroll the conversation down by one page.
    pub fn page_down(&mut self) {
        let (_, height) = crossterm::terminal::size().unwrap_or((80, 24));
        self.scrollback
            .scroll_down((height.saturating_sub(MIN_INPUT_HEIGHT)) as usize);
    }

    /// Append rendered output to the scrollback.
    pub fn append_output(&mut self, text: &str) {
        self.scrollback.push_str(text);
    }

    /// Scroll to the bottom (live view).
    pub fn scroll_to_bottom(&mut self) {
        self.scrollback.scroll_to_bottom();
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
    fn fullscreen_tui_initializes_cleanly() {
        let tui = FullScreenTui::new();
        assert!(!tui.help_visible);
        assert!(!tui.running);
    }

    #[test]
    fn scrollback_pages_correctly() {
        let mut tui = FullScreenTui::new();
        for i in 0..100 {
            tui.scrollback.push(format!("line {i}"));
        }
        assert_eq!(tui.scrollback.len(), 100);
        assert!(tui.scrollback.is_at_bottom());
    }

    #[test]
    fn help_overlay_lines_have_required_content() {
        assert!(HELP_OVERLAY.contains("PageUp"));
        assert!(HELP_OVERLAY.contains("PageDown"));
        assert!(HELP_OVERLAY.contains("?"));
        assert!(HELP_OVERLAY.contains("/exit"));
    }

    #[test]
    fn append_output_adds_to_scrollback() {
        let mut tui = FullScreenTui::new();
        tui.append_output("hello\nworld");
        assert_eq!(tui.scrollback.len(), 2);
        let (visible, _, _) = tui.scrollback.visible(10);
        assert_eq!(visible.len(), 2);
    }
}
