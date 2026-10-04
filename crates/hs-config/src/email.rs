//! Outbound email: the SMTP server this server sends through, and the notification emails that
//! email pushers receive. Every setting here is read per mail (`hs_push::email`), so a change
//! applies to the running server at once.
//!
//! Corresponds to Synapse's `email` block. The keys keep Synapse's names where the meaning is
//! the same (`app_name`, `client_base_url`, `subjects` and its placeholders), so a Synapse
//! operator's values carry over; `smtp_host`/`smtp_port`/`smtp_user`/`smtp_pass` live under
//! `smtp`, and Synapse's three TLS booleans (`enable_tls`, `require_transport_security`,
//! `force_tls`) are one `security` choice.

use std::path::PathBuf;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::error::{Validate, ValidationErrors};
use crate::secret::{SecretString, resolve_secret_pair};
use crate::{ConfigError, Duration};

/// Outbound email and notification emails.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EmailConfig {
    /// The SMTP server mail is sent through. Until `smtp.host` is set, no email is sent:
    /// email pushers are stored and nothing is delivered to them.
    #[serde(default)]
    pub smtp: SmtpConfig,
    /// The sender of every email, as `Name <address>` or a bare address, for example
    /// `Matrix <noreply@example.org>`. Required once `smtp.host` is set. Corresponds to
    /// Synapse's `notif_from`; its `%(app)s` placeholder is replaced with `app_name`.
    #[serde(default)]
    pub from: Option<String>,
    /// What this service is called in emails: the subject's `[Matrix]` prefix and the
    /// sender's name. Corresponds to Synapse's `app_name`.
    #[serde(default = "default_app_name")]
    pub app_name: String,
    /// The web client the links in a notification email open, for example
    /// `https://app.example.org`: a room is linked as `<client_base_url>/#/room/<room id>`.
    /// Unset, links go to `https://matrix.to`, which opens the reader's own client.
    /// Corresponds to Synapse's `client_base_url`.
    #[serde(default)]
    pub client_base_url: Option<String>,
    /// The notification emails that email pushers receive: when they are sent and what their
    /// subject says.
    #[serde(default)]
    pub notifications: NotificationEmailConfig,
}

fn default_app_name() -> String {
    "Matrix".to_owned()
}

/// How a mail connection is secured.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SmtpSecurity {
    /// Connect in the clear and upgrade with STARTTLS; refuse to send if the server does not
    /// offer it. The usual choice for port 587.
    #[default]
    Starttls,
    /// TLS from the first byte (implicit TLS). The usual choice for port 465.
    Tls,
    /// No encryption, nor an attempt at it. Only for a relay on the same host or a private
    /// network, or a test mail catcher.
    None,
}

/// The SMTP server mail is sent through.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SmtpConfig {
    /// The SMTP server's host name or address. Unset, no email is sent. Corresponds to
    /// Synapse's `smtp_host`.
    #[serde(default)]
    pub host: Option<String>,
    /// The SMTP server's port: 587 for STARTTLS submission, 465 for implicit TLS, 25 for a
    /// local relay. Corresponds to Synapse's `smtp_port`.
    #[serde(default = "default_smtp_port")]
    pub port: u16,
    /// How the connection is secured: `starttls` (the default), `tls` or `none`. Corresponds
    /// to Synapse's `enable_tls`, `require_transport_security` and `force_tls` together:
    /// `force_tls: true` is `tls`; `enable_tls: false` is `none`; otherwise `starttls`, which
    /// always requires the upgrade (Synapse's `require_transport_security: true`).
    #[serde(default)]
    pub security: SmtpSecurity,
    /// The user name to authenticate with, if the server wants one. Corresponds to Synapse's
    /// `smtp_user`.
    #[serde(default)]
    pub username: Option<String>,
    /// The password for `username`. Prefer `password_file`. Corresponds to Synapse's
    /// `smtp_pass`.
    #[serde(default)]
    pub password: SecretString,
    /// Path to a file holding the SMTP password, read in place of `password` so the secret
    /// stays out of the database. Corresponds to Synapse's `smtp_pass_path`.
    #[serde(default)]
    pub password_file: Option<PathBuf>,
    /// The name the server's TLS certificate is checked against, when it is not `host` (a
    /// relay reached by address, say). Corresponds to Synapse's `tlsname`.
    #[serde(default)]
    pub tls_name: Option<String>,
}

