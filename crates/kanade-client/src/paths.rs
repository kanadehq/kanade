//! Unix per-user paths from SPEC §2.11.7.3. No process-global HOME lookup.
//!
//! Absolute paths alone do not establish ownership. Before accessing these paths,
//! a future Unix storage adapter must validate ownership, reject symlink traversal,
//! and create directories 0700 / files 0600 as the target UID. In particular, two
//! users may supply the same XDG override; that must fail ownership validation,
//! never silently share configuration, logs or UI state. Windows keeps its paths.

use std::ffi::{OsStr, OsString};
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnixPlatform {
    Linux,
    MacOs,
}

/// Account-database identity, not inherited root HOME or unverified environment.
#[derive(Debug, Clone)]
pub struct UserAccount {
    pub uid: u32,
    pub home: PathBuf,
}

/// Values acquired in the target user's context, never copied from root Agent.
#[derive(Debug, Default, Clone)]
pub struct XdgOverrides {
    pub config: Option<OsString>,
    pub state: Option<OsString>,
    pub cache: Option<OsString>,
}

/// OS boundary: bind both account data and environment to the requested UID.
/// An implementation must reject an account/environment identity mismatch.
pub trait UserPathsResolver {
    type Error;
    fn resolve(&self, uid: u32) -> Result<(UserAccount, XdgOverrides), Self::Error>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientPaths {
    pub uid: u32,
    pub config: PathBuf,
    pub log: PathBuf,
    pub cache: PathBuf,
    pub window_size: PathBuf,
}

#[derive(Debug, PartialEq, Eq)]
pub enum PathError {
    InvalidHome,
}

// Unix syntax even when policy tests are compiled on Windows. Path::is_absolute
// would incorrectly accept drive/UNC paths there and reject Unix rooted paths.
fn unix_absolute(path: &OsStr) -> bool {
    path.as_encoded_bytes().first() == Some(&b'/') && !path.as_encoded_bytes().contains(&0)
}

impl ClientPaths {
    pub fn resolve(
        platform: UnixPlatform,
        user: &UserAccount,
        xdg: &XdgOverrides,
    ) -> Result<Self, PathError> {
        if !unix_absolute(user.home.as_os_str()) {
            return Err(PathError::InvalidHome);
        }
        let (config, state, logs, cache) = match platform {
            UnixPlatform::Linux => {
                let base = |value: &Option<OsString>, fallback: &str| {
                    value
                        .as_deref()
                        .filter(|v| unix_absolute(v))
                        .map(PathBuf::from)
                        .unwrap_or_else(|| user.home.join(fallback))
                        .join("kanade-client")
                };
                let config = base(&xdg.config, ".config");
                let state = base(&xdg.state, ".local/state");
                let cache = base(&xdg.cache, ".cache");
                (config, state.clone(), state.join("logs"), cache)
            }
            UnixPlatform::MacOs => {
                let library = user.home.join("Library");
                let support = library.join("Application Support/Kanade/Client");
                (
                    support.clone(),
                    support,
                    library.join("Logs/Kanade/Client"),
                    library.join("Caches/Kanade/Client"),
                )
            }
        };
        Ok(Self {
            uid: user.uid,
            config: config.join("client.toml"),
            log: logs.join("client.log"),
            cache,
            window_size: state.join(crate::window_size::FILE_NAME),
        })
    }
}

/// Storage boundary for later Unix integration. Implementations must verify the
/// effective UID equals `paths.uid`, ownership of existing private directories
/// and files, reject symlinks, and enforce 0700/0600 before any read or write.
/// Fail closed on shared/foreign paths; callers warn and continue without saved
/// state. This is not implemented by the legacy Windows window-size store.
pub trait PrivateUserStorage {
    type Error;
    fn validate_or_create(&self, paths: &ClientPaths) -> Result<(), Self::Error>;
}

/// One component of private app storage, after OS validation. The adapter must
/// check each existing private component below the selected XDG base (or macOS
/// app directory), not require unrelated config/log files to exist. Shared
/// ancestors such as /home are outside this private subtree.
#[derive(Debug, Default)]
pub struct PrivateStorageFacts {
    pub effective_uid: Option<u32>,
    pub owner_uid: Option<u32>,
    /// Permission bits only (mode & 0o7777), excluding file-type bits.
    pub permissions: Option<u32>,
    pub no_symlinks_verified: bool,
}

#[derive(Debug, Clone, Copy)]
pub enum PrivateEntry {
    Directory,
    File,
}

/// Read/write admission for one component after creation/validation. Unknown
/// ownership or traversal checks fail closed. New entries must be created
/// privately and validated by the adapter. This is unrelated to KLP admission.
pub fn private_storage_access(
    paths: &ClientPaths,
    entry: PrivateEntry,
    facts: &PrivateStorageFacts,
) -> bool {
    let mode = match entry {
        PrivateEntry::Directory => 0o700,
        PrivateEntry::File => 0o600,
    };
    facts.effective_uid == Some(paths.uid)
        && facts.owner_uid == Some(paths.uid)
        && facts.permissions == Some(mode)
        && facts.no_symlinks_verified
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(uid: u32) -> UserAccount {
        UserAccount {
            uid,
            home: PathBuf::from(format!("/users/利用者{uid}")),
        }
    }

    #[test]
    fn xdg_unset_empty_relative_and_windows_paths_fall_back() {
        for value in [
            None,
            Some(""),
            Some("relative"),
            Some("C:\\data"),
            Some("\\\\server\\share"),
        ] {
            let xdg = XdgOverrides {
                config: value.map(Into::into),
                state: value.map(Into::into),
                cache: value.map(Into::into),
            };
            let p = ClientPaths::resolve(UnixPlatform::Linux, &user(1001), &xdg).unwrap();
            assert_eq!(
                p.config,
                user(1001).home.join(".config/kanade-client/client.toml")
            );
            assert_eq!(
                p.log,
                user(1001)
                    .home
                    .join(".local/state/kanade-client/logs/client.log")
            );
            assert_eq!(p.cache, user(1001).home.join(".cache/kanade-client"));
            assert_eq!(
                p.window_size,
                user(1001)
                    .home
                    .join(".local/state/kanade-client/window-size.json")
            );
        }
    }

    #[test]
    fn absolute_xdg_and_user_separation() {
        let xdg = XdgOverrides {
            config: Some("/private/config".into()),
            state: Some("/private/state".into()),
            cache: Some("/private/cache".into()),
        };
        let p = ClientPaths::resolve(UnixPlatform::Linux, &user(1001), &xdg).unwrap();
        assert_eq!(
            p.config,
            PathBuf::from("/private/config/kanade-client/client.toml")
        );
        assert_eq!(
            p.log,
            PathBuf::from("/private/state/kanade-client/logs/client.log")
        );
        assert_eq!(
            p.window_size,
            PathBuf::from("/private/state/kanade-client/window-size.json")
        );
        assert_eq!(p.cache, PathBuf::from("/private/cache/kanade-client"));
        for platform in [UnixPlatform::Linux, UnixPlatform::MacOs] {
            let a = ClientPaths::resolve(platform, &user(1001), &Default::default()).unwrap();
            let b = ClientPaths::resolve(platform, &user(1002), &Default::default()).unwrap();
            assert_ne!(a.uid, b.uid);
            assert_ne!(a.config, b.config);
            assert_ne!(a.log, b.log);
            assert_ne!(a.cache, b.cache);
            assert_ne!(a.window_size, b.window_size);
        }
    }

    #[test]
    fn mac_library_layout_ignores_xdg_and_rejects_relative_home() {
        let xdg = XdgOverrides {
            config: Some("/ignored/config".into()),
            state: Some("/ignored/state".into()),
            cache: Some("/ignored/cache".into()),
        };
        let p = ClientPaths::resolve(UnixPlatform::MacOs, &user(501), &xdg).unwrap();
        assert_eq!(
            p.config,
            user(501)
                .home
                .join("Library/Application Support/Kanade/Client/client.toml")
        );
        assert_eq!(
            p.window_size,
            user(501)
                .home
                .join("Library/Application Support/Kanade/Client/window-size.json")
        );
        assert_eq!(
            p.log,
            user(501).home.join("Library/Logs/Kanade/Client/client.log")
        );
        assert_eq!(p.cache, user(501).home.join("Library/Caches/Kanade/Client"));
        assert_eq!(
            ClientPaths::resolve(
                UnixPlatform::Linux,
                &UserAccount {
                    uid: 1,
                    home: "relative".into()
                },
                &Default::default()
            ),
            Err(PathError::InvalidHome)
        );
    }
}

#[cfg(test)]
mod isolation_tests {
    use super::*;

