//! Event bridge for the full-screen ratatui TUI mode.
//!
//! Provides a channel-based communication system between the blocking
//! `ConversationRuntime` (which runs on a background thread) and the
//! ratatui event loop (which runs on the main thread).  All streaming
//! output, tool execution, permission requests, and status updates are
//! normalised into [`TuiEvent`] values and sent across this bridge so
//! the UI can update incrementally without blocking.

use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

use ninmu_runtime::{
    ContentBlock, ConversationMessage, MessageRole, PermissionPromptDecision, PermissionRequest,
    PromptCacheEvent, TokenUsage,
};

/// Events that can be emitted during a TUI turn and consumed by the
/// ratatui render loop on the main thread.
#[derive(Debug)]
pub enum TuiEvent {
    /// A chunk of assistant text arrived from the model stream.
    TextDelta(String),

    /// The model requested a tool invocation.
    ToolUse { name: String, input: String },

    /// A tool finished executing (success or error).
    ToolResult {
        name: String,
        output: String,
        is_error: bool,
    },

    /// Token-usage update from the provider stream.
    Usage(TokenUsage),

    /// The model entered an extended thinking / reasoning block.
    ThinkingStart,

    /// The model finished its thinking block (elapsed time + optional char count).
    ThinkingStop {
        elapsed: Duration,
        chars: Option<usize>,
    },

    /// An error occurred while running the turn.
    Error(String),

    /// The turn completed successfully.
    TurnComplete,

    /// A permission prompt was raised. The runtime thread blocks until a
    /// decision is sent back through `response_tx`.
    PermissionPrompt {
        request: PermissionRequest,
        response_tx: Sender<PermissionPromptDecision>,
    },

    /// A heartbeat / progress tick from long-running tool execution.
    ToolProgress { name: String, elapsed: Duration },

    /// Clear the scrollback and load conversation history (used by /resume).
    LoadHistory { messages: Vec<ConversationMessage> },

    /// Reasoning effort or thinking mode was changed (used by /effort).
    ReasoningUpdate {
        effort: Option<String>,
        thinking: Option<bool>,
    },

    /// Model was changed (used by /model).
    ModelUpdate { model: String },

    /// Prompt-cache break event — surfaced when cache tokens drop unexpectedly.
    PromptCache(PromptCacheEvent),
}

/// Bridge that lets the streaming / tool layer push events to the TUI.
///
/// This is intentionally simple (a standard library [`mpsc::channel`]) so
/// the runtime code does not depend on any async runtime or heavyweight
/// crossbeam / tokio channels.
#[derive(Debug, Clone)]
pub struct TuiEventBridge {
    tx: Sender<TuiEvent>,
}

impl TuiEventBridge {
    /// Create a new bridge together with its receiving end.
    pub fn new() -> (Self, Receiver<TuiEvent>) {
        let (tx, rx) = mpsc::channel();
        (Self { tx }, rx)
    }

    /// Push an event.  Never blocks – if the receiving end has been
    /// dropped the event is silently discarded.
    pub fn push(&self, event: TuiEvent) {
        let _ = self.tx.send(event);
    }

    /// Push a text delta event.
    pub fn text(&self, text: impl Into<String>) {
        self.push(TuiEvent::TextDelta(text.into()));
    }

    /// Push a tool-use event.
    pub fn tool_use(&self, name: impl Into<String>, input: impl Into<String>) {
        self.push(TuiEvent::ToolUse {
            name: name.into(),
            input: input.into(),
        });
    }

    /// Push a tool-result event.
    pub fn tool_result(&self, name: impl Into<String>, output: impl Into<String>, is_error: bool) {
        self.push(TuiEvent::ToolResult {
            name: name.into(),
            output: output.into(),
            is_error,
        });
    }

    /// Push a usage event.
    pub fn usage(&self, usage: TokenUsage) {
        self.push(TuiEvent::Usage(usage));
    }

    /// Push a thinking-start event.
    pub fn thinking_start(&self) {
        self.push(TuiEvent::ThinkingStart);
    }

    /// Push a thinking-stop event.
    pub fn thinking_stop(&self, elapsed: Duration, chars: Option<usize>) {
        self.push(TuiEvent::ThinkingStop { elapsed, chars });
    }

    /// Push an error event.
    pub fn error(&self, message: impl Into<String>) {
        self.push(TuiEvent::Error(message.into()));
    }

    /// Push a turn-complete event.
    pub fn turn_complete(&self) {
        self.push(TuiEvent::TurnComplete);
    }

    /// Push a permission-prompt event and return the receiver that will
    /// yield the user's decision.  The runtime thread blocks on this
    /// receiver until the TUI sends the response.
    pub fn permission_prompt(
        &self,
        request: PermissionRequest,
    ) -> Receiver<PermissionPromptDecision> {
        let (response_tx, response_rx) = mpsc::channel();
        self.push(TuiEvent::PermissionPrompt {
            request,
            response_tx,
        });
        response_rx
    }

