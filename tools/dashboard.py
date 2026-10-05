#!/usr/bin/env python3
"""Generate the parity dashboard: docs/status/dashboard.md.

Owned by track 14 (`docs/workstreams/14-test-and-conformance.md`). `PLAN.md` section 12 says
coverage is reported as "a single parity dashboard: spec coverage percentage, Complement and
Sytest pass counts, Synapse route checklist completion, bridge conformance, and the performance
table," and `docs/workstreams/README.md` rule 5 says "the parity dashboard (owned by 14) is the
only status report the project publishes." This script assembles it from committed inputs:

1. Every `docs/status/*.md` track status file (its title, the date of its newest dated section,
   and Done/In progress/Blockers bullet counts where a file still has those sections).
2. Spec coverage: `hs-spec-coverage` run (through cargo) against `refs/matrix-spec/data/api` and
   the committed `docs/status/routes.json` manifest (`docs/rfcs/0005-routes-json-manifest.md`;
   `--refresh-routes` rewrites it first with `hs routes-manifest`). Each successful run is
   saved to `docs/status/spec-coverage.json`, and that committed snapshot is what the
   dashboard reports when cargo or the spec checkout is absent, or with `--coverage-from`.
3. Sytest: every committed run under `docs/status/sytest/` (`<run>-results.txt`, one
   `PASS|FAIL|SKIP <name>` line per test, with `<run>-summary.txt` and
   `<run>-are-we-synapse-yet.txt` beside it), newest first.
4. Complement: `docs/status/complement-<suite>-results.txt`, the top-level baselines
   `tools/complement_triage.py --write-baseline` writes (one `PASS|FAIL|SKIP <Test>` line per
   top-level test, and a `# Run N, <date>, <commit> (...): A of B assertions` header).
5. The `PLAN.md` section 12 test-layer table (L0-L12), whose status per layer is *detected* from
   what exists on disk and from the results above (not hand-maintained), so this script keeps
   telling the truth as layers land.

Nothing here needs cargo, Docker or the network unless asked: with no cargo on the host the
spec-coverage numbers come from the committed snapshot and say so.

Usage:
    python3 tools/dashboard.py                    # writes docs/status/dashboard.md
    python3 tools/dashboard.py --print            # prints to stdout instead of writing
    python3 tools/dashboard.py --skip-coverage    # omit the spec-coverage section
    python3 tools/dashboard.py --coverage-from docs/status/spec-coverage.json
                                                  # report a saved summary, run no cargo
    python3 tools/dashboard.py --refresh-routes   # rewrite docs/status/routes.json first
"""

from __future__ import annotations

import argparse
import datetime
import json
import re
import shutil
import subprocess
import sys
import tempfile
from dataclasses import dataclass, field
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
STATUS_DIR = ROOT / "docs" / "status"
DASHBOARD_PATH = STATUS_DIR / "dashboard.md"
SYTEST_DIR = STATUS_DIR / "sytest"
ROUTES_PATH = STATUS_DIR / "routes.json"
COVERAGE_SNAPSHOT = STATUS_DIR / "spec-coverage.json"
SPEC_SUBDIR = Path("refs") / "matrix-spec" / "data" / "api"

DATE_RE = re.compile(r"\b(20\d\d-\d\d-\d\d)\b")
# A run's name starts with its date and may run straight on (`2026-10-01b`).
RUN_DATE_RE = re.compile(r"^(20\d\d-\d\d-\d\d)")


@dataclass
class TrackStatus:
    number: str
    title: str
    path: Path
    last_updated: str | None
    latest_section: str | None


def _short(text: str, limit: int = 110) -> str:
    text = re.sub(r"\s+", " ", text).strip()
    return text if len(text) <= limit else text[: limit - 1].rstrip() + "…"


