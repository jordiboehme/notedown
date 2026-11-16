# Getting started

This guide takes you from zero to a Supernote that syncs with Notedown. It has two parts. Part 1 turns notes into Markdown. Part 2 runs Notedown as the Private Cloud of the device. You can stop after part 1.

Take your time with part 2. A wrong step can make the device delete its own files. The steps say why they're there.

The examples use a Nomad. Other Supernote devices should work the same way, but they're not tested yet.

## Part 1: notes to Markdown

### 1. Install Notedown

There is no release yet, so build it from source. rustup installs the toolchain that
`rust-toolchain.toml` names:

```sh
git clone https://github.com/jordiboehme/notedown.git
cd notedown
cargo install --path crates/notedown
```

On macOS the OCR model runs on the GPU (Metal). On Linux it runs on the CPU. The source build on Linux needs `cmake` and a C++ compiler.

### 2. Download the OCR model

```sh
notedown model pull
```

This is done once. The model is about 3.4 GB and every file is checked against a pinned SHA-256 sum. If the download stops, run the command again: it continues where it stopped. `notedown model path` shows where the files are and `notedown model verify` checks them again.

### 3. Get your notes onto the computer

Any copy works: USB, "Browse & Access" on the device (a web page on port 8089) or the backup command from part 2.

### 4. Convert

```sh
notedown convert ~/Supernote/Note --out ~/notes-md
```

Notedown searches the folder, converts every `.note` file and writes one Markdown file per note. Run the same command again later: notes that didn't change are skipped, and the model isn't even loaded when nothing changed.

Useful options:

- `--images`: also save the page images and link them in the Markdown.
- `--force`: convert again, even when the Markdown is up to date.
- `notedown render "My note.note"`: only the page images, no OCR.

The [reference](reference.md) has all options and the output format.

## Part 2: Notedown as Private Cloud

The device can sync with a "Private Cloud" instead of Ratta's cloud. Notedown can be that cloud. Your files then live as real files in a folder on your computer, and every change is written to a journal.

Notes, documents and the other device folders sync. Digests and to-dos sync too (see "What to expect").

Before you start:

- The computer and the device must be on the same network.
- The server speaks plain HTTP. Keep it in your home network. Put it behind TLS (a reverse proxy) or a VPN before you use it from outside.
- Keep the data folder on the computer's own disk. On a Mac that's APFS. The file system must keep file names exactly as they are (APFS does, HFS+ and some network shares don't). The server warns at start if it doesn't.

The examples use the data folder `~/notedown-data` and the account `me`. Pick your own names.

### 1. Stop writing on the device

Anything you write after the backup in step 3 isn't in the backup. Leave the device alone until step 10.

### 2. Create the account

```sh
notedown cloud account --data ~/notedown-data add me --email you@example.org
```

Notedown asks for a password. The device logs in with this email and password. Use a password that you use nowhere else: the data folder stores a simple MD5 hash of it (the device protocol needs that).

### 3. Back up the device

Turn on "Browse & Access" on the device. It shows an address like `http://192.168.1.20:8089`. Then run:

```sh
notedown cloud backup-device http://192.168.1.20:8089 ~/supernote-backup
```

Use a new, empty folder. Notedown copies every file and writes `backup.json` with the result. This backup is your safety net and the start of the cloud in step 4.

Then make the device's own backup too. Use the backup function of the device. It writes one file with a name like `Supernote_<date>_<time>.snbak`. Copy that file to the computer. It holds your digests (with their handwritten comments), and step 4 puts them into the cloud.

Good to know:

- The `.snbak` file holds digests and plugins, not your notes. For the notes, the folder above is the backup that counts.
- The `.snbak` file doesn't hold your to-dos. The cloud starts with no to-dos. What the device does with its own to-dos at the first sync isn't tested yet. If you have to-dos, write them down before you go on.
- The SD card is skipped. Add `--include-sd` to copy it too (it lands below `REMOVABLE0/`). The cloud never syncs the SD card.
- If a file failed, run the same command again with `--resume`. Use `--resume` only to finish this same backup, never on an older folder.

Check that the backup is complete before you go on. `backup.json` lists every error.

### 4. Import the backup

First look at what would happen. `--dry-run` writes nothing.

```sh
notedown cloud import ~/supernote-backup --data ~/notedown-data --account me --dry-run
notedown cloud import ~/supernote-backup --data ~/notedown-data --account me
```

Why this matters: when the device syncs, it compares its files with the cloud. A file that's missing in the cloud looks like a file you deleted somewhere else, so the device deletes it too. The import makes the cloud hold exactly what the device holds.

Notedown protects you here. Until the import is done, the server refuses every file request of the device (protect mode). The import refuses a backup that looks wrong:

- a `.part` file is left over (an interrupted download);
- `backup.json` is missing or lists errors (a copy made over USB needs `--no-report`);
- a file name starts with a dot inside the device folders (`.DS_Store`, `._*` and `.localized` are fine);
- the backup has no files (that needs `--allow-empty`);
- the account already has files (that needs `--replace`).

An import that failed or was stopped leaves files in the account. Run the next one with `--replace`. The error message says so too.

Then import the digests from the `.snbak` file. Again, look first:

```sh
notedown cloud import-app-data Supernote_<date>_<time>.snbak --data ~/notedown-data --account me --dry-run
notedown cloud import-app-data Supernote_<date>_<time>.snbak --data ~/notedown-data --account me
```

Why this matters: the device compares its digests with the cloud by id. The import puts the digests into the cloud with the ids the device knows, so the device finds nothing to change at the first sync. The dry run lists every digest with its id, its source file and the start of its text.

Until this import is done, the server refuses every digest and to-do request of the device, and the device shows "App Data Sync Failed". Your files still sync. The import needs the file import first. It refuses a `.snbak` file that looks wrong:

- it has no digest list (`backup/DIGEST/knowledge.json`);
- a digest names a handwritten comment that isn't in the file;
- two digests have the same id;
- the account already has digests (that needs `--replace`).

A digest or a digest group that never reached a cloud has no id yet. The import leaves it out and lists it. The device adds it itself at the first sync. The import also lists a digest whose group isn't in the file.

`import-app-data` doesn't touch to-dos. It doesn't count them, and `--replace` doesn't clear them.

A device without any digests needs no `.snbak` file: run `notedown cloud import-app-data --empty --data ~/notedown-data --account me` instead.

### 5. Start the server

```sh
notedown serve --data ~/notedown-data --capture
```

The first start writes `server.toml` with the defaults. Leave `protect = true` and `sync = "accept"`. `--capture` writes every request into `~/notedown-data/capture/` for this run. It helps if something goes wrong. Drop it once you trust the setup.

If the device reaches the server through a reverse proxy, set `external_base_url` in `server.toml`, for example `external_base_url = "https://notes.example.org"`. The device gets the upload and download URLs from this base, so it must be an address the device can reach. Without it, Notedown uses the host of the request. Restart the server after you change it.

The output shows one or more lines like `device URL: http://192.168.1.10:19072`. Use the one of your home network. macOS may ask if Notedown may accept incoming connections. Allow it.

### 6. Log out of Ratta's cloud

The device asks what to do with digests and to-dos: "Retain locally" or "Delete locally". **Always choose "Retain locally".**

### 7. Log in to the Private Cloud

Use the Private Cloud login on the device. Enter the device URL from step 5 and the email and password from step 2.

### 8. Select the folders to sync

After the login to a new account, no folders are selected. Every sync then ends at once with "No folders selected to sync". Tap "Select Now" and choose the same folders as before (usually Note, Document, MyStyle, EXPORT, SCREENSHOT and INBOX). Leave "Auto Sync" off for now.

### 9. One manual sync

Start a manual sync on the device. If the import was complete, the device finds nothing to change and ends with success. Nothing is uploaded, downloaded or deleted.

Then check that nothing was deleted before you do anything else:

```sh
notedown cloud journal tail --data ~/notedown-data
```

The import wrote one `file.created` line per file and one `digest.created` line per digest. This sync must add no `file.deleted` line and no `digest.deleted` line from the device (`"origin":"device"`). An earlier import with `--replace` wrote `digest.deleted` lines with `"origin":"cli"`. Those are fine. Go on only then. The full protocol log is in `~/notedown-data/logs/` (one file per day, passwords and tokens masked) if someone helps you look into a problem.

### 10. Try one change

Write a few strokes into a note and sync again. The server stores the new version and keeps the old one in the recycle bin. `notedown cloud journal tail` now shows a `file.modified` line and a `page.modified` line for the changed page.

Now you can turn "Auto Sync" on. Done.

## Daily use

**Add a file from the computer.** The simplest way is `put`:

```sh
notedown cloud put report.pdf --data ~/notedown-data --account me --to DOCUMENT/Document/Inbox
```

Notedown makes the connected devices of the account sync (it closes their connection, and a device syncs when it connects again). With "Auto Sync" on, the device syncs a few seconds after the `put`. Without it, the device downloads the file at its next manual sync.

If the device doesn't sync after a `put`, start a manual sync. The `push` setting in `server.toml` changes how Notedown tells the device (see "What to expect").

**Change files by hand.** You can add, change, rename and delete files in `accounts/me/files/` with Finder or any tool. Notedown notices such changes within a few seconds: it watches the folder and scans what changed. It also looks every `scan_interval` seconds (300 by default), when a device starts a sync and before it answers a device's folder listing. The timer matters for network volumes, which often report no changes. If the data folder has no `accounts/` folder yet when the server starts, the watch starts within 30 seconds after it appears. Set `watch = false` in `server.toml` to turn the watch off, and restart the server after you change it, as for `scan_interval`. Notedown writes each change to the journal and the device follows. A file you rename or move keeps its place in the device's list, as long as its content stays the same. A rename plus an edit counts as a delete and a new file. To see it at once, run `notedown cloud scan --data ~/notedown-data --account me`. Add `--dry-run` to only see what it would do. Without `--account` it scans every account. Set `scan_interval = 0` in `server.toml` to turn the timer off. That also skips the scan at server start. The scan at sync start and before listings stays. Restart the server after you change it.

A delete by hand is a real delete. The old file isn't in the recycle bin, because Notedown never sees its bytes. Move files out of the folder instead of deleting them if you want a copy.

One safety stop: if a scan would delete files and leave fewer than `scan_min_files` files in the account, Notedown doesn't apply it. The default is 1, so an empty folder or an unmounted disk stops it. Device sync then waits, because the device would delete everything it has. Put the files back and the stop lifts by itself at the next scan. With the watch on, that takes a few seconds. If you really want those files gone, run `notedown cloud scan --data ~/notedown-data --account me --accept`. Deleting your last file on purpose needs `--accept` too. Set `scan_min_files = 0` to turn the stop off. `notedown serve` prints a warning when it stops.

**Watch the changes.** `notedown cloud journal tail --data ~/notedown-data --follow` prints every change as a JSON line while it happens. Each line has a `seq` number. Start at a later one with `--from <seq>`.

**Find an old version.** Every overwrite and every delete moves the old file into `accounts/me/recycle/`. `recycle/index.jsonl` lists what is where. The server prunes the recycle bin by these rules. For each file it keeps the 3 newest old versions, the version the file had at the end of each of the last 7 days and the version it had at the end of each of the last 12 months. Every other old version is removed. A deleted file's versions are removed 90 days after the delete. The settings are `recycle_prune`, `recycle_keep_latest`, `recycle_keep_daily`, `recycle_keep_monthly` and `recycle_keep_deleted_days` in `server.toml`. Set `recycle_prune = false` and the server never prunes. Restart the server after you change them.

An app can list the deleted files and folders, bring them back and empty the bin over the [REST API](rest-api.md) (`GET /recycle`, `POST /recycle/restore` and `POST /recycle/empty`). To restore, it needs a token for writing. Empty needs one too. A restored file comes back with a new id, and the device sees it as a new file. An old version of a replaced file can't be restored this way. Copy it out of `recycle/` by hand.

Empty removes every deleted file and folder from `recycle/` at once, also those the app does not list. It does not remove the old versions of replaced files, also not of files that were restored. The server prunes them by the rules above, so they are not kept for good. After Empty, the old versions of a deleted file are removed 90 days after the newest of them, which can be right away.

To see what a prune would remove, run `notedown cloud recycle prune --data ~/notedown-data --account me --dry-run`. It changes nothing. Without `--dry-run` it prunes right away, also when `recycle_prune` is off. Run the dry run before the first start of a new version. The server prunes about once an hour while it runs. The first prune runs one hour after it starts. A server that's restarted more often than that prunes less, so then run `notedown cloud recycle prune` by hand.

After `notedown cloud import --replace` every file gets a new id. The old versions from before the import then count as versions of a deleted file, so they're removed 90 days after the import.

**Back up.** The data folder holds everything: accounts, files, journal, recycle bin and logs. Back it up like any other folder (Time Machine is fine). Stop the server first if you want a backup that's exactly in sync.

## PDFs of your notes

The server keeps a PDF of every note in `share/` in the data folder, for example `~/notedown-data/share/me/`. You don't need to set anything for it. To use another folder, set `render_dir` in `server.toml`, for example `render_dir = "~/Documents/Supernote PDFs"`. `~` alone is your home folder. The folder must exist. The path must start with `/` or `~` and point outside the data folder, or the server doesn't start. To turn the PDFs off, set `render_dir = ""`. There is one subfolder per account, with the same folders as on the device. Two notes that would get the same PDF name (for example `a.note` and `a.NOTE`) get two PDFs. The note that had its PDF first keeps `a.pdf`. The other one gets its full name, as `a.NOTE.pdf`. When both notes are new, the one whose name sorts first gets `a.pdf`. A changed note is rendered again a few seconds after the sync. A moved or deleted note moves or deletes its PDF. Other files in that folder are never touched. To render everything again, run `notedown cloud render --data <data folder> --all`. For one note without the server, run `notedown render <file.note> --pdf`.

## The server on another machine

The sync server is small. It can run on any machine that stays on all day, so your computer doesn't have to. This can be a small machine, for example a Raspberry Pi or a NAS, or an Intel box or a Mac.

Do step 1 of part 1 first (clone the repository and install rustup). Then build it on that machine without the OCR part. Any Linux machine works (64-bit Arm or x86_64). On a Raspberry Pi, use 64-bit Raspberry Pi OS (arm64):

```sh
cargo install --path crates/notedown --no-default-features
```

This build has `serve`, `cloud` and `render`. It has no OCR, so there is no `pipeline`, `convert`, `model` or `mcp`, and `serve` has no `--pipeline`. The build needs a C compiler (on Debian and Raspberry Pi OS: `sudo apt install build-essential`). It does not need the C++ toolchain for llama.cpp that the OCR build needs. There are no release binaries yet.

Set up the account, the import and the server as in part 2, with the installed `notedown`. Two things to know:

- The machine needs the right time. Most Raspberry Pis have no clock that runs while they are off, so they need NTP after every start (Raspberry Pi OS does this by default). Check that NTP runs on your machine too. The server signs the upload and download URLs with the time. It refuses a URL that is more than 5 minutes in the future or older than its time limit (30 minutes for uploads and 24 hours for downloads).
- The device keeps the address of its Private Cloud. If the address of the server changes, you have to log out of the cloud on the device and set it up again. Give the server a fixed address: a fixed lease in your router or a static address.

Note text for an agent needs the OCR build, so it does not run on this build. Read the server from a Mac instead, as described in the next section.

### A pipeline on a Mac

The pipeline can run on a second machine (for example a Mac) and read the server over the network. The server keeps your files. The Mac keeps the note text.

**On the server.** Nothing to set up. `notedown serve` prints a line like this next to the device URL:

```
REST API: http://192.168.1.10:19072/rest/v1 (connect a pipeline with notedown connect http://192.168.1.10:19072)
```

**On the Mac.** Build Notedown with the OCR part and get the model, as in part 1. Then log in once:

```sh
notedown connect http://192.168.1.10:19072 --data ~/notedown-mac --email you@example.org
```

It asks for the password. With `--password-stdin` it reads the password from the first line of stdin instead. It sends a login to the server and keeps a token in `~/notedown-mac/tokens/`. It also writes the `pipeline.toml` of that folder, so you don't have to. With `--name` you choose the name the token gets on the server (it is `pipeline` by default).

Then start the pipeline and add the MCP server, as in the next section, with the new folder:

```sh
notedown pipeline --data ~/notedown-mac
claude mcp add notedown -- notedown mcp --data ~/notedown-mac
```

For a second account, run `notedown connect` again with its email. The running pipeline picks it up. You don't need to restart it.

**The token is like a password.** It lets whoever has it read the notes of the account. The file in `~/notedown-mac/tokens/` has mode 0600 and the folder has mode 0700. On the server you can see the tokens of an account and revoke them:

```sh
notedown cloud account --data ~/notedown-data tokens list me
notedown cloud account --data ~/notedown-data tokens revoke me <id>
notedown cloud account --data ~/notedown-data tokens revoke me --all
```

`tokens list` shows the id, the scope (`read` or `write`), the name and the creation time of each token, never the token itself. `notedown connect` makes a `read` token. A password change (`cloud account passwd`) revokes every token. A new `connect` leaves the old token valid until you revoke it. A restore of an old backup of the server brings back the tokens that you had revoked since. Revoke them again.

The same API lets an app browse, upload, rename, move and delete the files of an account. It needs a token made for writing. See the [REST API](rest-api.md).

**Update the Mac before the server.** The server writes journal lines with the origin `app` for the changes of an app. An older pipeline skips them.

**When the Mac deletes note text.** Only when the account is gone on the server, or when its name now belongs to a new account. A refused token, a locked account or a server that can't be reached only stop the reading. The Mac keeps the text it has.

**Both machines need the right time (NTP).** The Mac waits until a page has been quiet for a while. It measures that by the time the server wrote the page. If the clocks differ a lot, the Mac reads too early or too late.

**The API is plain HTTP**, like the sync of the device, and made for the home network. The login does not send the password itself. The token goes over the network as it is, so a reverse proxy with HTTPS is the way to reach the API from outside.

## Your notes as text for an agent

Notedown can read the pages of your notes as text (OCR) and keep that text for an agent such as Claude Code. The agent then sees what changed since its last run, page by page, and can put it into Crystalline.

**Read the pages.** Run the pipeline next to the server:

```sh
notedown pipeline --data ~/notedown-data
```

Or start both with one command: `notedown serve --data ~/notedown-data --pipeline`. The pipeline stops when the server stops. It needs the OCR model (`notedown model pull`). It waits 10 minutes after the last change of a note before it reads it, so a note you are still writing is read once. `notedown pipeline --data ~/notedown-data --once` reads everything that is due now and stops.

The first run reads every note in scope, newest note first, and prints its progress (`ocr: 12 of 380 pages`). One page takes up to about half a minute, so a few hundred pages take a few hours. You can stop it at any time: every finished page is kept. By default only notes in `NOTE/Note` are read. The `[pipeline]` section of `server.toml` changes that (`ingest_folders`, `ingest_exclude`, `ingest_keyword`). The pipeline writes only its own files in the data folder: the `pipeline/` folder. It never writes your files. With `--markdown-dir` it also keeps one Markdown file per note in the folder you name (see the [reference](reference.md#markdown-files)).

**Add the MCP server to Claude Code.**

```sh
claude mcp add notedown -- notedown mcp --data ~/notedown-data
```

With more than one account in the data folder, add `--account me`. The server only reads the text and the bookmarks of the agents, and it keeps the time an agent last read the changes. It never runs OCR. Ask Claude "what changed in my notes since yesterday?" to try it.

**Install the ingest skill.** The skill tells the agent how to put the changes into Crystalline: it plans, gives the notes of one folder to one sub-agent, and confirms only when all of them succeeded. Copy it from a source checkout of Notedown:

```sh
cp -R skills/ingest-notes ~/.claude/skills/
```

Then ask Claude to "ingest my notes".

**Run it every night.** `scripts/ingest-notes.sh` does the same without a conversation: it reads the list of changed notes, runs `claude -p` per folder and confirms when every run succeeded. Questions and errors go to `~/Library/Logs/notedown-ingest.log`. The log also names notes with pages that OCR could not read after three tries. Such a page is read again when you change it on the device. The top of the script lists its settings. It needs `jq`. It finds the instructions for each run, `skills/ingest-notes/SKILL.md`, by its own location, so keep the script in the checkout (you can start it from any folder). With more than one account in the data folder, set `NOTEDOWN_ACCOUNT`. To start it every night at 2:30 with launchd, save this as `~/Library/LaunchAgents/de.notedown.ingest.plist` (change the paths to yours):

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>de.notedown.ingest</string>
  <key>ProgramArguments</key>
  <array>
    <string>/Users/me/notedown/scripts/ingest-notes.sh</string>
  </array>
  <key>EnvironmentVariables</key>
  <dict>
    <key>PATH</key>
    <string>/Users/me/.local/bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin</string>
    <key>NOTEDOWN_DATA</key>
    <string>/Users/me/notedown-data</string>
  </dict>
  <key>StartCalendarInterval</key>
  <dict>
    <key>Hour</key>
    <integer>2</integer>
    <key>Minute</key>
    <integer>30</integer>
  </dict>
</dict>
</plist>
```

Load it with `launchctl load ~/Library/LaunchAgents/de.notedown.ingest.plist`. `PATH` must hold `notedown`, `claude` and `jq`, because launchd starts with a short one. Claude Code must know the Crystalline MCP server. If its name is not `crystalline`, set `CRYSTALLINE_MCP` (see the top of the script). When one run fails, the script confirms nothing. The next night repeats the same changes, which is safe.

## What to expect

- Digests and to-dos sync with every sync of the device. Each change shows up in the journal (`digest.created`, `digest.updated`, `digest.deleted`, `task.created` and so on). The digests are in `accounts/me/digests/` and the to-dos in `accounts/me/tasks/`, as JSON files. A handwritten comment of a digest is a `.mark` file in `accounts/me/digests/marks/`.
- If you import an old `.snbak` file, a digest can show up twice on the device. Always import a fresh one.
- A digest you delete on one device can come back when a second device still has it and changes it.
- A file you add with `put` arrives a few seconds later when "Auto Sync" is on. Without it, the file arrives at the next manual sync. Notedown doesn't push while a sync runs or while protect mode is closed. A change you make while `notedown serve` is stopped isn't pushed later. The device gets it at its next sync.
- The `push` setting in `server.toml` has three values. `detail` (the default) sends the changed files and folders to the device after its next heartbeat, in the same form as Ratta's cloud. A file you add with `put` then downloads on the Nomad without a reconnect. `startsync` sends one sync message instead. The Nomad receives it but doesn't sync on it. `off` does nothing. An older `server.toml` may still say `push = "reconnect"`. That value is gone: the server uses `detail` and prints a warning at start. Remove the line or set `push = "detail"`. `notedown serve` prints the active value. Restart the server after you change it.
- Export on the device (PDF or PNG) stays private. The device renders the file itself and puts it into `EXPORT/`, which syncs to Notedown like any other file. "Share via QR Code" is different: it goes to Ratta's own service, not to Notedown, even when you use the Private Cloud. The QR code holds a link on `viewer.supernote.com` that is valid for one day. Don't use it for notes that must stay on your own server.
- You can add, change, rename and delete files in the data folder by hand (see "Change files by hand"). A delete there is a real delete: the device removes its own copy too, and the file doesn't go to the recycle bin.
- Don't move `files/` in the data folder and don't replace it with a symlink. If `files/` or a device folder is missing or is a symlink, the server refuses every file request, and the device keeps all of its files. The error message says how to repair it.

## The way back

Log out of the Private Cloud on the device, choose "Retain locally" and log in to Ratta's cloud again. Your files stay on the device. The data folder stays as it is.

To come back later, never reuse the old backup: the device changed in the meantime, and a file that's missing in the cloud gets deleted on the device. Stop writing, make a new backup into a new folder and a new `.snbak` file (step 3), import both with `--replace` (step 4) and go on with step 5.

## Troubleshooting

**The device says the sync is busy or failed.** Look at the server output and at the newest file in `~/notedown-data/logs/`. Before the import is done, every file request is refused on purpose.

**"App Data Sync Failed".** The digests aren't imported yet. Run `import-app-data` (step 4) or `import-app-data --empty` if the device has no digests. The files sync anyway.

**"No folders selected to sync".** See step 8.

**The device can't reach the server.** Check that both are on the same network, that the URL has the port `19072` and that macOS allowed incoming connections for Notedown (System Settings, Network, Firewall).

**The server says the store is damaged.** Something changed `files/` from outside. The message names the case and the repair. The device keeps its files until the store is fine again.

**Login fails after a few tries.** The account is locked after 6 wrong passwords. `notedown cloud account --data ~/notedown-data passwd me` sets a new password and unlocks it.

## When something goes wrong

Run this, also while the server runs:

```sh
notedown cloud diagnose --data ~/notedown-data
```

It prints a short report: your devices, what they did in the last 3 days and a list of things that need a look. It also writes a zip with the report, the protocol log and the journal of that time, a summary of each account and a test scan. Attach the zip to a GitHub issue.

The zip is safe to share. Note, folder and account names, device serials, IP addresses and the text of digests and to-dos are replaced by stand-ins such as `n-7f3a.note`, `acct-1` and `dev-1`. No file content and no password or token is in it. Before the zip is written, Notedown checks it for every real name it knows. If one is left, it writes nothing and says so.

- `--since 12h` or `--since 2026-10-09` sets another start of the time span.
- `--device <serial>` and `--account <name>` keep only the lines of one device or account.
- `--private` keeps the real names, for your own debugging. Don't share such a zip.

## Record the protocol (optional)

This is for people who want to help document the device protocol. The server refuses every sync, so **no files are exchanged** and the device keeps all of its files.

Run it from a source checkout of Notedown:

```sh
scripts/capture-session.sh ~/notedown-capture
```

The script starts the server with `--capture` and `--refuse-sync` for this run only. Create an account in `~/notedown-capture` first (step 2 above). Then log in on the device as in steps 6 and 7, leave it idle for 10 minutes and try one manual sync. It will say busy or failed. That's expected. Stop the server with Ctrl-C, log out on the device ("Retain locally") and log back in to Ratta's cloud.

The result is in `~/notedown-capture/logs/` and `~/notedown-capture/capture/`. Secrets are masked before anything is written.
