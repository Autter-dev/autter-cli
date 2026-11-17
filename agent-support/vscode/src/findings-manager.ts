import { spawn } from "child_process";
import * as path from "path";
import * as vscode from "vscode";
import { getAutterBinary, resolveAutterBinary } from "./utils/binary-path";
import { Config } from "./utils/config";
import {
  applyEdit,
  type CliFinding,
  type CliFindingsResult,
  diagnosticMessage,
  type FindingsOutcome,
  inlineText,
  parseFindingsOutput,
  type PlacedFinding,
  placeFindings,
  severityLabel,
  statusSummary,
} from "./findings-model";

const SOURCE = "autter";
const DISMISSED_KEY = "autter.findings.dismissed";
const SAVE_DEBOUNCE_MS = 3_000;
const POLL_MS = 5 * 60_000;
const FOCUS_REFRESH_MIN_MS = 60_000;
const RUN_TIMEOUT_MS = 90_000;

/** One git work tree we show findings for. */
interface Root {
  workdir: string;
  result: CliFindingsResult | null;
  /** Placed findings by repo-relative path, kept in step with edits. */
  placed: Map<string, PlacedFinding[]>;
  running: boolean;
  rerun: boolean;
  lastRun: number;
}

/**
 * Review findings of the branch's pull request, in the editor: squiggles and
 * Problems entries, optional after-the-line annotations, quick fixes, and a
 * status bar item that doubles as the sign-in button. Data comes from
 * `autter findings --json --write`, which also keeps `.autter/findings.md`.
 */
export class FindingsManager implements vscode.CodeActionProvider {
  private readonly diagnostics = vscode.languages.createDiagnosticCollection(SOURCE);
  private readonly decoration = vscode.window.createTextEditorDecorationType({
    after: {
      margin: "0 0 0 2em",
      color: new vscode.ThemeColor("editorCodeLens.foreground"),
      fontStyle: "italic",
    },
    isWholeLine: true,
  });
  private readonly statusBar = vscode.window.createStatusBarItem(vscode.StatusBarAlignment.Left, 90);
  private readonly roots = new Map<string, Root>();
  /** Folder → its work tree, once a run has told us. */
  private readonly folderWorkdir = new Map<string, string>();
  private readonly disposables: vscode.Disposable[] = [];
  private saveTimer: NodeJS.Timeout | undefined;
  private pollTimer: NodeJS.Timeout | undefined;
  private signedOut = false;
  private lastError: string | null = null;

  constructor(private readonly context: vscode.ExtensionContext) {}

  activate(): void {
    this.disposables.push(
      this.diagnostics,
      this.decoration,
      this.statusBar,
      vscode.languages.registerCodeActionsProvider({ scheme: "file" }, this, {
        providedCodeActionKinds: [vscode.CodeActionKind.QuickFix],
      }),
      vscode.commands.registerCommand("autter.findings.refresh", () => this.refreshAll(true)),
      vscode.commands.registerCommand("autter.findings.reviewChanges", () => this.reviewChanges()),
      vscode.commands.registerCommand("autter.findings.signIn", () => this.signIn()),
      vscode.commands.registerCommand("autter.findings.openFile", () => this.openFindingsFile()),
      vscode.commands.registerCommand("autter.findings.copyFixPrompt", (f: CliFinding) => this.copyFixPrompt(f)),
      vscode.commands.registerCommand("autter.findings.openInDashboard", (f: CliFinding) => this.openInDashboard(f)),
      vscode.commands.registerCommand("autter.findings.dismiss", (f: CliFinding) => this.dismiss(f)),
      vscode.commands.registerCommand("autter.findings.restoreDismissed", () => this.restoreDismissed()),
      vscode.workspace.onDidSaveTextDocument(() => this.scheduleRefresh()),
      vscode.workspace.onDidChangeTextDocument((e) => this.onEdit(e)),
      vscode.workspace.onDidChangeWorkspaceFolders(() => this.refreshAll(true)),
      vscode.window.onDidChangeVisibleTextEditors(() => this.renderDecorations()),
      vscode.window.onDidChangeWindowState((state) => {
        if (state.focused) {
          this.refreshAll(false);
        }
      }),
      vscode.workspace.onDidChangeConfiguration((e) => {
        if (e.affectsConfiguration("autter.findings")) {
          this.refreshAll(true);
        }
      }),
    );
    this.watchHeads();
    this.pollTimer = setInterval(() => this.refreshAll(true), POLL_MS);
    this.refreshAll(true);
  }

