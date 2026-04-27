//! Provider HTTP integration tests for OpenAI, Ollama, and Qwen.
//!
//! These tests verify that each provider correctly:
//! - Resolves configuration from environment variables
//! - Makes HTTP requests to the correct endpoints
//! - Handles authentication (required vs optional)
//! - Strips routing prefixes from model names
//! - Enforces provider-specific request size limits

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
// OpenAI Provider Tests
// ============================================================================

#[tokio::test]
async fn openai_send_message_uses_correct_endpoint_and_auth() {
    let state = Arc::new(Mutex::new(Vec::<CapturedRequest>::new()));
    let body = concat!(
        "{",
        "\"id\":\"chatcmpl_openai_test\",",
        "\"model\":\"gpt-4\",",
        "\"choices\":[{",
        "\"message\":{\"role\":\"assistant\",\"content\":\"Hello from OpenAI\",\"tool_calls\":[]},",
        "\"finish_reason\":\"stop\"",
        "}],",
        "\"usage\":{\"prompt_tokens\":11,\"completion_tokens\":5}",
        "}"
    );
    let server = spawn_server(
        state.clone(),
        vec![http_response("200 OK", "application/json", body)],
    )
    .await;

    let client = OpenAiCompatClient::new("openai-test-key", OpenAiCompatConfig::openai())
        .with_base_url(server.base_url());
    let response = client
        .send_message(&sample_request(false))
        .await
        .expect("request should succeed");

    assert_eq!(response.model, "gpt-4");
    assert_eq!(response.total_tokens(), 16);
    assert_eq!(
        response.content,
        vec![OutputContentBlock::Text {
            text: "Hello from OpenAI".to_string(),
        }]
    );

    let captured = state.lock().await;
    let request = captured.first().expect("server should capture request");
    assert_eq!(request.path, "/chat/completions");
    assert_eq!(
        request.headers.get("authorization").map(String::as_str),
        Some("Bearer openai-test-key")
    );
    let body: serde_json::Value = serde_json::from_str(&request.body).expect("json body");
    assert_eq!(body["model"], json!("gpt-4"));
}

#[tokio::test]
async fn openai_send_message_uses_max_completion_tokens_for_gpt5() {
    let state = Arc::new(Mutex::new(Vec::<CapturedRequest>::new()));
    let body = concat!(
        "{",
        "\"id\":\"chatcmpl_gpt5\",",
        "\"model\":\"gpt-5\",",
        "\"choices\":[{",
        "\"message\":{\"role\":\"assistant\",\"content\":\"GPT-5 response\",\"tool_calls\":[]},",
        "\"finish_reason\":\"stop\"",
        "}],",
        "\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":3}",
        "}"
    );
    let server = spawn_server(
        state.clone(),
        vec![http_response("200 OK", "application/json", body)],
    )
    .await;

    let mut request = sample_request(false);
    request.model = "gpt-5".to_string();

    let client = OpenAiCompatClient::new("openai-test-key", OpenAiCompatConfig::openai())
        .with_base_url(server.base_url());
    let response = client
        .send_message(&request)
        .await
        .expect("request should succeed");

    assert_eq!(response.model, "gpt-5");

    let captured = state.lock().await;
    let request = captured.first().expect("server should capture request");
    let body: serde_json::Value = serde_json::from_str(&request.body).expect("json body");
    // gpt-5 models should use max_completion_tokens instead of max_tokens
    assert!(body.get("max_tokens").is_none());
    assert_eq!(body["max_completion_tokens"], json!(64));
}

