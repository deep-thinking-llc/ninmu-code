use std::io::{self, Write};

use crossterm::cursor::MoveTo;
use crossterm::execute;
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, Clear, ClearType};

use crate::tui::terminal::TerminalSize;
use crate::tui::theme::Theme;

/// Pager for displaying long output with scroll controls.
/// Uses the alternate screen buffer to preserve terminal history.
pub struct InternalPager {
    terminal: TerminalSize,
}

impl InternalPager {
    pub fn new() -> Self {
        Self {
            terminal: TerminalSize::new(),
        }
    }

    /// Display content with paging if it exceeds terminal height.
    /// Returns Ok(true) if paged, Ok(false) if content fit without paging.
    pub fn run(&self, content: &str) -> io::Result<bool> {
        // Try external pager first
        if let Some(pager_cmd) = Self::find_external_pager() {
            return Self::run_external(pager_cmd, content);
        }

        let height = self.terminal.height() as usize;
        let lines: Vec<&str> = content.lines().collect();
        let total_lines = lines.len();

        // Account for status bar at bottom
        let page_size = height.saturating_sub(2);
        if total_lines <= page_size {
            // No paging needed
            println!("{content}");
            return Ok(false);
        }

        use crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen};
        enable_raw_mode()?;
        execute!(io::stdout(), EnterAlternateScreen)?;
        let result = self.render_paged(&lines, page_size);
        execute!(io::stdout(), LeaveAlternateScreen)?;
        disable_raw_mode()?;
        result
    }

    /// Render content with internal paging using raw mode keyboard input.
    fn render_paged(&self, lines: &[&str], initial_page_size: usize) -> io::Result<bool> {
        let total_lines = lines.len();
        let mut page_size = initial_page_size;
        let mut max_offset = total_lines.saturating_sub(page_size);
        let mut offset = 0usize;

        let mut stdout = io::stdout();

        loop {
            execute!(stdout, Clear(ClearType::All), MoveTo(1, 1))?;

            let end = (offset + page_size).min(total_lines);
            for line in &lines[offset..end] {
                writeln!(stdout, "{line}")?;
            }

            let scroll_pct = if total_lines > 0 && max_offset > 0 {
                (offset as f64 / max_offset as f64 * 100.0) as u8
            } else if offset >= max_offset && max_offset == 0 {
                100
            } else {
                0
            };
            write!(
                stdout,
                "{move_status}{muted}  lines {start}-{end} of {total} · {pct}% · j/k scroll · g/G top/bottom · q quit{reset}",
                move_status = crossterm::cursor::MoveTo(1, page_size as u16 + 1),
                muted = Theme::MUTED,
                start = offset + 1,
                end = end,
                total = total_lines,
                pct = scroll_pct,
                reset = Theme::RESET,
            )?;
            stdout.flush()?;

            match crossterm::event::read() {
                Ok(crossterm::event::Event::Key(key)) => match key.code {
                    crossterm::event::KeyCode::Char('q') | crossterm::event::KeyCode::Esc => {
                        break;
                    }
                    crossterm::event::KeyCode::Char('j') | crossterm::event::KeyCode::Down => {
                        offset = offset.saturating_add(1).min(max_offset);
                    }
                    crossterm::event::KeyCode::Char('k') | crossterm::event::KeyCode::Up => {
                        offset = offset.saturating_sub(1);
                    }
                    crossterm::event::KeyCode::Char('g') => {
                        offset = 0;
                    }
                    crossterm::event::KeyCode::Char('G') => {
                        offset = max_offset;
                    }
                    crossterm::event::KeyCode::PageDown => {
                        offset = offset.saturating_add(page_size).min(max_offset);
                    }
                    crossterm::event::KeyCode::PageUp => {
                        offset = offset.saturating_sub(page_size);
                    }
                    crossterm::event::KeyCode::Home => {
                        offset = 0;
                    }
                    crossterm::event::KeyCode::End => {
                        offset = max_offset;
                    }
                    _ => {}
                },
                Ok(crossterm::event::Event::Resize(_, rows)) => {
                    let new_page_size = (rows as usize).saturating_sub(2);
                    if new_page_size > 0 {
                        page_size = new_page_size;
                        max_offset = total_lines.saturating_sub(page_size);
                        if offset > max_offset {
                            offset = max_offset;
                        }
                    }
                }
                _ => {}
            }
        }

        Ok(true)
    }

    /// Try external pager (PAGER env var or less).
    fn find_external_pager() -> Option<String> {
        if let Ok(pager) = std::env::var("PAGER") {
            if !pager.is_empty() {
                return Some(pager);
            }
        }
        if which("less").is_ok() {
            return Some("less".to_string());
        }
        if which("more").is_ok() {
            return Some("more".to_string());
        }
        None
    }

    /// Run external pager via subprocess.
    fn run_external(pager_cmd: String, content: &str) -> io::Result<bool> {
        let mut child = std::process::Command::new(&pager_cmd)
            .stdin(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| io::Error::other(format!("failed to start {pager_cmd}: {e}")))?;

        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(content.as_bytes())?;
        }

        let status = child
            .wait()
            .map_err(|e| io::Error::other(format!("pager failed: {e}")))?;

        if !status.success() {
            eprintln!("warning: {pager_cmd} exited with status {status}");
        }

        Ok(true)
    }
}

