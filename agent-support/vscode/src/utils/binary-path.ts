import { execFile } from "child_process";
import * as os from "os";
import * as vscode from "vscode";

let resolvedPath: string | null = null;
let resolvePromise: Promise<string | null> | null = null;
let extensionMode: vscode.ExtensionMode | null = null;

/**
 * Call once at activation to pass in the extension context's mode.
 */
export function initBinaryResolver(mode: vscode.ExtensionMode): void {
  extensionMode = mode;
}

/**
 * Resolve the full path to the `autter` binary using a login shell.
 * Only runs in development mode — in production the plain "autter" name
 * is used directly (relies on the process PATH).
 *
 * The result is cached after the first successful resolution.
 */
export function resolveAutterBinary(): Promise<string | null> {
  // Skip shell resolution in production — just use "autter"
  if (extensionMode !== vscode.ExtensionMode.Development) {
    return Promise.resolve(null);
  }

  if (resolvedPath) {
    return Promise.resolve(resolvedPath);
  }
  if (resolvePromise) {
    return resolvePromise;
  }

  resolvePromise = new Promise((resolve) => {
    const platform = os.platform();

    if (platform === "win32") {
      // Windows: use `where autter`
      execFile("where", ["autter"], (err, stdout) => {
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
  return resolvedPath || "autter";
}
