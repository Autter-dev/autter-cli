#!/usr/bin/env python3
"""
Compare Git test failures between:
  - standard GIT_TEST_INSTALLED: /opt/homebrew/bin
  - autter GIT_TEST_INSTALLED:   /Users/svarlamov/projects/autter/target/gitwrap/bin

Runs prove on a pattern (default: t[0-9]*k.sh), parses the Test Summary Report,
and produces:
  - ./report.txt          (human-readable diff: which subtests failed on autter but not standard)
  - ./re-run-failed.sh    (executable: one command per impacted test, with -v and --run=1-<max>)
"""

import argparse
import datetime
import os
import re
import shlex
import stat
import subprocess
import sys
import hashlib
import csv
from typing import Dict, List, Set, Tuple

DEFAULT_STANDARD = "/opt/homebrew/bin"
DEFAULT_AUTTER = "/Users/svarlamov/projects/autter/target/gitwrap/bin"
DEFAULT_PATTERN = "t[0-9]*.sh"  # Test filter pattern
GIT_TESTS_DIR = "/Users/svarlamov/projects/git/t"
REPORT_PATH = "./tests/git-compat/report.txt"
RERUN_PATH = "./tests/git-compat/re-run-failed.sh"
WHITELIST_PATH = "./tests/git-compat/whitelist.csv"

def compute_standard_cache_path() -> str:
    """Compute cache file path for the standard git run based on defaults.
    Hash includes DEFAULT_STANDARD + DEFAULT_PATTERN + GIT_TESTS_DIR.
    """
    key = f"{DEFAULT_STANDARD}|{DEFAULT_PATTERN}|{GIT_TESTS_DIR}"
    digest = hashlib.sha256(key.encode("utf-8")).hexdigest()
    return f"./tests/git-compat/cached-standard-run-{digest}.txt"

def read_cached_standard_run(path: str) -> Tuple[int, str]:
    """Read cached standard run. First line may contain EXIT_CODE header."""
    with open(path, "r", encoding="utf-8") as f:
        first = f.readline()
        if first.startswith("EXIT_CODE:"):
            code_str = first.split(":", 1)[1].strip()
            try:
                code = int(code_str)
            except ValueError:
                code = 0
            output = f.read()
            return code, output
        else:
            # No header present; treat whole file as output and assume exit code 0
            rest = f.read()
            return 0, first + rest

def write_cached_standard_run(path: str, exit_code: int, output: str) -> None:
    """Write cache atomically with an EXIT_CODE header followed by output."""
    tmp_path = f"{path}.tmp"
    with open(tmp_path, "w", encoding="utf-8") as f:
        f.write(f"EXIT_CODE: {exit_code}\n")
        f.write(output)
    os.replace(tmp_path, path)

def run_prove(git_test_installed: str, pattern: str, jobs: int) -> Tuple[int, str]:
    """Run prove with the given GIT_TEST_INSTALLED, streaming output, and return (exit_code, combined_output)."""
    env = os.environ.copy()
    env["GIT_TEST_INSTALLED"] = git_test_installed

    # If no files matched, pass pattern through to prove as-is (prove will error if bad).
    cmd = f"bash -c 'prove -j{jobs} {shlex.quote(pattern)}'"

    # Stream stdout/stderr to terminal while also capturing for summary parsing.
    proc = subprocess.Popen(
        cmd,
        shell=True,
        env=env,
        cwd=GIT_TESTS_DIR,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
        bufsize=1,
    )

    combined_lines: List[str] = []
    assert proc.stdout is not None
    for line in proc.stdout:
        sys.stdout.write(line)
        sys.stdout.flush()
        combined_lines.append(line)

    proc.wait()
    return proc.returncode, "".join(combined_lines)


def extract_summary_section(prove_output: str) -> str:
    """Extract the 'Test Summary Report' block from prove output (if present)."""
    m = re.search(r"(?ms)^Test Summary Report\n[-]+\n(.*)$", prove_output)
    return m.group(1).strip() if m else ""


def parse_failed_indices_list(s: str) -> Set[int]:
    """
    Parse a comma/space separated list like '1-3, 5, 8-10' into {1,2,3,5,8,9,10}.
    Handles minor punctuation artifacts.
    """
    out: Set[int] = set()
    for tok in re.split(r"[,\s]+", s.strip()):
        if not tok:
            continue
        tok = tok.strip().rstrip(".")
        # Normalize weird trailing punctuation
        tok = re.sub(r"[^\d\-]", "", tok)
        if not tok:
            continue
        if "-" in tok:
            a, b = tok.split("-", 1)
            if a.isdigit() and b.isdigit():
                lo, hi = int(a), int(b)
                if hi < lo:
                    lo, hi = hi, lo
                out.update(range(lo, hi + 1))
        elif tok.isdigit():
            out.add(int(tok))
    return out


