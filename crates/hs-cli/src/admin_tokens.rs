//! Admin API tokens narrower than a full administrator's: the durable store `hs serve` wires
//! behind `hs_admin::admin_tokens` ([`TablesAdminTokens`]), and the client half of
//! `hs admin-token` ([`create`], [`list`], [`revoke`]), which talks to a running server's
//! `/api/v1/admin-tokens` with an administrator's credential.
//!
//! `hs-admin` deliberately depends on no storage crate: it defines the source as a trait
//! (`hs_admin::admin_tokens::AdminTokenSource`) and lets whoever composes the server supply one,
//! as with the audit sink ([`crate::audit`]). Two keyspaces: `hs_admin.tokens`, keyed by the
//! token's id, and `hs_admin.tokens_by_hash`, mapping a token's SHA-256 to its id so verifying
//! a bearer is one lookup and never a scan. Both rows are written in one transaction, so a
//! token is either findable by both or by neither.
//!
//! The CLI half is the same request the web interface's Settings, Admin tokens page makes,
//! for an operator scripting a deployment: the credential is `--token`, else `HS_ADMIN_TOKEN`,
//! else prompted for without echo. The minted token is printed on stdout alone, so
//! `TOKEN=$(hs admin-token create ...)` works; everything else goes to stderr.

use std::sync::Arc;

use hs_admin::admin_tokens::{AdminToken, AdminTokenRecord, AdminTokenSource, NewAdminToken};
use hs_admin::model::Scope;
use hs_admin::sources::SourceError;
use hs_kv::{KvBackend, KvError, RangeSpec, TransactConfig, transact};
use hs_tables::keyspace::TypedKeyspace;

/// A durable admin token store over any `hs-kv` backend.
pub struct TablesAdminTokens<B: KvBackend> {
    backend: B,
    /// `id -> record JSON`.
    tokens: Arc<TypedKeyspace<B::Keyspace, (String,)>>,
    /// `secret hash -> id`.
    by_hash: Arc<TypedKeyspace<B::Keyspace, (String,)>>,
}

impl<B: KvBackend> TablesAdminTokens<B> {
    /// Opens (or creates) the two keyspaces on `backend`.
    ///
    /// # Errors
    /// Returns the backend's own error if either keyspace cannot be opened.
    pub fn open(backend: B) -> Result<Self, KvError> {
        let tokens = backend.keyspace("hs_admin.tokens")?;
        let by_hash = backend.keyspace("hs_admin.tokens_by_hash")?;
        Ok(Self {
            backend,
            tokens: Arc::new(TypedKeyspace::new(tokens)),
            by_hash: Arc::new(TypedKeyspace::new(by_hash)),
        })
    }

    fn read_record<R: hs_kv::KvRead<Keyspace = B::Keyspace>>(
        &self,
        read: &R,
        id: &str,
    ) -> Result<Option<AdminTokenRecord>, SourceError> {
        match self
            .tokens
            .get(read, &(id.to_owned(),))
            .map_err(unavailable)?
        {
            Some(bytes) => serde_json::from_slice(&bytes)
                .map(Some)
                .map_err(unavailable),
            None => Ok(None),
        }
    }
}

#[derive(Debug, thiserror::Error)]
#[error("admin token {0} already exists")]
struct Exists(String);

fn unavailable(e: impl std::fmt::Display) -> SourceError {
    SourceError::Unavailable(e.to_string())
}

