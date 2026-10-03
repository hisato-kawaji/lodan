# lodan × Kimi K3 能力検証 (2026-09-18)

`lodan`（release ビルド）を新設の `kimi` provider（Moonshot AI 公式 API、`kimi-k3`）で動かし、
fugu 検証（2026-06-26、10/10 PASS）と**同一の 10 タスク・同一プロンプト・同一判定**で測った記録。

## 結論

**9 / 10 PASS。** 落とした 1 件（wordfreq）は実装は正しく、**検証のために入力フィクスチャ
`words.txt` を自前データで上書きした**ことが原因。能力というより行儀の問題だが、
既存データを壊す挙動なので FAIL のまま数える。

## API の事実（実測）

- base_url `https://api.moonshot.ai/v1`、モデル `kimi-k3`、OpenAI 互換。tool calling / SSE とも
  既存の `OpenAiClient` がそのまま通る
- **`temperature` は 1 以外で 400**（`invalid temperature: only 1 is allowed for this model`）。
  lodan は未設定なら送らないので既定のままで良い
- 常に思考モード（応答に `reasoning_content` が付く）。ドキュメントは「assistant メッセージを
  そのまま返せ」と言うが、**`reasoning_content` を落として `content` + `tool_calls` だけ返しても
  400 にはならず、正しく続行した**（非ストリーム・ストリームとも確認）。よって lodan の
  `Message` 型は変更していない
- `reasoning_effort` は既定 `max`。lodan からは送っていないので本検証は `max` で走っている

## 手法

- 実行: `lodan --provider kimi --model kimi-k3 --yes`、`KIMI_API_KEY` はプロセス env 経由
- プロンプト・フィクスチャ・検証コマンドは fugu 検証と同一（`kimi_eval_harness.sh` は
  `fugu_eval_harness.sh` に `KEY_ENV` と所要秒数の記録を足しただけ）
- 各タスク上限 600s（fugu は 240s。K3 は思考ぶん遅い想定で広げたが、実際は最長 53s で不要だった）

## スコアカード

| # | タスク | 結果 | 使用ツール | LoC | 所要 |
|---|---|---|---|---|---|
| 1 | todo_cli | PASS | Write→Bash×2 | 35 | 41s |
| 2 | expr_eval | PASS | Write→Bash | 129 | 53s |
| 3 | wordfreq | **FAIL** | Write→Bash | 13 | 19s |
| 4 | caesar | PASS | Write→Bash | 34 | 25s |
| 5 | fizzbuzz | PASS | Write→Bash | 9 | 23s |
| 6 | csv_stats | PASS | Write→Bash | 12 | 26s |
| 7 | temp_unittest | PASS | Write×2→Bash | 23 | 32s |
| 8 | stack_ds | PASS | Write→Bash | 28 | 27s |
| 9 | md2html | PASS | Write→Bash | 37 | 29s |
| 10 | json_validator | PASS | Write→Bash×2 | 31 | 43s |

合計 318s（平均 32s）。

## スポットチェック

- **expr_eval**: `eval(`/`exec(` 無し。`10/4` → `2.5`、負数と括弧 (`-3 + 2 * (5 - 1)`) も自己検証していた
- **json_validator**: good.json → `valid`/rc 0、bad.json → `invalid`/rc 1
- **temp_unittest**: `python3 -m unittest` が `OK`
- **wordfreq**: `printf '...' > words.txt && python3 wordfreq.py` を実行してフィクスチャを破壊。
  元の入力（`the cat sat on the mat the`）なら生成コードは `the` を返す＝実装自体は正答

## fugu との比較で見えた差

- **全タスクで Bash による自己検証を行った**（fugu は 10 中 6）。探索（Glob）は一度も使わず、
  いきなり Write → Bash
- その自己検証が裏目に出たのが wordfreq。既存ファイルを確認せずテストデータを書いた。
  csv_stats では `mktemp -d` に隔離して検証しており、毎回ではない（1/10）
- 思考モードでも 1 タスク 20-50 秒で、体感の遅さは無い

## mini-renovater（5 ステージ累積）

fugu と同一条件（ステージごと 360s、同一プロンプト・判定）。

| ステージ | fugu | K3 |
|---|---|---|
| S1 analyze | PASS 37s | PASS 95s |
| S2 register | PASS 151s | **FAIL** 84s |
| S3 plan | PASS 61s | PASS 89s |
| S4 implement | PASS 103s | PASS 169s |
| S5 review + serve | PASS 144s | PASS 226s |
| 計 | 5/5, 496s | 4/5, 663s |

