#!/usr/bin/env bash
# Wait out any in-flight OpenCode Review run on a PR's branch.
#
# The trial posts as the same `github-actions[bot]` identity as the gate, and
# the gate approves by reading back the *last* bot comment. A trial comment
# landing between the gate's review post and its verdict readback would hide
# the verdict and silently skip the approval, so the trial must not post while
# a gate run is in flight. A run that could be in that gap is still
# `in_progress` in the runs API and therefore visible here.
#
# Usage: wait-for-gate.sh <pull request number>
set -euo pipefail

pr=$1
head_ref=$(gh pr view "$pr" --json headRefName --jq .headRefName)

for _ in $(seq 1 40); do
  running=$(gh run list --repo "$GITHUB_REPOSITORY" \
    --workflow opencode-review.yml --branch "$head_ref" --limit 10 \
    --json status \
    --jq '[.[] | select(.status == "queued" or .status == "in_progress")] | length')
  if [ "$running" = "0" ]; then
    echo "No OpenCode Review run in flight on $head_ref."
    exit 0
  fi
  echo "OpenCode Review still in flight; waiting 15s..."
  sleep 15
done

echo "OpenCode Review still in flight after 10 minutes; refusing to post." >&2
exit 1
