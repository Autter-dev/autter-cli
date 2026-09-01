#!/bin/bash

set -euo pipefail
IFS=$'\n\t'

# ============================================================
# Ensure HOME is set when running via MDMs (e.g. JAMF) or other environments where HOME may be unbound.
# ============================================================
INSTALL_USER=""

if [ -z "${HOME:-}" ]; then
    if command -v scutil >/dev/null 2>&1; then
        CURRENT_USER=$( /usr/sbin/scutil <<< "show State:/Users/ConsoleUser" | awk '/Name :/ { print $3 }' || true )
        if [ -n "${CURRENT_USER:-}" ] && [ "$CURRENT_USER" != "loginwindow" ] && [ "$CURRENT_USER" != "_mbsetupuser" ]; then
            export HOME=$( /usr/bin/dscl . -read "/Users/$CURRENT_USER" NFSHomeDirectory | awk '{print $2}' )
            INSTALL_USER="$CURRENT_USER"
        else
            echo "Error: No console user logged in. Deferring installation." >&2
            exit 1
        fi
    elif id -un >/dev/null 2>&1; then
        INSTALL_USER="$(id -un)"
        export HOME=$(getent passwd "$INSTALL_USER" | cut -d: -f6)
        if [ -z "$HOME" ]; then
            export HOME="/root"
        fi
    else
        export HOME="/root"
    fi
fi

# Ensure SHELL is set (also may be unbound in JAMF)
if [ -z "${SHELL:-}" ]; then
    if command -v zsh >/dev/null 2>&1; then
        SHELL="$(command -v zsh)"
    elif command -v bash >/dev/null 2>&1; then
        SHELL="$(command -v bash)"
    else
        SHELL="/bin/sh"
    fi
    export SHELL
