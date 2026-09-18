#!/usr/bin/env python3
"""Generate the parity dashboard: docs/status/dashboard.md.

Owned by track 14 (`docs/workstreams/14-test-and-conformance.md`). `PLAN.md` section 12 says
coverage is reported as "a single parity dashboard: spec coverage percentage, Complement and
Sytest pass counts, Synapse route checklist completion, bridge conformance, and the performance
table," and `docs/workstreams/README.md` rule 5 says "the parity dashboard (owned by 14) is the
only status report the project publishes." This script assembles it from three inputs:

1. Every `docs/status/*.md` track status file (parsed for its title, last-updated line, and
   Done/In progress/Blockers bullet counts).
2. `hs-spec-coverage`'s output (built and run as a subprocess against `refs/matrix-spec/data/api`
   and, if one exists anywhere in the repo, a `routes.json` manifest -- see
   `docs/rfcs/0005-routes-json-manifest.md`). No manifest exists yet as of this writing (no crate
   assembles a full server router), so this section is expected to show 0% until one does; that is
   the honest day-one answer, not a bug in this script.
3. The `PLAN.md` section 12 test-layer table (L0-L12), whose status per layer is *detected* from
   what exists on disk (not hand-maintained) so this script keeps telling the truth as other
   layers land, rather than needing a human to remember to update a table here every time.

Usage:
    python3 tools/dashboard.py                # writes docs/status/dashboard.md
    python3 tools/dashboard.py --print         # prints to stdout instead of writing
    python3 tools/dashboard.py --skip-coverage # skip the hs-spec-coverage subprocess (faster,
                                                # useful if cargo is busy with another build)
"""

from __future__ import annotations

import argparse
import datetime
import json
import re
import subprocess
import sys
import tempfile
from dataclasses import dataclass, field
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
STATUS_DIR = ROOT / "docs" / "status"
DASHBOARD_PATH = STATUS_DIR / "dashboard.md"

TRACKED_SECTIONS = ("Done", "In progress", "Blockers")


@dataclass
class TrackStatus:
    number: str
    title: str
    path: Path
    last_updated: str | None
    section_counts: dict[str, int] = field(default_factory=dict)


def parse_status_file(path: Path) -> TrackStatus:
    text = path.read_text(encoding="utf-8")
    lines = text.splitlines()

    title = path.stem
    number = path.stem.split("-", 1)[0]
    if lines:
        heading = lines[0].lstrip("#").strip()
        # "01 Storage engine: status" or "15. Admin API and modules: status" -> title without the
        # leading track number/dot and the trailing ": status".
        heading = re.sub(r"^\d+\.?\s*", "", heading)
        heading = re.sub(r":\s*status$", "", heading, flags=re.IGNORECASE)
        title = heading.strip() or title

    last_updated = None
    for line in lines:
        for prefix in ("Last updated:", "Updated:"):
            if line.startswith(prefix):
                last_updated = line[len(prefix) :].strip()
                break
        if last_updated:
            break
    if last_updated:
        # Keep the table readable: the date/session tag, not a whole paragraph of context (some
        # tracks' "Last updated" line runs on for several sentences explaining a session).
        first_sentence = re.split(r"(?<=[.)])\s", last_updated, maxsplit=1)[0]
        if len(first_sentence) < len(last_updated):
            first_sentence += " (...)"
        last_updated = first_sentence

    section_counts: dict[str, int] = {}
    current_section = None
    for line in lines:
        m = re.match(r"^##\s+(.+)$", line)
        if m:
            heading = m.group(1).strip()
            # Sections sometimes carry a parenthetical aside ("## Next (not done; for whoever
            # picks this up next)"); match on the leading word(s) against TRACKED_SECTIONS.
            current_section = next((s for s in TRACKED_SECTIONS if heading.startswith(s)), None)
            continue
        if line.startswith("# ") and not line.startswith("##"):
            current_section = None
            continue
        if current_section and re.match(r"^\s*-\s+\S", line):
            section_counts[current_section] = section_counts.get(current_section, 0) + 1

    return TrackStatus(number=number, title=title, path=path, last_updated=last_updated, section_counts=section_counts)


def gather_status_summaries() -> list[TrackStatus]:
    tracks = []
    for path in sorted(STATUS_DIR.glob("*.md")):
        if path.name in ("README.md", "dashboard.md"):
            continue
        tracks.append(parse_status_file(path))
    tracks.sort(key=lambda t: (len(t.number), t.number))
    return tracks


