mod common;

use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use common::{assert_success, unique_temp_dir};
use ninmu_mock_anthropic_service::{MockAnthropicService, SCENARIO_PREFIX};
use serde_json::json;
use serde_json::Value;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("..")
}

fn fixture_request_path() -> PathBuf {
    repo_root().join("examples/harness/task-request.json")
}

struct MockTaskFixture {
    request_path: PathBuf,
    home: PathBuf,
    config_home: PathBuf,
}

fn write_mock_task_fixture(label: &str, scenario: &str) -> MockTaskFixture {
    let root = unique_temp_dir(label);
    let workspace = root.join("workspace");
    let home = root.join("home");
    let config_home = root.join("config");
    fs::create_dir_all(&workspace).expect("workspace should exist");
    fs::create_dir_all(&home).expect("home should exist");
    fs::create_dir_all(&config_home).expect("config home should exist");
    let request_path = root.join("task.json");
    let workspace_json = workspace.display().to_string();
    fs::write(
        &request_path,
        serde_json::to_vec(&json!({
            "protocol": "ninmu.harness/v1alpha1",
            "mission_id": format!("mission-{label}"),
            "task_id": format!("task-{label}"),
            "objective": format!("MOCK_TASK_SCENARIO:{scenario}"),
            "workdir": workspace_json,
            "model": "mock-runtime",
            "permission_mode": "read-only",
            "allowed_tools": [],
            "sandbox": {
                "allowed_roots": [workspace_json],
                "permission_mode": "read-only",
                "network_policy": "enabled"
            }
        }))
        .expect("request should serialize"),
    )
    .expect("request should write");

    MockTaskFixture {
        request_path,
        home,
        config_home,
    }
}

fn task_command(fixture: &MockTaskFixture) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_ninmu"));
    command
        .current_dir(repo_root().join("rust"))
        .env_clear()
        .env("NINMU_CODE_TASK_MOCK_RUNTIME", "1")
        .env("NINMU_CONFIG_HOME", &fixture.config_home)
        .env("HOME", &fixture.home)
        .env("NO_COLOR", "1")
        .env("PATH", "/usr/bin:/bin");
    command
}

fn write_real_runtime_task_fixture(
    label: &str,
    scenario: &str,
    permission_mode: &str,
    allowed_tools: &[&str],
) -> MockTaskFixture {
    let root = unique_temp_dir(label);
    let workspace = root.join("workspace");
    let home = root.join("home");
    let config_home = root.join("config");
    fs::create_dir_all(&workspace).expect("workspace should exist");
    fs::create_dir_all(&home).expect("home should exist");
    fs::create_dir_all(&config_home).expect("config home should exist");
    let request_path = root.join("task.json");
    let workspace_json = workspace.display().to_string();
    fs::write(
        &request_path,
        serde_json::to_vec(&json!({
            "protocol": "ninmu.harness/v1alpha1",
            "mission_id": format!("mission-{label}"),
            "task_id": format!("task-{label}"),
            "objective": format!("{SCENARIO_PREFIX}{scenario}"),
            "workdir": workspace_json,
            "model": "claude-sonnet-4-6",
            "permission_mode": permission_mode,
            "allowed_tools": allowed_tools,
            "sandbox": {
                "allowed_roots": [workspace_json],
                "permission_mode": permission_mode,
                "network_policy": "enabled"
            }
        }))
        .expect("request should serialize"),
    )
    .expect("request should write");

    MockTaskFixture {
        request_path,
        home,
        config_home,
    }
}

fn real_runtime_task_command(fixture: &MockTaskFixture, base_url: &str) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_ninmu"));
    command
        .current_dir(repo_root().join("rust"))
        .env_clear()
        .env("ANTHROPIC_API_KEY", "test-run-task-key")
        .env("ANTHROPIC_BASE_URL", base_url)
        .env("NINMU_CONFIG_HOME", &fixture.config_home)
        .env("HOME", &fixture.home)
        .env("NO_COLOR", "1")
        .env("PATH", "/usr/bin:/bin");
    command
}

