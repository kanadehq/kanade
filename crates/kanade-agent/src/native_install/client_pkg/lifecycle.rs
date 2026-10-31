//! Transaction-bound launch exclusion. Native signals use kernel audit tokens,
//! never executable basenames or caller-selected PIDs. GUI eligibility is not
//! KLP admission. Remote typed dispatch remains disabled.
use super::{Error, Result};
#[cfg(target_os = "macos")]
use kanade_shared::client_lifecycle::Presence;
use kanade_shared::client_lifecycle::{
    Instance,
    gate::{Gate, Writer},
};
#[cfg(target_os = "macos")]
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;
#[cfg(target_os = "macos")]
use std::time::Instant;

/// A stop policy shared by native and deterministic adapters. Every escalation
/// must revalidate the full captured identity at the syscall boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopAction {
    GracefulRequested { instance: Instance },
    SentSigterm { instance: Instance },
    SentSigkill { instance: Instance },
    Exited { instance: Instance },
}
pub trait StopAdapter {
    fn record(&mut self, action: StopAction);
    fn graceful(&mut self, instance: Instance) -> Result<()>;
    fn alive(&self, instance: Instance) -> bool;
    fn signal(&mut self, instance: Instance, signal: i32) -> Result<()>;
    fn wait(&mut self, duration: Duration);
}
pub fn stop(adapter: &mut dyn StopAdapter, instance: Instance) -> Result<()> {
    adapter.graceful(instance)?;
    adapter.record(StopAction::GracefulRequested { instance });
    for signal in [libc::SIGTERM, libc::SIGKILL] {
        for _ in 0..50 {
            if !adapter.alive(instance) {
                adapter.record(StopAction::Exited { instance });
                return Ok(());
            }
            adapter.wait(Duration::from_millis(100));
        }
        if adapter.alive(instance) {
            adapter.signal(instance, signal)?;
            adapter.record(if signal == libc::SIGTERM {
                StopAction::SentSigterm { instance }
            } else {
                StopAction::SentSigkill { instance }
            });
        }
    }
    for _ in 0..20 {
        if !adapter.alive(instance) {
            adapter.record(StopAction::Exited { instance });
            return Ok(());
        }
        adapter.wait(Duration::from_millis(100));
    }
    Err(Error::Deferred(
        "verified Client did not stop within shutdown budget".into(),
    ))
}