const fn default_smtp_port() -> u16 {
    587
}

impl Default for SmtpConfig {
    fn default() -> Self {
        Self {
            host: None,
            port: default_smtp_port(),
            security: SmtpSecurity::default(),
            username: None,
            password: SecretString::default(),
            password_file: None,
            tls_name: None,
        }
    }
}

/// When notification emails are sent, and what they say.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NotificationEmailConfig {
    /// Whether email pushers are delivered to at all. On by default; mail still needs
    /// `smtp.host`. Off, email pushers are stored and nothing is sent. Corresponds to
    /// Synapse's `enable_notifs`.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// How long the first unread notification in a room waits before its email goes, so
    /// someone who reads the message in a client meanwhile gets no email (a read receipt
    /// cancels it). Zero sends the first email at once. Synapse waits ten minutes.
    #[serde(default = "default_delay_before_mail")]
    pub delay_before_mail: Duration,
    /// After an email about a room, how long before the next one about that room may go,
    /// while its messages stay unread. Each further email waits `throttle_multiplier` times
    /// longer than the last, up to `throttle_max`; reading the room resets it. Synapse's
    /// values: ten minutes, times six, up to a day.
    #[serde(default = "default_throttle_start")]
    pub throttle_start: Duration,
    /// The longest wait between two emails about one room whose messages go unread.
    #[serde(default = "default_throttle_max")]
    pub throttle_max: Duration,
    /// How much longer each successive email about an unread room waits.
    #[serde(default = "default_throttle_multiplier")]
    pub throttle_multiplier: u32,
    /// After this long without a notification in a room, its wait goes back to
    /// `throttle_start`.
    #[serde(default = "default_throttle_reset_after")]
    pub throttle_reset_after: Duration,
    /// The subject line of each kind of email. `%(app)s` is `app_name`, `%(person)s` the
    /// sender's name and `%(room)s` the room's. The same keys and placeholders as Synapse's
    /// `subjects`.
    #[serde(default)]
    pub subjects: SubjectsConfig,
}

const fn default_true() -> bool {
    true
}

const fn default_delay_before_mail() -> Duration {
    Duration::ZERO
}

const fn default_throttle_start() -> Duration {
    Duration::from_mins(10)
}

const fn default_throttle_max() -> Duration {
    Duration::from_hours(24)
}

const fn default_throttle_multiplier() -> u32 {
    6
}

const fn default_throttle_reset_after() -> Duration {
    Duration::from_hours(12)
}

impl Default for NotificationEmailConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            delay_before_mail: default_delay_before_mail(),
            throttle_start: default_throttle_start(),
            throttle_max: default_throttle_max(),
            throttle_multiplier: default_throttle_multiplier(),
            throttle_reset_after: default_throttle_reset_after(),
            subjects: SubjectsConfig::default(),
        }
    }
}

