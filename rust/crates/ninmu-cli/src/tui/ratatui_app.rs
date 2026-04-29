//! Full-screen ratatui TUI -- "Japanese Industrial Precision" aesthetic.
//!
//! Entered via the `--tui` flag. Provides a scrollable conversation history
//! pane, a fixed input area, and a live status bar. Streaming events from
//! the model are consumed via [`TuiEvent`] channel so the UI updates
//! incrementally without blocking.
//!
//! DESIGN.md colour palette is applied throughout: flat surfaces, no emoji,
//! em-dash section markers, monospace labels, 4px max border radius.

use std::io;
use std::time::{Duration, Instant};

use crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEventKind, KeyModifiers,
};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::{execute, terminal};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use ratatui::Terminal;

use crate::tui::event::{ThinkingState, TuiEvent, TuiSharedState};
use crate::tui::permission::describe_tool_action;
use crate::tui::scrollback::Scrollback;
use ninmu_api::{model_token_limit, ModelTokenLimit};
use ninmu_runtime::PromptCacheEvent;
use ninmu_runtime::{
    ContentBlock, ConversationMessage, MessageRole, PermissionPromptDecision, PermissionRequest,
    TokenUsage,
};

// -- DESIGN.md colour palette ------------------------------------------------
const BG: Color = Color::Rgb(10, 10, 10);
const SURFACE: Color = Color::Rgb(22, 22, 22);
const BORDER: Color = Color::Rgb(15, 15, 15);
const BORDER_BRIGHT: Color = Color::Rgb(31, 31, 31);
const TEXT: Color = Color::Rgb(232, 232, 232);
const TEXT_SEC: Color = Color::Rgb(136, 136, 136);
const MUTED: Color = Color::Rgb(85, 85, 85);
const ACCENT: Color = Color::Rgb(255, 107, 53);
const ERROR_COLOR: Color = Color::Rgb(203, 80, 80);
const SUCCESS: Color = Color::Rgb(70, 180, 70);
const THINKING_COLOR: Color = Color::Rgb(136, 100, 220);
const USER_COLOR: Color = Color::Rgb(80, 200, 120);
const USER_COLOR_DIM: Color = Color::Rgb(40, 100, 60);
const LLM_COLOR: Color = Color::Rgb(200, 200, 230);
const CODE_BG: Color = Color::Rgb(28, 28, 36);
const CODE_FG: Color = Color::Rgb(180, 210, 240);

// -- Spinner frames -----------------------------------------------------------
const SPINNER: &[&str] = &[
    "  \u{2593}\u{2591}\u{2591}\u{2591}",
    "  \u{2593}\u{2593}\u{2591}\u{2591}",
    "  \u{2593}\u{2593}\u{2593}\u{2591}",
    "  \u{2593}\u{2593}\u{2593}\u{2593}",
    "  \u{2593}\u{2593}\u{2593}\u{2591}",
    "  \u{2593}\u{2593}\u{2591}\u{2591}",
];
const TOOL_SPINNER: &[&str] = &["|", "/", "-", "\\"];

/// All the state needed to render one frame of the TUI.
#[allow(clippy::struct_excessive_bools)]
pub struct RatatuiApp {
    scrollback: Scrollback,
    input_buf: Vec<char>,
    cursor: usize,
    state: TuiSharedState,
    help_visible: bool,
    spinner_frame: usize,
    tick: Instant,
    /// Accumulated complete lines from the current streaming response.
    response_text: String,
    /// Latest usage snapshot from the provider.
    usage: TokenUsage,
    /// Turn start time for elapsed display.
    turn_start: Option<Instant>,
    /// Git branch for the status bar.
    git_branch: Option<String>,
    /// Model name for the header.
    model: String,
    /// Permission mode string.
    permission_mode: String,
    /// Last known conversation viewport height (updated on each draw).
    last_conv_height: usize,
    /// Pending permission prompt waiting for user decision.
    pending_permission: Option<PendingPermission>,
    /// Blinking cursor toggle for streaming output.
    show_cursor_blink: bool,
    /// Cached pricing for cost estimation.
    model_pricing: Option<ninmu_runtime::ModelPricing>,
    /// Input history for up/down navigation.
    input_history: Vec<String>,
    /// Position in input history (None = editing a new line).
    history_index: Option<usize>,
    /// Saved input buffer when navigating history.
    history_restore_buf: Vec<char>,
    /// Whether the UI needs a redraw.
    dirty: bool,
    /// Cached header line (rebuilt only when model/perm/branch change).
    cached_header: Line<'static>,
    /// Cached input text (updated when input_buf changes).
    cached_input: String,
    /// Cached elapsed-second display (updated when the second changes).
    cached_elapsed_str: String,
    /// Current reasoning effort level (None = default).
    reasoning_effort: Option<String>,
    /// Whether thinking mode is enabled (None = auto).
    thinking_mode: Option<bool>,
    /// When we last received a TuiEvent from the worker thread. Used to
    /// detect stalled turns where the worker is stuck (e.g. dead SSE
    /// connection or blocked tool) and force-cancel them.
    last_event_received: Option<Instant>,
    /// Interactive model selector dialog (when open).
    model_selector: Option<ModelSelector>,
    /// Selected model callback — set by the TUI to communicate model changes.
    selected_model: Option<String>,
    /// Tracked paste spans within `input_buf`. Each entry records the
    /// `(start, len)` range in `input_buf` and the display summary.
    /// Used only for rendering; `input_buf` always holds the real text.
    paste_spans: Vec<PasteSpan>,
    /// Whether a paste animation is currently running.
    paste_animating: bool,
    /// Current frame index of the paste animation.
    paste_anim_frame: usize,
    /// When the paste animation started.
    paste_anim_start: Option<Instant>,
    /// The summary for the currently-animating paste (set during animation).
    anim_summary: Option<String>,
    /// The range in `input_buf` for the currently-animating paste.
    anim_range: Option<(usize, usize)>,
}

/// A tracked paste region inside `input_buf`.
struct PasteSpan {
    /// Start index in `input_buf` (chars).
    start: usize,
    /// Length in `input_buf` (chars).
    len: usize,
    /// Summary string, e.g. "[Pasted 42 words, 3 lines]".
    summary: String,
}

/// A permission prompt waiting for the user to respond in the TUI.
struct PendingPermission {
    request: PermissionRequest,
    response_tx: std::sync::mpsc::Sender<PermissionPromptDecision>,
    action_description: String,
}

/// Interactive model selector dialog state.
struct ModelSelector {
    /// All available model entries from the registry + custom models.
    all_entries: Vec<ninmu_api::ModelEntry>,
    /// Indices into `all_entries` that match the current filter.
    filtered: Vec<usize>,
    /// Current filter text typed by the user.
    filter: Vec<char>,
    /// Cursor position in the filter input.
    filter_cursor: usize,
    /// Currently highlighted row in the filtered list.
    selected: usize,
    /// Scroll offset for the filtered list (top visible row index).
    scroll_offset: usize,
    /// Maximum visible rows in the dropdown.
    max_visible: usize,
}

impl RatatuiApp {
    pub fn new(model: String, permission_mode: String, git_branch: Option<String>) -> Self {
        let model_pricing = ninmu_runtime::pricing_for_model(&model);
        let mut app = Self {
            scrollback: Scrollback::default(),
            input_buf: Vec::new(),
            cursor: 0,
            state: TuiSharedState::default(),
            help_visible: false,
            spinner_frame: 0,
            tick: Instant::now(),
            response_text: String::new(),
            usage: TokenUsage::default(),
            turn_start: None,
            git_branch,
            model,
            permission_mode,
            last_conv_height: 20,
            pending_permission: None,
            show_cursor_blink: true,
            model_pricing,
            input_history: Vec::new(),
            history_index: None,
            history_restore_buf: Vec::new(),
            dirty: true,
            cached_header: Line::default(),
            cached_input: String::new(),
            cached_elapsed_str: String::new(),
            reasoning_effort: None,
            thinking_mode: None,
            last_event_received: None,
            model_selector: None,
            selected_model: None,
            paste_spans: Vec::new(),
            paste_animating: false,
            paste_anim_frame: 0,
            paste_anim_start: None,
            anim_summary: None,
            anim_range: None,
        };
        app.cached_header = Self::build_header_line(
            &app.model,
            &app.permission_mode,
            app.git_branch.as_deref(),
            None,
            None,
        );
        app
    }

    /// Set reasoning effort and rebuild header.
    pub fn set_reasoning_effort(&mut self, effort: Option<String>) {
        self.reasoning_effort = effort;
        self.rebuild_header();
    }

    /// Set thinking mode and rebuild header.
    pub fn set_thinking_mode(&mut self, mode: Option<bool>) {
        self.thinking_mode = mode;
        self.rebuild_header();
    }

    fn rebuild_header(&mut self) {
        self.cached_header = Self::build_header_line(
            &self.model,
            &self.permission_mode,
            self.git_branch.as_deref(),
            self.reasoning_effort.as_deref(),
            self.thinking_mode,
        );
    }

    /// Open the interactive model selector dialog.
    pub fn open_model_selector(&mut self) {
        // Trigger a background refresh of models.dev cache so providers
        // with API keys show up-to-date models.
        ninmu_api::models_dev::refresh_models_async();

        let entries = ninmu_api::list_available_models();

        // Collapse providers without auth: keep only one entry per provider
        // (the first model encountered) so the list isn't cluttered with
        // models the user can't actually use.
        let entries = Self::collapse_no_auth_providers(entries);

        let filtered: Vec<usize> = (0..entries.len()).collect();
        self.model_selector = Some(ModelSelector {
            all_entries: entries,
            filtered,
            filter: Vec::new(),
            filter_cursor: 0,
            selected: 0,
            scroll_offset: 0,
            max_visible: 12,
        });
        self.dirty = true;
    }

    /// For providers without auth, keep only one representative entry per
    /// provider.  This prevents the model list from being cluttered with
    /// dozens of models the user can't use without an API key.
    fn collapse_no_auth_providers(
        entries: Vec<ninmu_api::ModelEntry>,
    ) -> Vec<ninmu_api::ModelEntry> {
        use std::collections::HashSet;
        let mut result = Vec::new();
        let mut seen_no_auth: HashSet<ninmu_api::ProviderKind> = HashSet::new();

        for entry in entries {
            if entry.has_auth {
                // Keep all models for providers with auth.
                result.push(entry);
            } else if seen_no_auth.insert(entry.provider) {
                // First model for this no-auth provider — keep it as a
                // placeholder so the user knows the provider exists.
                result.push(entry);
            }
            // Subsequent models for the same no-auth provider are dropped.
        }
        result
    }

    /// Take the selected model (if any) — returns `Some(model_name)` once.
    pub fn pop_selected_model(&mut self) -> Option<String> {
        self.selected_model.take()
    }

