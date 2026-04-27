//! Agent-to-agent message bus with typed channels.
//!
//! The [`MessageBus`] enables agents to publish and subscribe to typed messages
//! on named topics. Each topic is backed by a `tokio::sync::broadcast` channel.
//! A ring-buffer history retains recent messages for late subscribers.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use serde_json::Value;

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
}

impl MessageBus {
    /// Create a new message bus with the given history retention limit.
    #[must_use]
    pub fn new(max_history: usize) -> Self {
        Self {
            channels: Arc::new(Mutex::new(HashMap::new())),
            history: Arc::new(Mutex::new(VecDeque::with_capacity(max_history))),
            max_history,
        }
    }

    /// Publish a message to all subscribers of a topic.
    ///
    /// The message is recorded in the history ring-buffer and sent to all
    /// active receivers on the topic's broadcast channel.
    pub fn publish(&self, topic: &str, mut message: AgentMessage) {
        message.channel = topic.to_string();

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
        let tx = channels
            .entry(topic.to_string())
            .or_insert_with(|| {
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
        history.iter().filter(|m| m.channel == topic).cloned().collect()
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
    use tokio;

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
        bus.publish("test.topic", test_message("alice", "test.topic"));
        let msg = rx.recv().await.expect("should receive message");
        assert_eq!(msg.from_agent, "alice");
    }

    #[tokio::test]
    async fn multiple_subscribers() {
        let bus = MessageBus::new(100);
        let mut rx1 = bus.subscribe("test.topic");
        let mut rx2 = bus.subscribe("test.topic");
        bus.publish("test.topic", test_message("alice", "test.topic"));
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
        bus.publish("topic.a", test_message("alice", "topic.a"));
        let msg_a = rx_a.recv().await.expect("topic.a should receive");
        assert_eq!(msg_a.from_agent, "alice");
        // rx_b should NOT receive this message; use select with timeout
        let result = tokio::time::timeout(
            std::time::Duration::from_millis(50),
            rx_b.recv(),
        )
        .await;
        assert!(result.is_err(), "topic.b should not receive topic.a messages");
    }

    #[tokio::test]
    async fn broadcast_message() {
        let bus = MessageBus::new(100);
        let mut rx = bus.subscribe_all();
        bus.publish("some.topic", test_message("alice", "some.topic"));
        let msg = rx.recv().await.expect("should receive broadcast");
        assert_eq!(msg.from_agent, "alice");
    }

    #[tokio::test]
    async fn correlation_id_tracking() {
        let bus = MessageBus::new(100);
        let mut rx = bus.subscribe("tasks");
        let mut msg = test_message("alice", "tasks");
        msg.correlation_id = Some("task-42".into());
        bus.publish("tasks", msg);
        let received = rx.recv().await.expect("should receive");
        assert_eq!(received.correlation_id.as_deref(), Some("task-42"));
    }

    #[tokio::test]
    async fn history_retention() {
        let bus = MessageBus::new(3);
        for i in 0..5 {
            let mut msg = test_message(&format!("agent-{i}"), "topic");
            msg.payload = serde_json::json!({"i": i});
            bus.publish("topic", msg);
        }
        let history = bus.history("topic");
        assert_eq!(history.len(), 3);
        assert_eq!(history[0].from_agent, "agent-2");
        assert_eq!(history[2].from_agent, "agent-4");
    }

    #[test]
    fn unsubscribe_dropped() {
        let bus = MessageBus::new(100);
        let rx = bus.subscribe("test");
        drop(rx);
        // After dropping the receiver, publishing should not panic
        bus.publish("test", test_message("alice", "test"));
        // Channel still exists but has no active receivers
        assert!(bus.topic_count() > 0);
    }
}
