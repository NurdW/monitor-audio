#!/usr/bin/env bash
# Run every check this project has.
#
# The unit tests need no desktop, but several of them shell out to pactl, pw-dump
# and systemd-analyze, so they expect a live PipeWire session like everything
# else here.
set -uo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/rust"

if ! command -v cargo >/dev/null 2>&1 && [ -x "$HOME/.cargo/bin/cargo" ]; then
    export PATH="$HOME/.cargo/bin:$PATH"
fi

if ! command -v cargo >/dev/null 2>&1; then
    echo "cargo is not installed. Install Rust with: rustup toolchain install stable" >&2
    exit 1
fi

failed=0

printf 'cargo test  '
if timeout 900 cargo test >/tmp/monitor-audio-check.log 2>&1; then
    grep -c '^test result: ok' /tmp/monitor-audio-check.log | tr -d '\n'
    echo " suites passed"
else
    echo "FAIL  (full output: /tmp/monitor-audio-check.log)"
    tail -25 /tmp/monitor-audio-check.log | sed 's/^/    /'
    failed=$((failed + 1))
fi

printf 'clippy      '
if timeout 600 cargo clippy --all-targets >/tmp/monitor-audio-clippy.log 2>&1; then
    count="$(grep -cE '^(warning|error)' /tmp/monitor-audio-clippy.log)"
    if [ "$count" -eq 0 ]; then
        echo "clean"
    else
        echo "reported $count problem(s)"
        grep -E '^(warning|error)' -A6 /tmp/monitor-audio-clippy.log | head -20 | sed 's/^/    /'
        failed=$((failed + 1))
    fi
else
    echo "FAIL  (full output: /tmp/monitor-audio-clippy.log)"
    failed=$((failed + 1))
fi

if [ "$failed" -eq 0 ]; then
    echo "all checks passed"
else
    echo "$failed check(s) failed"
fi
exit "$failed"
