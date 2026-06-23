---
description: Review a pull request using the pr-reviewer subagent (Opus) and post the verdict as a PR comment. On-demand counterpart to /loop-pr-review.
allowed-tools: Bash, Read, Grep, Glob, Task
argument-hint: <pr-number>
---

# PR Review

Review PR **#$1** on `hisato-kawaji/lodan`.

## Steps

1. Validate `$1` is a positive integer. If not, exit with a clear error.
2. Confirm the PR exists and is open: `gh pr view $1 --json state`. If `MERGED` / `CLOSED`, ask the caller whether to proceed anyway.
3. Confirm the PR is not labeled `skip-claude-review`. If labeled, exit silently with a one-line log.
4. Invoke the `pr-reviewer` subagent. Brief:
   - PR number: `$1`
   - Source of truth: `docs/policy/pr-review.md` §1 (read at start)
   - **Force re-review** (this is manual / on-demand — ignore any existing SHA marker)
   - Output: post a comment via `gh pr comment $1 --body-file <tmp>`
5. After the subagent returns, print the posted comment URL.

## When to use this vs `/loop-pr-review`

- **`/loop-pr-review`**: hands-off, polls every N hours within the session, skips PRs already reviewed at the current head.
- **`/pr-review <PR#>`**: targeted, single PR, **forces a re-review** even if the head was already reviewed (useful when the policy doc changed and you want a fresh verdict, or when comments were lost).

## Rules

- Read-only on the working tree.
- If the PR has > 1000 changed lines, surface a note that the review focuses on the highest-risk paths.

## Cost note

`pr-reviewer` defaults to Opus. Each invocation is non-trivial in token cost. For routine PR review, start `/loop 12h /loop-pr-review` once per session and let the SHA marker dedupe.
