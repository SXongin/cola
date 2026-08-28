#!/bin/sh
# Validate the commit message subject follows Conventional Commits.
# Hooked up via lefthook: scripts/check-commit-msg.sh {1}
set -eu

msg_file="${1:?usage: check-commit-msg.sh <commit-msg-file>}"

first_line="$(grep -v '^#' "$msg_file" | grep -v '^[[:space:]]*$' | head -n 1)"

# Tolerate git-generated merge/revert messages.
case "$first_line" in
  Merge*) exit 0 ;;
  Revert*) exit 0 ;;
esac

if ! printf '%s\n' "$first_line" | grep -Eq '^(feat|fix|docs|style|refactor|test|chore|ci|build|perf|revert)(\([a-z0-9_-]+\))?!?: .+'; then
  echo "error: commit subject must follow Conventional Commits" >&2
  echo "  <type>(<scope>)?: <subject>" >&2
  echo "  types: feat, fix, docs, style, refactor, test, chore, ci, build, perf, revert" >&2
  echo "  got: $first_line" >&2
  exit 1
fi

len=$(printf '%s' "$first_line" | wc -m)
if [ "$len" -gt 72 ]; then
  echo "error: commit subject is $len chars (max 72)" >&2
  echo "  $first_line" >&2
  exit 1
fi

exit 0