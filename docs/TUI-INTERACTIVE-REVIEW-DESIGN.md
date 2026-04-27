# TUI / Interactive Review — Design & Implementation Plan (Phase 3.4)

## 1. Overview

A full-screen terminal UI for `ninmu` that enables real-time agent monitoring, keyboard-driven review, and visual session tree navigation. Activated via `ninmu --tui`.

The TUI complements the existing inline REPL — it is **opt-in** and does not replace the default mode.

## 2. Architecture

```
┌───────────────────────────────────────────────┐
│  ratatui Event Loop                            │
│  ┌──────────────┬──────────────┬─────────────┐ │
│  │  Conversation  │  Tool/Event   │  Session    │ │
│  │  Pane         │  Pane         │  Tree Pane  │ │
│  │  (scrollable) │  (live feed)  │  (branches) │ │
│  ├──────────────┴──────────────┴─────────────┤ │
│  │  Status Bar: model | tokens | mode | git  │ │
│  ├───────────────────────────────────────────┤ │
│  │  Input Bar: [prompt]                      │ │
│  └───────────────────────────────────────────┘ │
│                                                │
│  ┌───────────────────────────────────────────┐ │
│  │  Overlays (toggled via keyboard):          │ │
│  │  • ? — Keyboard shortcuts help             │ │
│  │  • / — Command palette (fuzzy search)      │ │
│  │  • y/n — Permission prompt on tool call    │ │
│  └───────────────────────────────────────────┘ │
└───────────────────────────────────────────────┘
```

### Component Tree

```
TuiApp
├── ConversationPane      # Main text area with scrollback
│   ├── assistant blocks  # Streamed text (markdown rendered)
│   ├── tool calls        # Expandable/collapsible
│   └── tool results      # Syntax-highlighted output
├── ToolEventPane         # Right sidebar (optional, width toggle)
│   ├── tool timeline     # Live: tool → status → duration
│   └── event log         # Structured event feed
├── SessionTreePane       # Bottom-right (optional, toggle)
│   ├── branch list       # All branches for current session
│   └── current position  # Highlighted active node
├── StatusBar             # Always-visible bottom line
│   ├── model             # Current model name
│   ├── tokens            # Cumulative input/output
│   ├── cost              # Estimated cost (accent highlight)
│   ├── mode              # Permission mode
│   ├── git               # Current branch (if in repo)
│   └── elapsed           # Current turn duration
├── InputBar              # Bottom input area
│   ├── prompt text       # User input with @-completion
│   └── slash commands    # Autocomplete as in REPL
└── OverlayManager
    ├── help_overlay      # ? — keyboard shortcuts
    ├── palette_overlay   # / — fuzzy command palette
    ├── permission_overlay# y/n — tool approval prompt
    └── search_overlay    # Ctrl+F — conversation search
```

### Data Flow

```
ninmu rpc (Rust backend)
    │ stream events
    ▼
EventBus (in-memory channel)
    │ subscribe
    ▼
TuiApp::update()
    │ match event type
    ▼
┌─ TextDelta   → append to ConversationPane buffer
├─ ToolUse     → add to ToolEventPane, show permission overlay
├─ ToolResult  → update tool status, append result
├─ Usage       → update StatusBar token/cost counters
├─ MessageStop → finalize turn, update SessionTreePane
└─ Error       → show error overlay
```

## 3. Key Bindings (Vim-inspired)

| Key | Action |
|-----|--------|
| `j` / `k` | Scroll conversation pane up/down |
| `g` / `G` | Scroll to top / bottom |
| `Ctrl+d` / `Ctrl+u` | Page down / up (half-screen) |
| `Tab` / `Shift+Tab` | Focus next/previous pane |
| `Enter` | Accept input / expand item |
| `y` / `n` | Approve / deny permission prompt |
| `e` | Edit permission prompt response |
| `/` | Open command palette |
| `?` | Toggle help overlay |
| `t` | Toggle tool/event pane |
| `s` | Toggle session tree pane |
| `r` | Refresh / re-render |
| `Esc` | Close overlay / return to input |
| `Ctrl+c` | Cancel current turn / exit |
| `Ctrl+f` | Search conversation (enter text, Enter to find, n/N for next/prev) |

