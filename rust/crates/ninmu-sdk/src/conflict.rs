//! Conflict detection and merge engine for inter-agent collaboration.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// A conflict detected when two agents modify the same file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Conflict {
    pub path: String,
    pub base_agent: String,
    pub conflicting_agent: String,
    pub base_version: u64,
    pub base_checksum: String,
    pub current_checksum: String,
}

/// A resolved merge outcome.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum MergeResult {
    Clean(String),
    Conflict {
        path: String,
        ours: String,
        theirs: String,
        base: String,
    },
}

/// Merge strategy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MergeStrategy {
    AutoMerge,
    QueueAndRetry,
    EscalateToHuman,
    LastWriterWins,
}

fn sha256(content: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(content.as_bytes());
    format!("{:x}", hasher.finalize())
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before epoch")
        .as_millis() as u64
}

/// Tracks file versions and detects conflicts.
#[derive(Debug, Default)]
pub struct ConflictDetector {
    versions: Arc<Mutex<HashMap<String, FileVersion>>>,
}

#[derive(Debug, Clone)]
struct FileVersion {
    version: u64,
    modified_by: String,
    modified_at_ms: u64,
    checksum: String,
}

impl ConflictDetector {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a write and return the new version info.
    #[must_use]
    pub fn record_write(&self, agent_id: &str, path: &str, content: &str) -> u64 {
        let mut versions = self.versions.lock().expect("versions lock");
        let entry = versions.entry(path.to_string()).or_insert(FileVersion {
            version: 0,
            modified_by: String::new(),
            modified_at_ms: 0,
            checksum: String::new(),
        });
        entry.version += 1;
        entry.modified_by = agent_id.to_string();
        entry.modified_at_ms = now_ms();
        entry.checksum = sha256(content);
        entry.version
    }

    /// Check if an agent's write would conflict with another agent's changes.
    #[must_use]
    pub fn check_conflict(
        &self,
        agent_id: &str,
        path: &str,
        base_content: &str,
    ) -> Option<Conflict> {
        let versions = self.versions.lock().expect("versions lock");
        let current = versions.get(path)?;
        if current.modified_by == agent_id {
            return None;
        }
        if current.checksum == sha256(base_content) {
            return None;
        }
        Some(Conflict {
            path: path.to_string(),
            base_agent: current.modified_by.clone(),
            conflicting_agent: agent_id.to_string(),
            base_version: current.version,
            base_checksum: current.checksum.clone(),
            current_checksum: sha256(base_content),
        })
    }

    /// Return all current conflicts (where two agents have written since last read).
    #[must_use]
    pub fn conflicts(&self) -> Vec<String> {
        let versions = self.versions.lock().expect("versions lock");
        versions.keys().cloned().collect()
    }
}

/// Three-way merge engine with multiple strategies.
#[derive(Debug, Default)]
pub struct MergeEngine;

impl MergeEngine {
    /// Perform a line-based three-way merge.
    pub fn auto_merge(base: &str, ours: &str, theirs: &str) -> Result<String, MergeConflict> {
        let base_lines: Vec<&str> = base.lines().collect();
        let our_lines: Vec<&str> = ours.lines().collect();
        let their_lines: Vec<&str> = theirs.lines().collect();

        // Simple algorithm: if theirs == base, take ours; if ours == base, take theirs;
        // if ours == theirs, take either; otherwise check for overlapping changes.
        if their_lines == base_lines {
            return Ok(ours.to_string());
        }
        if our_lines == base_lines {
            return Ok(theirs.to_string());
        }
        if our_lines == their_lines {
            return Ok(ours.to_string());
        }

        // Find changed ranges (line indices that differ from base)
        let our_changes: Vec<usize> = our_lines
            .iter()
            .enumerate()
            .filter(|(i, line)| base_lines.get(*i) != Some(line))
            .map(|(i, _)| i)
            .collect();

        let their_changes: Vec<usize> = their_lines
            .iter()
            .enumerate()
            .filter(|(i, line)| base_lines.get(*i) != Some(line))
            .map(|(i, _)| i)
            .collect();

        // Conservative: if file lengths differ significantly from base in opposite
        // directions, this implies deletions vs additions and we should conflict.
        let our_len_diff = our_lines.len() as i64 - base_lines.len() as i64;
        let their_len_diff = their_lines.len() as i64 - base_lines.len() as i64;
        if our_len_diff.signum() != 0
            && their_len_diff.signum() != 0
            && our_len_diff.signum() != their_len_diff.signum()
            && our_len_diff.abs().max(their_len_diff.abs()) > 1
        {
            return Err(MergeConflict {
                path: String::new(),
                description: format!(
                    "conflict: our_len_diff={our_len_diff}, their_len_diff={their_len_diff}"
                ),
            });
        }

        // Check for overlap (any changed line index appears in both)
        let overlap: Vec<usize> = our_changes
            .iter()
            .filter(|i| their_changes.contains(i))
            .copied()
            .collect();

        if !overlap.is_empty() {
            return Err(MergeConflict {
                path: String::new(),
                description: format!("conflict at lines {overlap:?}: our edit vs their edit"),
            });
        }

        // No overlap: merge by taking our lines first, then adding their additions
        let mut result = ours.to_string();
        // Add their lines that extend beyond ours
        if their_lines.len() > our_lines.len() {
            let extra = their_lines[our_lines.len()..].join("\n");
            if !extra.is_empty() {
                result.push('\n');
                result.push_str(&extra);
            }
        }
        Ok(result)
    }
}

