# lodan

ローカル LLM で動くコーディングエージェント CLI。Anthropic の [Claude Code](https://github.com/anthropics/claude-code) の発想（ツール駆動の対話ループ + 破壊的操作の事前承認 + 簡潔な system prompt）を Rust に最小移植したものです。

## 特徴

- **ランタイム非依存**: OpenAI 互換の Chat Completions + tool calling を話せる任意のサーバーに接続可能（Ollama / llama.cpp `--jinja` / vLLM / LM Studio など）
- **マルチプロバイダ**: ローカル LLM と Sakana AI (`fugu` / `fugu-ultra`) を環境変数で随時切り替え
- **MCP クライアント (stdio / HTTP + tools / prompts / resources)**: `.mcp.json` を CWD に置くと MCP サーバ（ローカル stdio / リモート Streamable HTTP）へ接続し、公開 tools を取り込み、prompts は `/mcp__<server>__<prompt>`、resources は `mcp__<server>__read_resource` で扱える
- **ストリーミング**: SSE でアシスタント本文をリアルタイム表示
- **コアツール**: `Read` / `Write` / `Edit` / `Bash`（`run_in_background` で detached 実行も可） / `Grep` / `Glob` / `TodoWrite` / `MultiEdit` / `NotebookEdit`（.ipynb セル編集） / `WebFetch`（http(s) GET → テキスト化） / `WebSearch`（Brave Search API） / `AskUserQuestion`（選択式の質問） / `Monitor`（バックグラウンドプロセスの増分出力・状態取得） / `KillShell`（バックグラウンドプロセスの終了） / `Task`（調査用サブエージェント）
- **パーミッションゲート**: 破壊的ツール（Write / Edit / Bash / MCP 全般）は実行前にユーザー確認 (`y / n / a / e`)
  - `WebFetch` は read-only な GET なので**非破壊**（ゲートを経ない）。⚠️ ただしフェッチ先 URL はモデルが決めるため、内部ネットワーク到達 (SSRF) やクエリ経由の情報送出があり得る。http/https のみ許可・タイムアウト・サイズ上限を課し、リダイレクトも各ホップを http/https に限定して最大 5 ホップに制限する。**ただしリダイレクト先の内部ホスト到達まではブロックしない**ため、実行環境を信頼する前提（hooks / `.mcp.json` と同じ）で使うこと
  - `WebSearch` も read-only（非破壊）。env `BRAVE_API_KEY` が要り、未設定ならエラーを返す。クエリは外部 (Brave) へ送られるため、上と同じ信頼前提で使うこと。エンドポイントは env `BRAVE_SEARCH_API_URL` で差し替え可能だが（テスト用）、こちらも http/https のみ許可する
- **gitignore-aware 検索**: ripgrep の内部クレート (`ignore` + `grep-searcher` + `grep-regex`) を直接利用
- **hooks**: `SessionStart` / `SessionEnd` / `UserPromptSubmit` / `PreToolUse` / `PostToolUse` / `Stop` で外部コマンドを発火し、exit code でツール実行やターン停止を制御（後述）
- **ユーザー定義 slash コマンド**: `.lodan/commands/*.md` をプロンプトテンプレートとして読み込み、`/name 引数` で展開（後述）
- **サブエージェント (`Task`)**: 読み取り専用ツールで調査タスクを子エージェントに委譲（後述）
- **skills**: `.lodan/skills/<name>/SKILL.md` を読み込み、`Skill` ツールとしてモデルへ公開（後述）
- **プロジェクトメモリ**: cwd 階層の `LODAN.md`（無ければ `CLAUDE.md`）と `~/.lodan/LODAN.md` を読み、system prompt へ注入（後述）
- **拡張機構の枠だけ用意**: skills 等のモジュールは骨組みのみ存在し、現状はコメントアウトで非接続

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

組み込み slash: `/exit` `/quit` `/help` `/clear` `/tools`（ユーザー定義コマンドは後述）。`/help` は組み込み・ユーザー定義・MCP prompt を説明付きで、`/tools` は各ツールを説明付きで一覧する。

**端末装飾**: ツール出力・エラー・承認プロンプトを ANSI で色分けし、LLM 応答待ちは `…thinking` インジケータを表示する。stdout が tty でない（パイプ／リダイレクト）とき、または `NO_COLOR` 環境変数が設定されているときは着色・インジケータを一切出さない。

破壊的ツール承認:
- `y` 一度だけ許可
- `n` 拒否（LLM には "user denied execution" が返り、別アプローチを促せる）
- `a` セッション中はこのツールを常時許可
- `e` Bash の場合のみ、その完全一致コマンドを常時許可

## MCP サーバ接続 (stdio / HTTP + tools / prompts / resources)

`$CWD/.mcp.json` を置くと REPL 起動時に MCP サーバへ接続し、公開された tools を `mcp__<server>__<tool>` 名で `ToolRegistry` に取り込む。サーバが prompts を公開していれば `mcp__<server>__<prompt>` 名の slash コマンドとしても取り込む。

```json
// .mcp.json (Claude Code 互換スキーマ)
{
  "mcpServers": {
    "fs": {                                              // stdio transport
      "command": "npx",
      "args": ["-y", "@modelcontextprotocol/server-filesystem", "/tmp/lodan-fs"]
    },
    "remote": {                                          // Streamable HTTP transport
      "url": "https://example.com/mcp",
      "headers": { "Authorization": "Bearer YOUR_TOKEN" }
    }
  }
}
```

サンプルは `.mcp.json.example` を参照。`command` があれば stdio、`url` があれば HTTP（両方／どちらも無いはエラー）。

- **transport**: **stdio** (`command`) と **Streamable HTTP** (`url`)。HTTP は POST で JSON-RPC を送り、`application/json` または `text/event-stream` (SSE) の応答を受ける。`Mcp-Session-Id` を引き継ぎ、`headers` で認証ヘッダを付与できる。HTTP の server→client GET ストリームは未対応
- **capabilities**: tools / prompts / resources / roots、および opt-in の **sampling**。**roots** はクライアントが作業ディレクトリ (cwd) を `file://` root としてサーバへ公開する（initialize で capability 宣言 → サーバの `roots/list` リクエストに応答）。server→client リクエストの受信は **stdio のみ**対応。⚠️ roots はサーバに **cwd の絶対パスを開示**します（`.mcp.json` のサーバを信頼する前提と同じ範囲）
- **permission**: MCP 由来の **tools/call は常に destructive** 扱いで初回呼び出しに `y/n/a/e` 確認 (Claude Code 同様)。**resources は read-only なので非破壊** (ゲートを経ない)
- **起動失敗の扱い**: サーバ起動 / `tools/list` 失敗は warning に留め、REPL は built-in ツールのみで継続起動。`prompts/list` / `resources/list` 非対応サーバは warning で skip
- **プロトコル**: MCP `2025-06-18`、JSON-RPC 2.0（stdio は newline-delimited、HTTP は POST 1 リクエスト/レスポンス）

### MCP prompts

サーバが公開する prompt は `/mcp__<server>__<prompt> 引数...` で呼び出せます。
位置引数を prompt の宣言引数に順番で対応づけて `prompts/get` を実行し、返ってきたメッセージを
テキスト化してユーザターンとしてエージェントへ投入します（`/help` に一覧表示）。
ユーザー定義 slash・skills と同じ「サーバ提供のコマンド」レイヤです。

> ⚠️ **信頼前提**: prompt の本文は接続先 MCP サーバが返すもので、そのままモデルへのプロンプトとして注入されます。信頼できない MCP サーバの prompt は prompt injection ベクタになり得ます（`.mcp.json` のサーバ自体を信頼する前提と同じ）。破壊的ツールは従来どおりパーミッションゲートを通ります。

### MCP resources

サーバが resources を公開していれば、サーバごとに **`mcp__<server>__read_resource`** ツールが 1 つ登録されます。
ツールの説明文に公開 resource の `uri` 一覧が載り、モデルが `uri` を指定して呼ぶと `resources/read` の内容を
テキスト化して返します。read-only なので**非破壊**（パーミッションゲートを経ません）。バイナリ (blob) リソースは件数のみ注記してスキップします。

> ⚠️ **信頼前提**: read_resource は非ゲートなので、`file://` 等を公開するサーバ相手では**無確認の任意ファイル読み出し**になり得ます。クライアントは `uri` を制限せず認可境界はサーバ側に委ねるため、`.mcp.json` のサーバ自体を信頼する前提（prompts と同じ信頼モデル）で利用してください。

### MCP sampling (server→client の LLM 補完)

サーバが `sampling/createMessage` でクライアント側の LLM 補完を要求できます。サーバが
こちらの**モデル・トークンを駆動**するため、既定では無効。`.mcp.json` の各サーバに
**`"allowSampling": true`** を付けたサーバにのみ許可し、initialize で sampling capability を
広告します（stdio のみ）。許可サーバの要求は、現在 active な provider/model の LLM へ
そのまま渡され、結果を assistant テキストとして返します。サーバ指定の `maxTokens` は
生成上限として LLM に渡され、無制限生成を防ぎます。

```jsonc
{
  "mcpServers": {
    "trusted": { "command": "npx", "args": ["-y", "some-mcp"], "allowSampling": true }
  }
}
```

> ⚠️ **信頼前提**: sampling を許可したサーバは、こちらの LLM へ任意のプロンプトを投げて
> モデル/トークンを消費できます。`allowSampling` は信頼するサーバにのみ付けてください
> （MVP では都度承認プロンプトは挟まず、config の opt-in で一括許可します）。未許可サーバの
> `sampling/createMessage` は `method not found` を返します。

REPL 起動時に MCP サーバが見つかると次の行がバナーに出る:

```
mcp: 1 server(s), 11 tool(s), 2 prompt(s), 1 resource(s) registered
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
│   ├── registry.rs      # 既定 14 ツール登録
│   ├── read.rs / write.rs / edit.rs / bash.rs / grep.rs / glob.rs
│   ├── todo_write.rs / multi_edit.rs / notebook_edit.rs        # 追加ビルトイン
│   ├── web_fetch.rs / web_search.rs / ask_user_question.rs     # 追加ビルトイン
│   ├── monitor.rs / kill_shell.rs                              # BG プロセス監視・終了
│   └── background.rs                                           # BG プロセス共有ストア
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
event = "PreToolUse"     # SessionStart | SessionEnd | UserPromptSubmit | PreToolUse | PostToolUse | Stop
matcher = "Bash"         # 省略可: ツール名一致（Pre/PostToolUse のみ）。空 / "*" で全ツール
command = "./scripts/guard.sh"
```

- **SessionStart**: `{"hook_event_name", "cwd"}`。REPL 起動時に発火。ブロックしても起動は止めず警告のみ。
- **SessionEnd**: `{"hook_event_name"}`。REPL 終了時に発火（ベストエフォート、ブロック不可）。
- **UserPromptSubmit**: `{"prompt"}`。ブロック時はそのターンを実行せず破棄。
- **PreToolUse**: `{"tool_name", "tool_input"}`。ブロック時はツールを実行せず、理由をモデルへ返す。
- **PostToolUse**: `{"tool_name", "tool_input", "tool_output"}`。実行後の観測用（取り消し不可、理由を表示）。
- **Stop**: `{"hook_event_name", "last_message"}`。ターン終端（最終アシスタントテキスト）で発火。**ブロックすると停止せず、その理由をユーザー入力として注入し次ターンへ継続する**（暴走は `max_iterations` で停止）。「条件を満たすまで作業を続ける」系の自律ループの土台。

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

## サブエージェント（`Task` ツール）

メインエージェントは `Task` ツールで調査タスクを子エージェントに委譲できます。

- 子は **読み取り専用ツール（`Read` / `Grep` / `Glob`）だけ** を持ち、headless にツールループを回して最終テキストを 1 つの要約として返します。
- 破壊的操作を持たないため承認ゲートを経ず、`Task` 自身を含めないため無限再帰しません。
- 引数: `description`（短いラベル）+ `prompt`（自己完結した調査指示。子は親の会話を見ません）。
- ループは `agent.max_iterations` と子専用上限（12）の小さい方で打ち切られます。起動時に `↳ Task: <description>` を表示します。

```jsonc
// メインエージェントが発行する tool call の例
{ "name": "Task",
  "arguments": { "description": "find config loaders",
                 "prompt": "config.toml を読む箇所を列挙し、優先順位を要約して" } }
```

## skills（`Skill` ツール）

`$CWD/.lodan/skills/<name>/SKILL.md` を置くと、起動時に読み込まれ `Skill` ツールとしてモデルへ公開されます。
ユーザーが `/name` で明示起動する slash コマンドと対になり、**skills はモデルが必要に応じて自分で起動**します。

```markdown
<!-- .lodan/skills/review/SKILL.md -->
---
name: review
description: コードレビューの観点と手順
---
次の観点で diff をレビューしてください: 1. 正しさ 2. 命名 3. テスト ...
```

- `Skill` ツールの説明文に利用可能な skill 一覧（`name: description`）が載り、モデルが `Skill { "name": "review" }` を呼ぶと **本文（instructions）が返されて文脈に載ります**（progressive disclosure）。
- frontmatter の `name` を省略するとディレクトリ名が使われます。`description` は一覧表示用です。
- skill が 1 つも無ければ `Skill` ツール自体を登録しません。

> ⚠️ **信頼前提**: `SKILL.md` の本文は CWD の `.lodan/skills/` から読まれ、そのままモデルへのプロンプトとして注入されます。信頼できないリポジトリの skill は prompt injection ベクタになり得ます（hooks / slash / `.mcp.json` と同じ CWD 信頼前提）。破壊的ツールは従来どおりパーミッションゲートを通ります。

## バックグラウンド実行と Monitor

`Bash` に `run_in_background: true` を渡すと、子プロセスを detached で起動して即座に
プロセス ID（`bash_N`）を返す。`Monitor` ツールに `id` を渡すと、前回読んだ位置以降の
**増分出力**と `running` / `exited(code)` のステータスが返る（cursor はセッション内で保持）。
出力は stdout/stderr 混在で 1 MiB を上限に蓄積し、超過分は `...[truncated]...` で打ち切る。

```text
Bash { "command": "cargo build", "run_in_background": true }   → started background process bash_1
Monitor { "id": "bash_1" }                                     → 新規出力 + status: running
Monitor { "id": "bash_1" }                                     → 続き + status: exited(0)
KillShell { "id": "bash_1" }                                   → kill 合図 → 以降 Monitor は status: killed
```

`Monitor` は読み取り専用なのでパーミッションゲートを経ない（`Bash` の起動自体は従来どおりゲート対象）。
`KillShell` はプロセスを終了させる副作用があるため**破壊的ツール扱い**で承認ゲートを通る。

## プロジェクトメモリ（`LODAN.md` / `CLAUDE.md`）

起動時に **cwd から上方向**（`$HOME` まで、無ければ root まで）の各ディレクトリにある `LODAN.md`（無ければ `CLAUDE.md`）と、ユーザ全体の `~/.lodan/LODAN.md` を読み込み、**system prompt の末尾へ注入**する。Claude Code の `CLAUDE.md` 階層に相当。

- 連結順は **外側（汎用）→ 内側（具体）**。各エントリに `# Memory: <path>` ヘッダが付く。
- 合計 32 KiB を上限に、超過分は文字境界で打ち切る（`...[memory truncated]...`）。
- 中身が空（空白のみ）のファイルは無視する。
- cwd が `$HOME` 配下なら遡上は `$HOME` で打ち切る。cwd が home 外（例 `/opt/proj`）の場合は filesystem root まで遡る（Claude Code と同じ挙動）。

> ⚠️ **信頼前提**: memory は CWD 階層からそのままプロンプトへ注入される。信頼できないリポジトリの `LODAN.md` / `CLAUDE.md` は prompt injection ベクタになり得る（skills / hooks / `.mcp.json` と同じ CWD 信頼前提）。注入時に「承認ゲートを回避する指示ではない」旨を system prompt に明記している。

## ロードマップ（MVP 外、骨組みは存在）

- トークン会計
- 中断時の副作用ロールバック

各ファイルは `src/{mcp,tools/...}` に存在し、`unimplemented!()` で待機中。`agent/loop.rs` の該当呼び出しは `// MVP 外` でコメントアウトされており、肉付け箇所が一目で分かる作りです。

## テスト

```bash
cargo test
```

カバレッジ:
- `tools/edit.rs` — 一意マッチ / 多重マッチ拒否 / Read 必須
- `tools/read.rs` — offset / limit
- `tools/todo_write.rs` — replace / clear / multi-in_progress 拒否 / 引数不正
- `tools/registry.rs` — 動的名登録 / built-in 既定 14 ツール
- `tools/background.rs` / `tools/bash.rs` — BG ストアの増分読み出し・上限 append・kill 合図 / Bash の run_in_background → Monitor / KillShell 一周
- `memory/mod.rs` — LODAN.md/CLAUDE.md 探索・優先順・外内連結・空ファイル除外・上限の文字境界打ち切り
- `permission.rs` — auto_approve / always-tool / always-command の判定
- `repl.rs` — slash command 判定（絶対パス始まりは LLM に流す）
- `mcp/config.rs` / `mcp/protocol.rs` / `mcp/transport.rs` / `mcp/client.rs` / `mcp/tool.rs` / `mcp/prompt.rs` / `mcp/resource.rs` / `mcp/roots.rs` / `mcp/sampling.rs` — `.mcp.json` パース、JSON-RPC + MCP 型、stdio/HTTP transport、transport 非依存クライアント、tool / prompt / resource の namespacing、roots 提供、sampling (server→client LLM 補完) の opt-in 橋渡し
- `tests/e2e_mock.rs` — 6 ツールを順に走らせるエンドツーエンドのモック試験
- `tests/e2e_mcp.rs` — mock MCP サーバとの handshake + tools/list + tools/call 一周

## ライセンス

MIT OR Apache-2.0
