//! Shared file staging area for inter-agent collaboration.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// A lock token granting exclusive write access to a staging file.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct StagingLock {
    pub task_id: String,
    pub agent_id: String,
    pub rel_path: String,
    pub version: u64,
    pub(crate) acquired_at: Instant,
    pub(crate) timeout: Duration,
}

/// RAII guard that automatically releases the staging lock on drop.
pub struct StagingLockGuard<'a> {
    lock: Option<StagingLock>,
    staging: &'a SharedStaging,
}

impl Drop for StagingLockGuard<'_> {
    fn drop(&mut self) {
        if let Some(ref lock) = self.lock {
            self.staging.unlock(lock);
        }
    }
}

impl StagingLockGuard<'_> {
    /// Take ownership of the inner `StagingLock`, preventing auto-unlock on drop.
    /// Call this only if you intend to manage the lock lifecycle manually.
    #[must_use]
    pub fn into_inner(mut self) -> StagingLock {
        self.lock.take().expect("lock present")
    }
}

#[derive(Debug, Default)]
struct StagingState {
    file_locks: HashMap<String, (String, u64, Instant, Duration)>,
    lock_counter: u64,
}

/// Shared file staging area owned by the orchestrator.
#[derive(Debug)]
pub struct SharedStaging {
    root: PathBuf,
    state: Arc<Mutex<StagingState>>,
    default_lock_timeout: Duration,
}