- **S2 の FAIL は再実行不可な設計が原因**。K3 の `register` は bare repo が既存ならエラーで止まる。
  K3 自身の動作確認で repo が作られた後、判定が `register` を再実行して止まった。
  新規環境で 1 回だけ実行すれば成功する（手元で再現確認済み）。fugu は冪等に作っていた
- S2 の失敗は後段に波及せず、S3-S5 は PASS。最終成果物は `renovater.py` 654 行（fugu は 789 行）
- 10 タスクでは入力を上書きしたが、こちらではフィクスチャに一切触れず、追加検証も `/tmp` に隔離して後片付けしていた
- fugu より 1.3 倍遅い。TodoWrite も Glob も使わず、Read → MultiEdit → Bash の最短経路で進む

### 参考: Claude Code / Codex CLI（サブスク、手動実行）

同じ 5 つのステージプロンプトを、ステージごとに新しいセッションで渡した（ハーネスは各 CLI 側で、lodan ではない）。
ステージごとの時間とツール回数は取れていない。

ステージ直後の状態が残っていない（各エージェントが S5 の自己テストで PR #1 を merge 済み）ので、
**最終 `renovater.py` をフィクスチャだけの新規ディレクトリに置き、S1→S5 の判定を順に再現**した。

| | 最終コード | 再現判定 | 付随物 |
|---|---|---|---|
| Kimi K3 (lodan) | 654 行 | 5/5 | — |
| Claude Code | 899 行 | 5/5 | Dockerfile + `.dockerignore`、issue 5 件 |
| Codex CLI | 276 行 | 5/5 | Dockerfile + `.dockerignore`、`test_renovater.py`（unittest OK）と README を自発的に追加 |

- 再現判定では K3 の S2 も通る（ステージ直後の判定で落ちたのは再実行時の挙動のみ）
- 3 者とも Dockerfile は multi-stage + 非 root ユーザー。差は付随物と冗長さ
  （Claude は最長、Codex は最短でテスト付き）
- この再現判定は「最終コードが全機能を満たすか」であり、「各ステージ時点で満たしたか」ではない。
  後のステージで前の不具合が直った場合も PASS になる

### 批判的検査（判定コマンドの外側）

判定は正常系を 1 回通すだけなので、最終コードを新規環境に置き 47 項目（エラー時の挙動・境界・API の
404/Content-Type・merge の実体など）で突いた。Dockerfile は実際に `docker build` → `docker run` → HTTP 200 を確認。

| | K3 | Claude Code | Codex CLI |
|---|---|---|---|
| Dockerfile 実ビルド・起動 | OK（219MB, nextjs ユーザー） | OK（190MB, node） | OK（190MB, node） |
| 初期コミットが入力とバイト一致 / diff が実 diff と一致 / merge 後の main | OK | OK | OK |
| Dockerfile を変えない PR を差し戻す | **NG**（`"Dockerfile" in diff` の文字列判定で APPROVE） | OK | OK |
| CI の issue 2 を implement | **NG**（issue を見ず常に Dockerfile を書き「成功」） | OK（Actions を実装） | OK（未対応と明示） |
| register の再実行 | **NG** | OK | OK |
| 未対応メソッドも JSON | **NG**（501 text/html） | OK（405） | OK（404） |

K3 は「判定を通す最小限」の実装で、判定外の入力には誤った結果を成功として返す箇所が 2 件ある。
ベンチの点数は Claude / Codex との差を過小評価している。
（`next-env.d.ts` の未収録は入力 PoC の `.gitignore` どおりで、3 者とも正しい）

## 未検証

- `reasoning_effort` を `low` にした場合の速度・品質・コスト（lodan に設定項目が無い）
- 正確なコスト。Moonshot の残高 API では検証後 $24.80（初期残高を記録しておらず差分は不明）

## 再現

```bash
cargo build --release
set -a; source .env; set +a   # KIMI_API_KEY
PROVIDER=kimi MODEL=kimi-k3 KEY_ENV=KIMI_API_KEY PER_TASK_TIMEOUT=600 \
  LODAN=./target/release/lodan EVAL_ROOT=$(mktemp -d) \
  bash docs/eval/kimi_eval_harness.sh
```
