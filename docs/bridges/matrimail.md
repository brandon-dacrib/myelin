# matrimail: email over IMAP, one room per thread

matrimail is a Matrix <-> email bridge on mautrix-go's `bridgev2`: it signs in to a mailbox
over IMAP (IDLE, so new mail arrives at once), makes one room per email thread with the
people on the thread as ghost users, and sends what you type in a thread's room back out as
a reply over SMTP (port 587, STARTTLS), or through the Gmail API when a Gmail account was
connected with OAuth. Attachments become media; tracking pixels and spacer images are
dropped. Gmail, Outlook, iCloud, Yahoo and FastMail are known to it by domain; anything else
is guessed from the address.

**What it is, honestly:** community software. One maintainer, no stars to speak of, no
release, no published image. The upstream (Leicas/matrimail) last moved on 2026-06-03; the
fork (moiri-gamboni/matrimail) carries the fixes on a deployment branch pushed 2026-10-16,
and that is what the catalogue runs, at a pinned commit. It is in the catalogue because the
owner uses Gmail and asked (decision 0055), and it is marked as what it is in the entry's
description. What Myelin has verified is the part Myelin is responsible for: the bridge
starts from the files the server renders, registers, answers its bot's commands, and answers
the provisioning API. Nobody here has yet signed a real mailbox in through it.

The image is ours: `ghcr.io/brandon-dacrib/matrimail:24cd0f9` (the tag is the commit),
built by `.github/workflows/matrimail-image.yml` from `deploy/bridges/matrimail/Dockerfile`.
matrimail is AGPL-3.0-or-later (with portions from mautrix-whatsapp); the image carries its
LICENSE and OCI labels naming the source repository and commit, and nothing of Myelin's is
linked into it.

## Offering it

In the interface: Bridges → Offer a bridge → Email (matrimail). The options are the mautrix
ones (encryption on by default, double puppeting, backfill, who can have one). From the
admin API:

```sh
curl -X PUT "$HS/api/v1/bridge-offerings/matrimail" -H "authorization: Bearer $TOKEN" \
  -H 'content-type: application/json' -d '{"runtime": "cluster"}'
```

Each person then gets their own instance the usual way (RFC 0017): a chat with
`@emailbot_<them>:server` appears, and their mail's ghosts are `@email_<them>_*:server`.

## What the Gmail side needs

1. **2-step verification** on the Google account (Google account → Security). App passwords
   exist only with it on.
2. **An app password**: <https://myaccount.google.com/apppasswords>, any name. Google shows
   it once, sixteen letters; give the bot that, never the Google password. Revoking it in the
   same place signs the bridge out.
3. **IMAP on**: Gmail → See all settings → Forwarding and POP/IMAP → Enable IMAP. (New
   accounts have it on; accounts that were ever set to POP only may not.)

A Google Workspace account may have app passwords switched off by the administrator; the
bridge's Gmail OAuth path (`network.gmail_oauth` in its config, a Google Cloud project with
the Gmail API and a Desktop OAuth client) is the alternative, and is not rendered by Myelin:
an administrator who wants it edits the offering's instance config by hand, as the fork's
`docs/gmail-oauth-setup.md` describes.

## Signing in

In the chat with your bot (a bare command there; `!matrimail …` anywhere else):

- `login` walks through it: the address, the app password, then the folders.
- `login email:you@gmail.com password:abcdabcdabcdabcd` does it in one line.
- The bot lists the folders (for Gmail, the labels too) and asks which to watch, by number;
  `default` is the inbox and Sent. Only mail in the chosen folders becomes rooms.

Then: `list` (accounts, their state, their folders), `status`, `sync` (check now),
`reconnect`, `logout [address]`, `help`.

The admin API's `GET /api/v1/appservices/{id}/logins?user_id=…` asks the bridge's
provisioning API (`/_matrix/provision/v3/whoami`, with the secret the server wrote into its
config) and reports the mailbox logins and their state, as for any `bridgev2` bridge; the
bridge's page shows it.

## Using it

- A thread's room: typing in it sends a reply to the thread's last message from someone
  else, with `In-Reply-To` and `References` set, from the account the thread arrived in. A
  message you sent from another client (Gmail on the phone) shows up in the room from you,
  read from the Sent folder.
- A new thread: `compose to:alice@example.org subject:"Project update"` in the bot's chat
  makes a room; the first message in it is the email. Clients that can "start a chat"
  through a bridge (Element, Beeper) take an email address there too.
- Participant changes (someone added to Cc) are posted as notices.