fi

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[0;33m'
NC='\033[0m' # No Color

# Release-fill placeholders. The release workflow blindly string-replaces each
# placeholder token EVERYWHERE in this file, so the guards below compare
# against *_SENTINEL values built by concatenation — those survive the fill.
# Comparing against the literal token would self-destruct on fill: the pinned
# copy would ignore its version pin and skip checksum verification.

# Repository ("owner/repo"); the sentinel defaults to the canonical repo.
REPO="__REPO_PLACEHOLDER__"
REPO_SENTINEL='__REPO_''PLACEHOLDER__'
if [ "$REPO" = "$REPO_SENTINEL" ]; then
    REPO="autter-dev/autter-cli"
fi

# Version pin (e.g. "v1.6.8") in release copies; the sentinel means "latest".
PINNED_VERSION="__VERSION_PLACEHOLDER__"
VERSION_SENTINEL='__VERSION_''PLACEHOLDER__'

# Pipe-separated "sha256  filename" entries in release copies. Public installer
# copies replace the sentinel by downloading the release's checksums.txt.
EMBEDDED_CHECKSUMS="__CHECKSUMS_PLACEHOLDER__"
CHECKSUMS_SENTINEL='__CHECKSUMS_''PLACEHOLDER__'

# Print helpers use printf, not `echo -e`: when this script is run with
# `curl … | sh` the shebang is bypassed, and POSIX-mode shells (dash,
# macOS /bin/sh) print `echo -e`'s "-e" literally.

# Function to print error messages
error() {
    printf '%b\n' "${RED}Error: $1${NC}" >&2
    exit 1
}

warn() {
    printf '%b\n' "${YELLOW}Warning: $1${NC}" >&2
}

# Function to print success messages
success() {
    printf '%b\n' "${GREEN}$1${NC}"
}

# Function to verify checksum of downloaded binary
verify_checksum() {
    local file="$1"
    local binary_name="$2"

    # Local developer installs do not download a release artifact.
    if [ -n "${AUTTER_LOCAL_BINARY:-}" ]; then
        return 0
    fi

    if [ "$EMBEDDED_CHECKSUMS" = "$CHECKSUMS_SENTINEL" ]; then
        error "Release checksums were not loaded; refusing to install $binary_name"
    fi

    # Extract expected checksum for this binary
    local expected=""
    local old_ifs="$IFS"
    IFS='|' read -ra CHECKSUM_ENTRIES <<< "$EMBEDDED_CHECKSUMS"
    IFS="$old_ifs"
    for entry in "${CHECKSUM_ENTRIES[@]}"; do
        if [[ "$entry" =~ ^[[:xdigit:]]{64}[[:space:]]+$binary_name$ ]]; then
            expected=$(echo "$entry" | awk '{print $1}')
            break
        fi
    done

    if [ -z "$expected" ]; then
        error "No checksum found for $binary_name"
    fi

    # Calculate actual checksum
    local actual=""
    if command -v sha256sum >/dev/null 2>&1; then
        actual=$(sha256sum "$file" | awk '{print $1}')
    elif command -v shasum >/dev/null 2>&1; then
        actual=$(shasum -a 256 "$file" | awk '{print $1}')
    else
        rm -f "$file" 2>/dev/null || true
        error "Neither sha256sum nor shasum is available; refusing to install an unverified executable"
    fi

    expected=$(printf '%s' "$expected" | tr '[:upper:]' '[:lower:]')
    actual=$(printf '%s' "$actual" | tr '[:upper:]' '[:lower:]')
    if [ "$expected" != "$actual" ]; then
        rm -f "$file" 2>/dev/null || true
        error "Checksum verification failed for $binary_name\nExpected: $expected\nActual:   $actual"
    fi

    success "Checksum verified for $binary_name"
}

# Function to detect all shells with existing config files
# Returns shell configurations in format: "shell_name|config_file" (one per line)
detect_all_shells() {
    local shells=""

    # Check for bash configs. Interactive non-login shells read ~/.bashrc while
    # login shells (macOS Terminal/iTerm, SSH sessions) read ~/.bash_profile,
    # so configure BOTH when both exist -- the env file they source guards
    # against duplicate PATH entries, making the double-source harmless.
    if [ -f "$HOME/.bashrc" ]; then
        shells="${shells}bash|$HOME/.bashrc\n"
    fi
    if [ -f "$HOME/.bash_profile" ]; then
        shells="${shells}bash|$HOME/.bash_profile\n"
    fi
    
    # Check for zsh config
    if [ -f "$HOME/.zshrc" ]; then
        shells="${shells}zsh|$HOME/.zshrc\n"
    fi
    
    # Check for fish config
    if [ -f "$HOME/.config/fish/config.fish" ]; then
        shells="${shells}fish|$HOME/.config/fish/config.fish\n"
    fi
    
    # If no configs found, fall back to $SHELL detection and create config for that shell only
    if [ -z "$shells" ]; then
        local login_shell=""
        if [ -n "${SHELL:-}" ]; then
            login_shell=$(basename "$SHELL")
        fi
        case "$login_shell" in
            fish)
                shells="fish|$HOME/.config/fish/config.fish"
                ;;
            zsh)
                shells="zsh|$HOME/.zshrc"
                ;;
            bash|*)
                shells="bash|$HOME/.bashrc"
                ;;
        esac
    fi
    
    # Remove trailing newline and output
    printf '%b' "$shells" | sed '/^$/d'
}

# Write the sourceable env files that shell rc lines point at. Sourcing one of
# these is the single reliable way to make autter available in an already-open
# shell, regardless of which rc file the installer edited or what the user's
# rc files do to PATH afterwards. Written unconditionally so upgrades from
# older installs (which appended a raw `export PATH=...` line) get them too.
write_env_files() {
    # POSIX sh version, sourced by bash/zsh (and safe under dash).
    cat > "$HOME/.autter/env" << 'ENV_EOF'
#!/bin/sh
# autter shell setup: prepends ~/.autter/bin to PATH once per shell.
# Sourced by shell rc files; safe to source repeatedly.
case ":${PATH}:" in
    *:"$HOME/.autter/bin":*)
        ;;
    *)
        export PATH="$HOME/.autter/bin:$PATH"
        # Forget cached command lookups so shells that hash PATH results
        # (bash) resolve autter immediately after sourcing this file.
        hash -r 2>/dev/null || true
        ;;
esac
ENV_EOF

    # Fish version. fish_add_path is itself idempotent (fish >= 3.2); the
    # fallback covers older fish releases.
    cat > "$HOME/.autter/env.fish" << 'ENV_FISH_EOF'
# autter shell setup: prepends ~/.autter/bin to PATH.
# Sourced by config.fish; safe to source repeatedly.
if type -q fish_add_path
    fish_add_path -g "$HOME/.autter/bin"
else if not contains -- "$HOME/.autter/bin" $PATH
    set -gx PATH "$HOME/.autter/bin" $PATH
