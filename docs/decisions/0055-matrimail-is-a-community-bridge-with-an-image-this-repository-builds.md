# 0055. matrimail is a community bridge, offered on an image this repository builds

Date: 2026-10-20. Status: accepted. Track 11 (appservices and bridges); the catalogue
(`crates/hs-admin/src/bridge_types.rs`), the manager (`crates/hs-bridges`), the image
(`deploy/bridges/matrimail/`, `.github/workflows/matrimail-image.yml`).

## Context

The owner uses Gmail and asked for email in the catalogue. matrimail is a Matrix <-> email
bridge on mautrix-go's `bridgev2` (`mxmain.BridgeMain`, mautrix v0.24.2): a mailbox over IMAP
IDLE with a password (Gmail: an app password, 2-step verification required), one room per
email thread, replies out over SMTP 587 STARTTLS or the Gmail API with OAuth, `!matrimail
login email:… password:…`, `!matrimail config folders`, SQLite by default, Postgres optional,
and a passphrase (`MATRIMAIL_PASSPHRASE`) the mailbox credentials it keeps are encrypted with.

It is community software: one maintainer, no stars to speak of, **no release and no container
image**. The upstream, Leicas/matrimail, last moved on 2026-06-03 (1.9.3). The fork,
moiri-gamboni/matrimail, has `main` equal to upstream's and a deployment branch,
`deploy/roost`, with 82 commits over it (pushed 2026-10-16): the README the owner read is that
branch's. Its `go.mod` pins mautrix v0.24.2, whose example config is the `bridgev2` layout the
catalogue already renders (`database`, `encryption.msc4190`, `provisioning.shared_secret`,
`double_puppet`), and nothing in its connector overrides any of it; its `GetName` says
`NetworkID: email`, `DefaultPort: 29319`, `DefaultCommandPrefix: !matrimail`.

Every other catalogue entry runs an image its project publishes, pinned to a release
(decision 0037). Nobody publishes one for this.

## Decision

1. **The fork at a pinned commit.** The catalogue entry `matrimail` is built from
   moiri-gamboni/matrimail at `24cd0f981f803897f7b71693c1c84738e78cf9d7` (`deploy/roost`,
   2026-10-16), not upstream: the fork's branch holds the fixes (passphrase safety: the bridge
   refuses to start with a passphrase that cannot read its stored credentials instead of
   overwriting them; reply addressing; MIME parser bounds; media encrypted for the room;
   Gmail threading and retries) and the README that describes the bridge as it is now, and
   its `Makefile` builds with mautrix-go's pure-Go Olm (`goolm`), so no libolm is compiled or
   shipped. Never `nocrypto`: the rooms are encrypted. A deployment branch can be rewritten,
   so the pin is the commit, fetched by hash; if the commit ever stops being served, the build
   fails and the pin is moved on purpose.
2. **This repository builds and publishes the image**: `ghcr.io/brandon-dacrib/matrimail`,
   tagged with the commit's first seven characters (`24cd0f9`, what the catalogue pins) and
   `latest`, for linux/amd64 and linux/arm64 on native runners, by
   `.github/workflows/matrimail-image.yml` (on a push to `main` that touches the Dockerfile
   or the workflow, weekly, and on dispatch). `deploy/bridges/matrimail/Dockerfile` is ours,
   not the project's: theirs ends in a distroless image with no shell, and the operator's init
   container runs a `sh` script in the bridge's image to write `/data`'s files; and theirs
   runs as a fixed non-root user that cannot write a volume the cluster mounted as root. Ours
   ends in `debian:bookworm-slim` and starts the way mautrix's images do (`docker-run.sh`: as
   root, `chown /data` to UID 1337, then the bridge as that user on `/data/config.yaml`). The
   image carries matrimail's AGPL-3.0 LICENSE and OCI labels naming its source and revision;
   nothing of Myelin's is linked into it. A test in the catalogue keeps the Dockerfile's
   `MATRIMAIL_COMMIT` and the entry's tag equal, and the workflow refuses to build when the
   catalogue does not pin what the Dockerfile says.
3. **The catalogue entry is rendered like a mautrix bridge** (`Runtime::Mautrix`,
   `ConfigLayout::BridgeV2`, `per_user`, `deployable`, provisioning `mautrix_v3` so `logins`
   works): the bot `emailbot` and ghosts `email_*` (what its own `--generate-registration`
   claims), port **29339** in the rendered config (its connector's default is mautrix-meta's,
   and the catalogue gives every type a port of its own; the bridge listens where its config
   says), command prefix `!matrimail`, encryption and double puppeting as the other mautrix
   entries. Its description and sign-in notes say what it is: community, one maintainer, no
   release, unproven here beyond starting and answering its bot.
4. **The credentials passphrase is minted and kept like `pickle_key`**, and delivered as a
   file, not an environment variable: the `Bridge` resource has no `env`, and matrimail reads
   `./data/passphrase` under its working directory when `MATRIMAIL_PASSPHRASE` is unset, so
   the image's working directory is `/` and the manager writes `/data/passphrase` with the
   bridge's other files (`InstanceRow::credentials_passphrase`, 64 hex characters, minted with
   the tokens; added to the render's files by the manager, so the files Secret, the files
   answer and the compose text all carry it). A re-render writes the same one; a different one
   would make every stored login unreadable, and the bridge now refuses to start rather than
   guess. A bridge run by hand without the file writes one into `/data` on its first start.

## Consequences

- The first push of the image is the first run of the workflow after this merges (the push
  of its own path triggers it; a dispatch does the same). Until it has run, an offering of
  the type cannot pull its image, and `real_matrimail.rs` skips on CI saying so. On the
  desktop the image is built locally from the Dockerfile and the test runs against it.
- `deploy/bridges/check-image-tags.py` lists the entry as "(no upstream mapping)": there is no
  upstream release to compare a commit pin with. Bumping is by hand: a new commit in the
  Dockerfile and the catalogue (and the real test's `IMAGE`), the real-bridge test, a merge.
- Signing in needs a real mailbox and is not run by any test here; the catalogue's steps and
  `docs/bridges/matrimail.md` carry what Gmail needs. What is verified is the bridge starting
  from the rendered files, registering, answering `help`, and answering the provisioning
  API's `whoami`.
- The passphrase is in the files Secret beside the config that already holds the bridge's
  tokens and pickle key: the same place, the same readers (`bridges:write`).
