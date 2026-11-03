//! Local Client coordination, separate from KLP UID admission.
#[cfg(unix)]
pub mod gate;
#[cfg(target_os = "macos")]
pub mod macos;

use serde::{Deserialize, Serialize};

/// Detect state corruption in addition to filesystem ownership and mode checks.
/// This is an unkeyed checksum, not proof of authorship.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ChecksumEnvelope<T> {
    body: T,
    sha256: crate::artifact::ArtifactSha256,
}
pub fn encode_envelope<T: Serialize>(body: &T) -> std::io::Result<Vec<u8>> {
    let sha256 = crate::artifact::ArtifactSha256::of(&serde_json::to_vec(body)?);
    Ok(serde_json::to_vec(&ChecksumEnvelope { body, sha256 })?)
}
pub fn decode_envelope<T: serde::de::DeserializeOwned + Serialize>(
    bytes: &[u8],
) -> std::io::Result<T> {
    let envelope: ChecksumEnvelope<T> = serde_json::from_slice(bytes)?;
    if envelope.sha256 != crate::artifact::ArtifactSha256::of(&serde_json::to_vec(&envelope.body)?)
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "lifecycle checksum mismatch",
        ));
    }
    Ok(envelope.body)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Instance {
    /// Kernel audit token: includes UID, audit session and PID generation.
    pub token: [u32; 8],
    pub device: u64,
    pub inode: u64,
}
impl Instance {
    pub fn pid(self) -> u32 {
        self.token[5]
    }
    pub fn uid(self) -> u32 {
        self.token[1]
    }
    pub fn session(self) -> u32 {
        self.token[6]
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Presence {
    pub gui: Instance,
    pub helper: Instance,
    pub generation: u64,
    pub sequence: u64,
    pub local_active_unlocked: bool,
    pub quit_generation: u64,
    pub opted_out: bool,
}
impl Presence {
    /// Caller must separately authenticate peer/process and bound receipt age.
    /// A different helper connection/session never inherits restart intent.
    pub fn may_restore(&self, current: &Self) -> bool {
        self.gui == current.gui
            && self.helper == current.helper
            && self.generation == current.generation
            && current.sequence > self.sequence
            && self.gui.uid() != 0
            && self.gui.session() != 0
            && self.local_active_unlocked
            && current.local_active_unlocked
            && self.quit_generation == 0
            && current.quit_generation == 0
            && !self.opted_out
            && !current.opted_out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn presence(uid: u32) -> Presence {
        let mut token = [0; 8];
        token[1] = uid;
        token[5] = 42;
        token[6] = 9;
        token[7] = 123;
        let gui = Instance {
            token,
            device: 1,
            inode: 2,
        };
        token[5] = 43;
        Presence {
            gui,
            helper: Instance { token, ..gui },
            generation: 1,
            sequence: 1,
            local_active_unlocked: true,
            quit_generation: 0,
            opted_out: false,
        }
    }
    #[test]
    fn checksum_rejects_valid_json_with_modified_body() {
        let bytes = encode_envelope(&presence(501)).unwrap();
        let restored: Presence = decode_envelope(&bytes).unwrap();
        assert_eq!(restored, presence(501));
        let mut changed: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        changed["body"]["opted_out"] = true.into();
        assert!(decode_envelope::<Presence>(&serde_json::to_vec(&changed).unwrap()).is_err());
        assert!(decode_envelope::<Presence>(b"{}").is_err());
    }
    #[test]
    fn restore_requires_fresh_same_session_without_quit_or_optout() {
        let before = presence(501);
        let after = Presence {
            sequence: 2,
            ..before
        };
        assert!(before.may_restore(&after));
        assert!(!before.may_restore(&before));
        for changed in [
            Presence {
                local_active_unlocked: false,
                ..after
            },
            Presence {
                quit_generation: 1,
                ..after
            },
            Presence {
                opted_out: true,
                ..after
            },
            Presence {
                generation: 2,
                ..after
            },
            presence(502),
        ] {
            assert!(!before.may_restore(&changed));
        }
        let mut reused = after;
        reused.gui.token[7] += 1;
        assert!(!before.may_restore(&reused));
        let mut switched = after;
        switched.gui.token[6] += 1;
        assert!(!before.may_restore(&switched));
        // Quit and opt-out are independent; neither can be erased by restart intent.
        assert!(
            !Presence {
                quit_generation: 1,
                ..before
            }
            .may_restore(&after)
        );
        assert!(
            !Presence {
                opted_out: true,
                ..before
            }
            .may_restore(&after)
        );
    }
}
