use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use ninmu_runtime::harness_contract::{
    HarnessCompletionEvidence, HarnessConfidence, HarnessProtocolVersion, HarnessTaskRequest,
    HarnessTaskResult, HarnessTaskStatus, HarnessTestReport, HarnessTestStatus, HarnessUsage,
};
use ninmu_runtime::{pricing_for_model, ModelPricing};
use serde_json::json;

use crate::app::{collect_tool_results, collect_tool_uses, final_assistant_text, LiveCli};
use crate::args::CliOutputFormat;
use crate::format::{default_permission_mode, parse_permission_mode_arg};
use crate::task_events::TaskEventSink;
use crate::task_evidence;
use crate::task_sandbox;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PathOrStdin {
    Path(PathBuf),
    Stdin,
}

impl PathOrStdin {
    pub(crate) fn parse(value: &str) -> Self {
        if value == "-" {
            Self::Stdin
        } else {
            Self::Path(PathBuf::from(value))
        }
    }
}

#[derive(Debug)]
pub(crate) enum RunTaskError {
    Process(String),
    ContractValidation(String),
}

impl std::fmt::Display for RunTaskError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Process(message) | Self::ContractValidation(message) => {
                formatter.write_str(message)
            }
        }
    }
}

impl std::error::Error for RunTaskError {}

pub(crate) fn run_task(
    input: PathOrStdin,
    output_format: CliOutputFormat,
    event_log: Option<PathBuf>,
) -> Result<(), RunTaskError> {
    if output_format != CliOutputFormat::Json {
        return Err(RunTaskError::Process(
            "run-task only supports --output-format json".to_string(),
        ));
    }

    let raw = read_input(input)?;
    let request: HarnessTaskRequest = serde_json::from_str(&raw)
        .map_err(|error| RunTaskError::Process(format!("invalid task JSON: {error}")))?;
    if let Err(error) = request.validate() {
        return Err(RunTaskError::ContractValidation(error.to_string()));
    }

    let mut events = match event_log {
        Some(path) => TaskEventSink::file(path)
            .map_err(|error| RunTaskError::Process(format!("failed to open event log: {error}")))?,
        None => TaskEventSink::disabled(),
    };
    events
        .emit(
            &request.mission_id,
            &request.task_id,
            "task.started",
            json!({"objective": request.objective.clone()}),
        )
        .map_err(|error| RunTaskError::Process(format!("failed to write event: {error}")))?;
    let result = execute_task_request(&request)?;
    emit_result_events(&mut events, &request, &result)?;
    let terminal_kind = match result.status {
        HarnessTaskStatus::Completed => "task.completed",
        HarnessTaskStatus::Failed => "task.failed",
        HarnessTaskStatus::Blocked => "task.blocked",
        HarnessTaskStatus::Cancelled => "task.cancelled",
    };
    events
        .emit(
            &request.mission_id,
            &request.task_id,
            terminal_kind,
            json!({"status": result.status, "summary": result.summary.clone()}),
        )
        .map_err(|error| RunTaskError::Process(format!("failed to write event: {error}")))?;
    result
        .validate()
        .map_err(|error| RunTaskError::Process(format!("invalid generated result: {error}")))?;
    let stdout = io::stdout();
    let mut lock = stdout.lock();
    serde_json::to_writer(&mut lock, &result)
        .map_err(|error| RunTaskError::Process(error.to_string()))?;
    lock.write_all(b"\n")
        .map_err(|error| RunTaskError::Process(error.to_string()))?;
    Ok(())
}

