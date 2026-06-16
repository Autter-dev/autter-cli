#!/bin/bash

set -euo pipefail

# Parse arguments
BUILD_TYPE="debug"
if [[ "$#" -gt 0 && "$1" == "--release" ]]; then
    BUILD_TYPE="release"
fi

# Clean up old dev-symlinks.sh PATH export if present
_detect_shell_profile() {
    if [[ "${SHELL:-}" == */zsh ]]; then
        if [[ -f "$HOME/.zshrc" ]]; then
            echo "$HOME/.zshrc"
        else
            echo "$HOME/.zprofile"
        fi
    elif [[ "${SHELL:-}" == */bash ]]; then
        if [[ "$(uname)" == "Darwin" ]]; then
            if [[ -f "$HOME/.bash_profile" ]]; then
                echo "$HOME/.bash_profile"
            else
                echo "$HOME/.bashrc"
            fi
        else
            if [[ -f "$HOME/.bashrc" ]]; then
                echo "$HOME/.bashrc"
            else
                echo "$HOME/.bash_profile"
            fi
        fi
    else
        echo "$HOME/.profile"
    fi
}

_PROFILE="$(_detect_shell_profile)"
if [[ -f "$_PROFILE" ]] && grep -q '\.autter-local-dev/gitwrap/bin' "$_PROFILE"; then
    sed -i.bak '/# autter local dev/d' "$_PROFILE"
    sed -i.bak '/\.autter-local-dev\/gitwrap\/bin/d' "$_PROFILE"
    rm -f "$_PROFILE.bak"
    echo "Cleaned up old autter local dev PATH export from $_PROFILE"
fi

# Build the binary
echo "Building $BUILD_TYPE binary..."
if [[ "$BUILD_TYPE" == "release" ]]; then
    cargo build --release
else
    cargo build
fi

BUILT_BIN="$(pwd)/target/$BUILD_TYPE/autter"
REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"

# Bootstrap ~/.autter (bin dir, config.json, PATH in profile) if it isn't set up yet.
# Uses the local install.sh with the freshly built binary instead of downloading a
# release, so local dev works without a published GitHub release.
if [[ ! -d "$HOME/.autter/bin" ]] || [[ ! -f "$HOME/.autter/config.json" ]] || \
   { [[ -f "$_PROFILE" ]] && ! grep -q '\.autter/bin' "$_PROFILE"; } || \
   { [[ ! -f "$_PROFILE" ]]; }; then
    echo "Running autter installer (local binary)..."
    AUTTER_LOCAL_BINARY="$BUILT_BIN" bash "$REPO_ROOT/install.sh"
fi

# Install binary via temp file + atomic mv to avoid macOS code signature cache
# issues: direct cp reuses the inode, causing syspolicyd to fail validating the
# changed binary, leaving the process stuck in launched-suspended state unkillably.
echo "Installing binary to ~/.autter/bin/autter..."
TMP_BIN="$HOME/.autter/bin/autter.tmp.$$"
cp "target/$BUILD_TYPE/autter" "$TMP_BIN"
mv -f "$TMP_BIN" "$HOME/.autter/bin/autter"
chmod +x "$HOME/.autter/bin/autter"

# Run install hooks
echo "Running install hooks..."
~/.autter/bin/autter install

echo "Done!"
