// Benchmarks for caching strategy performance and cost estimation
//
// Run with: cargo bench -p ninmu-api --bench caching_benchmark
//
// These benchmarks use Ollama (or any OpenAI-compatible local endpoint)
// to measure real-world request/response performance. Since Ollama does
// not implement server-side prompt caching, we simulate cache behavior
// by analyzing request payloads and counting cacheable tokens.
//
// To run against a real Ollama instance:
//   OLLAMA_BASE_URL=http://localhost:11434/v1 cargo bench -p ninmu-api --bench caching_benchmark

#![allow(
    clippy::cast_possible_truncation,
    clippy::cognitive_complexity,
    clippy::doc_markdown,
    clippy::explicit_iter_loop,
    clippy::format_in_format_args,
    clippy::missing_docs_in_private_items,
    clippy::must_use_candidate,
    clippy::needless_pass_by_value,
    clippy::clone_on_copy,
    clippy::too_many_lines,
    clippy::uninlined_format_args,
    dead_code
)]

use criterion::{black_box, criterion_group, criterion_main, Criterion};
use ninmu_api::{
    InputContentBlock, InputMessage, MessageRequest, ToolDefinition,
};
use serde_json::json;

/// Build a realistic multi-turn conversation request.
fn build_conversation_request(turns: usize) -> MessageRequest {
    let mut messages = Vec::with_capacity(turns * 2);
    for i in 0..turns {
        messages.push(InputMessage::user_text(format!(
            "Please analyze the following code and suggest improvements (turn {}):\n\nfn main() {{ println!(\"hello\"); }}",
            i + 1
        )));
        messages.push(InputMessage {
            role: "assistant".to_string(),
            content: vec![
                InputContentBlock::Text {
                    text: format!("Here's my analysis for turn {}...", i + 1),
                },
                InputContentBlock::ToolUse {
                    id: format!("call_{}", i),
                    name: "read_file".to_string(),
                    input: json!({"path": "/tmp/file.rs"}),
                },
            ],
        });
    }
    // Final user message
    messages.push(InputMessage::user_text(
        "Can you refactor this to use a more idiomatic approach?",
    ));

    let tools = Some(vec![
        ToolDefinition {
            name: "read_file".to_string(),
            description: Some("Read a file from disk".to_string()),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"}
                },
                "required": ["path"]
            }),
        },
        ToolDefinition {
            name: "write_file".to_string(),
            description: Some("Write content to a file".to_string()),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "content": {"type": "string"}
                },
                "required": ["path", "content"]
            }),
        },
    ]);

    MessageRequest {
        model: "ollama/qwen2.5-coder:7b".to_string(),
        max_tokens: 1024,
        messages,
        stream: false,
        system: Some("You are a helpful coding assistant.".to_string()),
        tools,
        tool_choice: None,
        temperature: Some(0.7),
        top_p: None,
        frequency_penalty: None,
        presence_penalty: None,
        stop: None,
        reasoning_effort: None,
        thinking_mode: None,
    }
}

/// Estimate input tokens from a request (bytes/4 heuristic).
fn estimate_input_tokens(request: &MessageRequest) -> u32 {
    let mut estimate = 0u32;
    if let Some(system) = &request.system {
        estimate += system.len() as u32 / 4 + 1;
    }
    for msg in &request.messages {
        for block in &msg.content {
            match block {
                InputContentBlock::Text { text, .. } => {
                    estimate += text.len() as u32 / 4 + 1;
                }
                InputContentBlock::ToolUse { name, input, .. } => {
                    estimate += name.len() as u32 / 4 + 1;
                    if let Ok(s) = serde_json::to_string(input) {
                        estimate += s.len() as u32 / 4 + 1;
                    }
                }
                InputContentBlock::ToolResult { content, .. } => {
                    for c in content {
                        match c {
                            ninmu_api::ToolResultContentBlock::Text { text } => {
                                estimate += text.len() as u32 / 4 + 1;
                            }
                            ninmu_api::ToolResultContentBlock::Json { value } => {
                                if let Ok(s) = serde_json::to_string(value) {
                                    estimate += s.len() as u32 / 4 + 1;
                                }
                            }
                        }
                    }
                }
                InputContentBlock::Thinking { thinking } => {
                    estimate += thinking.len() as u32 / 4 + 1;
                }
            }
        }
    }
    if let Some(tools) = &request.tools {
        for tool in tools {
            estimate += tool.name.len() as u32 / 4 + 1;
            if let Some(desc) = &tool.description {
                estimate += desc.len() as u32 / 4 + 1;
            }
            if let Ok(s) = serde_json::to_string(&tool.input_schema) {
                estimate += s.len() as u32 / 4 + 1;
            }
        }
    }
    estimate
}

