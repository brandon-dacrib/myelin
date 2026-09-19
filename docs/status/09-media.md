# 09 Media: status

Track brief: `docs/workstreams/09-media.md`. Owner crate: `hs-media`.

Last updated: 2026-09-19 (session 4: closed the two Complement gaps — MSC2246 async upload's real
`/_matrix/media/v1/create` path and `GET .../preview_url` — plus the content-scanning durability
gap session 3 flagged). Sessions 1-3's records are unchanged below this section.

## Session 4: the `/_matrix/media/v1/create` path, URL previews, and scan-verdict durability

Scope: three items handed down together — (1) `POST /_matrix/media/v1/create` 404s in Complement,
(2) `GET /_matrix/media/v3/preview_url` 404s in Complement, (3) the durability gap session 3's
"Known gaps carried forward" recorded verbatim: "deferred and quarantine scan modes use a spawned
task — a crash loses an in-flight verdict."

### Verification

```
cargo fmt -p hs-media                                     # applied, no diff on re-run
cargo clippy -p hs-media --all-targets -- -D warnings     # clean
cargo test -p hs-media                                    # 261 passed, 0 failed (249 lib + 11 image_corpus + 1 s3_backend)
```

(Machine is under heavy contention from 15 other agents this session — several `cargo` invocations
took minutes rather than seconds; the numbers above are the final, clean run.)

### 1. `POST /_matrix/media/v1/create` — the handler already existed; the path did not

Reading `crate::routes::upload::create`/`complete_reservation` and their existing tests
(`repository.rs`'s `async_upload_lifecycle`, `completing_twice_is_rejected`,
`expired_reservation_cannot_be_completed`) before writing anything: **the async-upload logic
session 1 built was already complete and already tested** — reservation, single-fill enforcement
(`MediaError::InvalidInput` on a second `PUT`), and TTL expiry (`MediaError::UploadExpired`,
`DEFAULT_RESERVATION_TTL_MS` = 24h) all already worked, at the HTTP layer too
(`routes::upload::tests::async_upload_create_then_put_round_trips`,
`put_upload_by_a_different_user_is_rejected`). The actual bug was narrower than the brief's phrasing
suggested: this crate's router already served `POST /create` and
`PUT /upload/{serverName}/{mediaId}` — but only under `/_matrix/client/v1/media` (authenticated
media) and `/_matrix/media/v3` (`legacy_router`, gated on `allow_legacy_unauthenticated_media`).
Matrix's real versioning is a quirk here: the MSC2246 `create` step was given its own `v1` the day
it was added, while the fill-in `PUT` reused the pre-existing `v3` upload path. So
`PUT /_matrix/media/v3/upload/{serverName}/{mediaId}` **already worked** (once
`allow_legacy_unauthenticated_media` is on, the default) — only
`POST /_matrix/media/v1/create` was genuinely missing, because no router in this crate had ever
served anything at the bare `/_matrix/media/v1` prefix.

