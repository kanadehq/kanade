//! Unix domain socket listener for KLP on Linux / macOS (SPEC §2.12.1).
//!
//! The socket is `0666` inside a root-owned `0755` directory, so any
//! local process can connect; authorization happens here, at accept
//! time, from the kernel-attested peer credentials and *before a single
//! byte is read* ([`crate::klp::auth::unix`]). A refused peer gets one
//! `id: null` `-32000 Unauthorized` frame and is closed without being
//! read from.
//!
//! Lifecycle: a stale socket file left by a crashed agent is removed at
//! start (but never a regular file, symlink, or a socket another agent
//! is still serving); [`KlpServer`]'s `Drop` removes the socket again on
//! clean shutdown — only if it is still the inode we created.

use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tokio::io::AsyncWriteExt;
use tokio::net::{UnixListener, UnixStream};
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use super::{ListenerContext, serve_connection};
use crate::klp::auth::unix::{
    AccountLookup, MIN_INTERACTIVE_UID, Rejection, SystemLookup, admit_with_timeout,
    peer_credentials, rejection_frame,
};
use crate::klp::framing::write_frame;

/// Environment override for the socket path (tests / development).
pub const ENV_SOCKET_PATH: &str = "KANADE_KLP_SOCKET_PATH";

/// SPEC §2.12.1 default endpoint.
#[cfg(target_os = "linux")]
pub const DEFAULT_SOCKET_PATH: &str = "/run/kanade/agent.sock";
#[cfg(not(target_os = "linux"))]
pub const DEFAULT_SOCKET_PATH: &str = "/var/run/kanade/agent.sock";

/// How long a refused peer gets to accept the rejection frame.
const REJECT_WRITE_TIMEOUT: Duration = Duration::from_secs(2);
/// Bound on the "is anything still serving this socket?" probe.
const PROBE_TIMEOUT: Duration = Duration::from_secs(1);

/// Socket path from [`ENV_SOCKET_PATH`] if set and non-empty, else the default.
pub fn socket_path() -> PathBuf {
    match std::env::var_os(ENV_SOCKET_PATH) {
        Some(p) if !p.is_empty() => PathBuf::from(p),
        _ => PathBuf::from(DEFAULT_SOCKET_PATH),
    }
}

/// Everything that varies between production and tests.
pub struct ListenerConfig {
    pub path: PathBuf,
    pub lookup: Arc<dyn AccountLookup>,
    pub min_uid: u32,
}

impl ListenerConfig {
    /// Production settings: env-or-default path, real passwd lookup, platform UID floor.
    pub fn from_env() -> Self {
        Self {
            path: socket_path(),
            lookup: Arc::new(SystemLookup),
            min_uid: MIN_INTERACTIVE_UID,
        }
    }
}

/// A running listener. Dropping it stops accepting and removes the
/// socket file (if it is still ours).
pub struct KlpServer {
    task: JoinHandle<()>,
    path: PathBuf,
    ino: (u64, u64),
}

impl Drop for KlpServer {
    fn drop(&mut self) {
        self.task.abort();
        match std::fs::symlink_metadata(&self.path) {
            Ok(m) if (m.dev(), m.ino()) == self.ino => {
                if let Err(e) = std::fs::remove_file(&self.path) {
                    warn!(error = %e, path = %self.path.display(), "KLP: could not remove socket");
                } else {
                    info!(path = %self.path.display(), "KLP socket removed");
                }
            }
            // Gone, or replaced by someone else's socket: leave it alone.
            _ => {}
        }
    }
}

