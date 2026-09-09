#!/bin/sh
# autofork bootstrap: put the right autofork + autofork-daemon binaries into
# ${CLAUDE_PLUGIN_DATA}/bin. Prefers a prebuilt GitHub release, falls back
# to building from source with cargo. Never fails the calling hook.

set -u

REPO="TheUnderdev/autofork"
DATA="${CLAUDE_PLUGIN_DATA:-}"
ROOT="${CLAUDE_PLUGIN_ROOT:-}"
if [ -z "$DATA" ] || [ -z "$ROOT" ]; then
    echo "autofork bootstrap: CLAUDE_PLUGIN_DATA/CLAUDE_PLUGIN_ROOT unset" >&2
    exit 0
fi
# Windows: Claude Code passes both as `C:\Users\…`. Git Bash opens such
# paths, but GNU tar unescapes the backslashes in a `-C` directory and fails
# ("C\:\\Users…: Cannot open"). `C:/Users/…` is the spelling every tool here
# accepts, so both are normalized once, up front.
if command -v cygpath >/dev/null 2>&1; then
    DATA=$(cygpath -m "$DATA")
    ROOT=$(cygpath -m "$ROOT")
fi
mkdir -p "$DATA" 2>/dev/null

# Wanted version = the plugin's own version.
WANT=$(sed -n 's/.*"version"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' \
    "$ROOT/.claude-plugin/plugin.json" | head -n1)
if [ -z "$WANT" ]; then
    echo "autofork bootstrap: cannot read plugin version" >&2
    exit 0
fi

OS=$(uname -s)
ARCH=$(uname -m)
# Windows (Git Bash / MSYS): binaries carry .exe, and `uname -s` reads
# MINGW64_NT-10.0-… or MSYS_NT-…
# GNU tar (Git for Windows) reads a `C:/…` path as `host:file` and fails with
# "Cannot connect to C"; --force-local makes it a local file again. bsdtar
# (macOS) has no such flag and never sees such a path.
case "$OS" in
    MINGW* | MSYS* | CYGWIN*) EXE=".exe"; TAR_LOCAL="--force-local" ;;
    *) EXE=""; TAR_LOCAL="" ;;
esac

HAVE=""
if [ -x "$DATA/bin/autofork$EXE" ]; then
    HAVE=$("$DATA/bin/autofork$EXE" --version 2>/dev/null | awk '{print $2}')
fi
if [ "$HAVE" = "$WANT" ]; then
    exit 0
fi
echo "autofork bootstrap: want v$WANT, have '${HAVE:-none}'"

case "$OS-$ARCH" in
    Darwin-arm64) TARGET="aarch64-apple-darwin" ;;
    Darwin-x86_64) TARGET="x86_64-apple-darwin" ;;
    Linux-x86_64) TARGET="x86_64-unknown-linux-musl" ;;
    Linux-aarch64 | Linux-arm64) TARGET="aarch64-unknown-linux-musl" ;;
    MINGW*-x86_64 | MSYS*-x86_64 | CYGWIN*-x86_64) TARGET="x86_64-pc-windows-gnu" ;;
    MINGW*-aarch64 | MSYS*-aarch64 | MINGW*-arm64 | MSYS*-arm64) TARGET="aarch64-pc-windows-gnullvm" ;;
    *) TARGET="" ;;
esac

TMP="$DATA/tmp"
rm -rf "$TMP"
mkdir -p "$TMP"

install_bins() {
    # $1 = dir holding new autofork + autofork-daemon
    mkdir -p "$DATA/bin.new"
    cp "$1/autofork$EXE" "$1/autofork-daemon$EXE" "$DATA/bin.new/" || return 1
    chmod +x "$DATA/bin.new/autofork$EXE" "$DATA/bin.new/autofork-daemon$EXE"
    rm -rf "$DATA/bin.old"
    if [ -d "$DATA/bin" ]; then
        mv "$DATA/bin" "$DATA/bin.old"
    fi
    mv "$DATA/bin.new" "$DATA/bin" || return 1
    rm -rf "$DATA/bin.old" "$TMP"
    echo "autofork bootstrap: installed v$WANT"
    # A running daemon keeps its old inode; the CLI's version handshake
    # retires it on the next session-start/stop.
    return 0
}

checksum_ok() {
    # $1 = file, $2 = .sha256 file (format: "<hex>  <name>")
    if command -v sha256sum >/dev/null 2>&1; then
        (cd "$TMP" && sha256sum -c "$(basename "$2")" >/dev/null 2>&1)
    elif command -v shasum >/dev/null 2>&1; then
        (cd "$TMP" && shasum -a 256 -c "$(basename "$2")" >/dev/null 2>&1)
    else
        echo "autofork bootstrap: no sha256 tool; skipping verification" >&2
        return 0
    fi
}

# --- Attempt 1: prebuilt release artifact ---
if [ -n "$TARGET" ] && command -v curl >/dev/null 2>&1; then
    ASSET="autofork-v$WANT-$TARGET.tar.gz"
    URL="https://github.com/$REPO/releases/download/v$WANT/$ASSET"
    echo "autofork bootstrap: fetching $URL"
    if curl -fsSL --retry 2 -o "$TMP/$ASSET" "$URL" &&
        curl -fsSL --retry 2 -o "$TMP/$ASSET.sha256" "$URL.sha256"; then
        if checksum_ok "$TMP/$ASSET" "$TMP/$ASSET.sha256"; then
            # shellcheck disable=SC2086
            if tar $TAR_LOCAL -xzf "$TMP/$ASSET" -C "$TMP" && install_bins "$TMP/bin"; then
                exit 0
            fi
        else
            echo "autofork bootstrap: checksum mismatch for $ASSET" >&2
        fi
    else
        echo "autofork bootstrap: download failed (no artifact for $TARGET?)" >&2
    fi
fi

# --- Attempt 2: build from source ---
if command -v cargo >/dev/null 2>&1 && command -v git >/dev/null 2>&1; then
    echo "autofork bootstrap: building v$WANT from source (this can take a few minutes)"
    SRC="$TMP/src"
    if git clone --quiet --depth 1 --branch "v$WANT" "https://github.com/$REPO.git" "$SRC" &&
        (cd "$SRC" && cargo build --quiet --release -p autofork -p autofork-daemon) &&
        install_bins "$SRC/target/release"; then
        exit 0
    fi
    echo "autofork bootstrap: source build failed" >&2
fi

echo "autofork bootstrap: could not install. Install manually with:" >&2
echo "  cargo install --git https://github.com/$REPO autofork autofork-daemon --root <dir>" >&2
echo "and place both binaries in $DATA/bin/" >&2
exit 0
