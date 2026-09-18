# Forward note: message importance ranking, and integrating attentionos

Status: design note, not scheduled work. Written 2026-09-18 at the user's request, to record the shape of a thing they intend to build later so the server does not foreclose it.

The user wants a service that ranks the importance of Matrix messages, feeding their local attention system (`~/Documents/git/attentionos`: ActivityWatch-derived watchers, local Ollama processing, outbound access disabled unless explicitly approved, third-party plugins treated as hostile until reviewed).

## ICAP is the wrong protocol for this

ICAP is the right answer for media (`docs/rfcs/0008-content-scanning.md`) and the wrong one here. Four reasons, each independently sufficient:

1. **Events cannot be modified.** A Matrix event is hashed and signed at creation. ICAP's central capability, returning a replacement body, is unusable: there is nowhere to put a score. You would be left smuggling it through a custom header.
2. **ICAP has no structured return.** A service answers `204`, a block, or a replacement body. A ranking result is structured data (a score, labels, reasons), which the protocol has no room for.
3. **Ranking is per-recipient.** Importance is relative to a reader: the same message is urgent for one member and noise for another. ICAP is one transaction over one body, so per-recipient ranking means one round trip per recipient per event. At fan-out that is untenable.
4. **Encryption.** The server holds ciphertext for encrypted rooms and no key. Server-side ranking is blind exactly where it matters most, since direct messages are almost always encrypted. This is the same wall as scanning, but far more limiting, because scanning a public file store still has value while ranking only encrypted conversations has almost none.

ICAP is a proxy-era protocol for adapting HTTP bodies in flight. Matrix events are neither HTTP bodies nor in flight through a proxy. Forcing them through it buys nothing and costs the four problems above.

## What fits: attentionos as an appservice

The idiomatic Matrix answer, and the one that suits a local-first, privacy-conscious system:

- **An appservice is a legitimate room member with its own encryption keys.** This is exactly how bridges read encrypted rooms today, and the machinery is already in scope for track 11: MSC3202 device masquerading and one-time key counts, MSC4203 to-device delivery, MSC4190 device management. A ranking service registered as an appservice receives events through transactions and can decrypt the rooms it belongs to.
- **Ranking then happens where the plaintext already is**, on the user's own machine, with Ollama, matching attentionos's stated posture of keeping processing local and outbound access off. No message content leaves the host.
- **Scope is explicit.** An appservice registration declares namespaces, so what it can see is inspectable and reviewable, which suits a project that treats plugins as hostile until audited.

The complement, for signals that do not need message bodies: the module hook framework (`hs-modules`, eleven callback categories over a versioned JSON HTTP protocol) can already return structured data and sees server-side facts that are visible even in encrypted rooms, such as sender, room, mention status, thread participation, reply depth and timing. Those are strong ranking features on their own and cost no decryption.

## What this project should do now

Nothing beyond staying out of the way. Specifically, three things already planned must not regress, because they are what make the above possible:

1. **Encrypted appservices must work properly**, which is track 11's assignment and already the user's stated priority for bridges. A ranking appservice is a bridge-shaped consumer and benefits from the same work.
2. **The module hook protocol should keep returning structured JSON**, not collapse to allow-or-deny, so a future ranking hook has somewhere to put a score.
3. **Per-user, per-event server-side annotations are worth designing when the need is real.** Rankings cannot live in the event, so they need a home: per-user account data is the specification-native option, a dedicated annotation store is the efficient one. Deferred deliberately; noting it so the storage layer is not designed in a way that makes it awkward.

## The honest caveat

If the ranking service runs on the user's machine as an appservice, it must be reachable by the homeserver, which means either running the homeserver locally too (entirely reasonable: this project supports a single static binary on a small host) or exposing the appservice to wherever the homeserver runs. The second option contradicts attentionos's outbound-access posture and should be avoided. The clean version of this is a local homeserver and a local ranking appservice on the same host, which is a configuration this project explicitly supports.
