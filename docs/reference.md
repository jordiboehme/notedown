# Reference

This page has the details that the [README](../README.md) leaves out: the install from source, every
option of `convert` and `render`, when Notedown skips or overwrites a file, the output format and the
Private Cloud in short. For the setup step by step, see the [getting started guide](getting-started.md).

## Install

There is no release yet. Build it from source with the Rust toolchain that `rust-toolchain.toml`
names (rustup picks it up by itself):

```sh
git clone https://github.com/jordiboehme/notedown.git
cd notedown
cargo install --path crates/notedown
```

Homebrew comes with the first release.

For a sync server without the OCR part (a NAS or a Raspberry Pi), add `--no-default-features`. See
[the server on another machine](getting-started.md#the-server-on-another-machine).

On macOS the model runs on the GPU with Metal automatically. On Linux the build needs `cmake` and a C++
compiler, and inference runs on the CPU.

Then download the OCR model once (about 3.4 GB, checked against pinned SHA-256 sums):

```sh
notedown model pull
```

The files go to `~/Library/Caches/notedown/models` on macOS and `$XDG_CACHE_HOME/notedown/models` on
Linux. Set `NOTEDOWN_MODEL_DIR` to use another folder. `notedown model path` shows the paths and
`notedown model verify` checks the files.

A pull that stops (you end it, or the connection breaks) keeps what it has. The next
`notedown model pull` continues from there. Run only one pull at a time.

`notedown model pull --json` is for apps. It prints one JSON line per progress step on stdout:
`{"file": "<name>", "done": <bytes>, "total": <bytes>}`. The last line is `{"ready": "<path>"}`.
An error is plain text on stderr, and the exit code is not 0.

## Convert and render

```sh
# one file, the Markdown file is written next to it
notedown convert "Test 2.note"

# a whole folder, searched recursively, written under another folder
notedown convert ~/Supernote/Note --out ~/notes-md

# also save the page images and link them in the Markdown
notedown convert "Test 2.note" --images

# also write <name>.debug.json with the raw OCR output and metadata
notedown convert "Test 2.note" --debug

# only render the pages as PNG images (no OCR)
notedown render "Test 2.note" --out pages
```

`render` writes `<name>-page-001.png`, `<name>-page-002.png` and so on. The template (ruled lines or
grid) is left out because it confuses OCR; add `--keep-template` to keep it, in `render` and in
`convert`.

`render` takes more options:

- `--page N` renders only page N (1-based). Repeat it for more pages. Each path is printed on its own
  line as soon as the PNG is written. A page that fails (for example a number past the end) prints
  `error: page N: <reason>` on stderr and the run goes on with the next page. The exit code is 1 at
  the end if any page failed. `--page` cannot be combined with `--pdf`.
- `--allow-unknown-version` opens a note of a newer format version, as `convert` does.
- The file name does not need the `.note` extension, because the format is read from the file itself.

Other `convert` options: `--force`, `--model GGUF`, `--mmproj GGUF`, `--max-tokens N` (default 4096),
`--threads N` and `--allow-unknown-version`.

### Skipping and overwriting

A file is skipped when its `.md` already exists and the front matter has the same source checksum, the
same model and the same Notedown major version. The model is not even loaded when every file is
skipped.

A `.md` file from an older Notedown has a front matter without `type`. When its source checksum, model
and major version still match, Notedown only replaces the front matter with the new one and keeps the
text below it byte for byte. No OCR runs for this and the model isn't loaded. The run prints "updated
the header of ..." for each such file. `generated` keeps the old version and time, `source_date` and
`last_modified` come from the source file as it is now.

The same header-only update runs when the text is current but the source file moved or only its time
changed, so `resource`, `source_date` and `last_modified` are right again. A header-only update keeps
the links in the old text as they are. Use `--force` to convert the file again and rewrite them.

Other tools may change the front matter of a converted file, for example Crystalline when it writes the
note back or adds a `verified` key. Notedown still knows the file as its own as long as `generated.by`
starts with `notedown/`. Keys that Notedown doesn't know are kept and written after its own keys when
Notedown writes the file again, together with comment lines. Notedown's own keys (like `tags` or
`title`) get Notedown's values again. A file with CRLF line endings keeps them.