**Limits** (the bridge's, from its README): edits and redactions are not sent as email
(email cannot be unsent; an edit shows a status icon and sends nothing); changing the
watched folders means `logout` and `login` again (`config folders` says so); reactions go
out only for Gmail accounts connected with OAuth; a file over 25 MiB is replaced by a
notice. Its Beeper-only features (archive and read sync through the Beeper Desktop API) do
nothing here.

## What the server renders

The files (`POST /bridge-offerings/matrimail/instances/{user}/files`, or the Files button on
the instance's page; the `files` map in the answer has every one by its name in `/data`):

- `config.yaml`: the `bridgev2` layout the mautrix entries get (`homeserver`, `appservice`
  with the bot `emailbot_<them>`, the template `email_<them>_{{.}}` and **port 29339**,
  `database` (SQLite in the volume), `bridge.permissions`, `backfill`, `double_puppet`,
  `encryption` in appservice mode with MSC4190 and the kept `pickle_key`,
  `provisioning.shared_secret`, logging to standard output). The bridge completes and
  rewrites it on its first start, as every mautrix bridge does. Port 29339 is the
  catalogue's: matrimail's own default, 29319, is mautrix-meta's, and the catalogue gives
  every type a port of its own; the bridge listens where its config says.
- `registration.yaml`: the bot and the ghosts, exclusive, with `de.sorunome.msc2409.push_ephemeral`,
  `org.matrix.msc3202` and `io.element.msc4190` (the last two when encryption is on), the
  provisioning secret for `logins`.
- `passphrase`: 64 hex characters the bridge encrypts the mailbox credentials it keeps with.
  matrimail reads `MATRIMAIL_PASSPHRASE`, else `./data/passphrase` under its working
  directory; the image's working directory is `/`, so this file is it. The server mints it
  with the instance's tokens and **keeps it on the instance's row**, the way a mautrix
  bridge's `pickle_key` is kept: every render writes the same one. A different one would make
  every stored login unreadable, and the bridge (since the fork's passphrase-safety fixes)
  refuses to start rather than guess. Never replace it.

The `Bridge` the manager writes is the usual one (`bridgeType: matrimail`, the image's
repository and the `24cd0f9` tag, `port: 29339`, the files Secret, a 5 GiB volume that only
grows). The image starts as root, `chown`s `/data` to UID 1337 and runs the bridge as that
user on `/data/config.yaml`, so the operator's init container can write the files and the
bridge can write its database, whatever the storage class made the volume's root.

### By hand

The compose text on the instance's page runs the same image with `./<id>` mounted at
`/data`; save every file from the `files` map there first. Without the `passphrase` file
the bridge writes one into `/data` on its first start (or set `MATRIMAIL_PASSPHRASE`); keep
it with the volume. `docker run --rm ghcr.io/brandon-dacrib/matrimail:24cd0f9 --help` and
`--version` work as they would on the binary.

## The image

`deploy/bridges/matrimail/Dockerfile`, not the project's own, because the operator's init
container runs a `sh` script in the bridge's image (the project's final stage is distroless,
with no shell) and because the project's image runs as a fixed non-root user that cannot
write a volume the cluster mounted as root. Ours: `golang:1.25-bookworm` builds the one
pinned commit (fetched by hash) with `-tags goolm` (mautrix-go's pure-Go Olm; no libolm is
compiled or shipped, and never `nocrypto`) and cgo for SQLite; `debian:bookworm-slim` runs it
with `ca-certificates`, `tzdata` and `docker-run.sh`. Both bases come through
`mirror.gcr.io` by default. Built on the desktop on 2026-10-20 (`docker build -f
deploy/bridges/matrimail/Dockerfile -t ghcr.io/brandon-dacrib/matrimail:24cd0f9
deploy/bridges/matrimail`, 131 MB, arm64): `--version` says
`matrimail 0.1.0+dev.24cd0f98 (built … with go1.25.14)`, `--generate-example-config` writes
the `bridgev2` example, and a run without `/data/config.yaml` exits 1 saying so.

The workflow builds linux/amd64 and linux/arm64 on native runners, pushes each by digest,
and tags the manifest list `24cd0f9` and `latest`; it runs on a push to `main` that touches
the Dockerfile or the workflow, weekly, and on dispatch, and refuses to build when the
Dockerfile's `MATRIMAIL_COMMIT` and the catalogue's tag differ. **Bumping the pin**: a new
commit in the Dockerfile (both `ARG` lines), `MATRIMAIL_COMMIT` and `MATRIMAIL_IMAGE` in
`crates/hs-admin/src/bridge_types.rs`, the mock catalogue, `real_matrimail.rs`'s `IMAGE` and
this page; a local build; the real-bridge test; a merge. `deploy/bridges/check-image-tags.py`
lists the entry as "(no upstream mapping)", since there is no upstream release to compare a
commit with.

## Verified (2026-10-20, the desktop, the locally built image)

`crates/hs-bridge-conformance/tests/real_matrimail.rs`, against the real `hs` binary: the
offering made, alice's instance registered, its three files rendered and written into the
container's `/data`, the image started from them and the instance went `ready`; her chat
with `@emailbot_alice:test.local` appeared, started as her; the bridge's rewrite of the
config kept port 29339, her bot and template, MSC4190 and the kept pickle key, and the
passphrase file was the server's (the bridge generated none); `help` in the chat, encrypted
(her client excluding insecure devices, the bot's device cross-signed by the manager, nothing
withheld) and plain, was answered with the bridge's command list; and `GET
/appservices/{id}/logins` reached the bridge's `whoami` with the rendered secret and reported
no mailbox signed in. Signing a mailbox in was not run: it needs a mailbox.
