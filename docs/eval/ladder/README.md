# 難易度ラダー — 小型モデルが「どの粒度で壊れるか」を測る

## なぜ作ったか

先行の [mini-renovater ベンチ](../mini-renovater-bench-2026-07.md) は、ローカル 9B/12B が
**0/5** という結果で止まった。この数字には 3 つの問題がある。

1. **二値なので改善が見えない** — 「サブ要件 1 個の不足」も「ステージ丸ごとの崩壊」も同じ 0 になる
2. **指標が stdout の grep 依存** — 表示形式を変えると壊れ、ツール呼び出しの成否や所要時間は取れない
3. **緩和策の寄与を分離できない** — 温度・整形再要求・重複抑止・ナッジをまとめて入れたので、何が効いたか言えない

その結果、結論が「モデルが軽量すぎる」で止まってしまい、次に直す場所が分からない。
ラダーはこの 3 点を潰すために、**難易度を刻み・部分点を出し・機能フラグ別に走らせる**。

## 設計

| 軸 | 内容 |
|---|---|
| **レベル** | L0 (ツール 1-2 回) → L1 (数ツールの連鎖) → L2 (要件 4-7 個を 1 ファイルで満たす) |
| **判定** | 全てファイルシステムの状態と実際の実行で判定する。チェック項目単位で数え、`checks_passed/checks_total` を部分点として記録 |
| **反復** | 同一条件を `REPEAT` 回 (既定 3)。先行ベンチで確認された PASS/FAIL の揺れを分散として見る |
| **ablation** | `CONFIGS` で緩和策の組み合わせを切り替える。`base` は全て切った素の状態 |
| **計測** | lodan の `--log-jsonl` を各実行で取り、LLM 呼び出し数・ツール呼び出し数・緩和策の発火回数・トークンを機械的に集計 |

`CONFIGS` の各水準は先行ベンチの履歴に対応する。

| config | フラグ | 対応 |
|---|---|---|
| `base` | 緩和策すべて off | v1 (温度未指定・対策なし) |
| `temp` | `--temperature 0.2` | 温度固定だけ |
| `mitig` | `+ --malformed-retry --dup-suppress` | v2 (#61) |
| `nudge` | `+ --finish-nudge` | v4 (#63) |

## 使い方

```bash
cargo build --release

# 一式 (10 タスク × 4 config × 3 反復)
MODEL=llama3.1:8b PROVIDER=local bash docs/eval/ladder/ladder.sh

# 速い反復: 特定タスク・単一 config・1 回だけ
TASKS="L0 L1-fix-bug" CONFIGS=mitig REPEAT=1 MODEL=llama3.1:8b bash docs/eval/ladder/ladder.sh

# 参照実装 (API 級) と比較する
MODEL=fugu PROVIDER=sakana CONFIGS=mitig bash docs/eval/ladder/ladder.sh

python3 docs/eval/ladder/summarize.py docs/eval/ladder/results.jsonl
```

主な環境変数: `MODEL` / `PROVIDER` / `LABEL` / `REPEAT` / `CONFIGS` / `TASKS` /
`T_L0` `T_L1` `T_L2` (レベル別の実行上限秒) / `RESULTS` / `RUNS_DIR` / `LODAN`。

結果は `results.jsonl` へ 1 実行 1 行で追記され、同じ `key`
(`label/config/task/run`) の行があるものはスキップされる。長時間の実行が
中断しても、そのまま再実行すれば続きから進む。

## タスク一覧

| task | level | 測っているもの |
|---|---|---|
| `L0-write` | L0 | ツールコールが成立するか (最小) |
| `L0-read-report` | L0 | Read の結果を次の呼び出しへ運べるか |
| `L0-bash` | L0 | コマンド実行と出力の保存 |
| `L0-grep` | L0 | 検索結果を取り違えずに書き出せるか |
| `L1-fix-bug` | L1 | 既存コードへの最小介入・無関係箇所を壊さない |
| `L1-add-func` | L1 | 既存を維持したまま足す (累積の最小単位) |
| `L1-script-run` | L1 | Write → Bash の連鎖 |
| `L2-cli-json` | L2 | 要件 7 個を 1 ファイルで満たす (mini-renovater S1 の縮小版) |
| `L2-extend-cli` | L2 | 動いている既存 CLI へ 1 段積む (S2 以降の縮小版) |
| `L2-http` | L2 | 実際に起動して応答するものを作る (S5 の縮小版) |

## タスクの足し方

`tasks/<name>.sh` を置くだけで拾われる。契約は 4 つ。

```bash
LEVEL=L1                  # L0 / L1 / L2 (実行時間上限の選択に使う)
DESC="一行の説明"
setup() { ... }           # 作業ディレクトリ (cwd) に fixture を用意する
PROMPT="モデルへ渡す 1 行の指示"
checks() {                # check <名前> <シェルコマンド>; exit 0 で合格
  check exists 'test -f out.txt'
}
```

**チェックを書くときの注意**: 否定条件は「対象が存在しないだけ」で通ってしまう。
`! grep -q x out.txt` ではなく `test -f out.txt && ! grep -q x out.txt` と書く。
部分点が水増しされると break point がぼやける。

## 運用メモ

先行ベンチで得た教訓はそのまま当てはまる。

- ローカルモデルの cold start は lodan の `timeout_secs` を超えるため、ハーネスは
  作業 cwd の `.lodan/config.toml` で 900s に上げ、実行前に `keep_alive` でプリウォームする
- ollama 既定の `num_ctx` 4096 では system prompt + ツール定義が溢れる。
  Modelfile 派生 (`FROM x` + `PARAMETER num_ctx 8192`) で固定してから測る
- 長時間の実行は `nohup` + `caffeinate -is -w <pid>` で切り離す。中断しても
  `results.jsonl` から再開できる
