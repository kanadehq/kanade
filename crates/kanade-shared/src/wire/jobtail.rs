//! Wire types for the live job-tail path
//! (`job.tail.<pc_id>` NATS request/reply).
//!
//! Operator / backend → agent: [`JobTailRequest`] (the `result_id`
//! of an in-flight run). Agent → operator: [`JobTailReply`] carrying
//! the current ring-buffer tail of that job's stdout/stderr.
//!
//! Unlike `logs.fetch.<pc_id>` (which tails the agent's whole rolling
//! log file), this is scoped to a single job's captured output and
//! only ever returns data while the run is live (plus a short grace
//! window after it finishes — see `kanade-agent::live_tail`). Once the
//! agent has dropped the live buffer, `found = false` and the caller
//! falls back to the persisted `execution_results` row.

use serde::{Deserialize, Serialize};

/// Request the live tail of a single job's output. The `result_id`
/// is the agent-minted per-PC UUID that keys `execution_results`, so
/// the SPA already has it from the Activity row / detail page.
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
#[serde(default)]
pub struct JobTailRequest {
    pub result_id: String,
}

/// Agent's reply with the current captured tail.
///
/// `found = false` means the agent has no live buffer for this
/// `result_id` (never started here, or already evicted past the grace
/// window) — the caller should fall back to the stored result row.
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
#[serde(default)]
pub struct JobTailReply {
    /// The agent currently holds a live buffer for this `result_id`.
    pub found: bool,
    /// The child process is still executing. `false` once it has
    /// exited but the buffer is being retained through the grace
    /// window so the final tail is still serveable.
    pub running: bool,
    /// stdout tail (lossy UTF-8). Bounded by the agent's ring cap.
    pub stdout: String,
    /// stderr tail (lossy UTF-8). Bounded by the agent's ring cap.
    pub stderr: String,
    /// The ring dropped older stdout bytes to stay under the cap, so
    /// `stdout` is a suffix of the real output, not the whole of it.
    pub stdout_truncated: bool,
    /// As `stdout_truncated`, for stderr.
    pub stderr_truncated: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_round_trips() {
        let req = JobTailRequest {
            result_id: "abc-123".into(),
        };
        let json = serde_json::to_string(&req).unwrap();
        let back: JobTailRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(back.result_id, "abc-123");
    }

    #[test]
    fn request_missing_field_defaults_empty() {
        let req: JobTailRequest = serde_json::from_str("{}").unwrap();
        assert_eq!(req.result_id, "");
    }

    #[test]
    fn reply_defaults_are_not_found() {
        let reply = JobTailReply::default();
        assert!(!reply.found);
        assert!(!reply.running);
        assert!(reply.stdout.is_empty());
    }

    #[test]
    fn reply_round_trips() {
        let reply = JobTailReply {
            found: true,
            running: true,
            stdout: "hello".into(),
            stderr: "".into(),
            stdout_truncated: true,
            stderr_truncated: false,
        };
        let json = serde_json::to_string(&reply).unwrap();
        let back: JobTailReply = serde_json::from_str(&json).unwrap();
        assert!(back.found);
        assert!(back.running);
        assert_eq!(back.stdout, "hello");
        assert!(back.stdout_truncated);
    }
}
