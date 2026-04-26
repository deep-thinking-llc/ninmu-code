# Provider Implementation Plan — Second Pass

## Completed (survived rebrand)

| # | Feature | Status | Location |
|---|---------|--------|----------|
| 1 | 8 built-in providers (DeepSeek, Ollama, Qwen ext, vLLM + originals) | ✅ | `api/src/providers/mod.rs` |
| 2 | DeepSeek R1 reasoning/thinking content streaming | ✅ | `api/src/providers/openai_compat.rs` |
| 3 | Token limits (DeepSeek: 8,192 out / 131,072 ctx) | ✅ | `api/src/providers/mod.rs` |
| 4 | Pricing (DeepSeek chat/reasoner, Ollama/vLLM $0.00) | ✅ | `runtime/src/usage.rs` |
| 5 | Qwen API key + base URL fallback chains | ✅ | `api/src/providers/openai_compat.rs` |
| 6 | Ollama Cloud (optional OLLAMA_API_KEY, api.ollama.com) | ✅ | `api/src/providers/openai_compat.rs` |
| 7 | models.json `api` field validation | ✅ | `api/src/providers/models_file.rs` |
| 8 | CLI doctor: check_providers_health() with 8 providers | ✅ | `ninmu-cli/src/cli_commands.rs` |
| 9 | Model auto-discovery (Ollama /api/tags, vLLM /v1/models) | ✅ | `ninmu-cli/src/cli_commands.rs` |
| 10 | CLI init: .env.example template with all 8 providers | ✅ | `ninmu-cli/src/init.rs` |
| 11 | README: Built-in Providers table | ✅ | `README.md` |
| 12 | Doc comments on all public API types/functions | ✅ | `api/src/providers/*.rs` |
| 13 | Tests: 166 API + 11 provider integration + 8 openai-compat e2e | ✅ | `api/tests/` |

## Gaps identified in second pass

### Gap 1: `ProviderFallbackConfig` exists but is dead code
- **File:** `runtime/src/config.rs` — `ProviderFallbackConfig` struct is parsed from JSON but never used
- **Fix:** Wire it into the conversation turn loop so if a provider fails, fallbacks are tried

### Gap 2: No provider-aware cost tracking
- `TokenUsage` and `UsageCostEstimate` are model-agnostic
- No per-provider aggregation in session summaries
- **Fix:** Add `PerProviderUsage` tracking to `UsageTracker`

### Gap 3: No `--provider` CLI flag
- Provider selection is implicit via model prefix or env var sniffing
- No way to say "use DeepSeek for this session regardless of model name"
- **Fix:** Add `--provider <name>` flag to CLI arg parser

### Gap 4: No provider-specific defaults in settings.json
- Global `maxTokens`, `temperature` are applied uniformly
- DeepSeek needs different max tokens than Anthropic
- **Fix:** Add `providers: { deepseek: { maxTokens: 8192 } }` config section

### Gap 5: Integration tests use ad-hoc TCP listeners
- `client_integration.rs` has a compilation error after rebrand refactoring
- Tests are coupled to specific test binaries
- **Fix:** Extract reusable `MockServer` to `api/tests/common/`

### Gap 6: Missing e2e test for provider fallback
- No test that verifies fallback from primary → secondary provider
- **Fix:** Add e2e test with mock server that fails on first model, succeeds on second

---

## Implementation Plan (prioritized)

### 1. Config — Provider-specific defaults (highest value, low risk)

**What:** Allow per-provider overrides for `maxTokens`, `temperature`, `top_p`, `reasoning_effort` in `settings.json`.

```json
{
  "providers": {
    "deepseek": { "maxTokens": 8192 },
    "ollama": { "maxTokens": 4096 }
  }
}
```

**Files:**
- `runtime/src/config.rs` — parse `providers` key, add `ProviderDefaultConfig` struct
- `ninmu-cli/src/app.rs` — resolve provider defaults when building `MessageRequest`
- `api/src/providers/mod.rs` — add `provider_kind_for_model()` helper

**Tests:** 3-4 (config parsing, override precedence, model resolution)

### 2. Runtime — Provider fallback chains

**What:** Wire up existing `providerFallbacks` config into the conversation loop.

```json
{
  "providerFallbacks": {
    "primary": "deepseek-chat",
    "fallbacks": ["claude-haiku-4-5-20251213", "ollama/llama3.1:8b"]
  }
}
```

**Files:**
- `runtime/src/conversation.rs` — catch `ApiError`, try fallbacks
- `api/src/client.rs` — add `from_model_chain()` that tries each model

**Tests:** 2-3 (fallback success, fallback exhaustion, non-provider errors skip fallback)

### 3. CLI — `--provider` flag

**What:** Explicit provider selection overrides implicit prefix routing.

```bash
ninmu --provider deepseek --model chat prompt "hello"
```

**Files:**
- `ninmu-cli/src/args.rs` — add `--provider` flag
- `ninmu-cli/src/app.rs` — force provider when flag is set

**Tests:** 2 (flag parsing, provider override behavior)

### 4. Observability — Per-provider metrics

**What:** Track token usage and cost per provider across a session.

**Files:**
- `runtime/src/usage.rs` — add `PerProviderUsage`, update `UsageTracker::record()`
- `ninmu-cli/src/format/cost.rs` or status line — show per-provider breakdown

**Tests:** 2-3 (per-provider accumulation, cost breakdown, session reconstruction)

### 5. Testing — Mock server extraction

**What:** Fix `client_integration.rs` compilation error, extract reusable mock server.

**Files:**
- `api/tests/common/mock_http.rs` — new file with `MockServer`
- `api/tests/client_integration.rs` — fix compilation, use common mock
- `api/tests/openai_compat_integration.rs` — migrate to common mock

**Tests:** Mock server unit tests (route matching, error simulation)

### 6. E2E test — Provider fallback

**What:** Test that provider fallback works end-to-end with a mock server.

**Files:**
- `api/tests/provider_fallback_integration.rs` — new test file

**Tests:** 1 e2e test (mock fails first, succeeds on second)

---

## Estimated effort

| Priority | Item | Files | Tests | Est. lines |
|----------|------|-------|-------|-----------|
| 1 | Config defaults | 3 | 3-4 | ~120 |
| 2 | Fallback chains | 2 | 2-3 | ~150 |
| 3 | --provider flag | 2 | 2 | ~80 |
| 4 | Per-provider metrics | 2 | 2-3 | ~150 |
| 5 | Mock server | 3 | 2-3 | ~200 |
| 6 | E2E fallback test | 1 | 1 | ~60 |
| **Total** | | **13** | **12-16** | **~760** |