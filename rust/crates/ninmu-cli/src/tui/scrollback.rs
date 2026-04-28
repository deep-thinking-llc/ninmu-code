/// A ring buffer for conversation scrollback.
///
/// Stores rendered lines with scroll offset tracking.
/// Used by the full-screen TUI mode to provide scrollable
/// conversation history.
///
/// Supports collapsible entries: a single logical entry (e.g. long tool output)
/// can be stored in collapsed or expanded form, and toggled interactively.
/// Collapsible entries are tracked by their starting line index.
pub struct Scrollback {
    /// Rendered lines in display order (newest appended).
    lines: Vec<String>,
    /// Maximum number of lines to retain.
    max_lines: usize,
    /// Current scroll offset (0 = bottom/newest, N = N lines up).
    scroll_offset: usize,
    /// Entries that can be collapsed/expanded, keyed by starting line index.
    /// Stores (`line_index`, `full_content_lines`, `collapsed_summary_lines`, `is_expanded`).
    collapsible_entries: Vec<(usize, Vec<String>, Vec<String>, bool)>,
}

impl Scrollback {
    /// Create a new scrollback buffer with the given capacity.
    pub fn new(max_lines: usize) -> Self {
        Self {
            lines: Vec::with_capacity(max_lines.min(1024)),
            max_lines,
            scroll_offset: 0,
            collapsible_entries: Vec::new(),
        }
    }

    /// Append a line to the buffer. Evicts oldest lines if over capacity.
    pub fn push(&mut self, line: String) {
        if self.lines.len() >= self.max_lines {
            // Batch-evict the oldest 25 % of lines to amortise the O(n)
            // shift cost.  Removing one element at a time from the front
            // of a Vec is O(n) per push; draining a chunk once every
            // N pushes is O(n) amortised over those N pushes.
            let evict = (self.max_lines / 4).max(1);
            self.lines.drain(..evict);
            // Adjust scroll offset so the view doesn't jump.
            self.scroll_offset = self.scroll_offset.saturating_sub(evict);
            // Adjust collapsible entry indices.
            for entry in &mut self.collapsible_entries {
                if entry.0 >= evict {
                    entry.0 -= evict;
                } else {
                    entry.0 = 0;
                }
            }
            // Remove entries whose content was fully evicted.
            self.collapsible_entries
                .retain(|(start, full, _, _)| start.saturating_add(full.len()) > 0);
        }
        self.lines.push(line);
    }

