#!/bin/sh
# Python provides the same advisory lock on Linux and macOS.
set -eu
exec python3 -c '
import datetime
import fcntl
import pathlib
import subprocess
import sys

entry = sys.stdin.read()
if not entry.strip():
    raise SystemExit("review entry must not be empty")
common = pathlib.Path(subprocess.check_output(
    ["git", "rev-parse", "--path-format=absolute", "--git-common-dir"], text=True
).strip())
records = subprocess.check_output(
    ["git", "worktree", "list", "--porcelain", "-z"]
).split(b"\0")
root = pathlib.Path(next(record[9:].decode() for record in records
                         if record.startswith(b"worktree ")))
log_dir = root / "docs" / "review-logs"
with (common / "review-log.lock").open("a") as lock:
    fcntl.flock(lock, fcntl.LOCK_EX)
    log_dir.mkdir(parents=True, exist_ok=True)
    target = log_dir / (datetime.date.today().isoformat() + ".md")
    with target.open("a") as log:
        log.write(entry)
        if not entry.endswith("\n"):
            log.write("\n")
print(target)
'
