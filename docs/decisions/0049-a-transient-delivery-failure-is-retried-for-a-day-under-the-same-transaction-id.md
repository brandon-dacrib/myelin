# 0049: 2026-10-15: a transient delivery failure is retried for a day under the same transaction id

Status: accepted (track 11; touches 15's admin API (OpenAPI 0.1.18) and 16's bridge pages).

## The problem

On the demo on 2026-10-15 (v0.3.22) the server logged `could not deliver to this appservice; it
will be tried again ... count: 20, dead_lettered: 20, operation timed out` for `discord-brandon`
every 31 seconds, mautrix-discord logged "Event channel is full" 3,787 times in ten minutes, and
the Bridges page called Discord "Degraded" without saying why.

What happened, from the server's log, the bridge's log and the queue gauges:

1. At 19:56 UTC Discord's 260 dead letters were replayed from its page. They were the leftovers
   of the 2026-10-12 flood (decision 0046: the bridge's registration claimed its owner, so it was
   sent every room he was in): queued 2026-10-12 19:34 (`hs_appservice_queue_oldest_age_seconds`
   260,626 at 19:58), `m.room.encrypted` events of rooms the bridge does not bridge, the owner's
   to-device messages and one-time-key counts.
2. A replay kept each entry's spent attempt count, so each batch had one attempt left.
3. A batch was twenty queue entries, and an entry is one room's page of up to a hundred events:
   the transactions carried 624, 1,060, 1,026, 913, 781 ... events. mautrix-go's transaction
   handler does not answer until every event is in its event channel, and the bridge spends
   3 + 22 seconds on each event it cannot decrypt; it took 91 seconds to dispatch the first one.
4. The 30-second request timeout fired on every transaction; with no attempts left, each batch was
   dead-lettered at once and the next one sent, 13 in six and a half minutes. The bridge went on
   dispatching them after the server hung up (14 "Finished dispatching" by 20:25), so everything
   the server counted as failed was in fact delivered, and the 260 dead letters were back.
5. The transaction ids were 20 apart because each was a new batch of twenty entries, not a retry.
   The one retry seen (3624, after the server restarted at 19:58) reused its id, as the spec
   requires. But a retry re-formed its batch: a failed batch that had fewer than twenty entries
   and grew before its retry went under a new id with the old events in it.

Separately, the same day the Slack bridge's pod restarted for five minutes and its transaction
was dead-lettered after ten attempts, four minutes: any failure counted against ten attempts.

## What was chosen

1. **A retry is the same transaction.** Each entry of a failed batch records the batch's id
   (`QueuedTransaction::txn_id`); the retry sends exactly those entries under it.
2. **A failure costs what it says.** A transient failure (no connection, timed out, 5xx, 408,
   429) is retried, backing off to five minutes, until it has failed for a day
   (`SchedulerConfig::give_up_after_ms`); a timeout waits at least a minute before its retry,
   since the bridge is probably still working on it. A transaction the bridge refuses (any other
   status) is dead-lettered after ten attempts, as before, so as not to hold the queue for a day.
   Synapse never gives up on a transaction; a day keeps one poisoned entry from holding a bridge
   forever while riding out restarts and node outages.
3. **Transactions are small, and smaller when the bridge is slow.** A new transaction carries at
   most 100 items (events, ephemeral events, to-device messages; Synapse's cap), the first entry
   always. The limit halves on each timeout and doubles back on each delivery
   (`hs_appservice_batch_item_limit`).
4. **The request timeout is 60 seconds**, up from 30.
5. **A replay gives a fresh retry budget** (attempts, first failure and id reset).
6. **Dead letters are kept until an operator acts**, and the operator can now see what they are
   and choose: the backlog page lists when each was queued, what it carries and its rooms, and
   `POST /api/v1/appservices/{id}/discard` deletes them (never an entry that waits), as replay
   sends them. They are not expired automatically: dropping a person's messages unseen is worse
   than a red number, and the page now says what the number is.
7. **Health says why.** `AppService.health_reason` and `AppServiceHealth.reason`; and a ping
   answered after the last failed delivery makes a bridge healthy again, so a bridge whose
   failures were all dead-lettered and that is sent nothing since is not "degraded" forever.

New metrics: `hs_appservice_transaction_seconds{appservice,outcome}`,
`hs_appservice_transaction_failures_total{appservice,reason}`,
`hs_appservice_transaction_retries_total`, `hs_appservice_dead_lettered_total`,
`hs_appservice_batch_item_limit`. The failure log names the transaction id, the attempt, the
reason and when it is tried again, and says separately when a bridge is too slow and when entries
are dead-lettered; replays and discards are logged.

## Consequences

- A bridge that is down for less than a day loses nothing; its queue (and every bridge's own
  ordering) waits for it. The queue gauges show the backlog meanwhile.
- A replay of an incident's leftovers sends the bridge old events again; the page says so before
  it does, and discarding is the usual answer for them.
- The queue row has two new fields with serde defaults; rows written before read as never failed
  under this scheme.
