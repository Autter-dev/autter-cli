import * as vscode from "vscode";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { shouldSkipLegacyCopilotHooks } from "./utils/vscode-hooks";

const USER_COPILOT_HOOKS_LOCATION = "~/.copilot/hooks";

/**
 * On VS Code >= 1.109.3 this extension stops capturing Copilot agent edits
 * itself and relies on VS Code's native agent hooks running
 * `autter checkpoint github-copilot` from ~/.copilot/hooks/autter.json.
 * If that chain isn't configured, agent-mode edits silently produce no
 * checkpoints while `autter install-hooks` / `autter debug` from an older
 * install still look healthy. This check makes that state visible and
 * actionable instead of silent.
 */
export function checkNativeHooksHealth(): void {
  if (!shouldSkipLegacyCopilotHooks(vscode.version)) {
    // Legacy capture is active; this extension handles checkpoints itself.
    return;
  }

  const problems: string[] = [];

  const hooksFile = path.join(os.homedir(), ".copilot", "hooks", "autter.json");
  let hooksFileExists = false;
  try {
    hooksFileExists = fs.existsSync(hooksFile);
  } catch {
    // Treat unreadable as missing; the warning below tells the user how to fix.
  }
  if (!hooksFileExists) {
    problems.push(`the hook file ${hooksFile} is missing`);
  }

  // When the hookFilesLocations setting exists, VS Code only loads hook files
  // from locations that are enabled in its effective value.
  const locations = vscode.workspace
    .getConfiguration("chat")
    .get<Record<string, unknown>>("hookFilesLocations");
  if (locations && locations[USER_COPILOT_HOOKS_LOCATION] !== true) {
    problems.push(
      `"${USER_COPILOT_HOOKS_LOCATION}" is not enabled in the chat.hookFilesLocations setting`
    );
  }

  if (problems.length === 0) {
    console.log("[autter] Native hooks health: OK (hook file present, location enabled)");
    return;
  }

  const message =
    "autter: AI edits from VS Code's Copilot agent are NOT being tracked — " +
    problems.join("; ") +
    ". Run `autter install-hooks` in a terminal, then restart VS Code.";
  console.warn("[autter] " + message);
  vscode.window.showWarningMessage(message);
}