  dispose(): void {
    if (this.saveTimer) {
      clearTimeout(this.saveTimer);
    }
    if (this.pollTimer) {
      clearInterval(this.pollTimer);
    }
    for (const d of this.disposables) {
      d.dispose();
    }
  }

  // ── Refresh ──────────────────────────────────────────────────────────────

  /** A checkout or commit changes `.git/HEAD`: refresh for the new branch. */
  private watchHeads(): void {
    const watcher = vscode.workspace.createFileSystemWatcher("**/.git/{HEAD,ORIG_HEAD}");
    const onHead = () => this.scheduleRefresh();
    this.disposables.push(watcher, watcher.onDidChange(onHead), watcher.onDidCreate(onHead));
  }

  private scheduleRefresh(): void {
    if (this.saveTimer) {
      clearTimeout(this.saveTimer);
    }
    this.saveTimer = setTimeout(() => this.refreshAll(true), SAVE_DEBOUNCE_MS);
  }

  private refreshAll(force: boolean): void {
    if (Config.getFindingsDisplay() === "off") {
      this.clearAll();
      this.statusBar.hide();
      return;
    }
    const folders = vscode.workspace.workspaceFolders ?? [];
    const seen = new Set<string>();
    for (const folder of folders) {
      const cwd = folder.uri.fsPath;
      const key = this.folderWorkdir.get(cwd) ?? cwd;
      if (seen.has(key)) {
        continue;
      }
      seen.add(key);
      void this.refresh(cwd, force);
    }
  }

  private rootFor(key: string): Root {
    let root = this.roots.get(key);
    if (!root) {
      root = { workdir: key, result: null, placed: new Map(), running: false, rerun: false, lastRun: 0 };
      this.roots.set(key, root);
    }
    return root;
  }

  private async refresh(cwd: string, force: boolean): Promise<void> {
    const root = this.rootFor(this.folderWorkdir.get(cwd) ?? cwd);
    if (!force && Date.now() - root.lastRun < FOCUS_REFRESH_MIN_MS) {
      return;
    }
    if (root.running) {
      root.rerun = true;
      return;
    }
    root.running = true;
    root.lastRun = Date.now();
    try {
      const outcome = await this.runCli(cwd);
      this.apply(cwd, root, outcome);
    } finally {
      root.running = false;
      if (root.rerun) {
        root.rerun = false;
        void this.refresh(cwd, true);
      }
    }
  }

  private async runCli(cwd: string): Promise<FindingsOutcome> {
    if (!(await resolveAutterBinary())) {
      return { kind: "unavailable", message: "The autter CLI is not installed." };
    }
    const args = ["findings", "--json"];
    if (Config.isFindingsFileEnabled()) {
      args.push("--write");
    }
    return new Promise((resolve) => {
      let stdout = "";
      let settled = false;
      const done = (outcome: FindingsOutcome) => {
        if (!settled) {
          settled = true;
          resolve(outcome);
        }
      };
      const proc = spawn(getAutterBinary(), args, { cwd, timeout: RUN_TIMEOUT_MS });
      proc.stdout.on("data", (d) => {
        stdout += d.toString();
      });
      proc.stderr.on("data", () => {});
      proc.on("error", (err) => done({ kind: "unavailable", message: `Could not run autter: ${err.message}` }));
      proc.on("close", (code) => done(parseFindingsOutput(stdout, code)));
    });
  }