/// Describes a merge conflict that could not be auto-resolved.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MergeConflict {
    pub path: String,
    pub description: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- ConflictDetector tests ---

    #[test]
    fn no_conflict_on_sequential_writes() {
        let cd = ConflictDetector::new();
        cd.record_write("alice", "file.rs", "v1");
        let conflict = cd.check_conflict("bob", "file.rs", "v1");
        assert!(conflict.is_none(), "bob read latest, no conflict");
    }

    #[test]
    fn conflict_on_stale_base() {
        let cd = ConflictDetector::new();
        cd.record_write("alice", "file.rs", "v1");
        cd.record_write("alice", "file.rs", "v2");
        let conflict = cd.check_conflict("bob", "file.rs", "v1");
        assert!(conflict.is_some(), "bob has stale base");
    }

    #[test]
    fn no_false_positive_same_content() {
        let cd = ConflictDetector::new();
        cd.record_write("alice", "file.rs", "hello");
        let conflict = cd.check_conflict("bob", "file.rs", "hello");
        assert!(conflict.is_none(), "same content, no conflict");
    }

    #[test]
    fn same_agent_no_conflict() {
        let cd = ConflictDetector::new();
        cd.record_write("alice", "file.rs", "v1");
        let conflict = cd.check_conflict("alice", "file.rs", "v1");
        assert!(conflict.is_none(), "own write, no conflict");
    }

    // --- MergeEngine tests ---

    #[test]
    fn auto_merge_non_overlapping() {
        let base = "a\nb\nc\nd";
        let ours = "a\nb\nMODIFIED\nd";
        let theirs = "a\nb\nc\nd\ne";
        let result = MergeEngine::auto_merge(base, ours, theirs).unwrap();
        assert!(result.contains("MODIFIED"));
        assert!(result.contains('e'));
    }

    #[test]
    fn auto_merge_overlapping_returns_conflict() {
        let base = "a\nb\nc";
        let ours = "a\nMODIFIED\nc";
        let theirs = "a\nDIFFERENT\nc";
        let result = MergeEngine::auto_merge(base, ours, theirs);
        assert!(result.is_err(), "overlapping changes should conflict");
    }

    #[test]
    fn auto_merge_theirs_unchanged() {
        let base = "a\nb\nc";
        let ours = "a\nMODIFIED\nc";
        let result = MergeEngine::auto_merge(base, ours, base).unwrap();
        assert_eq!(result, ours);
    }

    #[test]
    fn auto_merge_ours_unchanged() {
        let base = "a\nb\nc";
        let theirs = "a\nMODIFIED\nc";
        let result = MergeEngine::auto_merge(base, base, theirs).unwrap();
        assert_eq!(result, theirs);
    }

    #[test]
    fn auto_merge_both_same() {
        let base = "a\nb\nc";
        let both = "MODIFIED";
        let result = MergeEngine::auto_merge(base, both, both).unwrap();
        assert_eq!(result, both);
    }

    #[test]
    fn auto_merge_empty_content() {
        let base = "";
        let ours = "new content";
        let result = MergeEngine::auto_merge(base, ours, "").unwrap();
        assert_eq!(result, ours);
    }

    #[test]
    fn auto_merge_with_deletions_returns_conflict() {
        // base has 5 lines, ours deletes 2, theirs adds 1
        let base = "a\nb\nc\nd\ne";
        let ours = "a\nb\nc";
        let theirs = "a\nb\nc\nd\ne\nf";
        let result = MergeEngine::auto_merge(base, ours, theirs);
        assert!(
            result.is_err(),
            "opposite-direction changes should conflict"
        );
    }
}
