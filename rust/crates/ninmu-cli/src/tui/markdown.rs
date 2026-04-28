//! Simple markdown-to-ANSI terminal renderer.
//!
//! Handles basic inline formatting: **bold**, *italic*, `code`, and headings.

use crate::tui::theme::Theme;

/// Render a single line of markdown to ANSI-styled terminal text.
///
/// Supports:
/// - `**bold**` → bold
/// - `*italic*` → italic (dimmed as fallback)
/// - `` `code` `` → accent-colored
/// - `# ` headings → bold accent
/// - `> ` blockquotes → muted prefix
/// - `- ` / `* ` list items → bullet
pub fn render_markdown_line(line: &str) -> String {
    let trimmed = line.trim_start();

    // Headings: # ## ###
    if let Some(rest) = trimmed.strip_prefix("# ") {
        return format!("{}{}{}", Theme::ACCENT, render_inline(rest), Theme::RESET);
    }
    if let Some(rest) = trimmed.strip_prefix("## ") {
        return format!("{}{}{}", Theme::ACCENT, render_inline(rest), Theme::RESET);
    }
    if let Some(rest) = trimmed.strip_prefix("### ") {
        return format!("{}{}{}", Theme::ACCENT, render_inline(rest), Theme::RESET);
    }

    // Blockquotes
    if let Some(rest) = trimmed.strip_prefix("> ") {
        return format!("{}│ {}{}", Theme::MUTED, render_inline(rest), Theme::RESET);
    }

    // List items
    if trimmed.starts_with("- ") || trimmed.starts_with("* ") {
        let rest = &trimmed[2..];
        return format!("  • {}", render_inline(rest));
    }

    // Numbered lists: "1. "
    if trimmed.len() > 3
        && trimmed.as_bytes()[0].is_ascii_digit()
        && trimmed.as_bytes().get(1) == Some(&b'.')
        && trimmed.as_bytes().get(2) == Some(&b' ')
    {
        let num = &trimmed[..1];
        let rest = &trimmed[3..];
        return format!("  {num}. {}", render_inline(rest));
    }

    // Horizontal rule: --- or ***
    if trimmed.chars().all(|c| c == '-' || c == '*') && trimmed.len() >= 3 {
        return format!("{}{}{}", Theme::MUTED, "─".repeat(40), Theme::RESET);
    }

    // Regular line
    render_inline(line)
}

/// Render inline markdown formatting: **bold**, *italic*, `code`.
fn render_inline(text: &str) -> String {
    let mut result = String::with_capacity(text.len() + 32);
    let mut chars = text.chars().peekable();

    while let Some(c) = chars.next() {
        if c == '`' {
            // Inline code
            let code: String = chars.by_ref().take_while(|&ch| ch != '`').collect();
            result.push_str(&format!("{}{code}{}", Theme::ACCENT, Theme::RESET));
        } else if c == '*' && chars.peek() == Some(&'*') {
            // Bold: **text**
            chars.next(); // skip second *
            let bold: String = chars.by_ref().take_while(|&ch| ch != '*').collect();
            // Skip closing **
            if chars.peek() == Some(&'*') {
                chars.next();
            }
            result.push_str(&format!("\x1b[1m{bold}\x1b[0m"));
        } else if c == '*' {
            // Italic: *text* (use dimmed as fallback since not all terminals support italic)
            let italic: String = chars.by_ref().take_while(|&ch| ch != '*').collect();
            result.push_str(&format!("\x1b[2m{italic}\x1b[0m"));
        } else if c == '_' && chars.peek() == Some(&'_') {
            // Bold: __text__
            chars.next();
            let bold: String = chars.by_ref().take_while(|&ch| ch != '_').collect();
            if chars.peek() == Some(&'_') {
                chars.next();
            }
            result.push_str(&format!("\x1b[1m{bold}\x1b[0m"));
        } else if c == '~' && chars.peek() == Some(&'~') {
            // Strikethrough: ~~text~~
            chars.next();
            let strike: String = chars.by_ref().take_while(|&ch| ch != '~').collect();
            if chars.peek() == Some(&'~') {
                chars.next();
            }
            // Use dim + strikethrough if supported, else just dim
            result.push_str(&format!("\x1b[9m{strike}\x1b[0m"));
        } else {
            result.push(c);
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn heading_renders() {
        let out = render_markdown_line("# Hello");
        assert!(out.contains("Hello"));
        assert!(out.contains(Theme::ACCENT));
    }

    #[test]
    fn bold_renders() {
        let out = render_inline("say **hello** world");
        assert!(out.contains("hello"));
        assert!(out.contains("\x1b[1m"));
    }

    #[test]
    fn italic_renders() {
        let out = render_inline("say *hello* world");
        assert!(out.contains("hello"));
        assert!(out.contains("\x1b[2m"));
    }

    #[test]
    fn code_renders() {
        let out = render_inline("use `foo` here");
        assert!(out.contains("foo"));
        assert!(out.contains(Theme::ACCENT));
    }

    #[test]
    fn blockquote_renders() {
        let out = render_markdown_line("> note");
        assert!(out.contains("│"));
        assert!(out.contains("note"));
    }

    #[test]
    fn list_item_renders() {
        let out = render_markdown_line("- item");
        assert!(out.contains("•"));
        assert!(out.contains("item"));
    }

    #[test]
    fn horizontal_rule_renders() {
        let out = render_markdown_line("---");
        assert!(out.contains("─"));
    }

    #[test]
    fn numbered_list_renders() {
        let out = render_markdown_line("1. first");
        assert!(out.contains("1."));
        assert!(out.contains("first"));
    }

    #[test]
    fn heading_level_2_renders() {
        let out = render_markdown_line("## Subtitle");
        assert!(out.contains("Subtitle"));
        assert!(out.contains(Theme::ACCENT));
    }

    #[test]
    fn nested_bold_in_code_not_mangled() {
        // Code should take precedence; ** inside backticks is literal
        let out = render_inline("`**not bold**`");
        assert!(out.contains("**not bold**"));
    }

    #[test]
    fn empty_line_passthrough() {
        let out = render_markdown_line("");
        assert_eq!(out, "");
    }
}
