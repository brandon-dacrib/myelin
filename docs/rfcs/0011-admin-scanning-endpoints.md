# RFC 0011. Admin endpoints for content scanning

Date: 2026-09-18. Status: draft for review by track 15. Owner: track 09 (proposal only — track 15 owns `crates/hs-admin/openapi/openapi.yaml` and the router; this document is the wire-shape proposal for that seam, not an edit to it). Companion: `docs/rfcs/0008-content-scanning.md` section 8 ("Admin API: list recent verdicts, rescan a media item, rescan everything matching a filter after a signature update, and show current provider health. These extend the existing media resource in the admin OpenAPI document rather than forming a new one.") and `docs/rfcs/0004-admin-api.md` (the conventions this document follows throughout — read that first if anything here is unclear).

## 1. Motivation

`crates/hs-media/src/scanning/admin.rs` already defines the Rust interface these endpoints call into (`ScanAdmin`), because content scanning (`docs/rfcs/0008-content-scanning.md`, wired into the upload path this session — see `docs/status/09-media.md`) is otherwise invisible to an operator: nothing in the ordinary upload/download flow surfaces which provider is configured, whether it is reachable, what it has found, or lets an operator force a re-check after a ClamAV signature update or a provider config change. RFC 0004 section 4.6 already sketches the `/media` resource family (list, get, delete, quarantine/unquarantine, protect/unprotect, bulk delete, purge remote cache); this document is the same family's scanning-specific extension, per RFC 0008 section 8's explicit instruction to extend `/media` rather than invent a new resource.

## 2. What already exists to build on