end
ENV_FISH_EOF
}

# ============================================================
# Warn when installing as root/sudo (not recommended).
# Running as root creates files that normal-user processes
# cannot access, causing persistent daemon lock failures.
# ============================================================
if [ "$(id -u)" = "0" ] && [ "${AUTTER_ALLOW_SUPERUSER:-}" != "1" ]; then
    # Auto-allow in CI environments, MDM deployments (JAMF, etc.),
    # and daemon-triggered self-updates (AUTTER_DAEMON_UPGRADE is set internally by the upgrade command)
    IS_CI_OR_MDM=false
    if [ -n "${CI:-}" ] || [ -n "${GITHUB_ACTIONS:-}" ] || [ -n "${GITLAB_CI:-}" ] \
        || [ -n "${JENKINS_URL:-}" ] || [ -n "${BUILDKITE:-}" ] || [ -n "${CIRCLECI:-}" ] \
        || [ -n "${CODEBUILD_BUILD_ID:-}" ] || [ -n "${AGENT_OS:-}" ] \
        || [ -n "${KUBERNETES_SERVICE_HOST:-}" ] || [ -n "${INSTALL_USER:-}" ] \
        || [ -n "${AUTTER_DAEMON_UPGRADE:-}" ] \
        || [ -n "${container:-}" ] || [ -f "/.dockerenv" ]; then
        IS_CI_OR_MDM=true
    fi

    if [ "$IS_CI_OR_MDM" = "false" ]; then
        echo ""
        printf '%b\n' "${YELLOW}Warning: installing autter as root/sudo is not recommended.${NC}"
        echo ""
        echo "Running with elevated privileges creates files owned by root that become"
        echo "inaccessible to your normal user account, causing persistent daemon lock"
        echo "failures. A future version may refuse to install in this configuration."
        echo ""
        echo "To suppress this warning, either:"
        echo "  - Run this installer as your normal user (recommended), or"
        echo "  - Set AUTTER_ALLOW_SUPERUSER=1"
        echo ""
    fi
    # Propagate to child autter invocations (install-hooks, exchange-nonce, login)
    export AUTTER_ALLOW_SUPERUSER=1
fi

# Detect OS and architecture
OS=$(uname -s | tr '[:upper:]' '[:lower:]')
ARCH=$(uname -m)


# Map architecture to binary name
case $ARCH in
    "x86_64")
        ARCH="x64"
        ;;
    "aarch64"|"arm64")
        ARCH="arm64"
        ;;
    *)
        error "Unsupported architecture: $ARCH"
        ;;
esac

# Minimum glibc for Linux release binaries (built on Ubuntu 22.04).
MIN_GLIBC_MAJOR=2
MIN_GLIBC_MINOR=35

# Require git before downloading — autter wraps git and cannot function without it.
check_git() {
    if ! command -v git >/dev/null 2>&1; then
        error "git is required but not found. Install git 2.22 or newer, then re-run the installer."
    fi
}

