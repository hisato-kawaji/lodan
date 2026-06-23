---
description: Loop body for /loop — poll open PRs on hisato-kawaji/lodan, identify ones not yet reviewed at their current head SHA, and run the pr-reviewer subagent on each. Use via /loop <interval> /loop-pr-review during sessions where you want hands-off PR review.
allowed-tools: Bash, Read, Grep, Glob, Task
argument-hint: (no arguments)
---

# Loop: PR Review

Intended for `/loop` (**recommended: `/loop 12h /loop-pr-review`** — every 12 hours / half-day).

Polls open non-draft PRs on `hisato-kawaji/lodan`, reviews any whose current head SHA has not yet been reviewed by `pr-reviewer`, and posts the result as a PR comment.

## Each tick

1. **List candidates**:
   ```bash
   gh pr list --repo hisato-kawaji/lodan --state open \
     --json number,headRefOid,isDraft,labels,updatedAt,title \
     --jq '.[] | select(.isDraft == false and ((.labels | map(.name)) | contains(["skip-claude-review"]) | not))'
   ```
2. For each PR, **check whether the current head SHA has already been reviewed**:
   - `gh pr view <N> --json comments --jq '.comments[].body'`
   - Look for the marker `<!-- pr-reviewer: <full-sha> -->` in any comment
   - If a marker equals the PR's `headRefOid` → **skip** this PR this tick
3. For PRs needing review, invoke the `pr-reviewer` subagent (one call per PR).
   - Brief includes: PR number, current head SHA (the subagent must embed this SHA in its comment marker), source-of-truth doc path `docs/policy/pr-review.md`.
4. After all PRs handled, output one line:
   ```
   tick <iso>: reviewed=N skipped=M total_open=T
   ```

## Stop conditions

- `.claude/scratch/STOP-PR-LOOP` file exists → exit silently.
- User explicitly says "stop the loop" → exit.
- 2 consecutive ticks with `reviewed=0 skipped=N total_open=N` AND no `updatedAt` change on any PR since last tick → keep looping but stay quiet (just heartbeat). Don't chatter when idle.
- Loop has been running > 8 hours → exit (sanity stop).
- Hard error during `gh pr list` (auth lost) → exit with a one-line note.

## Safety

- **Comment-only.** Never approve / request-changes via `gh pr review`. Never push, merge, or close PRs.
- The `pr-reviewer` subagent is Opus by default — each review is non-trivial in cost. Don't tighten the interval below 300s without reason.
- If a PR has > 1000 changed lines, the subagent will emit a partial verdict — that's expected, not an error.

## Output discipline

- When `reviewed > 0`: print which PRs were reviewed, one line each, with the comment URL.
- When `reviewed == 0` and `skipped == total_open` (everything is up to date): print one heartbeat line, then quiet.
- When something errors (gh auth lost, subagent crashed, etc): surface immediately, do not retry blindly.

## Token cost guidance

lodan は個人開発で PR の流量が低い。初回レビューは緊急性が低いので default cadence は **半日 1 回** (= 「twice-a-day sweep」):

| Interval | Use case |
|---|---|
| `/loop 12h` (43200s) | **Recommended default.** Twice-daily sweep, near-zero idle cost |
| `/loop 6h` (21600s) | Four-times-daily — still cheap, slightly snappier |
| `/loop 1h` (3600s) | Active development day, you want feedback within an hour |
| `/loop 10m` or shorter | Rapid iteration on a specific PR — bump to manual `/pr-review` instead |

The SHA marker (`<!-- pr-reviewer: <sha> -->`) prevents redundant re-reviews of the same head, so the loop cost is dominated by **how many NEW commits arrive between ticks**, not by the tick frequency itself.

## Manual override

Inside a session, you can still run `/pr-review <PR#>` to re-review a single PR **right now** without waiting for the next tick. This is the right tool when:
- you just pushed and want immediate feedback
- you updated `docs/policy/pr-review.md` and want to re-evaluate an existing PR
- the loop tick is hours away