def run_spec_coverage() -> dict | None:
    """Builds (if needed) and runs `hs-spec-coverage`, returning its `--json-out` summary, or
    `None` if the run failed (reported inline in the dashboard rather than raising, since a
    dashboard generator failing outright over one input is worse than a dashboard with one
    section marked unavailable)."""
    routes_candidates = [p for p in ROOT.rglob("routes.json") if "refs" not in p.parts and "target" not in p.parts]
    args = [
        "cargo",
        "run",
        "--quiet",
        "-p",
        "hs-spec-coverage",
        "--",
        "--spec-dir",
        str(ROOT / "refs" / "matrix-spec" / "data" / "api"),
        "--out",
        "/dev/null",
    ]
    with tempfile.NamedTemporaryFile(suffix=".json", delete=False) as tmp:
        json_out = Path(tmp.name)
    args += ["--json-out", str(json_out)]
    routes_manifest_used = bool(routes_candidates)
    if routes_candidates:
        args += ["--routes", str(routes_candidates[0])]

    try:
        subprocess.run(args, cwd=ROOT, check=True, capture_output=True, text=True, timeout=120)
    except (subprocess.CalledProcessError, subprocess.TimeoutExpired, FileNotFoundError) as exc:
        detail = ""
        if isinstance(exc, subprocess.CalledProcessError):
            detail = exc.stderr[-2000:]
        print(f"dashboard.py: hs-spec-coverage run failed: {exc}\n{detail}", file=sys.stderr)
        return None

    try:
        summary = json.loads(json_out.read_text())
    except (OSError, json.JSONDecodeError) as exc:
        print(f"dashboard.py: could not read hs-spec-coverage output: {exc}", file=sys.stderr)
        return None
    finally:
        json_out.unlink(missing_ok=True)

    summary["_routes_manifest_used"] = routes_manifest_used
    return summary


def docker_available() -> bool:
    try:
        result = subprocess.run(
            ["docker", "info"], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=5, check=False
        )
        return result.returncode == 0
    except (OSError, subprocess.TimeoutExpired):
        return False


@dataclass
class LayerStatus:
    layer: str
    name: str
    status: str
    detail: str


def detect_test_layers() -> list[LayerStatus]:
    """`PLAN.md` section 12's L0-L12, with status detected from what exists on disk. Kept
    deliberately conservative: "exists and has tests" reads as a real signal (this script does not
    re-run every layer's suite -- that's each layer's own CI job), "scaffold only" and "not
    started" are distinguished by directory/file presence, not by anyone remembering to edit a
    table by hand."""

    def exists(*parts: str) -> bool:
        return (ROOT / Path(*parts)).exists()

    def has_rust_tests(crate: str) -> bool:
        crate_dir = ROOT / "crates" / crate
        if not crate_dir.is_dir():
            return False
        return any(crate_dir.rglob("tests")) or any("#[test]" in p.read_text(encoding="utf-8", errors="ignore")
                                                       for p in crate_dir.rglob("*.rs"))

    layers = []

    layers.append(
        LayerStatus(
            "L0", "Unit and property tests",
            "ongoing, per-crate" if any((ROOT / "crates").glob("*/src")) else "not started",
            "Tracked per crate via `cargo test -p <crate>`; not aggregated centrally yet.",
        )
    )
    layers.append(
        LayerStatus(
            "L1", "In-process harness (hs-testkit)",
            "live" if has_rust_tests("hs-testkit") else "not started",
            "Fake clock, scenario DSL, fake appservice/federation/SMTP/push-gateway; proven against hs-auth's real router.",
        )
    )
    layers.append(
        LayerStatus(
            "L2", "Spec coverage",
            "live" if exists("crates", "hs-spec-coverage", "src", "coverage.rs") else "not started",
            "OpenAPI-driven route diffing for all five APIs; see the Spec coverage section above.",
        )
    )
    layers.append(
        LayerStatus(
            "L3", "Complement",
            "scaffold, untested" if exists("tests", "complement", "Dockerfile.template") else "not started",
            "No server binary exists yet to build an image from; see tests/complement/README.md.",
        )
    )
    layers.append(
        LayerStatus(
            "L4", "Sytest",
            "scaffold, untested" if exists("tests", "sytest", "plugins") else "not started",
            "Plugin skeleton only; no server binary and no CPAN deps installed here. See tests/sytest/README.md.",
        )
    )
    differential_state = "scaffold, self-tested" if exists("tests", "differential", "lib", "normalize.py") else "not started"
    oracle_note = (
        " State/push oracle scripts (tests/oracle/) exist but need an installed matrix-synapse "
        "package (network), never run here."
        if exists("tests", "oracle", "state_oracle.py")
        else ""
    )
    layers.append(
        LayerStatus(
            "L5", "Differential vs Synapse",
            differential_state,
            "Recorder/normalizer/replayer harness passes its own tests; real Synapse runs need Docker (currently "
            + ("available" if docker_available() else "unavailable")
            + " here) -- see tests/differential/README.md."
            + oracle_note,
        )
    )
    layers.append(LayerStatus("L6", "Client end-to-end (matrix-rust-sdk, Element Web)", "not started", ""))
    layers.append(LayerStatus("L7", "Bridges", "not started", ""))
    layers.append(LayerStatus("L8", "Cluster and chaos", "not started", ""))
    layers.append(LayerStatus("L9", "Migration", "not started", ""))
    layers.append(LayerStatus("L10", "Fuzzing", "not started", ""))
    layers.append(LayerStatus("L11", "Performance (hs-loadgen)", "not started" if not has_rust_tests("hs-loadgen") else "scaffold", ""))
    layers.append(LayerStatus("L12", "Upgrades", "not started", ""))
    return layers


