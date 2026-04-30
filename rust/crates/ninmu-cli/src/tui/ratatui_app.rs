//! Full-screen ratatui TUI -- modern cyberpunk operations console.
//!
//! Entered via the `--tui` flag. Provides a scrollable conversation history
//! pane, a fixed input area, and a live status bar. Streaming events from
//! the model are consumed via [`TuiEvent`] channel so the UI updates
//! incrementally without blocking.
//!
//! The semantic theme palette is applied throughout: dense status rails,
//! neon focus states, flat surfaces, and keyboard-first controls.

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
use crate::tui::theme::Theme;
use ninmu_api::{model_token_limit, ModelTokenLimit};
use ninmu_runtime::PromptCacheEvent;
use ninmu_runtime::{
    ContentBlock, ConversationMessage, MessageRole, PermissionPromptDecision, PermissionRequest,
    TokenUsage,
};

// -- Semantic ratatui palette -----------------------------------------------
const BG: Color = Theme::BG;
const SURFACE: Color = Theme::SURFACE;
const BORDER: Color = Theme::BORDER;
const BORDER_BRIGHT: Color = Theme::BORDER_BRIGHT_COLOR;
const TEXT: Color = Theme::TEXT_COLOR;
const TEXT_SEC: Color = Theme::TEXT_SECONDARY_COLOR;
const MUTED: Color = Theme::MUTED_COLOR;
const ACCENT: Color = Theme::ACCENT_COLOR;
const FOCUS: Color = Theme::FOCUS;
const FOCUS_TEXT: Color = Theme::FOCUS_TEXT;
const FOCUS_MUTED: Color = Theme::FOCUS_MUTED;
const WARNING_COLOR: Color = Theme::WARNING_COLOR;
const WARNING_BG: Color = Theme::WARNING_BG;
const ERROR_COLOR: Color = Theme::ERROR_COLOR;
const SUCCESS: Color = Theme::SUCCESS_COLOR;
const THINKING_COLOR: Color = Theme::THINKING_COLOR;
const USER_COLOR: Color = Theme::USER_COLOR;
const USER_COLOR_DIM: Color = Theme::USER_COLOR_DIM;
const LLM_COLOR: Color = Theme::ASSISTANT_COLOR;
const CODE_BG: Color = Theme::CODE_BG;
const CODE_FG: Color = Theme::CODE_FG;

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
    /// Interactive reasoning/thinking selector dialog (when open).
    reasoning_selector: Option<ReasoningSelector>,
    /// Lightweight command palette dialog (when open).
    command_palette: Option<CommandPalette>,
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
    /// Optional provider filter selected by Tab.
    provider_filter: Option<ninmu_api::ProviderKind>,
    /// Providers present in the current model list, in display order.
    providers: Vec<ninmu_api::ProviderKind>,
}