def parse_failures(summary_text: str) -> Dict[str, List[int]]:
    """
    Parse the summary block into {test_script: [failed_indices...]}.
    Robust to single 'Failed test:' or plural 'Failed tests:' and to wrapped lines.
    """
    failures: Dict[str, Set[int]] = {}
    lines = summary_text.splitlines()
    i = 0
    current = None

    header_re = re.compile(r"^(t\d{4}-.+?\.sh)\s+\(Wstat:.*\)$")
    failed_re = re.compile(r"^\s*Failed tests?:\s*(.+)$")

    while i < len(lines):
        line = lines[i].rstrip("\n")
        header = header_re.match(line.strip())
        if header:
            current = header.group(1)
            failures.setdefault(current, set())
            i += 1
            continue

        if current:
            m = failed_re.match(line)
            if m:
                first = m.group(1).strip()
                # Continue collecting wrapped numeric lines
                j = i + 1
                cont_parts: List[str] = []
                while j < len(lines):
                    nxt = lines[j]
                    if re.match(r"^\s{2,}[\d,\-\s\.]+$", nxt):
                        cont_parts.append(nxt.strip())
                        j += 1
                    else:
                        break
                full = ", ".join([first] + cont_parts) if cont_parts else first
                failures[current].update(parse_failed_indices_list(full))
                i = j
                continue

        i += 1

    return {k: sorted(v) for k, v in failures.items()}


def condense_indices(nums: List[int]) -> str:
    """Turn [1,2,3,5,8,9,10] into '1-3, 5, 8-10'."""
    if not nums:
        return ""
    nums = sorted(nums)
    ranges = []
    start = prev = nums[0]
    for n in nums[1:]:
        if n == prev + 1:
            prev = n
        else:
            ranges.append(f"{start}-{prev}" if start != prev else f"{start}")
            start = prev = n
    ranges.append(f"{start}-{prev}" if start != prev else f"{start}")
    return ", ".join(ranges)


def compute_diff(
    ai_fail: Dict[str, List[int]], std_fail: Dict[str, List[int]]
) -> Dict[str, List[int]]:
    """
    Return per-test indices that failed under autter but NOT under standard.
    """
    diff: Dict[str, List[int]] = {}
    for test, ai_list in ai_fail.items():
        ai_set = set(ai_list)
        std_set = set(std_fail.get(test, []))
        only_ai = sorted(ai_set - std_set)
        if only_ai:
            diff[test] = only_ai
    return diff


def load_whitelist(path: str) -> Dict[str, Set[int]]:
    """Load whitelist CSV mapping test script -> set of subtest indices to ignore.
    Expects header with columns including 'file' and 'test' (or 'tests').
    The 'test(s)' field may contain comma/range lists like '1-3, 5, 8'.
    """
    whitelist: Dict[str, Set[int]] = {}
    if not os.path.exists(path):
        return whitelist
    try:
        with open(path, "r", encoding="utf-8", newline="") as f:
            reader = csv.DictReader(f)
            for row in reader:
                file_key = (row.get("file") or "").strip().strip('"')
                tests_field = (row.get("test") or row.get("tests") or "").strip().strip('"')
                if not file_key or not tests_field:
                    continue
                indices = parse_failed_indices_list(tests_field)
                if not indices:
                    continue
                whitelist.setdefault(file_key, set()).update(indices)
    except Exception as e:
        print(f"[!] Failed to read whitelist {path}: {e}")
    return whitelist


def apply_whitelist(diff: Dict[str, List[int]], whitelist: Dict[str, Set[int]]) -> Dict[str, List[int]]:
    """Filter out any diff entries that are whitelisted.
    If a test has all its failing indices whitelisted, drop it from the diff entirely.
    """
    if not whitelist:
        return diff
    filtered: Dict[str, List[int]] = {}
    for test_name, indices in diff.items():
        wl = whitelist.get(test_name)
        if wl:
            remaining = [i for i in indices if i not in wl]
        else:
            remaining = list(indices)
        if remaining:
            filtered[test_name] = remaining
    return filtered


