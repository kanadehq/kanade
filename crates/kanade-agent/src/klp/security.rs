//! Named Pipe security descriptor for KLP (SPEC §2.12.1).
//!
//! Per SPEC: "Pipe security descriptor を Authenticated Users に
//! RW、Everyone / Anonymous は拒否". The default Windows Named
//! Pipe SD grants LocalSystem + admins full control and Everyone
//! read — which means:
//!
//! - A standard interactive user can READ the pipe but cannot
//!   send commands (the agent's responses go nowhere from their
//!   side; their writes are rejected). The Client App is broken.
//! - The Anonymous Logon account inherits the Everyone read,
//!   which we want to refuse outright.
//!
//! Both problems are fixed by an explicit DACL. SDDL is the
//! least-verbose way to construct one: a single string compiles
//! to the right SECURITY_DESCRIPTOR, and `LookupAccountName`-style
//! sub-authority bookkeeping stays inside the Win32 ACL APIs.
//!
//! ## SDDL string breakdown
//!
//! `D:(D;;GA;;;AN)(A;;GA;;;AU)` — a DACL with two ACEs:
//!
//! - `(D;;GA;;;AN)` — Deny `Generic All` to `Anonymous Logon`
//!   (S-1-5-7). Deny ACEs MUST precede allow ACEs in canonical
//!   DACL order; Windows evaluates top-down and the first match
//!   wins.
//! - `(A;;GA;;;AU)` — Allow `Generic All` to `Authenticated
//!   Users` (S-1-5-11). Covers every interactive user that has
//!   completed a logon — exactly the set the Client App targets.
//!
//! Note that LocalSystem (S-1-5-18) is itself an Authenticated
//! User, so the agent's own self-test connection (and any
//! service-context tooling) still works after the SD is applied.

use std::ffi::c_void;

use anyhow::{Context, Result};
use windows::Win32::Foundation::{HLOCAL, LocalFree};
use windows::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
use windows::core::{BOOL, w};

const PIPE_SDDL: windows::core::PCWSTR = w!("D:(D;;GA;;;AN)(A;;GA;;;AU)");

/// Owned [`SECURITY_ATTRIBUTES`] + backing
/// [`SECURITY_DESCRIPTOR`] suitable for tokio's
/// `ServerOptions::create_with_security_attributes_raw`.
///
/// The wrapper is constructed once per listener; the same
/// instance can be reused for every pipe re-arm because Windows
/// copies the SD into each newly-created pipe handle internally
/// (the API contract for `CreateNamedPipe`).
///
/// Drop releases the SD via `LocalFree` per the
/// `ConvertStringSecurityDescriptorToSecurityDescriptorW` API
/// contract. **Don't take a raw pointer out and stash it past
/// the wrapper's lifetime** — use [`Self::as_ptr`] inline with
/// the unsafe tokio call only.
pub struct PipeSecurity {
    sa: SECURITY_ATTRIBUTES,
    sd: PSECURITY_DESCRIPTOR,
}

// SAFETY: the wrapper holds raw pointers (`*mut c_void` inside
// `SECURITY_ATTRIBUTES` and `PSECURITY_DESCRIPTOR`), so the
// auto-derived `!Send + !Sync` would forbid use across `await`
// points in the listener task. The SD itself is immutable after
// construction; the Win32 APIs that read it (CreateNamedPipe)
// are documented thread-safe; and the wrapper has no shared
// mutable state. Crossing threads (tokio's multi-thread runtime
// may move the future) is safe.
unsafe impl Send for PipeSecurity {}
unsafe impl Sync for PipeSecurity {}

impl PipeSecurity {
    /// Build a fresh SECURITY_DESCRIPTOR from
    /// [`PIPE_SDDL`]. The descriptor is LocalAlloc'd by the Win32
    /// API; the wrapper's Drop releases it.
    pub fn new() -> Result<Self> {
        let mut sd = PSECURITY_DESCRIPTOR::default();
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                PIPE_SDDL,
                SDDL_REVISION_1,
                &mut sd,
                None,
            )
            .context("ConvertStringSecurityDescriptorToSecurityDescriptorW failed")?;
        }
        let sa = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: sd.0,
            // We don't want child processes of the agent to
            // inherit the pipe handle by accident.
            bInheritHandle: BOOL(0),
        };
        Ok(Self { sa, sd })
    }

    /// Raw pointer to the embedded `SECURITY_ATTRIBUTES`, suitable
    /// for the `attrs` parameter of tokio's
    /// `create_with_security_attributes_raw`. The pointer stays
    /// valid as long as `self` is borrowed; don't outlive it.
    pub fn as_ptr(&self) -> *mut c_void {
        &self.sa as *const SECURITY_ATTRIBUTES as *mut c_void
    }
}

impl Drop for PipeSecurity {
    fn drop(&mut self) {
        if !self.sd.0.is_null() {
            // ConvertStringSecurityDescriptorToSecurityDescriptorW
            // uses LocalAlloc internally per its API contract; the
            // matching free is LocalFree, NOT free(). Cast through
            // HLOCAL to satisfy the signature.
            unsafe {
                let _ = LocalFree(Some(HLOCAL(self.sd.0)));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pipe_security_constructs_and_drops_without_leak() {
        // Smoke test: just builds + drops the wrapper. Will panic
        // (via `unwrap`) if the SDDL is malformed, the feature
        // flags are wrong, or the LocalFree on Drop double-frees.
        let sec = PipeSecurity::new().expect("SDDL → SD should succeed");
        let ptr = sec.as_ptr();
        assert!(
            !ptr.is_null(),
            "SECURITY_ATTRIBUTES pointer must be non-null"
        );
        // Drop runs here; nothing to assert beyond "no crash".
        drop(sec);
    }

    #[test]
    fn pipe_security_as_ptr_is_stable_for_lifetime() {
        // The `unsafe` tokio API holds the pointer across one call;
        // if `as_ptr()` returned a fresh address each call (e.g. via
        // Box allocation), repeated SD applications could race. Pin
        // the stability so a future refactor can't quietly break this.
        let sec = PipeSecurity::new().unwrap();
        let p1 = sec.as_ptr();
        let p2 = sec.as_ptr();
        assert_eq!(p1, p2, "as_ptr() must return a stable address");
    }
}
