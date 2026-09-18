"""Diffs two normalized step-result lists (`{"name", "response": {"status", "json"}}`, in the
shape both `driver.run_workload` and `replay.py`'s baseline loader produce) and reports, per step,
whether they matched.
"""

from __future__ import annotations

from typing import Any


def diff_runs(
    baseline_steps: list[dict[str, Any]], candidate_steps: list[dict[str, Any]]
) -> list[dict[str, Any]]:
    """Returns one result dict per step name seen on either side:

    - `{"name", "match": True, "baseline_status", "candidate_status"}` when both status and
      normalized JSON body matched.
    - `{"name", "match": False, "baseline_status", "candidate_status", "baseline_json",
      "candidate_json"}` when both sides had the step but disagreed.
    - `{"name", "match": False, "reason": "missing in candidate" | "missing in baseline"}` when
      only one side ran that step at all (a sign the workload and the baseline have drifted out
      of sync, not itself a behavioral finding).
    """
    by_name_candidate = {s["name"]: s for s in candidate_steps}
    seen: set[str] = set()
    results: list[dict[str, Any]] = []

    for baseline in baseline_steps:
        name = baseline["name"]
        seen.add(name)
        candidate = by_name_candidate.get(name)
        if candidate is None:
            results.append({"name": name, "match": False, "reason": "missing in candidate"})
            continue
        status_match = baseline["response"]["status"] == candidate["response"]["status"]
        body_match = baseline["response"]["json"] == candidate["response"]["json"]
        results.append(
            {
                "name": name,
                "match": status_match and body_match,
                "baseline_status": baseline["response"]["status"],
                "candidate_status": candidate["response"]["status"],
                "baseline_json": baseline["response"]["json"],
                "candidate_json": candidate["response"]["json"],
            }
        )

    for candidate in candidate_steps:
        if candidate["name"] not in seen:
            results.append(
                {"name": candidate["name"], "match": False, "reason": "missing in baseline"}
            )

    return results


def summarize(results: list[dict[str, Any]]) -> tuple[int, int]:
    """`(passed, total)` step counts."""
    total = len(results)
    passed = sum(1 for r in results if r["match"])
    return passed, total
