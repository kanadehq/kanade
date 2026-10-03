//! The one receive pipeline for commands delivered over NATS, shared by the
//! live subscriptions and the JetStream replay consumer.
//!
//! The order is the point:
//!
//! 1. is the command addressed to this host,
//! 2. does its provenance verify (the existing verifier and enforcement),
//! 3. admit it to the durable ledger,
//! 4. only then acknowledge the delivery.
//!
//! Verification precedes admission so that a message that fails it can never
//! reserve a legitimate request id. Acknowledgement follows admission so a
//! message is never consumed without having been either recorded or refused on
//! a terminal decision. A ledger that cannot be written is neither: the message
//! is left unacknowledged and nothing is launched.

use std::sync::Arc;

use futures::future::BoxFuture;
use kanade_shared::signing::SigHeaders;
use kanade_shared::wire::Command;
use tracing::{debug, error, info, warn};

use crate::admission_ledger::{
    AdmitOutcome, Ledger, LedgerError, NewAdmission, RecoveredPending, Standing, Ticket,
    fingerprint_of,
};
use crate::command_verify::{Admission, Verifier};

/// Acknowledges one delivery. A core subscription has nothing to acknowledge;
/// a JetStream message does, and *when* is what this module exists to control.
pub trait Acker: Send + Sync {
    fn ack(&self) -> BoxFuture<'_, ()>;
}

/// For core NATS deliveries, which carry no acknowledgement.
pub struct NoAck;

impl Acker for NoAck {
    fn ack(&self) -> BoxFuture<'_, ()> {
        Box::pin(async {})
    }
}

/// Acknowledges a JetStream message.
pub struct JsAck<'a>(pub &'a async_nats::jetstream::Message);

impl Acker for JsAck<'_> {
    fn ack(&self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            if let Err(e) = self.0.ack().await {
                warn!(error = %e, "could not acknowledge a command delivery");
            }
        })
    }
}

/// One received message, reduced to what the pipeline needs.
pub struct Delivery<'a> {
    pub subject: &'a str,
    pub payload: &'a [u8],
    pub headers: SigHeaders,
    /// Whether the subject addresses this host. The live subscriptions are
    /// created for this host's own subjects, so they pass `true`; the replay
    /// consumer re-checks against the current group membership.
    pub addressed: bool,
}

