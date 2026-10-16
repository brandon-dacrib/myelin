//! The [`Requester`]: who an authenticated HTTP request is from.
//!
//! Every Matrix client-server handler in the workspace takes a `Requester` (produced by the
//! [`crate::middleware`] extractor) instead of re-deriving identity from headers itself. See
//! `docs/rfcs/0002-auth-tokens-and-requester.md` section 4 for the full design rationale,
//! including why appservice masquerading and admin/guest flags live here rather than being
//! re-checked ad hoc by every handler.

use ruma::{OwnedDeviceId, OwnedUserId};
use serde::{Deserialize, Serialize};

use crate::token::TokenHash;

/// Who is making this request, and under what authority.
///
/// `Serialize`/`Deserialize` so it can cross the cluster mesh as-is: track 03's forwarding
/// envelope carries the authenticated user context from the replica that terminated the HTTP
/// request to the replica that owns the room or user session (`docs/workstreams/README.md`'s
/// week-6 seam; see [`RequesterContext`] for the type alias tracks 03 and others should name).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Requester {
    /// The Matrix user ID this request acts as. For an appservice request this is the
    /// masqueraded `user_id` (or the appservice's own `sender` if none was given), not the
    /// appservice's own identity — use [`Requester::authenticated_entity`] for that.
    pub user_id: OwnedUserId,

    /// The device this request is acting on behalf of, if any. Present for normal user sessions
    /// (bound at login) and, when MSC3202 masquerading was used, for appservice requests too.
    pub device_id: Option<OwnedDeviceId>,

    /// True if `user_id` is a guest account. Guests are forbidden from most endpoints unless a
    /// handler explicitly opts in (mirrors Synapse's `allow_guest`).
    pub is_guest: bool,

    /// True if `user_id` has the server-administrator flag. Distinct from the admin API's OAuth
    /// scopes (`docs/rfcs/0004-admin-api.md`): this is the legacy per-user flag that also makes a
    /// user's legacy access token carry `admin:write` for the compatibility admin surface.
    pub is_admin: bool,

    /// True if the account is shadow-banned: writes silently succeed from the user's point of
    /// view but never reach other users. Carried on the requester so every handler that fans out
    /// an event can apply it without a second lookup.
    pub shadow_banned: bool,

    /// True if the account is suspended by an administrator. Unlike a locked or deactivated
    /// account, a suspended account still authenticates successfully (reads keep working); it is
    /// up to each write handler to call [`Requester::require_not_suspended`] before performing a
    /// write the spec says suspension blocks (`M_USER_SUSPENDED`).
    pub suspended: bool,

    /// Set when this request was authenticated with an application service token. `None` means
    /// an ordinary user (or guest) session.
    pub appservice: Option<AppserviceIdentity>,

    /// The internal id of the access token used, if this request was authenticated by one
    /// (as opposed to, say, an appservice token, which is identified by
    /// [`AppserviceIdentity::appservice_id`] instead). Used for token-scoped rate limiting, for
    /// `mark used` bookkeeping, and by `/logout` to invalidate exactly this session.
    pub access_token_id: Option<TokenHash>,

    /// Set when this request's access token was issued to an application service: the session
    /// an `m.login.application_service` login (or registration) made, as a bridge double
    /// puppeting a real user does (decision 0053). Unlike [`Requester::appservice`] the request
    /// is an ordinary user session in every other respect (no masquerading, no namespace
    /// checks, no appservice-only endpoints); this only attributes it to that appservice for
    /// rate limiting ([`Requester::rate_limit_exemption`]). Resolved against the appservice's
    /// current registration when the request is authenticated. `default` so that a requester
    /// serialized without it (a replica one release behind, over the mesh) still reads.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issued_by_appservice: Option<IssuingAppservice>,
}

/// The application service a user's access token was issued to
/// ([`Requester::issued_by_appservice`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssuingAppservice {
    /// The registration's `id`.
    pub appservice_id: String,
    /// The registration's `rate_limited` flag when the request was authenticated. `false` means
    /// the session is exempt from rate limiting, as the appservice's own requests are, unless an
    /// administrator gave the user a rate-limit override.
    pub rate_limited: bool,
}

/// Why a request is exempt from rate limiting ([`Requester::rate_limit_exemption`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RateLimitExemption {
    /// The request is the appservice's own (its `as_token`, masquerading or not) and its
    /// registration says `rate_limited: false`: exempt from every limit, a per-user override
    /// included (as Synapse, which checks the appservice before the override).
    Appservice,
    /// The request's access token was issued to an appservice whose registration says
    /// `rate_limited: false` (a bridge's double puppet): exempt from the limits the
    /// appservice's own requests are exempt from, unless an administrator set a rate-limit
    /// override for the user, which then applies -- the account is a real person's, and an
    /// override on it is a deliberate choice about it.
    IssuedToAppservice,
}