/// Bind the socket and start accepting. Errors here mean the listener
/// could not be established (unsafe directory, live competitor, foreign
/// file in the way, bind failure).
pub async fn start(ctx: ListenerContext, cfg: ListenerConfig) -> Result<KlpServer> {
    prepare_parent_dir(&cfg.path)?;
    remove_stale_socket(&cfg.path).await?;

    let listener = UnixListener::bind(&cfg.path)
        .with_context(|| format!("bind KLP socket {}", cfg.path.display()))?;
    // chmod rather than rely on umask: the mode is part of the contract.
    std::fs::set_permissions(&cfg.path, std::fs::Permissions::from_mode(0o666))
        .with_context(|| format!("chmod 0666 {}", cfg.path.display()))?;
    let meta = std::fs::symlink_metadata(&cfg.path)?;
    let ino = (meta.dev(), meta.ino());

    info!(path = %cfg.path.display(), min_uid = cfg.min_uid, "KLP listener ready");
    let path = cfg.path.clone();
    let task = tokio::spawn(accept_loop(listener, ctx, cfg));
    Ok(KlpServer { task, path, ino })
}

/// Create the socket's directory (0755) if missing; when running as
/// root require it to be root-owned and not group/other-writable.
fn prepare_parent_dir(sock: &Path) -> Result<()> {
    let dir = sock
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .context("KLP socket path has no parent directory")?;
    let created = !dir.exists();
    if created {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o755))
            .with_context(|| format!("chmod 0755 {}", dir.display()))?;
    }
    let meta = std::fs::metadata(dir).with_context(|| format!("stat {}", dir.display()))?;
    if !meta.is_dir() {
        bail!("{} is not a directory", dir.display());
    }
    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } == 0 {
        if meta.uid() != 0 || meta.mode() & 0o022 != 0 {
            bail!(
                "refusing to use {}: as root the KLP socket directory must be root-owned and not group/other-writable (uid {}, mode {:o})",
                dir.display(),
                meta.uid(),
                meta.mode() & 0o7777
            );
        }
        // Root-owned and not writable by others, so tightening or opening the
        // read/execute bits is safe: SPEC §2.12.1 wants exactly 0755 so that
        // unprivileged users can reach the socket (e.g. a pre-existing 0700 dir).
        if meta.mode() & 0o7777 != 0o755 {
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o755))
                .with_context(|| format!("chmod 0755 {}", dir.display()))?;
        }
    }
    if meta.mode() & 0o005 != 0o005 {
        warn!(dir = %dir.display(), mode = format!("{:o}", meta.mode() & 0o7777), "KLP socket directory is not world-traversable; clients cannot connect");
    }
    Ok(())
}

/// Remove a leftover socket from a previous run. Anything that is not
/// a socket, or a socket that still answers, aborts startup untouched.
async fn remove_stale_socket(path: &Path) -> Result<()> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e).with_context(|| format!("stat {}", path.display())),
    };
    if !meta.file_type().is_socket() {
        bail!(
            "{} exists and is not a socket; refusing to remove it",
            path.display()
        );
    }
    // A live listener (another agent) accepts the probe connection.
    if let Ok(Ok(_)) = tokio::time::timeout(PROBE_TIMEOUT, UnixStream::connect(path)).await {
        bail!("another process is already listening on {}", path.display());
    }
    std::fs::remove_file(path)
        .with_context(|| format!("remove stale socket {}", path.display()))?;
    info!(path = %path.display(), "KLP: removed stale socket");
    Ok(())
}

async fn accept_loop(listener: UnixListener, ctx: ListenerContext, cfg: ListenerConfig) {
    let min_uid = cfg.min_uid;
    let lookup = cfg.lookup;
    loop {
        let stream = match listener.accept().await {
            Ok((s, _)) => s,
            Err(e) => {
                warn!(error = %e, "KLP accept() failed; retrying");
                tokio::time::sleep(Duration::from_millis(200)).await;
                continue;
            }
        };
        let ctx = ctx.clone();
        let lookup = lookup.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_connection(stream, ctx, lookup, min_uid).await {
                warn!(error = %e, "KLP connection task failed");
            }
        });
    }
}

async fn handle_connection(
    stream: UnixStream,
    ctx: ListenerContext,
    lookup: Arc<dyn AccountLookup>,
    min_uid: u32,
) -> Result<()> {
    // Admission BEFORE any read (SPEC §2.12.4.1).
    let admitted = match peer_credentials(&stream) {
        Ok(cred) => admit_with_timeout(lookup, cred.uid, cred.pid, min_uid).await,
        Err(r) => Err(r),
    };
    let peer = match admitted {
        Ok(p) => p,
        Err(rejection) => {
            reject(stream, &rejection).await;
            return Ok(());
        }
    };
    debug!(
        user = %peer.user_display,
        key = %peer.user_key,
        pid = peer.pid,
        session = ?peer.session,
        "KLP peer connected",
    );
    serve_connection(stream, peer, ctx).await
}

