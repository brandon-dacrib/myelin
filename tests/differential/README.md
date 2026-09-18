# Differential testing against Synapse

Layer L5 of `PLAN.md` section 12: run the same scripted client workload against Synapse and
against this project's server, normalize away nondeterministic fields, and diff. A clean diff is
the working definition of behavioral compatibility for whatever the workload covers.

Docker is off in this development environment, so `run_differential.py` detects that
(`docker info` failing) and exits 0 with a explanation rather than failing the build — see that
file's docstring. This document is how to actually record a baseline and run a comparison once
Docker (or any reachable Synapse 1.161 instance) is available.

## Layout

| Path | What |
|---|---|
| `lib/proxy.py` | The recording proxy: forwards every request to `--target`, appends one JSON line per request/response pair to `--out`. |
| `lib/driver.py` | Runs a scripted workload (`workloads/*.json`) against one base URL, substituting `{{var}}` placeholders extracted from earlier steps' responses. |
| `lib/normalize.py` | Rewrites nondeterministic fields (timestamps, event IDs, room IDs, tokens, unordered scalar arrays) to stable placeholders so two independently-generated but equivalent responses compare equal. |
| `lib/diff.py` | Diffs two normalized, per-step response lists and reports matches/mismatches. |
| `record.py` | Drives one workload through an already-running `lib/proxy.py`, producing a baseline recording. |
| `replay.py` | Runs one workload directly against a candidate server and diffs its normalized responses against a recorded baseline. |
| `run_differential.py` | CI/local entry point: checks Docker, then runs every workload with a baseline against `--candidate-url`. |
| `workloads/*.json` | The scripted workloads: `registration`, `room_creation`, `messaging`, `membership`, `sync`. |
| `baselines/` | Where recorded baselines live (`<workload>.<label>.jsonl`), gitignored — see below. |
| `tests/` | Unit and pipeline tests for this harness itself (`python3 -m unittest discover -s tests/differential/tests`), independent of Docker/Synapse. |

## Recording a Synapse 1.161 baseline

Requires Docker.

1. Start Synapse 1.161 with a throwaway config (registration open, no rate limiting, so the
   workloads run unimpeded):

   ```bash
   mkdir -p /tmp/synapse-differential/data
   docker run --rm -it \
     -v /tmp/synapse-differential/data:/data \
     -e SYNAPSE_SERVER_NAME=differential.example.org \
     -e SYNAPSE_REPORT_STATS=no \
     matrixdotorg/synapse:v1.161.0 generate

   # Then edit /tmp/synapse-differential/data/homeserver.yaml:
   #   enable_registration: true
   #   enable_registration_without_verification: true
   #   registration_shared_secret:  # leave unset; open registration is fine for a throwaway instance
   #   rc_registration: {per_second: 1000, burst_count: 1000}
   #   rc_login: {address: {per_second: 1000, burst_count: 1000}}

   docker run --rm -d --name synapse-differential \
     -p 8008:8008 \
     -v /tmp/synapse-differential/data:/data \
     matrixdotorg/synapse:v1.161.0
   ```

2. Start the recording proxy in front of it, in a separate terminal:

   ```bash
   python3 tests/differential/lib/proxy.py \
     --listen-port 19080 \
     --target http://localhost:8008 \
     --out tests/differential/baselines/registration.synapse-1.161.jsonl
   ```

3. Drive each workload through the proxy (repeat per workload; use a fresh Synapse instance, or
   at least fresh usernames, between recordings — the workloads use fixed literal usernames like
   `differential_reg_user`, so re-running against the same already-populated Synapse will hit
   `M_USER_IN_USE` on the second attempt):

   ```bash
   python3 tests/differential/record.py --workload registration --through http://127.0.0.1:19080
   ```

   Repeat for `room_creation`, `messaging`, `membership`, `sync`, restarting the proxy with a new
   `--out` file (`tests/differential/baselines/<workload>.synapse-1.161.jsonl`) for each — or run
   several proxies on different ports in parallel against independently-provisioned Synapse
   instances if recording all five at once.

4. Stop Synapse and the proxy. `tests/differential/baselines/` now has one `.jsonl` file per
   workload. These files are the "baseline recording" `run_differential.py` and `replay.py` diff
   candidates against; they are gitignored (see `.gitignore`) since they are large, environment-
   specific, and reproducible from this procedure — commit them to a separate artifact store or
   attach them to CI runs if longer-term storage is wanted.

## Comparing a candidate server against a recorded baseline

Once this workspace has an assembled server binary (`hs-server`, not built as of this writing —
`hs-http` and the per-track routers like `hs-auth::routes::router()` are library crates without a
listener wired up yet) serving on some port:

```bash
python3 tests/differential/run_differential.py --candidate-url http://localhost:8009
```

or, for one workload at a time with full diff output:

```bash
python3 tests/differential/replay.py \
  --workload registration \
  --baseline tests/differential/baselines/registration.synapse-1.161.jsonl \
  --candidate-url http://localhost:8009 \
  --verbose
```

Exit code is 0 if every step's normalized response matched the baseline, 1 otherwise.

## Adding a workload

A workload is a JSON file under `workloads/` (see `lib/driver.py`'s docstring for the step
schema): a `name`, and an ordered list of `steps`, each an HTTP method/path/body plus optional
`auth_token` (referencing an earlier step's `extract`ed value) and `extract` (JSONPath-lite
pointers pulling values like `access_token`/`room_id`/`event_id` out of the response for later
steps to reference as `{{var}}`). Keep usernames and other identifiers unique per workload file so
two workloads can run against the same server without colliding.

## Normalization rules (what gets blanked or reordered)

See `lib/normalize.py` for the authoritative implementation; in short:

- Any JSON object key ending in `ts`/`timestamp(s)` (case-insensitive): blanked to `<TS>`.
- `access_token`, `refresh_token`, `next_batch`, `prev_batch`, `since`, `txn_id`, `nonce`,
  `session`, `registration_session_id`: blanked to `<TOKEN>`.
- A string value matching `$...` (an event ID) or `!...:...` (a room ID): replaced with a stable
  `<EVENT_n>` / `<ROOM_n>` placeholder, numbered by first appearance *within one normalized run* —
  so the same underlying ID reused later in the same response set (e.g. one event's `replaces`
  pointing at another event from the same run) still normalizes to matching placeholders, which a
  single global blank-out would lose.
- A JSON array whose every element is a bare scalar (not an object) is sorted before comparing,
  treating it as an unordered set (capability flag lists, login flow type lists, ...). Arrays of
  *objects* (event timelines, room lists) are left in their original order, since those are
  normally meaningfully ordered and a bug that reorders them should be caught, not hidden.

A response field this ruleset does not cover (a new kind of ID, a field this project adds that
Synapse doesn't have) shows up as an unexplained diff — extend `lib/normalize.py`, not the
individual workload, when that happens.