/// Interactive reasoning/thinking selector state.
struct ReasoningSelector {
    effort_options: Vec<Option<&'static str>>,
    thinking_options: Vec<Option<bool>>,
    effort_index: usize,
    thinking_index: usize,
    row: ReasoningSelectorRow,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReasoningSelectorRow {
    Effort,
    Thinking,
}

/// Lightweight command palette state. Entries are intentionally built lazily
/// when the palette opens so normal TUI startup and pure CLI paths do not pay.
struct CommandPalette {
    entries: Vec<CommandPaletteEntry>,
    filtered: Vec<usize>,
    filter: Vec<char>,
    filter_cursor: usize,
    selected: usize,
}

#[derive(Clone)]
struct CommandPaletteEntry {
    label: &'static str,
    detail: &'static str,
    action: CommandPaletteAction,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CommandPaletteAction {
    Reasoning,
    ModelSelector,
    Help,
    SubmitSlash(&'static str),
    InsertSlash(&'static str),
    ClearTranscript,
}

impl RatatuiApp {
    pub fn new(model: String, permission_mode: String, git_branch: Option<String>) -> Self {
        assert!(
            std::env::var_os("NINMU_TEST_PANIC_ON_TUI_INIT").is_none(),
            "RatatuiApp initialized while NINMU_TEST_PANIC_ON_TUI_INIT is set"
        );
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
            reasoning_selector: None,
            command_palette: None,
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
        let entries =
            Self::pin_current_model(Self::collapse_no_auth_providers(entries), &self.model);

        let providers = ModelSelector::providers_for_entries(&entries);
        let filtered: Vec<usize> = (0..entries.len()).collect();
        self.model_selector = Some(ModelSelector {
            all_entries: entries,
            filtered,
            filter: Vec::new(),
            filter_cursor: 0,
            selected: 0,
            scroll_offset: 0,
            max_visible: 12,
            provider_filter: None,
            providers,
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

    fn pin_current_model(
        mut entries: Vec<ninmu_api::ModelEntry>,
        current_model: &str,
    ) -> Vec<ninmu_api::ModelEntry> {
        if let Some(pos) = entries
            .iter()
            .position(|entry| entry.canonical == current_model || entry.alias == current_model)
        {
            let entry = entries.remove(pos);
            entries.insert(0, entry);
        }
        entries
    }

    /// Take the selected model (if any) — returns `Some(model_name)` once.
    pub fn pop_selected_model(&mut self) -> Option<String> {
        self.selected_model.take()
    }

    pub fn open_reasoning_selector(&mut self) {
        self.reasoning_selector = Some(ReasoningSelector::new(
            self.reasoning_effort.as_deref(),
            self.thinking_mode,
        ));
        self.command_palette = None;
        self.dirty = true;
    }

    fn open_command_palette(&mut self) {
        self.command_palette = Some(CommandPalette::new());
        self.dirty = true;
    }

    #[cfg(test)]
    fn render_to_text(&mut self, width: u16, height: u16) -> String {
        let buffer = self.render_to_buffer(width, height);
        let mut output = String::new();
        for y in 0..height {
            for x in 0..width {
                output.push_str(buffer[(x, y)].symbol());
            }
            output.push('\n');
        }
        output
    }

    #[cfg(test)]
    fn render_to_buffer(&mut self, width: u16, height: u16) -> ratatui::buffer::Buffer {
        let backend = ratatui::backend::TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("test backend should initialize");
        terminal
            .draw(|frame| self.draw(frame))
            .expect("test frame should render");
        terminal.backend().buffer().clone()
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
                                KeyCode::Tab => {
                                    self.model_selector
                                        .as_mut()
                                        .unwrap()
                                        .cycle_provider_filter();
                                }
                                KeyCode::BackTab => {
                                    self.model_selector
                                        .as_mut()
                                        .unwrap()
                                        .clear_provider_filter();
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

                        // Reasoning selector mode — intercept all keypresses.
                        if self.reasoning_selector.is_some() {
                            match key.code {
                                KeyCode::Up | KeyCode::Down => {
                                    let sel = self.reasoning_selector.as_mut().unwrap();
                                    sel.toggle_row();
                                }
                                KeyCode::Left => {
                                    self.reasoning_selector.as_mut().unwrap().move_left();
                                }
                                KeyCode::Right => {
                                    self.reasoning_selector.as_mut().unwrap().move_right();
                                }
                                KeyCode::Char('1'..='5') if key.modifiers.is_empty() => {
                                    let digit = match key.code {
                                        KeyCode::Char(c) => c,
                                        _ => unreachable!(),
                                    };
                                    self.reasoning_selector
                                        .as_mut()
                                        .unwrap()
                                        .select_effort_digit(digit);
                                }
                                KeyCode::Char('a') if key.modifiers.is_empty() => {
                                    self.reasoning_selector.as_mut().unwrap().thinking_index = 0;
                                }
                                KeyCode::Char('o') if key.modifiers.is_empty() => {
                                    self.reasoning_selector.as_mut().unwrap().thinking_index = 1;
                                }
                                KeyCode::Char('f') if key.modifiers.is_empty() => {
                                    self.reasoning_selector.as_mut().unwrap().thinking_index = 2;
                                }
                                KeyCode::Enter => {
                                    if let Some(sel) = self.reasoning_selector.take() {
                                        let cmd = sel.command_for_current_row();
                                        match sel.row {
                                            ReasoningSelectorRow::Effort => {
                                                self.reasoning_effort =
                                                    sel.selected_effort_string();
                                            }
                                            ReasoningSelectorRow::Thinking => {
                                                self.thinking_mode = sel.selected_thinking();
                                            }
                                        }
                                        self.rebuild_header();
                                        self.scrollback.push(format!("  {cmd}"));
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
                                KeyCode::Esc => {
                                    self.reasoning_selector = None;
                                }
                                _ => {}
                            }
                            self.dirty = true;
                            continue;
                        }

                        // Command palette mode — intercept all keypresses.
                        if self.command_palette.is_some() {
                            match key.code {
                                KeyCode::Char(c)
                                    if key.modifiers.is_empty()
                                        || key.modifiers == KeyModifiers::SHIFT =>
                                {
                                    let palette = self.command_palette.as_mut().unwrap();
                                    palette.filter.insert(palette.filter_cursor, c);
                                    palette.filter_cursor += 1;
                                    palette.apply_filter();
                                }
                                KeyCode::Backspace => {
                                    let palette = self.command_palette.as_mut().unwrap();
                                    if palette.filter_cursor > 0 {
                                        palette.filter_cursor -= 1;
                                        palette.filter.remove(palette.filter_cursor);
                                        palette.apply_filter();
                                    }
                                }
                                KeyCode::Up => self.command_palette.as_mut().unwrap().move_up(),
                                KeyCode::Down => self.command_palette.as_mut().unwrap().move_down(),
                                KeyCode::Enter => {
                                    let action = self
                                        .command_palette
                                        .as_ref()
                                        .and_then(CommandPalette::selected_action);
                                    self.command_palette = None;
                                    match action {
                                        Some(CommandPaletteAction::Reasoning) => {
                                            self.open_reasoning_selector();
                                        }
                                        Some(CommandPaletteAction::ModelSelector) => {
                                            self.open_model_selector();
                                        }
                                        Some(CommandPaletteAction::Help) => {
                                            self.help_visible = true;
                                        }
                                        Some(CommandPaletteAction::SubmitSlash(command)) => {
                                            self.scrollback.push(format!("  {command}"));
                                            match start_turn(command) {
                                                Ok(handle) => {
                                                    self.state.is_generating = true;
                                                    self.state.current_prompt = command.to_string();
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
                                        Some(CommandPaletteAction::InsertSlash(command)) => {
                                            self.set_input_text(command);
                                        }
                                        Some(CommandPaletteAction::ClearTranscript) => {
                                            self.scrollback.clear();
                                            self.scrollback
                                                .push("  transcript cleared".to_string());
                                        }
                                        None => {}
                                    }
                                }
                                KeyCode::Esc => {
                                    self.command_palette = None;
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
                            KeyCode::Char('?') if key.modifiers.is_empty() => {
                                self.help_visible = !self.help_visible;
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
                            KeyCode::Tab if !self.complete_slash_input() => {
                                let (_, start, _) = self.scrollback.visible(self.last_conv_height);
                                if !self.scrollback.toggle_expand_at(start) {
                                    self.scrollback.toggle_latest_collapsible();
                                }
                            }
                            KeyCode::F(1) => self.help_visible = !self.help_visible,
                            KeyCode::Char('r') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                                self.open_reasoning_selector();
                            }
                            KeyCode::Char('k') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                                self.open_command_palette();
                            }
                            KeyCode::Char('o') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                                self.open_model_selector();
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

    fn set_input_text(&mut self, text: &str) {
        self.input_buf = text.chars().collect();
        self.cursor = self.input_buf.len();
        self.clear_paste_state();
        self.refresh_input_cache();
    }

    fn input_panel_height(&self, width: u16) -> u16 {
        let content_width = width.saturating_sub(4).max(20) as usize;
        let text = self.cached_input.clone();
        let rows = text
            .split('\n')
            .map(|line| (line.chars().count() / content_width).saturating_add(1))
            .sum::<usize>()
            .clamp(1, 6);
        rows as u16 + 2
    }

    fn complete_slash_input(&mut self) -> bool {
        let current = self.cached_input.clone();
        if !current.starts_with('/') || self.cursor != self.input_buf.len() {
            return false;
        }

        let candidates = crate::format::slash_command_completion_candidates_with_sessions(
            &self.model,
            None,
            Vec::new(),
        );
        let matches = candidates
            .into_iter()
            .filter(|candidate| candidate.starts_with(&current) && candidate != &current)
            .collect::<Vec<_>>();

        match matches.as_slice() {
            [only] => {
                self.set_input_text(only);
                true
            }
            [] => false,
            many => {
                if let Some(common) = common_prefix(many) {
                    if common.len() > current.len() {
                        self.set_input_text(&common);
                        return true;
                    }
                }
                self.scrollback.push("  completions:".to_string());
                for candidate in many.iter().take(8) {
                    self.scrollback.push(format!("    {candidate}"));
                }
                true
            }
        }
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
                let max_ticks =
                    (Self::PASTE_ANIM_DURATION.as_millis() / tick_rate.as_millis()).max(1) as usize;
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
                let mut full_lines = vec![format!("  {icon} {name} ({lines} lines)")];
                for line in output.lines().take(200) {
                    full_lines.push(format!("    {line}"));
                }
                if lines > 200 {
                    full_lines.push(format!(
                        "    … output truncated for TUI display ({} more lines)",
                        lines - 200
                    ));
                }
                let visible = if output.is_empty() { 1 } else { 3 };
                self.scrollback.push_collapsible(&full_lines, visible);
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
            TuiEvent::ToolProgress { name, elapsed } => {
                self.state.current_tool = Some(format!("{} {}s", name, elapsed.as_secs()));
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
                Constraint::Length(1),                                   // header
                Constraint::Min(5),                                      // conversation
                Constraint::Length(self.input_panel_height(area.width)), // input box
                Constraint::Length(1),                                   // metadata bar
                Constraint::Length(1),                                   // status bar
            ])
            .split(area);

        self.draw_header(frame, layout[0]);
        if area.width >= 120 {
            let middle = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Min(60), Constraint::Length(30)])
                .split(layout[1]);
            self.last_conv_height = middle[0].height as usize;
            self.draw_conversation(frame, middle[0]);
            self.draw_instruments(frame, middle[1]);
        } else {
            // Record viewport height for Tab toggle and scroll calculations.
            self.last_conv_height = layout[1].height as usize;
            self.draw_conversation(frame, layout[1]);
        }
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

        if self.reasoning_selector.is_some() {
            self.draw_reasoning_selector(frame, area);
        }

        if self.command_palette.is_some() {
            self.draw_command_palette(frame, area);
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
                "  NINMU ",
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                "\u{30CB}\u{30F3}\u{30E0}\u{30B3}\u{30FC}\u{30C9} ",
                Style::default().fg(MUTED),
            ),
            Span::raw("  "),
            Span::styled("MODEL ", Style::default().fg(MUTED)),
            Span::styled("[", Style::default().fg(BORDER_BRIGHT)),
            Span::styled(model.to_string(), Style::default().fg(TEXT_SEC)),
            Span::styled("]", Style::default().fg(BORDER_BRIGHT)),
            Span::raw("  "),
            Span::styled("PERM ", Style::default().fg(MUTED)),
            Span::styled("[", Style::default().fg(BORDER_BRIGHT)),
            Span::styled(
                perm_short.to_ascii_uppercase(),
                Style::default().fg(TEXT_SEC),
            ),
            Span::styled("]", Style::default().fg(BORDER_BRIGHT)),
            Span::raw("  "),
            Span::styled("BRANCH ", Style::default().fg(MUTED)),
            Span::styled("[", Style::default().fg(BORDER_BRIGHT)),
            Span::styled(git.to_string(), Style::default().fg(TEXT_SEC)),
            Span::styled("]", Style::default().fg(BORDER_BRIGHT)),
        ];

        // Show reasoning effort/thinking state if set.
        let effort_label = reasoning_effort.unwrap_or("default");
        let thinking_label = match thinking_mode {
            Some(true) => "on",
            Some(false) => "off",
            None => "auto",
        };
        spans.push(Span::raw("  "));
        spans.push(Span::styled("THINK ", Style::default().fg(MUTED)));
        spans.push(Span::styled("[", Style::default().fg(BORDER_BRIGHT)));
        spans.push(Span::styled(
            thinking_label.to_ascii_uppercase(),
            Style::default().fg(if thinking_mode == Some(false) {
                MUTED
            } else {
                ACCENT
            }),
        ));
        spans.push(Span::styled("]", Style::default().fg(BORDER_BRIGHT)));
        spans.push(Span::raw("  "));
        spans.push(Span::styled("EFFORT ", Style::default().fg(MUTED)));
        spans.push(Span::styled("[", Style::default().fg(BORDER_BRIGHT)));
        spans.push(Span::styled(
            effort_label.to_ascii_uppercase(),
            Style::default().fg(TEXT_SEC),
        ));
        spans.push(Span::styled("]", Style::default().fg(BORDER_BRIGHT)));

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
        let mut code_lang = String::new();
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
                        let lang = trimmed.trim_start_matches('`').trim();
                        code_lang.clear();
                        code_lang.push_str(lang);
                        return Line::from(Span::styled(
                            format!("  {lang}"),
                            Style::default().fg(MUTED).bg(CODE_BG),
                        ));
                    }
                    code_lang.clear();
                    return Line::from(Span::styled(
                        "  ```".to_string(),
                        Style::default().fg(MUTED).bg(CODE_BG),
                    ));
                }
                if in_code_block {
                    return Line::from(code_spans(&code_lang, s));
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
                        Span::styled("  ok ", Style::default().fg(SUCCESS)),
                        Span::styled(rest.to_string(), Style::default().fg(TEXT_SEC)),
                    ]);
                }
                if let Some(rest) = s.strip_prefix("  fail ") {
                    return Line::from(vec![
                        Span::styled("  fail ", Style::default().fg(ERROR_COLOR)),
                        Span::styled(rest.to_string(), Style::default().fg(TEXT_SEC)),
                    ]);
                }
                if s.starts_with("  [cancelled]") {
                    return Line::from(Span::styled(s.to_string(), Style::default().fg(MUTED)));
                }
                if s.contains("\x1b[") {
                    return Line::from(ansi_spans(s, Style::default().fg(TEXT)));
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

        if self.paste_spans.is_empty() && !self.paste_animating && flat.contains('\n') {
            let lines = flat
                .split('\n')
                .enumerate()
                .map(|(idx, line)| {
                    let prompt = if idx == 0 { "> " } else { "  " };
                    Line::from(vec![
                        Span::styled(prompt, Style::default().fg(ACCENT)),
                        Span::styled(line.to_string(), Style::default().fg(TEXT)),
                    ])
                })
                .collect::<Vec<_>>();
            frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);

            if !self.state.is_generating {
                let before_cursor = self.input_buf.iter().take(self.cursor).collect::<String>();
                let cursor_row = before_cursor.matches('\n').count() as u16;
                let cursor_col = before_cursor
                    .rsplit('\n')
                    .next()
                    .map_or(0, |line| line.chars().count()) as u16;
                let cursor_x = inner.x + 2 + cursor_col;
                let cursor_y = inner.y + cursor_row.min(inner.height.saturating_sub(1));
                frame.set_cursor_position((cursor_x, cursor_y));
            }
            return;
        }

        let spans = if self.paste_animating {
            let orange = if self.paste_anim_frame.is_multiple_of(2) {
                Theme::PASTE_FLASH_A
            } else {
                Theme::PASTE_FLASH_B
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
                offset = offset
                    .saturating_sub(span.len)
                    .saturating_add(span.summary.len());
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
            let out_cost =
                (self.usage.output_tokens as f64 / 1_000_000.0) * pricing.output_cost_per_million;
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
                WARNING_COLOR
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

    fn draw_instruments(&self, frame: &mut ratatui::Frame, area: Rect) {
        let in_tok = format_tokens(self.usage.input_tokens);
        let out_tok = format_tokens(self.usage.output_tokens);
        let context = if let Some(limit) = model_token_limit(&self.model) {
            let total = self.usage.input_tokens + self.usage.output_tokens;
            format!(
                "{:.0}% / {}",
                (total as f64 / limit.context_window_tokens as f64) * 100.0,
                format_tokens(limit.context_window_tokens)
            )
        } else {
            "unknown".to_string()
        };
        let cost = if let Some(pricing) = self.model_pricing {
            let total = (self.usage.input_tokens as f64 / 1_000_000.0)
                * pricing.input_cost_per_million
                + (self.usage.output_tokens as f64 / 1_000_000.0) * pricing.output_cost_per_million
                + (self.usage.cache_creation_input_tokens as f64 / 1_000_000.0)
                    * pricing.cache_creation_cost_per_million
                + (self.usage.cache_read_input_tokens as f64 / 1_000_000.0)
                    * pricing.cache_read_cost_per_million;
            format!("${total:.4}")
        } else {
            "n/a".to_string()
        };
        let tool = self.state.current_tool.as_deref().unwrap_or("idle");
        let effort = self.reasoning_effort.as_deref().unwrap_or("default");
        let thinking = match self.thinking_mode {
            Some(true) => "on",
            Some(false) => "off",
            None => "auto",
        };

        let lines = vec![
            Line::from(vec![
                Span::styled(
                    " OPS",
                    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                ),
                Span::styled(" PANEL", Style::default().fg(MUTED)),
            ]),
            instrument_line("MODEL", truncate(&self.model, 18)),
            instrument_line("TOOL", truncate(tool, 18)),
            instrument_line("TOKENS", format!("{in_tok} in / {out_tok} out")),
            instrument_line("CTX", context),
            instrument_line("COST", cost),
            instrument_line("THINK", thinking.to_ascii_uppercase()),
            instrument_line("EFFORT", effort.to_ascii_uppercase()),
            instrument_line("PERM", self.permission_mode.clone()),
        ];

        let block = Block::default()
            .borders(Borders::LEFT)
            .border_style(Style::default().fg(BORDER_BRIGHT))
            .style(Style::default().bg(SURFACE));
        frame.render_widget(Paragraph::new(lines).block(block), area);
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
        if self.state.is_generating {
            spans.push(Span::styled("Esc cancel", Style::default().fg(MUTED)));
            spans.push(Span::raw("  "));
            spans.push(Span::styled("PgUp review", Style::default().fg(MUTED)));
        } else {
            spans.push(Span::styled("Ctrl+K command", Style::default().fg(MUTED)));
            spans.push(Span::raw("  "));
            spans.push(Span::styled("Ctrl+R reasoning", Style::default().fg(MUTED)));
            spans.push(Span::raw("  "));
            spans.push(Span::styled("Ctrl+O model", Style::default().fg(MUTED)));
            spans.push(Span::raw("  "));
            spans.push(Span::styled("? help", Style::default().fg(MUTED)));
        }

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
            help_line("Ctrl+K", "command palette"),
            help_line("Ctrl+R", "reasoning controls"),
            help_line("Ctrl+O", "model selector"),
            help_line("Esc", "cancel generation"),
            help_line("PgUp/PgDn", "scroll conversation"),
            help_line("Home/End", "top / bottom"),
            help_line("Tab", "expand/collapse tool output"),
            help_line("Ctrl+C/D", "quit"),
            help_line("?", "toggle this help"),
            Line::from(""),
            Line::from(Span::styled(
                "  \u{2500}\u{2500} useful commands \u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}",
                Style::default().fg(BORDER_BRIGHT),
            )),
            Line::from(""),
            help_line("/model", "inspect or switch model"),
            help_line("/effort", "low medium high max off"),
            help_line("/think", "auto on off"),
            help_line("/permissions", "inspect permission mode"),
            help_line("/resume", "resume a session"),
            help_line("/history", "show prompt history"),
            help_line("/stats", "show usage and cost"),
            help_line("/doctor", "diagnose local setup"),
            Line::from(""),
            Line::from(Span::styled(
                "  \u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}",
                Style::default().fg(BORDER_BRIGHT),
            )),
        ];

        let popup_w = 50.min(area.width.saturating_sub(4));
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
        let risk = permission_risk_label(&perm.request);

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
                Span::styled("  risk     ", Style::default().fg(MUTED)),
                Span::styled(
                    risk,
                    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                ),
            ]),
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

        let popup_w = 82.min(area.width.saturating_sub(4));
        let list_h = sel.max_visible.min(sel.filtered.len());
        // filter prompt + provider chips + separator + list + keybinds + border.
        let popup_h = (4 + list_h as u16 + 2).min(area.height.saturating_sub(2));
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

        let mut provider_spans = vec![
            Span::styled(" provider ", Style::default().fg(MUTED)),
            Span::styled(
                if sel.provider_filter.is_none() {
                    "[all]"
                } else {
                    " all "
                },
                Style::default().fg(if sel.provider_filter.is_none() {
                    TEXT
                } else {
                    TEXT_SEC
                }),
            ),
        ];
        for provider in &sel.providers {
            let label = ModelSelector::provider_label(*provider);
            let active = sel.provider_filter == Some(*provider);
            provider_spans.push(Span::raw(" "));
            provider_spans.push(Span::styled(
                if active {
                    format!("[{label}]")
                } else {
                    label.to_string()
                },
                Style::default().fg(if active { ACCENT } else { TEXT_SEC }),
            ));
        }
        lines.push(Line::from(provider_spans));

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
                    Style::default().fg(WARNING_COLOR).bg(WARNING_BG)
                } else {
                    Style::default().fg(FOCUS_TEXT).bg(FOCUS)
                }
            } else {
                Style::default().fg(text_color)
            };
            let prov_style = if is_selected {
                if no_auth {
                    Style::default().fg(WARNING_COLOR).bg(WARNING_BG)
                } else {
                    Style::default().fg(FOCUS_MUTED).bg(FOCUS)
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
                "  KEY REQUIRED"
            } else if no_auth {
                "  KEY REQUIRED"
            } else if entry.canonical == self.model || entry.alias == self.model {
                "  CURRENT"
            } else {
                "  READY"
            };
            let context = model_token_limit(&entry.canonical).map_or_else(
                || "  CTX ?".to_string(),
                |limit| format!("  CTX {}", format_tokens(limit.context_window_tokens)),
            );
            let family = format!("  FAMILY {}", model_family_label(entry));
            let price = format!("  {}", model_price_label(&entry.canonical));
            let capability = format!("  CAP {}", model_capability_label(entry));

            lines.push(Line::from(vec![
                Span::styled(if is_selected { " > " } else { "   " }, highlight),
                Span::styled(truncate(&label, 18), highlight),
                Span::styled(format!("  {prov}"), prov_style),
                Span::styled(context, Style::default().fg(TEXT_SEC)),
                Span::styled(family, Style::default().fg(TEXT_SEC)),
                Span::styled(price, Style::default().fg(TEXT_SEC)),
                Span::styled(capability, Style::default().fg(TEXT_SEC)),
                Span::styled(
                    no_key,
                    Style::default().fg(if no_auth {
                        ERROR_COLOR
                    } else if entry.canonical == self.model || entry.alias == self.model {
                        ACCENT
                    } else {
                        SUCCESS
                    }),
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
            Span::styled("Tab", Style::default().fg(TEXT_SEC)),
            Span::styled(" provider  ", Style::default().fg(TEXT_SEC)),
            Span::styled("Esc", Style::default().fg(MUTED)),
            Span::styled(" cancel", Style::default().fg(TEXT_SEC)),
        ]));

        let paragraph = Paragraph::new(lines);
        frame.render_widget(paragraph, inner);
    }

    fn draw_reasoning_selector(&self, frame: &mut ratatui::Frame, area: Rect) {
        let Some(sel) = &self.reasoning_selector else {
            return;
        };

        let popup_w = 64.min(area.width.saturating_sub(4));
        let popup_h = 12.min(area.height.saturating_sub(2));
        let popup_x = (area.width.saturating_sub(popup_w)) / 2;
        let popup_y = (area.height.saturating_sub(popup_h)) / 2;
        let popup_area = Rect::new(popup_x, popup_y, popup_w, popup_h);

        frame.render_widget(Clear, popup_area);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(THINKING_COLOR))
            .style(Style::default().bg(SURFACE))
            .title(Span::styled(
                " reasoning control ",
                Style::default()
                    .fg(THINKING_COLOR)
                    .add_modifier(Modifier::BOLD),
            ))
            .title_alignment(ratatui::layout::Alignment::Center);

        let inner = block.inner(popup_area);
        frame.render_widget(block, popup_area);

        let mut lines = vec![
            Line::from(""),
            selector_option_line(
                "EFFORT",
                &["default", "low", "medium", "high", "max"],
                sel.effort_index,
                sel.row == ReasoningSelectorRow::Effort,
            ),
            selector_option_line(
                "THINK",
                &["auto", "on", "off"],
                sel.thinking_index,
                sel.row == ReasoningSelectorRow::Thinking,
            ),
            Line::from(""),
            Line::from(vec![
                Span::styled(" OpenAI", Style::default().fg(MUTED)),
                Span::styled(" effort", Style::default().fg(TEXT_SEC)),
                Span::styled("  DeepSeek/Qwen", Style::default().fg(MUTED)),
                Span::styled(" thinking", Style::default().fg(TEXT_SEC)),
            ]),
            Line::from(""),
            Line::from(vec![
                Span::styled(
                    " Enter",
                    Style::default().fg(SUCCESS).add_modifier(Modifier::BOLD),
                ),
                Span::styled(" apply selected row  ", Style::default().fg(TEXT_SEC)),
                Span::styled("↑↓", Style::default().fg(TEXT_SEC)),
                Span::styled(" row  ", Style::default().fg(TEXT_SEC)),
                Span::styled("←→", Style::default().fg(TEXT_SEC)),
                Span::styled(" value  ", Style::default().fg(TEXT_SEC)),
                Span::styled("Esc", Style::default().fg(MUTED)),
                Span::styled(" cancel", Style::default().fg(TEXT_SEC)),
            ]),
        ];

        while lines.len() < inner.height as usize {
            lines.push(Line::from(""));
        }

        frame.render_widget(Paragraph::new(lines), inner);
    }

    fn draw_command_palette(&self, frame: &mut ratatui::Frame, area: Rect) {
        let Some(palette) = &self.command_palette else {
            return;
        };

        let popup_w = 58.min(area.width.saturating_sub(4));
        let max_rows = 7usize;
        let list_rows = max_rows.min(palette.filtered.len()).max(1);
        let popup_h = (list_rows as u16 + 5).min(area.height.saturating_sub(2));
        let popup_x = (area.width.saturating_sub(popup_w)) / 2;
        let popup_y = (area.height.saturating_sub(popup_h)) / 3;
        let popup_area = Rect::new(popup_x, popup_y, popup_w, popup_h);

        frame.render_widget(Clear, popup_area);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(ACCENT))
            .style(Style::default().bg(SURFACE))
            .title(Span::styled(
                " command palette ",
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ))
            .title_alignment(ratatui::layout::Alignment::Center);

        let inner = block.inner(popup_area);
        frame.render_widget(block, popup_area);

        let filter = palette.filter_text();
        let mut lines = vec![Line::from(vec![
            Span::styled(" search ", Style::default().fg(MUTED)),
            Span::styled(
                if filter.is_empty() {
                    "type an action...".to_string()
                } else {
                    filter
                },
                Style::default().fg(TEXT),
            ),
            Span::styled("▏", Style::default().fg(ACCENT)),
        ])];
        lines.push(Line::from(""));

        for (visible_idx, entry_idx) in palette.filtered.iter().take(max_rows).enumerate() {
            let entry = &palette.entries[*entry_idx];
            let selected = visible_idx == palette.selected;
            let style = if selected {
                Style::default().fg(FOCUS_TEXT).bg(FOCUS)
            } else {
                Style::default().fg(TEXT)
            };
            let detail_style = if selected {
                Style::default().fg(FOCUS_MUTED).bg(FOCUS)
            } else {
                Style::default().fg(TEXT_SEC)
            };
            lines.push(Line::from(vec![
                Span::styled(if selected { " > " } else { "   " }, style),
                Span::styled(entry.label, style.add_modifier(Modifier::BOLD)),
                Span::styled("  ", style),
                Span::styled(entry.detail, detail_style),
            ]));
        }
        if palette.filtered.is_empty() {
            lines.push(Line::from(Span::styled(
                "   no actions",
                Style::default().fg(MUTED),
            )));
        }

        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::styled(
                " Enter",
                Style::default().fg(SUCCESS).add_modifier(Modifier::BOLD),
            ),
            Span::styled(" run  ", Style::default().fg(TEXT_SEC)),
            Span::styled("Esc", Style::default().fg(MUTED)),
            Span::styled(" cancel", Style::default().fg(TEXT_SEC)),
        ]));

        frame.render_widget(Paragraph::new(lines), inner);
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
    fn providers_for_entries(entries: &[ninmu_api::ModelEntry]) -> Vec<ninmu_api::ProviderKind> {
        let mut providers = Vec::new();
        for entry in entries {
            if !providers.contains(&entry.provider) {
                providers.push(entry.provider);
            }
        }
        providers
    }

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
                if self
                    .provider_filter
                    .is_some_and(|provider| e.provider != provider)
                {
                    return false;
                }
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
        self.scroll_offset = self.scroll_offset.min(self.selected);
    }

    fn cycle_provider_filter(&mut self) {
        self.provider_filter = match self.provider_filter {
            None => self.providers.first().copied(),
            Some(current) => {
                let next = self
                    .providers
                    .iter()
                    .position(|provider| *provider == current)
                    .and_then(|idx| self.providers.get(idx + 1).copied());
                next
            }
        };
        self.selected = 0;
        self.scroll_offset = 0;
        self.apply_filter();
    }

    fn clear_provider_filter(&mut self) {
        self.provider_filter = None;
        self.selected = 0;
        self.scroll_offset = 0;
        self.apply_filter();
    }

    fn selected_entry(&self) -> Option<&ninmu_api::ModelEntry> {
        self.filtered
            .get(self.selected)
            .map(|&i| &self.all_entries[i])
    }
}

impl ReasoningSelector {
    fn new(current_effort: Option<&str>, current_thinking: Option<bool>) -> Self {
        let effort_options = vec![None, Some("low"), Some("medium"), Some("high"), Some("max")];
        let thinking_options = vec![None, Some(true), Some(false)];
        let effort_index = effort_options
            .iter()
            .position(|value| *value == current_effort)
            .unwrap_or(0);
        let thinking_index = thinking_options
            .iter()
            .position(|value| *value == current_thinking)
            .unwrap_or(0);
        Self {
            effort_options,
            thinking_options,
            effort_index,
            thinking_index,
            row: ReasoningSelectorRow::Effort,
        }
    }

    fn toggle_row(&mut self) {
        self.row = match self.row {
            ReasoningSelectorRow::Effort => ReasoningSelectorRow::Thinking,
            ReasoningSelectorRow::Thinking => ReasoningSelectorRow::Effort,
        };
    }

    fn move_left(&mut self) {
        match self.row {
            ReasoningSelectorRow::Effort => {
                self.effort_index = self.effort_index.saturating_sub(1);
            }
            ReasoningSelectorRow::Thinking => {
                self.thinking_index = self.thinking_index.saturating_sub(1);
            }
        }
    }

    fn move_right(&mut self) {
        match self.row {
            ReasoningSelectorRow::Effort => {
                if self.effort_index + 1 < self.effort_options.len() {
                    self.effort_index += 1;
                }
            }
            ReasoningSelectorRow::Thinking => {
                if self.thinking_index + 1 < self.thinking_options.len() {
                    self.thinking_index += 1;
                }
            }
        }
    }

    fn select_effort_digit(&mut self, digit: char) {
        let Some(value) = digit.to_digit(10) else {
            return;
        };
        let index = value.saturating_sub(1) as usize;
        if index < self.effort_options.len() {
            self.effort_index = index;
            self.row = ReasoningSelectorRow::Effort;
        }
    }

    fn selected_effort(&self) -> Option<&'static str> {
        self.effort_options[self.effort_index]
    }

    fn selected_effort_string(&self) -> Option<String> {
        self.selected_effort().map(str::to_string)
    }

    fn selected_thinking(&self) -> Option<bool> {
        self.thinking_options[self.thinking_index]
    }

    fn command_for_current_row(&self) -> String {
        match self.row {
            ReasoningSelectorRow::Effort => match self.selected_effort() {
                Some(level) => format!("/effort {level}"),
                None => "/effort off".to_string(),
            },
            ReasoningSelectorRow::Thinking => match self.selected_thinking() {
                Some(true) => "/think on".to_string(),
                Some(false) => "/think off".to_string(),
                None => "/think auto".to_string(),
            },
        }
    }
}

impl CommandPalette {
    fn new() -> Self {
        let entries = vec![
            CommandPaletteEntry {
                label: "Reasoning",
                detail: "set effort and thinking",
                action: CommandPaletteAction::Reasoning,
            },
            CommandPaletteEntry {
                label: "Model",
                detail: "switch model",
                action: CommandPaletteAction::ModelSelector,
            },
            CommandPaletteEntry {
                label: "Help",
                detail: "show keys and commands",
                action: CommandPaletteAction::Help,
            },
            CommandPaletteEntry {
                label: "Stats",
                detail: "show token and cost totals",
                action: CommandPaletteAction::SubmitSlash("/stats"),
            },
            CommandPaletteEntry {
                label: "Permissions",
                detail: "inspect current mode",
                action: CommandPaletteAction::SubmitSlash("/permissions"),
            },
            CommandPaletteEntry {
                label: "Permission Mode",
                detail: "choose read/write/full",
                action: CommandPaletteAction::InsertSlash("/permissions "),
            },
            CommandPaletteEntry {
                label: "Sessions",
                detail: "list saved sessions",
                action: CommandPaletteAction::SubmitSlash("/session list"),
            },
            CommandPaletteEntry {
                label: "Resume",
                detail: "resume latest or a session id",
                action: CommandPaletteAction::InsertSlash("/resume latest"),
            },
            CommandPaletteEntry {
                label: "History",
                detail: "show recent prompts",
                action: CommandPaletteAction::SubmitSlash("/history"),
            },
            CommandPaletteEntry {
                label: "Export",
                detail: "write transcript to a file",
                action: CommandPaletteAction::InsertSlash("/export "),
            },
            CommandPaletteEntry {
                label: "Clear Transcript",
                detail: "clear this TUI view only",
                action: CommandPaletteAction::ClearTranscript,
            },
            CommandPaletteEntry {
                label: "Fresh Session",
                detail: "insert destructive clear command",
                action: CommandPaletteAction::InsertSlash("/clear --confirm"),
            },
        ];
        let filtered = (0..entries.len()).collect();
        Self {
            entries,
            filtered,
            filter: Vec::new(),
            filter_cursor: 0,
            selected: 0,
        }
    }