  private apply(cwd: string, root: Root, outcome: FindingsOutcome): void {
    switch (outcome.kind) {
      case "ok": {
        this.signedOut = false;
        this.lastError = null;
        const workdir = outcome.result.local?.workdir ? path.resolve(outcome.result.local.workdir) : root.workdir;
        let target = root;
        if (workdir !== root.workdir) {
          // First run from this folder: key the root by its real work tree.
          this.clearRoot(root);
          this.roots.delete(root.workdir);
          this.folderWorkdir.set(cwd, workdir);
          target = this.rootFor(workdir);
          target.lastRun = root.lastRun;
        }
        this.setResult(target, outcome.result);
        break;
      }
      case "signed_out":
        this.signedOut = true;
        this.clearRoot(root);
        break;
      case "unavailable":
        // Not a repo Autter knows (or no CLI): show nothing for it.
        this.clearRoot(root);
        root.result = null;
        break;
      case "error":
        // Keep what is shown; a transient failure shouldn't blank the editor.
        this.lastError = outcome.message;
        break;
    }
    this.renderStatus();
  }

  private setResult(root: Root, result: CliFindingsResult): void {
    const dismissed = new Set(this.context.workspaceState.get<string[]>(DISMISSED_KEY, []));
    const fresh = placeFindings(result, dismissed);
    // The CLI placed findings on the files on disk. An editor with unsaved
    // changes no longer matches them, so keep that file's edit-tracked
    // placement until it is saved.
    for (const doc of vscode.workspace.textDocuments) {
      if (!doc.isDirty || doc.uri.scheme !== "file") {
        continue;
      }
      const rel = this.relativePath(root, doc.uri);
      if (rel === null) {
        continue;
      }
      const kept = root.placed.get(rel);
      if (kept) {
        fresh.set(rel, kept.filter((p) => !dismissed.has(p.finding.id)));
      } else {
        fresh.delete(rel);
      }
    }
    this.clearRoot(root);
    root.result = result;
    root.placed = fresh;
    for (const rel of fresh.keys()) {
      this.publish(root, rel);
    }
    this.renderDecorations();
  }

  // ── Display ──────────────────────────────────────────────────────────────

  private relativePath(root: Root, uri: vscode.Uri): string | null {
    const rel = path.relative(root.workdir, uri.fsPath);
    if (!rel || rel.startsWith("..") || path.isAbsolute(rel)) {
      return null;
    }
    return rel.split(path.sep).join("/");
  }

  private rootForUri(uri: vscode.Uri): { root: Root; rel: string } | null {
    for (const root of this.roots.values()) {
      const rel = this.relativePath(root, uri);
      if (rel !== null && root.placed.has(rel)) {
        return { root, rel };
      }
    }
    return null;
  }

  private publish(root: Root, rel: string): void {
    const uri = vscode.Uri.file(path.join(root.workdir, ...rel.split("/")));
    const placed = root.placed.get(rel) ?? [];
    this.diagnostics.set(uri, placed.map((p) => this.toDiagnostic(p)));
  }

  private toDiagnostic(p: PlacedFinding): vscode.Diagnostic {
    const range = new vscode.Range(p.line, 0, p.line, Number.MAX_SAFE_INTEGER);
    const severity =
      p.finding.severity === "critical"
        ? vscode.DiagnosticSeverity.Error
        : p.finding.severity === "warning"
          ? vscode.DiagnosticSeverity.Warning
          : vscode.DiagnosticSeverity.Information;
    const diagnostic = new vscode.Diagnostic(range, diagnosticMessage(p.finding), severity);
    diagnostic.source = SOURCE;
    const label =
      p.origin === "local"
        ? `${severityLabel(p.finding.severity)}, local review`
        : severityLabel(p.finding.severity);
    diagnostic.code = p.finding.url ? { value: label, target: vscode.Uri.parse(p.finding.url) } : label;
    return diagnostic;
  }

