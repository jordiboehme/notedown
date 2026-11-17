#!/usr/bin/env bash
# Usage: gh api "repos/$REPO/actions/runs/$RUN_ID/jobs?filter=latest" --paginate \
#          | jobs-passed.sh
#
# The per-leg check of `ci-passed`. Reads the jobs of a run as the API lists
# them (with --paginate one {total_count, jobs} object per page) and judges
# every job that has completed, `ci-passed` itself left out: each must have
# the conclusion success or skipped. Jobs that have not completed yet (the
# release jobs that wait for `ci-passed`) are not judged.
#
# Exit 0 when every completed job passed or was skipped. Exit 1 with an
# ::error:: line naming the others, when no completed job is listed at all
# (an empty answer proves nothing), or when the input is not JSON.
set -euo pipefail

if ! jobs="$(jq -cs '[.[].jobs[]
    | select(.name != "ci-passed" and .status == "completed")
    | {name, conclusion}]')"; then
  echo "::error::the jobs list is not the JSON the API returns" >&2
  exit 1
fi

jq -r '.[] | "  \(.conclusion)  \(.name)"' <<<"$jobs"

if [ "$(jq 'length' <<<"$jobs")" = 0 ]; then
  echo "::error::the jobs list holds no completed job, so it proves nothing" >&2
  exit 1
fi

bad="$(jq -r '.[] | select(.conclusion != "success" and .conclusion != "skipped")
  | "\(.name): \(.conclusion)"' <<<"$jobs")"
if [ -n "$bad" ]; then
  echo "::error::not every job passed: $(tr '\n' ';' <<<"$bad" | sed 's/;$//; s/;/; /g')" >&2
  exit 1
fi
echo "jobs-passed: every completed job passed or was skipped"