    /// Remove and return the last line, if any.
    pub fn pop(&mut self) -> Option<String> {
        self.lines.pop()
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

    /// Push a collapsible entry: full content is stored but only the first
    /// `visible_lines` are rendered initially. Pressing Tab toggles expand/collapse.
    /// Returns the number of lines pushed.
    pub fn push_collapsible(&mut self, full_lines: &[String], visible_lines: usize) -> usize {
        let start_idx = self.lines.len();
        let collapsed_count = visible_lines.min(full_lines.len());
        let has_hint = full_lines.len() > collapsed_count;
        let collapsed_lines: Vec<String> =
            full_lines.iter().take(collapsed_count).cloned().collect();

        // Push the collapsed (or full) lines
        for line in &collapsed_lines {
            self.push(line.clone());
        }
        let display_lines = if has_hint {
            let extra = full_lines.len() - collapsed_count;
            let hint = format!("[+] [Tab to expand · {extra} more lines]");
            self.push(hint);
            let mut with_hint = collapsed_lines;
            with_hint.push(format!("[+] [Tab to expand · {extra} more lines]"));
            with_hint
        } else {
            collapsed_lines
        };

        self.collapsible_entries
            .push((start_idx, full_lines.to_vec(), display_lines, false));

        full_lines.len()
    }

    /// Toggle the expansion state of the collapsible entry at the given cursor
    /// position (line index). Returns true if a toggle occurred.
    pub fn toggle_expand_at(&mut self, line_index: usize) -> bool {
        // Find the entry that contains this line
        // Entries are sorted by start_idx; find the last one whose start_idx <= line_index
        let entry_pos = self
            .collapsible_entries
            .iter()
            .rposition(|(start, full, _, _)| {
                let end = start + full.len();
                *start <= line_index && line_index < end
            });

        let Some(ep) = entry_pos else {
            return false;
        };

        let (start, full, collapsed, is_expanded) = self.collapsible_entries.remove(ep);
        let now_expanded = !is_expanded;

        // Calculate the height delta
        let old_count = if is_expanded {
            full.len()
        } else {
            collapsed.len()
        };
        let new_count = if now_expanded {
            full.len()
        } else {
            collapsed.len()
        };

        // Replace lines in the buffer — use Vec::splice for O(n) batch replace
        let replacement: Vec<String> = if now_expanded {
            full.clone()
        } else {
            collapsed.clone()
        };
        let _ = self.lines.splice(start..start + old_count, replacement);

        // Update start indices for all subsequent entries
        let height_diff = new_count as isize - old_count as isize;
        for entry in &mut self.collapsible_entries {
            if entry.0 > start {
                entry.0 = (entry.0 as isize + height_diff) as usize;
            }
        }

        // Re-insert the updated entry
        self.collapsible_entries
            .push((start, full, collapsed, now_expanded));

        // Sort by start_idx (insert order may have shifted)
        self.collapsible_entries.sort_by_key(|e| e.0);

        // Adjust scroll offset if needed
        if height_diff < 0 {
            // Content got shorter; clamp offset
            let max = self.max_scroll();
            if self.scroll_offset > max {
                self.scroll_offset = max;
            }
        }

        true
    }

    /// Whether the given line index is the start of a collapsible entry.
    pub fn is_collapsible_start(&self, line_index: usize) -> bool {
        self.collapsible_entries
            .iter()
            .any(|(start, _, _, _)| *start == line_index)
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
    /// Returns (`visible_lines`, `start_index_in_buffer`, `total_lines`).
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
                .map(std::string::String::as_str)
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

    // ── Collapsible entry tests ──────────────────────────────────────────

    #[test]
    fn push_collapsible_short_fits_without_hint() {
        let mut sb = Scrollback::new(100);
        let lines: Vec<String> = vec!["a".to_string(), "b".to_string()];
        let count = sb.push_collapsible(&lines, 10);
        assert_eq!(count, 2);
        assert_eq!(sb.len(), 2);
        // Should not contain "[+]" since it fits
        let (visible, _, _) = sb.visible(10);
        let joined = visible.join("\n");
        assert!(!joined.contains("[Tab"));
    }

    #[test]
    fn push_collapsible_long_shows_hint() {
        let mut sb = Scrollback::new(100);
        let lines: Vec<String> = (1..=20).map(|i| format!("line {i}")).collect();
        let count = sb.push_collapsible(&lines, 5);
        assert_eq!(count, 20);
        // 5 visible lines + 1 hint line
        assert_eq!(sb.len(), 6);
        let (visible, _, _) = sb.visible(20);
        let joined = visible.join("\n");
        assert!(joined.contains("[Tab to expand"));
        assert!(joined.contains("15 more lines"));
    }

    #[test]
    fn toggle_expand_reveals_full_content() {
        let mut sb = Scrollback::new(100);
        let lines: Vec<String> = (1..=10).map(|i| format!("line {i}")).collect();
        sb.push_collapsible(&lines, 3);
        // Initially collapsed: 3 lines + hint
        assert_eq!(sb.len(), 4);

        // Toggle at line 0 (start of the entry)
        let toggled = sb.toggle_expand_at(0);
        assert!(toggled);
        // Now expanded: all 10 lines
        assert_eq!(sb.len(), 10);
        let (visible, _, _) = sb.visible(20);
        assert!(visible[0].contains("line 1"));
        assert!(visible[9].contains("line 10"));
    }

    #[test]
    fn toggle_collapse_restores_collapsed_view() {
        let mut sb = Scrollback::new(100);
        let lines: Vec<String> = (1..=10).map(|i| format!("line {i}")).collect();
        sb.push_collapsible(&lines, 3);
        // Expand
        sb.toggle_expand_at(0);
        assert_eq!(sb.len(), 10);
        // Collapse again
        let toggled = sb.toggle_expand_at(0);
        assert!(toggled);
        assert_eq!(sb.len(), 4); // 3 + hint
        let (visible, _, _) = sb.visible(20);
        let joined = visible.join("\n");
        assert!(joined.contains("[Tab to expand"));
    }

    #[test]
    fn toggle_expand_outside_entry_returns_false() {
        let mut sb = Scrollback::new(100);
        sb.push("plain line".to_string());
        let toggled = sb.toggle_expand_at(0);
        assert!(!toggled);
    }

    #[test]
    fn is_collapsible_start_identifies_entry_start() {
        let mut sb = Scrollback::new(100);
        sb.push("before".to_string());
        let lines: Vec<String> = (1..=5).map(|i| format!("line {i}")).collect();
        sb.push_collapsible(&lines, 2);
        assert!(!sb.is_collapsible_start(0));
        assert!(sb.is_collapsible_start(1));
    }

    #[test]
    fn toggle_expand_after_scroll_adjusts_offset_if_needed() {
        let mut sb = Scrollback::new(50);
        let lines: Vec<String> = (1..=20).map(|i| format!("line {i}")).collect();
        sb.push_collapsible(&lines, 3);
        sb.scroll_to_top();
        // Toggle expand — offset should be clamped to max_scroll
        sb.toggle_expand_at(0);
        // After expansion there are 20 lines, max_scroll = 19
        assert!(sb.scroll_offset() <= sb.max_scroll());
    }

    #[test]
    fn pop_removes_last_line() {
        let mut sb = Scrollback::new(100);
        sb.push("first".to_string());
        sb.push("second".to_string());
        sb.push("third".to_string());
        assert_eq!(sb.len(), 3);
        assert_eq!(sb.pop(), Some("third".to_string()));
        assert_eq!(sb.len(), 2);
        assert_eq!(sb.pop(), Some("second".to_string()));
        assert_eq!(sb.len(), 1);
    }

    #[test]
    fn pop_on_empty_returns_none() {
        let mut sb = Scrollback::new(100);
        assert_eq!(sb.pop(), None);
    }
}
