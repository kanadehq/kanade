//! `GET /api/agents/{pc_id}/location` & `POST /api/agents/{pc_id}/location/request`
//! — lost-device location visibility and on-demand acquisition.
//!
//! # Architecture and Privacy Guards
//!
//! 1. On-demand only: coordinates are never collected passively or added to heartbeats.
//! 2. Bounded retention: location data has a configurable retention window (default 7 days,
//!    max 30 days). Expired coordinates are withheld from API responses and purged by cleanup.
//! 3. Explicit enablement: feature is disabled by default in server settings and requires
//!    admin opt-in. Disabling the feature purges stored location coordinates.
//! 4. Audit privacy: views are deduplicated per caller/endpoint window; requests and
//!    settings changes are audited individually. Coordinates
//!    are strictly omitted from audit logs to prevent leaking sensitive location history.
//! 5. Separation of state: last known successful location is preserved across subsequent failed
//!    requests, while latest request status and reason are tracked separately.
//! 6. Offline endpoints: offline or unresponsive endpoints never appear freshly located; an
//!    offline state is recorded and surfaced explicitly.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use chrono::{DateTime, Utc};
use kanade_shared::kv::{BUCKET_SERVER_SETTINGS, KEY_SERVER_SETTINGS};
use kanade_shared::kv_cas;
use kanade_shared::subject;
use kanade_shared::wire::{
    LocationReading, LocationReply, LocationRequest, LocationSettings, LocationSource,
    LocationStatus, MAX_LOCATION_RETENTION_DAYS, ServerSettings,
};
use serde::{Deserialize, Serialize};
use sqlx::{Row, SqlitePool};
use std::collections::HashMap;
use std::time::{Duration, Instant};
use tracing::{info, warn};
use uuid::Uuid;

use crate::api::AppState;
use crate::audit::{self, Caller};
use crate::auth::Claims;

/// Threshold after which an endpoint's missing heartbeat is considered offline.
const OFFLINE_HEARTBEAT_THRESHOLD_SECS: i64 = 90;

/// Request timeout for on-demand location acquisition via NATS.
const LOCATION_REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

/// A success is shown as the device's current position only while it is this
/// young. Past it the reading is "last known": the endpoint may have been
/// powered off or moved since, and nothing here can tell.
const CURRENT_READING_MAX_AGE_SECS: i64 = 120;

/// Tolerated clock skew between endpoint and backend when judging a reading's
/// acquisition timestamp.
const CLOCK_SKEW_SECS: i64 = 30;

/// Polls and multiple tabs share one audit entry per caller/endpoint per window.
const VIEW_AUDIT_WINDOW: Duration = Duration::from_secs(300);
const MAX_VIEW_AUDIT_WINDOWS: usize = 4096;

#[derive(Default)]
struct ViewAuditWindows(HashMap<(Option<String>, String), Instant>);

impl ViewAuditWindows {
    fn should_record(&mut self, caller: &Caller, pc_id: &str, now: Instant) -> bool {
        self.0
            .retain(|_, start| now.duration_since(*start) < VIEW_AUDIT_WINDOW);
        let key = (caller.sub.clone(), pc_id.to_owned());
        if self.0.contains_key(&key) {
            return false;
        }
        // Bound memory without suppressing the first access from a new caller.
        if self.0.len() == MAX_VIEW_AUDIT_WINDOWS
            && let Some(oldest) = self
                .0
                .iter()
                .min_by_key(|(_, start)| **start)
                .map(|(key, _)| key.clone())
        {
            self.0.remove(&oldest);
        }
        self.0.insert(key, now);
        true
    }
}

static VIEW_AUDIT_WINDOWS: std::sync::LazyLock<std::sync::Mutex<ViewAuditWindows>> =
    std::sync::LazyLock::new(Default::default);

/// Endpoints with a location request currently in flight (one at a time each).
static IN_FLIGHT: std::sync::LazyLock<std::sync::Mutex<std::collections::HashMap<String, bool>>> =
    std::sync::LazyLock::new(Default::default);

// Settings updates, endpoint deletion and reply persistence share this lock.
// The epoch invalidates replies across disable/re-enable cycles while NATS is
// in flight; IN_FLIGHT's validity flag invalidates replies for deleted endpoints.
static SETTINGS_EPOCH: tokio::sync::RwLock<u64> = tokio::sync::RwLock::const_new(0);

async fn reply_epoch_guard(epoch: u64) -> Option<tokio::sync::RwLockReadGuard<'static, u64>> {
    let guard = SETTINGS_EPOCH.read().await;
    (*guard == epoch).then_some(guard)
}

fn heartbeat_online(heartbeat: Option<&str>, now: DateTime<Utc>) -> bool {
    heartbeat
        .and_then(|hb| DateTime::parse_from_rfc3339(hb).ok())
        .is_some_and(|hb| {
            let age = now.signed_duration_since(hb.with_timezone(&Utc));
            age >= -chrono::Duration::seconds(CLOCK_SKEW_SECS)
                && age <= chrono::Duration::seconds(OFFLINE_HEARTBEAT_THRESHOLD_SECS)
        })
}

struct InFlightGuard(String);

impl InFlightGuard {
    fn acquire(pc_id: &str) -> Option<Self> {
        let mut requests = IN_FLIGHT.lock().unwrap_or_else(|e| e.into_inner());
        match requests.entry(pc_id.to_string()) {
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(true);
                Some(Self(pc_id.to_string()))
            }
            std::collections::hash_map::Entry::Occupied(_) => None,
        }
    }

    fn is_valid(&self) -> bool {
        IN_FLIGHT
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&self.0)
            .copied()
            .unwrap_or(false)
    }
}

/// Serialize endpoint deletion with location writes and invalidate its pending
/// request, including when the same pc_id is registered again before the reply.
/// Keep this guard until both location and registry rows have been deleted.
pub(super) async fn endpoint_deletion_guard(
    pc_id: &str,
) -> tokio::sync::RwLockWriteGuard<'static, u64> {
    let guard = SETTINGS_EPOCH.write().await;
    if let Some(valid) = IN_FLIGHT
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get_mut(pc_id)
    {
        *valid = false;
    }
    guard
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        IN_FLIGHT
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.0);
    }
}

