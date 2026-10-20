#!/usr/bin/env bash
# Run every check this project has. Each one needs a live KDE + PipeWire session,
# so there is no offline variant; they talk to the running service.
#
#   ./check.sh            all of them
#   ./check.sh keepalive  only the ones whose name contains "keepalive"
set -uo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")"
filter="${1:-check_}"
failed=0

for suite in tests/check_*.py; do
    name="$(basename "$suite" .py)"
    case "$name" in
        *"$filter"*) ;;
        *) continue ;;
    esac
    printf '%-22s ' "$name"
    if timeout 300 python3 "$suite" >/tmp/monitor-audio-check.log 2>&1; then
        echo "pass"
    else
        echo "FAIL  (full output: /tmp/monitor-audio-check.log)"
        tail -20 /tmp/monitor-audio-check.log | sed 's/^/    /'
        failed=$((failed + 1))
    fi
done

if [ "$failed" -eq 0 ]; then
    echo "all suites passed"
else
    echo "$failed suite(s) failed"
fi
exit "$failed"
