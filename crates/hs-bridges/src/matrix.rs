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
    /// Against `base` (`http://127.0.0.1:8008`). The client is `hs_http::client::builder()`'s,
    /// so the loopback connection goes through the outbound address policy and is counted with
    /// every other outbound connection (`hs_outbound_connections_total`).
    ///
    /// # Panics
    /// If `base` is not a URL; the caller builds it from a bound socket address.
    #[must_use]
    pub fn new(base: &str) -> Self {
        Self {
            http: hs_http::client::builder()
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

    /// `user`'s own keys as `/keys/query` lists them, asked as `user`: the whole response
    /// (`device_keys`, `master_keys`, `self_signing_keys`, ...).
    ///
    /// # Errors
    /// On failure.
    pub async fn keys_query(&self, token: &str, user: &str) -> Result<Value, MatrixError> {
        self.call(
            reqwest::Method::POST,
            self.url(&["keys", "query"], Some(user)),
            token,
            Some(json!({"device_keys": {user: []}})),
            "keys/query",
        )
        .await
    }

    /// Publishes `user`'s cross-signing keys (`body` from
    /// [`crate::cross_signing::BotIdentity::upload_body`]) as the appservice, which this
    /// server lets replace existing keys without user-interactive auth (MSC4190).
    ///
    /// # Errors
    /// On failure, including a `401` from a server that asks for user-interactive auth.
    pub async fn upload_cross_signing_keys(
        &self,
        token: &str,
        user: &str,
        body: Value,
    ) -> Result<(), MatrixError> {
        self.call(
            reqwest::Method::POST,
            self.url(&["keys", "device_signing", "upload"], Some(user)),
            token,
            Some(body),
            "keys/device_signing/upload",
        )
        .await
        .map(|_| ())
    }

    /// Uploads signatures (`body` from [`crate::cross_signing::BotIdentity::sign_device`]) as
    /// `user`. A `200` whose `failures` names a key is an error here: nothing was signed.
    ///
    /// # Errors
    /// On failure, or when the server reported a failure for any key.
    pub async fn upload_signatures(
        &self,
        token: &str,
        user: &str,
        body: Value,
    ) -> Result<(), MatrixError> {
        let answer = self
            .call(
                reqwest::Method::POST,
                self.url(&["keys", "signatures", "upload"], Some(user)),
                token,
                Some(body),
                "keys/signatures/upload",
            )
            .await?;
        let failures = answer.get("failures").and_then(Value::as_object);
        match failures {
            Some(f) if !f.is_empty() => Err(MatrixError {
                context: format!(
                    "keys/signatures/upload: the server refused the signature: {}",
                    Value::Object(f.clone())
                ),
                status: 200,
                errcode: "M_INVALID_SIGNATURE".to_owned(),
            }),
            _ => Ok(()),
        }
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

    /// Leaves `room_id` as `user`; one not in it is fine.
    ///
    /// # Errors
    /// On any other failure.
    pub async fn leave(&self, token: &str, user: &str, room_id: &str) -> Result<(), MatrixError> {
        let result = self
            .call(
                reqwest::Method::POST,
                self.url(&["rooms", room_id, "leave"], Some(user)),
                token,
                Some(json!({})),
                "leave",
            )
            .await;
        match result {
            Err(e) if e.status == 403 && e.errcode == "M_FORBIDDEN" => Ok(()), // not in it
            other => other.map(|_| ()),
        }
    }

    /// The joined members of `room_id`, as `user` sees them.
    ///
    /// # Errors
    /// On failure, a 403 included when `user` is not in the room.
    pub async fn joined_members(
        &self,
        token: &str,
        user: &str,
        room_id: &str,
    ) -> Result<Vec<String>, MatrixError> {
        let body = self
            .call(
                reqwest::Method::GET,
                self.url(&["rooms", room_id, "joined_members"], Some(user)),
                token,
                None,
                "joined_members",
            )
            .await?;
        Ok(body["joined"]
            .as_object()
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default())
    }

    /// Who created `room_id`: the sender of its `m.room.create`, read from the room's state as
    /// `user` (room version 11 dropped `creator` from the event's content, so the content
    /// alone does not say).
    ///
    /// # Errors
    /// On failure, or when the state has no create event.
    pub async fn room_creator(
        &self,
        token: &str,
        user: &str,
        room_id: &str,
    ) -> Result<String, MatrixError> {
        let state = self
            .call(
                reqwest::Method::GET,
                self.url(&["rooms", room_id, "state"], Some(user)),
                token,
                None,
                "state",
            )
            .await?;
        state
            .as_array()
            .and_then(|events| {
                events
                    .iter()
                    .find(|e| e["type"] == "m.room.create")
                    .and_then(|e| e["sender"].as_str())
                    .map(str::to_owned)
            })
            .ok_or_else(|| MatrixError {
                context: "state: no m.room.create".into(),
                status: 200,
                errcode: String::new(),
            })
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
        let _ = self.add_direct(token, user, invitee, &room_id).await;
        Ok(room_id)
    }

    /// Adds `room_id` to `user`'s `m.direct` under `other`, keeping every other entry: a `PUT`
    /// of account data replaces the whole event, and a person's direct chats are not this
    /// bridge's to forget.
    ///
    /// # Errors
    /// On failure to read or write it.
    pub async fn add_direct(
        &self,
        token: &str,
        user: &str,
        other: &str,
        room_id: &str,
    ) -> Result<(), MatrixError> {
        let path = ["user", user, "account_data", "m.direct"];
        let mut direct = match self
            .call(
                reqwest::Method::GET,
                self.url(&path, Some(user)),
                token,
                None,
                "m.direct",
            )
            .await
        {
            Ok(Value::Object(map)) => map,
            Ok(_) => serde_json::Map::new(),
            Err(e) if e.status == 404 => serde_json::Map::new(),
            Err(e) => return Err(e),
        };
        let rooms = direct.entry(other).or_insert_with(|| json!([]));
        if !rooms.is_array() {
            *rooms = json!([]);
        }
        if let Some(list) = rooms.as_array_mut()
            && !list.iter().any(|r| r == room_id)
        {
            list.push(json!(room_id));
        }
        self.call(
            reqwest::Method::PUT,
            self.url(&path, Some(user)),
            token,
            Some(Value::Object(direct)),
            "m.direct",
        )
        .await
        .map(|_| ())
    }
}
