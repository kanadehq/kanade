//! Read-only process evidence. An empty snapshot never grants a launch lease.
use std::path::PathBuf;

use super::{Error, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientProcess {
    pub pid: u32,
    pub uid: u32,
    pub start_unix: u64,
    pub executable: PathBuf,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct ProcessSnapshot {
    pub clients: Vec<ClientProcess>,
    pub ambiguous: Vec<u32>,
    pub installer_active: bool,
}
impl ProcessSnapshot {
    pub fn require_inactive(&self) -> Result<()> {
        if !self.clients.is_empty() || !self.ambiguous.is_empty() || self.installer_active {
            return Err(Error::Deferred(
                "active Client/installer or ambiguous process evidence; no process was signalled"
                    .into(),
            ));
        }
        Ok(())
    }
}

/// No stop, signal or launch operation is exposed.
pub trait ProcessView {
    fn snapshot(&self) -> Result<ProcessSnapshot>;
}

pub struct NativeProcessView {
    pub client_exe: PathBuf,
}
struct Observation {
    pid: u32,
    name: String,
    exe: Option<PathBuf>,
    uid: Option<u32>,
    start_unix: u64,
}
impl NativeProcessView {
    fn classify(
        &self,
        observations: impl IntoIterator<Item = Observation>,
    ) -> Result<ProcessSnapshot> {
        let mut snapshot = ProcessSnapshot::default();
        let mut count = 0;
        for p in observations {
            count += 1;
            match p.exe.as_deref() {
                Some(exe) if exe == self.client_exe => {
                    if let Some(uid) = p.uid.filter(|_| p.start_unix > 0) {
                        snapshot.clients.push(ClientProcess {
                            pid: p.pid,
                            uid,
                            start_unix: p.start_unix,
                            executable: exe.to_owned(),
                        });
                    } else {
                        snapshot.ambiguous.push(p.pid);
                    }
                }
                Some(exe) if exe == std::path::Path::new("/usr/sbin/installer") => {
                    snapshot.installer_active = true;
                }
                None if p.name == "kanade-client" || p.name == "installer" => {
                    snapshot.ambiguous.push(p.pid);
                }
                _ => {}
            }
        }
        if count == 0 {
            return Err(Error::Deferred("process table empty or unreadable".into()));
        }
        Ok(snapshot)
    }
}
impl ProcessView for NativeProcessView {
    fn snapshot(&self) -> Result<ProcessSnapshot> {
        use sysinfo::{ProcessesToUpdate, System};
        let mut system = System::new();
        system.refresh_processes(ProcessesToUpdate::All, true);
        self.classify(system.processes().iter().map(|(pid, p)| Observation {
            pid: pid.as_u32(),
            name: p.name().to_string_lossy().into_owned(),
            exe: p.exe().map(std::path::Path::to_owned),
            uid: p.user_id().map(|uid| **uid),
            start_unix: p.start_time(),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view() -> NativeProcessView {
        NativeProcessView {
            client_exe: "/isolated/Applications/Kanade Client.app/Contents/MacOS/kanade-client"
                .into(),
        }
    }
    fn observed(name: &str, exe: Option<PathBuf>) -> Observation {
        Observation {
            pid: 42,
            name: name.into(),
            exe,
            uid: Some(501),
            start_unix: 123,
        }
    }

    #[test]
    fn executable_identity_is_required_and_name_only_matches_are_ambiguous() {
        let view = view();
        let unrelated = view
            .classify([observed(
                "kanade-client",
                Some("/other/kanade-client".into()),
            )])
            .unwrap();
        assert!(unrelated.clients.is_empty());
        assert!(unrelated.require_inactive().is_ok());
        let matched = view
            .classify([observed("renamed", Some(view.client_exe.clone()))])
            .unwrap();
        assert_eq!(
            matched.clients,
            [ClientProcess {
                pid: 42,
                uid: 501,
                start_unix: 123,
                executable: view.client_exe.clone(),
            }]
        );
        assert!(matched.require_inactive().is_err());
        for name in ["kanade-client", "installer"] {
            let ambiguous = view.classify([observed(name, None)]).unwrap();
            assert_eq!(ambiguous.ambiguous, [42]);
            assert!(ambiguous.require_inactive().is_err());
        }
        let installer = view
            .classify([observed("renamed", Some("/usr/sbin/installer".into()))])
            .unwrap();
        assert!(installer.installer_active);
        assert!(installer.require_inactive().is_err());
    }

    #[test]
    fn unavailable_table_or_incomplete_client_identity_defers() {
        let view = view();
        assert!(matches!(view.classify([]), Err(Error::Deferred(_))));
        for (uid, start_unix) in [(None, 123), (Some(501), 0)] {
            let mut p = observed("client", Some(view.client_exe.clone()));
            p.uid = uid;
            p.start_unix = start_unix;
            let snapshot = view.classify([p]).unwrap();
            assert_eq!(snapshot.ambiguous, [42]);
            assert!(snapshot.require_inactive().is_err());
        }
    }
}
