# RFC 0009: appservice capability flags on `AppserviceRecord`/`AppserviceIdentity`

Status: proposed. Author: track 11 (appservices and bridges). Affects: track 07 (`hs-auth`), track
15 (admin/console, rate limiter wiring), track 04 (room actor, once it exists).

## Problem

`hs-auth::appservice::AppserviceRecord` (`crates/hs-auth/src/appservice.rs`) and
`hs-auth::requester::AppserviceIdentity` (`crates/hs-auth/src/requester.rs`) currently carry only
`appservice_id`, `sender`, and (`AppserviceRecord` only) `user_namespaces`. Track 11's real
registry (`hs-appservice::registry::Registry`, `crates/hs-appservice/src/auth_registry.rs`) now
implements `AppserviceRegistry` against real registrations that carry several more capability
flags Synapse's own request handling consults per-request, none of which a handler downstream of
`Requester` can currently see:

1. **Rate-limit exemption** (`PLAN.md` section 8.1 point 7): a registration's `rate_limited: false`
   must make every request `hs-http`'s rate limiter sees from that appservice (its own sender, and
   every masqueraded user) exempt. Today nothing on `Requester`/`AppserviceIdentity` says whether
   the authenticating appservice is rate-limited or not, so the rate limiter (wherever it ends up
   living — `hs-http::ratelimit` or `hs-auth::ratelimit`) has no signal to key off.
2. **MSC4190 device management** (`PLAN.md` section 8.1 point 2): `PUT /devices/{deviceId}` must
   create the device instead of 404ing, and `DELETE /devices/{deviceId}` must skip UIA
   re-authentication, when the requester is an appservice whose registration set
   `io.element.msc4190: true`. `crates/hs-auth/src/routes/devices.rs`'s `put_device` (unconditional
   404 on an unknown device) and `delete_device` (unconditional `reauth::run`) have no such branch
   today, and have no way to learn this flag even if they did.

`hs-bridge-conformance`'s MSC4190 and rate-limit-exemption scenarios
(`crates/hs-bridge-conformance/`) are written against this gap and currently only assert the
registry-level flag round-trips correctly (`Registry`/`AppserviceRow`), not that a real request is
actually exempted or allowed to create a device — see
`docs/status/11-appservices-and-bridges.md`'s "Known gaps" for the precise scenarios blocked.

## Proposed interface

Add two fields, both populated by `hs-appservice::auth_registry::RegistryAppserviceAdapter` from
the registration row it already has in hand:

```rust
// crates/hs-auth/src/appservice.rs
pub struct AppserviceRecord {
    pub appservice_id: String,
    pub sender: OwnedUserId,
    pub user_namespaces: Vec<NamespaceRule>,
    pub rate_limited: bool,      // NEW — default true if a track wants a non-breaking rollout
    pub msc4190_enabled: bool,   // NEW
}

// crates/hs-auth/src/requester.rs
pub struct AppserviceIdentity {
    pub appservice_id: String,
    pub sender: OwnedUserId,
    pub masqueraded_user: bool,
    pub masqueraded_device_id: Option<OwnedDeviceId>,
    pub rate_limited: bool,      // NEW — copied from AppserviceRecord at authentication time
    pub msc4190_enabled: bool,   // NEW
}
```

`crate::middleware::authenticate_appservice` (`crates/hs-auth/src/middleware.rs`) copies both
fields from the looked-up `AppserviceRecord` onto the `AppserviceIdentity` it builds, the same way
it already copies `appservice_id` and `sender`.

Then:

- Whatever owns rate limiting checks `requester.appservice.as_ref().is_some_and(|a| !a.rate_limited)`
  and skips the limiter entirely for that request.
- `put_device`/`delete_device` in `crates/hs-auth/src/routes/devices.rs` check
  `requester.appservice.as_ref().is_some_and(|a| a.msc4190_enabled)` to take the MSC4190 branch.

## Why not solve it inside `hs-appservice` alone

`Requester` is constructed inside `hs-auth::middleware`, before any handler (including a
hypothetical `hs-appservice`-owned one) sees the request; there is no seam for another crate to
inject data into it after the fact without `hs-auth` carrying the fields itself. This is the same
reasoning the existing `crates/hs-auth/src/appservice.rs` module doc already gives for why the
`AppserviceRegistry` trait lives in `hs-auth` and not `hs-appservice`.

## Non-breaking rollout

Both new fields are plain `bool`s with obvious safe defaults (`rate_limited: true`,
`msc4190_enabled: false` — "assume nothing extra is granted" — matches every existing test
fixture's implicit expectation), so adding them is a mechanical, non-behavior-changing patch to the
two structs plus the four lines in `middleware.rs` and `devices.rs` that read them. Track 11 will
apply this patch itself if track 07 has not picked it up by the time both tracks are back in the
same integration window — this RFC exists so the interface is agreed first, per
`docs/workstreams/README.md`'s ownership rule (11 may not edit `hs-auth` unilaterally).
