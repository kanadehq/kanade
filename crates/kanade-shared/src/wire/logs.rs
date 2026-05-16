//! Wire types for the on-demand log fetch path
//! (`logs.fetch.<pc_id>` NATS request/reply).
//!
//! Operator → agent: `LogsRequest`. Agent → operator: raw UTF-8 log
//! bytes (the tail of the most-recent rolling file). Replies are
//! capped well under the 1 MB NATS payload limit by the requested
//! line count; for full-file pulls a future variant will fall back
//! to JetStream Object Store.

use serde::{Deserialize, Serialize};

/// Payload sent on `logs.fetch.<pc_id>` to request the tail of an
/// agent's log file. Unknown / missing fields fall back to sensible
/// defaults so the CLI / Web UI can grow the shape later without
/// retraining every fleet host.
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(default)]
pub struct LogsRequest {
    /// Number of trailing lines to return from the most recent log
    /// file. Defaults to 500 — fits comfortably in a single NATS
    /// reply.
    pub tail_lines: u32,
}

impl Default for LogsRequest {
    fn default() -> Self {
        Self { tail_lines: 500 }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_to_500_lines() {
        assert_eq!(LogsRequest::default().tail_lines, 500);
    }

    #[test]
    fn missing_field_round_trips_to_default() {
        let req: LogsRequest = serde_json::from_str("{}").unwrap();
        assert_eq!(req.tail_lines, 500);
    }

    #[test]
    fn explicit_field_overrides_default() {
        let req: LogsRequest = serde_json::from_str(r#"{"tail_lines": 100}"#).unwrap();
        assert_eq!(req.tail_lines, 100);
    }
}
