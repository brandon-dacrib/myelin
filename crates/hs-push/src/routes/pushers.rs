//! `GET /pushers` and `POST /pushers/set`.

use axum::Json;
use axum::extract::State;
use hs_kv::KvBackend;
use ruma::push::{HttpPusherData, PushFormat};
use ruma::api::client::push::{EmailPusherData, Pusher, PusherIds, PusherInit, PusherKind};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::state::{PushRequester, PushState};

/// `GET /pushers`.
pub async fn get_pushers<B: KvBackend + 'static>(
    PushRequester(requester): PushRequester,
    State(state): State<PushState<B>>,
) -> Result<Json<Value>, hs_http::error::MatrixError> {
    let pushers = state.pushers.get_pushers(&requester.user_id).await.map_err(store_err)?;
    Ok(Json(json!({ "pushers": pushers })))
}

/// `POST /pushers/set`'s request body.
#[derive(Deserialize)]
pub struct SetPusherBody {
    pushkey: String,
    app_id: String,
    kind: Option<String>,
    #[serde(default)]
    app_display_name: String,
    #[serde(default)]
    device_display_name: String,
    #[serde(default)]
    profile_tag: Option<String>,
    #[serde(default = "default_lang")]
    lang: String,
    #[serde(default)]
    data: serde_json::Map<String, Value>,
    #[serde(default)]
    #[allow(dead_code)] // `append` is accepted but this store has no cross-user pushkey index to
    // honor the "false replaces any pusher with this pushkey for any user" nuance yet -- see
    // `crate::pushers`'s module docs.
    append: bool,
}

fn default_lang() -> String {
    "en".to_owned()
}

/// `POST /pushers/set`: create, update or delete a pusher.
pub async fn post_pushers_set<B: KvBackend + 'static>(
    PushRequester(requester): PushRequester,
    State(state): State<PushState<B>>,
    Json(body): Json<SetPusherBody>,
) -> Result<Json<Value>, hs_http::error::MatrixError> {
    let Some(kind) = body.kind else {
        let ids = PusherIds::new(body.pushkey, body.app_id);
        state
            .pushers
            .delete_pusher(&requester.user_id, &ids)
            .await
            .map_err(store_err)?;
        return Ok(Json(json!({})));
    };

    let pusher_kind = match kind.as_str() {
        "http" => {
            let url = body
                .data
                .get("url")
                .and_then(Value::as_str)
                .ok_or_else(|| hs_http::error::MatrixError::bad_json("http pushers require data.url"))?
                .to_owned();
            let mut http_data = HttpPusherData::new(url);
            if let Some(format) = body.data.get("format").and_then(Value::as_str) {
                http_data.format = Some(PushFormat::from(format));
            }
            for (k, v) in &body.data {
                if k != "url" && k != "format" {
                    http_data.data.insert(k.clone(), v.clone());
                }
            }
            PusherKind::Http(http_data)
        }
        "email" => {
            let mut email_data = EmailPusherData::new();
            email_data.data = body.data.clone();
            PusherKind::Email(email_data)
        }
        other => {
            return Err(hs_http::error::MatrixError::custom(
                axum::http::StatusCode::BAD_REQUEST,
                hs_http::error::MatrixErrorCode::InvalidParam,
                format!("unsupported pusher kind {other:?}"),
            ));
        }
    };

    let pusher: Pusher = PusherInit {
        ids: PusherIds::new(body.pushkey, body.app_id),
        kind: pusher_kind,
        app_display_name: body.app_display_name,
        device_display_name: body.device_display_name,
        profile_tag: body.profile_tag,
        lang: body.lang,
    }
    .into();
    state
        .pushers
        .set_pusher(&requester.user_id, pusher)
        .await
        .map_err(store_err)?;
    Ok(Json(json!({})))
}

fn store_err(e: crate::error::StoreError) -> hs_http::error::MatrixError {
    hs_http::error::MatrixError::custom(
        axum::http::StatusCode::INTERNAL_SERVER_ERROR,
        hs_http::error::MatrixErrorCode::Unknown,
        e.to_string(),
    )
}