/// Reject readings that cannot be believed: out-of-range or non-finite
/// coordinates, or an acquisition time that is not between the operator's
/// request and now (a stale OS cache or a wrong endpoint clock).
fn validate_reading(
    r: &LocationReading,
    requested_at: &str,
    now: DateTime<Utc>,
) -> Result<(), String> {
    if !(r.latitude.is_finite() && (-90.0..=90.0).contains(&r.latitude))
        || !(r.longitude.is_finite() && (-180.0..=180.0).contains(&r.longitude))
        || !(r.accuracy_meters.is_finite() && r.accuracy_meters >= 0.0)
    {
        return Err("Endpoint returned out-of-range coordinates".into());
    }
    let acquired = DateTime::parse_from_rfc3339(&r.acquired_at)
        .map_err(|_| "Endpoint returned an unparseable acquisition time".to_string())?
        .with_timezone(&Utc);
    let requested = DateTime::parse_from_rfc3339(requested_at)
        .map_err(|_| "unparseable request time".to_string())?
        .with_timezone(&Utc);
    let skew = chrono::Duration::seconds(CLOCK_SKEW_SECS);
    if acquired < requested - skew {
        return Err("Reading predates the request (stale OS cache or endpoint clock)".into());
    }
    if acquired > now + skew {
        return Err("Reading is timestamped in the future (endpoint clock skew)".into());
    }
    Ok(())
}

/// Whether the stored success is a fresh answer to the latest request rather
/// than merely the last known position.
fn is_current(
    latest_status: Option<LocationStatus>,
    acquired_at: &str,
    now: DateTime<Utc>,
    online: bool,
) -> bool {
    online
        && latest_status == Some(LocationStatus::Success)
        && DateTime::parse_from_rfc3339(acquired_at)
            .map(|dt| {
                let age = now.signed_duration_since(dt.with_timezone(&Utc));
                age >= -chrono::Duration::seconds(CLOCK_SKEW_SECS)
                    && age <= chrono::Duration::seconds(CURRENT_READING_MAX_AGE_SECS)
            })
            .unwrap_or(false)
}

/// Minimum interval between subsequent location requests for the same PC.
const MIN_REQUEST_INTERVAL_SECS: i64 = 5;

/// Response payload for `GET /api/agents/{pc_id}/location` and request completion.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct DeviceLocationResponse {
    pub pc_id: String,
    pub enabled: bool,
    pub retention_days: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_known: Option<LastKnownLocation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latest_request: Option<LatestRequestStatus>,
}

/// Last known successful coordinate reading.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct LastKnownLocation {
    pub latitude: f64,
    pub longitude: f64,
    pub accuracy_meters: f64,
    pub source: LocationSource,
    pub acquired_at: String,
    /// `true` only for a successful answer to the latest request that is still
    /// young; otherwise this is the *last known* position, not where the
    /// device is now.
    pub current: bool,
    /// Direct link to view coordinates in OpenStreetMap (opened only on explicit click).
    pub map_url: String,
}

/// Status of the most recent on-demand request.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct LatestRequestStatus {
    pub request_id: String,
    pub requested_at: String,
    pub status: LocationStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Update payload for `PUT /api/server-settings/location`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct LocationSettingsUpdate {
    pub enabled: bool,
    pub retention_days: u32,
}

/// Build an OpenStreetMap URL for the given coordinates.
pub fn format_osm_url(lat: f64, lon: f64) -> String {
    format!("https://www.openstreetmap.org/?mlat={lat:.6}&mlon={lon:.6}#map=16/{lat:.6}/{lon:.6}")
}

/// Open the server settings KV bucket.
async fn open_server_settings_bucket(
    s: &AppState,
) -> Result<async_nats::jetstream::kv::Store, (StatusCode, String)> {
    s.jetstream
        .get_key_value(BUCKET_SERVER_SETTINGS)
        .await
        .map_err(|e| {
            warn!(error = %e, "open server_settings bucket");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("open server_settings bucket: {e}"),
            )
        })
}

/// Load current location settings from KV.
async fn load_location_settings(s: &AppState) -> LocationSettings {
    let Ok(kv) = open_server_settings_bucket(s).await else {
        return LocationSettings::default();
    };
    match kv.get(KEY_SERVER_SETTINGS).await {
        Ok(Some(bytes)) => serde_json::from_slice::<ServerSettings>(&bytes)
            .map(|ss| ss.effective_location())
            .unwrap_or_default(),
        _ => LocationSettings::default(),
    }
}

/// `GET /api/agents/{pc_id}/location`
///
/// View the managed endpoint's last known location and latest request outcome.
/// Requires Operator role + Feature::Location. Coordinates are strictly omitted
/// from audit logging.
pub async fn get_location(
    State(s): State<AppState>,
    Path(pc_id): Path<String>,
    caller: Caller,
) -> Result<Json<DeviceLocationResponse>, (StatusCode, String)> {
    let response = load_location_response(&s, &pc_id).await?;
    let should_record = VIEW_AUDIT_WINDOWS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .should_record(&caller, &pc_id, Instant::now());
    if should_record {
        audit::record(
            &s.nats,
            "operator",
            "agent.location.view",
            Some(&pc_id),
            Some(&caller),
            serde_json::json!({
                "pc_id": &pc_id,
                "has_last_known": response.last_known.is_some(),
                "dedup_window_seconds": VIEW_AUDIT_WINDOW.as_secs(),
            }),
        )
        .await;
    }
    Ok(Json(response))
}

/// Build a response without auditing a view. Collection requests already emit
/// their own audit event and must not create an additional view event.
async fn load_location_response(
    s: &AppState,
    pc_id: &str,
) -> Result<DeviceLocationResponse, (StatusCode, String)> {
    let settings_guard = SETTINGS_EPOCH.read().await;
    let loc_settings = load_location_settings(s).await;
    let retention_days = loc_settings.effective_retention_days();

    // Query device_locations table
    let row = sqlx::query(
        r#"
        SELECT
            last_latitude,
            last_longitude,
            last_accuracy,
            last_source,
            last_acquired_at,
            last_request_id,
            last_request_at,
            last_request_status,
            last_request_reason,
            (SELECT last_heartbeat FROM agents WHERE agents.pc_id = device_locations.pc_id) AS heartbeat
        FROM device_locations
        WHERE pc_id = ? AND EXISTS (
            SELECT 1 FROM agents WHERE agents.pc_id = device_locations.pc_id
        )
        "#,
    )
    .bind(pc_id)
    .fetch_optional(&s.pool)
    .await
    .map_err(|e| {
        warn!(error = %e, pc_id = %pc_id, "query device_locations");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("query device_locations: {e}"),
        )
    })?;

    let mut last_known = None;
    let mut latest_request = None;

    if let Some(r) = row {
        let lat: Option<f64> = r.get("last_latitude");
        let lon: Option<f64> = r.get("last_longitude");
        let acc: Option<f64> = r.get("last_accuracy");
        let src: Option<String> = r.get("last_source");
        let acq: Option<String> = r.get("last_acquired_at");

        if let (Some(latitude), Some(longitude), Some(accuracy_meters), Some(acquired_at)) =
            (lat, lon, acc, acq)
        {
            // Verify bounded retention: if acquired_at is older than retention_days, withhold
            let is_expired = DateTime::parse_from_rfc3339(&acquired_at)
                .map(|dt| {
                    let age = Utc::now().signed_duration_since(dt.with_timezone(&Utc));
                    age > chrono::Duration::days(retention_days as i64)
                })
                .unwrap_or(true);

            let latest_status = r
                .get::<Option<String>, _>("last_request_status")
                .as_deref()
                .and_then(LocationStatus::parse);
            if !is_expired && loc_settings.enabled {
                let now = Utc::now();
                let heartbeat: Option<String> = r.get("heartbeat");
                let current = is_current(
                    latest_status,
                    &acquired_at,
                    now,
                    heartbeat_online(heartbeat.as_deref(), now),
                );
                let source = src
                    .as_deref()
                    .and_then(LocationSource::parse)
                    .unwrap_or(LocationSource::Unknown);
                let map_url = format_osm_url(latitude, longitude);
                last_known = Some(LastKnownLocation {
                    latitude,
                    longitude,
                    accuracy_meters,
                    source,
                    acquired_at,
                    current,
                    map_url,
                });
            }
        }

        let req_id: Option<String> = r.get("last_request_id");
        let req_at: Option<String> = r.get("last_request_at");
        let req_st: Option<String> = r.get("last_request_status");
        let req_rs: Option<String> = r.get("last_request_reason");

        if let (Some(request_id), Some(requested_at), Some(status_str)) = (req_id, req_at, req_st) {
            let status = LocationStatus::parse(&status_str).unwrap_or(LocationStatus::Unavailable);
            latest_request = Some(LatestRequestStatus {
                request_id,
                requested_at,
                status,
                reason: req_rs,
            });
        }
    }

    drop(settings_guard);

    Ok(DeviceLocationResponse {
        pc_id: pc_id.to_owned(),
        enabled: loc_settings.enabled,
        retention_days,
        last_known,
        latest_request,
    })
}