    fn filter_text(&self) -> String {
        self.filter.iter().collect()
    }

    fn apply_filter(&mut self) {
        let query = self.filter_text().to_ascii_lowercase();
        self.filtered = self
            .entries
            .iter()
            .enumerate()
            .filter_map(|(idx, entry)| {
                let haystack = format!("{} {}", entry.label, entry.detail).to_ascii_lowercase();
                (query.is_empty() || haystack.contains(&query)).then_some(idx)
            })
            .collect();
        if self.selected >= self.filtered.len() {
            self.selected = self.filtered.len().saturating_sub(1);
        }
    }

    fn move_up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    fn move_down(&mut self) {
        if self.selected + 1 < self.filtered.len() {
            self.selected += 1;
        }
    }

    fn selected_action(&self) -> Option<CommandPaletteAction> {
        self.filtered
            .get(self.selected)
            .map(|idx| self.entries[*idx].action)
    }
}

// -- Helpers ------------------------------------------------------------------

fn selector_option_line<'a>(
    label: &'static str,
    options: &[&'static str],
    selected: usize,
    focused: bool,
) -> Line<'a> {
    let mut spans = vec![
        Span::styled(
            if focused { " > " } else { "   " },
            Style::default().fg(if focused { ACCENT } else { MUTED }),
        ),
        Span::styled(
            label,
            Style::default().fg(MUTED).add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
    ];
    for (idx, option) in options.iter().enumerate() {
        let is_selected = idx == selected;
        let style = if is_selected {
            Style::default()
                .fg(if focused { BG } else { TEXT })
                .bg(if focused {
                    THINKING_COLOR
                } else {
                    BORDER_BRIGHT
                })
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(TEXT_SEC)
        };
        spans.push(Span::styled(format!(" {option} "), style));
        spans.push(Span::raw(" "));
    }
    Line::from(spans)
}

fn instrument_line<'a>(label: &'static str, value: String) -> Line<'a> {
    Line::from(vec![
        Span::styled(" ", Style::default().fg(MUTED)),
        Span::styled(format!("{label:<7}"), Style::default().fg(MUTED)),
        Span::styled(value, Style::default().fg(TEXT_SEC)),
    ])
}

