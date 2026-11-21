import * as assert from "assert";
import { PostHog } from "posthog-node";

suite("PostHog telemetry client", () => {
  test("capture sends startup events to the capture v1 analytics endpoint", async () => {
    const requestedUrls: string[] = [];

    const client = new PostHog("phc_synthetic_test_token", {
      host: "https://us.i.posthog.com",
      flushAt: 1,
      flushInterval: 60_000,
      fetch: async (url) => {
        requestedUrls.push(String(url));
        const body = JSON.stringify({ status: 1, results: {} });
        return {
          status: 200,
          text: async () => body,
          json: async () => JSON.parse(body),
          headers: { get: () => null },
        };
      },
    });

    try {
      // Mirrors the extension's fire-and-forget startup capture call.
      client.capture({
        distinctId: "synthetic-distinct-id",
        event: "vscode_extension_startup",
        properties: { ide_host: "vscode" },
      });
      await client.flush();
    } finally {
      await client.shutdown(1_000);
    }

    assert.ok(
      requestedUrls.some((url) => url.endsWith("/i/v1/analytics/events")),
      `expected the capture v1 analytics endpoint, got: ${requestedUrls.join(", ")}`,
    );
  });
});