#[async_trait::async_trait]
impl<B: KvBackend + 'static> AdminTokenSource for TablesAdminTokens<B> {
    async fn list(&self) -> Result<Vec<AdminToken>, SourceError> {
        let snap = self.backend.snapshot();
        let mut records: Vec<AdminTokenRecord> = self
            .tokens
            .range(&snap, RangeSpec::full())
            .map(|item| {
                let (_key, bytes) = item.map_err(unavailable)?;
                serde_json::from_slice(&bytes).map_err(unavailable)
            })
            .collect::<Result<_, _>>()?;
        // ULIDs sort by time within a millisecond only approximately; `created_at` is the
        // order an operator expects.
        records.sort_by(|a, b| a.token.created_at.cmp(&b.token.created_at));
        Ok(records.into_iter().map(|r| r.token).collect())
    }

    async fn get(&self, id: &str) -> Result<Option<AdminToken>, SourceError> {
        let snap = self.backend.snapshot();
        Ok(self.read_record(&snap, id)?.map(|r| r.token))
    }

    async fn create(&self, token: NewAdminToken) -> Result<AdminToken, SourceError> {
        let record = token.record;
        let id_key = (record.token.id.clone(),);
        let hash_key = (record.secret_hash.clone(),);
        let value = serde_json::to_vec(&record).map_err(unavailable)?;
        let id_bytes = record.token.id.as_bytes().to_vec();
        transact(&self.backend, TransactConfig::default(), |txn| {
            let taken = self
                .tokens
                .get(txn, &id_key)
                .map_err(KvError::backend)?
                .is_some()
                || self
                    .by_hash
                    .get(txn, &hash_key)
                    .map_err(KvError::backend)?
                    .is_some();
            if taken {
                return Err(KvError::backend(Exists(record.token.id.clone())));
            }
            self.tokens
                .put(txn, &id_key, &value)
                .map_err(KvError::backend)?;
            self.by_hash
                .put(txn, &hash_key, &id_bytes)
                .map_err(KvError::backend)
        })
        .map_err(|e| match &e {
            KvError::Backend(inner) if inner.downcast_ref::<Exists>().is_some() => {
                SourceError::Conflict(e.to_string())
            }
            _ => unavailable(e),
        })?;
        Ok(record.token)
    }

    async fn delete(&self, id: &str) -> Result<(), SourceError> {
        let id_key = (id.to_owned(),);
        let existed = transact(&self.backend, TransactConfig::default(), |txn| {
            let Some(bytes) = self.tokens.get(txn, &id_key).map_err(KvError::backend)? else {
                return Ok(false);
            };
            let record: AdminTokenRecord =
                serde_json::from_slice(&bytes).map_err(KvError::backend)?;
            self.tokens.delete(txn, &id_key).map_err(KvError::backend)?;
            self.by_hash
                .delete(txn, &(record.secret_hash,))
                .map_err(KvError::backend)?;
            Ok(true)
        })
        .map_err(unavailable)?;
        if existed {
            Ok(())
        } else {
            Err(SourceError::NotFound)
        }
    }

    async fn find_by_hash(&self, hash: &str) -> Result<Option<AdminTokenRecord>, SourceError> {
        let snap = self.backend.snapshot();
        let Some(id_bytes) = self
            .by_hash
            .get(&snap, &(hash.to_owned(),))
            .map_err(unavailable)?
        else {
            return Ok(None);
        };
        let id = String::from_utf8(id_bytes.as_ref().to_vec()).map_err(unavailable)?;
        // The index and the row are written together, so a dangling index entry is a store
        // fault, reported as such rather than as "no such token".
        self.read_record(&snap, &id)?
            .map(Some)
            .ok_or_else(|| unavailable(format!("admin token index names {id}, which is missing")))
    }
}

/// Errors from `hs admin-token`.
#[derive(Debug, thiserror::Error)]
pub enum AdminTokenCmdError {
    /// The HTTP request itself failed.
    #[error(
        "could not reach {url}: {source}. Is the server running there? --server sets the address"
    )]
    Request {
        /// The URL that failed.
        url: String,
        /// The underlying error.
        #[source]
        source: reqwest::Error,
    },
    /// The server answered, and the answer was no.
    #[error("the server refused ({status}): {detail}")]
    Refused {
        /// The HTTP status.
        status: reqwest::StatusCode,
        /// The problem document's `detail`, or the raw body.
        detail: String,
    },
    /// A success status with a body of the wrong shape.
    #[error("unexpected response from {url}: {source}")]
    UnexpectedResponse {
        /// The URL whose response could not be parsed.
        url: String,
        /// The underlying JSON error.
        #[source]
        source: reqwest::Error,
    },
    /// A `--scope` that is not one of the six.
    #[error("unknown scope {name:?}; the scopes are {}", scope_names())]
    UnknownScope {
        /// The name that is not a scope.
        name: String,
    },
}

fn scope_names() -> String {
    hs_admin::admin_tokens::ALL_SCOPES
        .iter()
        .map(|s| s.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Parses `--scope` values. Empty means "the default: a full administrator's".
///
/// # Errors
/// [`AdminTokenCmdError::UnknownScope`] for a name outside the catalog.
pub fn parse_scopes(names: &[String]) -> Result<Vec<Scope>, AdminTokenCmdError> {
    names
        .iter()
        .map(|n| {
            Scope::parse(n).ok_or_else(|| AdminTokenCmdError::UnknownScope { name: n.clone() })
        })
        .collect()
}

/// What `hs admin-token create` sends.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct CreateRequest {
    /// The token's name.
    pub name: String,
    /// The scopes, or none for the server's default.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scopes: Option<Vec<Scope>>,
    /// The expiry (RFC 3339), if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
}

/// What `POST /admin-tokens` answers: the record and, once, the token.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct Created {
    /// The listed record.
    #[serde(flatten)]
    pub token: AdminToken,
    /// The bearer token.
    #[serde(rename = "token")]
    pub secret: String,
}

