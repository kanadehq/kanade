pub mod boot_sentinel;
pub mod bootstrap;
pub mod check_eval;
pub mod config;
pub mod default_paths;
pub mod exe_version;
pub mod feature;
pub mod ipc;
pub mod kv;
pub mod kv_cas;
pub mod manifest;
pub mod nats_client;
pub mod secrets;
pub mod signing;
pub mod strict;
pub mod subject;
pub mod wire;

pub use wire::{
    Command, DiskInfo, ExecResult, Heartbeat, HostPerf, HwInventory, ProcessPerf, ProcessSnapshot,
    Shell,
};

/// Built-in fallback product name the end-user Client App shows when no
/// operator has configured `agent_config.client_display_name`.
///
/// Single source of truth for the Rust side: the client (`kanade-client`
/// `app.rs` window title / header) and the agent (`kanade-agent`
/// `client_shortcut` Start-Menu label) both alias this so the two can't
/// drift. The WebView (`web/src/main.ts`) and `index.html` carry the same
/// literal independently — they can't reference a Rust const — so an edit
/// here must be mirrored there too (their comments point back to this).
/// The client UI is Japanese-only (no i18n), so the default is Japanese;
/// English-speaking fleets override it via the backend config.
pub const DEFAULT_CLIENT_DISPLAY_NAME: &str = "端末管理支援ツール";