fn emit_result_events(
    events: &mut TaskEventSink,
    request: &HarnessTaskRequest,
    result: &HarnessTaskResult,
) -> Result<(), RunTaskError> {
    events
        .emit(
            &request.mission_id,
            &request.task_id,
            "turn.started",
            json!({"model": request.model.clone(), "workdir": request.workdir.clone()}),
        )
        .map_err(|error| RunTaskError::Process(format!("failed to write event: {error}")))?;
    for tool_use in &result.tool_uses {
        events
            .emit(
                &request.mission_id,
                &request.task_id,
                "tool.started",
                tool_use.clone(),
            )
            .map_err(|error| RunTaskError::Process(format!("failed to write event: {error}")))?;
    }
    for tool_result in &result.tool_results {
        events
            .emit(
                &request.mission_id,
                &request.task_id,
                "tool.completed",
                tool_result.clone(),
            )
            .map_err(|error| RunTaskError::Process(format!("failed to write event: {error}")))?;
    }
    for changed_file in &result.changed_files {
        events
            .emit(
                &request.mission_id,
                &request.task_id,
                "file.changed",
                json!({"path": changed_file}),
            )
            .map_err(|error| RunTaskError::Process(format!("failed to write event: {error}")))?;
    }
    for test in &result.tests {
        events
            .emit(
                &request.mission_id,
                &request.task_id,
                "test.started",
                json!({"command": test.command}),
            )
            .map_err(|error| RunTaskError::Process(format!("failed to write event: {error}")))?;
        events
            .emit(
                &request.mission_id,
                &request.task_id,
                "test.completed",
                json!({
                    "command": test.command,
                    "status": test.status,
                    "exit_code": test.exit_code,
                }),
            )
            .map_err(|error| RunTaskError::Process(format!("failed to write event: {error}")))?;
    }
    Ok(())
}

pub(crate) fn write_error_and_exit(error: RunTaskError) -> ! {
    match error {
        RunTaskError::Process(message) => {
            write_error("process", &message);
            std::process::exit(1);
        }
        RunTaskError::ContractValidation(message) => {
            write_error("contract_validation", &message);
            std::process::exit(2);
        }
    }
}

fn write_error(kind: &str, message: &str) {
    let stderr = io::stderr();
    let mut lock = stderr.lock();
    let _ = writeln!(
        lock,
        "{}",
        json!({
            "type": "error",
            "kind": kind,
            "error": message,
        })
    );
}

fn read_input(input: PathOrStdin) -> Result<String, RunTaskError> {
    match input {
        PathOrStdin::Path(path) => fs::read_to_string(&path).map_err(|error| {
            RunTaskError::Process(format!("failed to read {}: {error}", path.display()))
        }),
        PathOrStdin::Stdin => {
            let mut buffer = String::new();
            io::stdin()
                .read_to_string(&mut buffer)
                .map_err(|error| RunTaskError::Process(format!("failed to read stdin: {error}")))?;
            Ok(buffer)
        }
    }
}

pub(crate) fn execute_task_request(
    request: &HarnessTaskRequest,
) -> Result<HarnessTaskResult, RunTaskError> {
    if env::var("NINMU_CODE_TASK_MOCK_RUNTIME").as_deref() == Ok("1") {
        Ok(execute_mock_task(request))
    } else {
        execute_task(request)
    }
}

fn execute_task(request: &HarnessTaskRequest) -> Result<HarnessTaskResult, RunTaskError> {
    let workdir = PathBuf::from(&request.workdir);
    let _cwd_guard = CwdGuard::enter(&workdir)?;
    let permission_mode = request
        .permission_mode
        .as_deref()
        .map(parse_permission_mode_arg)
        .transpose()
        .map_err(RunTaskError::Process)?
        .unwrap_or_else(default_permission_mode);
    let allowed_tools = crate::normalize_allowed_tools(&request.allowed_tools)
        .map_err(|error| RunTaskError::Process(format!("invalid allowed_tools: {error}")))?;
    let model = request
        .model
        .clone()
        .unwrap_or_else(|| crate::DEFAULT_MODEL.to_string());
    let mut cli = LiveCli::new(model.clone(), true, allowed_tools, permission_mode, None)
        .map_err(|error| RunTaskError::Process(error.to_string()))?;
    let summary = cli
        .run_turn_summary(&task_prompt(request))
        .map_err(|error| RunTaskError::Process(error.to_string()))?;
    let mut result = build_result_from_summary(request, &model, &summary);
    run_acceptance_tests(request, &mut result);
    apply_evidence(&mut result);
    Ok(result)
}