async fn refusal(response: reqwest::Response) -> AdminTokenCmdError {
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    let detail = serde_json::from_str::<serde_json::Value>(&body)
        .ok()
        .and_then(|v| {
            let detail = v.get("detail")?.as_str()?.to_owned();
            match v.get("required_scope").and_then(|s| s.as_str()) {
                Some(scope) => Some(format!("{detail} (required scope: {scope})")),
                None => Some(detail),
            }
        })
        .unwrap_or(body);
    AdminTokenCmdError::Refused { status, detail }
}

fn base(server_url: &str) -> String {
    format!("{}/api/v1/admin-tokens", server_url.trim_end_matches('/'))
}

/// `POST /api/v1/admin-tokens` as `bearer`.
///
/// # Errors
/// See [`AdminTokenCmdError`].
pub async fn create(
    client: &reqwest::Client,
    server_url: &str,
    bearer: &str,
    request: &CreateRequest,
) -> Result<Created, AdminTokenCmdError> {
    let url = base(server_url);
    let response = client
        .post(&url)
        .bearer_auth(bearer)
        .json(request)
        .send()
        .await
        .map_err(|source| AdminTokenCmdError::Request {
            url: url.clone(),
            source,
        })?;
    if !response.status().is_success() {
        return Err(refusal(response).await);
    }
    response
        .json()
        .await
        .map_err(|source| AdminTokenCmdError::UnexpectedResponse { url, source })
}

/// `GET /api/v1/admin-tokens` as `bearer`, every page.
///
/// # Errors
/// See [`AdminTokenCmdError`].
pub async fn list(
    client: &reqwest::Client,
    server_url: &str,
    bearer: &str,
) -> Result<Vec<AdminToken>, AdminTokenCmdError> {
    #[derive(serde::Deserialize)]
    struct Page {
        items: Vec<AdminToken>,
        next_cursor: Option<String>,
    }
    let mut items = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let url = base(server_url);
        let mut request = client
            .get(&url)
            .bearer_auth(bearer)
            .query(&[("limit", "200")]);
        if let Some(c) = &cursor {
            request = request.query(&[("cursor", c.as_str())]);
        }
        let response = request
            .send()
            .await
            .map_err(|source| AdminTokenCmdError::Request {
                url: url.clone(),
                source,
            })?;
        if !response.status().is_success() {
            return Err(refusal(response).await);
        }
        let page: Page = response
            .json()
            .await
            .map_err(|source| AdminTokenCmdError::UnexpectedResponse { url, source })?;
        items.extend(page.items);
        match page.next_cursor {
            Some(next) => cursor = Some(next),
            None => return Ok(items),
        }
    }
}

/// `DELETE /api/v1/admin-tokens/{id}` as `bearer`.
///
/// # Errors
/// See [`AdminTokenCmdError`].
pub async fn revoke(
    client: &reqwest::Client,
    server_url: &str,
    bearer: &str,
    id: &str,
) -> Result<(), AdminTokenCmdError> {
    let url = format!("{}/{id}", base(server_url));
    let response = client
        .delete(&url)
        .bearer_auth(bearer)
        .send()
        .await
        .map_err(|source| AdminTokenCmdError::Request {
            url: url.clone(),
            source,
        })?;
    if !response.status().is_success() {
        return Err(refusal(response).await);
    }
    Ok(())
}

/// `--expires-in 30d` (also `h`, `m`, `s`) as an RFC 3339 instant from `now_ms`.
#[must_use]
pub fn expires_in_to_rfc3339(spec: &str, now_ms: i64) -> Option<String> {
    let spec = spec.trim();
    let (number, unit) = spec.split_at(spec.len().checked_sub(1)?);
    let n: i64 = number.parse().ok()?;
    let ms = match unit {
        "d" => n.checked_mul(86_400_000)?,
        "h" => n.checked_mul(3_600_000)?,
        "m" => n.checked_mul(60_000)?,
        "s" => n.checked_mul(1_000)?,
        _ => return None,
    };
    if ms <= 0 {
        return None;
    }
    let at = time::OffsetDateTime::from_unix_timestamp_nanos(
        i128::from(now_ms.checked_add(ms)?) * 1_000_000,
    )
    .ok()?;
    Some(hs_http::time::format_rfc3339(at))
}