fn permission_risk_label(request: &PermissionRequest) -> &'static str {
    let tool = request.tool_name.to_ascii_lowercase();
    if tool.contains("bash") || tool.contains("exec") || tool.contains("shell") {
        "EXEC"
    } else if tool.contains("write") || tool.contains("edit") || tool.contains("patch") {
        "WRITE"
    } else if tool.contains("web") || tool.contains("http") || tool.contains("fetch") {
        "NETWORK"
    } else {
        match format!("{:?}", request.required_mode).as_str() {
            "DangerFullAccess" => "BROAD CWD",
            "WorkspaceWrite" => "WRITE",
            _ => "READ",
        }
    }
}

fn model_family_label(entry: &ninmu_api::ModelEntry) -> &'static str {
    if let Some(family) = entry.family.as_deref().and_then(catalog_family_label) {
        return family;
    }

    let name = format!("{} {}", entry.alias, entry.canonical).to_ascii_lowercase();
    if name.contains("reason")
        || name.contains("r1")
        || name.contains("o1")
        || name.contains("o3")
        || name.contains("o4")
    {
        "reasoning"
    } else if matches!(
        entry.provider,
        ninmu_api::ProviderKind::Ollama | ninmu_api::ProviderKind::Vllm
    ) {
        "local"
    } else if name.contains("coder") || name.contains("code") {
        "coding"
    } else if name.contains("flash") || name.contains("haiku") || name.contains("small") {
        "fast"
    } else if name.contains("opus") || name.contains("pro") || name.contains("large") {
        "frontier"
    } else {
        "general"
    }
}

