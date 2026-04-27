//! Provider HTTP integration tests for Mistral, Gemini, Cohere, vLLM, and DashScope.
//!
//! These tests verify that each provider correctly:
//! - Resolves configuration from environment variables
//! - Makes HTTP requests to the correct endpoints
//! - Handles authentication (required vs optional)
//! - Strips routing prefixes from model names
//! - Enforces provider-specific request size limits
//! - Handles provider-specific request formatting (e.g., kimi is_error rejection)

use std::collections::HashMap;
use std::ffi::OsString;
use std::sync::Arc;
use std::sync::{Mutex as StdMutex, OnceLock};

use ninmu_api::{
    ApiError, InputContentBlock, InputMessage, MessageRequest, OpenAiCompatClient,
    OpenAiCompatConfig, OutputContentBlock, ProviderClient, ProviderKind, StreamEvent, ToolChoice,
    ToolDefinition,
};
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::Mutex;

#[derive(Debug, Clone, PartialEq, Eq)]
struct CapturedRequest {
    path: String,
    headers: HashMap<String, String>,
    body: String,
}

struct TestServer {
    base_url: String,
    join_handle: tokio::task::JoinHandle<()>,
}

impl TestServer {
    fn base_url(&self) -> String {
        self.base_url.clone()
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.join_handle.abort();
    }
}

async fn spawn_server(
    state: Arc<Mutex<Vec<CapturedRequest>>>,
    responses: Vec<String>,
) -> TestServer {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("listener should bind");
    let address = listener.local_addr().expect("listener addr");
    let join_handle = tokio::spawn(async move {
        for response in responses {
            let (mut socket, _) = listener.accept().await.expect("accept");
            let mut buffer = Vec::new();
            let mut header_end = None;
            loop {
                let mut chunk = [0_u8; 1024];
                let read = socket.read(&mut chunk).await.expect("read request");
                if read == 0 {
                    break;
                }
                buffer.extend_from_slice(&chunk[..read]);
                if let Some(position) = find_header_end(&buffer) {
                    header_end = Some(position);
                    break;
                }
            }

            let header_end = header_end.expect("headers should exist");
            let (header_bytes, remaining) = buffer.split_at(header_end);
            let header_text = String::from_utf8(header_bytes.to_vec()).expect("utf8 headers");
            let mut lines = header_text.split("\r\n");
            let request_line = lines.next().expect("request line");
            let path = request_line
                .split_whitespace()
                .nth(1)
                .expect("path")
                .to_string();
            let mut headers = HashMap::new();
            let mut content_length = 0_usize;
            for line in lines {
                if line.is_empty() {
                    continue;
                }
                let (name, value) = line.split_once(':').expect("header");
                let value = value.trim().to_string();
                if name.eq_ignore_ascii_case("content-length") {
                    content_length = value.parse().expect("content length");
                }
                headers.insert(name.to_ascii_lowercase(), value);
            }

            let mut body = remaining[4..].to_vec();
            while body.len() < content_length {
                let mut chunk = vec![0_u8; content_length - body.len()];
                let read = socket.read(&mut chunk).await.expect("read body");
                if read == 0 {
                    break;
                }
                body.extend_from_slice(&chunk[..read]);
            }

            state.lock().await.push(CapturedRequest {
                path,
                headers,
                body: String::from_utf8(body).expect("utf8 body"),
            });

            socket
                .write_all(response.as_bytes())
                .await
                .expect("write response");
        }
    });

    TestServer {
        base_url: format!("http://{address}"),
        join_handle,
    }
}

fn find_header_end(bytes: &[u8]) -> Option<usize> {
    bytes.windows(4).position(|window| window == b"\r\n\r\n")
}

fn http_response(status: &str, content_type: &str, body: &str) -> String {
    http_response_with_headers(status, content_type, body, &[])
}

