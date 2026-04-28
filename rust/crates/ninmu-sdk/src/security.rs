//! Security primitives: secret scrubbing and structured audit logging.
//!
//! Provides `SecretScrubber` for redacting known secret patterns in
//! strings and an append-only `AuditLog` for security-relevant events.

use std::collections::HashSet;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Secret patterns
// ---------------------------------------------------------------------------

/// A detected secret that should be scrubbed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecretMatch {
    /// Pattern name (e.g. "Anthropic API key").
    pub kind: String,
    /// Number of characters replaced.
    pub chars_replaced: usize,
}

// ---------------------------------------------------------------------------
// SecretScrubber
// ---------------------------------------------------------------------------

/// Redacts known secret prefix patterns from arbitrary text.
///
/// Does **not** require `regex`. Uses prefix matching and character-class
/// heuristics; intended for coarse scrubbing of logs/transcripts.
#[derive(Debug, Clone)]
pub struct SecretScrubber {
    max_preview: usize,
}

impl SecretScrubber {
    /// Create a scrubber with default settings.
    #[must_use]
    pub fn new() -> Self {
        Self { max_preview: 4 }
    }

    /// Set the number of leading characters left visible before `[REDACTED]`.
    #[must_use]
    pub fn with_max_preview(mut self, n: usize) -> Self {
        self.max_preview = n;
        self
    }

    /// Known secret prefixes with minimum length to trigger redaction.
    fn known_prefixes() -> &'static [(&'static str, &'static str, usize)] {
        // (prefix, pattern_name, min_length_after_prefix)
        &[
            ("sk-ant-api03-", "sk-ant-api", 20),
            ("sk-ant-", "sk-ant", 20),
            ("sk-live-", "sk-live", 24),
            ("sk-test-", "sk-test", 24),
            ("sk-openai--", "openai-key", 32),
            ("sk-", "generic-sk", 40),
            ("Bearer ", "bearer-token", 20),
            ("ghp_", "github-pat", 30),
            ("gho_", "github-oauth", 30),
            ("ghs_", "github-server", 30),
            ("ghr_", "github-refresh", 30),
            ("AKIA", "aws-key", 16),
            ("ASIA", "aws-session-key", 16),
            ("deepseek-", "deepseek-key", 32),
        ]
    }

    /// Scrub secrets from a single string.
    #[must_use]
    pub fn scrub(&self, input: &str) -> (String, Vec<SecretMatch>) {
        let mut output = input.to_string();
        let mut matches = Vec::new();

        for (prefix, name, min_len) in Self::known_prefixes() {
            let mut byte_start = 0usize;
            while let Some(pos) = output.as_bytes()[byte_start..]
                .windows(prefix.len())
                .position(|w| w == prefix.as_bytes())
            {
                let match_byte_start = byte_start + pos;
                let prefix_byte_len = prefix.len();

                // Find end of secret: whitespace, quote, or end of string
                let rest = &output[match_byte_start + prefix_byte_len..];
                let mut secret_char_len = prefix.chars().count();
                let mut secret_byte_len = prefix_byte_len;
                for ch in rest.chars() {
                    if ch.is_ascii_whitespace()
                        || ch == '"'
                        || ch == '\''
                        || ch == '\n'
                        || ch == '\r'
                    {
                        break;
                    }
                    secret_char_len += 1;
                    secret_byte_len += ch.len_utf8();
                }

                if secret_char_len < prefix.chars().count() + min_len {
                    byte_start = match_byte_start.saturating_add(1);
                    continue;
                }

                let visible_chars = self.max_preview.min(prefix.chars().count());
                let preview = output[match_byte_start..]
                    .chars()
                    .take(visible_chars)
                    .collect::<String>();
                let replacement = format!("{preview}[REDACTED]");
                output.replace_range(
                    match_byte_start..match_byte_start + secret_byte_len,
                    &replacement,
                );
                matches.push(SecretMatch {
                    kind: name.to_string(),
                    chars_replaced: secret_char_len,
                });
                byte_start = match_byte_start + replacement.len();
            }
        }
        (output, matches)
    }

    /// Check whether any known secret prefix appears in the text.
    #[must_use]
    pub fn has_secret(&self, input: &str) -> bool {
        Self::known_prefixes()
            .iter()
            .any(|(prefix, _, _)| input.contains(prefix))
    }

    /// Scrub environment variables known to hold secrets.
    #[must_use]
    pub fn scrub_env(input: &[(String, String)]) -> Vec<(String, String)> {
        let secret_suffixes: HashSet<&str> = HashSet::from_iter([
            "_KEY",
            "_TOKEN",
            "_SECRET",
            "_PASSWORD",
            "_API_KEY",
            "_AUTH",
        ]);
        input
            .iter()
            .cloned()
            .map(|(k, v)| {
                if secret_suffixes
                    .iter()
                    .any(|s| k.to_ascii_uppercase().ends_with(s))
                {
                    if v.len() <= 8 {
                        (k, "[REDACTED]".to_string())
                    } else {
                        let visible = v.chars().take(4).collect::<String>();
                        (k, format!("{visible}[REDACTED]"))
                    }
                } else {
                    (k, v)
                }
            })
            .collect()
    }
}

