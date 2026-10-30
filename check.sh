#!/usr/bin/env bash
# Run every check this project has.
#
# The unit tests need no desktop, but several of them shell out to pactl, pw-dump
# and systemd-analyze, so they expect a live PipeWire session like everything
# else here.
set -uo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/rust"

# Build output lives with the project, not in /tmp: a log that survives only
# until the next reboot is no use for reading a failure you were not watching.
logs="$(cd .. && pwd)/build"
mkdir -p "$logs"

if ! command -v cargo >/dev/null 2>&1 && [ -x "$HOME/.cargo/bin/cargo" ]; then
    export PATH="$HOME/.cargo/bin:$PATH"
fi

if ! command -v cargo >/dev/null 2>&1; then
    echo "cargo is not installed. Install Rust with: rustup toolchain install stable" >&2
    exit 1
fi

failed=0

printf 'cargo test  '
if timeout 900 cargo test >"$logs/check.log" 2>&1; then
    grep -c '^test result: ok' "$logs/check.log" | tr -d '\n'
    echo " suites passed"
else
    echo "FAIL  (full output: $logs/check.log)"
    tail -25 "$logs/check.log" | sed 's/^/    /'
    failed=$((failed + 1))
fi

printf 'clippy      '
if timeout 600 cargo clippy --all-targets >"$logs/clippy.log" 2>&1; then
    count="$(grep -cE '^(warning|error)' "$logs/clippy.log")"
    if [ "$count" -eq 0 ]; then
        echo "clean"
    else
        echo "reported $count problem(s)"
        grep -E '^(warning|error)' -A6 "$logs/clippy.log" | head -20 | sed 's/^/    /'
        failed=$((failed + 1))
    fi
else
    echo "FAIL  (full output: $logs/clippy.log)"
    failed=$((failed + 1))
fi

if [ "$failed" -eq 0 ]; then
    echo "all checks passed"
else
    echo "$failed check(s) failed"
fi
exit "$failed"