def write_rerun_script(diff: Dict[str, List[int]], autter_path: str) -> None:
    """Create ./re-run-failed.sh with one command per impacted test."""
    lines = [
        "#!/usr/bin/env bash",
        "set -euo pipefail",
        "",
        f"# Auto-generated by compare_prove_runs.py on {datetime.datetime.now().isoformat(timespec='seconds')}",
        f"# Re-runs only the subtests that failed on autter but not on standard.",
        "",
    ]
    for test in sorted(diff.keys()):
        failed = diff[test]
        highest = max(failed)
        comment = f"# {test}: failed (autter only) subtests: {condense_indices(failed)}"
        cmd = (
            f"GIT_TEST_INSTALLED='{autter_path}' ./{test} -v --run=1-{highest}"
        )
        lines.append(comment)
        lines.append(cmd)
        lines.append("")

    with open(RERUN_PATH, "w", encoding="utf-8") as f:
        f.write("\n".join(lines))

    # chmod +x
    st = os.stat(RERUN_PATH)
    os.chmod(RERUN_PATH, st.st_mode | stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH)


def main():
    ap = argparse.ArgumentParser(description="Compare prove failures between standard and autter.")
    ap.add_argument("--pattern", default=DEFAULT_PATTERN, help="Glob of tests to run (default: %(default)s)")
    ap.add_argument("--jobs", type=int, default=8, help="prove -j (default: %(default)s)")
    ap.add_argument("--standard", default=DEFAULT_STANDARD, help="GIT_TEST_INSTALLED for standard git")
    ap.add_argument("--autter", default=DEFAULT_AUTTER, help="GIT_TEST_INSTALLED for autter wrapper")
    ap.add_argument("--force", action="store_true", help="Force running standard and ignore cached output")
    args = ap.parse_args()

    started = datetime.datetime.now()
    cache_path = compute_standard_cache_path()
    if not args.force and os.path.exists(cache_path):
        print(f"[+] Using cached standard git run: {cache_path}")
        std_code, std_out = read_cached_standard_run(cache_path)
    else:
        print(f"[+] Running prove for standard git: {args.standard}")
        std_code, std_out = run_prove(args.standard, args.pattern, args.jobs)
        try:
            write_cached_standard_run(cache_path, std_code, std_out)
            print(f"[+] Cached standard run written to {cache_path}")
        except Exception as e:
            print(f"[!] Failed to write cache {cache_path}: {e}")
    std_summary = extract_summary_section(std_out)
    std_fail = parse_failures(std_summary)

    print(f"[+] Running prove for autter: {args.autter}")
    ai_code, ai_out = run_prove(args.autter, args.pattern, args.jobs)
    ai_summary = extract_summary_section(ai_out)
    ai_fail = parse_failures(ai_summary)

    diff = compute_diff(ai_fail, std_fail)
    whitelist = load_whitelist(WHITELIST_PATH)
    if whitelist:
        diff = apply_whitelist(diff, whitelist)
    write_rerun_script(diff, args.autter)

    # Build a concise report
    lines = []
    lines.append(f"Git test comparison report")
    lines.append(f"Generated: {started.isoformat(timespec='seconds')}")
    lines.append(f"Pattern: {args.pattern}  |  prove -j{args.jobs}")
    lines.append(f"standard GIT_TEST_INSTALLED: {args.standard} (exit {std_code})")
    lines.append(f"autter   GIT_TEST_INSTALLED: {args.autter} (exit {ai_code})")
    lines.append("")
    lines.append("Impacted tests (failed on autter but NOT on standard):")
    if not diff:
        lines.append("  (none)")
    else:
        for test in sorted(diff.keys()):
            only_ai = diff[test]
            lines.append(f"  {test}: {condense_indices(only_ai)}   [re-run: --run=1-{max(only_ai)}]")
    lines.append("")
    lines.append("---- Standard (Test Summary Report) ----")
    lines.append(std_summary or "(no failures)")
    lines.append("")
    lines.append("---- Autter (Test Summary Report) ----")
    lines.append(ai_summary or "(no failures)")
    lines.append("")
    lines.append(f"Re-run script written to: {RERUN_PATH}")

    report = "\n".join(lines)
    with open(REPORT_PATH, "w", encoding="utf-8") as f:
        f.write(report)

    print(report)
    print(f"[+] Report saved to {REPORT_PATH}")
    print(f"[+] Re-run script saved to {RERUN_PATH}")


if __name__ == "__main__":
    try:
        main()
    except KeyboardInterrupt:
        sys.exit(130)
