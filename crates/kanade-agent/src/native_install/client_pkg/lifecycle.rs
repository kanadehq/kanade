//! Transaction-bound launch exclusion. Native signals use kernel audit tokens,
//! never executable basenames or caller-selected PIDs. GUI eligibility is not
//! KLP admission. Remote typed dispatch remains disabled.
use super::{Error, Result};
#[cfg(any(target_os = "macos", test))]
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

/// Native exchanges authenticate kernel peers and bound response age before
/// returning Presence. Restore must recheck session/veto/process facts again at
/// launch, since Observe alone cannot authorize a later launch.
#[cfg(any(target_os = "macos", test))]
pub(super) trait SessionAdapter {
    fn before(&self) -> Presence;
    fn observe(&mut self) -> Result<Presence>;
    fn restore(&mut self) -> Result<bool>;
    fn release(&mut self);
}
#[cfg(any(target_os = "macos", test))]
pub(super) fn finish_sessions<S: SessionAdapter>(
    sessions: &mut [S],
) -> super::result::GuiActivation {
    use super::result::GuiActivation;
    let mut launched = 0;
    for session in sessions.iter_mut() {
        let before = session.before();
        if session.observe().is_ok_and(|p| before.may_restore(&p)) {
            if session.restore().unwrap_or(false) {
                launched += 1;
            }
        } else {
            session.release();
        }
    }
    match launched {
        0 => GuiActivation::NotLaunched,
        n if n == sessions.len() => GuiActivation::RestoreLaunchedUnverified,
        _ => GuiActivation::RestorePartiallyLaunchedUnverified,
    }
}
#[cfg(any(target_os = "macos", test))]
trait DiscoveryAdapter {
    type Session: SessionAdapter;
    fn observed(&mut self) -> Result<Vec<u32>>;
    /// Returns only a peer/process/session authenticated session. No endpoint
    /// means unregistered, never permission to stop by PID alone.
    fn probe(&mut self, pid: u32) -> Result<Option<Self::Session>>;
}
#[cfg(any(target_os = "macos", test))]
fn discover_sessions<A: DiscoveryAdapter>(adapter: &mut A) -> Result<Vec<A::Session>> {
    let observed = adapter.observed()?;
    let mut sessions = Vec::new();
    for pid in &observed {
        if let Some(session) = adapter.probe(*pid)? {
            sessions.push(session);
        }
    }
    verify_coverage(observed, sessions.iter().map(SessionAdapter::before))?;
    Ok(sessions)
}

/// Discovery must cover every observed fixed-image process exactly once;
/// an unregistered legacy GUI defers the whole operation before any stop.
#[cfg(any(target_os = "macos", test))]
fn verify_coverage(
    observed: impl IntoIterator<Item = u32>,
    sessions: impl IntoIterator<Item = Presence>,
) -> Result<()> {
    let mut covered = std::collections::BTreeSet::new();
    for p in sessions {
        if p.gui.uid() == 0
            || p.gui.uid() != p.helper.uid()
            || p.gui.session() == 0
            || p.gui.session() != p.helper.session()
            || !covered.insert(p.gui.pid())
            || !covered.insert(p.helper.pid())
        {
            return Err(Error::Deferred("ambiguous Client session coverage".into()));
        }
    }
    if covered != observed.into_iter().collect() {
        return Err(Error::Deferred(
            "legacy/unregistered/ambiguous Client; no signals sent".into(),
        ));
    }
    Ok(())
}

#[cfg(any(target_os = "macos", test))]
fn validate_response(
    before: Presence,
    sequence: u64,
    current: Presence,
    elapsed: Duration,
) -> Result<()> {
    if elapsed > Duration::from_secs(2)
        || current.gui != before.gui
        || current.helper != before.helper
        || current.generation != before.generation
        || current.sequence <= sequence
    {
        return Err(Error::Deferred(
            "stale/replayed/contradictory session presence".into(),
        ));
    }
    Ok(())
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
            Ok(finish_sessions(self.sessions.get_mut()))
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
        // /Library predates the payload-only pkg. Clients installed alongside
        // an older Agent pin it until their GUI exits, even before provisioning.
        let writer = if self.target == std::path::Path::new("/") {
            writer.with_bootstrap(std::path::Path::new("/Library"), 0)?
        } else {
            writer
        };
        #[cfg(target_os = "macos")]
        let sessions = if recovery {
            // Restart never rediscovers restart intent from an arbitrary PID.
            // Only sessions this process already stopped and still holds an
            // authenticated channel for are carried over; after a crash the
            // channels are gone and no automatic GUI launch happens.
            std::mem::take(&mut *self.carried.lock().unwrap())
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
        if !recovery {
            let pending: Vec<_> = lease.sessions.borrow_mut().drain(..).collect();
            let mut stopped = Vec::new();
            let mut failure = None;
            for mut session in pending {
                if failure.is_some() {
                    // Never stopped: its GUI keeps running; dropping the
                    // channel only disconnects the helper.
                    continue;
                }
                let gui = session.before.gui;
                match stop(&mut session, gui) {
                    Ok(()) => stopped.push(session),
                    Err(e) => failure = Some(e),
                }
            }
            if let Some(e) = failure {
                // Keep already-stopped sessions for the recovery lease.
                self.carried.lock().unwrap().extend(stopped);
                return Err(e);
            }
            *lease.sessions.borrow_mut() = stopped;
        }
        if !recovery {
            super::system::LaunchLease::recheck(&lease)?;
        }
        Ok(Box::new(lease))
    }
}

