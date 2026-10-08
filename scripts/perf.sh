#!/usr/bin/env bash
# Runs the notedown cloud benchmarks (the ignored tests in crates/notedown-cloud/tests/it/perf.rs)
# in release mode and prints their result lines.
#
#   scripts/perf.sh               20000 files, every benchmark
#   scripts/perf.sh 2000          2000 files
#   scripts/perf.sh 2000 snapshot only the benchmarks whose name starts with perf_snapshot
#
# Each benchmark builds its own account in a temp folder (about 190 KiB per file on disk) and
# removes it again.
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
files="${1:-20000}"
filter="perf::perf_${2:-}"
echo "machine: $(sysctl -n machdep.cpu.brand_string 2>/dev/null || uname -m), files: $files"
cd "$root"
NOTEDOWN_PERF_FILES="$files" cargo test --release -p notedown-cloud --test it "$filter" \
    -- --ignored --nocapture --test-threads=1 2>&1 | grep -oE '(perf|setup) [a-z_0-9]+ [a-z]+=.*|test result.*'
