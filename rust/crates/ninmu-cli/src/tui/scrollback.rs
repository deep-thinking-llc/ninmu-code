/// A ring buffer for conversation scrollback.
///
/// Stores rendered lines with scroll offset tracking.
/// Used by the full-screen TUI mode to provide scrollable
/// conversation history.
pub struct Scrollback {
    /// Rendered lines in display order (newest appended).
    lines: Vec<String>,
    /// Maximum number of lines to retain.
    max_lines: usize,
    /// Current scroll offset (0 = bottom/newest, N = N lines up).
    scroll_offset: usize,
}

impl Scrollback {
    /// Create a new scrollback buffer with the given capacity.
    pub fn new(max_lines: usize) -> Self {
        Self {
            lines: Vec::with_capacity(max_lines.min(1024)),
            max_lines,
            scroll_offset: 0,
        }
    }

    /// Append a line to the buffer. Evicts oldest lines if over capacity.
    pub fn push(&mut self, line: String) {
        if self.lines.len() >= self.max_lines {
            self.lines.remove(0);
        }
        self.lines.push(line);
    }

    /// Push multiple lines at once (typically from a multi-line string).
    pub fn push_str(&mut self, text: &str) {
        for line in text.lines() {
            self.push(line.to_string());
        }
        // If the text doesn't end with a newline we still capture it
        if text.is_empty() {
            return;
        }
        let last_byte = text.as_bytes().last().copied().unwrap_or(b'\n');
        if last_byte == b'\n' {
            // Keep an empty line for trailing newlines
            self.push(String::new());
        }
    }

    /// Total lines stored.
    pub fn len(&self) -> usize {
        self.lines.len()
    }

    /// Whether the buffer is empty.
    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    /// Scroll up by `n` lines (toward older content). Returns the new offset.
    pub fn scroll_up(&mut self, n: usize) -> usize {
        self.scroll_offset = (self.scroll_offset + n).min(self.max_scroll());
        self.scroll_offset
    }

    /// Scroll down by `n` lines (toward newer content). Returns the new offset.
    pub fn scroll_down(&mut self, n: usize) -> usize {
        self.scroll_offset = self.scroll_offset.saturating_sub(n);
        self.scroll_offset
    }

    /// Scroll to the top (oldest content).
    pub fn scroll_to_top(&mut self) {
        self.scroll_offset = self.max_scroll();
    }

    /// Scroll to the bottom (newest content, i.e., live view).
    pub fn scroll_to_bottom(&mut self) {
        self.scroll_offset = 0;
    }

    /// Current scroll offset from bottom (0 = bottom).
    pub fn scroll_offset(&self) -> usize {
        self.scroll_offset
    }

    /// Whether the view is at the bottom (live, following newest content).
    pub fn is_at_bottom(&self) -> bool {
        self.scroll_offset == 0
    }

    /// Maximum scroll offset.
    fn max_scroll(&self) -> usize {
        self.lines.len().saturating_sub(1)
    }

    /// Get a slice of lines for the visible portion, given a viewport height.
    /// Returns (visible_lines, start_index_in_buffer, total_lines).
    pub fn visible(&self, viewport_height: usize) -> (&[String], usize, usize) {
        if self.lines.is_empty() {
            return (&[], 0, 0);
        }

        let total = self.lines.len();
        let scroll = self.scroll_offset.min(self.max_scroll());
        let end = total.saturating_sub(scroll);
        let start = end.saturating_sub(viewport_height);

        (&self.lines[start..end], start, total)
    }
}

