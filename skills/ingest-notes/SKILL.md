---
name: ingest-notes
description: Use when asked to ingest, import or sync handwritten Supernote notes from notedown into Crystalline, or to bring Crystalline up to date with new or changed notes. Reads the changes of the notedown MCP server and distills them into engrams with sub-agents.
---

# Ingest notes from notedown into Crystalline

The notedown MCP server (`notedown mcp`, tools `mcp__notedown__*` when it was added as
`notedown`) gives the text of handwritten notes and what changed since the last run. This skill
turns those changes into Crystalline engrams.

You are the main agent. You plan, dispatch and confirm. You never load note text yourself.
Sub-agents read the text and write the engrams.

The consumer name is always `crystalline`.

## Main agent

1. **Plan.** Call `status`. Its `account` is the account slug. Then call `get_changes` with
   `{"consumer": "crystalline", "summary": true}`. This is the manifest: `from`, `upTo`, `more`
   and one row per changed note (key, path, folder, change, pages, blocks, pending, failed,
   chars). No text. A row with 0 pages that is `pending: true` or `failed: true` has nothing to
   ingest: it is listed only because pages wait for OCR or could not be read. Leave such rows
   out. Keep a row with 0 pages and no such flag, for example a `new` row with `oldNote`: its
   engrams still have to move to the new key. and keep the notes with `failed: true` for the run
   report. When no other row is left and `from` equals `upTo`, stop and say that nothing
   changed (name the unreadable notes, if any). When no other row is left but `from` is lower
   than `upTo`, go to step 4: the range holds lines that change no note text, and the ack
   moves the bookmark past them.
2. **Group.** Put the notes of one folder into one group. Split a folder when its notes add up
   to more than about 30,000 characters (the `chars` column). Keep related notes together: one
   sub-agent then decides about shared engrams, and two sub-agents never create the same engram
   twice. The rows left out in step 1 get no sub-agent: their text comes in a later run.
3. **Dispatch.** Start one sub-agent per group, at most 3 at the same time. Give each one:
   the account slug, the note keys of its group, the fixed range `from` and `upTo` of the
   manifest, and the sub-agent instructions below. Never give a sub-agent the consumer name:
   every sub-agent must see the same range as the manifest.
4. **Confirm.** Only when every sub-agent reported success, call `ack` with
   `{"consumer": "crystalline", "upTo": <upTo>}`. If one sub-agent failed, do not call `ack`.
   The next run then repeats the whole range. This is safe, because ingestion is idempotent by
   anchor.
5. **Repeat** from step 1 while `more` was true. Then give Jordi one short run report: notes
   and engrams per group, all open questions of the sub-agents, and the unreadable pages (the
   notes with `failed: true`, and the pages the sub-agents reported). An unreadable page stays
   in every run until it is written again on the device.

## Sub-agent instructions

Pass this section and the next one to every sub-agent, with its account slug, note keys,
`from` and `upTo`. `scripts/ingest-notes.sh` reads both sections from this file.

1. Fetch your delta: `get_changes` with `{"notes": [<keys>], "from": <from>, "upTo": <upTo>}`.
   Do not use `consumer` or `since`. Do not call `ack`.
2. Find the target domain: `list_domains` with `include_routing: true`, and follow the routing.
   When you were given a domain, use that one.
3. For each note, distill its text into engrams. One engram per topic, not one per block.
   - Put the anchors of the blocks you used into the metadata key `sources` (a list), for
     example `notedown://<account>/F2024.../P2024...#b3`.
   - Put the note address `notedown://<account>/<key>` into the metadata key `resource`.
4. Search before you write. A block that is already cited is updated, never ingested twice.
   - With Crystalline 0.24.3 or later: `search_engrams` with
     `metadata_filters: {"sources": {"$contains": "<anchor>"}}`.
   - Before 0.24.3: `search_engrams` with `metadata_filters: {"resource": "notedown://<account>/<key>"}`,
     then read the hits and check their `sources`.
   - When you cannot tell the version, or the `$contains` filter gives an error, use the
     `resource` search. An empty `$contains` result on an older server means nothing.
5. Check the `change` of each note first. On `unscoped`, stop there for that note and retire
   nothing: the note only left the ingest scope. Then handle each entry of `whatChanged`:
   - `edited`: update the engrams that cite the anchor to the new text.
   - `replaced`: update the engrams that cite the anchor, and move the anchor in their `sources`
     to the anchors in `replacedBy`. Do not retire them.
   - `deleted`: a deletion alone is never a reason to retire. Remove the anchor from `sources`.
     Retire only when no source is left and `search_notes` finds the text in no other note.
     When unsure, keep the engram and list it as an open question.
   - A new block has no entry. It is in `textNow`.
   - A page marked `unreadable` could not be read by OCR. Ingest nothing from the device text and
     nothing for the unread new ink. The `textNow` and `whatChanged` shown on that page are valid:
     handle them as usual. Retire nothing because of the unreadable ink. Report the unreadable
     page in the run report (note and page).
   - To change `sources` of an engram: `edit_engram` with `set_frontmatter` may not accept the
     key `sources`. Then rewrite the engram with `write_engram` and `overwrite: true` (same title
     and folder, full metadata), or use `edit_engram` `find_replace` on its frontmatter.
6. Report back in a few lines of plain text, no formatting: engrams created, updated and
   retired, and open questions (an illegible word, a removal you were not sure about). End with
   one line `RESULT: ok`, or `RESULT: failed` and the reason.

## The format of a delta

`get_changes` gives a TOON head and a Markdown body.

- The head: `range` (the range in words), `from`, `upTo`, `more`, `legend`, a table
  `notes[N]{key,title,path,change,pending,oldNote,oldPath,keywords,links}:` and a table
  `blocks[N]{note,page,block,change,replacedBy}:` with one row per changed block. A table row
  holds one record, its cells split by commas.
- The `note` column of the blocks table and the body headings name a note by its folder and
  title, for example `NOTE/Note/Planview`.
- The body: one `## <folder>/<title>, page <n> (<change>)` section per page. `### textNow` holds
  the full page text now, each block after an HTML comment with its anchor. Then one section
  per changed block, named with its block id: `### wordDiff #b3`, `### textBefore #b5`, and
  `### textNow #b4` when more than half of the words changed.
- A page whose heading ends in `unreadable` could not be read by OCR. Its `### unreadable`
  section holds the device's own handwriting recognition, or `no device text`. That text is
  only a hint for your report, never a source for engrams.
- A note with `oldNote` got a new key by a move: search by the old key, then change `resource`
  and the anchors in `sources` of those engrams to the new key. A note with only `oldPath` keeps
  its key and its anchors.
- The legend: `textBefore` is the text when you last confirmed (or at your `since`).
  `wordDiff`: `[-...-]` removed, `[+...+]` added.
- `### beforeUnknown` (without a block id) means the old versions of a page were pruned, so the
  text before is unknown. Its row in the blocks table names the page anchor
  (`notedown://<account>/<key>/<pageId>`), not a block. Search by `resource` (the note address
  `notedown://<account>/<key>`), then compare the block anchors in the `sources` of the hits
  with the anchors in `textNow`: update the engrams that cite them, and treat only the blocks
  no hit cites as new. Never treat the page's blocks as new only because a `$contains` search
  found nothing.
- `get_block` with an anchor gives the current text of one block, or what replaced it.
