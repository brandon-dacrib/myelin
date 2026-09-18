# 10. Push and notifications

Wave 2, code starts week 8; the rules engine starts day one.

**Expert profile.** Matrix push rules, push gateways (APNs, FCM, WebPush, UnifiedPush), email templating, queueing with retries.

**Mission.** Correct and fast push-rule evaluation, notification counts that sync can trust, HTTP and email pushers, and an optional built-in push gateway. See `PLAN.md` sections 5.5 and 10.

**Owns.** `hs-push`: the rules engine with every spec condition and default rule plus MSC3664, MSC3381, MSC4210 and MSC4028, per-user compiled rule sets with invalidation on the push-rules stream, notification and highlight counts per room and per thread, HTTP pushers compatible with Sygnal's `/_matrix/push/v1/notify` including `event_id_only` and MSC3881, email pushers with digests rendered by `minijinja` from templates operators can customize, unsubscribe links, `/notifications`, `/pushrules`, `/pushers`; `hs-pushgw`: the optional gateway component (APNs via `a2`, FCM HTTP v1, WebPush, UnifiedPush passthrough).

**Provides.** Counts to 05; the pushers stream.

**Consumes.** 04 publish stream (events and the state needed by conditions), 05 (receipts reset counts), 07, 01, 13 (email config and templates), 03 (job leases for digests).

**Day-one work.** The rules engine on Ruma's `push` types (MIT) with spec vectors and Synapse-observed behavior re-expressed as tests (Synapse's evaluator is AGPL and is not copied); the counts data model; the pusher queue design with retries and backoff.

**Phase 0 deliverables.** Engine and counts on the in-memory backend; benchmarks for evaluating one event across many users.

**Phase 1 and 2 deliverables.** Pushers, email, the gateway, the API endpoints, batched evaluation on the room owner with 04, admin views with 15.

**Definition of done.** Complement push tests green; golden rendering of all templates; the gateway tested against fake APNs and FCM; differential tests against Synapse for evaluation outcomes on an event corpus.

**References.** Spec push rules and push gateway API; `refs/ruma/crates/ruma-common/src/push/`; `refs/synapse/synapse/push/` and `refs/synapse/rust/src/push/` (behavior only, AGPL); `refs/synapse/synapse/res/templates/` for the template set operators customize; Sygnal as a behavioral reference.

**Open questions to settle first.** Counts computed on the room owner versus the user session; digest scheduling under job leases.

**Risks.** Count drift between push and sync is highly visible; one source of truth, tested end to end.