# Linux release binaries need glibc 2.35+ (Ubuntu 22.04). Ubuntu 20.04 / older WSL2
# distros fail at runtime with GLIBC_2.32+ symbol errors — catch that up front.
check_linux_glibc() {
    if [ "$OS" != "linux" ] || [ -n "${AUTTER_LOCAL_BINARY:-}" ]; then
        return 0
    fi

    if ! command -v ldd >/dev/null 2>&1; then
        return 0
    fi

    local glibc_version
    glibc_version=$(ldd --version 2>&1 | head -n1 | grep -oE '[0-9]+\.[0-9]+' | head -n1)
    if [ -z "$glibc_version" ]; then
        return 0
    fi

    local major minor
    major=${glibc_version%%.*}
    minor=${glibc_version#*.}

    if [ "$major" -lt "$MIN_GLIBC_MAJOR" ] \
        || { [ "$major" -eq "$MIN_GLIBC_MAJOR" ] && [ "$minor" -lt "$MIN_GLIBC_MINOR" ]; }; then
        error "Unsupported glibc version ($glibc_version). autter requires glibc ${MIN_GLIBC_MAJOR}.${MIN_GLIBC_MINOR} or newer (Ubuntu 22.04+, Debian 12+, Fedora 36+).

On Ubuntu 20.04 or older WSL2 distros, use a newer base image or run inside Docker:
  docker run -it --rm -v \"\$PWD\":/work -w /work ubuntu:22.04 bash
  # then re-run this installer inside the container"
    fi
}

# Fail the install when the downloaded binary cannot execute (glibc mismatch, etc.).
verify_binary_runs() {
    local bin="$1"
    local output
    if output=$("$bin" --version 2>&1); then
        printf '%s' "$output"
        return 0
    fi

    rm -f "$bin" 2>/dev/null || true
    if [ "$OS" = "linux" ] && printf '%s' "$output" | grep -q 'GLIBC_'; then
        error "The autter binary could not run on this system (incompatible glibc).

$output

autter requires glibc ${MIN_GLIBC_MAJOR}.${MIN_GLIBC_MINOR} or newer (Ubuntu 22.04+). On Ubuntu 20.04 / older WSL2, switch to a newer distro or use Docker:
  docker run -it --rm -v \"\$PWD\":/work -w /work ubuntu:22.04 bash"
    fi
    error "The autter binary could not run on this system:

$output"
}

check_git
check_linux_glibc

# Map OS to binary name
case $OS in
    "darwin")
        OS="macos"
        ;;
    "linux")
        OS="linux"
        ;;
    mingw*|msys*|cygwin*)
        # Git Bash / MSYS2 / Cygwin: there is no build for these environments,
        # and a bare "unsupported OS" error leaves Windows users stranded —
        # spell out the two supported paths instead.
        printf '%b\n' "${RED}Error: this installer cannot run in Git Bash (or MSYS2/Cygwin).${NC}" >&2
        echo "" >&2
        echo "Install autter on Windows in one of two ways:" >&2
        echo "" >&2
        echo "  Native Windows - run the PowerShell installer. It works from PowerShell," >&2
        echo "  Command Prompt, and Git Bash, and 'autter' is available in all of them" >&2
        echo "  afterwards:" >&2
        echo "" >&2
        echo "    powershell -NoProfile -ExecutionPolicy Bypass -Command \"irm https://api.autter.dev/install.ps1 | iex\"" >&2
        echo "" >&2
        echo "  WSL - open a WSL terminal and re-run this same command there." >&2
        echo "" >&2
        echo "Install autter in the environment where your coding agents run: agents" >&2
        echo "launched from Windows need the native install, agents inside WSL need" >&2
        echo "the WSL install. If you copied this command from the Autter dashboard," >&2
        echo "switch the OS tab to Windows to keep the automatic sign-in." >&2
        exit 1
        ;;
    *)
        error "Unsupported operating system: $OS"
        ;;
esac

# Determine binary name
BINARY_NAME="autter-${OS}-${ARCH}"

# Determine release tag
# Priority: 1. Local binary override, 2. Pinned version (for release builds), 3. Environment variable, 4. "latest"
if [ -n "${AUTTER_LOCAL_BINARY:-}" ]; then
    RELEASE_TAG="local"
    DOWNLOAD_URL=""
elif [ "$PINNED_VERSION" != "$VERSION_SENTINEL" ]; then
    # Version-pinned install script from a release
    RELEASE_TAG="$PINNED_VERSION"
    DOWNLOAD_URL="https://github.com/${REPO}/releases/download/${RELEASE_TAG}/${BINARY_NAME}"
elif [ -n "${AUTTER_RELEASE_TAG:-}" ] && [ "${AUTTER_RELEASE_TAG:-}" != "latest" ]; then
    # Environment variable override
    RELEASE_TAG="$AUTTER_RELEASE_TAG"
    DOWNLOAD_URL="https://github.com/${REPO}/releases/download/${RELEASE_TAG}/${BINARY_NAME}"
else
    # Resolve the latest-release redirect to a concrete tag below.
    RELEASE_TAG="latest"
fi