def parse_status_file(path: Path) -> TrackStatus:
    """A track's status file: its title (the `# ` heading, or the file name when the file starts
    straight at a dated section), and its newest dated `## ` section, which is where every
    track writes what it did (the common brief: "a dated session section at the TOP")."""
    text = path.read_text(encoding="utf-8")
    lines = text.splitlines()

    number = path.stem.split("-", 1)[0]
    title = path.stem.split("-", 1)[1].replace("-", " ") if "-" in path.stem else path.stem
    title = title[:1].upper() + title[1:]
    if lines and lines[0].startswith("# "):
        heading = lines[0][2:].strip()
        # "01 Storage engine: status", "15. Admin API and modules: status", "Status: track 11,
        # appservices and bridges" -> the name alone.
        heading = re.sub(r"^status:\s*track\s*\d+,\s*", "", heading, flags=re.IGNORECASE)
        heading = re.sub(r"^\d+\.?\s*", "", heading)
        heading = re.sub(r":\s*status$", "", heading, flags=re.IGNORECASE)
        heading = heading.strip()
        if heading:
            title = heading[:1].upper() + heading[1:]

    newest: tuple[str, str] | None = None
    for line in lines:
        m = re.match(r"^##\s+(.+)$", line)
        if not m:
            continue
        dates = DATE_RE.findall(m.group(1))
        if not dates:
            continue
        date = max(dates)
        if newest is None or date > newest[0]:
            newest = (date, m.group(1))
    if newest is None:
        # No dated section: fall back to a "Last updated:" line, as the oldest files have.
        for line in lines:
            for prefix in ("Last updated:", "Updated:"):
                if line.startswith(prefix):
                    dates = DATE_RE.findall(line)
                    if dates:
                        newest = (max(dates), line[len(prefix) :])
                    break
            if newest:
                break

    return TrackStatus(
        number=number,
        title=title,
        path=path,
        last_updated=newest[0] if newest else None,
        latest_section=_short(newest[1]) if newest else None,
    )


def gather_status_summaries() -> list[TrackStatus]:
    tracks = []
    for path in sorted(STATUS_DIR.glob("*.md")):
        if path.name in ("README.md", "dashboard.md"):
            continue
        tracks.append(parse_status_file(path))
    tracks.sort(key=lambda t: (len(t.number), t.number))
    return tracks


# ---- spec coverage -------------------------------------------------------------------------


def refresh_routes_manifest() -> bool:
    """`hs routes-manifest -o docs/status/routes.json`: the router's own list of what it mounts,
    no configuration or server needed. Returns whether it worked."""
    if shutil.which("cargo") is None:
        print("dashboard.py: --refresh-routes needs cargo; keeping the committed routes.json", file=sys.stderr)
        return False
    args = ["cargo", "run", "--quiet", "-p", "hs-cli", "--bin", "hs", "--", "routes-manifest", "-o", str(ROUTES_PATH)]
    try:
        subprocess.run(args, cwd=ROOT, check=True, capture_output=True, text=True, timeout=1800)
    except (subprocess.CalledProcessError, subprocess.TimeoutExpired, FileNotFoundError) as exc:
        detail = exc.stderr[-2000:] if isinstance(exc, subprocess.CalledProcessError) else ""
        print(f"dashboard.py: hs routes-manifest failed: {exc}\n{detail}", file=sys.stderr)
        return False
    return True


def routes_manifest_date() -> str | None:
    try:
        return json.loads(ROUTES_PATH.read_text(encoding="utf-8")).get("generated_at")
    except (OSError, json.JSONDecodeError):
        return None


def find_spec_dir() -> Path | None:
    """`refs/matrix-spec/data/api` in this checkout, or in the main checkout when this is a git
    worktree (`refs/` is git-ignored, so a new worktree has none of its own)."""
    candidates = [ROOT / SPEC_SUBDIR]
    try:
        common = subprocess.run(
            ["git", "rev-parse", "--path-format=absolute", "--git-common-dir"],
            cwd=ROOT, capture_output=True, text=True, timeout=10, check=False,
        ).stdout.strip()
        if common:
            candidates.append(Path(common).parent / SPEC_SUBDIR)
    except (OSError, subprocess.TimeoutExpired):
        pass
    return next((c for c in candidates if c.is_dir()), None)


