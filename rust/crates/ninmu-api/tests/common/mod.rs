//! Shared test utilities for API integration tests.
//!
//! Removes duplication of `TestServer`, `CapturedRequest`, `spawn_server`,
//! HTTP response helpers, and `EnvVarGuard` across the individual test files.

use std::collections::HashMap;
use std::ffi::OsString;
use std::sync::{Mutex as StdMutex, OnceLock};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::Mutex;

/// Acquire a process-global lock to serialize tests that touch environment
/// variables.  Prevents concurrent `set_var` / `remove_var` races.
pub fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: OnceLock<StdMutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| StdMutex::new(()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// RAII guard that saves and restores (or removes) an environment variable.
pub struct EnvVarGuard {
    key: &'static str,
    original: Option<OsString>,
}

impl EnvVarGuard {
    pub fn set(key: &'static str, value: Option<&str>) -> Self {
        let original = std::env::var_os(key);
        match value {
            Some(v) => std::env::set_var(key, v),
            None => std::env::remove_var(key),
        }
        Self { key, original }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        match &self.original {
            Some(value) => std::env::set_var(self.key, value),
            None => std::env::remove_var(self.key),
        }
    }
}

/// A captured HTTP request received by the test server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapturedRequest {
    pub method: Option<String>,
    pub path: String,
    pub headers: HashMap<String, String>,
    pub body: String,
}

/// A disposable TCP server that replays a sequence of canned HTTP responses
/// and records the incoming requests in a shared `Mutex<Vec<CapturedRequest>>`.
pub struct TestServer {
    pub base_url: String,
    join_handle: tokio::task::JoinHandle<()>,
}

impl TestServer {
    pub fn base_url(&self) -> String {
        self.base_url.clone()
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.join_handle.abort();
    }
}

/// Spawn a test server that replays the given HTTP responses in order.
pub async fn spawn_server(
    state: std::sync::Arc<Mutex<Vec<CapturedRequest>>>,
    responses: Vec<String>,
    capture_method: bool,
) -> TestServer {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("listener should bind");
    let address = listener
        .local_addr()
        .expect("listener should have local addr");
    let join_handle = tokio::spawn(async move {
        for response in responses {
            let (mut socket, _) = listener.accept().await.expect("server should accept");
            let mut buffer = Vec::new();
            let mut header_end = None;

            loop {
                let mut chunk = [0_u8; 1024];
                let read = socket
                    .read(&mut chunk)
                    .await
                    .expect("request read should succeed");
                if read == 0 {
                    break;
                }
                buffer.extend_from_slice(&chunk[..read]);
                if let Some(position) = find_header_end(&buffer) {
                    header_end = Some(position);
                    break;
                }
            }

            let header_end = header_end.expect("request should include headers");
            let (header_bytes, remaining) = buffer.split_at(header_end);
            let header_text =
                String::from_utf8(header_bytes.to_vec()).expect("headers should be utf8");
            let mut lines = header_text.split("\r\n");
            let request_line = lines.next().expect("request line should exist");
            let mut parts = request_line.split_whitespace();
            let method = if capture_method {
                Some(parts.next().expect("method should exist").to_string())
            } else {
                parts.next(); // consume, don't store
                None
            };
            let path = parts.next().expect("path should exist").to_string();
            let mut headers = HashMap::new();
            let mut content_length = 0_usize;
            for line in lines {
                if line.is_empty() {
                    continue;
                }
                let (name, value) = line.split_once(':').expect("header should have colon");
                let value = value.trim().to_string();
                if name.eq_ignore_ascii_case("content-length") {
                    content_length = value.parse().expect("content length should parse");
                }
                headers.insert(name.to_ascii_lowercase(), value);
            }

            let mut body = remaining[4..].to_vec();
            while body.len() < content_length {
                let mut chunk = vec![0_u8; content_length - body.len()];
                let read = socket
                    .read(&mut chunk)
                    .await
                    .expect("body read should succeed");
                if read == 0 {
                    break;
                }
                body.extend_from_slice(&chunk[..read]);
            }

            state.lock().await.push(CapturedRequest {
                method,
                path,
                headers,
                body: String::from_utf8(body).expect("body should be utf8"),
            });

            socket
                .write_all(response.as_bytes())
                .await
                .expect("response write should succeed");
        }
    });

    TestServer {
        base_url: format!("http://{address}"),
        join_handle,
    }
}

/// Find the end of HTTP headers (`\r\n\r\n`).
pub fn find_header_end(bytes: &[u8]) -> Option<usize> {
    bytes.windows(4).position(|window| window == b"\r\n\r\n")
}

/// Build a raw HTTP/1.1 response string.
pub fn http_response(status: &str, content_type: &str, body: &str) -> String {
    http_response_with_headers(status, content_type, body, &[])
}

/// Build a raw HTTP/1.1 response string with extra headers.
pub fn http_response_with_headers(
    status: &str,
    content_type: &str,
    body: &str,
    headers: &[(&str, &str)],
) -> String {
    let mut extra_headers = String::new();
    for (name, value) in headers {
        use std::fmt::Write as _;
        write!(&mut extra_headers, "{name}: {value}\r\n").expect("header write should succeed");
    }
    format!(
        "HTTP/1.1 {status}\r\ncontent-type: {content_type}\r\n{extra_headers}content-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    )
}