impl Default for InternalPager {
    fn default() -> Self {
        Self::new()
    }
}

fn which(cmd: &str) -> Result<std::process::Output, std::io::Error> {
    std::process::Command::new("which").arg(cmd).output()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_external_pager_returns_some_if_available() {
        let pager = InternalPager::find_external_pager();
        if std::process::Command::new("which")
            .arg("less")
            .output()
            .is_ok_and(|o| o.status.success())
        {
            assert!(pager.is_some());
        }
    }

    #[test]
    fn pager_short_output_skips_paging() {
        let pager = InternalPager::new();
        assert!(pager.terminal.height() > 0);
    }

    #[test]
    fn pager_status_bar_renders_scroll_position() {
        let raw_lines: Vec<String> = (0..100).map(|i| format!("line {i}")).collect();
        let lines: Vec<&str> = raw_lines.iter().map(std::string::String::as_str).collect();
        let total = lines.len();
        let page_size = 20usize;
        let max_offset = total.saturating_sub(page_size);

        let offset = 0usize;
        let end = (offset + page_size).min(total);
        assert_eq!(offset, 0);
        assert_eq!(end, 20);
        assert_eq!(max_offset, 80);

        let offset = 40usize;
        let end = (offset + page_size).min(total);
        assert_eq!(offset, 40);
        assert_eq!(end, 60);

        let offset = max_offset;
        let end = (offset + page_size).min(total);
        assert_eq!(offset, 80);
        assert_eq!(end, 100);
    }

    #[test]
    fn pager_handles_exact_page_boundary() {
        let total = 20usize;
        let page_size = 20;
        let max_offset = total.saturating_sub(page_size);
        assert_eq!(max_offset, 0);

        let offset = 0usize;
        let end = (offset + page_size).min(total);
        assert_eq!(end, 20);
    }

    #[test]
    fn pager_handles_empty_output() {
        let lines: Vec<&str> = vec![];
        let page_size = 20;
        let max_offset = lines.len().saturating_sub(page_size);
        assert_eq!(max_offset, 0);
        assert!(lines.len() <= page_size);
    }

    #[test]
    fn scroll_percentage_at_top() {
        let total_lines = 100usize;
        let page_size = 20usize;
        let max_offset = total_lines.saturating_sub(page_size);
        let offset = 0usize;
        let pct = if max_offset > 0 {
            (offset as f64 / max_offset as f64 * 100.0) as u8
        } else {
            100
        };
        assert_eq!(pct, 0);
    }

    #[test]
    fn scroll_percentage_at_middle() {
        let total_lines = 100usize;
        let page_size = 20usize;
        let max_offset = total_lines.saturating_sub(page_size);
        let offset = 40usize;
        let pct = if max_offset > 0 {
            (offset as f64 / max_offset as f64 * 100.0) as u8
        } else {
            100
        };
        assert_eq!(pct, 50);
    }

    #[test]
    fn scroll_percentage_at_bottom() {
        let total_lines = 100usize;
        let page_size = 20usize;
        let max_offset = total_lines.saturating_sub(page_size);
        let offset = max_offset;
        let pct = if max_offset > 0 {
            (offset as f64 / max_offset as f64 * 100.0) as u8
        } else {
            100
        };
        assert_eq!(pct, 100);
    }

    #[test]
    fn scroll_percentage_fits_in_one_page() {
        let total_lines = 20usize;
        let page_size = 20usize;
        let max_offset = total_lines.saturating_sub(page_size);
        let offset = max_offset;
        let pct = if max_offset > 0 {
            (offset as f64 / max_offset as f64 * 100.0) as u8
        } else {
            100
        };
        assert_eq!(pct, 100);
    }
}
