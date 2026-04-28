//! Types for rich turn output in the TUI.

/// Token usage statistics from an LLM turn.
#[derive(Debug, Clone, Default)]
pub struct TurnUsage {
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub cache_creation_tokens: u32,
    pub cache_read_tokens: u32,
    pub cost_usd: f64,
    pub model: String,
}

/// Rich output from executing a turn in the TUI.
///
/// Contains the rendered text plus optional metadata like token usage.
pub struct TurnOutput {
    pub text: String,
    pub usage: Option<TurnUsage>,
}