pub struct NativeLease<'a> {
    pub(super) system: &'a super::system::NativeSystem,
    writer: Option<Writer>,
    #[cfg(target_os = "macos")]
    sessions: std::cell::RefCell<Vec<Session>>,
}
impl super::system::LaunchLease for NativeLease<'_> {
    fn recheck(&self) -> Result<()> {
        self.writer
            .as_ref()
            .ok_or_else(|| Error::Deferred("released launch lease".into()))?
            .recheck()?;
        #[cfg(target_os = "macos")]
        {
            let snapshot =
                super::processes::ProcessView::snapshot(&super::processes::NativeProcessView {
                    client_exe: self.system.executable(),
                })?;
            if snapshot.installer_active || !snapshot.ambiguous.is_empty() {
                return Err(Error::Deferred(
                    "ambiguous Client/installer evidence".into(),
                ));
            }
            for process in snapshot.clients {
                if !self.sessions.borrow().iter().any(|s| {
                    s.before.helper.pid() == process.pid
                        && kanade_shared::client_lifecycle::macos::alive(s.before.helper)
                }) {
                    return Err(Error::Deferred(
                        "Client appeared while launch gate closed".into(),
                    ));
                }
            }
        }
        self.writer.as_ref().unwrap().exclusive()?;
        Ok(())
    }
    fn mutation_intent(&self) -> Result<()> {
        self.recheck()?;
        self.writer.as_ref().unwrap().mutation_intent()?;
        Ok(())
    }
    fn mutation_recorded(&self) -> Result<Option<bool>> {
        Ok(Some(self.writer.as_ref().unwrap().mutation_recorded()?))
    }
    fn native(&self) -> Option<&NativeLease<'_>> {
        Some(self)
    }
    fn finish(mut self: Box<Self>) -> Result<super::result::GuiActivation> {
        // Called by transaction code only after terminal journal AND a fresh
        // installed/previous-state verification. Never called by Drop.
        self.writer.take().unwrap().finish()?;
        #[cfg(target_os = "macos")]
        {
            use kanade_shared::client_lifecycle::macos::Request;
            let mut launched = false;
            let mut skipped = false;
            for session in self.sessions.get_mut() {
                let fresh = session.exchange(Request::Observe);
                if fresh.is_ok_and(|p| session.before.may_restore(&p)) {
                    match session.restore() {
                        Ok(true) => launched = true,
                        _ => skipped = true,
                    }
                } else {
                    let _ = session.exchange(Request::Release);
                    skipped = true;
                }
            }
            return Ok(if launched && !skipped {
                super::result::GuiActivation::RestoreLaunchedUnverified
            } else if launched {
                super::result::GuiActivation::RestorePartiallyLaunchedUnverified
            } else {
                super::result::GuiActivation::NotLaunched
            });
        }
        #[cfg(not(target_os = "macos"))]
        Ok(super::result::GuiActivation::NotLaunched)
    }
}
impl super::system::NativeSystem {
    pub(super) fn executable(&self) -> PathBuf {
        self.target
            .join("Applications")
            .join(super::structure::APP)
            .join("Contents/MacOS/kanade-client")
    }
    pub(super) fn launch_gate(&self) -> PathBuf {
        self.target
            .join("Library/Application Support/Kanade/client-lifecycle")
    }
    pub(super) fn coordinate_native(
        &self,
        transaction: &str,
        recovery: bool,
    ) -> Result<Box<dyn super::system::LaunchLease + '_>> {
        if !cfg!(target_os = "macos") || unsafe { libc::geteuid() } != 0 {
            return Err(Error::Deferred(
                "macOS root launch coordinator required".into(),
            ));
        }
        if !recovery && self.executable().exists() {
            super::structure::lifecycle_participant(&std::fs::read(
                self.executable()
                    .parent()
                    .unwrap()
                    .parent()
                    .unwrap()
                    .join("Info.plist"),
            )?)?;
        }
        let writer = Gate::open(&self.launch_gate(), 0, true)?.writer(transaction)?;
        #[cfg(target_os = "macos")]
        let sessions = if recovery {
            // Restart never rediscovers restart intent from an arbitrary PID.
            // Lost authenticated helper channels mean no automatic GUI launch.
            Vec::new()
        } else {
            discover(&self.executable())?
        };
        #[cfg(not(target_os = "macos"))]
        let _ = recovery;
        let lease = NativeLease {
            system: self,
            writer: Some(writer),
            #[cfg(target_os = "macos")]
            sessions: std::cell::RefCell::new(sessions),
        };
        #[cfg(target_os = "macos")]
        for session in lease.sessions.borrow_mut().iter_mut() {
            let gui = session.before.gui;
            stop(session, gui)?;
        }
        if !recovery {
            super::system::LaunchLease::recheck(&lease)?;
        }
        Ok(Box::new(lease))
    }
}

