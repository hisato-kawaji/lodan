# 難易度ラダー ベースライン (2026-07-27)

ローカル 3 モデル × 4 config × 10 タスク、計 90 実行（各条件 1 回）。
ハーネスと指標の定義は [README](README.md)、先行データは
[mini-renovater ベンチ](../mini-renovater-bench-2026-07.md)。

- 環境: MacBook Air M3 / 24GB、ollama 0.32.1、lodan `feat/eval-instrumentation`
- モデルは全て `num_ctx 8192` の派生（`FROM <base>` + `PARAMETER num_ctx 8192`）
- 実行時間上限: llama/qwen は L0/L1/L2 = 150/300/480s、gemma は 400/700/900s
  （較正の理由は後述）

## 結論

1. **粒度を落とすと、先行ベンチで 0/5 だったモデルが 8/10 通る。**
   qwen3.5:9b と gemma4:12b は現行 lodan のまま L0-L2 を 8/10 完遂した。
   「9B/12B は多段累積のプロダクト組み上げに届かない」という先行結論は、
   **その粒度（800 行 × 5 ステージ）でのみ成立する**。
2. **温度固定が単独で最大の効果**。qwen で `base`(温度未指定) → `temp`(0.2) が
   70%→90%（チェック通過率 61%→94%）。
3. **#61 の 2 策（整形再要求・重複抑止）は全 90 実行で一度も発火しなかった**。
   この粒度では整形破綻自体が起きていない。`temp` と `mitig` は挙動として
   同一であり、両者のスコア差はノイズ。
4. **finish_nudge (#63) は遅いモデルに有害**。llama3.1:8b で timeout が
   0-1 件 → **6 件**に増えた。1 ターンあたりの LLM 往復が増え、時間予算を
   食い潰す。qwen（速い）では無害（timeout 0）。
5. **反復上限には一度も達していない**（`hit_max_iterations = 0`）。
   律速は反復回数ではなく時間。

## モデル別の到達（`mitig`、各 1 回）

| level | llama3.1:8b | qwen3.5:9b | gemma4:12b |
|---|---|---|---|
| L0 (4) | 3 pass | **4 pass** | **4 pass** |
| L1 (3) | 1 pass | **3 pass** | 2 pass / 1 partial |
| L2 (3) | 0 pass | 1 pass / 1 partial / 1 timeout | **2 pass** / 1 timeout |
| 計 | 4/10 | **8/10** | **8/10** |

## ablation（モデル固定・各 1 回なので傾向のみ）

qwen3.5:9b

| config | pass | チェック通過 | timeout | 中央値 秒 |
|---|---|---|---|---|
| base | 70% | 61% | 2 | 171 |
| **temp** | **90%** | **94%** | 0 | 122 |
| mitig | 80% | 92% | 1 | 96 |
| nudge | 80% | 83% | 0 | 111 |

llama3.1:8b

| config | pass | チェック通過 | timeout | 中央値 秒 |
|---|---|---|---|---|
| base | 18% | 20% | 0 | 64 |
| temp | 27% | 38% | 1 | 85 |
| mitig | 40% | 36% | 0 | 70 |
| **nudge** | 27% | 35% | **6** | 189 |

`temp` と `mitig` の差はノイズ（緩和策の発火が 0 のため挙動が同一）。
意味のある差は **base → temp** と、llama における **nudge の timeout 増**。

## 残る失敗の内訳

失敗 10 件を性質で分けると、「実装能力が足りない」は少数派だった。

| 失敗の型 | 件数 | 例 |
|---|---|---|
| plan-only 停止（ツール 0 回で終了） | 11 ターン | llama L2-cli-json / L2-http、gemma L1-add-func |
| スループット（作業中に時間切れ） | 4 | gemma L2-extend-cli（900s でも届かず）、qwen L2-cli-json（5/7 通過で切れ） |
| サブ要件の取りこぼし | 3 | qwen L2-cli-json の `files`/`types`、qwen L2-http の `/sum` |
| 既存機能の破壊 | 2 | llama L1-add-func（`shout()` を壊す）、llama L2-extend-cli（既存 `scan` を壊す） |

## 固定オーバーヘッド

初回応答の `prompt_tokens`（= system prompt + 14 ツール定義 + 1 行の依頼）。

| モデル | 中央値 |
|---|---|
| llama3.1:8b | 1,820 |
| gemma4:12b | 1,965 |
| qwen3.5:9b | 2,348 |

依頼本文が 20 トークン程度の L0 でもこれが毎回かかり、エージェントループの
反復ごとに再送される。gemma が L0 の 1 タスクに 200-400 秒を要したのは
主にこの prefill であり、**ツールプロファイル（14 → フェーズ別 4-6）は
遅いモデルでは実行可能性そのものに効く**。

## 計測上の落とし穴（このベースラインで実際に踏んだもの）

1. **実行時間上限をモデル共通にしてはいけない**。gemma を 150s で回して L0 が
   3 連続 timeout したが、うち 2 件は `llm_calls = 0`（最初の応答すら返る前に
   切られた）。上限を 400s に上げると同じタスクが全て pass した。
   `status = timeout` かつ `llm_calls = 0` は能力の判定材料にならない。
2. **壁時計は信用できない**。実行中にマシンがスリープし、実作業 372s の実行が
   35,793s と記録された。`metrics.active_ms`（lodan の `Instant` 由来、
   サスペンドを含まない）を使う。
3. **ハーネスを kill すると同じ key の行が二重に出る**（実測 3 件）。
   生き残った子シェルと再開後の実行が両方書くため。集計は後勝ちで畳む。
4. **複数モデルを混ぜたまま config を比較しない**。config ごとに実行した
   モデルの構成比が違うと、config の差ではなくモデルの差を見ることになる。

## 次に効きそうな順（このデータから）

1. **plan-only 停止の根治** — 最多の失敗型。finish_nudge は遅いモデルで
   timeout を増やすので、LLM 往復を増やさない形（ハーネス側の分解・
   要件台帳）で解く必要がある
2. **ツールプロファイル（E1）** — prefill が gemma の実行可能性を握っている。
   実装コストに対して効果が大きい
3. **サブ要件の取りこぼし（要件台帳 B1）** — qwen の L2 失敗は残り 1-2 要件
4. **既存機能の破壊（編集直後の検証 D2）** — llama で 2 件

## 再現

```bash
cargo build --release
# モデルは num_ctx 8192 の派生を作ってから
printf 'FROM qwen3.5:9b\nPARAMETER num_ctx 8192\n' > Modelfile && ollama create qwen35-9b-8k -f Modelfile

MODEL=qwen35-9b-8k LABEL=qwen3.5-9b PROVIDER=local \
  CONFIGS="base temp mitig nudge" REPEAT=1 T_L0=150 T_L1=300 T_L2=480 \
  bash docs/eval/ladder/ladder.sh

python3 docs/eval/ladder/summarize.py docs/eval/ladder/results.jsonl --label qwen3.5-9b
```

## 限界

- **各条件 1 回**。先行ベンチで確認された PASS/FAIL の揺れを考えると、
  config 間の数 % 差は読めない。反復 3 回のパスは未実施
- **API 級の参照点がない**。L2 が全滅した場合に「タスクが厳しすぎる」のか
  「モデルの限界」なのかを切り分ける較正点を持っていない（今回は 8/10 通ったので
  実害はなかったが、より上の L3 を足すときには必要）
- gemma は quick pass のみ（1 実行が長く ablation は時間対効果が悪い）
