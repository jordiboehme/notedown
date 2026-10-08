# Getting started

This guide takes you from zero to a Supernote that syncs with notedown. It has two parts. Part 1 turns notes into Markdown. Part 2 runs notedown as the Private Cloud of the device. You can stop after part 1.

Take your time with part 2. A wrong step can make the device delete its own files. The steps say why they're there.

The examples use a Nomad. Other Supernote devices should work the same way, but they're not tested yet.

## Part 1: notes to Markdown

### 1. Install notedown

```sh
brew install jordiboehme/tap/notedown
```

Or build it from source with a recent Rust toolchain:

```sh
cargo install --path crates/notedown
```

On macOS the OCR model runs on the GPU (Metal). On Linux it runs on the CPU. The source build on Linux needs `cmake` and a C++ compiler.

### 2. Download the OCR model

```sh
notedown model pull
```

This is done once. The model is about 3.4 GB and every file is checked against a pinned SHA-256 sum. `notedown model path` shows where the files are and `notedown model verify` checks them again.

### 3. Get your notes onto the computer

Any copy works: USB, "Browse & Access" on the device (a web page on port 8089) or the backup command from part 2.

### 4. Convert

```sh
notedown convert ~/Supernote/Note --out ~/notes-md
```

notedown searches the folder, converts every `.note` file and writes one Markdown file per note. Run the same command again later: notes that didn't change are skipped, and the model isn't even loaded when nothing changed.

Useful options:

- `--images`: also save the page images and link them in the Markdown.
- `--force`: convert again, even when the Markdown is up to date.
- `notedown render "My note.note"`: only the page images, no OCR.

The README has all options and the output format.

## Part 2: notedown as Private Cloud

The device can sync with a "Private Cloud" instead of Ratta's cloud. notedown can be that cloud. Your files then live as real files in a folder on your computer, and every change is written to a journal.

Status: experimental. Notes, documents and the other device folders sync. Digests and to-dos sync too (see "What to expect").

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

notedown asks for a password. The device logs in with this email and password. Use a password that you use nowhere else: the data folder stores a simple MD5 hash of it (the device protocol needs that).

### 3. Back up the device

Turn on "Browse & Access" on the device. It shows an address like `http://192.168.1.20:8089`. Then run:

```sh
notedown cloud backup-device http://192.168.1.20:8089 ~/supernote-backup
```

Use a new, empty folder. notedown copies every file and writes `backup.json` with the result. This backup is your safety net and the start of the cloud in step 4.

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

notedown protects you here. Until the import is done, the server refuses every file request of the device (protect mode). The import refuses a backup that looks wrong:

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

The output shows one or more lines like `device URL: http://192.168.1.10:19072`. Use the one of your home network. macOS may ask if notedown may accept incoming connections. Allow it.

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

notedown makes the connected devices of the account sync (it closes their connection, and a device syncs when it connects again). With "Auto Sync" on, the device syncs a few seconds after the `put`. Without it, the device downloads the file at its next manual sync.

If the device doesn't sync after a `put`, start a manual sync. The `push` setting in `server.toml` changes how notedown tells the device (see "What to expect").

**Change files by hand.** You can add, change, rename and delete files in `accounts/me/files/` with Finder or any tool. notedown notices such changes within a few seconds: it watches the folder and scans what changed. It also looks every `scan_interval` seconds (300 by default), when a device starts a sync and before it answers a device's folder listing. The timer matters for network volumes, which often report no changes. Set `watch = false` in `server.toml` to turn the watch off, and restart the server after you change it, as for `scan_interval`. notedown writes each change to the journal and the device follows. A file you rename or move keeps its place in the device's list, as long as its content stays the same. A rename plus an edit counts as a delete and a new file. To see it at once, run `notedown cloud scan --data ~/notedown-data --account me`. Add `--dry-run` to only see what it would do. Without `--account` it scans every account. Set `scan_interval = 0` in `server.toml` to turn the timer off. That also skips the scan at server start. The scan at sync start and before listings stays. Restart the server after you change it.

A delete by hand is a real delete. The old file isn't in the recycle bin, because notedown never sees its bytes. Move files out of the folder instead of deleting them if you want a copy.

One safety stop: if a scan would delete files and leave fewer than `scan_min_files` files in the account, notedown doesn't apply it. The default is 1, so an empty folder or an unmounted disk stops it. Device sync then waits, because the device would delete everything it has. Put the files back and the stop lifts by itself at the next scan. If you really want those files gone, run `notedown cloud scan --data ~/notedown-data --account me --accept`. Deleting your last file on purpose needs `--accept` too. Set `scan_min_files = 0` to turn the stop off. `notedown serve` prints a warning when it stops.

**Watch the changes.** `notedown cloud journal tail --data ~/notedown-data --follow` prints every change as a JSON line while it happens. Each line has a `seq` number. Start at a later one with `--from <seq>`.

**Find an old version.** Every overwrite and every delete moves the old file into `accounts/me/recycle/`. `recycle/index.jsonl` lists what is where. The server prunes the recycle bin by these rules. For each file it keeps the 3 newest old versions, the version the file had at the end of each of the last 7 days and the version it had at the end of each of the last 12 months. Every other old version is removed. A deleted file's versions are removed 90 days after the delete. The settings are `recycle_prune`, `recycle_keep_latest`, `recycle_keep_daily`, `recycle_keep_monthly` and `recycle_keep_deleted_days` in `server.toml`. Set `recycle_prune = false` and the server never prunes. Restart the server after you change them.