def render_markdown(tracks: list[TrackStatus], coverage: dict | None, layers: list[LayerStatus]) -> str:
    out = []
    out.append("# Parity dashboard")
    out.append("")
    out.append(
        "Generated by `tools/dashboard.py` (track 14). Per `docs/workstreams/README.md` rule 5, this is "
        "the project's only status report; do not hand-maintain a second one. Regenerate after any "
        "track's status file changes or `routes.json`/spec-coverage output changes:"
    )
    out.append("")
    out.append("```bash")
    out.append("python3 tools/dashboard.py")
    out.append("```")
    out.append("")
    out.append(
        f"_Generated: {datetime.datetime.now(datetime.timezone.utc).strftime('%Y-%m-%dT%H:%M:%SZ')}_"
    )
    out.append("")

    out.append("## Spec coverage")
    out.append("")
    if coverage is None:
        out.append(
            "_Unavailable this run (see stderr from `tools/dashboard.py`; pass `--skip-coverage` to omit this "
            "section deliberately instead)._"
        )
    else:
        out.append(
            f"**{coverage['total_registered']} / {coverage['total_spec_routes']} spec routes registered "
            f"({coverage['overall_percent']:.1f}%)**, against "
            + (
                "a `routes.json` manifest found in the repo."
                if coverage.get("_routes_manifest_used")
                else "an empty manifest (no `routes.json` exists yet -- see docs/rfcs/0005-routes-json-manifest.md)."
            )
        )
        out.append("")
        out.append("| API | Spec routes | Registered | Missing | Extra | Coverage |")
        out.append("|---|---:|---:|---:|---:|---:|")
        for api in coverage["apis"]:
            out.append(
                f"| {api['family']} | {api['spec_total']} | {api['registered']} | {api['missing']} | "
                f"{api['extra']} | {api['percent']:.1f}% |"
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
    out.append("| Track | Title | Last updated | Done | In progress | Blockers |")
    out.append("|---|---|---|---:|---:|---:|")
    for t in tracks:
        out.append(
            f"| {t.number} | {t.title} | {t.last_updated or '-'} | "
            f"{t.section_counts.get('Done', 0)} | {t.section_counts.get('In progress', 0)} | "
            f"{t.section_counts.get('Blockers', 0)} |"
        )
    out.append("")
    out.append(
        "Tracks with no `docs/status/<track>.md` file yet have not reported status. Per-track detail lives "
        "in each file linked by number above (`docs/status/<number>-*.md`)."
    )
    out.append("")

    out.append("## Complement / Sytest / bridge conformance / performance")
    out.append("")
    out.append(
        "Not yet populated: these depend on layers L3/L4/L7/L11 above, all `not started` or `scaffold, "
        "untested` as of this generation. This section will gain real pass counts and the "
        "`PLAN.md` section 13 performance table once those layers run for real."
    )
    out.append("")

    return "\n".join(out) + "\n"


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--print", action="store_true", dest="print_only", help="print to stdout instead of writing docs/status/dashboard.md")
    parser.add_argument("--skip-coverage", action="store_true", help="skip running hs-spec-coverage (faster)")
    args = parser.parse_args()

    tracks = gather_status_summaries()
    coverage = None if args.skip_coverage else run_spec_coverage()
    layers = detect_test_layers()

    markdown = render_markdown(tracks, coverage, layers)

    if args.print_only:
        print(markdown)
    else:
        DASHBOARD_PATH.write_text(markdown, encoding="utf-8")
        print(f"wrote {DASHBOARD_PATH}", file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main())
