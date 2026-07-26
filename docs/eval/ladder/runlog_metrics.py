#!/usr/bin/env python3
"""lodan の実行トレース (--log-jsonl) から 1 実行分の指標を JSON で出す。

ladder.sh が各実行の結果行へ埋め込む。stdout の表示形式に依存せず、
「何回 LLM を呼び、何回ツールを叩き、緩和策が何回発火したか」を数える。
"""

from __future__ import annotations

import json
import sys
from collections import Counter
from pathlib import Path


def collect(path: Path) -> dict:
    events = []
    for line in path.read_text(encoding="utf-8", errors="replace").splitlines():
        line = line.strip()
        if not line:
            continue
        try:
            events.append(json.loads(line))
        except json.JSONDecodeError:
            # 実行が途中で kill されると最終行が欠けることがある。捨てて続行する。
            continue

    by_event = Counter(e.get("event") for e in events)
    tool_rows = [e for e in events if e.get("event") == "tool_result"]
    reasons = Counter(t.get("reason") for t in tool_rows)
    llm_rows = [e for e in events if e.get("event") == "llm_response"]
    turn_ends = [e for e in events if e.get("event") == "turn_end"]

    # ツールを 1 度も使わずに終えたターン = 「計画だけ述べて実行しない」失敗。
    # 最終応答は必ずツールなしなので、応答単位ではなくターン単位で数える。
    plan_only_turns = sum(1 for e in turn_ends if e.get("tool_calls", 0) == 0)

    return {
        "llm_calls": len(llm_rows),
        "plan_only_turns": plan_only_turns,
        "tool_calls": len(tool_rows),
        "tool_errors": sum(1 for t in tool_rows if t.get("outcome") == "error"),
        "distinct_tools": len(sorted({t.get("name") for t in tool_rows if t.get("name")})),
        "tools_used": sorted({t.get("name") for t in tool_rows if t.get("name")}),
        "malformed_retries": by_event.get("malformed_retry", 0),
        "finish_nudges": by_event.get("finish_nudge", 0),
        "dup_suppressed": reasons.get("dup_readonly", 0),
        "denied": reasons.get("denied", 0),
        "compactions": by_event.get("compact", 0),
        "iterations": sum(e.get("iterations", 0) for e in turn_ends),
        "hit_max_iterations": sum(1 for e in turn_ends if e.get("reason") == "max_iterations"),
        "prompt_tokens": sum(e.get("prompt_tokens", 0) for e in llm_rows),
        "completion_tokens": sum(e.get("completion_tokens", 0) for e in llm_rows),
        # 生成の重さの目安。巨大な単一引数は小型モデルが壊れる主因なので見ておく。
        "max_tool_args_bytes": max((t.get("args_bytes", 0) for t in tool_rows), default=0),
    }


def main() -> int:
    if len(sys.argv) != 2:
        print("usage: runlog_metrics.py <run.jsonl>", file=sys.stderr)
        return 2
    path = Path(sys.argv[1])
    if not path.exists():
        print("{}")
        return 0
    print(json.dumps(collect(path), separators=(",", ":")))
    return 0


if __name__ == "__main__":
    sys.exit(main())
