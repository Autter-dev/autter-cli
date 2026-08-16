import { execFile } from "child_process";
import * as fs from "fs";
import * as os from "os";
import * as path from "path";
import * as vscode from "vscode";

let resolvedPath: string | null = null;
let resolvePromise: Promise<string | null> | null = null;

/**
 * Well-known locations the autter installers place the binary in
 * (install.sh uses ~/.autter/bin plus a ~/.local/bin symlink; install.ps1
 * uses %USERPROFILE%\.autter\bin). GUI-launched editors often run with a
 * stripped PATH, so preferring these absolute paths keeps checkpoints
 * working when `spawn("autter")` would fail with ENOENT — which previously
 * made human (known_human) and AI edit tracking die silently.
 */
function findWellKnownBinary(): string | null {
  const home = os.homedir();
  const candidates =
    os.platform() === "win32"
      ? [path.join(home, ".autter", "bin", "autter.exe")]
      : [
          path.join(home, ".autter", "bin", "autter"),
          path.join(home, ".local", "bin", "autter"),
          "/usr/local/bin/autter",
          "/opt/homebrew/bin/autter",
        ];
  for (const candidate of candidates) {
    try {
      if (fs.existsSync(candidate)) {
        return candidate;
      }
    } catch {
      // Inaccessible candidate; try the next one.
    }
  }
  return null;
}

/**
 * Call once at activation. Warms the resolution cache so the first
 * checkpoint doesn't race binary discovery.
 */
export function initBinaryResolver(mode: vscode.ExtensionMode): void {
  console.log("[autter] Initializing binary resolver (extension mode:", mode, ")");
  void resolveAutterBinary();
}

/**
 * Resolve the full path to the `autter` binary: well-known install locations
 * first, then a login shell / `where` lookup. Runs in every extension mode —
 * production extension hosts frequently lack the user's shell PATH.
 *
 * The result is cached after the first successful resolution.
 */
export function resolveAutterBinary(): Promise<string | null> {
  if (resolvedPath) {
    return Promise.resolve(resolvedPath);
  }

  const wellKnown = findWellKnownBinary();
  if (wellKnown) {
    resolvedPath = wellKnown;
    console.log("[autter] Resolved binary path:", resolvedPath);
    return Promise.resolve(resolvedPath);
  }

  if (resolvePromise) {
    return resolvePromise;
  }

  resolvePromise = new Promise((resolve) => {
    const platform = os.platform();

    if (platform === "win32") {
      // Windows: use `where autter`
      execFile("where", ["autter"], { timeout: 5000 }, (err, stdout) => {
        if (err || !stdout.trim()) {
          console.log("[autter] Could not resolve autter binary via 'where'");
          resolve(null);
        } else {
          // `where` can return multiple lines; take the first
          resolvedPath = stdout.trim().split(/\r?\n/)[0];
          console.log("[autter] Resolved binary path:", resolvedPath);
          resolve(resolvedPath);
        }
      });
    } else {
      // macOS/Linux: spawn a login shell so the user's profile is sourced
      const shell = process.env.SHELL || "/bin/bash";
      execFile(shell, ["-ilc", "which autter"], { timeout: 5000 }, (err, stdout) => {
        if (err || !stdout.trim()) {
          console.log("[autter] Could not resolve autter binary via login shell");
          resolve(null);
        } else {
          resolvedPath = stdout.trim();
          console.log("[autter] Resolved binary path:", resolvedPath);
          resolve(resolvedPath);
        }
      });
    }
  });

  return resolvePromise;
}

/**
 * Get the resolved autter binary path, or fall back to just "autter"
 * (which relies on the current process PATH).
 */
export function getAutterBinary(): string {
  if (!resolvedPath) {
    resolvedPath = findWellKnownBinary();
  }
  return resolvedPath || "autter";
}
