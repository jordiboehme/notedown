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

Status: experimental. Notes, documents and the other device folders sync. Digests and to-dos don't sync yet (see "What to expect").

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

Good to know:

- The built-in backup of the device (a `.snbak` file) holds only digests and plugins, not your notes. This backup is the one that counts.
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

Then look at the log before you do anything else. It's in `~/notedown-data/logs/` (one file per day). Passwords, login codes and tokens are masked. Go on only if the log shows no unexpected delete.

### 10. Try one change

Write a few strokes into a note and sync again. The server stores the new version and keeps the old one in the recycle bin. The journal names the changed page:

```sh
notedown cloud journal tail --data ~/notedown-data
```

Now you can turn "Auto Sync" on. Done.

## Daily use

**Add a file from the computer.** Don't copy it into the data folder by hand. Use `put`:

```sh
notedown cloud put report.pdf --data ~/notedown-data --account me --to DOCUMENT/Document/Inbox
```

The device downloads it at its next sync.

**Watch the changes.** `notedown cloud journal tail --data ~/notedown-data --follow` prints every change as a JSON line while it happens. Each line has a `seq` number. Start at a later one with `--from <seq>`.

**Find an old version.** Every overwrite and every delete moves the old file into `accounts/me/recycle/`. `recycle/index.jsonl` lists what is where. The server never empties the recycle bin.

**Back up.** The data folder holds everything: accounts, files, journal, recycle bin and logs. Back it up like any other folder (Time Machine is fine). Stop the server first if you want a backup that's exactly in sync.

## What to expect

- Digests and to-dos don't sync yet. The server answers them with "busy", so the device shows "App Data Sync Failed". Your files still sync.
- A file you add with `put` arrives at the next sync of the device, not right away.
- Don't change files in the data folder directly. The server doesn't notice such a change yet. A removed file looks like a delete to the device, and it removes its own copy too. Use `put`, or change files on the device.
- Don't move `files/` in the data folder and don't replace it with a symlink. If `files/` or a device folder is missing or is a symlink, the server refuses every file request, and the device keeps all of its files. The error message says how to repair it.

## The way back

Log out of the Private Cloud on the device, choose "Retain locally" and log in to Ratta's cloud again. Your files stay on the device. The data folder stays as it is, so you can come back later (run the import again with `--replace` first, because the device changed in the meantime).

## Troubleshooting

**The device says the sync is busy or failed.** Look at the server output and at the newest file in `~/notedown-data/logs/`. Before the import is done, every file request is refused on purpose.

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
