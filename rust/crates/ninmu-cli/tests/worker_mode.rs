mod common;

use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use common::{assert_success, unique_temp_dir};
use serde_json::{json, Value};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("..")
}

#[test]
fn worker_status_renders_json_without_service() {
    let output = Command::new(env!("CARGO_BIN_EXE_ninmu"))
        .current_dir(repo_root().join("rust"))
        .args(["worker", "status", "--json"])
        .output()
        .expect("ninmu should run");

    assert_success(&output);
    let parsed: Value = serde_json::from_slice(&output.stdout).expect("stdout should be JSON");
    assert_eq!(parsed["protocol"], "ninmu.harness/v1alpha1");
    assert_eq!(parsed["status"], "idle");
}

#[test]
fn worker_connect_requires_project_and_server() {
    let missing_project = Command::new(env!("CARGO_BIN_EXE_ninmu"))
        .current_dir(repo_root().join("rust"))
        .args(["worker", "connect", "--server", "http://127.0.0.1:9"])
        .output()
        .expect("ninmu should run");
    assert!(!missing_project.status.success());

    let missing_server = Command::new(env!("CARGO_BIN_EXE_ninmu"))
        .current_dir(repo_root().join("rust"))
        .args(["worker", "connect", "--project", "project_123"])
        .output()
        .expect("ninmu should run");
    assert!(!missing_server.status.success());
}

#[test]
fn worker_connect_registers_executes_and_reports_leases() {
    let root = unique_temp_dir("worker-mode");
    let workspace = root.join("workspace");
    let home = root.join("home");
    let config_home = root.join("config");
    fs::create_dir_all(&workspace).expect("workspace should exist");
    fs::create_dir_all(&home).expect("home should exist");
    fs::create_dir_all(&config_home).expect("config home should exist");
    let service = FakeManagedService::start(workspace);

    let output = Command::new(env!("CARGO_BIN_EXE_ninmu"))
        .current_dir(repo_root().join("rust"))
        .env_clear()
        .env("NINMU_CODE_TASK_MOCK_RUNTIME", "1")
        .env("NINMU_CONFIG_HOME", &config_home)
        .env("HOME", &home)
        .env("NO_COLOR", "1")
        .env("PATH", "/usr/bin:/bin")
        .args([
            "worker",
            "connect",
            "--project",
            "project_123",
            "--server",
            &service.base_url(),
            "--output-format",
            "json",
        ])
        .output()
        .expect("ninmu should run");

    assert_success(&output);
    let stdout: Value = serde_json::from_slice(&output.stdout).expect("stdout should be JSON");
    assert_eq!(stdout["status"], "idle");
    assert_eq!(stdout["processed_leases"], 3);

    let state = service.state();
    assert!(state.registered, "worker should register");
    assert!(state.heartbeat_seen, "worker should heartbeat");
    assert!(
        state
            .events
            .iter()
            .any(|event| event["kind"] == "task.started"),
        "events: {:#?}",
        state.events
    );
    let lease_1_events = state
        .events
        .iter()
        .filter(|event| event["payload"]["lease_id"] == "lease-1")
        .collect::<Vec<_>>();
    let lease_1_kinds = lease_1_events
        .iter()
        .map(|event| event["kind"].as_str().expect("kind should be text"))
        .collect::<Vec<_>>();
    assert_eq!(
        lease_1_kinds,
        vec!["task.started", "turn.started", "task.completed"]
    );
    for (index, event) in lease_1_events.iter().enumerate() {
        assert_eq!(event["sequence"], json!(index + 1));
    }
    assert_eq!(state.results.len(), 3, "results: {:#?}", state.results);
    assert_eq!(state.results[0]["result"]["status"], "completed");
    assert_eq!(state.results[1]["result"]["status"], "cancelled");
    assert_eq!(state.results[2]["result"]["status"], "blocked");
    assert_eq!(
        state.results[2]["result"]["block_reason"],
        "project ID does not match worker project"
    );
}

#[test]
fn worker_recovers_unacknowledged_lease_after_restart() {
    let root = unique_temp_dir("worker-recovery");
    let workspace = root.join("workspace");
    let home = root.join("home");
    let config_home = root.join("config");
    fs::create_dir_all(&workspace).expect("workspace should exist");
    fs::create_dir_all(&home).expect("home should exist");
    fs::create_dir_all(&config_home).expect("config home should exist");
    let service = RecoveryManagedService::start(workspace);

    let first = worker_command(&home, &config_home, &service.base_url())
        .output()
        .expect("first worker run should start");
    assert!(
        !first.status.success(),
        "first run should fail after service rejects completion"
    );

    let second = worker_command(&home, &config_home, &service.base_url())
        .output()
        .expect("second worker run should start");
    assert_success(&second);
    let stdout: Value = serde_json::from_slice(&second.stdout).expect("stdout should be JSON");
    assert_eq!(stdout["recovered_leases"], 1);

    let state = service.state();
    assert_eq!(state.complete_attempts, 2);
    assert_eq!(state.results.len(), 1, "results: {:#?}", state.results);
    assert_eq!(state.results[0]["lease_id"], "lease-recover");
    assert_eq!(state.results[0]["result"]["status"], "completed");
}