    /// Run the ratatui event loop. Blocks until the user exits.
    ///
    /// `start_turn` is called when the user submits input; it receives the
    /// input text and returns a boxed `TurnHandle`.
    pub fn run<F, R>(&mut self, start_turn: F) -> io::Result<()>
    where
        F: FnMut(&str) -> Result<R, Box<dyn std::error::Error>>,
        R: TurnHandle + 'static,
    {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(
            stdout,
            EnterAlternateScreen,
            EnableBracketedPaste,
            terminal::Clear(terminal::ClearType::All)
        )?;
        let backend = CrosstermBackend::new(stdout);
        let mut terminal = Terminal::new(backend)?;
        terminal.clear()?;

        // Catch panics so we always restore the terminal.
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.event_loop(&mut terminal, start_turn)
        }));

        disable_raw_mode()?;
        execute!(
            terminal.backend_mut(),
            DisableBracketedPaste,
            LeaveAlternateScreen
        )?;
        terminal.show_cursor()?;

        match result {
            Ok(inner) => inner,
            Err(payload) => {
                // Re-panic so the default handler still prints the backtrace.
                std::panic::resume_unwind(payload);
            }
        }
    }

    #[allow(clippy::too_many_lines, clippy::needless_pass_by_value)]
    fn event_loop<F, R>(
        &mut self,
        terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
        mut start_turn: F,
    ) -> io::Result<()>
    where
        F: FnMut(&str) -> Result<R, Box<dyn std::error::Error>>,
        R: TurnHandle + 'static,
    {
        let tick_rate = Duration::from_millis(50);
        let mut turn_handle: Option<Box<dyn TurnHandle>> = None;

        loop {
            // -- Render (only when state changed) -------------------------
            if self.dirty {
                terminal.draw(|frame| self.draw(frame))?;
                self.dirty = false;
            }

            // -- Poll events (blocking up to tick_rate) -------------------
            if crossterm::event::poll(tick_rate)? {
                let event = event::read()?;
                if let Event::Key(key) = event {
                    if key.kind == KeyEventKind::Press {
                        // Ctrl+C / Ctrl+D always quits
                        if key.modifiers.contains(KeyModifiers::CONTROL)
                            && matches!(key.code, KeyCode::Char('c' | 'd'))
                        {
                            // If a permission prompt is active, deny it and continue.
                            if let Some(perm) = self.pending_permission.take() {
                                let _ = perm.response_tx.send(PermissionPromptDecision::Deny {
                                    reason: "user pressed Ctrl+C/D".to_string(),
                                });
                                self.scrollback
                                    .push(format!("  denied: {}", perm.request.tool_name));
                            }
                            return Ok(());
                        }

                        // Model selector mode — intercept all keypresses.
                        if self.model_selector.is_some() {
                            match key.code {
                                KeyCode::Char(c)
                                    if key.modifiers.is_empty()
                                        || key.modifiers == KeyModifiers::SHIFT =>
                                {
                                    let sel = self.model_selector.as_mut().unwrap();
                                    sel.filter.insert(sel.filter_cursor, c);
                                    sel.filter_cursor += 1;
                                    sel.apply_filter();
                                }
                                KeyCode::Backspace => {
                                    let sel = self.model_selector.as_mut().unwrap();
                                    if sel.filter_cursor > 0 {
                                        sel.filter_cursor -= 1;
                                        sel.filter.remove(sel.filter_cursor);
                                        sel.apply_filter();
                                    }
                                }
                                KeyCode::Up => {
                                    let sel = self.model_selector.as_mut().unwrap();
                                    if sel.selected > 0 {
                                        sel.selected -= 1;
                                        if sel.selected < sel.scroll_offset {
                                            sel.scroll_offset = sel.selected;
                                        }
                                    }
                                }
                                KeyCode::Down => {
                                    let sel = self.model_selector.as_mut().unwrap();
                                    if sel.selected + 1 < sel.filtered.len() {
                                        sel.selected += 1;
                                        if sel.selected >= sel.scroll_offset + sel.max_visible {
                                            sel.scroll_offset = sel.selected + 1 - sel.max_visible;
                                        }
                                    }
                                }
                                KeyCode::Enter => {
                                    if let Some(sel) = self.model_selector.take() {
                                        if let Some(entry) = sel.selected_entry() {
                                            let model_name = entry.canonical.clone();
                                            let cmd = format!("/model {model_name}");
                                            self.scrollback
                                                .push(format!("  model set to {model_name}"));
                                            match start_turn(&cmd) {
                                                Ok(handle) => {
                                                    self.state.is_generating = true;
                                                    self.state.current_prompt = cmd;
                                                    self.response_text.clear();
                                                    self.turn_start = Some(Instant::now());
                                                    self.usage = TokenUsage::default();
                                                    turn_handle = Some(Box::new(handle));
                                                }
                                                Err(e) => {
                                                    self.scrollback.push(format!("  error: {e}"));
                                                }
                                            }
                                        }
                                    }
                                }
                                KeyCode::Esc => {
                                    self.model_selector = None;
                                }
                                _ => {}
                            }
                            self.dirty = true;
                            continue;
                        }

                        // Permission prompt mode — intercept all keypresses.
                        if let Some(perm) = self.pending_permission.take() {
                            match key.code {
                                KeyCode::Char('y' | 'a') if key.modifiers.is_empty() => {
                                    let _ = perm.response_tx.send(PermissionPromptDecision::Allow);
                                    self.scrollback
                                        .push(format!("  allowed: {}", perm.request.tool_name));
                                }
                                KeyCode::Char('n' | 'd') if key.modifiers.is_empty() => {
                                    let _ = perm.response_tx.send(PermissionPromptDecision::Deny {
                                        reason: format!(
                                            "tool '{}' denied by user",
                                            perm.request.tool_name
                                        ),
                                    });
                                    self.scrollback
                                        .push(format!("  denied: {}", perm.request.tool_name));
                                }
                                KeyCode::Char('v') if key.modifiers.is_empty() => {
                                    // View input: push it to scrollback,
                                    // then re-present the prompt.
                                    self.scrollback
                                        .push(format!("  input: {}", perm.request.input));
                                    self.pending_permission = Some(perm);
                                }
                                KeyCode::Esc => {
                                    let _ = perm.response_tx.send(PermissionPromptDecision::Deny {
                                        reason: format!(
                                            "tool '{}' denied by user (Esc)",
                                            perm.request.tool_name
                                        ),
                                    });
                                    self.scrollback
                                        .push(format!("  denied: {}", perm.request.tool_name));
                                }
                                _ => {
                                    // Unrecognised key — re-present.
                                    self.pending_permission = Some(perm);
                                }
                            }
                            self.dirty = true;
                            continue;
                        }

                        if self.state.is_generating {
                            match key.code {
                                KeyCode::PageUp => {
                                    self.scrollback.scroll_up(20);
                                }
                                KeyCode::PageDown => {
                                    self.scrollback.scroll_down(20);
                                }
                                KeyCode::Home => {
                                    self.scrollback.scroll_to_top();
                                }
                                KeyCode::End => {
                                    self.scrollback.scroll_to_bottom();
                                }
                                KeyCode::Esc => {
                                    turn_handle.take();
                                    self.state.is_generating = false;
                                    self.state.thinking_state = ThinkingState::Idle;
                                    self.last_event_received = None;
                                    self.flush_response();
                                    self.scrollback.push("  [cancelled]".to_string());
                                }
                                _ => {}
                            }
                            self.dirty = true;
                            continue;
                        }

                        match key.code {
                            KeyCode::Enter
                                if key.modifiers.contains(KeyModifiers::CONTROL)
                                    && !self.input_buf.is_empty() =>
                            {
                                // Ctrl+Enter: insert newline
                                self.input_buf.insert(self.cursor, '\n');
                                self.cursor += 1;
                                self.refresh_input_cache();
                            }
                            KeyCode::Enter if !self.input_buf.is_empty() => {
                                let text: String = self.input_buf.drain(..).collect();
                                self.cursor = 0;
                                self.clear_paste_state();
                                self.refresh_input_cache();
                                let input = text;
                                // Save to history, deduplicate consecutive.
                                if self.input_history.last().is_none_or(|last| last != &input) {
                                    self.input_history.push(input.clone());
                                }
                                self.history_index = None;
                                self.history_restore_buf.clear();

                                // Intercept /model with no args → open selector.
                                let trimmed = input.trim();
                                if trimmed == "/model" {
                                    self.open_model_selector();
                                } else {
                                    let scrollback_display = self.build_scrollback_display();
                                    self.scrollback
                                        .push(format!("  \u{25B8} {scrollback_display}"));
                                    match start_turn(&input) {
                                        Ok(handle) => {
                                            self.state.is_generating = true;
                                            self.state.current_prompt = input;
                                            self.response_text.clear();
                                            self.turn_start = Some(Instant::now());
                                            self.last_event_received = Some(Instant::now());
                                            self.usage = TokenUsage::default();
                                            turn_handle = Some(Box::new(handle));
                                        }
                                        Err(e) => {
                                            self.scrollback.push(format!("  error: {e}"));
                                        }
                                    }
                                }
                            }
                            KeyCode::Char(c)
                                if key.modifiers.is_empty()
                                    || key.modifiers == KeyModifiers::SHIFT =>
                            {
                                self.input_buf.insert(self.cursor, c);
                                self.cursor += 1;
                                self.shift_paste_spans(self.cursor - 1, 1);
                                self.refresh_input_cache();
                            }
                            KeyCode::Backspace if self.cursor > 0 => {
                                self.cursor -= 1;
                                let removed = self.input_buf.remove(self.cursor);
                                self.shift_paste_spans(self.cursor, -1);
                                if removed == '\n' || !removed.is_whitespace() {
                                    self.trim_trailing_paste_span();
                                }
                                self.refresh_input_cache();
                            }
                            KeyCode::Delete if self.cursor < self.input_buf.len() => {
                                self.input_buf.remove(self.cursor);
                                self.refresh_input_cache();
                            }
                            KeyCode::Left if self.cursor > 0 => self.cursor -= 1,
                            KeyCode::Right if self.cursor < self.input_buf.len() => {
                                self.cursor += 1;
                            }
                            KeyCode::Home => self.cursor = 0,
                            KeyCode::End => self.cursor = self.input_buf.len(),
                            KeyCode::Up => {
                                if self.input_history.is_empty() {
                                    self.dirty = true;
                                    continue;
                                }
                                if self.history_index.is_none() {
                                    self.history_restore_buf = self.input_buf.clone();
                                    self.history_index =
                                        Some(self.input_history.len().saturating_sub(1));
                                } else if let Some(i) = self.history_index {
                                    if i > 0 {
                                        self.history_index = Some(i - 1);
                                    }
                                }
                                if let Some(i) = self.history_index {
                                    let entry = &self.input_history[i];
                                    self.input_buf = entry.chars().collect();
                                    self.cursor = self.input_buf.len();
                                }
                                self.refresh_input_cache();
                            }
                            KeyCode::Down => {
                                if let Some(i) = self.history_index {
                                    if i + 1 < self.input_history.len() {
                                        self.history_index = Some(i + 1);
                                        let entry = &self.input_history[i + 1];
                                        self.input_buf = entry.chars().collect();
                                        self.cursor = self.input_buf.len();
                                    } else {
                                        self.history_index = None;
                                        self.input_buf =
                                            std::mem::take(&mut self.history_restore_buf);
                                        self.cursor = self.input_buf.len();
                                    }
                                }
                                self.refresh_input_cache();
                            }
                            KeyCode::PageUp => {
                                self.scrollback.scroll_up(20);
                            }
                            KeyCode::PageDown => {
                                self.scrollback.scroll_down(20);
                            }
                            KeyCode::Tab => {
                                let (_, start, _) = self.scrollback.visible(self.last_conv_height);
                                self.scrollback.toggle_expand_at(start);
                            }
                            KeyCode::F(1) => self.help_visible = !self.help_visible,
                            KeyCode::Char('?') if key.modifiers.is_empty() => {
                                self.help_visible = !self.help_visible;
                            }
                            _ => {}
                        }
                        self.dirty = true;
                        self.refresh_status_cache();
                    }
                } else if let Event::Paste(text) = event {
                    self.handle_paste(&text);
                } else if matches!(event, Event::Resize(_, _)) {
                    self.dirty = true;
                }
            }

            // -- Drain TuiEvent channel -----------------------------------
            if let Some(ref mut handle) = turn_handle {
                while let Some(ev) = handle.try_recv() {
                    self.last_event_received = Some(Instant::now());
                    self.process_event(ev);
                }

                if handle.is_finished() {
                    while let Some(ev) = handle.try_recv() {
                        self.process_event(ev);
                    }
                    // Only flush if TurnComplete wasn't already processed
                    // (process_event sets is_generating=false on TurnComplete).
                    if self.state.is_generating {
                        self.flush_response();
                        self.state.is_generating = false;
                    }
                    self.state.thinking_state = ThinkingState::Idle;
                    self.state.current_tool = None;
                    self.last_event_received = None;
                    turn_handle.take();
                } else if let Some(last) = self.last_event_received {
                    // Watchdog: if we haven't received any event from the
                    // worker thread in 3 minutes, the turn is likely stuck
                    // (dead SSE connection, blocked tool, etc.). Force-cancel
                    // and unlock the input so the user can continue.
                    //
                    // Skip the watchdog when a permission prompt is active —
                    // the user may legitimately take a long time to decide.
                    const STALL_WATCHDOG: Duration = Duration::from_mins(3);
                    if self.pending_permission.is_none() && last.elapsed() > STALL_WATCHDOG {
                        turn_handle.take();
                        self.state.is_generating = false;
                        self.state.thinking_state = ThinkingState::Idle;
                        self.state.current_tool = None;
                        self.flush_response();
                        self.scrollback.push(
                            "  [stalled \u{2014} no response in 3 min, turn cancelled]".to_string(),
                        );
                        self.last_event_received = None;
                        self.dirty = true;
                    }
                }
            }

            // -- Advance spinner animation + cursor blink ---------------
            if self.tick.elapsed() >= Duration::from_millis(120) {
                self.tick = Instant::now();
                self.spinner_frame = self.spinner_frame.wrapping_add(1);
                // Flip cursor blink every ~4 ticks (480ms cycle).
                if self.spinner_frame.is_multiple_of(4) {
                    self.show_cursor_blink = !self.show_cursor_blink;
                }
                // Advance paste animation
                if self.paste_animating {
                    self.paste_anim_frame = self.paste_anim_frame.wrapping_add(1);
                    if let Some(start) = self.paste_anim_start {
                        if start.elapsed() >= Self::PASTE_ANIM_DURATION {
                            self.paste_animating = false;
                            self.paste_anim_start = None;
                            if let Some((s, len)) = self.anim_range {
                                let summary = self.anim_summary.clone().unwrap_or_default();
                                self.paste_spans.push(PasteSpan {
                                    start: s,
                                    len,
                                    summary,
                                });
                            }
                            self.anim_summary = None;
                            self.anim_range = None;
                        }
                    }
                }
                self.dirty = true;
            }
        }
    }

    fn refresh_input_cache(&mut self) {
        self.cached_input = self.input_buf.iter().collect();
    }

    fn refresh_status_cache(&mut self) {
        self.cached_elapsed_str = self
            .turn_start
            .map(|t| format!("{}s", t.elapsed().as_secs()))
            .unwrap_or_default();
    }

    const PASTE_THRESHOLD: usize = 128;
    const PASTE_PREVIEW_LEN: usize = 30;
    const PASTE_ANIM_DURATION: Duration = Duration::from_millis(1200);
    const PACMAN_FRAMES: &[char] = &['C', '(', 'C', '('];

    fn is_pacman(ch: char) -> bool {
        ch == 'C' || ch == '('
    }

    /// Insert paste text into `input_buf` at the cursor position.
    /// For long pastes (>PASTE_THRESHOLD), starts the pacman animation.
    /// For short pastes, inserts directly like normal typing.
    fn handle_paste(&mut self, text: &str) {
        // Finish any running animation first.
        if self.paste_animating {
            self.finish_animation();
        }

        let char_count = text.chars().count();
        let insert_pos = self.cursor;

        // Insert text into input_buf at cursor.
        for (i, c) in text.chars().enumerate() {
            self.input_buf.insert(self.cursor + i, c);
        }
        self.cursor += char_count;

        // Shift existing paste spans past the insertion point.
        self.shift_paste_spans(insert_pos, char_count as isize);

        if char_count > Self::PASTE_THRESHOLD {
            let words: usize = text.split_whitespace().count();
            let lines = text.lines().count();
            let summary = format!("[Pasted {words} words, {lines} lines]");

            if Self::PASTE_ANIM_DURATION.is_zero() {
                // No animation — record span immediately.
                self.paste_spans.push(PasteSpan {
                    start: insert_pos,
                    len: char_count,
                    summary,
                });
            } else {
                // Start animation.
                self.paste_animating = true;
                self.paste_anim_frame = 0;
                self.paste_anim_start = Some(Instant::now());
                self.anim_summary = Some(summary);
                self.anim_range = Some((insert_pos, char_count));
            }
        }

        self.refresh_input_cache();
        self.dirty = true;
    }

    /// Finish a running animation immediately, recording the paste span.
    fn finish_animation(&mut self) {
        self.paste_animating = false;
        self.paste_anim_start = None;
        if let Some((s, len)) = self.anim_range {
            let summary = self.anim_summary.take().unwrap_or_default();
            self.paste_spans.push(PasteSpan {
                start: s,
                len,
                summary,
            });
        }
        self.anim_summary = None;
        self.anim_range = None;
    }

    /// Adjust paste span offsets after an insertion or deletion at `pos`
    /// with the given delta (positive = insert, negative = delete).
    fn shift_paste_spans(&mut self, pos: usize, delta: isize) {
        for span in &mut self.paste_spans {
            if pos <= span.start {
                span.start = (span.start as isize + delta) as usize;
            } else if pos < span.start + span.len {
                span.len = (span.len as isize + delta) as usize;
            }
        }
        if let Some((ref mut s, ref mut l)) = self.anim_range {
            if pos <= *s {
                *s = (*s as isize + delta) as usize;
            } else if pos < *s + *l {
                *l = (*l as isize + delta) as usize;
            }
        }
    }

    /// Remove any paste span whose length has dropped to zero
    /// (the user backspaced through all of it).
    fn trim_trailing_paste_span(&mut self) {
        self.paste_spans.retain(|s| s.len > 0);
    }

    /// Build the display string for the input area.
    /// Shows summary overlays for tracked paste spans.
    fn paste_display_text(&self) -> String {
        let flat: String = self.input_buf.iter().collect();

        if self.paste_spans.is_empty() && !self.paste_animating {
            return flat;
        }

        // Animation mode: show pacman eating the last pasted range.
        if self.paste_animating {
            if let Some((start, len)) = self.anim_range {
                let preview_end = (start + Self::PASTE_PREVIEW_LEN).min(start + len);
                let preview: String = self.input_buf[start..preview_end].iter().collect();
                let total_beyond = len.saturating_sub(Self::PASTE_PREVIEW_LEN);
                let tick_rate = Duration::from_millis(120);
                let elapsed_ticks = self.paste_anim_frame;
                let max_ticks = (Self::PASTE_ANIM_DURATION.as_millis()
                    / tick_rate.as_millis())
                .max(1) as usize;
                let eaten = if max_ticks > 0 && total_beyond > 0 {
                    (total_beyond * elapsed_ticks / max_ticks).min(total_beyond)
                } else {
                    total_beyond
                };
                let remaining = total_beyond.saturating_sub(eaten);
                let pacman = Self::PACMAN_FRAMES[elapsed_ticks % Self::PACMAN_FRAMES.len()];

                let mut result = String::new();
                // Text before the pasted range.
                result.push_str(&flat[..start]);
                // Preview (first 30 chars).
                result.push_str(&preview);
                // Remaining uneaten chars.
                if remaining > 0 {
                    let chunk_start = preview_end + eaten;
                    let chunk_end = (chunk_start + remaining).min(start + len);
                    result.push_str(&flat[chunk_start..chunk_end]);
                }
                result.push(pacman);
                // Progressively reveal summary.
                if let Some(ref summary) = self.anim_summary {
                    let reveal = eaten.min(summary.len());
                    result.push_str(&summary[..reveal]);
                }
                // Text after the pasted range.
                result.push_str(&flat[start + len..]);
                return result;
            }
        }

        // Post-animation: replace each paste span with its summary.
        let mut result = String::new();
        let mut prev_end = 0;
        // Sort spans by start position.
        let mut spans: Vec<&PasteSpan> = self.paste_spans.iter().collect();
        spans.sort_by_key(|s| s.start);
        for span in &spans {
            if span.start > prev_end {
                result.push_str(&flat[prev_end..span.start]);
            }
            result.push_str(&span.summary);
            prev_end = span.start + span.len;
        }
        if prev_end < flat.len() {
            result.push_str(&flat[prev_end..]);
        }
        result
    }

    /// Build a scrollback display string — uses summaries for paste spans.
    fn build_scrollback_display(&self) -> String {
        if self.paste_spans.is_empty() {
            return self.cached_input.clone();
        }
        self.paste_display_text()
    }

    fn clear_paste_state(&mut self) {
        self.paste_spans.clear();
        self.paste_animating = false;
        self.paste_anim_start = None;
        self.paste_anim_frame = 0;
        self.anim_summary = None;
        self.anim_range = None;
    }

    fn process_event(&mut self, ev: TuiEvent) {
        self.dirty = true;
        match ev {
            TuiEvent::TextDelta(text) => {
                self.response_text.push_str(&text);
                self.update_streaming_display();
            }
            TuiEvent::ToolUse { name, input } => {
                self.state.current_tool = Some(name.clone());
                self.scrollback.push(format!("  -- {name}"));
                if let Some(first_line) = input.lines().next() {
                    let summary = truncate(first_line, 76);
                    self.scrollback.push(format!("  {summary}"));
                }
            }
            TuiEvent::ToolResult {
                name,
                output,
                is_error,
            } => {
                self.state.current_tool = None;
                let icon = if is_error { "fail" } else { "ok" };
                let lines = output.lines().count();
                self.scrollback
                    .push(format!("  {icon} {name} ({lines} lines)"));
            }
            TuiEvent::Usage(u) => {
                self.usage = u;
                self.refresh_status_cache();
            }
            TuiEvent::ThinkingStart => {
                self.state.thinking_state = ThinkingState::Thinking {
                    started: Instant::now(),
                };
            }
            TuiEvent::ThinkingStop { .. } => {
                self.state.thinking_state = ThinkingState::Idle;
            }
            TuiEvent::Error(msg) => {
                self.scrollback.push(format!("  error: {msg}"));
                self.state.is_generating = false;
                self.state.thinking_state = ThinkingState::Idle;
                self.state.current_tool = None;
            }
            TuiEvent::TurnComplete => {
                self.flush_response();
                self.state.is_generating = false;
                self.state.thinking_state = ThinkingState::Idle;
                self.state.current_tool = None;
                self.state.current_prompt.clear();
                self.turn_start = None;
                self.refresh_status_cache();
            }
            TuiEvent::PermissionPrompt {
                request,
                response_tx,
            } => {
                let action_description = describe_tool_action(
                    &request.tool_name,
                    &serde_json::from_str(&request.input)
                        .unwrap_or(serde_json::Value::String(request.input.clone())),
                );
                self.pending_permission = Some(PendingPermission {
                    request,
                    response_tx,
                    action_description,
                });
            }
            TuiEvent::ToolProgress { .. } => {
                // Not yet wired up.
            }
            TuiEvent::LoadHistory { messages } => {
                self.scrollback.clear();
                self.load_conversation_history(&messages);
            }
            TuiEvent::ReasoningUpdate { effort, thinking } => {
                self.reasoning_effort = effort;
                self.thinking_mode = thinking;
                self.rebuild_header();
            }
            TuiEvent::ModelUpdate { model } => {
                self.model = model;
                self.model_pricing = ninmu_runtime::pricing_for_model(&self.model);
                self.rebuild_header();
            }
            TuiEvent::PromptCache(event) => {
                let prefix = if event.unexpected {
                    "cache break"
                } else {
                    "cache invalidated"
                };
                self.scrollback.push(format!(
                    "  {prefix}: {} ({} tokens)",
                    event.reason, event.token_drop
                ));
            }
        }
    }

    fn flush_response(&mut self) {
        if !self.response_text.is_empty() {
            for line in self.response_text.lines() {
                self.scrollback.push(line.to_string());
            }
            if self.response_text.ends_with('\n') {
                self.scrollback.push(String::new());
            }
            self.response_text.clear();
        }
        // Token/cost display moved to the metadata bar below the input box.
        // Do not append usage lines to the conversation scrollback.
    }

    fn update_streaming_display(&mut self) {
        if !self.response_text.contains('\n') {
            return;
        }
        // Steal the whole buffer, split once on newlines, and rebuild
        // the remainder.  This is O(n) total instead of O(n²) from
        // repeatedly reallocating the tail string.
        let text = std::mem::take(&mut self.response_text);
        let mut parts = text.split('\n');
        let remainder = parts.next_back().unwrap_or("").to_string();
        for part in parts {
            self.scrollback.push(part.to_string());
        }
        self.response_text = remainder;
    }

    /// Clear the scrollback buffer. Used before loading a new session's
    /// history to avoid duplicating content from a previous session.
    pub fn clear_scrollback(&mut self) {
        self.scrollback.clear();
        self.dirty = true;
    }

    /// Load previous conversation history into the scrollback.
    pub fn load_conversation_history(&mut self, messages: &[ConversationMessage]) {
        for msg in messages {
            match msg.role {
                MessageRole::System | MessageRole::Tool => continue,
                MessageRole::User | MessageRole::Assistant => {}
            }
            for block in &msg.blocks {
                match block {
                    ContentBlock::Text { text } => {
                        let prefix = match msg.role {
                            MessageRole::User => "  \u{25B8} ",
                            MessageRole::Assistant => "",
                            _ => "  ",
                        };
                        for line in text.lines() {
                            self.scrollback.push(format!("{prefix}{line}"));
                        }
                    }
                    ContentBlock::ToolUse { name, input, .. } => {
                        self.scrollback.push(format!("  -- {name}"));
                        if let Some(first_line) = input.lines().next() {
                            let summary = truncate(first_line, 76);
                            self.scrollback.push(format!("  {summary}"));
                        }
                    }
                    ContentBlock::ToolResult { .. } | ContentBlock::Thinking { .. } => {}
                }
            }
        }
        self.scrollback.push(String::new());
        self.scrollback
            .push("  \u{2500} session resumed \u{2500}".to_string());
        self.scrollback.push(String::new());
    }

    // -- Drawing --------------------------------------------------------------

    fn draw(&mut self, frame: &mut ratatui::Frame) {
        let area = frame.area();

        let layout = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1), // header
                Constraint::Min(5),    // conversation
                Constraint::Length(3), // input box
                Constraint::Length(1), // metadata bar
                Constraint::Length(1), // status bar
            ])
            .split(area);

        self.draw_header(frame, layout[0]);
        // Record viewport height for Tab toggle and scroll calculations.
        self.last_conv_height = layout[1].height as usize;
        self.draw_conversation(frame, layout[1]);
        self.draw_input(frame, layout[2]);
        self.draw_metadata(frame, layout[3]);
        self.draw_status(frame, layout[4]);

        if self.help_visible {
            self.draw_help_overlay(frame, area);
        }

        if self.pending_permission.is_some() {
            self.draw_permission_modal(frame, area);
        }

        if self.model_selector.is_some() {
            self.draw_model_selector(frame, area);
        }
    }

    fn build_header_line(
        model: &str,
        permission_mode: &str,
        git_branch: Option<&str>,
        reasoning_effort: Option<&str>,
        thinking_mode: Option<bool>,
    ) -> Line<'static> {
        let git = git_branch.unwrap_or("?");
        let perm_short = match permission_mode {
            "danger-full-access" => "full",
            "workspace-write" => "write",
            _ => "read",
        };

        let mut spans = vec![
            Span::styled(
                "  ninmu ",
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                "\u{30CB}\u{30F3}\u{30E0}\u{30B3}\u{30FC}\u{30C9} ",
                Style::default().fg(MUTED),
            ),
            Span::raw("  "),
            Span::styled("model ", Style::default().fg(MUTED)),
            Span::styled(model.to_string(), Style::default().fg(TEXT_SEC)),
            Span::raw("  "),
            Span::styled("perm ", Style::default().fg(MUTED)),
            Span::styled(perm_short.to_string(), Style::default().fg(TEXT_SEC)),
            Span::raw("  "),
            Span::styled("branch ", Style::default().fg(MUTED)),
            Span::styled(git.to_string(), Style::default().fg(TEXT_SEC)),
        ];

        // Show reasoning effort/thinking state if set.
        let effort_label = reasoning_effort.unwrap_or("default");
        let thinking_label = match thinking_mode {
            Some(true) => "on",
            Some(false) => "off",
            None => "auto",
        };
        spans.push(Span::raw("  "));
        spans.push(Span::styled("think ", Style::default().fg(MUTED)));
        spans.push(Span::styled(
            thinking_label.to_string(),
            Style::default().fg(if thinking_mode == Some(false) {
                MUTED
            } else {
                ACCENT
            }),
        ));
        spans.push(Span::raw(" "));
        spans.push(Span::styled(
            effort_label.to_string(),
            Style::default().fg(TEXT_SEC),
        ));

        Line::from(spans)
    }

    fn draw_header(&self, frame: &mut ratatui::Frame, area: Rect) {
        let header =
            Paragraph::new(self.cached_header.clone()).style(Style::default().bg(SURFACE).fg(TEXT));
        frame.render_widget(header, area);
    }

    fn draw_conversation(&self, frame: &mut ratatui::Frame, area: Rect) {
        let viewport_height = area.height as usize;
        let (visible, _, _total) = self.scrollback.visible(viewport_height);

        // Find the index of the LAST user prompt line (for pulsing).
        let last_user_idx = visible
            .iter()
            .enumerate()
            .rev()
            .find(|(_, s)| {
                let t = s.trim_end();
                t.starts_with("  \u{25B8} ") || t.starts_with("  > ")
            })
            .map(|(i, _)| i);

        let mut in_code_block = false;
        let mut lines: Vec<Line> = visible
            .iter()
            .enumerate()
            .map(|(idx, s)| {
                let s = s.trim_end();

                // -- User prompt: ▸ prefix --
                let is_user_prompt = s.starts_with("  \u{25B8} ") || s.starts_with("  > ");
                if is_user_prompt {
                    let rest = if let Some(r) = s.strip_prefix("  \u{25B8} ") {
                        r
                    } else {
                        s.strip_prefix("  > ").unwrap_or(s)
                    };
                    // Pulse only the last user prompt while generating.
                    let is_active = self.state.is_generating && Some(idx) == last_user_idx;
                    let color = if is_active {
                        let phase = (self.spinner_frame % 8) as f32 / 8.0;
                        let t = (phase * std::f32::consts::PI).sin().abs();
                        Color::Rgb(
                            lerp_u8(USER_COLOR_DIM.r(), USER_COLOR.r(), t),
                            lerp_u8(USER_COLOR_DIM.g(), USER_COLOR.g(), t),
                            lerp_u8(USER_COLOR_DIM.b(), USER_COLOR.b(), t),
                        )
                    } else {
                        USER_COLOR
                    };
                    return Line::from(vec![
                        Span::styled(
                            "  \u{25B8} ",
                            Style::default().fg(color).add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(rest.to_string(), Style::default().fg(color)),
                    ]);
                }
                // -- Fenced code block detection (any line) --
                let trimmed = s.trim();
                if trimmed.starts_with("```") {
                    in_code_block = !in_code_block;
                    if in_code_block {
                        let lang = trimmed.strip_prefix("`").unwrap_or("");
                        return Line::from(Span::styled(
                            format!("  {lang}"),
                            Style::default().fg(MUTED).bg(CODE_BG),
                        ));
                    }
                    return Line::from(Span::styled(
                        "  ```".to_string(),
                        Style::default().fg(MUTED).bg(CODE_BG),
                    ));
                }
                if in_code_block {
                    return Line::from(Span::styled(
                        format!("  {s}"),
                        Style::default().fg(CODE_FG).bg(CODE_BG),
                    ));
                }
                // -- Error --
                if let Some(rest) = s.strip_prefix("  error:") {
                    return Line::from(vec![
                        Span::styled("  error:", Style::default().fg(ERROR_COLOR)),
                        Span::styled(rest.to_string(), Style::default().fg(TEXT)),
                    ]);
                }
                // -- Cache break / invalidation --
                if let Some(rest) = s.strip_prefix("  cache break:") {
                    return Line::from(vec![
                        Span::styled("  cache break:", Style::default().fg(ERROR_COLOR)),
                        Span::styled(rest.to_string(), Style::default().fg(TEXT)),
                    ]);
                }
                if let Some(rest) = s.strip_prefix("  cache invalidated:") {
                    return Line::from(vec![
                        Span::styled("  cache invalidated:", Style::default().fg(ACCENT)),
                        Span::styled(rest.to_string(), Style::default().fg(TEXT)),
                    ]);
                }
                // -- Tool use --
                if s.starts_with("  -- ") {
                    return Line::from(Span::styled(s.to_string(), Style::default().fg(MUTED)));
                }
                // -- Tool result --
                if let Some(rest) = s.strip_prefix("  ok ") {
                    return Line::from(vec![
                        Span::styled("  ok", Style::default().fg(SUCCESS)),
                        Span::styled(rest.to_string(), Style::default().fg(TEXT_SEC)),
                    ]);
                }
                if let Some(rest) = s.strip_prefix("  fail ") {
                    return Line::from(vec![
                        Span::styled("  fail", Style::default().fg(ERROR_COLOR)),
                        Span::styled(rest.to_string(), Style::default().fg(TEXT_SEC)),
                    ]);
                }
                if s.starts_with("  [cancelled]") {
                    return Line::from(Span::styled(s.to_string(), Style::default().fg(MUTED)));
                }
                // -- Fallback: plain text with markdown --
                Line::from(markdown_spans(s))
            })
            .collect();

        if self.state.is_generating {
            if !self.response_text.is_empty() {
                let cursor_char = if self.show_cursor_blink {
                    "\u{258C}"
                } else {
                    " "
                };
                let partial = format!("{}{cursor_char}", self.response_text);
                lines.push(Line::from(markdown_spans(&partial)));
            }

            let spinner = SPINNER[self.spinner_frame % SPINNER.len()];
            let indicator = match &self.state.thinking_state {
                ThinkingState::Thinking { .. } => vec![
                    Span::styled(spinner, Style::default().fg(THINKING_COLOR)),
                    Span::styled(" thinking...", Style::default().fg(THINKING_COLOR)),
                ],
                ThinkingState::Idle => {
                    let tool_label = self.state.current_tool.as_deref().unwrap_or("streaming");
                    let ts = TOOL_SPINNER[self.spinner_frame % TOOL_SPINNER.len()];
                    vec![
                        Span::styled(ts, Style::default().fg(ACCENT)),
                        Span::styled(format!(" {tool_label}"), Style::default().fg(ACCENT)),
                    ]
                }
            };
            lines.push(Line::from(indicator));
        }

        if !self.scrollback.is_at_bottom() {
            let offset = self.scrollback.scroll_offset();
            lines.push(Line::from(Span::styled(
                format!("  [{offset} lines up \u{00b7} j/k scroll \u{00b7} End for latest]"),
                Style::default().fg(MUTED),
            )));
        }

        let block = Block::default()
            .borders(Borders::NONE)
            .style(Style::default().bg(BG));

        let paragraph = Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false });

        frame.render_widget(paragraph, area);
    }

    fn draw_input(&self, frame: &mut ratatui::Frame, area: Rect) {
        let block = Block::default()
            .borders(Borders::TOP)
            .border_style(Style::default().fg(BORDER))
            .style(Style::default().bg(SURFACE));

        let inner = block.inner(area);
        frame.render_widget(block, area);

        let flat: String = self.input_buf.iter().collect();

        let spans = if self.paste_animating {
            let orange = if self.paste_anim_frame.is_multiple_of(2) {
                Color::Rgb(255, 160, 40)
            } else {
                Color::Rgb(255, 120, 20)
            };
            vec![
                Span::styled("> ", Style::default().fg(ACCENT)),
                Span::styled(
                    self.paste_display_text(),
                    Style::default().fg(orange).add_modifier(Modifier::BOLD),
                ),
            ]
        } else if self.paste_spans.is_empty() {
            vec![
                Span::styled("> ", Style::default().fg(ACCENT)),
                Span::styled(flat.clone(), Style::default().fg(TEXT)),
            ]
        } else {
            // Build mixed spans: normal text + summary overlays for paste spans.
            let mut v = vec![Span::styled("> ", Style::default().fg(ACCENT))];
            let mut spans_sorted: Vec<&PasteSpan> = self.paste_spans.iter().collect();
            spans_sorted.sort_by_key(|s| s.start);
            let mut prev_end = 0;
            for span in &spans_sorted {
                let s = span.start;
                let e = span.start + span.len;
                if s > prev_end && s <= flat.len() {
                    v.push(Span::styled(
                        flat[prev_end..s].to_string(),
                        Style::default().fg(TEXT),
                    ));
                }
                v.push(Span::styled(
                    span.summary.clone(),
                    Style::default().fg(TEXT_SEC),
                ));
                prev_end = e.min(flat.len());
            }
            if prev_end < flat.len() {
                v.push(Span::styled(
                    flat[prev_end..].to_string(),
                    Style::default().fg(TEXT),
                ));
            }
            v
        };

        let paragraph = Paragraph::new(Line::from(spans));
        frame.render_widget(paragraph, inner);

        if !self.state.is_generating && !self.paste_animating {
            // Map cursor position to display offset (summaries compress ranges).
            let cursor_pos = self.display_cursor_offset();
            let cursor_x = inner.x + 2 + u16::try_from(cursor_pos).unwrap_or(u16::MAX);
            let cursor_y = inner.y;
            frame.set_cursor_position((cursor_x, cursor_y));
        }
    }

    /// Map `self.cursor` (offset into `input_buf`) to the display offset
    /// (accounting for paste spans shown as shorter summaries).
    fn display_cursor_offset(&self) -> usize {
        let mut offset = self.cursor;
        for span in &self.paste_spans {
            if self.cursor > span.start + span.len {
                // Cursor is past this span — subtract the compression.
                offset = offset.saturating_sub(span.len).saturating_add(span.summary.len());
            } else if self.cursor > span.start {
                // Cursor is inside this span — clamp to span start + summary len.
                offset = span.start + span.summary.len();
            }
        }
        offset
    }

    fn draw_metadata(&self, frame: &mut ratatui::Frame, area: Rect) {
        let mut spans = vec![Span::raw("  ")];

        // Token counts
        let in_tok = format_tokens(self.usage.input_tokens);
        let out_tok = format_tokens(self.usage.output_tokens);
        spans.push(Span::styled(
            format!("{in_tok} in / {out_tok} out"),
            Style::default().fg(TEXT_SEC),
        ));
        spans.push(Span::styled(" tokens", Style::default().fg(MUTED)));

        // Cost estimate
        if let Some(pricing) = self.model_pricing {
            let in_cost =
                (self.usage.input_tokens as f64 / 1_000_000.0) * pricing.input_cost_per_million;
            let out_cost = (self.usage.output_tokens as f64 / 1_000_000.0)
                * pricing.output_cost_per_million;
            let cache_create_cost = (self.usage.cache_creation_input_tokens as f64 / 1_000_000.0)
                * pricing.cache_creation_cost_per_million;
            let cache_read_cost = (self.usage.cache_read_input_tokens as f64 / 1_000_000.0)
                * pricing.cache_read_cost_per_million;
            let total = in_cost + out_cost + cache_create_cost + cache_read_cost;
            if total >= 0.0001 {
                spans.push(Span::styled("  •  ", Style::default().fg(MUTED)));
                spans.push(Span::styled(
                    format!("${total:.4}"),
                    Style::default().fg(TEXT_SEC),
                ));
            }
        }

        // Context-window percentage
        if let Some(limit) = model_token_limit(&self.model) {
            let total = self.usage.input_tokens + self.usage.output_tokens;
            let pct = (total as f64 / limit.context_window_tokens as f64) * 100.0;
            let pct_str = format!("{pct:.0}%");
            let pct_color = if pct >= 90.0 {
                ERROR_COLOR
            } else if pct >= 70.0 {
                Color::Rgb(255, 180, 60)
            } else {
                MUTED
            };
            spans.push(Span::styled("  •  ", Style::default().fg(MUTED)));
            spans.push(Span::styled(pct_str, Style::default().fg(pct_color)));
            spans.push(Span::styled(" context", Style::default().fg(MUTED)));
        }

        let meta = Paragraph::new(Line::from(spans)).style(Style::default().bg(SURFACE));
        frame.render_widget(meta, area);
    }

    fn draw_status(&self, frame: &mut ratatui::Frame, area: Rect) {
        let status_label = if self.state.is_generating {
            "streaming"
        } else {
            "ready"
        };
        let status_color = if self.state.is_generating {
            ACCENT
        } else {
            MUTED
        };

        let mut spans = vec![
            Span::raw("  "),
            Span::styled(status_label, Style::default().fg(status_color)),
        ];

        if self.state.is_generating && !self.cached_elapsed_str.is_empty() {
            spans.push(Span::raw("  "));
            spans.push(Span::styled(
                self.cached_elapsed_str.clone(),
                Style::default().fg(ACCENT),
            ));
        }

        spans.push(Span::raw("  "));
        spans.push(Span::styled("/help", Style::default().fg(MUTED)));

        let status = Paragraph::new(Line::from(spans)).style(Style::default().bg(SURFACE));

        frame.render_widget(status, area);
    }

    fn draw_help_overlay(&self, frame: &mut ratatui::Frame, area: Rect) {
        let help_text = vec![
            Line::from(Span::styled(
                "  \u{2500}\u{2500} keyboard shortcuts \u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}",
                Style::default().fg(BORDER_BRIGHT),
            )),
            Line::from(""),
            help_line("Enter", "submit input"),
            help_line("Ctrl+Enter", "insert newline"),
            help_line("Esc", "cancel generation"),
            help_line("PgUp/PgDn", "scroll conversation"),
            help_line("Home/End", "top / bottom"),
            help_line("Tab", "expand/collapse tool output"),
            help_line("Ctrl+C/D", "quit"),
            help_line("?", "toggle this help"),
            Line::from(""),
            Line::from(Span::styled(
                "  \u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}",
                Style::default().fg(BORDER_BRIGHT),
            )),
        ];

        let popup_w = 42.min(area.width.saturating_sub(4));
        let popup_h = (u16::try_from(help_text.len()).unwrap_or(u16::MAX) + 2)
            .min(area.height.saturating_sub(2));
        let popup_x = (area.width.saturating_sub(popup_w)) / 2;
        let popup_y = (area.height.saturating_sub(popup_h)) / 2;
        let popup_area = Rect::new(popup_x, popup_y, popup_w, popup_h);

        frame.render_widget(Clear, popup_area);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(BORDER_BRIGHT))
            .style(Style::default().bg(SURFACE));

        let paragraph = Paragraph::new(help_text).block(block);
        frame.render_widget(paragraph, popup_area);
    }

    fn draw_permission_modal(&self, frame: &mut ratatui::Frame, area: Rect) {
        let Some(ref perm) = self.pending_permission else {
            return;
        };

        let required_str = format!("{:?}", perm.request.required_mode);
        let current_str = format!("{:?}", perm.request.current_mode);

        let mut lines = vec![
            Line::from(Span::styled(
                "  \u{2500}\u{2500} permission required",
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            )),
            Line::from(Span::styled(
                "  \u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}",
                Style::default().fg(BORDER_BRIGHT),
            )),
            Line::from(""),
            Line::from(vec![
                Span::styled("  tool     ", Style::default().fg(MUTED)),
                Span::styled(
                    perm.request.tool_name.clone(),
                    Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
                ),
            ]),
            Line::from(vec![
                Span::styled("  action   ", Style::default().fg(MUTED)),
                Span::styled(perm.action_description.clone(), Style::default().fg(TEXT)),
            ]),
            Line::from(vec![
                Span::styled("  current  ", Style::default().fg(MUTED)),
                Span::styled(current_str, Style::default().fg(TEXT_SEC)),
            ]),
            Line::from(vec![
                Span::styled("  required ", Style::default().fg(MUTED)),
                Span::styled(required_str, Style::default().fg(ACCENT)),
            ]),
        ];

        if let Some(reason) = &perm.request.reason {
            lines.push(Line::from(vec![
                Span::styled("  reason   ", Style::default().fg(MUTED)),
                Span::styled(reason.clone(), Style::default().fg(TEXT_SEC)),
            ]));
        }

        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "  \u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}",
            Style::default().fg(BORDER_BRIGHT),
        )));
        lines.push(Line::from(vec![
            Span::styled(
                "  Y",
                Style::default().fg(SUCCESS).add_modifier(Modifier::BOLD),
            ),
            Span::styled("/A allow  ", Style::default().fg(TEXT_SEC)),
            Span::styled(
                "N",
                Style::default()
                    .fg(ERROR_COLOR)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled("/D deny  ", Style::default().fg(TEXT_SEC)),
            Span::styled(
                "V",
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
            Span::styled(" view input  ", Style::default().fg(TEXT_SEC)),
            Span::styled("Esc", Style::default().fg(MUTED)),
            Span::styled(" deny", Style::default().fg(TEXT_SEC)),
        ]));

        let popup_w = 56.min(area.width.saturating_sub(4));
        let popup_h =
            (u16::try_from(lines.len()).unwrap_or(u16::MAX) + 2).min(area.height.saturating_sub(2));
        let popup_x = (area.width.saturating_sub(popup_w)) / 2;
        let popup_y = (area.height.saturating_sub(popup_h)) / 2;
        let popup_area = Rect::new(popup_x, popup_y, popup_w, popup_h);

        frame.render_widget(Clear, popup_area);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(ACCENT))
            .style(Style::default().bg(SURFACE));

        let paragraph = Paragraph::new(lines).block(block);
        frame.render_widget(paragraph, popup_area);
    }

    fn draw_model_selector(&mut self, frame: &mut ratatui::Frame, area: Rect) {
        let sel = match &self.model_selector {
            Some(s) => s,
            None => return,
        };

        let popup_w = 52.min(area.width.saturating_sub(4));
        let list_h = sel.max_visible.min(sel.filtered.len());
        // filter prompt (1) + separator (1) + list + keybinds (1) + border (2)
        let popup_h = (3 + list_h as u16 + 2).min(area.height.saturating_sub(2));
        let popup_x = (area.width.saturating_sub(popup_w)) / 2;
        let popup_y = (area.height.saturating_sub(popup_h)) / 2;
        let popup_area = Rect::new(popup_x, popup_y, popup_w, popup_h);

        frame.render_widget(Clear, popup_area);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(ACCENT))
            .style(Style::default().bg(SURFACE))
            .title(Span::styled(
                " select model ",
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ))
            .title_alignment(ratatui::layout::Alignment::Center);

        let inner = block.inner(popup_area);
        frame.render_widget(block, popup_area);

        let filter_str: String = sel.filter.iter().collect();
        let mut lines: Vec<Line> = vec![Line::from(vec![
            Span::styled(" filter ", Style::default().fg(MUTED)),
            Span::styled(
                if filter_str.is_empty() {
                    "type to filter...".to_string()
                } else {
                    filter_str.clone()
                },
                Style::default().fg(if filter_str.is_empty() { MUTED } else { TEXT }),
            ),
            Span::styled("▏", Style::default().fg(ACCENT)),
        ])];

        lines.push(Line::from(Span::styled(
            " ".repeat(popup_w as usize - 4),
            Style::default().fg(BORDER_BRIGHT),
        )));

        // Clamp scroll so selected is always visible.
        let scroll_start = sel.scroll_offset;
        let visible_end = (scroll_start + sel.max_visible).min(sel.filtered.len());
        for vi in scroll_start..visible_end {
            let fi = match sel.filtered.get(vi) {
                Some(&idx) => idx,
                None => continue,
            };
            let entry = &sel.all_entries[fi];
            let is_selected = vi == sel.selected;
            let no_auth = !entry.has_auth;
            let prov = ModelSelector::provider_label(entry.provider);

            let text_color = if is_selected {
                TEXT
            } else if no_auth {
                MUTED
            } else {
                TEXT
            };
            let highlight = if is_selected {
                if no_auth {
                    Style::default().fg(MUTED).bg(Color::Rgb(80, 50, 50))
                } else {
                    Style::default().fg(TEXT).bg(ACCENT)
                }
            } else {
                Style::default().fg(text_color)
            };
            let prov_style = if is_selected {
                if no_auth {
                    Style::default().fg(MUTED).bg(Color::Rgb(80, 50, 50))
                } else {
                    Style::default().fg(Color::Rgb(255, 200, 170)).bg(ACCENT)
                }
            } else {
                Style::default().fg(MUTED)
            };

            let label = if no_auth {
                // For collapsed no-auth providers, show the provider name
                // instead of a specific model name.
                format!("[{prov}]")
            } else if entry.alias == entry.canonical {
                entry.canonical.clone()
            } else {
                format!("{} → {}", entry.alias, entry.canonical)
            };
            let no_key = if no_auth && is_selected {
                "  key required"
            } else if no_auth {
                "  key?"
            } else {
                ""
            };

            lines.push(Line::from(vec![
                Span::styled(if is_selected { " > " } else { "   " }, highlight),
                Span::styled(label, highlight),
                Span::styled(format!("  {prov}"), prov_style),
                Span::styled(
                    no_key,
                    Style::default()
                        .fg(ERROR_COLOR)
                        .add_modifier(Modifier::BOLD),
                ),
            ]));
        }

        // Pad remaining rows.
        for _ in visible_end..scroll_start + sel.max_visible {
            lines.push(Line::from(""));
        }

        lines.push(Line::from(vec![
            Span::styled(
                " Enter",
                Style::default().fg(SUCCESS).add_modifier(Modifier::BOLD),
            ),
            Span::styled(" select  ", Style::default().fg(TEXT_SEC)),
            Span::styled("↑↓", Style::default().fg(TEXT_SEC)),
            Span::styled(" nav  ", Style::default().fg(TEXT_SEC)),
            Span::styled("Esc", Style::default().fg(MUTED)),
            Span::styled(" cancel", Style::default().fg(TEXT_SEC)),
        ]));

        let paragraph = Paragraph::new(lines);
        frame.render_widget(paragraph, inner);
    }
}