def run_spec_coverage() -> dict | None:
    """Builds (if needed) and runs `hs-spec-coverage` against the spec checkout and the committed
    routes manifest, returning its `--json-out` summary, or `None` when it cannot run here (no
    cargo, no `refs/matrix-spec`) or failed (reported on stderr; the caller then falls back to
    the committed snapshot rather than failing the whole dashboard over one input)."""
    if shutil.which("cargo") is None:
        print("dashboard.py: no cargo on this host; spec coverage comes from the committed snapshot", file=sys.stderr)
        return None
    spec_dir = find_spec_dir()
    if spec_dir is None:
        print(f"dashboard.py: {SPEC_SUBDIR} is missing (tools/fetch-refs.sh); using the committed snapshot", file=sys.stderr)
        return None
    args = ["cargo", "run", "--quiet", "-p", "hs-spec-coverage", "--", "--spec-dir", str(spec_dir), "--out", "/dev/null"]
    with tempfile.NamedTemporaryFile(suffix=".json", delete=False) as tmp:
        json_out = Path(tmp.name)
    args += ["--json-out", str(json_out)]
    if ROUTES_PATH.exists():
        args += ["--routes", str(ROUTES_PATH)]

    try:
        subprocess.run(args, cwd=ROOT, check=True, capture_output=True, text=True, timeout=600)
        summary = json.loads(json_out.read_text())
    except (subprocess.CalledProcessError, subprocess.TimeoutExpired, FileNotFoundError) as exc:
        detail = exc.stderr[-2000:] if isinstance(exc, subprocess.CalledProcessError) else ""
        print(f"dashboard.py: hs-spec-coverage run failed: {exc}\n{detail}", file=sys.stderr)
        return None
    except (OSError, json.JSONDecodeError) as exc:
        print(f"dashboard.py: could not read hs-spec-coverage output: {exc}", file=sys.stderr)
        return None
    finally:
        json_out.unlink(missing_ok=True)

    return {
        "generated_at": utc_now(),
        "routes_manifest": str(ROUTES_PATH.relative_to(ROOT)) if ROUTES_PATH.exists() else None,
        "routes_manifest_generated_at": routes_manifest_date(),
        "summary": summary,
    }


def load_coverage_snapshot(path: Path) -> dict | None:
    try:
        snapshot = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as exc:
        print(f"dashboard.py: could not read the coverage snapshot {path}: {exc}", file=sys.stderr)
        return None
    if "summary" not in snapshot:
        # A bare `hs-spec-coverage --json-out` file.
        snapshot = {"generated_at": None, "routes_manifest": None, "summary": snapshot}
    return snapshot


def utc_now() -> str:
    return datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


# ---- Sytest and Complement results -----------------------------------------------------------


@dataclass
class Counts:
    passed: int = 0
    failed: int = 0
    skipped: int = 0
    failing: list[str] = field(default_factory=list)

    @property
    def total(self) -> int:
        return self.passed + self.failed + self.skipped

    @property
    def rate(self) -> float:
        ran = self.passed + self.failed
        return 100.0 * self.passed / ran if ran else 0.0


def count_results(path: Path) -> Counts:
    """`PASS|FAIL|SKIP <name>` lines; `#` lines are comments. Anything else is ignored."""
    counts = Counts()
    for line in path.read_text(encoding="utf-8", errors="replace").splitlines():
        word, _, name = line.partition(" ")
        if word == "PASS":
            counts.passed += 1
        elif word == "FAIL":
            counts.failed += 1
            counts.failing.append(name.strip())
        elif word == "SKIP":
            counts.skipped += 1
    return counts


def git_commit_time(path: Path) -> int | None:
    """When `path` was last committed (seconds), `None` when git cannot say (no git, or a file
    not committed yet, which then sorts as the newest)."""
    try:
        out = subprocess.run(
            ["git", "log", "-1", "--format=%ct", "--", str(path)],
            cwd=ROOT, capture_output=True, text=True, timeout=10, check=False,
        ).stdout.strip()
    except (OSError, subprocess.TimeoutExpired):
        return None
    return int(out) if out.isdigit() else None


@dataclass
class SytestRun:
    name: str
    date: str
    counts: Counts
    note: str | None
    awsy: list[tuple[str, int, int]]
    order: tuple


def gather_sytest_runs() -> list[SytestRun]:
    """Every committed Sytest run, newest first: by the date its name starts with, then by when
    its results were committed (several runs share a date)."""
    runs = []
    for path in sorted(SYTEST_DIR.glob("*-results.txt")):
        name = path.name[: -len("-results.txt")]
        m = RUN_DATE_RE.match(name)
        date = m.group(1) if m else ""
        note = None
        summary = SYTEST_DIR / f"{name}-summary.txt"
        if summary.exists():
            first = summary.read_text(encoding="utf-8", errors="replace").splitlines()[:1]
            if first and first[0].startswith("Sytest,"):
                note = _short(first[0], 220)
        awsy = []
        awsy_path = SYTEST_DIR / f"{name}-are-we-synapse-yet.txt"
        if awsy_path.exists():
            for line in awsy_path.read_text(encoding="utf-8", errors="replace").splitlines():
                m = re.match(r"^(\S[^:]*):\s+\d+% \((\d+)/(\d+) tests\)", line)
                if m:
                    awsy.append((m.group(1).strip(), int(m.group(2)), int(m.group(3))))
        committed = git_commit_time(path)
        order = (date, committed if committed is not None else float("inf"), name)
        runs.append(SytestRun(name, date, count_results(path), note, awsy, order))
    runs.sort(key=lambda r: r.order, reverse=True)
    return runs


