# AGENTS.md

This file provides guidance to AI coding agents when working in this repository.

## Naming conventions

### Design and implementation docs

Work-in-progress design and implementation documents use an `_` (underscore) prefix and are **gitignored** — they live only on your local machine. When a doc is ready for review, drop the `_` prefix to commit it.

| Pattern | Purpose |
|---------|---------|
| `_*.md` | WIP design/implementation doc (gitignored) |
| `*.md` | Final, committed doc |
| `_docs/` | WIP doc directories (gitignored) |

**Examples:**
```
docs/_provider-fallback-design.md    ← WIP, not tracked
docs/_testing-plan.md                ← WIP, not tracked
docs/provider-fallback-design.md     ← final, committed
```

### Provider naming

Provider labels in code, config, and docs use these canonical names (case-insensitive for user input):

| Canonical | Aliases | Env var prefix |
|-----------|---------|---------------|
| `anthropic` | — | `ANTHROPIC_` |
| `openai` | — | `OPENAI_` |
| `xai` | `grok` | `XAI_` |
| `deepseek` | — | `DEEPSEEK_` |
| `dashscope` | — | `DASHSCOPE_` |
| `ollama` | — | `OLLAMA_` |
| `qwen` | — | `QWEN_` |
| `vllm` | — | `VLLM_` |

## Verification

- Run Rust verification from `rust/`: `cargo fmt && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`
- API tests: `cargo test -p api`
- Runtime tests: `cargo test -p runtime`

## Provider architecture

### Adding a new provider

Follow this checklist (see `_provider-implementation-plan.md` for details):

1. `api/src/providers/mod.rs` — add `ProviderKind` variant, `MODEL_REGISTRY` entries, alias resolution, `metadata_for_model` routing, `detect_provider_kind` auth sniffing
2. `api/src/providers/openai_compat.rs` — add `OpenAiCompatConfig::new_provider()` constructor, default base URL, env vars, body size limits
3. `api/src/client.rs` — add `ProviderClient::NewProvider(OpenAiCompatClient)` variant, wire into `from_model_with_anthropic_auth`
4. `api/src/providers/models_file.rs` — add to `VALID_API_VALUES` and `custom_metadata_for_model`
5. `runtime/src/usage.rs` — add pricing entry in `pricing_for_model()` and `per_provider_label()`
6. `ninmu-cli/src/cli_commands.rs` — add to `check_providers_health()`
7. `ninmu-cli/src/format/model.rs` — add to `provider_label()`
8. `ninmu-cli/src/init.rs` — add to `PROVIDER_ENV_TEMPLATE`
9. `README.md` — add to Built-in Providers table
10. Tests — 3-4 per provider (alias, routing, config, credential)
