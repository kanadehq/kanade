//! Local macOS Client pkg transaction foundation. Not registered, advertised or
//! reachable from remote typed dispatch. All writable roots must be temporary.
//! Installer receipts belong to the OS: neither update nor rollback swaps app
//! files. GUI activation/handshake is explicitly unsupported in this slice.
mod structure;
pub mod system;
#[cfg(test)]
mod tests;
pub mod transaction;

use crate::native_install::model::InstallError;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("package contract refused: {0}")]
    Invalid(String),
    #[error("authorization/admission refused: {0}")]
    Authorization(String),
    #[error("deployment deferred: {0}")]
    Deferred(String),
    #[error("unmanaged bundle or installer receipt; client adoption is unsupported")]
    Unmanaged,
    #[error("recovery_required: {0}")]
    RecoveryRequired(String),
    #[error("install failed; previous verified pkg restored through installer")]
    RolledBack,
    #[error("simulated interruption")]
    Interrupted,
    #[error(transparent)]
    Io(#[from] std::io::Error),
}
type Result<T> = std::result::Result<T, Error>;
impl From<InstallError> for Error {
    fn from(e: InstallError) -> Self {
        Self::Authorization(e.to_string())
    }
}
