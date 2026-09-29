#!/bin/sh
# Fail if a Linux binary needs a newer glibc than MAX (default 2.17).
#   scripts/check-glibc-floor.sh target/x86_64-unknown-linux-gnu/release/chatgpt-use [2.17]
# A binary linked against the build host's glibc refuses to start on any older
# one ("version `GLIBC_2.39' not found"), even for symbols it only uses weakly.
# readelf is architecture-independent, so this also checks cross-built aarch64.
set -eu
BIN=${1:?usage: check-glibc-floor.sh <binary> [max-glibc]}
MAX=${2:-2.17}

need="$(readelf -V --wide "$BIN" | grep -o 'GLIBC_[0-9][0-9.]*' | sed 's/GLIBC_//' | sort -uV | tail -n1)"
[ -n "$need" ] || { echo "error: no GLIBC version references in $BIN" >&2; exit 1; }
top="$(printf '%s\n%s\n' "$need" "$MAX" | sort -V | tail -n1)"
if [ "$top" != "$MAX" ]; then
  echo "error: $BIN needs glibc $need, above the $MAX floor" >&2
  exit 1
fi
echo "ok: $BIN needs glibc $need (floor $MAX)"
