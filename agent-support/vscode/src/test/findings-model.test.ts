import * as assert from "assert";
import {
  applyEdit,
  type CliFinding,
  type CliFindingsResult,
  diagnosticMessage,
  inlineText,
  isLocalFindingId,
  parseFindingsOutput,
  type PlacedFinding,
  placeFindings,
  statusSummary,
} from "../findings-model";

function finding(id: string, over: Partial<CliFinding> = {}): CliFinding {
  return {
    id,
    title: `Title ${id}`,
    body: `Body ${id}`,
    path: "src/a.ts",
    line: 3,
    severity: "warning",
    fixPrompt: `Fix ${id}`,
    url: `https://app.autter.dev/acme/repositories/web/pull/7?finding=${id}`,
    local: { status: "unchanged", line: 3 },
    ...over,
  };
}

function result(findings: CliFinding[]): CliFindingsResult {
  return {
    pullRequest: { number: 7, title: "Add thing", url: "https://app/pull/7" },
    review: { headSha: "abc", stale: false },
    findings,
  };
}

function placed(line: number, id = "f"): PlacedFinding {
  return { finding: finding(id), origin: "pr", path: "src/a.ts", line };
}

suite("Findings model", () => {
  test("parses CLI output into outcomes", () => {
    const ok = parseFindingsOutput(JSON.stringify(result([finding("a")])), 0);
    assert.strictEqual(ok.kind, "ok");
    assert.strictEqual(
      parseFindingsOutput('{"error":"Generic error: Sign in with `autter login` first."}', 1).kind,
      "signed_out",
    );
    assert.strictEqual(
      parseFindingsOutput('{"error":"Run autter findings inside a git repository."}', 1).kind,
      "unavailable",
    );
    assert.strictEqual(parseFindingsOutput('{"error":"HTTP 500: boom"}', 1).kind, "error");
    assert.strictEqual(parseFindingsOutput("not json", 0).kind, "error");
    assert.strictEqual(parseFindingsOutput("{}", 0).kind, "error");
  });

  test("places only findings that still sit on a line the review saw", () => {
    const byPath = placeFindings(
      result([
        finding("same"),
        finding("moved", { local: { status: "moved", line: 9 } }),
        finding("changed", { local: { status: "changed", line: null } }),
        finding("unverified", { local: { status: "unverified", line: null } }),
        finding("pr", { path: "", local: { status: "no_line", line: null } }),
        finding("hidden"),
      ]),
      new Set(["hidden"]),
    );
    const list = byPath.get("src/a.ts") ?? [];
    assert.deepStrictEqual(
      list.map((p) => [p.finding.id, p.line]),
      [
        ["same", 2],
        ["moved", 8],
      ],
    );
    assert.strictEqual(byPath.size, 1);
  });

  test("places the saved local review next to the PR's findings", () => {
    const byPath = placeFindings({
      ...result([finding("pr")]),
      localReview: {
        findings: [
          finding("local-1", { local: { status: "moved", line: 7 } }),
          finding("local-2", { local: { status: "changed", line: null } }),
        ],
      },
    });
    assert.deepStrictEqual(
      (byPath.get("src/a.ts") ?? []).map((p) => [p.finding.id, p.origin, p.line]),
      [
        ["pr", "pr", 2],
        ["local-1", "local", 6],
      ],
    );
    assert.strictEqual(inlineText(finding("x"), 90, "local"), "autter review warning: Title x");
  });

  test("an edit above a finding moves it; an edit on its line drops it", () => {
    const list = [placed(2, "a"), placed(5, "b"), placed(9, "c")];
    // Two lines inserted at the end of line 0.
    const moved = applyEdit(list, { startLine: 0, startCharacter: 4, endLine: 0, endCharacter: 4, text: "\nx\ny" });
    assert.deepStrictEqual(moved.map((p) => p.line), [4, 7, 11]);
    // Typing on line 5 drops b.
    const typed = applyEdit(list, { startLine: 5, startCharacter: 3, endLine: 5, endCharacter: 3, text: "z" });
    assert.deepStrictEqual(typed.map((p) => p.finding.id), ["a", "c"]);
    // Deleting lines 4–6 (joining them) drops b and moves c up by 2.
    const joined = applyEdit(list, { startLine: 4, startCharacter: 0, endLine: 6, endCharacter: 0, text: "" });
    assert.deepStrictEqual(joined.map((p) => [p.finding.id, p.line]), [["a", 2], ["c", 7]]);
  });

  test("Enter at the start of a finding's line pushes it down instead of dropping it", () => {
    const list = [placed(5)];
    const out = applyEdit(list, { startLine: 5, startCharacter: 0, endLine: 5, endCharacter: 0, text: "\n" });
    assert.deepStrictEqual(out.map((p) => p.line), [6]);
    // Replacing text at the start of the line is an edit of the line.
    const replaced = applyEdit(list, { startLine: 5, startCharacter: 0, endLine: 5, endCharacter: 3, text: "x\n" });
    assert.strictEqual(replaced.length, 0);
  });

  test("formats diagnostic, inline and status text", () => {
    assert.strictEqual(diagnosticMessage(finding("a")), "Title a\n\nBody a\n\nNext step: Fix a");
    assert.strictEqual(inlineText(finding("a")), "autter warning: Title a");
    assert.ok(inlineText(finding("a", { title: "x".repeat(200) })).endsWith("…"));

    const summary = statusSummary(result([finding("a", { severity: "critical" }), finding("b")]), 1);
    assert.strictEqual(summary.text, "$(error) autter: 2");
    assert.ok(summary.tooltip.includes("1 not shown in the code"));
    assert.strictEqual(statusSummary({ ...result([]), pullRequest: null }, 0).text, "$(shield) autter: no PR");
    const withLocal = statusSummary(
      { ...result([]), pullRequest: null, review: null, localReview: { findings: [finding("l", { severity: "critical" })] } },
      1,
    );
    assert.strictEqual(withLocal.text, "$(error) autter: 1");
    assert.ok(withLocal.tooltip.includes("1 from your last local review"));
  });

  test("tells local-review findings from PR findings", () => {
    assert.strictEqual(isLocalFindingId("local-0123abcd-3"), true);
    assert.strictEqual(isLocalFindingId("f-123"), false);
    assert.strictEqual(isLocalFindingId("local-xyz"), false);
  });
});
