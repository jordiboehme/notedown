#!/usr/bin/env node
// Builds the notedown program in release mode (default features, so with OCR)
// in the root workspace and copies it to src-tauri/binaries/ under the name
// Tauri's `externalBin` expects: notedown-<target triple>. beforeDevCommand
// and beforeBuildCommand run this first. Paths resolve from this file, never
// from the working directory.

import { execFileSync } from "node:child_process";
import {
  chmodSync,
  copyFileSync,
  existsSync,
  mkdirSync,
  readdirSync,
  writeFileSync,
} from "node:fs";
import { homedir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const scriptDir = dirname(fileURLToPath(import.meta.url));
const appDir = resolve(scriptDir, "..");
const repoRoot = resolve(appDir, "..");

// The bundle resource of tauri.conf.json. A placeholder never replaces a real
// file; the release checks that the bundle carries the real one.
const notices = join(appDir, "src-tauri", "resources", "THIRD-PARTY-NOTICES.txt");
if (!existsSync(notices)) {
  mkdirSync(dirname(notices), { recursive: true });
  writeFileSync(
    notices,
    "Placeholder of a local build. The release build of Notedown puts the third-party notices here.\n",
  );
  console.log(`[prepare-sidecar] wrote a placeholder to ${notices}`);
}

// cargo: $CARGO, then PATH, then ~/.cargo/bin and the rustup toolchains, for
// shells that do not put cargo on PATH.
function locateCargo() {
  if (process.env.CARGO) return process.env.CARGO;
  try {
    execFileSync("cargo", ["--version"], { stdio: "ignore" });
    return "cargo";
  } catch {
    // try the places below
  }
  const candidates = [join(homedir(), ".cargo", "bin", "cargo")];
  const toolchains = join(homedir(), ".rustup", "toolchains");
  if (existsSync(toolchains)) {
    for (const tc of readdirSync(toolchains)) {
      candidates.push(join(toolchains, tc, "bin", "cargo"));
    }
  }
  for (const candidate of candidates) {
    if (existsSync(candidate)) return candidate;
  }
  throw new Error(
    "cargo is not on PATH, in $CARGO, in ~/.cargo/bin or in ~/.rustup. Install Rust or set CARGO.",
  );
}

// The target triple: `--target <triple>` (passed on to cargo), else Tauri's
// TAURI_ENV_TARGET_TRIPLE, else the host of rustc.
function resolveTriple(cargo, childEnv) {
  const argv = process.argv.slice(2);
  const flag = argv.indexOf("--target");
  if (flag !== -1 && argv[flag + 1]) {
    return { triple: argv[flag + 1], explicit: true };
  }
  if (process.env.TAURI_ENV_TARGET_TRIPLE) {
    return { triple: process.env.TAURI_ENV_TARGET_TRIPLE, explicit: true };
  }
  const rustc = cargo === "cargo" ? "rustc" : join(dirname(cargo), "rustc");
  const out = execFileSync(rustc, ["-vV"], {
    encoding: "utf8",
    env: childEnv,
    cwd: repoRoot,
  });
  const match = out.match(/^host:\s*(.+)$/m);
  if (!match) {
    throw new Error("cannot read the host triple from `rustc -vV`");
  }
  return { triple: match[1].trim(), explicit: false };
}

const cargo = locateCargo();

// A cargo found off PATH still calls its sibling rustc, so its folder goes on
// PATH for every child process.
const childEnv =
  cargo === "cargo"
    ? process.env
    : { ...process.env, PATH: `${dirname(cargo)}:${process.env.PATH ?? ""}` };

const { triple, explicit } = resolveTriple(cargo, childEnv);

const buildArgs = ["build", "--release", "--locked", "-p", "notedown"];
if (explicit) buildArgs.push("--target", triple);

console.log(`[prepare-sidecar] building notedown for ${triple}`);
execFileSync(cargo, buildArgs, {
  cwd: repoRoot,
  stdio: "inherit",
  env: childEnv,
});

const releaseDir = explicit
  ? join(repoRoot, "target", triple, "release")
  : join(repoRoot, "target", "release");
const src = join(releaseDir, "notedown");
if (!existsSync(src)) {
  throw new Error(`the built program is not at ${src}`);
}

const destDir = join(appDir, "src-tauri", "binaries");
mkdirSync(destDir, { recursive: true });
const dest = join(destDir, `notedown-${triple}`);
copyFileSync(src, dest);
chmodSync(dest, 0o755);

console.log(`[prepare-sidecar] copied ${src} to ${dest}`);