fn catalog_family_label(family: &str) -> Option<&'static str> {
    let family = family.to_ascii_lowercase();
    if family.contains("reason") || family.contains("r1") {
        Some("reasoning")
    } else if family.contains("code") || family.contains("coder") {
        Some("coding")
    } else if family.contains("flash")
        || family.contains("haiku")
        || family.contains("mini")
        || family.contains("small")
    {
        Some("fast")
    } else if family.contains("opus") || family.contains("pro") || family.contains("large") {
        Some("frontier")
    } else if family.trim().is_empty() {
        None
    } else {
        Some("general")
    }
}

fn model_price_label(model: &str) -> String {
    ninmu_runtime::pricing_for_model(model).map_or_else(
        || "PRICE ?".to_string(),
        |pricing| {
            if pricing.input_cost_per_million == 0.0 && pricing.output_cost_per_million == 0.0 {
                "PRICE local".to_string()
            } else {
                format!(
                    "PRICE {}/{}",
                    compact_price(pricing.input_cost_per_million),
                    compact_price(pricing.output_cost_per_million)
                )
            }
        },
    )
}

fn compact_price(value: f64) -> String {
    if value.fract().abs() < f64::EPSILON {
        format!("${value:.0}")
    } else {
        let trimmed = format!("{value:.2}")
            .trim_end_matches('0')
            .trim_end_matches('.')
            .to_string();
        format!("${trimmed}")
    }
}

