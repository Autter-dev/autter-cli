import * as assert from "assert";
import * as fs from "node:fs";
import * as path from "node:path";
import { PostHog } from "posthog-node";
import { isVersionSatisfied } from "../utils/semver";

suite("PostHog dependency version", () => {
  test("pins and installs posthog-node at least 6.0.6", async () => {
    const manifestPath = path.join(__dirname, "..", "..", "package.json");
    const manifest = JSON.parse(fs.readFileSync(manifestPath, "utf-8"));
    const declared = String(manifest.dependencies["posthog-node"] ?? "");
    const declaredVersion = /(\d+\.\d+\.\d+)/.exec(declared)?.[1] ?? "0.0.0";

    assert.ok(
      isVersionSatisfied(declaredVersion, "6.0.6"),
      `expected package.json to pin posthog-node >= 6.0.6, got ${declared}`,
    );

    const client = new PostHog("phc_synthetic_test_token", {
      host: "https://us.i.posthog.com",
      flushAt: 1,
      flushInterval: 60_000,
    });

    try {
      assert.ok(
        isVersionSatisfied(client.getLibraryVersion(), "6.0.6"),
        `expected posthog-node >= 6.0.6, got ${client.getLibraryVersion()}`,
      );
    } finally {
      await client.shutdown(1_000);
    }
  });
});
