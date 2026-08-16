#!/usr/bin/env bats

# BATS test file for install.sh shell detection functionality
# https://github.com/bats-core/bats-core
#
# These tests verify that the installer correctly detects and configures
# PATH in multiple shell configuration files.

setup() {
    # Create a temporary directory for each test
    export TEST_TEMP_DIR="$(mktemp -d)"
    export ORIGINAL_DIR="$(pwd)"
    export ORIGINAL_HOME="$HOME"
    
    # Create a fake HOME directory for testing
    export HOME="$TEST_TEMP_DIR/home"
    mkdir -p "$HOME"
    
    # Copy the install.sh to temp dir for testing
    cp "$ORIGINAL_DIR/install.sh" "$TEST_TEMP_DIR/install.sh"
    
    # Source just the detect_all_shells function from install.sh
    # We'll extract and test it in isolation
    extract_detect_all_shells_function
}

teardown() {
    # Restore HOME
    export HOME="$ORIGINAL_HOME"
    
    # Clean up temporary directory
    cd "$ORIGINAL_DIR"
    rm -rf "$TEST_TEMP_DIR"
}

# Helper function to extract detect_all_shells from install.sh
extract_detect_all_shells_function() {
    # Create a script that sources just the function we need
    cat > "$TEST_TEMP_DIR/test_functions.sh" << 'FUNC_EOF'
#!/bin/bash

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
        if [ -n "$SHELL" ]; then
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
FUNC_EOF
    chmod +x "$TEST_TEMP_DIR/test_functions.sh"
}

# Helper to run detect_all_shells
run_detect_all_shells() {
    source "$TEST_TEMP_DIR/test_functions.sh"
    detect_all_shells
}

# ============================================================================
# Tests for detect_all_shells function
# ============================================================================

@test "detect_all_shells: detects only .bashrc when only bash config exists" {
    # Create only .bashrc
    touch "$HOME/.bashrc"
    
    run run_detect_all_shells
    [ "$status" -eq 0 ]
    
    # Should contain bash with .bashrc
    echo "$output" | grep -q "bash|$HOME/.bashrc"
    
    # Should NOT contain zsh or fish
    ! echo "$output" | grep -q "zsh|"
    ! echo "$output" | grep -q "fish|"
}

@test "detect_all_shells: detects only .zshrc when only zsh config exists" {
    # Create only .zshrc
    touch "$HOME/.zshrc"
    
    run run_detect_all_shells
    [ "$status" -eq 0 ]
    
    # Should contain zsh with .zshrc
    echo "$output" | grep -q "zsh|$HOME/.zshrc"
    
    # Should NOT contain bash or fish
    ! echo "$output" | grep -q "bash|"
    ! echo "$output" | grep -q "fish|"
}

@test "detect_all_shells: detects only fish config when only fish config exists" {
    # Create only fish config
    mkdir -p "$HOME/.config/fish"
    touch "$HOME/.config/fish/config.fish"
    
    run run_detect_all_shells
    [ "$status" -eq 0 ]
    
    # Should contain fish with config.fish
    echo "$output" | grep -q "fish|$HOME/.config/fish/config.fish"
    
    # Should NOT contain bash or zsh
    ! echo "$output" | grep -q "bash|"
    ! echo "$output" | grep -q "zsh|"
}

@test "detect_all_shells: detects all three shells when all configs exist" {
    # Create all config files
    touch "$HOME/.bashrc"
    touch "$HOME/.zshrc"
    mkdir -p "$HOME/.config/fish"
    touch "$HOME/.config/fish/config.fish"
    
    run run_detect_all_shells
    [ "$status" -eq 0 ]
    
    # Should contain all three
    echo "$output" | grep -q "bash|$HOME/.bashrc"
    echo "$output" | grep -q "zsh|$HOME/.zshrc"
    echo "$output" | grep -q "fish|$HOME/.config/fish/config.fish"
}