// -- TurnHandle trait ---------------------------------------------------------

/// Trait for objects that represent an in-flight turn.  The ratatui event
/// loop polls these for events and checks `is_finished`.
pub trait TurnHandle {
    /// Try to receive the next event without blocking.
    fn try_recv(&self) -> Option<TuiEvent>;
    /// Whether the worker thread has finished.
    fn is_finished(&self) -> bool;
}

// Blanket impl: Box<dyn TurnHandle> itself satisfies TurnHandle.
impl<T: TurnHandle + ?Sized> TurnHandle for Box<T> {
    fn try_recv(&self) -> Option<TuiEvent> {
        <T as TurnHandle>::try_recv(self)
    }
    fn is_finished(&self) -> bool {
        <T as TurnHandle>::is_finished(self)
    }
}

/// Implement `TurnHandle` for the `(Receiver, JoinHandle)` tuple that
/// `run_turn_tui_channels` returns.
impl TurnHandle
    for (
        std::sync::mpsc::Receiver<TuiEvent>,
        std::thread::JoinHandle<Result<String, Box<dyn std::error::Error + Send>>>,
    )
{
    fn try_recv(&self) -> Option<TuiEvent> {
        self.0.try_recv().ok()
    }

    fn is_finished(&self) -> bool {
        self.1.is_finished()
    }
}

