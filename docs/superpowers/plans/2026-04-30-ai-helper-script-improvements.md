# AI Helper Script Improvements Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add `resume` and `pr` commands to the AI helper script for streamlined worktree management and Claude session launching.

**Architecture:** Extract common worktree launching logic into a shared helper function. Add new commands that handle branch resolution (`resume`) and PR checkout (`pr`) before launching Claude. Refactor existing `cmd_new` to use the shared helper.

**Tech Stack:** Python 3, subprocess for git/gh CLI, argparse for command parsing

---

## File Structure

**Modified Files:**
- `scripts/ai` - Add helper functions, new commands, refactor existing code

**No new files needed** - all changes in the existing script.

---

### Task 1: Extract Helper Functions

**Files:**
- Modify: `scripts/ai:22-24` (extract `get_repo_root()`)
- Modify: `scripts/ai:44-48` (extract `open_worktree()`)

- [ ] **Step 1: Extract `get_repo_root()` function**

Add after the `set_tmux_window_name` function (after line 16):

```python
def get_repo_root():
    """Get the repository root directory."""
    return subprocess.run(
        ["git", "rev-parse", "--show-toplevel"],
        capture_output=True, text=True, check=True,
    ).stdout.strip()
```

- [ ] **Step 2: Extract `open_worktree()` function**

Add after the `get_repo_root` function:

```python
def open_worktree(worktree_dir, branch_name, resume_session=False):
    """Open Claude in the specified worktree."""
    set_tmux_window_name(branch_name)
    print(f"Launching claude in {worktree_dir}...")
    os.chdir(worktree_dir)
    claude_args = ["claude", "--dangerously-skip-permissions"]
    if resume_session:
        claude_args.append("--resume")
    os.execvp("claude", claude_args)
```

- [ ] **Step 3: Refactor `cmd_new` to use helper functions**

Replace lines 22-48 in `cmd_new` with:

```python
def cmd_new(args):
    """Create a new worktree from origin/main and open claude in it."""
    branch = args.branch
    repo_root = get_repo_root()

    worktree_dir = os.path.join(repo_root, ".worktrees", branch)

    if os.path.exists(worktree_dir):
        print(f"Worktree already exists: {worktree_dir}", file=sys.stderr)
        sys.exit(1)

    base = args.base
    if base.startswith("origin/"):
        remote_branch = base[len("origin/"):]
        print(f"Fetching {base}...")
        run(["git", "fetch", "origin", remote_branch])
    else:
        print(f"Using local ref {base}...")

    print(f"Creating worktree at {worktree_dir} on branch {branch} from {base}...")
    run(["git", "worktree", "add", "-b", branch, worktree_dir, base])

    open_worktree(worktree_dir, branch, resume_session=False)
```

- [ ] **Step 4: Test refactored `new` command**

Run: `./scripts/ai new test-refactor-branch`

Expected: Creates worktree and launches Claude (exit Claude immediately to continue)

- [ ] **Step 5: Clean up test worktree**

Run: `git worktree remove .worktrees/test-refactor-branch && git branch -D test-refactor-branch`

- [ ] **Step 6: Commit refactoring**

```bash
git add scripts/ai
git commit -m "refactor: extract helper functions for worktree operations

Extract get_repo_root() and open_worktree() helpers to prepare for
new resume and pr commands.

Co-Authored-By: Claude Sonnet 4.5 <noreply@anthropic.com>"
```

---

### Task 2: Add Branch Checking Helper Functions

**Files:**
- Modify: `scripts/ai` (add helper functions after `open_worktree`)

- [ ] **Step 1: Add `check_local_branch_exists()` function**

Add after the `open_worktree` function:

```python
def check_local_branch_exists(branch):
    """Check if a local branch exists."""
    result = subprocess.run(
        ["git", "show-ref", "--verify", f"refs/heads/{branch}"],
        capture_output=True,
    )
    return result.returncode == 0
```

- [ ] **Step 2: Add `check_remote_branch_exists()` function**

Add after `check_local_branch_exists`:

```python
def check_remote_branch_exists(branch):
    """Check if a remote branch exists on origin."""
    result = subprocess.run(
        ["git", "show-ref", "--verify", f"refs/remotes/origin/{branch}"],
        capture_output=True,
    )
    return result.returncode == 0
```

- [ ] **Step 3: Add `is_branch_checked_out_in_root()` function**

Add after `check_remote_branch_exists`:

```python
def is_branch_checked_out_in_root(branch):
    """Check if a branch is currently checked out in the repo root."""
    result = subprocess.run(
        ["git", "-C", get_repo_root(), "symbolic-ref", "--short", "HEAD"],
        capture_output=True, text=True,
    )
    if result.returncode != 0:
        return False
    current_branch = result.stdout.strip()
    return current_branch == branch
```

