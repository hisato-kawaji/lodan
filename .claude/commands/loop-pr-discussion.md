---
description: Loop body — every 5 minutes, walk each open PR and progress any pr-reviewer discussion. As reviewer (your review was challenged) re-evaluate / reply / concede. As author (your PR has unresolved review points) implement small fixes and push, or push back with reasoning. Approve when all threads resolved and CI is green. Runs alongside /loop-pr-review.
allowed-tools: Bash, Read, Edit, Write, Grep, Glob, Task
argument-hint: (no arguments)
---

# Loop: PR Discussion

Intended for `/loop` (**recommended: `/loop 5m /loop-pr-discussion`**).

Counterpart to `loop-pr-review`:

| Loop | Cadence | Handles |
|---|---|---|
| `/loop 12h /loop-pr-review` | 半日 1 回 | **未レビューの head** に対する初回レビュー |
| `/loop 5m /loop-pr-discussion` | **5 分** | 既レビュー PR の **議論進行と最終判定** |

Both run in the same Claude session. They don't conflict — review-loop posts the initial verdict, discussion-loop walks the conversation forward.

## Roles in this repo

In `hisato-kawaji/lodan`, the same identity (hisato-kawaji via Claude) plays both **PR author** and **reviewer**. The discussion loop reasons across both roles per thread.

## Each tick

For every open, non-draft PR not labeled `skip-claude-review`:

### 1. Gather state

```bash
# Metadata
gh pr view <N> --json title,body,author,headRefOid,mergeable,labels,isDraft

# Issue comments (top-level discussion)
gh pr view <N> --comments

# Review comments (line-level threads)
gh api repos/hisato-kawaji/lodan/pulls/<N>/comments --paginate

# Reviews submitted
gh api repos/hisato-kawaji/lodan/pulls/<N>/reviews

# CI
gh pr checks <N>
```

### 2. Find the latest pr-reviewer verdict

- Walk PR comments newest → oldest
- First comment whose body starts with `<!-- pr-reviewer: <sha> -->` is the **active verdict**
- If none, this PR has no review yet → **skip** (loop-pr-review will handle)
- If the marker SHA != current `headRefOid`, the review is stale → **skip** (loop-pr-review will refresh)

### 3. Parse review threads from the verdict

The verdict table has 6 rows. For each row marked `WARN` or `FAIL`, treat the "根拠" cell as the thread's opening claim. Collect any comment / commit since that verdict timestamp that addresses it.

### 4. Decide action per unresolved thread

For each WARN/FAIL row (or PR-level reply if the author posted a general response):

| Situation | Action |
|---|---|
| New author comment refuting our point | **Re-evaluate**: invoke `pr-reviewer` subagent with a tight brief ("Re-evaluate this single point: <quote>. Author argument: <quote>. Verdict?"). If still valid → counter with citation. If invalid → concede in 1 comment |
| New author comment asking a clarifying question | Answer briefly, cite policy/code |
| New commits touch the cited path(s) AND CI is green | Verify the fix in the diff; reply `✅ Verified at <sha-short>` and mark the thread resolved |
| New commits touch cited path(s) BUT CI is red | Reply `Fix attempted at <sha-short> but CI failing on <job>. Please address.` (don't resolve) |
| No author response and no new commits since our last comment | **Skip** — wait |
| We previously replied and author hasn't responded | **Skip** — wait |

### 5. Decide author-side action (if WE need to fix something)

If a thread points to something we can fix ourselves (we own the PR):

| Fix size | Action |
|---|---|
| **Tiny and contained** (<50 lines, no design call, no public-API change) | Edit + commit + push to the PR branch with message `fix: <thread topic> (re: pr-review)`. Then reply `Applied in <sha-short>` |
| **Medium** (50–200 lines, or touches `Tool` trait / `Config` schema / `LlmClient` trait) | Reply with a **plan** + code snippet. Wait for confirmation in the next tick |
| **Large** (>200 lines, or architectural — e.g. agent loop / provider abstraction / permission model) | Reply `This needs architectural discussion — opening issue / requesting human review.` Don't auto-fix |

### 6. Overall PR-level decision

After processing all threads, check:

- All WARN/FAIL items have a resolving comment (verified, conceded, or waived)
- CI is `green` on the current head (`gh pr checks <N>` の `test` ジョブ pass)
- No thread's last word is a question or disagreement from any party
- The PR is `mergeable: MERGEABLE`

If all true AND we haven't already approved this head SHA:

```bash
gh pr review <N> --approve --body "All review threads resolved at <sha-short>. CI green. Approving."
```

After approval, post a one-line comment summarizing the head and any waivers.

### 7. Output one line per PR

```
PR#N head=<sha-short> threads=<n> resolved=<m> actions=<verb-list> ci=<status> verdict=<approved|in-progress|stuck>
```

## Stop conditions

- `.claude/scratch/STOP-PR-LOOP` exists → exit silently
- Loop has been running > 24 hours → exit (sanity)
- Hard auth error (gh / git) → exit
- 5 consecutive ticks with `actions=[]` across all PRs → stay alive but emit only a heartbeat line (no per-PR output)

## Hard safety rules

- **Never `gh pr merge`** — approval is not merging. Humans decide final merge.
- **Never `git push --force`** to any PR branch.
- **Never bypass branch protection** (`enforce_admins=true`, PR + CI required on `main`). If branch protection rejects, that's a hard stop — surface and exit.
- **At most 1 commit per PR per tick.** Avoids runaway feedback loops.
- **At most 5 self-fix commits per PR overall.** Beyond that, surface and stop touching this PR until a human acks.
- **Do not touch files outside the PR's diff** unless explicitly resolving a thread that names the file.
- **Do not modify `docs/policy/*`** to "win" a disagreement. Policy changes go through their own PR.
- **Do not skip CI hooks** (`--no-verify`, `--no-gpg-sign`) — if a hook fails, fix and retry as a new commit.
- **Do not delete or rewrite `.env` / `.fish_api` / API key state** during any auto-fix.

## When you should NOT take action this tick

- The PR has been updated since you started this tick (race) — re-fetch next tick
- A human reviewer left a comment that is not a question — defer to human, just acknowledge politely
- Multiple unresolved threads with conflicting requirements → emit `verdict=stuck` and stop touching the PR
- Auto-fix would require touching MVP-外 stub modules (`hooks/`, `mcp/`, `skills/`, `slash/`, `session.rs`) — that's a roadmap-sized change, not a discussion fix

## State / idempotency

The loop is fully idempotent based on timestamps:

- Our last comment timestamp `T_us`
- Their last comment / commit timestamp `T_them`
- Only act if `T_them > T_us` (or fix verification finds new SHA after our last verify)

This means running 2 ticks back-to-back is safe — second tick will see "no new info" and skip.

## Token cost

- Idle tick (nothing to do): only `gh` calls, ~zero LLM cost
- Action tick with a counter-argument: pr-reviewer subagent invocation (Opus, ~few thousand tokens)
- Action tick with a self-fix: 1 Claude turn writing code (Sonnet or Opus depending on complexity)
- Approval tick: 1 short Claude turn

With 5-min cadence and typically 1–2 open PRs on lodan, expect <5 action ticks per day on a normal workflow.

## Coordination with loop-pr-review

The 12h initial-review loop and the 5min discussion loop share the SHA-marker convention:

- A new commit on a PR → marker invalidated → **loop-pr-review** posts a fresh verdict next 12h tick
- An existing verdict + new author activity → **loop-pr-discussion** progresses the thread in the next 5min tick

Don't try to do the initial review from this loop. Don't try to handle discussion from the review loop. Keep responsibilities separate.
