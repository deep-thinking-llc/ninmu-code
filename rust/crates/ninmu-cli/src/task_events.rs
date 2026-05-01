use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use ninmu_runtime::harness_contract::{
    HarnessEvent, HarnessEventKind, HarnessProtocolVersion, MAX_EVENT_PAYLOAD_BYTES,
};
use serde_json::{json, Value};

pub(crate) struct TaskEventSink {
    writer: Option<File>,
    event_log: Option<PathBuf>,
    sequence: u64,
}

impl TaskEventSink {
    pub(crate) fn disabled() -> Self {
        Self {
            writer: None,
            event_log: None,
            sequence: 0,
        }
    }

    pub(crate) fn file(path: PathBuf) -> io::Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        Ok(Self {
            writer: Some(File::create(&path)?),
            event_log: Some(path),
            sequence: 0,
        })
    }

    pub(crate) fn emit(
        &mut self,
        mission_id: &str,
        task_id: &str,
        kind: &str,
        payload: Value,
    ) -> io::Result<()> {
        if self.writer.is_none() {
            return Ok(());
        }
        self.sequence += 1;
        let payload = self.bound_payload(task_id, kind, payload)?;
        let event = HarnessEvent {
            protocol: HarnessProtocolVersion::V1Alpha1,
            mission_id: mission_id.to_string(),
            task_id: task_id.to_string(),
            event_id: format!("{task_id}-event-{}", self.sequence),
            sequence: self.sequence,
            timestamp: unix_timestamp_string(),
            kind: HarnessEventKind::new(kind.to_string()),
            payload,
        };
        let writer = self.writer.as_mut().expect("writer checked");
        serde_json::to_writer(&mut *writer, &event)?;
        writer.write_all(b"\n")
    }

    fn bound_payload(&self, task_id: &str, kind: &str, payload: Value) -> io::Result<Value> {
        let size = serde_json::to_vec(&payload)?.len();
        if size <= MAX_EVENT_PAYLOAD_BYTES {
            return Ok(payload);
        }
        let Some(event_log) = &self.event_log else {
            return Ok(json!({"omitted": "payload exceeded inline event limit"}));
        };
        let artifact_dir = event_log.with_extension("artifacts");
        fs::create_dir_all(&artifact_dir)?;
        let artifact_path = artifact_dir.join(format!(
            "{}-{}-{}.json",
            task_id,
            kind.replace('.', "-"),
            self.sequence + 1
        ));
        fs::write(&artifact_path, serde_json::to_vec(&payload)?)?;
        Ok(json!({
            "artifact": {
                "path": display_path(&artifact_path),
                "kind": "event_payload",
                "description": "event payload exceeded inline limit"
            }
        }))
    }
}

fn unix_timestamp_string() -> String {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs());
    seconds.to_string()
}

fn display_path(path: &Path) -> String {
    path.display().to_string()
}

#[cfg(test)]
mod tests {
    use std::fs;

    use serde_json::json;

    use super::TaskEventSink;

    #[test]
    fn oversized_payload_is_written_as_artifact_reference() {
        let root = std::env::temp_dir().join(format!("ninmu-event-sink-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("temp root should exist");
        let event_log = root.join("events.ndjson");
        let mut sink = TaskEventSink::file(event_log.clone()).expect("sink should open");

        sink.emit(
            "mission",
            "task",
            "tool.completed",
            json!({"output": "x".repeat(70 * 1024)}),
        )
        .expect("event should emit");

        let line = fs::read_to_string(event_log).expect("event log should exist");
        let event: serde_json::Value =
            serde_json::from_str(line.trim()).expect("event should parse");
        let artifact = event["payload"]["artifact"]["path"]
            .as_str()
            .expect("artifact path should exist");
        assert!(std::path::Path::new(artifact).exists());
    }
}