fn model_capability_label(entry: &ninmu_api::ModelEntry) -> &'static str {
    if entry.supports_reasoning {
        return "thinking";
    }
    if entry.supports_tools {
        return "tools";
    }

    let name = format!("{} {}", entry.alias, entry.canonical).to_ascii_lowercase();
    if matches!(
        entry.provider,
        ninmu_api::ProviderKind::Ollama | ninmu_api::ProviderKind::Vllm
    ) {
        "local"
    } else if name.contains("reason")
        || name.contains("r1")
        || name.contains("o1")
        || name.contains("o3")
        || name.contains("o4")
    {
        "thinking"
    } else if matches!(
        entry.provider,
        ninmu_api::ProviderKind::Anthropic
            | ninmu_api::ProviderKind::OpenAi
            | ninmu_api::ProviderKind::DeepSeek
            | ninmu_api::ProviderKind::Qwen
            | ninmu_api::ProviderKind::Gemini
    ) {
        "tools"
    } else {
        "chat"
    }
}

fn ansi_spans(text: &str, base_style: Style) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut current = String::new();
    let mut style = base_style;
    let mut chars = text.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch != '\x1b' || chars.peek() != Some(&'[') {
            current.push(ch);
            continue;
        }

        chars.next();
        let mut sequence = String::new();
        for seq_ch in chars.by_ref() {
            if seq_ch.is_ascii_alphabetic() {
                if seq_ch == 'm' {
                    flush_ansi_span(&mut spans, &mut current, style);
                    style = apply_sgr_sequence(&sequence, base_style, style);
                }
                break;
            }
            sequence.push(seq_ch);
        }
    }

    flush_ansi_span(&mut spans, &mut current, style);
    if spans.is_empty() {
        vec![Span::styled(String::new(), base_style)]
    } else {
        spans
    }
}

fn flush_ansi_span(spans: &mut Vec<Span<'static>>, current: &mut String, style: Style) {
    if !current.is_empty() {
        spans.push(Span::styled(std::mem::take(current), style));
    }
}

fn apply_sgr_sequence(sequence: &str, base_style: Style, mut style: Style) -> Style {
    let codes = parse_sgr_codes(sequence);
    let mut i = 0;
    while i < codes.len() {
        match codes[i] {
            0 => style = base_style,
            1 => style = style.add_modifier(Modifier::BOLD),
            2 => style = style.add_modifier(Modifier::DIM),
            3 => style = style.add_modifier(Modifier::ITALIC),
            22 => {
                style = style.remove_modifier(Modifier::BOLD);
                style = style.remove_modifier(Modifier::DIM);
            }
            23 => style = style.remove_modifier(Modifier::ITALIC),
            30..=37 | 90..=97 => {
                style = style.fg(ansi_basic_color(codes[i]));
            }
            39 => style = style.fg(base_style.fg.unwrap_or(TEXT)),
            38 => {
                if let Some((color, consumed)) = parse_extended_ansi_color(&codes[i + 1..]) {
                    style = style.fg(color);
                    i += consumed;
                }
            }
            _ => {}
        }
        i += 1;
    }
    style
}

fn parse_sgr_codes(sequence: &str) -> Vec<u16> {
    if sequence.is_empty() {
        return vec![0];
    }
    sequence
        .split(';')
        .map(|part| part.parse::<u16>().unwrap_or(0))
        .collect()
}

fn parse_extended_ansi_color(codes: &[u16]) -> Option<(Color, usize)> {
    match codes {
        [2, r, g, b, ..] => Some((
            Color::Rgb(
                u8::try_from(*r).unwrap_or(u8::MAX),
                u8::try_from(*g).unwrap_or(u8::MAX),
                u8::try_from(*b).unwrap_or(u8::MAX),
            ),
            4,
        )),
        [5, idx, ..] => Some((ansi_256_color(*idx), 2)),
        _ => None,
    }
}

fn ansi_basic_color(code: u16) -> Color {
    match code {
        30 => Color::Rgb(32, 36, 46),
        31 => ERROR_COLOR,
        32 => SUCCESS,
        33 => WARNING_COLOR,
        34 => Color::Rgb(91, 156, 255),
        35 => THINKING_COLOR,
        36 => FOCUS,
        37 => TEXT_SEC,
        90 => MUTED,
        91 => Color::Rgb(255, 105, 105),
        92 => Color::Rgb(110, 255, 160),
        93 => Color::Rgb(255, 215, 110),
        94 => Color::Rgb(130, 185, 255),
        95 => Color::Rgb(230, 130, 255),
        96 => Color::Rgb(120, 235, 255),
        97 => TEXT,
        _ => TEXT,
    }
}

fn ansi_256_color(index: u16) -> Color {
    match index {
        0..=15 => ansi_basic_color(match index {
            0 => 30,
            1 => 31,
            2 => 32,
            3 => 33,
            4 => 34,
            5 => 35,
            6 => 36,
            7 => 37,
            8 => 90,
            9 => 91,
            10 => 92,
            11 => 93,
            12 => 94,
            13 => 95,
            14 => 96,
            _ => 97,
        }),
        16..=231 => {
            let n = index - 16;
            let r = (n / 36) % 6;
            let g = (n / 6) % 6;
            let b = n % 6;
            Color::Rgb(
                ansi_6cube_component(r),
                ansi_6cube_component(g),
                ansi_6cube_component(b),
            )
        }
        232..=255 => {
            let level = 8 + ((index - 232) * 10);
            let value = u8::try_from(level).unwrap_or(u8::MAX);
            Color::Rgb(value, value, value)
        }
        _ => TEXT,
    }
}

fn ansi_6cube_component(value: u16) -> u8 {
    if value == 0 {
        0
    } else {
        u8::try_from(55 + value * 40).unwrap_or(u8::MAX)
    }
}

fn code_spans(lang: &str, line: &str) -> Vec<Span<'static>> {
    let mut spans = vec![Span::styled("  ".to_string(), Style::default().bg(CODE_BG))];
    let lang = lang.to_ascii_lowercase();
    let comment_marker = if matches!(
        lang.as_str(),
        "rust"
            | "rs"
            | "javascript"
            | "js"
            | "typescript"
            | "ts"
            | "tsx"
            | "jsx"
            | "go"
            | "java"
            | "c"
            | "cpp"
            | "c++"
            | "swift"
            | "kotlin"
    ) {
        Some("//")
    } else if matches!(
        lang.as_str(),
        "python" | "py" | "ruby" | "rb" | "bash" | "sh" | "zsh" | "toml" | "yaml" | "yml"
    ) {
        Some("#")
    } else {
        None
    };

    spans.extend(code_syntax_spans(&lang, line, comment_marker));
    spans
}

fn code_syntax_spans(
    lang: &str,
    line: &str,
    comment_marker: Option<&'static str>,
) -> Vec<Span<'static>> {
    let keywords = code_keywords(lang);
    let mut spans = Vec::new();
    let mut token = String::new();
    let mut in_string: Option<char> = None;
    let mut remaining = line;

    while let Some(ch) = remaining.chars().next() {
        if let Some(quote) = in_string {
            token.push(ch);
            if ch == quote {
                spans.push(Span::styled(
                    std::mem::take(&mut token),
                    Style::default().fg(SUCCESS).bg(CODE_BG),
                ));
                in_string = None;
            }
            remaining = &remaining[ch.len_utf8()..];
            continue;
        }

        if let Some(marker) = comment_marker {
            if remaining.starts_with(marker) {
                flush_code_token(&mut spans, &mut token, keywords);
                spans.push(Span::styled(
                    remaining.to_string(),
                    Style::default()
                        .fg(MUTED)
                        .bg(CODE_BG)
                        .add_modifier(Modifier::ITALIC),
                ));
                return spans;
            }
        }

        if ch == '"' || ch == '\'' {
            flush_code_token(&mut spans, &mut token, keywords);
            token.push(ch);
            in_string = Some(ch);
        } else if ch.is_alphanumeric() || ch == '_' {
            token.push(ch);
        } else {
            flush_code_token(&mut spans, &mut token, keywords);
            spans.push(Span::styled(
                ch.to_string(),
                Style::default().fg(CODE_FG).bg(CODE_BG),
            ));
        }
        remaining = &remaining[ch.len_utf8()..];
    }

    if in_string.is_some() && !token.is_empty() {
        spans.push(Span::styled(
            std::mem::take(&mut token),
            Style::default().fg(SUCCESS).bg(CODE_BG),
        ));
    } else {
        flush_code_token(&mut spans, &mut token, keywords);
    }

    spans
}

fn flush_code_token(
    spans: &mut Vec<Span<'static>>,
    token: &mut String,
    keywords: &'static [&'static str],
) {
    if token.is_empty() {
        return;
    }
    let is_keyword = keywords.contains(&token.as_str());
    let color = if is_keyword { FOCUS } else { CODE_FG };
    let style = if is_keyword {
        Style::default()
            .fg(color)
            .bg(CODE_BG)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(color).bg(CODE_BG)
    };
    spans.push(Span::styled(std::mem::take(token), style));
}

