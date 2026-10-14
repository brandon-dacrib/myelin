# 0047: An appservice removes its users' devices without user-interactive auth, MSC4190 or not

Date: 2026-10-14. Track 11 (found), track 07 (`hs-auth`, where it lives).

## Context

RFC 0009 let `DELETE /devices/{deviceId}` skip user-interactive auth only for an appservice whose
registration sets `io.element.msc4190`, and `POST /delete_devices` never skipped it. The bridge
manager removes a bot device a bridge left behind (decision 0041) with the bridge's own token. For
the demo's mautrix-imessage bridge, a legacy mautrix-go bridge that logs its bot in with
`m.login.application_service` and does not set MSC4190, that `DELETE` got `401` (2026-10-14,
device `yLLbqe7oX4`), so the device stayed, the instance's identity step kept failing, and the
bridge was sent the device's MSC3202 key counts in every transaction.

## Decision

Any appservice requester skips user-interactive auth for `DELETE /devices/{deviceId}` and
`POST /delete_devices` (`hs_auth::routes::devices::skips_uia_to_delete_devices`). This is
Synapse's rule (`requester.app_service_id` in `DeviceRestServlet` and `DeleteDevicesRestServlet`).
Creating a device with `PUT /devices/{deviceId}` stays MSC4190's alone, as in Synapse.

## Consequences

An appservice may remove devices of any user in its namespaces, a double-puppeting registration's
owner included. It could already log in as those users, so this grants nothing new. Ordinary users
still need user-interactive auth for both routes.
