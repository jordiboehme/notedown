# Notedown for the Mac

The menu bar app of Notedown, made with Tauri 2 and Svelte 5. This file is for people who
build it. The app runs the `notedown` program as its sidecar: the server and the pipeline are
its children, and every call of the program is made in Rust.

`app/src-tauri` is a Cargo workspace of its own, with its own `Cargo.lock` and `deny.toml`.
The root workspace does not see it.

## What you need

- macOS 13 or later
- Rust (the version in `rust-toolchain.toml` of the repository)
- Node 22 and npm

## Build and run

Run everything from `app/`:

```sh
npm ci
npm run tauri dev
```

`npm run tauri dev` first runs `npm run prepare-sidecar`. It builds `notedown` in release mode
in the root workspace (with OCR) and copies it to `src-tauri/binaries/notedown-<target>`, the
name that Tauri's `externalBin` needs. The first build takes a while, because llama.cpp is
built with it. It also writes a placeholder `src-tauri/resources/THIRD-PARTY-NOTICES.txt` when
there is none: the bundle carries that file, and only the release build writes the real
notices. Cargo in `src-tauri` needs both files. You can run the step on its own:

```sh
npm run prepare-sidecar
```

A release build of the app, with the sidecar in the bundle:

```sh
npm run tauri build
```

The bundle also makes the updater files (`createUpdaterArtifacts`), and Tauri signs them with
the updater key. Without `TAURI_SIGNING_PRIVATE_KEY` the build fails with "A public key has
been found, but no private key". A local build does not need the release key. Turn the updater
files off:

```sh
npm run tauri build -- --config '{"bundle":{"createUpdaterArtifacts":false}}'
```

Or sign with a throwaway key. Nobody's updater installs a local build:

```sh
npx tauri signer generate --ci -w /tmp/throwaway.key
export TAURI_SIGNING_PRIVATE_KEY="$(cat /tmp/throwaway.key)"
export TAURI_SIGNING_PRIVATE_KEY_PASSWORD=''
npm run tauri build
```

## Updates

The app checks for a new version at start and then once a day. It reads `latest.json` of the
newest GitHub release and checks the signature of the update with the public key in
`tauri.conf.json`. A new version opens Settings on the About page, which shows its notes and
asks. "Check for Updates..." in the menu bar menu and the button on the About page check at
once. The install stops the server and the pipeline and starts Notedown again.

## Checks

```sh
npm run check
npx vitest run
npm run build
cd src-tauri
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
cargo deny check
```

The tests never run the real `notedown` program and never touch the Keychain. The Keychain
test is `#[ignore]`d. Run it by hand with `cargo test keychain_store_round_trips -- --ignored`.

## The Setup page

Settings opens at Setup on the first start. Later the "Set up" line in the menu bar panel leads
there. The page is a checklist of seven steps. A step needs the steps before it, so a later
step shows "Do step N first" until they are done. The OCR model and the agent need no other
step.

1. Choose the server. Local runs the server on this Mac and turns on Start at login once.
   Remote uses a server on another machine. Notedown also finds a setup that already exists on
   this Mac and offers to use it.
2. Log in, or create an account. The account name comes from the e-mail and you can change it.
   For a server on another machine, the same password also connects the OCR pipeline.
3. Check the connection. This shows that the server answers and that the login works.
4. Download the OCR model, about 3.4 GB. You can pause it and continue later.
5. Back up and import the Supernote. See below.
6. Connect the Supernote. It lists what to do on the device. After the first sync, Notedown
   checks the journal once and tells you if the Supernote deleted files. Then you can open the
   recycle bin.
7. Connect an agent. This step is optional. It gives the MCP settings for your agent.

Step 5 is the risky one. Never write on the Supernote between the backup and the first sync in
step 6. A file that is missing on the server is deleted on the Supernote. So the import
refuses a backup that is older than 24 hours or older than the last sync of the device, and a
`.snbak` file that is older than 24 hours. Then the only way is a new backup into a new folder.
The order is fixed: back up, check the files, import the files, check the `.snbak` file for
the digests, import the digests. A bad `.snbak` file sends you back to the choice of the file
without a second import of the files. "Import again" is a separate path for a Supernote that
synced before. It needs a new backup and the Replace option.

While the files or the digests are imported, the app's server is stopped. Start, the mode, log
in, log out, create account, connect, the Markdown folder and the model idle time answer "Wait
until the import is done." An import does not start while the server runs outside the app.

## The OCR page

"Free memory after" on the OCR page sets how long the pipeline keeps the OCR model loaded
without new pages: 1 min, 5 min (the default), 15 min or 1 hour. The app passes it to the
pipeline as `--model-idle`, and a change starts only the pipeline again (`set_model_idle`). The
model loads again for the next page.

## Commands

The Setup page calls these Tauri commands (`src-tauri/src/commands.rs`). The ones that run the
`notedown` program use its `--json` output:

- `create_account`, `connect_pipeline`: make the account and connect the pipeline. The password
  goes to the program on stdin and never into its arguments.
- `existing_details`: reads an existing setup on this Mac.
- `log_out`: revokes the token on the server (`POST /logout`), without waiting for it.
- `backup_start`, `backup_pause`, `backup_folder`: the backup and the check of its folder.
- `import_dry_run`, `import_run`, `app_data_dry_run`, `app_data_run`: the checks and the two
  imports.
- `job_state`: the job that runs now.
- `set_setup_record`, `sync_state`, `snbak_time`, `open_recycle_bin`: the record of the setup,
  the check of the first sync, the file time of the `.snbak` file and the recycle bin.
- `model_state`, `model_pull`, `model_pause`, `model_remove`: the OCR model.

The checklist itself is `src/settings/checklist.ts`. The rules of step 5 are in
`src/settings/importFlow.ts`. Both have tests.

## The Keychain after a rebuild

The app keeps the login token in the macOS Keychain. A dev build is signed anew on every
build, so macOS may ask again for access to the Keychain item after each rebuild. Choose
"Always Allow".

## Menu bar icons

The menu bar (tray) icons in `src-tauri/icons/tray` are made from
`docs/assets/icon-menubar.svg`, at 1x and 2x, plus an attention variant with a dot. After a
change to the SVG, make them again:

```sh
sh scripts/tray-icons.sh
```

The script needs `rsvg-convert` (`brew install librsvg`) and runs from any folder.