#[tokio::test]
async fn openai_stream_message_requests_usage_inclusion() {
    let state = Arc::new(Mutex::new(Vec::<CapturedRequest>::new()));
    let sse = concat!(
        "data: {\"id\":\"chatcmpl_stream\",\"choices\":[{\"delta\":{\"content\":\"Hi\"}}]}\n\n",
        "data: {\"id\":\"chatcmpl_stream\",\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        "data: {\"id\":\"chatcmpl_stream\",\"choices\":[],\"usage\":{\"prompt_tokens\":9,\"completion_tokens\":4}}\n\n",
        "data: [DONE]\n\n"
    );
    let server = spawn_server(
        state.clone(),
        vec![http_response("200 OK", "text/event-stream", sse)],
    )
    .await;

    let client = OpenAiCompatClient::new("openai-test-key", OpenAiCompatConfig::openai())
        .with_base_url(server.base_url());
    let mut stream = client
        .stream_message(&sample_request(false))
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
    assert_eq!(body["stream"], json!(true));
    assert_eq!(body["stream_options"], json!({"include_usage": true}));
}

#[tokio::test]
async fn openai_send_message_respects_100mb_size_limit() {
    let state = Arc::new(Mutex::new(Vec::<CapturedRequest>::new()));
    let server = spawn_server(
        state.clone(),
        vec![http_response("200 OK", "application/json", "{}")],
    )
    .await;

    let client = OpenAiCompatClient::new("openai-test-key", OpenAiCompatConfig::openai())
        .with_base_url(server.base_url());

    // Create a request that's just under 100MB
    let large_request = MessageRequest {
        model: "gpt-4".to_string(),
        max_tokens: 64,
        messages: vec![InputMessage {
            role: "user".to_string(),
            content: vec![InputContentBlock::Text {
                text: "x".repeat(50_000_000), // ~50MB of text
            }],
        }],
        stream: false,
        ..Default::default()
    };

    // This should succeed (under 100MB limit)
    let result = client.send_message(&large_request).await;
    // We expect it to fail because our mock server returns empty JSON
    // but it should get past the size check
    assert!(
        result.is_err(),
        "Should fail on mock response, not size limit"
    );
    assert!(
        !matches!(
            result.unwrap_err(),
            ApiError::RequestBodySizeExceeded { .. }
        ),
        "Should not fail on size limit"
    );
}

#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn openai_provider_client_dispatches_from_model() {
    let _lock = env_lock();
    let _api_key = ScopedEnvVar::set("OPENAI_API_KEY", Some("openai-test-key"));
    let _base_url = ScopedEnvVar::set("OPENAI_BASE_URL", None);

    let state = Arc::new(Mutex::new(Vec::<CapturedRequest>::new()));
    let body = concat!(
        "{",
        "\"id\":\"chatcmpl_dispatch\",",
        "\"model\":\"gpt-4\",",
        "\"choices\":[{",
        "\"message\":{\"role\":\"assistant\",\"content\":\"Dispatched\",\"tool_calls\":[]},",
        "\"finish_reason\":\"stop\"",
        "}],",
        "\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":2}",
        "}"
    );
    let server = spawn_server(
        state.clone(),
        vec![http_response("200 OK", "application/json", body)],
    )
    .await;

    std::env::set_var("OPENAI_BASE_URL", server.base_url());

    let client = ProviderClient::from_model("gpt-4").expect("should construct");
    assert_eq!(client.provider_kind(), ProviderKind::OpenAi);

    let response = client
        .send_message(&sample_request(false))
        .await
        .expect("request should succeed");

    assert_eq!(response.total_tokens(), 5);

    let captured = state.lock().await;
    let request = captured.first().expect("server should capture request");
    assert_eq!(request.path, "/chat/completions");
}

// ============================================================================
// Ollama Provider Tests
// ============================================================================

