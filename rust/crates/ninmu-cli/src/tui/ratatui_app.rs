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

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
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
use ninmu_runtime::{PermissionPromptDecision, PermissionRequest, TokenUsage};

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
}

/// A permission prompt waiting for the user to respond in the TUI.
struct PendingPermission {
    request: PermissionRequest,
    response_tx: std::sync::mpsc::Sender<PermissionPromptDecision>,
    action_description: String,
}

impl RatatuiApp {
    pub fn new(model: String, permission_mode: String, git_branch: Option<String>) -> Self {
        Self {
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
        }
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
        execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
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
            // -- Render ---------------------------------------------------
            terminal.draw(|frame| self.draw(frame))?;

            // -- Poll events (blocking up to tick_rate) -------------------
            if crossterm::event::poll(tick_rate)? {
                if let Event::Key(key) = event::read()? {
                    if key.kind == KeyEventKind::Press {
                        // Ctrl+C / Ctrl+D always quits
                        if key.modifiers.contains(KeyModifiers::CONTROL)
                            && matches!(key.code, KeyCode::Char('c' | 'd'))
                        {
                            // If a permission prompt is active, deny it and continue.
                            if let Some(perm) = self.pending_permission.take() {
                                let _ = perm.response_tx.send(
                                    PermissionPromptDecision::Deny {
                                        reason: "user pressed Ctrl+C/D".to_string(),
                                    },
                                );
                                self.scrollback.push(format!(
                                    "  denied: {}",
                                    perm.request.tool_name
                                ));
                            }
                            return Ok(());
                        }

                        // Permission prompt mode — intercept all keypresses.
                        if let Some(perm) = self.pending_permission.take() {
                            match key.code {
                                KeyCode::Char('y') | KeyCode::Char('a')
                                    if key.modifiers.is_empty() =>
                                {
                                    let _ = perm
                                        .response_tx
                                        .send(PermissionPromptDecision::Allow);
                                    self.scrollback.push(format!(
                                        "  allowed: {}",
                                        perm.request.tool_name
                                    ));
                                }
                                KeyCode::Char('n') | KeyCode::Char('d')
                                    if key.modifiers.is_empty() =>
                                {
                                    let _ = perm.response_tx.send(
                                        PermissionPromptDecision::Deny {
                                            reason: format!(
                                                "tool '{}' denied by user",
                                                perm.request.tool_name
                                            ),
                                        },
                                    );
                                    self.scrollback.push(format!(
                                        "  denied: {}",
                                        perm.request.tool_name
                                    ));
                                }
                                KeyCode::Char('v') if key.modifiers.is_empty() => {
                                    // View input: push it to scrollback,
                                    // then re-present the prompt.
                                    self.scrollback.push(format!(
                                        "  input: {}",
                                        perm.request.input
                                    ));
                                    self.pending_permission = Some(perm);
                                }
                                KeyCode::Esc => {
                                    let _ = perm.response_tx.send(
                                        PermissionPromptDecision::Deny {
                                            reason: format!(
                                                "tool '{}' denied by user (Esc)",
                                                perm.request.tool_name
                                            ),
                                        },
                                    );
                                    self.scrollback.push(format!(
                                        "  denied: {}",
                                        perm.request.tool_name
                                    ));
                                }
                                _ => {
                                    // Unrecognised key — re-present.
                                    self.pending_permission = Some(perm);
                                }
                            }
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
                                    self.flush_response();
                                    self.scrollback.push("  [cancelled]".to_string());
                                }
                                _ => {}
                            }
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
                            }
                            KeyCode::Enter if !self.input_buf.is_empty() => {
                                let input: String = self.input_buf.drain(..).collect();
                                self.cursor = 0;
                                self.scrollback.push(format!("  > {input}"));

                                match start_turn(&input) {
                                    Ok(handle) => {
                                        self.state.is_generating = true;
                                        self.state.current_prompt = input;
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
                            KeyCode::Char(c)
                                if key.modifiers.is_empty()
                                    || key.modifiers == KeyModifiers::SHIFT =>
                            {
                                self.input_buf.insert(self.cursor, c);
                                self.cursor += 1;
                            }
                            KeyCode::Backspace if self.cursor > 0 => {
                                self.cursor -= 1;
                                self.input_buf.remove(self.cursor);
                            }
                            KeyCode::Delete if self.cursor < self.input_buf.len() => {
                                self.input_buf.remove(self.cursor);
                            }
                            KeyCode::Left if self.cursor > 0 => self.cursor -= 1,
                            KeyCode::Right if self.cursor < self.input_buf.len() => {
                                self.cursor += 1;
                            }
                            KeyCode::Home => self.cursor = 0,
                            KeyCode::End => self.cursor = self.input_buf.len(),
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
                    }
                }
            }

            // -- Drain TuiEvent channel -----------------------------------
            if let Some(ref mut handle) = turn_handle {
                while let Some(ev) = handle.try_recv() {
                    self.process_event(ev);
                }

                if handle.is_finished() {
                    while let Some(ev) = handle.try_recv() {
                        self.process_event(ev);
                    }
                    self.flush_response();
                    self.state.is_generating = false;
                    self.state.thinking_state = ThinkingState::Idle;
                    self.state.current_tool = None;
                    turn_handle.take();
                }
            }

            // -- Advance spinner animation --------------------------------
            if self.tick.elapsed() >= Duration::from_millis(120) {
                self.tick = Instant::now();
                self.spinner_frame = self.spinner_frame.wrapping_add(1);
            }
        }
    }