  private renderDecorations(): void {
    const inline = Config.getFindingsDisplay() === "inline";
    for (const editor of vscode.window.visibleTextEditors) {
      if (!inline) {
        editor.setDecorations(this.decoration, []);
        continue;
      }
      const hit = this.rootForUri(editor.document.uri);
      const placed = hit ? (hit.root.placed.get(hit.rel) ?? []) : [];
      // One annotation per line: the most severe finding on it.
      const byLine = new Map<number, PlacedFinding>();
      for (const p of placed) {
        if (!byLine.has(p.line)) {
          byLine.set(p.line, p);
        }
      }
      const options: vscode.DecorationOptions[] = [];
      for (const [line, p] of byLine) {
        if (line >= editor.document.lineCount) {
          continue;
        }
        const more = placed.filter((q) => q.line === line).length - 1;
        options.push({
          range: new vscode.Range(line, 0, line, 0),
          renderOptions: {
            after: {
              contentText:
                more > 0
                  ? `${inlineText(p.finding, 90, p.origin)} (+${more} more)`
                  : inlineText(p.finding, 90, p.origin),
            },
          },
        });
      }
      editor.setDecorations(this.decoration, options);
    }
  }

  private renderStatus(): void {
    if (this.signedOut) {
      this.statusBar.text = "$(account) autter: Sign in";
      this.statusBar.tooltip = "Sign in to see Autter review findings next to your code.";
      this.statusBar.command = "autter.findings.signIn";
      this.statusBar.show();
      return;
    }
    const withResult = [...this.roots.values()].filter((r) => r.result);
    if (withResult.length === 0) {
      if (this.lastError) {
        this.statusBar.text = "$(warning) autter";
        this.statusBar.tooltip = `Could not load findings: ${this.lastError}\nClick to retry.`;
        this.statusBar.command = "autter.findings.refresh";
        this.statusBar.show();
      } else {
        this.statusBar.hide();
      }
      return;
    }
    // Several repos open: summarise the first; the file and Problems list the rest.
    const root = withResult[0];
    const result = root.result as CliFindingsResult;
    const shown = [...root.placed.values()].reduce((n, list) => n + list.length, 0);
    const summary = statusSummary(result, shown);
    this.statusBar.text = summary.text;
    this.statusBar.tooltip = this.lastError ? `${summary.tooltip}\n\nLast refresh failed: ${this.lastError}` : summary.tooltip;
    this.statusBar.command = "autter.findings.openFile";
    this.statusBar.show();
  }

  private clearRoot(root: Root): void {
    for (const rel of root.placed.keys()) {
      this.diagnostics.delete(vscode.Uri.file(path.join(root.workdir, ...rel.split("/"))));
    }
    root.placed = new Map();
    this.renderDecorations();
  }

  private clearAll(): void {
    for (const root of this.roots.values()) {
      this.clearRoot(root);
    }
  }

  // ── Edits ────────────────────────────────────────────────────────────────

  /** Keep findings on their lines while typing; drop ones whose line is edited. */
  private onEdit(event: vscode.TextDocumentChangeEvent): void {
    if (event.contentChanges.length === 0) {
      return;
    }
    const hit = this.rootForUri(event.document.uri);
    if (!hit) {
      return;
    }
    let placed = hit.root.placed.get(hit.rel) ?? [];
    for (const change of event.contentChanges) {
      placed = applyEdit(placed, {
        startLine: change.range.start.line,
        startCharacter: change.range.start.character,
        endLine: change.range.end.line,
        endCharacter: change.range.end.character,
        text: change.text,
      });
    }
    hit.root.placed.set(hit.rel, placed);
    this.publish(hit.root, hit.rel);
    this.renderDecorations();
  }

  // ── Code actions and commands ────────────────────────────────────────────

