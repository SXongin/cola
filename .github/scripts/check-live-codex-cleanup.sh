#!/usr/bin/env bash
# Re-check the live Codex suites' own cleanup marker after the test process has
# exited (#953). Both tiers use this script so the marker parsing and the
# process-scan fallback cannot drift apart.
#
# Usage: check-live-codex-cleanup.sh tier-1|tier-2
#
# The marker the suite writes is authoritative: it records the pids and runtime
# paths the test owned. The argv scan after it is only a backstop, and it is
# narrowed to the suite's own children (`app-server proxy` and
# `app-server --listen`) because the random temp prefix also matched unrelated
# processes.

set -euo pipefail

tier="${1:-}"
case "$tier" in
  tier-1 | tier-2) ;;
  *)
    echo "usage: check-live-codex-cleanup.sh tier-1|tier-2" >&2
    exit 2
    ;;
esac

marker="${COLA_LIVE_CODEX_PID_FILE:?COLA_LIVE_CODEX_PID_FILE is not set}"
if [ "${RUNNER_OS:-}" = "Windows" ]; then
  # The test writes a native path; Git Bash needs its own form.
  marker=$(cygpath -u "$marker" 2>/dev/null || echo "$marker")
fi

# A helper or proxy can outlive the test by a moment while it reaps its own
# children, and Windows can hold a just-removed handle briefly. Wait for the
# process table and filesystem to settle before failing: ten seconds is far
# longer than any observed shutdown and turns a transient race into a pass,
# while a genuinely leaked process or path still fails.
SETTLE_ATTEMPTS=100
SETTLE_SLEEP=0.1

pid_alive() {
  local pid="$1"
  if [ "${RUNNER_OS:-}" = "Windows" ]; then
    [ "$(powershell -NoProfile -Command \
      "if (Get-Process -Id $pid -ErrorAction SilentlyContinue) { 'alive' }")" = "alive" ]
  else
    kill -0 "$pid" 2>/dev/null
  fi
}

path_exists() {
  local path="$1"
  if [ "${RUNNER_OS:-}" = "Windows" ]; then
    path=$(cygpath -u "$path" 2>/dev/null || echo "$path")
  fi
  [ -e "$path" ]
}

if [ -f "$marker" ]; then
  while IFS= read -r line; do
    case "$line" in
      pid:*)
        pid="${line#pid:}"
        alive=1
        for ((attempt = 0; attempt < SETTLE_ATTEMPTS; attempt++)); do
          if ! pid_alive "$pid"; then
            alive=0
            break
          fi
          sleep "$SETTLE_SLEEP"
        done
        if [ "$alive" -eq 1 ]; then
          echo "$tier left pid $pid alive" >&2
          exit 1
        fi
        ;;
      path:*)
        path="${line#path:}"
        present=1
        for ((attempt = 0; attempt < SETTLE_ATTEMPTS; attempt++)); do
          if ! path_exists "$path"; then
            present=0
            break
          fi
          sleep "$SETTLE_SLEEP"
        done
        if [ "$present" -eq 1 ]; then
          echo "$tier left runtime path $path behind" >&2
          exit 1
        fi
        ;;
    esac
  done < "$marker"
fi

if [ "${RUNNER_OS:-}" = "Windows" ]; then
  if tasklist | grep -qi 'codex'; then
    tasklist | grep -i 'codex'
    echo "the $tier suite left a live codex process behind" >&2
    exit 1
  fi
else
  leftovers=""
  for pattern in 'app-server proxy' 'app-server --listen'; do
    # `pgrep` exits 1 when nothing matches; under `set -e` that would end the
    # step before the emptiness check. Drop this step's own shell as well.
    found=$(pgrep -fl "$pattern" | awk -v self="$$" '$1 != self' || true)
    if [ -n "$found" ]; then
      leftovers="$leftovers$found"$'\n'
    fi
  done
  if [ -n "$leftovers" ]; then
    printf '%s' "$leftovers"
    echo "the $tier suite left a live process behind" >&2
    exit 1
  fi
fi
