# 0010. The server is administered through the admin API and the web interface, never by editing files

Status: accepted, 2026-09-27. Author: the owner.
Applies to every track; most directly 13 (configuration), 11 (bridges), 15 (admin API) and 16 (`web/`).

## Decision

The admin API and the web interface built on it are how this service is administered. An
operator never edits a configuration file, a YAML file, or a registration file to change what
the server does -- and that includes the web interface: **no page edits YAML, JSON or any other
file format as text.** Every setting an operator can change has a real control (a toggle, a
number, a list editor, a form for each entry of a list of objects), backed by a typed admin API
operation.

## What this means in practice

- **Configuration.** The database-backed settings (already editable in the interface) are the
  configuration. The Configuration page's JSON textarea, used today for arrays of objects, maps
  and anything else without a better control (`JsonControl` in
  `web/src/pages/config/SettingControls.tsx`), is replaced by structured editors: a repeatable
  form per entry for arrays of objects, key/value rows for maps. A setting whose shape the
  interface cannot render is a bug to fix in the interface, not a reason for a text box.
- **Bridges.** A bridge is registered, changed and removed through the Bridges section and the
  `appservices.*` / `bridge_offerings.*` operations. `appservices.registration_files` (a list of
  paths to registration YAML on disk, loaded at start) is a Synapse-migration path only: the
  importer reads it once into the registry, and it is not how a bridge is added.
- **Bootstrap is the one exception.** Something has to tell a process where its database is
  before it can read any setting from it. The file and `HS__` environment variables keep only
  what is needed to reach the database and serve the setup page (server name, data directory
  or database URL, listeners, and in cluster mode the per-replica mesh identity). In Kubernetes
  those are Helm values, set once at install. Everything past that point is the API's.
- **Read-only renderings are allowed.** Showing a generated file for an operator to copy or
  download (for example a bridge's own `config.yaml` for a bridge that runs elsewhere) is not
  administering this server. Asking the operator to edit it is: whatever a generated file needs
  to say is asked for in the form that generates it.

## Consequences

- The Configuration page is not complete while any setting falls through to the JSON control;
  that is now on the completeness queue in `docs/next-steps.md`.
- New settings, and new admin operations, arrive with their interface control in the same
  change. "Edit it as JSON for now" is not an acceptable interim.
- The per-replica settings found by track 05 (a replica's `listeners` and `cluster.mesh.port`
  seeded into the shared database) belong to the bootstrap exception and are excluded from
  seeding, rather than made editable per replica in the interface.
- **The bootstrap set, as built (track 13, 2026-09-27; `crates/hs-config/src/bootstrap.rs`).**
  `storage`, `listeners`, `server.server_name`, `server.signing_key_path`,
  `cluster.single_node`, `cluster.mesh` (port, advertise address, TLS paths, shared secret) and
  `appservices.registration_files`. These come only from the bootstrap file, `HS__` variables and
  the command line (Helm values in Kubernetes): never seeded into the shared database, refused by
  `config.update` and `hs config set`, ignored if a pre-0010 store holds them, and purged from
  such a store at boot. The server name is additionally recorded once as the database's
  identity, so a second start with only a data directory still knows it. Everything else is
  administered. `media.scanning` and `server.unstable_features`, which were only settable
  through files named on the command line, are now administered settings (the flags remain,
  deprecated). Registration files are imported once into the appservice registry, recorded and
  audited as `appservices.import`, and never read again.
