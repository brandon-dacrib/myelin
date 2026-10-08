# 0033: 2026-10-08: a bridge is asked about its users by its own delivery worker, and each bridge's queue is a gauge

Status: accepted (track 11). Amends decision 0030's second point and replaces its first
consequence.

## The problem

Decision 0030 made the room pump ask a bridge about an unknown local user of its namespace (`GET
/_matrix/app/v1/users/{userId}`) before queueing an event naming them, as Synapse's
`_check_user_exists` does. The pump is one task for every room and every appservice, so a bridge
that took the connection and never answered held every bridge's delivery for the query timeout
(ten seconds), once per unknown user per minute. In Sytest an unanswered question failed two
unrelated tests behind it; on a server with a WhatsApp bridge stuck that way, the Signal bridge
would hear nothing for ten seconds at a time. Delivery itself was already per appservice
(`hs_appservice::delivery`, one worker each); the shared pump was the one place where a bridge's
slowness reached another's.

## What was chosen

1. **The question moves to the appservice's own delivery worker.** `Scheduler::drain`, just
   before it sends a batch, asks that appservice (and only it) about each user the batch's events
   name (sender, a membership's target) who is local, in its user namespace, not its bot, and has
   no account (`hs_appservice::known_users::UserQueries`, installed with
   `Scheduler::with_user_queries`), and waits. Several unknown users in one batch are asked at
   once, so they cost one timeout, not one each. A user an appservice did not provide is not asked
   of it again for a minute (`UNKNOWN_USER_RETRY_MS`, now kept per appservice).
2. **This asks everyone Synapse asks.** An event naming a user of an appservice's namespace is
   always one that appservice is sent (`pump::interested`: its user is the sender or the target),
   so asking each appservice before its own delivery covers every appservice the old question
   covered, and the bridge still hears of the user only after it was asked.
3. **The pump waits on nothing remote.** It reads rooms and local stores and queues, as before;
   ordering per appservice is the queue's (one worker drains it in order). The shard gating is
   unchanged: the pump on the owner of the global shard, an appservice's worker (and so its
   questions) on the owner of that appservice's shard.
4. **`QueryService::user_exists` and `room_alias_exists` ask their appservices at once** and take
   the first yes, so on the request path (an alias lookup, Complement's bridge-user join) one
   bridge that never answers does not hold back another's answer.
5. **Each bridge's queue is observable.** `hs_appservice_queue_depth{appservice}` (entries waiting,
   a backing-off one included), `hs_appservice_queue_dead_lettered{appservice}` and
   `hs_appservice_queue_oldest_age_seconds{appservice}` are read from the queue on every scrape
   (`hs_appservice::metrics::QueueCollector`), by the replica that delivers to the appservice
   only, so a cluster never reports one queue twice. The admin API's `AppService.queue` (OpenAPI
   0.1.11) carries the same numbers on every row of `appservices.list`, and the bridges list shows
   them ("Waiting to send").

## Consequences

- A bridge that never answers holds its own transactions for the timeout, once per unknown user
  per minute; every other bridge is unaffected (`hs-cli`'s `tests/appservice_isolation.rs`: the
  other bridge hears the room within a second while the first one's question is open).
- The question is asked when the batch is about to be sent, not when the event is queued. For a
  bridge that is behind, that is later than Synapse would ask; the bridge still learns of the
  user before it hears of them, which is what the spec and Sytest rely on.
- A worker asks again before each attempt of a failing batch only if the user is still unknown
  and the minute has passed; a provided user is registered by then and is not asked about again.
