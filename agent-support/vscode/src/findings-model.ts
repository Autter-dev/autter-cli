/**
 * Findings in the editor: the parts that don't need VS Code, so they can be
 * unit-tested. Reads `autter findings --json` and keeps each finding on the
 * right line while the user types.
 *
 * Rule: a finding is only ever shown on a line the review saw. The CLI drops
 * findings whose line changed since the reviewed commit (`local.status:
 * "changed"`); while typing, {@link applyEdit} drops a finding as soon as its
 * own line is edited, and moves it when lines above it are added or removed.
 */

export type FindingSeverity = "critical" | "warning" | "info";

export type PlacementStatus =
  | "unchanged"
  | "moved"
  | "changed"
  | "file_deleted"
  | "unverified"
  | "no_line";

export interface CliFinding {
  id: string;
  title: string;
  body: string;
  path: string;
  /** 1-based line at the reviewed commit. */
  line: number;
  severity: FindingSeverity;
  category?: string;
  fixPrompt: string | null;
  url: string | null;
  local?: { status: PlacementStatus; line: number | null };
}

export interface CliFindingsResult {
  repository?: { name: string; fullName: string };
  pullRequest: {
    number: number;
    title: string | null;
    url: string | null;
  } | null;
  review: { headSha: string; stale: boolean } | null;
  findings: CliFinding[];
  /** The saved `autter review` for this branch, placed on the working tree. */
  localReview?: {
    reviewId?: string;
    reviewedAt?: string | null;
    baseRef?: string | null;
    findings: CliFinding[];
  } | null;
  local?: {
    workdir?: string;
    branch?: string | null;
    markdownPath?: string;
    reviewedCommitAvailable?: boolean;
  };
}

export type FindingsOutcome =
  | { kind: "ok"; result: CliFindingsResult }
  | { kind: "signed_out"; message: string }
  /** Not a repo Autter can look up (no git repo, no remote, not connected). */
  | { kind: "unavailable"; message: string }
  | { kind: "error"; message: string };

const SIGNED_OUT = /autter login|sign in|signed in|HTTP 401/i;
const UNAVAILABLE =
  /inside a git repository|has no remote|detached|repo_not_found|not connected|ambiguous|Unknown repository|not found in this organization/i;

/** Classify what `autter findings --json` printed (and its exit code). */
export function parseFindingsOutput(stdout: string, exitCode: number | null): FindingsOutcome {
  let parsed: unknown;
  try {
    parsed = JSON.parse(stdout.trim() || "null");
  } catch {
    return { kind: "error", message: "autter findings printed something that isn't JSON. Update the autter CLI." };
  }
  if (parsed && typeof parsed === "object" && typeof (parsed as { error?: unknown }).error === "string") {
    const message = (parsed as { error: string }).error;
    if (SIGNED_OUT.test(message)) {
      return { kind: "signed_out", message };
    }
    if (UNAVAILABLE.test(message)) {
      return { kind: "unavailable", message };
    }
    return { kind: "error", message };
  }
  if (exitCode !== 0 || !parsed || typeof parsed !== "object" || !Array.isArray((parsed as { findings?: unknown }).findings)) {
    return { kind: "error", message: "autter findings failed. Is the autter CLI up to date?" };
  }
  return { kind: "ok", result: parsed as CliFindingsResult };
}

/** Where a finding comes from: the pull request's review, or `autter review` on this machine. */
export type FindingOrigin = "pr" | "local";

/** A finding placed on a 0-based line of a file in the working tree. */
export interface PlacedFinding {
  finding: CliFinding;
  origin: FindingOrigin;
  /** Repo-relative path. */
  path: string;
  /** 0-based line in the current document. */
  line: number;
}

/** Findings that have a line in the working tree, grouped by repo-relative path. */
export function placeFindings(
  result: CliFindingsResult,
  dismissed: ReadonlySet<string> = new Set(),
): Map<string, PlacedFinding[]> {
  const byPath = new Map<string, PlacedFinding[]>();
  const sources: Array<[FindingOrigin, CliFinding[]]> = [
    ["pr", result.findings],
    ["local", result.localReview?.findings ?? []],
  ];
  for (const [origin, findings] of sources) {
    for (const finding of findings) {
      if (dismissed.has(finding.id)) {
        continue;
      }
      const status = finding.local?.status;
      const line = finding.local?.line;
      if ((status !== "unchanged" && status !== "moved") || typeof line !== "number" || line < 1) {
        continue;
      }
      const list = byPath.get(finding.path) ?? [];
      list.push({ finding, origin, path: finding.path, line: line - 1 });
      byPath.set(finding.path, list);
    }
  }
  return byPath;
}

