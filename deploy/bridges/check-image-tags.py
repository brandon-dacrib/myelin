#!/usr/bin/env python3
"""Compare the bridge catalogue's pinned image tags with each upstream's newest release.

The catalogue (crates/hs-admin/src/bridge_types.rs) pins every bridge image to a release tag
(decision 0037), and bumping a pin is a release step. This is the first half of that step: it
reads the pins out of the source, asks GitHub for each project's latest release, and prints a
table with one row per bridge. Exit 1 when any pin is behind, so a workflow can open an issue
(.github/workflows/bridge-images.yml, weekly). The second half -- the real-bridge login test
and a demo roll -- stays a person's decision; nothing here edits the catalogue.

Usage: check-image-tags.py [--catalogue PATH] [--markdown]
Environment: GITHUB_TOKEN raises the API rate limit (optional).
"""
import argparse
import json
import os
import re
import sys
import urllib.error
import urllib.request

# Image repository -> (GitHub repository, how its release tags map to image tags).
# mautrix/* images are on dock.mau.dev under the project's name, tagged as the release.
RELEASE_TAG_TO_IMAGE_TAG = {
    "hif1/heisenbridge": ("hifi/heisenbridge", lambda t: t.lstrip("v")),
    "matrixdotorg/matrix-appservice-irc": ("matrix-org/matrix-appservice-irc", lambda t: "release-" + t.lstrip("v")),
    "ghcr.io/matrix-org/matrix-hookshot": ("matrix-org/matrix-hookshot", lambda t: t.lstrip("v")),
}

MAUTRIX_LINE = re.compile(r'^\s*"(mautrix-[a-z]+)",\s*"[^"]*",\s*"([a-z]+)",\s*"([^"]+)",')
IMAGE_LINE = re.compile(r'^\s*image:\s*"([^":]+):([^"]+)",')
ID_LINE = re.compile(r'^\s*id:\s*"([^"]+)",')


def pins(path):
    """Yields (bridge id, image repository, pinned tag, GitHub repository, tag mapper)."""
    last_id = None
    for line in open(path, encoding="utf-8"):
        m = MAUTRIX_LINE.match(line)
        if m:
            bridge, net, tag = m.groups()
            yield bridge, f"dock.mau.dev/mautrix/{net}", tag, f"mautrix/{net}", lambda t: t
            continue
        m = ID_LINE.match(line)
        if m:
            last_id = m.group(1)
            continue
        m = IMAGE_LINE.match(line)
        if m and last_id:
            repo, tag = m.groups()
            gh, mapper = RELEASE_TAG_TO_IMAGE_TAG.get(repo, (None, None))
            yield last_id, repo, tag, gh, mapper
            last_id = None


VERSION_TAG = re.compile(r"^v?\d+(\.\d+)+$")


def github(path):
    req = urllib.request.Request(
        f"https://api.github.com/repos/{path}",
        headers={"Accept": "application/vnd.github+json", "User-Agent": "myelin-bridge-image-check"},
    )
    token = os.environ.get("GITHUB_TOKEN")
    if token:
        req.add_header("Authorization", f"Bearer {token}")
    with urllib.request.urlopen(req, timeout=30) as resp:
        return json.load(resp)


def latest_release(gh_repo):
    """The project's latest release tag, or, for a project that tags without publishing
    releases (heisenbridge), its highest version-shaped tag."""
    try:
        try:
            return github(f"{gh_repo}/releases/latest").get("tag_name", "")
        except urllib.error.HTTPError as e:
            if e.code != 404:
                raise
        tags = [t["name"] for t in github(f"{gh_repo}/tags?per_page=100") if VERSION_TAG.match(t["name"])]
        if not tags:
            return "error: no releases and no version tags"
        return max(tags, key=lambda t: tuple(int(x) for x in t.lstrip("v").split(".")))
    except urllib.error.HTTPError as e:
        return f"error: HTTP {e.code}"
    except (urllib.error.URLError, TimeoutError) as e:
        return f"error: {e}"


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--catalogue", default=os.path.join(os.path.dirname(__file__), "..", "..",
                                                        "crates", "hs-admin", "src", "bridge_types.rs"))
    ap.add_argument("--markdown", action="store_true", help="a Markdown table (for a job summary or an issue)")
    args = ap.parse_args()
    rows = []
    behind = 0
    for bridge, repo, tag, gh, mapper in pins(args.catalogue):
        if not gh:
            rows.append((bridge, f"{repo}:{tag}", "(no upstream mapping)", "unknown"))
            continue
        release = latest_release(gh)
        if release.startswith("error"):
            rows.append((bridge, f"{repo}:{tag}", release, "unknown"))
            continue
        want = mapper(release)
        if want == tag:
            rows.append((bridge, f"{repo}:{tag}", release, "current"))
        else:
            behind += 1
            rows.append((bridge, f"{repo}:{tag}", release, f"behind: {want}"))
    if args.markdown:
        print("| bridge | pinned image | upstream latest release | status |")
        print("|---|---|---|---|")
        for r in rows:
            print("| " + " | ".join(r) + " |")
    else:
        w = [max(len(r[i]) for r in rows) for i in range(4)]
        for r in rows:
            print("  ".join(r[i].ljust(w[i]) for i in range(4)))
    print()
    print(f"{behind} of {len(rows)} pins behind their upstream's latest release")
    return 1 if behind else 0


if __name__ == "__main__":
    sys.exit(main())
