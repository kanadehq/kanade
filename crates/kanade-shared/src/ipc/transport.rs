//! KLP transport endpoints shared by the agent and client.

/// SPEC §2.12.1 Windows endpoint.
pub const PIPE_NAME: &str = r"\\.\pipe\kanade-agent";
/// Non-empty values override the Unix endpoint for development and tests.
pub const ENV_SOCKET_PATH: &str = "KANADE_KLP_SOCKET_PATH";

#[cfg(target_os = "linux")]
pub const DEFAULT_SOCKET_PATH: &str = "/run/kanade/agent.sock";
#[cfg(target_os = "macos")]
pub const DEFAULT_SOCKET_PATH: &str = "/var/run/kanade/agent.sock";

#[cfg(any(target_os = "linux", target_os = "macos"))]
/// Resolve an injected override without mutating process-wide environment.
pub fn socket_path_from_override(value: Option<std::ffi::OsString>) -> std::path::PathBuf {
    match value {
        Some(p) if !p.is_empty() => p.into(),
        _ => DEFAULT_SOCKET_PATH.into(),
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
/// Resolve the production endpoint using the shared environment override.
pub fn socket_path() -> std::path::PathBuf {
    socket_path_from_override(std::env::var_os(ENV_SOCKET_PATH))
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod tests {
    use super::*;

    #[test]
    fn socket_override_and_empty_default() {
        assert_eq!(
            socket_path_from_override(Some("test.sock".into())),
            std::path::PathBuf::from("test.sock")
        );
        assert_eq!(
            socket_path_from_override(None),
            std::path::PathBuf::from(DEFAULT_SOCKET_PATH)
        );
        assert_eq!(
            socket_path_from_override(Some("".into())),
            std::path::PathBuf::from(DEFAULT_SOCKET_PATH)
        );
    }
}
