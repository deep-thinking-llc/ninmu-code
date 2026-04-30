use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fs;
use std::io::{self, IsTerminal, Read, Write};
use std::net::TcpListener;
use std::ops::{Deref, DerefMut};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::OnceLock;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, UNIX_EPOCH};

use ninmu_api::{
    detect_provider_kind, resolve_startup_auth_source, AnthropicClient, ApiClientPool, AuthSource,
    ClientKey, ContentBlockDelta, InputContentBlock, InputMessage, MessageRequest, MessageResponse,
    OutputContentBlock, PromptCache, ProviderClient as ApiProviderClient, ProviderKind,
    StreamEvent as ApiStreamEvent, ToolChoice, ToolDefinition, ToolResultContentBlock,
};

use crate::cli_commands::{
    format_bughunter_report, format_issue_report, format_pr_report, format_ultraplan_report,
    git_output, render_config_report, render_diff_report, render_doctor_report, render_export_text,
    render_last_tool_debug_report, render_memory_report, render_teleport_report,
    resolve_export_path, run_init, run_mcp_serve, validate_no_args,
};
use crate::init::initialize_repo;
use crate::input;
use crate::render::{MarkdownStreamState, Spinner, TerminalRenderer};
use ninmu_commands::{
    classify_skills_slash_command, handle_agents_slash_command, handle_agents_slash_command_json,
    handle_mcp_slash_command, handle_mcp_slash_command_json, handle_plugins_slash_command,
    handle_skills_slash_command, handle_skills_slash_command_json, render_slash_command_help,
    render_slash_command_help_filtered, resolve_skill_invocation, resume_supported_slash_commands,
    slash_command_specs, validate_slash_command_input, SkillSlashDispatch, SlashCommand,
};
use ninmu_compat_harness::{extract_manifest, UpstreamPaths};
use ninmu_plugins::{PluginHooks, PluginManager, PluginManagerConfig, PluginRegistry};
use ninmu_runtime::{
    check_base_commit, format_stale_base_warning, format_usd, load_oauth_credentials,
    load_system_prompt, pricing_for_model, resolve_expected_base, resolve_sandbox_status,
    ApiClient, ApiRequest, AssistantEvent, CompactionConfig, ConfigLoader, ConfigSource,
    ContentBlock, ConversationMessage, ConversationRuntime, McpServer, McpServerManager,
    McpServerSpec, McpTool, MessageRole, ModelPricing, PermissionMode, PermissionPolicy,
    ProjectContext, PromptCacheEvent, ResolvedPermissionMode, RuntimeError, Session, TokenUsage,
    ToolError, ToolExecutor, UsageTracker,
};
use ninmu_tools::{
    execute_tool, mvp_tool_specs, GlobalToolRegistry, RuntimeToolDefinition, ToolSearchOutput,
};
use serde::Deserialize;
use serde_json::{json, Map, Value};

use crate::args::{enforce_broad_cwd_policy, try_resolve_bare_skill_prompt, CliOutputFormat};
use crate::format::{
    collect_session_prompt_history, confirm_session_deletion, create_managed_session_handle,
    delete_managed_session, extract_tool_path, filter_tool_specs, first_visible_line,
    format_auto_compaction_notice, format_commit_preflight_report, format_commit_skipped_report,
    format_compact_report, format_connected_line, format_cost_report, format_model_report,
    format_model_switch_report, format_permissions_report, format_permissions_switch_report,
    format_resume_report, format_sandbox_report, format_status_report, format_tool_call_start,
    format_tool_result, format_unknown_slash_command, format_user_visible_api_error,
    list_managed_sessions, load_session_reference, max_tokens_for_model, new_cli_session,
    normalize_permission_mode, parse_git_status_branch, parse_git_workspace_summary,
    parse_history_count, permission_mode_from_label, render_prompt_history_report,
    render_repl_help, render_resume_usage, render_session_list, resolve_git_branch_for,
    resolve_model_alias_with_config, resolve_repl_model, resolve_session_reference,
    slash_command_completion_candidates_with_sessions, status_context, summarize_tool_payload,
    truncate_for_summary, PromptHistoryEntry, SessionHandle, StatusUsage,
};
use crate::tui::permission::{
    format_enhanced_permission_prompt, parse_permission_response, PermissionDecision,
};
use crate::tui::status_bar::{StatusBar, StatusBarState};
use crate::tui::terminal::TerminalSize;
use crate::tui::theme::Theme;
use crate::tui::timeline::{SharedToolCallTimeline, ToolCallTimeline};
use crate::{
    AllowedToolSet, RuntimePluginStateBuildOutput, DEFAULT_DATE,
    INTERNAL_PROGRESS_HEARTBEAT_INTERVAL, POST_TOOL_STALL_TIMEOUT, STREAM_IDLE_TIMEOUT,
};

/// Print content using the internal pager if it exceeds terminal height.
/// Falls back to plain `println!` if paging is not needed or terminal is not interactive.
fn print_with_pager(content: &str) -> std::io::Result<bool> {
    let height = crate::tui::terminal::TerminalSize::new().height() as usize;
    let page_size = height.saturating_sub(2);
    let total_lines = content.lines().count();

    if total_lines <= page_size || !std::io::stdout().is_terminal() {
        println!("{content}");
        return Ok(false);
    }

    let pager = crate::tui::pager::InternalPager::new();
    pager.run(content)
}

// ═══════════════════════════════════════════════════════════════════════════
// LiveCli
// ═══════════════════════════════════════════════════════════════════════════

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BannerStyle {
    Full,
    Compact,
    None,
}

impl BannerStyle {
    pub(crate) fn from_config(value: Option<&str>) -> Self {
        match value {
            Some("full") => BannerStyle::Full,
            Some("none") => BannerStyle::None,
            _ => BannerStyle::Compact,
        }
    }
}

pub(crate) struct LiveCli {
    pub(crate) model: String,
    allowed_tools: Option<AllowedToolSet>,
    permission_mode: PermissionMode,
    banner_style: BannerStyle,
    system_prompt: Vec<String>,
    runtime: BuiltRuntime,
    session: SessionHandle,
    prompt_history: Vec<PromptHistoryEntry>,
    /// Cached plugin state so TUI turns don't reload config / re-init plugins.
    runtime_plugin_state: RuntimePluginState,
    /// Process-wide pool of shareable Anthropic API clients.
    client_pool: ApiClientPool,
}

/// Compute a [`ClientKey`] from the current model + session and retrieve (or
/// create) a shareable [`AnthropicClient`] from the pool.
fn get_pooled_client(
    pool: &ApiClientPool,
    model: &str,
    session_id: &str,
) -> Option<Arc<AnthropicClient>> {
    let resolved = ninmu_api::resolve_model_alias(model);
    let provider = detect_provider_kind(&resolved);
    if provider != ProviderKind::Anthropic {
        return None;
    }
    let auth = resolve_cli_auth_source().ok()?;
    let auth_hash = {
        let mut s = std::collections::hash_map::DefaultHasher::new();
        std::hash::Hash::hash(&format!("{auth:?}"), &mut s);
        std::hash::Hasher::finish(&s)
    };
    let scope = session_id.to_string();
    let key = ClientKey::new(provider, auth_hash, scope.clone());
    Some(pool.get_or_create(key, move || {
        AnthropicClient::from_auth(auth)
            .with_base_url(ninmu_api::read_base_url())
            .with_prompt_cache(PromptCache::new(&scope))
    }))
}

impl LiveCli {
    pub(crate) fn new(
        model: String,
        enable_tools: bool,
        allowed_tools: Option<AllowedToolSet>,
        permission_mode: PermissionMode,
        banner_style: Option<BannerStyle>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let system_prompt = build_system_prompt()?;
        let session_state = new_cli_session()?;
        let session = create_managed_session_handle(&session_state.session_id)?;
        let runtime_plugin_state = build_runtime_plugin_state()?;
        let client_pool = ApiClientPool::new();
        let shared_client = get_pooled_client(&client_pool, &model, &session.id);
        let runtime = build_runtime_with_plugin_state(
            session_state.with_persistence_path(session.path.clone()),
            &session.id,
            model.clone(),
            system_prompt.clone(),
            enable_tools,
            true,
            allowed_tools.clone(),
            permission_mode,
            None,
            runtime_plugin_state.clone(),
            shared_client,
        )?;
        let cli = Self {
            model,
            allowed_tools,
            permission_mode,
            banner_style: banner_style.unwrap_or(BannerStyle::Compact),
            system_prompt,
            runtime,
            session,
            prompt_history: Vec::new(),
            runtime_plugin_state,
            client_pool,
        };
        cli.persist_session()?;
        Ok(cli)
    }

    pub(crate) fn set_reasoning_effort(&mut self, effort: Option<String>) {
        if let Some(rt) = self.runtime.runtime.as_mut() {
            rt.api_client_mut().set_reasoning_effort(effort);
        }
    }

    pub(crate) fn set_thinking_mode(&mut self, mode: Option<bool>) {
        if let Some(rt) = self.runtime.runtime.as_mut() {
            rt.api_client_mut().set_thinking_mode(mode);
        }
    }

    pub(crate) fn startup_banner(&self) -> String {
        match self.banner_style {
            BannerStyle::Full => self.full_banner(),
            BannerStyle::Compact => self.compact_banner(),
            BannerStyle::None => String::new(),
        }
    }

    fn full_banner(&self) -> String {
        let cwd = env::current_dir().map_or_else(
            |_| "<unknown>".to_string(),
            |path| path.display().to_string(),
        );
        let status = status_context(None).ok();
        let git_branch = status
            .as_ref()
            .and_then(|context| context.git_branch.as_deref())
            .unwrap_or("unknown");
        let workspace = status.as_ref().map_or_else(
            || "unknown".to_string(),
            |context| context.git_summary.headline(),
        );
        let session_path = self.session.path.strip_prefix(Path::new(&cwd)).map_or_else(
            |_| self.session.path.display().to_string(),
            |path| path.display().to_string(),
        );
        format!(
            "{accent}ninmu{reset} {muted}ニンムコード{reset}\n\
             {muted}  model      {reset} {model}\n\
             {muted}  perm       {reset} {perm}\n\
             {muted}  branch     {reset} {branch}\n\
             {muted}  workspace  {reset} {workspace}\n\
             {muted}  directory  {reset} {cwd}\n\
             {muted}  session    {reset} {session_id}\n\
             {muted}  auto-save  {reset} {session_path}\n\n\
             {muted}/help{reset} · {muted}/diff{reset} {muted}/commit{reset} · {muted}Tab{reset}",
            accent = Theme::ACCENT,
            muted = Theme::MUTED,
            reset = Theme::RESET,
            model = self.model,
            perm = self.permission_mode.as_str(),
            branch = git_branch,
            workspace = workspace,
            cwd = cwd,
            session_id = self.session.id,
            session_path = session_path,
        )
    }

    fn compact_banner(&self) -> String {
        let cwd = env::current_dir().map_or_else(
            |_| "<unknown>".to_string(),
            |path| path.display().to_string(),
        );
        let status = status_context(None).ok();
        let git_branch = status
            .as_ref()
            .and_then(|context| context.git_branch.as_deref())
            .unwrap_or("unknown");
        format!(
            "{accent}ninmu{reset} {muted}ニンムコード{reset}  \
             {muted}model{reset} {model}  \
             {muted}perm{reset} {perm}  \
             {muted}branch{reset} {branch}\n\
             {muted}{cwd}/{reset}  \
             {muted}/help{reset} · {muted}/diff{reset} {muted}/commit{reset} · {muted}Tab{reset}",
            accent = Theme::ACCENT,
            muted = Theme::MUTED,
            reset = Theme::RESET,
            model = self.model,
            perm = self.permission_mode.as_str(),
            branch = git_branch,
            cwd = cwd,
        )
    }

    pub(crate) fn repl_completion_candidates(
        &self,
    ) -> Result<Vec<String>, Box<dyn std::error::Error>> {
        Ok(slash_command_completion_candidates_with_sessions(
            &self.model,
            Some(&self.session.id),
            list_managed_sessions()?
                .into_iter()
                .map(|session| session.id)
                .collect(),
        ))
    }

    fn prepare_turn_runtime(
        &self,
        emit_output: bool,
    ) -> Result<(BuiltRuntime, HookAbortMonitor), Box<dyn std::error::Error>> {
        let hook_abort_signal = ninmu_runtime::HookAbortSignal::new();
        let shared_client = get_pooled_client(&self.client_pool, &self.model, &self.session.id);
        let runtime = build_runtime_with_plugin_state(
            self.runtime.session().clone(),
            &self.session.id,
            self.model.clone(),
            self.system_prompt.clone(),
            true,
            emit_output,
            self.allowed_tools.clone(),
            self.permission_mode,
            None,
            self.runtime_plugin_state.clone(),
            shared_client,
        )?
        .with_hook_abort_signal(hook_abort_signal.clone());
        let hook_abort_monitor = HookAbortMonitor::spawn(hook_abort_signal);

        Ok((runtime, hook_abort_monitor))
    }

    fn replace_runtime(&mut self, runtime: BuiltRuntime) -> Result<(), Box<dyn std::error::Error>> {
        self.runtime.shutdown_plugins()?;
        self.runtime = runtime;
        Ok(())
    }

    pub(crate) fn run_turn(&mut self, input: &str) -> Result<(), Box<dyn std::error::Error>> {
        // Expand @file references
        let expansion = crate::file_ref::expand_file_refs(input);
        if !expansion.resolved.is_empty() {
            for path in &expansion.resolved {
                println!("{}-- attached {}{}", Theme::ACCENT, path, Theme::RESET);
            }
        }
        if !expansion.failed.is_empty() {
            for (path, err) in &expansion.failed {
                eprintln!(
                    "{}-- could not read {}: {}{}",
                    Theme::ERROR,
                    path,
                    err,
                    Theme::RESET
                );
            }
        }
        let input = expansion.expanded;

        let (mut runtime, hook_abort_monitor) = self.prepare_turn_runtime(true)?;
        let mut spinner = Spinner::new();
        let mut stdout = io::stdout();
        spinner.tick(
            "-- processing",
            TerminalRenderer::new().color_theme(),
            &mut stdout,
        )?;
        let mut permission_prompter = CliPermissionPrompter::new(self.permission_mode);
        let result = runtime.run_turn(input, Some(&mut permission_prompter));
        hook_abort_monitor.stop();
        match result {
            Ok(summary) => {
                self.replace_runtime(runtime)?;
                spinner.finish(
                    "-- done",
                    TerminalRenderer::new().color_theme(),
                    &mut stdout,
                )?;
                println!();
                if let Some(event) = summary.auto_compaction {
                    println!(
                        "{}",
                        format_auto_compaction_notice(event.removed_message_count)
                    );
                }
                self.persist_session()?;
                Ok(())
            }
            Err(error) => {
                runtime.shutdown_plugins()?;
                spinner.fail(
                    "-- failed",
                    TerminalRenderer::new().color_theme(),
                    &mut stdout,
                )?;
                Err(Box::new(error))
            }
        }
    }

    pub(crate) fn run_turn_with_output(
        &mut self,
        input: &str,
        output_format: CliOutputFormat,
        compact: bool,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let expansion = crate::file_ref::expand_file_refs(input);
        let input = expansion.expanded;
        match output_format {
            CliOutputFormat::Json if compact => self.run_prompt_compact_json(&input),
            CliOutputFormat::Text if compact => self.run_prompt_compact(&input),
            CliOutputFormat::Text => self.run_turn(&input),
            CliOutputFormat::Json => self.run_prompt_json(&input),
        }
    }

    /// Run a turn without printing to stdout. Returns the assistant's
    /// final text response. Used by the TUI which manages its own screen.
    pub(crate) fn run_turn_text(
        &mut self,
        input: &str,
    ) -> Result<String, Box<dyn std::error::Error>> {
        let expansion = crate::file_ref::expand_file_refs(input);
        let input = expansion.expanded;
        let (mut runtime, hook_abort_monitor) = self.prepare_turn_runtime(false)?;
        let mut permission_prompter = CliPermissionPrompter::new(self.permission_mode);
        let result = runtime.run_turn(input, Some(&mut permission_prompter));
        hook_abort_monitor.stop();
        let summary = result?;
        let text = final_assistant_text(&summary);
        self.replace_runtime(runtime)?;
        self.persist_session()?;
        let mut prefix = String::new();
        if let Some(event) = summary.prompt_cache_events.first() {
            prefix.push_str(&format_cache_break_warning(event));
            prefix.push('\n');
        }
        if let Some(event) = summary.auto_compaction {
            let notice = format_auto_compaction_notice(event.removed_message_count);
            prefix.push_str(&notice);
            prefix.push('\n');
        }
        if prefix.is_empty() {
            Ok(text)
        } else {
            Ok(format!("{prefix}{text}"))
        }
    }