/// One line per token, for `hs admin-token list`.
#[must_use]
pub fn format_list(tokens: &[AdminToken]) -> String {
    if tokens.is_empty() {
        return "no admin tokens\n".to_owned();
    }
    let mut out = String::new();
    for t in tokens {
        let scopes = t
            .scopes
            .iter()
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .join(",");
        out.push_str(&format!(
            "{}\t{}\t{}\tcreated {} by {}\texpires {}\n",
            t.id,
            t.name,
            scopes,
            t.created_at,
            t.created_by,
            t.expires_at.as_deref().unwrap_or("never")
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use hs_admin::admin_tokens::{ValidatedCreate, hash_token, mint};
    use hs_kv::memory::MemoryBackend;

    #[tokio::test]
    async fn the_store_round_trips_finds_by_hash_and_revokes_both_rows() {
        let store = TablesAdminTokens::open(MemoryBackend::new()).expect("opens");
        let (new_token, secret) = mint(
            ValidatedCreate {
                name: "bridge team".into(),
                scopes: vec![Scope::BridgesRead],
                expires_at_ms: None,
                expires_at: None,
            },
            "@ops:example.org",
            1_700_000_000_000,
        );
        let id = new_token.record.token.id.clone();
        let hash = new_token.record.secret_hash.clone();
        assert_eq!(hash, hash_token(&secret));
        let listed = store.create(new_token.clone()).await.expect("stored");
        assert_eq!(listed.id, id);

        // Twice is a conflict, not a second row.
        assert!(matches!(
            store.create(new_token).await,
            Err(SourceError::Conflict(_))
        ));

        let found = store
            .find_by_hash(&hash)
            .await
            .expect("reads")
            .expect("found");
        assert_eq!(found.token.scopes, vec![Scope::BridgesRead]);
        assert_eq!(store.get(&id).await.unwrap().unwrap().name, "bridge team");
        assert_eq!(store.list().await.unwrap().len(), 1);
        assert!(store.find_by_hash("nope").await.unwrap().is_none());

        store.delete(&id).await.expect("revoked");
        assert!(store.find_by_hash(&hash).await.unwrap().is_none());
        assert!(store.get(&id).await.unwrap().is_none());
        assert!(matches!(
            store.delete(&id).await,
            Err(SourceError::NotFound)
        ));
    }

    #[test]
    fn expires_in_parses_the_units_and_refuses_the_rest() {
        assert_eq!(
            expires_in_to_rfc3339("1d", 0).as_deref(),
            Some("1970-01-02T00:00:00.000Z")
        );
        assert_eq!(
            expires_in_to_rfc3339("2h", 0).as_deref(),
            Some("1970-01-01T02:00:00.000Z")
        );
        assert_eq!(
            expires_in_to_rfc3339("90m", 0).as_deref(),
            Some("1970-01-01T01:30:00.000Z")
        );
        assert_eq!(
            expires_in_to_rfc3339("5s", 0).as_deref(),
            Some("1970-01-01T00:00:05.000Z")
        );
        for bad in ["", "d", "0d", "-1h", "1w", "1.5h", "soon"] {
            assert_eq!(expires_in_to_rfc3339(bad, 0), None, "{bad:?}");
        }
    }

    #[test]
    fn scopes_parse_and_an_unknown_one_is_named() {
        assert_eq!(
            parse_scopes(&["bridges:read".into(), "admin:write".into()]).unwrap(),
            vec![Scope::BridgesRead, Scope::AdminWrite]
        );
        assert_eq!(parse_scopes(&[]).unwrap(), Vec::<Scope>::new());
        let err = parse_scopes(&["root".into()]).unwrap_err();
        assert!(err.to_string().contains("root"), "{err}");
        assert!(err.to_string().contains("moderation:write"), "{err}");
    }

    #[test]
    fn the_list_is_one_line_per_token_with_its_scopes() {
        assert_eq!(format_list(&[]), "no admin tokens\n");
        let line = format_list(&[AdminToken {
            id: "01J".into(),
            name: "ci".into(),
            scopes: vec![Scope::AdminRead, Scope::AdminWrite],
            created_at: "2026-10-02T00:00:00.000Z".into(),
            created_by: "@ops:example.org".into(),
            expires_at: None,
        }]);
        assert_eq!(
            line,
            "01J\tci\tadmin:read,admin:write\tcreated 2026-10-02T00:00:00.000Z by \
             @ops:example.org\texpires never\n"
        );
    }
}