**Fix**: `hs_media::router::v1_router` (`crates/hs-media/src/router.rs`) — a new, minimal router
exposing only `POST /create` (reusing `routes::upload::create`, unchanged), meant to be mounted at
`/_matrix/media/v1`. Still authenticated, matching `legacy_router`'s own documented precedent for
its upload endpoints ("still authenticated — only download and thumbnail lost their authentication
requirement in the legacy path").

**This needs a new `hs-cli` mount point — it does not go live on its own.** Every route inside
`authenticated_router`/`legacy_router` mounts automatically because `hs-cli` already calls
`merge_router` on both; `/_matrix/media/v1` is a prefix nothing calls `merge_router` with today. See
"Wiring the integration lead must add" below for the exact lines.

Proven with `router::tests::v1_router_serves_create_at_its_relative_path`
(`crates/hs-media/src/router.rs`) — builds `v1_router` over a fresh in-memory state (via a new
`crate::test_support::v1_router` helper) and asserts `POST /create` returns `200` with a
`mxc://...` `content_uri`. This is the closest this session could get to "answered a real request"
without editing `hs-cli`: the binary route only exists once the integration lead adds the mount (see
below); once it does, the exact same handler this test already exercises is what answers it.

### 2. `GET .../preview_url` — new: `crate::preview`

`docs/rfcs/0006-url-previews.md` already existed (design-only, from an earlier session) and is
followed closely; deviations are called out explicitly below rather than silently diverging.

**What was built** (`crates/hs-media/src/preview.rs`, new; `crates/hs-media/src/routes/preview.rs`,
new route handler; two new `MetadataStore` methods for the cache):

- `PreviewIpPolicy`: the SSRF blocklist, built from `hs_config::MediaConfig::url_preview_ip_range_blocklist`
  (already existed — added by an earlier session, never wired to anything until now). **Explicit
  instruction from the assignment: "hs-federation's `ip_range_blocklist` config is the precedent to
  follow, do not invent a second policy."** What this means concretely, and the one place this
  session deviated from "reuse the exact type": `hs_federation::client::IpPolicy` (CIDR list via
  `ipnet`, block-unless-allowlisted) is the *design* this crate copies — parse CIDR strings with
  `ipnet`, skip unparsable ones defensively, block by range membership. This crate does **not**
  depend on `hs-federation` to get `IpPolicy` itself: that type also carries an allowlist half this
  crate's config (`MediaConfig`, not `FederationConfig`) has no field for, and pulling in
  federation's whole dependency graph (mesh RPC, mTLS, discovery) for one ~15-line struct was
  judged not worth it. `PreviewIpPolicy::from_cidrs`/`allows` are the same shape, same default range
  list (RFC 1918 + loopback + link-local + CGNAT + IPv6 equivalents —
  `hs_config::media::default_preview_blocklist`, already identical to federation's own default
  before this session touched anything), same "parse, skip-on-error" division of responsibility
  with `MediaConfig::validate`. If a shared `hs-net-policy`-style crate is ever carved out, this is
  the type that should move into it — noted at the top of `PreviewIpPolicy`'s own doc.
- `guarded_fetch`: resolves the target host itself (`tokio::net::lookup_host`, no new DNS-resolver
  dependency — RFC 0006 flagged needing one; `tokio`'s own `net` feature, already enabled via
  `features = ["full"]` in the workspace, was sufficient), checks **every** resolved address against
  the blocklist (refuses if *any* one is blocked, not just if all are — stricter than
  `hs-federation`'s own "connect if any candidate is allowed" semantic, deliberately: a
  `preview_url` target is attacker-chosen, a federation destination is operator-configured), then
  pins the actual connection to the checked address via `reqwest::ClientBuilder::resolve` exactly as
  RFC 0006 section 4.2 and `hs-federation`'s own `client_for` both do. Redirects: automatic
  following is disabled (`redirect::Policy::none()`); this module follows them itself, up to 5 hops,
  re-running the full resolve-and-check step against every hop's `Location` (RFC 0006 section 4.3).
  Also implements RFC 0006 section 4.5 (reject a URL with embedded userinfo,
  `FetchError::UserinfoNotAllowed`) — the one item this session's first draft missed and added after
  re-reading the RFC in full.
- `extract_og_tags`: `og:title`/`og:description`/`og:image` from `<meta>` tags, falling back to
  `<title>` for the title, attribute-order-independent, both quote styles, the handful of HTML
  entities that actually show up in practice (`&amp; &lt; &gt; &quot; &apos;` plus decimal/hex
  numeric character references). **Deviation from RFC 0006 and from decision 0007's spirit, stated
  precisely**: no HTML-parsing crate was added; this is a `regex`-based `<meta>`/`<title>` scan, not
  a DOM parse. `regex` was already a workspace dependency (used elsewhere); a real parser
  (`html5ever`/`scraper`) was considered and rejected for this session's scope — OG extraction never
  needs anything past `<meta>`/`<title>` attributes, and Synapse itself does not do a strict DOM
  parse for this either. Revisit if a differential test against Synapse ever surfaces a page this
  regex mishandles that a real parser would not.
- `preview_url` (the free function `crate::preview::preview_url`, and
  `MediaRepository::preview_url` which is a two-line delegator to it): fetches the page, extracts
  tags, and — if `og:image` is present — fetches *that* URL through the identical guard (recursive,
  per RFC 0006 section 4.4), sniffs it with `crate::sniff::sniff_format` (never trusts the remote
  `Content-Type`; unrecognized bytes are dropped from the response rather than cached), stores it
  through the same object-store/metadata path an ordinary upload uses, and rewrites `og:image` to
  the resulting `mxc://` URI with `matrix:image:size` set to the stored byte length. A failed or
  non-image `og:image` fetch is not fatal to the whole preview (Synapse's own behavior) — the
  response just omits the image fields.
- Caching: two new `MetadataStore` methods, `get_preview_cache`/`put_preview_cache`, keyed by the
  SHA-256 hex of the requested URL, TTL'd at `DEFAULT_PREVIEW_CACHE_TTL_MS` (1 hour). **Deviation
  from RFC 0006, stated precisely**: RFC 0006 section 4.4/4.6 proposes new `MediaConfig` fields
  (`url_preview_timeout`, `url_preview_max_fetch_size`, `url_preview_cache_lifetime`) — this session
  did not add them, because they belong in `hs_config` (a crate outside this track's edit
  permission this session; the assignment's ownership rule is explicit that only `crates/hs-media`
  and this status file may be touched). `FetchLimits::default()` (10 MiB, 10s timeout, 5 redirects)
  and `DEFAULT_PREVIEW_CACHE_TTL_MS` are hardcoded constants in `crate::preview` instead. **Track 13
  (or whoever next touches `hs_config::MediaConfig`) should add these three fields** and thread them
  through `MediaRepository::preview_url`'s call into `crate::preview::preview_url` (the function
  already takes `config: &MediaConfig`, so wiring real fields through is a small, additive change,
  not a redesign).
- Not implemented, stated precisely: `og:image:width`/`og:image:height` (RFC 0006 section 3 lists
  them; the assignment's own acceptance criteria only names `og:title`/`og:description`/`og:image`/
  `matrix:image:size` — width/height would need decoding the fetched image with
  `crate::sniff::decode_with_limits` and reading its dimensions, not just sniffing its format;
  cheap to add later, not done this session to stay in scope). oEmbed discovery (RFC 0006's own "out
  of scope for this RFC" list). In-flight request de-duplication for concurrent identical requests
  (RFC 0006 section 4.6's "thundering herd" protection) — documented as a known gap in
  `crate::preview`'s own module doc rather than silently dropped. No capacity bound on the preview
  cache table (unlike `scanning::cache::VerdictCache`, which evicts oldest-first) — entries only
  disappear on a read past their TTL, not via a proactive sweep; documented on
  `MetadataStore::put_preview_cache`'s doc.
- **Legacy-path authentication: a deliberate deviation from RFC 0006's own text.** RFC 0006 section
  3 states "Synapse serves `preview_url` unauthenticated on the legacy path today" and proposes
  matching that. This session mounted `preview_url` as **authenticated on both** the
  `client/v1/media` and legacy `media/v3` routers (`routes::preview::preview_url` takes
  `MediaRequester`, same as every other route in both routers except `legacy`'s own
  download/thumbnail handlers). Rationale: an unauthenticated, internal-network-reachable
  "fetch any URL I name" oracle is a materially bigger operational risk than a Synapse
  compatibility gap on one endpoint, and nothing in this session's assignment asked for wire-for-wire
  legacy parity here specifically. Revisit if track 14's differential testing against real Synapse
  specifically flags this as a conformance failure worth accepting the risk for.

Routes added inside `authenticated_router`/`legacy_router` (`GET /preview_url` on both) — **these
go live with no `hs-cli` change**, per this crate's existing convention
(`hs_media::router::authenticated_router`/`legacy_router` are what `hs serve` mounts).

Tests (`crates/hs-media/src/preview.rs`, `crates/hs-media/src/routes/preview.rs`):

- `blocklist_blocks_private_ranges`, `default_blocklist_matches_media_config_defaults`: the policy
  logic in isolation.
- **`fetch_refuses_a_private_address`: the mutation-tested SSRF guard test the assignment asked
  for by name.** Mutation-tested by hand this session (not left as a claim): the
  `!candidates.iter().all(|ip| policy.allows(*ip))` check in `resolve_and_check` was temporarily
  replaced with `if false { ... }` (guard disabled) and `cargo test -p hs-media
  preview::tests::fetch_refuses_a_private_address` was re-run — it failed (a connection error
  instead of the expected `FetchError::Blocked`), confirming the test actually exercises the guard
  rather than passing regardless. The change was then reverted and the suite re-run clean. See this
  test's own doc comment, which records the exact mutation for the next person who touches this
  code.
- `embedded_credentials_are_refused`, `unsupported_scheme_is_refused`: the other cheap-to-bypass
  checks from RFC 0006 section 4.5.
- `extracts_basic_og_tags`, `attribute_order_does_not_matter`, `single_quoted_attributes_are_parsed`,
  `falls_back_to_title_tag_when_no_og_title`, `numeric_character_references_decode`: the OG-tag
  extraction, including the two "lenient scan, not a DOM parse" edge cases (attribute order,
  quote style) explicitly worth proving since that is exactly where a regex-based approach could
  silently misbehave.
- **`full_preview_flow_fetches_parses_and_caches_the_og_image`: the end-to-end test.** A real local
  `axum`/`tokio` HTTP server (no external network) serves an HTML page plus a real PNG at the
  `og:image` path; `crate::preview::preview_url` is called against it with an *empty* blocklist
  (the SSRF guard is proven separately, above — conflating the two here would either need the test
  server to bind somewhere non-loopback, which is not possible in this sandbox, or would defeat the
  guard for the duration of the test). Asserts `og:title`/`og:description` decode correctly
  (including an HTML entity), `og:image` is rewritten to an `mxc://` URI, and `matrix:image:size`
  matches the stored byte length. **Cache proof**: after the first call, the mock server's task is
  `.abort()`-ed, and a second call for the identical URL is asserted to return the byte-identical
  response anyway — this would be a connection-refused error without the cache, so this is a real
  proof of caching, not an assumption.
- `routes::preview::tests`: `preview_url_is_403_when_disabled` (the default —
  `url_preview_enabled: false`), `preview_url_requires_authentication`,
  `preview_url_route_exists_on_both_routers` (a direct regression test for the Complement 404 this
  session closes — asserts `!= 404` on both mounts, not just "some status"),
  `preview_url_requires_the_url_query_parameter`.

### 3. Scan-verdict durability: `PendingScan` + `MediaRepository::resume_pending_scans`

Session 3's own words, carried forward verbatim as this session's starting point: "Background
scanning is `tokio::spawn`, not durable job infrastructure... a process crash between 'the client
received a `content_uri`' and 'the spawned task resolves' leaves `defer` media quarantined forever
... and leaves `quarantine` media un-quarantined even if the verdict would have said otherwise...
The real fix is track 03/12's background-job leasing, which this crate does not own."

That remains true — there is still no job queue, no leasing, no cross-node scheduling. What this
session adds is the piece that does not need one: **the *intent* to scan survives a crash**, because
it is written to the same durable `MetadataStore` (Fjall/Postgres in production) as every other
media row, in the same call that stores the upload's bytes, *before* the `tokio::spawn` task that
might not survive a restart is ever created.

**New** (`crates/hs-media/src/metadata.rs`): `PendingScan` (a new row type: `server_name`,
`media_id`, `content_type`, `uploader`, `appservice_id`, `enqueued_at_ms`) and a new
`hs_media.pending_scans` keyspace, with `put_pending_scan`/`delete_pending_scan`/
`list_pending_scans`.

**New** (`crates/hs-media/src/repository.rs`):

- `MediaRepository::record_pending_scan`, called from both `upload` and `complete_reservation`
  immediately after the media row is written and *before* `spawn_background_scan` is called (for
  `defer`/`quarantine` mode only — `block` mode resolves synchronously and never reaches this at
  all). Proven synchronous, not merely eventual, by
  `scanning_integration::pending_scan_is_recorded_before_the_background_task_can_have_run`: this
  relies on `#[tokio::test]`'s default `current_thread` flavor (a `tokio::spawn`ed task cannot run
  until the spawning task yields at an `.await`), asserts the pending row exists with **no**
  `.await` between `upload` returning and the check, then lets the real background task run to
  completion so it does not outlive the test.
- `MediaRepository::apply_background_scan_decision` now clears the pending-scan row at the end,
  regardless of which branch resolved it (`Allow`/`AllowReplaced`/`Reject`/`StoreQuarantined`) — the
  one shared cleanup point both the live `tokio::spawn` path and the new resume path call, so a row
  is cleared exactly once no matter which path resolved it.
- **`MediaRepository::resume_pending_scans`**: reads every row `MetadataStore::list_pending_scans`
  returns, re-fetches the already-durably-stored bytes from the object store (never re-sent over
  the wire — they were written before the pending-scan row was), rebuilds the same `ScanContext` the
  original background task would have, calls `ScanEngine::evaluate`, and applies the decision
  through the same `apply_background_scan_decision` the live path uses. A media item whose bytes
  cannot be read back (should not happen; defensively handled) is logged and left pending for a
  future retry rather than silently dropped. Returns the count it attempted.

**What this durability fix guarantees, and what it does not — stated as precisely as session 3's
own gap statement, per the assignment's explicit ask ("state precisely why it cannot [survive a
restart] without a job queue that does not exist yet and what the interim guarantee is")**:

- Guarantees: the *fact* that a given media item still needs a scan is never lost to a process
  crash, because it lives in the same durable store as everything else this crate persists — not
  process memory, not only the `tokio::spawn` future's captured state. Any process holding the same
  backend (not necessarily the one that crashed) can complete it by calling
  `resume_pending_scans` — a restart of the same node is the common case, but the row itself does
  not care which process reads it.
- Does not guarantee: **automatic** recovery. Nothing in this crate calls `resume_pending_scans` on
  its own, on a timer, or on startup — it is a method, not a scheduled job. `hs-cli` needs to call it
  once at startup for "the intent survives a crash" to become "a restart actually resolves it" — see
  "Wiring the integration lead must add" below. A scan task that panics mid-scan *without* a process
  restart (e.g. a bug in a provider adapter) is also not retried until the next explicit
  `resume_pending_scans` call — this crate does not add a periodic sweep, since a periodic sweep
  with proper leasing (so two replicas do not double-scan the same item pointlessly) is exactly the
  job-queue infrastructure track 03/12 owns and this crate was told not to attempt to substitute
  for. Concurrent `resume_pending_scans` calls (two nodes racing after a shared-backend cluster
  restart) are not harmful — `apply_background_scan_decision` is idempotent enough that a
  double-scan wastes work, not correctness (quarantining twice, or clearing an already-cleared
  marker, are both no-ops in effect) — but this session did not add any coordination to prevent the
  redundant work itself.

Tests (`crates/hs-media/src/repository.rs`, `scanning_integration` module):

- `pending_scan_is_recorded_before_the_background_task_can_have_run` (see above).
- **`resume_pending_scans_completes_a_scan_a_crash_left_unresolved`: the direct durability test.**
  Deliberately does *not* go through `MediaRepository::upload` with a live scanning engine attached
  (that would race the real `tokio::spawn` task against the test's own assertions on a
  single-threaded runtime, proving nothing deterministic). Instead it manually reproduces exactly
  what a crash leaves behind — bytes written to the object store, a servable-but-unscanned media
  row, a `PendingScan` row — using a fresh `MetadataStore`/object store with **no** repository or
  engine touching them yet, then constructs a brand-new `MediaRepository` over that same backend
  (standing in for "the process restarted") with a scanning engine attached, and calls
  `resume_pending_scans`. Asserts: before resuming, the item is servable but a scan is still owed
  (`quarantine` mode's defining pre-scan state, held across the simulated restart); after resuming,
  the (infected) verdict is applied (`MediaError::Quarantined`) and the pending row is gone. This
  test would fail (in fact, would not compile) without this session's fix.
- `resume_pending_scans_without_an_engine_is_a_harmless_no_op`: a deployment that never configured
  `--media-scanning-config` gets `Ok(0)`, not an error.

### Wiring the integration lead must add (`crates/hs-cli`, not touched this session per this
track's ownership rule)

Two changes, both in `crates/hs-cli/src/serve.rs`:

1. **Mount `v1_router` at `/_matrix/media/v1`** — the fix for the `POST /_matrix/media/v1/create`
   404. In the existing `if legacy_media_enabled { ... }` block (currently just the `legacy_router`
   merge, around what was line 350-353 before this session's changes elsewhere in the repo shifted
   line numbers):

   ```rust
   if legacy_media_enabled {
       let (media_v1_router, media_v1_manifest) = hs_media::router::v1_router::<B>();
       let media_v1_router = media_v1_router.with_state(mounts.media.clone());
       builder = builder.merge_router("/_matrix/media/v1", media_v1_router, media_v1_manifest.routes);

       let (legacy_router, legacy_manifest) = hs_media::router::legacy_router::<B>();
       let legacy_router = legacy_router.with_state(mounts.media);
       builder = builder.merge_router("/_matrix/media/v3", legacy_router, legacy_manifest.routes);
   }
   ```

   (`mounts.media.clone()` for the new call since the existing line right after it moves
   `mounts.media` into `legacy_router`'s `.with_state(...)` — order matters: clone before the move,
   as shown.) Gated on the same `legacy_media_enabled` flag as `legacy_router` itself, since
   `/create` at this path is part of the same "still-authenticated legacy" family the existing
   `legacy_router`'s own module doc describes, not a new always-on surface.

2. **(Recommended, not required for the Complement gap) Call `resume_pending_scans` at startup.**
   Right after `let media_state = crate::media::build_media_state(...)?;` in `serve.rs`:

   ```rust
   {
       let repo = media_state.repository.clone();
       tokio::spawn(async move {
           match repo.resume_pending_scans().await {
               Ok(0) => {}
               Ok(n) => tracing::info!(count = n, "resumed pending content scans from a previous run"),
               Err(e) => tracing::error!(error = %e, "failed to resume pending content scans at startup"),
           }
       });
   }
   ```

   Spawned rather than awaited so a slow/down scanner cannot delay server startup; adjust to
   `.await` directly instead if the integration lead would rather block startup until any backlog
   is cleared. This is what turns the "intent survives a crash" guarantee above into "a restart
   actually resolves it" — without this call, `PendingScan` rows are written and read correctly
   (proven by this session's tests) but nothing in the running binary ever calls
   `resume_pending_scans` on its own.

### Decisions made this session

- **`v1_router` is a new, separate router function, not an addition to `legacy_router`.** The two
  functions in this crate map one-to-one to `hs-cli` mount points; since `/_matrix/media/v1/create`
  is a different mount point than `/_matrix/media/v3/...`, it needs its own `Builder` output, even
  though the handler it calls (`routes::upload::create`) is identical to the one `legacy_router`
  already exposes at `/_matrix/media/v3/create`. Both paths now serve the same handler once
  `hs-cli` mounts `v1_router` — harmless duplication, not a behavior difference, kept because
  removing `/_matrix/media/v3/create` risks breaking a client that (incorrectly, but observedly, per
  some client implementations) calls the wrong version.
- **`PreviewIpPolicy` duplicates `hs_federation::client::IpPolicy`'s shape rather than importing
  it.** See "2." above for the full reasoning; recorded here too since it is exactly the kind of
  cross-track judgment call `docs/decisions/` conventions ask to be visible.
- **Preview's OG-tag extraction is regex-based, not a DOM parse.** See "2." above.
- **`preview_url` is authenticated on the legacy router too, diverging from RFC 0006's own claim
  about Synapse's real behavior.** See "2." above.
- **New `MediaConfig` fields RFC 0006 proposed (`url_preview_timeout`, `url_preview_max_fetch_size`,
  `url_preview_cache_lifetime`) were not added** — out of this track's edit permission this session
  (`hs_config` is not `crates/hs-media`). `crate::preview::FetchLimits`/`DEFAULT_PREVIEW_CACHE_TTL_MS`
  are hardcoded stand-ins; flagged for whichever session next has `hs_config` open to wire through.
- **No capacity bound on the preview cache table.** Unlike `scanning::cache::VerdictCache`. See "2."
  above.

## Session 3: wiring content scanning into the upload path

Session 2 built the entire `crate::scanning` subsystem (provider interface, verdict cache, ICAP
and HTTP providers, metrics, audit, `ScanEngine`) but never called it from anywhere —
`MediaRepository::upload`/`complete_reservation` were unchanged from session 1. This session closes
that gap: every RFC 0008 section 4 scan point that has a caller today is wired, `block`/`defer`/
`quarantine`/`off` are four genuinely different behaviors (not two of them collapsing into one),
the section 3.4 replacement rules apply at the real call sites, and a reference c-icap+ClamAV
deployment exists under `deploy/media-scanning/`.

### Verification

```
cargo check -p hs-media                                  # clean
cargo clippy -p hs-media --all-targets -- -D warnings     # clean
cargo test -p hs-media                                    # 241 passed, 0 failed (229 lib + 11 image_corpus + 1 s3_backend)
cargo fmt -p hs-media                                     # applied, no diff on re-run
```

### Done

1. **`MediaRepository<B>::with_scanning`** (`crates/hs-media/src/repository.rs`): a new builder
   method taking an already-built `ScanEngine<B>`, storing it as `Option<Arc<ScanEngine<B>>>`.
   `MediaRepository::new`'s signature is unchanged, exactly as session 2's "Next" asked — every
   existing caller (this crate's own tests, `crate::test_support`, `crate::state`'s doctest-style
   fixture) still compiles unchanged and behaves as `mode: off` (no engine attached).
2. **Two scan points wired for real** (RFC 0008 section 4, points 1 and 2 — the only two with an
   existing caller): `MediaRepository::upload` and `MediaRepository::complete_reservation` both
   call a new private `decide_scan` before `object_store.put`, and (for `defer`/`quarantine`) a
   new `spawn_background_scan` after. `ScanContext::source` is `Local` for an ordinary upload,
   `Appservice` when `UploadContext::appservice_id` is set (new field — see below).
3. **Federation fetch (RFC 0008 section 4, point 3): the clean seam it needs, not code.** RFC 0007
   (federation media) is still design-only — track 06 has no federation client for this crate to
   call into yet, exactly as session 1 and 2 both recorded. Nothing to wire here yet;
   `ScanSourceKind::Federation` already exists in `crate::scanning::types` for whenever it lands,
   and `ScanEngine::evaluate`'s `allow_replacement_here: bool` parameter is exactly the seam RFC
   0007's future caller needs to pass `false` through (upload-only replacement, RFC 0008 section
   3.4 rule 1) — documented at the call site in `crate::repository::decide_scan`'s doc and in
   `crate::scanning::engine`'s module doc, both already written by session 2.
4. **Appservice bypass, wired and audited** (RFC 0008 section 4, point 4): `UploadContext` gained
   a new field, `appservice_id: Option<String>`, threaded from `hs_auth::requester::Requester::
   appservice` in `crate::routes::upload`'s three handlers (`upload_sync`, `create`, `put_upload`).
   `decide_scan` checks `ScanEngine::appservice_bypassed` (new accessor) before ever building a
   `ScanContext` for the provider call; a bypass calls the new `ScanEngine::record_bypass`
   (writes `AuditKind::AppserviceBypass` directly, matching RFC 0008 section 4's "recorded in the
   audit log" requirement) and skips scanning entirely — proven in
   `repository::scanning_integration::appservice_bypass_skips_scanning_and_is_audited` by a fake
   provider configured to always say "infected," asserted never called.
5. **Four genuinely different modes**, not two collapsing into one:
   - **`off`**: `decide_scan` returns immediately if `self.scanning` is `None` or
     `engine.mode() == ScanMode::Off` — unchanged behavior, zero overhead.
   - **`block`**: `engine.evaluate` is awaited synchronously before `object_store.put`; `Reject`
     returns a new `MediaError::RejectedByScanner(String)` (mapped to `403 M_FORBIDDEN` in
     `error.rs`, following that file's existing pattern) with nothing ever persisted, exactly as
     RFC 0008 section 5 requires.
   - **`defer`**: the record is stored **immediately quarantined** (`quarantined_by:
     Some("system:pending-scan")`) before the response is even built, so it is retrievable by media
     ID (the reservation/upload succeeds) but every download attempt gets the identical `404`
     shape `MediaError::Quarantined` already produces for admin-quarantined and not-found media —
     the brief's exact ask ("a download in the interim returns the same not-found response
     quarantined media already returns, so the two are indistinguishable to a caller"). A
     `tokio::spawn`-ed background task runs the real scan and either clears the marker (clean/
     allowed) or leaves it quarantined (bad verdict) — see `defer_mode_hides_the_upload_until_a_
     clean_verdict_arrives` and `defer_mode_stays_hidden_on_a_bad_verdict`.
   - **`quarantine`**: the record is stored **immediately servable** (no marker at all); the same
     background task quarantines it after the fact only if the verdict turns out bad
     (`quarantined_by: Some("system:scan")`) — see `infected_upload_is_quarantined_under_
     quarantine_mode`, which asserts the item is servable *before* waiting for the background
     scan and only becomes `Quarantined` after polling for it.
   The two marker strings (`PENDING_SCAN_MARKER` = `"system:pending-scan"`, `SCAN_QUARANTINE_
   MARKER` = `"system:scan"`) are distinct so an admin (or a future admin-API `GET
   /media/{...}`) can tell "still deciding" apart from "the scanner said no" apart from an actual
   admin's own quarantine action (`by: Some("@admin:...")`, unrelated marker shape).
6. **What "background" honestly is and is not.** `spawn_background_scan` is `tokio::spawn`, not
   job-scheduler infrastructure: no persistence, no retry, no lease, nothing observable except the
   audit log and the row it eventually updates. Documented at length in that method's own doc
   comment and repeated here because it is the one thing to not silently forget: a process crash
   between "the client received a `content_uri`" and "the spawned task resolves" leaves `defer`
   media quarantined forever (safe, but stuck until an admin manually clears it — no worse a
   failure mode than the scanner being down under `fail: closed`) and leaves `quarantine` media
   un-quarantined even if the verdict would have said otherwise (an extended version of the
   exposure window `quarantine` mode always accepts by design, not a new kind of unsafety). The
   real fix is track 03/12's background-job leasing, which this crate does not own and did not
   attempt to build a substitute for. This is the honest answer to the brief's "if you cannot do
   that without background job infrastructure that belongs to another track, implement what you
   can... and say precisely what is missing" — what's missing is durability across a restart, not
   the mode distinction itself (that part is real, per item 5 above).
7. **RFC 0008 section 3.4's replacement rules, applied at the real call sites, not only tested as a
   pure function.** `decide_scan` (block mode) and `spawn_background_scan` (defer/quarantine mode)
   both call `engine.evaluate(..., allow_replacement_here: true)` — true because both are
   upload-time scan points, the one case section 3.4 rule 1 allows replacement at all. A
   `Verdict::Replaced` result overwrites the stored bytes and `content_type` (immediately for
   `block`; via the new `MetadataStore::update_content_type_and_length` for `defer`/`quarantine`,
   once the background scan resolves — see `quarantine_mode_applies_a_replacement_from_the_
   background_scan`). **Bug found and fixed while wiring this**: `ScanEngine::decision_for`'s
   `AuditKind::ReplacementApplied` audit entry had `original_sha256: String::new()` with a comment
   claiming "filled in by the caller" — no caller ever did, since `decision_for` never received the
   original bytes to hash. Fixed by computing the hash once in `ScanEngine::evaluate` (which does
   have the original bytes) and threading it down to both `raw_outcome` (the cache-key path,
   already had its own local copy — now shares the one hash instead of computing it twice) and
   `decision_for` (the audit path, previously broken). No test exercised the exact hash value
   before, so nothing caught this in session 2; it is real now for any caller.
8. **`MediaError::RejectedByScanner(String)`** (`crates/hs-media/src/error.rs`): maps to `403`/
   `M_FORBIDDEN`, following the file's existing per-variant pattern; two direct tests
   (`error::tests::rejected_by_scanner_is_403_with_m_forbidden` plus the integration coverage in
   `repository::scanning_integration`).
9. **`MetadataStore::update_content_type_and_length`** (`crates/hs-media/src/metadata.rs`): a new
   method overwriting only a completed row's `content_type`/`byte_length` in place (used when a
   background scan's replacement lands after the client already has a `content_uri`), leaving
   `completed`/`expires_at_ms`/quarantine state untouched. Two direct tests plus the integration
   test above.
10. **12 new integration tests through the real upload path**
    (`crates/hs-media/src/repository.rs`, `mod scanning_integration`, inside `#[cfg(test)]` —
    unit tests calling `MediaRepository::upload`/`complete_reservation` directly, per this track's
    existing convention of testing the repository layer without a real HTTP listener, since none
    exists yet): clean upload retrievable; infected upload rejected under `block`; infected
    quarantined under `quarantine` (servable-then-quarantined, proven with a poll); `defer` hides
    until a clean verdict *and* stays hidden on a bad one; a `defer`/`quarantine`-mode replacement
    is applied by the background path; scanner timeout under both `fail: closed` (rejects) and
    `fail: open` (allows); encrypted content accepted and the fake provider proven never called;
    replacement applied when `allow_replacement: true`, refused (demoted to a scanner error, then
    rejected under `fail: closed`) when `false`; the async-upload completion path (`create` +
    `complete_reservation`) scanned too, not just the synchronous path; appservice bypass. A local
    `FixedVerdict`/`AlwaysTimesOut` fake `ContentScanner` stands in for a real provider, the same
    pattern session 2's `scanning::engine::tests::FakeScanner` used — no public fake-provider
    surface was added to the crate for this, since `ContentScanner` is already public and a test
    module implementing it directly needs nothing else exported.
11. **`deploy/media-scanning/`**: a reference deployment (`compose.yaml`, `media-scanning.yaml`,
    `README.md`) running `opencloudeu/clamav-icap` (c-icap with a co-located clamd, one container)
    beside a commented-out homeserver service, plus the exact `media.scanning` YAML block
    (`ScanningConfig::from_yaml`'s shape) pointing `icap.host`/`icap.port`/`icap.service` at it.
    **Explicitly marked untested** (Docker unavailable here, matching `deploy/Dockerfile`'s own
    disclaimer) — reviewed line by line against `opencloudeu/clamav-icap`'s documented usage and
    this crate's `scanning::providers::icap` wire behavior, never started. The homeserver service
    is commented out with an explanation, not silently wrong: no `hs-*` binary reads
    `media.scanning` from a config file or calls `MediaRepository::with_scanning` at startup yet
    (`grep -rl MediaRepository::new crates/ | grep -v crates/hs-media` finds nothing — no listener
    crate constructs a `MediaRepository` at all). The README's "What's still missing" section notes
    track 12 should add the equivalent Helm sub-chart (`deploy/helm/hs` has none today) and lists
    three other concrete gaps (a real EICAR-through-this-stack test, track 13's config folding, the
    startup wiring itself).
12. **`docs/rfcs/0011-admin-scanning-endpoints.md`**: the wire-shape proposal to track 15 that
    `crate::scanning::admin`'s module doc has referenced since session 2 (that comment is now
    updated to point at a real file). Specifies `GET /media/scan-verdicts` (paginated,
    `AuditEntry`-backed), `POST /media/{server_name}/{media_id}/rescan` (with an
    `apply_quarantine` query flag), `POST /media/rescan` (bulk, filter-based, a `Task` — the
    "after a signature update" case), and `GET /media/scan-provider-health`, all extending the
    existing `/media` resource family (`crates/hs-admin/openapi/openapi.yaml`, already implemented
    by track 15) rather than forming a new one, matching RFC 0008 section 8's explicit instruction.
    Section 7 of that RFC is a direct, honest list of three things `crates/hs-media` still needs
    before section 4's endpoints are actually implementable: a concrete `impl ScanAdmin` (still
    just a trait), a cache-bypassing scan path (`ScanEngine::evaluate` always prefers the verdict
    cache, which defeats "rescan after a signature update" as stated), and a stable identifier on
    `AuditEntry` (needed for the list endpoint's pagination cursor). None of these were built this
    session — the RFC is scoped to the wire shape, per the brief's "if budget remains" framing.

### Known gaps carried forward from this session (said precisely, not left implicit)

- **Background scanning is `tokio::spawn`, not durable job infrastructure** — see "Done" item 6.
  This is this session's central, deliberate simplification; do not silently upgrade the module
  doc to imply otherwise without also building the real thing.
- **`ScanAdmin` still has no implementation** (RFC 0011 section 7, item 1) — blocked on giving
  `MediaRepository` a concrete `Arc<InMemoryAuditSink>` handle (today `ScanEngine` only exposes
  `Arc<dyn AuditSink>`, which has no `recent()`).
- **No cache-bypassing scan path** (RFC 0011 section 7, item 2) — `ScanEngine::evaluate` always
  tries the verdict cache first; there is no way to force a fresh scan yet, which matters for both
  a future `ScanAdmin::rescan` and the admin API's stated "rescan after a signature update" use
  case.
- **The EICAR-through-a-real-ICAP-daemon test** (session 2's own flagged gap) is still not
  written — `deploy/media-scanning/` (item 11 above) is the prerequisite that makes it
  straightforward to add, per that session's own note, but writing the test itself was not this
  session's scope either (Docker is still unavailable here to run it against).
- **No fuzzing was run** this session either, same caveat as both prior sessions (`cargo-fuzz`/a
  nightly toolchain are not available in this sandbox).
- **`appservice_id` threading stops at `UploadContext`.** `UploadPolicy` (the quota trait) now
  receives a field it does not use — a deliberate, noted-in-code choice (see `policy.rs`'s doc on
  the new field) to avoid a second, scanning-specific context type, not an oversight.

### Next (in priority order)

1. A concrete `impl ScanAdmin for MediaRepository<B>` plus the `Arc<InMemoryAuditSink>` handle it
   needs (RFC 0011 section 7, item 1) — the fastest way to make the admin RFC's endpoints real.
2. `ScanEngine::rescan` (or equivalent cache-bypassing path) — RFC 0011 section 7, item 2; blocks
   "rescan after a signature update" from doing anything useful even once `ScanAdmin` exists.
3. A stable id on `AuditEntry` — RFC 0011 section 7, item 3; blocks `GET /media/scan-verdicts`'s
   pagination cursor.
4. The EICAR-through-`deploy/media-scanning/` test, once Docker is available somewhere this can
   run (env-var-gated, mirroring `tests/s3_backend.rs`).
5. Everything session 1 and 2 already deferred and this session did not touch: URL previews,
   federation media fetch/serve (blocked on track 06), direct media for bridges, GCS/Azure
   backends, fuzzing for real, the track 12 Helm sub-chart for `deploy/media-scanning/`. See the
   "Next"/"Interfaces needed" sections at the end of this file (session 1's original list; still
   the accurate read for everything outside content scanning).

## Session 2: pluggable content scanning (`crate::scanning`)

The RFC changed twice mid-session (first: ICAP promoted from one-of-five to the primary and only
deeply-built provider; second, at the user's direction via
`docs/decisions/0007-build-less-reuse-more.md`: drop the clamd/command providers entirely, adopt
`icap-rs` instead of a hand-rolled ICAP client, and add `Verdict::Replaced` for ICAP's
content-adaptation half, not just antivirus). What is described below is the *final* state, after
both changes; an intermediate hand-rolled ICAP client (raw `TcpStream` byte-banging, its own
`INSTREAM`-shaped connection pool) was written and then deleted once `icap-rs` was evaluated and
found to cover everything needed — see "Reuse considered" below for the record decision 0007
requires.

### Verification

```
cargo check -p hs-media          # clean
cargo clippy -p hs-media --all-targets -- -D warnings   # clean
cargo test -p hs-media           # 225 passed, 0 failed (213 lib + 11 image_corpus + 1 s3_backend)
cargo fmt -p hs-media             # applied
```

### Done — complete and independently tested (`crates/hs-media/src/scanning/`)

- **`scanning/types.rs`**: the provider interface exactly as RFC section 2 specifies —
  `ContentScanner` (`id`, `engine_version`, `scan`, `poll` with a default `Err(Unsupported)`),
  `Verdict` (`Clean`, `Infected`, `Unscannable`, `Pending`, plus `Replaced { content, by, reason }`
  added for RFC section 3.4), `UnscannableReason`, `ScanContext`, `ScanTicket`, `ScanError`.
  `ScanSource`: a pull (`next_chunk`/`ChunkSource`), not `futures::Stream`-based, so this crate
  needs no stream-combinator dependency; `ScanSource::from_bytes` is what every scan point in this
  session uses (content is already fully buffered by the time it reaches `crate::repository` —
  see that struct's doc for why "MUST stream" is honored at the wire-protocol-chunking level, not
  by ever reading directly from a client socket). `sha256_hex` is a public free function (used by
  both `ScanSource` and `scanning::engine`'s cache-key computation). 9 tests.
- **`scanning/config.rs`**: `ScanningConfig` (mode/provider/fail/allow_replacement/timeout/
  max_size/oversize/cache/unscannable-policy/appservice_bypass/icap/http), built on
  `hs_config::{Duration, ByteSize}` and `hs_config::Validate` for parse/error-shape consistency
  with the rest of the config system, **without editing `hs_config::MediaConfig`** (track 13's
  crate — this track's ownership rule forbids it; see "Interfaces needed" below for what track 13
  needs to do). `ScanningConfig::validated()` is the configuration-error enforcement point: mode
  enabled without a chosen `fail` policy, or a provider selected without its settings block, both
  fail with every problem listed at once. Only `icap`, `http` and `none` exist as
  `ProviderKind` variants — `ClamAv`/`Command` were deleted mid-session (see "Reuse considered").
  15 tests, including the RFC's example YAML parsing end to end.
- **`scanning/cache.rs`**: `VerdictCache<B: KvBackend>` over `hs-tables`, keyed on
  `(sha256_hex, provider_id, version_key)` (unversioned verdicts use a sentinel version key, so
  they still get their own cache line, TTL'd separately via `cache.unversioned_ttl`). Capacity
  enforcement evicts oldest-inserted-first via a secondary `(cached_at_ms, seq)` order index and a
  dedicated `hs-kv` counter keyspace for the insertion sequence — no unbounded table scan on the
  hot path. `Verdict::Pending` and `Verdict::Replaced` are never cached (see the module doc for
  why on each). 10 tests, including the signature-version-change-invalidates-the-cache test the
  RFC's testing section asks for by name, and a capacity-eviction test.
- **`scanning/providers/none.rs`**: always `Clean`, drains the source. 1 test.
- **`scanning/providers/icap.rs`**: the provider. Built on `icap-rs = "0.3"` (see "Reuse
  considered"), not a hand-rolled client — this module only adds the antivirus/adaptation
  semantics `icap-rs` has no opinion on: `X-Infection-Found`/`X-Virus-ID`/`X-Violations-Found`
  parsing across the ICAP response's own headers, the embedded HTTP response's headers, *and*
  chunk trailers (`to_verdict`); byte-comparing the returned body against what was sent to
  distinguish an unmodified `200` (still `Clean` — some AV services echo the body back rather than
  answering `204`) from a genuine `Verdict::Replaced`; and `ISTag` (from `icap-rs`'s own OPTIONS
  cache) as `engine_version`. 13 tests: 6 pure (`ParsedResponse::from_raw` against literal
  recorded bytes, no socket) plus 7 in `tests::live`, which start `icap_rs::server::Server` — a
  real, independent RFC 3507 implementation — in-process and drive `IcapScanner` against it end to
  end (OPTIONS/ISTag, `204`, an infection header on a genuine `200`, preview-then-remainder body
  delivery verified by the server-side handler reading the full body back, `Transfer-Ignore`
  skipping proven by asserting the handler is never called, connection-refused). See "What has
  never run" below for what this does and does not prove.
- **`scanning/providers/http.rs`**: the JSON submit-and-poll contract this crate defines (RFC
  section 3.1 names CrowdStrike Falcon as the motivating case; there is no existing standard wire
  format to adopt for "a JSON scanning API" the way there is for ICAP, so this is this crate's own
  small contract, documented in the module doc, the same pattern `hs-modules`'
  `HttpCallbackClient` already established for module hooks). Covers clean/infected/unscannable/
  pending-then-poll/replaced (base64-encoded content), auth-token-as-bearer, and connection-refused
  classification. 9 tests against a real in-process `axum`/`tokio` mock HTTP server (no external
  network).
- **`scanning/engine.rs`**: `ScanEngine<B>`, the orchestrator. Read this module's doc first — it
  states the two guarantees the user called out as the ones to get right, and *how* they are
  structural, not just documented:
  1. **Encrypted media is never reported clean.** `looks_like_encrypted` (Matrix clients upload
     encrypted attachments as `Content-Type: application/octet-stream`, since the plaintext MIME
     type lives in the encrypted event's own content, never the HTTP upload — a documented
     heuristic, not a certainty, erring toward the safe direction per RFC section 7) runs *before*
     any provider call; when it fires, `evaluate` never constructs a `ScanSource` or touches the
     provider at all, so there is no code path through which a provider's answer could become a
     false "clean". Directly tested with a provider fake that answers `Clean` to everything,
     asserting `evaluate` still returns the encrypted policy outcome *and* that the fake was never
     called (`tests::encrypted_content_is_never_reported_clean_even_if_the_provider_would_say_so`).
  2. **A down scanner fails only by explicit operator choice.** `ScanEngine::new` requires an
     already-`validated()` config; `ScanningConfig::validated()` is the only path scanning can be
     enabled without a chosen `fail` policy failing to compile-time-adjacent (build-time-adjacent)
     construction. Timeout/connection-error/malformed-response are all routed through one
     `action_for` match on `self.config.fail`, tested under both policies individually plus a
     specific timeout test and a specific malformed-response test.
  Also implements: the verdict cache read/write around the provider call (cache hit records
  `ScanMetrics::record_cache_hit`, never `record_scan` — tested); a bounded `Verdict::Pending` poll
  loop respecting `ScanContext::deadline`; RFC section 3.4's replacement rules as a pure,
  independently-tested function (`refuse_disallowed_replacement`) — refused when
  `allow_replacement` is off, refused when not an upload-time scan point
  (`allow_replacement_here: false`), refused for encrypted content *regardless* of the other two
  (tested directly, per the RFC's explicit ask, not only as a side effect of guarantee 1's
  short-circuit); and audit-entry writing (infected, scanner-error, replacement-applied with both
  content hashes — `original_sha256`/`adapted_sha256`). 19 tests.
- **`scanning/metrics.rs`**: `ScanMetrics`, registered into `hs_telemetry::metrics::Metrics`'s
  shared `Registry` (added `hs-telemetry` as an in-tree path dependency — consuming its interface,
  not editing its crate). The four metrics RFC section 8 names:
  `hs_media_scans_total{provider,verdict,source}`,
  `hs_media_scan_duration_seconds{provider,verdict,source}`,
  `hs_media_scan_cache_hits_total{provider}`, `hs_media_scan_errors_total{provider,kind}` —
  registered per `hs_telemetry::metrics`'s own naming convention (no `_total` in the string passed
  to `registry.register`, the doubled-suffix bug that convention's doc warns about explicitly).
  2 tests, including one asserting the exact non-doubled metric names on the wire.
- **`scanning/audit.rs`**: `AuditSink` trait; `TracingAuditSink` (default, logs via `tracing`);
  `InMemoryAuditSink` (bounded ring buffer, backs the admin "recent verdicts" surface and this
  session's tests); `FanOutAuditSink` (write to several sinks — use both `Tracing` and `InMemory`
  in a real deployment). 3 tests.
- **`scanning/admin.rs`**: `ScanAdmin` trait (`recent_verdicts`, `rescan`, `rescan_many` with a
  default fan-out-to-`rescan` implementation, `provider_health`) and `ProviderHealth`. **Interface
  only — no concrete implementation** (see "Next" below for exactly why and where one belongs).

### Not done: wiring into `crate::repository::MediaRepository`

`ScanEngine::evaluate` is written to be a drop-in call from any of RFC section 4's four scan
points (it takes plain bytes, a content type, a `ScanContext`, and returns an `EngineDecision` —
`Allow` / `AllowReplaced` / `Reject` / `StoreQuarantined` — that already encodes what the caller
should do), but **no call site actually calls it yet**. `crate::repository::MediaRepository::new`
and its `upload`/`complete_reservation` methods are unchanged from session 1. This is this
session's largest incomplete piece; see "Next" for the concrete plan.

### What has never run against a real daemon (constraint: no Docker, no network scanners here)

- **`scanning::providers::icap`**: no real ICAP server (c-icap, a commercial appliance, or an
  ICAP gateway) has been reached. `tests::wire`-equivalent pure tests use
  `icap_rs::response::Response::from_raw` against literal bytes (no daemon, no socket).
  `tests::live` uses `icap_rs::server::Server` — a real, independent RFC 3507 implementation,
  running in-process — as the test double; this proves `IcapScanner` and `icap-rs`'s own client
  and server agree on the wire protocol (OPTIONS/ISTag, 204, an infection header on 200,
  preview-then-remainder delivery, Transfer-Ignore skipping), which is the strongest test double
  achievable without an external daemon, but it is **not** proof of interoperability with a real
  c-icap+ClamAV stack, a commercial ICAP appliance, or ICAPeg. The EICAR-string-through-a-real-
  ClamAV test the RFC's testing section asks for by name was **not written** in this session (no
  reachable ICAP/ClamAV to skip cleanly against, unlike `tests/s3_backend.rs`'s pattern of a real
  env-var-gated network test) — this is a gap, not a skip; see "Next".
- **`scanning::providers::http`**: fully tested against a real in-process mock HTTP server
  (`axum`/`tokio`, no external network) — this is a complete test, not a partial one, since there
  is no real CrowdStrike Falcon (or similar) credential available here to test against, and the
  wire contract is this crate's own definition rather than a third-party spec to interoperate
  with.
- **No fuzzing was run** for the ICAP wire parsing this session adds on top of `icap-rs` (the
  crate's own parser is presumably fuzzed upstream; this crate's `to_verdict`/header-extraction
  layer was not run through `crates/hs-media/fuzz/`'s existing harness or a new one). Flagged as a
  gap, not attempted due to session time, same caveat session 1 already recorded about
  `cargo-fuzz`/nightly not being available in this sandbox.
- **Docker-based deploy verification**: `deploy/`'s c-icap+ClamAV reference compose file
  (RFC section 3.2 / decision 0007's requirement) was **not written this session** — see "Next".

### Reuse considered (decision 0007's required heading)

- **`icap-rs = "0.3.0"` (MIT, crates.io) — adopted, not reimplemented.** Evaluated by downloading
  and reading its source (`~/.cargo/registry/cache/.../icap-rs-0.3.0.crate`, extracted to the
  scratchpad) before writing any protocol code, per decision 0007's instruction. Verdict: it
  covers RFC section 3.3 comprehensively — `client/options_cache.rs` implements the exact
  `Options-TTL`-refreshed cache with RFC 3507 §5's ISTag-mismatch invalidation rule and §4.10.2's
  `Transfer-Preview`/`-Ignore`/`-Complete` policy (matched by `Content-Type` for RESPMOD, via its
  own `file_ext_from_request`/`ext_from_content_type`); `Client::send` drives the full preview/
  `100 Continue` handshake as one `await`; `Allow: 204` and connection reuse (`keep_alive`) are
  builder options; `Encapsulated` chunked framing is entirely internal to the crate. It is
  `#![forbid(unsafe_code)]` and built under `#![deny(clippy::pedantic, clippy::nursery, ...)]` —
  a stricter bar than this project's own. **No upstream contribution was needed or made**: nothing
  in section 3.3 was missing. `crates/hs-media/src/scanning/providers/icap.rs`'s module doc
  records this evaluation inline as well, for anyone reading that file without this status file.
  An earlier hand-rolled ICAP client (raw `TcpStream`, manual `INSTREAM`-style chunk framing, a
  home-grown connection pool and OPTIONS cache) was written *before* this evaluation happened
  (the user's first mid-session correction asked for ICAP depth without yet mentioning
  decision 0007) and was **deleted in full** once `icap-rs` was found to cover the same ground
  better and with far less code to maintain.
- **A direct clamd `INSTREAM` client, and a spawn-a-binary command runner — deliberately not
  built**, per decision 0007 directly: c-icap's `virus_scan` service already drives ClamAV with
  packaged container images, so either would have been a second path to a problem c-icap already
  solves. Config scaffolding for both (`ProviderKind::ClamAv`/`Command`, `ClamAvConfig`,
  `CommandConfig`) existed for part of the session and was deleted before any client code was
  written for either — `scanning/config.rs`'s module doc records this explicitly so a reader of
  just that file (not this status file) also sees the scope cut and its reason.
- **`hs-modules`' `CallbackRequest`/`CallbackResponse` JSON envelope — considered, not reused** for
  `scanning::providers::http`. The RFC's original text suggested extending that protocol; the
  rewritten RFC (after decision 0007) instead frames the `http` provider as its own small,
  independent JSON contract (submit bytes + headers, JSON verdict response), which is what this
  session implemented. `hs-modules`'s envelope is a general callback-shape for policy hooks
  (allow/deny/replace-content decisions about Matrix objects); a scan submission is closer to "post
  a file, get a verdict" than to that shape, and forcing it through the callback envelope would
  have meant JSON-encoding raw bytes (base64) for the *request* as well as the response, which
  `hs-modules`'s protocol was never designed to carry efficiently. Not reused, with this stated
  reason, per decision 0007's own test ("if it exists but is unmoduled/unsuitable, say so
  explicitly, with the reason").
- **`hs-telemetry::metrics::Metrics`** — reused as designed (this crate registers its own metric
  families into the shared registry via `with_registry`, exactly the pattern that crate's own
  tests demonstrate for other subsystems); no new metrics infrastructure was built.

### Next (in priority order)

1. **Wire `ScanEngine` into `crate::repository::MediaRepository`** (RFC section 4, points 1 and
   2 — the only two with an existing caller). Concretely:
   - Add `scanning: Option<Arc<ScanEngine<B>>>` to `MediaRepository` via a new
     `MediaRepository::with_scanning(self, engine: ScanEngine<B>) -> Self` builder method (do
     **not** change `MediaRepository::new`'s signature — every existing call site, including
     `crate::test_support`, `crate::state`'s tests and `crate::repository`'s own tests, constructs
     via `new` today and should keep compiling unchanged).
   - In `MediaRepository::upload` and `MediaRepository::complete_reservation`, after the existing
     size/quota checks and before the `object_store.put` call: if `self.scanning` is `Some` and
     `engine.mode() != ScanMode::Off`, build a `ScanContext` (media id, server name, uploader from
     `ctx.user_id`, `source: ScanSourceKind::Local` — see point 3 below for `Appservice`,
     `deadline: now + engine's configured timeout`, computed via the repository's existing
     injectable clock) and call `engine.evaluate(content_type, bytes.clone(), scan_ctx, now_ms,
     allow_replacement_here: true).await`. Match on the returned `EngineDecision`:
     - `Allow` → proceed exactly as today.
     - `AllowReplaced(content)` → store `content.bytes` instead of the original, use
       `content.content_type` if `Some` (else keep the declared type), and update
       `byte_length`/`content_type` in the `MediaRecord` accordingly.
     - `Reject(reason)` → return a new `MediaError` variant (add one, e.g.
       `MediaError::RejectedByScanner(String)`, mapped to a `4xx` `M_FORBIDDEN`-shaped
       `MatrixError` in `error.rs`, following that file's existing pattern) **without** calling
       `object_store.put` at all — per RFC section 5's `block` mode, infected/rejected content
       must never even be persisted.
     - `StoreQuarantined(reason)` → store normally, then call the existing
       `MetadataStore::set_quarantined` (already implemented and tested — see session 1's
       `crate::metadata`) with `by: Some("system:scan")` (or a distinct value distinguishing
       automated quarantine from an admin's manual one, an open call this session did not make).
   - **Known simplification to carry forward, not silently fix**: with no background-job
     scheduler in the codebase yet (session 1's own note: "background-job leasing is track 03/12's
     infrastructure"), `defer` and `quarantine` modes are implemented identically by
     `ScanEngine::action_for`'s `mode_default_action` (both map a bad verdict to
     `Action::Quarantine`) — see `scanning/engine.rs`'s module doc for the reasoning. `quarantine`
     mode's defining property (the scan does not block the upload response) is therefore **not**
     actually true yet in this design: `evaluate` is awaited synchronously inside the upload
     handler regardless of mode. Making `quarantine` mode genuinely non-blocking needs either (a)
     `tokio::spawn`-ing the scan-then-quarantine step (needs `MediaRepository<B>` and its fields to
     be `'static`-cloneable into the task, which they mostly already are — `MetadataStore<B>` is
     `Clone`, `object_store`/`policy`/`config`/`clock` are all `Arc`s already; the ergonomic
     obstacle is only `ScanEngine<B>` also needing to be cheaply `Clone`d into the spawned task,
     which it is not yet — wrap it in `Arc` at the call site) or (b) real background-job infra once
     it exists. Recorded as a decision to revisit, not a silent gap.
2. **Appservice bypass + `ScanSourceKind::Appservice`** (RFC section 4, point 4). In
   `crate::routes::upload`'s handlers, `MediaRequester(requester)` already exposes
   `requester.appservice: Option<hs_auth::requester::AppserviceIdentity>` (see session 1's
   `MediaRequester` decision). Thread that through: if `Some(identity)` and
   `identity.appservice_id` is in `ScanningConfig::appservice_bypass.exempt_appservice_ids`, skip
   the `engine.evaluate` call entirely and write an `AuditEntry { kind:
   AuditKind::AppserviceBypass { appservice_id }, .. }` directly via the engine's `AuditSink`
   (`ScanEngine` does not currently expose its `audit` field publicly — add an accessor, or a
   `ScanEngine::record_bypass(&self, ctx, appservice_id)` convenience method). Otherwise pass
   `source: ScanSourceKind::Appservice` instead of `Local` in the `ScanContext`. This also needs
   `crate::policy::UploadContext` (or a new, richer context) to carry the appservice id through to
   wherever `ScanContext` gets built — today `UploadContext` only has `user_id`/`server_name`.
3. **Remote media fetch (RFC section 4, point 3)**: still blocked on track 06 exactly as session 1
   recorded for RFC 0007 — there is no federation media fetch code to wire scanning into yet. When
   it lands, the call site should use `allow_replacement_here: false` (RFC section 3.4 rule 1 —
   replacement is upload-only) and `ScanSourceKind::Federation`.
4. **A concrete `ScanAdmin` implementation** (`scanning/admin.rs` is interface-only). The natural
   home is an `impl ScanAdmin for MediaRepository<B>` once step 1 lands (rescanning needs object-
   store access to re-read bytes by media id; `recent_verdicts` needs a shared `InMemoryAuditSink`
   handle, which the repository would need to hold alongside its `ScanEngine`).
5. **`docs/rfcs/0011-admin-scanning-endpoints.md (not yet written)`**: referenced from `scanning/admin.rs`'s module
   doc as the wire-shape proposal to track 15, but **not actually written this session** — this is
   a doc-only gap, quick for the next session (or track 15 directly) to close; the trait in
   `scanning/admin.rs` already states the four operations RFC section 8 asks for, which is most of
   the content that RFC needs.
6. **`deploy/`**: a c-icap + ClamAV reference compose file (decision 0007 / RFC section 3.2), plus
   a status-file note for track 12 to add the equivalent Helm sub-chart — neither was written this
   session.
7. **The EICAR-through-a-real-ICAP-daemon test** (`tests/icap_eicar.rs` or similar, env-var-gated
   like `tests/s3_backend.rs`, skipping cleanly with an `eprintln!` when unreachable): not written.
   Once `deploy/`'s compose file exists (item 6), this becomes straightforward to add and to
   actually run locally against it.

## Session 1 (original delivery below, unchanged)

### Done

This session delivered the brief's day-one work and Phase 0 deliverables in full, plus the
Synapse layout adapter and both Phase 1/2 design RFCs. Not delivered: URL previews and federation
media fetching *implementations* (design-only, by the brief's own ordering — federation media is
explicitly blocked on track 06), GCS/Azure backends, admin endpoints, retention sweeping, direct
media, metrics. See "Next" for the complete list.

- **`crates/hs-media/src/security.rs`**: the media security rules as rustdoc and tests (this
  track's item 1). `INLINE_SAFE_CONTENT_TYPES` (Synapse's `INLINE_CONTENT_TYPES` allowlist,
  behavior only) plus an `ALWAYS_ATTACHMENT` list (`text/html`, `image/svg+xml`, ...) checked
  first as defense in depth; `CSP_HEADER_VALUE` sent on every response unconditionally;
  `X-Content-Type-Options: nosniff` always sent; `content_disposition` (RFC 6266 quoting, RFC 5987
  `filename*=UTF-8''...` for non-ASCII, control-character/path-separator stripping,
  `CRLF`-injection-safe); `response_headers` composes all of it; `parse_range`/`ByteRange`/
  `RangeOutcome` for `Range:` (suffix ranges, open-ended ranges, clamping, first-of-multi-range,
  `416` on an unsatisfiable range). 32 tests.
- **`crates/hs-media/src/id.rs`**: `MediaId::generate` (24 random alphanumeric characters,
  Synapse's `random_string(24)` alphabet — behavior only) and `MediaId::parse` (validates an
  externally supplied ID, rejecting path traversal and any character unsafe as an object-store key
  or filesystem component).
- **`crates/hs-media/src/store.rs`**: `store::build` turns `hs_config::media::MediaStorageBackend`
  into `Arc<dyn object_store::ObjectStore>` — `Local` (creates its directory) and `S3` (via
  `object_store::aws::AmazonS3Builder`, no network needed to construct) implemented; `Gcs`/`Azure`
  return a clean `MediaError::Store("not yet implemented")` (the crate features `gcp`/`azure`
  already existed in the scaffold, off by default). `content_key`/`thumbnail_key` shard media by
  the first four characters (`b0`/`b1`/rest), independent of — not a copy of — Synapse's own
  on-disk scheme (`crate::synapse_layout` is what actually reproduces Synapse's scheme, for
  reading, not writing).
- **`crates/hs-media/src/metadata.rs`**: `MediaRecord`/`ThumbnailRecord` over `hs-tables`/`hs-kv`,
  `MetadataStore<B: hs_kv::KvBackend>` generic over the backend (tested against
  `hs_kv::memory::MemoryBackend`, per `docs/workstreams/README.md` rule 2). Keyed
  `(server_name, media_id)` deliberately origin-agnostic (a local upload's `server_name` is this
  server's own name; a federation-cached copy's is the origin's) so RFC 0007's remote cache needs
  no schema change. Quarantine (`quarantined_by`/`safe_from_quarantine`), async-upload
  `completed`/`expires_at_ms`, thumbnail variant listing.
- **`crates/hs-media/src/policy.rs`**: `UploadPolicy` trait (`check` before accepting bytes,
  `record` after they are stored) plus `InMemoryQuotaPolicy` (per-user and per-server cumulative
  byte totals). Pluggable, per the brief.
- **`crates/hs-media/src/sniff.rs`**: `sniff_format`/`format_matches_declared_type` (magic-byte
  detection, never fed back into the served `Content-Type` — see `security.rs`'s Rule 4) and
  `decode_with_limits` (the decompression-bomb defense: `image::Limits` on width/height/allocation,
  applied via `ImageReader::into_decoder` + `ImageDecoder::set_limits` *before* any pixel buffer is
  materialized).
- **`crates/hs-media/src/thumbnail.rs`**: crop (`resize_to_fill`) and scale
  (aspect-preserving, never-upscales) via `image::imageops::FilterType::Lanczos3`;
  `default_sizes()` re-exports `hs_config::media::MediaConfig::default().thumbnail_sizes` (32x32
  crop, 96x96 crop, 320x240/640x480/800x600 scale — the brief's table); `output_format_for` (PNG
  for alpha-capable sources — PNG, GIF, WebP — JPEG otherwise, Synapse's rule, behavior only);
  animated sources (GIF) thumbnail from the first frame only, never animated output;
  `ThumbnailPolicy` (`allow_dynamic` + max bounds) is the brief's "dynamic thumbnails" knob —
  off by default, matching Synapse's `dynamic_thumbnails: false` default.
- **`crates/hs-media/src/repository.rs`**: `MediaRepository<B>` — synchronous `upload`; async
  `create_reservation` (returns `(MediaId, expires_at_ms)`, `DEFAULT_RESERVATION_TTL_MS` = 24h,
  Synapse's default) + `complete_reservation` (checked expiry, checked "not already uploaded");
  `get_record` (quarantine looks identical to not-found to a non-admin caller; not-yet-uploaded
  vs. expired distinguished); `get_content` (range-aware, uses `object_store`'s `get_range` so a
  ranged download never reads the whole object); `get_thumbnail` (cache-or-generate, checks
  `ThumbnailPolicy` first); `set_quarantined`. 15 tests, including a real expiry test with an
  injectable clock.
- **`crates/hs-media/src/state.rs`**: `MediaState<B>` (embeds `hs_auth::state::AuthState`) and
  `MediaRequester` — see "Decisions made" for why this bridge exists and is the pattern other
  tracks composing `hs-auth` into their own state should copy.
- **`crates/hs-media/src/routes/`**: `upload.rs` (`upload_sync`, `create`, `put_upload` — the last
  checks `server_name == this server` and `uploader == requester` before completing a
  reservation, `404`-shaped either way so a reservation's existence is not leaked to a non-owner),
  `download.rs` (`download`, `download_with_filename`, shared `build_response` also used by
  `legacy.rs`), `thumbnail.rs`, `config.rs` (`m.upload.size`), `legacy.rs` (unauthenticated,
  frozen — see "Decisions made").
- **`crates/hs-media/src/router.rs`**: `authenticated_router`/`legacy_router` over
  `hs_http::router::Builder`, spec-relative paths (mounting/prefixing is the listener's job,
  matching `hs-auth`'s own `routes::router()` convention).
- **`crates/hs-media/src/multipart.rs`**: a `multipart/mixed` parser for MSC3916 federation media
  responses, built and fuzzed ahead of having a caller (track 06 does not exist yet) — see
  `docs/rfcs/0007-federation-media.md`. Bounded-iteration argument in the module doc; 15 tests
  including a mutation sweep (every single-byte flip and every truncation of a valid body must not
  panic).
- **`crates/hs-media/src/synapse_layout.rs`**: a read-only adapter over Synapse's on-disk media
  layout (`refs/synapse/synapse/media/filepath.py`, read for behavior — the actual Rust here is an
  independent reimplementation, not a translation) — all six subtrees (`local_content`,
  `remote_content`, `local_thumbnails`, `remote_thumbnail`, `url_cache` including both the dated
  and legacy sharded forms, `url_cache_thumbnails`). `classify_relative_path` is pure and
  filesystem-free (unit-tested directly); `SynapseMediaStore::iter_entries`/`read_content` do the
  real, read-only filesystem walk (integration-tested against a real temp directory tree).
- **`crates/hs-media/src/test_fixtures.rs`** (Cargo feature `test-fixtures`, on by default): valid
  PNG/JPEG/animated-GIF/WebP generators plus every adversarial case this track's brief names by
  name — `decompression_bomb_png` (a hand-crafted, ~50-byte PNG whose `IHDR` declares a 50000x50000
  image, built with `crc32fast` rather than via `image`'s own encoder specifically so generating
  the fixture never performs the large allocation it exists to test against), `truncate`,
  `svg_with_script`, `html_masquerading_as_image`.
- **`crates/hs-media/examples/gen_fixtures.rs`** + **`crates/hs-media/tests/fixtures/images/`**:
  the committed test corpus (10 files: 4 valid formats, decompression bomb, truncated header,
  truncated body, mismatched extension, adversarial SVG, HTML-as-image) and
  **`crates/hs-media/tests/image_corpus.rs`** (11 tests asserting this crate's own decode/sniff/
  thumbnail path handles every one of them correctly — valid ones succeed, adversarial ones fail
  cleanly, nothing panics).
- **`crates/hs-media/tests/s3_backend.rs`**: a real `object_store` S3-compatible put/get/get_range/
  delete round trip, entirely environment-variable-configured
  (`HS_MEDIA_TEST_S3_{BUCKET,ENDPOINT,ACCESS_KEY_ID,SECRET_ACCESS_KEY}`), skipping cleanly
  (`eprintln!` + return, not `#[ignore]`, so the skip is visible in normal `cargo test` output)
  when they are not set — this track's "skip S3 tests cleanly when no credentials are present"
  item, satisfied with an actual network-capable test rather than only a config-construction test.
- **`crates/hs-media/fuzz/`**: a separate `cargo-fuzz`-shaped crate (its own `[workspace]`, per
  the standard `cargo fuzz init` layout — the root workspace's `crates/*` glob does not reach it)
  with three targets: `decode_image` (the decoder path), `multipart_parse` (the federation-response
  parser), `thumbnail_generate` (decode + resize + re-encode). Type-checks cleanly on stable
  (`cargo check` inside `crates/hs-media/fuzz/`); **not actually run** — this sandbox has neither
  `rustup`/a nightly toolchain nor `cargo-fuzz` installed (verified: `which cargo-fuzz` and
  `rustup toolchain list` both fail). Seed corpora (`fuzz/corpus/{decode_image,thumbnail_generate,
  multipart_parse}/`) copied from the committed test fixtures. Whoever has `cargo-fuzz` and a
  nightly toolchain should run `cargo +nightly fuzz run <target>` from `crates/hs-media/fuzz/`
  before trusting the decoders/parser against real adversarial input beyond this session's fixed
  corpus.
- **`crates/hs-media/benches/thumbnails.rs`**: a real Criterion benchmark (the stub `fn main() {}`
  replaced), generating a non-trivial synthetic JPEG source (1920x1080, high-frequency content so
  JPEG's DCT does real work) and benchmarking every one of the five default sizes plus one dynamic
  1600x1600 scale. Actually run this session (`cargo bench -p hs-media --bench thumbnails --
  --sample-size 10 --measurement-time 1`, a reduced-sample invocation appropriate for a shared,
  contended sandbox machine — not a recorded perf baseline, just proof the benchmark executes
  correctly end to end and produces numbers for every size).
- **`docs/rfcs/0006-url-previews.md`**: design (not implemented) for `GET .../preview_url` —
  threat model (SSRF, DNS rebinding, redirect-based blocklist bypass, resource exhaustion,
  userinfo credential leakage), IP-range-blocklist-against-resolved-addresses, DNS pinning,
  manual re-checked redirect following, bounded fetch, caching. Proposes a shared `crate::fetch`
  primitive RFC 0007 also uses.
- **`docs/rfcs/0007-federation-media.md`**: design (not implemented, blocked on track 06) for
  fetching remote media over federation — a `FederationMediaClient` trait this crate needs from
  track 06, the multipart/redirect/legacy-proxy fetch flow, reuse of RFC 0006's fetch primitive
  for SSRF defense against a malicious/compromised origin's redirect, the remote cache (no schema
  change needed — see `metadata.rs` above), and a note that this crate will also eventually need a
  multipart *builder* (only a parser exists today) to serve federation media requests itself.
- 160 tests total (148 unit + 11 image-corpus integration + 1 S3 integration), `cargo fmt -p
  hs-media` and `cargo clippy -p hs-media --all-targets -- -D warnings` both clean.

## In progress

Nothing left mid-flight; this session's scope is complete and green.

## Next

In the brief's stated priority order:

- **URL previews** (RFC 0006 exists; no code). Needs `reqwest` added to `hs-media`'s own
  `Cargo.toml` (already an `[workspace.dependencies]` entry, added by track 15 — no workspace
  change needed) plus a DNS resolver capable of returning individual IPs before connecting
  (not yet in the workspace).
- **Remote media fetching over federation** (RFC 0007 exists; no code) — blocked on track 06's
  federation client existing, per this track's own brief and the dependency graph in
  `docs/workstreams/README.md`.
- **Quarantine and retention as admin operations**: `MediaRepository::set_quarantined` exists and
  is tested; no admin HTTP endpoint calls it yet (track 15's admin API surface). A
  `remote_media_retention`-driven eviction sweep is unimplemented (the config field exists; no
  background job runs it — background-job leasing is track 03/12's infrastructure, this crate
  would only supply the sweep logic once that exists).
- **Direct media for bridges** (track 11 collaboration) — not started; needs track 11's real
  `AppserviceRegistry` (today only `hs_auth::appservice::InMemoryAppserviceRegistry` exists) and,
  per `PLAN.md` 8.4, an embedded mini federation server for `hs-bridge-conformance` to test
  against, which itself depends on RFC 0007.
- **Federation media endpoints this server serves** (as opposed to fetches) — not implemented;
  needs X-Matrix request verification from track 06/07 and the multipart *builder* noted in RFC
  0007 section 3.5.
- **Admin media endpoints** (list, delete, purge, quarantine by room/user) — not started, track 15
  collaboration.
- **Metrics** — not started; would use `hs-telemetry` once this crate's HTTP surface is actually
  mounted by a real listener.
- **GCS and Azure backends** — the `gcp`/`azure` Cargo features exist (inherited from the
  scaffold) but `store::build` returns a clean "not implemented" error for both; only local
  filesystem and S3-compatible were this session's scope.
- **Fuzzing**: run the three targets in `crates/hs-media/fuzz/` for real, for hours/overnight, with
  `cargo-fuzz` and a nightly toolchain (see "Done" above for why this session could not).
- **Pre-generation of thumbnails at upload time** was one of the brief's "open questions to settle
  first"; this session settled it as on-demand-generate-then-cache (see "Decisions made"), so
  pre-generation is not planned unless a later session revisits that decision.

## Blockers

None for this session's delivered scope. Remote media fetching (RFC 0007) is blocked on track 06
existing, as the brief itself anticipates.

## Interfaces provided

- **`hs_media::security`**: `is_inline_safe`, `disposition_kind`, `content_disposition`,
  `response_headers`, `parse_range`/`ByteRange`/`RangeOutcome`, `CSP_HEADER_VALUE`,
  `INLINE_SAFE_CONTENT_TYPES`. Anything serving media-like content (avatars, previews) should route
  its response headers through `response_headers` rather than reinventing the inline/attachment or
  CSP decision.
- **`hs_media::{MediaRepository, MediaState, MediaRequester, MediaError, MediaId}`**: the crate's
  main surface, generic over `B: hs_kv::KvBackend`.
- **`hs_media::router::{authenticated_router, legacy_router}`**: spec-relative `axum::Router`
  fragments plus `hs_http::router::RouteManifest`, ready for whichever crate owns the real client
  listener to mount under `/_matrix/client/v1/media` and `/_matrix/media/v3` with a concrete
  `MediaState<B>` supplied via `.with_state(...)`. Both now also serve `GET /preview_url`
  (session 4).
- **`hs_media::router::v1_router`** (session 4, new): the same shape, for
  `/_matrix/media/v1` — currently just `POST /create` (MSC2246). **Not yet mounted by `hs-cli`** —
  see session 4's "Wiring the integration lead must add" for the exact lines.
- **`MediaRepository::preview_url`/`resume_pending_scans`** (session 4, new): see session 4's
  entries above for what each guarantees.
- **`hs_media::synapse_layout`**: `SynapseMediaStore`, `SynapseMediaEntry`,
  `classify_relative_path` — track 13's importer's read-only source for a Synapse media store on
  disk. `docs/compat/synapse-importer-mapping.md` (the policy for what the importer does with what
  this finds — conflict handling, ID remapping) does not exist yet; per this track's instructions,
  writing it is track 13's job, not this crate's — this module is ready to be consumed by it.
- **`hs_media::multipart`**: `parse`, `boundary_from_content_type`, `Part`, `MultipartMixed` — a
  federation-client-independent building block, ready for track 06 (see RFC 0007).
- **`docs/rfcs/0006-url-previews.md`**, **`docs/rfcs/0007-federation-media.md`**: design documents,
  open for review by their listed consumer tracks.

## Interfaces needed

- **06 (federation)**: the `FederationMediaClient` trait RFC 0007 section 3.1 proposes — needed
  before remote media fetching can be implemented at all.
- **07 (auth)**: consumed today via `hs_auth::{state::AuthState, requester::Requester,
  middleware}` — no changes needed, but see "Decisions made" for the `MediaRequester` bridge this
  crate had to add because `Requester: FromRequestParts<AuthState>` is not generic over caller
  state.
- **11 (appservices/bridges)**: the real `AppserviceRegistry` and, per `PLAN.md` 8.4, an embedded
  mini federation server, both needed for direct media.
- **13 (config/compat)**: `docs/compat/synapse-importer-mapping.md` (the importer policy that
  consumes `hs_media::synapse_layout`); the Synapse `homeserver.yaml` mapping for the new
  `MediaConfig` fields RFC 0006 proposes (`url_preview_timeout`, `url_preview_max_fetch_size`,
  `url_preview_cache_lifetime`) — **still not implemented as of session 4**, which built
  `crate::preview` against hardcoded constants instead precisely because adding fields to
  `hs_config::MediaConfig` is outside this track's edit permission; see session 4's entry above.
- **14 (test/conformance)**: Complement media tests and differential tests against Synapse 1.161
  for headers and thumbnail dimensions (this track's definition of done) — not yet run against
  this crate; this crate's own test count (258 as of session 4) is not a substitute for that.
- **15 (admin API)**: admin media endpoints (list/delete/purge/quarantine) calling into
  `MediaRepository`.
- **hs-cli (integration lead)**: the two changes to `crates/hs-cli/src/serve.rs` session 4's
  "Wiring the integration lead must add" spells out verbatim — mounting `v1_router` at
  `/_matrix/media/v1` (required to close the Complement 404) and, optionally, calling
  `resume_pending_scans` at startup (turns the durability fix's "intent survives a crash"
  guarantee into "a restart actually resolves it").

## Decisions made

- **Media IDs match Synapse's shape exactly (24 random alphanumeric characters)** so imported
  Synapse media IDs and freshly generated ones are indistinguishable, mirroring track 07's same
  decision for token shapes. `MediaId::parse` (for externally supplied IDs — remote servers,
  imported data) is deliberately more permissive than `MediaId::generate`'s own output shape, only
  rejecting what would be unsafe as a path/object-store-key component. See `crate::id`'s module
  doc.
- **`MediaRequester`: a thin per-crate bridge, not a generic `hs-auth` extractor.**
  `hs_auth::requester::Requester` implements `FromRequestParts<AuthState>` for the concrete
  `AuthState` type (track 07 deliberately left mounting/composition to whichever crate owns a
  listener — see `docs/status/07-auth-and-identity.md`'s "Interfaces needed"). Since this crate's
  router state is `MediaState<B>`, not `AuthState`, a handler cannot take `Requester` directly.
  The fix: embed `AuthState` in `MediaState<B>`, implement `FromRef<MediaState<B>> for AuthState`
  (trivial — every field is an `Arc`), and define `MediaRequester(pub Requester)` implementing
  `FromRequestParts<MediaState<B>>` by building an `AuthState` via `FromRef` and delegating. Cheap,
  and the same pattern works for any crate in the same position — recorded here so another track
  hitting the identical issue does not have to rediscover it. `MediaError::Auth` wraps
  `hs_auth::error::MatrixError` (a distinct type from `hs_http::MatrixError` — `hs-auth` predates
  `hs-http`) verbatim so the real status/errcode reach the client rather than collapsing every auth
  failure into a generic `400`.
- **Thumbnails are generated on demand and cached, not pre-generated at upload time.** One of the
  brief's "open questions to settle first". On-demand matches Synapse's default and keeps upload
  latency independent of how many thumbnail sizes are configured; the cache (`MetadataStore` +
  object-store) makes the second request for any given size free regardless.
- **Thumbnail output is always PNG or JPEG, never WebP or AVIF**, even though this crate can
  *decode* WebP (`docs/workstreams/09-media.md`'s "Open questions to settle first" also asked about
  WebP/AVIF *output*). PNG for alpha-capable sources (PNG, GIF, WebP), JPEG otherwise — Synapse's
  rule, behavior only, `crate::thumbnail::output_format_for`. Revisit if a differential test against
  Synapse 1.161 (track 14) shows it now emits WebP thumbnails in some configuration; not observed
  in the reference source read this session (`refs/synapse/synapse/media/thumbnailer.py` was not
  read in full — this decision is based on the crate's own output-format simplicity, not a
  byte-for-byte Synapse behavior claim, unlike the on-disk layout in `synapse_layout.rs`, which
  *is* read directly from `refs/synapse/synapse/media/filepath.py`).
- **Animated sources always thumbnail to a single static frame** (first frame only) — matches
  Synapse's observed behavior and avoids the CPU/memory cost of thumbnailing every frame of a
  large animated GIF a client will only ever see as a static preview.
  `crate::sniff::decode_with_limits`'s doc explains why this falls out naturally from using
  `ImageDecoder`'s ordinary (non-`AnimationDecoder`) path.
- **Quarantine is indistinguishable from not-found to a non-admin caller** (`MediaError::Quarantined`
  maps to the identical `404 M_NOT_FOUND` shape as `MediaError::NotFound` — see
  `crate::error::tests::quarantined_media_looks_identical_to_not_found`), so quarantine state is
  never leaked to a caller checking whether some `mxc://` URI resolves.
- **Legacy media routes are frozen, not simply always-on.** `MediaState::legacy_freeze_ms` (`None`
  by default — matching `allow_legacy_unauthenticated_media: true` with no freeze date, a
  permissive default a real deployment can tighten once it sets a real cutover) — anything created
  at or after the configured instant is `404` on the legacy, unauthenticated path even though it
  remains fully servable on the authenticated one. See `crate::routes::legacy`'s module doc for the
  full rationale (without this, the authenticated-media requirement is pointless — any client can
  fall back to the always-unauthenticated legacy path).
- **`hs-kv`/`hs-tables` calls inside `MetadataStore` are synchronous, called directly from async
  handlers, not wrapped in `tokio::task::spawn_blocking`.** `hs-kv` transactions are deliberately
  synchronous by that crate's own design ("keep transactions short" — no `.await` inside a
  transaction body). For `MemoryBackend` (what this session's tests run against, per
  `docs/workstreams/README.md` rule 2) this is effectively free. A production `FjallBackend` caller
  on the hot HTTP path may want `spawn_blocking` if profiling shows contention; not done this
  session since no production listener exists yet to profile. Noted in `metadata.rs`'s module doc
  as a flagged follow-up, not a silent gap.
- **`futures`, `hex` and `http-body-util`, present in the crate's Cargo.toml scaffold before this
  session, were removed** (unused by anything written this session — verified by grep before
  removing) to keep the dependency tree lean per this track's instructions. `http` was kept even
  though this crate only ever reaches it through `axum::http` today, since header-type code outside
  an axum handler (the RFC 0006/0007 fetch primitive, notably) is likely to want it directly.

## Shared dependencies added

**Session 2 added one new `[workspace.dependencies]` entry: `icap-rs = "0.3"`** (MIT), a
tokio-based RFC 3507 ICAP/1.0 client/server library — see the session 2 section above ("Reuse
considered") for the evaluation that justified adopting it instead of a hand-rolled client.
Session 2 also added, to `crates/hs-media/Cargo.toml` only (all already present in the root
workspace table before this session, added by other tracks): `reqwest` (track 15), `base64`
(root), `prometheus-client` (root), `serde_yaml_ng` (root), `schemars` (root), plus a new in-tree
path dependency on `hs-telemetry` (for `scanning::metrics::ScanMetrics` to register into the
shared `Registry`).

Session 1 added none, beyond what is recorded below for that session:

None. Every dependency this session's code uses (`object_store` with its already-enabled `aws`
feature, `image` with its already-enabled `jpeg`/`png`/`gif`/`webp` features, `ruma`, `crc32fast`,
`tempfile`, `criterion`) was already present in the root `Cargo.toml`'s `[workspace.dependencies]`
before this session started (`crc32fast` and `ruma` added by tracks 07 and 02/07 respectively, per
their own status files) — this session only added ordinary path dependencies on `hs-kv`,
`hs-tables`, `hs-auth`, `hs-http` and `hs-config` to `crates/hs-media/Cargo.toml` itself (in-tree
crates, not workspace-level entries) plus `libfuzzer-sys` inside the standalone
`crates/hs-media/fuzz/Cargo.toml`, which is its own separate `[workspace]` by design (see "Done")
and therefore outside the root workspace's dependency table entirely.
