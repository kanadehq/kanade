//! `job.tail.<pc_id>` request/reply handler.
//!
//! On-demand only — the backend (on the SPA's behalf) sends a
//! [`JobTailRequest`] with a `result_id` and the agent replies with
//! the current ring-buffer tail of that in-flight job's stdout/stderr
//! from the in-memory [`crate::live_tail`] registry. We never push
//! live output upstream on our own; the SPA polls this while a job is
//! running (same shape as `logs.fetch.<pc_id>`).
//!
//! When the agent holds no live buffer for the `result_id` (the run
//! never started here, or it finished and was evicted past the grace
//! window), the reply carries `found = false` so the backend falls
//! back to the persisted `execution_results` row.

use futures::StreamExt;
use kanade_shared::subject;
use kanade_shared::wire::{JobTailReply, JobTailRequest};
use tracing::{info, warn};

/// Subscribe to `job.tail.<pc_id>` and serve every incoming request.
/// Each request is a cheap in-memory lookup, so we serve them inline
/// on this task — no fan-out spawning needed.
pub async fn serve(client: async_nats::Client, pc_id: String, tracker: crate::staleness::Tracker) {
    let subject = subject::job_tail(&pc_id);

    // Outer reconnect loop mirrors `logs::serve` — a transient
    // subscribe failure or a closed subscription must not kill the
    // handler permanently.
    loop {
        let mut sub =
            crate::nats_retry::wait_for_subscribe(&client, &tracker, &subject, "job_tail").await;
        info!(subject = %subject, "job.tail handler ready");

        while let Some(msg) = sub.next().await {
            let Some(reply) = msg.reply.clone() else {
                warn!("job.tail without reply subject — caller must use request/reply");
                continue;
            };
            let req: JobTailRequest = match serde_json::from_slice(&msg.payload) {
                Ok(r) => r,
                Err(e) => {
                    // A malformed payload still gets a safe `found=false`
                    // reply (default result_id ""), but warn so a broken
                    // caller leaves a trace — consistent with the other
                    // deserialize sites in the agent.
                    warn!(error = %e, "job.tail: deserialize JobTailRequest; replying not-found");
                    JobTailRequest::default()
                }
            };
            let body = build_reply(&req);
            let payload = match serde_json::to_vec(&body) {
                Ok(p) => p,
                Err(e) => {
                    warn!(error = %e, "encode JobTailReply");
                    continue;
                }
            };
            if let Err(e) = client.publish(reply, payload.into()).await {
                warn!(error = %e, "publish job.tail reply");
            }
        }
        warn!(subject = %subject, "job.tail subscription closed; reopening");
        crate::nats_retry::reopen_pause().await;
    }
}

/// Build the reply for one request: look up the live buffer and
/// snapshot it, or report `found = false`.
fn build_reply(req: &JobTailRequest) -> JobTailReply {
    match crate::live_tail::get(&req.result_id) {
        Some(tail) => {
            let snap = tail.snapshot();
            JobTailReply {
                found: true,
                running: snap.running,
                stdout: snap.stdout,
                stderr: snap.stderr,
                stdout_truncated: snap.stdout_truncated,
                stderr_truncated: snap.stderr_truncated,
            }
        }
        None => JobTailReply {
            found: false,
            ..Default::default()
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_result_id_is_not_found() {
        let reply = build_reply(&JobTailRequest {
            result_id: "no-such-job".into(),
        });
        assert!(!reply.found);
        assert!(!reply.running);
    }

    #[test]
    fn live_result_id_is_found_and_running() {
        let handle = crate::live_tail::register("job-tail-serve-test");
        handle.tail().push_stdout(b"progress 50%");
        let reply = build_reply(&JobTailRequest {
            result_id: "job-tail-serve-test".into(),
        });
        assert!(reply.found);
        assert!(reply.running);
        assert_eq!(reply.stdout, "progress 50%");
    }
}