/// The application service identity asserted for a request, including MSC3202 device
/// masquerading.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppserviceIdentity {
    /// The appservice's registration id (`id` in its registration YAML), used for rate-limit
    /// exemption, audit and the `authenticated_entity` shown to abuse tooling.
    pub appservice_id: String,

    /// The appservice's own `sender_localpart` user, i.e. who is acting when no `user_id` query
    /// parameter masquerades as someone else.
    pub sender: OwnedUserId,

    /// True if the request masqueraded as a `user_id` other than `sender` (still validated to be
    /// inside the appservice's registered namespaces before a `Requester` is ever constructed).
    pub masqueraded_user: bool,

    /// The device masqueraded via `device_id`/`org.matrix.msc3202.device_id`, if any. Already
    /// validated to exist for `Requester::user_id` (Synapse rejects unknown devices here with
    /// `M_UNKNOWN_DEVICE` before a requester is built; see `crate::middleware`).
    pub masqueraded_device_id: Option<OwnedDeviceId>,

    /// Copied from `AppserviceRecord::rate_limited` at authentication time. `false` means every
    /// request from this appservice is exempt from rate limiting.
    /// `docs/rfcs/0009-appservice-identity-capability-flags.md` point 1.
    pub rate_limited: bool,

    /// Copied from `AppserviceRecord::msc4190_enabled` at authentication time. `true` enables the
    /// MSC4190 device-management branch in `crate::routes::devices`.
    /// `docs/rfcs/0009-appservice-identity-capability-flags.md` point 2.
    pub msc4190_enabled: bool,
}

impl Requester {
    /// The entity that actually presented credentials: the appservice id for appservice
    /// requests, or `user_id` otherwise. Distinct from `user_id` whenever an appservice
    /// masquerades as a user — rate limiting, audit and abuse tooling should key on this, not on
    /// `user_id`, so that one appservice with many masqueraded users is throttled as one entity.
    #[must_use]
    pub fn authenticated_entity(&self) -> String {
        match &self.appservice {
            Some(app) => app.appservice_id.clone(),
            None => self.user_id.to_string(),
        }
    }

    /// Whether, and why, this request is exempt from rate limiting: its appservice's
    /// registration (or that of the appservice its access token was issued to) says
    /// `rate_limited: false`. Every rate-limit check that exempts appservices asks this, through
    /// [`Requester::exempt_from_rate_limits`] when it knows about per-user overrides.
    #[must_use]
    pub fn rate_limit_exemption(&self) -> Option<RateLimitExemption> {
        if self.appservice.as_ref().is_some_and(|a| !a.rate_limited) {
            Some(RateLimitExemption::Appservice)
        } else if self
            .issued_by_appservice
            .as_ref()
            .is_some_and(|a| !a.rate_limited)
        {
            Some(RateLimitExemption::IssuedToAppservice)
        } else {
            None
        }
    }

    /// True if this request skips a rate limit, given whether the user has an administrator's
    /// rate-limit override (`user_has_override`; `false` for a limit no override applies to).
    /// The order: an appservice's own request is exempt even over an override; a session issued
    /// to an appservice is exempt unless there is an override; anyone else is not exempt.
    #[must_use]
    pub fn exempt_from_rate_limits(&self, user_has_override: bool) -> bool {
        match self.rate_limit_exemption() {
            Some(RateLimitExemption::Appservice) => true,
            Some(RateLimitExemption::IssuedToAppservice) => !user_has_override,
            None => false,
        }
    }

    /// A plain, non-guest, non-admin user requester with no device — convenience for tests and
    /// for internal call paths (background jobs, imports) that act as a user without going
    /// through HTTP authentication.
    #[must_use]
    pub fn for_user(user_id: OwnedUserId) -> Self {
        Self {
            user_id,
            device_id: None,
            is_guest: false,
            is_admin: false,
            shadow_banned: false,
            suspended: false,
            appservice: None,
            access_token_id: None,
            issued_by_appservice: None,
        }
    }

    /// `403 M_USER_SUSPENDED` if this requester's account is suspended. Write handlers that the
    /// spec forbids to a suspended account (profile changes, room writes, account changes; not
    /// `/logout`, which must keep working) call this before doing anything else.
    pub fn require_not_suspended(&self) -> Result<(), crate::error::MatrixError> {
        if self.suspended {
            Err(crate::error::MatrixError::user_suspended())
        } else {
            Ok(())
        }
    }
}

