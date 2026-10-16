# Documentation

Start with the one that matches what you are doing.

## Running a server

| Read | For |
|---|---|
| [deployment.md](deployment.md) | Deploying on Docker, a host or Kubernetes; the reverse proxy and TLS; federation and DNS; the hardening checklist; backup and restore; monitoring; upgrades; troubleshooting |
| [install-and-upgrade.md](install-and-upgrade.md) | Every Helm command, in order: install (single node, on the internet, cluster mode), upgrade, rollback, uninstall, and the one-time steps for old installs |
| [user-guide.md](user-guide.md) | The admin interface from the first sign-in on, with screenshots: users, rooms, bridges, federation, configuration, and a section to send to the people who use the server |
| [config.md](config.md) | Every setting: type, default, whether it applies at once or at a restart, and what it corresponds to in Synapse. Generated from the schema |
| [recovery.md](recovery.md) | Getting an administrator back in when nobody can sign in |
| [scaling.md](scaling.md) | What a replica buys, what it does not, and which of that is built |
| [ops/memory.md](ops/memory.md), [ops/tracing.md](ops/tracing.md) | What to watch on `/metrics`, and traces with Jaeger or Tempo |
| [bridges/](bridges/) | mautrix bridges, heisenbridge and iMessage, as actually run against this server |
| [compat/](compat/) | Migrating from Synapse: the runbook, the config translation table, the importer's mapping, the `/_synapse/admin` routes and the CLI shims |
| [crds/](crds/) | The Kubernetes custom resources the operator reconciles |

## Understanding the project

| Read | For |
|---|---|
| [../PLAN.md](../PLAN.md) | The design: requirements, architecture, the decisions behind it, the roadmap |
| [landscape.md](landscape.md) | The other homeservers as they stand today, and where this one sits |
| [next-steps.md](next-steps.md) | Where things are right now, what was done in each session, the known gaps, and what comes next |
| [status/](status/) | One file per track with what was verified and when; `dashboard.md` is the generated measurement; `sytest/` and the Complement results are the per-test records |
| [decisions/](decisions/) | Dated, numbered decision records, from the license to how rooms are unloaded under memory pressure |
| [rfcs/](rfcs/) | Interface proposals between tracks |
| [design/](design/) | The interface's information architecture and design system, the room actor protocol, the federation threat model |
| [workstreams/](workstreams/) | The sixteen areas the work is split into, each with its brief and interfaces |
| [synapse-inventory.md](synapse-inventory.md) | The behavioural parity checklist against Synapse, generated |

## Contributing

[../CONTRIBUTING.md](../CONTRIBUTING.md) has the build and the checks; [../SECURITY.md](../SECURITY.md)
is how to report a vulnerability; [../AGENTS.md](../AGENTS.md) is the full set of working rules
for the repository.