#[test]
fn run_task_uses_real_runtime_path_with_mock_provider() {
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should build");
    let server = runtime
        .block_on(MockAnthropicService::spawn())
        .expect("mock service should start");
    let fixture = write_real_runtime_task_fixture(
        "run-task-real-runtime",
        "streaming_text",
        "read-only",
        &[],
    );
    let output = real_runtime_task_command(&fixture, &server.base_url())
        .args([
            "run-task",
            "--input",
            fixture.request_path.to_str().expect("request path utf8"),
            "--output-format",
            "json",
        ])
        .output()
        .expect("ninmu should run");

    assert_success(&output);
    assert!(output.stderr.is_empty(), "task mode should not log");
    let stdout = String::from_utf8(output.stdout).expect("stdout utf8");
    assert!(!stdout.contains("\u{1b}["), "stdout must not contain ANSI");
    assert_eq!(
        stdout
            .lines()
            .filter(|line| !line.trim().is_empty())
            .count(),
        1,
        "stdout should contain one JSON result"
    );
    let parsed: Value = serde_json::from_str(stdout.trim()).expect("stdout should be JSON");
    assert_eq!(parsed["status"], "completed");
    assert!(
        parsed["summary"]
            .as_str()
            .expect("summary text")
            .contains("Mock streaming says hello from the parity harness"),
        "result JSON: {parsed:#}"
    );
    assert!(parsed["usage"]["input_tokens"].as_u64().unwrap_or(0) > 0);
    assert!(parsed["tool_uses"].as_array().is_some());
    assert!(parsed["tool_results"].as_array().is_some());
    assert!(parsed["sandbox"].is_object());
    assert!(parsed["evidence"].is_object());
    assert!(parsed["confidence"].is_object());

    let captured = runtime.block_on(server.captured_requests());
    assert!(
        captured
            .iter()
            .any(|request| request.path == "/v1/messages"),
        "real runtime should call the Anthropic messages endpoint"
    );
}

#[test]
fn run_task_real_runtime_reports_changed_files() {
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should build");
    let server = runtime
        .block_on(MockAnthropicService::spawn())
        .expect("mock service should start");
    let fixture = write_real_runtime_task_fixture(
        "run-task-real-runtime-changes",
        "write_file_allowed",
        "workspace-write",
        &["write_file"],
    );
    let output = real_runtime_task_command(&fixture, &server.base_url())
        .args([
            "run-task",
            "--input",
            fixture.request_path.to_str().expect("request path utf8"),
            "--output-format",
            "json",
        ])
        .output()
        .expect("ninmu should run");

    assert_success(&output);
    assert!(output.stderr.is_empty(), "task mode should not log");
    let parsed: Value = serde_json::from_slice(&output.stdout).expect("stdout should be JSON");
    assert_eq!(parsed["status"], "completed");
    let changed_files = parsed["changed_files"]
        .as_array()
        .expect("changed_files should be an array");
    assert!(
        changed_files
            .iter()
            .any(|path| path.as_str() == Some("generated/output.txt")),
        "result JSON: {parsed:#}"
    );
}

#[test]
fn run_task_reads_input_path_and_writes_exactly_one_json_result() {
    let fixture = write_mock_task_fixture("run-task-path", "streaming_text");
    let output = task_command(&fixture)
        .args([
            "run-task",
            "--input",
            fixture.request_path.to_str().expect("fixture path utf8"),
            "--output-format",
            "json",
        ])
        .output()
        .expect("ninmu should run");

    assert_success(&output);
    assert!(
        output.stderr.is_empty(),
        "task mode should not log for stub"
    );

    let stdout = String::from_utf8(output.stdout).expect("stdout utf8");
    assert!(!stdout.contains("\u{1b}["), "stdout must not contain ANSI");
    assert_eq!(
        stdout
            .lines()
            .filter(|line| !line.trim().is_empty())
            .count(),
        1,
        "stdout should contain one JSON object"
    );
    let parsed: Value = serde_json::from_str(stdout.trim()).expect("stdout should be JSON");
    assert_eq!(parsed["protocol"], "ninmu.harness/v1alpha1");
    assert_eq!(parsed["status"], "completed");
    assert_eq!(parsed["mission_id"], "mission-run-task-path");
    assert_eq!(parsed["task_id"], "task-run-task-path");
    assert_eq!(parsed["retryable"], false);
    assert!(parsed["usage"].is_object());
    assert!(parsed["sandbox"].is_object());
    assert!(parsed["evidence"].is_object());
    assert!(parsed["confidence"].is_object());
}

