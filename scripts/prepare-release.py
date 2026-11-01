#!/usr/bin/env python3
"""Prepare an automatic release from conventional commits since the last tag.

Prints the new tag after bumping every version file, or nothing when no commit
is releasable. feat -> minor, fix -> patch; everything else is ignored, which
also skips the bot's own chore(release) commits. CI commits, tags and pushes.
"""
import re
import subprocess
from pathlib import Path

TAG_PREFIX = "v"
# Only commits touching these paths count; empty means the whole repository.
RELEASE_PATHS = []
# (file, regex with the version as group 2, expected number of matches)
VERSION_FILES = [
    ("Cargo.toml", r'\A(.*?^version = ")([^"]+)', 1),
    ("Cargo.lock", r'(^name = "chatgpt-use"\nversion = ")([^"]+)', 1),
]
RELEASABLE = re.compile(r"^(feat|fix)(\([^)]*\))?(!)?: ")


def git(*args):
    return subprocess.check_output(("git",) + args, text=True)


def main():
    stable = re.compile(re.escape(TAG_PREFIX) + r"(\d+)\.(\d+)\.(\d+)$")
    tags = git("tag", "--list", TAG_PREFIX + "*", "--sort=-version:refname").split()
    latest = next(tag for tag in tags if stable.match(tag))
    log = ["log", "--no-merges", "--format=%s", latest + "..HEAD"]
    if RELEASE_PATHS:
        log += ["--"] + RELEASE_PATHS
    subjects = [RELEASABLE.match(line) for line in git(*log).splitlines()]
    subjects = [match for match in subjects if match]
    if not subjects:
        return
    major, minor, patch = map(int, stable.match(latest).groups())
    if any(match.group(1) == "feat" or match.group(3) for match in subjects):
        minor, patch = minor + 1, 0
    else:
        patch += 1
    current = latest[len(TAG_PREFIX):]
    version = "%d.%d.%d" % (major, minor, patch)
    if (TAG_PREFIX + version) in tags:
        raise SystemExit("tag for %s already exists" % version)
    for name, pattern, count in VERSION_FILES:
        path = Path(name)
        text = path.read_text(encoding="utf-8")
        found = re.findall(pattern, text, re.M | re.S)
        if len(found) != count or any(item[1] != current for item in found):
            raise SystemExit("%s: expected version %s x%d, found %r" % (name, current, count, found))
        path.write_text(
            re.sub(pattern, lambda m: m.group(1) + version, text, flags=re.M | re.S),
            encoding="utf-8",
        )
    print(TAG_PREFIX + version)


if __name__ == "__main__":
    main()