    #[test]
    fn shared_xdg_paths_do_not_authorize_another_user_or_unknown_storage() {
        let xdg = XdgOverrides {
            config: Some("/shared".into()),
            state: Some("/shared".into()),
            cache: Some("/shared".into()),
        };
        let a = ClientPaths::resolve(
            UnixPlatform::Linux,
            &UserAccount {
                uid: 1001,
                home: "/users/a".into(),
            },
            &xdg,
        )
        .unwrap();
        let b = ClientPaths::resolve(
            UnixPlatform::Linux,
            &UserAccount {
                uid: 1002,
                home: "/users/b".into(),
            },
            &xdg,
        )
        .unwrap();
        assert_eq!(a.config, b.config);
        let mut facts = PrivateStorageFacts {
            effective_uid: Some(1001),
            owner_uid: Some(1001),
            permissions: Some(0o600),
            no_symlinks_verified: true,
        };
        assert!(private_storage_access(&a, PrivateEntry::File, &facts));
        facts.permissions = Some(0o700);
        assert!(private_storage_access(&a, PrivateEntry::Directory, &facts));
        facts.permissions = Some(0o755);
        assert!(!private_storage_access(&a, PrivateEntry::Directory, &facts));
        facts.permissions = Some(0o600);
        assert!(!private_storage_access(&b, PrivateEntry::File, &facts));
        facts.effective_uid = Some(1002);
        assert!(!private_storage_access(&b, PrivateEntry::File, &facts));
        facts.effective_uid = Some(1001);
        facts.no_symlinks_verified = false;
        assert!(!private_storage_access(&a, PrivateEntry::File, &facts));
        facts.no_symlinks_verified = true;
        facts.permissions = Some(0o644);
        assert!(!private_storage_access(&a, PrivateEntry::File, &facts));
        assert!(!private_storage_access(
            &a,
            PrivateEntry::File,
            &Default::default()
        ));
    }
}

#[cfg(all(test, unix))]
mod unix_path_tests {
    use super::*;
    use std::os::unix::ffi::OsStringExt;