#[cfg(target_os = "macos")]
struct Session {
    stream: std::os::unix::net::UnixStream,
    before: Presence,
    sequence: u64,
    executable: PathBuf,
    received: Instant,
}
#[cfg(target_os = "macos")]
impl Session {
    fn exchange(
        &mut self,
        request: kanade_shared::client_lifecycle::macos::Request,
    ) -> Result<Presence> {
        use kanade_shared::client_lifecycle::macos;
        if macos::peer(&self.stream)? != self.before.helper.token
            || !macos::alive(self.before.helper)
        {
            return Err(Error::Deferred("presence helper process changed".into()));
        }
        let start = Instant::now();
        macos::send(&mut self.stream, &request)?;
        let response: macos::Response = macos::receive(&mut self.stream)?;
        self.validate(response.presence, start)?;
        Ok(response.presence)
    }
    fn validate(&mut self, p: Presence, start: Instant) -> Result<()> {
        if start.elapsed() > Duration::from_secs(2)
            || p.gui != self.before.gui
            || p.helper != self.before.helper
            || p.generation != self.before.generation
            || p.sequence <= self.sequence
        {
            return Err(Error::Deferred(
                "stale/replayed/contradictory session presence".into(),
            ));
        }
        self.sequence = p.sequence;
        self.received = Instant::now();
        Ok(())
    }
    fn restore(&mut self) -> Result<bool> {
        use kanade_shared::client_lifecycle::macos;
        if self.received.elapsed() > Duration::from_secs(2)
            || macos::peer(&self.stream)? != self.before.helper.token
            || !macos::alive(self.before.helper)
        {
            return Ok(false);
        }
        let start = Instant::now();
        macos::send(&mut self.stream, &macos::Request::Restore)?;
        let response: macos::Response = macos::receive(&mut self.stream)?;
        self.validate(response.presence, start)?;
        Ok(response.launched && self.before.may_restore(&response.presence))
    }
}
#[cfg(target_os = "macos")]
impl StopAdapter for Session {
    fn record(&mut self, action: StopAction) {
        tracing::info!(?action, "Client lifecycle stop");
    }
    fn graceful(&mut self, instance: Instance) -> Result<()> {
        if instance != self.before.gui {
            return Err(Error::Deferred("wrong graceful-stop identity".into()));
        }
        let p = self.exchange(kanade_shared::client_lifecycle::macos::Request::Stop)?;
        // Preserve the freshest pre-stop vetoes, never manufacture restart intent.
        self.before = p;
        Ok(())
    }
    fn alive(&self, instance: Instance) -> bool {
        kanade_shared::client_lifecycle::macos::alive(instance)
    }
    fn signal(&mut self, instance: Instance, signal: i32) -> Result<()> {
        if instance != self.before.gui {
            return Err(Error::Deferred("wrong signal identity".into()));
        }
        kanade_shared::client_lifecycle::macos::signal(instance, &self.executable, signal)?;
        Ok(())
    }
    fn wait(&mut self, duration: Duration) {
        std::thread::sleep(duration);
    }
}
#[cfg(target_os = "macos")]
fn discover(executable: &Path) -> Result<Vec<Session>> {
    use super::processes::{NativeProcessView, ProcessView};
    use kanade_shared::client_lifecycle::macos;
    let snapshot = NativeProcessView {
        client_exe: executable.into(),
    }
    .snapshot()?;
    if !snapshot.ambiguous.is_empty() || snapshot.installer_active {
        return Err(Error::Deferred(
            "ambiguous Client/installer snapshot".into(),
        ));
    }
    let mut sessions = Vec::new();
    for process in &snapshot.clients {
        let Ok(mut stream) = std::os::unix::net::UnixStream::connect(macos::endpoint(process.pid))
        else {
            continue;
        };
        macos::configure(&stream)?;
        // Reuse the KLP kernel credential/interactive account trust contract,
        // retaining session=None. GUI proof is additional, separate evidence.
        let peer = crate::klp::auth::unix::peer_credentials(&stream)
            .map_err(|e| Error::Deferred(e.detail))?;
        crate::klp::auth::unix::admit(
            peer.uid,
            peer.pid,
            &crate::klp::auth::unix::SystemLookup,
            crate::klp::auth::unix::MIN_INTERACTIVE_UID,
        )
        .map_err(|e| Error::Deferred(e.detail))?;
        let token = macos::peer(&stream)?;
        if peer.pid != token[5] || peer.uid != token[1] || peer.pid != process.pid {
            return Err(Error::Deferred("helper credential mismatch".into()));
        }
        let helper = macos::identity(token, executable)?;
        let start = Instant::now();
        macos::send(&mut stream, &macos::Request::Observe)?;
        let response: macos::Response = macos::receive(&mut stream)?;
        let p = response.presence;
        let gui = macos::identity(p.gui.token, executable)?;
        let mut bsd: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of_val(&bsd) as i32;
        let read = unsafe {
            libc::proc_pidinfo(
                helper.pid() as i32,
                libc::PROC_PIDTBSDINFO,
                0,
                (&mut bsd as *mut libc::proc_bsdinfo).cast(),
                size,
            )
        };
        if start.elapsed() > Duration::from_secs(2)
            || p.helper != helper
            || p.gui != gui
            || read != size
            || bsd.pbi_ppid != gui.pid()
            || gui.uid() != helper.uid()
            || gui.session() != helper.session()
            || p.generation != u64::from(helper.token[7])
        {
            return Err(Error::Deferred(
                "unverified helper/GUI/session relationship".into(),
            ));
        }
        sessions.push(Session {
            stream,
            before: p,
            sequence: p.sequence,
            executable: executable.into(),
            received: Instant::now(),
        });
    }
    // A legacy Client without participation is a migration deferral. Merely
    // finding no GUI session is not permission to signal an unregistered app.
    let covered: std::collections::BTreeSet<_> = sessions
        .iter()
        .flat_map(|s| [s.before.gui.pid(), s.before.helper.pid()])
        .collect();
    let observed: std::collections::BTreeSet<_> = snapshot.clients.iter().map(|p| p.pid).collect();
    if covered != observed || covered.len() != sessions.len() * 2 {
        return Err(Error::Deferred(
            "legacy/unregistered/ambiguous Client; no signals sent".into(),
        ));
    }
    Ok(sessions)
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Fake {
        instance: Instance,
        live: bool,
        refuse: bool,
        elapsed: Duration,
        signals: Vec<i32>,
        graceful: usize,
        exit_at: Option<Duration>,
        reuse_at_signal: Option<i32>,
        actions: Vec<StopAction>,
    }
    impl StopAdapter for Fake {
        fn record(&mut self, action: StopAction) {
            self.actions.push(action);
        }
        fn graceful(&mut self, i: Instance) -> Result<()> {
            assert_eq!(i, self.instance);
            self.graceful += 1;
            Ok(())
        }
        fn alive(&self, i: Instance) -> bool {
            self.live && i == self.instance
        }
        fn signal(&mut self, i: Instance, s: i32) -> Result<()> {
            assert_eq!(i, self.instance);
            // Simulate reuse between alive() and the signal syscall, including
            // the second escalation after SIGTERM has already been delivered.
            if self.reuse_at_signal == Some(s) {
                self.instance.token[7] += 1;
            }
            if self.refuse || i != self.instance {
                return Err(Error::Deferred("identity changed before signal".into()));
            }
            self.signals.push(s);
            Ok(())
        }
        fn wait(&mut self, d: Duration) {
            self.elapsed += d;
            if self.exit_at.is_some_and(|t| self.elapsed >= t) {
                self.live = false;
            }
        }
    }
    fn fake() -> Fake {
        Fake {
            instance: Instance {
                token: [42; 8],
                device: 1,
                inode: 2,
            },
            live: true,
            refuse: false,
            elapsed: Duration::ZERO,
            signals: vec![],
            graceful: 0,
            exit_at: None,
            reuse_at_signal: None,
            actions: vec![],
        }
    }
    #[test]
    fn graceful_exit_never_escalates_and_timeout_is_bounded() {
        let mut f = fake();
        f.exit_at = Some(Duration::from_secs(1));
        let i = f.instance;
        stop(&mut f, i).unwrap();
        assert_eq!(f.graceful, 1);
        assert!(f.signals.is_empty());
        let mut f = fake();
        assert!(stop(&mut f, i).is_err());
        assert_eq!(f.signals, [libc::SIGTERM, libc::SIGKILL]);
        assert_eq!(f.elapsed, Duration::from_secs(12));
        assert_eq!(
            f.actions,
            [
                StopAction::GracefulRequested { instance: i },
                StopAction::SentSigterm { instance: i },
                StopAction::SentSigkill { instance: i }
            ]
        );
    }
    #[test]
    fn pid_reuse_at_each_signal_boundary_preserves_stop_trace() {
        for signal in [libc::SIGTERM, libc::SIGKILL] {
            let mut f = fake();
            let i = f.instance;
            f.reuse_at_signal = Some(signal);
            assert!(stop(&mut f, i).is_err());
            let mut expected = vec![StopAction::GracefulRequested { instance: i }];
            if signal == libc::SIGKILL {
                assert_eq!(f.signals, [libc::SIGTERM]);
                expected.push(StopAction::SentSigterm { instance: i });
            } else {
                assert!(f.signals.is_empty());
            }
            assert_eq!(f.actions, expected);
            assert_ne!(f.instance.token[7], i.token[7]);
        }
    }
    #[test]
    fn reused_pid_or_changed_image_is_never_signalled() {
        let mut f = fake();
        f.refuse = true;
        let i = f.instance;
        assert!(stop(&mut f, i).is_err());
        assert!(f.signals.is_empty());
        let mut f = fake();
        f.instance.token[7] += 1;
        // A kernel token no longer resolves; an unrelated reused PID is alive
        // in the fake OS, but the captured instance is gone.
        assert!(!f.alive(i));
    }
}
