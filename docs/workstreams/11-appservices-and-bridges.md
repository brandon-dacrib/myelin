# 11. Appservices and bridges

Wave 2 for the scheduler (week 8); the registry, the registration parser and the conformance suite start day one and are validated against Synapse first.

**Expert profile.** Bridge ecosystems (mautrix in depth), the appservice API and its MSCs, operational tooling, comfortable pairing with 15 on console pages.

**Mission.** Every homeserver feature bridges use, on by default, plus first-class management: registry, health, replay, conformance suite, real bridges in CI, and the semantics behind the `Bridge` custom resource. See `PLAN.md` section 8 and Appendix B.

**Owns.** `hs-appservice`: the store-backed registry with registration-file import, namespaces with Python-regex semantics (`fancy-regex`), exclusivity and claiming, the transaction scheduler per appservice with batching, retries, backoff, dead-letter and replay, MSC2409 ephemeral events, MSC3202 fields (both one-time-key-count spellings), MSC4203 to-device, MSC4190 device management, identity assertion and device masquerading with 07, `ts` massaging with 04, ping in both directions, `/thirdparty/*`, user and alias queries to appservices, MSC3983 and MSC3984 key proxies with 08, MSC4512 with 06, rate-limit exemption, health and backlog metrics and admin API, `hs appservice` CLI, `hs-bridge-conformance`, the real-bridge CI, the `Bridge` resource semantics with 12, the console bridge pages with 15, the native `com.devture.shared_secret_auth` login provider with 07.

**Provides.** The registry API for 12's operator and 15's console; the scheduler; the conformance suite (also runnable against Synapse as a control).

**Consumes.** 04 publish stream, 07, 08, 06, 03 (appservice shards), 01, 14.

**Day-one work.** Registry model and registration parser covering every field in Appendix B; the conformance suite written from Appendix B against a stub and then run against Synapse 1.161 in Docker to prove the suite itself is right; Compose files for `ergo` plus `mautrix-irc` and for Zulip plus `mautrix-zulip`, first targeting Synapse.

**Phase 0 deliverables.** Registry, parser and CLI; conformance suite v0 green against Synapse; scheduler design.

**Phase 1 and 2 deliverables.** Scheduler with all MSCs; ping; queries; key proxies; health and replay; admin API; real bridges green against us; direct media with 09 and 06; encrypted-bridging soak in appservice mode and sync mode; `Bridge` resource spec with 12; console pages with 15.

**Definition of done.** Conformance green on us with Synapse as the control; `mautrix-irc`, `mautrix-zulip`, one `mautrix-python` bridge, `matrix-hookshot`, `heisenbridge` and `matrix-appservice-irc` green in CI; encrypted soak green in both modes; a bridge that is down for an hour replays without loss or duplicates.

**References.** Spec appservice API; MSC2409, MSC3202, MSC4203, MSC4190, MSC3983, MSC3984, MSC4512; `refs/mautrix-go/appservice/`, `refs/mautrix-go/bridgev2/matrix/`, `refs/mautrix-go/versions.go`, `refs/mautrix-python/mautrix/appservice/`; `refs/synapse/synapse/appservice/` and `refs/synapse/synapse/handlers/appservice.py` (behavior only, AGPL); `refs/conduit/conduit/src/service/admin/` for the register-by-command UX; the mautrix docs on end-to-bridge encryption and double puppeting.

**Open questions to settle first.** Batching windows and per-appservice concurrency; precedence between registry rows and registration files; how sync-mode bots interact with ephemeral push.

**Risks.** Field spellings and ordering that mautrix silently depends on; the control run against Synapse is what catches them.
