//! The SMTP [`Mailer`]: one connection per email through `lettre`, over tokio and rustls. No
//! connection pool: notification email is rare enough that a connection per mail is simpler
//! than a pool to keep healthy, and a configuration change (`SmtpMailer::set`) needs nothing
//! drained.
//!
//! A relay reached without TLS and without credentials that refuses `EHLO` (`500`/`502`) gets
//! the email again over plain `HELO` ([`super::helo`]), as RFC 5321 section 4.1.4 says a client
//! should: an old relay or a minimal mail catcher (Sytest's) speaks nothing else. TLS needs
//! `EHLO` (STARTTLS is an extension) and so do credentials (`AUTH` is one), so those settings
//! never fall back.

use std::sync::{Arc, PoisonError, RwLock};
use std::time::Duration;

use lettre::message::{Mailbox, MultiPart};
use lettre::transport::smtp::authentication::Credentials;
use lettre::transport::smtp::client::{Tls, TlsParameters};
use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};

use super::{MailError, Mailer, OutboundMail};

/// How a mail connection is secured. The same choice as `hs_config::email::SmtpSecurity`,
/// restated here so this crate does not depend on `hs-config`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Security {
    /// Plain connection upgraded with STARTTLS, which the server must offer.
    Starttls,
    /// TLS from the first byte.
    Tls,
    /// No encryption.
    None,
}

/// The SMTP server to send through.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SmtpSettings {
    /// Host name or address.
    pub host: String,
    /// Port.
    pub port: u16,
    /// How the connection is secured.
    pub security: Security,
    /// Credentials, if the server wants them.
    pub credentials: Option<(String, String)>,
    /// The name the server's certificate is checked against, when not `host`.
    pub tls_name: Option<String>,
    /// How long a connection, command or send may take.
    pub timeout: Duration,
}

/// A [`Mailer`] over SMTP whose settings can be replaced while it runs.
pub struct SmtpMailer {
    settings: RwLock<Option<Arc<SmtpSettings>>>,
}

impl Default for SmtpMailer {
    fn default() -> Self {
        Self::new(None)
    }
}

impl SmtpMailer {
    /// A mailer with `settings` in force (`None`: nothing can be sent until [`Self::set`]).
    #[must_use]
    pub fn new(settings: Option<SmtpSettings>) -> Self {
        Self {
            settings: RwLock::new(settings.map(Arc::new)),
        }
    }

    /// Replaces the settings for the next email; one in flight finishes on the old ones.
    pub fn set(&self, settings: Option<SmtpSettings>) {
        *self
            .settings
            .write()
            .unwrap_or_else(PoisonError::into_inner) = settings.map(Arc::new);
    }

    fn current(&self) -> Option<Arc<SmtpSettings>> {
        self.settings
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn transport(settings: &SmtpSettings) -> Result<AsyncSmtpTransport<Tokio1Executor>, MailError> {
        let mut builder = AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(&settings.host)
            .port(settings.port)
            .timeout(Some(settings.timeout));
        let tls_name = settings
            .tls_name
            .clone()
            .unwrap_or_else(|| settings.host.clone());
        let tls = match settings.security {
            Security::None => Tls::None,
            Security::Starttls => Tls::Required(
                TlsParameters::new(tls_name).map_err(|e| MailError::Transport(e.to_string()))?,
            ),
            Security::Tls => Tls::Wrapper(
                TlsParameters::new(tls_name).map_err(|e| MailError::Transport(e.to_string()))?,
            ),
        };
        builder = builder.tls(tls);
        if let Some((user, pass)) = &settings.credentials {
            builder = builder.credentials(Credentials::new(user.clone(), pass.clone()));
        }
        Ok(builder.build())
    }
}

/// Builds the MIME message: `From`, `To`, `Subject`, and a `multipart/alternative` body with
/// the text and HTML parts.
///
/// # Errors
/// [`MailError::Address`] if the sender or recipient does not parse as a mailbox.
pub fn build_message(mail: &OutboundMail) -> Result<Message, MailError> {
    let from: Mailbox = mail
        .from
        .parse()
        .map_err(|e| MailError::Address(format!("sender {:?}: {e}", mail.from)))?;
    let to: Mailbox = mail
        .to
        .parse()
        .map_err(|e| MailError::Address(format!("recipient: {e}")))?;
    Message::builder()
        .from(from)
        .to(to)
        .subject(mail.subject.clone())
        .multipart(MultiPart::alternative_plain_html(
            mail.text.clone(),
            mail.html.clone(),
        ))
        .map_err(|e| MailError::Transport(format!("building the message: {e}")))
}

/// Whether `address` is a well-formed email address (`local@domain`), as a pusher's pushkey
/// must be.
#[must_use]
pub fn is_valid_address(address: &str) -> bool {
    address.parse::<lettre::Address>().is_ok()
}

#[async_trait::async_trait]
impl Mailer for SmtpMailer {
    fn is_configured(&self) -> bool {
        self.current().is_some()
    }

