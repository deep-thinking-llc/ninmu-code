//! Agent-to-agent message bus with typed channels.
//!
//! The [`MessageBus`] enables agents to publish and subscribe to typed messages
//! on named topics. Each topic is backed by a `tokio::sync::broadcast` channel.
//! A ring-buffer history retains recent messages for late subscribers.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::security::SecretScrubber;

/// Token that authenticates a publisher on the message bus.
#[derive(Debug, Clone)]
pub struct PublisherToken {
    pub agent_id: String,
}

/// A strongly-typed message that agents can send to each other.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentMessage {
    /// The agent that published this message.
    pub from_agent: String,
    /// Optional specific recipient. `None` means broadcast to all subscribers.
    pub to_agent: Option<String>,
    /// The topic/channel name.
    pub channel: String,
    /// Arbitrary JSON payload.
    pub payload: Value,
    /// Unix-epoch milliseconds timestamp.
    pub timestamp_ms: u64,
    /// Optional correlation ID for tracking multi-step workflows.
    pub correlation_id: Option<String>,
}

/// Agent-to-agent message bus owned by the orchestrator.
#[derive(Debug)]
pub struct MessageBus {
    channels: Arc<Mutex<HashMap<String, tokio::sync::broadcast::Sender<AgentMessage>>>>,
    history: Arc<Mutex<VecDeque<AgentMessage>>>,
    max_history: usize,
    scrubber: Option<SecretScrubber>,
}

impl MessageBus {
    /// Create a new message bus with the given history retention limit.
    #[must_use]
    pub fn new(max_history: usize) -> Self {
        Self {
            channels: Arc::new(Mutex::new(HashMap::new())),
            history: Arc::new(Mutex::new(VecDeque::with_capacity(max_history))),
            max_history,
            scrubber: None,
        }
    }

    /// Enable secret scrubbing on message payloads before storing history.
    #[must_use]
    pub fn with_scrubber(mut self, scrubber: SecretScrubber) -> Self {
        self.scrubber = Some(scrubber);
        self
    }

    /// Publish a message to all subscribers of a topic.
    ///
    /// The `token` MUST belong to the same agent as `message.from_agent`.
    pub fn publish(&self, token: &PublisherToken, topic: &str, mut message: AgentMessage) {
        if message.from_agent != token.agent_id {
            return; // silently drop impersonation attempts
        }
        message.channel = topic.to_string();

        // Scrub secrets from payload before storing history
        if let Some(scrubber) = &self.scrubber {
            if let Some(payload_str) = message.payload.as_str() {
                let (scrubbed, _) = scrubber.scrub(payload_str);
                message.payload = Value::String(scrubbed);
            }
        }

        // Record in history
        {
            let mut history = self.history.lock().expect("history lock");
            history.push_back(message.clone());
            while history.len() > self.max_history {
                history.pop_front();
            }
        }

        // Send to subscribers
        let channels = self.channels.lock().expect("channels lock");

        // Send to topic-specific subscribers
        if let Some(tx) = channels.get(topic) {
            let _ = tx.send(message.clone());
        }

        // Also forward to wildcard subscribers
        if let Some(tx) = channels.get("__wildcard__") {
            let _ = tx.send(message);
        }
    }

    /// Subscribe to a specific topic.
    ///
    /// Returns a receiver that will receive all future messages on this topic.
    /// If no channel exists for the topic yet, one is created.
    #[must_use]
    pub fn subscribe(&self, topic: &str) -> tokio::sync::broadcast::Receiver<AgentMessage> {
        let mut channels = self.channels.lock().expect("channels lock");
        let tx = channels.entry(topic.to_string()).or_insert_with(|| {
            let (tx, _rx) = tokio::sync::broadcast::channel(256);
            tx
        });
        tx.subscribe()
    }

    /// Subscribe to all topics (wildcard).
    ///
    /// Returns a receiver on a dedicated "wildcard" channel. All messages
    /// published via [`publish`](Self::publish) are also forwarded here.
    #[must_use]
    pub fn subscribe_all(&self) -> tokio::sync::broadcast::Receiver<AgentMessage> {
        let mut channels = self.channels.lock().expect("channels lock");
        let tx = channels
            .entry("__wildcard__".to_string())
            .or_insert_with(|| {
                let (tx, _rx) = tokio::sync::broadcast::channel(256);
                tx
            });
        tx.subscribe()
    }

    /// Return the recent message history for a specific topic.
    #[must_use]
    pub fn history(&self, topic: &str) -> Vec<AgentMessage> {
        let history = self.history.lock().expect("history lock");
        history
            .iter()
            .filter(|m| m.channel == topic)
            .cloned()
            .collect()
    }

    /// Return the recent message history across all topics.
    #[must_use]
    pub fn history_all(&self) -> Vec<AgentMessage> {
        let history = self.history.lock().expect("history lock");
        history.iter().cloned().collect()
    }