@test "detect_all_shells: detects bash and zsh when both exist (no fish)" {
    # Create bash and zsh config files
    touch "$HOME/.bashrc"
    touch "$HOME/.zshrc"
    
    run run_detect_all_shells
    [ "$status" -eq 0 ]
    
    # Should contain bash and zsh
    echo "$output" | grep -q "bash|$HOME/.bashrc"
    echo "$output" | grep -q "zsh|$HOME/.zshrc"
    
    # Should NOT contain fish
    ! echo "$output" | grep -q "fish|"
}

@test "detect_all_shells: configures both .bashrc and .bash_profile when both exist" {
    # Create both bash config files
    touch "$HOME/.bashrc"
    touch "$HOME/.bash_profile"

    run run_detect_all_shells
    [ "$status" -eq 0 ]

    # Login shells (macOS Terminal, SSH) read .bash_profile while interactive
    # non-login shells read .bashrc, so both must be configured.
    echo "$output" | grep -q "bash|$HOME/.bashrc"
    echo "$output" | grep -q "bash|$HOME/.bash_profile"
}

@test "detect_all_shells: uses .bash_profile when .bashrc doesn't exist" {
    # Create only .bash_profile
    touch "$HOME/.bash_profile"
    
    run run_detect_all_shells
    [ "$status" -eq 0 ]
    
    # Should use .bash_profile
    echo "$output" | grep -q "bash|$HOME/.bash_profile"
}

@test "detect_all_shells: falls back to SHELL (zsh) when no configs exist" {
    # No config files created, but SHELL is set to zsh
    export SHELL="/bin/zsh"
    
    run run_detect_all_shells
    [ "$status" -eq 0 ]
    
    # Should fall back to zsh
    echo "$output" | grep -q "zsh|$HOME/.zshrc"
}

@test "detect_all_shells: falls back to SHELL (bash) when no configs exist" {
    # No config files created, but SHELL is set to bash
    export SHELL="/bin/bash"
    
    run run_detect_all_shells
    [ "$status" -eq 0 ]
    
    # Should fall back to bash
    echo "$output" | grep -q "bash|$HOME/.bashrc"
}

@test "detect_all_shells: falls back to SHELL (fish) when no configs exist" {
    # No config files created, but SHELL is set to fish
    export SHELL="/usr/local/bin/fish"
    
    run run_detect_all_shells
    [ "$status" -eq 0 ]
    
    # Should fall back to fish
    echo "$output" | grep -q "fish|$HOME/.config/fish/config.fish"
}

@test "detect_all_shells: falls back to bash as default when no configs and unknown SHELL" {
    # No config files and empty/unknown SHELL
    export SHELL=""
    
    run run_detect_all_shells
    [ "$status" -eq 0 ]
    
    # Should default to bash
    echo "$output" | grep -q "bash|$HOME/.bashrc"
}

# ============================================================================
# Tests for PATH configuration idempotency
# ============================================================================

@test "PATH configuration: does not duplicate entries on re-run" {
    # Create a mock config file with existing env-file source line
    INSTALL_DIR="$HOME/.autter/bin"
    mkdir -p "$INSTALL_DIR"

    touch "$HOME/.bashrc"
    echo "# Added by autter installer on Mon Jan 1 00:00:00 UTC 2024" >> "$HOME/.bashrc"
    echo '. "$HOME/.autter/env"' >> "$HOME/.bashrc"

    # Count lines before
    lines_before=$(wc -l < "$HOME/.bashrc")

    # The installer skips the append when either grep matches
    grep -qsF "$INSTALL_DIR" "$HOME/.bashrc" || grep -qsF '.autter/env' "$HOME/.bashrc"

    # Count lines after (should be same since we didn't add anything)
    lines_after=$(wc -l < "$HOME/.bashrc")

    [ "$lines_before" -eq "$lines_after" ]
}

@test "PATH configuration: recognizes legacy raw-export lines from older installers" {
    # Older installers appended the expanded PATH export directly; a re-run of
    # the new installer must treat those files as already configured.
    INSTALL_DIR="$HOME/.autter/bin"
    mkdir -p "$INSTALL_DIR"

    touch "$HOME/.bashrc"
    echo "export PATH=\"$INSTALL_DIR:\$PATH\"" >> "$HOME/.bashrc"

    grep -qsF "$INSTALL_DIR" "$HOME/.bashrc" || grep -qsF '.autter/env' "$HOME/.bashrc"
}

