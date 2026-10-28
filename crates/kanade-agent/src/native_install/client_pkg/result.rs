//! Serializable local results. Receipt/bundle verification never claims GUI
//! activation or a KLP handshake. No transport or result publication is wired.
use serde::Serialize;

use super::transaction::{Outcome, Report};
use super::{Error, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GuiActivation {
    UnsupportedNotAttempted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureClass {
    Rejected,
    Deferred,
    RolledBack,
    RecoveryRequired,
}
impl Error {
    pub fn class(&self) -> FailureClass {
        match self {
            Self::Invalid(_) | Self::Authorization(_) | Self::Unmanaged => FailureClass::Rejected,
            Self::Deferred(_) => FailureClass::Deferred,
            Self::RolledBack => FailureClass::RolledBack,
            // An unqualified I/O failure or interruption cannot prove that
            // installer has left the receipt database and bundle unchanged.
            Self::RecoveryRequired(_) | Self::Interrupted | Self::Io(_) => {
                FailureClass::RecoveryRequired
            }
        }
    }
    pub fn code(&self) -> &'static str {
        match self {
            Self::Invalid(_) => "package_refused",
            Self::Authorization(_) => "authorization_refused",
            Self::Deferred(_) => "deferred",
            Self::Unmanaged => "unmanaged_existing",
            Self::RecoveryRequired(_) => "recovery_required",
            Self::RolledBack => "rolled_back",
            Self::Interrupted => "interrupted",
            Self::Io(_) => "io_error",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ResultStatus {
    InstalledVerified,
    NoOpVerified,
    Rejected,
    Deferred,
    RolledBack,
    RecoveryRequired,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ResultRecord {
    pub status: ResultStatus,
    pub code: String,
    pub detail: String,
    pub installed_verified: bool,
    pub version: Option<String>,
    pub gui_activation: GuiActivation,
    pub failure_class: Option<FailureClass>,
}
impl ResultRecord {
    pub fn from_result(result: &Result<Report>) -> Self {
        match result {
            Ok(report) => Self {
                status: match report.outcome {
                    Outcome::InstalledVerified => ResultStatus::InstalledVerified,
                    Outcome::NoOpVerified => ResultStatus::NoOpVerified,
                },
                code: match report.outcome {
                    Outcome::InstalledVerified => "installed_verified",
                    Outcome::NoOpVerified => "no_op_verified",
                }
                .into(),
                detail: String::new(),
                installed_verified: true,
                version: Some(report.version.clone()),
                gui_activation: report.gui_activation,
                failure_class: None,
            },
            Err(error) => Self {
                status: match error.class() {
                    FailureClass::Rejected => ResultStatus::Rejected,
                    FailureClass::Deferred => ResultStatus::Deferred,
                    FailureClass::RolledBack => ResultStatus::RolledBack,
                    FailureClass::RecoveryRequired => ResultStatus::RecoveryRequired,
                },
                code: error.code().into(),
                detail: error.to_string(),
                installed_verified: false,
                version: None,
                gui_activation: GuiActivation::UnsupportedNotAttempted,
                failure_class: Some(error.class()),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failures_never_claim_installed_or_gui_verification() {
        for (error, class) in [
            (Error::Invalid("payload".into()), FailureClass::Rejected),
            (
                Error::Authorization("expired".into()),
                FailureClass::Rejected,
            ),
            (Error::Unmanaged, FailureClass::Rejected),
            (Error::Deferred("lock".into()), FailureClass::Deferred),
            (Error::RolledBack, FailureClass::RolledBack),
            (
                Error::RecoveryRequired("receipt".into()),
                FailureClass::RecoveryRequired,
            ),
            (Error::Interrupted, FailureClass::RecoveryRequired),
            (
                Error::Io(std::io::Error::other("unknown phase")),
                FailureClass::RecoveryRequired,
            ),
        ] {
            let record = ResultRecord::from_result(&Err(error));
            assert_eq!(record.failure_class, Some(class));
            assert!(!record.installed_verified);
            assert_eq!(record.version, None);
            assert_eq!(
                record.gui_activation,
                GuiActivation::UnsupportedNotAttempted
            );
            let json = serde_json::to_value(&record).unwrap();
            assert_eq!(json["installed_verified"], false);
            assert_eq!(json["gui_activation"], "unsupported_not_attempted");
            assert!(!record.detail.is_empty());
        }
    }
}
