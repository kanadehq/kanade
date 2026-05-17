use kanade_shared::subject;
use kanade_shared::wire::{EffectiveConfig, Heartbeat};
use tokio::sync::watch;
use tracing::{info, warn};

/// `COMPUTERNAME` on Windows, `HOSTNAME` on Unix-likes. Best-effort —
/// the heartbeat baseline is happier with `Some("MINIPC")` than with
/// a panic when the env var is missing, so we shrug it off as
/// `None` and the backend backfills via inventory later.
fn hostname() -> Option<String> {
    std::env::var("COMPUTERNAME")
        .ok()
        .or_else(|| std::env::var("HOSTNAME").ok())
}

/// Heartbeat publisher loop. Cadence is taken from
/// [`EffectiveConfig::heartbeat_duration`] and updates live the
/// moment the config_supervisor pushes a new value on the watch
/// channel.
pub async fn heartbeat_loop(
    client: async_nats::Client,
    pc_id: String,
    agent_version: String,
    mut cfg_rx: watch::Receiver<EffectiveConfig>,
) {
    let mut current_dur = cfg_rx.borrow().heartbeat_duration();
    let mut ticker = tokio::time::interval(current_dur);
    // Read once at startup — neither hostname nor OS family changes
    // over the lifetime of an agent process.
    let hostname = hostname();
    let os_family = Some(std::env::consts::OS.to_string());
    info!(
        ?current_dur,
        ?hostname,
        ?os_family,
        "heartbeat loop scheduled",
    );

    loop {
        tokio::select! {
            _ = ticker.tick() => {
                let hb = Heartbeat {
                    pc_id: pc_id.clone(),
                    at: chrono::Utc::now(),
                    agent_version: agent_version.clone(),
                    hostname: hostname.clone(),
                    os_family: os_family.clone(),
                };
                let payload = match serde_json::to_vec(&hb) {
                    Ok(b) => b,
                    Err(e) => {
                        warn!(error = %e, "serialize heartbeat");
                        continue;
                    }
                };
                if let Err(e) = client
                    .publish(subject::heartbeat(&pc_id), payload.into())
                    .await
                {
                    warn!(error = %e, "publish heartbeat");
                }
            }
            res = cfg_rx.changed() => {
                if res.is_err() {
                    // Supervisor dropped: just keep ticking at the
                    // last cadence we knew.
                    continue;
                }
                let new_dur = cfg_rx.borrow().heartbeat_duration();
                if new_dur != current_dur {
                    info!(
                        old = ?current_dur,
                        new = ?new_dur,
                        "heartbeat interval changed; rescheduling ticker",
                    );
                    current_dur = new_dur;
                    ticker = tokio::time::interval(current_dur);
                }
            }
        }
    }
}
