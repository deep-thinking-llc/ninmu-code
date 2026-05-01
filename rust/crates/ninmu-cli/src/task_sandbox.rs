use std::env;
use std::path::{Path, PathBuf};

use ninmu_runtime::harness_contract::{HarnessSandboxSummary, HarnessTaskRequest};

pub(crate) fn summarize_sandbox(request: &HarnessTaskRequest) -> HarnessSandboxSummary {
    let sandbox = request.sandbox.as_ref();
    HarnessSandboxSummary {
        actual_workdir: request.workdir.clone(),
        allowed_roots: sandbox
            .map(|sandbox| sandbox.allowed_roots.clone())
            .filter(|roots| !roots.is_empty())
            .unwrap_or_else(|| vec![request.workdir.clone()]),
        permission_mode: sandbox
            .and_then(|sandbox| sandbox.permission_mode.clone())
            .or_else(|| request.permission_mode.clone())
            .unwrap_or_else(|| "default".to_string()),
        network_policy: sandbox
            .and_then(|sandbox| sandbox.network_policy.clone())
            .unwrap_or_else(|| "unspecified".to_string()),
        denied_paths: Vec::new(),
        policy_violations: Vec::new(),
    }
}

pub(crate) fn record_denied_path(summary: &mut HarnessSandboxSummary, path: &Path) {
    summary.denied_paths.push(redact_private_path(path));
    summary
        .policy_violations
        .push("path outside allowed roots denied".to_string());
}

fn redact_private_path(path: &Path) -> String {
    let raw = path.display().to_string();
    let Ok(home) = env::var("HOME") else {
        return raw;
    };
    let home_path = PathBuf::from(home);
    if let Ok(stripped) = path.strip_prefix(&home_path) {
        let suffix = stripped.display().to_string();
        if suffix.is_empty() {
            "~".to_string()
        } else {
            format!("~/{suffix}")
        }
    } else {
        raw
    }
}

#[cfg(test)]
mod tests {
    use ninmu_runtime::harness_contract::{
        HarnessProtocolVersion, HarnessSandboxRequest, HarnessTaskRequest,
    };

    use super::{record_denied_path, summarize_sandbox};

    fn request() -> HarnessTaskRequest {
        HarnessTaskRequest {
            protocol: HarnessProtocolVersion::V1Alpha1,
            mission_id: "mission".to_string(),
            task_id: "task".to_string(),
            objective: "do work".to_string(),
            workdir: "/tmp/work".to_string(),
            model: None,
            permission_mode: Some("workspace-write".to_string()),
            allowed_tools: Vec::new(),
            acceptance_tests: Vec::new(),
            timeout_seconds: None,
            sandbox: Some(HarnessSandboxRequest {
                allowed_roots: vec!["/tmp/work".to_string()],
                permission_mode: Some("read-only".to_string()),
                network_policy: Some("disabled".to_string()),
            }),
            skill_profile: None,
            project_profile: None,
            previous_context: None,
        }
    }

    #[test]
    fn sandbox_summary_reports_requested_workdir_and_policy() {
        let summary = summarize_sandbox(&request());

        assert_eq!(summary.actual_workdir, "/tmp/work");
        assert_eq!(summary.allowed_roots, vec!["/tmp/work"]);
        assert_eq!(summary.permission_mode, "read-only");
        assert_eq!(summary.network_policy, "disabled");
    }

    #[test]
    fn denied_home_paths_are_redacted() {
        let _lock = test_env_lock();
        let _home_guard = EnvGuard::capture("HOME");
        std::env::set_var("HOME", "/Users/example");
        let mut summary = summarize_sandbox(&request());

        record_denied_path(
            &mut summary,
            std::path::Path::new("/Users/example/private/secret.txt"),
        );

        assert_eq!(summary.denied_paths, vec!["~/private/secret.txt"]);
        assert_eq!(
            summary.policy_violations,
            vec!["path outside allowed roots denied"]
        );
    }

    fn test_env_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| std::sync::Mutex::new(()))
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    struct EnvGuard {
        key: &'static str,
        value: Option<String>,
    }

    impl EnvGuard {
        fn capture(key: &'static str) -> Self {
            Self {
                key,
                value: std::env::var(key).ok(),
            }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.value {
                Some(value) => std::env::set_var(self.key, value),
                None => std::env::remove_var(self.key),
            }
        }
    }
}
