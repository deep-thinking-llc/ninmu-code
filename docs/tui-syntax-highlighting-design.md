# TUI Syntax Highlighting & Rich Tool Output

## Problem

The ratatui TUI (`--tui`) discards tool output and renders all code as flat text.
The non-TUI terminal path already has full syntect + pulldown-cmark rendering,
but the TUI bypasses it entirely. Three concrete symptoms:

1. **Invisible tool output** — After a bash command, read, grep, or edit, the TUI
   shows only `"ok bash (42 lines)"`. The actual stdout / file content / search
   results are thrown away at `ratatui_app.rs:549`.

2. **Flat code blocks** — Fenced code in LLM responses is detected (`` ``` ``)
   but rendered with a single foreground color (`CODE_FG`) at `ratatui_app.rs:856`.
   No per-token syntax highlighting.

3. **Minimal markdown** — `markdown_spans()` (line 1196) only handles `**bold**`,
   `*italic*`, and `` `code` ``. Headings, lists, blockquotes, tables, and links
   all render as plain text.

---

## Architecture — Two Divergent Paths

The worker thread (`app.rs`) already formats tool results with full highlighting
and sends them to stdout. But the TUI receives raw events on a channel and
re-formats them from scratch, discarding the highlighted output.

```
WORKER THREAD (app.rs)                         MAIN THREAD (ratatui_app.rs)
─────────────────────────                      ────────────────────────────
                                              
CliToolExecutor::execute()                     process_event(ToolResult):
  │                                              │
  ├─► bridge.tool_result(name, output, err)  ──► │  scrollback.push(
  │     sends raw JSON output                     │    "  ok bash (42 lines)")
  │                                               │  ← OUTPUT DISCARDED
  ├─► format_tool_result(name, output,       ╳   │
  │     is_error, Some(&highlight))               │
  │     formats with syntect highlighting         │
  │                                               │
  └─► renderer.stream_markdown(formatted)    ╳   │
        writes ANSI to stdout                     │
        ← GOES NOWHERE IN TUI MODE               │
```

The highlighted formatting is done on the worker thread but sent to stdout,
which is invisible in the alternate-screen TUI. The `TuiEvent::ToolResult`
carries the raw output, but `process_event()` ignores it.

### Key Files (with line numbers)

| File | Key items | Status |
|------|-----------|--------|
| `render.rs:187` | `struct TerminalRenderer` — syntect `SyntaxSet` + `Theme` | ✅ |
| `render.rs:569` | `fn highlight_code(&self, code, language) -> String` — ANSI output | ✅ |
| `render.rs:251` | `fn render_markdown(&self, markdown) -> String` — full markdown→ANSI | ✅ |
| `format/tool_fmt.rs:15` | `type HighlightFn = Option<&dyn Fn(&str, &str) -> String>` | ✅ |
| `format/tool_fmt.rs:84` | `fn format_tool_result(name, output, is_error, highlight) -> String` | ✅ |
| `format/tool_fmt.rs:146` | `fn format_bash_result(icon, parsed, highlight) -> String` | ✅ |
| `format/tool_fmt.rs:216` | `fn format_read_result(icon, parsed, highlight) -> String` | ✅ |
| `format/tool_fmt.rs:298` | `fn format_grep_result(icon, parsed) -> String` | ✅ |
| `tui/ratatui_app.rs:64` | `struct RatatuiApp` — no `TerminalRenderer` field | ❌ |
| `tui/ratatui_app.rs:540` | `TuiEvent::ToolResult` handler — discards output | ❌ |
| `tui/ratatui_app.rs:856` | Code block rendering — flat `CODE_FG` color | ❌ |
| `tui/ratatui_app.rs:1196` | `fn markdown_spans(text) -> Vec<Span>` — bold/italic/code only | ❌ |
| `tui/scrollback.rs:10` | `struct Scrollback { lines: Vec<String> }` — plain strings only | ❌ |
| `tui/scrollback.rs:81` | `fn push_collapsible(full_lines, visible_lines)` — exists, unused for tools | ✅ |
| `tui/event.rs:29` | `TuiEvent::ToolResult { name, output, is_error }` — carries raw output | ✅ |
| `tui/theme.rs` | ANSI color constants | ✅ |

---

## Non-Goals

- **Terminal emulator features**: No image rendering, sixel, or Kitty protocol.
- **LSP integration**: No language-server-based highlighting; syntect is sufficient.
- **Editable code blocks**: Code is display-only, not editable.
- **Custom themes**: Use the existing `base16-ocean.dark` syntect theme and DESIGN.md palette.

---

## Phase 1: Show Tool Output in TUI

**Goal**: Tool results display actual content (truncated) with syntax highlighting.

### 1a. Add `TerminalRenderer` to `RatatuiApp`

**File**: `tui/ratatui_app.rs`

Add a `renderer` field to `RatatuiApp` (line ~64). This is the same `TerminalRenderer`
used by the non-TUI path. It owns a `SyntaxSet` and syntect `Theme`.

```rust
use crate::render::TerminalRenderer;

pub struct RatatuiApp {
    // ... existing fields ...
    renderer: TerminalRenderer,  // NEW — owns SyntaxSet + syntect Theme
}
```

Initialize in `new()` (line ~155):
```rust
renderer: TerminalRenderer::new(),
```

### 1b. Build `ansi_to_spans()` converter

**File**: `tui/ratatui_app.rs` (new function, near `markdown_spans` at line 1196)

This is the critical bridge function. It converts ANSI-escaped strings (produced by
`TerminalRenderer::highlight_code()` and `format_tool_result()`) into ratatui
`Vec<Span<'static>>` with proper `Style` (fg color, bold, dim, italic).

ANSI sequences to handle (from syntect's `as_24_bit_terminal_escaped`):

| Sequence | Meaning | ratatui mapping |
|----------|---------|-----------------|
| `\x1b[38;2;R;G;Bm` | 24-bit foreground | `Style::default().fg(Color::Rgb(R, G, B))` |
| `\x1b[48;2;R;G;Bm` | 24-bit background | `Style::default().bg(Color::Rgb(R, G, B))` |
| `\x1b[38;5;Nm` | 256-color foreground | `Color::Indexed(N)` |
| `\x1b[48;5;Nm` | 256-color background | `Color::Indexed(N)` (bg) |
| `\x1b[1m` | Bold | `Modifier::BOLD` |
| `\x1b[2m` | Dim | `Modifier::DIM` (if supported) |
| `\x1b[3m` | Italic | `Modifier::ITALIC` |
| `\x1b[9m` | Strikethrough | `Modifier::CROSSED_OUT` |
| `\x1b[0m` | Reset | Reset style to default |
| `\x1b[0;48;5;236m` | Reset + bg | Reset fg, set bg (from `apply_code_block_background`) |

Implementation outline:

```rust
fn ansi_to_spans(s: &str) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut current = String::new();
    let mut style = Style::default();
    let mut chars = s.chars().peekable();

    while let Some(c) = chars.next() {
        if c == '\x1b' && chars.peek() == Some(&'[') {
            chars.next(); // skip '['
            // Collect parameter bytes until a letter (the command)
            let mut params = String::new();
            while let Some(&next) = chars.peek() {
                if next.is_ascii_alphabetic() {
                    chars.next();
                    break;
                }
                params.push(next);
                chars.next();
            }
            // Apply the SGR sequence
            style = apply_sgr(style, &params);
        } else {
            current.push(c);
        }
    }
    if !current.is_empty() {
        spans.push(Span::styled(std::mem::take(&mut current), style));
    }
    spans
}

fn apply_sgr(current: Style, params: &str) -> Style {
    let codes: Vec<u8> = params.split(';').filter_map(|p| p.parse().ok()).collect();
    let mut style = current;
    let mut i = 0;
    while i < codes.len() {
        match codes[i] {
            0 => style = Style::default(),
            1 => style = style.add_modifier(Modifier::BOLD),
            2 => style = style.add_modifier(Modifier::DIM),
            3 => style = style.add_modifier(Modifier::ITALIC),
            9 => style = style.add_modifier(Modifier::CROSSED_OUT),
            38 => {
                // Extended foreground
                if i + 1 < codes.len() {
                    match codes[i + 1] {
                        2 if i + 4 < codes.len() => {
                            // 24-bit: 38;2;R;G;B
                            style = style.fg(Color::Rgb(codes[i+2], codes[i+3], codes[i+4]));
                            i += 4;
                        }
                        5 if i + 2 < codes.len() => {
                            // 256-color: 38;5;N
                            style = style.fg(Color::Indexed(codes[i+2]));
                            i += 2;
                        }
                        _ => {}
                    }
                }
            }
            48 => {
                // Extended background (same pattern)
                if i + 1 < codes.len() {
                    match codes[i + 1] {
                        2 if i + 4 < codes.len() => {
                            style = style.bg(Color::Rgb(codes[i+2], codes[i+3], codes[i+4]));
                            i += 4;
                        }
                        5 if i + 2 < codes.len() => {
                            style = style.bg(Color::Indexed(codes[i+2]));
                            i += 2;
                        }
                        _ => {}
                    }
                }
            }
            30..=37 => {
                // Standard foreground colors
                let color = match codes[i] {
                    30 => Color::Black,
                    31 => Color::Red,
                    32 => Color::Green,
                    33 => Color::Yellow,
                    34 => Color::Blue,
                    35 => Color::Magenta,
                    36 => Color::Cyan,
                    37 => Color::White,
                    _ => unreachable!(),
                };
                style = style.fg(color);
            }
            _ => {} // ignore unknown codes
        }
        i += 1;
    }
    style
}
```

**Why this approach**: syntect's `as_24_bit_terminal_escaped` produces a limited
subset of ANSI SGR sequences (just color and style). We don't need a full terminal
emulator — just enough to handle the patterns syntect emits.

### 1c. Format and display tool results with highlighting

**File**: `tui/ratatui_app.rs`, `process_event()` at line 540

Replace the current `TuiEvent::ToolResult` handler:

```rust
TuiEvent::ToolResult { name, output, is_error } => {
    self.state.current_tool = None;
    let highlight = |code: &str, lang: &str| self.renderer.highlight_code(code, lang);
    let formatted = format_tool_result(&name, &output, is_error, Some(&highlight));

    let lines: Vec<String> = formatted.lines().map(String::from).collect();
    if lines.len() > 14 {
        // Collapsible: show first 10 lines, Tab to expand
        self.scrollback.push_collapsible(&lines, 10);
    } else {
        for line in lines {
            self.scrollback.push(line);
        }
    }
}
```

This reuses the existing `format_tool_result()` from `tool_fmt.rs` — the same
function the non-TUI path uses. It produces ANSI-highlighted strings for:
- Bash stdout/stderr (with "bash" language highlighting)
- File read content (with language detection from extension)
- Grep search results (with matching lines)
- Glob search results (with file list)
- Edit diffs (with +/- coloring)

### 1d. Render ANSI-containing lines in `draw_conversation()`

**File**: `tui/ratatui_app.rs`, `draw_conversation()` at line 805

Currently every line goes through `markdown_spans()`. Add an ANSI detection
branch before the markdown fallback:

```rust
// In the final fallback branch (line ~890), replace:
//   Line::from(markdown_spans(s))
// with:
if s.contains('\x1b') {
    Line::from(ansi_to_spans(s))
} else {
    Line::from(markdown_spans(s))
}
```

Also update the code block detection to handle ANSI-containing lines inside
code fences (they won't start with `` ``` ``):

```rust
if in_code_block {
    if s.contains('\x1b') {
        // Pre-highlight line from format_tool_result
        return Line::from(ansi_to_spans(s));
    }
    return Line::from(Span::styled(
        format!("  {s}"),
        Style::default().fg(CODE_FG).bg(CODE_BG),
    ));
}
```

### 1e. Update `load_conversation_history()` for tool results

**File**: `tui/ratatui_app.rs`, `load_conversation_history()` at line 676

Currently, `ContentBlock::ToolResult` is skipped entirely:

```rust
ContentBlock::ToolResult { .. } | ContentBlock::Thinking { .. } => {}
```

Format and display tool results when loading history, using the same
`format_tool_result()` + collapsible logic from 1c.

### Phase 1 Acceptance Criteria

- [ ] `cargo test -p ninmu-cli` passes
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` passes
- [ ] Bash command output visible in TUI with syntax-highlighted code
- [ ] `read_file` output visible with language-detected highlighting
- [ ] `grep_search` results visible with matching lines
- [ ] Long tool output is collapsible (Tab to expand/collapse)
- [ ] Tool results from `/resume` session history are displayed
- [ ] `ansi_to_spans()` handles 24-bit, 256-color, bold, dim, italic, reset

---

## Phase 2: Syntax-Highlight Code Blocks in LLM Responses

**Goal**: Fenced code blocks in the model's streaming text get per-token highlighting.

### 2a. Track code fence state during streaming

**File**: `tui/ratatui_app.rs`

Add fields to `RatatuiApp`:

```rust
/// Accumulator for code block content during streaming.
code_block_buf: String,
/// Language tag from the opening fence (e.g., "rust", "bash").
code_block_lang: String,
/// Whether we're currently inside a fenced code block.
in_code_block: bool,
```

### 2b. Intercept code blocks in `update_streaming_display()`

**File**: `tui/ratatui_app.rs`, line 649

Currently, `update_streaming_display()` splits `response_text` on `\n` and pushes
each line to scrollback. Modify it to detect code fences and accumulate code blocks:

```rust
fn update_streaming_display(&mut self) {
    if !self.response_text.contains('\n') {
        return;
    }
    let text = std::mem::take(&mut self.response_text);
    let mut parts = text.split('\n');
    let remainder = parts.next_back().unwrap_or("").to_string();

    for part in parts {
        let trimmed = part.trim();
        if trimmed.starts_with("```") && !self.in_code_block {
            // Opening fence
            self.in_code_block = true;
            self.code_block_lang = trimmed.trim_start_matches('`').to_string();
            self.code_block_buf.clear();
            self.scrollback.push(part.to_string()); // push the fence line
        } else if trimmed.starts_with("```") && self.in_code_block {
            // Closing fence — highlight and push accumulated code
            self.in_code_block = false;
            let highlighted = self.renderer.highlight_code(
                &self.code_block_buf,
                &self.code_block_lang,
            );
            for hl_line in highlighted.lines() {
                self.scrollback.push(hl_line.to_string());
            }
            self.scrollback.push(part.to_string()); // push the closing fence
            self.code_block_buf.clear();
            self.code_block_lang.clear();
        } else if self.in_code_block {
            // Inside code block — accumulate
            if !self.code_block_buf.is_empty() {
                self.code_block_buf.push('\n');
            }
            self.code_block_buf.push_str(part);
        } else {
            self.scrollback.push(part.to_string());
        }
    }
    self.response_text = remainder;
}
```

### 2c. Handle unclosed code blocks in `flush_response()`

**File**: `tui/ratatui_app.rs`, line 612

If the model's response ends while inside a code block (stream interrupted, or
the model didn't close the fence), flush the partial code block:

```rust
fn flush_response(&mut self) {
    // Flush any partial code block first
    if self.in_code_block && !self.code_block_buf.is_empty() {
        let highlighted = self.renderer.highlight_code(
            &self.code_block_buf,
            &self.code_block_lang,
        );
        for line in highlighted.lines() {
            self.scrollback.push(line.to_string());
        }
        self.in_code_block = false;
        self.code_block_buf.clear();
        self.code_block_lang.clear();
    }
    // ... existing flush logic ...
}
```

### 2d. Update `draw_conversation()` for pre-highlighted code lines

Lines from highlighted code blocks will contain ANSI escapes. The `ansi_to_spans()`
function from Phase 1 handles this. No additional changes needed if the ANSI
detection branch is in place.

### Phase 2 Acceptance Criteria

- [ ] Rust code blocks get keyword/string/comment highlighting
- [ ] Bash code blocks get shell syntax highlighting
- [ ] Python/JS/etc. blocks get language-appropriate highlighting
- [ ] Partial code blocks (stream interrupted) are still highlighted
- [ ] Opening/closing ``` fence lines remain unstyled (muted)
- [ ] Code block background color matches DESIGN.md palette

---

## Phase 3: Rich Inline Markdown

**Goal**: Headings, lists, blockquotes, tables, and links render with proper styling.

### 3a. Replace `markdown_spans()` with pulldown-cmark

**File**: `tui/ratatui_app.rs`, line 1196

Replace the hand-rolled parser with `pulldown_cmark::Parser`. The challenge is that
pulldown-cmark operates on full documents, but we're rendering one scrollback line
at a time. Two options:

**Option A — Line-at-a-time (simpler, sufficient for most cases)**:
Feed each line to pulldown-cmark independently. This handles headings, bold, italic,
code, links, and blockquotes. It breaks for multi-line constructs (tables, nested
lists), but those are rare in LLM output and the existing line-at-a-time approach
already breaks for them.

**Option B — Full-document state machine (correct, more complex)**:
Maintain a `RenderState` (like `render.rs:132`) across lines in `draw_conversation()`.
Track heading level, emphasis depth, quote depth, list stack, and table state.
Feed each line through pulldown-cmark and emit styled `Span`s.

**Recommendation**: Option A for Phase 3, Option B as a follow-up if needed.

### 3b. Example: pulldown-cmark → ratatui Spans

```rust
fn markdown_spans_v2(text: &str) -> Vec<Span<'static>> {
    use pulldown_cmark::{Options, Parser, Event, Tag, TagEnd};

    let mut spans = Vec::new();
    let mut current = String::new();
    let mut style = Style::default().fg(TEXT);

    for event in Parser::new_ext(text, Options::all()) {
        match event {
            Event::Start(Tag::Heading { .. }) => {
                style = Style::default().fg(ACCENT).add_modifier(Modifier::BOLD);
            }
            Event::End(TagEnd::Heading(..)) => {
                style = Style::default().fg(TEXT);
            }
            Event::Start(Tag::Emphasis) => {
                flush(&mut spans, &mut current, style);
                style = style.add_modifier(Modifier::ITALIC);
            }
            Event::End(TagEnd::Emphasis) => {
                flush(&mut spans, &mut current, style);
                style = Style::default().fg(TEXT);
            }
            Event::Start(Tag::Strong) => {
                flush(&mut spans, &mut current, style);
                style = style.add_modifier(Modifier::BOLD);
            }
            Event::End(TagEnd::Strong) => {
                flush(&mut spans, &mut current, style);
                style = Style::default().fg(TEXT);
            }
            Event::Code(code) => {
                flush(&mut spans, &mut current, style);
                spans.push(Span::styled(
                    code.to_string(),
                    Style::default().fg(ACCENT).bg(Color::Rgb(30, 30, 30)),
                ));
            }
            Event::Text(text) => current.push_str(&text),
            Event::Start(Tag::Link { dest_url, .. }) => {
                flush(&mut spans, &mut current, style);
                // Store dest_url for display after link text
            }
            Event::End(TagEnd::Link) => {
                // Could append URL in muted color
            }
            _ => {}
        }
    }
    flush(&mut spans, &mut current, style);
    spans
}

fn flush(spans: &mut Vec<Span<'static>>, current: &mut String, style: Style) {
    if !current.is_empty() {
        spans.push(Span::styled(std::mem::take(current), style));
    }
}
```

### Phase 3 Acceptance Criteria

- [ ] `# Heading` renders in accent color, bold
- [ ] `## Heading` renders in accent color
- [ ] `- list item` renders with bullet marker
- [ ] `> blockquote` renders with muted border
- [ ] `[text](url)` renders as underlined link
- [ ] `**bold**` and `*italic*` render correctly
- [ ] Inline `` `code` `` renders with accent color + code background

---

## Phase 4: File Snippets from Grep/Read

**Goal**: When the LLM reads a file or greps for content, show relevant snippets
with line numbers and syntax highlighting.

### 4a. Extract file content from tool result JSON

**File**: `tui/ratatui_app.rs`, in the `ToolResult` handler (Phase 1c)

For `read_file` results, parse the JSON to extract:
- `file.path` — file path (for language detection)
- `file.content` — file content
- `file.startLine` — starting line number
- `file.numLines` — number of lines
- `file.totalLines` — total lines in file

For `grep_search` results, parse:
- `content` — matching lines with context
- `numMatches` / `numFiles` — summary counts

### 4b. Render file snippets with line numbers

```
  ── src/main.rs (lines 14-22 of 142) ─────────
  14 │ fn main() {
  15 │     let args = Args::parse();
  16 │     let config = Config::load(&args)?;
  17 │     // ...
  18 │ }
  ──────────────────────────────────────────────
```

Use `format_read_result()` from `tool_fmt.rs` which already produces this format
with syntax highlighting. The Phase 1c implementation handles this automatically
since `format_tool_result()` dispatches to `format_read_result()` for `read_file`.

### 4c. Grep results with context

`format_grep_result()` already formats grep output with matching lines. The
Phase 1c implementation handles this. For richer display, consider extracting
the matched file paths and showing a brief snippet around each match.

### Phase 4 Acceptance Criteria

- [ ] `read_file` results show file content with line numbers and syntax highlighting
- [ ] `grep_search` results show matching lines with file context
- [ ] `glob_search` results show matched file list
- [ ] File language is detected from extension for highlighting
- [ ] Long file reads are collapsible (Tab to expand)

---

## Implementation Order & Dependencies

```
Phase 1a (TerminalRenderer) ──┐
                               ├─► Phase 1c (format + display)
Phase 1b (ansi_to_spans) ─────┘         │
                                         ├─► Phase 1d (draw ANSI lines)
                                         ├─► Phase 1e (history loading)
                                         │
                                         ▼
                               Phase 2 (code block tracking)
                                         │
                                         ▼
                               Phase 3 (rich markdown)
                                         │
                                         ▼
                               Phase 4 (file snippets)
```

Phases 2-4 are independent of each other but all depend on Phase 1.

### Effort Estimates

| Phase | Tasks | Estimate |
|-------|-------|----------|
| **1a** | Add `TerminalRenderer` field | 15 min |
| **1b** | `ansi_to_spans()` + `apply_sgr()` | 2-3 hours |
| **1c** | Tool result handler + collapsible | 1 hour |
| **1d** | Draw function ANSI branch | 30 min |
| **1e** | History loading | 30 min |
| **2** | Code fence tracking in streaming | 2-3 hours |
| **3** | pulldown-cmark → Spans | 3-4 hours |
| **4** | File snippets (mostly handled by Phase 1) | 1 hour |
| **Tests** | Unit + integration tests per phase | 2-3 hours |
| **Total** | | ~15-18 hours |

---

## Risks & Mitigations

| Risk | Impact | Mitigation |
|------|--------|------------|
| `ansi_to_spans()` misses an SGR pattern | Colored text renders as raw escape codes | Unit test every SGR pattern syntect emits; add a fallback that strips unknown sequences |
| Syntect `SyntaxSet` is 3-5MB in memory | Increased startup time and memory | `SyntaxSet::load_defaults_newlines()` is already used by `TerminalRenderer`; one instance shared across TUI lifetime is fine |
| Code fence tracking drifts during streaming | Highlighted code leaks into prose or vice versa | Reuse the fence-detection logic from `normalize_nested_fences()` in `render.rs`; add property tests |
| `push_collapsible()` stores full content in memory | Large tool outputs (10k+ lines) consume memory | Already bounded by `Scrollback`'s 10k-line ring buffer; collapsible entries share the same eviction |
| ANSI codes in scrollback interact with ratatui's width calculation | Lines may wrap incorrectly | Use `strip_ansi()` (already exists at line 1250) for width measurement; render with `ansi_to_spans()` for display |

---

## Testing Strategy

### Unit Tests

1. **`ansi_to_spans()`** — test each SGR code type:
   - 24-bit fg: `\x1b[38;2;255;107;53m` → `Color::Rgb(255, 107, 53)`
   - 256-color: `\x1b[38;5;70m` → `Color::Indexed(70)`
   - Bold: `\x1b[1m` → `Modifier::BOLD`
   - Reset: `\x1b[0m` → `Style::default()`
   - Compound: `\x1b[0;48;5;236m` → reset fg, set bg
   - Mixed text + escapes: `"hello \x1b[31mworld\x1b[0m"` → 3 spans

2. **`apply_sgr()`** — test parameter parsing edge cases:
   - Empty params: `\x1b[m` (treat as reset)
   - Multiple params: `\x1b[1;3;38;2;255;0;0m`
   - Unknown codes: `\x1b[52m` (ignore gracefully)

3. **Tool result formatting** — test that `format_tool_result()` output
   correctly round-trips through `ansi_to_spans()`:
   - Bash result with stdout
   - Read result with Rust code
   - Grep result with matches
   - Error result

4. **Code fence tracking** — test `update_streaming_display()`:
   - Single complete code block across multiple deltas
   - Code block split across 3+ deltas
   - Unclosed code block in `flush_response()`
   - Nested fences (``` inside ````)

### Integration Tests

5. Push `TuiEvent::ToolResult` with bash output → verify scrollback contains
   highlighted lines (check for ANSI escapes in stored strings)

6. Push multiple `TextDelta` events forming a code block → verify scrollback
   contains syntax-highlighted code lines

7. Verify `push_collapsible()` + `toggle_expand_at()` works with ANSI-containing lines

### Visual Verification

8. Run `cargo run -- --tui` and execute:
   - A bash command with code output
   - A `read_file` on a `.rs` file
   - A `grep_search` for a function name
   - An LLM response with fenced code blocks
   - Verify all are highlighted and collapsible