    fn process_event(&mut self, ev: TuiEvent) {
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
        }
    }

    fn flush_response(&mut self) {
        if !self.response_text.is_empty() {
            self.scrollback.push_str(&self.response_text);
            self.response_text.clear();
        }
        // Show usage summary after response completes
        let total_tokens = self.usage.input_tokens + self.usage.output_tokens;
        if total_tokens > 0 {
            self.scrollback.push(format!(
                "  {} in / {} out tokens",
                self.usage.input_tokens, self.usage.output_tokens,
            ));
        }
    }

    fn update_streaming_display(&mut self) {
        while let Some(pos) = self.response_text.find('\n') {
            let line = self.response_text[..pos].to_string();
            self.scrollback.push(line);
            self.response_text = self.response_text[pos + 1..].to_string();
        }
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
                Constraint::Length(1), // status bar
            ])
            .split(area);

        self.draw_header(frame, layout[0]);
        // Record viewport height for Tab toggle and scroll calculations.
        self.last_conv_height = layout[1].height as usize;
        self.draw_conversation(frame, layout[1]);
        self.draw_input(frame, layout[2]);
        self.draw_status(frame, layout[3]);

        if self.help_visible {
            self.draw_help_overlay(frame, area);
        }

        if self.pending_permission.is_some() {
            self.draw_permission_modal(frame, area);
        }
    }

    fn draw_header(&self, frame: &mut ratatui::Frame, area: Rect) {
        let git = self.git_branch.as_deref().unwrap_or("?");
        let perm_short = match self.permission_mode.as_str() {
            "danger-full-access" => "full",
            "workspace-write" => "write",
            _ => "read",
        };

        let spans = vec![
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
            Span::styled(&*self.model, Style::default().fg(TEXT_SEC)),
            Span::raw("  "),
            Span::styled("perm ", Style::default().fg(MUTED)),
            Span::styled(perm_short, Style::default().fg(TEXT_SEC)),
            Span::raw("  "),
            Span::styled("branch ", Style::default().fg(MUTED)),
            Span::styled(git, Style::default().fg(TEXT_SEC)),
        ];

        let header = Paragraph::new(Line::from(spans)).style(Style::default().bg(SURFACE).fg(TEXT));

        frame.render_widget(header, area);
    }

    fn draw_conversation(&self, frame: &mut ratatui::Frame, area: Rect) {
        let viewport_height = area.height as usize;
        let (visible, _, _total) = self.scrollback.visible(viewport_height);

        let mut lines: Vec<Line> = visible
            .iter()
            .map(|s| {
                let s = s.trim_end();
                // Apply ratatui-native styling based on line content.
                if let Some(rest) = s.strip_prefix("  > ") {
                    // User prompt: accent-colored prompt marker
                    Line::from(vec![
                        Span::styled("  > ", Style::default().fg(ACCENT)),
                        Span::styled(rest.to_string(), Style::default().fg(TEXT)),
                    ])
                } else if let Some(rest) = s.strip_prefix("  error:") {
                    Line::from(vec![
                        Span::styled("  error:", Style::default().fg(ERROR_COLOR)),
                        Span::styled(rest.to_string(), Style::default().fg(TEXT)),
                    ])
                } else if s.starts_with("  -- ") {
                    // Tool use marker
                    Line::from(Span::styled(s.to_string(), Style::default().fg(MUTED)))
                } else if let Some(rest) = s.strip_prefix("  ok ") {
                    Line::from(vec![
                        Span::styled("  ok", Style::default().fg(SUCCESS)),
                        Span::styled(rest.to_string(), Style::default().fg(TEXT_SEC)),
                    ])
                } else if let Some(rest) = s.strip_prefix("  fail ") {
                    Line::from(vec![
                        Span::styled("  fail", Style::default().fg(ERROR_COLOR)),
                        Span::styled(rest.to_string(), Style::default().fg(TEXT_SEC)),
                    ])
                } else if s.starts_with("  [cancelled]") {
                    Line::from(Span::styled(s.to_string(), Style::default().fg(MUTED)))
                } else {
                    // Apply inline markdown formatting
                    Line::from(markdown_spans(s))
                }
            })
            .collect();

        if self.state.is_generating {
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

        let input_text: String = self.input_buf.iter().collect();

        let spans = vec![
            Span::styled("> ", Style::default().fg(ACCENT)),
            Span::styled(input_text, Style::default().fg(TEXT)),
        ];

        let paragraph = Paragraph::new(Line::from(spans));
        frame.render_widget(paragraph, inner);

        if !self.state.is_generating {
            let cursor_x = inner.x + 2 + u16::try_from(self.cursor).unwrap_or(u16::MAX);
            let cursor_y = inner.y;
            frame.set_cursor_position((cursor_x, cursor_y));
        }
    }

    fn draw_status(&self, frame: &mut ratatui::Frame, area: Rect) {
        let elapsed_str = self
            .turn_start
            .map(|t| format!("{}s", t.elapsed().as_secs()))
            .unwrap_or_default();

        let total_tokens = self.usage.input_tokens + self.usage.output_tokens;
        let tokens_str = format_tokens(total_tokens);

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
            Span::raw("  "),
            Span::styled("tokens ", Style::default().fg(MUTED)),
            Span::styled(tokens_str, Style::default().fg(TEXT_SEC)),
            Span::raw("  "),
        ];

        if self.state.is_generating && !elapsed_str.is_empty() {
            spans.push(Span::styled(elapsed_str, Style::default().fg(ACCENT)));
            spans.push(Span::raw("  "));
        }

        spans.push(Span::styled("? help", Style::default().fg(MUTED)));

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

        let lines = vec![
            Line::from(vec![
                Span::styled("  tool: ", Style::default().fg(MUTED)),
                Span::styled(
                    &perm.request.tool_name,
                    Style::default().fg(ACCENT),
                ),
            ]),
            Line::from(vec![
                Span::styled("  action: ", Style::default().fg(MUTED)),
                Span::styled(
                    &perm.action_description,
                    Style::default().fg(TEXT),
                ),
            ]),
            Line::from(""),
            Line::from(Span::styled(
                "  y/a = allow  n/d = deny  v = view input  Esc = deny",
                Style::default().fg(TEXT_SEC),
            )),
        ];

        let popup_w = 60.min(area.width.saturating_sub(4));
        let popup_h = 6;
        let popup_x = (area.width.saturating_sub(popup_w)) / 2;
        let popup_y = (area.height.saturating_sub(popup_h)) / 2;
        let popup_area = Rect::new(popup_x, popup_y, popup_w, popup_h);

        frame.render_widget(Clear, popup_area);

        let block = Block::default()
            .title(" permission ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(ACCENT))
            .style(Style::default().bg(SURFACE));

        let paragraph = Paragraph::new(lines).block(block);
        frame.render_widget(paragraph, popup_area);
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
            // Flush current
            if !current.is_empty() {
                spans.push(Span::styled(current.clone(), Style::default().fg(TEXT)));
                current.clear();
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
                spans.push(Span::styled(current.clone(), Style::default().fg(TEXT)));
                current.clear();
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
                spans.push(Span::styled(current.clone(), Style::default().fg(TEXT)));
                current.clear();
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
        assert!(app.scrollback.len() >= 1);
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
        assert!(app.scrollback.len() >= 1);
    }
}
