# Installing autter with Nix

This project provides a Nix flake for easy installation on NixOS, nix-darwin, or any system using Home Manager or Nix profiles.

## Quick Start

Try without installing:
```bash
nix run github:acunniffe/autter -- --version
```

Install to user profile:
```bash
nix profile install github:acunniffe/autter
```

## What's Included

The package provides three commands:

| Command | Description |
|---------|-------------|
| `git` | Routes through autter (tracks AI authorship) |
| `autter` | Direct autter commands |
| `git-og` | Bypasses autter, calls real git |

## Flake Outputs

```
packages.${system}.default   # Complete package with git wrapper
packages.${system}.minimal   # Without git symlink (for manual integration)
packages.${system}.unwrapped # Just the binary
devShells.${system}.default  # Development environment
nixosModules.default         # NixOS module
homeManagerModules.default   # Home Manager module (hooks and config only)
overlays.default             # Nixpkgs overlay
```

## Installation Methods

### 1. Home Manager with programs.git (Recommended)

The cleanest approach is to set autter as your git package and use the module for hooks.

Add the input to your flake:
```nix
{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    home-manager.url = "github:nix-community/home-manager";
    autter.url = "github:acunniffe/autter";
  };
}
```

In your Home Manager configuration:
```nix
{ inputs, system, ... }:

{
  imports = [ inputs.autter.homeManagerModules.default ];

  # Use autter as the git implementation
  programs.git = {
    enable = true;
    package = inputs.autter.packages.${system}.default;
    # ... your other git settings (signing, aliases, etc.)
  };

  # Enable autter hooks for IDE/agent integration
  programs.autter = {
    enable = true;
    installHooks = true;  # Runs autter install-hooks on activation
  };
}
```

This approach:
- Replaces the standard git with autter throughout your environment
- Installs IDE/agent hooks automatically
- Creates `~/.autter/config.json` with the correct git path
- Avoids package conflicts

### 2. nix-darwin with Home Manager

```nix
{
  inputs = {
    darwin.url = "github:lnl7/nix-darwin";
    home-manager.url = "github:nix-community/home-manager";
    autter.url = "github:acunniffe/autter";
  };

  outputs = { darwin, home-manager, autter, nixpkgs, ... }: {
    darwinConfigurations.myhost = darwin.lib.darwinSystem {
      system = "aarch64-darwin";
      modules = [
        home-manager.darwinModules.home-manager
        {
          home-manager.users.myuser = { pkgs, ... }: {
            imports = [ autter.homeManagerModules.default ];

            programs.git = {
              enable = true;
              package = autter.packages.${pkgs.system}.default;
            };

            programs.autter = {
              enable = true;
              installHooks = true;
            };
          };
        }
      ];
    };
  };
}
```

### 3. NixOS System-Wide

```nix
{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    autter.url = "github:acunniffe/autter";
  };

  outputs = { nixpkgs, autter, ... }: {
    nixosConfigurations.myhost = nixpkgs.lib.nixosSystem {
      modules = [
        autter.nixosModules.default
        {
          programs.autter = {
            enable = true;
            installHooks = true;
          };

          # Add autter to system packages
          environment.systemPackages = [
            autter.packages.x86_64-linux.default
          ];
        }
      ];
    };
  };
}
```

### 4. Direct Package (Standalone)

If not using Home Manager's `programs.git`, add the package directly:
```nix
{ inputs, pkgs, ... }:

{
  home.packages = [
    inputs.autter.packages.${pkgs.system}.default
  ];
}
```

**Note:** This may conflict if you also have `programs.git.enable = true`. Use the `minimal` package to avoid conflicts:
```nix
home.packages = [
  inputs.autter.packages.${pkgs.system}.minimal  # No git symlink
];
```

### 5. Using the Overlay

```nix
{
  nixpkgs.overlays = [ inputs.autter.overlays.default ];

  # Then use:
  home.packages = [ pkgs.autter ];
}
```

## Development

Enter a development shell with Rust toolchain:
```bash
nix develop github:acunniffe/autter
```

Or clone and develop locally:
```bash
git clone https://github.com/acunniffe/autter
cd autter
nix develop

cargo build
cargo test
cargo run -- --version
```

## Local Flake Development

For developing from a local checkout:
```nix
{
  inputs.autter.url = "git+file:///path/to/autter";
}
```

## Module Options

### homeManagerModules.default

The Home Manager module handles hooks and configuration only (not package installation).

| Option | Type | Default | Description |
|--------|------|---------|-------------|
| `enable` | bool | `false` | Enable autter hooks and config |
| `package` | package | flake default | The autter package (for hooks) |
| `installHooks` | bool | `true` | Run `autter install-hooks` on activation |

### nixosModules.default

| Option | Type | Default | Description |
|--------|------|---------|-------------|
| `enable` | bool | `false` | Enable autter |
| `package` | package | flake default | The autter package to use |
| `installHooks` | bool | `true` | Run `autter install-hooks` on activation |
| `setGitAlias` | bool | `true` | Add autter to system PATH |

## Platforms

Supported systems:
- `x86_64-linux`
- `aarch64-linux`
- `x86_64-darwin`
- `aarch64-darwin`
