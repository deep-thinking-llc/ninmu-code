#!/usr/bin/env bash
set -euo pipefail

# Coarse local memory smoke check for pure CLI mode. This intentionally avoids
# --tui so regressions from rich UI initialization show up in peak RSS.
#
# To print measured maximum RSS for the built binary:
#   NINMU_MEMORY_REPORT=1 rust/scripts/memory_smoke_cli.sh
#
# To fail when any measured run exceeds a byte budget:
#   NINMU_MEMORY_MAX_RSS_BYTES=25000000 rust/scripts/memory_smoke_cli.sh
#
# macOS reports bytes via /usr/bin/time -l; Linux reports KB via -v.

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WORKDIR="$(mktemp -d "${TMPDIR:-/tmp}/ninmu-memory-smoke.XXXXXX")"
CONFIG_HOME="$WORKDIR/home/.ninmu"

cleanup() {
  rm -rf "$WORKDIR"
}
trap cleanup EXIT

mkdir -p "$CONFIG_HOME"

cd "$ROOT"

export NINMU_CONFIG_HOME="$CONFIG_HOME"
export NINMU_CONFIG_DIR="$WORKDIR/.ninmu"
export NINMU_TEST_PANIC_ON_TUI_INIT=1

cargo build -q -p ninmu-cli

NINMU_BIN="$ROOT/target/debug/ninmu"
MAX_RSS_BYTES=0

measure_ninmu() {
  if [[ "${NINMU_MEMORY_REPORT:-}" != "1" && -z "${NINMU_MEMORY_MAX_RSS_BYTES:-}" ]]; then
    "$NINMU_BIN" "$@" >/dev/null
    return
  fi

  local log="$WORKDIR/time.log"
  local rss_bytes=0
  if /usr/bin/time -l true >/dev/null 2>"$WORKDIR/time-probe.log"; then
    /usr/bin/time -l "$NINMU_BIN" "$@" >/dev/null 2>"$log"
    rss_bytes="$(awk '/maximum resident set size/ {print $1}' "$log")"
  else
    /usr/bin/time -v "$NINMU_BIN" "$@" >/dev/null 2>"$log"
    local rss_kb
    rss_kb="$(awk -F: '/Maximum resident set size/ {gsub(/^ +/, "", $2); print $2}' "$log")"
    rss_bytes="$((rss_kb * 1024))"
  fi

  if [[ "$rss_bytes" -gt "$MAX_RSS_BYTES" ]]; then
    MAX_RSS_BYTES="$rss_bytes"
  fi

  if [[ "${NINMU_MEMORY_REPORT:-}" == "1" ]]; then
    printf 'max_rss_bytes=%s\n' "$rss_bytes"
  fi
}

measure_ninmu --output-format json status
measure_ninmu --model ollama/llama3.1:8b status

if [[ -n "${NINMU_MEMORY_MAX_RSS_BYTES:-}" && "$MAX_RSS_BYTES" -gt "$NINMU_MEMORY_MAX_RSS_BYTES" ]]; then
  printf 'pure CLI max RSS exceeded budget: observed=%s budget=%s\n' \
    "$MAX_RSS_BYTES" "$NINMU_MEMORY_MAX_RSS_BYTES" >&2
  exit 1
fi

printf 'pure CLI memory smoke completed without TUI initialization\n'