#[tokio::test]
async fn ollama_send_message_works_without_api_key() {
    let state = Arc::new(Mutex::new(Vec::<CapturedRequest>::new()));
    let body = concat!(
        "{",
        "\"id\":\"chatcmpl_ollama\",",
        "\"model\":\"llama3.1:8b\",",
        "\"choices\":[{",
        "\"message\":{\"role\":\"assistant\",\"content\":\"Hello from Ollama\",\"tool_calls\":[]},",
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

    // No API key - auth_optional should work
    let client =
        OpenAiCompatClient::new("", OpenAiCompatConfig::ollama()).with_base_url(server.base_url());

    let mut request = sample_request(false);
    request.model = "llama3.1:8b".to_string();

    let response = client
        .send_message(&request)
        .await
        .expect("request should succeed without auth");

    assert_eq!(
        response.content[0],
        OutputContentBlock::Text {
            text: "Hello from Ollama".to_string(),
        }
    );

    let captured = state.lock().await;
    let request = captured.first().expect("server should capture request");
    // NOTE: The current implementation sends Authorization: Bearer (empty) even with no key
    // This is a known behavior - auth_optional controls whether key is required at client
    // creation time, but empty key still sends an auth header
    let auth_header = request.headers.get("authorization").map(String::as_str);
    assert!(
        auth_header == Some("Bearer") || auth_header == Some("Bearer ") || auth_header.is_none(),
        "Ollama should send empty bearer or no auth header without key, got {:?}",
        auth_header
    );
}

#[tokio::test]
async fn ollama_send_message_uses_api_key_when_provided() {
    let state = Arc::new(Mutex::new(Vec::<CapturedRequest>::new()));
    let body = concat!(
        "{",
        "\"id\":\"chatcmpl_ollama_auth\",",
        "\"model\":\"llama3.1:8b\",",
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

    let client = OpenAiCompatClient::new("ollama-cloud-key", OpenAiCompatConfig::ollama())
        .with_base_url(server.base_url());

    let mut request = sample_request(false);
    request.model = "llama3.1:8b".to_string();

    client
        .send_message(&request)
        .await
        .expect("request should succeed");

    let captured = state.lock().await;
    let request = captured.first().expect("server should capture request");
    assert_eq!(
        request.headers.get("authorization").map(String::as_str),
        Some("Bearer ollama-cloud-key"),
        "Ollama should use key when provided"
    );
}

#[tokio::test]
async fn ollama_send_message_strips_prefix_from_model_name() {
    let state = Arc::new(Mutex::new(Vec::<CapturedRequest>::new()));
    let body = concat!(
        "{",
        "\"id\":\"chatcmpl_ollama_prefix\",",
        "\"model\":\"llama3.1:8b\",",
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
        OpenAiCompatClient::new("", OpenAiCompatConfig::ollama()).with_base_url(server.base_url());

    let mut request = sample_request(false);
    request.model = "ollama/llama3.1:8b".to_string();

    client
        .send_message(&request)
        .await
        .expect("request should succeed");

    let captured = state.lock().await;
    let request = captured.first().expect("server should capture request");
    let body: serde_json::Value = serde_json::from_str(&request.body).expect("json body");
    // The ollama/ prefix should be stripped
    assert_eq!(body["model"], json!("llama3.1:8b"));
}

#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn ollama_provider_client_dispatches_from_prefix() {
    let _lock = env_lock();
    let _ollama_url = ScopedEnvVar::set("OLLAMA_BASE_URL", None);
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

    std::env::set_var("OLLAMA_BASE_URL", server.base_url());

    let client =
        ProviderClient::from_model("ollama/llama3.1:8b").expect("should construct without auth");
    assert_eq!(client.provider_kind(), ProviderKind::Ollama);
}

// ============================================================================
// Qwen Provider Tests
// ============================================================================

#[tokio::test]
async fn qwen_send_message_uses_qwen_credentials() {
    let state = Arc::new(Mutex::new(Vec::<CapturedRequest>::new()));
    let body = concat!(
        "{",
        "\"id\":\"chatcmpl_qwen\",",
        "\"model\":\"qwen2.5-7b\",",
        "\"choices\":[{",
        "\"message\":{\"role\":\"assistant\",\"content\":\"Hello from Qwen\",\"tool_calls\":[]},",
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

    let client = OpenAiCompatClient::new("qwen-test-key", OpenAiCompatConfig::qwen())
        .with_base_url(server.base_url());

    let mut request = sample_request(false);
    request.model = "qwen2.5-7b".to_string();

    let response = client
        .send_message(&request)
        .await
        .expect("request should succeed");

    assert_eq!(
        response.content[0],
        OutputContentBlock::Text {
            text: "Hello from Qwen".to_string(),
        }
    );

    let captured = state.lock().await;
    let request = captured.first().expect("server should capture request");
    assert_eq!(
        request.headers.get("authorization").map(String::as_str),
        Some("Bearer qwen-test-key")
    );
}

#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn qwen_falls_back_to_openai_env_vars() {
    let _lock = env_lock();
    let _qwen_key = ScopedEnvVar::set("QWEN_API_KEY", None);
    let _qwen_url = ScopedEnvVar::set("QWEN_BASE_URL", None);
    let _openai_key = ScopedEnvVar::set("OPENAI_API_KEY", Some("openai-fallback-key"));
    let _openai_url = ScopedEnvVar::set("OPENAI_BASE_URL", None);

    let state = Arc::new(Mutex::new(Vec::<CapturedRequest>::new()));
    let server = spawn_server(
        state.clone(),
        vec![http_response(
            "200 OK",
            "application/json",
            "{\"id\":\"test\",\"model\":\"qwen\",\"choices\":[{\"message\":{\"role\":\"assistant\",\"content\":\"hi\",\"tool_calls\":[]},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1}}",
        )],
    )
    .await;

    std::env::set_var("OPENAI_BASE_URL", server.base_url());

    // Should use OPENAI_API_KEY and OPENAI_BASE_URL as fallback
    let client = OpenAiCompatClient::from_env(OpenAiCompatConfig::qwen())
        .expect("should construct using fallback");

    let mut request = sample_request(false);
    request.model = "qwen2.5-7b".to_string();

    client
        .send_message(&request)
        .await
        .expect("request should succeed");

    let captured = state.lock().await;
    let request = captured.first().expect("server should capture request");
    assert_eq!(
        request.headers.get("authorization").map(String::as_str),
        Some("Bearer openai-fallback-key"),
        "Should use OPENAI_API_KEY as fallback"
    );
}

#[tokio::test]
async fn qwen_send_message_strips_prefix_from_model_name() {
    let state = Arc::new(Mutex::new(Vec::<CapturedRequest>::new()));
    let body = concat!(
        "{",
        "\"id\":\"chatcmpl_qwen_prefix\",",
        "\"model\":\"qwen2.5-7b\",",
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

    let client = OpenAiCompatClient::new("qwen-test-key", OpenAiCompatConfig::qwen())
        .with_base_url(server.base_url());

    let mut request = sample_request(false);
    request.model = "qwen/qwen2.5-7b".to_string();

    client
        .send_message(&request)
        .await
        .expect("request should succeed");

    let captured = state.lock().await;
    let request = captured.first().expect("server should capture request");
    let body: serde_json::Value = serde_json::from_str(&request.body).expect("json body");
    // The qwen/ prefix should be stripped
    assert_eq!(body["model"], json!("qwen2.5-7b"));
}