/// `POST /api/agents/{pc_id}/location/request`
///
/// Initiate an on-demand location acquisition request to a managed endpoint.
/// Requires Operator role + Feature::Location. Offline endpoints are detected
/// and surfaced explicitly without pretending to locate them.
pub async fn request_location(
    State(s): State<AppState>,
    Path(pc_id): Path<String>,
    caller: Caller,
) -> Result<Json<DeviceLocationResponse>, (StatusCode, String)> {
    let epoch_guard = SETTINGS_EPOCH.read().await;
    let request_epoch = *epoch_guard;
    let loc_settings = load_location_settings(&s).await;
    if !loc_settings.enabled {
        return Err((
            StatusCode::BAD_REQUEST,
            "Lost-device location visibility is disabled in server settings".into(),
        ));
    }

    // Keep the shared lock through endpoint lookup and in-flight registration
    // (and offline persistence), so deletion cannot miss a newly started request.
    // Check endpoint existence and heartbeat.
    let agent_row = sqlx::query("SELECT last_heartbeat FROM agents WHERE pc_id = ?")
        .bind(&pc_id)
        .fetch_optional(&s.pool)
        .await
        .map_err(|e| {
            warn!(error = %e, pc_id = %pc_id, "query agent heartbeat");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("query agent: {e}"),
            )
        })?;

    let Some(agent) = agent_row else {
        return Err((
            StatusCode::NOT_FOUND,
            format!("endpoint `{pc_id}` does not exist"),
        ));
    };

    let Some(in_flight) = InFlightGuard::acquire(&pc_id) else {
        return Err((
            StatusCode::CONFLICT,
            format!("A location request for `{pc_id}` is already in progress"),
        ));
    };

    // Rate-limiting check: avoid flooding requests (offline ones included,
    // which would otherwise each rewrite the row and emit an audit event)
    let last_req_row = sqlx::query("SELECT last_request_at FROM device_locations WHERE pc_id = ?")
        .bind(&pc_id)
        .fetch_optional(&s.pool)
        .await
        .unwrap_or(None);

    if let Some(r) = last_req_row {
        let prev_req_at: Option<String> = r.get("last_request_at");
        if let Some(prev) = prev_req_at
            && let Ok(prev_dt) = DateTime::parse_from_rfc3339(&prev)
        {
            let diff = Utc::now().signed_duration_since(prev_dt.with_timezone(&Utc));
            if diff.num_seconds() < MIN_REQUEST_INTERVAL_SECS {
                return Err((
                    StatusCode::TOO_MANY_REQUESTS,
                    format!(
                        "A location request for `{pc_id}` was already initiated recently. Please wait {MIN_REQUEST_INTERVAL_SECS}s."
                    ),
                ));
            }
        }
    }

    let last_hb_str: Option<String> = agent.get("last_heartbeat");
    let is_offline = !heartbeat_online(last_hb_str.as_deref(), Utc::now());

    let request_id = Uuid::new_v4().to_string();
    let requested_at = Utc::now().to_rfc3339();
    let now = Utc::now().to_rfc3339();

    if is_offline {
        // Record offline state directly without sending request to broker
        record_failed_request(
            &s.pool,
            &pc_id,
            &request_id,
            &requested_at,
            LocationStatus::Offline,
            Some("Endpoint is offline or heartbeat is overdue"),
            &now,
        )
        .await?;

        drop(epoch_guard);
        audit::record(
            &s.nats,
            "operator",
            "agent.location.request",
            Some(&pc_id),
            Some(&caller),
            serde_json::json!({
                "pc_id": &pc_id,
                "request_id": &request_id,
                "status": LocationStatus::Offline.as_str(),
            }),
        )
        .await;

        return load_location_response(&s, &pc_id).await.map(Json);
    }

    drop(epoch_guard);

    // Prepare NATS on-demand request
    let expires_at = (Utc::now() + chrono::Duration::seconds(15)).to_rfc3339();
    let req_payload = LocationRequest {
        request_id: request_id.clone(),
        requested_at: requested_at.clone(),
        expires_at,
    };

    let body = serde_json::to_vec(&req_payload).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("serialize location request: {e}"),
        )
    })?;

    let req_subject = subject::location_req(&pc_id);

    info!(pc_id = %pc_id, request_id = %request_id, "sending on-demand location request");

    let reply_result = tokio::time::timeout(
        LOCATION_REQUEST_TIMEOUT,
        s.nats.send_request(
            req_subject,
            async_nats::Request::new()
                .payload(body.into())
                .timeout(Some(LOCATION_REQUEST_TIMEOUT))
                // Shared agent inbox grants must not expose sensitive replies.
                .inbox(format!("location.reply.{}", Uuid::new_v4())),
        ),
    )
    .await;

    let (status, reason, reading) = match reply_result {
        Err(_) => (
            LocationStatus::Timeout,
            Some("Endpoint timed out waiting for location reading".to_string()),
            None,
        ),
        Ok(Err(e)) => (
            LocationStatus::Unavailable,
            Some(format!("NATS request error: {e}")),
            None,
        ),
        Ok(Ok(msg)) => match serde_json::from_slice::<LocationReply>(&msg.payload) {
            Ok(reply) => {
                if reply.request_id != request_id {
                    (
                        LocationStatus::Unavailable,
                        Some("Mismatched request ID in location reply".to_string()),
                        None,
                    )
                } else {
                    (reply.status, reply.reason, reply.reading)
                }
            }
            Err(e) => (
                LocationStatus::Unavailable,
                Some(format!("Invalid location reply payload: {e}")),
                None,
            ),
        },
    };

    // A success without a believable reading is not a success.
    let (status, reason, reading) = match (status, reading) {
        (LocationStatus::Success, Some(r)) => match validate_reading(&r, &requested_at, Utc::now())
        {
            Ok(()) => (LocationStatus::Success, reason, Some(r)),
            Err(why) => (LocationStatus::Unavailable, Some(why), None),
        },
        (LocationStatus::Success, None) => (
            LocationStatus::Unavailable,
            Some("Endpoint reported success without a reading".to_string()),
            None,
        ),
        (st, _) => (st, reason, None),
    };

    // Serialize with disabling/purging. A request from an older settings
    // epoch must never restore data, even if location has since been enabled.
    let epoch_guard = reply_epoch_guard(request_epoch).await;
    if epoch_guard.is_none() || !in_flight.is_valid() || !load_location_settings(&s).await.enabled {
        drop(epoch_guard);
        audit::record(
            &s.nats,
            "operator",
            "agent.location.request",
            Some(&pc_id),
            Some(&caller),
            serde_json::json!({"pc_id": &pc_id, "request_id": &request_id,
                "status": "expired", "reason": "Location settings or endpoint changed during acquisition"}),
        )
        .await;
        return Err((
            StatusCode::CONFLICT,
            "Location settings or endpoint changed during acquisition; reading discarded".into(),
        ));
    }

    // Update database
    if let (LocationStatus::Success, Some(r)) = (status, reading.as_ref()) {
        record_successful_reading(&s.pool, &pc_id, r, &request_id, &requested_at, &now).await?;
    } else {
        record_failed_request(
            &s.pool,
            &pc_id,
            &request_id,
            &requested_at,
            status,
            reason.as_deref(),
            &now,
        )
        .await?;
    }

    drop(epoch_guard);

    // Audit log: record request outcome, but NEVER include coordinates
    audit::record(
        &s.nats,
        "operator",
        "agent.location.request",
        Some(&pc_id),
        Some(&caller),
        serde_json::json!({
            "pc_id": &pc_id,
            "request_id": &request_id,
            "status": status.as_str(),
            "reason": reason,
        }),
    )
    .await;

    load_location_response(&s, &pc_id).await.map(Json)
}