impl Default for SecretScrubber {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Audit log
// ---------------------------------------------------------------------------

/// Category of security-relevant event.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AuditEvent {
    ToolExecuted,
    PermissionChanged,
    CredentialAccessed,
    SandboxStarted,
    SandboxFailed,
    FileAccessed,
    FileModified,
    FileDeleted,
    ConfigLoaded,
    SessionExported,
    ReviewApproved,
    ReviewRejected,
    ReviewRequested,
}

/// A single audit-log entry.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AuditEntry {
    /// Event timestamp in ms since epoch.
    pub timestamp_ms: u64,
    /// Event category.
    pub event: AuditEvent,
    /// Human-readable summary.
    pub summary: String,
    /// The actor (user, agent, tool) responsible.
    pub actor: String,
    /// Whether the action succeeded.
    pub success: bool,
    /// Optional detailed payload.
    pub details: Option<serde_json::Value>,
}

impl AuditEntry {
    /// Create a new audit entry with the current timestamp.
    pub fn new(event: AuditEvent, summary: impl Into<String>, actor: impl Into<String>) -> Self {
        Self {
            timestamp_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64,
            event,
            summary: summary.into(),
            actor: actor.into(),
            success: true,
            details: None,
        }
    }

    /// Mark the entry as a failure.
    #[must_use]
    pub fn failed(mut self) -> Self {
        self.success = false;
        self
    }

    /// Attach a JSON payload.
    #[must_use]
    pub fn with_details(mut self, payload: serde_json::Value) -> Self {
        self.details = Some(payload);
        self
    }
}

/// An append-only structured audit log.
#[derive(Debug, Clone)]
pub struct AuditLog {
    entries: Vec<AuditEntry>,
    path: Option<PathBuf>,
    in_memory_limit: usize,
}

impl AuditLog {
    /// Create an in-memory-only audit log.
    #[must_use]
    pub fn in_memory() -> Self {
        Self {
            entries: Vec::new(),
            path: None,
            in_memory_limit: 10_000,
        }
    }

    /// Create an audit log that also persists to a JSONL file.
    pub fn file(filename: &str) -> Result<Self, String> {
        if filename.contains('/') || filename.contains('\\') {
            return Err("invalid filename: path separators not allowed".to_string());
        }
        let path = PathBuf::from(".claw").join(filename);
        let parent = path.parent().expect("parent exists");
        if !parent.exists() {
            fs::create_dir_all(parent)
                .map_err(|e| format!("failed to create audit log dir: {e}"))?;
        }
        let mut log = Self {
            entries: Vec::new(),
            path: Some(path),
            in_memory_limit: 10_000,
        };
        if let Some(p) = &log.path {
            if p.is_file() {
                let raw = fs::read_to_string(p).map_err(|e| format!("read audit log: {e}"))?;
                for line in raw.lines() {
                    if line.trim().is_empty() {
                        continue;
                    }
                    if let Ok(entry) = serde_json::from_str::<AuditEntry>(line) {
                        log.entries.push(entry);
                    }
                }
            }
        }
        Ok(log)
    }

    /// Append an entry.
    pub fn append(&mut self, entry: AuditEntry) {
        if let Some(path) = &self.path {
            if let Ok(line) = serde_json::to_string(&entry) {
                let _ = OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(path)
                    .and_then(|mut f| writeln!(f, "{line}"));
            }
        }
        self.entries.push(entry);
        if self.entries.len() > self.in_memory_limit {
            let drop_count = self.entries.len() - self.in_memory_limit;
            self.entries.drain(0..drop_count);
        }
    }