# Resolve a specific release and load the checksum file produced by release.yml
# before downloading the executable. A missing/malformed checksum is fatal.
if [ -z "${AUTTER_LOCAL_BINARY:-}" ]; then
    CHECKSUMS_TMP=$(mktemp "${TMPDIR:-/tmp}/autter-checksums.XXXXXX") || error "Failed to create checksum temporary file"
    if [ "$RELEASE_TAG" = "latest" ]; then
        if ! LATEST_RELEASE_URL=$(curl --fail --location --silent --show-error \
            --output /dev/null --write-out '%{url_effective}' "https://github.com/${REPO}/releases/latest"); then
            rm -f "$CHECKSUMS_TMP" 2>/dev/null || true
            error "Failed to resolve the latest release"
        fi
        RELEASE_TAG=$(printf '%s' "$LATEST_RELEASE_URL" | sed -n 's#^.*/releases/tag/\([^/?]*\).*$#\1#p')
        [ -n "$RELEASE_TAG" ] || { rm -f "$CHECKSUMS_TMP"; error "Failed to resolve latest release to a specific version"; }
    fi
    CHECKSUMS_URL="https://github.com/${REPO}/releases/download/${RELEASE_TAG}/checksums.txt"
    if ! curl --fail --location --silent --show-error \
        --output "$CHECKSUMS_TMP" "$CHECKSUMS_URL"; then
        rm -f "$CHECKSUMS_TMP" 2>/dev/null || true
        error "Failed to download release checksums"
    fi
    EMBEDDED_CHECKSUMS=$(awk 'NF { printf "%s%s", separator, $0; separator="|" }' "$CHECKSUMS_TMP")
    rm -f "$CHECKSUMS_TMP"
    [ -n "$EMBEDDED_CHECKSUMS" ] || error "Release checksums are empty"
    DOWNLOAD_URL="https://github.com/${REPO}/releases/download/${RELEASE_TAG}/${BINARY_NAME}"
fi

# ============================================================
# Anonymous install ping.
# One fire-and-forget event so we can count installs. Contains only:
# OS, CPU architecture, requested release tag, and whether this run
# is a fresh install or a daemon self-upgrade. No hostname, username,
# paths, or any personal data. Disable with AUTTER_NO_INSTALL_PING=1.
# The API key is a public write-only project token (same one baked
# into release builds for opt-in telemetry).
# ============================================================
INSTALL_PING_API_KEY="phc_aWveMd1bPhuEYtFnCS1G2IHgln3iGQqjfIdkfnuolxI"
INSTALL_PING_HOST="https://us.i.posthog.com"

report_install_ping() {
    # Respect opt-out, and don't count local dev installs
    if [ "${AUTTER_NO_INSTALL_PING:-}" = "1" ] || [ -n "${AUTTER_LOCAL_BINARY:-}" ]; then
        return 0
    fi
    command -v curl >/dev/null 2>&1 || return 0

    local trigger="install"
    if [ -n "${AUTTER_DAEMON_UPGRADE:-}" ]; then
        trigger="upgrade"
    fi

    # Random, unlinkable ID for this ping only
    local ping_id=""
    if command -v uuidgen >/dev/null 2>&1; then
        ping_id=$(uuidgen 2>/dev/null | tr '[:upper:]' '[:lower:]' || true)
    fi
    if [ -z "$ping_id" ]; then
        ping_id=$(od -An -N16 -tx1 /dev/urandom 2>/dev/null | tr -d ' \n' || true)
    fi
    [ -z "$ping_id" ] && ping_id="unknown"

    # RELEASE_TAG can come from an env var; sanitize before embedding in JSON
    local safe_tag
    safe_tag=$(printf '%s' "$RELEASE_TAG" | tr -cd 'A-Za-z0-9._-')

    # Keep daemon self-upgrades quiet; tell interactive installers what is sent
    if [ "$trigger" = "install" ]; then
        echo "Counting this install with an anonymous ping (OS, architecture, and version only)."
        echo "Set AUTTER_NO_INSTALL_PING=1 to disable."
    fi

    curl --silent --max-time 5 --output /dev/null \
        --header 'Content-Type: application/json' \
        --data "{\"api_key\":\"${INSTALL_PING_API_KEY}\",\"event\":\"install_script_run\",\"distinct_id\":\"${ping_id}\",\"properties\":{\"os\":\"${OS}\",\"arch\":\"${ARCH}\",\"release_tag\":\"${safe_tag}\",\"trigger\":\"${trigger}\",\"source\":\"install.sh\"}}" \
        "${INSTALL_PING_HOST}/capture/" >/dev/null 2>&1 &
}

report_install_ping

# Install into the user's bin directory ~/.autter/bin
INSTALL_DIR="$HOME/.autter/bin"

# Create directory if it doesn't exist
mkdir -p "$INSTALL_DIR"