  provideCodeActions(
    document: vscode.TextDocument,
    _range: vscode.Range,
    context: vscode.CodeActionContext,
  ): vscode.CodeAction[] {
    const actions: vscode.CodeAction[] = [];
    const hit = this.rootForUri(document.uri);
    const placed = hit ? (hit.root.placed.get(hit.rel) ?? []) : [];
    for (const diagnostic of context.diagnostics) {
      if (diagnostic.source !== SOURCE) {
        continue;
      }
      // Diagnostics come back as copies: find the finding by line and text.
      const finding = placed.find(
        (p) => p.line === diagnostic.range.start.line && diagnosticMessage(p.finding) === diagnostic.message,
      )?.finding;
      if (!finding) {
        continue;
      }
      const add = (title: string, command: string) => {
        const action = new vscode.CodeAction(title, vscode.CodeActionKind.QuickFix);
        action.diagnostics = [diagnostic];
        action.command = { title, command, arguments: [finding] };
        actions.push(action);
      };
      if (finding.fixPrompt) {
        add("autter: Copy fix prompt (paste into your AI agent)", "autter.findings.copyFixPrompt");
      }
      if (finding.url) {
        add("autter: Open finding in the dashboard", "autter.findings.openInDashboard");
      }
      add("autter: Dismiss this finding", "autter.findings.dismiss");
    }
    return actions;
  }

  private async copyFixPrompt(finding: CliFinding): Promise<void> {
    const text = [
      `Fix this code review finding in ${finding.path}${finding.local?.line ? `:${finding.local.line}` : ""}: ${finding.title}`,
      finding.body,
      finding.fixPrompt ?? "",
    ]
      .filter((s) => s.trim())
      .join("\n\n");
    await vscode.env.clipboard.writeText(text);
    void vscode.window.showInformationMessage("autter: Fix prompt copied.");
  }

  private openInDashboard(finding: CliFinding): void {
    if (finding.url) {
      void vscode.env.openExternal(vscode.Uri.parse(finding.url));
    }
  }

  private async dismiss(finding: CliFinding): Promise<void> {
    const dismissed = new Set(this.context.workspaceState.get<string[]>(DISMISSED_KEY, []));
    dismissed.add(finding.id);
    await this.context.workspaceState.update(DISMISSED_KEY, [...dismissed]);
    for (const root of this.roots.values()) {
      for (const [rel, list] of root.placed) {
        const kept = list.filter((p) => p.finding.id !== finding.id);
        if (kept.length !== list.length) {
          root.placed.set(rel, kept);
          this.publish(root, rel);
        }
      }
    }
    this.renderDecorations();
    this.renderStatus();
  }

  private async restoreDismissed(): Promise<void> {
    await this.context.workspaceState.update(DISMISSED_KEY, []);
    this.refreshAll(true);
  }

  private async openFindingsFile(): Promise<void> {
    const root = [...this.roots.values()].find((r) => r.result);
    const file = root?.result?.local?.markdownPath;
    if (file) {
      const doc = await vscode.workspace.openTextDocument(vscode.Uri.file(file));
      await vscode.window.showTextDocument(doc, { preview: true });
      return;
    }
    if (root?.result?.pullRequest?.url) {
      void vscode.env.openExternal(vscode.Uri.parse(root.result.pullRequest.url));
      return;
    }
    await vscode.commands.executeCommand("workbench.actions.view.problems");
  }

  /** The work tree to review: the active editor's, else the first folder's. */
  private reviewFolder(): string | null {
    const active = vscode.window.activeTextEditor?.document.uri;
    const folder =
      (active && vscode.workspace.getWorkspaceFolder(active)) ?? vscode.workspace.workspaceFolders?.[0];
    return folder?.uri.fsPath ?? null;
  }

