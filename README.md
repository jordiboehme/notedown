<picture>
  <source media="(prefers-reduced-motion: reduce)" srcset="docs/assets/banner-static.png">
  <img src="docs/assets/banner.gif" alt="Notedown" width="100%">
</picture>

# Notedown

**Ink in. Markdown out. Agents welcome.**

[![CI](https://github.com/jordiboehme/notedown/actions/workflows/ci.yml/badge.svg)](https://github.com/jordiboehme/notedown/actions/workflows/ci.yml)
[![License: AGPL-3.0](https://img.shields.io/github/license/jordiboehme/notedown)](LICENSE)
[![Works best with Crystalline](.github/badges/works-best-with-crystalline.svg)](https://github.com/jordiboehme/crystalline)

You write on a Supernote. Your AI agent reads Markdown. Notedown sits in between.

Notedown reads your handwriting with a local OCR model and writes Markdown. It can also be the
Private Cloud your Supernote syncs with, and it gives the note text to your AI agent. It's for people
who think with a pen and want their notes to stay on their own machine.

One Rust binary. No accounts at a vendor, no telemetry. Notedown is for Supernote devices and is not
affiliated with Ratta.

## What you get

- **Handwriting to Markdown.** Every page of a `.note` file (or a PNG page) becomes text, read by a
  local OCR model.
- **Diagrams as Mermaid.** A drawn diagram becomes a Mermaid block. Links between notes stay links.
  Digests and keywords land in the front matter.
- **Your own Private Cloud.** Clouds? Where we're going, we don't need someone else's cloud. Your
  Supernote syncs with a small server at home. Your files stay real files in a folder, every change
  lands in a journal and every delete and overwrite goes to a recycle bin first.
- **Note text for your AI agent.** A pipeline turns new pages into text while you still hold the pen.
  Agents read it over MCP.
- **Everything local.** OCR runs with Metal on your Mac. Nothing leaves your machine.

```mermaid
flowchart LR
    S[Supernote] -->|sync| C[notedown serve]
    C -->|journal and files| P[notedown pipeline]
    P -->|OKF Markdown| K[Knowledge base]
    P -->|MCP over stdio| A[AI agent]
```

The Markdown follows the frontmatter conventions of [Crystalline](https://github.com/jordiboehme/crystalline),
so your notes become knowledge your agents can search, link and build on. Office files and PDFs go
through [Rezzer](https://github.com/jordiboehme/rezzer), the same way.

Status: `convert`, the Private Cloud and the pipeline work today. There is no release yet, so you
build it from source. Next: a new vision engine that reads layout and content in two steps, and a Mac
menu bar app that does the setup and the daily work for you. Until the app is there, the command
line is the way in. Star the repo if you want to see where this goes.

## Get started

All you need is a Mac with Apple Silicon and the Rust toolchain ([rustup](https://rustup.rs)).

```sh
# build and install notedown (rustup picks the right toolchain by itself)
git clone https://github.com/jordiboehme/notedown.git
cd notedown
cargo install --path crates/notedown

# download the OCR model once (about 3.4 GB)
notedown model pull

# turn a folder of notes into Markdown
notedown convert ~/Supernote/Note --out ~/notes-md
```

That's it for Markdown. Run `convert` again later: notes that didn't change are skipped.

**Your Mac as the Private Cloud.** Notedown can also be the cloud your Supernote syncs with, and it
reads every new page while it runs. Fill the cloud with a backup of the device first: a cloud that
misses files makes the device delete them.

Every command that takes `--data` uses `$NOTEDOWN_DATA` when you leave it out, or `~/notedown-data` when that is not set.

```sh
# an account for the device to log in with
notedown cloud account --data ~/notedown-data add me --email you@example.org
# back up the device (turn on "Browse & Access" on the device first)
notedown cloud backup-device http://<device ip>:8089 ~/supernote-backup
# fill the cloud with the backup and the digests from the device's own .snbak backup
notedown cloud import ~/supernote-backup --data ~/notedown-data --account me
notedown cloud import-app-data Supernote_<date>_<time>.snbak --data ~/notedown-data --account me
# start the server and the pipeline that turns new pages into text
notedown serve --data ~/notedown-data --pipeline
```

On the device, log out of Ratta's cloud and **always choose "Retain locally"**. Then log in to the
Private Cloud with the `device URL` that the server prints, and select the folders to sync.

Give the note text to your agent, for example Claude Code:

```sh
claude mcp add notedown -- notedown mcp --data ~/notedown-data
```

Don't skip a step here. The [getting started guide](docs/getting-started.md) describes every step:
the backup, the import, the device login, the first sync, daily use and the way back.

## Run it in your homelab

Want a more professional setup? The sync server is small and runs on any machine that is always on:
a NAS, a Raspberry Pi, an Intel box or any other home server (Linux on 64-bit Arm or x86_64, or a
Mac). Build it there without the OCR part. Your Supernote then syncs with the server even when your Mac sleeps.
The Mac connects to the server over its REST API, reads the new pages and keeps the note text for
your agent.

```mermaid
flowchart LR
    S[Supernote] -->|sync| C["notedown serve<br/>NAS or home server"]
    C -->|REST API| P["notedown pipeline<br/>Mac"]
    P -->|MCP| A[AI agent]
    X[Your app] -->|REST API| C
```

On the server:

```sh
# in a clone of the repository: the sync server without OCR (needs a C compiler, no llama.cpp)
cargo install --path crates/notedown --no-default-features
# set up the account and the import as above, then start the server
notedown serve --data ~/notedown-data
```

On the Mac, with the full build and the model:

```sh
# log in once, keeps a read-only token and writes pipeline.toml
notedown connect http://<server>:19072 --data ~/notedown-mac --email you@example.org
# read the new pages as text
notedown pipeline --data ~/notedown-mac
# give the text to your agent
claude mcp add notedown -- notedown mcp --data ~/notedown-mac
```

Give the server a fixed address (a fixed lease in your router or a static address): the device keeps
the address of its Private Cloud. Both machines need the right time (NTP). The details are in the
guide: [the server on another machine](docs/getting-started.md#the-server-on-another-machine) and
[a pipeline on a Mac](docs/getting-started.md#a-pipeline-on-a-mac).

Building an app? The same [REST API](docs/rest-api.md) lets it browse, upload, rename, move and
delete the files of an account.

## Go deeper

- [Getting started](docs/getting-started.md): the full guide, from the first note to a Supernote that syncs with Notedown.
- [Reference](docs/reference.md): install details, every `convert` and `render` option, [skipping and overwriting](docs/reference.md#skipping-and-overwriting), the [output format](docs/reference.md#output-format) and the [Private Cloud](docs/reference.md#private-cloud) in short.
- [REST API](docs/rest-api.md): every route for apps and for the pipeline on a Mac.
- [Sync protocol](docs/sync-protocol.md): how the device talks to its cloud, and the files and journal on disk.
- [Note format](docs/note-format.md): the `.note` file format, as the parser reads it.

## Principles

- **Local first.** OCR runs on your machine. Your notes never go to a vendor's cloud, ours included (there is none).
- **Never lose a note.** A network error or a missing folder means "nothing known", never "deleted". Every delete and overwrite goes to a recycle bin first.
- **Honest output.** A page that hit the token limit is marked as a draft. A drawing that isn't a diagram says so.
- **One binary.** No Python environment, no helper programs.

## Security

`notedown serve` speaks plain HTTP on your home network, like the Supernote expects. Don't expose it
to the internet directly. Found a problem? Please report it privately, see [SECURITY.md](SECURITY.md).

## Support

Notedown is free and open source. If it earned its place next to your pen, you can support or sponsor
the work here:

[![ko-fi](https://ko-fi.com/img/githubbutton_sm.svg)](https://ko-fi.com/V7V31T6CL9)
[![PayPal](https://img.shields.io/badge/PayPal-sponsor-blue?logo=paypal)](https://paypal.me/jordiboehmelopez)

Missing a feature for your Supernote workflow? [Open an issue](https://github.com/jordiboehme/notedown/issues)
and let's talk.

## License

Notedown is licensed under AGPL-3.0-or-later, see `LICENSE`.

The OCR model is Chandra OCR 2 by Datalab. Notedown downloads the weights on request and does not ship
them. They are under a modified OpenRAIL-M license: free for personal use and for organisations under 2
million USD revenue. See the model card for the exact terms.