fn http_response_with_headers(
    status: &str,
    content_type: &str,
    body: &str,
    headers: &[(&str, &str)],
) -> String {
    let mut extra_headers = String::new();
    for (name, value) in headers {
        use std::fmt::Write as _;
        write!(&mut extra_headers, "{name}: {value}\r\n").expect("header write");
    }
    format!(
        "HTTP/1.1 {status}\r\ncontent-type: {content_type}\r\n{extra_headers}content-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    )
}

fn sample_request(stream: bool) -> MessageRequest {
    MessageRequest {
        model: "gpt-4".to_string(),
        max_tokens: 64,
        messages: vec![InputMessage {
            role: "user".to_string(),
            content: vec![InputContentBlock::Text {
                text: "Say hello".to_string(),
            }],
        }],
        system: Some("Use tools when needed".to_string()),
        tools: Some(vec![ToolDefinition {
            name: "weather".to_string(),
            description: Some("Fetches weather".to_string()),
            input_schema: json!({
                "type": "object",
                "properties": {"city": {"type": "string"}},
                "required": ["city"]
            }),
        }]),
        tool_choice: Some(ToolChoice::Auto),
        stream,
        ..Default::default()
    }
}

fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: OnceLock<StdMutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| StdMutex::new(()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

struct ScopedEnvVar {
    key: &'static str,
    previous: Option<OsString>,
}

impl ScopedEnvVar {
    fn set(key: &'static str, value: Option<&str>) -> Self {
        let previous = std::env::var_os(key);
        match value {
            Some(value) => std::env::set_var(key, value),
            None => std::env::remove_var(key),
        }
        Self { key, previous }
    }
}

impl Drop for ScopedEnvVar {
    fn drop(&mut self) {
        match &self.previous {
            Some(value) => std::env::set_var(self.key, value),
            None => std::env::remove_var(self.key),
        }
    }
}

// ============================================================================
// Mistral Provider Tests
// ============================================================================

#[tokio::test]
async fn mistral_send_message_uses_correct_endpoint_and_auth() {
    let state = Arc::new(Mutex::new(Vec::<CapturedRequest>::new()));
    let body = concat!(
        "{",
        "\"id\":\"chatcmpl_mistral\",",
        "\"model\":\"mistral-large-latest\",",
        "\"choices\":[{",
        "\"message\":{\"role\":\"assistant\",\"content\":\"Hello from Mistral\",\"tool_calls\":[]},",
        "\"finish_reason\":\"stop\"",
        "}],",
        "\"usage\":{\"prompt_tokens\":8,\"completion_tokens\":4}",
        "}"
    );
    let server = spawn_server(
        state.clone(),
        vec![http_response("200 OK", "application/json", body)],
    )
    .await;

    let client = OpenAiCompatClient::new("mistral-test-key", OpenAiCompatConfig::mistral())
        .with_base_url(server.base_url());

    let mut request = sample_request(false);
    request.model = "mistral-large-latest".to_string();

    let response = client
        .send_message(&request)
        .await
        .expect("request should succeed");

    assert_eq!(
        response.content[0],
        OutputContentBlock::Text {
            text: "Hello from Mistral".to_string(),
        }
    );

    let captured = state.lock().await;
    let request = captured.first().expect("server should capture request");
    assert_eq!(request.path, "/chat/completions");
    assert_eq!(
        request.headers.get("authorization").map(String::as_str),
        Some("Bearer mistral-test-key")
    );
    let body: serde_json::Value = serde_json::from_str(&request.body).expect("json body");
    assert_eq!(body["model"], json!("mistral-large-latest"));
}

#[tokio::test]
async fn mistral_stream_message_emits_response() {
    let state = Arc::new(Mutex::new(Vec::<CapturedRequest>::new()));
    let sse = concat!(
        "data: {\"id\":\"chatcmpl_mistral_stream\",\"choices\":[{\"delta\":{\"content\":\"Bonjour\"}}]}\n\n",
        "data: {\"id\":\"chatcmpl_mistral_stream\",\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        "data: [DONE]\n\n"
    );
    let server = spawn_server(
        state.clone(),
        vec![http_response("200 OK", "text/event-stream", sse)],
    )
    .await;

    let client = OpenAiCompatClient::new("mistral-test-key", OpenAiCompatConfig::mistral())
        .with_base_url(server.base_url());

    let mut request = sample_request(false);
    request.model = "mistral-small-latest".to_string();

    let mut stream = client
        .stream_message(&request)
        .await
        .expect("stream should start");

    let mut events = Vec::new();
    while let Some(event) = stream.next_event().await.expect("event should parse") {
        events.push(event);
    }

    assert!(matches!(events[0], StreamEvent::MessageStart(_)));
    assert!(events.len() > 0);

    let captured = state.lock().await;
    let request = captured.first().expect("captured request");
    let body: serde_json::Value = serde_json::from_str(&request.body).expect("json body");
    assert_eq!(body["model"], json!("mistral-small-latest"));
}

// ============================================================================
// Gemini Provider Tests
// ============================================================================

#[tokio::test]
async fn gemini_send_message_uses_correct_endpoint_and_auth() {
    let state = Arc::new(Mutex::new(Vec::<CapturedRequest>::new()));
    let body = concat!(
        "{",
        "\"id\":\"chatcmpl_gemini\",",
        "\"model\":\"gemini-2.5-pro\",",
        "\"choices\":[{",
        "\"message\":{\"role\":\"assistant\",\"content\":\"Hello from Gemini\",\"tool_calls\":[]},",
        "\"finish_reason\":\"stop\"",
        "}],",
        "\"usage\":{\"prompt_tokens\":8,\"completion_tokens\":4}",
        "}"
    );
    let server = spawn_server(
        state.clone(),
        vec![http_response("200 OK", "application/json", body)],
    )
    .await;

    let client = OpenAiCompatClient::new("gemini-test-key", OpenAiCompatConfig::gemini())
        .with_base_url(server.base_url());

    let mut request = sample_request(false);
    request.model = "gemini-2.5-pro".to_string();

    let response = client
        .send_message(&request)
        .await
        .expect("request should succeed");

    assert_eq!(
        response.content[0],
        OutputContentBlock::Text {
            text: "Hello from Gemini".to_string(),
        }
    );

    let captured = state.lock().await;
    let request = captured.first().expect("server should capture request");
    assert_eq!(
        request.headers.get("authorization").map(String::as_str),
        Some("Bearer gemini-test-key")
    );
}

#[tokio::test]
async fn gemini_send_message_respects_context_window_limit() {
    // Note: Gemini has a 10MB HTTP body size limit, but also a 1M token (~4MB) context window.
    // The context window check happens first in preflight, so large requests fail there.
    let state = Arc::new(Mutex::new(Vec::<CapturedRequest>::new()));
    let server = spawn_server(
        state.clone(),
        vec![http_response("200 OK", "application/json", "{}")],
    )
    .await;

    let client = OpenAiCompatClient::new("gemini-test-key", OpenAiCompatConfig::gemini())
        .with_base_url(server.base_url());

    // Create a request that exceeds Gemini's 1M token context window (~4MB of text)
    let large_request = MessageRequest {
        model: "gemini-2.5-pro".to_string(),
        max_tokens: 64,
        messages: vec![InputMessage {
            role: "user".to_string(),
            content: vec![InputContentBlock::Text {
                text: "x".repeat(5_000_000), // ~5MB of text > 1M tokens
            }],
        }],
        stream: false,
        ..Default::default()
    };

    let result = client.send_message(&large_request).await;
    assert!(
        matches!(result, Err(ApiError::ContextWindowExceeded { .. })),
        "Should fail on Gemini context window limit, got {:?}",
        result
    );
}

// ============================================================================
// Cohere Provider Tests
// ============================================================================

#[tokio::test]
async fn cohere_send_message_uses_correct_endpoint_and_auth() {
    let state = Arc::new(Mutex::new(Vec::<CapturedRequest>::new()));
    let body = concat!(
        "{",
        "\"id\":\"chatcmpl_cohere\",",
        "\"model\":\"command-r-plus\",",
        "\"choices\":[{",
        "\"message\":{\"role\":\"assistant\",\"content\":\"Hello from Cohere\",\"tool_calls\":[]},",
        "\"finish_reason\":\"stop\"",
        "}],",
        "\"usage\":{\"prompt_tokens\":8,\"completion_tokens\":4}",
        "}"
    );
    let server = spawn_server(
        state.clone(),
        vec![http_response("200 OK", "application/json", body)],
    )
    .await;

    let client = OpenAiCompatClient::new("cohere-test-key", OpenAiCompatConfig::cohere())
        .with_base_url(server.base_url());

    let mut request = sample_request(false);
    request.model = "command-r-plus".to_string();

    let response = client
        .send_message(&request)
        .await
        .expect("request should succeed");

    assert_eq!(
        response.content[0],
        OutputContentBlock::Text {
            text: "Hello from Cohere".to_string(),
        }
    );

    let captured = state.lock().await;
    let request = captured.first().expect("server should capture request");
    assert_eq!(
        request.headers.get("authorization").map(String::as_str),
        Some("Bearer cohere-test-key")
    );
}

// ============================================================================
// vLLM Provider Tests
// ============================================================================

#[tokio::test]
async fn vllm_send_message_works_without_auth() {
    let state = Arc::new(Mutex::new(Vec::<CapturedRequest>::new()));
    let body = concat!(
        "{",
        "\"id\":\"chatcmpl_vllm\",",
        "\"model\":\"meta-llama/Llama-3.1-8B\",",
        "\"choices\":[{",
        "\"message\":{\"role\":\"assistant\",\"content\":\"Hello from vLLM\",\"tool_calls\":[]},",
        "\"finish_reason\":\"stop\"",
        "}],",
        "\"usage\":{\"prompt_tokens\":8,\"completion_tokens\":4}",
        "}"
    );
    let server = spawn_server(
        state.clone(),
        vec![http_response("200 OK", "application/json", body)],
    )
    .await;

    // vLLM has no API key env var and auth_optional = true
    let client =
        OpenAiCompatClient::new("", OpenAiCompatConfig::vllm()).with_base_url(server.base_url());

    let mut request = sample_request(false);
    request.model = "meta-llama/Llama-3.1-8B".to_string();

    let response = client
        .send_message(&request)
        .await
        .expect("request should succeed without auth");

    assert_eq!(
        response.content[0],
        OutputContentBlock::Text {
            text: "Hello from vLLM".to_string(),
        }
    );
}

#[tokio::test]
async fn vllm_send_message_strips_prefix_from_model_name() {
    let state = Arc::new(Mutex::new(Vec::<CapturedRequest>::new()));
    let body = concat!(
        "{",
        "\"id\":\"chatcmpl_vllm_prefix\",",
        "\"model\":\"meta-llama/Llama-3.1-8B\",",
        "\"choices\":[{",
        "\"message\":{\"role\":\"assistant\",\"content\":\"Hello\",\"tool_calls\":[]},",
        "\"finish_reason\":\"stop\"",
        "}],",
        "\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":2}",
        "}"
    );
    let server = spawn_server(
        state.clone(),
        vec![http_response("200 OK", "application/json", body)],
    )
    .await;

    let client =
        OpenAiCompatClient::new("", OpenAiCompatConfig::vllm()).with_base_url(server.base_url());

    let mut request = sample_request(false);
    request.model = "vllm/meta-llama/Llama-3.1-8B".to_string();

    client
        .send_message(&request)
        .await
        .expect("request should succeed");

    let captured = state.lock().await;
    let request = captured.first().expect("server should capture request");
    let body: serde_json::Value = serde_json::from_str(&request.body).expect("json body");
    // The vllm/ prefix should be stripped
    assert_eq!(body["model"], json!("meta-llama/Llama-3.1-8B"));
}

#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn vllm_provider_client_dispatches_from_prefix() {
    let _lock = env_lock();
    let _vllm_url = ScopedEnvVar::set("VLLM_BASE_URL", None);
    let _anthropic = ScopedEnvVar::set("ANTHROPIC_API_KEY", None);
    let _openai = ScopedEnvVar::set("OPENAI_API_KEY", None);

    let state = Arc::new(Mutex::new(Vec::<CapturedRequest>::new()));
    let server = spawn_server(
        state.clone(),
        vec![http_response(
            "200 OK",
            "application/json",
            "{\"id\":\"test\",\"model\":\"llama\",\"choices\":[{\"message\":{\"role\":\"assistant\",\"content\":\"hi\",\"tool_calls\":[]},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1}}",
        )],
    )
    .await;

    std::env::set_var("VLLM_BASE_URL", server.base_url());

    let client = ProviderClient::from_model("vllm/meta-llama/Llama-3.1-8B")
        .expect("should construct without auth");
    assert_eq!(client.provider_kind(), ProviderKind::Vllm);
}

