# D2 編集直後の構文チェック — 実装計画（#65 Phase 2）

**狙い**: 小型モデルが壊れた内容を書いたとき、何ターンも気付かれずに積み上がるのを防ぐ。
ベンチの「長い自己編集で内容が崩れる」「正しい方向で作業中に TIMEOUT」に効く。
A2（骨格 → Edit で埋める）と組み合わせると、埋めた直後に毎回検査が入る。

## 設定

```toml
[agent]
syntax_check = true          # 既定 false (ablation 用に ladder の `syntax-none`)

# 言語ごとのコマンド。既定はこの表 (上書き・追加・空文字で無効化ができる)
[agent.syntax_checks]
py   = "python3 -m py_compile {path}"
js   = "node --check {path}"
mjs  = "node --check {path}"
ts   = ""                    # 既定では無し (tsc は遅く、単ファイルで完結しない)
json = "python3 -m json.tool {path}"
rs   = ""                    # cargo check はプロジェクト全体で遅いので既定では無し
sh   = "bash -n {path}"
toml = ""                    # 要 Python tomllib。既定は無し
```

- `{path}` は書いたファイルの絶対パス。他のプレースホルダは無し
- 拡張子に当てはまる設定が無い / 空文字なら**何もしない**
- 1 回の上限は 5 秒（固定）。超えたら「検査できなかった」として通す（ブロックしない）

## 実行場所

`Session::run_tool`（`gate.decide_assessed` → `Allow` → `tool.execute` の直後）に 1 か所。
`snapshot_for_undo` と同じ「ファイル系ツールかつ `path` 引数」の判定を使い回す:

- 対象ツール: `UNDOABLE_FILE_TOOLS`（Write / Edit / MultiEdit / NotebookEdit）と同じ集合
- 成功した実行だけ（`!output.is_error`）
- サンドボックスは**通さない**（lodan 本体が走らせる検査コマンドで、モデルが書いた文字列ではない）。
  ただしコマンドは利用者の設定由来なので、`sh -c` に渡すのは `{path}` を shell-quote した形
- 失敗したら、ツール出力の後ろに注記を足す（ツール自体は成功のまま。ファイルは書かれている）:

  ```
  wrote /x/scanner.py (812 bytes)

  [syntax] python3 -m py_compile failed (exit 1):
  SyntaxError: invalid syntax (scanner.py, line 42)
  Fix it with Edit before moving on.
  ```

- runlog: `syntax_check { path, ext, ok, ms }`（ablation で発火回数と検出率を数える）

## やらないこと（意図）

- **ブロックしない**。書き込み自体は済んでいるし、拒否すると「書けない」ループに入る。
  モデルに知らせて直させるのが目的
- プロジェクト全体のビルド（`cargo check` / `tsc`）は D1（`[[verify]]`）の持ち場。D2 は単ファイル・数百 ms で終わるものだけ
- 構文が通ったことは言わない（出力を増やさない）。runlog にだけ残す

## テスト

- `syntax.rs`: 拡張子 → コマンドの解決（既定・上書き・空文字・未知の拡張子）、`{path}` の置換と quote
- `loop.rs`: 壊れた Python を Write したら注記が付き、正しいものには付かない。`syntax_check = false` では走らない
- e2e: mock に壊れた .py を書かせ、stream-json のツール結果に `[syntax]` が入り、runlog に `syntax_check` が出る

## 触るファイル

`src/tools/syntax.rs`（新規）/ `src/agent/loop.rs`（`run_tool` の後ろ）/ `src/config.rs` / `src/cli.rs`
（`--syntax-check[=<bool>]`）/ `docs/eval/ladder/ladder.sh`（`syntax-none`）/ README

---

# A2 の修正案（実測で分かった問題。Phase 2 に入れる）

2026-10-11 の ablation で、上限を 40 行にして**初めて A2 が発動**した 3 実行のうち 1 本が
「骨格 → Edit で埋める」の**後半で壊れた**。

| split40-none L2-cli-json | 結果 | 埋め戻し |
|---|---|---|
| #1 | 2/7 | 132 行を拒否 → 40 行の骨格 → Edit ×2 成功 → **`old_string not found` ×3** → 未完成で終了 (exit 1) |
| #2 | 7/7 | 79 行を拒否 → 骨格 → **Edit 1 回で成功** → Bash で検証 |

9B は自分が書いたプレースホルダを**正確に再現できない**ことがある (#145 のレビューが指摘した
「重複で弾かれる」より厳しい、不一致で当たらない形)。

## 対策: 埋め戻しを Edit ではなく「追記」にする

- `Write` に `append: true` を足す（または `Append` ツール）。`old_string` の一致が要らないので、
  小型モデルでも外さない
- 拒否メッセージの誘導を「骨格 → 末尾に追記 ×N」に変える。1 回の追記も `write_max_lines` で切る
- 追記は既存ファイルへの書き込みなので、`was_read` の扱いと `/undo` のスナップショットは Write と同じ
- D2（編集直後の構文チェック）があれば、未完成のまま終わるのを検出して直させられる（#1 の
  失敗は「83 行の未完成品が残って終了」なので、構文チェックで気付ける可能性が高い）

## 計測のしかた（次の ablation）

- 上限は **40**（ラダーの L2 が作るファイルは 51〜77 行なので 80 では発動しない。実測）
- `write_split_refused` が発火した実行だけを A2 の評価対象にする（発火の有無は runlog で機械判定）
- 同条件 3 回。L2-cli-json / L2-http が対象（L0 / L1 は小さすぎて発動しない）
