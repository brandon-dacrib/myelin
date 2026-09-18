# 13. Configuration, compatibility and migration

Wave 1, starts day one. Much of this track is analysis that needs no code from anyone else.

**Expert profile.** Has run Synapse at scale; PostgreSQL; data migration; config systems; knows the admin tooling ecosystem (`synapse-admin`, Draupnir, Mjolnir, MAS).

**Mission.** Native configuration, Synapse configuration translation, the online importer, the Synapse admin-API and metric-name surfaces, the CLI shims and the migration runbook. See `PLAN.md` section 9.

**Owns.** `hs-config` (schema, defaults, validation, environment and file-secret overrides, hot reload of reloadable sections); `hs-compat`: the translation table for Synapse's 229 options and 51 experimental flags (mapped, mapped with documented difference, unsupported with reason), the translator and its report, the importer (online and incremental from Synapse schema 94, with the media layout adapter from 09), the `/_synapse/admin` surface (77 routes) mapped onto 15's admin model, `/_synapse/client/*` routes, `/_synapse/mas/*` in delegation mode with 07, the Synapse metric-name exporter with 12, the CLI shims and the shared-secret registration protocol with 07, `SYNAPSE_*` environment handling in the image entrypoint with 12, the rule for accepting legacy sync tokens with 05, the migration runbook and rehearsal tooling with 14, maintenance of `docs/synapse-inventory.md` per Synapse release, the pinned-version policy.

**Provides.** Week 4: the native config schema. Then the translation table, the importer, the compat surfaces.

**Consumes.** Every track's data model; 14's differential harness.

**Day-one work.** Native config schema design; the translation table (pure analysis from the inventory and Synapse's config docs; large and fully parallelizable); the importer mapping document (Synapse tables to our tables, reading `refs/synapse/synapse/storage/schema/` for structure only); the admin route map; the CLI shim specification; a corpus of real `homeserver.yaml` files (Ansible, Docker generator, Helm, NixOS outputs).

**Phase 0 deliverables.** `hs-config` complete; translator v0 with the table complete and tested against the corpus; importer design plus a read-only extraction prototype against a Synapse 1.161 fixture database produced with 14.

**Phase 1 and 2 deliverables.** The importer complete and incremental with the cutover procedure; admin-API compatibility; the metric exporter; the shims; the runbook; rehearsals on volunteer deployments; the reverse exporter after 1.0.

**Definition of done.** Translation table at 100 percent with tests over the corpus; importer round trip on the Synapse 1.161 fixture with differential reads clean and tokens surviving; `synapse-admin` and Draupnir smoke tests green through the compat surface.

**References.** `refs/synapse/docs/usage/configuration/config_documentation.md`, `refs/synapse/synapse/config/`, `refs/synapse/synapse/storage/schema/`, `refs/synapse/docs/admin_api/`, `refs/synapse/synapse/rest/admin/` (route and JSON shapes), `refs/synapse/docker/` (structure and behavior only, AGPL); `docs/synapse-inventory.md`; `tools/synapse_inventory.py`.

**Open questions to settle first.** Which Synapse-specific admin semantics to emulate versus answer "not applicable" (background updates, purge status); importer transaction boundaries; lazy versus eager media copy.

**Risks.** The table goes stale every Synapse release (the inventory tool and a release checklist keep it current); importer state correctness (the cross-check against Synapse's state-group mapping is mandatory).