fn execute_mock_task(request: &HarnessTaskRequest) -> HarnessTaskResult {
    let mut result = base_result(request);
    result.summary = "streaming text parity complete.".to_string();
    result.output = json!({
        "message": result.summary,
        "iterations": 1,
    });
    result.usage = HarnessUsage {
        input_tokens: 12,
        output_tokens: 8,
        total_tokens: 20,
    };
    result.estimated_cost = 0.0;
    result.tool_uses = Vec::new();
    result.tool_results = Vec::new();
    run_acceptance_tests(request, &mut result);
    apply_evidence(&mut result);
    result
}

struct CwdGuard {
    original: PathBuf,
}

impl CwdGuard {
    fn enter(workdir: &Path) -> Result<Self, RunTaskError> {
        let original = env::current_dir()
            .map_err(|error| RunTaskError::Process(format!("failed to read cwd: {error}")))?;
        env::set_current_dir(workdir).map_err(|error| {
            RunTaskError::Process(format!(
                "failed to enter workdir {}: {error}",
                workdir.display()
            ))
        })?;
        Ok(Self { original })
    }
}

impl Drop for CwdGuard {
    fn drop(&mut self) {
        let _ = env::set_current_dir(&self.original);
    }
}

fn task_prompt(request: &HarnessTaskRequest) -> String {
    let mut prompt = request.objective.clone();
    if let Some(skill_profile) = &request.skill_profile {
        for skill in &skill_profile.skills {
            for fragment in &skill.prompt_fragments {
                prompt.push_str("\n\n");
                prompt.push_str(fragment);
            }
        }
    }
    if !request.acceptance_tests.is_empty() {
        prompt.push_str("\n\nAcceptance tests requested by orchestrator:\n");
        for command in &request.acceptance_tests {
            prompt.push_str("- ");
            prompt.push_str(command);
            prompt.push('\n');
        }
    }
    prompt
}

fn build_result_from_summary(
    request: &HarnessTaskRequest,
    model: &str,
    summary: &ninmu_runtime::TurnSummary,
) -> HarnessTaskResult {
    let text = final_assistant_text(summary);
    let cost = summary
        .usage
        .estimate_cost_usd_with_pricing(
            pricing_for_model(model).unwrap_or_else(ModelPricing::default_sonnet_tier),
        )
        .total_cost_usd();
    let mut result = base_result(request);
    result.summary.clone_from(&text);
    result.output = json!({
        "message": text,
        "iterations": summary.iterations,
    });
    result.tool_uses = collect_tool_uses(summary);
    result.tool_results = collect_tool_results(summary);
    result.changed_files = collect_changed_files(&result.tool_results, Path::new(&request.workdir));
    result.usage = HarnessUsage {
        input_tokens: u64::from(summary.usage.input_tokens),
        output_tokens: u64::from(summary.usage.output_tokens),
        total_tokens: u64::from(summary.usage.total_tokens()),
    };
    result.estimated_cost = cost;
    result
}

fn collect_changed_files(tool_results: &[serde_json::Value], workdir: &Path) -> Vec<String> {
    let mut changed = BTreeSet::new();
    for result in tool_results {
        let Some(output) = result.get("output").and_then(serde_json::Value::as_str) else {
            continue;
        };
        let Ok(parsed) = serde_json::from_str::<serde_json::Value>(output) else {
            continue;
        };
        let Some(file_path) = parsed.get("filePath").and_then(serde_json::Value::as_str) else {
            continue;
        };
        changed.insert(workspace_relative_path(file_path, workdir));
    }
    changed.into_iter().collect()
}

fn workspace_relative_path(file_path: &str, workdir: &Path) -> String {
    let path = fs::canonicalize(file_path).unwrap_or_else(|_| PathBuf::from(file_path));
    let workdir = fs::canonicalize(workdir).unwrap_or_else(|_| workdir.to_path_buf());
    path.strip_prefix(&workdir)
        .unwrap_or(path.as_path())
        .to_string_lossy()
        .trim_start_matches('/')
        .to_string()
}

