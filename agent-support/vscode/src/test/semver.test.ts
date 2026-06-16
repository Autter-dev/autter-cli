import * as assert from "assert";
import { compareSemver, isVersionSatisfied } from "../utils/semver";

suite("Semver Utilities", () => {
  test("compareSemver handles identical versions", () => {
    assert.strictEqual(compareSemver("1.0.3", "1.0.3"), 0);
    assert.strictEqual(compareSemver("v1.2.0", "1.2.0"), 0);
  });

  test("compareSemver detects higher major, minor, and patch numbers", () => {
    assert.ok(compareSemver("2.0.0", "1.9.9") > 0);
    assert.ok(compareSemver("1.10.0", "1.9.9") > 0);
    assert.ok(compareSemver("1.0.5", "1.0.4") > 0);
  });

  test("compareSemver detects lower versions", () => {
    assert.ok(compareSemver("0.9.9", "1.0.0") < 0);
    assert.strictEqual(compareSemver("1.2.3", "1.2.3+build"), 0);
    assert.ok(compareSemver("1.2.3-alpha", "1.2.3") < 0);
  });

  test("compareSemver orders pre-release versions correctly", () => {
    assert.ok(compareSemver("1.2.3-alpha", "1.2.3-alpha.1") < 0);
    assert.ok(compareSemver("1.2.3-alpha.2", "1.2.3-alpha.10") < 0);
    assert.ok(compareSemver("1.2.3-alpha.1", "1.2.3-beta") < 0);
    assert.ok(compareSemver("1.2.3-beta", "1.2.3-beta.1") < 0);
    assert.strictEqual(compareSemver("1.2.3-alpha+001", "1.2.3-alpha"), 0);
  });

  test("isVersionSatisfied evaluates thresholds", () => {
    assert.strictEqual(isVersionSatisfied("1.0.3", "1.0.3"), true);
    assert.strictEqual(isVersionSatisfied("1.0.4", "1.0.3"), true);
    assert.strictEqual(isVersionSatisfied("1.0.2", "1.0.3"), false);
    assert.strictEqual(isVersionSatisfied("1.1.0-alpha", "1.0.3"), true);
    assert.strictEqual(isVersionSatisfied("1.0.3-alpha", "1.0.3"), false);
  });
});
