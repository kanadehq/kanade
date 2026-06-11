//! `system.*` method handlers (SPEC §2.12.5).
//!
//! - `system.handshake` — protocol-version negotiation + session
//!   info (SPEC §2.12.6).
//! - `system.ping` — round-trip liveness check.
//! - `system.version` — running agent version + self-update
//!   target + (eventually) pinned client version.
//! - `system.log_tail` — last N lines of agent.log, for support
//!   handoff diagnostics.

use std::io;

use kanade_shared::ipc::error::{ErrorKind, RpcError};
use kanade_shared::ipc::handshake::{HandshakeParams, HandshakeResult, PROTOCOL_V1};
use kanade_shared::ipc::system::{
    LogTailParams, LogTailResult, PingParams, PingResult, VersionParams, VersionResult,
};
use tracing::warn;

use super::super::connection::ConnectionState;
use crate::log_tail::read_tail_lines;

/// Result type for KLP handlers — either a JSON-encoded response
/// payload or a [`RpcError`] the dispatcher will wrap into the
/// envelope's `error` slot. `anyhow::Error` is reserved for
/// internal failures the dispatcher turns into
/// [`ErrorKind::InternalError`] (-32603).
pub type HandlerResult<T> = std::result::Result<T, RpcError>;

/// Features this agent currently advertises in handshake. None
/// yet — push handlers (`push.notifications`, `push.jobs`,
/// `push.state`, `support.diagnostics`) land in their own PRs and
/// each one adds an entry here.
const SUPPORTED_FEATURES: &[&str] = &[];

/// Per-call cap on the number of log lines `system.log_tail`
/// returns, no matter what the caller asks for. Keeps the response
/// inside the 1 MiB framing cap (SPEC §2.12.2) with comfortable
/// headroom — at ~1 KiB per line (the agent's longest typical
/// `tracing` event), 1000 lines fits in ~1 MiB. Callers needing
/// more pull the full file via `support.upload_diagnostics`.
const LOG_TAIL_HARD_CAP: u32 = 1000;

/// `system.handshake` — protocol negotiation + session info.
///
/// SPEC §2.12.6:
/// - Pick the highest mutually-supported version from
///   `params.protocol`. KLP v1 only knows `PROTOCOL_V1` today.
/// - Return the agent's session info derived from the OS
///   (`conn.session()`), never from payload.
/// - If no protocol overlap, return
///   [`ErrorKind::StaleProtocol`].
pub fn handle_handshake(
    conn: &mut ConnectionState,
    params: HandshakeParams,
) -> HandlerResult<HandshakeResult> {
    if params.protocol.is_empty() {
        return Err(RpcError::new(
            ErrorKind::InvalidParams,
            "handshake.protocol must contain at least one version",
        ));
    }

    // Pick highest mutually-supported. Today we only know v1, so
    // the question collapses to "does the client mention v1?".
    let agreed = params
        .protocol
        .iter()
        .copied()
        .filter(|&v| v == PROTOCOL_V1)
        .max();

    let Some(agreed) = agreed else {
        return Err(RpcError::new(
            ErrorKind::StaleProtocol,
            format!(
                "no overlap with client versions {:?} (agent supports {:?})",
                params.protocol,
                [PROTOCOL_V1],
            ),
        ));
    };

    conn.mark_handshake(agreed);

    Ok(HandshakeResult {
        protocol: agreed,
        agent_version: conn.agent_version.clone(),
        features: SUPPORTED_FEATURES.iter().map(|&s| s.to_string()).collect(),
        session: conn.session(),
    })
}

/// `system.ping` — agent wall-clock at the moment it answered.
/// No params required; the envelope's `params: omitted` is
/// equivalent to `params: {}` via the
/// [`kanade_shared::ipc::envelope::decode_params`] helper.
pub fn handle_ping(_conn: &ConnectionState, _params: PingParams) -> HandlerResult<PingResult> {
    Ok(PingResult {
        agent_time: chrono::Utc::now(),
    })
}

