#!/usr/bin/env bash
# Ingests the changed notes of notedown into Crystalline without a main agent (spec 6.2).
# Start it by hand or from launchd, for example every night:
#
#   scripts/ingest-notes.sh
#   NOTEDOWN_DATA=/srv/notedown NOTEDOWN_ACCOUNT=jordi scripts/ingest-notes.sh
#
# The flow of skills/ingest-notes/SKILL.md as code: read the manifest of consumer
# "crystalline", group the notes by folder (a group stays below about 30,000 characters), run
# one `claude -p` per group with the fixed range from/upTo, and confirm with
# `notedown pipeline ack` only when every run reported success. A manifest without notes is
# confirmed too when its range is not empty, so the bookmark moves past lines that change no
# note text. Each run gets the sections "Sub-agent instructions" and "The format of a delta" of
# the skill file, plus the account, the note keys, the range and the domain. Repeat while the manifest says
# `more`. One failed run stops the script without an ack; the next run repeats the whole range,
# which is safe because ingestion is idempotent by anchor. Questions of the runs go to the log.
#
# The groups run one after the other: slower than parallel sub-agents, but easy to follow in
# the log.
#
# Settings (environment variables, with defaults):
#   NOTEDOWN_DATA        data folder                        ~/notedown-data
#   NOTEDOWN_ACCOUNT     account slug; empty: the only account of the data folder
#   NOTEDOWN_CONSUMER    bookmark name                      crystalline
#   NOTEDOWN_INGEST_LOG  run log                            ~/Library/Logs/notedown-ingest.log
#   NOTEDOWN_BIN         the notedown command               notedown
#   CLAUDE_BIN           the claude command                 claude
#   CRYSTALLINE_MCP      the name of the Crystalline MCP server in Claude Code, as its tools
#                        are named (mcp__<name>__search_engrams). A claude.ai connector is
#                        named claude_ai_Crystalline, a server added with
#                        `claude mcp add crystalline ...` is named crystalline.  crystalline
#   CRYSTALLINE_DOMAIN   the domain for the engrams; empty: the routing of list_domains decides
#   GROUP_CHARS          the size limit of a group in characters  30000
#   MAX_ROUNDS           stop after this many manifests (a guard against a loop)  50
#   INGEST_SKILL         the skill file the run instructions come from
#                        <this repo>/skills/ingest-notes/SKILL.md
#
# Needs jq to read the JSON of `notedown pipeline changes`. launchd starts with a short PATH,
# so set PATH in the plist or give full paths in NOTEDOWN_BIN and CLAUDE_BIN.
set -euo pipefail

data="${NOTEDOWN_DATA:-$HOME/notedown-data}"
account="${NOTEDOWN_ACCOUNT:-}"
consumer="${NOTEDOWN_CONSUMER:-crystalline}"
log="${NOTEDOWN_INGEST_LOG:-$HOME/Library/Logs/notedown-ingest.log}"
notedown="${NOTEDOWN_BIN:-notedown}"
claude="${CLAUDE_BIN:-claude}"
crystalline="${CRYSTALLINE_MCP:-crystalline}"
domain="${CRYSTALLINE_DOMAIN:-}"
group_chars="${GROUP_CHARS:-30000}"
max_rounds="${MAX_ROUNDS:-50}"
skill="${INGEST_SKILL:-$(cd "$(dirname "$0")/.." && pwd)/skills/ingest-notes/SKILL.md}"
server="notedown"

mkdir -p "$(dirname "$log")"

say() {
    printf '%s %s\n' "$(date '+%Y-%m-%d %H:%M:%S')" "$*" >>"$log"
}

trap 'say "stop: error at line $LINENO"' ERR

need() {
    if ! command -v "$1" >/dev/null 2>&1; then
        say "error: $1 not found (PATH=$PATH)"
        echo "ingest-notes: $1 not found, see $log" >&2
        exit 2
    fi
}

need "$notedown"
need "$claude"
need jq

fail() {
    say "error: $*"
    echo "ingest-notes: $*" >&2
    exit 2
}

# The run instructions: the skill from "## Sub-agent instructions" to its end, without the
# paragraph for the main agent.
if [[ ! -f "$skill" ]]; then
    fail "the skill file $skill is missing (set INGEST_SKILL)"
fi
instructions="$(awk '
    /^## Sub-agent instructions/ { on = 1; skip = 1; print; print ""; next }
    on && skip && /^1\. / { skip = 0 }
    on && !skip' "$skill")"
if [[ "$instructions" != *"## The format of a delta"* ]]; then
    fail "no sections \"Sub-agent instructions\" and \"The format of a delta\" in $skill"
fi

# The account slug: the runs need it for the note addresses notedown://<account>/<key>.
if [[ -z "$account" ]]; then
    slugs="$("$notedown" pipeline consumers --data "$data" 2>>"$log" | jq -r 'keys[]')"
    if [[ "$(grep -c . <<<"$slugs")" != "1" ]]; then
        fail "set NOTEDOWN_ACCOUNT, the data folder has these accounts: $(tr '\n' ' ' <<<"$slugs")"
    fi
    account="$slugs"