/// Helper to persist a successful location reading.
async fn record_successful_reading(
    pool: &SqlitePool,
    pc_id: &str,
    reading: &LocationReading,
    request_id: &str,
    requested_at: &str,
    now: &str,
) -> Result<(), (StatusCode, String)> {
    sqlx::query(
        r#"
        INSERT INTO device_locations (
            pc_id,
            last_latitude,
            last_longitude,
            last_accuracy,
            last_source,
            last_acquired_at,
            last_request_id,
            last_request_at,
            last_request_status,
            last_request_reason,
            updated_at
        ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, 'success', NULL, ?)
        ON CONFLICT(pc_id) DO UPDATE SET
            last_latitude = excluded.last_latitude,
            last_longitude = excluded.last_longitude,
            last_accuracy = excluded.last_accuracy,
            last_source = excluded.last_source,
            last_acquired_at = excluded.last_acquired_at,
            last_request_id = excluded.last_request_id,
            last_request_at = excluded.last_request_at,
            last_request_status = excluded.last_request_status,
            last_request_reason = excluded.last_request_reason,
            updated_at = excluded.updated_at
        "#,
    )
    .bind(pc_id)
    .bind(reading.latitude)
    .bind(reading.longitude)
    .bind(reading.accuracy_meters)
    .bind(reading.source.as_str())
    .bind(&reading.acquired_at)
    .bind(request_id)
    .bind(requested_at)
    .bind(now)
    .execute(pool)
    .await
    .map_err(|e| {
        warn!(error = %e, pc_id = %pc_id, "record successful reading");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("record location: {e}"),
        )
    })?;

    Ok(())
}

/// Helper to record a failed / non-success request outcome while preserving existing coordinates.
async fn record_failed_request(
    pool: &SqlitePool,
    pc_id: &str,
    request_id: &str,
    requested_at: &str,
    status: LocationStatus,
    reason: Option<&str>,
    now: &str,
) -> Result<(), (StatusCode, String)> {
    sqlx::query(
        r#"
        INSERT INTO device_locations (
            pc_id,
            last_latitude,
            last_longitude,
            last_accuracy,
            last_source,
            last_acquired_at,
            last_request_id,
            last_request_at,
            last_request_status,
            last_request_reason,
            updated_at
        ) VALUES (?, NULL, NULL, NULL, NULL, NULL, ?, ?, ?, ?, ?)
        ON CONFLICT(pc_id) DO UPDATE SET
            last_request_id = excluded.last_request_id,
            last_request_at = excluded.last_request_at,
            last_request_status = excluded.last_request_status,
            last_request_reason = excluded.last_request_reason,
            updated_at = excluded.updated_at
        "#,
    )
    .bind(pc_id)
    .bind(request_id)
    .bind(requested_at)
    .bind(status.as_str())
    .bind(reason)
    .bind(now)
    .execute(pool)
    .await
    .map_err(|e| {
        warn!(error = %e, pc_id = %pc_id, "record failed request");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("record location status: {e}"),
        )
    })?;

    Ok(())
}

/// `GET /api/server-settings/location`
///
/// View server-wide location settings.
pub async fn get_settings(
    State(s): State<AppState>,
) -> Result<Json<LocationSettings>, (StatusCode, String)> {
    let settings = load_location_settings(&s).await;
    Ok(Json(settings))
}

pub fn validate_retention_days(days: u32) -> Result<u32, (StatusCode, String)> {
    if days == 0 || days > MAX_LOCATION_RETENTION_DAYS {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("retention_days must be between 1 and {MAX_LOCATION_RETENTION_DAYS} days"),
        ));
    }
    Ok(days)
}