    async fn send(&self, mail: &OutboundMail) -> Result<(), MailError> {
        let settings = self.current().ok_or(MailError::NotConfigured)?;
        let message = build_message(mail)?;
        let transport = Self::transport(&settings)?;
        let error = match transport.send(message.clone()).await {
            Ok(_) => return Ok(()),
            Err(error) => error.to_string(),
        };
        if !may_fall_back_to_helo(&settings, &error) {
            return Err(MailError::Transport(error));
        }
        tracing::info!(
            %error,
            host = %settings.host,
            port = settings.port,
            "the SMTP server refused EHLO; sending with HELO"
        );
        super::helo::send(
            &settings.host,
            settings.port,
            &mail.from,
            &mail.to,
            &message.formatted(),
        )
        .await
        .map_err(MailError::Transport)
    }
}

/// Whether a send that failed with `error` should be retried over plain `HELO`: the server
/// refused `EHLO` itself (`500` or `502`, which `lettre` reports as `(500)`/`(502)`), on a
/// connection without TLS and without credentials. See the module docs.
fn may_fall_back_to_helo(settings: &SmtpSettings, error: &str) -> bool {
    settings.security == Security::None
        && settings.credentials.is_none()
        && ["(500)", "(502)"].iter().any(|code| error.contains(code))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mail() -> OutboundMail {
        OutboundMail {
            from: "Myelin <noreply@example.org>".to_owned(),
            to: "alice@example.org".to_owned(),
            subject: "[Myelin] hi".to_owned(),
            text: "hello".to_owned(),
            html: "<p>hello</p>".to_owned(),
        }
    }

    #[test]
    fn the_message_carries_both_parts_and_the_headers() {
        let message = build_message(&mail()).unwrap();
        let raw = String::from_utf8(message.formatted()).unwrap();
        assert!(
            raw.contains("From: ")
                && raw.contains("Myelin")
                && raw.contains("<noreply@example.org>")
        );
        assert!(raw.contains("To: alice@example.org"));
        assert!(raw.contains("Subject: [Myelin] hi"));
        assert!(raw.contains("multipart/alternative"));
        assert!(raw.contains("text/plain"));
        assert!(raw.contains("text/html"));
    }

    #[test]
    fn addresses_are_checked() {
        assert!(is_valid_address("alice@example.org"));
        assert!(!is_valid_address("alice"));
        assert!(!is_valid_address("@example.org"));
        let mut bad = mail();
        bad.to = "not an address".to_owned();
        assert!(matches!(build_message(&bad), Err(MailError::Address(_))));
    }

    #[tokio::test]
    async fn an_unconfigured_mailer_refuses_and_says_so() {
        let mailer = SmtpMailer::default();
        assert!(!mailer.is_configured());
        assert!(matches!(
            mailer.send(&mail()).await,
            Err(MailError::NotConfigured)
        ));
    }

    /// A server that knows only `HELO` (Sytest's mail server) refuses `EHLO` with `500`; the
    /// mailer sends the email again with `HELO`, and it arrives.
    #[tokio::test]
    async fn a_relay_that_refuses_ehlo_gets_the_email_over_helo() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let mut data = String::new();
            // `lettre`'s attempt, then the `HELO` one.
            for _ in 0..2 {
                let (stream, _) = listener.accept().await.unwrap();
                let (read, mut write) = stream.into_split();
                let mut read = BufReader::new(read);
                write.write_all(b"220 hi\r\n").await.unwrap();
                let mut in_data = false;
                loop {
                    let mut line = String::new();
                    if read.read_line(&mut line).await.unwrap_or(0) == 0 {
                        break;
                    }
                    if in_data {
                        if line == ".\r\n" {
                            in_data = false;
                            write.write_all(b"250 ok\r\n").await.unwrap();
                        } else {
                            data.push_str(&line);
                        }
                        continue;
                    }
                    let reply: &[u8] = match line.split_whitespace().next().unwrap_or("") {
                        "HELO" | "MAIL" | "RCPT" => b"250 ok\r\n",
                        "DATA" => {
                            in_data = true;
                            b"354 go\r\n"
                        }
                        "QUIT" => break,
                        _ => b"500 Syntax error: unrecognized command\r\n",
                    };
                    if write.write_all(reply).await.is_err() {
                        break;
                    }
                }
            }
            data
        });
        let mut settings = SmtpSettings {
            host: "127.0.0.1".to_owned(),
            port,
            security: Security::None,
            credentials: None,
            tls_name: None,
            timeout: Duration::from_secs(10),
        };
        let mailer = SmtpMailer::new(Some(settings.clone()));
        mailer.send(&mail()).await.unwrap();
        let data = server.await.unwrap();
        assert!(data.contains("Subject: [Myelin] hi"), "{data}");
        assert!(data.contains("hello"), "{data}");

        // Only without TLS and without credentials.
        let refused = "permanent error (500): Syntax error";
        assert!(may_fall_back_to_helo(&settings, refused));
        assert!(!may_fall_back_to_helo(
            &settings,
            "permanent error (550): no"
        ));
        settings.credentials = Some(("u".to_owned(), "p".to_owned()));
        assert!(!may_fall_back_to_helo(&settings, refused));
        settings.credentials = None;
        settings.security = Security::Starttls;
        assert!(!may_fall_back_to_helo(&settings, refused));
    }

    #[tokio::test]
    async fn a_server_that_is_not_there_is_a_transport_error() {
        // A port nothing listens on: bound and dropped, so it is free.
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let mailer = SmtpMailer::new(Some(SmtpSettings {
            host: "127.0.0.1".to_owned(),
            port,
            security: Security::None,
            credentials: None,
            tls_name: None,
            timeout: Duration::from_secs(2),
        }));
        assert!(mailer.is_configured());
        assert!(matches!(
            mailer.send(&mail()).await,
            Err(MailError::Transport(_))
        ));
        mailer.set(None);
        assert!(!mailer.is_configured());
    }
}