/** One text change, in the document's lines before the change (0-based). */
export interface LineEdit {
  startLine: number;
  startCharacter: number;
  endLine: number;
  endCharacter: number;
  /** The inserted text. */
  text: string;
}

/**
 * Move findings through one edit. A finding on a line the edit touches is
 * dropped (the review never saw the new code); one below it moves by the
 * lines added or removed. Pressing Enter at the very start of a finding's
 * line only pushes it down.
 */
export function applyEdit(placed: PlacedFinding[], edit: LineEdit): PlacedFinding[] {
  const inserted = edit.text.split("\n").length - 1;
  const delta = inserted - (edit.endLine - edit.startLine);
  // Text ending in a newline inserted at column 0 leaves the line's own
  // content as it was, one or more lines further down.
  const pushesDown =
    edit.startLine === edit.endLine &&
    edit.startCharacter === 0 &&
    edit.endCharacter === 0 &&
    edit.text.endsWith("\n");
  const out: PlacedFinding[] = [];
  for (const p of placed) {
    if (p.line < edit.startLine) {
      out.push(p);
    } else if (p.line > edit.endLine || (p.line === edit.startLine && pushesDown)) {
      out.push({ ...p, line: p.line + delta });
    }
    // Otherwise the edit touched the finding's line: drop it.
  }
  return out;
}

const SEVERITY_LABEL: Record<FindingSeverity, string> = {
  critical: "Critical",
  warning: "Warning",
  info: "Info",
};

export function severityLabel(severity: FindingSeverity): string {
  return SEVERITY_LABEL[severity] ?? "Info";
}

/** The diagnostic text: title, then the body, then the next step. */
export function diagnosticMessage(finding: CliFinding): string {
  const parts = [finding.title.trim()];
  if (finding.body.trim()) {
    parts.push(finding.body.trim());
  }
  if (finding.fixPrompt?.trim()) {
    parts.push(`Next step: ${finding.fixPrompt.trim()}`);
  }
  return parts.join("\n\n");
}

/** The short text shown after the line when inline annotations are on. */
export function inlineText(finding: CliFinding, max = 90, origin: FindingOrigin = "pr"): string {
  const who = origin === "local" ? "autter review" : "autter";
  const text = `${who} ${severityLabel(finding.severity).toLowerCase()}: ${finding.title.replace(/\s+/g, " ").trim()}`;
  return text.length > max ? `${text.slice(0, max - 1)}…` : text;
}

/** The status bar text for a result (`shown`: findings with a place in the code). */
export function statusSummary(result: CliFindingsResult, shown: number): { text: string; tooltip: string } {
  const local = result.localReview?.findings ?? [];
  const pr = result.pullRequest && result.review ? result.findings : [];
  const total = pr.length + local.length;
  const lines: string[] = [];
  if (!result.pullRequest) {
    lines.push("No open pull request for this branch yet.");
  } else if (!result.review) {
    lines.push(`Autter has not reviewed PR #${result.pullRequest.number} yet.`);
  } else {
    lines.push(
      `${pr.length} finding${pr.length === 1 ? "" : "s"} on PR #${result.pullRequest.number}${result.review.stale ? " (from an older push)" : ""}.`,
    );
  }
  if (result.localReview) {
    lines.push(`${local.length} from your last local review (autter review).`);
  } else {
    lines.push("Run \"Autter: Review My Changes\" to review your changes before a PR.");
  }
  const elsewhere = total - shown;
  if (elsewhere > 0) {
    lines.push(`${elsewhere} not shown in the code: on lines you changed since the review, or about the change as a whole.`);
  }
  lines.push("Click to open .autter/findings.md.");
  if (total === 0 && !result.pullRequest && !result.localReview) {
    return { text: "$(shield) autter: no PR", tooltip: lines.join("\n") };
  }
  if (total === 0 && result.pullRequest && !result.review && !result.localReview) {
    return { text: `$(shield) autter: PR #${result.pullRequest.number} not reviewed`, tooltip: lines.join("\n") };
  }
  const all = [...pr, ...local];
  const icon = all.some((f) => f.severity === "critical") ? "$(error)" : total > 0 ? "$(warning)" : "$(pass)";
  return { text: `${icon} autter: ${total}`, tooltip: lines.join("\n") };
}

/** A finding from a local review (`autter review`), whose dismissal is sent to Autter. */
export function isLocalFindingId(id: string): boolean {
  return /^local-[0-9a-f]{1,8}-\d+$/i.test(id);
}