impl Default for Scrollback {
    fn default() -> Self {
        Self::new(10_000)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_scrollback() {
        let sb = Scrollback::new(100);
        assert!(sb.is_empty());
        assert_eq!(sb.len(), 0);
        assert!(sb.is_at_bottom());
    }

    #[test]
    fn push_and_retrieve() {
        let mut sb = Scrollback::new(100);
        sb.push("line 1".to_string());
        sb.push("line 2".to_string());
        assert_eq!(sb.len(), 2);
        assert!(!sb.is_empty());
    }

    #[test]
    fn push_str_splits_lines() {
        let mut sb = Scrollback::new(100);
        sb.push_str("line 1\nline 2\n");
        assert_eq!(sb.len(), 3); // includes trailing empty line
    }

    #[test]
    fn scroll_offset_tracking() {
        let mut sb = Scrollback::new(100);
        for i in 0..50 {
            sb.push(format!("line {i}"));
        }
        assert!(sb.is_at_bottom());
        assert_eq!(sb.scroll_offset(), 0);
        sb.scroll_up(5);
        assert_eq!(sb.scroll_offset(), 5);
        assert!(!sb.is_at_bottom());
        sb.scroll_down(2);
        assert_eq!(sb.scroll_offset(), 3);
        sb.scroll_to_bottom();
        assert_eq!(sb.scroll_offset(), 0);
        sb.scroll_to_top();
        assert_eq!(sb.scroll_offset(), sb.max_scroll());
    }

    #[test]
    fn visible_returns_correct_subset() {
        let mut sb = Scrollback::new(100);
        for i in 0..50 {
            sb.push(format!("line {i}"));
        }
        let (visible, start, total) = sb.visible(10);
        assert_eq!(visible.len(), 10);
        assert_eq!(total, 50);
        // Most recent 10 lines (bottom)
        assert!(visible[0].starts_with("line 4"));
        assert!(visible[9].starts_with("line 49"));

        // Scroll up 5
        sb.scroll_up(5);
        let (visible, start, _) = sb.visible(10);
        assert_eq!(start, 35);
        assert!(visible[0].starts_with("line 35"));
    }

    #[test]
    fn max_limit_eviction() {
        let mut sb = Scrollback::new(5);
        for i in 0..10 {
            sb.push(format!("line {i}"));
        }
        assert_eq!(sb.len(), 5);
        assert_eq!(
            sb.visible(10)
                .0
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>(),
            vec!["line 5", "line 6", "line 7", "line 8", "line 9"]
        );
    }

    #[test]
    fn scroll_boundaries() {
        let mut sb = Scrollback::new(100);
        for i in 0..20 {
            sb.push(format!("line {i}"));
        }
        // Scroll up beyond max
        sb.scroll_up(100);
        assert_eq!(sb.scroll_offset(), sb.max_scroll());
        // Scroll down beyond 0
        sb.scroll_down(100);
        assert_eq!(sb.scroll_offset(), 0);
    }

    #[test]
    fn visible_with_smaller_than_viewport() {
        let mut sb = Scrollback::new(100);
        sb.push("only line".to_string());
        let (visible, _, total) = sb.visible(10);
        assert_eq!(visible.len(), 1);
        assert_eq!(total, 1);
    }

    #[test]
    fn push_str_without_trailing_newline() {
        let mut sb = Scrollback::new(100);
        sb.push_str("hello\nworld");
        assert_eq!(sb.len(), 2);
        let (visible, _, _) = sb.visible(10);
        assert_eq!(visible[0], "hello");
        assert_eq!(visible[1], "world");
    }

    #[test]
    fn push_str_empty_does_nothing() {
        let mut sb = Scrollback::new(100);
        sb.push_str("");
        assert_eq!(sb.len(), 0);
    }

    #[test]
    fn push_str_with_only_newline() {
        let mut sb = Scrollback::new(100);
        sb.push_str("\n");
        assert_eq!(sb.len(), 2);
    }

    #[test]
    fn scroll_offset_clamped_after_eviction() {
        let mut sb = Scrollback::new(5);
        for i in 0..5 {
            sb.push(format!("line {i}"));
        }
        sb.scroll_up(3);
        assert_eq!(sb.scroll_offset(), 3);
        for i in 5..10 {
            sb.push(format!("line {i}"));
        }
        // After eviction: buffer = [5,6,7,8,9] (5 lines), max_scroll=4
        // offset=3 is within bounds, stays
        assert_eq!(sb.scroll_offset(), 3);
    }
}