To see what a prune would remove, run `notedown cloud recycle prune --data ~/notedown-data --account me --dry-run`. It changes nothing. Without `--dry-run` it prunes right away, also when `recycle_prune` is off. Run the dry run before the first start of a new version. The server prunes about once an hour while it runs. The first prune runs one hour after it starts. A server that's restarted more often than that prunes less, so then run `notedown cloud recycle prune` by hand.

After `notedown cloud import --replace` every file gets a new id. The old versions from before the import then count as versions of a deleted file, so they're removed 90 days after the import.

**Back up.** The data folder holds everything: accounts, files, journal, recycle bin and logs. Back it up like any other folder (Time Machine is fine). Stop the server first if you want a backup that's exactly in sync.

## PDFs of your notes

Set `render_dir` in `server.toml` to a folder, for example `render_dir = "~/Documents/Supernote PDFs"`. The folder must exist. The subfolder of an account must not be the data folder, lie inside it or hold it. The server keeps a PDF of every note there, in one subfolder per account, with the same folders as on the device. Two notes that would get the same PDF name (for example `a.note` and `a.NOTE`) get two PDFs. The note that had its PDF first keeps `a.pdf`. The other one gets its full name, as `a.NOTE.pdf`. When both notes are new, the one whose name sorts first gets `a.pdf`. A changed note is rendered again a few seconds after the sync. A moved or deleted note moves or deletes its PDF. Other files in that folder are never touched. To render everything again, run `notedown cloud render --data <data folder> --all`. For one note without the server, run `notedown render <file.note> --pdf`.

## What to expect

- Digests and to-dos sync with every sync of the device. Each change shows up in the journal (`digest.created`, `digest.updated`, `digest.deleted`, `task.created` and so on). The digests are in `accounts/me/digests/` and the to-dos in `accounts/me/tasks/`, as JSON files. A handwritten comment of a digest is a `.mark` file in `accounts/me/digests/marks/`.
- If you import an old `.snbak` file, a digest can show up twice on the device. Always import a fresh one.
- A digest you delete on one device can come back when a second device still has it and changes it.
- A file you add with `put` arrives a few seconds later when "Auto Sync" is on. Without it, the file arrives at the next manual sync. notedown doesn't push while a sync runs or while protect mode is closed. A change you make while `notedown serve` is stopped isn't pushed later. The device gets it at its next sync.
- The `push` setting in `server.toml` has three values. `detail` (the default) sends the changed files and folders to the device after its next heartbeat, in the same form as Ratta's cloud. A file you add with `put` then downloads on the Nomad without a reconnect. `startsync` sends one sync message instead. The Nomad receives it but doesn't sync on it. `off` does nothing. An older `server.toml` may still say `push = "reconnect"`. That value is gone: the server uses `detail` and prints a warning at start. Remove the line or set `push = "detail"`. `notedown serve` prints the active value. Restart the server after you change it.
- You can add, change, rename and delete files in the data folder by hand (see "Change files by hand"). A delete there is a real delete: the device removes its own copy too, and the file doesn't go to the recycle bin.
- Don't move `files/` in the data folder and don't replace it with a symlink. If `files/` or a device folder is missing or is a symlink, the server refuses every file request, and the device keeps all of its files. The error message says how to repair it.

## The way back

Log out of the Private Cloud on the device, choose "Retain locally" and log in to Ratta's cloud again. Your files stay on the device. The data folder stays as it is.

To come back later, never reuse the old backup: the device changed in the meantime, and a file that's missing in the cloud gets deleted on the device. Stop writing, make a new backup into a new folder and a new `.snbak` file (step 3), import both with `--replace` (step 4) and go on with step 5.

## Troubleshooting

**The device says the sync is busy or failed.** Look at the server output and at the newest file in `~/notedown-data/logs/`. Before the import is done, every file request is refused on purpose.

**"App Data Sync Failed".** The digests aren't imported yet. Run `import-app-data` (step 4) or `import-app-data --empty` if the device has no digests. The files sync anyway.

**"No folders selected to sync".** See step 8.

**The device can't reach the server.** Check that both are on the same network, that the URL has the port `19072` and that macOS allowed incoming connections for notedown (System Settings, Network, Firewall).

**The server says the store is damaged.** Something changed `files/` from outside. The message names the case and the repair. The device keeps its files until the store is fine again.

**Login fails after a few tries.** The account is locked after 6 wrong passwords. `notedown cloud account --data ~/notedown-data passwd me` sets a new password and unlocks it.

## Record the protocol (optional)

This is for people who want to help document the device protocol. The server refuses every sync, so **no files are exchanged** and the device keeps all of its files.

Run it from a source checkout of notedown:

```sh
scripts/capture-session.sh ~/notedown-capture
```

The script starts the server with `--capture` and `--refuse-sync` for this run only. Create an account in `~/notedown-capture` first (step 2 above). Then log in on the device as in steps 6 and 7, leave it idle for 10 minutes and try one manual sync. It will say busy or failed. That's expected. Stop the server with Ctrl-C, log out on the device ("Retain locally") and log back in to Ratta's cloud.

The result is in `~/notedown-capture/logs/` and `~/notedown-capture/capture/`. Secrets are masked before anything is written.
