# 0008. Pluggable content scanning for uploads

Status: accepted, 2026-09-18. Author: integration lead. Owner: track 09 (media), with track 15 (modules) for the callback transport and track 15/16 for the admin surface.

Operators must be able to choose their own malware scanner and swap it without changing the server: ClamAV today, an enterprise agent such as CrowdStrike tomorrow, both during a migration. This RFC specifies that seam.

## 1. Why the existing hook is not enough

`hs_modules::ModuleHooks::check_media_for_spam` exists and mirrors Synapse's shape: one call, after upload, returning allow or deny. Real scanning needs more than that shape can express:

- A verdict is not binary. Clean, infected with a signature name, unscannable (encrypted, archive too deep, oversized), and scanner-error are four different outcomes with four different policies.
- Scanning is slow and sometimes asynchronous. A cloud scanner may accept a submission and return a verdict seconds later.
- The same bytes are scanned repeatedly without caching, since media is downloaded far more often than uploaded.
- There is no answer for what happens when the scanner is down, which is the question every operator asks first.

So content scanning gets its own provider interface. The module hook stays for policy that is not malware scanning (blocklists, content policy) and continues to be consulted independently.

## 2. The provider interface

In `hs-media`, a `ContentScanner` trait:

```rust
#[async_trait]
pub trait ContentScanner: Send + Sync {
    /// Stable identity, recorded in verdict cache keys and audit entries, e.g. "clamav".
    fn id(&self) -> &str;

    /// Signature or engine version, mixed into the cache key so a signature update
    /// invalidates prior verdicts. `None` means the provider cannot report one, and
    /// verdicts are then cached only for `cache.unversioned_ttl`.
    async fn engine_version(&self) -> Option<String>;

    /// Scan the content. Implementations MUST respect `ctx.deadline` and MUST stream
    /// rather than buffering the whole body where the transport allows it.
    async fn scan(&self, content: ScanSource<'_>, ctx: &ScanContext) -> Result<Verdict, ScanError>;

    /// Poll a previously returned `Verdict::Pending`. Providers that are always
    /// synchronous may leave the default, which returns `Unsupported`.
    async fn poll(&self, ticket: &ScanTicket) -> Result<Verdict, ScanError> { ... }
}

pub enum Verdict {
    Clean,
    Infected { signature: String, details: Option<String> },
    Unscannable { reason: UnscannableReason },
    Pending { ticket: ScanTicket, retry_after: Duration },
}

pub enum UnscannableReason { Encrypted, TooLarge, TooDeep, UnsupportedFormat, Other(String) }
```

`ScanSource` is a stream plus the declared content type, size when known, and the content hash. `ScanContext` carries the deadline, the uploading user, whether the upload arrived from a local client, an appservice or federation, and the media identifier.

## 3. What we ship, and what we deploy

**We ship one ICAP client. We ship no antivirus integration of our own.**

c-icap is a mature ICAP server that hosts scanning services, and its `c-icap-modules` `virus_scan` service drives ClamAV. That is the division of labour: c-icap owns talking to engines, we own talking to c-icap. Writing our own clamd client would duplicate a service that already exists, is packaged, and has ready-made container images.

### 3.1 Providers

| Provider | Role |
|---|---|
| `icap` | The provider. Everything reaches us through it. |
| `http` | Only for cloud APIs with no ICAP fronting, notably CrowdStrike Falcon, which is submit-then-poll and is why `Verdict::Pending` exists. |
| `none` | Default. Scanning off. |

Deliberately **not** shipped: a clamd client, a command runner, or any per-engine adapter. Each would be a second path to a problem c-icap already solves, and each would need its own maintenance, tests and failure modes. An operator wanting ClamAV runs c-icap in front of it, which is one container.

### 3.2 What an operator deploys

| Scanner | Deployment |
|---|---|
| ClamAV | c-icap with `virus_scan`, packaged images already exist; the Helm chart ships this as an optional sub-chart and the compose files include it |
| Commercial engines | Their own native ICAP interfaces: Symantec Protection Engine, Kaspersky, McAfee Web Gateway, Sophos via SAVDI, Trend Micro, F-Secure, MetaDefender, Check Point |
| Cloud APIs | An ICAP gateway such as ICAPeg (VirusTotal, Cloudmersive, ClamAV) or Cloudmersive's ICAP server; or our `http` provider directly |
| Anything else | Put it behind c-icap or ICAPeg |

The reference deployment (c-icap plus ClamAV alongside the homeserver) is documented and shipped as chart and compose configuration, so "turn on virus scanning" is a configuration change rather than an integration project.

### 3.3 The ICAP client itself

`icap-rs` is a tokio-based Rust ICAP/1.0 client and server library following RFC 3507, with protocol types, parsers and serializers. **Evaluate it before writing any protocol code**, per decision 0007. Use it if it covers what follows; contribute upstream if it is close; write our own only with a recorded reason.

Whether through that crate or otherwise, the client must do all of this, because the difference from a toy shows up in both throughput and correctness:

