# 0028: 2026-10-04: a deployment declares bridge offerings once, and a bridge registered by hand beside an offering is named, not paused

Status: accepted (track 11; touches 12's chart, 15's admin API and 16's pages).

## The problem

RFC 0017 section 6 replaces the demo's shared WhatsApp registration (2026-09-25, made through
the wizard) with the WhatsApp offering. Two things had no answer: how a deployment says which
offerings it wants (decision 0010 keeps administered settings in the database, not in files),
and what the server does while both exist. The shared registration's exclusive
`@whatsapp_.*` covers every instance's ghosts (`@whatsapp_brandon_.*`), the registry's conflict
check allows it (neither claims the other's bot, the patterns differ), and every ghost's events
reach both bridges.

## What was chosen

1. **Declared once, then administered.** The chart's `bridges.offerings` (a list of a catalogue
   `type` plus the fields of `PUT /bridge-offerings/{type}`) reaches the server as
   `MYELIN_BRIDGES_OFFERINGS`. The manager creates each declared offering the first time it runs
   with it, records the type in its own row (`ManagerRow.declared`), and never applies it
   again. An offering that already exists is adopted as it is; one an administrator removes
   stays removed; one this server cannot create (a cluster runtime without a cluster) is logged
   and tried at the next start. A declaration that does not parse or names no catalogue type
   fails startup. It is the same pattern as `appservices.registration_files`: a file seeds, the
   database owns.
2. **Named, not paused.** A registration not made by the manager that is a bridge of an
   offered network (the same `io.myelin.bridge_type`, the catalogue's bot name, or an exclusive
   user rule covering the catalogue's ghosts) is reported on its health
   (`AppServiceHealth.overlaps_offering`) and on the offering
   (`BridgeOffering.overlapping_appservices`), with what to do, and logged at `WARN` when it
   appears. The server does not pause or remove it: someone may still be signed in through it,
   and cutting it off silently would lose their messages. The line offers Pause, which stops
   delivery at once and keeps the queue.
3. **An offering's options reach its instances' registrations.** The manager keeps a
   fingerprint of the registration it last wrote (`registered_fingerprint`, everything but
   `url`) and patches the namespaces and feature flags when the rendered one differs, so a
   changed `double_puppeting` adds or drops the owner's claim on the server, once. `url` is left
   out because an administrator points an instance run elsewhere at where it really listens.

## Consequences

- A new install of the demo comes up with the WhatsApp offering from
  `deploy/demo/values-bridges.yaml`; the existing demo keeps its offering untouched.
- The admin API grew two additive fields (OpenAPI 0.1.8); the web shows both.
- `hs_bridges::directory::OfferingAwareDirectory` wraps the appservice directory the admin API
  reads, so the registry (track 11's `hs-appservice`) stays unaware of offerings.
