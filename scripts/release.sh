#!/bin/sh
# Release chatgpt-use: bump Cargo.toml, test, tag, let release-binaries.yml build and publish the
# GitHub Release, then sync the plugin marketplace so Claude Code plugin installs see the new version
# right away (no token: uses your `gh` login).
#   scripts/release.sh 0.0.6            # real release
#   scripts/release.sh 0.0.6 --dry-run  # preflight + tests + bump diff, then revert
set -eu
V=${1:?usage: scripts/release.sh <version> [--dry-run]}
DRY=${2:-}
REPO=leeguooooo/chatgpt-use
NAME=chatgpt-use
MARKETPLACE=leeguooooo/plugins
cd "$(dirname "$0")/.."

[ "$(git rev-parse --abbrev-ref HEAD)" = main ] || { echo "error: not on main" >&2; exit 1; }
[ -z "$(git status --porcelain)" ] || { echo "error: working tree not clean" >&2; exit 1; }
git pull -q --ff-only
[ "$(git rev-parse HEAD)" = "$(git rev-parse origin/main)" ] || { echo "error: main differs from origin/main" >&2; exit 1; }
git ls-remote --exit-code --tags origin "v$V" >/dev/null && { echo "error: v$V already exists" >&2; exit 1; }

sed -i.bak "1,/^version = /s/^version = \".*\"/version = \"$V\"/" Cargo.toml && rm Cargo.toml.bak
cargo test --quiet   # offline; also refreshes Cargo.lock
git --no-pager diff --stat
if [ "$DRY" = --dry-run ]; then
  git --no-pager diff
  git checkout -q -- Cargo.toml Cargo.lock
  echo "dry run: v$V would be committed, tagged and released; reverted"
  exit 0
fi

git commit -qam "chore: release v$V"
git tag "v$V"
# Push the tag first: the marketplace reads the version from Cargo.toml on main, so main only moves
# once the release binaries exist.
git push -q origin "v$V"
echo "waiting for release-binaries on v$V..."
RUN=
for _ in $(seq 30); do
  RUN=$(gh run list -R "$REPO" -w release-binaries.yml -b "v$V" -e push -L 1 --json databaseId -q '.[0].databaseId')
  [ -n "$RUN" ] && break
  sleep 5
done
[ -n "$RUN" ] || { echo "error: no release-binaries run for v$V; main not pushed" >&2; exit 1; }
gh run watch "$RUN" -R "$REPO" --exit-status >/dev/null || { echo "error: release-binaries run $RUN failed; main not pushed" >&2; exit 1; }
gh release view "v$V" -R "$REPO" --json assets -q '.assets[].name'
git push -q origin main

# Run the marketplace sync now instead of waiting for the hourly cron.
gh workflow run auto-sync-versions.yml -R "$MARKETPLACE"
sleep 5
RUN=$(gh run list -R "$MARKETPLACE" -w auto-sync-versions.yml -e workflow_dispatch -L 1 --json databaseId -q '.[0].databaseId')
gh run watch "$RUN" -R "$MARKETPLACE" --exit-status >/dev/null && echo "marketplace synced" || echo "warn: marketplace sync run $RUN failed; the hourly run will retry"
echo "marketplace $NAME: $(gh api -H 'Accept: application/vnd.github.raw' "repos/$MARKETPLACE/contents/.claude-plugin/marketplace.json" \
  --jq ".plugins[] | select(.name==\"$NAME\").version")"