impl SharedStaging {
    #[must_use]
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            state: Arc::new(Mutex::new(StagingState::default())),
            default_lock_timeout: Duration::from_mins(5),
        }
    }

    #[must_use]
    pub fn with_lock_timeout(mut self, timeout: Duration) -> Self {
        self.default_lock_timeout = timeout;
        self
    }

    fn resolve_path(&self, task_id: &str, rel_path: &str) -> PathBuf {
        self.root.join(task_id).join(rel_path)
    }

    fn ensure_dir(path: &Path) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("mkdir failed: {e}"))?;
        }
        Ok(())
    }

    /// Validate that a relative path does not escape the staging root.
    fn validate_path(rel_path: &str) -> Result<(), String> {
        let normalized = Path::new(rel_path);
        let mut components = Vec::new();
        for component in normalized.components() {
            match component {
                std::path::Component::ParentDir => {
                    if components.is_empty() {
                        return Err("path escapes staging root".to_string());
                    }
                    components.pop();
                }
                std::path::Component::Normal(c) => {
                    components.push(c.to_string_lossy().to_string());
                }
                std::path::Component::RootDir => {
                    return Err("absolute path not allowed".to_string());
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn validate_task_id(task_id: &str) -> Result<(), String> {
        Self::validate_path(task_id)
    }

    /// Remove expired locks from memory. Called lazily on `lock` and `lock_guard`.
    fn sweep_expired(&self) {
        let mut state = self.state.lock().expect("state lock");
        let now = Instant::now();
        state
            .file_locks
            .retain(|_, (_, _, at, tmo)| now.duration_since(*at) <= *tmo);
    }

    fn atomic_write(path: &Path, content: &[u8]) -> Result<(), String> {
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, content).map_err(|e| format!("write tmp failed: {e}"))?;
        std::fs::rename(&tmp, path).map_err(|e| format!("rename failed: {e}"))?;
        Ok(())
    }

    pub fn write(
        &self,
        task_id: &str,
        agent_id: &str,
        rel_path: &str,
        content: &str,
    ) -> Result<(), String> {
        Self::validate_task_id(task_id)?;
        Self::validate_path(rel_path)?;
        let lock_key = format!("{task_id}/{rel_path}");
        {
            let state = self.state.lock().map_err(|e| format!("lock: {e}"))?;
            if let Some((owner, _v, at, tmo)) = state.file_locks.get(&lock_key) {
                if owner != agent_id || at.elapsed() > *tmo {
                    return Err(format!("cannot write {rel_path}: held by {owner}"));
                }
            }
        }
        let path = self.resolve_path(task_id, rel_path);
        Self::ensure_dir(&path)?;
        Self::atomic_write(&path, content.as_bytes()).map_err(|e| format!("write failed: {e}"))
    }

    pub fn read(&self, task_id: &str, rel_path: &str) -> Result<String, String> {
        Self::validate_task_id(task_id)?;
        Self::validate_path(rel_path)?;
        let path = self.resolve_path(task_id, rel_path);
        std::fs::read_to_string(&path).map_err(|e| format!("read failed: {e}"))
    }

    #[must_use]
    pub fn list(&self, task_id: &str) -> Vec<String> {
        let task_dir = self.root.join(task_id);
        if !task_dir.is_dir() {
            return Vec::new();
        }
        let mut files = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&task_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_file() {
                    if let Ok(rel) = path.strip_prefix(&task_dir) {
                        files.push(rel.to_string_lossy().to_string());
                    }
                }
            }
        }
        files
    }

    pub fn lock(
        &self,
        task_id: &str,
        agent_id: &str,
        rel_path: &str,
    ) -> Result<StagingLock, String> {
        Self::validate_task_id(task_id)?;
        Self::validate_path(rel_path)?;
        self.sweep_expired();
        let lock_key = format!("{task_id}/{rel_path}");
        let mut state = self.state.lock().map_err(|e| format!("lock: {e}"))?;

        if let Some((owner, _v, at, tmo)) = state.file_locks.get(&lock_key) {
            if owner == agent_id {
                state.lock_counter += 1;
                return Ok(StagingLock {
                    task_id: task_id.to_string(),
                    agent_id: agent_id.to_string(),
                    rel_path: rel_path.to_string(),
                    version: state.lock_counter,
                    acquired_at: Instant::now(),
                    timeout: self.default_lock_timeout,
                });
            }
            if at.elapsed() <= *tmo {
                return Err(format!("lock held by {owner}"));
            }
        }

        state.lock_counter += 1;
        let version = state.lock_counter;
        let acquired_at = Instant::now();
        state.file_locks.insert(
            lock_key,
            (
                agent_id.to_string(),
                version,
                acquired_at,
                self.default_lock_timeout,
            ),
        );
        Ok(StagingLock {
            task_id: task_id.to_string(),
            agent_id: agent_id.to_string(),
            rel_path: rel_path.to_string(),
            version,
            acquired_at,
            timeout: self.default_lock_timeout,
        })
    }

    /// Acquire a lock and return an RAII guard that auto-releases on drop.
    pub fn lock_guard(
        &self,
        task_id: &str,
        agent_id: &str,
        rel_path: &str,
    ) -> Result<StagingLockGuard<'_>, String> {
        let lock = self.lock(task_id, agent_id, rel_path)?;
        Ok(StagingLockGuard {
            lock: Some(lock),
            staging: self,
        })
    }

    pub fn unlock(&self, lock: &StagingLock) {
        let lock_key = format!("{}/{}", lock.task_id, lock.rel_path);
        let mut state = self.state.lock().expect("state lock");
        if let Some((owner, v, _, _)) = state.file_locks.get(&lock_key) {
            if owner == &lock.agent_id && *v == lock.version {
                state.file_locks.remove(&lock_key);
            }
        }
    }

    pub fn promote(
        &self,
        task_id: &str,
        rel_path: &str,
        workspace_root: &Path,
    ) -> Result<(), String> {
        Self::validate_task_id(task_id)?;
        Self::validate_path(rel_path)?;
        let src = self.resolve_path(task_id, rel_path);
        let dst = workspace_root.join(rel_path);
        Self::ensure_dir(&dst)?;
        let tmp = dst.with_extension("tmp");
        std::fs::copy(&src, &tmp).map_err(|e| format!("promote failed: {e}"))?;
        std::fs::rename(&tmp, &dst).map_err(|e| format!("rename failed: {e}"))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn staging() -> (SharedStaging, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("temp dir");
        let s = SharedStaging::new(dir.path().join("staging"))
            .with_lock_timeout(Duration::from_mins(1));
        (s, dir)
    }

    #[test]
    fn write_read_roundtrip() {
        let (s, _d) = staging();
        let lock = s.lock("t1", "a", "f.rs").unwrap();
        s.write("t1", "a", "f.rs", "content").unwrap();
        s.unlock(&lock);
        assert_eq!(s.read("t1", "f.rs").unwrap(), "content");
    }

    #[test]
    fn list_files_by_task() {
        let (s, _d) = staging();
        let l1 = s.lock("t1", "a", "a.rs").unwrap();
        s.write("t1", "a", "a.rs", "a").unwrap();
        s.unlock(&l1);
        let l2 = s.lock("t2", "b", "b.rs").unwrap();
        s.write("t2", "b", "b.rs", "b").unwrap();
        s.unlock(&l2);
        assert_eq!(s.list("t1"), vec!["a.rs"]);
        assert_eq!(s.list("t2"), vec!["b.rs"]);
    }

    #[test]
    fn lock_prevents_concurrent_write() {
        let (s, _d) = staging();
        let _a = s.lock("t1", "a", "s.rs").unwrap();
        assert!(s.lock("t1", "b", "s.rs").unwrap_err().contains("held by"));
    }

    #[test]
    fn lock_release_allows_write() {
        let (s, _d) = staging();
        let l = s.lock("t1", "a", "s.rs").unwrap();
        s.unlock(&l);
        assert!(s.lock("t1", "b", "s.rs").is_ok());
    }

    #[test]
    fn lock_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let s = SharedStaging::new(dir.path().join("staging"))
            .with_lock_timeout(Duration::from_millis(1));
        let _ = s.lock("t1", "a", "f.rs").unwrap();
        std::thread::sleep(Duration::from_millis(5));
        assert!(s.lock("t1", "b", "f.rs").is_ok());
    }

    #[test]
    fn promote_to_workspace() {
        let (s, _d) = staging();
        let l = s.lock("t1", "a", "out.txt").unwrap();
        s.write("t1", "a", "out.txt", "hello").unwrap();
        s.unlock(&l);
        let ws = tempfile::tempdir().unwrap();
        s.promote("t1", "out.txt", ws.path()).unwrap();
        assert_eq!(
            std::fs::read_to_string(ws.path().join("out.txt")).unwrap(),
            "hello"
        );
    }

    #[test]
    fn read_nonexistent_returns_error() {
        let (s, _d) = staging();
        assert!(s.read("tx", "m.rs").is_err());
    }

    #[test]
    fn path_traversal_blocked() {
        assert!(SharedStaging::validate_path("../../etc/passwd").is_err());
        assert!(SharedStaging::validate_path("good/path.rs").is_ok());
    }

    #[test]
    fn task_id_traversal_blocked() {
        let (s, _d) = staging();
        let result = s.lock("../../etc", "a", "passwd");
        assert!(result.is_err(), "task_id should be validated");
    }

    #[test]
    fn task_id_absolute_blocked() {
        let (s, _d) = staging();
        let result = s.lock("/etc", "a", "passwd");
        assert!(result.is_err(), "absolute task_id should be blocked");
    }

    #[test]
    fn lock_guard_auto_releases() {
        let (s, _d) = staging();
        {
            let _guard = s.lock_guard("t1", "a", "g.rs").unwrap();
            // lock held
            assert!(s.lock("t1", "b", "g.rs").is_err());
        }
        // guard dropped, lock released
        assert!(s.lock("t1", "b", "g.rs").is_ok());
    }

    #[test]
    fn sweep_removes_expired() {
        let dir = tempfile::tempdir().unwrap();
        let s = SharedStaging::new(dir.path().join("staging"))
            .with_lock_timeout(Duration::from_millis(10));
        let _lock = s.lock("t1", "a", "sweep.rs").unwrap();
        std::thread::sleep(Duration::from_millis(20));
        // next lock() sweeps expired
        let result = s.lock("t1", "b", "sweep.rs");
        assert!(result.is_ok(), "expired lock should have been swept");
    }

    #[test]
    fn atomic_write_no_tmp_leaked() {
        let (s, _d) = staging();
        let _guard = s.lock_guard("t1", "a", "atomic.txt").unwrap();
        s.write("t1", "a", "atomic.txt", "hello").unwrap();
        // No .tmp file should remain
        let path = s.resolve_path("t1", "atomic.txt");
        assert!(path.exists());
        assert!(!path.with_extension("tmp").exists());
    }
}
