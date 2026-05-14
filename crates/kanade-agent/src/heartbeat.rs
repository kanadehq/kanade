use std::time::Duration;

use kanade_shared::{Heartbeat, subject};
use tracing::warn;

const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);

pub async fn heartbeat_loop(client: async_nats::Client, pc_id: String, agent_version: String) {
    let mut interval = tokio::time::interval(HEARTBEAT_INTERVAL);
    loop {
        interval.tick().await;
        let hb = Heartbeat {
            pc_id: pc_id.clone(),
            at: chrono::Utc::now(),
            agent_version: agent_version.clone(),
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
}