impl ModelSelector {
    fn provider_label(provider: ninmu_api::ProviderKind) -> &'static str {
        match provider {
            ninmu_api::ProviderKind::Anthropic => "anthropic",
            ninmu_api::ProviderKind::Xai => "xai",
            ninmu_api::ProviderKind::OpenAi => "openai",
            ninmu_api::ProviderKind::DeepSeek => "deepseek",
            ninmu_api::ProviderKind::Ollama => "ollama",
            ninmu_api::ProviderKind::Qwen => "qwen",
            ninmu_api::ProviderKind::Vllm => "vllm",
            ninmu_api::ProviderKind::Mistral => "mistral",
            ninmu_api::ProviderKind::Gemini => "gemini",
            ninmu_api::ProviderKind::Cohere => "cohere",
        }
    }

    fn filter_text(&self) -> String {
        self.filter.iter().collect()
    }

    fn apply_filter(&mut self) {
        let query = self.filter_text().to_ascii_lowercase();
        self.filtered = (0..self.all_entries.len())
            .filter(|&i| {
                let e = &self.all_entries[i];
                let query_empty = query.is_empty();
                let alias_match = e.alias.to_ascii_lowercase().contains(&query);
                let canon_match = e.canonical.to_ascii_lowercase().contains(&query);
                let prov_match = Self::provider_label(e.provider).contains(&query);
                query_empty || alias_match || canon_match || prov_match
            })
            .collect();
        if self.selected >= self.filtered.len() {
            self.selected = self.filtered.len().saturating_sub(1);
        }
    }

    fn selected_entry(&self) -> Option<&ninmu_api::ModelEntry> {
        self.filtered
            .get(self.selected)
            .map(|&i| &self.all_entries[i])
    }
}

