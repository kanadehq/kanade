//! Generic outbound email over an internal SMTP relay.
//!
//! A deliberately thin transport: [`Mailer::send`] takes recipients, a
//! subject, and a plaintext body — nothing about compliance, groups, or
//! the alert that triggered it. The first caller is the compliance-alert
//! projector (which resolves `notify_groups` → addresses via the
//! `group_contacts` KV before calling `send`), but the same `Mailer`
//! lives on [`crate::api::AppState`] so any future feature (e.g.
//! account-creation emails) can reuse it without re-plumbing SMTP.
//!
//! Built from the optional `[mail]` config section ([`MailSection`]).
//! When that section is absent the backend never constructs a `Mailer`
//! and email is a no-op — the in-app / NATS notification path is
//! unaffected. The SMTP password is NOT read here: the caller resolves it
//! from the `MailPassword` registry secret (or `$KANADE_MAIL_PASSWORD`)
//! and passes it in, so this module knows nothing about the registry.

use std::time::Duration;

use anyhow::{Context, Result};
use kanade_shared::config::{MailEncryption, MailSection};
use lettre::message::Mailbox;
use lettre::transport::smtp::AsyncSmtpTransport;
use lettre::transport::smtp::authentication::Credentials;
use lettre::{AsyncTransport, Message, Tokio1Executor};
use tracing::warn;

/// Per-operation socket timeout for every SMTP send. Bounds how long a
/// single send can block on a slow / unreachable relay — important because
/// the compliance-alert path spawns sends fire-and-forget, so without this
/// a dead relay would leave tasks hanging (and accumulating under a burst
/// of alerts) for the OS-default TCP timeout. Applies to all callers.
const SMTP_TIMEOUT: Duration = Duration::from_secs(30);

/// A built, ready-to-use SMTP relay client. `Clone` is cheap — the inner
/// `AsyncSmtpTransport` is connection-pooled + `Arc`-backed and `Mailbox`
/// is small — so callers can clone one into a spawned task (the
/// compliance-alert path sends email off the projector thread).
#[derive(Clone)]
pub struct Mailer {
    transport: AsyncSmtpTransport<Tokio1Executor>,
    from: Mailbox,
}

impl Mailer {
    /// Build a relay client from the non-secret `[mail]` config plus the
    /// separately-resolved SMTP password (registry / env). `password` is
    /// only applied when `cfg.username` is also set; an internal relay
    /// with neither runs unauthenticated.
    pub fn from_config(cfg: &MailSection, password: Option<String>) -> Result<Self> {
        let from: Mailbox = cfg
            .from
            .parse()
            .with_context(|| format!("invalid [mail] from address: {:?}", cfg.from))?;

        let mut builder = match cfg.encryption {
            MailEncryption::Starttls => {
                AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&cfg.host)
                    .with_context(|| format!("STARTTLS relay setup for {:?}", cfg.host))?
            }
            MailEncryption::Tls => AsyncSmtpTransport::<Tokio1Executor>::relay(&cfg.host)
                .with_context(|| format!("implicit-TLS relay setup for {:?}", cfg.host))?,
            // Plaintext: a trusted internal segment / a local mail catcher.
            MailEncryption::None => {
                AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(&cfg.host)
            }
        }
        .port(cfg.port)
        .timeout(Some(SMTP_TIMEOUT));

        match (cfg.username.as_deref(), password) {
            (Some(user), Some(pass)) if !user.is_empty() => {
                builder = builder.credentials(Credentials::new(user.to_owned(), pass));
            }
            (Some(user), None) if !user.is_empty() => {
                warn!(
                    user,
                    "[mail] username is set but no MailPassword secret / \
                     $KANADE_MAIL_PASSWORD — sending unauthenticated"
                );
            }
            _ => {}
        }

        Ok(Self {
            transport: builder.build(),
            from,
        })
    }

    /// Send the same plaintext email to each address in `to`, as **one
    /// message per recipient** — not a single message with everyone on the
    /// `To:` line. That keeps a group's contact list from being disclosed
    /// to every recipient (compliance contacts can be a sensitive set) and
    /// keeps one declined address from sinking the rest. Addresses that
    /// fail to parse are skipped with a warning; `Ok` as long as at least
    /// one recipient was delivered, `Err` only when none were (no valid
    /// addresses, or every send failed).
    pub async fn send(&self, to: &[String], subject: &str, body: &str) -> Result<()> {
        let mailboxes: Vec<Mailbox> = to
            .iter()
            .filter_map(|addr| match addr.parse::<Mailbox>() {
                Ok(mb) => Some(mb),
                Err(e) => {
                    warn!(addr, error = %e, "skipping invalid email recipient");
                    None
                }
            })
            .collect();
        if mailboxes.is_empty() {
            anyhow::bail!("no valid recipients among {} address(es)", to.len());
        }

        let mut delivered = 0usize;
        let mut last_err = None;
        for mb in mailboxes {
            let email = Message::builder()
                .from(self.from.clone())
                .to(mb.clone())
                .subject(subject)
                .body(body.to_owned())
                .context("build email message")?;
            match self.transport.send(email).await {
                Ok(_) => delivered += 1,
                Err(e) => {
                    warn!(to = %mb, error = %e, "SMTP send to one recipient failed");
                    last_err = Some(e);
                }
            }
        }
        if delivered == 0 {
            let e = last_err.expect("non-empty recipients ⇒ at least one attempt");
            return Err(anyhow::Error::new(e).context("all SMTP sends failed"));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(encryption: MailEncryption) -> MailSection {
        MailSection {
            host: "smtp.example.com".into(),
            port: 587,
            encryption,
            from: "kanade-noreply@example.com".into(),
            username: None,
        }
    }

    // `from_config` builds a pooled transport, whose `.build()` grabs the
    // current Tokio runtime handle — so these run under `#[tokio::test]`
    // even though they don't await (production calls it from main's async
    // context).
    #[tokio::test]
    async fn builds_for_each_encryption() {
        for enc in [
            MailEncryption::Starttls,
            MailEncryption::Tls,
            MailEncryption::None,
        ] {
            assert!(
                Mailer::from_config(&cfg(enc), None).is_ok(),
                "from_config should build for {enc:?}"
            );
        }
    }

    #[tokio::test]
    async fn builds_with_credentials() {
        let mut c = cfg(MailEncryption::Starttls);
        c.username = Some("kanade-noreply".into());
        assert!(Mailer::from_config(&c, Some("hunter2".into())).is_ok());
    }

    #[tokio::test]
    async fn rejects_a_malformed_from_address() {
        let mut c = cfg(MailEncryption::Starttls);
        c.from = "not an email".into();
        assert!(Mailer::from_config(&c, None).is_err());
    }
}