- [ ] **Step 4: Commit helper functions**

```bash
git add scripts/ai
git commit -m "feat: add branch checking helper functions

Add helpers to check for local/remote branch existence and verify
branch checkout status in repo root.

Co-Authored-By: Claude Sonnet 4.5 <noreply@anthropic.com>"
```

---

### Task 3: Implement `resume` Command

**Files:**
- Modify: `scripts/ai` (add `cmd_resume` function and argparse configuration)

- [ ] **Step 1: Add `cmd_resume()` function**

Add after the helper functions (before `build_parser`):

```python
def cmd_resume(args):
    """Resume work on an existing branch."""
    branch = args.branch
    repo_root = get_repo_root()
    worktree_dir = os.path.join(repo_root, ".worktrees", branch)
    
    # 1. Check if worktree already exists
    if os.path.exists(worktree_dir):
        print(f"Using existing worktree: {worktree_dir}")
        open_worktree(worktree_dir, branch, resume_session=True)
        return
    
    # 2. Check if local branch exists
    local_branch_exists = check_local_branch_exists(branch)
    
    # 3. If not, check remote and fetch
    if not local_branch_exists:
        remote_branch_exists = check_remote_branch_exists(branch)
        if not remote_branch_exists:
            print(f"Error: Branch '{branch}' not found locally or on origin", file=sys.stderr)
            sys.exit(1)
        print(f"Fetching origin/{branch}...")
        run(["git", "fetch", "origin", branch])
        # Create local tracking branch
        run(["git", "branch", branch, f"origin/{branch}"])
    
    # 4. Verify branch not checked out in repo root
    if is_branch_checked_out_in_root(branch):
        print(f"Error: Branch '{branch}' is checked out in main repo, cannot create worktree", file=sys.stderr)
        sys.exit(1)
    
    # 5. Create worktree
    print(f"Creating worktree at {worktree_dir} for branch {branch}...")
    run(["git", "worktree", "add", worktree_dir, branch])
    
    # 6. Launch Claude with resume
    open_worktree(worktree_dir, branch, resume_session=True)
```

- [ ] **Step 2: Add `resume` subcommand to parser**

In `build_parser()`, add after the `p_new` configuration (after line 61):

```python
    p_resume = subparsers.add_parser("resume", help="Resume work on an existing branch")
    p_resume.add_argument("branch", help="Branch name to resume")
    p_resume.set_defaults(func=cmd_resume)
```

- [ ] **Step 3: Test `resume` with existing worktree**

Run: `./scripts/ai resume ai-helper-script-improvements-apr-19`

Expected: "Using existing worktree" message, launches Claude with --resume (exit immediately)

- [ ] **Step 4: Test `resume` with local branch**

Setup: `git branch test-local-resume main`

Run: `./scripts/ai resume test-local-resume`

Expected: Creates worktree, launches Claude with --resume (exit immediately)

Cleanup: `git worktree remove .worktrees/test-local-resume && git branch -D test-local-resume`

- [ ] **Step 5: Test `resume` with remote-only branch**

Setup: `git branch -D codex-apply-patch-telemetry` (delete local copy of existing remote branch)

Run: `./scripts/ai resume codex-apply-patch-telemetry`

Expected: Fetches branch, creates local tracking branch, creates worktree, launches Claude (exit immediately)

Cleanup: `git worktree remove .worktrees/codex-apply-patch-telemetry && git branch -D codex-apply-patch-telemetry`

- [ ] **Step 6: Test `resume` with non-existent branch**

Run: `./scripts/ai resume nonexistent-branch-xyz`

Expected: Error message "Branch 'nonexistent-branch-xyz' not found locally or on origin"

- [ ] **Step 7: Test `resume` with branch checked out in root**

Setup: `cd /home/ubuntu/projects/autter && git checkout main`

Run: `./scripts/ai resume main`

Expected: Error message "Branch 'main' is checked out in main repo, cannot create worktree"

- [ ] **Step 8: Commit `resume` command**

```bash
git add scripts/ai
git commit -m "feat: add resume command for existing branches

The resume command:
- Reuses existing worktrees
- Creates worktrees from local branches
- Fetches and creates tracking branches for remote-only branches
- Launches Claude with --resume flag

Co-Authored-By: Claude Sonnet 4.5 <noreply@anthropic.com>"
```

---

### Task 4: Implement `pr` Command

**Files:**
- Modify: `scripts/ai` (add `get_pr_info` and `cmd_pr` functions, add import for json, update argparse)

- [ ] **Step 1: Add json import**

At the top of the file, change line 3 to:

```python
import argparse
import json
import os
import subprocess
import sys
```

- [ ] **Step 2: Add `get_pr_info()` function**

Add after the branch checking helper functions (before `cmd_new`):