    #[test]
    fn non_utf8_xdg_is_preserved_without_lossy_conversion() {
        let override_path = OsString::from_vec(b"/private/\xff".to_vec());
        let xdg = XdgOverrides {
            state: Some(override_path.clone()),
            ..Default::default()
        };
        let account = UserAccount {
            uid: 1001,
            home: "/users/利用者".into(),
        };
        let paths = ClientPaths::resolve(UnixPlatform::Linux, &account, &xdg).unwrap();
        assert_eq!(
            paths.window_size,
            PathBuf::from(override_path).join("kanade-client/window-size.json")
        );
    }

    #[test]
    fn cache_removal_does_not_remove_persistent_size() {
        let home = std::env::temp_dir().join(format!("kanade-paths-{}", std::process::id()));
        let account = UserAccount {
            uid: 1001,
            home: home.clone(),
        };
        let paths =
            ClientPaths::resolve(UnixPlatform::Linux, &account, &Default::default()).unwrap();
        // Exercise existing size logic, not a replacement Unix store. A real
        // adapter must validate private ownership/modes before these operations.
        crate::window_size::save(
            &paths.window_size,
            900.0,
            650.0,
            crate::window_size::DEFAULT_MIN,
            None,
        )
        .unwrap();
        std::fs::create_dir_all(&paths.cache).unwrap();
        std::fs::remove_dir_all(&paths.cache).unwrap();
        assert!(
            crate::window_size::load(&paths.window_size, crate::window_size::DEFAULT_MIN, None)
                .is_some()
        );
        std::fs::remove_dir_all(home).unwrap();
    }
}