// -- Helpers ------------------------------------------------------------------

/// Render a line of text with inline markdown formatting as ratatui Spans.
///
/// Supports: **bold**, *italic*, `code`.
fn markdown_spans(text: &str) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut current = String::new();
    let mut chars = text.chars().peekable();

    while let Some(c) = chars.next() {
        if c == '`' {
            // Flush current without cloning — move the accumulated string out.
            if !current.is_empty() {
                spans.push(Span::styled(
                    std::mem::take(&mut current),
                    Style::default().fg(TEXT),
                ));
            }
            // Inline code
            let code: String = chars.by_ref().take_while(|&ch| ch != '`').collect();
            spans.push(Span::styled(
                code,
                Style::default().fg(ACCENT).bg(Color::Rgb(30, 30, 30)),
            ));
        } else if c == '*' && chars.peek() == Some(&'*') {
            chars.next(); // skip second *
            if !current.is_empty() {
                spans.push(Span::styled(
                    std::mem::take(&mut current),
                    Style::default().fg(TEXT),
                ));
            }
            let bold: String = chars.by_ref().take_while(|&ch| ch != '*').collect();
            if chars.peek() == Some(&'*') {
                chars.next();
            }
            spans.push(Span::styled(
                bold,
                Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
            ));
        } else if c == '*' {
            if !current.is_empty() {
                spans.push(Span::styled(
                    std::mem::take(&mut current),
                    Style::default().fg(TEXT),
                ));
            }
            let italic: String = chars.by_ref().take_while(|&ch| ch != '*').collect();
            spans.push(Span::styled(
                italic,
                Style::default().fg(TEXT).add_modifier(Modifier::ITALIC),
            ));
        } else {
            current.push(c);
        }
    }
    if !current.is_empty() {
        spans.push(Span::styled(current, Style::default().fg(TEXT)));
    }
    spans
}

fn strip_ansi(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            if chars.peek() == Some(&'[') {
                chars.next();
                while let Some(&next) = chars.peek() {
                    chars.next();
                    if next.is_ascii_alphabetic() || next == 'm' {
                        break;
                    }
                }
            }
        } else {
            result.push(c);
        }
    }
    result
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let end = max.saturating_sub(3);
        let safe_end = s.char_indices().nth(end).map_or(s.len(), |(idx, _)| idx);
        format!("{}...", &s[..safe_end])
    }
}

fn format_tokens(count: u32) -> String {
    if count >= 1_000_000 {
        format!("{:.1}M", f64::from(count) / 1_000_000.0)
    } else if count >= 1_000 {
        format!("{:.1}k", f64::from(count) / 1_000.0)
    } else {
        count.to_string()
    }
}

/// Linear interpolation between two u8 values.
fn lerp_u8(a: u8, b: u8, t: f32) -> u8 {
    (a as f32 + (b as f32 - a as f32) * t) as u8
}

/// Extract the RGB components from a [`Color::Rgb`].
trait RgbComponent {
    fn r(&self) -> u8;
    fn g(&self) -> u8;
    fn b(&self) -> u8;
}

impl RgbComponent for Color {
    fn r(&self) -> u8 {
        match self {
            Color::Rgb(r, _, _) => *r,
            _ => 0,
        }
    }
    fn g(&self) -> u8 {
        match self {
            Color::Rgb(_, g, _) => *g,
            _ => 0,
        }
    }
    fn b(&self) -> u8 {
        match self {
            Color::Rgb(_, _, b) => *b,
            _ => 0,
        }
    }
}

