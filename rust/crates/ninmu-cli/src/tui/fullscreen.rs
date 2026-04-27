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
    fn read_input(
        &mut self,
        stdout: &mut impl Write,
        prompt_row: u16,
        conv_height: u16,
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
                        let col = PROMPT.len() + buffer.len() - 1;
                        queue!(
                            stdout,
                            MoveTo(col as u16, prompt_row),
                            Print(c.to_string()),
                        )?;
                        stdout.flush()?;
                    }
                    crossterm::event::KeyCode::Backspace => {
                        if !buffer.is_empty() {
                            buffer.pop();
                            let col = PROMPT.len() + buffer.len();
                            queue!(stdout, MoveTo(col as u16, prompt_row), Print(" "))?;
                            stdout.flush()?;
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
                    crossterm::event::KeyCode::Home => {
                        self.scrollback.scroll_to_top();
                        return Ok(String::new());
                    }
                    crossterm::event::KeyCode::End => {
                        self.scrollback.scroll_to_bottom();
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
}