## 4. Test Plan (TDD-First)

### Testing Strategy

All TUI code is written test-first in three layers:

1. **Unit tests** — Pure logic, no terminal required (use `TestBackend` from ratatui)
2. **Integration tests** — Wire mock `ApiClient` + mock `ToolExecutor`, run `TuiApp::update()` cycle
3. **E2E tests** — Launch binary with `--tui` flag, simulate key events via stdin, capture rendered frames

### Unit Tests

All tests use `ratatui::backend::TestBackend` — no real terminal needed.

| # | Test | What it verifies | Coverage |
|---|------|------------------|----------|
| U1 | `tui_app_initializes_clean` | TuiApp construct, panes default state, no crash | Setup |
| U2 | `tui_app_renders_initial_frame` | First render produces a non-empty buffer | Rendering |
| U3 | `conversation_pane_appends_text` | TextDelta appended, scroll follows bottom | Data flow |
| U4 | `conversation_pane_scroll_up_down` | Scroll offset changes with j/k | Navigation |
| U5 | `conversation_pane_scroll_to_top_bottom` | g/G navigate to extremes | Navigation |
| U6 | `conversation_pane_page_up_down` | Ctrl+d/u moves by half viewport | Navigation |
| U7 | `conversation_pane_truncates_at_bounds` | 10K lines → only last N rendered | Performance |
| U8 | `conversation_pane_markdown_renders_headers` | `# Title` → rendered as styled heading | Rendering |
| U9 | `conversation_pane_markdown_renders_code_blocks` | Triple-backtick → syntax highlighted | Rendering |
| U10 | `conversation_pane_markdown_renders_lists` | `- item` → bullet list | Rendering |
| U11 | `conversation_pane_markdown_renders_tables` | Markdown table → aligned columns | Rendering |
| U12 | `tool_event_pane_shows_tool_call` | ToolUse event → entry in tool pane | Data flow |
| U13 | `tool_event_pane_updates_status` | Started → running → completed/failed | State |
| U14 | `tool_event_pane_shows_duration` | Completed tool shows elapsed time | UI |
| U15 | `tool_event_pane_toggle_visibility` | `t` key toggles pane on/off | Navigation |
| U16 | `status_bar_shows_model` | Model name displayed in status | Info |
| U17 | `status_bar_shows_token_counts` | TokenUsage events update counters | Info |
| U18 | `status_bar_shows_cost` | Cost estimate displayed in accent | Info |
| U19 | `status_bar_shows_permission_mode` | Mode displayed (read-only/workspace-write/etc) | Info |
| U20 | `status_bar_shows_git_branch` | Git branch from cwd parsed and displayed | Info |
| U21 | `status_bar_shows_elapsed_time` | Elapsed timer ticks every second | Info |
| U22 | `status_bar_updates_in_real_time` | Multiple Usage events → incremental update | Data flow |
| U23 | `input_bar_accepts_text` | Typing characters → text appears | Input |
| U24 | `input_bar_at_completion` | `@src/` → shows file completions | Input |
| U25 | `input_bar_slash_completion` | `/mod` → Tab completes to `/model` | Input |
| U26 | `input_bar_submits_on_enter` | Enter → fires turn event | Input |
| U27 | `input_bar_multiline` | Shift+Enter → newline, Enter → submit | Input |
| U28 | `session_tree_pane_shows_branches` | Branch fork → new entry in tree | Data flow |
| U29 | `session_tree_pane_highlights_current` | Active node visually distinct | Navigation |
| U30 | `session_tree_pane_navigate` | Select branch → cursor moves | Navigation |
| U31 | `session_tree_pane_toggle` | `s` key toggles visibility | Navigation |
| U32 | `help_overlay_shows_bindings` | ? → overlay with all bindings listed | Overlay |
| U33 | `help_overlay_closes_on_esc` | Esc → overlay hidden | Overlay |
| U34 | `command_palette_fuzzy_search` | `/mod` → shows /model, /mode related | Overlay |
| U35 | `command_palette_executes_selection` | Enter on selected → command dispatched | Overlay |
| U36 | `command_palette_closes_on_esc` | Esc → overlay hidden | Overlay |
| U37 | `permission_overlay_shows_tool_info` | ToolUse → overlay with name + input | Overlay |
| U38 | `permission_overlay_approve` | `y` → allow tool execution | Overlay |
| U39 | `permission_overlay_deny` | `n` → deny tool, send error to model | Overlay |
| U40 | `permission_overlay_edit` | `e` → edit prompt before responding | Overlay |
| U41 | `search_overlay_highlights_matches` | Ctrl+F → terms highlighted in text | Overlay |
| U42 | `search_overlay_navigate_results` | n/N → next/prev match scrolls | Overlay |
| U43 | `keyboard_focus_cycles_panes` | Tab/Shift+Tab cycles through visible panes | Navigation |
| U44 | `resize_triggers_rerender` | Terminal resize → all panes re-render | Lifeycle |
| U45 | `ctrl_c_cancels_turn` | Ctrl+C during turn → abort event | Lifeycle |
| U46 | `ctrl_c_exits_when_idle` | Ctrl+C when no active turn → clean exit | Lifeycle |
| U47 | `event_type_all_8_handled` | All 8 AssistantEvent types → correct pane update | Data flow |
| U48 | `tool_result_long_truncated` | 500-line tool output → shows "N more lines" | Display |
| U49 | `tool_result_expand_collapse` | Enter on truncated entry → full output | Display |
| U50 | `conversation_search_case_insensitive` | "Foo" matches "foo", "FOO" | Search |