    /// Run a turn in TUI mode. Returns a receiver for streaming events
    /// that the ratatui loop consumes on the main thread.
    pub(crate) fn run_turn_tui_channels(
        &mut self,
        input: &str,
    ) -> Result<
        (
            std::sync::mpsc::Receiver<crate::tui::TuiEvent>,
            std::thread::JoinHandle<Result<String, Box<dyn std::error::Error + Send>>>,
        ),
        Box<dyn std::error::Error>,
    > {
        let expansion = crate::file_ref::expand_file_refs(input);
        let input = expansion.expanded;

        let (bridge, rx) = crate::tui::TuiEventBridge::new();
        let bridge2 = bridge.clone();

        // Snapshot everything needed to build a runtime on the worker thread.
        let model = self.model.clone();
        let allowed_tools = self.allowed_tools.clone();
        let permission_mode = self.permission_mode;
        let system_prompt = self.system_prompt.clone();
        let session = self.runtime.session().clone();
        let session_id = self.session.id.clone();
        let runtime_plugin_state = self.runtime_plugin_state.clone();
        // Propagate reasoning/thinking settings to the worker thread's client.
        let reasoning_effort = self
            .runtime
            .runtime
            .as_ref()
            .and_then(|rt| rt.api_client().reasoning_effort.clone());
        let thinking_mode = self
            .runtime
            .runtime
            .as_ref()
            .and_then(|rt| rt.api_client().thinking_mode);
        // Retrieve (or create) the shared AnthropicClient before spawning so the
        // worker thread reuses the same pooled instance and its prompt cache.
        let shared_client = get_pooled_client(&self.client_pool, &self.model, &self.session.id);

        let handle = std::thread::spawn(move || {
            let hook_abort_signal = ninmu_runtime::HookAbortSignal::new();
            let hook_abort_monitor = HookAbortMonitor::spawn(hook_abort_signal.clone());

            let mut runtime = build_runtime_with_plugin_state(
                session,
                &session_id,
                model.clone(),
                system_prompt.clone(),
                true,
                false,
                allowed_tools.clone(),
                permission_mode,
                None,
                runtime_plugin_state,
                shared_client,
            )
            .map_err(|e| {
                let msg = e.to_string();
                Box::new(std::io::Error::other(msg)) as Box<dyn std::error::Error + Send>
            })?
            .with_hook_abort_signal(hook_abort_signal);

            // Inject bridge into the API client and tool executor.
            if let Some(r) = runtime.runtime.as_mut() {
                r.api_client_mut().event_bridge = Some(bridge2.clone());
                if let Some(effort) = reasoning_effort {
                    r.api_client_mut().set_reasoning_effort(Some(effort));
                }
                if let Some(mode) = thinking_mode {
                    r.api_client_mut().set_thinking_mode(Some(mode));
                }
                r.tool_executor_mut().event_bridge = Some(bridge2.clone());
            }

            let mut permission_prompter = TuiPermissionPrompter::new(bridge2.clone());
            let result = runtime.run_turn(input, Some(&mut permission_prompter));
            hook_abort_monitor.stop();
            let summary = match result {
                Ok(s) => s,
                Err(e) => {
                    bridge2.error(e.to_string());
                    bridge2.turn_complete();
                    return Err(Box::new(e) as Box<dyn std::error::Error + Send>);
                }
            };
            for event in &summary.prompt_cache_events {
                bridge2.prompt_cache(event.clone());
            }
            bridge2.turn_complete();
            Ok(final_assistant_text(&summary))
        });

        Ok((rx, handle))
    }

    fn run_prompt_compact(&mut self, input: &str) -> Result<(), Box<dyn std::error::Error>> {
        let (mut runtime, hook_abort_monitor) = self.prepare_turn_runtime(false)?;
        let mut permission_prompter = CliPermissionPrompter::new(self.permission_mode);
        let result = runtime.run_turn(input, Some(&mut permission_prompter));
        hook_abort_monitor.stop();
        let summary = result?;
        self.replace_runtime(runtime)?;
        self.persist_session()?;
        if let Some(event) = summary.prompt_cache_events.first() {
            eprintln!("{}", format_cache_break_warning(event));
        }
        let final_text = final_assistant_text(&summary);
        println!("{final_text}");
        Ok(())
    }

    fn run_prompt_compact_json(&mut self, input: &str) -> Result<(), Box<dyn std::error::Error>> {
        let (mut runtime, hook_abort_monitor) = self.prepare_turn_runtime(false)?;
        let mut permission_prompter = CliPermissionPrompter::new(self.permission_mode);
        let result = runtime.run_turn(input, Some(&mut permission_prompter));
        hook_abort_monitor.stop();
        let summary = result?;
        self.replace_runtime(runtime)?;
        self.persist_session()?;
        println!(
            "{}",
            json!({
                "message": final_assistant_text(&summary),
                "compact": true,
                "model": self.model,
                "usage": {
                    "input_tokens": summary.usage.input_tokens,
                    "output_tokens": summary.usage.output_tokens,
                    "cache_creation_input_tokens": summary.usage.cache_creation_input_tokens,
                    "cache_read_input_tokens": summary.usage.cache_read_input_tokens,
                },
            })
        );
        Ok(())
    }

    fn run_prompt_json(&mut self, input: &str) -> Result<(), Box<dyn std::error::Error>> {
        let (mut runtime, hook_abort_monitor) = self.prepare_turn_runtime(false)?;
        let mut permission_prompter = CliPermissionPrompter::new(self.permission_mode);
        let result = runtime.run_turn(input, Some(&mut permission_prompter));
        hook_abort_monitor.stop();
        let summary = result?;
        self.replace_runtime(runtime)?;
        self.persist_session()?;
        println!(
            "{}",
            json!({
                "message": final_assistant_text(&summary),
                "model": self.model,
                "iterations": summary.iterations,
                "auto_compaction": summary.auto_compaction.map(|event| json!({
                    "removed_messages": event.removed_message_count,
                    "notice": format_auto_compaction_notice(event.removed_message_count),
                })),
                "tool_uses": collect_tool_uses(&summary),
                "tool_results": collect_tool_results(&summary),
                "prompt_cache_events": collect_prompt_cache_events(&summary),
                "usage": {
                    "input_tokens": summary.usage.input_tokens,
                    "output_tokens": summary.usage.output_tokens,
                    "cache_creation_input_tokens": summary.usage.cache_creation_input_tokens,
                    "cache_read_input_tokens": summary.usage.cache_read_input_tokens,
                },
                "estimated_cost": format_usd(
                    summary.usage.estimate_cost_usd_with_pricing(
                        pricing_for_model(&self.model)
                            .unwrap_or_else(ninmu_runtime::ModelPricing::default_sonnet_tier)
                    ).total_cost_usd()
                )
            })
        );
        Ok(())
    }

    #[allow(clippy::too_many_lines)]
    pub(crate) fn handle_repl_command(
        &mut self,
        command: SlashCommand,
    ) -> Result<bool, Box<dyn std::error::Error>> {
        Ok(match command {
            SlashCommand::Help => {
                println!("{}", render_repl_help());
                false
            }
            SlashCommand::Status => {
                self.print_status();
                false
            }
            SlashCommand::Bughunter { scope } => {
                self.run_bughunter(scope.as_deref())?;
                false
            }
            SlashCommand::Commit => {
                self.run_commit(None)?;
                false
            }
            SlashCommand::Pr { context } => {
                self.run_pr(context.as_deref())?;
                false
            }
            SlashCommand::Issue { context } => {
                self.run_issue(context.as_deref())?;
                false
            }
            SlashCommand::Ultraplan { task } => {
                self.run_ultraplan(task.as_deref())?;
                false
            }
            SlashCommand::Teleport { target } => {
                Self::run_teleport(target.as_deref())?;
                false
            }
            SlashCommand::DebugToolCall => {
                self.run_debug_tool_call(None)?;
                false
            }
            SlashCommand::Sandbox => {
                Self::print_sandbox_status();
                false
            }
            SlashCommand::Compact => {
                self.compact()?;
                false
            }
            SlashCommand::Model { model } => self.set_model(model)?,
            SlashCommand::Permissions { mode } => self.set_permissions(mode)?,
            SlashCommand::Clear { confirm } => self.clear_session(confirm)?,
            SlashCommand::Cost => {
                self.print_cost();
                false
            }
            SlashCommand::Resume { session_path } => self.resume_session(session_path)?,
            SlashCommand::Config { section } => {
                Self::print_config(section.as_deref())?;
                false
            }
            SlashCommand::Mcp { action, target } => {
                let args = match (action.as_deref(), target.as_deref()) {
                    (None, None) => None,
                    (Some(action), None) => Some(action.to_string()),
                    (Some(action), Some(target)) => Some(format!("{action} {target}")),
                    (None, Some(target)) => Some(target.to_string()),
                };
                Self::print_mcp(args.as_deref(), CliOutputFormat::Text)?;
                false
            }
            SlashCommand::Memory => {
                Self::print_memory()?;
                false
            }
            SlashCommand::Init => {
                run_init(CliOutputFormat::Text)?;
                false
            }
            SlashCommand::Diff => {
                Self::print_diff()?;
                false
            }
            SlashCommand::Version => {
                Self::print_version(CliOutputFormat::Text);
                false
            }
            SlashCommand::Export { path } => {
                self.export_session(path.as_deref())?;
                false
            }
            SlashCommand::Session { action, target } => {
                self.handle_session_command(action.as_deref(), target.as_deref())?
            }
            SlashCommand::Plugins { action, target } => {
                self.handle_plugins_command(action.as_deref(), target.as_deref())?
            }
            SlashCommand::Agents { args } => {
                Self::print_agents(args.as_deref(), CliOutputFormat::Text)?;
                false
            }
            SlashCommand::Skills { args } => {
                match classify_skills_slash_command(args.as_deref()) {
                    SkillSlashDispatch::Invoke(prompt) => self.run_turn(&prompt)?,
                    SkillSlashDispatch::Local => {
                        Self::print_skills(args.as_deref(), CliOutputFormat::Text)?;
                    }
                }
                false
            }
            SlashCommand::Doctor => {
                println!("{}", render_doctor_report()?.render());
                false
            }
            SlashCommand::History { count } => {
                self.print_prompt_history(count.as_deref());
                false
            }
            SlashCommand::Stats => {
                let usage = UsageTracker::from_session(self.runtime.session()).cumulative_usage();
                println!("{}", format_cost_report(usage));
                false
            }
            SlashCommand::Effort { level } => {
                match level.as_deref() {
                    Some("low" | "medium" | "high" | "max") => {
                        self.set_reasoning_effort(level.clone());
                        println!("reasoning effort set to {}", level.as_deref().unwrap());
                    }
                    Some("off") | None => {
                        self.set_reasoning_effort(None);
                        println!("reasoning effort reset to default");
                    }
                    Some(other) => {
                        println!("unknown effort level: {other}");
                        println!("valid levels: low, medium, high, max, off");
                    }
                }
                false
            }
            SlashCommand::Think { mode } => {
                match mode.as_deref() {
                    Some("on" | "enable") => {
                        self.set_thinking_mode(Some(true));
                        println!("thinking mode enabled");
                    }
                    Some("off" | "disable") => {
                        self.set_thinking_mode(Some(false));
                        println!("thinking mode disabled");
                    }
                    Some("auto") | None => {
                        self.set_thinking_mode(None);
                        println!("thinking mode set to auto (model default)");
                    }
                    Some(other) => {
                        println!("unknown thinking mode: {other}");
                        println!("valid modes: on, off, auto");
                    }
                }
                false
            }
            SlashCommand::Login
            | SlashCommand::Logout
            | SlashCommand::Vim
            | SlashCommand::Upgrade
            | SlashCommand::Share
            | SlashCommand::Feedback
            | SlashCommand::Files
            | SlashCommand::Fast
            | SlashCommand::Exit
            | SlashCommand::Summary
            | SlashCommand::Desktop
            | SlashCommand::Brief
            | SlashCommand::Advisor
            | SlashCommand::Stickers
            | SlashCommand::Insights
            | SlashCommand::Thinkback
            | SlashCommand::ReleaseNotes
            | SlashCommand::SecurityReview
            | SlashCommand::Keybindings
            | SlashCommand::PrivacySettings
            | SlashCommand::Plan { .. }
            | SlashCommand::Review { .. }
            | SlashCommand::Tasks { .. }
            | SlashCommand::Theme { .. }
            | SlashCommand::Voice { .. }
            | SlashCommand::Usage { .. }
            | SlashCommand::Rename { .. }
            | SlashCommand::Copy { .. }
            | SlashCommand::Hooks { .. }
            | SlashCommand::Context { .. }
            | SlashCommand::Color { .. }
            | SlashCommand::Branch { .. }
            | SlashCommand::Rewind { .. }
            | SlashCommand::Ide { .. }
            | SlashCommand::Tag { .. }
            | SlashCommand::OutputStyle { .. }
            | SlashCommand::AddDir { .. } => {
                let cmd_name = command.slash_name();
                eprintln!("{cmd_name} is not yet implemented in this build.");
                false
            }
            SlashCommand::Unknown(name) => {
                eprintln!("{}", format_unknown_slash_command(&name));
                false
            }
        })
    }

    pub(crate) fn persist_session(&self) -> Result<(), Box<dyn std::error::Error>> {
        self.runtime.session().save_to_path(&self.session.path)?;
        Ok(())
    }

    fn print_status(&self) {
        let cumulative = self.runtime.usage().cumulative_usage();
        let latest = self.runtime.usage().current_turn_usage();
        let report = format_status_report(
            &self.model,
            StatusUsage {
                message_count: self.runtime.session().messages.len(),
                turns: self.runtime.usage().turns(),
                latest,
                cumulative,
                estimated_tokens: self.runtime.estimated_tokens(),
            },
            self.permission_mode.as_str(),
            &status_context(Some(&self.session.path)).expect("status context should load"),
            None,
        );
        let _ = print_with_pager(&report);
    }

    pub(crate) fn record_prompt_history(&mut self, prompt: &str) {
        let timestamp_ms = std::time::SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()
            .map_or(self.runtime.session().updated_at_ms, |duration| {
                u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
            });
        let entry = PromptHistoryEntry {
            timestamp_ms,
            text: prompt.to_string(),
        };
        self.prompt_history.push(entry);
        if let Err(error) = self.runtime.session_mut().push_prompt_entry(prompt) {
            eprintln!("warning: failed to persist prompt history: {error}");
        }
    }

    fn print_prompt_history(&self, count: Option<&str>) {
        let limit = match parse_history_count(count) {
            Ok(limit) => limit,
            Err(message) => {
                eprintln!("{message}");
                return;
            }
        };
        let session_entries = &self.runtime.session().prompt_history;
        let entries = if session_entries.is_empty() {
            if self.prompt_history.is_empty() {
                collect_session_prompt_history(self.runtime.session())
            } else {
                self.prompt_history
                    .iter()
                    .map(|entry| PromptHistoryEntry {
                        timestamp_ms: entry.timestamp_ms,
                        text: entry.text.clone(),
                    })
                    .collect()
            }
        } else {
            session_entries
                .iter()
                .map(|entry| PromptHistoryEntry {
                    timestamp_ms: entry.timestamp_ms,
                    text: entry.text.clone(),
                })
                .collect()
        };
        println!("{}", render_prompt_history_report(&entries, limit));
    }

    pub(crate) fn print_sandbox_status() {
        let cwd = env::current_dir().expect("current dir");
        let loader = ConfigLoader::default_for(&cwd);
        let runtime_config = loader
            .load()
            .unwrap_or_else(|_| ninmu_runtime::RuntimeConfig::empty());
        println!(
            "{}",
            format_sandbox_report(&resolve_sandbox_status(runtime_config.sandbox(), &cwd))
        );
    }

