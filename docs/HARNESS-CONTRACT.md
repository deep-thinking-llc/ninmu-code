# Ninmu Harness Contract

Protocol version: `ninmu.harness/v1alpha1`

This contract defines the process-safe task interface between `ninmu` and `ninmu-code`.
Task mode is stricter than prompt mode: stdout is machine-owned, schema-versioned, and stable.

## Task Request

`HarnessTaskRequest` is a JSON object with:

- `protocol`: must be `ninmu.harness/v1alpha1`
- `mission_id`: non-empty orchestrator mission identifier
- `task_id`: non-empty orchestrator task identifier
- `objective`: non-empty coding task objective
- `workdir`: non-empty project or worktree directory for execution
- `model`: optional requested model
- `permission_mode`: optional permission policy label
- `allowed_tools`: optional list of tool names
- `acceptance_tests`: optional list of test commands
- `timeout_seconds`: optional task timeout
- `sandbox`: optional requested sandbox policy
- `skill_profile`: optional applied skill metadata
- `project_profile`: optional project metadata
- `previous_context`: optional mission context

## Task Result

`HarnessTaskResult` is the single JSON object written to stdout by task mode.

Required fields:

- `protocol`
- `mission_id`
- `task_id`
- `status`: one of `completed`, `failed`, `blocked`, `cancelled`
- `summary`
- `output`
- `changed_files`
- `artifacts`
- `tests`
- `usage`
- `estimated_cost`
- `retryable`
- `block_reason`
- `sandbox`
- `evidence`
- `confidence`

`block_reason` is required when `status` is `blocked`. Task-level `failed` or `blocked` statuses are not process failures; orchestrators must read the result status.

## Events

`HarnessEvent` is a newline-delimited JSON object for event streams.

Required fields:

- `protocol`
- `mission_id`
- `task_id`
- `event_id`
- `sequence`
- `timestamp`
- `kind`
- `payload`

`kind` values use dotted names such as `task.started`, `tool.completed`, and `task.completed`. Inline payloads are bounded. Large tool or command output must be represented as an artifact reference.

## Stdout And Stderr

In `run-task --output-format json`:

- stdout contains exactly one JSON result object
- stdout contains no TUI, ANSI, spinner, progress, or log lines
- stderr may contain process diagnostics or newline-delimited JSON events
- human progress output belongs to prompt/TUI modes, not task mode stdout

## Exit Codes

- `0`: transport succeeded and a task result was produced, including task-level `failed` or `blocked`
- `1`: runtime or process error such as unreadable input, invalid JSON, or unsupported output format
- `2`: contract validation error such as a missing objective or workdir

## Cancellation

Process cancellation may produce no result if the process is terminated externally. When cancellation is handled in-process, task mode returns a `cancelled` result and exits `0`.

## Compatibility Notes

`ninmu`, OpenCode adapters, and Pi-style workers should consume the shared Rust contract structs rather than copying task-mode JSON shapes. RPC and worker mode should converge on these request, result, and event objects as they mature.

## Manual Smoke

From `rust/`:

```bash
cargo build --workspace
cargo run -p ninmu-cli -- run-task --input ../examples/harness/task-request.json --output-format json
```

The command must print one `HarnessTaskResult` JSON object on stdout. A task-level `failed` or `blocked` result is still a successful harness exchange; invalid input or a runtime crash is a process failure.
