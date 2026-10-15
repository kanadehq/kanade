//! Startup decisions, independent of OS activation and notification delivery.
//! Parsers retain the existing Windows URI compatibility and argument semantics.

const SHOW_NOTIFICATION_FLAG: &str = "--show-notification";
const RESURFACE_FLAG: &str = "--resurface";
const PROTOCOL_SCHEME_PREFIX: &str = "kanade-client://";

/// Parse `--show-notification <id>` out of an arbitrary arg sequence.
/// Returns the id when the flag is present with a non-empty value.
/// Shared by the startup parse (`std::env::args`) and the single-instance
/// handler (which gets the *second* process's argv, #624).
pub fn parse_show_notification<I: IntoIterator<Item = String>>(args: I) -> Option<String> {
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        if a == SHOW_NOTIFICATION_FLAG {
            return it.next().filter(|s| !s.is_empty());
        }
    }
    None
}

/// Whether `--resurface` is present in an arg sequence (#647). Shared by the
/// startup parse (`std::env::args`) and the single-instance handler.
pub fn has_resurface_flag<I: IntoIterator<Item = String>>(args: I) -> bool {
    args.into_iter().any(|a| a == RESURFACE_FLAG)
}

/// Parse the notification id out of a `kanade-client://show?id=<id>` protocol
/// argument (#647 toast-click). The protocol handler passes the whole URI as
/// one arg, but Windows canonicalises it first, so the exact shape is NOT
/// guaranteed: a launch may insert a slash after the authority
/// (`kanade-client://show/?id=<id>`) or append a trailing slash/fragment. So
/// match on the scheme + the `id` query param wherever it lands rather than a
/// fixed `…show?id=` prefix (the old strict prefix missed the slash-inserted
/// form, so the window opened but never navigated to the notification). Take
/// just the id token (`[A-Za-z0-9_.-]`, the validated notification-id
/// charset). Shared by the startup parse and the single-instance handler.
pub fn parse_protocol_show<S: AsRef<str>, I: IntoIterator<Item = S>>(args: I) -> Option<String> {
    args.into_iter().find_map(|a| {
        let s = a.as_ref();
        // Only our own scheme — Windows lowercases the scheme on protocol
        // activation, so match case-insensitively. Compare the scheme-length
        // head in place (no `to_ascii_lowercase` allocation); guard the slice
        // with `is_char_boundary` so a multi-byte first char can't panic.
        let prefix_len = PROTOCOL_SCHEME_PREFIX.len();
        if s.len() < prefix_len
            || !s.is_char_boundary(prefix_len)
            || !s[..prefix_len].eq_ignore_ascii_case(PROTOCOL_SCHEME_PREFIX)
        {
            return None;
        }
        // Validate the route is `show` (allowing the Windows-canonicalised
        // `show/` trailing slash) BEFORE reading the query, so a malformed
        // `kanade-client://?id=` or a future same-scheme route
        // (`kanade-client://other?id=`) can't spuriously focus a notification.
        // Splitting on '?' collapses both the `…show?id=` and the
        // slash-inserted `…show/?id=` forms to the same `id=<id>[&…]` query.
        let rest = &s[prefix_len..];
        let (target, query) = rest.split_once('?')?;
        if target.trim_end_matches('/') != "show" {
            return None;
        }
        query.split('&').find_map(|pair| {
            let v = pair.strip_prefix("id=")?;
            let id: String = v
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
                .collect();
            (!id.is_empty()).then_some(id)
        })
    })
}

/// Parsed activation facts. Parsing and OS URL-open events remain adapter work.
#[derive(Debug, Default, Clone, Copy)]
pub struct StartupIntent {
    pub tray: bool,
    pub notification: bool,
    pub resurface: bool,
    pub deep_link: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub enum ExistingInstanceAction {
    Resurface,
    FocusNotification,
    DeliverNotification,
    KeepVisibility,
    Show,
}

impl StartupIntent {
    /// Unix argv policy; Windows wiring deliberately retains its legacy tray
    /// handling. URL-open adapters may supply deep_link separately.
    pub fn from_args(args: &[String]) -> Self {
        Self {
            tray: args.iter().any(|arg| arg == "--tray"),
            notification: parse_show_notification(args.iter().cloned()).is_some(),
            resurface: has_resurface_flag(args.iter().cloned()),
            deep_link: parse_protocol_show(args).is_some(),
        }
    }

    /// Cold startup preserves the current emergency-over-focus precedence.
    /// Visibility is never restored from disk, and tray absence does not reveal UI.
    pub fn initially_visible(self) -> bool {
        !(self.tray || self.notification || self.resurface)
    }

