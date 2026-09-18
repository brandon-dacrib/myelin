"""Executes a scripted workload (`tests/differential/workloads/*.json`) against one HTTP base URL.

A workload is `{"name": ..., "steps": [...]}`, each step:

    {
      "name": "register",                 # required; used to align steps across two runs
      "method": "POST",                   # required
      "path": "/_matrix/client/v3/register",  # required; may reference {{vars}} extracted earlier
      "body": {...},                      # optional JSON body; may reference {{vars}}
      "auth_token": "{{access_token}}",   # optional; sent as `Authorization: Bearer <value>`
      "extract": {"access_token": "$.access_token"}   # optional; pulls values out of the
                                                       # response JSON for later steps' {{vars}}
    }

`extract` pointers are a minimal JSONPath-lite: `$.foo.bar.0.baz`, dot-separated, numeric segments
index into lists. This is intentionally not a full JSONPath implementation -- the workloads in
this directory only ever need to pull a handful of top-level or one-level-nested fields
(`access_token`, `room_id`, `event_id`, ...) out of a response.
"""

from __future__ import annotations

import json
import re
import urllib.error
import urllib.request
from typing import Any

_VAR_RE = re.compile(r"\{\{(\w+)\}\}")


def _substitute(value: Any, variables: dict[str, Any]) -> Any:
    if isinstance(value, str):
        whole = _VAR_RE.fullmatch(value)
        if whole and whole.group(1) in variables:
            # Whole-string reference: preserve the variable's original type (a room_id stays a
            # string here too, but this matters for non-string extracted values in the future).
            return variables[whole.group(1)]
        return _VAR_RE.sub(lambda m: str(variables.get(m.group(1), m.group(0))), value)
    if isinstance(value, dict):
        return {k: _substitute(v, variables) for k, v in value.items()}
    if isinstance(value, list):
        return [_substitute(v, variables) for v in value]
    return value


def _extract(response_json: Any, pointer: str, variables: dict[str, Any], name: str) -> None:
    node = response_json
    body = pointer[1:] if pointer.startswith("$") else pointer
    parts = [p for p in body.split(".") if p]
    for part in parts:
        if node is None:
            break
        if isinstance(node, list):
            node = node[int(part)]
        elif isinstance(node, dict):
            node = node.get(part)
        else:
            node = None
    variables[name] = node


def run_workload(
    base_url: str, workload: dict[str, Any], extra_headers: dict[str, str] | None = None
) -> list[dict[str, Any]]:
    """Runs every step of `workload` against `base_url`, in order, substituting `{{var}}`
    placeholders from earlier steps' `extract`ed values.

    Returns one `{"name", "request": {...}, "response": {"status", "json"}}` dict per step, in
    the same order the steps ran (which is also the order a recording proxy watching this traffic
    would have logged them in -- see `replay.py` for why that ordering is what aligns a recorded
    baseline to a workload's step names).
    """
    variables: dict[str, Any] = {}
    results: list[dict[str, Any]] = []
    for step in workload["steps"]:
        method = step["method"]
        path = _substitute(step["path"], variables)
        body = _substitute(step.get("body"), variables) if "body" in step else None
        headers = dict(extra_headers or {})
        headers.setdefault("Content-Type", "application/json")
        if step.get("auth_token"):
            token = _substitute(step["auth_token"], variables)
            headers["Authorization"] = f"Bearer {token}"

        data = json.dumps(body).encode("utf-8") if body is not None else None
        req = urllib.request.Request(
            base_url.rstrip("/") + path, data=data, method=method, headers=headers
        )
        try:
            with urllib.request.urlopen(req, timeout=10) as resp:
                status = resp.status
                raw = resp.read()
        except urllib.error.HTTPError as exc:
            status = exc.code
            raw = exc.read()

        try:
            resp_json = json.loads(raw) if raw else None
        except json.JSONDecodeError:
            resp_json = None

        for var_name, pointer in step.get("extract", {}).items():
            _extract(resp_json, pointer, variables, var_name)

        results.append(
            {
                "name": step["name"],
                "request": {"method": method, "path": path, "body": body},
                "response": {"status": status, "json": resp_json},
            }
        )
    return results
