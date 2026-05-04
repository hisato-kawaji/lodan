# lodan

ローカル LLM で動くコーディングエージェント CLI。Anthropic の [Claude Code](https://github.com/anthropics/claude-code) の発想（ツール駆動の対話ループ + 破壊的操作の事前承認 + 簡潔な system prompt）を Rust に最小移植したものです。

## 特徴

- **ランタイム非依存**: OpenAI 互換の Chat Completions + tool calling を話せる任意のサーバーに接続可能（Ollama / llama.cpp `--jinja` / vLLM / LM Studio など）
- **ストリーミング**: SSE でアシスタント本文をリアルタイム表示
- **MVP コアツール**: `Read` / `Write` / `Edit` / `Bash` / `Grep` / `Glob`
- **パーミッションゲート**: 破壊的ツール（Write / Edit / Bash）は実行前にユーザー確認 (`y / n / a / e`)
- **gitignore-aware 検索**: ripgrep の内部クレート (`ignore` + `grep-searcher` + `grep-regex`) を直接利用
- **拡張機構の枠だけ用意**: hooks / MCP / サブエージェント / skills / slash 等のモジュールは骨組みのみ存在し、現状はコメントアウトで非接続

## 必要環境

- Rust 1.85+ (edition 2024)
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

## クイックスタート (llama.cpp)

```bash
# tool-call テンプレ描画のため --jinja は必須
llama-server -m qwen2.5-coder-7b.gguf --jinja --port 8080

cargo run --release -- \
    --base-url http://localhost:8080/v1 \
    --model qwen2.5-coder
```

## 設定

階層: 既定値 ← `~/.config/lodan/config.toml` ← `$CWD/.lodan/config.toml` ← 環境変数 ← CLI フラグ

```toml
# ~/.config/lodan/config.toml
[llm]
base_url = "http://localhost:11434/v1"
model    = "qwen2.5-coder:7b"
api_key  = ""
timeout_secs = 120

[agent]
max_iterations = 25
auto_approve   = false

[tools.bash]
timeout_secs = 30
```

環境変数: `LODAN_BASE_URL` / `LODAN_MODEL` / `LODAN_API_KEY` / `LODAN_AUTO_APPROVE`

CLI フラグ: `--base-url` / `--model` / `--api-key` / `--config <path>` / `--yes`

## REPL の使い方

```
$ cargo run
lodan 0.1.0 — type /help for commands, /exit to quit
model: qwen2.5-coder:7b @ http://localhost:11434/v1
lodan> /tmp/lodan-demo/hello.txt に hi と書いて
[lodan] Allow Write: /tmp/lodan-demo/hello.txt
  (y) yes once  (n) no  (a) always allow Write  (e) always allow this exact
> y
[Write] wrote /tmp/lodan-demo/hello.txt (2 bytes)
hi を書きました。
lodan> /exit
```

組み込み slash: `/exit` `/quit` `/help` `/clear` `/tools`

破壊的ツール承認:
- `y` 一度だけ許可
- `n` 拒否（LLM には "user denied execution" が返り、別アプローチを促せる）
- `a` セッション中はこのツールを常時許可
- `e` Bash の場合のみ、その完全一致コマンドを常時許可

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
│   ├── mod.rs           # trait LlmClient (chat / chat_stream)
│   └── openai.rs        # 非ストリーム + SSE 実装
├── tools/
│   ├── mod.rs           # trait Tool, ToolCtx, ToolOutput
│   ├── registry.rs      # 既定 6 ツール登録（スコープ外はコメントアウト）
│   ├── read.rs / write.rs / edit.rs / bash.rs / grep.rs / glob.rs
│   └── todo_write.rs / web_fetch.rs / web_search.rs / ask_user_question.rs
│       monitor.rs / notebook_edit.rs / multi_edit.rs    # MVP 外スタブ
├── hooks/   mcp/   skills/   slash/   session.rs        # MVP 外スタブ
```

## ロードマップ（MVP 外、骨組みは存在）

- hooks (PreToolUse / PostToolUse / SessionStart 等のディスパッチ)
- MCP (stdio / http トランスポートのクライアント・サーバー登録)
- サブエージェント spawn
- TodoWrite / WebFetch / WebSearch / AskUserQuestion / Monitor / NotebookEdit / MultiEdit
- skills（`.lodan/skills` のロード）
- slash 拡張（ユーザー定義コマンド）
- 永続セッション・トランスクリプト保存・トークン会計
- 中断時の副作用ロールバック

各ファイルは `src/{hooks,mcp,skills,slash,session,tools/...}` に存在し、`unimplemented!()` で待機中。`agent/loop.rs` の該当呼び出しは `// MVP 外` でコメントアウトされており、肉付け箇所が一目で分かる作りです。

## テスト

```bash
cargo test
```

カバレッジ:
- `tools/edit.rs` — 一意マッチ / 多重マッチ拒否 / Read 必須
- `tools/read.rs` — offset / limit
- `permission.rs` — auto_approve / always-tool / always-command の判定

## ライセンス

MIT OR Apache-2.0
