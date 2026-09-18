//! A fake SMTP sink: an in-memory recorder standing in for whatever mailer trait track 07 ends up
//! defining for password-reset and 3PID-verification email (no such trait exists in this
//! workspace yet). Unlike the HTTP-shaped fakes in this crate, this is not a protocol-level SMTP
//! server (`refs/synapse`'s `sendmail` config option talks real SMTP; standing up a listener for
//! it here would mean carrying a full SMTP implementation as a dependency on a shared, resource
//! constrained machine for a feature no crate sends mail through yet). [`FakeSmtpSink::send`]'s
//! signature is deliberately the minimal shape any mailer trait is likely to need
//! (`to`/`subject`/`body_text`); once track 07 lands a real trait, adapting this type to it should
//! be a one-line `impl`.

use crate::record_log::RecordLog;

/// One recorded outbound email.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RecordedEmail {
    /// The envelope/`To` recipient address.
    pub to: String,
    /// The subject line.
    pub subject: String,
    /// The plain-text body.
    pub body_text: String,
}

/// A fake SMTP sink: `send` records instead of delivering.
#[derive(Clone, Default)]
pub struct FakeSmtpSink {
    log: std::sync::Arc<RecordLog>,
}

impl FakeSmtpSink {
    /// A fresh sink with no recorded mail.
    #[must_use]
    pub fn new() -> Self {
        Self {
            log: std::sync::Arc::new(RecordLog::new()),
        }
    }

    /// Records one email; never fails (there is no backend to fail against).
    pub fn send(
        &self,
        to: impl Into<String>,
        subject: impl Into<String>,
        body_text: impl Into<String>,
    ) {
        self.log.record(&RecordedEmail {
            to: to.into(),
            subject: subject.into(),
            body_text: body_text.into(),
        });
    }

    /// Every recorded email, oldest first.
    #[must_use]
    pub fn sent(&self) -> Vec<RecordedEmail> {
        self.log
            .all_as()
            .expect("this fake only ever writes RecordedEmail values")
    }

    /// Every recorded email addressed to `address`, oldest first.
    #[must_use]
    pub fn sent_to(&self, address: &str) -> Vec<RecordedEmail> {
        self.sent()
            .into_iter()
            .filter(|m| m.to == address)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_and_filters_by_recipient() {
        let sink = FakeSmtpSink::new();
        sink.send("alice@example.org", "Verify your email", "code: 123456");
        sink.send("bob@example.org", "Welcome", "hi bob");

        assert_eq!(sink.sent().len(), 2);
        let to_alice = sink.sent_to("alice@example.org");
        assert_eq!(to_alice.len(), 1);
        assert_eq!(to_alice[0].subject, "Verify your email");
        assert!(sink.sent_to("carol@example.org").is_empty());
    }
}