/// An admitted command, ready to run under its ticket.
pub struct Admitted {
    pub ticket: Ticket,
    pub cmd: Command,
    pub envelope_deadline: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Settled {
    NotForMe,
    Undecodable,
    /// Refused by the verifier. Nothing is recorded.
    Refused,
    /// An id that was admitted before.
    Duplicate,
    /// An id that was admitted before, now with different bytes.
    Conflict,
}

pub enum Intake {
    /// Launch it. Admitted and acknowledged.
    Launch(Box<Admitted>),
    /// A terminal decision was made and the delivery acknowledged.
    Settled(Settled),
    /// The ledger could not be written. Nothing was admitted, launched or
    /// acknowledged.
    Held,
}

pub async fn process_delivery(
    d: &Delivery<'_>,
    verifier: &Verifier,
    ledger: &Arc<Ledger>,
    acker: &dyn Acker,
) -> Intake {
    // Addressed-to-me runs before provenance: a refusal now produces a result
    // under this host's pc_id, and a command that was never this host's must
    // not make it report one.
    if !d.addressed {
        warn!(
            subject = d.subject,
            "command not addressed to this agent; dropping",
        );
        acker.ack().await;
        return Intake::Settled(Settled::NotForMe);
    }

    let (cmd, envelope_deadline) = match verifier.admit(d.payload, &d.headers, d.subject) {
        Admission::Run {
            cmd,
            envelope_deadline,
        } => (cmd, envelope_deadline),
        Admission::Undecodable(e) => {
            warn!(error = %e, subject = d.subject, "deserialize command");
            acker.ack().await;
            return Intake::Settled(Settled::Undecodable);
        }
        Admission::RefusedLegacy { cmd, reason } => {
            warn!(
                request_id = %cmd.request_id,
                subject = d.subject,
                reason,
                "REFUSED: command did not verify",
            );
            // Nothing is written to the ledger: a refusal must not consume the
            // request id, because the operator's fix (provision the key,
            // correct the clock, sign it properly) produces a retry that can
            // legitimately carry the same id.
            let _ = crate::commands::publish_signature_refused(
                ledger.outbox_dir().to_path_buf(),
                ledger.pc_id(),
                &cmd,
                reason,
            )
            .await;
            acker.ack().await;
            return Intake::Settled(Settled::Refused);
        }
        // Logged and reported by `admit`; no result for a host that may never
        // have been the recipient, and the id stays unconsumed.
        Admission::Refused => {
            acker.ack().await;
            return Intake::Settled(Settled::Refused);
        }
    };

    let request_id = cmd.request_id.clone();
    let fingerprint = fingerprint_of(d.payload);
    let new = NewAdmission {
        command: cmd.clone(),
        envelope_deadline,
        subject: d.subject.to_string(),
        payload: d.payload.to_vec(),
        headers: d.headers.clone(),
    };
    let l = ledger.clone();
    let admitted = tokio::task::spawn_blocking(move || l.admit(new))
        .await
        .unwrap_or_else(|e| Err(LedgerError::Io(format!("admission task failed: {e}"))));

    match admitted {
        Ok(AdmitOutcome::Admitted(ticket)) => {
            ledger.report_healthy();
            acker.ack().await;
            Intake::Launch(Box::new(Admitted {
                ticket,
                cmd,
                envelope_deadline,
            }))
        }
        Ok(AdmitOutcome::Duplicate(standing)) => {
            ledger.report_healthy();
            debug!(
                request_id = %request_id,
                ?standing,
                "duplicate delivery of an admitted command; not launching again",
            );
            // A finished run whose outcome never reached the upload queue is
            // queued now. Never a relaunch.
            if let Standing::Finished {
                outbox_enqueued: false,
                ..
            } = standing
            {
                let l = ledger.clone();
                let rid = request_id.clone();
                match tokio::task::spawn_blocking(move || l.republish(&rid)).await {
                    Ok(Ok(_)) => {}
                    Ok(Err(e)) => {
                        warn!(request_id = %request_id, error = %e, "could not re-queue the stored outcome; the maintenance task will retry")
                    }
                    Err(e) => warn!(request_id = %request_id, error = %e, "re-queue task failed"),
                }
            }
            acker.ack().await;
            Intake::Settled(Settled::Duplicate)
        }
        Ok(AdmitOutcome::Conflict) => {
            ledger.report_healthy();
            error!(
                request_id = %request_id,
                subject = d.subject,
                fingerprint = %fingerprint,
                "SECURITY: a command arrived with an admitted request id but different bytes; not executed",
            );
            ledger.emit_obs(
                "command_admission_conflict",
                format!("conflict:{request_id}:{}", &fingerprint[..16]),
                serde_json::json!({
                    "request_id": request_id,
                    "subject": d.subject,
                    "fingerprint": fingerprint,
                }),
            );
            acker.ack().await;
            Intake::Settled(Settled::Conflict)
        }
        Err(e) => {
            ledger.report_failure(&request_id, &e);
            Intake::Held
        }
    }
}

/// Re-verify and run what the ledger holds as admitted-but-never-started, now,
/// against the keys trusted now. The start gates (deadline, staleness,
/// revocation, version pin) are re-evaluated by the same path every command
/// takes; none of this needs the broker to be reachable.
pub async fn run_recovered(
    pending: Vec<RecoveredPending>,
    client: async_nats::Client,
    pc_id: String,
    verifier: Arc<Verifier>,
    staleness: crate::staleness::Tracker,
    script_cache: crate::script_cache::ScriptCache,
    check_sink: crate::check_cache::CheckSink,
) {
    if pending.is_empty() {
        return;
    }
    info!(
        count = pending.len(),
        "recovering admitted commands that never started"
    );
    let jetstream = async_nats::jetstream::new(client.clone());
    let script_current = jetstream
        .get_key_value(kanade_shared::kv::BUCKET_SCRIPT_CURRENT)
        .await
        .ok();
    let script_status = jetstream
        .get_key_value(kanade_shared::kv::BUCKET_SCRIPT_STATUS)
        .await
        .ok();
    for p in pending {
        let (cmd, envelope_deadline) = match verifier.admit(&p.payload, &p.headers, &p.subject) {
            Admission::Run {
                cmd,
                envelope_deadline,
            } => (cmd, envelope_deadline),
            // The keys trusted now no longer vouch for it (rotation, revocation,
            // a signature that has aged out). Close the record with a visible
            // refusal; leaving it pending would hold the id for ever.
            refused => {
                let reason = match refused {
                    Admission::RefusedLegacy { reason, .. } => reason,
                    _ => "no longer verifies",
                };
                warn!(
                    request_id = %p.command.request_id,
                    reason,
                    "REFUSED: recovered command no longer verifies",
                );
                let result = crate::commands::signature_refusal_result(
                    &pc_id,
                    &p.command,
                    reason,
                    chrono::Utc::now(),
                );
                let ticket = p.ticket;
                let _ = tokio::task::spawn_blocking(move || ticket.finish_keep_id(result)).await;
                continue;
            }
        };
        let (client, pc_id) = (client.clone(), pc_id.clone());
        let (cur, sta) = (script_current.clone(), script_status.clone());
        let (stl, sc, cs) = (staleness.clone(), script_cache.clone(), check_sink.clone());
        tokio::spawn(async move {
            if let Err(e) = crate::commands::handle_command(
                client,
                pc_id,
                cmd,
                cur,
                sta,
                stl,
                sc,
                cs,
                crate::commands::CommandSource::Nats,
                envelope_deadline,
                Some(p.ticket),
            )
            .await
            {
                error!(error = %e, "recovered command handler failed");
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::admission_ledger::tests_support::Fixture;
    use ed25519_dalek::SigningKey;
    use kanade_shared::signing::sign;
    use std::sync::Mutex;

    /// Records every ack along with what the ledger held at that moment, so
    /// "acknowledged only after admission" is an assertion about order rather
    /// than about the final state.
    struct SpyAck {
        ledger: Arc<Ledger>,
        seen: Mutex<Vec<usize>>,
    }

    impl SpyAck {
        fn new(ledger: &Arc<Ledger>) -> Self {
            Self {
                ledger: ledger.clone(),
                seen: Mutex::new(Vec::new()),
            }
        }
        fn acks(&self) -> Vec<usize> {
            self.seen.lock().unwrap().clone()
        }
    }

    impl Acker for SpyAck {
        fn ack(&self) -> BoxFuture<'_, ()> {
            let records = self.ledger.record_count_on_disk();
            self.seen.lock().unwrap().push(records);
            Box::pin(async {})
        }
    }

    /// Kinds of every event queued on the obs outbox (the verifier queues its
    /// own signing-state events there too).
    fn obs_kinds(fx: &Fixture) -> Vec<String> {
        std::fs::read_dir(fx.obs_dir())
            .map(|rd| {
                rd.flatten()
                    .filter_map(|e| std::fs::read(e.path()).ok())
                    .filter_map(|b| {
                        serde_json::from_slice::<kanade_shared::wire::ObsEvent>(&b).ok()
                    })
                    .map(|e| e.kind)
                    .collect()
            })
            .unwrap_or_default()
    }

    fn key() -> SigningKey {
        SigningKey::from_bytes(&[9u8; 32])
    }

    fn enforcing_verifier(fx: &Fixture) -> Verifier {
        Verifier::enforcing_for_test("PC1", fx.obs_dir(), "k1", &key().verifying_key())
    }

    fn payload(request_id: &str) -> Vec<u8> {
        serde_json::to_vec(&crate::admission_ledger::tests_support::command(request_id)).unwrap()
    }

    fn signed(body: &[u8]) -> SigHeaders {
        sign(&key(), "k1", body, chrono::Utc::now().timestamp_millis())
    }

    fn delivery<'a>(body: &'a [u8], headers: SigHeaders) -> Delivery<'a> {
        Delivery {
            subject: "commands.all",
            payload: body,
            headers,
            addressed: true,
        }
    }

    #[tokio::test]
    async fn ack_follows_admission() {
        let fx = Fixture::new();
        let ledger = fx.open();
        let v = enforcing_verifier(&fx);
        let body = payload("req-1");
        let spy = SpyAck::new(&ledger);
        let out = process_delivery(&delivery(&body, signed(&body)), &v, &ledger, &spy).await;
        assert!(matches!(out, Intake::Launch(_)));
        // Exactly one ack, and the record was already on disk when it came.
        assert_eq!(spy.acks(), vec![1]);
    }

    #[tokio::test]
    async fn no_ack_and_no_launch_when_the_ledger_cannot_be_written() {
        let fx = Fixture::new();
        let ledger = fx.open();
        ledger.set_fault(true);
        let v = enforcing_verifier(&fx);
        let body = payload("req-1");
        let spy = SpyAck::new(&ledger);
        let out = process_delivery(&delivery(&body, signed(&body)), &v, &ledger, &spy).await;
        assert!(matches!(out, Intake::Held));
        assert!(
            spy.acks().is_empty(),
            "an unrecorded delivery is never acked"
        );
        // Surfaced through the existing observability events.
        assert!(obs_kinds(&fx).contains(&"command_admission_unavailable".to_string()));

        // The same delivery is admitted once the disk recovers.
        ledger.set_fault(false);
        let out = process_delivery(&delivery(&body, signed(&body)), &v, &ledger, &spy).await;
        assert!(matches!(out, Intake::Launch(_)));
        assert_eq!(spy.acks().len(), 1);
    }

    #[tokio::test]
    async fn an_invalid_signature_does_not_reserve_the_id() {
        let fx = Fixture::new();
        let ledger = fx.open();
        let v = enforcing_verifier(&fx);
        let body = payload("req-1");
        let spy = SpyAck::new(&ledger);

        // Wrong key: refused, acknowledged as a terminal decision, nothing recorded.
        let forged = sign(
            &SigningKey::from_bytes(&[1u8; 32]),
            "k1",
            &body,
            chrono::Utc::now().timestamp_millis(),
        );
        let out = process_delivery(&delivery(&body, forged), &v, &ledger, &spy).await;
        assert!(matches!(out, Intake::Settled(Settled::Refused)));
        assert_eq!(spy.acks(), vec![0], "acked with nothing in the ledger");
        assert_eq!(ledger.record_count_on_disk(), 0);

        // Unsigned: likewise.
        let out =
            process_delivery(&delivery(&body, SigHeaders::default()), &v, &ledger, &spy).await;
        assert!(matches!(out, Intake::Settled(Settled::Refused)));
        assert_eq!(ledger.record_count_on_disk(), 0);

        // The genuine message with the same id is still admitted.
        let out = process_delivery(&delivery(&body, signed(&body)), &v, &ledger, &spy).await;
        assert!(matches!(out, Intake::Launch(_)));
    }

    #[tokio::test]
    async fn the_same_id_with_different_bytes_is_a_reported_conflict() {
        let fx = Fixture::new();
        let ledger = fx.open();
        let v = enforcing_verifier(&fx);
        let first = payload("req-1");
        let spy = SpyAck::new(&ledger);
        assert!(matches!(
            process_delivery(&delivery(&first, signed(&first)), &v, &ledger, &spy).await,
            Intake::Launch(_)
        ));

        // Same request id, different script, validly signed.
        let mut other: serde_json::Value = serde_json::from_slice(&first).unwrap();
        other["script"] = "Remove-Item -Recurse C:\\".into();
        let other = serde_json::to_vec(&other).unwrap();
        let out = process_delivery(&delivery(&other, signed(&other)), &v, &ledger, &spy).await;
        assert!(matches!(out, Intake::Settled(Settled::Conflict)));
        // Reported as a security diagnostic.
        assert!(obs_kinds(&fx).contains(&"command_admission_conflict".to_string()));
    }

    #[tokio::test]
    async fn live_and_replay_delivering_one_id_concurrently_launch_once() {
        let fx = Fixture::new();
        let ledger = fx.open();
        let v = Arc::new(enforcing_verifier(&fx));
        let body = Arc::new(payload("req-1"));
        let headers = signed(&body);
        let launches = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut tasks = Vec::new();
        for _ in 0..12 {
            let (ledger, v, body, headers, launches) = (
                ledger.clone(),
                v.clone(),
                body.clone(),
                headers.clone(),
                launches.clone(),
            );
            tasks.push(tokio::spawn(async move {
                let d = delivery(&body, headers);
                if matches!(
                    process_delivery(&d, &v, &ledger, &NoAck).await,
                    Intake::Launch(_)
                ) {
                    launches.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                }
            }));
        }
        for t in tasks {
            t.await.unwrap();
        }
        assert_eq!(launches.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_redelivery_after_restart_is_not_launched_again() {
        let fx = Fixture::new();
        let v = enforcing_verifier(&fx);
        let body = payload("req-1");
        {
            let ledger = fx.open();
            let Intake::Launch(a) =
                process_delivery(&delivery(&body, signed(&body)), &v, &ledger, &NoAck).await
            else {
                panic!("first delivery must launch");
            };
            a.ticket.mark_launching().unwrap();
        }
        // Restart: a fresh ledger over the same directory, then the broker
        // redelivers the retained message.
        let ledger = fx.open();
        let recovery = ledger.recover();
        assert!(recovery.pending.is_empty());
        let spy = SpyAck::new(&ledger);
        let out = process_delivery(&delivery(&body, signed(&body)), &v, &ledger, &spy).await;
        assert!(matches!(out, Intake::Settled(Settled::Duplicate)));
        assert_eq!(
            spy.acks().len(),
            1,
            "a duplicate is a terminal decision: acked"
        );
    }

    #[tokio::test]
    async fn a_command_for_another_host_is_acked_and_never_recorded() {
        let fx = Fixture::new();
        let ledger = fx.open();
        let v = enforcing_verifier(&fx);
        let body = payload("req-1");
        let spy = SpyAck::new(&ledger);
        let mut d = delivery(&body, signed(&body));
        d.addressed = false;
        let out = process_delivery(&d, &v, &ledger, &spy).await;
        assert!(matches!(out, Intake::Settled(Settled::NotForMe)));
        assert_eq!(spy.acks(), vec![0]);
        assert_eq!(ledger.record_count_on_disk(), 0);
    }

    #[tokio::test]
    async fn an_undecodable_message_is_terminated_without_a_record() {
        let fx = Fixture::new();
        let ledger = fx.open();
        let v = enforcing_verifier(&fx);
        let spy = SpyAck::new(&ledger);
        let out = process_delivery(
            &delivery(b"not json at all", SigHeaders::default()),
            &v,
            &ledger,
            &spy,
        )
        .await;
        assert!(matches!(out, Intake::Settled(Settled::Undecodable)));
        assert_eq!(spy.acks(), vec![0]);
    }
}
