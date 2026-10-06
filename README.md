# notedown

Turn Supernote handwritten notes into Markdown. notedown reads `.note` files (and PNG pages), runs a
local OCR model on every page and writes a Markdown file. Diagrams become Mermaid blocks. Nothing leaves
your machine.

notedown is for Supernote devices and is not affiliated with Ratta.

## Install

From source, with a recent Rust toolchain:

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

notedown can run a small server that the Supernote Nomad accepts as its Private Cloud. It stores your files as real files in a data folder.

The server has two run modes. `sync = "accept"` in `server.toml` is the default: the device syncs its files. `--refuse-sync` is the capture-only mode: every sync is refused and no files are exchanged. Protect mode is on by default. It keeps a recycle bin for deletes and overwrites, and it refuses every file request until `notedown cloud import` has filled the account.

In this section `notedown` means the release binary `target/release/notedown`. Build it once with `cargo build --release -p notedown`, or put it in your PATH.

Start with the capture session below. Switch your Nomad over only after that, with the second guide.

### Capture session

This session only records how the device talks to the server, so the protocol can be documented. **No files are exchanged.** Every sync is refused with a "busy" answer, and the device keeps all of its files.

Follow these steps in this order.

1. **Back up the Nomad first.** The built-in backup of the device (a `.snbak` file) holds only digests and plugins, not your notes. Copy your notes yourself. Connect the Nomad by USB and copy the folders to the Mac. Or open "Browse & Access" on the device. It serves a web page on port 8089 where you can download every file. Check that the copy is complete before you go on.
2. **Create the account.** Run this once from the repository folder. Pick your own email and a password. The device will log in with them. Use a password that you use nowhere else. The server speaks plain HTTP, and the data folder stores a simple MD5 hash of the password. The server is for your home network only. Later it belongs behind TLS (for example a reverse proxy) or a VPN.

   ```
   cargo build --release -p notedown
   notedown cloud account --data ~/notedown-cloud \
     add jordi --email you@example.org
   ```

3. **Start the server.** The script builds the release binary when it is needed and starts the server in capture mode with sync refused.

   ```
   scripts/capture-session.sh ~/notedown-cloud
   ```

   The script turns on capture mode and passes `--refuse-sync`, for this run only. That keeps this guide true: every sync is refused and no files are exchanged. `server.toml` has a `sync` key (`"accept"` is the default, `"refuse"` refuses every sync). The flag overrides it for one run and never rewrites the file. `server.toml` also has a `capture` key. It is for one capture session only, because the dumps in `capture/` have no size cap. Leave it at `false`. The server prints a warning when it is on.

   You can start the script from any folder. macOS may ask if notedown may accept incoming connections. Allow it. The Mac and the Nomad must be on the same network.
4. **Find the IP address of the Mac.** The startup output shows one or more lines like `device URL: http://192.168.1.20:19072`. Use the address of your home network.
5. **Log in on the Nomad.** Log out of Ratta's cloud only now, after the backup is complete. The device asks what to do with digests and to-dos: "Retain locally" or "Delete locally". **Always choose "Retain locally", never "Delete locally".** Then use the Private Cloud login on the device. Enter the address `http://<mac ip>:19072` and the email and the password from step 2.
6. **Leave it idle for 10 minutes.** Then try one manual sync on the device. It will show that the sync is busy or failed. This is expected. The server refuses the sync on purpose.
7. **Stop the server.** Press Ctrl-C in the terminal. Then log out of the private cloud on the Nomad, choose "Retain locally" (never "Delete locally"), and log back in to Ratta's cloud.
8. **The logs.** Everything the device sent is in `~/notedown-cloud/logs/` (one file per day) and, in capture mode, in `~/notedown-cloud/capture/`. Passwords, login codes and tokens are masked in the logs. They are replaced by `<masked:N chars>` before anything is written. Please keep these folders. They are the result of the session.

### Switching the Nomad to notedown

Do this only after the capture session. Take your time. A wrong step can make the Nomad delete its own files, so follow the order.