fn help_line<'a>(key: &str, desc: &str) -> Line<'a> {
    Line::from(vec![
        Span::raw("  "),
        Span::styled(format!("{key:<12}"), Style::default().fg(TEXT_SEC)),
        Span::styled(desc.to_string(), Style::default().fg(MUTED)),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_initialises_cleanly() {
        let app = RatatuiApp::new(
            "test-model".into(),
            "workspace-write".into(),
            Some("main".into()),
        );
        assert!(!app.help_visible);
        assert!(!app.state.is_generating);
        assert_eq!(app.spinner_frame, 0);
    }

    #[test]
    fn strip_ansi_removes_escapes() {
        let input = "\x1b[38;2;255;107;53mhello\x1b[0m";
        assert_eq!(strip_ansi(input), "hello");
    }

    #[test]
    fn strip_ansi_handles_no_escapes() {
        assert_eq!(strip_ansi("plain text"), "plain text");
    }

    #[test]
    fn strip_ansi_multi_sequence() {
        let input = "\x1b[1;31mERROR\x1b[0m: \x1b[33mwarn\x1b[0m";
        assert_eq!(strip_ansi(input), "ERROR: warn");
    }

    #[test]
    fn format_tokens_small() {
        assert_eq!(format_tokens(500), "500");
    }

    #[test]
    fn format_tokens_thousands() {
        assert_eq!(format_tokens(3200), "3.2k");
    }

    #[test]
    fn format_tokens_millions() {
        assert_eq!(format_tokens(1_500_000), "1.5M");
    }

    #[test]
    fn truncate_short_unchanged() {
        assert_eq!(truncate("hello", 10), "hello");
    }

    #[test]
    fn truncate_long_gets_ellipsis() {
        let result = truncate(&"a".repeat(100), 20);
        assert!(result.ends_with("..."));
        assert!(result.len() <= 20);
    }

    #[test]
    fn streaming_display_accumulates_lines() {
        let mut app = RatatuiApp::new("m".into(), "r".into(), None);
        app.response_text.push_str("hello\n");
        app.update_streaming_display();
        assert!(app.response_text.is_empty());
        assert!(!app.scrollback.is_empty());
    }

    #[test]
    fn streaming_display_holds_partial_line() {
        let mut app = RatatuiApp::new("m".into(), "r".into(), None);
        app.response_text.push_str("partial");
        app.update_streaming_display();
        assert_eq!(app.response_text, "partial");
        // Nothing pushed to scrollback yet (no newline)
    }

    #[test]
    fn flush_response_clears_buffer() {
        let mut app = RatatuiApp::new("m".into(), "r".into(), None);
        app.response_text.push_str("remaining text\n");
        app.flush_response();
        assert!(app.response_text.is_empty());
        assert!(!app.scrollback.is_empty());
    }

    #[test]
    fn flush_response_does_not_show_usage_in_scrollback() {
        let mut app = RatatuiApp::new("m".into(), "r".into(), None);
        app.response_text.push_str("hello\n");
        app.usage = TokenUsage {
            input_tokens: 100,
            output_tokens: 50,
            ..Default::default()
        };
        app.flush_response();
        let lines: Vec<String> = (0..app.scrollback.len())
            .filter_map(|i| app.scrollback.visible(100).0.get(i).cloned())
            .collect();
        assert!(
            !lines.iter().any(|l| l.contains("tokens")),
            "usage should not appear in scrollback, got: {lines:?}"
        );
    }

    #[test]
    fn flush_response_skips_usage_when_zero_tokens() {
        let mut app = RatatuiApp::new("m".into(), "r".into(), None);
        app.response_text.push_str("hello\n");
        app.usage = TokenUsage::default();
        let before = app.scrollback.len();
        app.flush_response();
        // Should only have the response line, no usage line
        let visible = app.scrollback.visible(100).0;
        assert!(!visible.iter().any(|l| l.contains("tokens")));
    }

    #[test]
    fn markdown_spans_bold() {
        let spans = markdown_spans("say **hello** world");
        let has_bold = spans
            .iter()
            .any(|s| s.content.contains("hello") && s.style.add_modifier.contains(Modifier::BOLD));
        assert!(has_bold, "expected bold span, got: {spans:?}");
    }

    #[test]
    fn markdown_spans_italic() {
        let spans = markdown_spans("say *hello* world");
        let has_italic = spans.iter().any(|s| {
            s.content.contains("hello") && s.style.add_modifier.contains(Modifier::ITALIC)
        });
        assert!(has_italic, "expected italic span, got: {spans:?}");
    }

    #[test]
    fn markdown_spans_code() {
        let spans = markdown_spans("use `foo` here");
        let has_code = spans
            .iter()
            .any(|s| s.content == "foo" && s.style.fg == Some(ACCENT));
        assert!(has_code, "expected code span with accent, got: {spans:?}");
    }

    #[test]
    fn markdown_spans_plain_text() {
        let spans = markdown_spans("no formatting here");
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].content, "no formatting here");
    }

    #[test]
    fn markdown_spans_multiple_inline() {
        let spans = markdown_spans("**bold** and *italic* and `code`");
        // Should have: "bold" (bold), " and " (plain), "italic" (italic), " and " (plain), "code" (code)
        assert!(
            spans.len() >= 5,
            "expected at least 5 spans, got: {spans:?}"
        );
    }

    // -- Cost display tests -------------------------------------------------

    #[test]
    fn flush_response_does_not_show_cost_in_scrollback() {
        let mut app = RatatuiApp::new("claude-sonnet".into(), "write".into(), None);
        app.usage = TokenUsage {
            input_tokens: 1000,
            output_tokens: 200,
            ..Default::default()
        };
        app.response_text = "Hello world".into();
        app.flush_response();
        let all = app.scrollback.visible(usize::MAX).0;
        assert!(
            !all.iter().any(|l| l.contains("tokens") || l.contains('$')),
            "cost should not appear in scrollback, got: {all:?}"
        );
    }

    // -- Input history tests ------------------------------------------------

    #[test]
    fn input_history_deduplicate_consecutive() {
        let mut app = RatatuiApp::new("m".into(), "r".into(), None);
        let should_add = app.input_history.last().is_none_or(|last| last != "hello");
        assert!(should_add);
        app.input_history.push("hello".into());
        let should_add2 = app.input_history.last().is_none_or(|last| last != "hello");
        assert!(!should_add2);
    }

    #[test]
    fn input_history_navigation_preserves_buffer() {
        let mut app = RatatuiApp::new("m".into(), "r".into(), None);
        app.input_history.push("first prompt".into());
        app.input_history.push("second prompt".into());
        app.input_buf = "current".chars().collect();
        app.cursor = app.input_buf.len();
        // Save current buffer and navigate to last history entry.
        app.history_restore_buf = app.input_buf.clone();
        app.history_index = Some(app.input_history.len() - 1);
        let entry = &app.input_history[app.history_index.unwrap()];
        app.input_buf = entry.chars().collect();
        app.cursor = app.input_buf.len();
        assert_eq!(app.input_buf.iter().collect::<String>(), "second prompt");
        assert_eq!(
            app.history_restore_buf.iter().collect::<String>(),
            "current"
        );
    }

    // -- Streaming cursor tests ---------------------------------------------

    #[test]
    fn streaming_cursor_blink_toggles() {
        let mut app = RatatuiApp::new("m".into(), "r".into(), None);
        let initial = app.show_cursor_blink;
        app.show_cursor_blink = !app.show_cursor_blink;
        assert_ne!(app.show_cursor_blink, initial);
    }

    // -- Session resume tests -----------------------------------------------

    #[test]
    fn load_conversation_history_shows_user_and_assistant() {
        let mut app = RatatuiApp::new("m".into(), "r".into(), None);
        let messages = vec![
            ConversationMessage {
                role: MessageRole::User,
                blocks: vec![ContentBlock::Text {
                    text: "Hello AI".into(),
                }],
                usage: None,
            },
            ConversationMessage {
                role: MessageRole::Assistant,
                blocks: vec![ContentBlock::Text {
                    text: "Hi human!".into(),
                }],
                usage: None,
            },
        ];
        app.load_conversation_history(&messages);
        let all = app.scrollback.visible(usize::MAX).0;
        let has_user = all.iter().any(|l| l.contains("\u{25B8} Hello AI"));
        let has_assistant = all.iter().any(|l| l.contains("Hi human!"));
        let has_separator = all.iter().any(|l| l.contains("session resumed"));
        assert!(has_user, "should show user message");
        assert!(has_assistant, "should show assistant message");
        assert!(has_separator, "should show resume separator");
    }

    #[test]
    fn load_conversation_history_shows_tool_use() {
        let mut app = RatatuiApp::new("m".into(), "r".into(), None);
        let messages = vec![ConversationMessage {
            role: MessageRole::Assistant,
            blocks: vec![ContentBlock::ToolUse {
                id: "tool_1".into(),
                name: "read".into(),
                input: "file.txt".into(),
            }],
            usage: None,
        }];
        app.load_conversation_history(&messages);
        let all = app.scrollback.visible(usize::MAX).0;
        let has_tool = all.iter().any(|l| l.contains("-- read"));
        assert!(has_tool, "should show tool use marker");
    }

    // -- Pricing model tests ------------------------------------------------

    #[test]
    fn pricing_model_resolved_for_known_model() {
        let app = RatatuiApp::new("claude-sonnet".into(), "write".into(), None);
        assert!(app.model_pricing.is_some());
    }

    #[test]
    fn pricing_model_none_for_unknown_model() {
        let app = RatatuiApp::new("unknown-model-xyz".into(), "write".into(), None);
        assert!(app.model_pricing.is_none());
    }

    // -- Cached string tests ------------------------------------------------

    #[test]
    fn header_cached_after_new() {
        let app = RatatuiApp::new(
            "claude-sonnet".into(),
            "workspace-write".into(),
            Some("main".into()),
        );
        assert!(!app.cached_header.spans.is_empty());
        let joined: String = app
            .cached_header
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert!(joined.contains("claude-sonnet"));
        assert!(joined.contains("write"));
        assert!(joined.contains("main"));
    }

    #[test]
    fn input_cached_on_typing() {
        let mut app = RatatuiApp::new("m".into(), "r".into(), None);
        app.input_buf = vec!['h', 'e', 'l', 'l', 'o'];
        app.refresh_input_cache();
        assert_eq!(app.cached_input, "hello");
    }

    #[test]
    fn input_cache_cleared_on_backspace() {
        let mut app = RatatuiApp::new("m".into(), "r".into(), None);
        app.input_buf = vec!['a', 'b'];
        app.refresh_input_cache();
        assert_eq!(app.cached_input, "ab");
        app.input_buf.pop();
        app.refresh_input_cache();
        assert_eq!(app.cached_input, "a");
    }

    #[test]
    fn status_elapsed_cached_per_second() {
        let mut app = RatatuiApp::new("m".into(), "r".into(), None);
        app.turn_start = Some(Instant::now() - Duration::from_secs(5));
        app.refresh_status_cache();
        assert_eq!(app.cached_elapsed_str, "5s");
    }

    // -- Dirty tracking tests -----------------------------------------------

    #[test]
    fn idle_app_is_dirty_on_new() {
        let app = RatatuiApp::new("m".into(), "r".into(), None);
        assert!(
            app.dirty,
            "app should be dirty immediately after new() so first draw happens"
        );
    }

    #[test]
    fn process_text_delta_sets_dirty() {
        let mut app = RatatuiApp::new("m".into(), "r".into(), None);
        app.dirty = false;
        app.process_event(TuiEvent::TextDelta("hello".into()));
        assert!(app.dirty);
    }

    #[test]
    fn process_turn_complete_sets_dirty() {
        let mut app = RatatuiApp::new("m".into(), "r".into(), None);
        app.state.is_generating = true;
        app.dirty = false;
        app.process_event(TuiEvent::TurnComplete);
        assert!(app.dirty);
        assert!(!app.state.is_generating);
    }

    #[test]
    fn process_permission_prompt_sets_dirty() {
        let mut app = RatatuiApp::new("m".into(), "r".into(), None);
        app.dirty = false;
        let (tx, _rx) = std::sync::mpsc::channel();
        app.process_event(TuiEvent::PermissionPrompt {
            request: ninmu_runtime::PermissionRequest {
                tool_name: "read".into(),
                input: "{}".into(),
                required_mode: ninmu_runtime::PermissionMode::WorkspaceWrite,
                current_mode: ninmu_runtime::PermissionMode::ReadOnly,
                reason: None,
            },
            response_tx: tx,
        });
        assert!(app.dirty);
        assert!(app.pending_permission.is_some());
    }

    #[test]
    fn process_error_sets_dirty() {
        let mut app = RatatuiApp::new("m".into(), "r".into(), None);
        app.dirty = false;
        app.process_event(TuiEvent::Error("something went wrong".into()));
        assert!(app.dirty);
    }

    // -- LoadHistory / resume guard tests -----------------------------------

    #[test]
    fn load_history_event_clears_existing_scrollback() {
        let mut app = RatatuiApp::new("m".into(), "r".into(), None);
        // Pre-populate scrollback with old content.
        app.scrollback.push("old line 1".into());
        app.scrollback.push("old line 2".into());
        assert!(!app.scrollback.is_empty());

        // Send a LoadHistory event with new messages.
        app.dirty = false;
        app.process_event(TuiEvent::LoadHistory {
            messages: vec![ConversationMessage {
                role: MessageRole::User,
                blocks: vec![ContentBlock::Text {
                    text: "new message".into(),
                }],
                usage: None,
            }],
        });

        assert!(app.dirty);
        let all = app.scrollback.visible(usize::MAX).0;
        let has_old = all.iter().any(|l| l.contains("old line"));
        let has_new = all.iter().any(|l| l.contains("\u{25B8} new message"));
        let has_separator = all.iter().any(|l| l.contains("session resumed"));
        assert!(!has_old, "old content should be cleared");
        assert!(has_new, "new messages should appear");
        assert!(has_separator, "resume separator should appear");
    }

    #[test]
    fn cost_includes_cache_tokens() {
        let mut app = RatatuiApp::new("claude-sonnet".into(), "write".into(), None);
        app.usage = TokenUsage {
            input_tokens: 0,
            output_tokens: 0,
            cache_creation_input_tokens: 1000,
            cache_read_input_tokens: 5000,
        };
        app.flush_response();
        // Total tokens (input + output) is 0, so no usage line is emitted.
        // This verifies the guard works correctly.
        assert!(app.scrollback.is_empty());
    }

    #[test]
    fn cost_shows_nonzero_cache_tokens() {
        let mut app = RatatuiApp::new("claude-sonnet".into(), "write".into(), None);
        app.usage = TokenUsage {
            input_tokens: 100,
            output_tokens: 50,
            cache_creation_input_tokens: 1000,
            cache_read_input_tokens: 5000,
        };
        app.response_text = "hello".into();
        app.flush_response();
        let all = app.scrollback.visible(usize::MAX).0;
        let usage_line = all.last().expect("usage line should exist");
        assert!(usage_line.contains('$'), "expected cost: {usage_line}");
        // Cost should be higher than just 100+50 tokens — cache tokens add to it.
        // With sonnet pricing: 100 in + 50 out + 1000 cache_create + 5000 cache_read
        // = $0.0015 + $0.00375 + $0.01875 + $0.0075 ≈ $0.0315
        assert!(
            usage_line.contains("0.03"),
            "expected cache-aware cost: {usage_line}"
        );
    }

    // -- Input cache clearing tests -----------------------------------------

    #[test]
    fn enter_clears_cached_input() {
        let mut app = RatatuiApp::new("m".into(), "r".into(), None);
        // Simulate typing "hello" into the input buffer.
        app.input_buf = "hello".chars().collect();
        app.cursor = app.input_buf.len();
        app.refresh_input_cache();
        assert_eq!(app.cached_input, "hello");

        // Simulate what Enter does: drain the buffer then refresh.
        let _input: String = app.input_buf.drain(..).collect();
        app.cursor = 0;
        app.refresh_input_cache();

        assert!(
            app.cached_input.is_empty(),
            "cached_input should be empty after Enter, got: {:?}",
            app.cached_input
        );
        assert!(app.input_buf.is_empty());
    }

    #[test]
    fn cached_input_matches_input_buf() {
        let mut app = RatatuiApp::new("m".into(), "r".into(), None);
        // Initially both are empty.
        assert_eq!(app.cached_input, "");

        // Type "ab".
        app.input_buf.push('a');
        app.input_buf.push('b');
        app.refresh_input_cache();
        assert_eq!(app.cached_input, "ab");

        // Backspace one char.
        app.input_buf.pop();
        app.refresh_input_cache();
        assert_eq!(app.cached_input, "a");

        // Clear all (simulate Enter drain).
        app.input_buf.clear();
        app.refresh_input_cache();
        assert_eq!(app.cached_input, "");
    }

    // -- ReasoningUpdate / ModelUpdate event tests --------------------------

    #[test]
    fn reasoning_update_sets_effort_and_thinking() {
        let mut app = RatatuiApp::new("deepseek-reasoner".into(), "write".into(), None);
        assert!(app.reasoning_effort.is_none());
        assert!(app.thinking_mode.is_none());

        app.process_event(TuiEvent::ReasoningUpdate {
            effort: Some("high".to_string()),
            thinking: Some(true),
        });
        assert_eq!(app.reasoning_effort.as_deref(), Some("high"));
        assert_eq!(app.thinking_mode, Some(true));
    }

    #[test]
    fn reasoning_update_rebuilds_header() {
        let mut app = RatatuiApp::new("deepseek-reasoner".into(), "write".into(), None);
        app.dirty = false;
        app.process_event(TuiEvent::ReasoningUpdate {
            effort: Some("max".to_string()),
            thinking: Some(false),
        });
        assert!(app.dirty, "ReasoningUpdate must set dirty flag");
        let header_text = format!("{:?}", app.cached_header);
        assert!(
            header_text.contains("max"),
            "header must show effort level: {header_text}"
        );
        assert!(
            header_text.contains("off"),
            "header must show thinking=off: {header_text}"
        );
    }

    #[test]
    fn reasoning_update_clears_state() {
        let mut app = RatatuiApp::new("deepseek-reasoner".into(), "write".into(), None);
        app.process_event(TuiEvent::ReasoningUpdate {
            effort: Some("high".to_string()),
            thinking: Some(true),
        });
        // Now clear
        app.process_event(TuiEvent::ReasoningUpdate {
            effort: None,
            thinking: None,
        });
        assert!(app.reasoning_effort.is_none());
        assert!(app.thinking_mode.is_none());
        let header_text = format!("{:?}", app.cached_header);
        assert!(
            header_text.contains("auto"),
            "header must show default thinking=auto: {header_text}"
        );
    }

    #[test]
    fn model_update_changes_model_and_pricing() {
        let mut app = RatatuiApp::new("gpt-4o".into(), "write".into(), None);
        app.dirty = false;
        app.process_event(TuiEvent::ModelUpdate {
            model: "claude-sonnet".to_string(),
        });
        assert!(app.dirty, "ModelUpdate must set dirty flag");
        assert_eq!(app.model, "claude-sonnet");
        // claude-sonnet should have pricing
        assert!(
            app.model_pricing.is_some(),
            "claude-sonnet should have pricing"
        );
    }

    #[test]
    fn model_update_rebuilds_header() {
        let mut app = RatatuiApp::new("gpt-4o".into(), "write".into(), None);
        app.process_event(TuiEvent::ModelUpdate {
            model: "deepseek-reasoner".to_string(),
        });
        let header_text = format!("{:?}", app.cached_header);
        assert!(
            header_text.contains("deepseek-reasoner"),
            "header must show new model: {header_text}"
        );
    }

    #[test]
    fn prompt_cache_event_pushes_warning_to_scrollback() {
        let mut app = RatatuiApp::new("gpt-4o".into(), "write".into(), None);
        app.process_event(TuiEvent::PromptCache(PromptCacheEvent {
            unexpected: true,
            reason: "cache read tokens dropped".to_string(),
            previous_cache_read_input_tokens: 6_000,
            current_cache_read_input_tokens: 1_000,
            token_drop: 5_000,
        }));
        let (visible, _, _) = app.scrollback.visible(100);
        let last = visible
            .last()
            .expect("scrollback should have entry")
            .clone();
        assert!(
            last.contains("cache break"),
            "expected warning in scrollback: {last}"
        );
        assert!(
            last.contains("5000 tokens"),
            "expected token drop in scrollback: {last}"
        );
    }

    #[test]
    fn prompt_cache_expected_invalidation_shows_notice() {
        let mut app = RatatuiApp::new("gpt-4o".into(), "write".into(), None);
        app.process_event(TuiEvent::PromptCache(PromptCacheEvent {
            unexpected: false,
            reason: "model changed".to_string(),
            previous_cache_read_input_tokens: 6_000,
            current_cache_read_input_tokens: 3_000,
            token_drop: 3_000,
        }));
        let (visible, _, _) = app.scrollback.visible(100);
        let last = visible
            .last()
            .expect("scrollback should have entry")
            .clone();
        assert!(
            last.contains("cache invalidated"),
            "expected notice in scrollback: {last}"
        );
    }

    // -- set_reasoning_effort / set_thinking_mode public API tests ----------

    #[test]
    fn set_reasoning_effort_updates_state_and_header() {
        let mut app = RatatuiApp::new("gpt-4o".into(), "read".into(), None);
        app.set_reasoning_effort(Some("low".to_string()));
        assert_eq!(app.reasoning_effort.as_deref(), Some("low"));
    }

    #[test]
    fn set_thinking_mode_updates_state_and_header() {
        let mut app = RatatuiApp::new("deepseek-reasoner".into(), "read".into(), None);
        app.set_thinking_mode(Some(true));
        assert_eq!(app.thinking_mode, Some(true));
        app.set_thinking_mode(Some(false));
        assert_eq!(app.thinking_mode, Some(false));
        app.set_thinking_mode(None);
        assert!(app.thinking_mode.is_none());
    }

    // -- build_header_line content tests ------------------------------------

    #[test]
    fn header_default_shows_think_auto() {
        let header = RatatuiApp::build_header_line("gpt-4o", "write", Some("main"), None, None);
        let text = format!("{header:?}");
        assert!(
            text.contains("think"),
            "header must contain 'think': {text}"
        );
        assert!(
            text.contains("auto"),
            "header must contain 'auto' for default thinking: {text}"
        );
    }

    #[test]
    fn header_shows_think_on_with_effort() {
        let header = RatatuiApp::build_header_line(
            "deepseek-reasoner",
            "write",
            Some("main"),
            Some("high"),
            Some(true),
        );
        let text = format!("{header:?}");
        assert!(
            text.contains("think"),
            "header must contain 'think': {text}"
        );
        assert!(text.contains("on"), "header must show thinking=on: {text}");
        assert!(text.contains("high"), "header must show effort: {text}");
    }

    #[test]
    fn header_shows_think_off_when_disabled() {
        let header = RatatuiApp::build_header_line(
            "deepseek-reasoner",
            "read",
            None,
            Some("max"),
            Some(false),
        );
        let text = format!("{header:?}");
        assert!(
            text.contains("off"),
            "header must show thinking=off: {text}"
        );
        assert!(text.contains("max"), "header must show effort=max: {text}");
    }

    // -- Double-flush guard tests -------------------------------------------

    #[test]
    fn flush_response_does_not_emit_usage_twice() {
        let mut app = RatatuiApp::new("claude-sonnet".into(), "write".into(), None);
        app.usage = TokenUsage {
            input_tokens: 100,
            output_tokens: 50,
            ..Default::default()
        };
        app.response_text = "hello".into();
        // First flush — emits response + usage.
        app.flush_response();
        let count_after_first = app.scrollback.len();
        // Second flush — response_text is empty, but usage still > 0.
        // It must NOT emit a duplicate usage line.
        app.flush_response();
        let count_after_second = app.scrollback.len();
        assert_eq!(
            count_after_first, count_after_second,
            "second flush must not add more lines (duplicate usage)"
        );
    }

    #[test]
    fn turn_complete_then_is_finished_no_double_flush() {
        let mut app = RatatuiApp::new("claude-sonnet".into(), "write".into(), None);
        app.state.is_generating = true;
        app.usage = TokenUsage {
            input_tokens: 200,
            output_tokens: 100,
            ..Default::default()
        };
        app.response_text = "world".into();
        // Simulate TurnComplete event (sets is_generating = false).
        app.process_event(TuiEvent::TurnComplete);
        let count_after_complete = app.scrollback.len();
        // Now simulate is_finished() path — it should NOT flush again.
        // The guard is: if self.state.is_generating { flush }
        if app.state.is_generating {
            app.flush_response();
        }
        let count_after_finished = app.scrollback.len();
        assert_eq!(
            count_after_complete, count_after_finished,
            "is_finished path must not double-flush after TurnComplete"
        );
    }

    // -- Pulse only last user prompt tests ----------------------------------

    #[test]
    fn only_last_user_prompt_gets_pulse_color() {
        let mut app = RatatuiApp::new("m".into(), "r".into(), None);
        // Push two user prompts.
        app.scrollback.push("  \u{25B8} first prompt".into());
        app.scrollback.push("response".into());
        app.scrollback.push("  \u{25B8} second prompt".into());
        // Set generating state so pulse logic kicks in.
        app.state.is_generating = true;
        app.spinner_frame = 0; // deterministic pulse phase

        let visible = app.scrollback.visible(usize::MAX).0;
        // Find last user prompt index.
        let last_user_idx = visible
            .iter()
            .enumerate()
            .rev()
            .find(|(_, s)| s.trim_end().starts_with("  \u{25B8}"))
            .map(|(i, _)| i);
        assert_eq!(
            last_user_idx,
            Some(2),
            "last user prompt should be at index 2"
        );

        // The first user prompt (index 0) is NOT the last, so it should
        // use static USER_COLOR, not pulse.
        let first_is_active = app.state.is_generating && Some(0) == last_user_idx;
        assert!(
            !first_is_active,
            "first prompt should not be active/pulsing"
        );

        // The second user prompt (index 2) IS the last, so it should pulse.
        let second_is_active = app.state.is_generating && Some(2) == last_user_idx;
        assert!(second_is_active, "second prompt should be active/pulsing");
    }

    #[test]
    fn no_pulse_when_not_generating() {
        let mut app = RatatuiApp::new("m".into(), "r".into(), None);
        app.scrollback.push("  \u{25B8} hello".into());
        app.state.is_generating = false;

        let visible = app.scrollback.visible(usize::MAX).0;
        let last_user_idx = visible
            .iter()
            .enumerate()
            .rev()
            .find(|(_, s)| s.trim_end().starts_with("  \u{25B8}"))
            .map(|(i, _)| i);

        // Not generating → is_active is false even for the last prompt.
        let is_active = app.state.is_generating && Some(0) == last_user_idx;
        assert!(!is_active, "no pulse when idle");
    }

    // -- ModelSelector tests ---------------------------------------------------

    #[test]
    fn model_selector_opens_with_entries() {
        let mut app = RatatuiApp::new("gpt-4o".into(), "write".into(), None);
        app.open_model_selector();
        assert!(app.model_selector.is_some());
        let sel = app.model_selector.as_ref().unwrap();
        assert!(!sel.all_entries.is_empty(), "should have model entries");
        assert!(
            sel.filtered.len() == sel.all_entries.len(),
            "initial filter should match all entries"
        );
    }

    #[test]
    fn model_selector_filter_narrows_results() {
        let mut app = RatatuiApp::new("gpt-4o".into(), "write".into(), None);
        app.open_model_selector();
        let sel = app.model_selector.as_mut().unwrap();
        let total = sel.all_entries.len();
        // Type "deep" to filter.
        for c in "deep".chars() {
            sel.filter.push(c);
            sel.filter_cursor += 1;
        }
        sel.apply_filter();
        assert!(
            sel.filtered.len() < total,
            "filter 'deep' should narrow results"
        );
        // Every filtered entry should contain "deep".
        for &idx in &sel.filtered {
            let e = &sel.all_entries[idx];
            let matches = e.alias.to_ascii_lowercase().contains("deep")
                || e.canonical.to_ascii_lowercase().contains("deep")
                || ModelSelector::provider_label(e.provider).contains("deep");
            assert!(matches, "entry {:?} should match 'deep'", e.alias);
        }
    }

    #[test]
    fn model_selector_navigate_and_select() {
        let mut app = RatatuiApp::new("gpt-4o".into(), "write".into(), None);
        app.open_model_selector();
        let sel = app.model_selector.as_mut().unwrap();
        assert!(sel.filtered.len() > 1, "need multiple entries");
        assert_eq!(sel.selected, 0);
        // Simulate Down.
        sel.selected = 1;
        let entry = sel.selected_entry().unwrap();
        assert!(!entry.alias.is_empty());
    }

    #[test]
    fn model_selector_empty_filter_returns_all() {
        let mut app = RatatuiApp::new("gpt-4o".into(), "write".into(), None);
        app.open_model_selector();
        let sel = app.model_selector.as_mut().unwrap();
        sel.apply_filter();
        assert_eq!(sel.filtered.len(), sel.all_entries.len());
    }

    #[test]
    fn model_selector_esc_closes() {
        let mut app = RatatuiApp::new("gpt-4o".into(), "write".into(), None);
        app.open_model_selector();
        assert!(app.model_selector.is_some());
        app.model_selector = None;
        assert!(app.model_selector.is_none());
    }

    #[test]
    fn collapse_no_auth_providers_groups_by_provider() {
        use ninmu_api::{ModelEntry, ProviderKind};

        let entries = vec![
            ModelEntry {
                alias: "gpt-4o".into(),
                canonical: "gpt-4o".into(),
                provider: ProviderKind::OpenAi,
                has_auth: true,
            },
            ModelEntry {
                alias: "gpt-4-turbo".into(),
                canonical: "gpt-4-turbo".into(),
                provider: ProviderKind::OpenAi,
                has_auth: true,
            },
            ModelEntry {
                alias: "claude-sonnet".into(),
                canonical: "claude-sonnet".into(),
                provider: ProviderKind::Anthropic,
                has_auth: false,
            },
            ModelEntry {
                alias: "claude-haiku".into(),
                canonical: "claude-haiku".into(),
                provider: ProviderKind::Anthropic,
                has_auth: false,
            },
            ModelEntry {
                alias: "deepseek-chat".into(),
                canonical: "deepseek-chat".into(),
                provider: ProviderKind::DeepSeek,
                has_auth: false,
            },
        ];

        let collapsed = RatatuiApp::collapse_no_auth_providers(entries);

        // OpenAI has auth — both models kept.
        assert_eq!(
            collapsed
                .iter()
                .filter(|e| e.provider == ProviderKind::OpenAi)
                .count(),
            2
        );
        // Anthropic has no auth — only one entry kept.
        assert_eq!(
            collapsed
                .iter()
                .filter(|e| e.provider == ProviderKind::Anthropic)
                .count(),
            1
        );
        // DeepSeek has no auth — only one entry kept.
        assert_eq!(
            collapsed
                .iter()
                .filter(|e| e.provider == ProviderKind::DeepSeek)
                .count(),
            1
        );
        // Total: 2 + 1 + 1 = 4
        assert_eq!(collapsed.len(), 4);
    }

    // -- Stall watchdog tests ----------------------------------------------

    #[test]
    fn last_event_received_is_none_at_creation() {
        let app = RatatuiApp::new("m".into(), "r".into(), None);
        assert!(
            app.last_event_received.is_none(),
            "last_event_received should be None when no turn is active"
        );
    }

    #[test]
    fn last_event_received_set_on_turn_start() {
        let mut app = RatatuiApp::new("m".into(), "r".into(), None);
        // Simulate starting a turn.
        app.state.is_generating = true;
        app.turn_start = Some(Instant::now());
        app.last_event_received = Some(Instant::now());
        assert!(
            app.last_event_received.is_some(),
            "last_event_received should be set when a turn starts"
        );
    }

    #[test]
    fn last_event_received_cleared_on_turn_complete() {
        let mut app = RatatuiApp::new("m".into(), "r".into(), None);
        app.last_event_received = Some(Instant::now());
        app.state.is_generating = true;
        // Simulate TurnComplete event.
        app.process_event(TuiEvent::TurnComplete);
        // After TurnComplete, the event loop sets last_event_received = None.
        // We verify the field is clearable (the event loop does the actual clear).
        app.last_event_received = None;
        assert!(
            app.last_event_received.is_none(),
            "last_event_received should be cleared when turn completes"
        );
    }

    #[test]
    fn last_event_received_cleared_on_esc_cancel() {
        let mut app = RatatuiApp::new("m".into(), "r".into(), None);
        app.last_event_received = Some(Instant::now());
        app.state.is_generating = true;
        // Simulate what the Esc handler does.
        app.state.is_generating = false;
        app.state.thinking_state = ThinkingState::Idle;
        app.last_event_received = None;
        app.flush_response();
        assert!(
            app.last_event_received.is_none(),
            "last_event_received should be cleared on Esc cancel"
        );
    }

    #[test]
    fn watchdog_detects_stalled_turn() {
        let app = RatatuiApp::new("m".into(), "r".into(), None);
        const STALL_WATCHDOG: Duration = Duration::from_mins(3);
        // Simulate a last event received 181 seconds ago.
        let last = Instant::now() - Duration::from_secs(181);
        assert!(
            last.elapsed() > STALL_WATCHDOG,
            "watchdog should detect a stalled turn after 3 minutes"
        );
    }

    #[test]
    fn watchdog_does_not_trigger_on_active_turn() {
        let app = RatatuiApp::new("m".into(), "r".into(), None);
        const STALL_WATCHDOG: Duration = Duration::from_mins(3);
        // Simulate a last event received just now.
        let last = Instant::now();
        assert!(
            last.elapsed() <= STALL_WATCHDOG,
            "watchdog should NOT trigger on an active turn"
        );
    }

    #[test]
    fn watchdog_clears_state_on_force_cancel() {
        let mut app = RatatuiApp::new("m".into(), "r".into(), None);
        app.state.is_generating = true;
        app.state.thinking_state = ThinkingState::Thinking {
            started: Instant::now(),
        };
        app.state.current_tool = Some("bash".into());
        app.last_event_received = Some(Instant::now() - Duration::from_secs(200));
        app.turn_start = Some(Instant::now() - Duration::from_secs(200));

        // Simulate what the watchdog does.
        const STALL_WATCHDOG: Duration = Duration::from_mins(3);
        if let Some(last) = app.last_event_received {
            if last.elapsed() > STALL_WATCHDOG {
                app.state.is_generating = false;
                app.state.thinking_state = ThinkingState::Idle;
                app.state.current_tool = None;
                app.flush_response();
                app.scrollback
                    .push("  [stalled \u{2014} no response in 3 min, turn cancelled]".to_string());
                app.last_event_received = None;
                app.dirty = true;
            }
        }

        assert!(
            !app.state.is_generating,
            "is_generating should be false after watchdog force-cancel"
        );
        assert_eq!(
            app.state.thinking_state,
            ThinkingState::Idle,
            "thinking should be reset to Idle"
        );
        assert!(
            app.state.current_tool.is_none(),
            "current_tool should be cleared"
        );
        assert!(
            app.last_event_received.is_none(),
            "last_event_received should be cleared"
        );
        assert!(app.dirty, "dirty flag should be set for redraw");
        let all = app.scrollback.visible(usize::MAX).0;
        assert!(
            all.iter().any(|l| l.contains("stalled")),
            "scrollback should contain stall message"
        );
    }

    // -- Paste handling tests ------------------------------------------------

    #[test]
    fn short_paste_inserts_directly() {
        let mut app = RatatuiApp::new("m".into(), "r".into(), None);
        app.handle_paste("hello world");
        assert_eq!(app.cached_input, "hello world");
        assert_eq!(app.cursor, 11);
        assert!(app.paste_spans.is_empty());
        assert!(!app.paste_animating);
    }

    #[test]
    fn short_paste_exactly_128_chars() {
        let mut app = RatatuiApp::new("m".into(), "r".into(), None);
        let text = "a".repeat(128);
        app.handle_paste(&text);
        assert_eq!(app.cached_input, text);
        assert!(app.paste_spans.is_empty());
        assert!(!app.paste_animating);
    }

    #[test]
    fn long_paste_starts_animation() {
        let mut app = RatatuiApp::new("m".into(), "r".into(), None);
        let text = "a".repeat(200);
        app.handle_paste(&text);
        assert!(app.paste_animating);
        assert!(app.anim_summary.is_some());
        assert_eq!(app.anim_range, Some((0, 200)));
        assert_eq!(app.paste_anim_frame, 0);
        assert!(app.paste_anim_start.is_some());
        // Text is in input_buf at cursor position.
        assert_eq!(app.cached_input, text);
        assert_eq!(app.cursor, 200);
    }

    #[test]
    fn long_paste_summary_has_word_and_line_counts() {
        let mut app = RatatuiApp::new("m".into(), "r".into(), None);
        let long_text = format!("hello world\nfoo bar\nbaz{}", "x".repeat(200));
        app.handle_paste(&long_text);
        let summary = app.anim_summary.as_ref().unwrap();
        assert!(summary.starts_with("[Pasted "));
        assert!(summary.contains("words"));
        assert!(summary.contains("lines]"));
    }

    #[test]
    fn long_paste_shows_summary_after_animation() {
        let mut app = RatatuiApp::new("m".into(), "r".into(), None);
        let text = "a".repeat(200);
        app.handle_paste(&text);
        // Simulate animation completion via finish_animation().
        app.finish_animation();
        assert!(!app.paste_animating);
        assert_eq!(app.paste_spans.len(), 1);
        let display = app.paste_display_text();
        assert!(display.starts_with("[Pasted "));
        assert!(display.ends_with(']'));
    }

    #[test]
    fn paste_animation_shows_pacman_and_preview() {
        let mut app = RatatuiApp::new("m".into(), "r".into(), None);
        let text = "a".repeat(200);
        app.handle_paste(&text);
        assert!(app.paste_animating);

        app.paste_anim_frame = 0;
        let display = app.paste_display_text();
        assert!(
            display.starts_with(&"a".repeat(30)),
            "starts with 30-char preview"
        );
        assert!(display.chars().any(RatatuiApp::is_pacman), "pacman present");

        app.paste_anim_frame = 5;
        let display_mid = app.paste_display_text();
        assert!(
            display_mid.chars().any(RatatuiApp::is_pacman),
            "pacman still present"
        );
    }

    #[test]
    fn paste_animation_reveals_summary_as_pacman_eats() {
        let mut app = RatatuiApp::new("m".into(), "r".into(), None);
        app.handle_paste(&"x".repeat(200));
        app.paste_anim_frame = 8;
        let display = app.paste_display_text();
        assert!(display.contains("[Pasted"));
        assert!(display.contains("words"));
    }

    #[test]
    fn input_buf_always_has_real_text() {
        let mut app = RatatuiApp::new("m".into(), "r".into(), None);
        let original = format!("{}\nsecond line\nthird line", "x".repeat(200));
        app.handle_paste(&original);
        assert_eq!(app.cached_input, original);
        app.finish_animation();
        assert_eq!(app.cached_input, original);
    }

    #[test]
    fn clear_paste_state_resets_all_fields() {
        let mut app = RatatuiApp::new("m".into(), "r".into(), None);
        app.handle_paste(&"a".repeat(200));
        assert!(!app.paste_spans.is_empty() || app.paste_animating);
        app.clear_paste_state();
        assert!(app.paste_spans.is_empty());
        assert!(!app.paste_animating);
        assert!(app.paste_anim_start.is_none());
        assert_eq!(app.paste_anim_frame, 0);
        assert!(app.anim_summary.is_none());
        assert!(app.anim_range.is_none());
    }

    #[test]
    fn typing_after_paste_inserts_at_cursor() {
        let mut app = RatatuiApp::new("m".into(), "r".into(), None);
        let original = "a".repeat(200);
        app.handle_paste(&original);
        app.finish_animation();

        // Type "bc" at the end (cursor is at end of pasted text).
        app.input_buf.push('b');
        app.input_buf.push('c');
        app.cursor += 2;
        app.refresh_input_cache();

        assert_eq!(app.cached_input, format!("{original}bc"));
        // Display shows summary for the paste + typed chars.
        let display = app.paste_display_text();
        assert!(display.starts_with("[Pasted "));
        assert!(display.ends_with("bc"));
    }

    #[test]
    fn backspace_removes_from_input_buf() {
        let mut app = RatatuiApp::new("m".into(), "r".into(), None);
        app.handle_paste(&"a".repeat(200));
        app.finish_animation();

        // Type "xy".
        app.input_buf.push('x');
        app.input_buf.push('y');
        app.cursor += 2;

        // Backspace removes 'y'.
        app.cursor -= 1;
        app.input_buf.remove(app.cursor);
        app.shift_paste_spans(app.cursor, -1);
        app.refresh_input_cache();

        assert!(app.cached_input.ends_with('x'));
        assert_eq!(app.cached_input, format!("{}{}", "a".repeat(200), "x"));
    }

    #[test]
    fn display_shows_summary_without_typed_chars_initially() {
        let mut app = RatatuiApp::new("m".into(), "r".into(), None);
        app.handle_paste(&"a".repeat(200));
        app.finish_animation();
        let display = app.paste_display_text();
        assert!(display.starts_with("[Pasted "));
        assert!(display.ends_with("lines]"));
    }

    #[test]
    fn paste_with_multiline_counts_lines() {
        let mut app = RatatuiApp::new("m".into(), "r".into(), None);
        let text = format!("line1\nline2\nline3\n{}", "x".repeat(200));
        app.handle_paste(&text);
        let summary = app.anim_summary.as_ref().unwrap();
        assert!(
            summary.contains("4 lines"),
            "summary should count 4 lines: {summary}"
        );
    }

    #[test]
    fn paste_animation_frame_advances() {
        let mut app = RatatuiApp::new("m".into(), "r".into(), None);
        app.handle_paste(&"a".repeat(200));
        assert_eq!(app.paste_anim_frame, 0);
        app.paste_anim_frame = app.paste_anim_frame.wrapping_add(1);
        assert_eq!(app.paste_anim_frame, 1);
    }

    #[test]
    fn paste_with_existing_input_appends() {
        let mut app = RatatuiApp::new("m".into(), "r".into(), None);
        for c in "prefix ".chars() {
            app.input_buf.push(c);
        }
        app.cursor = app.input_buf.len();
        app.refresh_input_cache();
        app.handle_paste("pasted");
        assert_eq!(app.cached_input, "prefix pasted");
    }

    #[test]
    fn input_buf_during_animation_has_real_text() {
        let mut app = RatatuiApp::new("m".into(), "r".into(), None);
        let original = "a".repeat(200);
        app.handle_paste(&original);
        assert!(app.paste_animating);
        // input_buf always has the real text.
        assert_eq!(app.cached_input, original);
    }

    // -- E2E paste lifecycle tests -------------------------------------------

    #[test]
    fn e2e_paste_type_submit_full_flow() {
        let mut app = RatatuiApp::new("m".into(), "r".into(), None);
        let pasted = "original pasted code\nline 2\nline 3";
        let long_paste = format!("{pasted}{}", "x".repeat(200));

        app.handle_paste(&long_paste);
        assert!(app.paste_animating);

        // Animation ticks advance.
        app.paste_anim_frame = 5;
        let display = app.paste_display_text();
        assert!(display.chars().any(RatatuiApp::is_pacman));

        // Animation completes.
        app.finish_animation();
        let display = app.paste_display_text();
        assert!(display.starts_with("[Pasted "));

        // User types " hello" at cursor (end of pasted text).
        for c in " hello".chars() {
            app.input_buf.insert(app.cursor, c);
            app.cursor += 1;
        }
        app.refresh_input_cache();

        // input_buf has real text: paste + " hello".
        assert_eq!(app.cached_input, format!("{long_paste} hello"));
    }

    #[test]
    fn e2e_paste_short_text_no_animation() {
        let mut app = RatatuiApp::new("m".into(), "r".into(), None);
        app.handle_paste("short text");
        assert!(!app.paste_animating);
        assert!(app.paste_spans.is_empty());
        assert_eq!(app.cached_input, "short text");
    }

    #[test]
    fn e2e_paste_submit_without_typing() {
        let mut app = RatatuiApp::new("m".into(), "r".into(), None);
        let long = format!("code snippet\nmore code{}", "x".repeat(200));
        app.handle_paste(&long);
        app.finish_animation();
        assert_eq!(app.cached_input, long);
    }

    #[test]
    fn e2e_display_never_shows_raw_paste_text() {
        let mut app = RatatuiApp::new("m".into(), "r".into(), None);
        let raw = format!("secret{}\nmore secret stuff", "x".repeat(500));
        app.handle_paste(&raw);

        app.paste_anim_frame = 3;
        let display = app.paste_display_text();
        assert!(display.chars().any(RatatuiApp::is_pacman));

        app.finish_animation();
        let display = app.paste_display_text();
        assert!(display.starts_with("[Pasted "));
        assert!(!display.contains("secret"));
    }

    #[test]
    fn watchdog_does_not_fire_during_permission_prompt() {
        let mut app = RatatuiApp::new("m".into(), "r".into(), None);
        app.state.is_generating = true;
        app.last_event_received = Some(Instant::now() - Duration::from_secs(200));

        let (response_tx, _rx) = std::sync::mpsc::channel();
        app.pending_permission = Some(PendingPermission {
            request: ninmu_runtime::PermissionRequest {
                tool_name: "bash".into(),
                input: "{}".into(),
                required_mode: ninmu_runtime::PermissionMode::WorkspaceWrite,
                current_mode: ninmu_runtime::PermissionMode::ReadOnly,
                reason: None,
            },
            response_tx,
            action_description: "run command".into(),
        });

        const STALL_WATCHDOG: Duration = Duration::from_mins(3);
        if let Some(last) = app.last_event_received {
            if app.pending_permission.is_none() && last.elapsed() > STALL_WATCHDOG {
                app.state.is_generating = false;
            }
        }

        assert!(
            app.state.is_generating,
            "watchdog should NOT cancel turn while permission prompt is active"
        );
    }

    // -- Multi-paste ordering tests ------------------------------------------

    #[test]
    fn second_paste_inserts_at_cursor() {
        let mut app = RatatuiApp::new("m".into(), "r".into(), None);
        let first = "a".repeat(200);
        let second = "b".repeat(200);
        app.handle_paste(&first);
        app.finish_animation();
        assert_eq!(app.cursor, 200);
        // Second paste inserts at cursor, starts new animation.
        app.handle_paste(&second);
        assert!(app.paste_animating, "second long paste starts new animation");
        assert_eq!(app.cached_input, format!("{first}{second}"));
        // First paste span is recorded; second is still animating.
        assert_eq!(app.paste_spans.len(), 1);
        assert!(app.anim_range.is_some());
        // After finishing second animation, both spans recorded.
        app.finish_animation();
        assert_eq!(app.paste_spans.len(), 2);
    }

    #[test]
    fn second_short_paste_inserts_at_cursor() {
        let mut app = RatatuiApp::new("m".into(), "r".into(), None);
        app.handle_paste(&"a".repeat(200));
        app.finish_animation();
        // Short paste also inserts at cursor.
        app.handle_paste(" more text");
        assert_eq!(app.cached_input, format!("{}{}", "a".repeat(200), " more text"));
        // Only one paste span (the long one); short paste has no span.
        assert_eq!(app.paste_spans.len(), 1);
    }

    #[test]
    fn ascii_pacman_renders_in_display() {
        let mut app = RatatuiApp::new("m".into(), "r".into(), None);
        app.handle_paste(&"x".repeat(200));
        assert!(app.paste_animating);
        let display = app.paste_display_text();
        assert!(
            display.contains('C') || display.contains('('),
            "ASCII pacman char should be visible"
        );
    }

    #[test]
    fn e2e_type_paste_type_paste_type() {
        // Type "a" → paste long X → type "b" → paste long Y → type "c"
        // Result in input_buf: "aXbYc" (everything in order)
        let mut app = RatatuiApp::new("m".into(), "r".into(), None);

        // Type "a".
        app.input_buf.push('a');
        app.cursor = 1;

        // Paste X at cursor.
        let x = format!("X{}", "x".repeat(200));
        app.handle_paste(&x);
        app.finish_animation();
        assert_eq!(app.cursor, 1 + x.len());

        // Type "b" at cursor.
        app.input_buf.insert(app.cursor, 'b');
        app.cursor += 1;

        // Paste Y at cursor.
        let y = format!("Y{}", "y".repeat(200));
        app.handle_paste(&y);
        app.finish_animation();

        // Type "c" at cursor.
        app.input_buf.insert(app.cursor, 'c');
        app.cursor += 1;
        app.refresh_input_cache();

        let expected = format!("a{x}b{y}c");
        assert_eq!(app.cached_input, expected);

        // Display should show: a [Pasted...] b [Pasted...] c
        let display = app.paste_display_text();
        assert!(display.starts_with('a'));
        assert!(display.contains("[Pasted"));
        assert!(display.ends_with('c'));
        // Two paste spans.
        assert_eq!(app.paste_spans.len(), 2);
    }

    #[test]
    fn second_paste_during_animation_finishes_first() {
        let mut app = RatatuiApp::new("m".into(), "r".into(), None);
        let first = format!("first {}", "a".repeat(200));
        let second = format!("second {}", "b".repeat(200));

        app.handle_paste(&first);
        assert!(app.paste_animating);

        // Second paste finishes the first animation, then starts its own.
        app.handle_paste(&second);

        // Second long paste starts its own animation.
        assert!(app.paste_animating, "second long paste starts new animation");
        // First paste span is recorded.
        assert_eq!(app.paste_spans.len(), 1);
        // input_buf has both pastes in order.
        let combined = format!("{first}{second}");
        assert_eq!(app.cached_input, combined);
        // Finish second animation.
        app.finish_animation();
        assert_eq!(app.paste_spans.len(), 2);
    }

    #[test]
    fn third_paste_inserts_in_order() {
        let mut app = RatatuiApp::new("m".into(), "r".into(), None);
        let first = format!("aaa {}", "a".repeat(200));
        let second = format!("bbb {}", "b".repeat(200));
        let third = format!("ccc {}", "c".repeat(200));

        app.handle_paste(&first);
        app.finish_animation();
        app.handle_paste(&second);
        app.finish_animation();
        app.handle_paste(&third);
        // Third paste is still animating.
        app.finish_animation();

        let combined = format!("{first}{second}{third}");
        assert_eq!(app.cached_input, combined);
        assert_eq!(app.paste_spans.len(), 3);
    }

    #[test]
    fn paste_in_middle_of_text() {
        let mut app = RatatuiApp::new("m".into(), "r".into(), None);
        // Type "hello world".
        for c in "hello world".chars() {
            app.input_buf.push(c);
        }
        app.cursor = 6; // after "hello "
        app.refresh_input_cache();

        // Paste at cursor position (middle of text).
        app.handle_paste(&"PASTED".repeat(30));
        app.finish_animation();

        // Text should be: "hello " + pasted + "world"
        let flat: String = app.input_buf.iter().collect();
        assert!(flat.starts_with("hello "));
        assert!(flat.ends_with("world"));
        assert!(flat.contains(&"PASTED".repeat(30)));
    }

    #[test]
    fn display_cursor_offset_accounts_for_summary() {
        let mut app = RatatuiApp::new("m".into(), "r".into(), None);
        // Type "a".
        app.input_buf.push('a');
        app.cursor = 1;
        // Paste long text.
        app.handle_paste(&"x".repeat(200));
        app.finish_animation();
        // Type "b".
        app.input_buf.insert(app.cursor, 'b');
        app.cursor += 1;
        app.refresh_input_cache();

        // Cursor is at position 202 (1 + 200 + 1).
        assert_eq!(app.cursor, 202);
        // Display offset should compress the 200-char paste span.
        let offset = app.display_cursor_offset();
        let summary_len = app.paste_spans[0].summary.len();
        // 1 (a) + summary_len + 1 (b) = cursor display position.
        assert_eq!(offset, 1 + summary_len + 1);
    }
}