fn worker_command(home: &std::path::Path, config_home: &std::path::Path, server: &str) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_ninmu"));
    command
        .current_dir(repo_root().join("rust"))
        .env_clear()
        .env("NINMU_CODE_TASK_MOCK_RUNTIME", "1")
        .env("NINMU_CONFIG_HOME", config_home)
        .env("HOME", home)
        .env("NO_COLOR", "1")
        .env("PATH", "/usr/bin:/bin")
        .args([
            "worker",
            "connect",
            "--project",
            "project_123",
            "--server",
            server,
            "--output-format",
            "json",
        ]);
    command
}

#[derive(Clone, Default)]
struct FakeState {
    registered: bool,
    heartbeat_seen: bool,
    next_count: usize,
    events: Vec<Value>,
    results: Vec<Value>,
}

struct FakeManagedService {
    base_url: String,
    state: Arc<Mutex<FakeState>>,
    thread: Option<thread::JoinHandle<()>>,
}

impl FakeManagedService {
    fn start(workspace: PathBuf) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("test server should bind");
        listener
            .set_nonblocking(true)
            .expect("listener should be nonblocking");
        let base_url = format!("http://{}", listener.local_addr().expect("local addr"));
        let state = Arc::new(Mutex::new(FakeState::default()));
        let thread_state = Arc::clone(&state);
        let thread = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(10);
            while Instant::now() < deadline {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let request = read_request(&mut stream);
                        let response = handle_request(&thread_state, &workspace, &request);
                        let _ = stream.write_all(response.as_bytes());
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(_) => break,
                }
                let done = {
                    let state = thread_state.lock().expect("state lock");
                    state.results.len() >= 3 && state.next_count >= 4
                };
                if done {
                    break;
                }
            }
        });
        Self {
            base_url,
            state,
            thread: Some(thread),
        }
    }

    fn base_url(&self) -> String {
        self.base_url.clone()
    }

    fn state(mut self) -> FakeState {
        if let Some(thread) = self.thread.take() {
            thread.join().expect("server thread should finish");
        }
        self.state.lock().expect("state lock").clone()
    }
}

fn read_request(stream: &mut TcpStream) -> String {
    let mut buffer = Vec::new();
    let mut chunk = [0_u8; 1024];
    loop {
        let read = stream.read(&mut chunk).expect("request should read");
        if read == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(header_end) = find_header_end(&buffer) {
            let headers = String::from_utf8_lossy(&buffer[..header_end]);
            let content_length = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim())
                })
                .and_then(|value| value.trim().parse::<usize>().ok())
                .unwrap_or(0);
            let have_body = buffer.len().saturating_sub(header_end + 4);
            if have_body >= content_length {
                break;
            }
        }
    }
    String::from_utf8(buffer).expect("request utf8")
}

fn find_header_end(buffer: &[u8]) -> Option<usize> {
    buffer.windows(4).position(|window| window == b"\r\n\r\n")
}

fn handle_request(
    state: &Arc<Mutex<FakeState>>,
    workspace: &std::path::Path,
    request: &str,
) -> String {
    let request_line = request.lines().next().unwrap_or_default();
    let body = request
        .split_once("\r\n\r\n")
        .map(|(_, body)| body)
        .unwrap_or_default();
    match request_line {
        "POST /workers/register HTTP/1.1" => {
            state.lock().expect("state lock").registered = true;
            json_response(&json!({"worker_id": "worker-test"}))
        }
        "POST /workers/worker-test/heartbeat HTTP/1.1" => {
            state.lock().expect("state lock").heartbeat_seen = true;
            json_response(&json!({"status": "ok"}))
        }
        "GET /projects/project_123/leases/next HTTP/1.1" => {
            let mut state = state.lock().expect("state lock");
            state.next_count += 1;
            match state.next_count {
                1 => json_response(&json!({
                    "lease_id": "lease-1",
                    "idempotency_key": "idem-1",
                    "task_request": task_request(workspace, "task-worker-1"),
                    "cancelled": false
                })),
                2 => json_response(&json!({
                    "lease_id": "lease-2",
                    "idempotency_key": "idem-2",
                    "task_request": task_request(workspace, "task-worker-2"),
                    "cancelled": true
                })),
                3 => json_response(&json!({
                    "lease_id": "lease-3",
                    "idempotency_key": "idem-3",
                    "task_request": rejected_task_request(workspace, "task-worker-3"),
                    "cancelled": false
                })),
                _ => empty_response(204),
            }
        }
        "POST /leases/lease-1/events HTTP/1.1"
        | "POST /leases/lease-2/events HTTP/1.1"
        | "POST /leases/lease-3/events HTTP/1.1" => {
            let event: Value = serde_json::from_str(body).expect("event JSON");
            state.lock().expect("state lock").events.push(event);
            json_response(&json!({"status": "ok"}))
        }
        "POST /leases/lease-1/complete HTTP/1.1"
        | "POST /leases/lease-2/complete HTTP/1.1"
        | "POST /leases/lease-3/complete HTTP/1.1" => {
            let result: Value = serde_json::from_str(body).expect("result JSON");
            state.lock().expect("state lock").results.push(result);
            json_response(&json!({"status": "ok"}))
        }
        _ => empty_response(404),
    }
}

