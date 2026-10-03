#!/bin/sh
# Run the page probes in src/channel.rs against DOM fixtures of each known
# ChatGPT renderer (scripts/dom-fixtures.mjs). Offline: jsdom, never chatgpt.com.
# Needs node + npm; jsdom is installed once under target/dom-check.
set -eu
cd "$(dirname "$0")/.."
cargo test --quiet dump_probe_js -- --ignored >/dev/null
DEPS=target/dom-check
if [ ! -d "$DEPS/node_modules/jsdom" ]; then
  mkdir -p "$DEPS"
  npm install --silent --no-audit --no-fund --prefix "$DEPS" jsdom@26 >/dev/null
fi
cp scripts/dom-fixtures.mjs "$DEPS/dom-fixtures.mjs"
node "$DEPS/dom-fixtures.mjs" target/probe-js
