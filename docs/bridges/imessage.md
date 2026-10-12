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
- no `bridge.login_shared_secret`, so the bridge does not double puppet (since 2026-10-12,
  decision 0046). This build knows only the literal `appservice` there, an appservice login with
  the bridge's own token, which needs the bridge's own registration to claim its person; that
  claim made the server send the bridge every room the person is in, and stalled five of the
  demo's bridges. It takes anything else, `as_token:<token>` included, as an HMAC secret. The
  manager still starts the person's chat with the bot as them, with the instance's
  double-puppeting registration (`<instance>-doublepuppet`, no url);
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
- **Run on the owner's Mac, 2026-10-12** (the pinned build `300ba6d0`, macOS 26.6). Three
  findings, all fixed:
  1. `run.sh` downloaded the build only when the program was missing. With the program there
     and `libolm.3.dylib` not, the bridge aborted at launch (`Library not loaded:
     @rpath/libolm.3.dylib`, five crash reports). It now downloads when either is missing,
     checked by running the script with the program present and the library absent.
  2. Encryption: this framework logs its bot in with `m.login.application_service` and first
     checks that `GET /login` lists it. The server did not, so the bridge stopped with
     `homeserver does not support appservice login`. The server now lists it, as Synapse
     does. Checked with the real build against a local `hs` with the fix: `Logged in as bridge
     bot`, its keys uploaded, `Bridge started!`.
  3. Double puppeting: this build's `login_shared_secret` knows the literal `appservice`, not
     `as_token:<token>`, which it took as an HMAC secret for a password login (`403 Invalid
     username or password`). The config said `appservice`, checked the same way:
     `Successfully automatically enabled custom puppet`. **Taken out the same night** (decision
     0046): `appservice` needs the bridge's own registration to claim the person, which floods
     the bridge with every room they are in. The config now has no `login_shared_secret`, so
     what the person sends from their own Apple devices is not shown as them. It can come back
     when the bridge takes an `as_token:` secret (mautrix-go's newer frameworks do).

  Still to see on the demo: a message both ways, which needs Full Disk Access for the
  terminal that runs `run.sh` and the server with fix 2. Whether the config upgrader keeps
  `websocket_proxy: null` is also unconfirmed.