// ============================================================================
// DashScope Provider Tests
// ============================================================================

#[tokio::test]
async fn dashscope_send_message_uses_correct_endpoint_and_auth() {
    let state = Arc::new(Mutex::new(Vec::<CapturedRequest>::new()));
    let body = concat!(
        "{",
        "\"id\":\"chatcmpl_dashscope\",",
        "\"model\":\"qwen-plus\",",
        "\"choices\":[{",
        "\"message\":{\"role\":\"assistant\",\"content\":\"Hello from DashScope\",\"tool_calls\":[]},",
        "\"finish_reason\":\"stop\"",
        "}],",
        "\"usage\":{\"prompt_tokens\":8,\"completion_tokens\":4}",
        "}"
    );
    let server = spawn_server(
        state.clone(),
        vec![http_response("200 OK", "application/json", body)],
    )
    .await;

    let client = OpenAiCompatClient::new("dashscope-test-key", OpenAiCompatConfig::dashscope())
        .with_base_url(server.base_url());

    let mut request = sample_request(false);
    request.model = "qwen-plus".to_string();

    let response = client
        .send_message(&request)
        .await
        .expect("request should succeed");

    assert_eq!(
        response.content[0],
        OutputContentBlock::Text {
            text: "Hello from DashScope".to_string(),
        }
    );

    let captured = state.lock().await;
    let request = captured.first().expect("server should capture request");
    assert_eq!(
        request.headers.get("authorization").map(String::as_str),
        Some("Bearer dashscope-test-key")
    );
}

