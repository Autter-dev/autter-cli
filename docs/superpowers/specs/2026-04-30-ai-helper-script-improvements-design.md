# AI Helper Script Improvements

**Date:** 2026-04-30

## Overview

Extend the `./scripts/ai` helper script with two new commands: `resume` and `pr`. These commands streamline the workflow of returning to existing work or reviewing pull requests by automating worktree management and Claude session launching.

## Requirements

### 1. `resume` Command

Resume work on an existing branch by automatically finding or creating a worktree and launching Claude in resume mode.

**Usage:** `./ai resume <branch-name>`

**Behavior:**
- If worktree already exists at `.worktrees/<branch-name>`: reuse it
- If branch exists locally: create worktree from it
- If branch exists on remote (`origin/<branch-name>`): fetch and create local tracking branch, then create worktree
- If branch doesn't exist locally or remotely: error out
- Before creating worktree: verify branch isn't checked out in repo root (git constraint)
- Launch Claude with `--resume` flag to enter resume flow

### 2. `pr` Command

Check out a pull request into a worktree and launch Claude for review.

**Usage:** `./ai pr <pr-number>`

**Behavior:**
- Use GitHub CLI to fetch PR metadata (branch name, repository owner)
- Determine worktree directory name:
  - Same repository: `.worktrees/<branch-name>`
  - Fork: `.worktrees/pr-<number>`
- If worktree already exists: reuse it with message "Using existing worktree for PR #<number>"
- Use `gh pr checkout <pr-number>` to fetch and check out the PR branch
- Before creating worktree: verify branch isn't checked out in repo root
- Launch Claude without `--resume` flag

## Architecture

### Common Helper Function

Extract worktree launching logic into a shared function:

```python
def open_worktree(worktree_dir, branch_name, resume_session=False):
    """Open Claude in the specified worktree."""
    set_tmux_window_name(branch_name)
    os.chdir(worktree_dir)
    claude_args = ["claude", "--dangerously-skip-permissions"]
    if resume_session:
        claude_args.append("--resume")
    os.execvp("claude", claude_args)
```

This function:
- Sets tmux window name to the branch name
- Changes to the worktree directory
- Executes Claude with appropriate flags

### Command Structure

All three commands (`new`, `resume`, `pr`) follow a similar pattern:

1. **Resolve target branch** - Command-specific logic to determine which branch to work with
2. **Ensure worktree exists** - Check for existing worktree or create new one
3. **Launch Claude** - Call `open_worktree()` helper

### Error Handling

All git and gh operations use `subprocess.run()` with `check=True` for automatic error propagation. Custom error messages for:
- Branch not found (locally or remotely)
- Branch checked out in repo root
- PR not found
- Worktree creation failures

## Implementation Details

### `resume` Command

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

### `pr` Command

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
    run(["gh", "pr", "checkout", pr_number])
    
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

### Helper Functions

**`get_repo_root()`** - Get repository root directory
```python
def get_repo_root():
    return subprocess.run(
        ["git", "rev-parse", "--show-toplevel"],
        capture_output=True, text=True, check=True,
    ).stdout.strip()
```

**`check_local_branch_exists(branch)`** - Check if local branch exists
```python
def check_local_branch_exists(branch):
    result = subprocess.run(
        ["git", "show-ref", "--verify", f"refs/heads/{branch}"],
        capture_output=True,
    )
    return result.returncode == 0
```

**`check_remote_branch_exists(branch)`** - Check if remote branch exists
```python
def check_remote_branch_exists(branch):
    result = subprocess.run(
        ["git", "show-ref", "--verify", f"refs/remotes/origin/{branch}"],
        capture_output=True,
    )
    return result.returncode == 0
```

**`is_branch_checked_out_in_root(branch)`** - Check if branch is checked out in repo root
```python
def is_branch_checked_out_in_root(branch):
    result = subprocess.run(
        ["git", "-C", get_repo_root(), "symbolic-ref", "--short", "HEAD"],
        capture_output=True, text=True,
    )
    if result.returncode != 0:
        return False
    current_branch = result.stdout.strip()
    return current_branch == branch
```

**`get_pr_info(pr_number)`** - Fetch PR metadata using GitHub CLI
```python
def get_pr_info(pr_number):
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

## Refactoring `cmd_new`

The existing `cmd_new` function should be refactored to use the new `open_worktree()` helper:

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

## Testing Considerations

Manual testing should cover:

1. **`resume` with existing worktree** - Should reuse it
2. **`resume` with local branch** - Should create worktree
3. **`resume` with remote branch only** - Should fetch and create worktree
4. **`resume` with non-existent branch** - Should error
5. **`resume` with branch in repo root** - Should error
6. **`pr` with same-repo PR** - Should use branch name for worktree
7. **`pr` with fork PR** - Should use `pr-<number>` for worktree
8. **`pr` with existing worktree** - Should reuse it
9. **`pr` with invalid PR number** - Should error
10. **Tmux integration** - Window names should be set correctly
11. **Claude flags** - `resume` should pass `--resume`, `pr` should not

## Non-Goals

- **List command** - Not needed; users can run `git worktree list` directly
- **Clean command** - Not needed for now; manual cleanup is acceptable
- **Update existing worktrees** - Reusing existing worktrees as-is; no auto-fetch/reset