def latest_full_sytest_run(runs: list[SytestRun]) -> SytestRun | None:
    """The newest run of the whole suite: a run with at least 90% of the most tests any run
    has (a run of one file or a few is a partial run and never the headline)."""
    if not runs:
        return None
    most = max(r.counts.total for r in runs)
    return next((r for r in runs if r.counts.total >= 0.9 * most), None)


@dataclass
class ComplementSuite:
    suite: str
    path: Path
    counts: Counts
    header: str | None
    run: str | None
    date: str | None
    commit: str | None
    assertions: tuple[int, int] | None


def gather_complement_suites() -> list[ComplementSuite]:
    suites = []
    for path in sorted(STATUS_DIR.glob("complement-*-results.txt")):
        suite = path.name[len("complement-") : -len("-results.txt")]
        header = None
        for line in path.read_text(encoding="utf-8", errors="replace").splitlines():
            if line.startswith("# Run "):
                header = line[2:].strip()
                break
        run = date = commit = None
        assertions = None
        if header:
            m = re.match(r"Run (\d+), (\d{4}-\d\d-\d\d), ([0-9a-f]{7,40})", header)
            if m:
                run, date, commit = m.groups()
            a = re.findall(r"(\d+) of (\d+) assertions", header)
            if a:
                # The last one is this run's (the parenthesis names the previous run's first).
                assertions = (int(a[-1][0]), int(a[-1][1]))
        suites.append(ComplementSuite(suite, path, count_results(path), header, run, date, commit, assertions))
    return suites


@dataclass
class LayerStatus:
    layer: str
    name: str
    status: str
    detail: str


