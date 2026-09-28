# 0014. Moderation flags are recorded on the account and enforced where the writes happen

Date: 2026-09-28. Status: accepted. Tracks: 15 (admin API), 07 (auth), 04 (rooms), 09 (media).

## Context

The admin API had `users.suspend`, `users.shadow_ban` and `users.rate_limit.*` declared and
answering 501. `hs-auth`'s `UserRecord` already carried `suspended` and `shadow_banned`, and the
`Requester` carried both, but nothing refused or dropped anything because of them, and there was
no per-user rate limit anywhere: no route in the server enforces `rate_limits.message` at all.

## Decision

1. **The account record is the one place the decision lives.** `suspended`, `shadow_banned` and
   the new `rate_limit_override` (`RateLimitOverrideRecord { per_second, burst_count }`) are
   fields of `hs_auth::store::UserRecord`, set through `UserStore::{set_suspended,
   set_shadow_banned, set_rate_limit_override}`. The admin API reaches them through
   `hs_admin::user_moderation::UserModerationSource`, implemented by
   `hs_auth::admin_moderation::AuthStoreUserModeration`.
2. **Enforcement is in the write routes, not in the admin API.**
   - Suspension (MSC3823 / Matrix 1.14): `403 M_USER_SUSPENDED` from `hs-room`'s send, state,
     createRoom, join, invite, knock, kick, ban, unban, upgrade and alias routes, from
     redacting somebody else's event, from `hs-auth`'s profile writes and from `hs-media`'s
     uploads. Leaving, forgetting, redacting one's own events, signing out and every read keep
     working.
   - Shadow-ban (Synapse's behavior): `hs-room` answers sends, state events and redactions
     with a made-up event ID and invitations with `{}`, and does none of them; createRoom's
     invitations are dropped. Joins and leaves are real.
   - Rate-limit override: `hs_room::moderation::SendLimiter` (one token bucket per user with an
     override, in the `RoomRegistry`) applies to sends, state events and redactions:
     `429 M_LIMIT_EXCEEDED` with `retry_after_ms`. `per_second: 0` exempts. Without an
     override nothing is limited, because the server-wide `rate_limits.message` bucket is still
     not enforced; enforcing it changes every client's pace and is a separate decision.
3. **Tests and other callers constructing a `Requester` by hand** get `suspended: false` and
   `shadow_banned: false` as before; the limiter reads the override from the store per send
   (one extra point read) rather than widening `Requester`, which is `Eq` and crosses the mesh.

## Consequences

- In cluster mode the limiter is per replica. With room-sharded routing that is close to
  per-room; a user spamming many rooms on many replicas gets a multiple of the override.
- A support session minted by `users.login_as` is a device whose ID starts with
  `ADMINSUPPORT`; that prefix is how `users.sessions.list` marks it.
- Every room write refused, swallowed or throttled this way is counted in
  `hs_room_moderated_writes_total{outcome}` (`suspended`, `shadow_banned`, `rate_limited`);
  the per-write logs are `debug`, so a shadow-banned spammer cannot fill the log.