/// `PUT /api/server-settings/location`
///
/// Update server-wide location settings. Admin-only.
/// Disabling the feature purges stored location coordinates across all endpoints.
pub async fn put_settings(
    State(s): State<AppState>,
    axum::Extension(claims): axum::Extension<Claims>,
    caller: Caller,
    Json(update): Json<LocationSettingsUpdate>,
) -> Result<Json<LocationSettings>, (StatusCode, String)> {
    if claims.role() != crate::auth::Role::Admin {
        return Err((
            StatusCode::FORBIDDEN,
            "Only administrators may update location settings".into(),
        ));
    }

    let retention_days = validate_retention_days(update.retention_days)?;

    let new_location = LocationSettings {
        enabled: update.enabled,
        retention_days,
    };

    // Held across the KV write on purpose: replies persist only under a read
    // guard of the same epoch, so releasing it between purge and the setting
    // write would let a reading land after the purge.
    let mut epoch_guard = SETTINGS_EPOCH.write().await;
    let kv = open_server_settings_bucket(&s).await?;
    let previous = load_location_settings(&s).await;

    // Purge before the setting is saved, so a failed purge leaves the old
    // setting in place instead of a disabled setting with coordinates behind
    // it. Leaving the disabled state purges as well: nothing kept while
    // disabled (e.g. by an older build) may reappear on re-enable.
    if !new_location.enabled || !previous.enabled {
        purge_coordinates(&s.pool).await.map_err(|e| {
            warn!(error = %e, "purge location coordinates");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("purge location: {e}"),
            )
        })?;
        // In-flight requests from before the purge must not restore data,
        // even if the setting write below fails.
        *epoch_guard = epoch_guard.wrapping_add(1);
    }

    let new_loc_clone = new_location.clone();
    let _ = kv_cas::read_modify_write::<ServerSettings, _>(&kv, KEY_SERVER_SETTINGS, |doc| {
        doc.location = Some(new_loc_clone.clone());
        true
    })
    .await
    .map_err(|e| {
        warn!(error = %e, "update server_settings.location");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("update server_settings: {e}"),
        )
    })?;

    *epoch_guard = epoch_guard.wrapping_add(1);
    drop(epoch_guard);

    audit::record(
        &s.nats,
        "operator",
        "server_settings.location.update",
        Some("server_settings"),
        Some(&caller),
        serde_json::json!({
            "enabled": new_location.enabled,
            "retention_days": new_location.retention_days,
        }),
    )
    .await;

    Ok(Json(new_location))
}

/// Clear every stored coordinate, keeping request status rows.
async fn purge_coordinates(pool: &SqlitePool) -> Result<u64, sqlx::Error> {
    let res = sqlx::query(
        r#"
        UPDATE device_locations
        SET last_latitude = NULL,
            last_longitude = NULL,
            last_accuracy = NULL,
            last_source = NULL,
            last_acquired_at = NULL
        "#,
    )
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
}