def detect_test_layers(sytest: SytestRun | None, complement: list[ComplementSuite]) -> list[LayerStatus]:
    """`PLAN.md` section 12's L0-L12, with status detected from what exists on disk and from the
    committed results. Conservative: "measured" means a committed result file says so, "live"
    means the harness and its tests exist in the tree (each layer's own CI job runs them; this
    script does not), "not started" means nothing is there."""

    def exists(*parts: str) -> bool:
        return (ROOT / Path(*parts)).exists()

    def globbed(pattern: str) -> list[Path]:
        return sorted(ROOT.glob(pattern))

    def has_rust_tests(crate: str) -> bool:
        crate_dir = ROOT / "crates" / crate
        if not crate_dir.is_dir():
            return False
        return any(crate_dir.rglob("tests")) or any(
            "#[test]" in p.read_text(encoding="utf-8", errors="ignore") or "#[tokio::test" in p.read_text(encoding="utf-8", errors="ignore")
            for p in crate_dir.rglob("*.rs")
        )

    layers = []
    layers.append(
        LayerStatus(
            "L0", "Unit and property tests",
            "live, per crate" if any((ROOT / "crates").glob("*/src")) else "not started",
            "`cargo test --workspace --all-targets` in CI (`.github/workflows/ci.yml`); not aggregated here.",
        )
    )
    layers.append(
        LayerStatus(
            "L1", "In-process harness (hs-testkit)",
            "live" if has_rust_tests("hs-testkit") else "not started",
            f"Fake clock, scenario DSL, fake appservice/federation/SMTP/push gateway; "
            f"{len(globbed('crates/hs-cli/tests/*.rs'))} real-server test files in `crates/hs-cli/tests/`.",
        )
    )
    layers.append(
        LayerStatus(
            "L2", "Spec coverage",
            "live" if exists("crates", "hs-spec-coverage", "src", "coverage.rs") else "not started",
            "OpenAPI-driven route diffing for all five APIs; see Spec coverage above.",
        )
    )
    if complement:
        parts = []
        for c in complement:
            top = f"{c.counts.passed} of {c.counts.total} top-level"
            if c.assertions:
                top += f", {c.assertions[0]} of {c.assertions[1]} assertions"
            if c.run:
                top += f" (run {c.run}, {c.date}, `{c.commit}`)"
            parts.append(f"{c.suite} {top}")
        layers.append(LayerStatus("L3", "Complement", "measured", "; ".join(parts) + ". See Complement below."))
    else:
        layers.append(
            LayerStatus(
                "L3", "Complement",
                "harness, no committed results" if exists("tests", "complement", "Dockerfile.template") else "not started",
                "See tests/complement/README.md.",
            )
        )
    if sytest:
        layers.append(
            LayerStatus(
                "L4", "Sytest", "measured",
                f"{sytest.counts.passed} of {sytest.counts.total} pass, {sytest.counts.rate:.1f}% of those run "
                f"(`{sytest.name}`). See Sytest below.",
            )
        )
    else:
        layers.append(
            LayerStatus(
                "L4", "Sytest",
                "harness, no committed results" if exists("tests", "sytest", "plugins") else "not started",
                "See tests/sytest/README.md.",
            )
        )
    differential_state = "harness, self-tested" if exists("tests", "differential", "lib", "normalize.py") else "not started"
    layers.append(
        LayerStatus(
            "L5", "Differential vs Synapse", differential_state,
            "Recorder/normalizer/replayer (tests/differential/) and the state/push oracles (tests/oracle/); "
            "real Synapse runs need Docker"
            + (" and tests/federation-synapse/ runs this server against one." if exists("tests", "federation-synapse", "run.sh") else "."),
        )
    )
    e2e_real = globbed("web/e2e-real/*.spec.ts")
    layers.append(
        LayerStatus(
            "L6", "Client end-to-end (matrix-rust-sdk, Element Web)",
            "live" if has_rust_tests("hs-loadgen") or e2e_real else "not started",
            f"matrix-rust-sdk scenarios in `hs-loadgen`; {len(e2e_real)} Playwright specs against a real server in `web/e2e-real/`.",
        )
    )
    layers.append(
        LayerStatus(
            "L7", "Bridges",
            "live" if has_rust_tests("hs-bridge-conformance") else "not started",
            "`hs-bridge-conformance`: the appservice conformance suite with a fake bridge on matrix-rust-sdk.",
        )
    )
    cluster_tests = globbed("crates/hs-cli/tests/cluster_*.rs")
    layers.append(
        LayerStatus(
            "L8", "Cluster and chaos",
            "live" if cluster_tests else "not started",
            f"{len(cluster_tests)} multi-replica test files (`crates/hs-cli/tests/cluster_*.rs`; the PostgreSQL ones need "
            "`HS_CLUSTER_TEST_POSTGRES_DSN`)"
            + ("; failover and rolling-restart scripts in `deploy/two-pod/`." if exists("deploy", "two-pod", "failover.py") else "."),
        )
    )
    layers.append(
        LayerStatus(
            "L9", "Migration",
            "live" if exists("crates", "hs-cli", "tests", "migration.rs") else "not started",
            "`crates/hs-cli/tests/migration.rs`: the Synapse importer through the real binary.",
        )
    )
    fuzz_crates = [p.parent.parent.name for p in globbed("crates/*/fuzz/Cargo.toml")]
    layers.append(
        LayerStatus(
            "L10", "Fuzzing",
            "live" if fuzz_crates and exists("tests", "fuzz", "run_all.sh") else "not started",
            ("cargo-fuzz targets in " + ", ".join(f"`{c}`" for c in fuzz_crates) + "; `tests/fuzz/run_all.sh` in the `fuzz` workflow.")
            if fuzz_crates else "",
        )
    )
    layers.append(
        LayerStatus(
            "L11", "Performance (hs-loadgen)",
            "harness" if has_rust_tests("hs-loadgen") else "not started",
            "`hs-loadgen` scenarios and `cargo bench` in the `nightly-bench` workflow; no committed performance table yet."
            if exists(".github", "workflows", "nightly-bench.yml") else "",
        )
    )
    layers.append(
        LayerStatus(
            "L12", "Upgrades",
            "partial" if exists("deploy", "two-pod", "rolling.py") else "not started",
            "A rolling restart across two replicas (`deploy/two-pod/rolling.py`, run on the cluster); no version-to-version upgrade suite."
            if exists("deploy", "two-pod", "rolling.py") else "",
        )
    )
    return layers


