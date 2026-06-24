# lodan

ローカル LLM で動くコーディングエージェント CLI。Anthropic の [Claude Code](https://github.com/anthropics/claude-code) の発想（ツール駆動の対話ループ + 破壊的操作の事前承認 + 簡潔な system prompt）を Rust に最小移植したものです。

## 特徴

- **ランタイム非依存**: OpenAI 互換の Chat Completions + tool calling を話せる任意のサーバーに接続可能（Ollama / llama.cpp `--jinja` / vLLM / LM Studio など）
- **マルチプロバイダ**: ローカル LLM と Sakana AI (`fugu` / `fugu-ultra`) を環境変数で随時切り替え
- **MCP クライアント (stdio + tools)**: `.mcp.json` を CWD に置くと MCP サーバを起動して公開 tools を取り込む
- **ストリーミング**: SSE でアシスタント本文をリアルタイム表示
- **コアツール**: `Read` / `Write` / `Edit` / `Bash` / `Grep` / `Glob` / `TodoWrite`
- **パーミッションゲート**: 破壊的ツール（Write / Edit / Bash / MCP 全般）は実行前にユーザー確認 (`y / n / a / e`)
- **gitignore-aware 検索**: ripgrep の内部クレート (`ignore` + `grep-searcher` + `grep-regex`) を直接利用
- **hooks**: `UserPromptSubmit` / `PreToolUse` / `PostToolUse` で外部コマンドを発火し、exit code でツール実行をブロック（後述）
- **ユーザー定義 slash コマンド**: `.lodan/commands/*.md` をプロンプトテンプレートとして読み込み、`/name 引数` で展開（後述）
- **拡張機構の枠だけ用意**: サブエージェント / skills 等のモジュールは骨組みのみ存在し、現状はコメントアウトで非接続

## 必要環境

- Rust 1.86+（`edition = "2024"` の最低要件は 1.85 だが、依存クレート（`icu_properties` など）が 1.86 以上を要求するため）
- ローカル LLM サーバー（後述）

## インストール

```bash
git clone <this repo> lodan
cd lodan
cargo build --release
```

バイナリは `target/release/lodan`。

## クイックスタート (Ollama)

```bash
ollama serve &
ollama pull qwen2.5-coder:7b

# 既定で http://localhost:11434/v1, qwen2.5-coder:7b を見にいく
cargo run --release
```

## クイックスタート (Sakana AI)

Sakana Fugu API は OpenAI 互換の Chat Completions を喋るので、`--provider sakana` を渡すだけで切り替わる。API キーは `.env` または環境変数から拾われる。

```bash
# lodan/.env を作る（自分で書く / .gitignore 済み）
echo 'SAKANA_API_KEY=sk-...' > .env

cargo run --release -- --provider sakana --model fugu
# あるいは fugu-ultra
cargo run --release -- --provider sakana --model fugu-ultra
```

環境変数だけで切り替える例:

```bash
LODAN_PROVIDER=sakana LODAN_MODEL=fugu cargo run --release
```

## クイックスタート (llama.cpp)

```bash
# tool-call テンプレ描画のため --jinja は必須
llama-server -m qwen2.5-coder-7b.gguf --jinja --port 8080

cargo run --release -- \
    --base-url http://localhost:8080/v1 \
    --model qwen2.5-coder
```

GGUF をローカルに用意せず HuggingFace から自動取得することもできる:

```bash
llama-server -hf bartowski/Meta-Llama-3.1-8B-Instruct-GGUF:Q4_K_M \
    --jinja --port 8080

cargo run --release -- \
    --base-url http://localhost:8080/v1 \
    --model "bartowski/Meta-Llama-3.1-8B-Instruct-GGUF:Q4_K_M"
```

### モデルの相性メモ

ツール呼び出し (tool calling) を安定して通すには、モデルが自身の chat template が要求する形式（Qwen 系なら `<tool_call>...</tool_call>` 単数）を厳密に守る必要がある。低ビット量子化（Q4_K_M 以下）の小型モデルはこの形式を時々踏み外し、llama.cpp 側のパーサが構造化 tool_calls を抽出できず素テキストで返してしまうことがある。

手元の動作確認 (Apple M3 / llama.cpp b9020) で得た傾向:

| モデル | ツール呼び出し成功率の体感 |
|---|---|
| `bartowski/Meta-Llama-3.1-8B-Instruct-GGUF:Q4_K_M` | 安定 |
| `bartowski/Qwen2.5-Coder-7B-Instruct-GGUF:Q5_K_M` 以上 | 概ね安定 |
| `bartowski/Qwen2.5-Coder-7B-Instruct-GGUF:Q4_K_M` | 不安定（`<tool_calls>` 複数形を吐いて素テキストになることがある） |

lodan が「LLM が応答するだけでツールが起きない」場合は、まず量子化を Q5_K_M 以上に上げるか、Llama-3.1-8B-Instruct のような instruction-following が強いモデルに切り替えると良い。

## 設定

階層: 既定値 ← `~/.config/lodan/config.toml` ← `$CWD/.lodan/config.toml` ← `$CWD/.env` ← 環境変数 ← CLI フラグ

```toml
# ~/.config/lodan/config.toml
[llm]
provider = "local"   # "local" または "sakana"

[llm.local]
base_url     = "http://localhost:11434/v1"
model        = "qwen2.5-coder:7b"
api_key      = ""
timeout_secs = 120

[llm.sakana]
base_url     = "https://api.sakana.ai/v1"
model        = "fugu"      # または "fugu-ultra"
api_key      = ""           # 空なら SAKANA_API_KEY env を使う
timeout_secs = 120

[agent]
max_iterations = 25
auto_approve   = false

[tools.bash]
timeout_secs = 30
```

`--base-url` / `--model` / `--api-key` および対応する `LODAN_*` env は **現在 active な provider** の設定を上書きする (provider を `--provider` で切り替えれば反対側を触らずに済む)。

環境変数:
- `LODAN_PROVIDER` (`local` | `sakana`)
- `LODAN_BASE_URL` / `LODAN_MODEL` / `LODAN_API_KEY` / `LODAN_AUTO_APPROVE`
- `SAKANA_API_KEY` (provider=sakana のときに `api_key` が空ならフォールバック)

CLI フラグ: `--provider` / `--base-url` / `--model` / `--api-key` / `--config <path>` / `--yes`

`$CWD/.env` は起動時に自動ロード (dotenvy)。コミット対象外 (`.gitignore` 済)。

### v0.1.0 以前からのスキーマ移行

`[llm]` 直下にあった `base_url` / `model` / `api_key` / `timeout_secs` は `[llm.local]` 配下に移った。Sakana 側 (`[llm.sakana]`) は新規追加。既存の `~/.config/lodan/config.toml` は上記の新フォーマットに書き換えが必要。

## REPL の使い方

```
$ cargo run
lodan 0.1.0 — type /help for commands, /exit to quit
model: qwen2.5-coder:7b @ http://localhost:11434/v1 (local)
lodan> /tmp/lodan-demo/hello.txt に hi と書いて
[lodan] Allow Write: /tmp/lodan-demo/hello.txt
  (y) yes once  (n) no  (a) always allow Write  (e) always allow this exact
> y
[Write] wrote /tmp/lodan-demo/hello.txt (2 bytes)
hi を書きました。
lodan> /exit
```

組み込み slash: `/exit` `/quit` `/help` `/clear` `/tools`（ユーザー定義コマンドは後述）

破壊的ツール承認:
- `y` 一度だけ許可
- `n` 拒否（LLM には "user denied execution" が返り、別アプローチを促せる）
- `a` セッション中はこのツールを常時許可
- `e` Bash の場合のみ、その完全一致コマンドを常時許可

## MCP サーバ接続 (stdio + tools)

`$CWD/.mcp.json` を置くと REPL 起動時に MCP サーバを spawn し、公開された tools を `mcp__<server>__<tool>` 名で `ToolRegistry` に取り込む。

```json
// .mcp.json (Claude Code 互換スキーマ)
{
  "mcpServers": {
    "fs": {
      "command": "npx",
      "args": ["-y", "@modelcontextprotocol/server-filesystem", "/tmp/lodan-fs"]
    }
  }
}
```

サンプルは `.mcp.json.example` を参照。

- **transport**: stdio のみ (Streamable HTTP は未対応)
- **capabilities**: tools のみ (resources / prompts / sampling / roots は未対応)
- **permission**: MCP 由来のツールは **常に destructive** 扱い。初回呼び出しで `y/n/a/e` の確認が出る (Claude Code 同様)
- **起動失敗の扱い**: サーバ起動 / `tools/list` 失敗は warning に留め、REPL は built-in 7 ツールのみで継続起動
- **プロトコル**: MCP `2025-06-18`、JSON-RPC 2.0、newline-delimited

REPL 起動時に MCP サーバが見つかると次の行がバナーに出る:

```
mcp: 1 server(s), 11 tool(s) registered
```

## アーキテクチャ概要

```
src/
├── main.rs / cli.rs / config.rs / repl.rs
├── prompt.rs            # system prompt 生成
├── permission.rs        # 4 択ゲート
├── agent/
│   ├── messages.rs      # OpenAI Chat スキーマ準拠の Message / ToolCall
│   ├── loop.rs          # run_turn(): chat_stream → tool dispatch → 反復
│   └── subagent.rs      # MVP 外（スタブ）
├── llm/
│   ├── mod.rs           # trait LlmClient + provider 分岐 (build_client)
│   ├── openai.rs        # ローカル/汎用 OpenAI 互換クライアント
│   └── sakana.rs        # Sakana AI (Fugu) adapter (内部で OpenAiClient に委譲)
├── tools/
│   ├── mod.rs           # trait Tool, ToolCtx, ToolOutput
│   ├── registry.rs      # 既定 7 ツール登録（スコープ外はコメントアウト）
│   ├── read.rs / write.rs / edit.rs / bash.rs / grep.rs / glob.rs
│   ├── todo_write.rs                                    # セッション scope の Todo リスト
│   └── web_fetch.rs / web_search.rs / ask_user_question.rs
│       monitor.rs / notebook_edit.rs / multi_edit.rs    # MVP 外スタブ
├── hooks/                                                # 外部コマンド hook ディスパッチ
├── slash/                                                # ユーザー定義 slash コマンド
├── session.rs                                            # セッション永続化 (transcript / resume)
├── mcp/   skills/                                         # MVP 外スタブ
```

## hooks

`config.toml` の `[[hooks]]` 配列で、ライフサイクルイベント発火時に外部コマンドを実行できます。
コマンドはイベントの JSON ペイロードを stdin で受け取り、終了コードで制御します
（exit 0 = 続行、非 0 = ブロック。理由は stderr → stdout の順で採用）。

```toml
# Bash 実行前にガードスクリプトを通す。non-zero で実行をブロック。
[[hooks]]
event = "PreToolUse"     # UserPromptSubmit | PreToolUse | PostToolUse
matcher = "Bash"         # 省略可: ツール名一致（Pre/PostToolUse のみ）。空 / "*" で全ツール
command = "./scripts/guard.sh"
```

- **UserPromptSubmit**: ペイロード `{"prompt": "..."}`。ブロック時はそのターンを実行せず破棄。
- **PreToolUse**: `{"tool_name", "tool_input"}`。ブロック時はツールを実行せず、理由をモデルへ返す。
- **PostToolUse**: `{"tool_name", "tool_input", "tool_output"}`。実行後の観測用（取り消し不可、理由を表示）。

hook の起動自体に失敗した場合は警告のみで続行（fail-open）、30 秒でタイムアウトします。

> ⚠️ **信頼前提**: hook コマンドは CWD のプロジェクト `config.toml` から無確認で `sh -c` 実行されます（パーミッションゲートを経ません）。`.mcp.json` と同様、信頼できないリポジトリの設定をそのまま起動しないでください（任意コード実行になり得ます）。

## ユーザー定義 slash コマンド

`$CWD/.lodan/commands/<name>.md` を置くと、起動時に読み込まれて `/name` で使えます。
ファイル本文がプロンプトテンプレートになり、`/name 引数...` で展開してエージェントへ投入されます。

```markdown
---
description: 直近の diff をレビューする
---
git diff を確認して、$ARGUMENTS の観点でレビューしてください。
```

- `$ARGUMENTS` → 引数全体、`$1`..`$9` → 空白区切りの位置引数（該当なしは空文字）
- frontmatter の `description:` は任意で、`/help` の一覧に表示されます
- 組み込み（`/exit` `/clear` `/tools` `/help`）と同名のファイルは警告して無視されます

> ⚠️ **信頼前提**: コマンドファイルは CWD の `.lodan/commands/` から読まれ、本文がそのままモデルへのプロンプトになります。信頼できないリポジトリのコマンドは prompt injection ベクタになり得ます（hooks / `.mcp.json` と同じ CWD 信頼前提）。ただし展開結果はユーザー入力と同じ経路で、破壊的ツールは従来どおりパーミッションゲートを通ります。

## セッション永続化・再開

REPL セッションは自動的に保存され、後から再開できます。

- 保存先: `<データディレクトリ>/lodan/sessions/<id>/`（macOS なら `~/Library/Application Support/lodan/sessions/`）
  - `meta.json`: id / 作成時刻 / cwd / provider / model
  - `transcript.jsonl`: 各メッセージを 1 行 1 件でターンごとに追記
- `lodan sessions` — 保存済みセッションを一覧表示
- `lodan --resume <id>` — 指定 id を再開（`--resume last` で直近を再開）

```console
$ lodan
session: 1782332785130-31477   # 起動時に新規 id を表示
...
$ lodan --resume last
session: resumed 1782332785130-31477 (12 messages)
```

再開時は保存済みの会話を読み戻したうえで、**system prompt は現在の環境（ツール一覧）で作り直します**。
永続化に失敗してもセッションは継続します（その場合は保存なしの ephemeral 動作）。

- `--resume last` は **cwd を問わず全セッションの最新**を選びます（現状はプロジェクト単位の索引なし）。別ディレクトリのセッションを拾い得る点に注意。
- transcript には Read したファイル内容や貼り付けた値が**平文**で残ります。セッションディレクトリは本人のみアクセス可（unix で dir `0700` / file `0600`）に制限しますが、秘密情報の扱いには留意してください。
- 中断などで tool 呼び出しの結果が揃わなかったターンは、再投入の整合性のため保存されません（解決済みの履歴のみ追記）。

## ロードマップ（MVP 外、骨組みは存在）

- MCP の拡張: Streamable HTTP transport / resources / prompts / sampling / roots
- サブエージェント spawn
- WebFetch / WebSearch / AskUserQuestion / Monitor / NotebookEdit / MultiEdit
- skills（`.lodan/skills` のロード）
- トークン会計
- 中断時の副作用ロールバック

各ファイルは `src/{mcp,skills,tools/...}` に存在し、`unimplemented!()` で待機中。`agent/loop.rs` の該当呼び出しは `// MVP 外` でコメントアウトされており、肉付け箇所が一目で分かる作りです。

## テスト

```bash
cargo test
```

カバレッジ:
- `tools/edit.rs` — 一意マッチ / 多重マッチ拒否 / Read 必須
- `tools/read.rs` — offset / limit
- `tools/todo_write.rs` — replace / clear / multi-in_progress 拒否 / 引数不正
- `tools/registry.rs` — 動的名登録 / built-in 既定 7 ツール
- `permission.rs` — auto_approve / always-tool / always-command の判定
- `repl.rs` — slash command 判定（絶対パス始まりは LLM に流す）
- `mcp/config.rs` / `mcp/protocol.rs` / `mcp/tool.rs` — `.mcp.json` パース、JSON-RPC + MCP 型、namespacing
- `tests/e2e_mock.rs` — 6 ツールを順に走らせるエンドツーエンドのモック試験
- `tests/e2e_mcp.rs` — mock MCP サーバとの handshake + tools/list + tools/call 一周

## ライセンス

MIT OR Apache-2.0
