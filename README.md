# notedown

Turn Supernote handwritten notes into Markdown. notedown reads `.note` files (and PNG pages), runs a
local OCR model on every page and writes a Markdown file. Diagrams become Mermaid blocks. Nothing leaves
your machine.

notedown is for Supernote devices and is not affiliated with Ratta.

## Quick start

```sh
brew install jordiboehme/tap/notedown
notedown model pull                       # once, about 3.4 GB
notedown convert ~/Supernote/Note --out ~/notes-md
```

notedown can also be the Private Cloud of your Supernote. Back up the device, fill the cloud with the
backup and the digests from the device's own `.snbak` backup, start the server and log in on the
device:

```sh
notedown cloud account --data ~/notedown-data add me --email you@example.org
notedown cloud backup-device http://<device ip>:8089 ~/supernote-backup
notedown cloud import ~/supernote-backup --data ~/notedown-data --account me
notedown cloud import-app-data Supernote_<date>_<time>.snbak --data ~/notedown-data --account me
notedown serve --data ~/notedown-data
```

On the device, log out of Ratta's cloud and **always choose "Retain locally"**. Then log in to the
Private Cloud with the `device URL` that the server prints, and select the folders to sync.

Don't skip a step here: a cloud that misses files makes the device delete them. The
[getting started guide](docs/getting-started.md) describes every step, with the backup, the first
sync and the way back.

## Install

```sh
brew install jordiboehme/tap/notedown
```

Or from source, with a recent Rust toolchain:

```sh
cargo install --path crates/notedown
```

On macOS the model runs on the GPU with Metal automatically. On Linux the build needs `cmake` and a C++
compiler, and inference runs on the CPU.

Then download the OCR model once (about 3.4 GB, checked against pinned SHA-256 sums):

```sh
notedown model pull
```

The files go to `~/Library/Caches/notedown/models` on macOS and `$XDG_CACHE_HOME/notedown/models` on
Linux. Set `NOTEDOWN_MODEL_DIR` to use another folder. `notedown model path` shows the paths and
`notedown model verify` checks the files.

## Usage

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

Other `convert` options: `--force`, `--model GGUF`, `--mmproj GGUF`, `--max-tokens N` (default 4096),
`--threads N` and `--allow-unknown-version`.

### Skipping and overwriting

A file is skipped when its `.md` already exists and the front matter has the same source checksum, the
same model and the same notedown major version. The model is not even loaded when every file is
skipped.

notedown never overwrites a `.md` file it did not write itself. Such a file counts as a failure (exit
code 1) and the error is printed.

`--force` does both: it converts files that are up to date again, and it replaces a `.md` file that
notedown did not write.

When notedown searches a folder, it skips what it writes itself: `<name>.assets/` folders, the `--out`
folder and `<name>-page-NNN.png` images next to `<name>.note`. Hidden files and symbolic links to
folders are skipped too.

One failing file does not stop the run. The exit code is 1 if any file failed and 2 for wrong usage.

## Output format

```markdown
---
source: Test 2.note
source_sha256: <hex>
kind: supernote-note
file_id: F20261005225535133622IHP5FqtIJoxD
pages: 1
model: chandra-ocr-2 Q4_K_M
converted_at: "2026-10-05T23:10:00Z"
generator: notedown 0.1.0
keywords: []
links:
  - { page: 1, target_file_id: F20240708174639550429bHksqaP2wKQ8, target_path: /storage/emulated/0/Note/Bike.note, target_md: Bike.md }
---

<a id="page-1"></a>
## Page 1
<!-- page_id: P20261005225535142460xFi05hBOdWD6 -->

![Page 1](Test%202.assets/page-001.png)

This note is without real-time recognition.

Links: [Bike](Bike.md)
```

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
itself is stored outside the `.note` file and is not available to notedown. The key is left out when
a note has no digests.

## Private Cloud (experimental)

notedown can run a small server that the Supernote Nomad accepts as its Private Cloud. It stores your
files as real files in a data folder and writes every change to a journal.

- Protect mode is on by default. The server refuses every file request until `notedown cloud import`
  has filled the account, and it keeps a recycle bin for every delete and overwrite. The server prunes the recycle bin once an hour: it keeps the 3 newest versions of a file, one per day for 7 days and one per month for 12 months. A deleted file is kept for 90 days. See `server.toml` and `notedown cloud recycle prune --dry-run`.
- `notedown cloud put` adds a file from the computer. notedown then tells the connected devices to sync.
  With "Auto Sync" on, the file usually arrives a few seconds later. If it doesn't, start a manual sync.
- `notedown cloud journal tail` prints the changes as JSON lines.
- `notedown serve --refuse-sync` refuses every sync and exchanges no files. It's for recording the
  protocol only.
- Digests (with groups and handwritten comments) and to-dos sync too. They're stored as JSON files
  and every change goes to the journal. `notedown cloud import-app-data` fills the digests from the
  device's `.snbak` backup first (the backup holds no to-dos). Until then the device shows "App Data
  Sync Failed" and only the files sync.

Read the [getting started guide](docs/getting-started.md) before you switch a device. It covers the
backup, the import, the first sync, daily use, the way back and troubleshooting. The protocol is
documented in `docs/sync-protocol.md`.

## Format documentation and tests

The `.note` format is documented in `docs/note-format.md`. The parser in `crates/notedown-note` is a
clean-room implementation of that document. The golden test renderings are generated by supernotelib
through `scripts/reference.sh`; it is used only as a test tool.

## License

notedown is licensed under AGPL-3.0-or-later, see `LICENSE`.

The OCR model is Chandra OCR 2 by Datalab. notedown downloads the weights on request and does not ship
them. They are under a modified OpenRAIL-M license: free for personal use and for organisations under 2
million USD revenue. See the model card for the exact terms.