Notedown never overwrites a `.md` file it did not write itself. Such a file counts as a failure (exit
code 1) and the error is printed.

`--force` does both: it converts files that are up to date again, and it replaces a `.md` file that
Notedown did not write.

When Notedown searches a folder, it skips what it writes itself: `<name>.assets/` folders, the `--out`
folder and `<name>-page-NNN.png` images next to `<name>.note`. Hidden files and symbolic links to
folders are skipped too.

One failing file does not stop the run. The exit code is 1 if any file failed and 2 for wrong usage.

## Output format

Every `.md` file is a concept document of the
[Open Knowledge Format](https://github.com/GoogleCloudPlatform/knowledge-catalog/tree/main/okf) (OKF
v0.2), so a folder of converted notes is an OKF bundle. Crystalline reads it as it is.

```markdown
---
type: note
title: "Test 2"
description: "Handwritten Supernote note, 1 page, read by OCR."
resource: "Test 2.note"
tags: [supernote]
generated: { by: notedown/0.1.0, model: "chandra-ocr-2 Q4_K_M", at: 2026-10-05T23:10:00Z }
source_date: 2026-10-05
sources:
  - { id: note, resource: "Test 2.note", last_modified: 2026-10-05T22:55:35Z }
status: stable
source_sha256: "<hex>"
file_id: "F20261005225535133622IHP5FqtIJoxD"
pages: 1
keywords: []
links:
  - { page: 1, target_file_id: "F20240708174639550429bHksqaP2wKQ8", target_path: "/storage/emulated/0/Note/Bike.note", target_md: "Bike.md" }
---

<a id="page-1"></a>
## Page 1
<!-- page_id: P20261005225535142460xFi05hBOdWD6 -->

![Page 1](Test%202.assets/page-001.png)

This note is without real-time recognition.

Links: [Bike](Bike.md)
```

The keys of the front matter:

- `type` is always `note`, also for a PNG page.
- `title` is the name of the source file without its extension.
- `description` says what the file is: a note with its number of pages, or a single PNG page.
- `resource` is the source file, relative to the folder of the `.md` file. It works with and without
  `--out`, and also when `--out` is a symbolic link.
- `tags` is always `[supernote]`.
- `generated` says who wrote the text and when: `by` is Notedown with its version, `model` the OCR
  model and `at` the time of the conversion in UTC.
- `source_date` is the day the source file was last changed, in local time.
- `sources` names the source file once more, with `last_modified`, the time it was last changed in
  UTC. When that time can't be read, `source_date` and `last_modified` are left out.
- `status` is `draft` when a page hit the token limit and its text may be cut off. Otherwise it's
  `stable`.
- `source_sha256` is the checksum of the source file. Notedown uses it to skip files that didn't
  change.
- `file_id` is the `FILE_ID` of the note. A PNG page has none.
- `pages` is the number of pages.
- `keywords` lists the keywords of the note as `p<page>: <text>`.
- `links` and `digests` are described below.

OKF reserves the names `index.md` and `log.md`. A source named `index` or `log` (in any case) keeps its
extension in the name of the `.md` file: `index.note` becomes `index.note.md` and `Log.png` becomes
`Log.png.md`. Links to such a note use that name. An `index.md` or `log.md` that an older Notedown
wrote for the same note stays where it is and is no longer updated. The run prints a warning that names
it, so you can delete it.

Text values are written in double quotes, so no YAML reader takes a name like `2024` or `yes` for a
number or a yes/no value. Dates, times and numbers are written without quotes.

Every page starts with an `<a id="page-N"></a>` anchor, so links to a page inside the same note work
as `#page-N`. The image line and the `<name>.assets/` folder appear only with `--images`. A page that
hit the token limit gets `<!-- notedown: truncated -->`. Drawn diagrams that the model reads as Mermaid
become `mermaid` code blocks, followed by the model's description of the diagram as a `>` blockquote.
A drawing that is not a diagram becomes a `> Figure: ...` blockquote.

