#!/usr/bin/env python3
"""Delete the dependency artifacts in a target dir that the last builds didn't use.

    cargo ... --message-format json | sweep-target.py <target dir>

rust-cache keeps every artifact whose crate is still a dependency, at any version, so a cache
restored for an older Cargo.lock and saved again holds both builds, and main's cache grows with
every lockfile change. CI runs this on main before rust-cache saves. It reads the JSON messages of
every build the job ran (fresh units are reported too), keeps exactly the artifacts, build script
outputs and fingerprints of the registry and git dependencies they name, and deletes everything
else in the dev profile: other builds of those crates, and the workspace's own artifacts, which
rust-cache drops anyway.
"""

import json
import re
import shutil
import sys
from pathlib import Path

# Cargo names a unit's files and directories `<name>-<16 hex digits>[.ext]`.
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