/// The subject of each kind of notification email. Synapse's `subjects` keys, with Synapse's
/// defaults.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SubjectsConfig {
    /// One message, from one person, in a named room.
    #[serde(default = "default_message_from_person_in_room")]
    pub message_from_person_in_room: String,
    /// One message from one person in a room with no name (a direct chat).
    #[serde(default = "default_message_from_person")]
    pub message_from_person: String,
    /// Several messages from one person in a room with no name.
    #[serde(default = "default_messages_from_person")]
    pub messages_from_person: String,
    /// Several messages in one named room.
    #[serde(default = "default_messages_in_room")]
    pub messages_in_room: String,
    /// Messages in several rooms, the first of them named.
    #[serde(default = "default_messages_in_room_and_others")]
    pub messages_in_room_and_others: String,
    /// Messages in several rooms, from one person in the first of them.
    #[serde(default = "default_messages_from_person_and_others")]
    pub messages_from_person_and_others: String,
    /// An invitation to a room with no name.
    #[serde(default = "default_invite_from_person")]
    pub invite_from_person: String,
    /// An invitation to a named room.
    #[serde(default = "default_invite_from_person_to_room")]
    pub invite_from_person_to_room: String,
}

fn default_message_from_person_in_room() -> String {
    "[%(app)s] You have a message on %(app)s from %(person)s in the %(room)s room...".to_owned()
}

fn default_message_from_person() -> String {
    "[%(app)s] You have a message on %(app)s from %(person)s...".to_owned()
}

fn default_messages_from_person() -> String {
    "[%(app)s] You have messages on %(app)s from %(person)s...".to_owned()
}

fn default_messages_in_room() -> String {
    "[%(app)s] You have messages on %(app)s in the %(room)s room...".to_owned()
}

fn default_messages_in_room_and_others() -> String {
    "[%(app)s] You have messages on %(app)s in the %(room)s room and others...".to_owned()
}

fn default_messages_from_person_and_others() -> String {
    "[%(app)s] You have messages on %(app)s from %(person)s and others...".to_owned()
}

fn default_invite_from_person() -> String {
    "[%(app)s] %(person)s has invited you to chat on %(app)s...".to_owned()
}

fn default_invite_from_person_to_room() -> String {
    "[%(app)s] %(person)s has invited you to join the %(room)s room on %(app)s...".to_owned()
}

impl Default for SubjectsConfig {
    fn default() -> Self {
        Self {
            message_from_person_in_room: default_message_from_person_in_room(),
            message_from_person: default_message_from_person(),
            messages_from_person: default_messages_from_person(),
            messages_in_room: default_messages_in_room(),
            messages_in_room_and_others: default_messages_in_room_and_others(),
            messages_from_person_and_others: default_messages_from_person_and_others(),
            invite_from_person: default_invite_from_person(),
            invite_from_person_to_room: default_invite_from_person_to_room(),
        }
    }
}

impl EmailConfig {
    /// Whether mail can be sent at all: an SMTP host is set and a sender is named.
    #[must_use]
    pub fn is_configured(&self) -> bool {
        self.smtp.host.as_deref().is_some_and(|h| !h.is_empty()) && self.from.is_some()
    }

    /// Resolves the SMTP password's `*_file` form.
    pub(crate) fn resolve_secrets(&mut self, prefix: &str) -> Result<(), ConfigError> {
        resolve_secret_pair(
            &format!("{prefix}.smtp.password"),
            &mut self.smtp.password,
            &self.smtp.password_file,
        )
    }
}

