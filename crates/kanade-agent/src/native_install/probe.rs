//! The one thing the installer ever executes: the CLI's own `--version`.
//!
//! Fixed argv, empty environment, no stdin, bounded output and wall time, in
//! its own process group. There is no shell, no PATH lookup and no other
//! subprocess anywhere in the installer.

use std::path::Path;
use std::time::Duration;

/// Maximum bytes of `--version` output considered.
pub const MAX_PROBE_OUTPUT: usize = 4096;
pub const DEFAULT_PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// Adapter over "run `<executable> --version`". Tests substitute a fake so
/// the transaction logic can be exercised with synthetic Mach-O payloads.
pub trait VersionProbe: Send + Sync {
    /// The first line the executable prints for `--version`.
    fn version_line(&self, executable: &Path) -> Result<String, String>;
}

/// `kanade <version>` is what the CLI prints; this is the line to expect.
pub fn expected_line(version: &str) -> String {
    format!("kanade {version}")
}

#[cfg(unix)]
#[allow(unused_imports)]
pub use real::RealProbe;

#[cfg(unix)]
mod real {
    use std::io::Read;
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    use std::time::Instant;

    use super::*;

    pub struct RealProbe {
        pub timeout: Duration,
    }

    impl Default for RealProbe {
        fn default() -> Self {
            Self {
                timeout: DEFAULT_PROBE_TIMEOUT,
            }
        }
    }

    /// Kill the probe's own process group (it was started as its own group
    /// leader), so a child that inherited stdout cannot outlive the timeout.
    fn kill_group(child: &mut std::process::Child) {
        // SAFETY: signals only the group we just created for this child.
        unsafe {
            libc::killpg(child.id() as libc::pid_t, libc::SIGKILL);
        }
        let _ = child.kill();
        let _ = child.wait();
    }

    impl VersionProbe for RealProbe {
        fn version_line(&self, executable: &Path) -> Result<String, String> {
            if !executable.is_absolute() {
                return Err("probe path must be absolute".into());
            }
            let mut child = Command::new(executable)
                .arg("--version")
                .env_clear()
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .current_dir("/")
                .process_group(0)
                .spawn()
                .map_err(|e| format!("cannot run: {e}"))?;
            let mut stdout = child.stdout.take().expect("piped");
            let reader = std::thread::spawn(move || {
                let mut buf = Vec::new();
                let _ = (&mut stdout)
                    .take(MAX_PROBE_OUTPUT as u64 + 1)
                    .read_to_end(&mut buf);
                buf
            });
            let deadline = Instant::now() + self.timeout;
            let status = loop {
                match child.try_wait() {
                    Ok(Some(s)) => break Some(s),
                    Ok(None) if Instant::now() < deadline => {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Ok(None) => {
                        kill_group(&mut child);
                        break None;
                    }
                    Err(e) => return Err(format!("wait failed: {e}")),
                }
            };
            let out = reader.join().map_err(|_| "reader panicked".to_string())?;
            let status = status.ok_or_else(|| "timed out".to_string())?;
            if !status.success() {
                return Err(format!("exited with {status}"));
            }
            if out.len() > MAX_PROBE_OUTPUT {
                return Err("output too large".into());
            }
            let text = String::from_utf8(out).map_err(|_| "output is not UTF-8".to_string())?;
            Ok(text.lines().next().unwrap_or("").trim().to_owned())
        }
    }

    #[cfg(test)]
    mod tests {
        use std::os::unix::fs::PermissionsExt;

        use super::*;

        fn script(dir: &Path, body: &str) -> std::path::PathBuf {
            let p = dir.join("fake-cli");
            std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
            p
        }

        #[test]
        fn runs_with_a_fixed_argv_and_empty_environment() {
            let t = tempfile::tempdir().unwrap();
            let p = script(
                t.path(),
                r#"[ "$1" = "--version" ] && [ -z "$HOME" ] && [ -z "$PATH_X" ] && echo "kanade 1.2.3""#,
            );
            let line = RealProbe::default().version_line(&p).unwrap();
            assert_eq!(line, "kanade 1.2.3");
        }

        #[test]
        fn failure_timeout_and_flood_are_bounded() {
            let t = tempfile::tempdir().unwrap();
            let p = script(t.path(), "exit 3");
            assert!(RealProbe::default().version_line(&p).is_err());
            let p = script(t.path(), "sleep 30");
            let probe = RealProbe {
                timeout: Duration::from_millis(200),
            };
            let t0 = Instant::now();
            assert_eq!(probe.version_line(&p).unwrap_err(), "timed out");
            assert!(t0.elapsed() < Duration::from_secs(5));
            let p = script(t.path(), "yes kanade | head -c 100000");
            assert!(RealProbe::default().version_line(&p).is_err());
            assert!(
                RealProbe::default()
                    .version_line(Path::new("relative"))
                    .is_err()
            );
        }
    }
}
