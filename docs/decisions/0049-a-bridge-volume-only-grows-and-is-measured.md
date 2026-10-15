# 0049: A bridge's volume only grows, starts at 5 GiB, and is measured

Date: 2026-10-15. Track 12 (`hs-operator`, the chart), track 11 (`hs-bridges`), track 15
(`hs-admin`), track 16 (`web/`).

## Context

On 2026-10-14 the demo's Slack bridge (`Bridge` `bridge-slack-brandon`, a 1 GiB Longhorn claim)
filled its volume and stopped: its init container failed `cp: write error: No space left on
device` and the pod sat in `Init:CrashLoopBackOff`. Nothing said so for over a day. The kubelet's
`kubelet_volume_stats_used_bytes / capacity_bytes` read 0.09 on 10-12, 0.48 on 10-13 and 0.98
from 10-14 08:00; the Registrations page said "degraded"; the `Bridge`'s status said
"files: CrashLoopBackOff" (the init container kept no termination message).

What filled it: mautrix's default `logging` writes `./logs/bridge.log` in `/data` as well as
standard output, rotated at 100 MB with ten backups, up to 1.1 GB. The Slack bridge logs at
debug about 600 MB a day (Loki, 10-13), so it reached 1 GiB in under three days. Every other
mautrix bridge on the demo logs about 60 MB a day and would have filled its 1 GiB in a few weeks.

Raising the size by hand did not hold either: the bridge manager wrote `storage_size: "1Gi"` into
every `Bridge` it applied, and the operator's server-side apply of the claim then asked the API
server to shrink it, which it refuses, so every reconcile of that bridge would have failed.

## Decision

1. **A bridge's volume never shrinks.** The operator applies the claim at the larger of the
   `Bridge`'s `spec.storage.size` and what the claim requests or has
   (`hs_operator::bridge::claim_size`); a smaller spec is a `VolumeSize` condition
   (`ShrinkRefused`) and one `WARN`, never a failed reconcile. The homeserver's bridge client
   writes a `Bridge` with the larger of its size and the one already in the cluster
   (`hs_operator::deploy::keep_larger_storage`), and the admin API refuses a size below what the
   volume has (`400` at `/volume_size`). A larger spec the StorageClass will not grow is a
   `GrowthRefused` condition and one `WARN`; the rest of the bridge is reconciled.
2. **The default is 5 GiB** (`DEFAULT_STORAGE_SIZE`, `hs_admin::bridge_volume::DEFAULT_SIZE`),
   for the CRD's default and for every instance the server deploys; an administrator sets
   another per instance (`PUT /bridge-offerings/{type}/instances/{user_id}` with `volume_size`,
   the bridge's page). Existing instances move from 1 GiB to 5 GiB on the first roll (the
   deployment's fingerprint carries the size), which restarts each bridge once.
3. **A mautrix bridge logs to standard output only.** The rendered config has a `logging` section
   with one `stdout` writer (both mautrix-go layouts); the cluster's log collector keeps the logs.
   Checked against the real bridge: `real_mautrix_login.rs`,
   `a_bridge_rolled_with_a_render_without_its_pickle_key_keeps_its_own`.
4. **The operator measures every bridge's volume** about once a minute from the kubelet's
   `/stats/summary`, records it in the `Bridge`'s `status.volume`, sets
   `hs_operator_bridge_volume_used_ratio`, and logs a `WARN` at 85% (nearly full) and 95% (full).
   The server reads it through the `Bridge` (`hs_bridge_volume_used_ratio`, the `BridgeVolume`
   on the instance and on its registration, the `HsBridgeVolume*` alerts), and a bridge whose
   last words say "No space left on device" or "database or disk is full" is a full volume
   whatever was last measured.
5. **The kubelet is read directly, not through the API server's node proxy.** The chart's
   ClusterRole grants `get` on `nodes` and `nodes/stats`, as metrics-server needs; `nodes/proxy`
   (`--volume-stats apiserver`) also reaches every other kubelet endpoint (exec among them) and
   is opt-in. The kubelet's certificate is not verified by default
   (`bridges.operator.volumeStats.kubeletInsecureTLS: true`, metrics-server's
   `--kubelet-insecure-tls`): kubeadm, kind and Talos kubelets serve self-signed ones.
6. **The init container keeps its last words** (`terminationMessagePolicy:
   FallbackToLogsOnError`), so its failure reaches the `Bridge`'s status like the bridge's own.

## Consequences

- One restart of every deployed bridge on the release that carries this (the init container,
  the size and the rendered logging section all change the pod template).
- A bridge's old log files stay on its volume until removed by hand (`docs/bridges/mautrix.md`,
  "2026-10-15: a full volume").
- The operator needs a ClusterRole for the first time; `bridges.operator.volumeStats.source: off`
  goes without it. `deploy/operator/` grants the same.
- A `Bridge` written with an older CRD in the cluster loses `status.volume` to pruning; the chart
  applies the CRD on every upgrade (decision 0036).