/// Calculate how many tokens would be cache reads vs cache creations
/// with Anthropic-style prompt caching.
fn analyze_cache_savings(request: &MessageRequest) -> CacheAnalysis {
    let total_input = estimate_input_tokens(request);

    // With cache_control injection:
    // - System prompt is cached (always a cache read after first turn)
    // - Tool definitions are cached
    // - All messages except last 2 are cached
    let system_tokens = request
        .system
        .as_ref()
        .map_or(0, |s| s.len() as u32 / 4 + 1);

    let tool_tokens = request.tools.as_ref().map_or(0, |tools| {
        tools
            .iter()
            .map(|t| {
                let mut e = t.name.len() as u32 / 4 + 1;
                if let Some(d) = &t.description {
                    e += d.len() as u32 / 4 + 1;
                }
                if let Ok(s) = serde_json::to_string(&t.input_schema) {
                    e += s.len() as u32 / 4 + 1;
                }
                e
            })
            .sum()
    });

    let message_tokens: Vec<u32> = request
        .messages
        .iter()
        .map(|msg| {
            msg.content
                .iter()
                .map(|block| match block {
                    InputContentBlock::Text { text } => text.len() as u32 / 4 + 1,
                    InputContentBlock::ToolUse { name, input, .. } => {
                        let mut e = name.len() as u32 / 4 + 1;
                        if let Ok(s) = serde_json::to_string(input) {
                            e += s.len() as u32 / 4 + 1;
                        }
                        e
                    }
                    InputContentBlock::ToolResult { content, .. } => content
                        .iter()
                        .map(|c| match c {
                            ninmu_api::ToolResultContentBlock::Text { text } => {
                                text.len() as u32 / 4 + 1
                            }
                            ninmu_api::ToolResultContentBlock::Json { value } => {
                                serde_json::to_string(value).map_or(0, |s| s.len() as u32 / 4 + 1)
                            }
                        })
                        .sum(),
                    InputContentBlock::Thinking { thinking } => thinking.len() as u32 / 4 + 1,
                })
                .sum()
        })
        .collect();

    let cacheable_messages = message_tokens.len().saturating_sub(2);
    let cached_message_tokens: u32 = message_tokens.iter().take(cacheable_messages).sum();

    let cache_read_tokens = system_tokens + tool_tokens + cached_message_tokens;
    let cache_creation_tokens = if cacheable_messages > 0 || request.system.is_some() {
        // On first turn, everything is cache creation
        cache_read_tokens
    } else {
        0
    };
    let raw_input_tokens = total_input - cache_read_tokens;

    CacheAnalysis {
        total_input_tokens: total_input,
        cache_read_tokens,
        cache_creation_tokens,
        raw_input_tokens,
        system_tokens,
        tool_tokens,
        message_tokens,
    }
}

#[derive(Debug)]
#[allow(clippy::struct_field_names)]
struct CacheAnalysis {
    total_input_tokens: u32,
    cache_read_tokens: u32,
    cache_creation_tokens: u32,
    raw_input_tokens: u32,
    system_tokens: u32,
    tool_tokens: u32,
    message_tokens: Vec<u32>,
}