/// Write the single Unauthorized frame and close; never read.
async fn reject(mut stream: UnixStream, rejection: &Rejection) {
    warn!(rule = ?rejection.rule, retryable = rejection.retryable(), detail = %rejection.detail, "KLP peer rejected");
    let body = rejection_frame(rejection);
    let write = async {
        write_frame(&mut stream, &body).await?;
        stream.shutdown().await
    };
    match tokio::time::timeout(REJECT_WRITE_TIMEOUT, write).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => debug!(error = %e, "KLP: could not deliver rejection frame"),
        Err(_) => debug!("KLP: rejection frame write timed out"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::klp::auth::unix::Account;
    use crate::klp::framing::read_frame;
    use kanade_shared::wire::EffectiveConfig;
    use std::sync::Arc;
    use tokio::sync::{broadcast, watch};

    struct FixedLookup(Option<Account>);

    impl AccountLookup for FixedLookup {
        fn passwd(&self, _uid: u32) -> Result<Option<Account>, String> {
            Ok(self.0.clone())
        }
        fn login_shells(&self) -> Result<Vec<String>, String> {
            Ok(vec!["/bin/sh".into()])
        }
        fn session_of(&self, _pid: u32) -> Option<u32> {
            None
        }
    }

    fn me() -> u32 {
        unsafe { libc::geteuid() }
    }

    async fn test_ctx() -> ListenerContext {
        let nats = async_nats::ConnectOptions::new()
            .retry_on_initial_connect()
            .connect("127.0.0.1:1")
            .await
            .expect("lazy client");
        let (_cfg_tx, config_rx) = watch::channel(EffectiveConfig::builtin_defaults());
        let (_state_tx, state_rx) = watch::channel(kanade_shared::ipc::state::StateSnapshot {
            pc_id: "PC1".into(),
            online: true,
            checks: vec![],
            agent_version: "0.0.0".into(),
            target_version: "0.0.0".into(),
        });
        ListenerContext {
            pc_id: Arc::from("PC1"),
            agent_version: Arc::from("0.0.0"),
            config_rx,
            state_rx,
            log_path: PathBuf::from("agent.log"),
            nats,
            notif_tx: broadcast::channel(4).0,
            amend_tx: broadcast::channel(4).0,
        }
    }

    fn cfg(path: PathBuf, account: Option<Account>) -> ListenerConfig {
        ListenerConfig {
            path,
            lookup: Arc::new(FixedLookup(account)),
            // Independent of the real uid of whoever runs the tests.
            min_uid: 0,
        }
    }

    fn alice() -> Account {
        Account {
            name: "alice".into(),
            shell: "/bin/sh".into(),
        }
    }

    async fn send(stream: &mut UnixStream, v: serde_json::Value) {
        write_frame(stream, &serde_json::to_vec(&v).unwrap())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn handshake_over_real_socket_then_cleanup() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("klp").join("agent.sock");
        let server = start(test_ctx().await, cfg(path.clone(), Some(alice())))
            .await
            .unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o666);
        let dmode = std::fs::metadata(path.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(dmode, 0o755);

        let mut c = UnixStream::connect(&path).await.unwrap();
        send(
            &mut c,
            serde_json::json!({
                "jsonrpc": "2.0", "id": "h1", "method": "system.handshake",
                "params": {"client": "test", "client_version": "0", "protocol": [1], "features": []}
            }),
        )
        .await;
        let resp: serde_json::Value =
            serde_json::from_slice(&read_frame(&mut c).await.unwrap()).unwrap();
        assert_eq!(resp["id"], "h1");
        assert_eq!(resp["result"]["protocol"], 1);
        assert_eq!(resp["result"]["session"]["user"], "alice");
        assert_eq!(resp["result"]["session"]["session_id"], me());
        assert_eq!(resp["result"]["session"]["pc_id"], "PC1");
        drop(c);

        drop(server);
        assert!(!path.exists(), "socket removed on shutdown");
    }

    #[tokio::test]
    async fn rejected_peer_gets_one_retryable_frame_then_eof() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent.sock");
        let _server = start(test_ctx().await, cfg(path.clone(), None))
            .await
            .unwrap();

        let mut c = UnixStream::connect(&path).await.unwrap();
        let v: serde_json::Value =
            serde_json::from_slice(&read_frame(&mut c).await.unwrap()).unwrap();
        assert_eq!(v["id"], serde_json::Value::Null);
        assert_eq!(v["error"]["code"], -32000);
        assert_eq!(v["error"]["data"]["kind"], "Unauthorized");
        assert_eq!(v["error"]["data"]["retryable"], true);
        let eof = read_frame(&mut c).await.unwrap_err();
        assert_eq!(eof.kind(), std::io::ErrorKind::UnexpectedEof);
    }

    #[tokio::test]
    async fn uid_floor_rejection_is_permanent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent.sock");
        let mut c0 = cfg(path.clone(), Some(alice()));
        c0.min_uid = u32::MAX; // every real uid is below the floor
        let _server = start(test_ctx().await, c0).await.unwrap();
        let mut c = UnixStream::connect(&path).await.unwrap();
        let v: serde_json::Value =
            serde_json::from_slice(&read_frame(&mut c).await.unwrap()).unwrap();
        // uid 0 (root CI) reports the root rule instead; both are permanent.
        assert_eq!(v["error"]["data"]["retryable"], false);
    }

    #[tokio::test]
    async fn stale_socket_is_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent.sock");
        // Leave a dead socket file behind (bind, then drop without unlink).
        drop(std::os::unix::net::UnixListener::bind(&path).unwrap());
        assert!(path.exists());
        let _server = start(test_ctx().await, cfg(path.clone(), Some(alice())))
            .await
            .unwrap();
        UnixStream::connect(&path).await.unwrap();
    }

    #[tokio::test]
    async fn non_socket_file_is_not_removed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent.sock");
        std::fs::write(&path, b"precious").unwrap();
        let err = start(test_ctx().await, cfg(path.clone(), Some(alice())))
            .await
            .err()
            .expect("must refuse");
        assert!(err.to_string().contains("not a socket"), "{err}");
        assert_eq!(std::fs::read(&path).unwrap(), b"precious");
    }

    #[tokio::test]
    async fn live_listener_is_not_stolen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent.sock");
        let _first = start(test_ctx().await, cfg(path.clone(), Some(alice())))
            .await
            .unwrap();
        let err = start(test_ctx().await, cfg(path.clone(), Some(alice())))
            .await
            .err()
            .expect("must refuse");
        assert!(err.to_string().contains("already listening"), "{err}");
    }

    #[tokio::test]
    async fn drop_leaves_a_replaced_socket_alone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent.sock");
        let server = start(test_ctx().await, cfg(path.clone(), Some(alice())))
            .await
            .unwrap();
        std::fs::remove_file(&path).unwrap();
        let _other = std::os::unix::net::UnixListener::bind(&path).unwrap();
        drop(server);
        assert!(path.exists(), "someone else's socket must survive");
    }

    #[test]
    fn env_override_wins() {
        // Env mutation is process-global; this is the only test touching it.
        // SAFETY: no other thread reads this variable concurrently.
        unsafe { std::env::set_var(ENV_SOCKET_PATH, "/tmp/x/agent.sock") };
        assert_eq!(socket_path(), PathBuf::from("/tmp/x/agent.sock"));
        unsafe { std::env::set_var(ENV_SOCKET_PATH, "") };
        assert_eq!(socket_path(), PathBuf::from(DEFAULT_SOCKET_PATH));
        unsafe { std::env::remove_var(ENV_SOCKET_PATH) };
    }
}