/// The wire-serializable projection of [`Requester`] carried across the cluster mesh (track 03).
/// Currently identical to `Requester` itself, since every field is already mesh-safe; kept as a
/// distinct name so track 03's forwarding envelope has a stable type to name in its own docs
/// regardless of how `Requester` grows.
pub type RequesterContext = Requester;

#[cfg(test)]
mod tests {
    use super::*;
    use ruma::user_id;

    #[test]
    fn authenticated_entity_is_user_id_for_ordinary_requests() {
        let r = Requester::for_user(user_id!("@alice:example.org").to_owned());
        assert_eq!(r.authenticated_entity(), "@alice:example.org");
    }

    #[test]
    fn authenticated_entity_is_appservice_id_when_masquerading() {
        let mut r = Requester::for_user(user_id!("@bot_bob:example.org").to_owned());
        r.appservice = Some(AppserviceIdentity {
            appservice_id: "irc-bridge".to_string(),
            sender: user_id!("@irc-bridge:example.org").to_owned(),
            masqueraded_user: true,
            masqueraded_device_id: None,
            rate_limited: true,
            msc4190_enabled: false,
        });
        assert_eq!(r.authenticated_entity(), "irc-bridge");
        assert_eq!(r.user_id, user_id!("@bot_bob:example.org"));
    }

    /// Decision 0053's order: an appservice's own request is exempt even over a per-user
    /// override; a session issued to an appservice only without one; a `rate_limited: true`
    /// registration exempts neither.
    #[test]
    fn rate_limit_exemptions_and_overrides() {
        let identity = |rate_limited| AppserviceIdentity {
            appservice_id: "imessage".to_string(),
            sender: user_id!("@imessagebot:example.org").to_owned(),
            masqueraded_user: true,
            masqueraded_device_id: None,
            rate_limited,
            msc4190_enabled: false,
        };
        let issued = |rate_limited| IssuingAppservice {
            appservice_id: "imessage".to_string(),
            rate_limited,
        };
        let plain = Requester::for_user(user_id!("@brandon:example.org").to_owned());
        assert_eq!(plain.rate_limit_exemption(), None);
        assert!(!plain.exempt_from_rate_limits(false));

        let own = Requester {
            appservice: Some(identity(false)),
            ..plain.clone()
        };
        assert_eq!(
            own.rate_limit_exemption(),
            Some(RateLimitExemption::Appservice)
        );
        assert!(own.exempt_from_rate_limits(false));
        assert!(own.exempt_from_rate_limits(true));
        let limited_own = Requester {
            appservice: Some(identity(true)),
            ..plain.clone()
        };
        assert!(!limited_own.exempt_from_rate_limits(false));

        let puppet = Requester {
            issued_by_appservice: Some(issued(false)),
            ..plain.clone()
        };
        assert_eq!(
            puppet.rate_limit_exemption(),
            Some(RateLimitExemption::IssuedToAppservice)
        );
        assert!(puppet.exempt_from_rate_limits(false));
        assert!(!puppet.exempt_from_rate_limits(true));
        let limited_puppet = Requester {
            issued_by_appservice: Some(issued(true)),
            ..plain
        };
        assert_eq!(limited_puppet.rate_limit_exemption(), None);
        assert!(!limited_puppet.exempt_from_rate_limits(false));
    }

    /// A requester serialized by a replica that predates `issued_by_appservice` still reads
    /// (the mesh carries it as JSON), and one without it is serialized as before.
    #[test]
    fn a_requester_without_the_issuing_appservice_still_reads() {
        let r = Requester::for_user(user_id!("@alice:example.org").to_owned());
        let json = serde_json::to_value(&r).unwrap();
        assert!(json.get("issued_by_appservice").is_none(), "{json}");
        let back: Requester = serde_json::from_value(json).unwrap();
        assert_eq!(back.issued_by_appservice, None);
        let puppet = Requester {
            issued_by_appservice: Some(IssuingAppservice {
                appservice_id: "imessage".to_string(),
                rate_limited: false,
            }),
            ..r
        };
        let back: Requester =
            serde_json::from_value(serde_json::to_value(&puppet).unwrap()).unwrap();
        assert_eq!(back, puppet);
    }

    #[test]
    fn requester_round_trips_through_json() {
        let mut r = Requester::for_user(user_id!("@alice:example.org").to_owned());
        r.is_admin = true;
        r.access_token_id = Some(TokenHash::of("syt_whatever"));
        let json = serde_json::to_string(&r).unwrap();
        let back: RequesterContext = serde_json::from_str(&json).unwrap();
        assert_eq!(r, back);
    }
}
