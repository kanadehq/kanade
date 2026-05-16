//! Windows Service Control Manager (SCM) integration for kanade-
//! backend. Mirror of crates/kanade-agent/src/service.rs — same
//! reasoning, same shape, different service name. Without this,
//! sc.exe-registered backend processes hit Event ID 7009 /
//! 'service did not respond' because they never call
//! StartServiceCtrlDispatcher.

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

const SERVICE_NAME: &str = "KanadeBackend";
const SERVICE_TYPE: ServiceType = ServiceType::OWN_PROCESS;

pub fn try_run_as_service() -> windows_service::Result<()> {
    service_dispatcher::start(SERVICE_NAME, ffi_service_main)
}

pub fn is_not_under_scm(err: &windows_service::Error) -> bool {
    match err {
        windows_service::Error::Winapi(io) => io.raw_os_error() == Some(1063),
        _ => false,
    }
}

define_windows_service!(ffi_service_main, service_main);

fn service_main(_args: Vec<OsString>) {
    if let Err(e) = run_service() {
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

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| {
            tracing::error!(error = %e, "build tokio runtime");
            windows_service::Error::Winapi(e)
        })?;

    runtime.block_on(async {
        tokio::select! {
            res = crate::run_backend() => {
                if let Err(e) = res {
                    tracing::error!(error = %e, "run_backend exited with error");
                }
            }
            _ = poll_shutdown(shutdown) => {
                tracing::info!("SCM stop received; backend shutting down");
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
