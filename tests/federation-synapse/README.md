# Myelin <-> Synapse interop

`run.sh` stands up a real Synapse (`ghcr.io/element-hq/synapse:latest`) in Docker next to an `hs`
built from this tree, federates the two over TLS with a private CA it generates, and drives the
basic federation story through both servers' client APIs, recording one PASS/FAIL line per check
in `<workdir>/results.tsv`:

1. keys: each server's `/_matrix/key/v2/server`, and each notary fetching the other's keys;
2. a Myelin user joins a public Synapse room: messages both ways, pre-join history backfilled,
   member lists agree;
3. the same with a Synapse user joining a Myelin room;
4. invites both ways (accepted), leave and rejoin, kick, ban, a redaction, typing, a read receipt,
   `/keys/query` of the Myelin user from Synapse, a to-device message, media each way,
   `/publicRooms?server=`, profile and alias queries each way;
5. rooms of version 10, 11 and 12 joined in both directions, and a restricted join with Synapse as
   the authorising server.

Topology: Synapse's server name is `127.0.0.1:8448` (its TLS federation listener is published
there; its client API on `127.0.0.1:8408`); Myelin's is `fed-synapse-myelin:8449`, an nginx
container on the `fed-synapse` Docker network that terminates TLS and proxies to the host's
plaintext `hs` on `0.0.0.0:8449` (`hs serve` does not terminate TLS; `listeners[].tls` is only
warned about). Names carry ports, so both sides connect directly: no `.well-known`, no SRV.
Everything Docker-side is named `fed-synapse*` and removed on exit (`KEEP=1` keeps it).

```bash
tests/federation-synapse/run.sh                     # builds hs, runs, cleans up; exit 1 on a FAIL
HS_BINARY=target/debug/hs tests/federation-synapse/run.sh /tmp/fed   # reuse a binary and workdir
```

Without Docker (or `curl`, `jq`, `openssl`) it prints `SKIP` and exits 0, so it is safe in CI
legs that have no daemon. Logs: `<workdir>/myelin/hs.log` (`RUST_LOG=info,hs_federation=debug`)
and `docker logs fed-synapse-synapse` (Synapse's federation loggers at DEBUG).

Known caveat on a macOS agent session: `docker pull` from any registry fails with "keychain
cannot be accessed" because `~/.docker/config.json` names the `osxkeychain` credential store;
pull the image from a terminal that can open the keychain first, then run the script.