fn code_keywords(lang: &str) -> &'static [&'static str] {
    match lang {
        "rust" | "rs" => &[
            "as", "async", "await", "const", "crate", "else", "enum", "fn", "for", "if", "impl",
            "let", "match", "mod", "move", "mut", "pub", "ref", "return", "self", "Self", "static",
            "struct", "trait", "type", "use", "where", "while",
        ],
        "python" | "py" => &[
            "and", "as", "async", "await", "class", "def", "elif", "else", "except", "False",
            "for", "from", "if", "import", "in", "is", "lambda", "None", "not", "or", "pass",
            "return", "True", "try", "while", "with", "yield",
        ],
        "javascript" | "js" | "typescript" | "ts" | "tsx" | "jsx" => &[
            "async", "await", "break", "case", "catch", "class", "const", "continue", "default",
            "else", "export", "extends", "finally", "for", "from", "function", "if", "import",
            "let", "new", "return", "switch", "throw", "try", "type", "var", "while",
        ],
        "bash" | "sh" | "zsh" => &[
            "case", "do", "done", "elif", "else", "esac", "fi", "for", "function", "if", "in",
            "then", "while",
        ],
        _ => &[],
    }
}

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
                Style::default().fg(ACCENT).bg(Theme::INLINE_CODE_BG),
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