def render_markdown(
    tracks: list[TrackStatus],
    coverage: dict | None,
    coverage_source: str,
    sytest_runs: list[SytestRun],
    complement: list[ComplementSuite],
    layers: list[LayerStatus],
) -> str:
    out = []
    out.append("# Parity dashboard")
    out.append("")
    out.append(
        "Generated by `tools/dashboard.py` (track 14) from committed results. Per "
        "`docs/workstreams/README.md` rule 5, this is the project's only status report; do not "
        "hand-maintain a second one. Regenerate after a Sytest or Complement run is committed, or "
        "a track's status file changes (no cargo needed: spec coverage then comes from "
        "`docs/status/spec-coverage.json`):"
    )
    out.append("")
    out.append("```bash")
    out.append("python3 tools/dashboard.py")
    out.append("```")
    out.append("")
    out.append(f"_Generated: {utc_now()}_")
    out.append("")

    out.append("## Spec coverage")
    out.append("")
    if coverage is None:
        out.append(
            "_Unavailable this run: cargo could not run `hs-spec-coverage` and there is no snapshot "
            "(see stderr from `tools/dashboard.py`)._"
            if coverage_source != "skipped"
            else "_Skipped this run (`--skip-coverage`)._"
        )
    else:
        summary = coverage["summary"]
        manifest = coverage.get("routes_manifest")
        manifest_date = coverage.get("routes_manifest_generated_at")
        out.append(
            f"**{summary['total_registered']} / {summary['total_spec_routes']} spec routes registered "
            f"({summary['overall_percent']:.1f}%)**, against "
            + (
                f"the `{manifest}` manifest" + (f" (generated {manifest_date})" if manifest_date else "")
                if manifest
                else "no routes manifest"
            )
            + ". "
            + {
                "run": "Run by this generation.",
                "snapshot": f"From the committed snapshot `docs/status/spec-coverage.json`, made {coverage.get('generated_at') or 'at an unknown time'} (no cargo or spec checkout this run).",
                "file": f"From `{coverage.get('_path')}` (`--coverage-from`), made {coverage.get('generated_at') or 'at an unknown time'}.",
            }.get(coverage_source, "")
        )
        out.append("")
        out.append("| API | Spec routes | Registered | Missing | Extra | Coverage |")
        out.append("|---|---:|---:|---:|---:|---:|")
        for api in summary["apis"]:
            out.append(
                f"| {api['family']} | {api['spec_total']} | {api['registered']} | {api['missing']} | "
                f"{api['extra']} | {api['percent']:.1f}% |"
            )
        out.append("")
        out.append(
            "\"Extra\" is a mounted route the spec does not list (unstable and MSC endpoints, the admin "
            "API, `/_synapse/admin`). `hs routes-manifest -o docs/status/routes.json` (or "
            "`--refresh-routes`) brings the manifest up to date with the router."
        )
    out.append("")

    out.append("## Sytest")
    out.append("")
    latest = latest_full_sytest_run(sytest_runs)
    if not sytest_runs:
        out.append("_No committed runs under `docs/status/sytest/`._")
    else:
        if latest:
            c = latest.counts
            out.append(
                f"**{c.passed} of {c.total} pass ({c.rate:.1f}% of the {c.passed + c.failed} run; "
                f"{c.failed} fail, {c.skipped} skipped)** in the newest full run, `{latest.name}` "
                f"(`docs/status/sytest/{latest.name}-results.txt`)."
            )
            if latest.note:
                out.append("")
                out.append(f"> {latest.note}")
            if latest.awsy:
                out.append("")
                out.append("Its \"are we Synapse yet\" groups:")
                out.append("")
                out.append("| Group | Pass | Tests | % |")
                out.append("|---|---:|---:|---:|")
                for name, passed, total in latest.awsy:
                    out.append(f"| {name} | {passed} | {total} | {100.0 * passed / total if total else 0:.0f}% |")
            out.append("")
        out.append("Every committed run, newest first (a run of a few files is partial, not the headline):")
        out.append("")
        out.append("| Run | Tests | Pass | Fail | Skip | Pass rate (of run) |")
        out.append("|---|---:|---:|---:|---:|---:|")
        for r in sytest_runs:
            c = r.counts
            out.append(f"| `{r.name}` | {c.total} | {c.passed} | {c.failed} | {c.skipped} | {c.rate:.1f}% |")
    out.append("")

    out.append("## Complement")
    out.append("")
    if not complement:
        out.append("_No committed baselines (`docs/status/complement-<suite>-results.txt`)._")
    else:
        out.append(
            "Top-level tests from the committed baselines (`tools/complement_triage.py "
            "--write-baseline`); a top-level test passes only when every subtest does, so the "
            "assertion count is the finer measure."
        )
        out.append("")
        out.append("| Suite | Run | Top-level pass | Fail | Skip | Assertions |")
        out.append("|---|---|---:|---:|---:|---:|")
        for c in complement:
            run = f"{c.run}, {c.date}, `{c.commit}`" if c.run else "-"
            assertions = f"{c.assertions[0]} of {c.assertions[1]}" if c.assertions else "-"
            out.append(
                f"| {c.suite} | {run} | {c.counts.passed} of {c.counts.total} | {c.counts.failed} | "
                f"{c.counts.skipped} | {assertions} |"
            )
        out.append("")
        for c in complement:
            if c.counts.failing:
                out.append(f"Failing in {c.suite} ({len(c.counts.failing)}): " + ", ".join(f"`{n}`" for n in c.counts.failing) + ".")
                out.append("")
        out.append(
            "Four federation tests that race two servers run with a harness patch "
            "(`tests/complement/patches/`, README \"Patches to upstream tests\") from the run after "
            "the one above."
            if (ROOT / "tests" / "complement" / "patches").is_dir()
            else ""
        )
    out.append("")

    out.append("## Test layers (PLAN.md section 12)")
    out.append("")
    out.append("| Layer | What | Status | Detail |")
    out.append("|---|---|---|---|")
    for l in layers:
        out.append(f"| {l.layer} | {l.name} | {l.status} | {l.detail} |")
    out.append("")

    out.append("## Tracks")
    out.append("")
    out.append("| Track | Title | Last updated | Newest section |")
    out.append("|---|---|---|---|")
    for t in tracks:
        section = (t.latest_section or "-").replace("|", "\\|")
        out.append(f"| [{t.number}]({t.path.name}) | {t.title} | {t.last_updated or '-'} | {section} |")
    out.append("")
    out.append(
        "Per-track detail lives in each file linked by number above (`docs/status/<number>-*.md`), newest "
        "section first."
    )
    out.append("")

    out.append("## Bridge conformance / performance")
    out.append("")
    out.append(
        "Not yet reported here: the bridge conformance suite (L7) and the benchmarks (L11) run in CI "
        "but commit no result file for this script to read, and the `PLAN.md` section 13 "
        "performance table has no measurements yet."
    )
    out.append("")

    return "\n".join(out) + "\n"


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--print", action="store_true", dest="print_only", help="print to stdout instead of writing docs/status/dashboard.md")
    parser.add_argument("--skip-coverage", action="store_true", help="omit the spec-coverage section")
    parser.add_argument(
        "--coverage-from", type=Path, metavar="JSON",
        help="report this saved coverage summary (docs/status/spec-coverage.json, or a bare hs-spec-coverage --json-out file) and run no cargo",
    )
    parser.add_argument("--refresh-routes", action="store_true", help="rewrite docs/status/routes.json with `hs routes-manifest` first (cargo)")
    args = parser.parse_args()

    tracks = gather_status_summaries()

    coverage = None
    coverage_source = "skipped"
    if not args.skip_coverage:
        if args.coverage_from:
            coverage = load_coverage_snapshot(args.coverage_from)
            if coverage:
                coverage["_path"] = str(args.coverage_from)
            coverage_source = "file"
        else:
            if args.refresh_routes:
                refresh_routes_manifest()
            coverage = run_spec_coverage()
            coverage_source = "run"
            if coverage is not None and not args.print_only:
                COVERAGE_SNAPSHOT.write_text(json.dumps(coverage, indent=2) + "\n", encoding="utf-8")
                print(f"wrote {COVERAGE_SNAPSHOT}", file=sys.stderr)
            if coverage is None and COVERAGE_SNAPSHOT.exists():
                coverage = load_coverage_snapshot(COVERAGE_SNAPSHOT)
                coverage_source = "snapshot"

    sytest_runs = gather_sytest_runs()
    complement = gather_complement_suites()
    layers = detect_test_layers(latest_full_sytest_run(sytest_runs), complement)

    markdown = render_markdown(tracks, coverage, coverage_source, sytest_runs, complement, layers)

    if args.print_only:
        print(markdown)
    else:
        DASHBOARD_PATH.write_text(markdown, encoding="utf-8")
        print(f"wrote {DASHBOARD_PATH}", file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main())