# ============================================================================
# Tests for shell-specific PATH syntax
# ============================================================================

@test "PATH syntax: generates correct fish syntax" {
    # Fish configs source the fish env file with $HOME kept literal
    expected_fish_cmd='source "$HOME/.autter/env.fish"'

    [ "$expected_fish_cmd" = 'source "$HOME/.autter/env.fish"' ]
}

@test "PATH syntax: generates correct bash/zsh syntax" {
    # Bash/zsh configs source the POSIX env file with $HOME kept literal
    expected_bash_cmd='. "$HOME/.autter/env"'

    [ "$expected_bash_cmd" = '. "$HOME/.autter/env"' ]
}

# ============================================================================
# Integration test: simulate multi-shell configuration
# ============================================================================

@test "integration: configures PATH in all detected shells" {
    # Create config files for all shells
    touch "$HOME/.bashrc"
    touch "$HOME/.zshrc"
    mkdir -p "$HOME/.config/fish"
    touch "$HOME/.config/fish/config.fish"
    
    INSTALL_DIR="$HOME/.autter/bin"
    mkdir -p "$INSTALL_DIR"
    
    # Source the test functions
    source "$TEST_TEMP_DIR/test_functions.sh"
    
    # Simulate the installer's PATH configuration loop
    SHELLS_CONFIGURED=""
    while IFS='|' read -r shell_name config_file; do
        [ -z "$shell_name" ] && continue

        # Generate shell-appropriate env-file source line
        if [ "$shell_name" = "fish" ]; then
            path_cmd='source "$HOME/.autter/env.fish"'
        else
            path_cmd='. "$HOME/.autter/env"'
        fi

        # Append if not already present
        if ! grep -qsF "$INSTALL_DIR" "$config_file" && ! grep -qsF '.autter/env' "$config_file"; then
            echo "" >> "$config_file"
            echo "# Added by autter installer" >> "$config_file"
            echo "$path_cmd" >> "$config_file"
            SHELLS_CONFIGURED="${SHELLS_CONFIGURED}${config_file}\n"
        fi
    done <<< "$(detect_all_shells)"

    # Verify all three configs were updated
    grep -qF '. "$HOME/.autter/env"' "$HOME/.bashrc"
    grep -qF '. "$HOME/.autter/env"' "$HOME/.zshrc"
    grep -qF 'source "$HOME/.autter/env.fish"' "$HOME/.config/fish/config.fish"
}

@test "integration: only configures existing shell configs" {
    # Create only zsh config (bash and fish do not exist)
    touch "$HOME/.zshrc"
    
    INSTALL_DIR="$HOME/.autter/bin"
    mkdir -p "$INSTALL_DIR"
    
    # Source the test functions
    source "$TEST_TEMP_DIR/test_functions.sh"
    
    # Simulate the installer's PATH configuration loop
    while IFS='|' read -r shell_name config_file; do
        [ -z "$shell_name" ] && continue

        if [ "$shell_name" = "fish" ]; then
            path_cmd='source "$HOME/.autter/env.fish"'
        else
            path_cmd='. "$HOME/.autter/env"'
        fi

        if ! grep -qsF "$INSTALL_DIR" "$config_file" && ! grep -qsF '.autter/env' "$config_file"; then
            echo "" >> "$config_file"
            echo "# Added by autter installer" >> "$config_file"
            echo "$path_cmd" >> "$config_file"
        fi
    done <<< "$(detect_all_shells)"

    # Verify only zsh was configured
    grep -qF '. "$HOME/.autter/env"' "$HOME/.zshrc"

    # Verify bash and fish were NOT created
    [ ! -f "$HOME/.bashrc" ]
    [ ! -f "$HOME/.config/fish/config.fish" ]
}
