# 0016. Configuration lives in the database, not in a file

Status: accepted, partly implemented. Owner: track 13 (config), `crates/hs-config`. Consuming
tracks: 15 (admin API, `crates/hs-admin`), 16 (management web interface, `web/`), and whoever owns
the binary's boot path (`crates/hs-cli`).

Companion artifacts: `crates/hs-config/src/{document,store,layered}.rs` (the implementation),
`docs/next-steps.md` item 2, `crates/hs-admin/openapi/openapi.yaml`'s `/config*` operations (the
API shape, declared long before this and unchanged by it).

## 1. Why

The project owner's stated product priority is that this server be **fun and easy to install and
administer**, with the management web interface as the centrepiece. Every competing homeserver is
configured by hand-editing a file on a host and restarting; Synapse ships no admin UI at all. That
is the differentiator this project is spending its effort on.

Configuration through the UI is impossible while a file is the source of truth. A web interface
that writes YAML to a container's filesystem is writing to a layer that vanishes on redeploy, on a
host it may not share with the process it is configuring, in a format that has comments and
ordering a round trip destroys. Every homeserver that has tried this has ended up with a UI that
can *show* the configuration and not change it — which is exactly what this project's admin
interface was before this RFC.

There is a measured second reason. On 2026-09-20, against the published image, `hs generate-config`
wrote **158 lines of YAML**, of which three (`data_dir`, the media `path`, `signing_key_path`) had
to be repointed by hand before the container could write anything at all, and a fourth
(`auth.enable_registration`) before anyone could sign up. Nothing was broken; four hand-edits
simply stood between a `docker pull` and a working server. Most of those 158 lines describe things
that have no business being decided before the server starts.

The owner's instruction, on the same day: *"we don't have to keep the original yaml config format;
all configuration should actually live in the database so that it can be modified by the web ui."*
The YAML schema is therefore not a compatibility constraint here. It survives as a *bootstrap* and
*import* format, not as the source of truth.

## 2. The design

### 2.1 Layers

The effective configuration is the merge of four layers, lowest precedence first:

| Layer | Where it comes from | Who writes it |
|---|---|---|
| `default` | the schema's own `serde(default)` | nobody |
| `file` | `-c homeserver.yaml`, if one is given at all | an operator, by hand, once |
| `database` | the `config` keyspace in this server's own store | the admin API, i.e. the web interface |
| `environment` | `HS__SECTION__KEY` variables | the deployment: a Kubernetes manifest, a systemd unit, a compose file |

Two of those orderings are the whole design, and both are deliberate:

**The database beats the file.** If the file won, then every change an operator made in the web
interface would be silently reverted on the next restart by a `homeserver.yaml` that is still
mounted and that nobody remembers is there. That failure is invisible until it bites — the UI
reports success, the server obeys for the rest of its life, and the setting reverts at the worst
possible moment. Ranking the database above the file makes the file what it actually is after the
first boot: a seed.

**The environment beats the database.** A deployment that pins a setting in its manifest has said
something the server must not quietly undo; a GitOps repository is a source of truth in its own
right, and a config value that a cluster reapplies every reconcile cannot be owned by a UI. But the
honest consequence is that some settings are then not editable, and the API must *say so* rather
than accept a write it knows will have no effect. `Layers::pinned_by_environment` exists for this:
the admin API refuses such a patch, names the settings, and the UI renders them read-only with the
reason. Storing a value that is shadowed and reporting success would be a lie with a long fuse.

### 2.2 What cannot live in the database

`storage` — the section that says *where the database is*. It is read before there is a database to
read it from. `ConfigStore::patch_section` refuses it with an error that says so, rather than
accepting a write that would be stored somewhere nobody will look.

That is the entire bootstrap surface. Everything else — `server`, `listeners`, `media`,
`federation`, `rate_limits`, `auth`, `appservices`, `telemetry`, `cluster` — is storable, whether
or not changing it needs a restart. "Needs a restart" and "cannot be stored" are different
properties and are reported separately; `hs_config::reload` already owns the first one.

### 2.3 Patches, and what a reset means

Writes are RFC 7396 JSON Merge Patch, per section. A `null` removes the key rather than setting it
to null, which gives reset-to-default for free: the key leaves the merged document and the schema's
own default stands.

It reverts to the *default*, not to whatever a lower-precedence layer said. That is the only
reading that gives an operator the same result whether or not a bootstrap file happens to be
mounted — "reset" means "as if nobody had ever set this", and an operator should not have to know
what is in a file they have never seen to predict what a button does.

### 2.4 Concurrency

One revision counter for the whole store, bumped on every write, carried by the admin API as
`If-Match`. Two operators editing different sections at the same time is a revision mismatch for
the second one, which is stricter than it needs to be and produces a retry rather than a wrong
result; a per-section revision would be a strict improvement and is not implemented.

Validation happens against the configuration a patch would *produce*
(`Layers::resolve_with_patch`), never against the patch alone: a value can be legal on its own and
contradict another section, and a section can be invalid until something else is set.

### 2.5 History

Every write appends a `ChangeRecord` — revision, section, the patch, who, when — in the same
transaction as the write itself. The web interface gets a change history without a second store,
and an operator gets an answer to "who turned registration on".

## 3. Migration

An existing deployment keeps working untouched. On first boot, if the store has never been written,
the file's document is copied into it and marked `seeded_from`. From then on the file is inert:
re-seeding is a no-op, so a file left mounted after the move cannot revert a UI change, and an
operator who edits the file and restarts will find — correctly, and this is the surprising part —
that nothing happens. `hs config import` is the explicit way to push a file's contents in again.

A Synapse config translated by `hs-compat` seeds the store exactly as a native one does; the
translation already produces a `Config`, and this layer consumes documents.

## 4. Consequences, including the unpleasant ones

- **A file edit stops working after the first boot.** This is the point, and it will still surprise
  someone. `hs config show` reports every setting's origin so the answer is one command away, and
  the generated bootstrap file should say it in a comment.
- **Secrets are in the database.** They were in a file before, or in a file the config pointed at.
  `*_file` references are stored unresolved and resolved at load, so the Kubernetes-secret pattern
  still works and the secret itself need never enter the store. Secrets that *are* stored are never
  returned by the API in the clear.
- **A broken `listeners` change can lock an operator out of the UI that made it.** Mitigated by
  validating before writing, not by a rollback; a genuine watchdog (apply, and revert if nobody
  confirms within N seconds) is worth doing later and is not done.
- **Cluster members now share their configuration**, which is a real improvement over N copies of a
  file kept in sync by hand, but nothing propagates a change to a running peer yet: the store is
  read at boot and on reload. `hs-kv`'s `watch` is the obvious seam.
