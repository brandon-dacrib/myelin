//! The client API, spoken by the manager's bots and by an instance's bot over loopback, with an
//! appservice token and `user_id` masquerading: the same requests any bridge makes, so the
//! manager is held to the paths bridges are held to.

use reqwest::Url;
use serde_json::{Value, json};

/// A client of this server's own client API.
#[derive(Clone)]
pub struct MatrixClient {
    http: reqwest::Client,
    base: Url,
}

/// A failed request: the status and the body's `errcode`, when there was one.
#[derive(Debug, thiserror::Error)]
#[error("{context}: {status} {errcode}")]
pub struct MatrixError {
    pub context: String,
    pub status: u16,
    pub errcode: String,
}

impl MatrixClient {
    /// Against `base` (`http://127.0.0.1:8008`).
    ///
    /// # Panics
    /// If `base` is not a URL; the caller builds it from a bound socket address.
    #[must_use]
    pub fn new(base: &str) -> Self {
        Self {
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .unwrap_or_default(),
            base: Url::parse(base).expect("a loopback base URL"),
        }
    }

    fn url(&self, segments: &[&str], as_user: Option<&str>) -> Url {
        let mut url = self.base.clone();
        if let Ok(mut path) = url.path_segments_mut() {
            path.pop_if_empty();
            path.extend(["_matrix", "client", "v3"]);
            path.extend(segments);
        }
        if let Some(user) = as_user {
            url.query_pairs_mut().append_pair("user_id", user);
        }
        url
    }

    async fn call(
        &self,
        method: reqwest::Method,
        url: Url,
        token: &str,
        body: Option<Value>,
        context: &str,
    ) -> Result<Value, MatrixError> {
        let mut request = self.http.request(method, url).bearer_auth(token);
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().await.map_err(|e| MatrixError {
            context: format!("{context}: {e}"),
            status: 0,
            errcode: String::new(),
        })?;
        let status = response.status().as_u16();
        let body: Value = response.json().await.unwrap_or(Value::Null);
        if (200..300).contains(&status) {
            Ok(body)
        } else {
            Err(MatrixError {
                context: context.to_owned(),
                status,
                errcode: body["errcode"].as_str().unwrap_or_default().to_owned(),
            })
        }
    }

    /// Registers `localpart` as an appservice user; one that exists already is fine.
    ///
    /// # Errors
    /// Any other failure.
    pub async fn ensure_user(&self, token: &str, localpart: &str) -> Result<(), MatrixError> {
        let result = self
            .call(
                reqwest::Method::POST,
                self.url(&["register"], None),
                token,
                Some(json!({"type": "m.login.application_service", "username": localpart, "inhibit_login": true})),
                "register",
            )
            .await;
        match result {
            Ok(_) => Ok(()),
            Err(e) if e.errcode == "M_USER_IN_USE" => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Sets `user`'s display name.
    ///
    /// # Errors
    /// On failure.
    pub async fn set_display_name(
        &self,
        token: &str,
        user: &str,
        name: &str,
    ) -> Result<(), MatrixError> {
        self.call(
            reqwest::Method::PUT,
            self.url(&["profile", user, "displayname"], Some(user)),
            token,
            Some(json!({"displayname": name})),
            "displayname",
        )
        .await
        .map(|_| ())
    }

    /// Joins `room_id` as `user`.
    ///
    /// # Errors
    /// On failure.
    pub async fn join(&self, token: &str, user: &str, room_id: &str) -> Result<(), MatrixError> {
        self.call(
            reqwest::Method::POST,
            self.url(&["join", room_id], Some(user)),
            token,
            Some(json!({})),
            "join",
        )
        .await
        .map(|_| ())
    }

    /// Sends a notice (plain text and HTML) to `room_id` as `user`.
    ///
    /// # Errors
    /// On failure.
    pub async fn notice(
        &self,
        token: &str,
        user: &str,
        room_id: &str,
        text: &str,
        html: &str,
    ) -> Result<(), MatrixError> {
        let txn = format!("myelin-{}", crate::random_hex(8));
        self.call(
            reqwest::Method::PUT,
            self.url(
                &["rooms", room_id, "send", "m.room.message", &txn],
                Some(user),
            ),
            token,
            Some(json!({
                "msgtype": "m.notice",
                "body": text,
                "format": "org.matrix.custom.html",
                "formatted_body": html,
            })),
            "send",
        )
        .await
        .map(|_| ())
    }

    /// Invites `invitee` to `room_id` as `user`; one already there is fine.
    ///
    /// # Errors
    /// On failure.
    pub async fn invite(
        &self,
        token: &str,
        user: &str,
        room_id: &str,
        invitee: &str,
    ) -> Result<(), MatrixError> {
        let result = self
            .call(
                reqwest::Method::POST,
                self.url(&["rooms", room_id, "invite"], Some(user)),
                token,
                Some(json!({"user_id": invitee})),
                "invite",
            )
            .await;
        match result {
            Err(e) if e.status == 403 && e.errcode == "M_FORBIDDEN" => Ok(()), // already joined
            other => other.map(|_| ()),
        }
    }

    /// Makes a direct chat from `user` to `invitee`, encrypted if `encrypted`, and records it in
    /// `user`'s `m.direct`. Returns the room id.
    ///
    /// # Errors
    /// On failure.
    pub async fn create_dm(
        &self,
        token: &str,
        user: &str,
        invitee: &str,
        encrypted: bool,
    ) -> Result<String, MatrixError> {
        let mut initial_state = Vec::new();
        if encrypted {
            initial_state.push(json!({
                "type": "m.room.encryption",
                "state_key": "",
                "content": {"algorithm": "m.megolm.v1.aes-sha2"},
            }));
        }
        let body = self
            .call(
                reqwest::Method::POST,
                self.url(&["createRoom"], Some(user)),
                token,
                Some(json!({
                    "preset": "trusted_private_chat",
                    "is_direct": true,
                    "invite": [invitee],
                    "initial_state": initial_state,
                })),
                "createRoom",
            )
            .await?;
        let room_id = body["room_id"].as_str().unwrap_or_default().to_owned();
        let _ = self
            .call(
                reqwest::Method::PUT,
                self.url(&["user", user, "account_data", "m.direct"], Some(user)),
                token,
                Some(json!({ invitee: [room_id] })),
                "m.direct",
            )
            .await;
        Ok(room_id)
    }
}