#[test]
fn run_task_reads_input_from_stdin() {
    let fixture = write_mock_task_fixture("run-task-stdin", "streaming_text");
    let request = fs::read_to_string(&fixture.request_path).expect("fixture should exist");
    let mut child = task_command(&fixture)
        .args(["run-task", "--input", "-", "--output-format", "json"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("ninmu should spawn");
    {
        let stdin = child.stdin.as_mut().expect("stdin should be piped");
        stdin
            .write_all(request.as_bytes())
            .expect("request should write");
    }
    drop(child.stdin.take());

    let output = child.wait_with_output().expect("ninmu should finish");
    assert_success(&output);
    assert!(output.stderr.is_empty());
    let parsed: Value =
        serde_json::from_slice(&output.stdout).expect("stdout should be one JSON result");
    assert_eq!(parsed["status"], "completed");
}

#[test]
fn run_task_invalid_input_path_exits_one() {
    let output = Command::new(env!("CARGO_BIN_EXE_ninmu"))
        .current_dir(repo_root().join("rust"))
        .args([
            "run-task",
            "--input",
            "/definitely/not/a/task.json",
            "--output-format",
            "json",
        ])
        .output()
        .expect("ninmu should run");

    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
}

#[test]
fn run_task_invalid_json_exits_one() {
    let root = unique_temp_dir("run-task-invalid-json");
    fs::create_dir_all(&root).expect("temp dir should exist");
    let input = root.join("invalid.json");
    fs::write(&input, "{not json").expect("invalid fixture should write");

    let output = Command::new(env!("CARGO_BIN_EXE_ninmu"))
        .current_dir(repo_root().join("rust"))
        .args([
            "run-task",
            "--input",
            input.to_str().expect("input path utf8"),
            "--output-format",
            "json",
        ])
        .output()
        .expect("ninmu should run");

    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
}

#[test]
fn run_task_contract_validation_errors_exit_two() {
    let root = unique_temp_dir("run-task-invalid-contract");
    fs::create_dir_all(&root).expect("temp dir should exist");
    let input = root.join("missing-objective.json");
    let mut request: Value =
        serde_json::from_str(&fs::read_to_string(fixture_request_path()).expect("fixture exists"))
            .expect("fixture should parse");
    request
        .as_object_mut()
        .expect("request should be object")
        .remove("objective");
    fs::write(
        &input,
        serde_json::to_vec(&request).expect("request should serialize"),
    )
    .expect("invalid contract fixture should write");

    let output = Command::new(env!("CARGO_BIN_EXE_ninmu"))
        .current_dir(repo_root().join("rust"))
        .args([
            "run-task",
            "--input",
            input.to_str().expect("input path utf8"),
            "--output-format",
            "json",
        ])
        .output()
        .expect("ninmu should run");

    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    let stderr: Value = serde_json::from_slice(&output.stderr).expect("stderr should be JSON");
    assert_eq!(stderr["kind"], "contract_validation");
}

#[test]
fn run_task_rejects_non_json_output_format_as_process_error() {
    let output = Command::new(env!("CARGO_BIN_EXE_ninmu"))
        .current_dir(repo_root().join("rust"))
        .args([
            "run-task",
            "--input",
            fixture_request_path().to_str().expect("fixture path utf8"),
            "--output-format",
            "text",
        ])
        .output()
        .expect("ninmu should run");

    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
}

#[test]
fn run_task_fixture_is_ninmu_compatible() {
    let root = unique_temp_dir("run-task-compat-fixture");
    let home = root.join("home");
    let config_home = root.join("config");
    fs::create_dir_all(&home).expect("home should exist");
    fs::create_dir_all(&config_home).expect("config home should exist");
    let output = Command::new(env!("CARGO_BIN_EXE_ninmu"))
        .current_dir(repo_root().join("rust"))
        .env_clear()
        .env("NINMU_CODE_TASK_MOCK_RUNTIME", "1")
        .env("NINMU_CONFIG_HOME", &config_home)
        .env("HOME", &home)
        .env("NO_COLOR", "1")
        .env("PATH", "/usr/bin:/bin")
        .args([
            "run-task",
            "--input",
            fixture_request_path().to_str().expect("fixture path utf8"),
            "--output-format",
            "json",
        ])
        .output()
        .expect("ninmu should run");

    assert_success(&output);
    assert!(output.stderr.is_empty(), "task mode should not log");
    let parsed: ninmu_runtime::harness_contract::HarnessTaskResult =
        serde_json::from_slice(&output.stdout).expect("stdout should match HarnessTaskResult");
    parsed.validate().expect("result should validate");
    assert!(matches!(
        parsed.status,
        ninmu_runtime::harness_contract::HarnessTaskStatus::Completed
            | ninmu_runtime::harness_contract::HarnessTaskStatus::Failed
            | ninmu_runtime::harness_contract::HarnessTaskStatus::Blocked
    ));
}

#[test]
fn run_task_uses_deterministic_mock_runtime_path_without_stdout_noise() {
    let fixture = write_mock_task_fixture("run-task-mock-provider", "streaming_text");
    let output = task_command(&fixture)
        .args([
            "run-task",
            "--input",
            fixture.request_path.to_str().expect("request path utf8"),
            "--output-format",
            "json",
        ])
        .output()
        .expect("ninmu should run");

    assert_success(&output);
    assert!(output.stderr.is_empty(), "task mode should not log");
    let stdout = String::from_utf8(output.stdout).expect("stdout utf8");
    assert!(!stdout.contains("\u{1b}["), "stdout must not contain ANSI");
    assert_eq!(
        stdout
            .lines()
            .filter(|line| !line.trim().is_empty())
            .count(),
        1,
        "stdout should contain one JSON object"
    );
    let parsed: Value = serde_json::from_str(stdout.trim()).expect("stdout should be JSON");
    assert_eq!(parsed["status"], "completed");
    assert!(parsed["summary"]
        .as_str()
        .expect("summary text")
        .contains("streaming text parity complete"));
    assert!(parsed["tool_uses"].as_array().is_some());
    assert!(parsed["usage"]["input_tokens"].as_u64().unwrap_or(0) > 0);
    assert!(parsed["sandbox"].is_object());
    assert!(parsed["evidence"].is_object());
    assert!(parsed["confidence"].is_object());
}

#[test]
fn run_task_writes_ordered_event_log_without_stdout_noise() {
    let fixture = write_mock_task_fixture("run-task-event-log", "streaming_text");
    let event_log = fixture
        .request_path
        .parent()
        .expect("request has parent")
        .join("events.ndjson");
    let output = task_command(&fixture)
        .args([
            "run-task",
            "--input",
            fixture.request_path.to_str().expect("request path utf8"),
            "--event-log",
            event_log.to_str().expect("event log utf8"),
            "--output-format",
            "json",
        ])
        .output()
        .expect("ninmu should run");

    assert_success(&output);
    assert!(
        output.stderr.is_empty(),
        "event log mode should keep stderr quiet"
    );
    let stdout = String::from_utf8(output.stdout).expect("stdout utf8");
    assert_eq!(
        stdout
            .lines()
            .filter(|line| !line.trim().is_empty())
            .count(),
        1,
        "stdout should contain one JSON result"
    );
    let events = fs::read_to_string(event_log).expect("event log should exist");
    let parsed = events
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("event line should be JSON"))
        .collect::<Vec<_>>();

    assert_eq!(parsed.len(), 3);
    assert_eq!(parsed[0]["kind"], "task.started");
    assert_eq!(parsed[0]["sequence"], 1);
    assert_eq!(parsed[1]["kind"], "turn.started");
    assert_eq!(parsed[1]["sequence"], 2);
    assert_eq!(parsed[2]["kind"], "task.completed");
    assert_eq!(parsed[2]["sequence"], 3);
}

#[test]
fn run_task_event_log_includes_runtime_tool_and_file_events() {
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should build");
    let server = runtime
        .block_on(MockAnthropicService::spawn())
        .expect("mock service should start");
    let fixture = write_real_runtime_task_fixture(
        "run-task-event-log-runtime",
        "write_file_allowed",
        "workspace-write",
        &["write_file"],
    );
    let event_log = fixture
        .request_path
        .parent()
        .expect("request has parent")
        .join("events.ndjson");
    let output = real_runtime_task_command(&fixture, &server.base_url())
        .args([
            "run-task",
            "--input",
            fixture.request_path.to_str().expect("request path utf8"),
            "--event-log",
            event_log.to_str().expect("event log utf8"),
            "--output-format",
            "json",
        ])
        .output()
        .expect("ninmu should run");

    assert_success(&output);
    assert!(
        output.stderr.is_empty(),
        "event log should keep stderr quiet"
    );
    let stdout = String::from_utf8(output.stdout).expect("stdout utf8");
    assert_eq!(
        stdout
            .lines()
            .filter(|line| !line.trim().is_empty())
            .count(),
        1,
        "stdout should contain one JSON result"
    );

    let events = fs::read_to_string(event_log).expect("event log should exist");
    let parsed = events
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("event line should be JSON"))
        .collect::<Vec<_>>();
    let kinds = parsed
        .iter()
        .map(|event| event["kind"].as_str().expect("kind string"))
        .collect::<Vec<_>>();

    assert_eq!(kinds.first().copied(), Some("task.started"));
    assert!(kinds.contains(&"turn.started"), "events: {kinds:?}");
    assert!(kinds.contains(&"tool.started"), "events: {kinds:?}");
    assert!(kinds.contains(&"tool.completed"), "events: {kinds:?}");
    assert!(kinds.contains(&"file.changed"), "events: {kinds:?}");
    assert_eq!(kinds.last().copied(), Some("task.completed"));
    for (index, event) in parsed.iter().enumerate() {
        assert_eq!(event["sequence"], json!(index + 1));
    }
}

