//! Integration smoke test for the telemetry crate.
//!
//! Verifies that the public API can be exercised without linking errors
//! and that events round-trip through the memory sink.

use telemetry::{
    AnalyticsEvent, ClientIdentity, MemoryTelemetrySink, SessionTracer, TelemetryEvent,
};

#[test]
fn telemetry_events_round_trip_through_memory_sink() {
    let sink = std::sync::Arc::new(MemoryTelemetrySink::default());
    let tracer = SessionTracer::new("integration-session", sink.clone());

    tracer.record_http_request_started(1, "POST", "/v1/messages", Default::default());
    tracer.record_http_request_succeeded(1, "POST", "/v1/messages", 200, None, Default::default());
    tracer.record_analytics(
        AnalyticsEvent::new("cli", "turn_completed").with_property("ok", serde_json::json!(true)),
    );

    let events = sink.events();
    assert_eq!(events.len(), 6);
    assert!(matches!(
        events[0],
        TelemetryEvent::HttpRequestStarted { .. }
    ));
    assert!(
        matches!(events[1], TelemetryEvent::SessionTrace { .. }),
        "trace should follow the started event"
    );
    assert!(matches!(
        events[2],
        TelemetryEvent::HttpRequestSucceeded { .. }
    ));
    assert!(matches!(events[3], TelemetryEvent::SessionTrace { .. }));
    assert!(matches!(events[4], TelemetryEvent::Analytics { .. }));
    assert!(matches!(events[5], TelemetryEvent::SessionTrace { .. }));
}

#[test]
fn client_identity_default_is_populated() {
    let identity = ClientIdentity::default();
    assert!(identity.app_name.len() > 0);
    assert!(identity.app_version.len() > 0);
    assert_eq!(identity.runtime, "rust");
}