    /// Push a tool-progress heartbeat.
    pub fn tool_progress(&self, name: impl Into<String>, elapsed: Duration) {
        self.push(TuiEvent::ToolProgress {
            name: name.into(),
            elapsed,
        });
    }

    /// Push a load-history event.  The TUI will clear its scrollback and
    /// display the provided messages.
    pub fn load_history(&self, messages: Vec<ConversationMessage>) {
        self.push(TuiEvent::LoadHistory { messages });
    }

    /// Push a reasoning-update event.  The TUI will update its header.
    pub fn reasoning_update(&self, effort: Option<String>, thinking: Option<bool>) {
        self.push(TuiEvent::ReasoningUpdate { effort, thinking });
    }

    /// Push a model-update event.  The TUI will update its header.
    pub fn model_update(&self, model: String) {
        self.push(TuiEvent::ModelUpdate { model });
    }

    /// Push a prompt-cache break event.
    pub fn prompt_cache(&self, event: PromptCacheEvent) {
        self.push(TuiEvent::PromptCache(event));
    }
}

/// Shared state that both the TUI (main thread) and the runtime thread
/// can read / write safely.
#[derive(Debug)]
pub struct TuiSharedState {
    /// Current permission prompt waiting for user input, if any.
    pub pending_permission: Option<PermissionRequest>,

    /// Whether a turn is currently in flight.
    pub is_generating: bool,

    /// When the current turn started (for elapsed-time display).
    pub turn_start: Option<Instant>,

    /// Current thinking / reasoning state.
    pub thinking_state: ThinkingState,

    /// Latest token usage snapshot.
    pub latest_usage: Option<TokenUsage>,

    /// Current tool being executed (for the tool-progress bar).
    pub current_tool: Option<String>,

    /// The user input that initiated the current turn.
    pub current_prompt: String,
}

/// Whether the model is currently in a thinking / reasoning block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ThinkingState {
    Idle,
    Thinking { started: Instant },
}

impl Default for TuiSharedState {
    fn default() -> Self {
        Self {
            pending_permission: None,
            is_generating: false,
            turn_start: None,
            thinking_state: ThinkingState::Idle,
            latest_usage: None,
            current_tool: None,
            current_prompt: String::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bridge_send_and_receive() {
        let (bridge, rx) = TuiEventBridge::new();
        bridge.text("hello");
        bridge.usage(TokenUsage {
            input_tokens: 10,
            output_tokens: 5,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
        });

        let e1 = rx.recv().unwrap();
        let e2 = rx.recv().unwrap();

        assert!(matches!(&e1, TuiEvent::TextDelta(s) if s == "hello"));
        assert!(matches!(&e2, TuiEvent::Usage(u) if u.input_tokens == 10 && u.output_tokens == 5));
    }

    #[test]
    fn shared_state_default_is_idle() {
        let state = TuiSharedState::default();
        assert!(!state.is_generating);
        assert_eq!(state.thinking_state, ThinkingState::Idle);
        assert!(state.pending_permission.is_none());
    }

    #[test]
    fn bridge_reasoning_update() {
        let (bridge, rx) = TuiEventBridge::new();
        bridge.reasoning_update(Some("high".to_string()), Some(true));
        let ev = rx.recv().unwrap();
        match ev {
            TuiEvent::ReasoningUpdate { effort, thinking } => {
                assert_eq!(effort.as_deref(), Some("high"));
                assert_eq!(thinking, Some(true));
            }
            other => panic!("expected ReasoningUpdate, got {other:?}"),
        }
    }

    #[test]
    fn bridge_reasoning_update_with_none() {
        let (bridge, rx) = TuiEventBridge::new();
        bridge.reasoning_update(None, None);
        let ev = rx.recv().unwrap();
        match ev {
            TuiEvent::ReasoningUpdate { effort, thinking } => {
                assert!(effort.is_none());
                assert!(thinking.is_none());
            }
            other => panic!("expected ReasoningUpdate, got {other:?}"),
        }
    }

    #[test]
    fn bridge_model_update() {
        let (bridge, rx) = TuiEventBridge::new();
        bridge.model_update("deepseek-reasoner".to_string());
        let ev = rx.recv().unwrap();
        match ev {
            TuiEvent::ModelUpdate { model } => {
                assert_eq!(model, "deepseek-reasoner");
            }
            other => panic!("expected ModelUpdate, got {other:?}"),
        }
    }

    #[test]
    fn bridge_load_history() {
        let (bridge, rx) = TuiEventBridge::new();
        bridge.load_history(vec![ConversationMessage {
            role: MessageRole::User,
            blocks: vec![ContentBlock::Text {
                text: "test".into(),
            }],
            usage: None,
        }]);
        let ev = rx.recv().unwrap();
        assert!(matches!(&ev, TuiEvent::LoadHistory { messages } if messages.len() == 1));
    }
}
