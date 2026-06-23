#!/usr/bin/env python3
"""
Benchmark git notes fetch and merge performance at various scales.

This script simulates the autter workload:
- Single notes ref (refs/notes/ai)
- Fetch into tracking ref (refs/notes/ai-remote/origin)
- Merge from tracking ref into local refs/notes/ai using "ours" strategy
- Notes contain JSON-like content (simulating authorship logs)
"""

import subprocess
import tempfile
import shutil
import time
import os
import sys
import json
from pathlib import Path


def run_git(cmd, cwd, env=None):
    """Run a git command and return the result."""
    full_cmd = ["/opt/homebrew/bin/git"] + cmd
    result = subprocess.run(
        full_cmd,
        cwd=cwd,
        capture_output=True,
        text=True,
        env={**os.environ, **(env or {})}
    )
    if result.returncode != 0:
        print(f"Command failed: {' '.join(full_cmd)}")
        print(f"Error: {result.stderr}")
    return result


def generate_authorship_note(commit_hash, index):
    """Generate a realistic authorship note JSON."""
    return json.dumps({
        "metadata": {
            "schema_version": "3.0",
            "commit": commit_hash,
            "timestamp": "2025-10-08T12:00:00Z"
        },
        "session": {
            "id": f"session_{index}",
            "checkpoints": [
                {
                    "type": "user_prompt",
                    "content": f"Implement feature {index}",
                    "timestamp": "2025-10-08T12:00:00Z"
                },
                {
                    "type": "ai_response",
                    "content": f"Here's the implementation for feature {index}...",
                    "timestamp": "2025-10-08T12:01:00Z"
                }
            ]
        },
        "authorship": {
            "ai_percentage": 0.75,
            "human_percentage": 0.25
        }
    }, indent=2)


def create_test_repo(base_dir, name, num_commits, num_notes):
    """Create a test repository with specified number of commits and notes."""
    repo_path = base_dir / name
    repo_path.mkdir()

    print(f"Creating {name} with {num_commits} commits and {num_notes} notes...")

    run_git(["init", "-b", "main"], repo_path)
    run_git(["config", "user.name", "Test User"], repo_path)
    run_git(["config", "user.email", "test@example.com"], repo_path)

    # Create commits
    test_file = repo_path / "test.txt"
    commits = []
    for i in range(num_commits):
        test_file.write_text(f"Commit {i}\n")
        run_git(["add", "test.txt"], repo_path)
        run_git(["commit", "-m", f"Commit {i}"], repo_path)

        # Get the commit hash
        result = run_git(["rev-parse", "HEAD"], repo_path)
        commits.append(result.stdout.strip())

        if (i + 1) % 1000 == 0:
            print(f"  Created {i + 1} commits...")

    # Add notes to commits using refs/notes/ai
    for i, commit in enumerate(commits[:num_notes]):
        note_content = generate_authorship_note(commit, i)
        run_git(["notes", "--ref=ai", "add", "-f", "-m", note_content, commit], repo_path)

        if (i + 1) % 1000 == 0:
            print(f"  Created {i + 1} notes...")

    print(f"  ✓ Created {name}")
    return repo_path


def clone_repo(source, dest):
    """Clone a repository."""
    run_git(["clone", str(source), str(dest)], source.parent)
    # Fetch notes into tracking ref (simulating autter's fetch pattern)
    tracking_ref = "refs/notes/ai-remote/origin"
    run_git(["fetch", "origin", f"+refs/notes/ai:{tracking_ref}"], dest)
    # Copy tracking ref to local notes ref
    run_git(["update-ref", "refs/notes/ai", tracking_ref], dest)


def benchmark_fetch_merge(repo_path, num_new_notes):
    """Benchmark fetch and merge operations matching autter's workload."""
    print(f"\nBenchmarking with {num_new_notes} new commits and notes...")

    # Get the remote repo path (origin)
    result = run_git(["remote", "get-url", "origin"], repo_path)
    remote_path = Path(result.stdout.strip())

    # Create new commits with notes in the remote (simulating work from another clone)
    test_file = remote_path / "test.txt"
    new_commits = []

    for i in range(num_new_notes):
        # Create a new commit
        test_file.write_text(f"New commit {i}\n")
        run_git(["add", "test.txt"], remote_path)
        run_git(["commit", "-m", f"New commit {i}"], remote_path)

        # Get the commit hash
        result = run_git(["rev-parse", "HEAD"], remote_path)
        commit_hash = result.stdout.strip()
        new_commits.append(commit_hash)

        # Add a note to the new commit
        note_content = generate_authorship_note(commit_hash, 100000 + i)
        run_git(["notes", "--ref=ai", "add", "-f", "-m", note_content, commit_hash], remote_path)

    # Simulate autter's pre-push fetch and merge workflow
    tracking_ref = "refs/notes/ai-remote/origin"

    # Benchmark fetch into tracking ref
    start = time.time()
    result = run_git(
        ["fetch", "origin", f"+refs/notes/ai:{tracking_ref}"],
        repo_path
    )
    fetch_time = time.time() - start

    # Benchmark merge from tracking ref into refs/notes/ai with "ours" strategy
    start = time.time()
    result = run_git(
        ["notes", "--ref=ai", "merge", "-s", "ours", tracking_ref],
        repo_path
    )
    merge_time = time.time() - start

    return fetch_time, merge_time


def main():
    # Test configurations: (num_commits, num_notes)
    test_configs = [
        (1000, 1000),
        (10000, 10000),
        (50000, 50000),
        (100000, 100000),
    ]

    # Allow override from command line
    if len(sys.argv) > 1:
        num = int(sys.argv[1])
        test_configs = [(num, num)]

    results = []

    for num_commits, num_notes in test_configs:
        with tempfile.TemporaryDirectory() as tmpdir:
            tmpdir = Path(tmpdir)

            print(f"\n{'='*60}")
            print(f"Testing with {num_commits:,} commits and {num_notes:,} notes")
            print(f"{'='*60}")

            # Create origin repo
            origin = create_test_repo(tmpdir, "origin", num_commits, num_notes)

            # Create clone
            clone_path = tmpdir / "clone"
            print(f"\nCloning repository...")
            clone_repo(origin, clone_path)

            # Run benchmarks with different amounts of new notes
            for num_new in [10, 100, 500, 1000]:
                fetch_time, merge_time = benchmark_fetch_merge(clone_path, num_new)

                result = {
                    "total_commits": num_commits,
                    "total_notes": num_notes,
                    "new_notes": num_new,
                    "fetch_time": fetch_time,
                    "merge_time": merge_time,
                    "total_time": fetch_time + merge_time
                }
                results.append(result)

                print(f"\n  With {num_new} new remote notes:")
                print(f"    Fetch time:  {fetch_time:.3f}s")
                print(f"    Merge time:  {merge_time:.3f}s")
                print(f"    Total time:  {fetch_time + merge_time:.3f}s")

    # Print summary
    print(f"\n{'='*60}")
    print("SUMMARY")
    print(f"{'='*60}")
    print(f"{'Total Notes':<15} {'New Notes':<12} {'Fetch (s)':<12} {'Merge (s)':<12} {'Total (s)':<12}")
    print(f"{'-'*60}")
    for r in results:
        print(f"{r['total_notes']:<15,} {r['new_notes']:<12} "
              f"{r['fetch_time']:<12.3f} {r['merge_time']:<12.3f} {r['total_time']:<12.3f}")


if __name__ == "__main__":
    main()
