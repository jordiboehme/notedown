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
# `notedown pipeline ack` only when every run reported success. Repeat while the manifest says
# `more`. One failed run stops the script without an ack; the next run repeats the whole range,
# which is safe because ingestion is idempotent by anchor. Questions of the runs go to the log.
#
# The groups run one after the other: slower than parallel sub-agents, but easy to follow in
# the log.
#
# Settings (environment variables, with defaults):
#   NOTEDOWN_DATA        data folder                        ~/notedown-data
#   NOTEDOWN_ACCOUNT     account slug; empty when the data folder has only one account
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

account_args=()
if [[ -n "$account" ]]; then
    account_args=(--account "$account")
fi

# The notedown MCP server for the runs. The other MCP servers of Claude Code (Crystalline)
# come from its own configuration.
mcp_config="$(jq -cn --arg cmd "$notedown" --arg data "$data" --arg account "$account" '
    {mcpServers: {notedown: {
        command: $cmd,
        args: (["mcp", "--data", $data] + (if $account == "" then [] else ["--account", $account] end))
    }}}')"

# The runs may read notes and use Crystalline. They cannot ack: only this script confirms.
allowed_tools="mcp__notedown__status mcp__notedown__get_changes mcp__notedown__get_block"
allowed_tools+=" mcp__notedown__get_note mcp__notedown__search_notes mcp__notedown__list_notes"
allowed_tools+=" mcp__${crystalline}"

if [[ -n "$domain" ]]; then
    domain_rule="Write the engrams into the Crystalline domain \"$domain\"."
else
    domain_rule="Find the target domain with list_domains (include_routing: true) and follow its routing."
fi

# The sub-agent instructions of skills/ingest-notes/SKILL.md, with the keys and the range.
prompt_for() {
    local keys_json="$1" from="$2" up_to="$3"
    cat <<EOF
You ingest handwritten notes from notedown into Crystalline. Work alone, nobody answers questions.

1. Fetch your delta: call the notedown tool get_changes with {"notes": $keys_json, "from": $from, "upTo": $up_to}. Use no consumer and no since. Never call ack.
2. $domain_rule
3. For each note, distill its text into engrams, one engram per topic. Put the anchors of the blocks you used into the metadata key "sources" (a list, for example notedown://jordi/F2024.../P2024...#b3). Put the note address notedown://<account>/<key> into the metadata key "resource".
4. Search before you write. A block that is already cited is updated, never ingested twice. With Crystalline 0.24.3 or later: search_engrams with metadata_filters {"sources": {"\$contains": "<anchor>"}}. Before 0.24.3, or when you cannot tell the version, or when \$contains gives an error: search_engrams with metadata_filters {"resource": "notedown://<account>/<key>"}, then read the hits and check their sources.
5. For each entry of whatChanged: edited: update the engrams that cite the anchor. replaced: update them and move the anchor in their sources to the anchors in replacedBy, do not retire them. deleted: check before you retire, because tidying up the device does not make the knowledge wrong; remove the anchor from sources and retire only when the knowledge is gone. A note with change unscoped only left the scope: never retire anything for it. A new block has no entry, it is in textNow. A note with oldNote got a new key: change resource and the anchors in sources to the new key. To change sources: set_frontmatter may not take the key sources; then rewrite the engram with write_engram and overwrite true (same title and folder, full metadata), or use edit_engram find_replace on its frontmatter.
6. The delta has a TOON head (range, more, legend, tables notes[N]{...} and blocks[N]{...}, one record per row) and a Markdown body: per page "## <title>, page <n> (<change>)", then "### textNow" with each block after an HTML comment with its anchor, then "### wordDiff #b3" ([-...-] removed, [+...+] added), "### textBefore #b5" (the text at the start of the range) for the changed blocks.
7. Report in a few lines: engrams created, updated and retired, and open questions (an illegible word, a removal you were not sure about). The last line is exactly "RESULT: ok", or "RESULT: failed" and the reason.
EOF
}

# Runs one group. Returns 0 only when claude exits 0, reports no error, and the answer ends
# with "RESULT: ok".
run_group() {
    local keys_json="$1" from="$2" up_to="$3" out result
    if ! out="$("$claude" -p "$(prompt_for "$keys_json" "$from" "$up_to")" \
        --output-format json \
        --mcp-config "$mcp_config" \
        --allowedTools "$allowed_tools" \
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
    if [[ "$(printf '%s\n' "$result" | grep -v '^[[:space:]]*$' | tail -n 1)" != "RESULT: ok" ]]; then
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
