#!/usr/bin/env bash
# Usage: permissions-fit.sh [workflows-dir]
#
# Checks that every reusable workflow call in the repository asks for no more
# than its caller grants. GitHub checks this when a run is loaded: a called
# job that asks for a scope the caller job does not grant fails the whole
# calling run at startup, on every pull request and push, even when the
# calling job would be skipped. So the check runs in workflow-lint, before a
# merge.
#
# For every job in any workflow of the folder (default .github/workflows)
# whose `uses:` is a local workflow (./.github/workflows/<file>):
#
# - the grant is the caller job's `permissions:`, or the caller workflow's
#   top-level `permissions:` when the job has none. No block at all fails:
#   the default token scopes depend on repository settings.
# - the ask is the union of the called workflow's top-level `permissions:`
#   and every job's `permissions:` in it. A called workflow without a
#   top-level block fails for the same reason.
# - every scope of the ask must be granted at the same level or higher
#   (none < read < write). read-all and write-all count as every scope.
#
# release.yml has two rules of its own: at least one workflow calls it, and
# it never asks for `actions` (the ci.yml caller does not grant it).
#
# Needs python3 with PyYAML (the Ubuntu runner image has it), or yq (v4) to
# turn the YAML into JSON. Exit 0 when everything fits, 1 when not, 2 on a
# usage error.
set -euo pipefail

dir="${1:-$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)/.github/workflows}"
[ "$#" -le 1 ] || { echo "usage: $(basename "$0") [workflows-dir]" >&2; exit 2; }
[ -d "$dir" ] || { echo "error: $dir is not a folder" >&2; exit 2; }

if python3 -c 'import yaml' 2>/dev/null; then
  loader=yaml
elif command -v yq >/dev/null 2>&1; then
  loader=yq
else
  echo "::error::permissions-fit.sh needs python3 with PyYAML, or yq, and found neither" >&2
  exit 1
fi

exec python3 - "$dir" "$loader" <<'PY'
import json
import os
import subprocess
import sys

folder, loader = sys.argv[1], sys.argv[2]
LEVELS = {"none": 0, "read": 1, "write": 2}
# Every scope GITHUB_TOKEN knows, for read-all and write-all.
SCOPES = [
    "actions", "attestations", "checks", "contents", "deployments",
    "discussions", "id-token", "issues", "models", "packages", "pages",
    "pull-requests", "repository-projects", "security-events", "statuses",
]


def load(path):
    if loader == "yaml":
        import yaml
        with open(path, encoding="utf-8") as f:
            return yaml.safe_load(f)
    out = subprocess.run(["yq", "-o=json", ".", path], check=True,
                         capture_output=True, text=True).stdout
    return json.loads(out)


def perms(value, where):
    """A permissions block as {scope: level}. None when there is no block."""
    if value is None:
        return None
    if isinstance(value, str):
        if value in ("read-all", "write-all"):
            level = LEVELS[value.split("-")[0]]
            return {s: level for s in SCOPES}
        raise ValueError(f"{where}: unknown permissions value '{value}'")
    if isinstance(value, dict):
        out = {}
        for scope, level in value.items():
            if level not in LEVELS:
                raise ValueError(f"{where}: unknown level '{level}' for {scope}")
            out[str(scope)] = LEVELS[level]
        return out
    raise ValueError(f"{where}: permissions is neither a map nor a string")


def name(level):
    return {v: k for k, v in LEVELS.items()}[level]


errors = []
files = sorted(f for f in os.listdir(folder) if f.endswith((".yml", ".yaml")))
docs = {}
for f in files:
    try:
        docs[f] = load(os.path.join(folder, f)) or {}
    except Exception as e:  # a broken file is actionlint's to report
        errors.append(f"{f}: cannot read it ({e})")


def asks(called):
    doc = docs[called]
    top = perms(doc.get("permissions"), f"{called} (top level)")
    if top is None:
        raise ValueError(f"{called} has no top-level permissions block, so what it asks for depends on repository settings")
    union = dict(top)
    for job, body in (doc.get("jobs") or {}).items():
        p = perms((body or {}).get("permissions"), f"{called} job {job}")
        for scope, level in (p or {}).items():
            union[scope] = max(union.get(scope, 0), level)
    return {s: l for s, l in union.items() if l > 0}


callers = {}
for f in files:
    doc = docs.get(f)
    if doc is None:
        continue
    for job, body in (doc.get("jobs") or {}).items():
        uses = (body or {}).get("uses")
        if not isinstance(uses, str) or not uses.startswith("./.github/workflows/"):
            continue
        called = uses[len("./.github/workflows/"):].split("@")[0]
        callers.setdefault(called, []).append(f"{f} job {job}")
        if called not in docs:
            errors.append(f"{f} job {job} calls {uses}, which is not in {folder}")
            continue
        try:
            ask = asks(called)
            grant = perms(body.get("permissions"), f"{f} job {job}")
            if grant is None:
                grant = perms(doc.get("permissions"), f"{f} (top level)")
            if grant is None:
                errors.append(f"{f} job {job} calls {called} without a permissions block, here or at the top level")
                continue
        except ValueError as e:
            errors.append(str(e))
            continue
        short = False
        for scope, level in sorted(ask.items()):
            have = grant.get(scope, 0)
            if have < level:
                short = True
                errors.append(f"{called} asks for {scope}: {name(level)}, but {f} job {job} grants {name(have)}")
        if not short:
            shown = ", ".join(f"{s}: {name(l)}" for s, l in sorted(ask.items()))
            print(f"ok   {f} job {job} -> {called} ({shown})")

if "release.yml" in docs:
    if not callers.get("release.yml"):
        errors.append("no workflow calls release.yml, so its permissions are checked against nothing")
    try:
        if asks("release.yml").get("actions", 0) > 0:
            errors.append("release.yml asks for actions, which the ci.yml caller never grants")
    except ValueError as e:
        errors.append(str(e))

for e in dict.fromkeys(errors):
    print(f"::error::{e}")
sys.exit(1 if errors else 0)
PY
