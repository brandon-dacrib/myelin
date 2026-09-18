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

## 3. Providers shipped

Three tiers, matching how `hs-modules` already tiers extensions, so an operator picks by capability rather than by rewriting anything:

| Provider | Transport | Covers |
|---|---|---|
| `clamav` | clamd `INSTREAM` over TCP or unix socket, streaming in chunks | ClamAV, and anything speaking clamd |
| `icap` | ICAP RESPMOD, RFC 3507, with `X-Infection-Found` and `X-Virus-ID` parsing and 204-means-clean | The enterprise standard: Symantec, McAfee, Sophos, Trend Micro and others, including vendor appliances that front CrowdStrike |
| `http` | The versioned JSON callback protocol `hs-modules` already defines, extended with an asynchronous submit-and-poll flow | CrowdStrike Falcon, VirusTotal, any cloud or in-house scanner |
| `command` | Spawn a binary, feed content on stdin, map exit codes | `clamscan` and simple local setups |
| `none` | — | Default. Scanning off. |

ICAP earns its place by covering many products with one adapter. CrowdStrike is reachable two ways: through an ICAP-speaking appliance, or through the HTTP provider against the Falcon API, which is submit-then-poll and therefore the reason `Verdict::Pending` exists in the interface rather than being bolted on later.

WebAssembly providers are permitted through the existing module host once that lands, and need no change here: a Wasm module implements the same HTTP-shaped contract in process.

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
    provider: clamav
    fail: closed           # closed | open
    timeout: 30s
    max_size: 100MiB
    oversize: quarantine   # allow | block | quarantine
    cache:
      ttl: 7d
      unversioned_ttl: 1h
      capacity: 100000
    clamav:
      address: unix:/var/run/clamav/clamd.ctl
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
- The EICAR test string end to end through the ClamAV provider, skipped cleanly when no clamd is reachable, which is the case in this environment.
- A protocol-level test for clamd `INSTREAM` and for ICAP RESPMOD against recorded exchanges, so the wire format is covered without either daemon present.
- Failure-mode tests: timeout, connection refused and malformed response, asserted under both `fail: open` and `fail: closed`.
- A test that a signature version change invalidates the cache.
- A test that encrypted media is reported unscannable rather than clean, since reporting it clean would be the dangerous failure.
