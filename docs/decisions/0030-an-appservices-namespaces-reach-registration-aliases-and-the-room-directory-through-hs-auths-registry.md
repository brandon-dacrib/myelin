# 0030: 2026-10-04: an appservice's namespaces reach registration, aliases and the room directory through `hs-auth`'s registry trait, and a ghost acts once it is registered

Status: accepted (track 11; touches 07's `hs-auth` and 04's `hs-room` at named call sites).

## The problem

Sytest's `tests/60app-services/` failed ten tests on `c2d74174` because an appservice's
registration meant nothing outside delivery: a person could register `@astest-…` or create
`#astest-…` in a bridge's exclusive namespaces; an alias nobody had made was never asked of the
bridge (`GET /_matrix/app/v1/rooms/{alias}`), nor an invited ghost
(`GET /_matrix/app/v1/users/{userId}`); `/thirdparty/*` and `PUT
/directory/list/appservice/{networkId}/{roomId}` were not routes; `third_party_instance_id` and
`include_all_networks` were ignored by `/publicRooms`; and an appservice deactivating its ghost
was asked for UIA. Registration lives in `hs-auth`, aliases and `/publicRooms` in `hs-room`, and
neither depends on `hs-appservice` (which depends on `hs-auth`).

## What was chosen

1. **The seam is `hs_auth::appservice::AppserviceRegistry`**, the trait `hs-auth` and `hs-room`
   already hold (`AuthState::appservices`). Four methods with default bodies were added:
   `exclusive_user_owner`, `exclusive_alias_owner`, `query_room_alias` and `network_room_ids`.
   The defaults answer "nobody", so every existing implementation and test fixture is unchanged;
   `hs_appservice::auth_registry::RegistryAppserviceAdapter` answers from the registry and, with
   `with_queries`, asks the bridges. No new crate edge.
2. **`hs-appservice` owns what is the appservices'**: the network room directories (keyspace
   `hs_appservice.network_rooms`, removed with the appservice), the `/thirdparty/*` and
   `/directory/list/appservice/…` routes (`hs_appservice::client_routes`, mounted by `hs serve`
   under `v3` and `r0`), the questions to bridges (`QueryService::user_exists`,
   `room_alias_exists`, `protocols`, `thirdparty_lookup`, counted in
   `hs_appservice_queries_total`), and asking about an unknown local user before an event naming
   them is delivered (`Pump::with_user_queries`, Synapse's `_check_user_exists`).
3. **The call sites outside track 11 are the smallest that use the seam**: `/register` and
   `/register/available` refuse an exclusive user ID (`400 M_EXCLUSIVE`, also for another
   appservice's); `/account/deactivate` skips UIA for an appservice; `PUT /directory/room`
   refuses an exclusive alias to anyone but its appservice (`RoomError::Exclusive`); a local alias
   the directory does not hold is asked of the bridge before it is `404`
   (`hs_room::routes::aliases::resolve_local_alias`, used by `GET /directory/room` and `POST
   /join`); `/publicRooms` reads `third_party_instance_id` and `include_all_networks`.
4. **A ghost acts once it is registered.** `hs-auth`'s appservice authentication refuses a
   `user_id` masquerade as a user with no account on this server (`403`, "Application service has not
   registered this user"), as Synapse's `Auth.get_appservice_user` does; the appservice's bot is
   exempt. Without it an unregistered ghost could send events, and the pump would then ask the
   bridge about a user it never provided (in Sytest, a question nobody answered held delivery for
   ten seconds). Every bridge registers its ghosts first (mautrix treats `M_USER_IN_USE` as done);
   the real mautrix-whatsapp and mautrix-signal stories pass with it.
5. **Protocol metadata is kept five minutes** (`PROTOCOL_CACHE_MS`; Synapse keeps it an hour):
   a client's protocol list and then each protocol cost one round of questions, and a bridge's
   changed configuration shows within minutes.

## Consequences

- The pump is one task for every room: a bridge that never answers `/users/{userId}` holds
  delivery for the query timeout (ten seconds) once per unknown user per minute
  (`UNKNOWN_USER_RETRY_MS`). Synapse blocks the same way, per room. Accepted for now.
- Synapse's `instance_id` spelling (`{appservice id}|{network id}`) is what a client passes back
  as `third_party_instance_id`; Sytest relies on it.
