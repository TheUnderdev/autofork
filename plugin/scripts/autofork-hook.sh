#!/bin/sh
# autofork hook shim: exec the persistent-data binary (it survives plugin
# updates), or kick off a background bootstrap when it's missing. `exec`
# propagates the binary's exit code — the Stop hook (`stop-wait`) exits 2 to
# wake the session; a missing-binary bootstrap path exits 0 (swallows it).

# Recursion guard, kept as zero-cost defense in depth. Fork subagents emit
# SubagentStop (not Stop) so they never reach the trigger path; but if these
# vars were ever present we bail before any binary exec. The Rust entrypoint
# enforces the same guard as the real defense.
if [ -n "${AUTOFORK_FORK}" ] || [ -n "${AUTOFORK_SESSION_ID}" ]; then
    exit 0
fi

BIN="${CLAUDE_PLUGIN_DATA}/bin/autofork"
# Windows (Git Bash runs this shim there): the binary carries an .exe suffix.
[ -x "$BIN" ] || [ ! -x "$BIN.exe" ] || BIN="$BIN.exe"
if [ -x "$BIN" ]; then
    exec "$BIN" hook "$1"
fi

# First run (or broken install): bootstrap in the background, at most once
# per hour, and swallow this event.
STAMP="${CLAUDE_PLUGIN_DATA}/bootstrap-attempt"
mkdir -p "${CLAUDE_PLUGIN_DATA}" 2>/dev/null
if [ ! -f "$STAMP" ] || [ -n "$(find "$STAMP" -mmin +60 2>/dev/null)" ]; then
    touch "$STAMP"
    nohup "${CLAUDE_PLUGIN_ROOT}/scripts/bootstrap.sh" >>"${CLAUDE_PLUGIN_DATA}/bootstrap.log" 2>&1 &
fi
exit 0
