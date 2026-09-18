//! `GET /account/whoami`.

use axum::Json;
use serde_json::{Value, json};

use crate::middleware::AllowGuest;

/// `GET /account/whoami`: identifies the caller, including guests (the one endpoint besides
/// `/logout` guests are always allowed to call, since a client needs it to tell a guest session
/// apart from a full one).
pub async fn get_whoami(AllowGuest(requester): AllowGuest) -> Json<Value> {
    Json(json!({
        "user_id": requester.user_id,
        "device_id": requester.device_id,
        "is_guest": requester.is_guest,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::requester::Requester;
    use ruma::{device_id, user_id};

    #[tokio::test]
    async fn reports_user_id_device_id_and_guest_flag() {
        let mut requester = Requester::for_user(user_id!("@alice:example.org").to_owned());
        requester.device_id = Some(device_id!("DEV1").to_owned());
        let Json(body) = get_whoami(AllowGuest(requester)).await;
        assert_eq!(body["user_id"], "@alice:example.org");
        assert_eq!(body["device_id"], "DEV1");
        assert_eq!(body["is_guest"], false);
    }

    #[tokio::test]
    async fn reports_guest_accounts_as_guests() {
        let mut requester = Requester::for_user(user_id!("@guest:example.org").to_owned());
        requester.is_guest = true;
        let Json(body) = get_whoami(AllowGuest(requester)).await;
        assert_eq!(body["is_guest"], true);
    }
}
