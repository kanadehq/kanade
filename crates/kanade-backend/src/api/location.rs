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
//! 4. Audit privacy: every view, request and settings change is audit-logged, but coordinates
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
use std::time::Duration;
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

/// Endpoints with a location request currently in flight (one at a time each).
static IN_FLIGHT: std::sync::LazyLock<std::sync::Mutex<std::collections::HashSet<String>>> =
    std::sync::LazyLock::new(Default::default);

struct InFlightGuard(String);

impl InFlightGuard {
    fn acquire(pc_id: &str) -> Option<Self> {
        let mut set = IN_FLIGHT.lock().unwrap_or_else(|e| e.into_inner());
        set.insert(pc_id.to_string())
            .then(|| Self(pc_id.to_string()))
    }
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
) -> bool {
    latest_status == Some(LocationStatus::Success)
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
    let loc_settings = load_location_settings(&s).await;
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
            last_request_reason
        FROM device_locations
        WHERE pc_id = ?
        "#,
    )
    .bind(&pc_id)
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
                let current = is_current(latest_status, &acquired_at, Utc::now());
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

    // Audit log: record viewing access, but NEVER include coordinates
    audit::record(
        &s.nats,
        "operator",
        "agent.location.view",
        Some(&pc_id),
        Some(&caller),
        serde_json::json!({
            "pc_id": &pc_id,
            "has_last_known": last_known.is_some(),
        }),
    )
    .await;

    Ok(Json(DeviceLocationResponse {
        pc_id,
        enabled: loc_settings.enabled,
        retention_days,
        last_known,
        latest_request,
    }))
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
    let loc_settings = load_location_settings(&s).await;
    if !loc_settings.enabled {
        return Err((
            StatusCode::BAD_REQUEST,
            "Lost-device location visibility is disabled in server settings".into(),
        ));
    }

    // Check endpoint existence and heartbeat
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

    let last_hb_str: Option<String> = agent.get("last_heartbeat");
    let is_offline = match last_hb_str {
        Some(hb) => DateTime::parse_from_rfc3339(&hb)
            .map(|dt| {
                let diff = Utc::now().signed_duration_since(dt.with_timezone(&Utc));
                diff.num_seconds() > OFFLINE_HEARTBEAT_THRESHOLD_SECS
            })
            .unwrap_or(true),
        None => true,
    };

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

        return get_location(State(s), Path(pc_id), caller).await;
    }

    let Some(_in_flight) = InFlightGuard::acquire(&pc_id) else {
        return Err((
            StatusCode::CONFLICT,
            format!("A location request for `{pc_id}` is already in progress"),
        ));
    };

    // Rate-limiting check: avoid flooding requests
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
        s.nats.request(req_subject, body.into()),
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

    get_location(State(s), Path(pc_id), caller).await
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

    let kv = open_server_settings_bucket(&s).await?;

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

    // If disabled, purge stored coordinates from database
    if !new_location.enabled {
        let _ = sqlx::query(
            r#"
            UPDATE device_locations
            SET last_latitude = NULL,
                last_longitude = NULL,
                last_accuracy = NULL,
                last_source = NULL,
                last_acquired_at = NULL
            "#,
        )
        .execute(&s.pool)
        .await;
    }

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

        let stale = reading(&(now - chrono::Duration::minutes(10)).to_rfc3339());
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
        assert!(is_current(Some(LocationStatus::Success), &young, now));
        // Aged past the window: last known only.
        assert!(!is_current(Some(LocationStatus::Success), &old, now));
        // Any later failure (offline, timeout, denied...) demotes it.
        for st in [
            LocationStatus::Offline,
            LocationStatus::Timeout,
            LocationStatus::PermissionDenied,
            LocationStatus::Unsupported,
        ] {
            assert!(!is_current(Some(st), &young, now));
        }
        assert!(!is_current(None, &young, now));
        assert!(!is_current(Some(LocationStatus::Success), "garbage", now));
    }

    #[test]
    fn in_flight_guard_is_exclusive_per_endpoint() {
        let a = InFlightGuard::acquire("guard-pc-a").expect("first acquire");
        assert!(InFlightGuard::acquire("guard-pc-a").is_none());
        assert!(InFlightGuard::acquire("guard-pc-b").is_some());
        drop(a);
        assert!(InFlightGuard::acquire("guard-pc-a").is_some());
    }

    #[test]
    fn location_routes_are_operator_gated_and_settings_admin_gated() {
        // The router wires /location routes into the operator group and the
        // settings route into the admin group; guard the source so a refactor
        // cannot silently move them to the viewer-level base router.
        let src = include_str!("mod.rs");
        let op = src.find("let operator = Router::new()").unwrap();
        let adm = src.find("let admin = Router::new()").unwrap();
        let loc = src.find("location::get_location").unwrap();
        let req = src.find("location::request_location").unwrap();
        let set = src.find("location::get_settings").unwrap();
        assert!(op < loc && loc < adm);
        assert!(op < req && req < adm);
        assert!(set > adm);
    }
}