# Download and install
TMP_FILE="${INSTALL_DIR}/autter.tmp.$$"
if [ -n "${AUTTER_LOCAL_BINARY:-}" ]; then
    echo "Using local autter binary (release: ${RELEASE_TAG})..."
    if [ ! -f "$AUTTER_LOCAL_BINARY" ]; then
        error "Local binary not found at $AUTTER_LOCAL_BINARY"
    fi
    cp "$AUTTER_LOCAL_BINARY" "$TMP_FILE"
else
    echo "Downloading autter (release: ${RELEASE_TAG})..."
    if ! curl --fail --location --silent --show-error -o "$TMP_FILE" "$DOWNLOAD_URL"; then
        rm -f "$TMP_FILE" 2>/dev/null || true
        error "Failed to download binary (HTTP error)"
    fi
fi

# Basic validation: ensure file is not empty
if [ ! -s "$TMP_FILE" ]; then
    rm -f "$TMP_FILE" 2>/dev/null || true
    error "Downloaded file is empty"
fi

# Verify before the executable is moved into place or run.
verify_checksum "$TMP_FILE" "$BINARY_NAME"

mv -f "$TMP_FILE" "${INSTALL_DIR}/autter"

# Make executable
chmod +x "${INSTALL_DIR}/autter"

# Remove quarantine attribute on macOS
if [ "$OS" = "macos" ]; then
    xattr -d com.apple.quarantine "${INSTALL_DIR}/autter" 2>/dev/null || true
fi

# Create ~/.local/bin/autter symlink for systems where ~/.local/bin is already on PATH
LOCAL_BIN_DIR="$HOME/.local/bin"
if mkdir -p "$LOCAL_BIN_DIR" 2>/dev/null && ln -sf "${INSTALL_DIR}/autter" "${LOCAL_BIN_DIR}/autter" 2>/dev/null; then
    success "Created symlink at ${LOCAL_BIN_DIR}/autter"
else
    warn "Failed to create ~/.local/bin/autter symlink. This is non-fatal."
fi

# Verify the binary runs before reporting success (catches glibc mismatches, etc.).
INSTALLED_VERSION=$(verify_binary_runs "${INSTALL_DIR}/autter")
success "Successfully installed autter into ${INSTALL_DIR}"
success "You can now run 'autter' from your terminal"
echo "Installed autter ${INSTALLED_VERSION}"

# Login user with install token if provided
NEED_LOGIN=false
if [ -n "${INSTALL_NONCE:-}" ] && [ -n "${API_BASE:-}" ]; then
    if ! ${INSTALL_DIR}/autter exchange-nonce; then
        NEED_LOGIN=true
    fi
fi

echo "Setting up IDE/agent hooks..."
if ! ${INSTALL_DIR}/autter install-hooks; then
    warn "Warning: Failed to set up IDE/agent hooks. Please try running 'autter install-hooks' manually."
else
    success "Successfully set up IDE/agent hooks"
fi

# Write the env files and point every detected shell configuration at them
write_env_files

SHELLS_CONFIGURED=""
SHELLS_ALREADY_CONFIGURED=""
CREATED_SHELL_PATHS=""

while IFS='|' read -r shell_name config_file; do
    [ -z "$shell_name" ] && continue

    # Generate shell-appropriate env-file source line. $HOME is kept literal
    # (single-quoted here) so the rc line survives home-directory moves and
    # dotfile syncing across machines.
    if [ "$shell_name" = "fish" ]; then
        path_cmd='source "$HOME/.autter/env.fish"'
        # Create fish config directory if it doesn't exist (for fallback case)
        config_dir="$(dirname "$config_file")"
        if [ ! -d "$config_dir" ]; then
            mkdir -p "$config_dir"
            CREATED_SHELL_PATHS="${CREATED_SHELL_PATHS}${config_dir}\n"
        fi
    else
        path_cmd='. "$HOME/.autter/env"'
    fi

    # Create config file if it doesn't exist (for fallback case when no configs found)
    if [ ! -f "$config_file" ]; then
        CREATED_SHELL_PATHS="${CREATED_SHELL_PATHS}${config_file}\n"
    fi
    touch "$config_file"

    # Append if not already present. The first grep matches PATH lines written
    # by older installers (expanded install dir); the second matches the
    # env-file source lines written here.
    if ! grep -qsF "$INSTALL_DIR" "$config_file" && ! grep -qsF '.autter/env' "$config_file"; then
        echo "" >> "$config_file"
        echo "# Added by autter installer on $(date)" >> "$config_file"
        echo "$path_cmd" >> "$config_file"
        SHELLS_CONFIGURED="${SHELLS_CONFIGURED}${shell_name}|${config_file}\n"
    else
        SHELLS_ALREADY_CONFIGURED="${SHELLS_ALREADY_CONFIGURED}${shell_name}|${config_file}\n"
    fi