Links to other notes are listed in a `Links:` line at the end of the page. `links:` in the front
matter keeps each link to another note: `target_file_id` is the `FILE_ID` of the target note,
`target_path` its path on the device and `target_md` the generated Markdown file, relative to this
one. `target_md` is missing when the target note was not converted (yet); the `Links:` line then shows
the name and the device path instead of a link.

A note digest is a text box that the Supernote device created from selected handwriting and linked to
a digest. `digests:` in the front matter lists each such text box: `page` (1-based), `digest_id`,
the `text` of the box and its `rect` as `left,top,width,height` in page pixels. The text is also
part of the page text from the OCR, so the Markdown body does not change. The annotation of the digest
itself is stored outside the `.note` file and is not available to Notedown. The key is left out when
a note has no digests.

## Private Cloud

Notedown can run a small server that the Supernote Nomad accepts as its Private Cloud. It stores your
files as real files in a data folder and writes every change to a journal.

- Protect mode is on by default. The server refuses every file request until `notedown cloud import`
  has filled the account, and it keeps a recycle bin for every delete and overwrite.
- The server prunes the recycle bin about once an hour while it runs. For each file it keeps the 3
  newest old versions, the version the file had at the end of each of the last 7 days and at the end
  of each of the last 12 months. A deleted file's versions are removed 90 days after the delete. See
  `server.toml` and `notedown cloud recycle prune --dry-run`.
- `notedown cloud put` adds a file from the computer. Notedown then tells the connected devices to sync.
  With "Auto Sync" on, the file usually arrives a few seconds later. If it doesn't, start a manual sync.
- `notedown cloud journal tail` prints the changes as JSON lines.
- `notedown serve --refuse-sync` refuses every sync and exchanges no files. It's for recording the
  protocol only.
- Digests (with groups and handwritten comments) and to-dos sync too. They're stored as JSON files
  and every change goes to the journal. `notedown cloud import-app-data` fills the digests from the
  device's `.snbak` backup first (the backup holds no to-dos). Until then the device shows "App Data
  Sync Failed" and only the files sync.

Read the [getting started guide](getting-started.md) before you switch a device. It covers the
backup, the import, the first sync, daily use, the way back and troubleshooting. The protocol is
documented in the [sync protocol](sync-protocol.md).

## Pipeline status

`notedown pipeline status --data DIR [--account A]` prints the state of the OCR pipeline of one account
as one JSON object. `--account` can be left out when the data folder has only one account. It creates
and changes nothing. Each call reads the whole text log, so do not poll more often than every few
seconds. The fields:

- `account`: the account slug.
- `running`: a pipeline runs on this data folder now.
- `notes`, `pagesWithText`: the notes that are not deleted and their pages with text.
- `pending`: pages of notes the pipeline started on and has not read yet.
- `queued`: pages of the current pass that are not read yet. It is 0 after a full pass.
- `waiting`: pages of notes that changed less than the settle time ago, as of the last pass.
- `nextDueMs`: when the next settle time or retry ends (ms since 1970), or `null`.
- `failed`, `truncated`, `errors`: pages whose tries are used up, pages whose text was cut and failed
  pages that are tried again.
- `lastError`: `{"text", "atMs"}` of the last failure of the pipeline as a whole, or `null`. A good
  pass clears it.
- `lastRunMs`: when the last pass ended, or `null`.
- `inWork`: `{"note", "path", "page", "pages", "startedMs"}` of the page it reads now, or `null`.
- `consumers`: one entry per consumer of the changes, `{"name", "seq", "ackedAt", "readAt",
  "notesBehind", "more"}`.

## Format documentation and tests

The `.note` format is documented in the [note format](note-format.md). The parser in `crates/notedown-note` is a
clean-room implementation of that document. The golden test renderings are generated by supernotelib
through `scripts/reference.sh`; it is used only as a test tool.