#[test]
fn run_task_records_and_applies_skill_profile() {
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should build");
    let server = runtime
        .block_on(MockAnthropicService::spawn())
        .expect("mock service should start");
    let fixture =
        write_real_runtime_task_fixture("run-task-skills", "streaming_text", "read-only", &[]);
    let mut request: Value =
        serde_json::from_str(&fs::read_to_string(&fixture.request_path).expect("request exists"))
            .expect("request should parse");
    request["skill_profile"] = json!({
        "skills": [{
            "id": "skill.audit",
            "version": "2026-04-29",
            "prompt_fragments": ["SKILL_FRAGMENT: prefer evidence-first summaries"],
            "behavioural_examples": ["example.audit.concise"],
            "steering_strength": "strong"
        }]
    });
    fs::write(
        &fixture.request_path,
        serde_json::to_vec(&request).expect("request should serialize"),
    )
    .expect("request should write");

    let output = real_runtime_task_command(&fixture, &server.base_url())
        .args([
            "run-task",
            "--input",
            fixture.request_path.to_str().expect("request path utf8"),
            "--output-format",
            "json",
        ])
        .output()
        .expect("ninmu should run");

    assert_success(&output);
    let parsed: Value = serde_json::from_slice(&output.stdout).expect("stdout should be JSON");
    assert_eq!(parsed["applied_skills"][0], "skill.audit");
    assert_eq!(parsed["skill_evaluations"][0]["skill_id"], "skill.audit");
    assert_eq!(parsed["skill_evaluations"][0]["applied"], true);
    assert_eq!(
        parsed["skill_evaluations"][0]["prompt_fragment_ids"][0],
        "fragment-1"
    );
    assert_eq!(
        parsed["skill_evaluations"][0]["behavioural_example_ids"][0],
        "example.audit.concise"
    );
    assert_eq!(
        parsed["skill_evaluations"][0]["steering_strength"],
        "strong"
    );

    let captured = runtime.block_on(server.captured_requests());
    let messages_request = captured
        .iter()
        .find(|request| request.path == "/v1/messages")
        .expect("messages request should be captured");
    assert!(
        messages_request
            .raw_body
            .contains("SKILL_FRAGMENT: prefer evidence-first summaries"),
        "raw body: {}",
        messages_request.raw_body
    );
}
