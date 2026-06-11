pub mod audit;
pub mod consumer_reset;
pub mod events;
pub mod explode;
pub mod heartbeat;
pub mod history;
pub mod host_perf;
pub mod notifications;
pub mod obs_events;
pub mod process_perf;
pub mod results;
pub mod spec_cache;

/// #398: the `recorded_at` a projector should stamp on a row — the
/// JetStream **publish timestamp** of the message being projected
/// ("the time that this message was received by the server from its
/// publisher"), not `Utc::now()` at projection time. Publish time is
/// a property of the message, so re-projecting after a `-WipeDb`
/// (#389) reproduces the exact same arrival times instead of
/// collapsing all replayed history onto the replay instant.
///
/// Falls back to `Utc::now()` when the ack metadata can't be parsed
/// (`msg.info()` errors on a malformed reply subject — shouldn't
/// happen for JetStream deliveries, but a timestamp must never be
/// the reason a row fails to project).
pub(crate) fn publish_time(msg: &async_nats::jetstream::Message) -> chrono::DateTime<chrono::Utc> {
    msg.info()
        .ok()
        .and_then(|i| {
            // time::OffsetDateTime → chrono, via (secs, subsec nanos)
            // — no lossy i128→i64 cast. None only for timestamps
            // outside chrono's ±262k-year range, which no real broker
            // emits; the fallback below covers it anyway.
            chrono::DateTime::from_timestamp(i.published.unix_timestamp(), i.published.nanosecond())
        })
        .unwrap_or_else(chrono::Utc::now)
}