**Total: 50 unit tests**

### Integration Tests

Wire `ScriptedApiClient` (from `ninmu-runtime`) + `StaticToolExecutor`, run `TuiApp` through a full conversation cycle.

| # | Test | Description |
|---|------|-------------|
| I1 | `full_turn_with_tool_use` | User prompt → TextDelta → ToolUse → ToolResult → MessageStop. All panes update correctly |
| I2 | `full_turn_no_tools` | TextDelta → MessageStop. Only conversation pane changes |
| I3 | `multiple_tools_in_sequence` | 3 ToolUse + 3 ToolResult → tool pane shows all 3 with correct status |
| I4 | `permission_approve_flow` | ToolUse → permission overlay → y → tool executes |
| I5 | `permission_deny_flow` | ToolUse → permission overlay → n → tool denied, model notified |
| I6 | `session_tree_fork_view` | Start session, fork, navigate between branches in tree |
| I7 | `error_event_displays_notification` | RuntimeError → error overlay appears |
| I8 | `concurrent_event_ordering` | Usage events interleaved with TextDelta → counters update correctly |
| I9 | `auto_compaction_notification` | AutoCompactionEvent → status bar shows "compacted N messages" |
| I10 | `turn_cancellation_mid_stream` | Ctrl+C during TextDelta streaming → turn aborted cleanly |

**Total: 10 integration tests**

### E2E Tests

Launch `ninmu --tui` binary with controlled stdin, capture rendered frames via script(1) or `tmux`.

| # | Test | Description |
|---|------|-------------|
| E1 | `tui_startup_shows_banner` | Binary starts, initial frame shows status bar + input |
| E2 | `type_prompt_and_see_response` | Type text + Enter → response appears in conversation pane |
| E3 | `tab_focus_cycles_panes` | Tab key cycles focus through panes |
| E4 | `help_overlay_toggle` | `?` shows overlay, Esc hides it |
| E5 | `command_palette_opens` | `/` opens palette, type filters, Enter dispatches |
| E6 | `session_tree_view` | `s` toggles tree pane, shows current session |
| E7 | `tool_permission_prompt` | Tool called → overlay appears, `y` approves |
| E8 | `resize_during_conversation` | Terminal resize → layout adapts (no panic) |
| E9 | `exit_with_ctrl_c` | Ctrl+C → clean exit (exit code 0) |
| E10 | `@_file_completion_works` | `@` → file completions appear in input bar |

**Total: 10 e2e tests**

### Testing Gaps Checklist