    /// Return the number of tracked topics.
    #[must_use]
    pub fn topic_count(&self) -> usize {
        let channels = self.channels.lock().expect("channels lock");
        channels.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn alice_token() -> PublisherToken {
        PublisherToken {
            agent_id: "alice".to_string(),
        }
    }

    fn bob_token() -> PublisherToken {
        PublisherToken {
            agent_id: "bob".to_string(),
        }
    }

    fn test_message(from: &str, topic: &str) -> AgentMessage {
        AgentMessage {
            from_agent: from.to_string(),
            to_agent: None,
            channel: topic.to_string(),
            payload: Value::Null,
            timestamp_ms: 1000,
            correlation_id: None,
        }
    }

    #[tokio::test]
    async fn publish_subscribe_roundtrip() {
        let bus = MessageBus::new(100);
        let mut rx = bus.subscribe("test.topic");
        let token = alice_token();
        bus.publish(&token, "test.topic", test_message("alice", "test.topic"));
        let msg = rx.recv().await.expect("should receive message");
        assert_eq!(msg.from_agent, "alice");
    }

    #[tokio::test]
    async fn multiple_subscribers() {
        let bus = MessageBus::new(100);
        let mut rx1 = bus.subscribe("test.topic");
        let mut rx2 = bus.subscribe("test.topic");
        let token = alice_token();
        bus.publish(&token, "test.topic", test_message("alice", "test.topic"));
        let msg1 = rx1.recv().await.expect("rx1 should receive");
        let msg2 = rx2.recv().await.expect("rx2 should receive");
        assert_eq!(msg1.from_agent, "alice");
        assert_eq!(msg2.from_agent, "alice");
    }

    #[tokio::test]
    async fn topic_filtering() {
        let bus = MessageBus::new(100);
        let mut rx_a = bus.subscribe("topic.a");
        let mut rx_b = bus.subscribe("topic.b");
        let token = alice_token();
        bus.publish(&token, "topic.a", test_message("alice", "topic.a"));
        let msg_a = rx_a.recv().await.expect("topic.a should receive");
        assert_eq!(msg_a.from_agent, "alice");
        let result = tokio::time::timeout(std::time::Duration::from_millis(50), rx_b.recv()).await;
        assert!(
            result.is_err(),
            "topic.b should not receive topic.a messages"
        );
    }

    #[tokio::test]
    async fn broadcast_message() {
        let bus = MessageBus::new(100);
        let mut rx = bus.subscribe_all();
        let token = alice_token();
        bus.publish(&token, "some.topic", test_message("alice", "some.topic"));
        let msg = rx.recv().await.expect("should receive broadcast");
        assert_eq!(msg.from_agent, "alice");
    }

    #[tokio::test]
    async fn correlation_id_tracking() {
        let bus = MessageBus::new(100);
        let mut rx = bus.subscribe("tasks");
        let mut msg = test_message("alice", "tasks");
        msg.correlation_id = Some("task-42".into());
        let token = alice_token();
        bus.publish(&token, "tasks", msg);
        let received = rx.recv().await.expect("should receive");
        assert_eq!(received.correlation_id.as_deref(), Some("task-42"));
    }

    #[tokio::test]
    async fn history_retention() {
        let bus = MessageBus::new(3);
        let token = alice_token();
        for i in 0..5 {
            let mut msg = test_message("alice", "topic");
            msg.payload = serde_json::json!({"i": i});
            bus.publish(&token, "topic", msg);
        }
        let history = bus.history("topic");
        assert_eq!(history.len(), 3);
        assert_eq!(history[0].payload["i"], 2);
        assert_eq!(history[2].payload["i"], 4);
    }

    #[test]
    fn unsubscribe_dropped() {
        let bus = MessageBus::new(100);
        let rx = bus.subscribe("test");
        drop(rx);
        let token = alice_token();
        bus.publish(&token, "test", test_message("alice", "test"));
        assert!(bus.topic_count() > 0);
    }

    #[test]
    fn wrong_agent_id_rejected() {
        let bus = MessageBus::new(100);
        let token = alice_token(); // alice token
        let mut msg = test_message("bob", "test"); // but message says from bob
        msg.payload = serde_json::json!("payload");
        bus.publish(&token, "test", msg);
        // No subscriber, but history should be empty because message was dropped
        let history = bus.history("test");
        assert!(history.is_empty(), "impersonation should be dropped");
    }

    #[test]
    fn history_scrubs_secrets() {
        let scrubber = SecretScrubber::default();
        let bus = MessageBus::new(10).with_scrubber(scrubber);
        let token = alice_token();
        let mut msg = test_message("alice", "test");
        msg.payload = serde_json::json!("key=sk-ant-api03-secret1234567890abcdef");
        bus.publish(&token, "test", msg);
        let history = bus.history("test");
        assert_eq!(history.len(), 1);
        assert!(
            history[0].payload.as_str().unwrap().contains("[REDACTED]"),
            "secret should be scrubbed in history"
        );
    }
}
