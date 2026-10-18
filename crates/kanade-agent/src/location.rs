//! On-demand lost-device location listener for managed endpoints.
//!
//! Subscribes to `location.req.<pc_id>` on core NATS, checks OS-level location
//! service permissions and interactive user sessions, acquires coordinates via
//! platform-supported APIs without bypassing permissions, and replies with explicit
//! status and readings.
//!
//! # Platform Constraints and Behavioral Guarantees
//!
//! 1. No background/daemon bypass: OS location services restrict background/daemon
//!    access. Requests require an active interactive user session on the console.
//! 2. Explicit failure classification: Never claims success on failure. Distinguishes
//!    `Unsupported`, `PermissionDenied`, `LocationDisabled`, `NoUserSession`,
//!    `Unavailable`, `Timeout`, and `Expired`.
//! 3. Bounded execution: Requests enforce `expires_at` and a processing timeout.
//! 4. Linux endpoints: Explicitly reported as `Unsupported` rather than claiming success.

use std::time::Duration;

use chrono::{DateTime, Utc};
use futures::StreamExt;
use kanade_shared::subject;
use kanade_shared::wire::{
    LocationReading, LocationReply, LocationRequest, LocationSource, LocationStatus,
};
use tracing::{info, warn};

/// Processing timeout for local OS location measurement.
const LOCAL_MEASURE_TIMEOUT: Duration = Duration::from_secs(10);

/// Spawns the NATS listener task for location requests on this endpoint.
pub fn start_listener(client: async_nats::Client, pc_id: String) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let sub_topic = subject::location_req(&pc_id);
        info!(pc_id = %pc_id, subject = %sub_topic, "starting location request listener");

        let mut sub = match client.subscribe(sub_topic.clone()).await {
            Ok(s) => s,
            Err(e) => {
                warn!(error = %e, subject = %sub_topic, "failed to subscribe to location requests");
                return;
            }
        };

        while let Some(msg) = sub.next().await {
            let Some(reply_to) = msg.reply.clone() else {
                warn!("received location request with no reply inbox; dropping");
                continue;
            };

            let req: LocationRequest = match serde_json::from_slice(&msg.payload) {
                Ok(r) => r,
                Err(e) => {
                    warn!(error = %e, "failed to decode LocationRequest");
                    continue;
                }
            };

            let client_clone = client.clone();
            let reply_to_str = reply_to.to_string();

            // Handle each request concurrently in a task
            tokio::spawn(async move {
                let reply = handle_location_request(req).await;
                if let Ok(body) = serde_json::to_vec(&reply) {
                    if let Err(e) = client_clone.publish(reply_to_str, body.into()).await {
                        warn!(error = %e, "failed to publish location reply");
                    }
                }
            });
        }
    })
}

/// Handle a single incoming location request.
pub async fn handle_location_request(req: LocationRequest) -> LocationReply {
    // 1. Check expiration
    if let Ok(exp) = DateTime::parse_from_rfc3339(&req.expires_at) {
        if Utc::now() > exp.with_timezone(&Utc) {
            return LocationReply::failure(
                req.request_id,
                LocationStatus::Expired,
                "Location request expired before processing",
            );
        }
    }

    // 2. Measure location with bounded timeout
    match tokio::time::timeout(LOCAL_MEASURE_TIMEOUT, measure_device_location()).await {
        Err(_) => LocationReply::failure(
            req.request_id,
            LocationStatus::Timeout,
            "OS location acquisition timed out",
        ),
        Ok((status, reason, reading)) => {
            if let (LocationStatus::Success, Some(r)) = (status, reading) {
                LocationReply::success(req.request_id, r)
            } else {
                LocationReply::failure(
                    req.request_id,
                    status,
                    reason.unwrap_or_else(|| "Location acquisition failed".to_string()),
                )
            }
        }
    }
}

/// Platform-specific location measurement.
async fn measure_device_location() -> (LocationStatus, Option<String>, Option<LocationReading>) {
    #[cfg(target_os = "linux")]
    {
        (
            LocationStatus::Unsupported,
            Some("Linux endpoint location is not supported (GeoClue2 daemon integration requires active desktop session and location service backend)".to_string()),
            None,
        )
    }

    #[cfg(target_os = "macos")]
    {
        measure_macos_location().await
    }

    #[cfg(target_os = "windows")]
    {
        measure_windows_location().await
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        (
            LocationStatus::Unsupported,
            Some("Platform is not supported for native location services".to_string()),
            None,
        )
    }
}

#[cfg(target_os = "macos")]
async fn measure_macos_location() -> (LocationStatus, Option<String>, Option<LocationReading>) {
    // Check if an interactive console session is active.
    // CoreLocation cannot be authorized or queried without an active GUI user session.
    let uid = crate::screen_permission_macos::console_uid();
    if uid.is_none() {
        return (
            LocationStatus::NoUserSession,
            Some("No active console user session on endpoint".to_string()),
            None,
        );
    }

    // Call native CoreLocation bridge
    unsafe {
        let mut status_code: i32 = 0;
        let mut lat: f64 = 0.0;
        let mut lon: f64 = 0.0;
        let mut acc: f64 = 0.0;

        let res = kanade_macos_acquire_location(&mut status_code, &mut lat, &mut lon, &mut acc);

        match status_code {
            0 if res => {
                let reading = LocationReading {
                    latitude: lat,
                    longitude: lon,
                    accuracy_meters: acc,
                    acquired_at: Utc::now().to_rfc3339(),
                    // Neither CoreLocation nor the Windows reading API is asked which
                    // provider produced the fix, so do not guess.
                    source: LocationSource::Unknown,
                };
                (LocationStatus::Success, None, Some(reading))
            }
            1 => (
                LocationStatus::LocationDisabled,
                Some("macOS Location Services are disabled in System Settings".to_string()),
                None,
            ),
            2 => (
                LocationStatus::PermissionDenied,
                Some("Location permission was denied or not determined by user".to_string()),
                None,
            ),
            3 => (
                LocationStatus::Unavailable,
                Some("Location hardware or network fix is currently unavailable".to_string()),
                None,
            ),
            _ => (
                LocationStatus::Unavailable,
                Some("CoreLocation returned an error or no location fix".to_string()),
                None,
            ),
        }
    }
}

