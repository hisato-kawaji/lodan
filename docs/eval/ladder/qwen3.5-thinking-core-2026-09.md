# qwen3.5:9b: thinking off と `core` プロファイルの実測 (2026-09-21)

ローカルの thinking モデルで、#78（推論の深さ）と #72（ツールプロファイル）の寄与を難易度ラダーで測った。
10 タスク × 3 条件 × 1 回、計 30 実行。ハーネスと指標の定義は [README](README.md)、
API モデル側の実測は [kimi-k3 の `low` vs 既定](kimi-k3-effort-2026-09.md)。

- 環境: MacBook Air M3 / 24GB、Ollama、`qwen35-9b-8k`（`qwen3.5:9b` + `num_ctx 8192`）、lodan `main` @ `6947ab6`（release。kimi-k3 の実測と同じバイナリ）
- 条件（いずれも `--temperature 0.2`、緩和策は off）:
  - `temp` — 基準。`reasoning_effort` は送らない（thinking あり）、ツールは全部（定義 6,643 バイト）
  - `think-none` — `temp` + `--reasoning-effort none`（thinking off）
  - `core` — `temp` + `--tool-profile core`（ツール 6 個、定義 2,437 バイト。thinking あり）
- 実行時間上限: L0 / L1 / L2 = 150 / 300 / 480 秒
- 再現: `CONFIGS="temp think-none core" REPEAT=1 MODEL=qwen35-9b-8k:latest PROVIDER=local T_L0=150 T_L1=300 T_L2=480 bash docs/eval/ladder/ladder.sh`

## 結論

1. **thinking を切っても正答は落ちず、28% 速くなった。** `temp` と `think-none` はどちらも 9/10 合格・
   チェック 34/36。合計 1,784 → 1,292 秒、completion 8,173 → 5,082 トークン。落とした 1 件は別のタスク
   （`temp` は `L2-cli-json` が 5/7、`think-none` は `L2-http` が時間切れ）。
2. **thinking ありの実行の 65%（20 中 13）が、「思考だけで、本文もツール呼び出しも無い応答」でターンを終えていた。**
   `think-none` では 0/10。作業が済んだ後なら合格のまま終わる（9 件）が、**済む前に起きると、そこで打ち切られて
   未完成のまま終わる**（4 件。すべて `partial`）。lodan は「ツール呼び出しの無い応答 = 最終応答」として
   ターンを閉じるので、モデルが考えただけで何も出力しなかった回を、完了と取り違えている。#111 で起票した現象で、
   小型の thinking モデルでは例外ではなく常態だった。
3. **`core` は 6/10 に下がったが、プロファイルの寄与とは切り分けられない。** 落ちた 4 件のうち 3 件
   （`L1-add-func` / `L2-extend-cli` / `L2-http`）は、結論 2 の「1 回ツールを使った直後の空応答」で
   2 回目の呼び出しで終わっている（57 / 50 / 140 秒。時間は余っていた）。残る 1 件（`L2-cli-json`）は時間切れ。
   `core` は 1 呼び出しあたりの思考が長く（473 文字。`temp` は 216 文字）、空応答が「ツールを 1 回使った直後」に
   前倒しで起きている点は `temp` と違う。prompt トークンは 141,718 → 56,155（-60%）と狙いどおり減っている。**空応答の問題を塞いでから測り直さないと、
   プロファイルの寄与は分からない。**
4. 各条件 1 回の観測で、統計的な比較ではない。ただし結論 2 は 30 実行の全応答を数えた結果で、
   thinking の有無ではっきり分かれている（13/20 対 0/10）。

運用上の含意: いまの lodan で小型の thinking モデルを使うなら、**`reasoning_effort = "none"` の方が速く、
空応答による早期終了も起きない**（0/10 対 7/10。合格数そのものは同じ 9/10）。thinking を生かすには、
先に #111（思考だけの応答をターンの終わりと見なさず、答えか次の手を促す）が要る。既にある
`--finish-nudge`（#63。ツール呼び出しの無い応答で 1 回だけ続きを促す）がこの空応答に効くかは、今回は測っていない。

## タスク別

| task | `temp` | `think-none` | `core` |
|---|---|---|---|
| L0-bash | pass 2/2 · 68s · 2 calls | pass 2/2 · 39s · 2 calls | pass 2/2 · 93s · 3 calls |
| L0-grep | pass 4/4 · 79s · 3 calls | pass 4/4 · 93s · 3 calls | pass 4/4 · 95s · 3 calls |
| L0-read-report | pass 2/2 · 90s · 3 calls | pass 2/2 · 68s · 3 calls | pass 2/2 · 78s · 3 calls |
| L0-write | pass 2/2 · 55s · 2 calls | pass 2/2 · 34s · 2 calls | pass 2/2 · 57s · 2 calls |
| L1-add-func | pass 3/3 · 107s · 3 calls | pass 3/3 · 161s · 5 calls | **partial 1/3** · 57s · 2 calls ※ |
| L1-fix-bug | pass 3/3 · 84s · 3 calls | pass 3/3 · 76s · 3 calls | pass 3/3 · 88s · 3 calls |
| L1-script-run | pass 4/4 · 158s · 6 calls | pass 4/4 · 67s · 4 calls | pass 4/4 · 175s · 6 calls |
| L2-cli-json | **partial 5/7** · 219s · 5 calls ※ | pass 7/7 · 143s · 3 calls | **timeout 4/7** · 480s · 7 calls |
| L2-extend-cli | pass 5/5 · 444s · 7 calls | pass 5/5 · 131s · 4 calls | **partial 1/5** · 50s · 2 calls ※ |
| L2-http | pass 4/4 · 480s · 10 calls | **timeout 2/4** · 480s · 12 calls | **partial 2/4** · 140s · 2 calls ※ |

※ = 思考だけの空応答でターンが終わり、未完成のまま打ち切られた実行。

## 集計

| | `temp` | `think-none` | `core` |
|---|---|---|---|
| 合格 | 9/10 | 9/10 | 6/10 |
| チェック通過 | 34/36 | 34/36 | 25/36 |
| 時間切れ | 0 | 1 | 1 |
| 合計 秒（壁時計） | 1,784 | 1,292 | 1,313 |
| LLM 呼び出し | 44 | 41 | 33 |
| prompt トークン | 141,718 | 131,576 | 56,155 |
| completion トークン | 8,173 | 5,082 | 7,214 |
| 思考 文字 | 9,514 | 0 | 15,611 |
| 空応答で終わった実行 | 7/10 | 0/10 | 6/10 |
| うち未完成で打ち切り | 1 | 0 | 3 |

- 「空応答」= `--log-jsonl` の `llm_response` で `text_chars = 0` かつ `tool_calls` が空の応答。今回はすべて
  その実行の最後の応答だった（= それでターンが閉じた）
- 思考の文字数は `llm_response.reasoning_chars`（#110）。`think-none` の 0 は、Ollama が
  `reasoning_effort = "none"` で thinking を切っていることの確認にもなっている
- 「時間切れ」は結果のラベルが `timeout` の件数。`temp` の `L2-http` も 480 秒の上限で打ち切られているが、
  その時点で全チェックを通っていたので `pass` と記録されている（= `temp` の合計 1,784 秒は下限）。上限での打ち切りは
  `temp` と `think-none` に 1 件ずつなので、-28% の向きは変わらない
- `core` の合計秒が短いのは、未完成のまま早く終わった実行が 3 件あるため。速くなったわけではない