- `crates/hs-media/src/scanning/admin.rs`: `ScanAdmin` (`recent_verdicts`, `rescan`, `rescan_many` with a default fan-out, `provider_health`) and `ProviderHealth`. **Trait only, no implementation** — see section 7.
- `crates/hs-media/src/scanning/audit.rs`: `AuditEntry`/`AuditKind` (`Infected`, `ScannerError`, `ReplacementApplied`, `AppserviceBypass` — **not** `Clean`; a clean verdict is never audited, per RFC 0008 section 8's "audit entries for every infected and every error verdict" plus section 3.4's replacement audit and section 4's bypass audit) and `InMemoryAuditSink` (bounded ring buffer, the backing store `recent_verdicts` reads from today).
- The existing `/media` OpenAPI paths (`crates/hs-admin/openapi/openapi.yaml`, already implemented by track 15): `GET /media`, `GET /media/{server_name}/{media_id}`, `DELETE /media/{server_name}/{media_id}`, `.../protect`, `.../unprotect`, `.../quarantine`, `.../unquarantine`, `POST /media/delete` (bulk, Task), `POST /media/purge-remote-cache` (Task). This document's endpoints slot into the same file, the same `Media` tag, the same `MediaItem` schema for anything that returns a media item, and the same scopes (`moderation:read`/`moderation:write`) that already govern this family.

## 3. Two audit logs, not one — read this before designing the handler

`GET /media/scan-verdicts` (section 4.1) reads `hs_media::scanning::audit::AuditSink` entries — the record of what the **scanner** found. Calling any mutating endpoint in this document (`rescan`, bulk `rescan`) *also* produces an ordinary `hs-admin` `AuditEntry` (RFC 0004 section 9, D15.11) — the record of what the **operator** did. These are different logs, different schemas, different retention. A handler implementing this RFC writes both: the admin-API audit entry unconditionally (D15.11 requires this of every mutation), and, if the rescan changes anything, the `hs_media` audit entry is written by `ScanEngine::evaluate` itself (it already does this internally — see `crates/hs-media/src/scanning/engine.rs`), not by the handler.

## 4. Endpoints

All under `/api/v1`, the `Media` tag, following RFC 0004 section 3 exactly (pagination envelope, `Idempotency-Key` on `POST`, the shared Problem catalog, `X-Request-Id`).

### 4.1 `GET /media/scan-verdicts` — list recent verdicts

Operation id `media.scan_verdicts.list`. Scope `moderation:read`.

A paginated read-through of `ScanAdmin::recent_verdicts` (RFC 0004 section 3.3's envelope). Newest first (`sort=-recorded_at`, the only supported sort — `AuditEntry` carries no other useful ordering key today).

Query parameters, all optional, in addition to the standard `limit`/`cursor`/`include_total`:

| Parameter | Type | Meaning |
|---|---|---|
| `kind` | enum, repeatable | `infected`, `scanner_error`, `replacement_applied`, `appservice_bypass`. Repeating means "any of" (RFC 0004 section 3.4). |
| `provider` | string | Exact match on `AuditEntry::provider`. |
| `server_name` | string | Exact match. |
| `media_id` | string | Exact match — the common case is "show me everything recorded about this one item," which is otherwise unreachable once an item scrolls off a page. |
| `recorded_after`, `recorded_before` | date-time | RFC 0004 section 3.4's time-range convention. |

Response: `Page<MediaScanVerdict>`.

```json
{
  "id": "01J8RQ7...",
  "recorded_at": "2026-09-18T10:04:05.123Z",
  "provider": "icap",
  "server_name": "example.org",
  "media_id": "AbCdEf0123456789ghijklmn",
  "kind": "infected",
  "signature": "Eicar-Test-Signature",
  "message": null,
  "replacement": null,
  "appservice_id": null
}
```

`signature` is present only for `kind: infected`. `message` only for `kind: scanner_error`. `replacement` (`{by, original_sha256, adapted_sha256}`) only for `kind: replacement_applied`. `appservice_id` only for `kind: appservice_bypass`. This mirrors `AuditKind`'s four variants exactly — a handler maps one to the other field by field, nothing inferred.

**Implementation note (see section 7, item 3):** `AuditEntry` has no stable identifier today; `id` above assumes one is added.

### 4.2 `POST /media/{server_name}/{media_id}/rescan` — rescan one item

Operation id `media.rescan_one`. Scope `moderation:write`. Path parameters `ServerName`, `MediaId` (existing shared parameters). Query parameter `apply_quarantine` (boolean, default `false`): when `true` and the fresh verdict is bad (infected, or an unscannable/scanner-error outcome the configured policy would treat as `block`/`quarantine`), the handler also calls the existing quarantine machinery (`MetadataStore::set_quarantined`, the same path `POST .../quarantine` uses) so an operator does not need two requests to act on the result of the first. `ScanAdmin::rescan` itself never changes servability — see its doc — so `apply_quarantine: false` (the default) is purely informational.

Request body: none.

Response `200`:

```json
{
  "server_name": "example.org",
  "media_id": "AbCdEf0123456789ghijklmn",
  "verdict": {
    "kind": "infected",
    "signature": "Eicar-Test-Signature",
    "details": null
  },
  "quarantine_applied": false
}
```

`verdict` is a `ScanVerdict` — a direct, complete mapping of `crate::scanning::types::Verdict` (open enum: `clean`, `infected`, `unscannable`, `replaced` — **not** `pending`, since `ScanEngine::evaluate`/`rescan` never returns it to a caller; see that method's doc). This is deliberately a different, richer schema than `MediaScanVerdict` (section 4.1): a rescan can come back `clean`, which is never audited and so never appears in the verdicts list.

Errors: the shared `NotFound` (unknown `server_name`/`media_id`) and a new case this family did not previously need — `422 unprocessable` with `detail: "content scanning is not configured on this server"` when `MediaRepository` has no `ScanEngine` attached at all (`mode: off`, the default). Track 15's handler should check this once, up front, rather than letting it surface as an opaque `500` from deep inside `ScanAdmin`.

### 4.3 `POST /media/rescan` — bulk rescan by filter (Task)

Operation id `media.rescan_bulk`. Scope `moderation:write`. This is RFC 0008 section 8's "rescan everything matching a filter after a signature update" — the motivating case is an operator who just updated ClamAV's virus database (or repointed `icap.host` at a different, better-configured gateway) and wants every recent upload re-checked against it, without waiting for the verdict cache's TTL (RFC 0008 section 6) to expire naturally.

Request body (all filters optional; an empty body means "everything," which a real deployment will almost never want — the handler should probably require at least one filter, returning `400 validation-failed` on an empty body, the same defensive choice `POST /media/delete` already declines to make explicit but probably should too):

```json
{
  "server_name": "example.org",
  "provider": "icap",
  "uploaded_after": "2026-09-01T00:00:00.000Z",
  "uploaded_before": "2026-09-18T00:00:00.000Z",
  "content_type": "image/"
}
```

`provider`: rescan only items whose most recent recorded verdict (if any) came from this provider — the common case right after switching providers. `content_type`: prefix match (`"image/"` matches every image subtype), for scoping a rescan to the formats a newly added transcoding/adaptation rule actually affects.

Response `202` with a `Task` (RFC 0004 section 3.7), `action: "media.rescan"`, `resource: {type: "media", id: "*"}` (a filter-based bulk operation has no single resource id — the same shape `media.delete_bulk`'s Task presumably already uses), `progress: {current, total, unit: "items"}`. On completion, `result` is `{rescanned_count, quarantined_count, error_count}`.

The handler resolves the filter against `MetadataStore` (paging through matches itself — `ScanAdmin::rescan_many` takes an already-resolved `&[(String, String)]`, by design: `ScanAdmin` does not know about `crate::metadata`'s query shape, and should not need to) and calls `rescan_many`, applying quarantine to any bad verdict exactly as `apply_quarantine: true` does for the single-item endpoint (bulk rescan without ever acting on what it finds is not a useful operation, unlike the single-item case where an operator is watching and can decide).

**Implementation note (see section 7, item 2):** today's `ScanEngine::evaluate` always checks the verdict cache first (RFC 0008 section 6) and this session did not add a way to bypass it. A bulk rescan run immediately after a signature update, against content whose verdict is still cache-fresh, would silently re-serve the stale (pre-update) verdict instead of actually re-scanning — the exact case this endpoint exists for. This has to be fixed in `crates/hs-media` before `media.rescan_bulk` is useful for its stated purpose; it is harmless (just a no-op) before that fix lands, not unsafe.

### 4.4 `GET /media/scan-provider-health` — provider health

Operation id `media.scan_provider_health.get`. Scope `moderation:read`. No parameters.

```json
{
  "enabled": true,
  "mode": "block",
  "provider_id": "icap",
  "reachable": true,
  "engine_version": "\"a1b2c3\"",
  "checked_at": "2026-09-18T10:04:00.000Z"
}
```

`enabled`/`mode` are added at this HTTP layer, not part of `ProviderHealth` itself (`crate::scanning::config::ScanMode`, open enum `block`/`defer`/`quarantine`/`off`) — a server with scanning `off` is not an error case (`reachable`/`provider_id`/`engine_version`/`checked_at` are `null` and `200` is still the right status, not `503`; `503 unavailable` is reserved for "scanning is enabled but the provider cannot currently be reached," which `reachable: false` already conveys within a `200` too, per `ProviderHealth`'s own doc: "best-effort... not that a fresh probe was just made" — the RFC 0008 provider interface has no dedicated health-check call, so this can go stale between real scans on an idle server; document that plainly in the endpoint's `summary`/`description` rather than let an operator over-trust `checked_at`).

## 5. Scopes and events

| Endpoint | Scope |
|---|---|
| `GET /media/scan-verdicts` | `moderation:read` |
| `POST /media/{server_name}/{media_id}/rescan` | `moderation:write` |
| `POST /media/rescan` | `moderation:write` |
| `GET /media/scan-provider-health` | `moderation:read` |

Matches RFC 0004 section 8.2's existing grants for `/media` (`moderation:read` reads it and its sub-resources; `moderation:write` covers "quarantine and delete media" — a rescan that can end in a quarantine belongs in the same bucket).

New, additive (D15.10) values for the existing `media` event-stream namespace (RFC 0004 section 10.1):

- `media.rescanned` — one item finished a synchronous rescan (`4.2`). Data: `{server_name, media_id, verdict_kind, provider, quarantine_applied}`.
- `media.rescan_provider_health_changed` — `reachable` flipped since the last check (coalesced like `appservice.health_changed`, not emitted on every poll). Data: `{provider_id, reachable, engine_version}`.

The bulk rescan (`4.3`) needs no new event type: it already gets `task.scheduled`/`task.progress`/`task.succeeded`/`task.failed` for free from the Task framework, plus `media.rescanned` per item as it works through the filtered set (the same relationship `appservice.replay_started`/`appservice.replay_finished` bracket individual `appservice.*` events during a bulk replay).

Add `"media.rescan"` to `Task.action`'s open enum (RFC 0004 section 3.7's list already has `media.delete`, `media.purge_remote_cache` — this is the same shape).

## 6. Schemas (for the OpenAPI document)

```yaml
MediaScanVerdict:
  type: object
  properties:
    id: { type: string }                    # see section 7, item 3
    recorded_at: { type: string, format: date-time }
    provider: { type: string }
    server_name: { type: string }
    media_id: { type: string }
    kind: { type: string, enum: [infected, scanner_error, replacement_applied, appservice_bypass] }
    signature: { type: [string, 'null'] }
    message: { type: [string, 'null'] }
    replacement:
      type: [object, 'null']
      properties:
        by: { type: string }
        original_sha256: { type: string }
        adapted_sha256: { type: string }
    appservice_id: { type: [string, 'null'] }

ScanVerdict:
  type: object
  properties:
    kind: { type: string, enum: [clean, infected, unscannable, replaced] }
    signature: { type: [string, 'null'] }      # infected
    details: { type: [string, 'null'] }        # infected
    reason: { type: [string, 'null'] }         # unscannable (UnscannableReason, stringified) or replaced
    by: { type: [string, 'null'] }             # replaced
    content_type: { type: [string, 'null'] }   # replaced, if the provider changed it

ScanProviderHealth:
  type: object
  properties:
    enabled: { type: boolean }
    mode: { type: [string, 'null'], enum: [block, defer, quarantine, off, null] }
    provider_id: { type: [string, 'null'] }
    reachable: { type: [boolean, 'null'] }
    engine_version: { type: [string, 'null'] }
    checked_at: { type: [string, 'null'], format: date-time }
```

## 7. Changes this RFC needs in `crates/hs-media` (not done this session; owner: track 09, a future session)

1. **A concrete `impl ScanAdmin for MediaRepository<B>`.** `crates/hs-media/src/scanning/admin.rs` is interface-only as of this session. The natural home is `MediaRepository` itself, once it holds both a `ScanEngine<B>` (this session wired that in — `docs/status/09-media.md`) and a handle to whatever `AuditSink` backs `recent_verdicts` (today `ScanEngine` holds `audit: Arc<dyn AuditSink>`, a trait object with no `recent()` method — `MediaRepository::with_scanning` would need to also take, or `ScanEngine` would need to expose, a concrete `Arc<InMemoryAuditSink>` handle for this to be implementable without a downcast).
2. **A cache-bypassing scan path.** Section 4.3's stated purpose ("rescan everything matching a filter after a signature update") is defeated by `ScanEngine::evaluate` always trying the verdict cache first, with no way to force a fresh scan. The fix is probably a new `ScanEngine::rescan` (distinct from `evaluate`) that skips the cache read but still writes the fresh result to it, rather than plumbing a `force: bool` through `evaluate`'s existing five-argument signature and every one of its callers.
3. **A stable identifier on `AuditEntry`.** Needed for `GET /media/scan-verdicts`'s pagination cursor (RFC 0004 section 3.3's keyset cursors need a total order with a unique tiebreaker; `(recorded_at, media_id)` is not unique enough on its own — two entries for the same item in the same millisecond is plausible for a bulk rescan). A ULID assigned at `AuditSink::record` time is the natural fit, matching every other server-generated identifier in the admin API (RFC 0004 section 3.2).

None of these are large, but all three block section 4 from being implementable as specified, not just polish — track 15 should treat this RFC as blocked on a short follow-up session against `crates/hs-media`, not as ready to implement against today's `scanning/admin.rs` verbatim.

## 8. Open questions deferred

- Whether `POST /media/rescan`'s empty-body case should be rejected (`400`) or run against every item on the server. This document recommends rejecting it; RFC 0004's existing `media.delete_bulk` does not currently say either way, so track 15 may want to settle both at once for consistency.
- Whether provider health should support an active probe (`POST /media/scan-provider-health/check`, forcing a fresh `engine_version()` call right now) rather than only the passive, possibly-stale `checked_at` this document proposes. Deferred: RFC 0008's `ContentScanner` trait has no dedicated health-check method, and adding one changes a frozen interface (`docs/rfcs/0008-content-scanning.md` section 2) — worth revisiting only if the passive version proves insufficient in practice.