    /// Query entries by event type.
    #[must_use]
    pub fn filter(&self, event: AuditEvent) -> Vec<&AuditEntry> {
        self.entries.iter().filter(|e| e.event == event).collect()
    }

    /// All entries.
    #[must_use]
    pub fn entries(&self) -> &[AuditEntry] {
        &self.entries
    }

    /// Number of entries.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Is the log empty?
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Export the last `n` entries as JSON.
    #[must_use]
    pub fn export_json(&self, n: usize) -> String {
        let start = self.entries.len().saturating_sub(n);
        let slice = &self.entries[start..];
        serde_json::to_string_pretty(slice).unwrap_or_else(|_| "[]".to_string())
    }
}

// ---------------------------------------------------------------------------
// SecurityConfig
// ---------------------------------------------------------------------------

/// Configuration for security primitives.
#[derive(Debug, Clone)]
pub struct SecurityConfig {
    /// Enable secret scrubbing in logs and exports.
    pub scrub_enabled: bool,
    /// Append-only audit log path (None = in-memory only).
    pub audit_path: Option<PathBuf>,
    /// Maximum in-memory audit entries before dropping old ones.
    pub audit_memory_limit: usize,
    /// If true, refuse to start if a secret is detected in a prompt.
    pub block_prompt_secrets: bool,
}

impl SecurityConfig {
    /// Default production-safe config with scrubbing enabled.
    #[must_use]
    pub fn production() -> Self {
        Self {
            scrub_enabled: true,
            audit_path: Some(PathBuf::from(".claw/audit.jsonl")),
            audit_memory_limit: 10_000,
            block_prompt_secrets: true,
        }
    }