/// `system.version` — running agent version + self-update target.
///
/// `target_agent_version` comes from the live `EffectiveConfig`
/// the supervisor (Sprint 6) publishes on the watch channel; when
/// the KV stack has set no rollout target (or set the empty
/// string by mistake), it falls back to the running version so
/// the SPA's "restart pending" banner stays hidden when nothing's
/// actually pending.
///
/// `target_client_version` is `None` until the backend publishes
/// a pinned Client App version through a future config field —
/// Sprint 8 ships the Client App without a forced-upgrade banner.
pub fn handle_version(
    conn: &ConnectionState,
    _params: VersionParams,
) -> HandlerResult<VersionResult> {
    // Defensive filter against `Some("")` — a backend that
    // confuses "cleared" with "empty string" would otherwise
    // surface `target_agent_version = ""` and trip a phantom
    // restart banner on the SPA.
    let target = conn
        .config_rx
        .borrow()
        .target_version
        .as_deref()
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| conn.agent_version.clone());
    Ok(VersionResult {
        agent_version: conn.agent_version.clone(),
        target_agent_version: target,
        target_client_version: None,
    })
}

/// `system.log_tail` — last N lines of `agent.log` for support
/// handoff.
///
/// The `log_path` on `ConnectionState` is the *template* path from
/// `cfg.log.path` (e.g. `agent.log`); the daily-rotation
/// `tracing_appender` actually writes to
/// `<dir>/<stem>.YYYY-MM-DD.<ext>`, so we resolve to the active
/// file via [`crate::logs::locate_active_file`] before reading
/// (same function the NATS `logs.fetch` path uses).
///
/// Clamping semantics:
/// - Caller's `lines` is capped at [`LOG_TAIL_HARD_CAP`]. When
///   the cap kicks in (caller asked for more than the cap),
///   `truncated` is set so the SPA can prompt to escalate to
///   `support.upload_diagnostics`. `truncated` is NOT set merely
///   because the file has more lines than the caller asked for
///   — the caller got what they asked for.
/// - File-not-found returns an empty `lines` (not an error) — the
///   agent's logger may not have written anything yet on a fresh
///   boot.
///
/// Reading is delegated to [`read_tail_lines`], which keeps the
/// simple whole-file path for small (daily-rotated) logs and falls
/// back to a bounded reverse-read for files past
/// [`TAIL_WHOLE_FILE_THRESHOLD`] so a hundreds-of-MB log can't spike
/// agent RSS (issue #289).
pub async fn handle_log_tail(
    conn: &ConnectionState,
    params: LogTailParams,
) -> HandlerResult<LogTailResult> {
    let requested = params.lines.min(LOG_TAIL_HARD_CAP);
    let truncated = params.lines > LOG_TAIL_HARD_CAP;

    let active_path = match crate::logs::locate_active_file(&conn.log_path).await {
        Ok(p) => p,
        Err(e) => {
            // `locate_active_file` only fails when the directory
            // itself is missing — treat the same as a missing
            // active file. Surface for diagnostics but don't
            // block the caller's UI.
            warn!(
                error = %e,
                template = %conn.log_path.display(),
                "system.log_tail: log directory missing, returning empty result",
            );
            return Ok(LogTailResult {
                lines: vec![],
                truncated,
            });
        }
    };

    let lines = match read_tail_lines(&active_path, requested as usize).await {
        Ok(lines) => lines,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            warn!(
                path = %active_path.display(),
                "system.log_tail: log file not found, returning empty result",
            );
            return Ok(LogTailResult {
                lines: vec![],
                truncated,
            });
        }
        Err(e) => {
            return Err(RpcError::new(
                ErrorKind::InternalError,
                format!("read agent.log ({}): {e}", active_path.display()),
            ));
        }
    };

    Ok(LogTailResult { lines, truncated })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::klp::auth::PeerCredentials;
    use crate::log_tail::{TAIL_AVG_LINE_BYTES, TAIL_WHOLE_FILE_THRESHOLD};
    use kanade_shared::ipc::state::StateSnapshot;
    use kanade_shared::wire::EffectiveConfig;
    use std::path::PathBuf;
    use tempfile::NamedTempFile;
    use tokio::sync::{mpsc, watch};

    fn dummy_snapshot() -> StateSnapshot {
        StateSnapshot {
            pc_id: "PC1234".into(),
            online: true,
            vpn: "unknown".into(),
            checks: vec![],
            agent_version: "0.40.0".into(),
            target_version: "0.40.0".into(),
        }
    }

    fn fresh_conn_with(cfg: EffectiveConfig, log_path: PathBuf) -> ConnectionState {
        let (_cfg_tx, cfg_rx) = watch::channel(cfg);
        let (_state_tx, state_rx) = watch::channel(dummy_snapshot());
        let (push_tx, _push_rx) = mpsc::channel(8);
        ConnectionState::new(
            PeerCredentials {
                user: "DOMAIN\\alice".into(),
                user_sid: "S-1-5-21-1001".into(),
                session_id: 2,
            },
            "PC1234".into(),
            "0.40.0".into(),
            cfg_rx,
            state_rx,
            log_path,
            push_tx,
        )
    }

    fn fresh_conn() -> ConnectionState {
        fresh_conn_with(
            EffectiveConfig::builtin_defaults(),
            PathBuf::from("agent.log"),
        )
    }

    #[test]
    fn handshake_v1_only_client_succeeds() {
        let mut conn = fresh_conn();
        let result = handle_handshake(
            &mut conn,
            HandshakeParams {
                client: "kanade-client".into(),
                client_version: "0.1.0".into(),
                protocol: vec![PROTOCOL_V1],
                features: vec![],
            },
        )
        .expect("handshake should succeed");
        assert_eq!(result.protocol, PROTOCOL_V1);
        assert_eq!(result.agent_version, "0.40.0");
        assert_eq!(result.session.user, "DOMAIN\\alice");
        assert_eq!(result.session.pc_id, "PC1234");
        assert!(conn.handshake_complete());
    }

    #[test]
    fn handshake_picks_highest_mutual_version_from_multi_version_client() {
        // Future-proof: a client advertising [1, 2] talking to a
        // v1-only agent must downshift to 1, not error.
        let mut conn = fresh_conn();
        let result = handle_handshake(
            &mut conn,
            HandshakeParams {
                client: "kanade-client".into(),
                client_version: "0.2.0".into(),
                protocol: vec![1, 2, 3],
                features: vec![],
            },
        )
        .expect("handshake should succeed via downshift");
        assert_eq!(result.protocol, PROTOCOL_V1);
    }

    #[test]
    fn handshake_rejects_empty_protocol_list() {
        let mut conn = fresh_conn();
        let err = handle_handshake(
            &mut conn,
            HandshakeParams {
                client: "kanade-client".into(),
                client_version: "0.1.0".into(),
                protocol: vec![],
                features: vec![],
            },
        )
        .expect_err("empty protocol must fail");
        let data = err.data.as_ref().expect("data populated");
        assert_eq!(data.kind, ErrorKind::InvalidParams);
        assert!(!conn.handshake_complete(), "conn must stay pre-handshake");
    }

    #[test]
    fn handshake_rejects_when_no_version_overlap() {
        // Client speaks only v2 / v3; agent speaks only v1 → no
        // overlap → StaleProtocol per SPEC §2.12.6.
        let mut conn = fresh_conn();
        let err = handle_handshake(
            &mut conn,
            HandshakeParams {
                client: "kanade-client".into(),
                client_version: "9.9.9".into(),
                protocol: vec![2, 3],
                features: vec![],
            },
        )
        .expect_err("must fail with StaleProtocol");
        let data = err.data.as_ref().expect("data populated");
        assert_eq!(data.kind, ErrorKind::StaleProtocol);
        assert!(!conn.handshake_complete());
    }

    #[test]
    fn ping_returns_recent_agent_time() {
        let conn = fresh_conn();
        let before = chrono::Utc::now();
        let result = handle_ping(&conn, PingParams::default()).unwrap();
        let after = chrono::Utc::now();
        assert!(
            result.agent_time >= before && result.agent_time <= after,
            "agent_time {} should be in [{before}, {after}]",
            result.agent_time
        );
    }

    #[test]
    fn handshake_advertises_no_features_yet() {
        // SUPPORTED_FEATURES stays empty until each push handler
        // actually lands — advertising features whose methods
        // aren't routed would mislead clients about what we can do.
        let mut conn = fresh_conn();
        let result = handle_handshake(
            &mut conn,
            HandshakeParams {
                client: "kanade-client".into(),
                client_version: "0.1.0".into(),
                protocol: vec![PROTOCOL_V1],
                features: vec!["push.notifications".into()],
            },
        )
        .unwrap();
        assert!(
            result.features.is_empty(),
            "no features advertised yet; got {:?}",
            result.features,
        );
    }

    // ---- system.version ----

    #[test]
    fn version_falls_back_to_running_when_no_target_set() {
        // EffectiveConfig::builtin_defaults() has target_version =
        // None. The handler should report target_agent_version =
        // agent_version so the SPA doesn't show a phantom
        // "restart pending" banner.
        let conn = fresh_conn();
        let result = handle_version(&conn, VersionParams::default()).unwrap();
        assert_eq!(result.agent_version, "0.40.0");
        assert_eq!(result.target_agent_version, "0.40.0");
        assert!(result.target_client_version.is_none());
    }

    #[test]
    fn version_returns_distinct_target_when_supervisor_set_one() {
        // Sprint 6 supervisor has published a rollout target —
        // the handler must surface it so the SPA shows the
        // "restart pending" banner.
        let mut cfg = EffectiveConfig::builtin_defaults();
        cfg.target_version = Some("0.42.0".into());
        let conn = fresh_conn_with(cfg, PathBuf::from("agent.log"));
        let result = handle_version(&conn, VersionParams::default()).unwrap();
        assert_eq!(result.agent_version, "0.40.0");
        assert_eq!(result.target_agent_version, "0.42.0");
    }

    // ---- system.log_tail ----

    #[tokio::test]
    async fn log_tail_returns_empty_when_file_missing() {
        // Fresh-boot scenario: parent dir exists (the agent set
        // up its log directory at startup) but no log file has
        // been written yet. Don't error — return an empty result
        // so the SPA support flow works the first time it's
        // invoked.
        let tmpdir = tempfile::tempdir().unwrap();
        let conn = fresh_conn_with(
            EffectiveConfig::builtin_defaults(),
            tmpdir.path().join("agent.log"),
        );
        let result = handle_log_tail(&conn, LogTailParams::default())
            .await
            .expect("missing file is not an error");
        assert!(result.lines.is_empty());
        assert!(!result.truncated);
    }

    #[tokio::test]
    async fn log_tail_picks_rotated_file_matching_template() {
        // The HIGH bug from agy's review: tracing_appender writes
        // to `<stem>.YYYY-MM-DD.<ext>`, not the bare template
        // path. The handler must resolve to the active file via
        // locate_active_file before reading.
        let tmpdir = tempfile::tempdir().unwrap();
        let template = tmpdir.path().join("agent.log");
        // The actual file the appender would have created today.
        let active = tmpdir.path().join("agent.2026-05-24.log");
        std::fs::write(&active, "first\nsecond\nthird\n").unwrap();
        let conn = fresh_conn_with(EffectiveConfig::builtin_defaults(), template);
        let result = handle_log_tail(&conn, LogTailParams { lines: 10 })
            .await
            .expect("locate_active_file should find the rotated file");
        assert_eq!(result.lines, vec!["first", "second", "third"]);
    }

    #[tokio::test]
    async fn log_tail_returns_all_lines_when_file_smaller_than_request() {
        let f = NamedTempFile::new().unwrap();
        std::fs::write(f.path(), "alpha\nbeta\ngamma\n").unwrap();
        let conn = fresh_conn_with(EffectiveConfig::builtin_defaults(), f.path().to_path_buf());
        let result = handle_log_tail(
            &conn,
            LogTailParams {
                lines: 100, // way more than the file has
            },
        )
        .await
        .unwrap();
        assert_eq!(result.lines, vec!["alpha", "beta", "gamma"]);
        assert!(!result.truncated);
    }

    #[tokio::test]
    async fn log_tail_returns_only_last_n_when_file_larger_than_request() {
        let f = NamedTempFile::new().unwrap();
        let body = (1..=20)
            .map(|i| format!("line-{i:02}"))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(f.path(), body).unwrap();
        let conn = fresh_conn_with(EffectiveConfig::builtin_defaults(), f.path().to_path_buf());
        let result = handle_log_tail(&conn, LogTailParams { lines: 5 })
            .await
            .unwrap();
        assert_eq!(
            result.lines,
            vec!["line-16", "line-17", "line-18", "line-19", "line-20"]
        );
        // Caller asked for 5, we returned 5 — well within cap, so
        // not truncated (the extra unread lines don't count as
        // "truncated by the agent" — that label is reserved for
        // the cap-applied case so SPA can prompt for full log).
        assert!(!result.truncated);
    }

    #[tokio::test]
    async fn log_tail_clamps_to_hard_cap_and_flags_truncated() {
        // Build a file with > HARD_CAP lines so the clamp visibly
        // bites and the truncated flag flips.
        let f = NamedTempFile::new().unwrap();
        let body = (1..=(LOG_TAIL_HARD_CAP + 50))
            .map(|i| format!("L{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(f.path(), body).unwrap();
        let conn = fresh_conn_with(EffectiveConfig::builtin_defaults(), f.path().to_path_buf());
        // Caller asks for HARD_CAP + 100 → clamped down.
        let result = handle_log_tail(
            &conn,
            LogTailParams {
                lines: LOG_TAIL_HARD_CAP + 100,
            },
        )
        .await
        .unwrap();
        assert_eq!(result.lines.len(), LOG_TAIL_HARD_CAP as usize);
        assert!(result.truncated, "asking past the cap must set truncated");
        // Confirm it's the tail (newest), not the head.
        assert_eq!(
            result.lines.last().unwrap(),
            &format!("L{}", LOG_TAIL_HARD_CAP + 50)
        );
    }

    #[tokio::test]
    async fn log_tail_default_is_200_lines() {
        // SPEC default in LogTailParams is 200. Just verify the
        // default doesn't trigger the truncated flag (200 << cap).
        let f = NamedTempFile::new().unwrap();
        let body = (1..=500)
            .map(|i| format!("L{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(f.path(), body).unwrap();
        let conn = fresh_conn_with(EffectiveConfig::builtin_defaults(), f.path().to_path_buf());
        let result = handle_log_tail(&conn, LogTailParams::default())
            .await
            .unwrap();
        assert_eq!(result.lines.len(), 200);
        assert!(!result.truncated);
    }

    // ---- system.log_tail: bounded reverse-read (issue #289) ----

    /// Build a file comfortably past `TAIL_WHOLE_FILE_THRESHOLD` so
    /// the large-file seek path is exercised, with line content that
    /// uniquely identifies each line so we can prove the tail is
    /// correct (and that the mid-line fragment at the seek boundary
    /// was dropped). Each line is padded to ~80 bytes; `count` lines
    /// of that easily clears the 4 MiB threshold by count ≥ 60k.
    fn write_big_log(path: &std::path::Path, count: usize) {
        let pad = "x".repeat(64);
        let body = (1..=count)
            .map(|i| format!("line-{i:08}-{pad}"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            body.len() as u64 > TAIL_WHOLE_FILE_THRESHOLD,
            "fixture must exceed the whole-file threshold to hit the seek path; \
             got {} bytes",
            body.len(),
        );
        std::fs::write(path, body).unwrap();
    }

    #[tokio::test]
    async fn log_tail_large_file_returns_correct_tail_via_seek() {
        // The whole point of #289: a multi-MB log must still yield
        // the exact last N lines, and the partial leading line at
        // the seek boundary must be dropped (otherwise the first
        // returned line would be a fragment, not "line-00099901-…").
        let f = NamedTempFile::new().unwrap();
        let count = 100_000usize;
        write_big_log(f.path(), count);
        let conn = fresh_conn_with(EffectiveConfig::builtin_defaults(), f.path().to_path_buf());

        let result = handle_log_tail(&conn, LogTailParams { lines: 100 })
            .await
            .unwrap();

        assert_eq!(result.lines.len(), 100);
        let pad = "x".repeat(64);
        // First and last returned lines are intact (no fragment) and
        // are exactly the last 100 lines of the file.
        assert_eq!(result.lines[0], format!("line-{:08}-{pad}", count - 99));
        assert_eq!(
            result.lines.last().unwrap(),
            &format!("line-{count:08}-{pad}")
        );
        assert!(!result.truncated);
    }

    #[tokio::test]
    async fn log_tail_large_file_grows_window_when_initial_estimate_too_small() {
        // Force the grow loop: with ~3 KiB lines, the initial
        // `requested × TAIL_AVG_LINE_BYTES` (= 2 KiB/line) window
        // holds fewer lines than requested, so the first read falls
        // short and the window must double before we have enough.
        // The result must still be the exact tail.
        let f = NamedTempFile::new().unwrap();
        let line_pad = "z".repeat(3 * 1024); // ~3 KiB per line, > 2 KiB estimate
        let count = 2_000usize; // ~6 MiB total, past the threshold
        let body = (1..=count)
            .map(|i| format!("line-{i:06}-{line_pad}"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(body.len() as u64 > TAIL_WHOLE_FILE_THRESHOLD);
        std::fs::write(f.path(), &body).unwrap();
        let conn = fresh_conn_with(EffectiveConfig::builtin_defaults(), f.path().to_path_buf());

        let result = handle_log_tail(&conn, LogTailParams { lines: 50 })
            .await
            .unwrap();

        assert_eq!(result.lines.len(), 50);
        assert_eq!(
            result.lines[0],
            format!("line-{:06}-{line_pad}", count - 49)
        );
        assert_eq!(
            result.lines.last().unwrap(),
            &format!("line-{count:06}-{line_pad}")
        );
    }

    #[tokio::test]
    async fn log_tail_large_file_request_exceeds_line_count() {
        // Large file but fewer lines than requested: must return all
        // of them (exiting the grow loop at start == 0), not loop
        // forever or drop the genuine first line of the file.
        let f = NamedTempFile::new().unwrap();
        // Few but very long lines to clear the threshold without a
        // huge line count.
        let huge = "y".repeat(2 * 1024 * 1024); // 2 MiB per line
        let body = format!("first-{huge}\nsecond-{huge}\nthird-{huge}");
        assert!(body.len() as u64 > TAIL_WHOLE_FILE_THRESHOLD);
        std::fs::write(f.path(), &body).unwrap();
        let conn = fresh_conn_with(EffectiveConfig::builtin_defaults(), f.path().to_path_buf());

        let result = handle_log_tail(&conn, LogTailParams { lines: 100 })
            .await
            .unwrap();

        assert_eq!(result.lines.len(), 3);
        assert!(result.lines[0].starts_with("first-"));
        assert!(result.lines[2].starts_with("third-"));
    }

    #[tokio::test]
    async fn log_tail_large_file_window_with_no_newline_grows_until_it_finds_one() {
        // Exercises the `None => &[]` branch in read_tail_lines:
        // the file's last line is longer than the initial window, so
        // the first seek (start > 0) lands inside it and finds zero
        // newlines. The window must keep doubling — discarding each
        // newline-less read — until it reaches the `head\n` boundary
        // and returns the (single) requested last line intact.
        let f = NamedTempFile::new().unwrap();
        let huge = "B".repeat(5 * 1024 * 1024); // 5 MiB single last line
        let body = format!("head\n{huge}");
        assert!(body.len() as u64 > TAIL_WHOLE_FILE_THRESHOLD);
        // The last line must dwarf the initial 1-line window estimate
        // so the first read genuinely contains no `\n`.
        assert!(huge.len() as u64 > TAIL_AVG_LINE_BYTES);
        std::fs::write(f.path(), &body).unwrap();
        let conn = fresh_conn_with(EffectiveConfig::builtin_defaults(), f.path().to_path_buf());

        let result = handle_log_tail(&conn, LogTailParams { lines: 1 })
            .await
            .unwrap();

        assert_eq!(result.lines, vec![huge]);
    }
}