/// Periodic cleanup sweep for location records whose retention window has passed.
pub async fn sweep_expired_locations(
    pool: &SqlitePool,
    retention_days: u32,
) -> Result<u64, sqlx::Error> {
    let cutoff = (Utc::now() - chrono::Duration::days(retention_days as i64)).to_rfc3339();
    let res = sqlx::query(
        r#"
        UPDATE device_locations
        SET last_latitude = NULL,
            last_longitude = NULL,
            last_accuracy = NULL,
            last_source = NULL,
            last_acquired_at = NULL
        WHERE last_acquired_at IS NOT NULL AND last_acquired_at < ?
        "#,
    )
    .bind(cutoff)
    .execute(pool)
    .await?;

    Ok(res.rows_affected())
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::Row;
    use sqlx::sqlite::SqlitePoolOptions;

    async fn test_pool() -> SqlitePool {
        let pool = SqlitePoolOptions::new()
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        pool
    }

    #[tokio::test]
    async fn disable_and_reenable_rejects_inflight_success() {
        let pool = test_pool().await;
        let now = Utc::now().to_rfc3339();
        let fix = reading(&now);
        record_successful_reading(&pool, "epoch-pc", &fix, "before", &now, &now)
            .await
            .unwrap();
        let request_epoch = *SETTINGS_EPOCH.read().await;
        {
            let mut settings = SETTINGS_EPOCH.write().await;
            *settings = settings.wrapping_add(1); // disable + purge
            sqlx::query("DELETE FROM device_locations")
                .execute(&pool)
                .await
                .unwrap();
            *settings = settings.wrapping_add(1); // re-enable
        }
        // Same guard used at the production persistence boundary.
        if let Some(_guard) = reply_epoch_guard(request_epoch).await {
            record_successful_reading(&pool, "epoch-pc", &fix, "late", &now, &now)
                .await
                .unwrap();
        }
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM device_locations")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 0);
        let current = *SETTINGS_EPOCH.read().await;
        assert!(reply_epoch_guard(current).await.is_some());
    }

    #[test]
    fn view_audit_coalesces_polls_and_tabs_without_sliding_the_window() {
        let mut windows = ViewAuditWindows::default();
        let caller = Caller {
            sub: Some("operator-a".into()),
            source: Some("spa".into()),
        };
        let start = Instant::now();
        assert!(windows.should_record(&caller, "endpoint-a", start));
        // Ten-second polling across tabs does not generate additional events.
        for seconds in (10..300).step_by(10) {
            assert!(!windows.should_record(
                &caller,
                "endpoint-a",
                start + Duration::from_secs(seconds)
            ));
        }
        // Polling must not keep extending the suppression window indefinitely.
        assert!(windows.should_record(&caller, "endpoint-a", start + VIEW_AUDIT_WINDOW));
        assert!(!windows.should_record(&caller, "endpoint-a", start + VIEW_AUDIT_WINDOW));
    }

    #[test]
    fn view_audit_keeps_callers_and_endpoints_separate() {
        let mut windows = ViewAuditWindows::default();
        let a = Caller {
            sub: Some("operator-a".into()),
            source: None,
        };
        let b = Caller {
            sub: Some("operator-b".into()),
            source: None,
        };
        let now = Instant::now();
        assert!(windows.should_record(&a, "endpoint-a", now));
        assert!(windows.should_record(&b, "endpoint-a", now));
        assert!(windows.should_record(&a, "endpoint-b", now));
        // Changing client headers cannot reset the authenticated caller's window.
        let another_tab = Caller {
            source: Some("spa".into()),
            ..a
        };
        assert!(!windows.should_record(&another_tab, "endpoint-a", now));
    }

    #[test]
    fn retention_days_validation() {
        assert!(validate_retention_days(1).is_ok());
        assert!(validate_retention_days(7).is_ok());
        assert!(validate_retention_days(30).is_ok());

        assert!(validate_retention_days(0).is_err());
        assert!(validate_retention_days(31).is_err());
        assert!(validate_retention_days(100).is_err());
    }

    #[tokio::test]
    async fn db_record_success_then_failed_request_preserves_coordinates() {
        let pool = test_pool().await;
        let pc_id = "test-pc-01";
        let now = Utc::now().to_rfc3339();

        let reading = LocationReading {
            latitude: 35.681236,
            longitude: 139.767125,
            accuracy_meters: 15.5,
            acquired_at: now.clone(),
            source: LocationSource::Wifi,
        };

        // Record successful reading
        record_successful_reading(&pool, pc_id, &reading, "req-1", &now, &now)
            .await
            .unwrap();

        let row = sqlx::query("SELECT * FROM device_locations WHERE pc_id = ?")
            .bind(pc_id)
            .fetch_one(&pool)
            .await
            .unwrap();

        assert_eq!(row.get::<String, _>("pc_id"), pc_id);
        assert_eq!(row.get::<Option<f64>, _>("last_latitude"), Some(35.681236));
        assert_eq!(
            row.get::<Option<f64>, _>("last_longitude"),
            Some(139.767125)
        );
        assert_eq!(row.get::<Option<f64>, _>("last_accuracy"), Some(15.5));
        assert_eq!(
            row.get::<Option<String>, _>("last_source"),
            Some("wifi".to_string())
        );
        assert_eq!(
            row.get::<Option<String>, _>("last_request_status"),
            Some("success".to_string())
        );

        // Subsequent request fails (e.g. endpoint is offline)
        let fail_now = (Utc::now() + chrono::Duration::seconds(5)).to_rfc3339();
        record_failed_request(
            &pool,
            pc_id,
            "req-2",
            &fail_now,
            LocationStatus::Offline,
            Some("Endpoint is offline"),
            &fail_now,
        )
        .await
        .unwrap();

        let row_after_fail = sqlx::query("SELECT * FROM device_locations WHERE pc_id = ?")
            .bind(pc_id)
            .fetch_one(&pool)
            .await
            .unwrap();

        // Coordinates must be PRESERVED
        assert_eq!(
            row_after_fail.get::<Option<f64>, _>("last_latitude"),
            Some(35.681236)
        );
        assert_eq!(
            row_after_fail.get::<Option<f64>, _>("last_longitude"),
            Some(139.767125)
        );
        assert_eq!(
            row_after_fail.get::<Option<f64>, _>("last_accuracy"),
            Some(15.5)
        );
        assert_eq!(
            row_after_fail.get::<Option<String>, _>("last_source"),
            Some("wifi".to_string())
        );
        // Request status updated
        assert_eq!(
            row_after_fail.get::<Option<String>, _>("last_request_id"),
            Some("req-2".to_string())
        );
        assert_eq!(
            row_after_fail.get::<Option<String>, _>("last_request_status"),
            Some("offline".to_string())
        );
        assert_eq!(
            row_after_fail.get::<Option<String>, _>("last_request_reason"),
            Some("Endpoint is offline".to_string())
        );
    }

    #[tokio::test]
    async fn db_sweep_expired_locations_nullifies_coordinates() {
        let pool = test_pool().await;
        let pc_fresh = "pc-fresh";
        let pc_stale = "pc-stale";

        let now = Utc::now().to_rfc3339();
        let old = (Utc::now() - chrono::Duration::days(10)).to_rfc3339();

        let fresh_reading = LocationReading {
            latitude: 35.0,
            longitude: 139.0,
            accuracy_meters: 10.0,
            acquired_at: now.clone(),
            source: LocationSource::Gps,
        };
        let stale_reading = LocationReading {
            latitude: 36.0,
            longitude: 140.0,
            accuracy_meters: 20.0,
            acquired_at: old.clone(),
            source: LocationSource::Wifi,
        };

        record_successful_reading(&pool, pc_fresh, &fresh_reading, "req-fresh", &now, &now)
            .await
            .unwrap();
        record_successful_reading(&pool, pc_stale, &stale_reading, "req-stale", &old, &old)
            .await
            .unwrap();

        // Sweep with 7 days retention
        let swept = sweep_expired_locations(&pool, 7).await.unwrap();
        assert_eq!(swept, 1);

        let row_fresh = sqlx::query("SELECT * FROM device_locations WHERE pc_id = ?")
            .bind(pc_fresh)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert!(row_fresh.get::<Option<f64>, _>("last_latitude").is_some());

        let row_stale = sqlx::query("SELECT * FROM device_locations WHERE pc_id = ?")
            .bind(pc_stale)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert!(row_stale.get::<Option<f64>, _>("last_latitude").is_none());
        assert!(row_stale.get::<Option<f64>, _>("last_longitude").is_none());
        assert!(row_stale.get::<Option<f64>, _>("last_accuracy").is_none());
        assert!(
            row_stale
                .get::<Option<String>, _>("last_acquired_at")
                .is_none()
        );
        // Request history metadata remains
        assert_eq!(
            row_stale.get::<Option<String>, _>("last_request_id"),
            Some("req-stale".to_string())
        );
    }

    fn reading(acquired_at: &str) -> LocationReading {
        LocationReading {
            latitude: 35.0,
            longitude: 139.0,
            accuracy_meters: 10.0,
            acquired_at: acquired_at.into(),
            source: LocationSource::Unknown,
        }
    }

    #[test]
    fn reading_validation_rejects_stale_future_and_out_of_range() {
        let now = Utc::now();
        let requested = now.to_rfc3339();
        let ok = reading(&now.to_rfc3339());
        assert!(validate_reading(&ok, &requested, now).is_ok());

        let stale = reading(&(now - chrono::Duration::minutes(4)).to_rfc3339());
        assert!(validate_reading(&stale, &requested, now).is_err());

        let future = reading(&(now + chrono::Duration::minutes(10)).to_rfc3339());
        assert!(validate_reading(&future, &requested, now).is_err());

        let mut bad = reading(&now.to_rfc3339());
        bad.latitude = 91.0;
        assert!(validate_reading(&bad, &requested, now).is_err());
        bad.latitude = f64::NAN;
        assert!(validate_reading(&bad, &requested, now).is_err());
        assert!(validate_reading(&reading("garbage"), &requested, now).is_err());
    }

    #[test]
    fn only_young_success_is_current() {
        let now = Utc::now();
        let young = (now - chrono::Duration::seconds(10)).to_rfc3339();
        let old = (now - chrono::Duration::minutes(30)).to_rfc3339();
        assert!(is_current(Some(LocationStatus::Success), &young, now, true));
        assert!(!is_current(
            Some(LocationStatus::Success),
            &young,
            now,
            false
        ));
        let overdue = (now - chrono::Duration::seconds(91)).to_rfc3339();
        assert!(!heartbeat_online(Some(&overdue), now));
        assert!(!heartbeat_online(None, now));
        assert!(!heartbeat_online(Some("invalid"), now));
        assert!(heartbeat_online(Some(&young), now));
        // Aged past the window: last known only.
        assert!(!is_current(Some(LocationStatus::Success), &old, now, true));
        // Any later failure (offline, timeout, denied...) demotes it.
        for st in [
            LocationStatus::Offline,
            LocationStatus::Timeout,
            LocationStatus::PermissionDenied,
            LocationStatus::Unsupported,
        ] {
            assert!(!is_current(Some(st), &young, now, true));
        }
        assert!(!is_current(None, &young, now, true));
        assert!(!is_current(
            Some(LocationStatus::Success),
            "garbage",
            now,
            true
        ));
    }

    #[tokio::test]
    async fn deletion_invalidates_only_the_matching_inflight_request() {
        let request = InFlightGuard::acquire("deleted-pc").unwrap();
        let other = InFlightGuard::acquire("surviving-pc").unwrap();
        assert!(request.is_valid());
        {
            let _deletion = endpoint_deletion_guard("deleted-pc").await;
            assert!(!request.is_valid());
            assert!(other.is_valid());
        }
        // A re-registration cannot replace the pending request's invalid flag.
        assert!(InFlightGuard::acquire("deleted-pc").is_none());
        assert!(!request.is_valid());
        drop(request);
        assert!(InFlightGuard::acquire("deleted-pc").unwrap().is_valid());
    }

    #[test]
    fn in_flight_guard_is_exclusive_per_endpoint() {
        let a = InFlightGuard::acquire("guard-pc-a").expect("first acquire");
        assert!(InFlightGuard::acquire("guard-pc-a").is_none());
        assert!(InFlightGuard::acquire("guard-pc-b").is_some());
        drop(a);
        assert!(InFlightGuard::acquire("guard-pc-a").is_some());
    }
}

