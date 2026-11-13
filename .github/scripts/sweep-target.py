#!/usr/bin/env python3
"""Fresh units are reported in the JSON messages too, so replaying the job's builds names every
dependency artifact they used. The workspace's own artifacts go too: rust-cache drops them anyway.
"""

import json
import re
import shutil
import sys
from pathlib import Path

UNIT_HASH = re.compile(r"-([0-9a-f]{16})(?=[./]|$)")


def used_hashes(messages):
    keep = set()
    for line in messages:
        try:
            message = json.loads(line)
        except ValueError:
            continue
        if message.get("package_id", "").startswith("path+"):
            continue  # the workspace's own crates
        reason = message.get("reason")
        if reason == "compiler-artifact":
            paths = message["filenames"]
        elif reason == "build-script-executed":
            paths = [message["out_dir"]]
        else:
            continue
        for path in paths:
            keep.update(UNIT_HASH.findall(path))
    return keep


def sweep(profile_dir, keep):
    removed = kept = 0
    for kind in ("deps", "build", ".fingerprint"):
        directory = profile_dir / kind
        if not directory.is_dir():
            continue
        for entry in directory.iterdir():
            unit = UNIT_HASH.search(entry.name)
            if unit and unit.group(1) in keep:
                kept += 1
                continue
            if entry.is_dir() and not entry.is_symlink():
                shutil.rmtree(entry)
            else:
                entry.unlink()
            removed += 1
    return kept, removed


def main():
    if len(sys.argv) != 2:
        sys.exit("usage: cargo ... --message-format json | sweep-target.py <target dir>")
    keep = used_hashes(sys.stdin)
    if not keep:
        sys.exit("sweep-target: no dependency artifacts in the messages, so nothing is kept; not sweeping")
    kept, removed = sweep(Path(sys.argv[1]) / "debug", keep)
    print(f"sweep-target: kept {kept} entries of {len(keep)} dependency units, removed {removed}")


if __name__ == "__main__":
    main()
