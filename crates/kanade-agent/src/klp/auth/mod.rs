//! KLP peer identity (SPEC §2.12.4.1).
//!
//! [`PeerIdentity`] is the platform-neutral answer to "who connected?"
//! that every handler works from. The OS-specific half lives next
//! door:
//!
//! - [`windows`] — Pipe → `GetNamedPipeClientProcessId` → process
//!   token → SID / Session id.
//! - [`unix`] — `SO_PEERCRED` (Linux) / `LOCAL_PEERCRED` +
//!   `LOCAL_PEERPID` (macOS), plus the connect-time admission rules
//!   (UID floor, root, passwd entry, interactive shell).
//!
//! The OS identity is the *authoritative* answer; we never trust
//! user-supplied identity fields in the payload.

#[cfg(unix)]
pub mod unix;
#[cfg(target_os = "windows")]
pub mod windows;

#[cfg(target_os = "windows")]
pub use self::windows::resolve_peer;

/// What the auth shim derives from the OS at connect time. The
/// dispatcher pairs this with the agent's configured `pc_id` to
/// build the [`kanade_shared::ipc::handshake::HandshakeSession`]
/// returned to the client.
#[derive(Debug, Clone)]
pub struct PeerIdentity {
    /// Stable ownership / state key. Windows: canonical SID string
    /// (`S-1-5-21-…`, or `"<unknown>"` if the conversion failed).
    /// Unix: `"uid-<UID>"`. Safe to embed in NATS KV keys and
    /// subjects. This — NOT the renameable [`user_display`](Self::user_display)
    /// — keys the `notifications_read` KV row, the
    /// `events.notifications.acked.>` subject and unlock grants.
    pub user_key: String,
    /// Display / log name only, never used for authorization.
    /// Windows: `DOMAIN\\user`; Unix: the login name. `"<unknown>"`
    /// when the OS cannot resolve a friendly name.
    pub user_display: String,
    /// Peer process id. Identifies the connecting process; not an
    /// ownership key.
    pub pid: u32,
    /// OS session identifier when one is known (Windows session id;
    /// Linux logind session, best effort; macOS `None`). Display and
    /// logging only — never an authorization input.
    pub session: Option<u32>,
}

impl PeerIdentity {
    /// Value for the wire `HandshakeSession.session_id` (SPEC
    /// §2.12.6): the Windows session id, or the UID on Unix. It is a
    /// compatibility/display field, not an auth key and not a real
    /// session id on Unix; `0` when it cannot be derived.
    pub fn handshake_session_id(&self) -> u32 {
        #[cfg(target_os = "windows")]
        {
            self.session.unwrap_or(0)
        }
        #[cfg(not(target_os = "windows"))]
        {
            self.user_key
                .strip_prefix("uid-")
                .and_then(|n| n.parse().ok())
                .unwrap_or(0)
        }
    }
}