/// Authorization and settings behaviour through the real router, the
/// `auth::verify` middleware and a throwaway nats-server.
#[cfg(test)]
mod router_tests {
    use std::process::Stdio;

    use axum::Router;
    use axum::body::Body;
    use axum::http::Request;
    use sqlx::sqlite::SqlitePoolOptions;
    use tower::ServiceExt;

    use super::*;

    struct App {
        router: Router,
        pool: SqlitePool,
        kv: async_nats::jetstream::kv::Store,
        nats: async_nats::Client,
        _server: tokio::process::Child,
        _storage: tempfile::TempDir,
    }

    fn mint(sub: &str) -> String {
        use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
        let claims = Claims {
            sub: sub.into(),
            exp: 4_102_444_800,
            aud: Some(crate::auth::EXPECTED_AUDIENCE.to_string()),
            roles: Vec::new(),
            allowed_features: None,
        };
        encode(
            &Header::new(Algorithm::HS256),
            &claims,
            &EncodingKey::from_secret(crate::auth::signing_secret().as_bytes()),
        )
        .expect("mint")
    }

    /// Accounts: `admin`, `op` (operator), `op_loc` (operator limited to the
    /// location page), `op_inv` (operator without it), `view` (viewer).
    /// `PC-OFF` is registered with an overdue heartbeat.
    async fn app() -> App {
        let port = portpicker::pick_unused_port().expect("pick port");
        let storage = tempfile::TempDir::new().expect("storage tempdir");
        let server = tokio::process::Command::new("nats-server")
            .arg("-js")
            .arg("-p")
            .arg(port.to_string())
            .arg("-sd")
            .arg(storage.path())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .expect("spawn nats-server (is it in PATH?)");
        let url = format!("nats://127.0.0.1:{port}");
        let mut client = None;
        for _ in 0..50 {
            match async_nats::connect(&url).await {
                Ok(c) => {
                    client = Some(c);
                    break;
                }
                Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
            }
        }
        let nats = client.expect("nats-server did not come up in 5s");
        let js = async_nats::jetstream::new(nats.clone());
        let kv = js
            .create_key_value(async_nats::jetstream::kv::Config {
                bucket: BUCKET_SERVER_SETTINGS.to_string(),
                history: 5,
                ..Default::default()
            })
            .await
            .expect("create server_settings bucket");

        // One long-lived connection: the in-memory database lives as long as
        // it does, and per-connection pragmas apply to every handler query.
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .min_connections(1)
            .idle_timeout(None)
            .max_lifetime(None)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        for (name, role, features) in [
            ("admin", "admin", None),
            ("op", "operator", None),
            ("op_loc", "operator", Some(r#"["location"]"#)),
            ("op_inv", "operator", Some(r#"["inventory"]"#)),
            ("view", "viewer", None),
        ] {
            sqlx::query(
                "INSERT INTO users (username, password_hash, role, allowed_features) \
                 VALUES (?, 'x', ?, ?)",
            )
            .bind(name)
            .bind(role)
            .bind(features)
            .execute(&pool)
            .await
            .unwrap();
        }
        let overdue = (Utc::now() - chrono::Duration::minutes(10)).to_rfc3339();
        sqlx::query("INSERT INTO agents (pc_id, last_heartbeat) VALUES ('PC-OFF', ?)")
            .bind(overdue)
            .execute(&pool)
            .await
            .unwrap();

        let state = AppState {
            passkeys: None,
            pool: pool.clone(),
            query_pool: pool.clone(),
            commands: std::sync::Arc::new(crate::command_publisher::CommandPublisher::new(
                nats.clone(),
                None,
            )),
            nats: nats.clone(),
            jetstream: js,
            explode_spec_cache: Default::default(),
            sql_view_cache: crate::api::view_sql::new_cache(),
            group_cache: crate::api::group_sql::new_cache(),
            mailer: None,
            public_url: None,
            nats_url: String::new(),
            login_throttle: Default::default(),
        };
        let router = crate::api::router(state).layer(axum::middleware::from_fn_with_state(
            pool.clone(),
            crate::auth::verify,
        ));
        App {
            router,
            pool,
            kv,
            nats,
            _server: server,
            _storage: storage,
        }
    }

    async fn call(
        app: &App,
        method: &str,
        uri: &str,
        who: &str,
        body: Option<serde_json::Value>,
    ) -> (StatusCode, String) {
        let mut req = Request::builder()
            .method(method)
            .uri(uri)
            .header("authorization", format!("Bearer {}", mint(who)))
            .header("x-kanade-source", "cli");
        let body = match body {
            Some(v) => {
                req = req.header("content-type", "application/json");
                Body::from(v.to_string())
            }
            None => Body::empty(),
        };
        let resp = app
            .router
            .clone()
            .oneshot(req.body(body).unwrap())
            .await
            .unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, String::from_utf8_lossy(&bytes).into_owned())
    }

    const SETTINGS: &str = "/api/server-settings/location";
    const LOCATION: &str = "/api/agents/PC-OFF/location";
    const REQUEST: &str = "/api/agents/PC-OFF/location/request";

    async fn put_enabled(app: &App, who: &str, enabled: bool) -> (StatusCode, String) {
        call(
            app,
            "PUT",
            SETTINGS,
            who,
            Some(serde_json::json!({ "enabled": enabled, "retention_days": 7 })),
        )
        .await
    }

    async fn view(app: &App) -> DeviceLocationResponse {
        let (st, body) = call(app, "GET", LOCATION, "op", None).await;
        assert_eq!(st, StatusCode::OK, "{body}");
        serde_json::from_str(&body).unwrap()
    }

    async fn seed_reading(app: &App) {
        let now = Utc::now().to_rfc3339();
        let fix = LocationReading {
            latitude: 35.0,
            longitude: 139.0,
            accuracy_meters: 10.0,
            acquired_at: now.clone(),
            source: LocationSource::Wifi,
        };
        record_successful_reading(&app.pool, "PC-OFF", &fix, "seed", &now, &now)
            .await
            .unwrap();
    }

    async fn stored_latitude(app: &App) -> Option<f64> {
        sqlx::query_scalar("SELECT last_latitude FROM device_locations WHERE pc_id = 'PC-OFF'")
            .fetch_one(&app.pool)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn deleted_endpoint_discards_inflight_reply_even_after_reregistration() {
        use futures::StreamExt;

        let app = app().await;
        assert_eq!(put_enabled(&app, "admin", true).await.0, StatusCode::OK);
        sqlx::query("UPDATE agents SET last_heartbeat = ? WHERE pc_id = 'PC-OFF'")
            .bind(Utc::now().to_rfc3339())
            .execute(&app.pool)
            .await
            .unwrap();
        let mut requests = app
            .nats
            .subscribe(subject::location_req("PC-OFF"))
            .await
            .unwrap();
        app.nats.flush().await.unwrap();

        let acquisition = async { call(&app, "POST", REQUEST, "op_loc", None).await };
        let deletion = async {
            let message = tokio::time::timeout(Duration::from_secs(5), requests.next())
                .await
                .unwrap()
                .unwrap();
            let request: LocationRequest = serde_json::from_slice(&message.payload).unwrap();
            assert_eq!(
                call(&app, "DELETE", "/api/agents/PC-OFF", "admin", None)
                    .await
                    .0,
                StatusCode::NO_CONTENT
            );
            assert!(view(&app).await.last_known.is_none());
            sqlx::query("INSERT INTO agents (pc_id) VALUES ('PC-OFF')")
                .execute(&app.pool)
                .await
                .unwrap();
            let reply = serde_json::json!({
                "request_id": request.request_id,
                "status": "success",
                "reading": {
                    "latitude": 35.0, "longitude": 139.0, "accuracy_meters": 10.0,
                    "source": "wifi", "acquired_at": Utc::now().to_rfc3339()
                }
            });
            app.nats
                .publish(
                    message.reply.unwrap(),
                    serde_json::to_vec(&reply).unwrap().into(),
                )
                .await
                .unwrap();
        };
        let ((status, body), ()) = tokio::join!(acquisition, deletion);
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM device_locations")
            .fetch_one(&app.pool)
            .await
            .unwrap();
        assert_eq!(count, 0);
        assert!(view(&app).await.last_known.is_none());
    }

    #[tokio::test]
    async fn location_routes_enforce_role_and_feature_gates() {
        let app = app().await;

        // Settings are admin-only.
        for who in ["op", "view"] {
            let (st, body) = put_enabled(&app, who, true).await;
            assert_eq!(st, StatusCode::FORBIDDEN, "{who}: {body}");
        }
        assert!(!load_settings(&app).await.enabled);
        let (st, body) = put_enabled(&app, "admin", true).await;
        assert_eq!(st, StatusCode::OK, "{body}");
        assert!(load_settings(&app).await.enabled);

        // Viewers and operators without the location page are refused both
        // the read and the collection request.
        for who in ["view", "op_inv"] {
            let (st, body) = call(&app, "GET", LOCATION, who, None).await;
            assert_eq!(st, StatusCode::FORBIDDEN, "GET as {who}: {body}");
            let (st, body) = call(&app, "POST", REQUEST, who, None).await;
            assert_eq!(st, StatusCode::FORBIDDEN, "POST as {who}: {body}");
        }
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM device_locations")
            .fetch_one(&app.pool)
            .await
            .unwrap();
        assert_eq!(count, 0, "a refused request must not record anything");

        // Unrestricted operators and operators granted the page get through.
        for who in ["op", "op_loc"] {
            let (st, body) = call(&app, "GET", LOCATION, who, None).await;
            assert_eq!(st, StatusCode::OK, "GET as {who}: {body}");
        }
        let (st, body) = call(&app, "POST", REQUEST, "op_loc", None).await;
        assert_eq!(st, StatusCode::OK, "{body}");
        let resp: DeviceLocationResponse = serde_json::from_str(&body).unwrap();
        assert_eq!(
            resp.latest_request.map(|r| r.status),
            Some(LocationStatus::Offline)
        );
        assert!(resp.last_known.is_none());

        let (st, _) = call(
            &app,
            "POST",
            "/api/agents/NOPE/location/request",
            "op",
            None,
        )
        .await;
        assert_eq!(st, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn offline_requests_are_rate_limited() {
        let app = app().await;
        assert_eq!(put_enabled(&app, "admin", true).await.0, StatusCode::OK);

        let (st, body) = call(&app, "POST", REQUEST, "op", None).await;
        assert_eq!(st, StatusCode::OK, "{body}");
        let first = view(&app).await.latest_request.unwrap();
        assert_eq!(first.status, LocationStatus::Offline);

        let (st, body) = call(&app, "POST", REQUEST, "op", None).await;
        assert_eq!(st, StatusCode::TOO_MANY_REQUESTS, "{body}");
        let after = view(&app).await.latest_request.unwrap();
        assert_eq!(
            after.request_id, first.request_id,
            "row must not be rewritten"
        );
    }

    #[tokio::test]
    async fn failed_disable_purge_keeps_setting_and_reenable_purges_leftovers() {
        let app = app().await;
        assert_eq!(put_enabled(&app, "admin", true).await.0, StatusCode::OK);
        seed_reading(&app).await;
        assert!(view(&app).await.last_known.is_some());

        // The purge fails: the setting must stay enabled rather than become
        // a disabled setting with coordinates still stored behind it.
        sqlx::query("PRAGMA query_only = ON")
            .execute(&app.pool)
            .await
            .unwrap();
        let (st, body) = put_enabled(&app, "admin", false).await;
        sqlx::query("PRAGMA query_only = OFF")
            .execute(&app.pool)
            .await
            .unwrap();
        assert_eq!(st, StatusCode::INTERNAL_SERVER_ERROR, "{body}");
        assert!(load_settings(&app).await.enabled);
        assert!(stored_latitude(&app).await.is_some());

        // A successful disable purges.
        assert_eq!(put_enabled(&app, "admin", false).await.0, StatusCode::OK);
        assert!(stored_latitude(&app).await.is_none());

        // Coordinates left behind while disabled (an older build, a manual
        // edit) are purged on re-enable instead of reappearing.
        seed_reading(&app).await;
        assert_eq!(put_enabled(&app, "admin", true).await.0, StatusCode::OK);
        assert!(stored_latitude(&app).await.is_none());
        let resp = view(&app).await;
        assert!(resp.enabled);
        assert!(resp.last_known.is_none());
    }

    async fn load_settings(app: &App) -> LocationSettings {
        match app.kv.get(KEY_SERVER_SETTINGS).await.unwrap() {
            Some(bytes) => serde_json::from_slice::<ServerSettings>(&bytes)
                .unwrap()
                .effective_location(),
            None => LocationSettings::default(),
        }
    }
}