done <<< "$(detect_all_shells)"

# One activation command for the user's login shell. Telling users to source
# a whole rc file is unreliable (the edited rc may belong to a different
# shell, and rc files can rebuild PATH or early-return); sourcing the env
# file always works in an already-open shell.
if [ "$(basename "${SHELL:-}")" = "fish" ]; then
    ACTIVATE_CMD='source "$HOME/.autter/env.fish"'
else
    ACTIVATE_CMD='source "$HOME/.autter/env"'
fi

# Display results to user
if [ -n "$SHELLS_CONFIGURED" ]; then
    echo ""
    echo "Updated shell configurations:"
    printf '%b' "$SHELLS_CONFIGURED" | while IFS='|' read -r shell_name config_file; do
        [ -z "$shell_name" ] && continue
        success "  ✓ $config_file"
    done
fi

if [ -n "$SHELLS_ALREADY_CONFIGURED" ]; then
    echo ""
    echo "Already configured (no changes needed):"
    printf '%b' "$SHELLS_ALREADY_CONFIGURED" | while IFS='|' read -r shell_name config_file; do
        [ -z "$shell_name" ] && continue
        echo "  ✓ $config_file"
    done
fi

if [ -z "$SHELLS_CONFIGURED" ] && [ -z "$SHELLS_ALREADY_CONFIGURED" ]; then
    echo ""
    echo "Could not detect any shell config files."
    echo "Please add the following line to your shell config:"
    echo '  . "$HOME/.autter/env"'
    echo '(for fish: source "$HOME/.autter/env.fish")'
fi

# Fix file ownership when running as root for a different user (MDM deployments)
if [ "$(id -u)" = "0" ] && [ -n "$INSTALL_USER" ]; then
    chown -R "$INSTALL_USER" "$HOME/.autter" 2>/dev/null || true
    if [ -n "$CREATED_SHELL_PATHS" ]; then
        printf '%b' "$CREATED_SHELL_PATHS" | while IFS= read -r created_path; do
            [ -z "$created_path" ] && continue
            chown "$INSTALL_USER" "$created_path" 2>/dev/null || true
        done
    fi
fi

# Walk the user through onboarding: choose local-only vs connecting to the
# Autter platform. When the user opts to connect, this also handles login.
# Under `curl | sh` stdin is the pipe, not the terminal, so hand onboarding
# the real terminal when one is available — the guided prompts then run
# inline instead of asking the user to remember `autter onboard` later.
# Skips itself gracefully in truly non-interactive installs (CI, MDM).
# `[ -r /dev/tty ]` is not enough: the device node can look readable while
# opening it fails (no controlling terminal, e.g. CI or `bash < /dev/null`),
# which used to print a bare "/dev/tty: Device not configured" error. Actually
# try to open it instead.
echo ""
if ( : </dev/tty ) 2>/dev/null; then
    ${INSTALL_DIR}/autter onboard </dev/tty || true
else
    ${INSTALL_DIR}/autter onboard || true
fi

# PATH guidance goes LAST so it is the message left on screen after the
# install: the one thing every fresh install needs next is how to run autter
# in the terminal they already have open. (Onboarding above prints a lot,
# which used to scroll this advice out of view.)
echo ""
case ":${PATH}:" in
    *:"$INSTALL_DIR":*)
        # This process inherited a PATH that already resolves autter, so the
        # parent shell has it too (e.g. an upgrade over an existing install).
        printf '%b\n' "${GREEN}autter is installed and already on your PATH.${NC}"
        ;;
    *)
        printf '%b\n' "${GREEN}autter is installed.${NC} To use it in this terminal right now, run:"
        echo ""
        printf '%b\n' "    ${YELLOW}${ACTIVATE_CMD}${NC}"
        echo ""
        echo "New terminals pick it up automatically. Restart your IDE (not just its"
        echo "terminal tab) so IDE terminals and coding agents see it too."
        ;;
esac
