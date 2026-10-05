//! [`hs_auth::threepid::EmailSender`] over the SMTP mailer email pushers use: the validation
//! emails `POST /register/email/requestToken` and its siblings send go through the same
//! `email.smtp` server, from the same `email.from` sender, and a change to the `email` section
//! re-points both at once ([`ThreepidEmailSender::set`], called from `hs serve`'s live
//! configuration hook beside the push mailer's).

use std::sync::{Arc, PoisonError, RwLock};

use async_trait::async_trait;
use hs_push::email::Mailer;

/// The sender and app name an email goes out with, from the `email` section.
#[derive(Debug, Clone, Default)]
struct Settings {
    from: Option<String>,
    app_name: String,
    /// The relay, when it is reached without TLS: the one case [`crate::smtp_helo`] can fall
    /// back to plain `HELO` for.
    plain_relay: Option<(String, u16)>,
}

/// Sends validation emails through `hs serve`'s SMTP mailer. See the module docs.
pub struct ThreepidEmailSender {
    mailer: Arc<dyn Mailer>,
    settings: RwLock<Settings>,
}

impl ThreepidEmailSender {
    /// A sender over `mailer`, with `config`'s `email.from` and `email.app_name`.
    #[must_use]
    pub fn new(mailer: Arc<dyn Mailer>, config: &hs_config::EmailConfig) -> Self {
        let sender = Self {
            mailer,
            settings: RwLock::new(Settings::default()),
        };
        sender.set(config);
        sender
    }

    /// Takes a changed `email` section's sender and app name; the mailer itself is re-pointed
    /// by its own owner.
    pub fn set(&self, config: &hs_config::EmailConfig) {
        *self
            .settings
            .write()
            .unwrap_or_else(PoisonError::into_inner) = Settings {
            from: config.from.clone().filter(|f| !f.trim().is_empty()),
            app_name: config.app_name.clone(),
            plain_relay: match (&config.smtp.host, config.smtp.security) {
                (Some(host), hs_config::email::SmtpSecurity::None) => {
                    Some((host.clone(), config.smtp.port))
                }
                _ => None,
            },
        };
    }

    fn settings(&self) -> Settings {
        self.settings
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

#[async_trait]
impl hs_auth::threepid::EmailSender for ThreepidEmailSender {
    fn can_send(&self) -> bool {
        self.settings().from.is_some() && self.mailer.is_configured()
    }

    fn app_name(&self) -> String {
        self.settings().app_name
    }

    async fn send(&self, email: hs_auth::threepid::OutgoingEmail) -> Result<(), String> {
        let settings = self.settings();
        let Some(from) = settings.from else {
            return Err("email.from is not set".to_owned());
        };
        let mail = hs_push::email::OutboundMail {
            from,
            to: email.to,
            subject: email.subject,
            text: email.text,
            html: email.html,
        };
        match self.mailer.send(&mail).await {
            Ok(()) => Ok(()),
            // A relay without TLS that refused the opening `EHLO` (`500`/`502`): RFC 5321's
            // fall-back to `HELO` (`crate::smtp_helo`).
            Err(error)
                if settings.plain_relay.is_some()
                    && ["(500)", "(502)"]
                        .iter()
                        .any(|c| error.to_string().contains(c)) =>
            {
                let (host, port) = settings.plain_relay.unwrap_or_default();
                tracing::info!(%error, host, port, "the SMTP server refused EHLO; sending with HELO");
                let message = hs_push::email::smtp::build_message(&mail)
                    .map_err(|e| e.to_string())?
                    .formatted();
                crate::smtp_helo::send(&host, port, &mail.from, &mail.to, &message).await
            }
            Err(error) => Err(error.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hs_auth::threepid::{EmailSender, OutgoingEmail};
    use std::sync::Mutex;

    #[derive(Default)]
    struct Recording(Mutex<Vec<hs_push::email::OutboundMail>>);

    #[async_trait]
    impl Mailer for Recording {
        fn is_configured(&self) -> bool {
            true
        }
        async fn send(
            &self,
            mail: &hs_push::email::OutboundMail,
        ) -> Result<(), hs_push::email::MailError> {
            self.0.lock().unwrap().push(mail.clone());
            Ok(())
        }
    }

    #[tokio::test]
    async fn an_email_goes_out_from_the_configured_sender_and_follows_a_change() {
        let mailer = Arc::new(Recording::default());
        let mut config = hs_config::EmailConfig::default();
        let sender = ThreepidEmailSender::new(mailer.clone(), &config);
        assert!(!sender.can_send(), "no email.from yet");
        config.from = Some("Matrix <noreply@example.org>".into());
        config.app_name = "Example".into();
        sender.set(&config);
        assert!(sender.can_send());
        assert_eq!(sender.app_name(), "Example");
        sender
            .send(OutgoingEmail {
                to: "bob@example.com".into(),
                subject: "s".into(),
                text: "t".into(),
                html: "h".into(),
            })
            .await
            .unwrap();
        let sent = mailer.0.lock().unwrap();
        assert_eq!(sent[0].from, "Matrix <noreply@example.org>");
        assert_eq!(sent[0].to, "bob@example.com");
    }
}
