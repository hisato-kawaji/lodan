#!/usr/bin/env python3
"""ladder.sh の results.jsonl を Markdown の表にする。

出力は 3 つ:
  1. レベル別サマリ  — どの粒度で壊れ始めるか (break point)
  2. ablation 表     — 機能フラグの組み合わせごとの寄与
  3. タスク別内訳    — 落ちたチェック名まで含む詳細

使い方:
  python3 summarize.py results.jsonl
  python3 summarize.py results.jsonl --label llama3.1:8b
"""

from __future__ import annotations

import argparse
import json
from collections import defaultdict
from pathlib import Path

LEVELS = ["L0", "L1", "L2"]
CONFIG_ORDER = ["base", "temp", "mitig", "nudge"]


def load(path: Path, label: str | None) -> list[dict]:
    rows = []
    for line in path.read_text(encoding="utf-8").splitlines():
        line = line.strip()
        if not line:
            continue
        try:
            row = json.loads(line)
        except json.JSONDecodeError:
            continue
        if label and row.get("label") != label:
            continue
        rows.append(row)
    return rows


def pct(numerator: float, denominator: float) -> str:
    if not denominator:
        return "-"
    return f"{100 * numerator / denominator:.0f}%"


def config_key(name: str) -> tuple[int, str]:
    return (CONFIG_ORDER.index(name) if name in CONFIG_ORDER else len(CONFIG_ORDER), name)


def table(header: list[str], rows: list[list[str]]) -> str:
    out = ["| " + " | ".join(header) + " |", "|" + "|".join(["---"] * len(header)) + "|"]
    for r in rows:
        out.append("| " + " | ".join(r) + " |")
    return "\n".join(out)


def summarize_levels(rows: list[dict]) -> str:
    """レベル × config の到達率。break point を一目で見るための主表。"""
    buckets: dict[tuple[str, str], list[dict]] = defaultdict(list)
    for r in rows:
        buckets[(r["level"], r["config"])].append(r)

    configs = sorted({r["config"] for r in rows}, key=config_key)
    levels = [lv for lv in LEVELS if any(r["level"] == lv for r in rows)]

    header = ["level"] + [f"{c} (pass / checks)" for c in configs]
    body = []
    for level in levels:
        cells = [level]
        for cfg in configs:
            runs = buckets.get((level, cfg), [])
            if not runs:
                cells.append("-")
                continue
            passed = sum(1 for r in runs if r["status"] == "pass")
            checks_ok = sum(r["checks_passed"] for r in runs)
            checks_all = sum(r["checks_total"] for r in runs)
            cells.append(f"{pct(passed, len(runs))} / {pct(checks_ok, checks_all)}")
        body.append(cells)
    return table(header, body)


def summarize_ablation(rows: list[dict]) -> str:
    """config ごとの全体像と、緩和策が実際に何回発火したか。"""
    buckets: dict[str, list[dict]] = defaultdict(list)
    for r in rows:
        buckets[r["config"]].append(r)

    header = [
        "config",
        "runs",
        "pass",
        "checks",
        "timeout",
        "plan-only",
        "整形再要求",
        "重複抑止",
        "ナッジ",
        "中央値 秒",
    ]
    body = []
    for cfg in sorted(buckets, key=config_key):
        runs = buckets[cfg]
        m = [r.get("metrics") or {} for r in runs]
        secs = sorted(r["secs"] for r in runs)
        body.append(
            [
                cfg,
                str(len(runs)),
                pct(sum(1 for r in runs if r["status"] == "pass"), len(runs)),
                pct(sum(r["checks_passed"] for r in runs), sum(r["checks_total"] for r in runs)),
                str(sum(1 for r in runs if r["status"] == "timeout")),
                str(sum(x.get("plan_only_turns", 0) for x in m)),
                str(sum(x.get("malformed_retries", 0) for x in m)),
                str(sum(x.get("dup_suppressed", 0) for x in m)),
                str(sum(x.get("finish_nudges", 0) for x in m)),
                str(secs[len(secs) // 2]),
            ]
        )
    return table(header, body)


def summarize_tasks(rows: list[dict]) -> str:
    """タスク別。落ちたチェック名まで出すので、次に直すべき要件が分かる。"""
    buckets: dict[tuple[str, str], list[dict]] = defaultdict(list)
    for r in rows:
        buckets[(r["task"], r["config"])].append(r)

    header = ["task", "config", "pass", "checks", "落ちたチェック", "秒 (中央値)"]
    body = []
    for (task, cfg) in sorted(buckets, key=lambda k: (k[0], config_key(k[1]))):
        runs = buckets[(task, cfg)]
        failed: dict[str, int] = defaultdict(int)
        for r in runs:
            for name in (r.get("failed_checks") or "").split():
                failed[name] += 1
        secs = sorted(r["secs"] for r in runs)
        worst = ", ".join(
            f"{n}×{c}" for n, c in sorted(failed.items(), key=lambda kv: -kv[1])[:4]
        )
        body.append(
            [
                task,
                cfg,
                f"{sum(1 for r in runs if r['status'] == 'pass')}/{len(runs)}",
                pct(sum(r["checks_passed"] for r in runs), sum(r["checks_total"] for r in runs)),
                worst or "-",
                str(secs[len(secs) // 2]),
            ]
        )
    return table(header, body)


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("results", type=Path, nargs="?", default=Path("results.jsonl"))
    ap.add_argument("--label", help="この label の行だけ集計する")
    args = ap.parse_args()

    if not args.results.exists():
        print(f"no results file: {args.results}")
        return 1
    rows = load(args.results, args.label)
    if not rows:
        print("no rows matched")
        return 1

    labels = sorted({r["label"] for r in rows})
    print(f"# ladder results — {', '.join(labels)} ({len(rows)} runs)\n")
    print("## レベル別到達率 (run 合格率 / チェック通過率)\n")
    print(summarize_levels(rows))
    print("\n## ablation\n")
    print(summarize_ablation(rows))
    print("\n## タスク別\n")
    print(summarize_tasks(rows))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