    fn set_model(&mut self, model: Option<String>) -> Result<bool, Box<dyn std::error::Error>> {
        let Some(model) = model else {
            println!(
                "{}",
                format_model_report(
                    &self.model,
                    self.runtime.session().messages.len(),
                    self.runtime.usage().turns(),
                )
            );
            return Ok(false);
        };

        let model = resolve_model_alias_with_config(&model);

        if model == self.model {
            println!(
                "{}",
                format_model_report(
                    &self.model,
                    self.runtime.session().messages.len(),
                    self.runtime.usage().turns(),
                )
            );
            return Ok(false);
        }

        let previous = self.model.clone();
        let session = self.runtime.session().clone();
        let message_count = session.messages.len();
        let runtime = build_runtime(
            session,
            &self.session.id,
            model.clone(),
            self.system_prompt.clone(),
            true,
            true,
            self.allowed_tools.clone(),
            self.permission_mode,
            None,
        )?;
        self.replace_runtime(runtime)?;
        self.model.clone_from(&model);
        // Persist to user settings.json so it becomes the default on next launch.
        let _ = persist_model_to_settings(&model);
        println!(
            "{}",
            format_model_switch_report(&previous, &model, message_count)
        );
        Ok(true)
    }

    fn set_permissions(
        &mut self,
        mode: Option<String>,
    ) -> Result<bool, Box<dyn std::error::Error>> {
        let Some(mode) = mode else {
            println!(
                "{}",
                format_permissions_report(self.permission_mode.as_str())
            );
            return Ok(false);
        };

        let normalized = normalize_permission_mode(&mode).ok_or_else(|| {
            format!(
                "unsupported permission mode '{mode}'. Use read-only, workspace-write, or danger-full-access."
            )
        })?;

        if normalized == self.permission_mode.as_str() {
            println!("{}", format_permissions_report(normalized));
            return Ok(false);
        }

        let previous = self.permission_mode.as_str().to_string();
        let session = self.runtime.session().clone();
        self.permission_mode = permission_mode_from_label(normalized);
        let runtime = build_runtime(
            session,
            &self.session.id,
            self.model.clone(),
            self.system_prompt.clone(),
            true,
            true,
            self.allowed_tools.clone(),
            self.permission_mode,
            None,
        )?;
        self.replace_runtime(runtime)?;
        println!(
            "{}",
            format_permissions_switch_report(&previous, normalized)
        );
        Ok(true)
    }

    fn clear_session(&mut self, confirm: bool) -> Result<bool, Box<dyn std::error::Error>> {
        if !confirm {
            println!(
                "clear: confirmation required; run /clear --confirm to start a fresh session."
            );
            return Ok(false);
        }

        let previous_session = self.session.clone();
        let session_state = new_cli_session()?;
        self.session = create_managed_session_handle(&session_state.session_id)?;
        let runtime = build_runtime(
            session_state.with_persistence_path(self.session.path.clone()),
            &self.session.id,
            self.model.clone(),
            self.system_prompt.clone(),
            true,
            true,
            self.allowed_tools.clone(),
            self.permission_mode,
            None,
        )?;
        self.replace_runtime(runtime)?;
        println!(
            "Session cleared\n  Mode             fresh session\n  Previous session {}\n  Resume previous  /resume {}\n  Preserved model  {}\n  Permission mode  {}\n  New session      {}\n  Session file     {}",
            previous_session.id,
            previous_session.id,
            self.model,
            self.permission_mode.as_str(),
            self.session.id,
            self.session.path.display(),
        );
        Ok(true)
    }

    fn print_cost(&self) {
        let cumulative = self.runtime.usage().cumulative_usage();
        println!("{}", format_cost_report(cumulative));
    }

    fn resume_session(
        &mut self,
        session_path: Option<String>,
    ) -> Result<bool, Box<dyn std::error::Error>> {
        let Some(session_ref) = session_path else {
            println!("{}", render_resume_usage());
            return Ok(false);
        };

        let (handle, session) = load_session_reference(&session_ref)?;
        let message_count = session.messages.len();
        let session_id = session.session_id.clone();
        let shared_client = get_pooled_client(&self.client_pool, &self.model, &session_id);
        let runtime = build_runtime_with_plugin_state(
            session,
            &handle.id,
            self.model.clone(),
            self.system_prompt.clone(),
            true,
            true,
            self.allowed_tools.clone(),
            self.permission_mode,
            None,
            self.runtime_plugin_state.clone(),
            shared_client,
        )?;
        self.replace_runtime(runtime)?;
        self.session = SessionHandle {
            id: session_id,
            path: handle.path,
        };
        println!(
            "{}",
            format_resume_report(
                &self.session.path.display().to_string(),
                message_count,
                self.runtime.usage().turns(),
            )
        );
        Ok(true)
    }

    fn print_config(section: Option<&str>) -> Result<(), Box<dyn std::error::Error>> {
        let report = render_config_report(section)?;
        let _ = print_with_pager(&report);
        Ok(())
    }

    fn print_memory() -> Result<(), Box<dyn std::error::Error>> {
        let report = render_memory_report()?;
        let _ = print_with_pager(&report);
        Ok(())
    }

    pub(crate) fn print_agents(
        args: Option<&str>,
        output_format: CliOutputFormat,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let cwd = env::current_dir()?;
        match output_format {
            CliOutputFormat::Text => println!("{}", handle_agents_slash_command(args, &cwd)?),
            CliOutputFormat::Json => println!(
                "{}",
                serde_json::to_string_pretty(&handle_agents_slash_command_json(args, &cwd)?)?
            ),
        }
        Ok(())
    }

    pub(crate) fn print_mcp(
        args: Option<&str>,
        output_format: CliOutputFormat,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if matches!(args.map(str::trim), Some("serve")) {
            return run_mcp_serve();
        }
        let cwd = env::current_dir()?;
        match output_format {
            CliOutputFormat::Text => println!("{}", handle_mcp_slash_command(args, &cwd)?),
            CliOutputFormat::Json => println!(
                "{}",
                serde_json::to_string_pretty(&handle_mcp_slash_command_json(args, &cwd)?)?
            ),
        }
        Ok(())
    }

    pub(crate) fn print_skills(
        args: Option<&str>,
        output_format: CliOutputFormat,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let cwd = env::current_dir()?;
        match output_format {
            CliOutputFormat::Text => println!("{}", handle_skills_slash_command(args, &cwd)?),
            CliOutputFormat::Json => println!(
                "{}",
                serde_json::to_string_pretty(&handle_skills_slash_command_json(args, &cwd)?)?
            ),
        }
        Ok(())
    }

    pub(crate) fn print_plugins(
        action: Option<&str>,
        target: Option<&str>,
        output_format: CliOutputFormat,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let cwd = env::current_dir()?;
        let loader = ConfigLoader::default_for(&cwd);
        let runtime_config = loader.load()?;
        let mut manager = build_plugin_manager(&cwd, &loader, &runtime_config);
        let result = handle_plugins_slash_command(action, target, &mut manager)?;
        match output_format {
            CliOutputFormat::Text => println!("{}", result.message),
            CliOutputFormat::Json => println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "kind": "plugin",
                    "action": action.unwrap_or("list"),
                    "target": target,
                    "message": result.message,
                    "reload_runtime": result.reload_runtime,
                }))?
            ),
        }
        Ok(())
    }

    fn print_diff() -> Result<(), Box<dyn std::error::Error>> {
        let report = render_diff_report()?;
        let _ = print_with_pager(&report);
        Ok(())
    }

    fn print_version(output_format: CliOutputFormat) {
        let _ = crate::print_version(output_format);
    }

    fn export_session(
        &self,
        requested_path: Option<&str>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let export_path = resolve_export_path(requested_path, self.runtime.session())?;
        fs::write(&export_path, render_export_text(self.runtime.session()))?;
        println!(
            "Export\n  Result           wrote transcript\n  File             {}\n  Messages         {}",
            export_path.display(),
            self.runtime.session().messages.len(),
        );
        Ok(())
    }

    #[allow(clippy::too_many_lines)]
    fn handle_session_command(
        &mut self,
        action: Option<&str>,
        target: Option<&str>,
    ) -> Result<bool, Box<dyn std::error::Error>> {
        match action {
            None | Some("list") => {
                println!("{}", render_session_list(&self.session.id)?);
                Ok(false)
            }
            Some("switch") => {
                let Some(target) = target else {
                    println!("Usage: /session switch <session-id>");
                    return Ok(false);
                };
                let (handle, session) = load_session_reference(target)?;
                let message_count = session.messages.len();
                let session_id = session.session_id.clone();
                let runtime = build_runtime(
                    session,
                    &handle.id,
                    self.model.clone(),
                    self.system_prompt.clone(),
                    true,
                    true,
                    self.allowed_tools.clone(),
                    self.permission_mode,
                    None,
                )?;
                self.replace_runtime(runtime)?;
                self.session = SessionHandle {
                    id: session_id,
                    path: handle.path,
                };
                println!(
                    "Session switched\n  Active session   {}\n  File             {}\n  Messages         {}",
                    self.session.id,
                    self.session.path.display(),
                    message_count,
                );
                Ok(true)
            }
            Some("fork") => {
                let forked = self.runtime.fork_session(target.map(ToOwned::to_owned));
                let parent_session_id = self.session.id.clone();
                let handle = create_managed_session_handle(&forked.session_id)?;
                let branch_name = forked
                    .fork
                    .as_ref()
                    .and_then(|fork| fork.branch_name.clone());
                let forked = forked.with_persistence_path(handle.path.clone());
                let message_count = forked.messages.len();
                forked.save_to_path(&handle.path)?;
                let runtime = build_runtime(
                    forked,
                    &handle.id,
                    self.model.clone(),
                    self.system_prompt.clone(),
                    true,
                    true,
                    self.allowed_tools.clone(),
                    self.permission_mode,
                    None,
                )?;
                self.replace_runtime(runtime)?;
                self.session = handle;
                println!(
                    "Session forked\n  Parent session   {}\n  Active session   {}\n  Branch           {}\n  File             {}\n  Messages         {}",
                    parent_session_id,
                    self.session.id,
                    branch_name.as_deref().unwrap_or("(unnamed)"),
                    self.session.path.display(),
                    message_count,
                );
                Ok(true)
            }
            Some("delete") => {
                let Some(target) = target else {
                    println!("Usage: /session delete <session-id> [--force]");
                    return Ok(false);
                };
                let handle = resolve_session_reference(target)?;
                if handle.id == self.session.id {
                    println!(
                        "delete: refusing to delete the active session '{}'.\nSwitch to another session first with /session switch <session-id>.",
                        handle.id
                    );
                    return Ok(false);
                }
                if !confirm_session_deletion(&handle.id) {
                    println!("delete: cancelled.");
                    return Ok(false);
                }
                delete_managed_session(&handle.path)?;
                println!(
                    "Session deleted\n  Deleted session  {}\n  File             {}",
                    handle.id,
                    handle.path.display(),
                );
                Ok(false)
            }
            Some("delete-force") => {
                let Some(target) = target else {
                    println!("Usage: /session delete <session-id> [--force]");
                    return Ok(false);
                };
                let handle = resolve_session_reference(target)?;
                if handle.id == self.session.id {
                    println!(
                        "delete: refusing to delete the active session '{}'.\nSwitch to another session first with /session switch <session-id>.",
                        handle.id
                    );
                    return Ok(false);
                }
                delete_managed_session(&handle.path)?;
                println!(
                    "Session deleted\n  Deleted session  {}\n  File             {}",
                    handle.id,
                    handle.path.display(),
                );
                Ok(false)
            }
            Some(other) => {
                println!(
                    "Unknown /session action '{other}'. Use /session list, /session switch <session-id>, /session fork [branch-name], or /session delete <session-id> [--force]."
                );
                Ok(false)
            }
        }
    }

    fn handle_plugins_command(
        &mut self,
        action: Option<&str>,
        target: Option<&str>,
    ) -> Result<bool, Box<dyn std::error::Error>> {
        let cwd = env::current_dir()?;
        let loader = ConfigLoader::default_for(&cwd);
        let runtime_config = loader.load()?;
        let mut manager = build_plugin_manager(&cwd, &loader, &runtime_config);
        let result = handle_plugins_slash_command(action, target, &mut manager)?;
        println!("{}", result.message);
        if result.reload_runtime {
            self.reload_runtime_features()?;
        }
        Ok(false)
    }

    fn reload_runtime_features(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let runtime = build_runtime(
            self.runtime.session().clone(),
            &self.session.id,
            self.model.clone(),
            self.system_prompt.clone(),
            true,
            true,
            self.allowed_tools.clone(),
            self.permission_mode,
            None,
        )?;
        self.replace_runtime(runtime)?;
        self.persist_session()
    }

    fn compact(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let result = self.runtime.compact(CompactionConfig::default());
        let removed = result.removed_message_count;
        let kept = result.compacted_session.messages.len();
        let skipped = removed == 0;
        let runtime = build_runtime(
            result.compacted_session,
            &self.session.id,
            self.model.clone(),
            self.system_prompt.clone(),
            true,
            true,
            self.allowed_tools.clone(),
            self.permission_mode,
            None,
        )?;
        self.replace_runtime(runtime)?;
        self.persist_session()?;
        println!("{}", format_compact_report(removed, kept, skipped));
        Ok(())
    }

    pub(crate) fn run_internal_prompt_text_with_progress(
        &self,
        prompt: &str,
        enable_tools: bool,
        progress: Option<InternalPromptProgressReporter>,
    ) -> Result<String, Box<dyn std::error::Error>> {
        let session = self.runtime.session().clone();
        let mut runtime = build_runtime(
            session,
            &self.session.id,
            self.model.clone(),
            self.system_prompt.clone(),
            enable_tools,
            false,
            self.allowed_tools.clone(),
            self.permission_mode,
            progress,
        )?;
        let mut permission_prompter = CliPermissionPrompter::new(self.permission_mode);
        let summary = runtime.run_turn(prompt, Some(&mut permission_prompter))?;
        let text = final_assistant_text(&summary).trim().to_string();
        runtime.shutdown_plugins()?;
        Ok(text)
    }

    pub(crate) fn run_internal_prompt_text(
        &self,
        prompt: &str,
        enable_tools: bool,
    ) -> Result<String, Box<dyn std::error::Error>> {
        self.run_internal_prompt_text_with_progress(prompt, enable_tools, None)
    }

    fn run_bughunter(&self, scope: Option<&str>) -> Result<(), Box<dyn std::error::Error>> {
        println!("{}", format_bughunter_report(scope));
        Ok(())
    }

    fn run_ultraplan(&self, task: Option<&str>) -> Result<(), Box<dyn std::error::Error>> {
        println!("{}", format_ultraplan_report(task));
        Ok(())
    }

    fn run_teleport(target: Option<&str>) -> Result<(), Box<dyn std::error::Error>> {
        let Some(target) = target.map(str::trim).filter(|value| !value.is_empty()) else {
            println!("Usage: /teleport <symbol-or-path>");
            return Ok(());
        };

        println!("{}", render_teleport_report(target)?);
        Ok(())
    }

    fn run_debug_tool_call(&self, args: Option<&str>) -> Result<(), Box<dyn std::error::Error>> {
        validate_no_args("/debug-tool-call", args)?;
        println!("{}", render_last_tool_debug_report(self.runtime.session())?);
        Ok(())
    }

    fn run_commit(&mut self, args: Option<&str>) -> Result<(), Box<dyn std::error::Error>> {
        validate_no_args("/commit", args)?;
        let status = git_output(&["status", "--short", "--branch"])?;
        let summary = parse_git_workspace_summary(Some(&status));
        let branch = parse_git_status_branch(Some(&status));
        if summary.is_clean() {
            println!("{}", format_commit_skipped_report());
            return Ok(());
        }

        println!(
            "{}",
            format_commit_preflight_report(branch.as_deref(), summary)
        );
        Ok(())
    }

    fn run_pr(&self, context: Option<&str>) -> Result<(), Box<dyn std::error::Error>> {
        let branch =
            resolve_git_branch_for(&env::current_dir()?).unwrap_or_else(|| "unknown".to_string());
        println!("{}", format_pr_report(&branch, context));
        Ok(())
    }

    fn run_issue(&self, context: Option<&str>) -> Result<(), Box<dyn std::error::Error>> {
        println!("{}", format_issue_report(context));
        Ok(())
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// BuiltRuntime
// ═══════════════════════════════════════════════════════════════════════════

pub(crate) struct BuiltRuntime {
    pub(crate) runtime: Option<ConversationRuntime<AnthropicRuntimeClient, CliToolExecutor>>,
    plugin_registry: PluginRegistry,
    plugins_active: bool,
    pub(crate) mcp_state: Option<Arc<Mutex<RuntimeMcpState>>>,
    mcp_active: bool,
}

impl BuiltRuntime {
    fn new(
        runtime: ConversationRuntime<AnthropicRuntimeClient, CliToolExecutor>,
        plugin_registry: PluginRegistry,
        mcp_state: Option<Arc<Mutex<RuntimeMcpState>>>,
    ) -> Self {
        Self {
            runtime: Some(runtime),
            plugin_registry,
            plugins_active: true,
            mcp_state,
            mcp_active: true,
        }
    }

    fn with_hook_abort_signal(mut self, hook_abort_signal: ninmu_runtime::HookAbortSignal) -> Self {
        let runtime = self
            .runtime
            .take()
            .expect("runtime should exist before installing hook abort signal");
        self.runtime = Some(runtime.with_hook_abort_signal(hook_abort_signal));
        self
    }

    pub(crate) fn shutdown_plugins(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        if self.plugins_active {
            self.plugin_registry.shutdown()?;
            self.plugins_active = false;
        }
        Ok(())
    }

    fn shutdown_mcp(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        if self.mcp_active {
            if let Some(mcp_state) = &self.mcp_state {
                mcp_state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .shutdown()?;
            }
            self.mcp_active = false;
        }
        Ok(())
    }
}

impl Deref for BuiltRuntime {
    type Target = ConversationRuntime<AnthropicRuntimeClient, CliToolExecutor>;

    fn deref(&self) -> &Self::Target {
        self.runtime
            .as_ref()
            .expect("runtime should exist while built runtime is alive")
    }
}

impl DerefMut for BuiltRuntime {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.runtime
            .as_mut()
            .expect("runtime should exist while built runtime is alive")
    }
}