fi
account_args=(--account "$account")

# The notedown MCP server for the runs. The other MCP servers of Claude Code (Crystalline)
# come from its own configuration.
mcp_config="$(jq -cn --arg name "$server" --arg cmd "$notedown" --arg data "$data" \
    --arg account "$account" '
    {mcpServers: {($name): {command: $cmd, args: ["mcp", "--data", $data, "--account", $account]}}}')"

# The runs may read notes and use Crystalline. They cannot ack: only this script confirms.
allowed_tools="mcp__${server}__status mcp__${server}__get_changes mcp__${server}__get_block"
allowed_tools+=" mcp__${server}__get_note mcp__${server}__search_notes mcp__${server}__list_notes"
allowed_tools+=" mcp__${crystalline}"
disallowed_tools="mcp__${server}__ack"

if [[ -n "$domain" ]]; then
    domain_rule="Domain: write the engrams into the Crystalline domain \"$domain\"."
else
    domain_rule="Domain: none given, follow the routing of list_domains (include_routing: true)."
fi

# One run's prompt: the skill sections with the account, the keys, the range and the domain.
prompt_for() {
    local keys_json="$1" from="$2" up_to="$3"
    cat <<EOF
You are a sub-agent of the ingest-notes skill: you ingest handwritten notes from notedown into
Crystalline. Work alone, nobody answers questions. Write your report as plain text, no formatting.

Account: $account
Note keys: $keys_json
Range: from $from, upTo $up_to
$domain_rule
The notedown tools are named mcp__${server}__<tool>. Never call ack.

$instructions
EOF
}

# Runs one group. Returns 0 only when claude exits 0, reports no error, and the answer ends
# with "RESULT: ok".
run_group() {
    local keys_json="$1" from="$2" up_to="$3" out result last
    if ! out="$("$claude" -p "$(prompt_for "$keys_json" "$from" "$up_to")" \
        --output-format json \
        --mcp-config "$mcp_config" \
        --allowedTools "$allowed_tools" \
        --disallowedTools "$disallowed_tools" \
        --permission-mode dontAsk </dev/null 2>>"$log")"; then
        say "run failed: claude exited with an error"
        return 1
    fi
    if [[ "$(jq -r '.is_error // false' <<<"$out")" != "false" ]]; then
        say "run failed: $(jq -r '.result // .subtype // "no result"' <<<"$out")"
        return 1
    fi
    result="$(jq -r '.result // ""' <<<"$out")"
    printf '%s\n' "$result" | sed 's/^/    /' >>"$log"
    last="$(printf '%s\n' "$result" | tr -d '*`' | grep -v '^[[:space:]]*$' | tail -n 1 | sed 's/[[:space:]]*$//')"
    if [[ "$last" != "RESULT: ok" ]]; then
        say "run failed: no RESULT: ok"
        return 1
    fi
}

say "start: data $data, consumer $consumer"
round=0
while true; do
    round=$((round + 1))
    if ((round > max_rounds)); then
        say "stop: more than $max_rounds rounds"
        exit 1
    fi
    manifest="$("$notedown" pipeline changes --data "$data" ${account_args[@]+"${account_args[@]}"} \
        --consumer "$consumer" --summary 2>>"$log")"
    from="$(jq -r '.from' <<<"$manifest")"
    up_to="$(jq -r '.upTo' <<<"$manifest")"
    more="$(jq -r '.more' <<<"$manifest")"
    count="$(jq -r '.notes | length' <<<"$manifest")"
    if ((count == 0)) && [[ "$from" == "$up_to" ]]; then
        say "done: nothing changed"
        break
    fi
    say "range $from to $up_to: $count notes, more: $more"

    # Groups by folder, split by size. A note without text yet (pending, 0 pages) waits.
    groups="$(jq -c --argjson max "$group_chars" '
        [.notes[] | select((.pending and .pages == 0) | not)]
        | group_by(.folder)
        | map(reduce .[] as $n ([];
            if length > 0 and (.[length - 1].chars + $n.chars) <= $max
            then .[length - 1].keys += [$n.key] | .[length - 1].chars += $n.chars
            else . + [{folder: $n.folder, keys: [$n.key], chars: $n.chars}]
            end))
        | add // []
        | .[]' <<<"$manifest")"

    while IFS= read -r group; do
        [[ -z "$group" ]] && continue
        folder="$(jq -r '.folder' <<<"$group")"
        keys="$(jq -c '.keys' <<<"$group")"
        say "group $folder: $(jq -r '.keys | length' <<<"$group") notes, $(jq -r '.chars' <<<"$group") chars"
        if ! run_group "$keys" "$from" "$up_to"; then
            say "stop: group $folder failed, no ack for $from to $up_to"
            exit 1
        fi
    done <<<"$groups"

    ack="$("$notedown" pipeline ack --data "$data" ${account_args[@]+"${account_args[@]}"} \
        --consumer "$consumer" --up-to "$up_to" 2>>"$log")"
    say "ack: $(jq -c . <<<"$ack")"
    if [[ "$more" != "true" ]]; then
        say "done"
        break
    fi
done
