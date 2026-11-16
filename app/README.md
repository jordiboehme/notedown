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
built with it. You can run the step on its own:

```sh
npm run prepare-sidecar
```

A release build of the app, with the sidecar in the bundle:

```sh
npm run tauri build
```

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

## The Keychain after a rebuild

The app keeps the login token in the macOS Keychain. A dev build is signed anew on every
build, so macOS may ask again for access to the Keychain item after each rebuild. Choose
"Always Allow".

## Menu bar icons

The menu bar (tray) icons in `src-tauri/icons/tray` are made from
`docs/assets/icon-menubar.svg`, at 1x and 2x, plus an attention variant with a dot. After a
change to the SVG, make them again:

```sh
sh app/scripts/tray-icons.sh
```

The script needs `rsvg-convert` (`brew install librsvg`) and runs from any folder.
