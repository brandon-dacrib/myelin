//! The `none` provider: scanning off. This is [`crate::scanning::config::ProviderKind`]'s
//! default, and is also what [`crate::scanning::providers::build`] constructs when nothing else
//! was configured.
//!
//! Note the distinction from `mode: off` (`crate::scanning::config::ScanMode::Off`):
//! `crate::scanning::engine::ScanEngine` never even constructs a [`ContentScanner`] call when the
//! mode is off (see that module's doc). This provider exists for the separate case where a
//! provider is asked for but genuinely should do nothing — most usefully, as the harmless
//! fallback [`crate::scanning::providers::build`] returns when `provider: none` (the field's own
//! default), so a `ScanEngine` can always be constructed even with scanning fully disabled.

use async_trait::async_trait;

use crate::scanning::types::{ContentScanner, ScanContext, ScanError, ScanSource, Verdict};

/// Always reports [`Verdict::Clean`] without reading any content.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoneScanner;

#[async_trait]
impl ContentScanner for NoneScanner {
    fn id(&self) -> &str {
        "none"
    }

    async fn engine_version(&self) -> Option<String> {
        None
    }

    async fn scan(
        &self,
        mut content: ScanSource<'_>,
        _ctx: &ScanContext,
    ) -> Result<Verdict, ScanError> {
        // Drain (not scan) the source: a well-behaved `ContentScanner` should not leave chunks
        // unread, in case a future caller ever asserts a source was fully consumed.
        while content
            .next_chunk()
            .await
            .map_err(|e| ScanError::Other(e.to_string()))?
            .is_some()
        {}
        Ok(Verdict::Clean)
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use bytes::Bytes;

    use super::*;
    use crate::scanning::types::ScanSourceKind;

    fn ctx() -> ScanContext {
        ScanContext {
            deadline: Instant::now() + Duration::from_secs(1),
            uploader: Some("@alice:example.org".into()),
            source: ScanSourceKind::Local,
            media_id: "abc123".into(),
            server_name: "example.org".into(),
        }
    }

    #[tokio::test]
    async fn always_clean() {
        let scanner = NoneScanner;
        let source = ScanSource::from_bytes("text/plain", Bytes::from_static(b"anything"), 4);
        let verdict = scanner.scan(source, &ctx()).await.unwrap();
        assert_eq!(verdict, Verdict::Clean);
        assert_eq!(scanner.id(), "none");
        assert_eq!(scanner.engine_version().await, None);
    }
}