```python
def get_pr_info(pr_number):
    """Fetch PR metadata using GitHub CLI."""
    result = subprocess.run(
        ["gh", "pr", "view", str(pr_number), "--json", "headRefName,headRepository,headRepositoryOwner,baseRepository"],
        capture_output=True, text=True, check=True,
    )
    data = json.loads(result.stdout)
    
    # Determine if it's a fork by comparing repository owners
    is_fork = (
        data["headRepository"]["owner"]["login"] != 
        data["baseRepository"]["owner"]["login"]
    )
    
    return {
        "headRefName": data["headRefName"],
        "isFork": is_fork,
    }
```

- [ ] **Step 3: Add `cmd_pr()` function**

Add after `cmd_resume` (before `build_parser`):

```python
def cmd_pr(args):
    """Check out a PR into a worktree and launch Claude."""
    pr_number = args.pr_number
    repo_root = get_repo_root()
    
    # 1. Fetch PR metadata
    pr_info = get_pr_info(pr_number)
    branch_name = pr_info["headRefName"]
    is_fork = pr_info["isFork"]
    
    # 2. Determine worktree directory name
    if is_fork:
        worktree_name = f"pr-{pr_number}"
    else:
        worktree_name = branch_name
    
    worktree_dir = os.path.join(repo_root, ".worktrees", worktree_name)
    
    # 3. Check if worktree already exists
    if os.path.exists(worktree_dir):
        print(f"Using existing worktree for PR #{pr_number}")
        open_worktree(worktree_dir, branch_name, resume_session=False)
        return
    
    # 4. Checkout PR using gh CLI (handles fetching)
    print(f"Checking out PR #{pr_number}...")
    run(["gh", "pr", "checkout", str(pr_number)])
    
    # 5. Verify branch not checked out in repo root
    if is_branch_checked_out_in_root(branch_name):
        print(f"Error: Branch '{branch_name}' is checked out in main repo, cannot create worktree", file=sys.stderr)
        sys.exit(1)
    
    # 6. Create worktree from checked-out branch
    print(f"Creating worktree at {worktree_dir}...")
    run(["git", "worktree", "add", worktree_dir, branch_name])
    
    # 7. Launch Claude without resume
    open_worktree(worktree_dir, branch_name, resume_session=False)
```

- [ ] **Step 4: Add `pr` subcommand to parser**

In `build_parser()`, add after the `p_resume` configuration:

```python
    p_pr = subparsers.add_parser("pr", help="Check out a PR and open claude")
    p_pr.add_argument("pr_number", type=int, help="PR number to check out")
    p_pr.set_defaults(func=cmd_pr)
```

- [ ] **Step 5: Test `pr` command with same-repo PR**

Find a recent PR number: `gh pr list --state merged --limit 1 --json number --jq '.[0].number'`

Run: `./scripts/ai pr <pr-number>` (use the number from above)

Expected: Checks out PR, creates worktree with branch name, launches Claude (exit immediately)

Cleanup: Find branch name with `gh pr view <pr-number> --json headRefName --jq '.headRefName'`, then `git worktree remove .worktrees/<branch-name>`

- [ ] **Step 6: Test `pr` command with existing worktree**

Re-run the same PR: `./scripts/ai pr <pr-number>`

Expected: "Using existing worktree for PR #<number>" message, launches Claude (exit immediately)

Cleanup: `git worktree remove .worktrees/<branch-name>`

- [ ] **Step 7: Test `pr` command with invalid PR**

Run: `./scripts/ai pr 999999`

Expected: gh CLI error about PR not found

- [ ] **Step 8: Commit `pr` command**

```bash
git add scripts/ai
git commit -m "feat: add pr command for checking out pull requests

The pr command:
- Fetches PR metadata to determine branch name and fork status
- Uses branch name for same-repo PRs, pr-<number> for forks
- Reuses existing worktrees
- Launches Claude without --resume flag

Co-Authored-By: Claude Sonnet 4.5 <noreply@anthropic.com>"
```

---

## Self-Review Checklist

**Spec coverage:**
- ✓ `resume` command with worktree reuse (Task 3)
- ✓ `resume` with local branch (Task 3)
- ✓ `resume` with remote branch fetch (Task 3)
- ✓ `resume` with --resume flag (Task 3)
- ✓ `resume` error handling (Task 3)
- ✓ `pr` command with branch name detection (Task 4)
- ✓ `pr` with fork detection (Task 4)
- ✓ `pr` with worktree reuse (Task 4)
- ✓ `pr` without --resume flag (Task 4)
- ✓ Helper function extraction (Task 1)
- ✓ Refactor cmd_new (Task 1)

**Placeholders:** None - all code is complete

**Type consistency:** 
- `branch` and `branch_name` used consistently
- `worktree_dir` used consistently throughout
- `resume_session` boolean parameter consistent

**Testing:** Manual testing included for all major paths and error cases