#[cfg(target_os = "macos")]
unsafe extern "C" {
    /// Native bridge implemented in Objective-C.
    /// Status codes: 0 = Success, 1 = LocationDisabled, 2 = PermissionDenied, 3 = Unavailable
    fn kanade_macos_acquire_location(
        status_code: *mut i32,
        latitude: *mut f64,
        longitude: *mut f64,
        accuracy: *mut f64,
    ) -> bool;
}

#[cfg(target_os = "windows")]
async fn measure_windows_location() -> (LocationStatus, Option<String>, Option<LocationReading>) {
    use windows::Win32::System::RemoteDesktop::WTSGetActiveConsoleSessionId;

    // Check console session
    let session_id = unsafe { WTSGetActiveConsoleSessionId() };
    if session_id == 0xFFFFFFFF {
        return (
            LocationStatus::NoUserSession,
            Some("No active console session on endpoint".to_string()),
            None,
        );
    }

    // Call WinRT Geolocation API
    // Status codes: 0 = Success, 1 = LocationDisabled, 2 = PermissionDenied, 3 = Unavailable
    match acquire_winrt_location().await {
        Ok(reading) => (LocationStatus::Success, None, Some(reading)),
        Err((status, reason)) => (status, Some(reason), None),
    }
}

#[cfg(target_os = "windows")]
async fn acquire_winrt_location() -> Result<LocationReading, (LocationStatus, String)> {
    use windows::Devices::Geolocation::{GeolocationAccessStatus, Geolocator};

    let access = Geolocator::RequestAccessAsync()
        .map_err(|e| {
            (
                LocationStatus::Unavailable,
                format!("WinRT RequestAccess error: {e}"),
            )
        })?
        .await
        .map_err(|e| {
            (
                LocationStatus::Unavailable,
                format!("WinRT RequestAccess await error: {e}"),
            )
        })?;

    match access {
        GeolocationAccessStatus::Denied => Err((
            LocationStatus::PermissionDenied,
            "Windows location permission denied by user or group policy".to_string(),
        )),
        GeolocationAccessStatus::Unspecified => Err((
            LocationStatus::LocationDisabled,
            "Windows Location platform is unspecified or disabled in Settings".to_string(),
        )),
        GeolocationAccessStatus::Allowed => {
            let locator = Geolocator::new().map_err(|e| {
                (
                    LocationStatus::Unavailable,
                    format!("Failed to instantiate Geolocator: {e}"),
                )
            })?;

            let pos = locator
                .GetGeopositionAsync()
                .map_err(|e| {
                    (
                        LocationStatus::Unavailable,
                        format!("WinRT GetGeoposition error: {e}"),
                    )
                })?
                .await
                .map_err(|e| {
                    (
                        LocationStatus::Unavailable,
                        format!("WinRT GetGeoposition await error: {e}"),
                    )
                })?;

            let coord = pos.Coordinate().map_err(|e| {
                (
                    LocationStatus::Unavailable,
                    format!("Failed to get Coordinate: {e}"),
                )
            })?;

            let point = coord.Point().map_err(|e| {
                (
                    LocationStatus::Unavailable,
                    format!("Failed to get Point: {e}"),
                )
            })?;

            let pos_point = point.Position().map_err(|e| {
                (
                    LocationStatus::Unavailable,
                    format!("Failed to get Position: {e}"),
                )
            })?;

            let accuracy = coord.Accuracy().unwrap_or(50.0);

            Ok(LocationReading {
                latitude: pos_point.Latitude,
                longitude: pos_point.Longitude,
                accuracy_meters: accuracy,
                acquired_at: Utc::now().to_rfc3339(),
                // Neither CoreLocation nor the Windows reading API is asked which
                // provider produced the fix, so do not guess.
                source: LocationSource::Unknown,
            })
        }
        _ => Err((
            LocationStatus::Unavailable,
            "Unknown GeolocationAccessStatus".to_string(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn expired_request_is_rejected() {
        let req = LocationRequest {
            request_id: "test-expired".into(),
            requested_at: "2026-10-18T00:00:00Z".into(),
            expires_at: "2026-10-18T00:00:01Z".into(),
        };
        let reply = handle_location_request(req).await;
        assert_eq!(reply.status, LocationStatus::Expired);
        assert_eq!(reply.request_id, "test-expired");
        assert!(reply.reading.is_none());
    }

    #[tokio::test]
    async fn unexpired_request_runs_measure() {
        let req = LocationRequest {
            request_id: "test-valid".into(),
            requested_at: Utc::now().to_rfc3339(),
            expires_at: (Utc::now() + chrono::Duration::seconds(60)).to_rfc3339(),
        };
        let reply = handle_location_request(req).await;
        assert_eq!(reply.request_id, "test-valid");
        // On Linux or without GUI session/CoreLocation permission, it will return an explicit status
        assert!(
            matches!(
                reply.status,
                LocationStatus::Unsupported
                    | LocationStatus::NoUserSession
                    | LocationStatus::PermissionDenied
                    | LocationStatus::LocationDisabled
                    | LocationStatus::Unavailable
                    | LocationStatus::Success
            ),
            "Unexpected status: {:?}",
            reply.status
        );
    }
}
