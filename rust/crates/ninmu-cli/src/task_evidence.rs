use ninmu_runtime::harness_contract::{
    HarnessCompletionEvidence, HarnessConfidence, HarnessTaskStatus, HarnessTestReport,
    HarnessTestStatus,
};

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct EvidenceDecision {
    pub(crate) evidence: HarnessCompletionEvidence,
    pub(crate) confidence: HarnessConfidence,
    pub(crate) status: HarnessTaskStatus,
    pub(crate) status_reason: String,
    pub(crate) recommendation: String,
}

pub(crate) fn decide(
    tests: &[HarnessTestReport],
    changed_files: &[String],
    unresolved_blockers: &[String],
) -> EvidenceDecision {
    if !unresolved_blockers.is_empty() {
        return EvidenceDecision {
            evidence: HarnessCompletionEvidence {
                items: vec!["unresolved blocker reported".to_string()],
                acceptance_tests_passed: tests_passed(tests),
                changed_files_observed: !changed_files.is_empty(),
                unresolved_blockers: unresolved_blockers.to_vec(),
            },
            confidence: HarnessConfidence {
                score: 0.0,
                level: "blocked".to_string(),
            },
            status: HarnessTaskStatus::Blocked,
            status_reason: unresolved_blockers.join("; "),
            recommendation: "block".to_string(),
        };
    }

    if tests
        .iter()
        .any(|test| test.status == HarnessTestStatus::Failed)
    {
        return EvidenceDecision {
            evidence: HarnessCompletionEvidence {
                items: vec!["acceptance test failed".to_string()],
                acceptance_tests_passed: Some(false),
                changed_files_observed: !changed_files.is_empty(),
                unresolved_blockers: Vec::new(),
            },
            confidence: HarnessConfidence {
                score: 0.2,
                level: "low".to_string(),
            },
            status: HarnessTaskStatus::Failed,
            status_reason: "acceptance test failed".to_string(),
            recommendation: "retry".to_string(),
        };
    }

    let passed = tests_passed(tests);
    let (score, level, recommendation) = match (passed, changed_files.is_empty()) {
        (Some(true), false) => (0.9, "high", "accept"),
        (Some(true), true) => (0.7, "medium", "review"),
        (None, false) => (0.6, "medium", "review"),
        (None, true) => (0.4, "low", "review"),
        (Some(false), _) => unreachable!("failed tests returned earlier"),
    };

    EvidenceDecision {
        evidence: HarnessCompletionEvidence {
            items: evidence_items(passed, changed_files),
            acceptance_tests_passed: passed,
            changed_files_observed: !changed_files.is_empty(),
            unresolved_blockers: Vec::new(),
        },
        confidence: HarnessConfidence {
            score,
            level: level.to_string(),
        },
        status: HarnessTaskStatus::Completed,
        status_reason: "runtime completed".to_string(),
        recommendation: recommendation.to_string(),
    }
}

fn tests_passed(tests: &[HarnessTestReport]) -> Option<bool> {
    if tests.is_empty() {
        None
    } else {
        Some(
            tests
                .iter()
                .all(|test| test.status == HarnessTestStatus::Passed),
        )
    }
}

fn evidence_items(passed: Option<bool>, changed_files: &[String]) -> Vec<String> {
    let mut items = vec!["runtime turn completed".to_string()];
    match passed {
        Some(true) => items.push("acceptance tests passed".to_string()),
        Some(false) => items.push("acceptance tests failed".to_string()),
        None => {}
    }
    if !changed_files.is_empty() {
        items.push("changed files observed".to_string());
    }
    items
}

#[cfg(test)]
mod tests {
    use ninmu_runtime::harness_contract::{HarnessArtifact, HarnessTestReport, HarnessTestStatus};

    use super::decide;

    fn test_report(status: HarnessTestStatus) -> HarnessTestReport {
        HarnessTestReport {
            command: "cargo test".to_string(),
            status,
            exit_code: Some(0),
            output: None,
            artifact: None::<HarnessArtifact>,
        }
    }

    #[test]
    fn passing_tests_and_changed_files_have_high_confidence() {
        let decision = decide(
            &[test_report(HarnessTestStatus::Passed)],
            &["src/lib.rs".to_string()],
            &[],
        );

        assert_eq!(decision.status_reason, "runtime completed");
        assert_eq!(decision.confidence.level, "high");
        assert_eq!(decision.recommendation, "accept");
    }

    #[test]
    fn completed_without_acceptance_evidence_needs_review() {
        let decision = decide(&[], &[], &[]);

        assert_eq!(decision.confidence.level, "low");
        assert_eq!(decision.recommendation, "review");
    }

    #[test]
    fn failed_tests_return_failed_retry_decision() {
        let decision = decide(&[test_report(HarnessTestStatus::Failed)], &[], &[]);

        assert_eq!(
            decision.status,
            ninmu_runtime::harness_contract::HarnessTaskStatus::Failed
        );
        assert_eq!(decision.recommendation, "retry");
    }

    #[test]
    fn unresolved_blocker_returns_blocked_decision() {
        let decision = decide(&[], &[], &["needs human input".to_string()]);

        assert_eq!(
            decision.status,
            ninmu_runtime::harness_contract::HarnessTaskStatus::Blocked
        );
        assert_eq!(decision.recommendation, "block");
    }
}
