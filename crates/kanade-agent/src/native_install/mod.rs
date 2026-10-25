//! Native installer foundation (SPEC §2.13 Task 5, migration stage 5a):
//! the macOS Apple Silicon admin CLI — install, update, adopt, no-op and
//! recovery — and nothing else.
//!
//! This is a **local, offline** interface. Nothing in the agent calls it:
//! the handler registry that feeds the heartbeat stays empty, the backend's
//! typed-delivery barrier is untouched, and there is no remote path, no
//! scheduler hook and no privilege-escalating shortcut. A later stage wires
//! a verified signed command, replay protection, plan approval and result
//! publication to [`Installer::install`].
//!
//! What it does *not* do, by design: kill processes, register services,
//! launch anything in a user session, run a shell, or run any program other
//! than the CLI's own bounded `--version`.
//!
//! The archive, Mach-O, receipt/journal and result contracts compile on every
//! platform so they are tested everywhere; the filesystem transaction needs
//! Unix primitives (`openat`, `renameat`-with-flags, `flock`).

// Unused until a later stage wires the handler (see above).
#![allow(dead_code)]

pub mod admit;
pub mod archive;
pub mod macho;
pub mod model;
pub mod probe;

#[cfg(unix)]
pub mod fs;
#[cfg(unix)]
pub mod txn;

#[cfg(all(test, unix))]
mod tests;