    /// Warm startup preserves resurface > click > emergency precedence.
    /// A repeated tray launch leaves existing visibility unchanged.
    pub fn existing_instance_action(self) -> ExistingInstanceAction {
        if self.resurface {
            ExistingInstanceAction::Resurface
        } else if self.deep_link {
            ExistingInstanceAction::FocusNotification
        } else if self.notification {
            ExistingInstanceAction::DeliverNotification
        } else if self.tray {
            ExistingInstanceAction::KeepVisibility
        } else {
            ExistingInstanceAction::Show
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_protocol_show_plain_form() {
        assert_eq!(
            parse_protocol_show(["kanade-client://show?id=abc123"]),
            Some("abc123".to_string()),
        );
    }

    #[test]
    fn parse_protocol_show_slash_inserted_form() {
        // Windows protocol activation inserts a slash after the authority when
        // there is no path — the regression behind "window opens but the
        // notification screen doesn't" (#647). Must still parse.
        assert_eq!(
            parse_protocol_show(["kanade-client://show/?id=abc123"]),
            Some("abc123".to_string()),
        );
    }

    #[test]
    fn parse_protocol_show_trailing_slash_after_id() {
        assert_eq!(
            parse_protocol_show(["kanade-client://show?id=abc123/"]),
            Some("abc123".to_string()),
        );
    }

    #[test]
    fn parse_protocol_show_id_charset() {
        assert_eq!(
            parse_protocol_show(["kanade-client://show?id=n_1.2-3"]),
            Some("n_1.2-3".to_string()),
        );
    }

    #[test]
    fn parse_protocol_show_finds_uri_among_other_args() {
        assert_eq!(
            parse_protocol_show(["C:\\path\\kanade-client.exe", "kanade-client://show?id=xyz",]),
            Some("xyz".to_string()),
        );
    }

    #[test]
    fn parse_protocol_show_rejects_non_protocol_args() {
        assert_eq!(parse_protocol_show(["--show-notification", "abc"]), None,);
        assert_eq!(parse_protocol_show(["kanade-client.exe"]), None);
    }

    #[test]
    fn parse_protocol_show_rejects_missing_or_empty_id() {
        assert_eq!(parse_protocol_show(["kanade-client://show"]), None);
        assert_eq!(parse_protocol_show(["kanade-client://show?id="]), None);
        // A different param without `id` must not match.
        assert_eq!(parse_protocol_show(["kanade-client://show?foo=bar"]), None);
    }

    #[test]
    fn parse_protocol_show_does_not_match_uid_substring() {
        // `id=` must be a query-param key, not a substring of `uid=`.
        assert_eq!(parse_protocol_show(["kanade-client://show?uid=abc"]), None);
    }

    #[test]
    fn parse_protocol_show_requires_show_route() {
        // Right scheme + an `id=` param, but NOT the `show` route — must not
        // focus a notification (malformed / future same-scheme routes).
        assert_eq!(parse_protocol_show(["kanade-client://?id=abc"]), None);
        assert_eq!(parse_protocol_show(["kanade-client://other?id=abc"]), None);
        assert_eq!(parse_protocol_show(["kanade-client://other/?id=abc"]), None);
        // The canonicalised `show/` form is still the show route.
        assert_eq!(
            parse_protocol_show(["kanade-client://show/?id=abc"]),
            Some("abc".to_string()),
        );
    }
}

#[cfg(test)]
mod policy_tests {
    use super::*;

    #[test]
    fn manual_and_click_are_visible_automatic_launches_are_hidden() {
        for intent in [
            StartupIntent::default(),
            StartupIntent {
                deep_link: true,
                ..Default::default()
            },
        ] {
            assert!(intent.initially_visible());
        }
        for intent in [
            StartupIntent {
                tray: true,
                ..Default::default()
            },
            StartupIntent {
                notification: true,
                ..Default::default()
            },
            StartupIntent {
                resurface: true,
                ..Default::default()
            },
        ] {
            assert!(!intent.initially_visible());
        }
        assert_eq!(
            StartupIntent {
                tray: true,
                ..Default::default()
            }
            .existing_instance_action(),
            ExistingInstanceAction::KeepVisibility
        );
    }

    #[test]
    fn windows_mixed_argument_precedence_is_preserved() {
        let mut intent = StartupIntent {
            notification: true,
            deep_link: true,
            ..Default::default()
        };
        assert!(!intent.initially_visible());
        assert_eq!(
            intent.existing_instance_action(),
            ExistingInstanceAction::FocusNotification
        );
        intent.resurface = true;
        assert_eq!(
            intent.existing_instance_action(),
            ExistingInstanceAction::Resurface
        );
        assert_eq!(
            parse_show_notification(["--show-notification", ""].map(String::from)),
            None
        );
        assert_eq!(
            parse_show_notification(["--show-notification", "id"].map(String::from)),
            Some("id".into())
        );
        assert!(has_resurface_flag(["--resurface"].map(String::from)));
    }
}

#[cfg(test)]
mod argv_tests {
    use super::*;

    #[test]
    fn tray_is_hidden_on_first_and_repeated_launch() {
        let argv = ["kanade-client", "--tray"].map(String::from);
        let intent = StartupIntent::from_args(&argv);
        assert!(!intent.initially_visible());
        assert_eq!(
            intent.existing_instance_action(),
            ExistingInstanceAction::KeepVisibility
        );
        for (argv, action) in [
            (
                vec!["--show-notification", "id"],
                ExistingInstanceAction::DeliverNotification,
            ),
            (vec!["--resurface"], ExistingInstanceAction::Resurface),
            (
                vec!["kanade-client://show?id=id"],
                ExistingInstanceAction::FocusNotification,
            ),
            (vec!["kanade-client"], ExistingInstanceAction::Show),
        ] {
            let argv: Vec<_> = argv.into_iter().map(String::from).collect();
            assert_eq!(
                StartupIntent::from_args(&argv).existing_instance_action(),
                action
            );
        }
    }
}
