# Release notes

One file per release: `v<ver>.md`, for example `v0.1.0.md`. The file is the switch. A merge to main releases when the workspace version has a notes file here and its tag does not exist yet.

## How a release is made

1. **Bump the version in every pinned place.** `scripts/ci/version-pins.sh` lists them and CI fails when one is off:
   - `version` in `[workspace.package]` of `Cargo.toml` and the `version` of every internal path dependency there
   - `Cargo.lock` (`cargo update --workspace`)
   - `version` in `app/src-tauri/Cargo.toml` and `app/src-tauri/Cargo.lock` (`cargo update --workspace` in `app/src-tauri`)
   - `version` in `app/package.json` and both places in `app/package-lock.json` (`npm install` in `app`)

   `bash scripts/ci/version-pins.sh` checks it locally.
2. **Write `release-notes/v<ver>.md`.** It opens with the motto as an H1, then one line that says what the release is about, then the sections: New & Noteworthy, Fixes and Breaking changes. Write "Breaking changes: None." when there are none. Fixes lists only bugs that a user of an earlier release could see. The file becomes the text of the GitHub release, as it is.
3. **Open the pull request and merge it when CI is green.** `bash scripts/release-check.sh` tells you before the merge what main will do.

After the merge, CI on main runs `release-check` after `ci-passed`. It finds the notes file and no tag, and calls `release.yml`. That builds and signs everything, checks every file, creates the tag at the tested commit and publishes the release, the images and the Homebrew files. Nothing is published before every file was checked.

What does not release:

- A version bump without a notes file. CI stays green.
- A notes file for a version that is tagged already. CI stays green.
- A notes file without an H1. CI fails.

A push of a `v<ver>` tag also starts `release.yml`, as a fallback.

## Dry runs

- `gh workflow run release.yml --ref main` builds, signs and notarizes everything under the version `0.0.0-dev` and publishes nothing.
- `gh workflow run release-rehearsal.yml --ref main` calls `release.yml` the way CI does, with the current version, and publishes nothing.

Run them after a change to the release workflow, before the next release.

## Secrets

The secrets live only in the `release` environment (deploy rule: branch main and tags `v*`). `ci.yml` passes them to `release.yml` by name, but they are empty there by design: the calling job has no environment. Each job in `release.yml` that declares `environment: release` gets the real values. Do not move the secrets to the repository to "fix" the empty values, and never use `secrets: inherit`.

If the `release` environment ever gets required reviewers: the approval is per job, not per run. Every job that declares `environment: release` waits for its own approval, so one release asks more than once.