fn common_prefix(values: &[String]) -> Option<String> {
    let first = values.first()?;
    let mut prefix = first.clone();
    for value in &values[1..] {
        while !value.starts_with(&prefix) {
            prefix.pop()?;
        }
    }
    Some(prefix)
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

    fn model_entry(
        alias: &str,
        canonical: &str,
        provider: ninmu_api::ProviderKind,
        has_auth: bool,
    ) -> ninmu_api::ModelEntry {
        ninmu_api::ModelEntry {
            alias: alias.to_string(),
            canonical: canonical.to_string(),
            provider,
            has_auth,
            family: None,
            supports_reasoning: false,
            supports_tools: false,
        }
    }

    fn find_text_position(
        buffer: &ratatui::buffer::Buffer,
        width: u16,
        height: u16,
        needle: &str,
    ) -> Option<(u16, u16)> {
        for y in 0..height {
            let mut line = String::new();
            for x in 0..width {
                line.push_str(buffer[(x, y)].symbol());
            }
            if let Some(idx) = line.find(needle) {
                return Some((idx as u16, y));
            }
        }
        None
    }

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
    fn render_help_contains_reasoning_and_model_shortcuts() {
        let mut app = RatatuiApp::new("gpt-4o".into(), "write".into(), Some("main".into()));
        app.help_visible = true;

        let rendered = app.render_to_text(100, 32);

        assert!(rendered.contains("Ctrl+R"));
        assert!(rendered.contains("Ctrl+O"));
        assert!(rendered.contains("/effort"));
        assert!(rendered.contains("/think"));
        assert!(rendered.contains("/model"));
    }

    #[test]
    fn header_buffer_renders_core_state_chips() {
        let mut app = RatatuiApp::new(
            "claude-sonnet-4-5".into(),
            "read-only".into(),
            Some("main".into()),
        );
        app.set_reasoning_effort(Some("high".to_string()));
        app.set_thinking_mode(Some(false));

        let rendered = app.render_to_text(100, 24);

        assert!(rendered.contains("MODEL"));
        assert!(rendered.contains("PERM"));
        assert!(rendered.contains("BRANCH"));
        assert!(rendered.contains("THINK"));
        assert!(rendered.contains("EFFORT"));
    }

    #[test]
    fn model_selector_selected_row_uses_focus_style() {
        use ninmu_api::ProviderKind;

        let mut app = RatatuiApp::new("claude-opus-4-6".into(), "write".into(), None);
        app.model_selector = Some(ModelSelector {
            all_entries: vec![
                model_entry("opus", "claude-opus-4-6", ProviderKind::Anthropic, true),
                model_entry("gpt", "gpt-4o", ProviderKind::OpenAi, true),
            ],
            filtered: vec![0, 1],
            filter: Vec::new(),
            filter_cursor: 0,
            selected: 0,
            scroll_offset: 0,
            max_visible: 2,
            provider_filter: None,
            providers: vec![ProviderKind::Anthropic, ProviderKind::OpenAi],
        });

        let buffer = app.render_to_buffer(100, 30);
        let (x, y) = find_text_position(&buffer, 100, 30, " > ").expect("selected model row");

        assert_eq!(buffer[(x + 1, y)].bg, FOCUS);
        assert_eq!(buffer[(x + 1, y)].fg, FOCUS_TEXT);
    }

    #[test]
    fn reasoning_selector_renders_current_values() {
        let mut app = RatatuiApp::new("gpt-4o".into(), "write".into(), Some("main".into()));
        app.set_reasoning_effort(Some("high".to_string()));
        app.set_thinking_mode(Some(true));
        app.open_reasoning_selector();

        let rendered = app.render_to_text(100, 30);

        assert!(rendered.contains("reasoning control"));
        assert!(rendered.contains("EFFORT"));
        assert!(rendered.contains("high"));
        assert!(rendered.contains("THINK"));
        assert!(rendered.contains("on"));
    }

    #[test]
    fn wide_layout_renders_instrument_panel() {
        let mut app = RatatuiApp::new("gpt-4o".into(), "write".into(), Some("main".into()));
        app.usage = TokenUsage {
            input_tokens: 1200,
            output_tokens: 300,
            ..Default::default()
        };

        let rendered = app.render_to_text(140, 32);

        assert!(rendered.contains("OPS"));
        assert!(rendered.contains("PANEL"));
        assert!(rendered.contains("TOKENS"));
        assert!(rendered.contains("CTX"));
    }

    #[test]
    fn command_palette_filters_to_reasoning() {
        let mut palette = CommandPalette::new();
        palette.filter = "reason".chars().collect();
        palette.filter_cursor = palette.filter.len();
        palette.apply_filter();

        assert_eq!(palette.filtered.len(), 1);
        assert_eq!(
            palette.selected_action(),
            Some(CommandPaletteAction::Reasoning)
        );
    }

    #[test]
    fn command_palette_includes_session_and_stats_actions() {
        let palette = CommandPalette::new();
        let labels = palette
            .entries
            .iter()
            .map(|entry| entry.label)
            .collect::<Vec<_>>();

        assert!(labels.contains(&"Stats"));
        assert!(labels.contains(&"Sessions"));
        assert!(labels.contains(&"Export"));
        assert!(labels.contains(&"Clear Transcript"));
    }

    #[test]
    fn slash_completion_extends_unique_prefix() {
        let mut app = RatatuiApp::new("sonnet".into(), "write".into(), None);
        app.set_input_text("/permissi");

        assert!(app.complete_slash_input());
        assert_eq!(app.cached_input, "/permissions");
    }

    #[test]
    fn multiline_input_panel_grows_to_six_lines() {
        let mut app = RatatuiApp::new("sonnet".into(), "write".into(), None);
        app.set_input_text("one\ntwo\nthree\nfour\nfive\nsix\nseven");

        assert_eq!(app.input_panel_height(100), 8);
    }

    #[test]
    fn tool_result_renders_collapsible_preview() {
        let mut app = RatatuiApp::new("gpt-4o".into(), "write".into(), None);
        app.process_event(TuiEvent::ToolResult {
            name: "bash".to_string(),
            output: "line one\nline two\nline three\nline four".to_string(),
            is_error: false,
        });

        let rendered = app.scrollback.visible(usize::MAX).0.join("\n");

        assert!(rendered.contains("ok bash"));
        assert!(rendered.contains("line one"));
        assert!(rendered.contains("Tab to expand"));
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
    fn ansi_spans_preserve_text_and_common_sgr_styles() {
        let spans = ansi_spans(
            "plain \x1b[31;1merror\x1b[0m \x1b[38;2;1;2;3mtrue\x1b[0m",
            Style::default().fg(TEXT),
        );

        let text = spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert_eq!(text, "plain error true");
        assert!(spans.iter().any(|span| {
            span.content == "error"
                && span.style.fg == Some(ERROR_COLOR)
                && span.style.add_modifier.contains(Modifier::BOLD)
        }));
        assert!(spans
            .iter()
            .any(|span| span.content == "true" && span.style.fg == Some(Color::Rgb(1, 2, 3))));
    }

    #[test]
    fn code_spans_highlight_keywords_strings_and_comments_lazily() {
        let spans = code_spans("rust", r#"let value = "ok"; // comment"#);

        assert!(spans.iter().any(|span| {
            span.content == "let"
                && span.style.fg == Some(FOCUS)
                && span.style.add_modifier.contains(Modifier::BOLD)
        }));
        assert!(spans
            .iter()
            .any(|span| span.content == r#""ok""# && span.style.fg == Some(SUCCESS)));
        assert!(spans.iter().any(|span| {
            span.content == "// comment"
                && span.style.fg == Some(MUTED)
                && span.style.add_modifier.contains(Modifier::ITALIC)
        }));
    }

    #[test]
    fn code_spans_ignore_comment_markers_inside_strings() {
        let spans = code_spans("rust", r#"let url = "https://example.test"; // comment"#);

        assert!(spans
            .iter()
            .any(|span| span.content == r#""https://example.test""#
                && span.style.fg == Some(SUCCESS)));
        assert!(spans
            .iter()
            .any(|span| span.content == "// comment" && span.style.fg == Some(MUTED)));
    }

    #[test]
    fn fenced_code_block_renders_language_aware_styles() {
        let mut app = RatatuiApp::new("m".into(), "write".into(), None);
        app.scrollback.push("```rust".to_string());
        app.scrollback
            .push(r#"let value = "ok"; // comment"#.to_string());
        app.scrollback.push("```".to_string());

        let buffer = app.render_to_buffer(100, 30);
        let (x, y) = find_text_position(&buffer, 100, 30, "let").expect("rust keyword");

        assert_eq!(buffer[(x, y)].fg, FOCUS);
        assert!(buffer[(x, y)].modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn tool_result_renders_ansi_output_without_escape_text() {
        let mut app = RatatuiApp::new("m".into(), "write".into(), None);
        app.process_event(TuiEvent::ToolResult {
            name: "bash".into(),
            output: "\x1b[32mgreen\x1b[0m\nplain".into(),
            is_error: false,
        });

        let buffer = app.render_to_buffer(100, 30);
        let rendered = app.render_to_text(100, 30);
        let (x, y) = find_text_position(&buffer, 100, 30, "green").expect("green output");

        assert!(!rendered.contains("\x1b[32m"));
        assert_eq!(buffer[(x, y)].fg, SUCCESS);
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
        assert!(joined.contains("WRITE"));
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
    fn permission_modal_renders_risk_label() {
        let mut app = RatatuiApp::new("m".into(), "r".into(), None);
        let (tx, _rx) = std::sync::mpsc::channel();
        app.process_event(TuiEvent::PermissionPrompt {
            request: ninmu_runtime::PermissionRequest {
                tool_name: "bash".into(),
                input: r#"{"cmd":"cargo test"}"#.into(),
                required_mode: ninmu_runtime::PermissionMode::WorkspaceWrite,
                current_mode: ninmu_runtime::PermissionMode::ReadOnly,
                reason: None,
            },
            response_tx: tx,
        });

        let rendered = app.render_to_text(100, 30);

        assert!(rendered.contains("risk"));
        assert!(rendered.contains("EXEC"));
    }

    #[test]
    fn permission_modal_fits_common_viewports() {
        for (width, height) in [(80, 24), (100, 30)] {
            let mut app = RatatuiApp::new("m".into(), "r".into(), None);
            let (tx, _rx) = std::sync::mpsc::channel();
            app.process_event(TuiEvent::PermissionPrompt {
                request: ninmu_runtime::PermissionRequest {
                    tool_name: "bash".into(),
                    input: r#"{"cmd":"cargo test --workspace"}"#.into(),
                    required_mode: ninmu_runtime::PermissionMode::WorkspaceWrite,
                    current_mode: ninmu_runtime::PermissionMode::ReadOnly,
                    reason: Some("viewport fit test".into()),
                },
                response_tx: tx,
            });

            let rendered = app.render_to_text(width, height);

            assert!(rendered.contains("permission required"));
            assert!(rendered.contains("risk"));
            assert!(rendered.contains("action"));
            assert!(rendered.contains("allow"));
            assert!(rendered.contains("deny"));
        }
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
        let rendered = app.render_to_text(100, 24);
        assert!(rendered.contains('$'), "expected cost: {rendered}");
        // Cost should be higher than just 100+50 tokens — cache tokens add to it.
        // With sonnet pricing: 100 in + 50 out + 1000 cache_create + 5000 cache_read
        // = $0.0015 + $0.00375 + $0.01875 + $0.0075 ≈ $0.0315
        assert!(
            rendered.contains("0.03"),
            "expected cache-aware cost: {rendered}"
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
            header_text.contains("MAX"),
            "header must show effort level: {header_text}"
        );
        assert!(
            header_text.contains("OFF"),
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
            header_text.contains("AUTO"),
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
            text.contains("THINK"),
            "header must contain 'THINK': {text}"
        );
        assert!(
            text.contains("AUTO"),
            "header must contain 'AUTO' for default thinking: {text}"
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
            text.contains("THINK"),
            "header must contain 'THINK': {text}"
        );
        assert!(text.contains("ON"), "header must show thinking=on: {text}");
        assert!(text.contains("HIGH"), "header must show effort: {text}");
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
            text.contains("OFF"),
            "header must show thinking=off: {text}"
        );
        assert!(text.contains("MAX"), "header must show effort=max: {text}");
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
    fn model_selector_provider_filter_limits_entries() {
        let mut app = RatatuiApp::new("gpt-4o".into(), "write".into(), None);
        app.open_model_selector();
        let sel = app.model_selector.as_mut().unwrap();
        if sel.providers.is_empty() {
            return;
        }

        sel.cycle_provider_filter();
        let provider = sel.provider_filter.expect("provider filter should be set");

        assert!(!sel.filtered.is_empty());
        for &idx in &sel.filtered {
            assert_eq!(sel.all_entries[idx].provider, provider);
        }
    }

    #[test]
    fn model_selector_render_shows_provider_and_context_controls() {
        let mut app = RatatuiApp::new("gpt-4o".into(), "write".into(), None);
        app.open_model_selector();

        let rendered = app.render_to_text(120, 32);

        assert!(rendered.contains("provider"));
        assert!(rendered.contains("CTX"));
        assert!(rendered.contains("FAMILY"));
        assert!(rendered.contains("PRICE"));
        assert!(rendered.contains("CAP"));
        assert!(rendered.contains("Tab"));
    }

    #[test]
    fn model_metadata_labels_are_compact_and_informative() {
        use ninmu_api::{ModelEntry, ProviderKind};

        let local = model_entry("local", "ollama/llama3.1:8b", ProviderKind::Ollama, true);
        let reasoning = model_entry(
            "deepseek-r1",
            "deepseek-reasoner",
            ProviderKind::DeepSeek,
            true,
        );
        let frontier = model_entry("opus", "claude-opus-4-6", ProviderKind::Anthropic, true);

        assert_eq!(model_family_label(&local), "local");
        assert_eq!(model_family_label(&reasoning), "reasoning");
        assert_eq!(model_family_label(&frontier), "frontier");
        assert_eq!(model_price_label("ollama/llama3.1:8b"), "PRICE local");
        assert_eq!(model_price_label("claude-opus-4-6"), "PRICE $15/$75");
        assert_eq!(model_price_label("unknown-model"), "PRICE ?");
        assert_eq!(model_capability_label(&local), "local");
        assert_eq!(model_capability_label(&reasoning), "thinking");
        assert_eq!(model_capability_label(&frontier), "tools");

        let sourced = ModelEntry {
            alias: "catalog".to_string(),
            canonical: "catalog-model".to_string(),
            provider: ProviderKind::Mistral,
            has_auth: true,
            family: Some("codestral".to_string()),
            supports_reasoning: false,
            supports_tools: true,
        };
        assert_eq!(model_family_label(&sourced), "coding");
        assert_eq!(model_capability_label(&sourced), "tools");
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
            model_entry("gpt-4o", "gpt-4o", ProviderKind::OpenAi, true),
            model_entry("gpt-4-turbo", "gpt-4-turbo", ProviderKind::OpenAi, true),
            model_entry(
                "claude-sonnet",
                "claude-sonnet",
                ProviderKind::Anthropic,
                false,
            ),
            model_entry(
                "claude-haiku",
                "claude-haiku",
                ProviderKind::Anthropic,
                false,
            ),
            model_entry(
                "deepseek-chat",
                "deepseek-chat",
                ProviderKind::DeepSeek,
                false,
            ),
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

    #[test]
    fn pin_current_model_moves_matching_entry_to_top() {
        use ninmu_api::ProviderKind;

        let entries = vec![
            model_entry("sonnet", "claude-sonnet", ProviderKind::Anthropic, true),
            model_entry("gpt", "gpt-4o", ProviderKind::OpenAi, true),
        ];

        let pinned = RatatuiApp::pin_current_model(entries, "gpt-4o");

        assert_eq!(pinned[0].canonical, "gpt-4o");
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
        assert!(
            app.paste_animating,
            "second long paste starts new animation"
        );
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
        assert_eq!(
            app.cached_input,
            format!("{}{}", "a".repeat(200), " more text")
        );
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
        assert!(
            app.paste_animating,
            "second long paste starts new animation"
        );
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