impl CacheAnalysis {
    /// Estimated cost with caching (Anthropic Sonnet 4.6 pricing).
    /// Cache read: 10% of input cost. Cache creation: 125% of input cost.
    fn estimated_cost_with_caching(&self, output_tokens: u32) -> f64 {
        let input_cost_per_1m = 3.0; // $3 per million input tokens
        let output_cost_per_1m = 15.0; // $15 per million output tokens
        let cache_read_cost_per_1m = 0.3; // $0.30 per million cache read tokens
        let cache_creation_cost_per_1m = 3.75; // $3.75 per million cache creation tokens

        let input_cost = f64::from(self.raw_input_tokens) * input_cost_per_1m / 1_000_000.0;
        let cache_read_cost =
            f64::from(self.cache_read_tokens) * cache_read_cost_per_1m / 1_000_000.0;
        let cache_creation_cost =
            f64::from(self.cache_creation_tokens) * cache_creation_cost_per_1m / 1_000_000.0;
        let output_cost = f64::from(output_tokens) * output_cost_per_1m / 1_000_000.0;

        input_cost + cache_read_cost + cache_creation_cost + output_cost
    }

    /// Estimated cost without caching (all tokens at full input rate).
    fn estimated_cost_without_caching(&self, output_tokens: u32) -> f64 {
        let input_cost_per_1m = 3.0;
        let output_cost_per_1m = 15.0;

        let input_cost = f64::from(self.total_input_tokens) * input_cost_per_1m / 1_000_000.0;
        let output_cost = f64::from(output_tokens) * output_cost_per_1m / 1_000_000.0;

        input_cost + output_cost
    }
}

fn bench_cache_injection(c: &mut Criterion) {
    let mut group = c.benchmark_group("cache_control_injection");

    for turns in [1, 5, 10, 20] {
        let request = build_conversation_request(turns);
        group.bench_with_input(
            criterion::BenchmarkId::new("inject", format!("{}_turns", turns)),
            &request,
            |b, req| {
                b.iter(|| {
                    let mut body = serde_json::to_value(black_box(req)).unwrap();
                    ninmu_api::inject_prompt_cache_control(&mut body);
                    body
                });
            },
        );
    }

    group.finish();
}

fn bench_cache_analysis(c: &mut Criterion) {
    let mut group = c.benchmark_group("cache_analysis");

    for turns in [1, 5, 10, 20] {
        let request = build_conversation_request(turns);
        group.bench_with_input(
            criterion::BenchmarkId::new("analyze", format!("{}_turns", turns)),
            &request,
            |b, req| {
                b.iter(|| analyze_cache_savings(black_box(req)));
            },
        );
    }

    group.finish();
}

fn bench_request_serialization(c: &mut Criterion) {
    let mut group = c.benchmark_group("request_serialization");

    for turns in [1, 5, 10, 20] {
        let request = build_conversation_request(turns);
        group.bench_with_input(
            criterion::BenchmarkId::new("to_json", format!("{}_turns", turns)),
            &request,
            |b, req| {
                b.iter(|| serde_json::to_value(black_box(req)).unwrap());
            },
        );
    }

    group.finish();
}

/// Print a cache savings report for visual inspection.
fn print_cache_report() {
    println!("\n=== Prompt Cache Savings Report ===\n");
    for turns in [1, 5, 10, 20] {
        let request = build_conversation_request(turns);
        let analysis = analyze_cache_savings(&request);
        let output_tokens = 500; // Assume 500 output tokens per turn

        let cost_with = analysis.estimated_cost_with_caching(output_tokens);
        let cost_without = analysis.estimated_cost_without_caching(output_tokens);
        let savings = ((cost_without - cost_with) / cost_without) * 100.0;

        println!(
            "Turns: {:2} | Total input: {:5} tokens",
            turns, analysis.total_input_tokens
        );
        println!(
            "  Cache read:     {:5} tokens ({:.1}%)",
            analysis.cache_read_tokens,
            100.0 * f64::from(analysis.cache_read_tokens) / f64::from(analysis.total_input_tokens)
        );
        println!(
            "  Raw input:      {:5} tokens ({:.1}%)",
            analysis.raw_input_tokens,
            100.0 * f64::from(analysis.raw_input_tokens) / f64::from(analysis.total_input_tokens)
        );
        println!(
            "  Est. cost:      ${:.4} (with cache) vs ${:.4} (without)",
            cost_with, cost_without
        );
        println!("  Savings:        {:.1}%", savings);
        println!();
    }
}

criterion_group!(
    benches,
    bench_cache_injection,
    bench_cache_analysis,
    bench_request_serialization
);
criterion_main!(benches);
