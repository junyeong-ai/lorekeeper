#!/usr/bin/env bash
# Say out loud whatever `lore` says is due.
#
# The split every script here keeps: the binary answers what is due and retires it — one
# deterministic question — and the platform's own notifier is what turns a line into something a
# person sees. Shelling out to `osascript` from inside `lore` would put a macOS dependency into
# a binary that also runs on Linux, the same reason its checksums are computed in-process rather
# than through a `shasum` that may not exist.
#
# Run by a timer. `lore schedule --format launchd` emits the entry; a reminder due while the
# machine slept is said late rather than lost, because nothing but firing retires one.
set -euo pipefail

LORE_BIN="${LORE_BIN:-lore}"

# AppleScript quoting is NOT shell quoting. `${text@Q}` produces a shell-quoted string, and
# AppleScript has no single-quoted string literal — so every notification was a syntax error and
# nothing was ever shown. Escape a backslash and a double quote, then wrap in double quotes,
# which is the whole of AppleScript's string grammar.
applescript_string() {
    local escaped=${1//\\/\\\\}
    printf '"%s"' "${escaped//\"/\\\"}"
}

notify() {
    local text="$1"
    if command -v osascript >/dev/null 2>&1; then
        osascript -e "display notification $(applescript_string "$text") with title \"lore\"" \
            >/dev/null 2>&1 && return
    fi
    if command -v notify-send >/dev/null 2>&1; then
        notify-send "lore" "$text" >/dev/null 2>&1 && return
    fi
    # No notifier: say it on stdout, where a timer's log keeps it. Silence would be the one
    # outcome worse than a late reminder.
    printf '%s\n' "$text"
}

# Read the whole answer before saying any of it, so a non-zero exit is SEEN. Piped through
# `done < <(…)` the status belongs to the loop and vanishes under `set -e` — the timer then
# reports success while saying nothing, which is silence dressed as a quiet day.
if ! due=$("$LORE_BIN" ${LORE_CONFIG:+--config "$LORE_CONFIG"} task remind due); then
    echo "lore task remind due failed — no reminder was said" >&2
    exit 1
fi

while IFS= read -r line; do
    [ -n "$line" ] || continue
    notify "$line"
done <<<"$due"
