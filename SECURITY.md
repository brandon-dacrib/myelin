# Security policy

## Reporting a vulnerability

Please do not open a public issue for a security problem. Report it privately through GitHub's
vulnerability reporting for this repository:

<https://github.com/brandon-dacrib/myelin/security/advisories/new>

Include the version (`hs version`, or the image tag), the layout (single node or cluster, which
storage backend), what you observed, and how to reproduce it. A proof of concept against your own
server is welcome; please do not test against servers you do not operate.

You will get an acknowledgement within a few days. Confirmed problems are fixed on `main`, which
releases on every merge, and the advisory is published with the release that carries the fix
and credit to the reporter unless they prefer otherwise.

## Supported versions

Every merge to `main` is a release, and only the newest release of the newest line is
supported: a fix ships as the next release, not as a patch to an older one. Upgrade with
[docs/deployment.md](docs/deployment.md#upgrading-and-rolling-back).

## What is in scope

- The `hs` server and its CLI (`crates/`), including the client-server, federation, media,
  appservice and admin APIs it serves.
- The management web interface (`web/`).
- The published image, Helm chart and Kubernetes operator (`deploy/`).

Bridges and other appservices are separate projects; a problem in one of them goes to its own
maintainers, unless the problem is in how this server talks to it.

## Design notes an auditor should know

- The server is administered through the admin API (`/api/v1`) and the interface at `/admin/`,
  both behind an administrator's account or an API token that carries its scopes; every change
  is in the audit log. There is no configuration file to edit on a running server.
- The first-run setup link and the `hs recover` link are one-time tokens signed with the
  server's signing key; whoever can read the server's log, or run a command where the key is,
  can become an administrator. That is by design and documented.
- `/metrics` is unauthenticated and off by default. `/health/*` is unauthenticated.
- Registration is closed, rate limits are on, link previews are off and outbound federation
  verifies certificates by default. [docs/deployment.md](docs/deployment.md#hardening-checklist)
  lists what to check before going public.
- The published image is distroless, runs as a non-root user, and the chart sets a read-only
  root filesystem and drops all capabilities.
- `docs/design/06-federation-threat-model.md` is the federation threat model.