- **OPTIONS negotiation** before use, refreshed per `Options-TTL`, learning `Preview` size, `Max-Connections`, whether `Allow: 204` is advertised, and the `Transfer-Preview`, `Transfer-Ignore` and `Transfer-Complete` file-type lists.
- **Preview mode**: send the first N bytes, wait for `100 Continue` before sending the rest. Most verdicts are decided from a file header, so a large upload never crosses the wire. Not optional.
- **`204 No Content` means clean**, requested via `Allow: 204`, avoiding the server echoing the body back.
- **`Transfer-Ignore`** types skipped entirely rather than sent and discarded.
- **Connection reuse** within the advertised `Max-Connections`.
- **Correct `Encapsulated` framing** with chunked bodies, which is where naive implementations corrupt content.
- **Verdict headers across vendor spellings**: `X-Infection-Found`, `X-Virus-ID`, `X-Violations-Found`.

**`ISTag` is the engine version.** RFC 3507 defines it as an opaque tag the service changes when its configuration or signature set changes, which is exactly what the verdict cache in section 6 keys on. Cache invalidation after a signature update is therefore spec-native and free. Return the current `ISTag` from `engine_version` rather than inventing a versioning scheme.

## 4. Where scanning happens

1. **Local upload**, synchronous or deferred per mode, before the media is retrievable.
2. **Asynchronous upload completion**, at `complete_reservation`, same policy.
3. **Remote media fetched over federation**, before it enters the cache, because a bridge or client pulling a remote file is the same exposure.
4. **Appservice and bridge uploads**, through the ordinary upload path. Bridges are a major media source and get no exemption; a bridge may be configured to bypass only by explicit per-appservice configuration, which is recorded in the audit log.

Thumbnails are never scanned. They are derived from source bytes that were already scanned, and scanning them again doubles cost for no new information.

## 5. Modes and failure policy

```yaml
media:
  scanning:
    mode: block            # block | quarantine | defer | off
    provider: icap
    fail: closed           # closed | open
    timeout: 30s
    max_size: 100MiB
    oversize: quarantine   # allow | block | quarantine
    cache:
      ttl: 7d
      unversioned_ttl: 1h
      capacity: 100000
    icap:
      url: icap://c-icap:1344/virus_scan
      preview: negotiate   # negotiate | <bytes> | off
```

- **block**: the upload fails with a Matrix error until a verdict arrives. Safest, adds upload latency.
- **defer**: the upload succeeds, the media is not retrievable until a verdict arrives. Downloads return 404 in the interim, indistinguishable from unknown media, matching how quarantined media already behaves.
- **quarantine**: the media is served immediately and quarantined if the verdict is bad. Lowest latency, briefly exposes unscanned content. Correct for high-volume deployments that accept that trade.
- **off**: no scanning, and the readiness endpoint does not claim otherwise.

`fail: closed` treats scanner errors and timeouts as rejection; `fail: open` allows and records. Neither is defaulted silently: enabling scanning without choosing one is a configuration error, because the right answer is a policy decision and not a technical one.

## 6. Verdict cache

Keyed on `(sha256(content), provider_id, engine_version)`, stored through `hs-tables`, with the configured capacity and time to live. A signature update changes the engine version and so invalidates prior verdicts without an explicit purge. A cache hit must never be reported as a scan in metrics, or the metrics lie about scanner load.

## 7. End-to-end encrypted media, stated plainly

The server cannot scan encrypted media. It holds ciphertext and no key. This is not a limitation of this design; it is what end-to-end encryption means.

Therefore:

- Encrypted uploads yield `Unscannable { Encrypted }`, and the configured policy for that reason applies, defaulting to allow, since blocking it disables encrypted media entirely.
- The documentation must say this clearly, because an operator who deploys a scanner and believes encrypted rooms are covered has bought a false assurance. That is worse than no scanner.
- A client-assisted model, where a trusted scanning service receives the decryption key, is out of scope here and is the only way to scan encrypted content. If we implement it later it is a separate RFC.

## 8. Observability and control

- Metrics: `hs_media_scans_total` by provider, verdict and source; `hs_media_scan_duration_seconds`; `hs_media_scan_cache_hits_total`; `hs_media_scan_errors_total`. Cache hits are excluded from scan counts.
- Audit entries for every infected and every error verdict, including provider, signature and media identifier.
- Admin API: list recent verdicts, rescan a media item, rescan everything matching a filter after a signature update, and show current provider health. These extend the existing media resource in the admin OpenAPI document rather than forming a new one.
- Readiness: with `fail: closed`, an unreachable scanner makes the server report not ready rather than silently rejecting every upload.

## 9. Testing

- A fake provider driving every verdict, including pending-then-clean and pending-then-infected.
- The EICAR test string end to end through a real c-icap plus ClamAV, skipped cleanly when no ICAP service is reachable, which is the case in this environment.
- Protocol-level tests against recorded ICAP exchanges, so the wire format is covered without a daemon present: OPTIONS parsing, preview with `100 Continue`, preview answered with an early verdict so the body is never sent, `204` clean, `Transfer-Ignore` skipping, and `ISTag` changing between calls invalidating a cached verdict.
- Failure-mode tests: timeout, connection refused and malformed response, asserted under both `fail: open` and `fail: closed`.
- A test that a signature version change invalidates the cache.
- A test that encrypted media is reported unscannable rather than clean, since reporting it clean would be the dangerous failure.