  /**
   * `autter review`: review the branch's changes before a pull request, then
   * show the findings like the PR's. Saves open files first: the review reads
   * them from disk.
   */
  private async reviewChanges(): Promise<void> {
    const cwd = this.reviewFolder();
    if (!cwd) {
      void vscode.window.showWarningMessage("autter: Open a folder in a git repository to review your changes.");
      return;
    }
    if (!(await resolveAutterBinary())) {
      void vscode.window.showWarningMessage("autter: The autter CLI is not installed.");
      return;
    }
    await vscode.workspace.saveAll(false);
    const args = ["review", "--json"];
    if (Config.isFindingsFileEnabled()) {
      args.push("--write");
    }
    const outcome = await vscode.window.withProgress(
      {
        location: vscode.ProgressLocation.Notification,
        title: "autter: Reviewing your changes (a minute or two)…",
        cancellable: true,
      },
      (_progress, token) =>
        new Promise<{ stdout: string; code: number | null; cancelled: boolean }>((resolve) => {
          let stdout = "";
          const proc = spawn(getAutterBinary(), args, { cwd, timeout: 11 * 60_000 });
          token.onCancellationRequested(() => {
            proc.kill();
            resolve({ stdout: "", code: null, cancelled: true });
          });
          proc.stdout.on("data", (d) => {
            stdout += d.toString();
          });
          proc.stderr.on("data", () => {});
          proc.on("error", () => resolve({ stdout: "", code: null, cancelled: false }));
          proc.on("close", (code) => resolve({ stdout, code, cancelled: false }));
        }),
    );
    if (outcome.cancelled) {
      return;
    }
    let parsed: { error?: string; message?: string; localReview?: { findings?: CliFinding[] } | null } | null = null;
    try {
      parsed = JSON.parse(outcome.stdout.trim() || "null");
    } catch {
      parsed = null;
    }
    if (!parsed) {
      void vscode.window.showErrorMessage("autter: The review failed. Is the autter CLI up to date?");
      return;
    }
    if (parsed.error) {
      if (/autter login|HTTP 401/i.test(parsed.error)) {
        const choice = await vscode.window.showWarningMessage(`autter: ${parsed.error}`, "Sign in");
        if (choice === "Sign in") {
          await this.signIn();
        }
      } else {
        void vscode.window.showErrorMessage(`autter: ${parsed.error}`);
      }
      return;
    }
    if (!parsed.localReview) {
      void vscode.window.showInformationMessage(`autter: ${parsed.message ?? "No changes to review."}`);
      return;
    }
    const findings = parsed.localReview.findings ?? [];
    const shown = findings.filter((f) => f.local?.status === "unchanged" || f.local?.status === "moved").length;
    this.refreshAll(true);
    const text =
      findings.length === 0
        ? "autter: The review found nothing in your changes."
        : `autter: The review found ${findings.length} issue${findings.length === 1 ? "" : "s"}${shown < findings.length ? ` (${shown} shown in the code)` : ""}.`;
    const choice = await vscode.window.showInformationMessage(text, "Open findings");
    if (choice === "Open findings") {
      await this.openFindingsFile();
    }
  }

  /** Sign in with the CLI's device flow in a terminal, then refresh. */
  private async signIn(): Promise<void> {
    const binary = (await resolveAutterBinary()) ?? "autter";
    const terminal = vscode.window.createTerminal({ name: "autter login" });
    terminal.show();
    const quoted = /\s/.test(binary) ? `"${binary}"` : binary;
    terminal.sendText(`${quoted} login`);
    const sub = vscode.window.onDidCloseTerminal((closed) => {
      if (closed === terminal) {
        sub.dispose();
        this.refreshAll(true);
      }
    });
    this.disposables.push(sub);
    // Most people leave the terminal open after signing in: also refresh
    // when the window regains focus from the browser.
    const focus = vscode.window.onDidChangeWindowState((state) => {
      if (state.focused) {
        focus.dispose();
        this.refreshAll(true);
      }
    });
    this.disposables.push(focus);
  }
}