fn task_request(workspace: &std::path::Path, task_id: &str) -> Value {
    let workdir = workspace.display().to_string();
    json!({
        "protocol": "ninmu.harness/v1alpha1",
        "mission_id": "mission-worker",
        "task_id": task_id,
        "objective": "complete a managed worker task",
        "workdir": workdir,
        "model": "mock-runtime",
        "permission_mode": "read-only",
        "allowed_tools": [],
        "sandbox": {
            "allowed_roots": [workdir],
            "permission_mode": "read-only",
            "network_policy": "enabled"
        }
    })
}

fn rejected_task_request(workspace: &std::path::Path, task_id: &str) -> Value {
    let mut request = task_request(workspace, task_id);
    request["project_profile"] = json!({"project_id": "wrong_project"});
    request
}

fn json_response(value: &Value) -> String {
    let body = serde_json::to_string(&value).expect("response should serialize");
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    )
}

fn empty_response(status: u16) -> String {
    format!("HTTP/1.1 {status} OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
}

#[derive(Clone, Default)]
struct RecoveryState {
    lease_sent: bool,
    next_count: usize,
    complete_attempts: usize,
    results: Vec<Value>,
}

struct RecoveryManagedService {
    base_url: String,
    state: Arc<Mutex<RecoveryState>>,
    thread: Option<thread::JoinHandle<()>>,
}

impl RecoveryManagedService {
    fn start(workspace: PathBuf) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("test server should bind");
        listener
            .set_nonblocking(true)
            .expect("listener should be nonblocking");
        let base_url = format!("http://{}", listener.local_addr().expect("local addr"));
        let state = Arc::new(Mutex::new(RecoveryState::default()));
        let thread_state = Arc::clone(&state);
        let thread = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(20);
            while Instant::now() < deadline {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let request = read_request(&mut stream);
                        let response = handle_recovery_request(&thread_state, &workspace, &request);
                        let _ = stream.write_all(response.as_bytes());
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(_) => break,
                }
                let done = {
                    let state = thread_state.lock().expect("state lock");
                    state.results.len() == 1
                        && state.complete_attempts == 2
                        && state.next_count >= 2
                };
                if done {
                    break;
                }
            }
        });
        Self {
            base_url,
            state,
            thread: Some(thread),
        }
    }

    fn base_url(&self) -> String {
        self.base_url.clone()
    }

    fn state(mut self) -> RecoveryState {
        if let Some(thread) = self.thread.take() {
            thread.join().expect("server thread should finish");
        }
        self.state.lock().expect("state lock").clone()
    }
}

fn handle_recovery_request(
    state: &Arc<Mutex<RecoveryState>>,
    workspace: &std::path::Path,
    request: &str,
) -> String {
    let request_line = request.lines().next().unwrap_or_default();
    let body = request
        .split_once("\r\n\r\n")
        .map(|(_, body)| body)
        .unwrap_or_default();
    match request_line {
        "POST /workers/register HTTP/1.1" => json_response(&json!({"worker_id": "worker-test"})),
        "POST /workers/worker-test/heartbeat HTTP/1.1"
        | "POST /leases/lease-recover/events HTTP/1.1" => json_response(&json!({"status": "ok"})),
        "GET /projects/project_123/leases/next HTTP/1.1" => {
            let mut state = state.lock().expect("state lock");
            state.next_count += 1;
            if state.lease_sent {
                empty_response(204)
            } else {
                state.lease_sent = true;
                json_response(&json!({
                    "lease_id": "lease-recover",
                    "idempotency_key": "idem-recover",
                    "task_request": task_request(workspace, "task-recover"),
                    "cancelled": false
                }))
            }
        }
        "POST /leases/lease-recover/complete HTTP/1.1" => {
            let mut state = state.lock().expect("state lock");
            state.complete_attempts += 1;
            if state.complete_attempts == 1 {
                empty_response(500)
            } else {
                let result: Value = serde_json::from_str(body).expect("result JSON");
                state.results.push(result);
                json_response(&json!({"status": "ok"}))
            }
        }
        _ => empty_response(404),
    }
}