- [ ] **Input edge cases**: empty input, 100K-char input, only whitespace, only newlines, unicode RTL text, zero-width joiners
- [ ] **Display edge cases**: zero-width terminal (resize to 0 cols), extremely wide terminal (400+ cols), very tall terminal (200+ rows)
- [ ] **Performance**: 10K-line conversation buffer, 50 concurrent tool calls in timeline, 500-file session tree
- [ ] **Data integrity**: conversation text larger than 16-bit buffer (overflow), rapid event stream (100 events/sec), interleaved event types
- [ ] **Signal handling**: SIGINT during overlay, SIGTERM during tool execution, SIGHUP on terminal disconnect, SIGWINCH during rendering
- [ ] **Concurrency**: event stream messages arriving during overlay display, pane toggle while streaming, resize during animation
- [ ] **Cross-platform**: Windows console (no ANSI), tmux inside SSH, macOS Terminal.app, iTerm2, Alacritty, Kitty
- [ ] **Security**: escape sequence injection in model output, control characters in conversation text
- [ ] **Recovery**: process crash mid-render, partial frame render, corrupted event stream
- [ ] **User state**: ctrl+c mid-typing preserves input buffer, session tree state survives pane toggle, scroll position preserved on resize

## 5. Implementation Phases

### Phase 1: Core TUI Framework (4 days)

- [ ] Add `ratatui` dependency (optional, behind `tui` feature flag)
- [ ] `TuiApp` struct with `ratatui::Terminal<TestBackend>` for testing
- [ ] `ConversationPane` with scrollback buffer and markdown rendering (TDD: U1-U11)
- [ ] `StatusBar` with model/tokens/cost/mode/git/elapsed (TDD: U16-U22)
- [ ] `InputBar` with text input and @-completion (TDD: U23-U27)
- [ ] Event bus (crossbeam channel) connecting TuiApp to API events
- [ ] Keyboard event handling loop (TDD: U43-U47)
- [ ] 50 unit tests passing

### Phase 2: Tool & Session Visualization (3 days)

- [ ] `ToolEventPane` with live tool timeline (TDD: U12-U15)
- [ ] `SessionTreePane` with branch visualization (TDD: U28-U31)
- [ ] Collapsible tool output (TDD: U48-U49)
- [ ] Syntax-highlighted tool results
- [ ] 10 integration tests passing

### Phase 3: Overlays & Navigation (3 days)

- [ ] `HelpOverlay` (TDD: U32-U33)
- [ ] `CommandPalette` with fuzzy search (TDD: U34-U36)
- [ ] `PermissionOverlay` with approve/deny/edit (TDD: U37-U40)
- [ ] `SearchOverlay` with highlight + navigate (TDD: U41-U42)
- [ ] Terminal resize handling (TDD: U44)
- [ ] Ctrl+C cancellation (TDD: U45-U46)
- [ ] 10 e2e tests passing

### Phase 4: Polish & Release (2 days)

- [ ] Performance profiling: 10K-line scrollback, 50 concurrent tools
- [ ] Unicode/emoji rendering edge cases
- [ ] Zero-width terminal handling
- [ ] Theme integration (reuse existing `Theme` from `tui/theme.rs`)
- [ ] CI integration: unit tests on every PR, e2e on merge to main

## 6. Project Structure

```
crates/ninmu-cli/src/tui/
├── mod.rs                  # Module root, re-exports
├── app.rs                  # TuiApp: main event loop, state management
├── conversation_pane.rs    # Scrollable text area with markdown
├── status_bar.rs           # Bottom status line (reuse existing design)
├── input_bar.rs            # Text input area (reuse rustyline logic)
├── tool_event_pane.rs      # Live tool call timeline
├── session_tree_pane.rs    # Branch visualization
├── overlay.rs              # OverlayManager + overlay implementations
├── diff_view.rs            # Colored diff rendering
├── search.rs               # Conversation search + highlight
├── theme.rs                # TUI-specific theme mapping (reuse Theme)
└── layout.rs               # Pane layout calculations

crates/ninmu-cli/tests/
├── tui/
│   ├── test_app.rs         # Unit tests (50 tests)
│   ├── integration.rs      # Integration tests (10 tests)
│   └── e2e.rs              # End-to-end tests (10 tests)
```

## 7. CI Integration

```yaml
# Part of existing rust-ci.yml, added to test matrix
- name: Run TUI unit tests
  run: cargo test -p ninmu-cli --bin ninmu -- tui:: --test-threads=1
- name: Run TUI integration tests
  run: cargo test -p ninmu-cli --test tui --test-threads=1
```
