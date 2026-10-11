# iMessage: a bridge that runs on the person's Mac (offered 2026-10-11, not yet run for real)

mautrix-imessage is not a container. It is a program for a Mac that reads the Messages app's
own database, so it runs where Messages is signed in: on the person's Mac, outside the cluster.
Since branch `agent/imessage-offering` the catalogue knows it as such (`runs_on: mac`), an
administrator can offer it, and each person's instance is set up from the offering page. The
owner asked for exactly this shape: a download link for the program, a field for the Mac's
hostname or IP address, and the steps to run it.

## How it reaches the server, and the server it

mautrix-imessage's own documentation (`docs.mau.fi/bridges/go/imessage/mac/setup.html`) has the
bridge connect *out* over a websocket to mautrix-wsproxy, which the homeserver pushes to. Myelin
does not run a wsproxy. Instead the bridge listens on the Mac the way every other mautrix
bridge listens (`appservice.hostname: 0.0.0.0`, `port: 29331`; its framework starts the HTTP
listener when `homeserver.websocket_proxy` is empty, `bridge/bridge.go` lines 622-634 of its
mautrix-go), and the server pushes to the Mac's address. So **the server has to reach the Mac
on that port**: the same network, or a Tailscale-style private network. A Mac behind NAT with
no such path cannot be reached yet; a wsproxy-shaped websocket on the server's side is the
follow-up for that, not done.

The config switches the proxy off explicitly (`websocket_proxy: null`): the bridge's config
upgrader fills in every key the file lacks from its example, whose default is a proxy URL, and
with one named the bridge connects out instead of listening.

## What the server renders

The older framework's layout (the same family as mautrix-discord; status 11, 2026-10-11, has
the table) with iMessage's own sections, read from its `example-config.yaml` at commit
`300ba6d0`:

- `appservice.database`: SQLite beside the config (`file:<appservice id>.db`), since the Mac
  has no `/data`;
- `imessage.platform: mac`;
- `bridge.user`: the one person the bridge is for (no `permissions`);
- `bridge.login_shared_secret: as_token:<token>` for double puppeting (one value, not a map);
- `bridge.backfill.enable`;
- `bridge.encryption` with `allow`, `default`, `appservice`, and **no `msc4190`**: its
  framework predates MSC4190, so the registration asks for MSC2409 and MSC3202 only and the
  bot's device comes from `m.login.application_service`, which this server offers every
  appservice;
- no `provisioning` (`provisioning_api: none`) and no `pickle_key` (a constant in its binary).

And `run.sh`, to keep beside `config.yaml` and `registration.yaml` on the Mac: it downloads the
pinned build if the program is not there, clears macOS's quarantine flag (a downloaded program
is refused until it is), checks the two files are there, and starts the bridge in the
foreground with `-c config.yaml -r registration.yaml`, so that `operation not permitted` (no
Full Disk Access yet) is seen where it happens.

## The pinned build

mautrix-imessage has no release tags; its CI (`mau.dev/mautrix/imessage`) builds a universal
macOS binary per commit. The catalogue pins **pipeline 20935, job 95972 ("build universal"),
commit `300ba6d0`, 2026-05-14**: `https://mau.dev/mautrix/imessage/-/jobs/95972/artifacts/download`,
a zip of `mautrix-imessage`, `libolm.3.dylib` and `example-config.yaml`. A job's artifact link
is a permalink; the `master` link moves (decision 0037). The offering's "image tag" is the
commit, so the two name one build. Bumping it is a release step: find the newest successful
pipeline on `master` (`GET https://mau.dev/api/v4/projects/133/pipelines?ref=master&status=success`),
its `build universal` job id, and update `MacDownload` in `bridge_types.rs` and this file.

## The address

`PUT /api/v1/bridge-offerings/mautrix-imessage/instances/{user}` takes `{"address": ...}`:
`host`, `host:port` or an `http(s)://` URL (a bare host takes 29331). The manager keeps it as
the instance's `url`, patches the registry's registration at once (the registration
fingerprint leaves `url` out, so the step that patches changed claims would not carry it), and
the files render with it. Set before the instance is named, it survives naming. Refused for an
instance the server deploys. `BridgeInstance.url` says where the server reaches the bridge;
until the address is set it is the placeholder `http://<appservice id>:29331`.

## What was verified, and what was not

- `crates/hs-cli/tests/bridge_offerings.rs::an_imessage_offering_runs_on_a_mac_whose_address_the_administrator_sets`,
  against the real `hs` binary: the catalogue entry, an offering that can only run elsewhere,
  an instance with its address set in the same request, the registration's `url`, the files
  (layout, `websocket_proxy: null`, `bridge.user`, no MSC4190 in the registration, `run.sh`),
  a refused address, and the instance reaching **ready** once the address points at a stand-in
  that answers pings.
- Unit tests in `bridge_types.rs` (the config and the script, key by key), `bridge_offerings.rs`
  (the address grammar) and `manager.rs` (address before and after naming; refused for a
  deployed instance).
- **Not run on a real Mac yet.** The owner's Mac is the first: download, `./run.sh`, Full Disk
  Access, and a message both ways. Until then the config is written from the bridge's example
  and source, not from a bridge that started. Things most likely to need a look then: whether
  the bridge's config upgrader keeps `websocket_proxy: null` (it copies the key when present),
  whether `bridge.login_shared_secret` with an `as_token:` secret works on its framework (it
  does on discord's, `bridge/doublepuppet.go`), and the bot's appservice login for encryption.
