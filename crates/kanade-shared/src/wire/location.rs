//! Wire types for lost-device location visibility and on-demand acquisition.

use serde::{Deserialize, Serialize};

/// Built-in default retention window (days) for location data.
pub const DEFAULT_LOCATION_RETENTION_DAYS: u32 = 7;

/// Upper bound on location data retention window (days).
/// Sensitive location data must have bounded retention.
pub const MAX_LOCATION_RETENTION_DAYS: u32 = 30;

/// Settings governing lost-device location visibility and retention.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(default)]
pub struct LocationSettings {
    /// Whether lost-device location collection is enabled.
    /// Default: `false` (explicit opt-in required).
    pub enabled: bool,
    /// Bounded retention window in days (default: 7, max: 30).
    pub retention_days: u32,
}

impl Default for LocationSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            retention_days: DEFAULT_LOCATION_RETENTION_DAYS,
        }
    }
}

impl LocationSettings {
    /// Return the effective retention days, clamped to `[1, MAX_LOCATION_RETENTION_DAYS]`.
    pub fn effective_retention_days(&self) -> u32 {
        if self.retention_days == 0 {
            DEFAULT_LOCATION_RETENTION_DAYS
        } else {
            self.retention_days.min(MAX_LOCATION_RETENTION_DAYS)
        }
    }
}

/// Backend → Agent on-demand location request on `location.req.<pc_id>`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct LocationRequest {
    /// Unique identifier for this request (UUIDv4).
    pub request_id: String,
    /// When the request was initiated by the operator (RFC3339).
    pub requested_at: String,
    /// Request expiration timestamp (RFC3339). Stale requests must be refused.
    pub expires_at: String,
}

/// Agent → Backend reply for a [`LocationRequest`].
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct LocationReply {
    /// The matching request ID.
    pub request_id: String,
    /// Outcome status of the location acquisition attempt.
    pub status: LocationStatus,
    /// Human-readable explanation when status is not `Success`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Location reading when acquisition succeeded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reading: Option<LocationReading>,
}

impl LocationReply {
    pub fn success(request_id: String, reading: LocationReading) -> Self {
        Self {
            request_id,
            status: LocationStatus::Success,
            reason: None,
            reading: Some(reading),
        }
    }

    pub fn failure(request_id: String, status: LocationStatus, reason: impl Into<String>) -> Self {
        Self {
            request_id,
            status,
            reason: Some(reason.into()),
            reading: None,
        }
    }
}

/// Status of the location acquisition attempt.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LocationStatus {
    /// Successfully acquired fresh location coordinates.
    Success,
    /// Platform is not supported for native location services (e.g. Linux).
    Unsupported,
    /// Location access denied by the OS or user permissions (e.g. TCC / Windows Privacy).
    PermissionDenied,
    /// OS-level location service is disabled globally.
    LocationDisabled,
    /// No interactive user session exists (e.g. headless, logged out, service session).
    NoUserSession,
    /// Location is unavailable (e.g. hardware absent, no fix, sensor error).
    Unavailable,
    /// Acquisition timed out.
    Timeout,
    /// Request expired before processing.
    Expired,
    /// Endpoint is offline or unreachable.
    Offline,
}

impl LocationStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Unsupported => "unsupported",
            Self::PermissionDenied => "permission_denied",
            Self::LocationDisabled => "location_disabled",
            Self::NoUserSession => "no_user_session",
            Self::Unavailable => "unavailable",
            Self::Timeout => "timeout",
            Self::Expired => "expired",
            Self::Offline => "offline",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "success" => Some(Self::Success),
            "unsupported" => Some(Self::Unsupported),
            "permission_denied" => Some(Self::PermissionDenied),
            "location_disabled" => Some(Self::LocationDisabled),
            "no_user_session" => Some(Self::NoUserSession),
            "unavailable" => Some(Self::Unavailable),
            "timeout" => Some(Self::Timeout),
            "expired" => Some(Self::Expired),
            "offline" => Some(Self::Offline),
            _ => None,
        }
    }
}

/// A single location coordinate reading.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct LocationReading {
    /// Latitude in degrees (-90.0 to 90.0).
    pub latitude: f64,
    /// Longitude in degrees (-180.0 to 180.0).
    pub longitude: f64,
    /// Horizontal accuracy radius in meters.
    pub accuracy_meters: f64,
    /// When the reading was acquired from OS location service (RFC3339).
    pub acquired_at: String,
    /// Source / provider of the location reading.
    #[serde(default)]
    pub source: LocationSource,
}

/// Source technology reported by the OS location API.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum LocationSource {
    #[default]
    Unknown,
    Gps,
    Wifi,
    Cellular,
    Ip,
}

impl LocationSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Gps => "gps",
            Self::Wifi => "wifi",
            Self::Cellular => "cellular",
            Self::Ip => "ip",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "unknown" => Some(Self::Unknown),
            "gps" => Some(Self::Gps),
            "wifi" => Some(Self::Wifi),
            "cellular" => Some(Self::Cellular),
            "ip" => Some(Self::Ip),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn location_settings_defaults_and_clamping() {
        let def = LocationSettings::default();
        assert!(!def.enabled);
        assert_eq!(def.retention_days, 7);
        assert_eq!(def.effective_retention_days(), 7);

        let clamped = LocationSettings {
            enabled: true,
            retention_days: 90,
        };
        assert_eq!(clamped.effective_retention_days(), 30);

        let zero = LocationSettings {
            enabled: true,
            retention_days: 0,
        };
        assert_eq!(zero.effective_retention_days(), 7);
    }

    #[test]
    fn location_request_reply_roundtrip() {
        let req = LocationRequest {
            request_id: "req-123".into(),
            requested_at: "2026-10-18T00:00:00Z".into(),
            expires_at: "2026-10-18T00:00:15Z".into(),
        };
        let json = serde_json::to_string(&req).unwrap();
        let parsed: LocationRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(req, parsed);

        let reply = LocationReply::success(
            "req-123".into(),
            LocationReading {
                latitude: 35.6812,
                longitude: 139.7671,
                accuracy_meters: 15.5,
                acquired_at: "2026-10-18T00:00:05Z".into(),
                source: LocationSource::Wifi,
            },
        );
        let json = serde_json::to_string(&reply).unwrap();
        let parsed: LocationReply = serde_json::from_str(&json).unwrap();
        assert_eq!(reply, parsed);
    }

    #[test]
    fn location_status_roundtrip() {
        for status in [
            LocationStatus::Success,
            LocationStatus::Unsupported,
            LocationStatus::PermissionDenied,
            LocationStatus::LocationDisabled,
            LocationStatus::NoUserSession,
            LocationStatus::Unavailable,
            LocationStatus::Timeout,
            LocationStatus::Expired,
            LocationStatus::Offline,
        ] {
            assert_eq!(LocationStatus::parse(status.as_str()), Some(status));
        }
    }
}
