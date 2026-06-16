export type SemverPart = number | string;

interface ParsedSemver {
  core: number[];
  prerelease: SemverPart[];
}

function toNumeric(value: string): SemverPart {
  const numeric = Number(value);
  return Number.isNaN(numeric) ? value : numeric;
}

export function parseSemver(version: string): ParsedSemver {
  const trimmed = version.trim().replace(/^v/i, "");

  const [corePart = "", preReleasePart = ""] = trimmed.split("-", 2);
  const [coreOnly] = corePart.split("+", 2); // strip build metadata if present
  const [preReleaseOnly] = preReleasePart.split("+", 2);
  const coreSegments = coreOnly
    .split(".")
    .filter((segment) => segment.length > 0)
    .map((segment) => Number.parseInt(segment, 10))
    .map((value) => (Number.isNaN(value) ? 0 : value));

  const prereleaseSegments = preReleaseOnly
    .split(".")
    .filter((segment) => segment.length > 0)
    .map(toNumeric);

  return {
    core: coreSegments,
    prerelease: prereleaseSegments,
  };
}

function compareCore(a: number[], b: number[]): number {
  const length = Math.max(a.length, b.length);
  for (let i = 0; i < length; i++) {
    const aValue = a[i] ?? 0;
    const bValue = b[i] ?? 0;
    if (aValue > bValue) {
      return 1;
    }
    if (aValue < bValue) {
      return -1;
    }
  }
  return 0;
}

function comparePrerelease(a: SemverPart[], b: SemverPart[]): number {
  if (a.length === 0 && b.length === 0) {
    return 0;
  }

  if (a.length === 0) {
    return 1;
  }

  if (b.length === 0) {
    return -1;
  }

  const length = Math.max(a.length, b.length);
  for (let i = 0; i < length; i++) {
    const aValue = a[i];
    const bValue = b[i];

    if (aValue === undefined) {
      return -1;
    }
    if (bValue === undefined) {
      return 1;
    }

    if (aValue === bValue) {
      continue;
    }

    const aIsNumber = typeof aValue === "number";
    const bIsNumber = typeof bValue === "number";

    if (aIsNumber && bIsNumber) {
      return (aValue as number) > (bValue as number) ? 1 : -1;
    }

    if (aIsNumber !== bIsNumber) {
      return aIsNumber ? -1 : 1;
    }

    return (aValue as string) > (bValue as string) ? 1 : -1;
  }

  return 0;
}

export function compareSemver(a: string, b: string): number {
  const parsedA = parseSemver(a);
  const parsedB = parseSemver(b);

  const coreComparison = compareCore(parsedA.core, parsedB.core);
  if (coreComparison !== 0) {
    return coreComparison;
  }

  return comparePrerelease(parsedA.prerelease, parsedB.prerelease);
}

export function isVersionSatisfied(actual: string, minimum: string): boolean {
  return compareSemver(actual, minimum) >= 0;
}