impl Validate for EmailConfig {
    fn validate(&self, prefix: &str, errors: &mut ValidationErrors) {
        let host_set = self.smtp.host.as_deref().is_some_and(|h| !h.is_empty());
        if self.smtp.port == 0 {
            errors.push(format!("{prefix}.smtp.port"), "must be between 1 and 65535");
        }
        if self.smtp.username.is_some() && !self.smtp.password.is_some() {
            errors.push(
                format!("{prefix}.smtp.password"),
                "a username needs a password (`password` or `password_file`)",
            );
        }
        if self.smtp.password.is_some() && self.smtp.username.is_none() {
            errors.push(
                format!("{prefix}.smtp.username"),
                "a password needs a username",
            );
        }
        match self.from.as_deref() {
            Some(from) if !from.contains('@') => {
                errors.push(
                    format!("{prefix}.from"),
                    "must be an email address, as `Name <address>` or a bare address",
                );
            }
            None if host_set => {
                errors.push(
                    format!("{prefix}.from"),
                    "required once `smtp.host` is set: the address mail is sent from",
                );
            }
            _ => {}
        }
        if let Some(url) = &self.client_base_url
            && !(url.starts_with("https://") || url.starts_with("http://"))
        {
            errors.push(
                format!("{prefix}.client_base_url"),
                "must start with https:// (or http://)",
            );
        }
        let n = &self.notifications;
        if n.throttle_multiplier == 0 {
            errors.push(
                format!("{prefix}.notifications.throttle_multiplier"),
                "must be at least 1",
            );
        }
        if n.throttle_max.as_millis() < n.throttle_start.as_millis() {
            errors.push(
                format!("{prefix}.notifications.throttle_max"),
                "must be at least `throttle_start`",
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn validate(yaml: &str) -> Result<EmailConfig, Vec<String>> {
        let config: EmailConfig = serde_yaml_ng::from_str(yaml).map_err(|e| vec![e.to_string()])?;
        let mut errors = ValidationErrors::new();
        config.validate("email", &mut errors);
        if errors.is_empty() {
            Ok(config)
        } else {
            Err(errors.0.into_iter().map(|e| e.path).collect())
        }
    }

    #[test]
    fn the_default_sends_nothing_and_is_valid() {
        let config = validate("{}").unwrap();
        assert!(!config.is_configured());
        assert_eq!(config.smtp.port, 587);
        assert_eq!(config.smtp.security, SmtpSecurity::Starttls);
        assert_eq!(config.app_name, "Matrix");
        assert!(config.notifications.enabled);
        assert_eq!(config.notifications.delay_before_mail, Duration::ZERO);
        assert_eq!(config.notifications.throttle_multiplier, 6);
        assert!(
            config
                .notifications
                .subjects
                .message_from_person_in_room
                .contains("%(room)s")
        );
    }

    #[test]
    fn a_host_needs_a_sender_and_a_username_needs_a_password() {
        assert_eq!(
            validate("smtp:\n  host: mail.example.org\n").unwrap_err(),
            vec!["email.from"]
        );
        assert_eq!(
            validate("smtp:\n  host: mail.example.org\n  username: bob\nfrom: a@b\n").unwrap_err(),
            vec!["email.smtp.password"]
        );
        assert_eq!(
            validate("smtp:\n  host: mail.example.org\n  password: pw\nfrom: a@b\n").unwrap_err(),
            vec!["email.smtp.username"]
        );
        let ok = validate(
            "smtp:\n  host: mail.example.org\n  port: 465\n  security: tls\n  username: bob\n  password: pw\nfrom: Matrix <noreply@example.org>\n",
        )
        .unwrap();
        assert!(ok.is_configured());
        assert_eq!(ok.smtp.security, SmtpSecurity::Tls);
        assert_eq!(ok.smtp.password.as_str(), Some("pw"));
    }

    #[test]
    fn the_sender_the_client_url_and_the_throttle_are_checked() {
        assert_eq!(
            validate("from: not-an-address\n").unwrap_err(),
            vec!["email.from"]
        );
        assert_eq!(
            validate("client_base_url: app.example.org\n").unwrap_err(),
            vec!["email.client_base_url"]
        );
        assert_eq!(
            validate("notifications:\n  throttle_multiplier: 0\n  throttle_start: 2h\n  throttle_max: 1h\n")
                .unwrap_err(),
            vec![
                "email.notifications.throttle_multiplier",
                "email.notifications.throttle_max"
            ]
        );
        assert_eq!(
            validate("smtp:\n  port: 0\n").unwrap_err(),
            vec!["email.smtp.port"]
        );
    }

    #[test]
    fn the_password_is_redacted_in_debug_output() {
        let config = validate("smtp:\n  username: bob\n  password: hunter2\n").unwrap();
        assert!(!format!("{config:?}").contains("hunter2"));
    }
}