    /// Config for local development (scrubbing only, no audit persistence).
    #[must_use]
    pub fn development() -> Self {
        Self {
            scrub_enabled: true,
            audit_path: None,
            audit_memory_limit: 10_000,
            block_prompt_secrets: false,
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_scrubber_detects_anthropic_key() {
        let scrubber = SecretScrubber::default();
        let input = "key=sk-ant-api03-xXxXxXxXxXxXxXxXxXxXxXxXxXx";
        let (out, matches) = scrubber.scrub(input);
        assert!(matches.iter().any(|m| m.kind == "sk-ant-api"));
        assert!(!out.contains("sk-ant-api03-xXx"));
        assert!(out.contains("[REDACTED]"));
    }

    #[test]
    fn secret_scrubber_detects_bearer_token() {
        let scrubber = SecretScrubber::default();
        let input = "Authorization: Bearer eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9";
        let (out, matches) = scrubber.scrub(input);
        assert!(matches.iter().any(|m| m.kind == "bearer-token"));
        assert!(!out.contains("eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9"));
    }

    #[test]
    fn secret_scrubber_detects_github_pat() {
        let scrubber = SecretScrubber::default();
        let input = "token=ghp_xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx";
        let (out, matches) = scrubber.scrub(input);
        assert!(matches.iter().any(|m| m.kind == "github-pat"));
        assert!(!out.contains("ghp_xxxxxxxx"));
        assert!(out.contains("[REDACTED]"));
    }

    #[test]
    fn secret_scrubber_no_false_positives() {
        let scrubber = SecretScrubber::default();
        let input = "hello world no secrets here just normal text";
        let (out, matches) = scrubber.scrub(input);
        assert_eq!(matches.len(), 0);
        assert_eq!(out, input);
    }

    #[test]
    fn secret_scrubber_env_var_redaction() {
        let env = vec![
            (
                "ANTHROPIC_API_KEY".to_string(),
                "sk-ant-api03-secret".to_string(),
            ),
            ("OPENAI_API_KEY".to_string(), "sk-openai-secret".to_string()),
            ("USER".to_string(), "alice".to_string()),
        ];
        let scrubbed = SecretScrubber::scrub_env(&env);
        assert!(scrubbed[0].1.contains("[REDACTED]")); // ANTHROPIC_API_KEY
        assert!(scrubbed[1].1.contains("[REDACTED]")); // OPENAI_API_KEY
        assert_eq!(scrubbed[2].1, "alice"); // USER unaffected
    }

    #[test]
    fn secret_scrubber_has_secret() {
        let scrubber = SecretScrubber::default();
        assert!(scrubber.has_secret("key=sk-ant-api03-xxx"));
        assert!(!scrubber.has_secret("hello world"));
    }

    #[test]
    fn audit_log_in_memory() {
        let mut log = AuditLog::in_memory();
        log.append(AuditEntry::new(
            AuditEvent::ToolExecuted,
            "Read file src/main.rs",
            "bash_tool",
        ));
        assert_eq!(log.len(), 1);
        let tool_events = log.filter(AuditEvent::ToolExecuted);
        assert_eq!(tool_events.len(), 1);
    }

    #[test]
    fn audit_log_serde_round_trip() {
        let entry = AuditEntry::new(AuditEvent::PermissionChanged, "mode set to ask", "user")
            .failed()
            .with_details(serde_json::json!({"old": "danger", "new": "ask"}));
        let json = serde_json::to_string(&entry).expect("serialize");
        let parsed: AuditEntry = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed.event, entry.event);
        assert_eq!(parsed.summary, entry.summary);
        assert!(!parsed.success);
        assert_eq!(
            parsed.details,
            Some(serde_json::json!({"old": "danger", "new": "ask"}))
        );
    }

    #[test]
    fn audit_log_file_persistence() {
        let tmp = format!("audit-{}.jsonl", std::process::id());
        {
            let mut log = AuditLog::file(&tmp).expect("file log");
            log.append(AuditEntry::new(
                AuditEvent::FileModified,
                "Wrote src/main.rs",
                "tool",
            ));
        }
        {
            let log = AuditLog::file(&tmp).expect("reload");
            assert_eq!(log.len(), 1);
            assert_eq!(log.filter(AuditEvent::FileModified).len(), 1);
        }
        let _ = fs::remove_file(PathBuf::from(".claw").join(&tmp));
    }

    #[test]
    fn audit_log_memory_limit() {
        let mut log = AuditLog::in_memory();
        log.in_memory_limit = 5;
        for i in 0..10 {
            log.append(AuditEntry::new(
                AuditEvent::ToolExecuted,
                format!("op {i}"),
                "test",
            ));
        }
        assert_eq!(log.len(), 5);
        assert_eq!(log.entries().first().unwrap().summary, "op 5");
        assert_eq!(log.entries().last().unwrap().summary, "op 9");
    }

    #[test]
    fn security_config_defaults() {
        let prod = SecurityConfig::production();
        assert!(prod.scrub_enabled);
        assert!(prod.block_prompt_secrets);
        assert_eq!(prod.audit_memory_limit, 10_000);
        let dev = SecurityConfig::development();
        assert!(dev.scrub_enabled);
        assert!(!dev.block_prompt_secrets);
        assert!(dev.audit_path.is_none());
    }

    #[test]
    fn audit_log_export_json() {
        let mut log = AuditLog::in_memory();
        log.append(AuditEntry::new(
            AuditEvent::SandboxStarted,
            "sandbox active",
            "system",
        ));
        log.append(
            AuditEntry::new(AuditEvent::SandboxFailed, "docker not found", "system").failed(),
        );
        let json = log.export_json(1);
        assert!(json.contains("docker not found"));
    }

    #[test]
    fn secret_scrubber_too_short_not_redacted() {
        let scrubber = SecretScrubber::default();
        // sk- without enough trailing chars should be left alone
        let input = "prefix-sk-xx suffix";
        let (out, matches) = scrubber.scrub(input);
        // Generic sk- needs 40 chars after; this is too short
        assert_eq!(matches.len(), 0);
        assert_eq!(out, input);
    }

    #[test]
    fn audit_log_filter_empty() {
        let log = AuditLog::in_memory();
        let empty = log.filter(AuditEvent::FileDeleted);
        assert!(empty.is_empty());
    }

    #[test]
    fn audit_log_path_traversal_blocked() {
        let result = AuditLog::file("../../etc/passwd");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("path separators"));
    }

    #[test]
    fn secret_scrubber_short_secret_fully_redacted() {
        let env = vec![
            ("ANTHROPIC_API_KEY".to_string(), "short".to_string()),
            ("OPENAI_API_KEY".to_string(), "12345678".to_string()),
            ("LONG_SECRET".to_string(), "1234567890".to_string()),
        ];
        let scrubbed = SecretScrubber::scrub_env(&env);
        assert_eq!(scrubbed[0].1, "[REDACTED]"); // 5 chars -> fully redacted
        assert_eq!(scrubbed[1].1, "[REDACTED]"); // 8 chars -> fully redacted (threshold)
        assert!(scrubbed[2].1.starts_with("1234")); // 10 chars -> prefix visible
    }
}