#[cfg(target_os = "macos")]
pub(super) struct Session {
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
        validate_response(self.before, self.sequence, p, start.elapsed())?;
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
impl SessionAdapter for Session {
    fn before(&self) -> Presence {
        self.before
    }
    fn observe(&mut self) -> Result<Presence> {
        self.exchange(kanade_shared::client_lifecycle::macos::Request::Observe)
    }
    fn restore(&mut self) -> Result<bool> {
        Session::restore(self)
    }
    fn release(&mut self) {
        let _ = self.exchange(kanade_shared::client_lifecycle::macos::Request::Release);
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
    discover_sessions(&mut NativeDiscovery { executable })
}
#[cfg(target_os = "macos")]
struct NativeDiscovery<'a> {
    executable: &'a Path,
}
#[cfg(target_os = "macos")]
impl DiscoveryAdapter for NativeDiscovery<'_> {
    type Session = Session;
    fn observed(&mut self) -> Result<Vec<u32>> {
        use super::processes::{NativeProcessView, ProcessView};
        let snapshot = NativeProcessView {
            client_exe: self.executable.into(),
        }
        .snapshot()?;
        if !snapshot.ambiguous.is_empty() || snapshot.installer_active {
            return Err(Error::Deferred(
                "ambiguous Client/installer snapshot".into(),
            ));
        }
        Ok(snapshot.clients.into_iter().map(|p| p.pid).collect())
    }
    fn probe(&mut self, pid: u32) -> Result<Option<Session>> {
        use kanade_shared::client_lifecycle::macos;
        let executable = self.executable;
        let Ok(mut stream) = std::os::unix::net::UnixStream::connect(macos::endpoint(pid)) else {
            return Ok(None);
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
        if peer.pid != token[5] || peer.uid != token[1] || peer.pid != pid {
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
        Ok(Some(Session {
            stream,
            before: p,
            sequence: p.sequence,
            executable: executable.into(),
            received: Instant::now(),
        }))
    }
}

#[cfg(test)]
pub(super) mod simulation {
    use super::*;
    #[derive(Clone)]
    pub struct Session {
        pub before: Presence,
        pub observed: Option<Presence>,
        pub at_launch: Presence,
        pub helper_alive: bool,
        pub launches: usize,
        pub released: bool,
    }
    impl SessionAdapter for Session {
        fn before(&self) -> Presence {
            self.before
        }
        fn observe(&mut self) -> Result<Presence> {
            self.observed
                .ok_or_else(|| Error::Deferred("stale/unknown/disconnected session".into()))
        }
        fn restore(&mut self) -> Result<bool> {
            let eligible = self.helper_alive && self.before.may_restore(&self.at_launch);
            if eligible {
                self.launches += 1;
            }
            Ok(eligible)
        }
        fn release(&mut self) {
            self.released = true;
        }
    }
    pub fn session(uid: u32, audit: u32, pid: u32) -> Session {
        let mut token = [0; 8];
        token[1] = uid;
        token[5] = pid;
        token[6] = audit;
        token[7] = 17;
        let gui = Instance {
            token,
            device: 1,
            inode: 2,
        };
        token[5] += 1;
        let before = Presence {
            gui,
            helper: Instance { token, ..gui },
            generation: 17,
            sequence: 1,
            local_active_unlocked: true,
            quit_generation: 0,
            opted_out: false,
        };
        let fresh = Presence {
            sequence: 2,
            ..before
        };
        Session {
            before,
            observed: Some(fresh),
            at_launch: fresh,
            helper_alive: true,
            launches: 0,
            released: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn restore_two_uids_fast_switch_and_vetoes_at_launch() {
        use super::super::result::GuiActivation;
        use simulation::session;
        let mut sessions = [session(501, 9, 10), session(502, 11, 20)];
        // Second UID becomes inactive during update; no console-UID inference.
        sessions[1].at_launch.local_active_unlocked = false;
        assert_eq!(
            finish_sessions(&mut sessions),
            GuiActivation::RestorePartiallyLaunchedUnverified
        );
        assert_eq!((sessions[0].launches, sessions[1].launches), (1, 0));
        for veto in [
            "quit_before",
            "quit_during",
            "optout_before",
            "optout_during",
            "lock",
            "logout",
            "ssh",
            "unknown",
            "reuse",
            "switch",
            "helper_exit",
        ] {
            let mut s = session(501, 9, 10);
            match veto {
                "quit_before" => s.before.quit_generation = 1,
                "quit_during" => s.at_launch.quit_generation = 1,
                "optout_before" => s.before.opted_out = true,
                "optout_during" => s.at_launch.opted_out = true,
                "lock" | "logout" => s.at_launch.local_active_unlocked = false,
                "ssh" => s.before.local_active_unlocked = false,
                "unknown" => s.observed = None,
                "reuse" => s.at_launch.gui.token[7] += 1,
                "switch" => s.at_launch.gui.token[6] += 1,
                "helper_exit" => s.helper_alive = false,
                _ => unreachable!(),
            }
            let mut sessions = [s];
            assert_eq!(
                finish_sessions(&mut sessions),
                GuiActivation::NotLaunched,
                "{veto}"
            );
            assert_eq!(sessions[0].launches, 0, "{veto}");
        }
    }
    #[test]
    fn presence_validation_rejects_age_replay_and_changed_identity() {
        let before = simulation::session(501, 9, 10).before;
        let fresh = Presence {
            sequence: 2,
            ..before
        };
        validate_response(before, 1, fresh, Duration::from_millis(100)).unwrap();
        assert!(validate_response(before, 1, fresh, Duration::from_secs(3)).is_err());
        assert!(validate_response(before, 2, fresh, Duration::ZERO).is_err());
        for kind in [
            "pid_generation",
            "uid",
            "audit_session",
            "inode",
            "helper",
            "generation",
        ] {
            let mut changed = fresh;
            match kind {
                "pid_generation" => changed.gui.token[7] += 1,
                "uid" => changed.gui.token[1] += 1,
                "audit_session" => changed.gui.token[6] += 1,
                "inode" => changed.gui.inode += 1,
                "helper" => changed.helper.token[7] += 1,
                "generation" => changed.generation += 1,
                _ => unreachable!(),
            }
            assert!(
                validate_response(before, 1, changed, Duration::ZERO).is_err(),
                "{kind}"
            );
        }
    }
    struct FakeDiscovery {
        observed: Vec<u32>,
        sessions: Vec<simulation::Session>,
        ambiguous: bool,
    }
    impl DiscoveryAdapter for FakeDiscovery {
        type Session = simulation::Session;
        fn observed(&mut self) -> Result<Vec<u32>> {
            if self.ambiguous {
                return Err(Error::Deferred("unknown process snapshot".into()));
            }
            Ok(self.observed.clone())
        }
        fn probe(&mut self, pid: u32) -> Result<Option<Self::Session>> {
            Ok(self
                .sessions
                .iter()
                .find(|s| s.before.helper.pid() == pid)
                .cloned())
        }
    }
    #[test]
    fn discovery_adapter_covers_two_uids_and_refuses_unknown_or_unregistered() {
        let mut adapter = FakeDiscovery {
            observed: vec![10, 11, 20, 21],
            sessions: vec![
                simulation::session(501, 9, 10),
                simulation::session(502, 11, 20),
            ],
            ambiguous: false,
        };
        let mut sessions = discover_sessions(&mut adapter).unwrap();
        assert_eq!(sessions.len(), 2);
        assert_eq!(
            finish_sessions(&mut sessions),
            super::super::result::GuiActivation::RestoreLaunchedUnverified
        );
        adapter.observed.push(99);
        assert!(discover_sessions(&mut adapter).is_err());
        assert!(adapter.sessions.iter().all(|s| s.launches == 0));
        adapter.observed.pop();
        adapter.ambiguous = true;
        assert!(discover_sessions(&mut adapter).is_err());
    }
    #[test]
    fn discovery_defers_legacy_unregistered_or_ambiguous_processes() {
        let a = simulation::session(501, 9, 10).before;
        let b = simulation::session(502, 11, 20).before;
        verify_coverage([10, 11, 20, 21], [a, b]).unwrap();
        assert!(verify_coverage([10, 11, 99], [a]).is_err());
        assert!(verify_coverage([10, 11], [a, a]).is_err());
        let mut mismatched = a;
        mismatched.helper.token[1] = 502;
        assert!(verify_coverage([10, 11], [mismatched]).is_err());
        mismatched = a;
        mismatched.helper.token[6] += 1;
        assert!(verify_coverage([10, 11], [mismatched]).is_err());
    }
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