#[tokio::test]
async fn dashscope_send_message_respects_6mb_size_limit() {
    let state = Arc::new(Mutex::new(Vec::<CapturedRequest>::new()));
    let server = spawn_server(
        state.clone(),
        vec![http_response("200 OK", "application/json", "{}")],
    )
    .await;

    let client = OpenAiCompatClient::new("dashscope-test-key", OpenAiCompatConfig::dashscope())
        .with_base_url(server.base_url());

    // Create a request that's over 6MB
    let large_request = MessageRequest {
        model: "qwen-plus".to_string(),
        max_tokens: 64,
        messages: vec![InputMessage {
            role: "user".to_string(),
            content: vec![InputContentBlock::Text {
                text: "x".repeat(7_000_000), // ~7MB of text
            }],
        }],
        stream: false,
        ..Default::default()
    };

    let result = client.send_message(&large_request).await;
    assert!(
        matches!(result, Err(ApiError::RequestBodySizeExceeded { .. })),
        "Should fail on DashScope 6MB size limit, got {:?}",
        result
    );
}

#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn dashscope_provider_client_routes_qwen_models() {
    let _lock = env_lock();
    let _dashscope_key = ScopedEnvVar::set("DASHSCOPE_API_KEY", Some("dashscope-test-key"));
    let _openai_key = ScopedEnvVar::set("OPENAI_API_KEY", None);

    let state = Arc::new(Mutex::new(Vec::<CapturedRequest>::new()));
    let server = spawn_server(
        state.clone(),
        vec![http_response(
            "200 OK",
            "application/json",
            "{\"id\":\"test\",\"model\":\"qwen-plus\",\"choices\":[{\"message\":{\"role\":\"assistant\",\"content\":\"hi\",\"tool_calls\":[]},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1}}",
        )],
    )
    .await;

    std::env::set_var("DASHSCOPE_BASE_URL", server.base_url());

    let client = ProviderClient::from_model("qwen-plus").expect("should construct");
    assert_eq!(client.provider_kind(), ProviderKind::OpenAi);

    let mut request = sample_request(false);
    request.model = "qwen-plus".to_string();

    client
        .send_message(&request)
        .await
        .expect("request should succeed");

    let captured = state.lock().await;
    let request = captured.first().expect("server should capture request");
    assert_eq!(
        request.headers.get("authorization").map(String::as_str),
        Some("Bearer dashscope-test-key")
    );
}