fn base_result(request: &HarnessTaskRequest) -> HarnessTaskResult {
    HarnessTaskResult {
        protocol: HarnessProtocolVersion::V1Alpha1,
        mission_id: request.mission_id.clone(),
        task_id: request.task_id.clone(),
        status: HarnessTaskStatus::Completed,
        summary: "Task completed.".to_string(),
        output: json!({}),
        changed_files: Vec::new(),
        artifacts: Vec::new(),
        tests: Vec::new(),
        usage: HarnessUsage::default(),
        estimated_cost: 0.0,
        retryable: false,
        block_reason: None,
        sandbox: task_sandbox::summarize_sandbox(request),
        evidence: HarnessCompletionEvidence {
            items: vec!["request validated".to_string()],
            acceptance_tests_passed: None,
            changed_files_observed: false,
            unresolved_blockers: Vec::new(),
        },
        confidence: HarnessConfidence {
            score: 0.25,
            level: "low".to_string(),
        },
        status_reason: Some("request validated".to_string()),
        orchestrator_recommendation: Some("review".to_string()),
        diff: None,
        commit_sha: None,
        risk: None,
        tool_uses: Vec::new(),
        tool_results: Vec::new(),
        applied_skills: applied_skills(request),
        skill_confidence_delta: request
            .skill_profile
            .as_ref()
            .filter(|profile| !profile.skills.is_empty())
            .map(|_| 0.0),
        skill_evaluations: skill_evaluations(request),
    }
}

fn applied_skills(request: &HarnessTaskRequest) -> Vec<String> {
    request
        .skill_profile
        .as_ref()
        .map(|profile| {
            profile
                .skills
                .iter()
                .map(|skill| skill.id.clone())
                .collect()
        })
        .unwrap_or_default()
}

fn skill_evaluations(
    request: &HarnessTaskRequest,
) -> Vec<ninmu_runtime::harness_contract::HarnessSkillEvaluation> {
    request
        .skill_profile
        .as_ref()
        .map(|profile| {
            profile
                .skills
                .iter()
                .map(
                    |skill| ninmu_runtime::harness_contract::HarnessSkillEvaluation {
                        skill_id: skill.id.clone(),
                        applied: true,
                        notes: Some("skill profile applied to task prompt".to_string()),
                        prompt_fragment_ids: skill
                            .prompt_fragments
                            .iter()
                            .enumerate()
                            .map(|(index, _)| format!("fragment-{}", index + 1))
                            .collect(),
                        behavioural_example_ids: skill.behavioural_examples.clone(),
                        steering_strength: skill.steering_strength.clone(),
                    },
                )
                .collect()
        })
        .unwrap_or_default()
}

fn run_acceptance_tests(request: &HarnessTaskRequest, result: &mut HarnessTaskResult) {
    result.tests = request
        .acceptance_tests
        .iter()
        .map(|command| run_acceptance_test(command))
        .collect();
}

fn run_acceptance_test(command: &str) -> HarnessTestReport {
    match Command::new("sh").arg("-c").arg(command).output() {
        Ok(output) => {
            let status = if output.status.success() {
                HarnessTestStatus::Passed
            } else {
                HarnessTestStatus::Failed
            };
            HarnessTestReport {
                command: command.to_string(),
                status,
                exit_code: output.status.code(),
                output: Some(format!(
                    "{}{}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                )),
                artifact: None,
            }
        }
        Err(error) => HarnessTestReport {
            command: command.to_string(),
            status: HarnessTestStatus::Failed,
            exit_code: None,
            output: Some(error.to_string()),
            artifact: None,
        },
    }
}

fn apply_evidence(result: &mut HarnessTaskResult) {
    let decision = task_evidence::decide(&result.tests, &result.changed_files, &[]);
    result.status = decision.status;
    result.evidence = decision.evidence;
    result.confidence = decision.confidence;
    result.status_reason = Some(decision.status_reason);
    result.orchestrator_recommendation = Some(decision.recommendation);
    if result.status == HarnessTaskStatus::Failed {
        result.retryable = true;
    }
}
