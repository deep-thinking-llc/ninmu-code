//! Minimal API client for the JSON-RPC server.
//!
//! `RpcApiClient` implements `ninmu_runtime::ApiClient` by delegating to
//! `ninmu_api::ProviderClient`.  It is intentionally stripped-down:
//! no markdown rendering, no TUI bridging, no progress reporting — just
//! raw API calls and event conversion.

use ninmu_api::{
    MessageRequest, MessageResponse, OutputContentBlock, ProviderClient, ToolResultContentBlock,
    Usage,
};
use ninmu_runtime::{
    ApiClient, ApiRequest, AssistantEvent, ContentBlock, ConversationMessage, MessageRole,
    RuntimeError, TokenUsage,
};

/// A thin synchronous wrapper around `ProviderClient` for use in the
/// JSON-RPC server.
pub struct RpcApiClient {
    runtime: tokio::runtime::Runtime,
    client: ProviderClient,
    model: String,
}

impl RpcApiClient {
    /// Build a provider client for `model`.
    pub fn new(model: &str) -> Result<Self, Box<dyn std::error::Error>> {
        let runtime = tokio::runtime::Runtime::new()?;
        let resolved_model = ninmu_api::resolve_model_alias(model);
        let client = runtime.block_on(async {
            match ninmu_api::detect_provider_kind(&resolved_model) {
                ninmu_api::ProviderKind::Anthropic => {
                    let auth = ninmu_api::resolve_startup_auth_source(|| Ok(None))?;
                    let inner = ninmu_api::AnthropicClient::from_auth(auth)
                        .with_base_url(ninmu_api::read_base_url());
                    Ok::<_, Box<dyn std::error::Error>>(ProviderClient::Anthropic(inner))
                }
                _ => Ok(ProviderClient::from_model_with_anthropic_auth(
                    &resolved_model,
                    None,
                )?),
            }
        })?;
        Ok(Self {
            runtime,
            client,
            model: resolved_model,
        })
    }
}

impl ApiClient for RpcApiClient {
    fn stream(&mut self, request: ApiRequest) -> Result<Vec<AssistantEvent>, RuntimeError> {
        let message_request = MessageRequest {
            model: self.model.clone(),
            max_tokens: 4096,
            messages: request
                .messages
                .into_iter()
                .map(conversation_message_to_input_message)
                .collect::<Result<Vec<_>, _>>()?,
            system: if request.system_prompt.is_empty() {
                None
            } else {
                Some(request.system_prompt.join("\n\n"))
            },
            tools: None,
            tool_choice: None,
            stream: false,
            ..Default::default()
        };

        let response = self
            .runtime
            .block_on(self.client.send_message(&message_request))
            .map_err(|e| RuntimeError::new(e.to_string()))?;

        let mut events = response_to_events(response);
        push_prompt_cache_record(&self.client, &mut events);
        Ok(events)
    }
}

fn conversation_message_to_input_message(
    msg: ConversationMessage,
) -> Result<ninmu_api::InputMessage, RuntimeError> {
    let role = match msg.role {
        MessageRole::System => "system",
        MessageRole::User => "user",
        MessageRole::Assistant => "assistant",
        MessageRole::Tool => "tool",
    }
    .to_string();

    let content = msg
        .blocks
        .into_iter()
        .map(|b| match b {
            ContentBlock::Text { text } => Ok(ninmu_api::InputContentBlock::Text { text }),
            ContentBlock::ToolUse { id, name, input } => {
                let value = serde_json::from_str(&input)
                    .map_err(|e| RuntimeError::new(format!("invalid tool input JSON: {e}")))?;
                Ok(ninmu_api::InputContentBlock::ToolUse {
                    id,
                    name,
                    input: value,
                })
            }
            ContentBlock::ToolResult {
                tool_use_id,
                output,
                is_error,
                ..
            } => Ok(ninmu_api::InputContentBlock::ToolResult {
                tool_use_id,
                content: vec![ToolResultContentBlock::Text { text: output }],
                is_error,
            }),
            ContentBlock::Thinking { thinking } => {
                Ok(ninmu_api::InputContentBlock::Thinking { thinking })
            }
        })
        .collect::<Result<Vec<_>, _>>()?;

    Ok(ninmu_api::InputMessage { role, content })
}

fn response_to_events(response: MessageResponse) -> Vec<AssistantEvent> {
    let mut events = Vec::new();
    let mut pending_tool: Option<(String, String, String)> = None;

    for block in response.content {
        match block {
            OutputContentBlock::Text { text } => {
                events.push(AssistantEvent::TextDelta(text));
            }
            OutputContentBlock::ToolUse { id, name, input } => {
                if let Some((tid, tname, tinput)) = pending_tool.take() {
                    events.push(AssistantEvent::ToolUse {
                        id: tid,
                        name: tname,
                        input: tinput,
                    });
                }
                let input_str = serde_json::to_string(&input).unwrap_or_default();
                pending_tool = Some((id, name, input_str));
            }
            OutputContentBlock::Thinking { thinking, .. } => {
                events.push(AssistantEvent::ThinkingDelta(thinking));
            }
            OutputContentBlock::RedactedThinking { .. } => {}
        }
    }

    if let Some((id, name, input)) = pending_tool.take() {
        events.push(AssistantEvent::ToolUse { id, name, input });
    }

    events.push(AssistantEvent::Usage(usage_to_token_usage(&response.usage)));
    events.push(AssistantEvent::MessageStop);
    events
}

fn usage_to_token_usage(usage: &Usage) -> TokenUsage {
    TokenUsage {
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
        cache_creation_input_tokens: usage.cache_creation_input_tokens,
        cache_read_input_tokens: usage.cache_read_input_tokens,
    }
}

fn push_prompt_cache_record(client: &ProviderClient, events: &mut Vec<AssistantEvent>) {
    if let Some(record) = client.take_last_prompt_cache_record() {
        if let Some(event) = prompt_cache_record_to_runtime_event(record) {
            events.push(AssistantEvent::PromptCache(event));
        }
    }
}

fn prompt_cache_record_to_runtime_event(
    record: ninmu_api::PromptCacheRecord,
) -> Option<ninmu_runtime::PromptCacheEvent> {
    let cache_break = record.cache_break?;
    Some(ninmu_runtime::PromptCacheEvent {
        unexpected: cache_break.unexpected,
        reason: cache_break.reason,
        previous_cache_read_input_tokens: cache_break.previous_cache_read_input_tokens,
        current_cache_read_input_tokens: cache_break.current_cache_read_input_tokens,
        token_drop: cache_break.token_drop,
    })
}
