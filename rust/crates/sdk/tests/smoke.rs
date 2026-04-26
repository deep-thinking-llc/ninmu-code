//! Integration smoke test for the SDK crate.
//!
//! Verifies that the public API compiles and basic session construction works.

use sdk::{AgentSession, ToolRegistry};
use runtime::PermissionMode;

#[test]
#[ignore = "requires a working provider in the environment"]
fn sdk_session_can_be_constructed() {
    let (mut session, _event_bus) = AgentSession::new(
        "claude-sonnet-4-6",
        vec!["You are a helpful assistant.".to_string()],
        ToolRegistry::new(),
        PermissionMode::DangerFullAccess,
    )
    .expect("session should construct");

    let result = session.run_turn("Hello from integration test");
    // Without a real provider this will fail, but we verify the API compiles
    assert!(result.is_err());
}

#[test]
fn tool_registry_is_default_constructable() {
    let registry = ToolRegistry::new();
    assert!(registry.tool_names().is_empty());
}