impl Drop for BuiltRuntime {
    fn drop(&mut self) {
        let _ = self.shutdown_mcp();
        let _ = self.shutdown_plugins();
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// ToolSearchRequest / McpToolRequest / Resource request structs
// ═══════════════════════════════════════════════════════════════════════════

#[derive(Debug, Deserialize)]
pub(crate) struct ToolSearchRequest {
    query: String,
    max_results: Option<usize>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct McpToolRequest {
    #[serde(rename = "qualifiedName")]
    qualified_name: Option<String>,
    tool: Option<String>,
    arguments: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ListMcpResourcesRequest {
    server: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ReadMcpResourceRequest {
    server: String,
    uri: String,
}

// ═══════════════════════════════════════════════════════════════════════════
// RuntimePluginState
// ═══════════════════════════════════════════════════════════════════════════

#[derive(Clone)]
pub(crate) struct RuntimePluginState {
    pub(crate) feature_config: ninmu_runtime::RuntimeFeatureConfig,
    pub(crate) tool_registry: GlobalToolRegistry,
    pub(crate) plugin_registry: PluginRegistry,
    pub(crate) mcp_state: Option<Arc<Mutex<RuntimeMcpState>>>,
}

// ═══════════════════════════════════════════════════════════════════════════
// RuntimeMcpState
// ═══════════════════════════════════════════════════════════════════════════

pub(crate) struct RuntimeMcpState {
    runtime: std::sync::Arc<tokio::runtime::Runtime>,
    manager: McpServerManager,
    pending_servers: Vec<String>,
    degraded_report: Option<ninmu_runtime::McpDegradedReport>,
}

impl RuntimeMcpState {
    fn new(
        runtime_config: &ninmu_runtime::RuntimeConfig,
    ) -> Result<Option<(Self, ninmu_runtime::McpToolDiscoveryReport)>, Box<dyn std::error::Error>>
    {
        let mut manager = McpServerManager::from_runtime_config(runtime_config);
        if manager.server_names().is_empty() && manager.unsupported_servers().is_empty() {
            return Ok(None);
        }

        let runtime = std::sync::Arc::new(tokio::runtime::Runtime::new()?);
        let discovery = runtime.block_on(manager.discover_tools_best_effort());
        let pending_servers = discovery
            .failed_servers
            .iter()
            .map(|failure| failure.server_name.clone())
            .chain(
                discovery
                    .unsupported_servers
                    .iter()
                    .map(|server| server.server_name.clone()),
            )
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        let available_tools = discovery
            .tools
            .iter()
            .map(|tool| tool.qualified_name.clone())
            .collect::<Vec<_>>();
        let failed_server_names = pending_servers.iter().cloned().collect::<BTreeSet<_>>();
        let working_servers = manager
            .server_names()
            .into_iter()
            .filter(|server_name| !failed_server_names.contains(server_name))
            .collect::<Vec<_>>();
        let failed_servers = discovery
            .failed_servers
            .iter()
            .map(|failure| ninmu_runtime::McpFailedServer {
                server_name: failure.server_name.clone(),
                phase: ninmu_runtime::McpLifecyclePhase::ToolDiscovery,
                error: ninmu_runtime::McpErrorSurface::new(
                    ninmu_runtime::McpLifecyclePhase::ToolDiscovery,
                    Some(failure.server_name.clone()),
                    failure.error.clone(),
                    std::collections::BTreeMap::new(),
                    true,
                ),
            })
            .chain(discovery.unsupported_servers.iter().map(|server| {
                ninmu_runtime::McpFailedServer {
                    server_name: server.server_name.clone(),
                    phase: ninmu_runtime::McpLifecyclePhase::ServerRegistration,
                    error: ninmu_runtime::McpErrorSurface::new(
                        ninmu_runtime::McpLifecyclePhase::ServerRegistration,
                        Some(server.server_name.clone()),
                        server.reason.clone(),
                        std::collections::BTreeMap::from([(
                            "transport".to_string(),
                            format!("{:?}", server.transport).to_ascii_lowercase(),
                        )]),
                        false,
                    ),
                }
            }))
            .collect::<Vec<_>>();
        let degraded_report = (!failed_servers.is_empty()).then(|| {
            ninmu_runtime::McpDegradedReport::new(
                working_servers,
                failed_servers,
                available_tools.clone(),
                available_tools,
            )
        });

        Ok(Some((
            Self {
                runtime,
                manager,
                pending_servers,
                degraded_report,
            },
            discovery,
        )))
    }

    pub(crate) fn shutdown(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        self.runtime.block_on(self.manager.shutdown())?;
        Ok(())
    }

    pub(crate) fn pending_servers(&self) -> Option<Vec<String>> {
        (!self.pending_servers.is_empty()).then(|| self.pending_servers.clone())
    }

    pub(crate) fn degraded_report(&self) -> Option<ninmu_runtime::McpDegradedReport> {
        self.degraded_report.clone()
    }

    pub(crate) fn server_names(&self) -> Vec<String> {
        self.manager.server_names()
    }

    pub(crate) fn call_tool(
        &mut self,
        qualified_tool_name: &str,
        arguments: Option<serde_json::Value>,
    ) -> Result<String, ToolError> {
        let response = self
            .runtime
            .block_on(self.manager.call_tool(qualified_tool_name, arguments))
            .map_err(|error| ToolError::new(error.to_string()))?;
        if let Some(error) = response.error {
            return Err(ToolError::new(format!(
                "MCP tool `{qualified_tool_name}` returned JSON-RPC error: {} ({})",
                error.message, error.code
            )));
        }

        let result = response.result.ok_or_else(|| {
            ToolError::new(format!(
                "MCP tool `{qualified_tool_name}` returned no result payload"
            ))
        })?;
        serde_json::to_string_pretty(&result).map_err(|error| ToolError::new(error.to_string()))
    }

    pub(crate) fn list_resources_for_server(
        &mut self,
        server_name: &str,
    ) -> Result<String, ToolError> {
        let result = self
            .runtime
            .block_on(self.manager.list_resources(server_name))
            .map_err(|error| ToolError::new(error.to_string()))?;
        serde_json::to_string_pretty(&json!({
            "server": server_name,
            "resources": result.resources,
        }))
        .map_err(|error| ToolError::new(error.to_string()))
    }

    pub(crate) fn list_resources_for_all_servers(&mut self) -> Result<String, ToolError> {
        let mut resources = Vec::new();
        let mut failures = Vec::new();

        for server_name in self.server_names() {
            match self
                .runtime
                .block_on(self.manager.list_resources(&server_name))
            {
                Ok(result) => resources.push(json!({
                    "server": server_name,
                    "resources": result.resources,
                })),
                Err(error) => failures.push(json!({
                    "server": server_name,
                    "error": error.to_string(),
                })),
            }
        }

        if resources.is_empty() && !failures.is_empty() {
            let message = failures
                .iter()
                .filter_map(|failure| failure.get("error").and_then(serde_json::Value::as_str))
                .collect::<Vec<_>>()
                .join("; ");
            return Err(ToolError::new(message));
        }

        serde_json::to_string_pretty(&json!({
            "resources": resources,
            "failures": failures,
        }))
        .map_err(|error| ToolError::new(error.to_string()))
    }

    pub(crate) fn read_resource(
        &mut self,
        server_name: &str,
        uri: &str,
    ) -> Result<String, ToolError> {
        let result = self
            .runtime
            .block_on(self.manager.read_resource(server_name, uri))
            .map_err(|error| ToolError::new(error.to_string()))?;
        serde_json::to_string_pretty(&json!({
            "server": server_name,
            "contents": result.contents,
        }))
        .map_err(|error| ToolError::new(error.to_string()))
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// HookAbortMonitor
// ═══════════════════════════════════════════════════════════════════════════

pub(crate) struct HookAbortMonitor {
    stop_tx: Option<Sender<()>>,
    join_handle: Option<JoinHandle<()>>,
}

impl HookAbortMonitor {
    pub(crate) fn spawn(abort_signal: ninmu_runtime::HookAbortSignal) -> Self {
        Self::spawn_with_waiter(abort_signal, move |stop_rx, abort_signal| {
            let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                return;
            };

            runtime.block_on(async move {
                let wait_for_stop = tokio::task::spawn_blocking(move || {
                    let _ = stop_rx.recv();
                });

                tokio::select! {
                    result = tokio::signal::ctrl_c() => {
                        if result.is_ok() {
                            abort_signal.abort();
                        }
                    }
                    _ = wait_for_stop => {}
                }
            });
        })
    }

    pub(crate) fn spawn_with_waiter<F>(
        abort_signal: ninmu_runtime::HookAbortSignal,
        wait_for_interrupt: F,
    ) -> Self
    where
        F: FnOnce(Receiver<()>, ninmu_runtime::HookAbortSignal) + Send + 'static,
    {
        let (stop_tx, stop_rx) = mpsc::channel();
        let join_handle = thread::spawn(move || wait_for_interrupt(stop_rx, abort_signal));

        Self {
            stop_tx: Some(stop_tx),
            join_handle: Some(join_handle),
        }
    }

    pub(crate) fn stop(mut self) {
        if let Some(stop_tx) = self.stop_tx.take() {
            let _ = stop_tx.send(());
        }
        if let Some(join_handle) = self.join_handle.take() {
            let _ = join_handle.join();
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// build_runtime_mcp_state
// ═══════════════════════════════════════════════════════════════════════════

pub(crate) fn build_runtime_mcp_state(
    runtime_config: &ninmu_runtime::RuntimeConfig,
) -> Result<RuntimePluginStateBuildOutput, Box<dyn std::error::Error>> {
    let Some((mcp_state, discovery)) = RuntimeMcpState::new(runtime_config)? else {
        return Ok((None, Vec::new()));
    };

    let mut runtime_tools = discovery
        .tools
        .iter()
        .map(mcp_runtime_tool_definition)
        .collect::<Vec<_>>();
    if !mcp_state.server_names().is_empty() {
        runtime_tools.extend(mcp_wrapper_tool_definitions());
    }

    Ok((Some(Arc::new(Mutex::new(mcp_state))), runtime_tools))
}

pub(crate) fn mcp_runtime_tool_definition(
    tool: &ninmu_runtime::ManagedMcpTool,
) -> RuntimeToolDefinition {
    RuntimeToolDefinition {
        name: tool.qualified_name.clone(),
        description: Some(
            tool.tool
                .description
                .clone()
                .unwrap_or_else(|| format!("Invoke MCP tool `{}`.", tool.qualified_name)),
        ),
        input_schema: tool
            .tool
            .input_schema
            .clone()
            .unwrap_or_else(|| json!({ "type": "object", "additionalProperties": true })),
        required_permission: permission_mode_for_mcp_tool(&tool.tool),
    }
}

pub(crate) fn mcp_wrapper_tool_definitions() -> Vec<RuntimeToolDefinition> {
    vec![
        RuntimeToolDefinition {
            name: "MCPTool".to_string(),
            description: Some(
                "Call a configured MCP tool by its qualified name and JSON arguments.".to_string(),
            ),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "qualifiedName": { "type": "string" },
                    "arguments": {}
                },
                "required": ["qualifiedName"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::DangerFullAccess,
        },
        RuntimeToolDefinition {
            name: "ListMcpResourcesTool".to_string(),
            description: Some(
                "List MCP resources from one configured server or from every connected server."
                    .to_string(),
            ),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "server": { "type": "string" }
                },
                "additionalProperties": false
            }),
            required_permission: PermissionMode::ReadOnly,
        },
        RuntimeToolDefinition {
            name: "ReadMcpResourceTool".to_string(),
            description: Some("Read a specific MCP resource from a configured server.".to_string()),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "server": { "type": "string" },
                    "uri": { "type": "string" }
                },
                "required": ["server", "uri"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::ReadOnly,
        },
    ]
}

pub(crate) fn permission_mode_for_mcp_tool(tool: &McpTool) -> PermissionMode {
    let read_only = mcp_annotation_flag(tool, "readOnlyHint");
    let destructive = mcp_annotation_flag(tool, "destructiveHint");
    let open_world = mcp_annotation_flag(tool, "openWorldHint");

    if read_only && !destructive && !open_world {
        PermissionMode::ReadOnly
    } else if destructive || open_world {
        PermissionMode::DangerFullAccess
    } else {
        PermissionMode::WorkspaceWrite
    }
}

pub(crate) fn mcp_annotation_flag(tool: &McpTool, key: &str) -> bool {
    tool.annotations
        .as_ref()
        .and_then(|annotations| annotations.get(key))
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
}

// ═══════════════════════════════════════════════════════════════════════════
// build_runtime / build_runtime_with_plugin_state
// ═══════════════════════════════════════════════════════════════════════════

#[allow(clippy::needless_pass_by_value)]
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_runtime(
    session: Session,
    session_id: &str,
    model: String,
    system_prompt: Vec<String>,
    enable_tools: bool,
    emit_output: bool,
    allowed_tools: Option<AllowedToolSet>,
    permission_mode: PermissionMode,
    progress_reporter: Option<InternalPromptProgressReporter>,
) -> Result<BuiltRuntime, Box<dyn std::error::Error>> {
    let runtime_plugin_state = build_runtime_plugin_state()?;
    build_runtime_with_plugin_state(
        session,
        session_id,
        model,
        system_prompt,
        enable_tools,
        emit_output,
        allowed_tools,
        permission_mode,
        progress_reporter,
        runtime_plugin_state,
        None,
    )
}

#[allow(clippy::needless_pass_by_value)]
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_runtime_with_plugin_state(
    mut session: Session,
    session_id: &str,
    model: String,
    system_prompt: Vec<String>,
    enable_tools: bool,
    emit_output: bool,
    allowed_tools: Option<AllowedToolSet>,
    permission_mode: PermissionMode,
    progress_reporter: Option<InternalPromptProgressReporter>,
    runtime_plugin_state: RuntimePluginState,
    shared_client: Option<Arc<AnthropicClient>>,
) -> Result<BuiltRuntime, Box<dyn std::error::Error>> {
    if session.model.is_none() {
        session.model = Some(model.clone());
    }
    let RuntimePluginState {
        feature_config,
        tool_registry,
        mut plugin_registry,
        mcp_state,
    } = runtime_plugin_state;
    plugin_registry.initialize()?;
    let policy = permission_policy(permission_mode, &feature_config, &tool_registry)
        .map_err(std::io::Error::other)?;
    let mut runtime = ConversationRuntime::new_with_features(
        session,
        AnthropicRuntimeClient::new(
            session_id,
            model,
            enable_tools,
            emit_output,
            allowed_tools.clone(),
            tool_registry.clone(),
            progress_reporter,
            feature_config.provider_defaults().clone(),
            shared_client,
        )?,
        CliToolExecutor::new(
            allowed_tools.clone(),
            emit_output,
            tool_registry.clone(),
            mcp_state.clone(),
            None,
        ),
        policy,
        system_prompt,
        &feature_config,
    );
    if emit_output {
        runtime = runtime.with_hook_progress_reporter(Box::new(CliHookProgressReporter));
    }
    Ok(BuiltRuntime::new(runtime, plugin_registry, mcp_state))
}

// ═══════════════════════════════════════════════════════════════════════════
// build_runtime_plugin_state
// ═══════════════════════════════════════════════════════════════════════════

pub(crate) fn build_runtime_plugin_state() -> Result<RuntimePluginState, Box<dyn std::error::Error>>
{
    let cwd = env::current_dir()?;
    let loader = ConfigLoader::default_for(&cwd);
    let runtime_config = loader.load()?;
    build_runtime_plugin_state_with_loader(&cwd, &loader, &runtime_config)
}

pub(crate) fn build_runtime_plugin_state_with_loader(
    cwd: &Path,
    loader: &ConfigLoader,
    runtime_config: &ninmu_runtime::RuntimeConfig,
) -> Result<RuntimePluginState, Box<dyn std::error::Error>> {
    let plugin_manager = build_plugin_manager(cwd, loader, runtime_config);
    let plugin_registry = plugin_manager.plugin_registry()?;
    let plugin_hook_config =
        runtime_hook_config_from_plugin_hooks(plugin_registry.aggregated_hooks()?);
    let feature_config = runtime_config
        .feature_config()
        .clone()
        .with_hooks(runtime_config.hooks().merged(&plugin_hook_config));
    let (mcp_state, runtime_tools) = build_runtime_mcp_state(runtime_config)?;
    let tool_registry = GlobalToolRegistry::with_plugin_tools(plugin_registry.aggregated_tools()?)?
        .with_runtime_tools(runtime_tools)?;
    Ok(RuntimePluginState {
        feature_config,
        tool_registry,
        plugin_registry,
        mcp_state,
    })
}

// ═══════════════════════════════════════════════════════════════════════════
// build_plugin_manager / resolve_plugin_path / runtime_hook_config
// ═══════════════════════════════════════════════════════════════════════════

pub(crate) fn build_plugin_manager(
    cwd: &Path,
    loader: &ConfigLoader,
    runtime_config: &ninmu_runtime::RuntimeConfig,
) -> PluginManager {
    let plugin_settings = runtime_config.plugins();
    let mut plugin_config = PluginManagerConfig::new(loader.config_home().to_path_buf());
    plugin_config.enabled_plugins = plugin_settings.enabled_plugins().clone();
    plugin_config.external_dirs = plugin_settings
        .external_directories()
        .iter()
        .map(|path| resolve_plugin_path(cwd, loader.config_home(), path))
        .collect();
    plugin_config.install_root = plugin_settings
        .install_root()
        .map(|path| resolve_plugin_path(cwd, loader.config_home(), path));
    plugin_config.registry_path = plugin_settings
        .registry_path()
        .map(|path| resolve_plugin_path(cwd, loader.config_home(), path));
    plugin_config.bundled_root = plugin_settings
        .bundled_root()
        .map(|path| resolve_plugin_path(cwd, loader.config_home(), path));
    PluginManager::new(plugin_config)
}

pub(crate) fn resolve_plugin_path(cwd: &Path, config_home: &Path, value: &str) -> PathBuf {
    let path = PathBuf::from(value);
    if path.is_absolute() {
        path
    } else if value.starts_with('.') {
        cwd.join(path)
    } else {
        config_home.join(path)
    }
}

pub(crate) fn runtime_hook_config_from_plugin_hooks(
    hooks: PluginHooks,
) -> ninmu_runtime::RuntimeHookConfig {
    ninmu_runtime::RuntimeHookConfig::new(
        hooks.pre_tool_use,
        hooks.post_tool_use,
        hooks.post_tool_use_failure,
    )
}

// ═══════════════════════════════════════════════════════════════════════════
// CliHookProgressReporter
// ═══════════════════════════════════════════════════════════════════════════

pub(crate) struct CliHookProgressReporter;

impl ninmu_runtime::HookProgressReporter for CliHookProgressReporter {
    fn on_event(&mut self, event: &ninmu_runtime::HookProgressEvent) {
        match event {
            ninmu_runtime::HookProgressEvent::Started {
                event,
                tool_name,
                command,
            } => eprintln!(
                "[hook {event_name}] {tool_name}: {command}",
                event_name = event.as_str()
            ),
            ninmu_runtime::HookProgressEvent::Completed {
                event,
                tool_name,
                command,
            } => eprintln!(
                "[hook done {event_name}] {tool_name}: {command}",
                event_name = event.as_str()
            ),
            ninmu_runtime::HookProgressEvent::Cancelled {
                event,
                tool_name,
                command,
            } => eprintln!(
                "[hook cancelled {event_name}] {tool_name}: {command}",
                event_name = event.as_str()
            ),
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// CliPermissionPrompter
// ═══════════════════════════════════════════════════════════════════════════

pub(crate) struct CliPermissionPrompter {
    current_mode: PermissionMode,
    approve_all: bool,
}

impl CliPermissionPrompter {
    pub(crate) fn new(current_mode: PermissionMode) -> Self {
        Self {
            current_mode,
            approve_all: false,
        }
    }
}

impl ninmu_runtime::PermissionPrompter for CliPermissionPrompter {
    fn decide(
        &mut self,
        request: &ninmu_runtime::PermissionRequest,
    ) -> ninmu_runtime::PermissionPromptDecision {
        if self.approve_all {
            return ninmu_runtime::PermissionPromptDecision::Allow;
        }

        let input = serde_json::from_str(&request.input)
            .unwrap_or(serde_json::Value::String(request.input.clone()));
        let prompt = format_enhanced_permission_prompt(
            &request.tool_name,
            &input,
            self.current_mode.as_str(),
            request.required_mode.as_str(),
            request.reason.as_deref(),
        );
        println!("{prompt}");
        let _ = io::stdout().flush();

        let mut response = String::new();
        match io::stdin().read_line(&mut response) {
            Ok(_) => match parse_permission_response(&response) {
                PermissionDecision::Allow => ninmu_runtime::PermissionPromptDecision::Allow,
                PermissionDecision::AllowAll => {
                    self.approve_all = true;
                    ninmu_runtime::PermissionPromptDecision::Allow
                }
                PermissionDecision::ViewInput => {
                    // Print the raw input on its own line so the user can inspect it
                    println!();
                    println!("Input:\n{}", request.input);
                    // Re-prompt
                    self.decide(request)
                }
                PermissionDecision::Deny { reason: _ } => {
                    ninmu_runtime::PermissionPromptDecision::Deny {
                        reason: format!(
                            "tool '{}' denied by user approval prompt",
                            request.tool_name
                        ),
                    }
                }
            },
            Err(error) => ninmu_runtime::PermissionPromptDecision::Deny {
                reason: format!("permission approval failed: {error}"),
            },
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// TuiPermissionPrompter  —  bridge permission prompts into the ratatui event loop
// ═══════════════════════════════════════════════════════════════════════════

pub(crate) struct TuiPermissionPrompter {
    bridge: crate::tui::TuiEventBridge,
}

impl TuiPermissionPrompter {
    pub(crate) fn new(bridge: crate::tui::TuiEventBridge) -> Self {
        Self { bridge }
    }
}

impl ninmu_runtime::PermissionPrompter for TuiPermissionPrompter {
    fn decide(
        &mut self,
        request: &ninmu_runtime::PermissionRequest,
    ) -> ninmu_runtime::PermissionPromptDecision {
        let rx = self.bridge.permission_prompt(request.clone());
        // Block until the TUI sends a decision through the channel.
        match rx.recv() {
            Ok(decision) => decision,
            Err(_) => {
                // Channel dropped — TUI exited. Deny as a safe default.
                ninmu_runtime::PermissionPromptDecision::Deny {
                    reason: "permission channel closed".to_string(),
                }
            }
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// AnthropicRuntimeClient
// ═══════════════════════════════════════════════════════════════════════════

// NOTE: Despite the historical name `AnthropicRuntimeClient`, this struct
// now holds an `ApiProviderClient` which dispatches to Anthropic, xAI,
// OpenAI, or DashScope at construction time based on
// `detect_provider_kind(&model)`. The struct name is kept to avoid
// churning `BuiltRuntime` and every Deref/DerefMut site that references
// it. See ROADMAP #29 for the provider-dispatch routing fix.
/// Shared tokio runtime for all API clients in this process.
/// Creating a fresh runtime per turn is expensive (spawns OS threads).
static TOKIO_RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();

pub(crate) struct AnthropicRuntimeClient {
    pub(crate) runtime: tokio::runtime::Handle,
    pub(crate) client: ApiProviderClient,
    pub(crate) session_id: String,
    pub(crate) model: String,
    pub(crate) enable_tools: bool,
    pub(crate) emit_output: bool,
    pub(crate) allowed_tools: Option<AllowedToolSet>,
    pub(crate) tool_registry: GlobalToolRegistry,
    pub(crate) progress_reporter: Option<InternalPromptProgressReporter>,
    pub(crate) reasoning_effort: Option<String>,
    pub(crate) thinking_mode: Option<bool>,
    pub(crate) provider_defaults:
        std::collections::BTreeMap<String, ninmu_runtime::ProviderDefaultConfig>,
    pub(crate) event_bridge: Option<crate::tui::TuiEventBridge>,
}

impl AnthropicRuntimeClient {
    pub(crate) fn new(
        session_id: &str,
        model: String,
        enable_tools: bool,
        emit_output: bool,
        allowed_tools: Option<AllowedToolSet>,
        tool_registry: GlobalToolRegistry,
        progress_reporter: Option<InternalPromptProgressReporter>,
        provider_defaults: std::collections::BTreeMap<String, ninmu_runtime::ProviderDefaultConfig>,
        shared_client: Option<Arc<AnthropicClient>>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let resolved_model = ninmu_api::resolve_model_alias(&model);
        let client = match detect_provider_kind(&resolved_model) {
            ProviderKind::Anthropic => {
                if let Some(arc) = shared_client {
                    ApiProviderClient::Anthropic((*arc).clone())
                } else {
                    let auth = resolve_cli_auth_source()?;
                    let inner = AnthropicClient::from_auth(auth)
                        .with_base_url(ninmu_api::read_base_url())
                        .with_prompt_cache(PromptCache::new(session_id));
                    ApiProviderClient::Anthropic(inner)
                }
            }
            ProviderKind::Xai
            | ProviderKind::OpenAi
            | ProviderKind::DeepSeek
            | ProviderKind::Ollama
            | ProviderKind::Qwen
            | ProviderKind::Vllm
            | ProviderKind::Mistral
            | ProviderKind::Gemini
            | ProviderKind::Cohere => {
                ApiProviderClient::from_model_with_anthropic_auth(&resolved_model, None)?
            }
        };
        let handle = TOKIO_RUNTIME
            .get_or_init(|| tokio::runtime::Runtime::new().expect("tokio runtime"))
            .handle()
            .clone();
        Ok(Self {
            runtime: handle,
            client,
            session_id: session_id.to_string(),
            model,
            enable_tools,
            emit_output,
            allowed_tools,
            tool_registry,
            progress_reporter,
            reasoning_effort: None,
            thinking_mode: None,
            provider_defaults,
            event_bridge: None,
        })
    }

    pub(crate) fn set_reasoning_effort(&mut self, effort: Option<String>) {
        self.reasoning_effort = effort;
    }

    pub(crate) fn set_thinking_mode(&mut self, mode: Option<bool>) {
        self.thinking_mode = mode;
    }
}

pub(crate) fn resolve_cli_auth_source() -> Result<AuthSource, Box<dyn std::error::Error>> {
    Ok(resolve_cli_auth_source_for_cwd()?)
}

pub(crate) fn resolve_cli_auth_source_for_cwd() -> Result<AuthSource, ninmu_api::ApiError> {
    resolve_startup_auth_source(|| Ok(None))
}

impl ApiClient for AnthropicRuntimeClient {
    #[allow(clippy::too_many_lines)]
    fn stream(&mut self, request: ApiRequest) -> Result<Vec<AssistantEvent>, RuntimeError> {
        if let Some(progress_reporter) = &self.progress_reporter {
            progress_reporter.mark_model_phase();
        }
        let is_post_tool = request_ends_with_tool_result(&request);

        // Apply per-provider defaults from runtime config
        let mut max_tokens_override = None;
        let mut temperature_override = None;
        let mut top_p_override = None;
        let mut reasoning_effort_override = self.reasoning_effort.clone();
        ninmu_runtime::apply_provider_defaults_from_map(
            &mut max_tokens_override,
            &mut temperature_override,
            &mut top_p_override,
            &mut reasoning_effort_override,
            &self.model,
            &self.provider_defaults,
        );

        let message_request = MessageRequest {
            model: self.model.clone(),
            max_tokens: max_tokens_override.unwrap_or_else(|| max_tokens_for_model(&self.model)),
            messages: convert_messages(&request.messages),
            system: (!request.system_prompt.is_empty()).then(|| request.system_prompt.join("\n\n")),
            tools: self
                .enable_tools
                .then(|| filter_tool_specs(&self.tool_registry, self.allowed_tools.as_ref())),
            tool_choice: self.enable_tools.then_some(ToolChoice::Auto),
            stream: true,
            temperature: temperature_override,
            top_p: top_p_override,
            reasoning_effort: reasoning_effort_override,
            thinking_mode: self.thinking_mode,
            ..Default::default()
        };

        self.runtime.block_on(async {
            let max_attempts: usize = if is_post_tool { 2 } else { 1 };

            for attempt in 1..=max_attempts {
                let result = self
                    .consume_stream(&message_request, is_post_tool && attempt == 1)
                    .await;
                match result {
                    Ok(events) => return Ok(events),
                    Err(error)
                        if error.to_string().contains("post-tool stall")
                            && attempt < max_attempts =>
                    {
                        // Stalled after tool completion — nudge the model by
                        // re-sending the same request.
                    }
                    Err(error) => return Err(error),
                }
            }

            Err(RuntimeError::new("post-tool continuation nudge exhausted"))
        })
    }
}

impl AnthropicRuntimeClient {
    /// Consume a single streaming response, optionally applying a stall
    /// timeout on the first event for post-tool continuations.
    #[allow(clippy::too_many_lines)]
    async fn consume_stream(
        &self,
        message_request: &MessageRequest,
        apply_stall_timeout: bool,
    ) -> Result<Vec<AssistantEvent>, RuntimeError> {
        let mut stream = self
            .client
            .stream_message(message_request)
            .await
            .map_err(|error| {
                RuntimeError::new(format_user_visible_api_error(&self.session_id, &error))
            })?;
        let mut stdout = io::stdout();
        let mut sink = io::sink();
        let out: &mut dyn Write = if self.emit_output {
            &mut stdout
        } else {
            &mut sink
        };
        let renderer = TerminalRenderer::new();
        let mut markdown_stream = MarkdownStreamState::default();
        let mut events = Vec::new();
        let mut pending_tool: Option<(String, String, String)> = None;
        let mut block_has_thinking_summary = false;
        let mut saw_stop = false;
        let mut received_any_event = false;

        // Status bar state
        let mut cumulative_input_tokens: u64 = 0;
        let mut cumulative_output_tokens: u64 = 0;
        let turn_start = std::time::Instant::now();
        let terminal_size = TerminalSize::new();
        let mut tool_timeline = ToolCallTimeline::new();
        let mut thinking_start: Option<std::time::Instant> = None;
        let mut thinking_chars: usize = 0;

        loop {
            // Choose timeout strategy:
            //  1. First event of a post-tool continuation → short (10 s) stall
            //     detection (existing behaviour).
            //  2. Subsequent events → general idle timeout (120 s) to catch
            //     mid-stream SSE hangs that would otherwise block forever.
            //  3. First event of a brand-new turn (not post-tool) → no timeout
            //     because the model may legitimately think for a long time
            //     before sending the first token.
            let next = if apply_stall_timeout && !received_any_event {
                match tokio::time::timeout(POST_TOOL_STALL_TIMEOUT, stream.next_event()).await {
                    Ok(inner) => inner.map_err(|error| {
                        RuntimeError::new(format_user_visible_api_error(&self.session_id, &error))
                    })?,
                    Err(_elapsed) => {
                        return Err(RuntimeError::new(
                            "post-tool stall: model did not respond within timeout",
                        ));
                    }
                }
            } else if received_any_event {
                // General idle timeout: if no event arrives within
                // STREAM_IDLE_TIMEOUT the connection is considered dead.
                match tokio::time::timeout(STREAM_IDLE_TIMEOUT, stream.next_event()).await {
                    Ok(inner) => inner.map_err(|error| {
                        RuntimeError::new(format_user_visible_api_error(&self.session_id, &error))
                    })?,
                    Err(_elapsed) => {
                        return Err(RuntimeError::new(format!(
                            "stream stall: no event received in {}s, connection appears dead",
                            STREAM_IDLE_TIMEOUT.as_secs()
                        )));
                    }
                }
            } else {
                stream.next_event().await.map_err(|error| {
                    RuntimeError::new(format_user_visible_api_error(&self.session_id, &error))
                })?
            };

            let Some(event) = next else {
                break;
            };
            received_any_event = true;

            match event {
                ApiStreamEvent::MessageStart(start) => {
                    for block in start.message.content {
                        push_output_block(
                            block,
                            out,
                            &mut events,
                            &mut pending_tool,
                            true,
                            &mut block_has_thinking_summary,
                        )?;
                    }
                }
                ApiStreamEvent::ContentBlockStart(start) => {
                    push_output_block(
                        start.content_block,
                        out,
                        &mut events,
                        &mut pending_tool,
                        true,
                        &mut block_has_thinking_summary,
                    )?;
                }
                ApiStreamEvent::ContentBlockDelta(delta) => match delta.delta {
                    ContentBlockDelta::TextDelta { text } => {
                        if !text.is_empty() {
                            if let Some(progress_reporter) = &self.progress_reporter {
                                progress_reporter.mark_text_phase(&text);
                            }
                            if let Some(rendered) = markdown_stream.push(&renderer, &text) {
                                write!(out, "{rendered}")
                                    .and_then(|()| out.flush())
                                    .map_err(|error| RuntimeError::new(error.to_string()))?;
                            }
                            if let Some(ref bridge) = self.event_bridge {
                                bridge.text(&text);
                            }
                            events.push(AssistantEvent::TextDelta(text));
                        }
                    }
                    ContentBlockDelta::InputJsonDelta { partial_json } => {
                        if let Some((_, _, input)) = &mut pending_tool {
                            input.push_str(&partial_json);
                        }
                    }
                    ContentBlockDelta::ThinkingDelta { thinking } => {
                        events.push(AssistantEvent::ThinkingDelta(thinking.clone()));
                        if thinking_start.is_none() {
                            thinking_start = Some(std::time::Instant::now());
                            if let Some(ref bridge) = self.event_bridge {
                                bridge.thinking_start();
                            }
                        }
                        thinking_chars += thinking.len();
                        if !block_has_thinking_summary {
                            if let Some(progress_reporter) = &self.progress_reporter {
                                progress_reporter.mark_reasoning_phase();
                            }
                            render_thinking_block_summary(out, None, false)?;
                            block_has_thinking_summary = true;
                        }
                    }
                    ContentBlockDelta::SignatureDelta { .. } => {}
                },
                ApiStreamEvent::ContentBlockStop(_) => {
                    block_has_thinking_summary = false;
                    if let Some(thinking_start_time) = thinking_start.take() {
                        if let Some(ref bridge) = self.event_bridge {
                            bridge
                                .thinking_stop(thinking_start_time.elapsed(), Some(thinking_chars));
                        }
                        thinking_chars = 0;
                    }
                    if let Some(rendered) = markdown_stream.flush(&renderer) {
                        write!(out, "{rendered}")
                            .and_then(|()| out.flush())
                            .map_err(|error| RuntimeError::new(error.to_string()))?;
                    }
                    if let Some((id, name, input)) = pending_tool.take() {
                        if let Some(progress_reporter) = &self.progress_reporter {
                            progress_reporter.mark_tool_phase(&name, &input);
                        }
                        tool_timeline.start_tool(&name);
                        if let Some(ref bridge) = self.event_bridge {
                            bridge.tool_use(&name, &input);
                        }
                        writeln!(out, "\n{}", format_tool_call_start(&name, &input))
                            .and_then(|()| out.flush())
                            .map_err(|error| RuntimeError::new(error.to_string()))?;
                        events.push(AssistantEvent::ToolUse { id, name, input });
                    }
                }
                ApiStreamEvent::MessageDelta(delta) => {
                    let usage = delta.usage.token_usage();
                    cumulative_input_tokens += u64::from(usage.input_tokens);
                    cumulative_output_tokens += u64::from(usage.output_tokens);
                    events.push(AssistantEvent::Usage(usage.clone()));

                    // Emit usage update to TUI bridge
                    if let Some(ref bridge) = self.event_bridge {
                        bridge.usage(usage);
                    }

                    // Update status bar
                    let cost_str = pricing_for_model(&self.model).map_or_else(
                        || "$—".to_string(),
                        |p| {
                            let estimate = usage.estimate_cost_usd_with_pricing(p);
                            format!("${:.4}", estimate.total_cost_usd())
                        },
                    );
                    let status_state = StatusBarState {
                        model: self.model.clone(),
                        permission_mode: "active".to_string(),
                        message_count: 0,
                        cumulative_input_tokens,
                        cumulative_output_tokens,
                        estimated_cost_usd: cost_str,
                        turn_start,
                        git_branch: None,
                        terminal_width: terminal_size.width(),
                    };
                    let _ = StatusBar::render(&status_state, out);
                }
                ApiStreamEvent::MessageStop(_) => {
                    saw_stop = true;
                    if let Some(rendered) = markdown_stream.flush(&renderer) {
                        write!(out, "{rendered}")
                            .and_then(|()| out.flush())
                            .map_err(|error| RuntimeError::new(error.to_string()))?;
                    }
                    events.push(AssistantEvent::MessageStop);
                }
            }
        }

        push_prompt_cache_record(&self.client, &mut events);

        if !saw_stop
            && events.iter().any(|event| {
                matches!(event, AssistantEvent::TextDelta(text) if !text.is_empty())
                    || matches!(event, AssistantEvent::ToolUse { .. })
            })
        {
            events.push(AssistantEvent::MessageStop);
        }

        if events
            .iter()
            .any(|event| matches!(event, AssistantEvent::MessageStop))
        {
            // Render tool timeline if any tools were called
            if !tool_timeline.events().is_empty() {
                let timeline_render = tool_timeline.render();
                write!(out, "{timeline_render}")
                    .and_then(|()| out.flush())
                    .map_err(|error| RuntimeError::new(error.to_string()))?;
            }
            return Ok(events);
        }

        let response = self
            .client
            .send_message(&MessageRequest {
                stream: false,
                ..message_request.clone()
            })
            .await
            .map_err(|error| {
                RuntimeError::new(format_user_visible_api_error(&self.session_id, &error))
            })?;
        let mut events = response_to_events(response, out)?;
        push_prompt_cache_record(&self.client, &mut events);
        Ok(events)
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// CliToolExecutor
// ═══════════════════════════════════════════════════════════════════════════

pub(crate) struct CliToolExecutor {
    renderer: TerminalRenderer,
    emit_output: bool,
    allowed_tools: Option<AllowedToolSet>,
    tool_registry: GlobalToolRegistry,
    mcp_state: Option<Arc<Mutex<RuntimeMcpState>>>,
    tool_timeline: Option<SharedToolCallTimeline>,
    event_bridge: Option<crate::tui::TuiEventBridge>,
}

impl CliToolExecutor {
    pub(crate) fn new(
        allowed_tools: Option<AllowedToolSet>,
        emit_output: bool,
        tool_registry: GlobalToolRegistry,
        mcp_state: Option<Arc<Mutex<RuntimeMcpState>>>,
        tool_timeline: Option<SharedToolCallTimeline>,
    ) -> Self {
        Self {
            renderer: TerminalRenderer::new(),
            emit_output,
            allowed_tools,
            tool_registry,
            mcp_state,
            tool_timeline,
            event_bridge: None,
        }
    }

    /// Attach a shared timeline so tool execution duration is recorded.
    pub(crate) fn set_timeline(&mut self, timeline: SharedToolCallTimeline) {
        self.tool_timeline = Some(timeline);
    }

    fn execute_search_tool(&self, value: serde_json::Value) -> Result<String, ToolError> {
        let input: ToolSearchRequest = serde_json::from_value(value)
            .map_err(|error| ToolError::new(format!("invalid tool input JSON: {error}")))?;
        let (pending_mcp_servers, mcp_degraded) =
            self.mcp_state.as_ref().map_or((None, None), |state| {
                let state = state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                (state.pending_servers(), state.degraded_report())
            });
        serde_json::to_string_pretty(&self.tool_registry.search(
            &input.query,
            input.max_results.unwrap_or(5),
            pending_mcp_servers,
            mcp_degraded,
        ))
        .map_err(|error| ToolError::new(error.to_string()))
    }

    fn execute_runtime_tool(
        &self,
        tool_name: &str,
        value: serde_json::Value,
    ) -> Result<String, ToolError> {
        let Some(mcp_state) = &self.mcp_state else {
            return Err(ToolError::new(format!(
                "runtime tool `{tool_name}` is unavailable without configured MCP servers"
            )));
        };
        let mut mcp_state = mcp_state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        match tool_name {
            "MCPTool" => {
                let input: McpToolRequest = serde_json::from_value(value)
                    .map_err(|error| ToolError::new(format!("invalid tool input JSON: {error}")))?;
                let qualified_name = input
                    .qualified_name
                    .or(input.tool)
                    .ok_or_else(|| ToolError::new("missing required field `qualifiedName`"))?;
                mcp_state.call_tool(&qualified_name, input.arguments)
            }
            "ListMcpResourcesTool" => {
                let input: ListMcpResourcesRequest = serde_json::from_value(value)
                    .map_err(|error| ToolError::new(format!("invalid tool input JSON: {error}")))?;
                match input.server {
                    Some(server_name) => mcp_state.list_resources_for_server(&server_name),
                    None => mcp_state.list_resources_for_all_servers(),
                }
            }
            "ReadMcpResourceTool" => {
                let input: ReadMcpResourceRequest = serde_json::from_value(value)
                    .map_err(|error| ToolError::new(format!("invalid tool input JSON: {error}")))?;
                mcp_state.read_resource(&input.server, &input.uri)
            }
            _ => mcp_state.call_tool(tool_name, Some(value)),
        }
    }
}

impl ToolExecutor for CliToolExecutor {
    fn execute(&mut self, tool_name: &str, input: &str) -> Result<String, ToolError> {
        if self
            .allowed_tools
            .as_ref()
            .is_some_and(|allowed| !allowed.contains(tool_name))
        {
            return Err(ToolError::new(format!(
                "tool `{tool_name}` is not enabled by the current --allowedTools setting"
            )));
        }
        let value = serde_json::from_str(input)
            .map_err(|error| ToolError::new(format!("invalid tool input JSON: {error}")))?;
        let result = if tool_name == "ToolSearch" {
            self.execute_search_tool(value)
        } else if self.tool_registry.has_runtime_tool(tool_name) {
            self.execute_runtime_tool(tool_name, value)
        } else {
            self.tool_registry
                .execute(tool_name, &value)
                .map_err(ToolError::new)
        };
        match result {
            Ok(output) => {
                if let Some(ref timeline) = self.tool_timeline {
                    let lines = output.lines().count();
                    timeline.with(|t| t.complete_tool(false, lines > 100, lines));
                }
                if let Some(ref bridge) = self.event_bridge {
                    bridge.tool_result(tool_name, &output, false);
                }
                if self.emit_output {
                    let highlight =
                        |code: &str, lang: &str| self.renderer.highlight_code(code, lang);
                    let markdown = format_tool_result(tool_name, &output, false, Some(&highlight));
                    self.renderer
                        .stream_markdown(&markdown, &mut io::stdout())
                        .map_err(|error| ToolError::new(error.to_string()))?;
                }
                Ok(output)
            }
            Err(error) => {
                if let Some(ref timeline) = self.tool_timeline {
                    timeline.with(|t| t.complete_tool(true, false, 0));
                }
                if let Some(ref bridge) = self.event_bridge {
                    bridge.tool_result(tool_name, error.to_string(), true);
                }
                if self.emit_output {
                    let highlight =
                        |code: &str, lang: &str| self.renderer.highlight_code(code, lang);
                    let markdown =
                        format_tool_result(tool_name, &error.to_string(), true, Some(&highlight));
                    self.renderer
                        .stream_markdown(&markdown, &mut io::stdout())
                        .map_err(|stream_error| ToolError::new(stream_error.to_string()))?;
                }
                Err(error)
            }
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// permission_policy / convert_messages
// ═══════════════════════════════════════════════════════════════════════════

pub(crate) fn permission_policy(
    mode: PermissionMode,
    feature_config: &ninmu_runtime::RuntimeFeatureConfig,
    tool_registry: &GlobalToolRegistry,
) -> Result<PermissionPolicy, String> {
    Ok(tool_registry.permission_specs(None)?.into_iter().fold(
        PermissionPolicy::new(mode).with_permission_rules(feature_config.permission_rules()),
        |policy, (name, required_permission)| {
            policy.with_tool_requirement(name, required_permission)
        },
    ))
}

pub(crate) fn convert_messages(messages: &[ConversationMessage]) -> Vec<InputMessage> {
    messages
        .iter()
        .filter_map(|message| {
            let role = match message.role {
                MessageRole::System | MessageRole::User | MessageRole::Tool => "user",
                MessageRole::Assistant => "assistant",
            };
            let content = message
                .blocks
                .iter()
                .map(|block| match block {
                    ContentBlock::Text { text } => InputContentBlock::Text { text: text.clone() },
                    ContentBlock::ToolUse { id, name, input } => InputContentBlock::ToolUse {
                        id: id.clone(),
                        name: name.clone(),
                        input: serde_json::from_str(input)
                            .unwrap_or_else(|_| serde_json::json!({ "raw": input })),
                    },
                    ContentBlock::ToolResult {
                        tool_use_id,
                        output,
                        is_error,
                        ..
                    } => InputContentBlock::ToolResult {
                        tool_use_id: tool_use_id.clone(),
                        content: vec![ToolResultContentBlock::Text {
                            text: output.clone(),
                        }],
                        is_error: *is_error,
                    },
                    ContentBlock::Thinking { thinking } => InputContentBlock::Thinking {
                        thinking: thinking.clone(),
                    },
                })
                .collect::<Vec<_>>();
            (!content.is_empty()).then(|| InputMessage {
                role: role.to_string(),
                content,
            })
        })
        .collect()
}

// ═══════════════════════════════════════════════════════════════════════════
// run_repl / run_stale_base_preflight
// ═══════════════════════════════════════════════════════════════════════════

pub(crate) fn run_stale_base_preflight(flag_value: Option<&str>) {
    let Ok(cwd) = env::current_dir() else {
        return;
    };
    let source = resolve_expected_base(flag_value, &cwd);
    let state = check_base_commit(&cwd, source.as_ref());
    if let Some(warning) = format_stale_base_warning(&state) {
        eprintln!("{warning}");
    }
}

#[allow(clippy::needless_pass_by_value)]
pub(crate) fn run_repl(
    model: String,
    allowed_tools: Option<AllowedToolSet>,
    permission_mode: PermissionMode,
    base_commit: Option<String>,
    reasoning_effort: Option<String>,
    allow_broad_cwd: bool,
    startup_banner: Option<BannerStyle>,
    tui: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    enforce_broad_cwd_policy(allow_broad_cwd, CliOutputFormat::Text)?;
    run_stale_base_preflight(base_commit.as_deref());
    let resolved_model = resolve_repl_model(model);
    // Resolve banner style from config if not explicitly provided
    let banner = startup_banner.or_else(|| {
        let cwd = env::current_dir().ok()?;
        let loader = ninmu_runtime::ConfigLoader::default_for(&cwd);
        let config = loader.load().ok()?;
        Some(BannerStyle::from_config(config.startup_banner()))
    });
    let mut cli = LiveCli::new(resolved_model, true, allowed_tools, permission_mode, banner)?;
    cli.set_reasoning_effort(reasoning_effort);

    if tui {
        return run_tui_repl(&mut cli);
    }

    let mut editor = input::LineEditor::new(
        "> ",
        cli.repl_completion_candidates().unwrap_or_default(),
        input::CompletionProvider {
            model_names: vec![cli.model.clone()],
            session_ids: match list_managed_sessions() {
                Ok(sessions) => sessions.into_iter().map(|s| s.id).collect(),
                Err(_) => Vec::new(),
            },
        },
    );
    println!("{}", cli.startup_banner());
    println!("{}", format_connected_line(&cli.model));

    loop {
        editor.set_completions(cli.repl_completion_candidates().unwrap_or_default());
        match editor.read_line()? {
            input::ReadOutcome::Submit(input) => {
                let trimmed = input.trim().to_string();
                if trimmed.is_empty() {
                    continue;
                }
                if matches!(trimmed.as_str(), "/exit" | "/quit") {
                    cli.persist_session()?;
                    break;
                }
                match SlashCommand::parse(&trimmed) {
                    Ok(Some(command)) => {
                        if cli.handle_repl_command(command)? {
                            cli.persist_session()?;
                        }
                        continue;
                    }
                    Ok(None) => {}
                    Err(error) => {
                        eprintln!("{error}");
                        continue;
                    }
                }
                // Bare-word skill dispatch: if the first token of the input
                // matches a known skill name, invoke it as `/skills <input>`
                // rather than forwarding raw text to the LLM (ROADMAP #36).
                let cwd = std::env::current_dir().unwrap_or_default();
                if let Some(prompt) = try_resolve_bare_skill_prompt(&cwd, &trimmed) {
                    editor.push_history(input);
                    cli.record_prompt_history(&trimmed);
                    cli.run_turn(&prompt)?;
                    continue;
                }
                editor.push_history(input);
                cli.record_prompt_history(&trimmed);
                cli.run_turn(&trimmed)?;
            }
            input::ReadOutcome::Cancel => {}
            input::ReadOutcome::Exit => {
                cli.persist_session()?;
                break;
            }
        }
    }

    Ok(())
}

/// REPL variant using the full-screen ratatui TUI with streaming event feedback.
fn run_tui_repl(cli: &mut LiveCli) -> Result<(), Box<dyn std::error::Error>> {
    if !std::io::stdout().is_terminal() || !std::io::stdin().is_terminal() {
        return run_repl_standard(cli);
    }

    let git_branch = status_context(None)
        .ok()
        .and_then(|ctx| ctx.git_branch.clone());

    let mut app = crate::tui::ratatui_app::RatatuiApp::new(
        cli.model.clone(),
        cli.permission_mode.as_str().to_string(),
        git_branch,
    );

    // Sync initial reasoning effort / thinking mode into the TUI header.
    if let Some(rt) = cli.runtime.runtime.as_ref() {
        app.set_reasoning_effort(rt.api_client().reasoning_effort.clone());
        app.set_thinking_mode(rt.api_client().thinking_mode);
    }

    // Load existing session messages into the scrollback so the user
    // can see conversation context before typing their first prompt.
    if !cli.runtime.session().messages.is_empty() {
        app.load_conversation_history(&cli.runtime.session().messages);
    }

    // Run the ratatui event loop. The `start_turn` closure is called on each
    // user submission and returns a `(Receiver, JoinHandle)` tuple that
    // satisfies the `TurnHandle` trait.
    app.run(|input| -> Result<_, Box<dyn std::error::Error>> {
        let trimmed = input.trim().to_string();
        if trimmed.is_empty() {
            return Err("empty input".into());
        }
        if std::env::var_os("NINMU_TEST_SCRIPTED_TUI_TURN").is_some() {
            return Ok(scripted_tui_turn());
        }
        if matches!(trimmed.as_str(), "/exit" | "/quit") {
            cli.persist_session()?;
            // Signal that we should exit by returning an error that the
            // caller interprets as "exit".  The ratatui app will close
            // the alternate screen.
            return Err("exit".into());
        }
        match SlashCommand::parse(&trimmed) {
            Ok(Some(command)) => {
                let is_resume = matches!(command, SlashCommand::Resume { .. });
                let is_effort = matches!(command, SlashCommand::Effort { .. });
                let is_think = matches!(command, SlashCommand::Think { .. });
                let is_model = matches!(command, SlashCommand::Model { .. });
                // Capture stdout+stderr so slash command output goes to
                // scrollback instead of being written directly to the
                // alternate screen.
                let pid = std::process::id();
                let temp_out = std::env::temp_dir().join(format!("ninmu_tui_{pid}_stdout"));
                let temp_err = std::env::temp_dir().join(format!("ninmu_tui_{pid}_stderr"));
                let captured = (|| -> Result<(String, String), Box<dyn std::error::Error>> {
                    let out_file = std::fs::File::create(&temp_out)?;
                    let err_file = std::fs::File::create(&temp_err)?;
                    let redir_out = gag::Redirect::stdout(out_file)?;
                    let redir_err = gag::Redirect::stderr(err_file)?;
                    let result = cli.handle_repl_command(command);
                    drop(redir_err);
                    drop(redir_out);
                    result?;
                    let stdout = std::fs::read_to_string(&temp_out).unwrap_or_default();
                    let stderr = std::fs::read_to_string(&temp_err).unwrap_or_default();
                    Ok((stdout, stderr))
                })();
                let _ = std::fs::remove_file(&temp_out);
                let _ = std::fs::remove_file(&temp_err);
                let (captured_stdout, captured_stderr) = captured?;
                // Push captured output through the bridge as text events
                // so it appears in the scrollback.
                let (bridge, rx) = crate::tui::TuiEventBridge::new();
                for line in captured_stdout.lines() {
                    bridge.text(format!("{line}\n"));
                }
                for line in captured_stderr.lines() {
                    bridge.error(line.to_string());
                }
                bridge.turn_complete();
                // For /resume, clear scrollback and load the new session's
                // history so the user sees fresh context.
                if is_resume {
                    bridge.load_history(cli.runtime.session().messages.clone());
                }
                // For /effort and /think, sync reasoning state back to the TUI header.
                if is_effort || is_think {
                    let effort = cli
                        .runtime
                        .runtime
                        .as_ref()
                        .and_then(|rt| rt.api_client().reasoning_effort.clone());
                    let thinking = cli
                        .runtime
                        .runtime
                        .as_ref()
                        .and_then(|rt| rt.api_client().thinking_mode);
                    bridge.reasoning_update(effort, thinking);
                }
                // For /model, sync model name back to the TUI header.
                if is_model {
                    bridge.model_update(cli.model.clone());
                }
                return Ok((
                    rx,
                    std::thread::spawn(|| {
                        Ok(String::new()) as Result<String, Box<dyn std::error::Error + Send>>
                    }),
                ));
            }
            Ok(None) => {}
            Err(error) => return Err(error.to_string().into()),
        }
        cli.record_prompt_history(&trimmed);
        cli.run_turn_tui_channels(&trimmed)
    })?;

    cli.persist_session()?;
    Ok(())
}

fn scripted_tui_turn() -> (
    std::sync::mpsc::Receiver<crate::tui::TuiEvent>,
    std::thread::JoinHandle<Result<String, Box<dyn std::error::Error + Send>>>,
) {
    let (bridge, rx) = crate::tui::TuiEventBridge::new();
    let scenario = std::env::var("NINMU_TEST_SCRIPTED_TUI_TURN").unwrap_or_default();
    let handle = std::thread::spawn(move || {
        if scenario == "permission" {
            let request = ninmu_runtime::PermissionRequest {
                tool_name: "bash".to_string(),
                input: r#"{"cmd":"cargo test"}"#.to_string(),
                current_mode: PermissionMode::ReadOnly,
                required_mode: PermissionMode::WorkspaceWrite,
                reason: Some("scripted permission e2e".to_string()),
            };
            let decision_rx = bridge.permission_prompt(request);
            let decision = decision_rx.recv().map_err(|error| {
                Box::new(std::io::Error::other(error.to_string()))
                    as Box<dyn std::error::Error + Send>
            })?;
            match decision {
                ninmu_runtime::PermissionPromptDecision::Allow => {
                    bridge.text("Scripted permission allowed.\n");
                }
                ninmu_runtime::PermissionPromptDecision::Deny { reason } => {
                    bridge.text(format!("Scripted permission denied: {reason}\n"));
                }
            }
            bridge.turn_complete();
            return Ok("Scripted permission complete.".to_string())
                as Result<String, Box<dyn std::error::Error + Send>>;
        }
        if scenario == "tool-error" {
            bridge.text("Scripted failing turn online.\n");
            bridge.tool_use("bash", r#"{"cmd":"exit 2"}"#);
            bridge.tool_progress("bash", Duration::from_millis(25));
            std::thread::sleep(Duration::from_millis(60));
            bridge.tool_result(
                "bash",
                "\x1b[31mboom\x1b[0m\nstderr line\nstack hint\nretry impossible\nexit code 2",
                true,
            );
            bridge.text("Scripted failure handled.\n");
            bridge.turn_complete();
            return Ok("Scripted failure handled.".to_string())
                as Result<String, Box<dyn std::error::Error + Send>>;
        }

        bridge.text("Scripted turn online.\n");
        bridge.tool_use("read_file", r#"{"path":"fixture.txt"}"#);
        bridge.tool_progress("read_file", Duration::from_millis(25));
        std::thread::sleep(Duration::from_millis(60));
        bridge.tool_result(
            "read_file",
            "alpha line\nbeta line\ngamma line\ndelta line\nepsilon line",
            false,
        );
        bridge.text("Scripted final response.\n");
        bridge.turn_complete();
        Ok("Scripted final response.".to_string())
            as Result<String, Box<dyn std::error::Error + Send>>
    });
    (rx, handle)
}

/// Standard rustyline-based REPL (the non-TUI path).
fn run_repl_standard(cli: &mut LiveCli) -> Result<(), Box<dyn std::error::Error>> {
    let mut editor = input::LineEditor::new(
        "> ",
        cli.repl_completion_candidates().unwrap_or_default(),
        input::CompletionProvider {
            model_names: vec![cli.model.clone()],
            session_ids: match list_managed_sessions() {
                Ok(sessions) => sessions.into_iter().map(|s| s.id).collect(),
                Err(_) => Vec::new(),
            },
        },
    );
    println!("{}", cli.startup_banner());
    println!("{}", format_connected_line(&cli.model));

    loop {
        editor.set_completions(cli.repl_completion_candidates().unwrap_or_default());
        match editor.read_line()? {
            input::ReadOutcome::Submit(input) => {
                let trimmed = input.trim().to_string();
                if trimmed.is_empty() {
                    continue;
                }
                if matches!(trimmed.as_str(), "/exit" | "/quit") {
                    cli.persist_session()?;
                    break;
                }
                match SlashCommand::parse(&trimmed) {
                    Ok(Some(command)) => {
                        if cli.handle_repl_command(command)? {
                            cli.persist_session()?;
                        }
                        continue;
                    }
                    Ok(None) => {}
                    Err(error) => {
                        eprintln!("{error}");
                        continue;
                    }
                }
                let cwd = std::env::current_dir().unwrap_or_default();
                if let Some(prompt) = try_resolve_bare_skill_prompt(&cwd, &trimmed) {
                    editor.push_history(input);
                    cli.record_prompt_history(&trimmed);
                    cli.run_turn(&prompt)?;
                    continue;
                }
                editor.push_history(input);
                cli.record_prompt_history(&trimmed);
                cli.run_turn(&trimmed)?;
            }
            input::ReadOutcome::Cancel => {}
            input::ReadOutcome::Exit => {
                cli.persist_session()?;
                break;
            }
        }
    }
    Ok(())
}

// ═══════════════════════════════════════════════════════════════════════════
// build_system_prompt
// ═══════════════════════════════════════════════════════════════════════════

/// Persist the chosen model to the user-level settings.json so it becomes
/// the default on the next launch.
fn persist_model_to_settings(model: &str) -> Result<(), String> {
    use std::path::Path;
    let config_home = std::env::var("NINMU_CONFIG_HOME")
        .ok()
        .map(std::path::PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| {
            std::env::var("HOME").map_or_else(
                |_| Path::new(".").to_path_buf(),
                |h| Path::new(&h).join(".ninmu"),
            )
        });
    let settings_path = config_home.join("settings.json");
    let mut settings = ninmu_tools_read_json_object(&settings_path)?;
    settings.insert(
        "model".to_string(),
        serde_json::Value::String(model.to_string()),
    );
    ninmu_tools_write_json_object(&settings_path, &settings)
}

fn ninmu_tools_read_json_object(
    path: &std::path::Path,
) -> Result<serde_json::Map<String, serde_json::Value>, String> {
    match std::fs::read_to_string(path) {
        Ok(content) => serde_json::from_str::<serde_json::Value>(&content)
            .map_err(|e| e.to_string())?
            .as_object()
            .cloned()
            .ok_or_else(|| "config file must contain a JSON object".to_string()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(serde_json::Map::new()),
        Err(e) => Err(e.to_string()),
    }
}

fn ninmu_tools_write_json_object(
    path: &std::path::Path,
    value: &serde_json::Map<String, serde_json::Value>,
) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    std::fs::write(
        path,
        serde_json::to_string_pretty(value).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())
}

fn build_system_prompt() -> Result<Vec<String>, Box<dyn std::error::Error>> {
    Ok(load_system_prompt(
        env::current_dir()?,
        DEFAULT_DATE,
        env::consts::OS,
        "unknown",
    )?)
}

// ═══════════════════════════════════════════════════════════════════════════
// Internal prompt progress types
// ═══════════════════════════════════════════════════════════════════════════

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct InternalPromptProgressState {
    pub(crate) command_label: &'static str,
    pub(crate) task_label: String,
    pub(crate) step: usize,
    pub(crate) phase: String,
    pub(crate) detail: Option<String>,
    pub(crate) saw_final_text: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InternalPromptProgressEvent {
    Started,
    Update,
    Heartbeat,
    Complete,
    Failed,
}

#[derive(Debug)]
struct InternalPromptProgressShared {
    state: Mutex<InternalPromptProgressState>,
    output_lock: Mutex<()>,
    started_at: Instant,
}

#[derive(Debug, Clone)]
pub(crate) struct InternalPromptProgressReporter {
    shared: Arc<InternalPromptProgressShared>,
}

#[derive(Debug)]
pub(crate) struct InternalPromptProgressRun {
    reporter: InternalPromptProgressReporter,
    heartbeat_stop: Option<mpsc::Sender<()>>,
    heartbeat_handle: Option<thread::JoinHandle<()>>,
}

impl InternalPromptProgressReporter {
    pub(crate) fn ultraplan(task: &str) -> Self {
        Self {
            shared: Arc::new(InternalPromptProgressShared {
                state: Mutex::new(InternalPromptProgressState {
                    command_label: "Ultraplan",
                    task_label: task.to_string(),
                    step: 0,
                    phase: "planning started".to_string(),
                    detail: Some(format!("task: {task}")),
                    saw_final_text: false,
                }),
                output_lock: Mutex::new(()),
                started_at: Instant::now(),
            }),
        }
    }

    fn emit(&self, event: InternalPromptProgressEvent, error: Option<&str>) {
        let snapshot = self.snapshot();
        let line = format_internal_prompt_progress_line(event, &snapshot, self.elapsed(), error);
        self.write_line(&line);
    }

    pub(crate) fn mark_model_phase(&self) {
        let snapshot = {
            let mut state = self
                .shared
                .state
                .lock()
                .expect("internal prompt progress state poisoned");
            state.step += 1;
            state.phase = if state.step == 1 {
                "analyzing request".to_string()
            } else {
                "reviewing findings".to_string()
            };
            state.detail = Some(format!("task: {}", state.task_label));
            state.clone()
        };
        self.write_line(&format_internal_prompt_progress_line(
            InternalPromptProgressEvent::Update,
            &snapshot,
            self.elapsed(),
            None,
        ));
    }

    pub(crate) fn mark_tool_phase(&self, name: &str, input: &str) {
        let detail = describe_tool_progress(name, input);
        let snapshot = {
            let mut state = self
                .shared
                .state
                .lock()
                .expect("internal prompt progress state poisoned");
            state.step += 1;
            state.phase = format!("running {name}");
            state.detail = Some(detail);
            state.clone()
        };
        self.write_line(&format_internal_prompt_progress_line(
            InternalPromptProgressEvent::Update,
            &snapshot,
            self.elapsed(),
            None,
        ));
    }

    pub(crate) fn mark_reasoning_phase(&self) {
        let snapshot = {
            let mut state = self
                .shared
                .state
                .lock()
                .expect("internal prompt progress state poisoned");
            state.step += 1;
            state.phase = "deep reasoning".to_string();
            state.detail = Some("model is reasoning about the problem".to_string());
            state.clone()
        };
        self.write_line(&format_internal_prompt_progress_line(
            InternalPromptProgressEvent::Update,
            &snapshot,
            self.elapsed(),
            None,
        ));
    }

    pub(crate) fn mark_text_phase(&self, text: &str) {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return;
        }
        let detail = truncate_for_summary(first_visible_line(trimmed), 120);
        let snapshot = {
            let mut state = self
                .shared
                .state
                .lock()
                .expect("internal prompt progress state poisoned");
            if state.saw_final_text {
                return;
            }
            state.saw_final_text = true;
            state.step += 1;
            state.phase = "drafting final plan".to_string();
            state.detail = (!detail.is_empty()).then_some(detail);
            state.clone()
        };
        self.write_line(&format_internal_prompt_progress_line(
            InternalPromptProgressEvent::Update,
            &snapshot,
            self.elapsed(),
            None,
        ));
    }

    fn emit_heartbeat(&self) {
        let snapshot = self.snapshot();
        self.write_line(&format_internal_prompt_progress_line(
            InternalPromptProgressEvent::Heartbeat,
            &snapshot,
            self.elapsed(),
            None,
        ));
    }

    fn snapshot(&self) -> InternalPromptProgressState {
        self.shared
            .state
            .lock()
            .expect("internal prompt progress state poisoned")
            .clone()
    }

    fn elapsed(&self) -> Duration {
        self.shared.started_at.elapsed()
    }

    fn write_line(&self, line: &str) {
        let _guard = self
            .shared
            .output_lock
            .lock()
            .expect("internal prompt progress output lock poisoned");
        let mut stdout = io::stdout();
        let _ = writeln!(stdout, "{line}");
        let _ = stdout.flush();
    }
}

impl InternalPromptProgressRun {
    pub(crate) fn start_ultraplan(task: &str) -> Self {
        let reporter = InternalPromptProgressReporter::ultraplan(task);
        reporter.emit(InternalPromptProgressEvent::Started, None);

        let (heartbeat_stop, heartbeat_rx) = mpsc::channel();
        let heartbeat_reporter = reporter.clone();
        let heartbeat_handle = thread::spawn(move || loop {
            match heartbeat_rx.recv_timeout(INTERNAL_PROGRESS_HEARTBEAT_INTERVAL) {
                Ok(()) | Err(RecvTimeoutError::Disconnected) => break,
                Err(RecvTimeoutError::Timeout) => heartbeat_reporter.emit_heartbeat(),
            }
        });

        Self {
            reporter,
            heartbeat_stop: Some(heartbeat_stop),
            heartbeat_handle: Some(heartbeat_handle),
        }
    }

    pub(crate) fn reporter(&self) -> InternalPromptProgressReporter {
        self.reporter.clone()
    }

    pub(crate) fn finish_success(&mut self) {
        self.stop_heartbeat();
        self.reporter
            .emit(InternalPromptProgressEvent::Complete, None);
    }

    pub(crate) fn finish_failure(&mut self, error: &str) {
        self.stop_heartbeat();
        self.reporter
            .emit(InternalPromptProgressEvent::Failed, Some(error));
    }

    fn stop_heartbeat(&mut self) {
        if let Some(sender) = self.heartbeat_stop.take() {
            let _ = sender.send(());
        }
        if let Some(handle) = self.heartbeat_handle.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for InternalPromptProgressRun {
    fn drop(&mut self) {
        self.stop_heartbeat();
    }
}

pub(crate) fn format_internal_prompt_progress_line(
    event: InternalPromptProgressEvent,
    snapshot: &InternalPromptProgressState,
    elapsed: Duration,
    error: Option<&str>,
) -> String {
    let elapsed_seconds = elapsed.as_secs();
    let step_label = if snapshot.step == 0 {
        "current step pending".to_string()
    } else {
        format!("current step {}", snapshot.step)
    };
    let mut status_bits = vec![step_label, format!("phase {}", snapshot.phase)];
    if let Some(detail) = snapshot
        .detail
        .as_deref()
        .filter(|detail| !detail.is_empty())
    {
        status_bits.push(detail.to_string());
    }
    let status = status_bits.join(" \u{00b7} ");
    match event {
        InternalPromptProgressEvent::Started => {
            format!(
                "-- {} status · planning started · {status}",
                snapshot.command_label
            )
        }
        InternalPromptProgressEvent::Update => {
            format!("\u{2026} {} status \u{00b7} {status}", snapshot.command_label)
        }
        InternalPromptProgressEvent::Heartbeat => format!(
            "\u{2026} {} heartbeat \u{00b7} {elapsed_seconds}s elapsed \u{00b7} {status}",
            snapshot.command_label
        ),
        InternalPromptProgressEvent::Complete => format!(
            "\u{2714} {} status \u{00b7} completed \u{00b7} {elapsed_seconds}s elapsed \u{00b7} {} steps total",
            snapshot.command_label, snapshot.step
        ),
        InternalPromptProgressEvent::Failed => format!(
            "\u{2718} {} status \u{00b7} failed \u{00b7} {elapsed_seconds}s elapsed \u{00b7} {}",
            snapshot.command_label,
            error.unwrap_or("unknown error")
        ),
    }
}

pub(crate) fn describe_tool_progress(name: &str, input: &str) -> String {
    let parsed: serde_json::Value =
        serde_json::from_str(input).unwrap_or(serde_json::Value::String(input.to_string()));
    match name {
        "bash" | "Bash" => {
            let command = parsed
                .get("command")
                .and_then(|value| value.as_str())
                .unwrap_or_default();
            if command.is_empty() {
                "running shell command".to_string()
            } else {
                format!("command {}", truncate_for_summary(command.trim(), 100))
            }
        }
        "read_file" | "Read" => format!("reading {}", extract_tool_path(&parsed)),
        "write_file" | "Write" => format!("writing {}", extract_tool_path(&parsed)),
        "edit_file" | "Edit" => format!("editing {}", extract_tool_path(&parsed)),
        "glob_search" | "Glob" => {
            let pattern = parsed
                .get("pattern")
                .and_then(|value| value.as_str())
                .unwrap_or("?");
            let scope = parsed
                .get("path")
                .and_then(|value| value.as_str())
                .unwrap_or(".");
            format!("glob `{pattern}` in {scope}")
        }
        "grep_search" | "Grep" => {
            let pattern = parsed
                .get("pattern")
                .and_then(|value| value.as_str())
                .unwrap_or("?");
            let scope = parsed
                .get("path")
                .and_then(|value| value.as_str())
                .unwrap_or(".");
            format!("grep `{pattern}` in {scope}")
        }
        "web_search" | "WebSearch" => parsed
            .get("query")
            .and_then(|value| value.as_str())
            .map_or_else(
                || "running web search".to_string(),
                |query| format!("query {}", truncate_for_summary(query, 100)),
            ),
        _ => {
            let summary = summarize_tool_payload(input);
            if summary.is_empty() {
                format!("running {name}")
            } else {
                format!("{name}: {summary}")
            }
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// Streaming / response helpers
// ═══════════════════════════════════════════════════════════════════════════

pub(crate) fn render_thinking_block_summary(
    out: &mut (impl Write + ?Sized),
    char_count: Option<usize>,
    redacted: bool,
) -> Result<(), RuntimeError> {
    let summary = crate::tui::thinking::render_thinking_inline(char_count, redacted);
    write!(out, "{summary}")
        .and_then(|()| out.flush())
        .map_err(|error| RuntimeError::new(error.to_string()))
}

pub(crate) fn push_output_block(
    block: OutputContentBlock,
    out: &mut (impl Write + ?Sized),
    events: &mut Vec<AssistantEvent>,
    pending_tool: &mut Option<(String, String, String)>,
    streaming_tool_input: bool,
    block_has_thinking_summary: &mut bool,
) -> Result<(), RuntimeError> {
    match block {
        OutputContentBlock::Text { text } => {
            if !text.is_empty() {
                let rendered = TerminalRenderer::new().markdown_to_ansi(&text);
                write!(out, "{rendered}")
                    .and_then(|()| out.flush())
                    .map_err(|error| RuntimeError::new(error.to_string()))?;
                events.push(AssistantEvent::TextDelta(text));
            }
        }
        OutputContentBlock::ToolUse { id, name, input } => {
            let initial_input = if streaming_tool_input
                && input.is_object()
                && input.as_object().is_some_and(serde_json::Map::is_empty)
            {
                String::new()
            } else {
                input.to_string()
            };
            *pending_tool = Some((id, name, initial_input));
        }
        OutputContentBlock::Thinking { thinking, .. } => {
            render_thinking_block_summary(out, Some(thinking.chars().count()), false)?;
            if !thinking.is_empty() {
                events.push(AssistantEvent::ThinkingDelta(thinking));
            }
            *block_has_thinking_summary = true;
        }
        OutputContentBlock::RedactedThinking { .. } => {
            render_thinking_block_summary(out, None, true)?;
            *block_has_thinking_summary = true;
        }
    }
    Ok(())
}

pub(crate) fn response_to_events(
    response: MessageResponse,
    out: &mut (impl Write + ?Sized),
) -> Result<Vec<AssistantEvent>, RuntimeError> {
    let mut events = Vec::new();
    let mut pending_tool = None;

    for block in response.content {
        let mut block_has_thinking_summary = false;
        push_output_block(
            block,
            out,
            &mut events,
            &mut pending_tool,
            false,
            &mut block_has_thinking_summary,
        )?;
        if let Some((id, name, input)) = pending_tool.take() {
            events.push(AssistantEvent::ToolUse { id, name, input });
        }
    }

    events.push(AssistantEvent::Usage(response.usage.token_usage()));
    events.push(AssistantEvent::MessageStop);
    Ok(events)
}

pub(crate) fn push_prompt_cache_record(
    client: &ApiProviderClient,
    events: &mut Vec<AssistantEvent>,
) {
    if let Some(record) = client.take_last_prompt_cache_record() {
        if let Some(event) = prompt_cache_record_to_runtime_event(record) {
            events.push(AssistantEvent::PromptCache(event));
        }
    }
}

pub(crate) fn prompt_cache_record_to_runtime_event(
    record: ninmu_api::PromptCacheRecord,
) -> Option<PromptCacheEvent> {
    let cache_break = record.cache_break?;
    Some(PromptCacheEvent {
        unexpected: cache_break.unexpected,
        reason: cache_break.reason,
        previous_cache_read_input_tokens: cache_break.previous_cache_read_input_tokens,
        current_cache_read_input_tokens: cache_break.current_cache_read_input_tokens,
        token_drop: cache_break.token_drop,
    })
}

/// Format a user-visible warning for a cache break event.
pub(crate) fn format_cache_break_warning(event: &PromptCacheEvent) -> String {
    let label = if event.unexpected {
        "Warning: prompt cache broke unexpectedly"
    } else {
        "Notice: prompt cache invalidated"
    };
    format!(
        "{} — {} ({} fewer cached tokens)",
        label, event.reason, event.token_drop
    )
}

// ═══════════════════════════════════════════════════════════════════════════
// Turn summary helpers
// ═══════════════════════════════════════════════════════════════════════════

/// Returns `true` when the conversation ends with a tool-result message,
/// meaning the model is expected to continue after tool execution.
pub(crate) fn request_ends_with_tool_result(request: &ApiRequest) -> bool {
    request
        .messages
        .last()
        .is_some_and(|message| message.role == MessageRole::Tool)
}

pub(crate) fn final_assistant_text(summary: &ninmu_runtime::TurnSummary) -> String {
    summary
        .assistant_messages
        .last()
        .map(|message| {
            message
                .blocks
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("")
        })
        .unwrap_or_default()
}

pub(crate) fn collect_tool_uses(summary: &ninmu_runtime::TurnSummary) -> Vec<serde_json::Value> {
    summary
        .assistant_messages
        .iter()
        .flat_map(|message| message.blocks.iter())
        .filter_map(|block| match block {
            ContentBlock::ToolUse { id, name, input } => Some(json!({
                "id": id,
                "name": name,
                "input": input,
            })),
            _ => None,
        })
        .collect()
}

pub(crate) fn collect_tool_results(summary: &ninmu_runtime::TurnSummary) -> Vec<serde_json::Value> {
    summary
        .tool_results
        .iter()
        .flat_map(|message| message.blocks.iter())
        .filter_map(|block| match block {
            ContentBlock::ToolResult {
                tool_use_id,
                tool_name,
                output,
                is_error,
            } => Some(json!({
                "tool_use_id": tool_use_id,
                "tool_name": tool_name,
                "output": output,
                "is_error": is_error,
            })),
            _ => None,
        })
        .collect()
}

pub(crate) fn collect_prompt_cache_events(
    summary: &ninmu_runtime::TurnSummary,
) -> Vec<serde_json::Value> {
    summary
        .prompt_cache_events
        .iter()
        .map(|event| {
            json!({
                "unexpected": event.unexpected,
                "reason": event.reason,
                "previous_cache_read_input_tokens": event.previous_cache_read_input_tokens,
                "current_cache_read_input_tokens": event.current_cache_read_input_tokens,
                "token_drop": event.token_drop,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{format_cache_break_warning, AnthropicRuntimeClient};
    use ninmu_runtime::PromptCacheEvent;

    #[test]
    fn anthropic_runtime_client_is_send() {
        fn assert_send<T: Send>() {}
        assert_send::<AnthropicRuntimeClient>();
    }

    #[test]
    fn format_cache_break_warning_shows_unexpected() {
        let event = PromptCacheEvent {
            unexpected: true,
            reason: "cache read tokens dropped while prompt fingerprint remained stable"
                .to_string(),
            previous_cache_read_input_tokens: 6_000,
            current_cache_read_input_tokens: 1_000,
            token_drop: 5_000,
        };
        let warning = format_cache_break_warning(&event);
        assert!(
            warning.starts_with("Warning:"),
            "unexpected break should start with Warning: {warning}"
        );
        assert!(
            warning.contains("5000 fewer cached tokens"),
            "should mention token drop: {warning}"
        );
    }

    #[test]
    fn format_cache_break_warning_shows_expected() {
        let event = PromptCacheEvent {
            unexpected: false,
            reason: "model changed".to_string(),
            previous_cache_read_input_tokens: 6_000,
            current_cache_read_input_tokens: 3_000,
            token_drop: 3_000,
        };
        let warning = format_cache_break_warning(&event);
        assert!(
            warning.starts_with("Notice:"),
            "expected break should start with Notice: {warning}"
        );
        assert!(
            warning.contains("model changed"),
            "should include reason: {warning}"
        );
    }
}