#[tokio::test]
async fn dashscope_send_message_excludes_is_error_for_kimi_models() {
    let state = Arc::new(Mutex::new(Vec::<CapturedRequest>::new()));
    let body = concat!(
        "{",
        "\"id\":\"chatcmpl_kimi\",",
        "\"model\":\"kimi-k2.5\",",
        "\"choices\":[{",
        "\"message\":{\"role\":\"assistant\",\"content\":\"Hello from Kimi\",\"tool_calls\":[]},",
        "\"finish_reason\":\"stop\"",
        "}],",
        "\"usage\":{\"prompt_tokens\":8,\"completion_tokens\":4}",
        "}"
    );
    let server = spawn_server(
        state.clone(),
        vec![http_response("200 OK", "application/json", body)],
    )
    .await;

    let client = OpenAiCompatClient::new("dashscope-test-key", OpenAiCompatConfig::dashscope())
        .with_base_url(server.base_url());

    let mut request = sample_request(false);
    request.model = "kimi-k2.5".to_string();
    // Add a tool result to check is_error field handling
    request.messages.push(InputMessage {
        role: "user".to_string(),
        content: vec![InputContentBlock::ToolResult {
            tool_use_id: "tool_123".to_string(),
            content: vec![api::ToolResultContentBlock::Text {
                text: "Error occurred".to_string(),
            }],
            is_error: true,
        }],
    });

    client
        .send_message(&request)
        .await
        .expect("request should succeed");

    let captured = state.lock().await;
    let request = captured.first().expect("server should capture request");
    let body: serde_json::Value = serde_json::from_str(&request.body).expect("json body");

    // Find the tool result message in the messages array
    let messages = body["messages"]
        .as_array()
        .expect("messages should be array");
    let tool_message = messages
        .iter()
        .find(|m| m["role"] == "tool")
        .expect("should have tool message");

    // Kimi models should NOT have is_error field
    assert!(
        tool_message.get("is_error").is_none(),
        "Kimi models should not have is_error field, got {:?}",
        tool_message.get("is_error")
    );
}