1. **Stop writing on the Nomad** until the switch is done. Anything you write now is not in the backup.
2. **Back up the device** right before the switch. Turn on "Browse & Access" on the Nomad, then run this on the Mac.

   ```
   notedown cloud backup-device http://<nomad ip>:8089 ~/nomad-backup
   ```

   Use a new, empty folder for this final backup. It copies every file and writes `backup.json` with the result. It skips the SD card unless you add `--include-sd`. The Nomad lists the card at the top under its own name (for example `Extended`); with `--include-sd` it is stored below `REMOVABLE0/`, and the import skips it. If any file failed, run it again with `--resume` into the same folder. Use `--resume` only to finish this same backup, never on an older backup folder. With `--resume` it also removes stale `.part` files. It also lists local files that the device no longer has (in `backup.json` and on the screen). The import refuses such a backup.
3. **Import the backup.** First look at what would happen. Nothing is written with `--dry-run`.

   ```
   notedown cloud import ~/nomad-backup --data ~/notedown-cloud --account jordi --dry-run
   notedown cloud import ~/nomad-backup --data ~/notedown-cloud --account jordi
   ```

   The dry run shows the mapping and the counts. The import opens the protect gate only after it checked every file. Until then every file request of the device is refused with `E0079`. The import refuses when:
   - a `.part` file is present;
   - the report is missing or incomplete (a copy made over USB needs `--no-report`);
   - a name starting with a dot is found inside the device folders (`.DS_Store`, `._*` and `.localized` are fine);
   - there are 0 files (that needs `--allow-empty`);
   - the account already has files (that needs `--replace`).

   An import that failed or was stopped leaves files in the account. Run the next import with `--replace`. The error message says so too.
4. **Start the server** with `--capture` and with protect on (the default).

   ```
   notedown serve --data ~/notedown-cloud --capture
   ```

   Do not add `--refuse-sync`. Leave `protect` at `true` in `server.toml`.
5. **Log out on the Nomad** from Ratta's cloud. The device asks what to do with digests and to-dos: "Retain locally" or "Delete locally". **Always choose "Retain locally".**
6. **Log in to the private cloud** with the address `http://<mac ip>:19072` (the `device URL:` line of the server startup shows the address).
7. **Select the folders to sync.** After the login to a new account, no folders are selected, and every sync ends at once with "No folders selected to sync". Tap "Select Now" and choose the same folders as before (usually Note, Document, MyStyle, EXPORT, SCREENSHOT and INBOX). Leave "Auto Sync" off for now.
8. **Start one manual sync.** If the import was complete, the Nomad lists the files, finds nothing to change and ends the sync with success. Nothing is uploaded, downloaded or deleted.
9. **Check the protocol log** (or send it to whoever helps you) before you do anything else. The logs are in `~/notedown-cloud/logs/` and `~/notedown-cloud/capture/`. Continue only if the log shows no unexpected delete.
10. **Try one change.** Write a few strokes into a note and sync again. The server stores the new version, keeps the old one in `recycle/`, and the journal names the changed page. Then you can turn "Auto Sync" on.
11. **The way back.** Log out of the private cloud, choose "Retain locally", and log in to Ratta's cloud again.

What to expect:

- Digests and to-dos are not served yet. The server answers `E0079` for them, so the Nomad shows "App Data Sync Failed". This is expected until a later version. Your files still sync.
- To add a file from the Mac, use `notedown cloud put report.pdf --data ~/notedown-cloud --account jordi --to DOCUMENT/Document/Inbox`. The Nomad downloads it at its next sync. A later version will push a message that starts the sync.
- Do not delete or change files in the data folder directly. The device would see that as a delete and remove its own copy too. Use `put`, or change files on the device.
- Keep the data folder on the Mac's own disk with APFS. The file system must keep file names exactly as they are (APFS does, HFS+ and some network shares do not). The server warns at start if it does not.
- Do not move `files/` in the data folder, and do not replace it with a symlink. If `files/` or one of the device folders is missing or is a symlink, the server refuses every file request (`E0079`). The device then keeps all of its files. If `files/` is a symlink, remove the link and put the real folder back; `--replace` does not help in that case. If `files/` or a device folder is missing, move it back or import the device again with `--replace`. The error message names the case.

## Format documentation and tests

The `.note` format is documented in `docs/note-format.md`. The parser in `crates/notedown-note` is a
clean-room implementation of that document. The golden test renderings are generated by supernotelib
through `scripts/reference.sh`; it is used only as a test tool.

## License

notedown is licensed under AGPL-3.0-or-later, see `LICENSE`.

The OCR model is Chandra OCR 2 by Datalab. notedown downloads the weights on request and does not ship
them. They are under a modified OpenRAIL-M license: free for personal use and for organisations under 2
million USD revenue. See the model card for the exact terms.
