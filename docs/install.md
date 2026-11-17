# Install Notedown

Every release has ready-made packages on the [releases page](https://github.com/jordiboehme/notedown/releases/latest):

- macOS: the Notedown app (with OCR, the sync server and the menu bar) and the `notedown` command line tool. Apple Silicon only, macOS 13 or later.
- Linux: a `.deb` package with a systemd service, a plain archive and a container image. 64-bit Arm (arm64) and x86_64 (amd64). The Linux builds have no OCR: they have `serve`, `cloud` and `render`.

`<ver>` below is the version without the `v`, for example `0.1.0`.

## macOS

### The app

With [Homebrew](https://brew.sh):

```sh
brew install --cask jordiboehme/tap/notedown-app
```

Or download `notedown-app-v<ver>-macos.dmg`, open it and drag Notedown into Applications. The app is signed and notarized.

The app looks for updates at start and every 24 hours. When there is a new version, it opens Settings on the About page once and shows the version and its notes. It installs only when you say so. "Check for Updates..." in the menu bar checks right away.

### The command line tool

```sh
brew install jordiboehme/tap/notedown
```

Or download `notedown-v<ver>-macos.tar.gz`, unpack it and put `notedown` on your `PATH`. The binary is signed and notarized. It has OCR, so get the model next (`notedown model pull`, see [Getting started](getting-started.md)).

## Linux: the .deb (Debian, Ubuntu, Raspberry Pi OS)

Download `notedown_<ver>_arm64.deb` (Raspberry Pi with 64-bit Raspberry Pi OS, other Arm boards) or `notedown_<ver>_amd64.deb` (x86_64) and install it:

```sh
sudo apt install ./notedown_<ver>_arm64.deb
```

The package installs:

- `/usr/bin/notedown`
- a system user `notedown` with its home in `/var/lib/notedown`
- the service `notedown.service`. It is installed but not started.
- `/etc/default/notedown` for environment variables of the service. Today the server reads none from there.

Data folder: `/var/lib/notedown/notedown-data`. The service and every `notedown` command you run as the `notedown` user use it, so you never need `--data`.

### Set it up

Do the steps of part 2 of [Getting started](getting-started.md) as the `notedown` user. Three rules:

- Put `sudo -u notedown` in front of every `notedown` command.
- Leave out `--data`. Don't write `~` in a path: your shell turns it into your own home folder before sudo runs.
- Keep the files you import under `/var/lib/notedown`. The `notedown` user often can't read your home folder.

For example:

```sh
sudo -u notedown notedown cloud account add me --email you@example.org
sudo -u notedown notedown cloud backup-device http://192.168.1.20:8089 /var/lib/notedown/supernote-backup
sudo -u notedown notedown cloud import /var/lib/notedown/supernote-backup --account me --dry-run
sudo -u notedown notedown cloud import /var/lib/notedown/supernote-backup --account me
```

Then start the server, now and at every boot:

```sh
sudo systemctl enable --now notedown
journalctl -u notedown -f
```

The server listens on port 19072 on every address of the machine. The device URL is printed in the journal.

### Settings

The server keeps its settings in `/var/lib/notedown/notedown-data/server.toml`. The first start writes it with the defaults. Edit it as the `notedown` user (`sudo -u notedown nano /var/lib/notedown/notedown-data/server.toml`) and run `sudo systemctl restart notedown`.

The service can write only below `/var/lib/notedown`. This matters for `render_dir` (the folder for the PDFs):

- Default: the PDFs go to `share/` in the data folder. Nothing to do.
- Another folder: use one below `/var/lib/notedown` and outside the data folder, for example `/var/lib/notedown/pdfs`. Create it first with `sudo -u notedown mkdir /var/lib/notedown/pdfs`. This works without any change to the service.
- A folder somewhere else (not below `/home`, `/root` or `/run/user`): the folder must be writable by the `notedown` user, and the service needs a drop-in. Run `sudo systemctl edit notedown`, add the lines below and restart the service.

  ```ini
  [Service]
  ReadWritePaths=/srv/notedown-pdfs
  ```

- A folder below `/home`: this does not work. The service can't see `/home` at all (`ProtectHome=yes`), and `ReadWritePaths=` can't open it again. Keep the PDFs below `/var/lib/notedown` and share that folder if you need it somewhere else.

Do not set `NOTEDOWN_DATA` in `/etc/default/notedown`. The service would use it, but `sudo -u notedown notedown ...` never reads that file. The comments in the file say how to move the data folder, if you really need to.

### Upgrade and remove

Install the new `.deb` the same way. A running service restarts on the new version. A service you never enabled stays off.

`sudo apt remove notedown` and `sudo apt purge notedown` keep your data in `/var/lib/notedown` and the `notedown` user. `purge` also deletes `/etc/default/notedown`. To delete everything, run after the purge:

```sh
sudo rm -r /var/lib/notedown
sudo userdel notedown
```

## Linux: the archive

`notedown-v<ver>-linux-arm64.tar.gz` and `notedown-v<ver>-linux-amd64.tar.gz` hold a static binary. It runs on any Linux of that architecture. Unpack it and copy `notedown` to a folder on your `PATH`, for example `/usr/local/bin`. There is no user and no service: you run it yourself, as in [Getting started](getting-started.md).

## Container image

The image `ghcr.io/jordiboehme/notedown` runs the sync server. It is built for linux/amd64 and linux/arm64. Tags are the version without the `v` (`0.1.0`) and `latest`. The data folder is `/data`, the port is 19072. The server runs as user 65532, not as root.

```sh
docker run -d --name notedown --restart unless-stopped \
  -p 19072:19072 -v notedown-data:/data \
  ghcr.io/jordiboehme/notedown:<ver>
```

Or with Compose:

```yaml
services:
  notedown:
    image: ghcr.io/jordiboehme/notedown:<ver>
    restart: unless-stopped
    ports:
      - "19072:19072"
    volumes:
      - notedown-data:/data

volumes:
  notedown-data:
```

Run the commands of part 2 of [Getting started](getting-started.md) inside the container, without `--data`. Use `-it`, so the password prompt works:

```sh
docker exec -it notedown notedown cloud account add me --email you@example.org
docker restart notedown
```

Notes:

- The device connects to the address of the host, port 19072. Don't use the address the server prints inside the container: that is the address of the container network, which the device can't reach.
- A named volume just works. A bind mount (`-v /srv/notedown:/data`) must be writable by uid 65532 on the host: `sudo chown 65532:65532 /srv/notedown`.
- The image has no shell. `docker exec` runs `notedown` directly.
- Files you import must be inside the container, for example in the volume. `docker cp` copies them there.

## Check a download

Every release has a `SHA256SUMS` file with the checksum of every other file. `SHA256SUMS.sigstore.json` is its signature: the release workflow of this repository signed it with [cosign](https://docs.sigstore.dev/cosign/system_config/installation/), without a key. You need cosign 3 or later. Check the signature first, then the files:

```sh
cosign verify-blob SHA256SUMS \
  --bundle SHA256SUMS.sigstore.json \
  --certificate-identity-regexp '^https://github.com/jordiboehme/notedown/\.github/workflows/release\.yml@refs/(heads/main|tags/v.*)$' \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com
sha256sum -c SHA256SUMS --ignore-missing
```

On macOS the second command is `shasum -a 256 -c SHA256SUMS --ignore-missing`.

Every release file also has a build provenance attestation. It proves that the file was built by the release workflow of this repository. Check it with the GitHub CLI (`gh` must be signed in):

```sh
gh attestation verify notedown_<ver>_arm64.deb --repo jordiboehme/notedown
gh attestation verify oci://ghcr.io/jordiboehme/notedown:<ver> --repo jordiboehme/notedown
```

Each build also has a CycloneDX SBOM on the releases page (`*.cdx.json`): the list of everything that went into it.
