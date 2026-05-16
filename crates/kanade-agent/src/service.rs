//! Windows Service Control Manager (SCM) integration.
//!
//! Without this, running the agent under `sc.exe create … binPath=
//! "<exe>"` boots the process but never calls
//! `StartServiceCtrlDispatcher`, so SCM times out after 30 s and
//! marks the service "did not respond" (Event ID 7009). With it,
//! the agent registers a control handler, transitions to Running,
//! and blocks until SCM sends Stop / Shutdown — at which point the
//! tokio runtime is signalled, the agent shuts down, and we report
//! Stopped back to SCM.
//!
//! Self-update's `std::process::exit(64)` still works because
//! `sc.exe failure` + `failureflag 1` (configured by
//! deploy-agent.ps1) treat any non-zero exit — including those
//! that bypass the status-Stopped transition — as a recoverable
//! failure and restart the service. The atomic swap from
//! self_update.rs places the new binary at the same path SCM uses,
//! so the restart runs the new build.

#![cfg(target_os = "windows")]

use std::ffi::OsString;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use windows_service::define_windows_service;
use windows_service::service::{
    ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus, ServiceType,
};
use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
use windows_service::service_dispatcher;

const SERVICE_NAME: &str = "KanadeAgent";
const SERVICE_TYPE: ServiceType = ServiceType::OWN_PROCESS;

/// Try connecting to SCM. Returns `Ok(())` after a clean Service
/// dispatcher round-trip (service started, SCM sent Stop, we
/// returned). Returns `Err` with `ERROR_FAILED_SERVICE_CONTROLLER_CONNECT`
/// (Win32 1063) when we're not running under SCM at all — callers
/// take that as a signal to fall back to console mode.
pub fn try_run_as_service() -> windows_service::Result<()> {
    service_dispatcher::start(SERVICE_NAME, ffi_service_main)
}

/// Best-effort classifier for the "we're not under SCM" case so
/// main.rs can fall back to console mode without panicking on
/// other errors (which usually indicate a real misconfig).
pub fn is_not_under_scm(err: &windows_service::Error) -> bool {
    match err {
        windows_service::Error::Winapi(io) => io.raw_os_error() == Some(1063),
        _ => false,
    }
}

define_windows_service!(ffi_service_main, service_main);

fn service_main(_args: Vec<OsString>) {
    if let Err(e) = run_service() {
        // No stdout/stderr reaches the operator under SCM; the
        // best we can do is record into the existing tracing
        // subscriber if it's already up.
        tracing::error!(error = %e, "service_main exited with error");
    }
}

fn run_service() -> windows_service::Result<()> {
    let shutdown = Arc::new(AtomicBool::new(false));

    let handler_shutdown = shutdown.clone();
    let event_handler = move |control_event| -> ServiceControlHandlerResult {
        match control_event {
            ServiceControl::Stop | ServiceControl::Shutdown => {
                handler_shutdown.store(true, Ordering::SeqCst);
                ServiceControlHandlerResult::NoError
            }
            ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
            _ => ServiceControlHandlerResult::NotImplemented,
        }
    };

    let status_handle = service_control_handler::register(SERVICE_NAME, event_handler)?;

    status_handle.set_service_status(ServiceStatus {
        service_type: SERVICE_TYPE,
        current_state: ServiceState::Running,
        controls_accepted: ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN,
        exit_code: ServiceExitCode::Win32(0),
        checkpoint: 0,
        wait_hint: Duration::default(),
        process_id: None,
    })?;

    // Build a multi-thread tokio runtime and drive run_agent inside
    // a select! against the shutdown flag. The flag is polled rather
    // than channelled because the control handler runs on the SCM
    // dispatcher thread, where reaching into tokio's async primitives
    // is awkward; AtomicBool + a short polling loop is portable +
    // small + race-free.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| {
            tracing::error!(error = %e, "build tokio runtime");
            windows_service::Error::Winapi(e)
        })?;

    runtime.block_on(async {
        tokio::select! {
            res = crate::run_agent() => {
                if let Err(e) = res {
                    tracing::error!(error = %e, "run_agent exited with error");
                }
            }
            _ = poll_shutdown(shutdown) => {
                tracing::info!("SCM stop received; agent shutting down");
            }
        }
    });

    status_handle.set_service_status(ServiceStatus {
        service_type: SERVICE_TYPE,
        current_state: ServiceState::Stopped,
        controls_accepted: ServiceControlAccept::empty(),
        exit_code: ServiceExitCode::Win32(0),
        checkpoint: 0,
        wait_hint: Duration::default(),
        process_id: None,
    })?;

    Ok(())
}

async fn poll_shutdown(flag: Arc<AtomicBool>) {
    while !flag.load(Ordering::SeqCst) {
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}
