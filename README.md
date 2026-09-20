# lodan

ローカル LLM で動くコーディングエージェント CLI。Anthropic の [Claude Code](https://github.com/anthropics/claude-code) の発想（ツール駆動の対話ループ + 破壊的操作の事前承認 + 簡潔な system prompt）を Rust に最小移植したものです。

## 特徴

- **ランタイム非依存**: OpenAI 互換の Chat Completions + tool calling を話せる任意のサーバーに接続可能（Ollama / llama.cpp `--jinja` / vLLM / LM Studio など）
- **マルチプロバイダ**: ローカル LLM / Sakana AI (`fugu` / `fugu-ultra`) / さくらのAI Engine (`gpt-oss-120b` ほか) / Moonshot AI (`kimi-k3` ほか) を環境変数で随時切り替え
- **MCP クライアント (stdio / HTTP + tools / prompts / resources)**: `.mcp.json` を CWD に置くと MCP サーバ（ローカル stdio / リモート Streamable HTTP）へ接続し、公開 tools を取り込み、prompts は `/mcp__<server>__<prompt>`、resources は `mcp__<server>__read_resource` で扱える
- **ストリーミング**: SSE でアシスタント本文をリアルタイム表示
- **コアツール**: `Read` / `Write` / `Edit` / `Bash`（`run_in_background` で detached 実行も可） / `Grep` / `Glob` / `TodoWrite` / `MultiEdit` / `NotebookEdit`（.ipynb セル編集） / `WebFetch`（http(s) GET → テキスト化） / `WebSearch`（Brave Search API） / `AskUserQuestion`（選択式の質問） / `Monitor`（バックグラウンドプロセスの増分出力・状態取得） / `KillShell`（バックグラウンドプロセスの終了） / `Task`（調査用サブエージェント）
- **パーミッションゲート**: 破壊的ツール（Write / Edit / Bash / MCP 全般）は実行前にユーザー確認 (`y / n / a / e`)
  - `WebFetch` は read-only な GET なので**非破壊**（ゲートを経ない）。⚠️ ただしフェッチ先 URL はモデルが決めるため、内部ネットワーク到達 (SSRF) やクエリ経由の情報送出があり得る。http/https のみ許可・タイムアウト・サイズ上限を課し、リダイレクトも各ホップを http/https に限定して最大 5 ホップに制限する。**ただしリダイレクト先の内部ホスト到達まではブロックしない**ため、実行環境を信頼する前提（hooks / `.mcp.json` と同じ）で使うこと
  - `WebSearch` も read-only（非破壊）。env `BRAVE_API_KEY` が要り、未設定ならエラーを返す。クエリは外部 (Brave) へ送られるため、上と同じ信頼前提で使うこと。エンドポイントは env `BRAVE_SEARCH_API_URL` で差し替え可能だが（テスト用）、こちらも http/https のみ許可する
- **Bash のサンドボックス**: `[sandbox] mode = "workspace-write"` で、Bash が起動するプロセスの書き込み先（と任意でネットワーク）を OS の仕組みで制限（macOS seatbelt / Linux bwrap、後述）
- **gitignore-aware 検索**: ripgrep の内部クレート (`ignore` + `grep-searcher` + `grep-regex`) を直接利用
- **hooks**: `SessionStart` / `SessionEnd` / `UserPromptSubmit` / `PreToolUse` / `PermissionRequest` / `PostToolUse` / `PostToolUseFailure` / `PreCompact` / `PostCompact` / `SubagentStart` / `SubagentStop` / `Notification` / `Stop` で外部コマンドを発火し、終了コードと stdout の JSON（Claude Code 互換）でツール実行の可否・入力の書き換え・モデルへの追加文脈・ターン停止を制御（後述）
- **ユーザー定義 slash コマンド**: `.lodan/commands/*.md` をプロンプトテンプレートとして読み込み、`/name 引数` で展開（後述）
- **サブエージェント (`Task`)**: 読み取り専用ツールで調査タスクを子エージェントに委譲（後述）
- **skills**: `.lodan/skills/<name>/SKILL.md` を読み込み、`Skill` ツールとしてモデルへ公開（後述）
- **プロジェクトメモリ**: cwd 階層の `LODAN.md`（無ければ `CLAUDE.md`）と `~/.lodan/LODAN.md` を読み、system prompt へ注入（後述）

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

## クイックスタート (さくらのAI Engine)

さくらのAI Engine も OpenAI 互換なので `--provider sakura` で切り替わる。API キーは `.env` または `SAKURA_API_KEY` から拾われる。

```bash
echo 'SAKURA_API_KEY=...' >> .env

# 既定モデル
cargo run --release -- --provider sakura
# モデル指定 (利用可能な一覧は GET /v1/models)
cargo run --release -- --provider sakura --model preview/Kimi-K2.7-Code
```

tool calling は `gpt-oss-120b` / `preview/Kimi-K2.7-Code` / `preview/Qwen3.6-35B-A3B` で動作確認済み。
`llm-jp-3.1-8x13b-instruct4` はサーバ側が auto tool choice 無効のため、lodan からは利用できない
(`"auto" tool choice requires --enable-auto-tool-choice` が 400 で返る)。

## クイックスタート (Moonshot AI / Kimi)

Moonshot AI の公式 API も OpenAI 互換なので `--provider kimi` で切り替わる。既定モデルは `kimi-k3`。API キーは `.env` または `KIMI_API_KEY` から拾われる。

```bash
echo 'KIMI_API_KEY=...' >> .env

cargo run --release -- --provider kimi
```

- K3 は常に思考モードで 1 応答が長いため、既定の `timeout_secs` は 600
- K3 は `temperature` が 1 固定で、それ以外を送ると 400 になる。`kimi-k3*` に 1 以外の `temperature` (設定 / `--temperature` / `LODAN_TEMPERATURE`) が指定された場合、lodan は警告を出してその値を送らない

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

### 小型ローカルモデル向けのロバスト化

小型モデルはツール呼び出しの XML/JSON を崩しがち(サーバ側でパース不能 → ただのテキストとして届く)なため、lodan は次の 3 つの対策を内蔵している:

- **温度制御**: provider 設定の `temperature`(既定は未送信 = サーバ既定)。整形の破綻・綴りブレ・実行ごとの分散を抑えるには `0.1`〜`0.2` を推奨。
- **推論の深さ**(provider 設定の `reasoning_effort`、`--reasoning-effort`、`LODAN_REASONING_EFFORT`。既定は未送信 = サーバ既定): thinking 系のモデルは、推論の深さで 1 応答の所要時間が大きく変わる(kimi-k3 のサーバ既定は `max`)。値は**検査せずそのまま** `reasoning_effort` として送る — 受け付ける語彙はサーバとモデルごとに違うため(`low` / `medium` / `high` が共通、Ollama は `none` で thinking を切れる、`max` / `minimal` を持つサーバもある)。未対応の値はサーバが 400 で返す。`reasoning_effort` を読まないサーバ固有の切り方は `extra_body` で渡す:

  ```toml
  [llm.local]
  reasoning_effort = "low"
  # vLLM / llama.cpp の Qwen3 系: テンプレート側の thinking を切る
  [llm.local.extra_body]
  chat_template_kwargs = { enable_thinking = false }
  ```

  `extra_body` のキーはリクエスト body のトップレベルにそのまま足される。lodan 自身が組み立てるキー(`model` / `messages` / `tools` / `tool_choice` / `max_tokens` / `temperature` / `reasoning_effort` / `stream` / `stream_options`)を書くと、そのプロバイダは使えない（有効な provider なら起動時エラー。fallback provider なら警告を出して fallback 無しで続行する — fallback の他の設定不備と同じ扱い）。設定ファイルのレイヤー間では、他のテーブルと同じく**キー単位で重なる**（後段が同じキーを上書きし、書かなかったキーは残る）。`lodan config` は `extra_body` の中身をそのまま表示するので、秘密の値は置かないこと（`api_key` のマスクと合わせて #88 で扱う）。どちらも未設定なら、リクエスト body は 1 バイトも変わらない。`--log-jsonl` の `run_start` に `reasoning_effort` が残る。
- **壊れツールコールの再要求**: tool_calls が空なのに応答テキストへ呼び出しの痕跡(`<function=`、`call:Name{…}`、`<|tool_call` 等)が漏れている場合、「正しい tool call として再発行せよ」と自動で注入してターンを継続する(1 ターン 2 回まで)。
- **重複呼び出しの抑止**: 直前と完全同一(名前 + 引数)の **read-only** 呼び出しは実行せず「結果は不変。別の行動を」と返す(同一ファイルを延々 Read するループ対策)。Bash 再実行など破壊系の正当な繰り返しは対象外。
- **ツールプロファイル**(`[agent] tool_profile`、`--tool-profile`、`LODAN_TOOL_PROFILE`): ツール定義は**毎リクエスト全量が送られる**ので、小型モデルでは固定費がそのまま所要時間になる(ラダーのベースラインで 1.8k〜2.4k tok/呼び出し)。`core` は Read / Write / Edit / Bash / Grep / Glob の 6 個だけを見せ、定義の JSON は 6,643 → 2,437 バイト(-63%)。`readonly` は破壊的でないツールだけ。`tools = [...]`(`--tools Read,Grep,...`)で明示リストも指定できる。隠したツールは登録に残るので、モデルが名前を覚えていて呼んできても実行はされず「このプロファイルでは無効。使えるのは …」と返る(`--yes` でも通らない)。Task / Skill / MCP のツールも `core` では隠れる点に注意。`agent.tools` は設定ファイルのレイヤー間で連結されず後勝ち。実行時の `--tool-profile` は設定ファイルの `tools = [...]` より優先される(リストを残すとプロファイル指定が黙って無視されるため)。リストがどのツールにも一致しなければ、LLM を呼ぶ前にエラーで止まる。`Task` の内側の調査エージェントは常に Read / Grep / Glob の 3 個を使い、プロファイルの影響を受けない。`readonly` は WebFetch / WebSearch を含むので、権限の境界としては使わないこと(それは承認ゲートの役割)。起動時の `tools` イベント(`--log-jsonl` / `stream-json`)に、見せているツールと定義のバイト数が残る
- **終了前自己検証ナッジ**(`[agent] finish_nudge = true`、既定 false): ターンが終わろうとする最初の応答で 1 回だけ、ツール未使用なら「計画を述べ直さず今実行せよ」、使用済みなら「元の依頼を読み直し全要件の実装・検証を確認せよ」と促して継続させる。「計画だけ述べて終了」「長い自己編集中の要件脱落」対策(#63)。良行儀なモデルには余計なラウンドトリップになるため opt-in。

## 設定

階層: 既定値 ← `~/.config/lodan/config.toml` ← `$CWD/.lodan/config.toml` ← `$CWD/.lodan/config.local.toml` (個人用・コミットしない) ← `--config <path>` ← `$CWD/.env` ← 環境変数 ← CLI フラグ

ユーザ設定の場所は OS の流儀に従う (`directories` crate): Linux は `~/.config/lodan/config.toml`、**macOS は `~/Library/Application Support/lodan/config.toml`**。以下 `~/.config/lodan/` と書いている箇所は macOS では後者に読み替えること。

**LLM API の再試行**: 接続エラーと 408 / 429 / 5xx は `max_retries` 回まで自動で送り直す (全 provider 共通、`[llm.<provider>]` ごとに設定)。400 / 401 のような恒久的な失敗と、`timeout_secs` を使い切ったタイムアウトは再試行しない。ストリーミングは**本文を 1 文字も表示していない断だけ**を送り直す (表示後にやり直すと同じ文が二重に出るため)。再試行は stderr の警告と、`--log-jsonl` の `api_retry` イベントに残る。待ち時間中の Ctrl-C は即座に効く。

**fallback provider**: `[llm] fallback = "sakura"`(`--fallback-provider` / `LODAN_FALLBACK_PROVIDER`)を設定すると、primary が**一時的に**使えないとき — 再試行を使い切った接続エラー / 408 / 429 / 5xx、本文を 1 文字も出していないストリーム断、タイムアウト — に、その呼び出しを fallback の provider へ(**fallback 側の設定のモデルで**)投げ直す。400 / 401 のような設定ミスと、本文を表示した後の断(二重表示になる)は対象外。一度 fallback が成功したら、そのプロセスの間は fallback を使い続ける(primary が落ちている間、呼び出しのたびにバックオフを払わないため)。切替は stderr の警告と `provider_fallback` イベントに残る。fallback 側の API キーが無いなどで組めない場合は、警告して fallback なしで続行する。`--api-key` / `--model` / `--base-url` は primary だけに効くので、fallback 側のキーは設定ファイルの `[llm.<fallback>] api_key` か、その provider の env(`SAKANA_API_KEY` / `SAKURA_API_KEY` / `KIMI_API_KEY`)で渡すこと。切替後も MCP sampling の応答に載るモデル名と system prompt 内のモデル名は primary のまま(表示上の差異で、実際の呼び出しは fallback のモデルで行われる)。注意: 自動圧縮のしきい値と `/cost` は primary の `context_window` を基準にしたままなので、fallback の窓が小さい場合は `[llm.<fallback>] context_window` の小さい方に primary 側を合わせておくこと。

設定ファイルは**フィールド単位で重なる**。後段のファイルは自分が書いたキーだけを上書きし、書かなかったキーは前段の値が残る (プロジェクト側に `[agent] max_iterations = 40` だけ書いても、ユーザ設定の provider は消えない)。`[[hooks]]` だけは上書きではなく**連結**で、ユーザ設定 → プロジェクト設定 → `--config` の順に全て発火する。

`lodan config` は合成後の設定を表示する。`lodan config --show-origin` を付けると、各キーを最後に決めたもの (設定ファイルのパス、または `env or CLI flag`) も出る。載らないキーは既定値。

```toml
# ~/.config/lodan/config.toml
[llm]
provider = "local"   # "local" / "sakana" / "sakura" / "kimi"

[llm.local]
base_url       = "http://localhost:11434/v1"
model          = "qwen2.5-coder:7b"
api_key        = ""
timeout_secs   = 120
context_window = 32768   # モデルの文脈窓 (トークン)。自動圧縮のしきい値計算に使う。0 で自動圧縮無効
max_retries    = 3       # 一時的な失敗 (接続エラー / 408 / 429 / 5xx / 出力前のストリーム断) の再試行回数。0 で無効
retry_base_ms  = 500     # 再試行待ちの起点。試行ごとに倍 (上限 30s)。サーバの Retry-After があればそちらを優先
stream_idle_timeout_secs = 0   # ストリームがこの秒数黙ったら断とみなす。0 (既定) は無効
# temperature  = 0.2     # 未設定ならリクエストに含めない (サーバ既定)。小型モデルは 0.1-0.2 推奨

[llm.sakana]
base_url       = "https://api.sakana.ai/v1"
model          = "fugu"      # または "fugu-ultra"
api_key        = ""           # 空なら SAKANA_API_KEY env を使う
timeout_secs   = 120
context_window = 32768

[llm.sakura]
base_url       = "https://api.ai.sakura.ad.jp/v1"
model          = "gpt-oss-120b"
api_key        = ""           # 空なら SAKURA_API_KEY env を使う
timeout_secs   = 120
context_window = 32768

[llm.kimi]
base_url       = "https://api.moonshot.ai/v1"
model          = "kimi-k3"
api_key        = ""           # 空なら KIMI_API_KEY env を使う
timeout_secs   = 600
context_window = 32768

[agent]
max_iterations  = 25
auto_approve    = false
finish_nudge    = false   # 終了前自己検証ナッジ (#63)
malformed_retry = true    # テキストに漏れたツールコールの再要求 (#61)
dup_suppress    = true    # 直前と同一の read-only 呼び出しの抑止 (#61)
parallel_tools  = true    # 連続する並列可能なツール呼び出し (Read / Grep / Glob / WebFetch / WebSearch / Task) を同時に実行
tool_profile    = "full"  # モデルに見せるツール: full / core (6 個) / readonly
tools           = []      # 明示リスト。空でなければ tool_profile より優先 (例: ["Read", "Grep", "TodoWrite"])

[tools.bash]
timeout_secs = 30
```

`--base-url` / `--model` / `--api-key` および対応する `LODAN_*` env は **現在 active な provider** の設定を上書きする (provider を `--provider` で切り替えれば反対側を触らずに済む)。

環境変数:
- `LODAN_PROVIDER` (`local` | `sakana` | `sakura` | `kimi`)
- `LODAN_FALLBACK_PROVIDER` (同上。未設定なら fallback しない)
- `LODAN_BASE_URL` / `LODAN_MODEL` / `LODAN_API_KEY` / `LODAN_AUTO_APPROVE`
- `LODAN_REASONING_EFFORT` (推論の深さ。値はそのままサーバへ渡す)
- `LODAN_TEMPERATURE` / `LODAN_FINISH_NUDGE` / `LODAN_MALFORMED_RETRY` / `LODAN_DUP_SUPPRESS` (真偽値は `true`/`false`/`1`/`0`/`yes`/`no`)
- `LODAN_TOOL_PROFILE` / `LODAN_TOOLS` (カンマ区切り)
- `LODAN_PARALLEL_TOOLS` (真偽値。既定 true)
- `LODAN_MAX_REQUESTS` / `LODAN_MAX_TOKENS` (LLM リクエスト数 / 合計トークン数の上限。既定は無制限)
- `LODAN_TRUST` (真偽値。この実行に限ってプロジェクトの設定を信頼する)
- `LODAN_PERMISSION_MODE` (`default` | `accept-edits` | `plan` | `dont-ask` | `bypass`)
- `LODAN_SANDBOX` (`off` | `workspace-write` | `read-only`) / `LODAN_SANDBOX_NETWORK` (真偽値。既定 true)
- `LODAN_LOG_JSONL` (実行トレース JSONL の出力先)
- `SAKANA_API_KEY` (provider=sakana のときに `api_key` が空ならフォールバック)
- `SAKURA_API_KEY` (provider=sakura のときに `api_key` が空ならフォールバック)
- `KIMI_API_KEY` (provider=kimi のときに `api_key` が空ならフォールバック)

CLI フラグ（ヘッドレス実行の `-p` / `--output-format` / `--stdin` は[後述](#ヘッドレス実行-p)）: `--provider` / `--fallback-provider <provider>` / `--base-url` / `--model` / `--api-key` / `--config <path>` / `--yes` / `--trust[=<bool>]` / `--temperature <f32>` / `--reasoning-effort <LEVEL>` / `--log-jsonl <path>` / `--finish-nudge[=<bool>]` / `--malformed-retry[=<bool>]` / `--dup-suppress[=<bool>]` / `--parallel-tools[=<bool>]` / `--max-requests <N>` / `--max-tokens <N>` / `--permission-mode <mode>` / `--allowed-tools <RULE>` / `--disallowed-tools <RULE>` / `--tool-profile <full|core|readonly>` / `--tools <NAME,...>` / `--sandbox <off|workspace-write|read-only>` / `--sandbox-network[=<bool>]`

真偽値フラグは値なしで `true`。明示するときは **`=` でつなぐ** (`--dup-suppress=false`)。空白区切りの次の語は値として食わないので、`lodan --finish-nudge repl` はサブコマンドとして解釈される。設定ファイルで有効にした緩和策を評価実行から切る (ablation) ための形。

`$CWD/.env` (**cwd のものだけ**。親ディレクトリの `.env` は探さない) は、そのディレクトリを[信頼している](#workspace-trust--信頼していないディレクトリの設定は読まない)ときだけ自動ロードする (dotenvy)。コミット対象外 (`.gitignore` 済)。

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

cwd の中のファイルや単純な Bash コマンドでは、5 つ目の選択肢 `(p) always allow … in this project` も出る (この例は cwd の外の `/tmp` なので出ない。詳細は[権限ルールとモード](#権限ルールとモード))。

承認プロンプトは **Enter だけで yes** だが、それは stdin が端末のときに限る。プロンプトをパイプで渡した実行 (`echo "..." | lodan`) では、空行は答えとして扱わず、誰も答えないまま入力が尽きた (EOF) 承認は `(no input — denied)` で**拒否**する。無人実行で破壊的ツールを通したいときは `--yes` を明示すること。

組み込み slash: `/exit` `/quit` `/help` `/clear` `/tools` `/compact` `/cost` `/goal` `/loop` `/plan` `/accept` `/undo`（ユーザー定義コマンドは後述）。`/help` は組み込み・ユーザー定義・MCP prompt を説明付きで、`/tools` は各ツールを説明付きで一覧する。

**端末装飾**: ツール出力・エラー・承認プロンプトを ANSI で色分けし、LLM 応答待ちは `…thinking` インジケータを表示する。stdout が tty でない（パイプ／リダイレクト）とき、または `NO_COLOR` 環境変数が設定されているときは着色・インジケータを一切出さない。

**ツール出力の表示**: LLM へはツール出力を無加工で渡しつつ、端末表示はツール別に要約する — `Read` はヘッダ行のみ（ファイル内容を端末へエコーしない）、`Bash` は截断されても exit ステータスを保持。長い出力は先頭 8 行 / 600 字で行単位に切り、`… (+K more lines, M bytes total)` と省略量を明示する。

**補完（Tab）**: 行頭の `/…` は slash コマンド名（組み込み + ユーザ定義 + MCP prompt）を、それ以外の語はファイルパスを Tab で補完できる。

**複数行入力**: 行を <code>```</code> で始めると、閉じ <code>```</code> の行まで Enter で改行しながら入力を続けられる（送信時にフェンスは外され中身だけがプロンプトになる。<code>```python</code> のような言語タグも可）。また行末に `\` を置くと次の行へ継続できる（送信時に `\` を除いて改行連結）。

**承認プロンプトのプレビュー**: 破壊的ツールの承認時、何が起きるかをプロンプト内に表示する — `Edit` / `MultiEdit` は `- old`（赤）→ `+ new`（緑）の差分風プレビュー（1 ブロック 8 行・MultiEdit は先頭 3 edit まで）、`Write` は書き込む内容の先頭 8 行。パスは cwd 配下なら相対表示。

**実行中ターンの中断（Ctrl-C）**: LLM ストリーミング・ツール実行の途中でも Ctrl-C で現在のターン（`/goal` の自律ループ含む）をキャンセルしてプロンプトへ戻れる。中断後は履歴の整合性を自動修復する — ツール実行途中なら未応答の tool_call に `[interrupted by user before completion]` を補填し、応答前なら中断マーカーの assistant メッセージを補う（strict な user/assistant 交互を要求するローカルモデル対策）。foreground の Bash 子プロセスは中断時に kill される（バックグラウンド実行 `run_in_background` のものは残る — `KillShell` で止める）。承認プロンプトの応答待ち中は割り込めない（`y`/`n` を入力してから反映される）。プロンプト入力中の Ctrl-C は従来どおり入力破棄のみ。

破壊的ツール承認:
- `y` 一度だけ許可
- `n` 拒否（LLM には "user denied execution" が返り、別アプローチを促せる）
- `a` セッション中はこのツールを常時許可
- `e` Bash の場合のみ、その完全一致コマンドを常時許可

## ヘッドレス実行（`-p`）

REPL を開かずに 1 ターンだけ実行して終了する。スクリプト・CI・評価ハーネス向け。

```bash
lodan -p "src/config.rs の役割を 3 行で"                  # 最終応答だけが stdout に出る
git diff | lodan -p "このdiffをレビューして" --stdin        # 指示 + stdin のデータ
echo "READMEを要約して" | lodan -p                         # 引数なし: stdin がプロンプト本体
lodan -p "テストを直して" --yes --output-format json       # 結果を JSON 1 行で
lodan -p "..." --output-format stream-json | jq -c .       # 進行をイベント列で
lodan -p "続きをやって" --resume last
```

- **stdout は契約**。人間向けの表示（ストリーム本文・ツール出力の要約・`session:` などの通知・エラー）は全て **stderr** に出る
  - モデルやツールが書いたエスケープ列 (カーソル移動・行消去) や不可視文字は、**画面に出すときだけ** `\u{1b}` のような見える形にする (下記「端末に出す文字列」)。stdout がパイプなら結果は無加工、端末に直接出すときだけ無害化する
  - `text`（既定）: 最終応答の本文だけ
  - `json`: `{"type":"result","is_error","exit_code","result","error","session_id","usage":{…}}` を 1 行
  - `stream-json`: `--log-jsonl` と**同じイベント列**（`src/runlog.rs` の表）を stdout に流し、最後に `result` イベント。`--log-jsonl` と併用すればファイルにも同じものが残る
- **終了コード**: `0` 成功 / `1` エラー（起動時の失敗を含む。`json` / `stream-json` ではこの場合も結果オブジェクトを出す）/ `2` 引数の誤り（clap。stdout は空）/ `3` 最終応答に至らず `max_iterations` を使い切った / `4` [予算](#予算)（`max_requests` / `max_total_tokens`）を使い切った / `130` SIGINT
- **承認**: 尋ねる相手がいないので、`--yes` が無ければ破壊的ツール（Write / Edit / Bash …）は**尋ねずに拒否**され、モデルには「非対話実行なので再試行するな」と返る。ハングしない。`AskUserQuestion` も同様に即エラーを返す
- **stdin**: プロンプト引数があるときは stdin を**読まない**。CI や親プロセスから継承した stdin は端末でなくても閉じられないことがあり、EOF 待ちで固まるため。引数に stdin を足したいときは `--stdin` を明示する（上限 10 MiB）
- slash コマンド（`/compact` など）は解釈しない。プロンプトはそのままモデルに渡る
- hooks（SessionStart / UserPromptSubmit / PreToolUse / PostToolUse / Stop / SessionEnd）、MCP、skills、プロジェクトメモリ、セッション保存は REPL と同じ

## 端末に出す文字列

アシスタントの本文・計画 (`ExitPlanMode`)・ツールの名前と出力・hook の理由・`Task` の説明・`AskUserQuestion` の質問と選択肢・`/goal` の評価器の所見・`/tools` と `/help` に出る MCP サーバ由来の名前と説明・`/undo` のパス・エラー文 (プロバイダの応答本文が入り込む)・承認プロンプトは、どれも lodan の外から来る文字列 (モデルが書いたもの、読んだファイルや Web ページ、コマンドの出力)。ANSI エスケープ (カーソル移動・行消去) や双方向テキストの上書き (U+202E)・ゼロ幅文字をそのまま端末へ流すと、画面を書き換えて「直前に何が起きたか」「これから承認するものは何か」を偽れる — 読ませたファイルや Web ページ経由のプロンプトインジェクションの出口になる。

lodan はこれらを**画面に出すときだけ** `\u{1b}` のような見える形にする。改行とタブはそのまま、`\r\n` は改行、単独の CR (行の上書き) は `\r` と表示する。副作用として、ツール出力に含まれる色 (SGR) や進捗バーの上書きも文字として見える。色だけを許可リストで通すことはしていない — `\x1b[` で始まる列を自前で解釈することになり、その解釈自体が攻撃面になるため。

**加工しないもの**: モデルへ返す tool 応答、履歴と transcript、runlog、`-p` の `json` / `stream-json`、stdout がパイプのときの `-p` の text 結果。

## workspace trust — 信頼していないディレクトリの設定は読まない

プロジェクトのファイルは、開いただけで効いてしまう。

| ファイル | できること |
|---|---|
| `.lodan/config.toml` / `.lodan/config.local.toml` | `[[hooks]]` は任意のコマンドを実行する。`[llm.*] base_url` を書き換えれば API キーを外へ送れる。`[permissions] mode = "bypass"` や `allow = ["Bash(*)"]` で承認を素通しにできる |
| `.env` | 環境変数で渡せる設定は全部ここから渡せる: `LODAN_BASE_URL` (API キーの送り先)、`LODAN_PERMISSION_MODE=bypass`、`LODAN_TRUST=1` (自分で自分を信頼) |
| `.mcp.json` | 任意のプロセスを起動する |
| `.lodan/commands` / `.lodan/skills` / `LODAN.md` / `CLAUDE.md` | モデルへの指示を差し込む (メモリは cwd の祖先からも読まれるので、祖先の `LODAN.md` / `CLAUDE.md` も対象) |

clone してきたリポジトリで `lodan` を起動するだけでこれらが効くのは危ないので、**信頼済みのディレクトリ (とその配下) でだけ読む**。

- **REPL**: 上のファイルがあって未信頼のとき、起動時に一覧を見せて尋ねる — `(y)` 信頼して記録 / `(o)` 今回だけ / `(n)` 読まずに起動。Enter だけ・EOF では信頼しない。上のファイルが 1 つも無いディレクトリでは何も尋ねない
- **`-p` / `lodan config` / 入力をパイプした REPL**: 尋ねない。未信頼なら**読まずに続行**し、何を読まなかったかを stderr に出す (`lodan: /path is not a trusted directory; ignoring .lodan/config.toml, .mcp.json. …`)
- `--trust` (`LODAN_TRUST=1`) はその実行に限って信頼する (記録しない)。CI や評価ハーネス向け。`--trust=false` で env を打ち消せる
- `lodan trust` で今のディレクトリを記録、`lodan trust --list` で一覧、`lodan trust --remove` で取り消し
- 記録はユーザの設定ディレクトリの `trusted.toml` (Linux: `~/.config/lodan/`、macOS: `~/Library/Application Support/lodan/`)。リポジトリ側からは書き換えられない。比較は symlink を解決したパスで行う
- 信頼の判断は **`.env` を読む前**の環境変数と引数だけで行う。リポジトリの `.env` に `LODAN_TRUST=1` と書いても、そのリポジトリは信頼されない
- 未信頼でも読むもの: ユーザ設定 (`~/.config/lodan/config.toml`)、`--config <path>` で明示したファイル、`~/.lodan/LODAN.md`。自分で置いたものだけ

**これは設定の読み込みの話で、実行の隔離ではない**。信頼していないリポジトリの中で Bash を承認すれば、そのコマンドは普通に走る。

## 権限ルールとモード

既定では、破壊的ツール (Write / Edit / Bash …) は実行前に尋ね、read-only のツールはそのまま通る。`[permissions]` でこれを宣言的に変えられる。

```toml
[permissions]
mode  = "default"          # default / accept-edits / plan / dont-ask / bypass
allow = ["Bash(git status)", "Bash(git diff *)", "Bash(cargo check*)", "Edit(src/**)", "WebFetch(domain:docs.rs)"]
ask   = ["Bash(cargo publish*)"]
deny  = ["Read(**/.env)", "Grep(**/.env)", "Glob(**/.env)", "Read(~/.ssh/**)", "Bash(git push --force*)"]
```

**評価順は deny → ask → allow → 既定**。

- **deny は何にでも勝つ**: read-only のツールにも (`Read(**/.env)`)、`--yes` / `bypass` にも効く。「基本は全部通すが、これだけは絶対に通さない」が書ける。拒否されるとモデルには `denied by permission rule …` と、回り道をするなという指示が返る
- **ask** は必ず尋ねる (allow より優先、read-only にも効く)。**allow** は尋ねずに通す。ただし `--yes` / `bypass` は ask も尋ねずに通す (尋ねないのが bypass の意味なので。止めたいものは deny に書く)
- ルールはユーザ設定 → プロジェクト設定 → `--config` の間で**連結**される。プロジェクト設定はユーザ設定の deny を消せない。**ただし広げることはできる**: プロジェクトの `.lodan/config.toml` は `allow = ["Bash(*)"]` を足すことも `mode = "bypass"` にすることもできる (だからプロジェクト設定は[信頼済みのディレクトリ](#workspace-trust--信頼していないディレクトリの設定は読まない)でしか読まない)。`--allowed-tools <RULE>` / `--disallowed-tools <RULE>` (繰り返し可) も足すだけで、置き換えない
- 解釈できないルールが 1 つでもあれば**起動時にエラー**。権限の設定を黙って読み飛ばさない

**承認プロンプトから保存する**: プロンプトの `(p) always allow … in this project` を選ぶと、その呼び出しの allow ルールが `$CWD/.lodan/config.local.toml` に追記され、次のセッションからは尋ねられない (選んだセッションでも以後は尋ねない)。

- 保存されるのは、プロンプトに表示された**そのルール**。広さはツールごとに違う:
  - Bash: **そのコマンドの完全一致** (`Bash(cargo test --lib)`)
  - ファイルのツール (Edit / Write / …): **そのファイルのあるディレクトリ以下** (`Edit(src/agent/**)`。cwd 直下のファイルならそのファイルだけ `Edit(./Cargo.toml)`)。cwd の外のパスは保存できない
  - WebFetch: そのホスト (`WebFetch(domain:docs.rs)`)
  - それ以外 (MCP ツールなど): そのツールの全呼び出し
- 選んだセッションでも、保存したのと同じ広さでしか通らない (`Edit(src/**)` を保存して Edit 全体が通る、にはならない)
- 保存しても意図どおりに効かない呼び出しには `(p)` を出さない: 複合コマンド・`$(…)`・リダイレクト (allow として決して一致しない)、`*` や括弧を含むコマンド (保存するとワイルドカードや構文として解釈され、意味が変わる)、**ask ルールに当たる呼び出し** (ask が優先されるので、保存しても毎回尋ねられる)、計画の承認 (`ExitPlanMode` — 毎回目を通すもの)、不可視の文字を含むもの (下記)
- プロンプトに出るコマンドやパスは**モデルが渡した文字列**。ANSI エスケープ・CR・双方向テキストの上書き (U+202E)・ゼロ幅文字などは `\u{1b}` のような見える形で表示する — そのまま端末へ流すと、行を書き換えて「承認しようとしているもの」を偽れるため
- 保存の前に、そのルールが読み直せて同じ呼び出しを allow することを確かめる (読めないルールを保存すると、次の起動が設定エラーで止まる)
- `config.local.toml` は**個人用**で、共有の `.lodan/config.toml` とは別のレイヤー (プロジェクト設定の後、`--config` の前)。`.gitignore` に入れること。lodan が書き戻すのでコメントは残らない。広げすぎたら、このファイルから行を消せばよい

**ルールの構文** (Claude Code の `permissions` と同じ `Tool` / `Tool(pattern)`):

| 例 | 意味 |
|---|---|
| `Bash` / `WebSearch` | そのツールの全呼び出し |
| `Bash(git status)` | 完全一致 |
| `Bash(git diff *)` | `*` は任意の文字列 |
| `Bash(npm run test:*)` | `npm run test` そのもの、または後に引数が続くもの (`npm run testing` には一致しない) |
| `Read(src/**)` / `Edit(*.md)` | パスの glob。相対パターンは cwd 基準。`/` の無いパターンはどの階層のファイル名にも一致 (gitignore と同じ)。対象: Read / Write / Edit / MultiEdit / NotebookEdit / Glob / Grep |
| `Write(/etc/**)` / `Read(~/.ssh/**)` | 絶対パス / ホーム基準 |
| `WebFetch(domain:docs.rs)` | ホスト名だけを書く (サブドメインを含む。大文字・末尾ドット・IDN は URL 側と同じ形に正規化される)。判定するのは**最初の URL** だけで、リダイレクト先は見ない |
| `mcp__github` / `mcp__github__create_issue` | その MCP サーバの全ツール / 1 つだけ (ツール名には英数字と `_` `-` `.` が使える) |

**Bash の複合コマンド**: `Bash(git *)` を allow していても `git status && rm -rf /` は通らない。コマンドを `&&` `||` `;` `|` `&` と改行で分割し、**全ての部分が allow に一致したときだけ**通す。`$(…)`・バッククォート・プロセス置換・リダイレクト (`>` `<`) を含むコマンドは中身を追い切れないので、allow には決して一致させず尋ねる。deny は逆に、コマンド全体か**いずれかの部分**が一致すれば効く。

**検索ツール (Grep / Glob)** は `path` 以下を丸ごと読む (`path` 省略時は cwd)。deny / ask は、**その検索が実際に触れるファイルの中に一致するものがあれば**効く: `deny = ["Grep(secrets/**)"]` は `path` 無しの Grep も止める。判定は Grep / Glob と同じ走査 (`.gitignore` を尊重、隠しファイルは含む) で行うので、`Grep(**/.env)` は `.env` のあるディレクトリの検索だけを止め、gitignore された `.env` は上の階層からの検索では読まれないので止めない (ignore されたディレクトリ自体を起点に指定した検索は中を読むので、止める)。確認は 5 万エントリ / 1 回の判定あたり合計 0.3 秒 (ルールが何個あっても) で打ち切り、確かめきれなかった範囲は通さない (モデルには「`path` を狭めてやり直せ」と返る。`$HOME` 全体のような検索がこれに当たる)。

**パス**: `src/../.env` のような `..` は畳んでから照合する。deny / ask は大文字小文字を無視する (macOS / Windows では `.GITHUB/x` への書き込みが `.github/x` に着地するため)。allow は綴りどおり。Unicode の正規化 (NFC と NFD) は揃えない: macOS の APFS では `café.key` の合成形と分解形が同じファイルを指すが、ルールは書かれた形としか一致しない。非 ASCII のファイル名を deny で守るなら、ディレクトリ単位 (`Write(keys/**)`) で書くこと。symlink は解決後のパスも見る — allow は「どちらの見え方でも一致」、deny は「どちらかが一致」を条件にするので、cwd の外を指す symlink で `Edit(src/**)` を満たすことはできない。解決は OS と同じく左から 1 要素ずつ行い、まだ存在しないファイルでも求まる: `src/link -> /outside` の下の**新しい**ファイル、行き先のまだ無い symlink (`src/x -> /outside/new.txt`)、symlink の後ろの `..` (`src/link/../evil.txt`) は、どれも字句上は `src/` の下に見えるが、着地点で判定されるので `Write(src/**)` を満たせず、着地点への deny は効く。同じ理由で、macOS の `/tmp` や `/var` (それぞれ `/private/tmp`・`/private/var` への symlink) を指す**絶対パスの allow** は一致しない (`Write(/tmp/**)` は字句上の見え方にしか、`Write(/private/tmp/**)` は解決後の見え方にしか一致せず、allow は両方を要求する)。deny はどちらの綴りでも効く。

**モード** (`[permissions] mode` / `--permission-mode` / `LODAN_PERMISSION_MODE`):

| mode | 挙動 |
|---|---|
| `default` | 破壊的ツールは尋ねる |
| `accept-edits` | ファイル編集 (Write / Edit / MultiEdit / NotebookEdit) は尋ねずに通す。Bash などは尋ねる |
| `plan` | plan モードで開始する |
| `dont-ask` | 尋ねない。尋ねるはずだった呼び出しは拒否する。無人実行で「allow に書いたものだけ通す」ときに使う (`-p` は常にこの挙動) |
| `bypass` | 尋ねずに全て通す。`--yes` と同じ。**deny ルールは効く** |

`Task` の子エージェントにも同じルールが効く (子は親の承認を通らずに Read / Grep / Glob を実行するので、ここで見ないと deny を「Task に読ませる」だけですり抜けられる)。子の中では尋ねられないため、ask に当たる呼び出しも拒否される。

**限界** — ルールは lodan のツール呼び出しの**文字列**を見ているだけで、OS レベルの隔離ではない:

- **Bash の deny は回り道に弱い**。`deny = ["Bash(rm *)"]` は `/bin/rm`・`command rm`・`\rm`・`"rm" -rf`・`sudo rm`・`env rm`・`xargs rm`・`sh -c 'rm …'`・`X=rm; $X …` を止めない。Bash の deny は事故の防止であって、敵対的なモデルへの防壁ではない。確実に止めたいなら Bash 自体を尋ねる対象のままにして、**allow を狭く**書く
- **広い allow は実質 `Bash(*)`**。`Bash(git *)` は `git -c core.pager='…' log` や `git -c alias.x='!…' x`、`git config --global alias.…` を通すので、任意コマンド実行と同じ。`Bash(git status)` / `Bash(git diff *)` / `Bash(git log *)` のように、サブコマンドまで書くこと。`cargo *` / `npm *` / `make *` も同様 (ビルドスクリプトが何でも実行する)
- **ルールはツールごと**。`deny = ["Read(**/.env)"]` は Read を止めるだけで、`Grep` が一致行を返すことも、`Bash(cat .env)` も止めない。秘密を守るなら `Read` / `Grep` / `Glob` の 3 つに書き、Bash は尋ねる対象のままにする

- allow した Bash コマンドが内部で何をするか (`cargo test` がテストコードから何を実行するか) までは制御できない。サンドボックスは #75

## Bash のサンドボックス

承認ゲートも権限ルールも、見ているのはコマンドの**文字列**です。承認した `cargo test` や `make` が内側で何を書き換えるかまでは分かりません。`[sandbox]` を有効にすると、Bash ツールが起動するプロセス（とその子孫）を OS の仕組みで閉じ込めます。

```toml
[sandbox]
mode = "workspace-write"   # "off"（既定）| "workspace-write" | "read-only"
network = false            # 既定 true。false でサンドボックス内からの通信を遮断（loopback も）
```

| mode | 書ける場所 |
| --- | --- |
| `off` | 制限なし（既定。これまでと同じ） |
| `workspace-write` | 作業ディレクトリ以下と一時ディレクトリ（`/tmp`、`$TMPDIR`） |
| `read-only` | どこにも書けない（`/dev/null` などを除く） |

- **macOS** は `sandbox-exec`（seatbelt）、**Linux** は [`bwrap`（bubblewrap）](https://github.com/containers/bubblewrap)を使います。`mode` が `off` 以外なのに道具が無い・起動できない環境では、**素通しにせずコマンドを実行しません**（サンドボックスを頼んだのに黙って外で走るのが最悪なので）。Windows は未対応です。
- `workspace-write` でも、作業ディレクトリの中の次の場所は書けません: `.lodan/`、`.env`、`.mcp.json`、`.git/hooks/`、`.git/config`、`.git/modules/`（サブモジュールの git ディレクトリ）。ここに書けると、サンドボックスの中のコマンドが「次にサンドボックスの外で動くもの」（lodan の設定や hooks、MCP サーバ、git の hooks）を仕込めてしまうためです。`.git` という名前そのものも固定します（`.git` ごと rename して hooks 入りの別物に差し替える回り道を塞ぐため）。`git add` / `commit` / `checkout -b` などは通常どおり動きますが、**`git init` とサブモジュール内の git 操作はサンドボックスの外で**行ってください。linked worktree（`git worktree add`）やサブモジュールの中では `.git` が「本体の場所を書いたファイル」で、これも書き換えられません。その場合 git の本体（index や objects）は作業ディレクトリの外にあるので、**`git add` / `commit` もサンドボックスの外で**行うことになります。
- **Linux（bwrap）の制約**: bwrap が守れるのは起動時に存在するパスだけです。そのため lodan は空の `.lodan/`（と、git リポジトリなら `.git/hooks/`・`.git/modules/`）を先に作ります。まだ存在しない**ファイル**（`.env`・`.mcp.json`・`.git/config`）と、リポジトリでないディレクトリでの `.git` の作成は止められません。代わりに、コマンドの実行中にそれらが現れたら `[sandbox] WARNING` を端末とツール結果の両方に出します（フォアグラウンド実行のみ）。検出はコマンドの終了直後に 1 回行うだけなので、**バックグラウンドに残したプロセスが後から書いたものは拾えません** — Linux では「止められないものを、できる範囲で知らせる」に留まります。macOS は作成そのものを止めます。
- `$TMPDIR` が `/`・ホーム・作業ディレクトリを含む場所を指している場合は無視します（「一時ディレクトリは書ける」が「どこでも書ける」になってしまうため）。
- **サンドボックスを切れるのは設定を書ける人**です: 信頼済みディレクトリ（[workspace trust](#workspace-trust--信頼していないディレクトリの設定は読まない)）のプロジェクト設定や `.env` は、ユーザ設定の `[sandbox]` を上書きできます（他の設定と同じ後勝ち）。信頼していないディレクトリのものは読まれません。確実に効かせたい実行では `--sandbox <mode>` を付けてください（フラグが最優先）。
- 読み取りは制限しません（制限すると普通のビルドが動きません）。秘密のファイルを読ませたくなければ[権限ルール](#権限ルールとモード)の deny と併用してください。`network = true` のままだと、読めたものを外へ送れる点にも注意してください。
- 対象は **Bash ツールだけ**（フォアグラウンドと `run_in_background` の両方）です。Write / Edit などの組み込みツール、hooks、MCP サーバは lodan 本体と同じ権限で動きます。こちらは承認ゲートと権限ルールの持ち場です。
- サンドボックスに止められたらしい失敗（`Operation not permitted` など）には、結果に `[sandbox]` の注記が付きます（フォアグラウンド実行のみ。`run_in_background` の出力には付きません）。モデルが同じコマンドを繰り返したり抜け道を探したりせず、ユーザーに伝えるようにするためです。
- 承認を減らしたいときは `--permission-mode dont-ask` などと組み合わせられますが、サンドボックスは承認を**置き換えません**。モードやルールの判定はこれまでどおり先に行われます。

## ツール呼び出しの並列実行

モデルが 1 つの応答で複数のツールを呼んだとき、**並列可能なツールが 2 つ以上連続する区間**は同時に実行する(`[agent] parallel_tools`、既定 true。`--parallel-tools=false` / `LODAN_PARALLEL_TOOLS` で無効化)。API 級のモデルは 1 応答で Read や Grep を何本も出すので、待ち時間が直列に積まれなくなる。独立した調査を複数の `Task` に分けた場合も同時に走る。

- 並列にするのは、ツール自身が `parallel_safe()` を宣言したものだけ: **Read / Grep / Glob / WebFetch / WebSearch / Task**。read-only でも、共有状態を書く TodoWrite、stdin を取り合う AskUserQuestion、読み取り位置を持つ Monitor は対象外。MCP ツールと破壊的ツール(Write / Edit / Bash …)は常に 1 つずつ、承認も 1 つずつ
- 破壊的ツールや並列不可のツールが挟まると、そこで区間が切れる: `[Read, Read, Edit, Read]` は最初の 2 つだけが同時
- **結果の順序は変わらない**。表示・PostToolUse hook・runlog・モデルへ返す tool 応答は、逐次実行のときと同じ呼び出し順
- PreToolUse hook は区間内でも順番どおり 1 つずつ通り、ブロックされた呼び出しは実行されない。ただし hook の**噛み合い方は変わる**: 逐次では `pre1 → 実行1 → post1 → pre2 → …` だったものが、区間内では `pre1 → pre2 → (実行1 ∥ 実行2) → post1 → post2` になる。「1 つ目の PostToolUse が終わってから 2 つ目の PreToolUse」を前提にした hook を使っているなら `parallel_tools = false` にすること
- 先行実行するのは、**尋ねずに「通す」と決まる呼び出しだけ**。deny ルールに当たる Read は実行されず、ask ルールに当たるものは 1 つずつ尋ねる
- 同時に走らせるのは **4 個まで** (5 連続なら 4 個を同時に、残り 1 個は逐次で。`parallel` は実際に同時実行したものだけ true)。それより長い区間は 4 個ずつの組に分けて順に実行する。`Task` は承認を通らないので、上限が無いとモデルが並べた数だけ子エージェントの LLM ループが同時に走り、トークン消費が黙って膨らむ
- 直前と同一の呼び出し(重複抑止の対象)と、`ExitPlanMode` より後ろの呼び出し(承認されるとスキップされる決まり)は先行実行しない
- `tool_result` イベントの `parallel` で、同時実行されたかが分かる。`ms` は実際の実行時間

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
├── trust.rs             # workspace trust (未信頼ディレクトリのプロジェクト設定を読まない)
├── permission.rs        # 承認ゲート (4 択プロンプト / モード / ルールの適用)
├── permission_rules.rs  # `[permissions]` の allow / deny / ask ルール (構文・Bash の複合コマンド分割・パス照合)
├── agent/
│   ├── messages.rs      # OpenAI Chat スキーマ準拠の Message / ToolCall
│   ├── loop.rs          # run_turn(): chat_stream → tool dispatch → 反復
│   └── subagent.rs      # Task ツール (read-only サブエージェント)
├── llm/
│   ├── mod.rs           # trait LlmClient + provider 分岐 (build_client)
│   ├── openai.rs        # ローカル/汎用 OpenAI 互換クライアント
│   ├── kimi.rs          # Moonshot AI (Kimi) adapter (内部で OpenAiClient に委譲)
│   ├── sakana.rs        # Sakana AI (Fugu) adapter (内部で OpenAiClient に委譲)
│   └── sakura.rs        # さくらのAI Engine adapter (内部で OpenAiClient に委譲)
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
├── mcp/                                                  # MCP クライアント (stdio / HTTP、tools / prompts / resources / roots / sampling)
├── skills/                                               # SKILL.md のロードと Skill ツール
├── memory/                                               # LODAN.md / CLAUDE.md 階層ロード
├── goal/   loop_cmd/                                     # /goal・/loop
├── undo.rs                                               # /undo (ターン単位のファイル変更ロールバック)
├── runtime.rs                                            # REPL / ヘッドレス共通の起動処理 (LLM・ツール登録・セッション)
├── headless.rs                                           # `-p` 非対話実行 (text / json / stream-json、終了コード)
├── runlog.rs                                             # 実行トレース JSONL (--log-jsonl)
├── term.rs                                               # ANSI 色・tty 判定
```

## hooks

`config.toml` の `[[hooks]]` 配列で、ライフサイクルイベント発火時に外部コマンドを実行できます。コマンドはイベントの JSON ペイロードを stdin で受け取り、**終了コード**と **stdout の JSON** で制御します。プロトコルは Claude Code の hooks に合わせてあり、Claude Code 用に書いた command hook のスクリプトはそのまま動きます（公式ドキュメントの `block-rm.sh` を無改変で動かす e2e テストがあります）。

```toml
[[hooks]]
id = "guard"             # 省略可。後段のレイヤーから置き換え・無効化するための名前
event = "PreToolUse"     # 下の「ペイロード」にあるイベント名
matcher = "Edit|Write"   # 省略可。下の「matcher」を参照
command = "./scripts/guard.sh"
timeout_secs = 30        # 省略可（既定 30）
```

### 終了コード

| 終了コード | 意味 |
| --- | --- |
| `0` | 続行。stdout が JSON オブジェクトなら判断として読む（下記） |
| `2` | **ブロック**。理由は stderr。JSON で `allow` と言っていても止まる |
| その他の非 0 | hook 自身の失敗。**警告を出して続行**（ブロックしない） |

> ⚠️ **v0.1 からの破壊的変更**
> - 以前は「非 0 は全てブロック」でした。`exit 1` で止めていた hook は `exit 2` に直すか、設定のトップレベルに `hooks_compat = "v1"` を書いて旧挙動（非 0 は全てブロック・stdout の JSON は読まない）に戻してください。
> - **SessionStart の `matcher` が効くようになりました**（以前は無視）。照合する相手は `startup` / `resume` です。SessionStart の hook に `matcher = "Bash"` のような値が残っていると、**一度も発火しなくなります**。matcher を消すか `startup|resume` にしてください。

タイムアウトした hook はブロック扱いです（guard が固まったときに素通しにしないため。ここは Claude Code と異なります）。hook の起動自体に失敗した場合は警告のみで続行します（fail-open）。

### stdout の JSON

`{` で始まり `}` で終わる stdout だけを JSON として読みます（先頭の BOM は無視）。**JSON らしいのに読み取れない出力は黙って捨てません**: 配列で包まれている、構文が壊れている、`permissionDecision` が知らない値、`updatedInput` がオブジェクトでない、といった場合、PreToolUse では**ブロック**し（deny を言おうとして形を間違えた guard を素通しにしないため）、他のイベントでは警告して続行します。

```json
{
  "hookSpecificOutput": {
    "hookEventName": "PreToolUse",
    "permissionDecision": "deny",
    "permissionDecisionReason": "Destructive command blocked by hook",
    "updatedInput": { "command": "ls -la" },
    "additionalContext": "このリポジトリでは pnpm を使う"
  }
}
```

| フィールド | 効くイベント | 効果 |
| --- | --- | --- |
| `hookSpecificOutput.permissionDecision` | PreToolUse | `deny`: 実行せず理由をモデルへ返す / `ask`: 他の条件で通る呼び出しでも**必ず尋ねる**（`--yes` でも。尋ねる相手のいない `-p` では拒否） / `allow`: 承認プロンプトを省く。尋ねる相手のいない `-p` では hook が承認役になる（`--yes` 無しでも通る） |
| `hookSpecificOutput.updatedInput` | PreToolUse | ツール入力を差し替える（JSON オブジェクトのみ）。**権限ルールと承認は差し替え後の入力を見ます** |
| `hookSpecificOutput.additionalContext` | 全て | モデルに見せる追加の文脈。Pre/PostToolUse ではツール結果に、UserPromptSubmit / SessionStart / Stop では次のユーザ入力に `<hook-context>` で添える。同じ出力でブロックした場合は使われない |
| `decision: "block"` + `reason` | 全て | exit 2 と同じ |
| `continue: false` + `stopReason` | Stop 以外 | ブロック（Stop では「止まってよい」の意味なので何もしない） |
| `systemMessage` | 全て | 利用者への警告として stderr に表示 |

- hook の **`allow` は deny ルール・ask ルール・`dont-ask` モードに勝てません**。hook はプロジェクトの設定からも足せるので、利用者が[権限ルール](#権限ルールとモード)で書いた「禁止」「必ず尋ねる」「承認が要るものは通すな」を覆させないためです。
- hook の `ask` で出るプロンプトは yes / no だけです（「常に許可」や `(p)` を選んでも、次回また hook が尋ねさせるので効かないため）。
- 複数の hook が違う希望を出したら `deny` > `ask` > `allow`。ブロックした時点で残りの hook は実行しません。
- UserPromptSubmit と SessionStart では、JSON でない素の stdout もそのまま追加の文脈になります。

### hook の置き換えと無効化

`[[hooks]]` は設定ファイルのレイヤー間で**連結**されます（ユーザ設定の hook とプロジェクトの hook が両方効く）。そのままだと下のレイヤーの hook を外す手段が無いので、`id` を付けた hook は名指しで扱えます。

```toml
# ~/.config/lodan/config.toml
[[hooks]]
id = "notify"
event = "Stop"
command = "terminal-notifier -message done"

# <project>/.lodan/config.toml
disabled_hooks = ["notify"]      # このプロジェクトでは鳴らさない

[[hooks]]
id = "lint"                      # 同じ id があれば、後段のものに置き換わる
event = "PostToolUse"
command = "./scripts/lint-changed.sh"
```

`disabled_hooks` もレイヤー間で連結されます。`id` の無い hook は外せません。どの hook の `id` でもない名前が書かれていたら、起動時に警告します（綴り違いで「外したつもり」にならないように。別のマシンの設定には無い hook を名指しすることもあるので、エラーにはしません）。置き換えた hook は**後段の hook の位置**で発火します（hook は並び順に実行され、ブロックした時点で残りは実行されないので、順序が効く場面では注意）。`lodan config` の `[[hooks]]` は連結したままの一覧で、実際に発火するのは「`disabled_hooks` を除き、同じ `id` は最後の 1 つ」です。

> ⚠️ これは便利のための仕組みで、**守りにはなりません**。信頼したプロジェクトの設定は、ユーザ設定の guard hook を `disabled_hooks` で外せます（プロジェクトの設定は hook を足せる時点で任意のコマンドを実行できるので、信頼の範囲は変わりません）。

### matcher

| matcher | 解釈 |
| --- | --- |
| 空 / `"*"` / 省略 | 常に一致 |
| 英数と `_` `-` 空白 `,` `\|` だけ | 完全一致。`Edit\|Write` のような並びはどれかに完全一致 |
| それ以外の文字を含む | 正規表現（**部分一致**）。`Edit.*` は `NotebookEdit` にも当たる。全体一致は `^Edit$` |

MCP のツールをサーバ単位で拾うなら `mcp__memory__.*`（`mcp__memory` だけだと完全一致扱いで何にも当たりません）。正規表現として壊れている matcher は**起動時にエラー**にします（一度も発火しない guard を黙って受け入れないため）。matcher が照合する相手: PreToolUse / PostToolUse / PostToolUseFailure / PermissionRequest はツール名、SessionStart は `startup` / `resume`、PreCompact / PostCompact は `manual`（`/compact`）/ `auto`、SubagentStart / SubagentStop は子エージェントの種類（いまは `general-purpose` のみ）、Notification は通知の種類（`permission_prompt`）。UserPromptSubmit / Stop / SessionEnd は matcher を見ません。

### ペイロード

全イベント共通: `session_id` / `transcript_path`（永続化が無効なら null）/ `cwd` / `permission_mode`（`default` | `accept-edits` | `plan` | `dont-ask` | `bypass`）/ `hook_event_name`。

- **SessionStart**: `source`（`startup` | `resume`）。REPL / `-p` の開始時。ブロックしても起動は止めず警告のみ。
- **SessionEnd**: 終了時（ベストエフォート、ブロック不可）。
- **UserPromptSubmit**: `prompt`。ブロック時はそのターンを実行せず破棄。
- **PreToolUse**: `tool_name` / `tool_input`。ブロック時はツールを実行せず、理由をモデルへ返す。
- **PermissionRequest**: `tool_name` / `tool_input`。**承認プロンプトを出す直前**（= ルールでもモードでも決まらず、誰かの承認が要る呼び出し）に発火。`{"hookSpecificOutput": {"decision": {"behavior": "allow"}}}` で承認、`{"behavior": "deny", "message": "…"}` で拒否（`"decision": "allow"` の文字列形も可）。`allow` の効き方は PreToolUse の `allow` と同じで、deny / ask ルールと `dont-ask` には勝てない。尋ねる相手のいない `-p` でも発火するので、ヘッドレス実行の承認役にできる。
- **Notification**: `notification_type`（`permission_prompt`）/ `message`。REPL が承認プロンプトを出して**人の入力を待つ直前**に発火（デスクトップ通知などに）。出力は読まない。hook の終了を待ってからプロンプトを出すので、時間のかかる通知はコマンドの中でバックグラウンドに回すこと（`notify-send … &`）。
- **PostToolUse**: `tool_name` / `tool_input` / `tool_response`（旧名 `tool_output` も同じ値）。**成功した実行の後**に発火。実行後なので取り消せず、ブロックの理由はツール結果に追記されてモデルへ返る。
- **PostToolUseFailure**: PostToolUse の項目に加えて `error`。ツールを**実行して失敗した**ときに発火。PostToolUse と同じく実行後なので取り消せず、ブロックの理由はツール結果に追記されてモデルへ返る。hook やゲートが止めて実行に至らなかった呼び出しでは、PostToolUse も PostToolUseFailure も発火しない（`hooks_compat = "v1"` では従来どおり、全ての呼び出しで PostToolUse）。
- **PreCompact** / **PostCompact**: `trigger`（`manual` | `auto`）、PreCompact には `custom_instructions`（`/compact <指示>` の指示）。PreCompact をブロックすると圧縮しない。畳むものが無くて圧縮が見送られるときは発火しない。
- **SubagentStart** / **SubagentStop**: `agent_type` / `cwd`、Start には `prompt`（依頼文）、Stop には `last_assistant_message`（失敗時は `error`）。`Task` の子エージェントの開始と終了。通知用で、ブロックはできない。`session_id` などの共通フィールドは付かない。
- **Stop**: `last_assistant_message`（旧名 `last_message` も同じ値）。ターン終端で発火。**ブロックすると停止せず、その理由をユーザー入力として注入し次ターンへ継続する**（暴走は `max_iterations` で停止）。「条件を満たすまで作業を続ける」系の自律ループの土台。

> ⚠️ **信頼前提**: hook コマンドは `sh -c` で実行され、パーミッションゲートを経ません。プロジェクトの `config.toml` の hook が動くのは、そのディレクトリを[信頼した](#workspace-trust--信頼していないディレクトリの設定は読まない)ときだけです。信頼するのは中身を確認したリポジトリに限ってください（任意コード実行になります）。

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
  - `goal.json`: 未達の `/goal` があるときだけ（条件・通算ターン数・走っていた時間。[後述](#ゴール駆動の自律継続goal)）
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

## コンテキスト圧縮（`/compact` ＋ 自動圧縮）

`/compact [指示]` で会話履歴を圧縮する。System プロンプトと**直近 2 ユーザターン**を生のまま残し、それ以前を LLM 要約に畳む。要約（`[Summary of earlier conversation] ...`）は独立メッセージにせず**直近ターン先頭のユーザメッセージへ前置**する（user が 2 連続すると strict な user/assistant 交互を要求するローカルモデルでエラーになり得るため）。任意の指示（例: `/compact keep file paths`）を渡すと要約の重点を変えられる。

- 分割は**ユーザターン境界**に限定するので、Assistant の `tool_calls` と対応する Tool 応答の対を跨いで切らない。
- ユーザターンが 2 以下のときは何もしない（`skipped`）。要約 LLM が空を返したら `failed`。
- 要約には active モデルを使う（別モデル指定は未対応）。

### 自動圧縮（しきい値トリガ）

直近 LLM 呼び出しのコンテキストサイズ（`last_context_tokens`、トークン会計由来）が **`context_window` の 80%**（`[agent] auto_compact_percent` で変更可。`0` は無効）に達すると、ターン終端で自動的に `/compact` 相当の圧縮を実行する（`[auto-compact] ...` と dim 表示）。

- しきい値の分母は provider 設定の `context_window`（既定 32768）。**サービング側の実効窓**（例: ollama は既定 `num_ctx=4096`）と一致させること。`context_window = 0` で自動圧縮を無効化できる。
- 圧縮に失敗（要約 LLM エラー等）してもターンは成功扱いで、次ターン終端に再試行する。
- 履歴がまだ短い（ユーザターン 2 以下）ときは手動時と同じく `skipped`。

## ゴール駆動の自律継続（`/goal`）

Claude Code の `/goal` 移植。**達成条件を 1 つ設定すると、評価器 LLM が達成を確認するまでターンを自律的に継続する**。

```text
lodan> /goal cargo test が exit 0 で通る。または 10 ターンで諦める
```

- `/goal <条件>` — 条件（最大 4,000 字）を設定して即座にループ開始。条件は「測定可能な終了状態＋確認手段」で書くと堅い（例: 「`cargo test` が exit 0」）。
- `/goal` — 現在の状態（条件・今回の枠と通算のターン数・経過時間）を表示。
- `/goal resume`（別名 `continue`）— paused の goal を**続きから**走らせる。ターン数と時間の上限は走らせるたびに新しい枠になり、通算は引き継ぐ。モデルには「既に N ターン使っている。やり直さず続きから」と伝える。
- `/goal clear`（別名 `stop` / `off` / `reset` / `none` / `cancel`）— 解除。

動作: ターン完了ごとに評価器（既定は **active モデルを流用**、ツールなし・トランスクリプトのみ参照）が `{"met": bool, "reason": string}` を返す。未達なら `reason` を次ターンの入力として注入して継続、達成なら解除して報告する。

- **暴走防止（ハード上限）**: 1 回の実行につき 20 ターン / 30 分。到達すると必ず停止し、goal は paused として残る（`/goal` で確認、`/goal resume` で再開、`/goal clear` で破棄）。
- **評価器の出力がパース不能なときは安全側で停止する**（根拠のない自律継続はしない）。
- **承認ポリシー**: 破壊的ツール（Write / Edit / Bash …）は goal 中も**既定で通常どおり承認プロンプトを出す**。完全自律にしたい場合のみ `--yes`（または `agent.auto_approve`）を明示する。
- **Ctrl-C で自律ループを中断できる**（= 一時停止）。中断した goal は paused として残る（`/goal` で確認、`/goal resume` で再開、`/goal clear` で破棄）。
- **goal はセッションと一緒に保存される**（セッションのディレクトリの `goal.json`。各ターンの後と、停止・解除のたびに更新）。経過時間として数えるのは goal が**走っていた時間だけ**で、一時停止中に REPL を開いていた時間は入らない。`--resume` で開き直すと paused として戻り、`/goal resume` するまで勝手には走らない。達成・解除した goal は残さない。
- **評価器を別のモデルにできる**。作業したモデルが自分で合否を決めると甘くなりがちなので、判定だけ別のモデルに任せられる:

  ```toml
  [goal]
  evaluator_provider = "kimi"      # 省略時は作業している provider
  evaluator_model = "kimi-k3"       # 省略時はその provider の model
  ```

  設定したのに組めない（API キーが無いなど）ときは起動時エラー（`/goal` を使わない `-p` の実行でも。黙って作業側の自己判定に戻ると、気づけないため）。評価器には fallback provider は付かない: 評価器の呼び出しが失敗したら goal は paused で止まり、`/goal resume` でやり直せる。
- 評価器の呼び出しも `/cost` と[予算](#予算)に `goal_eval` として計上される。
- 制限: `-p` 非対話での `/goal` はスコープ外。

## ファイル変更の巻き戻し（`/undo`）

ターン中のファイル変更を**ターン単位で巻き戻す**（#37 の MVP）。ファイル系ツール（Write / Edit / MultiEdit / NotebookEdit）の実行直前に変更前スナップショット（そのターンで最初に触る時点の内容）を取り、`/undo` で直近ターンぶんをまとめて復元する。

```text
lodan> /undo
undo (turn 12):
  restored /path/to/src/main.rs
  removed  /path/to/new_file.txt
```

- 復元規則: ターン開始時に存在したファイルは**当時の内容へ書き戻し**、ターン中に新規作成されたファイルは**削除**。同一ターンに複数回変更しても first-touch の内容へ戻る。
- 繰り返し `/undo` すると 1 ターンずつ遡る（保持は直近 10 ターン、redo なし。`/undo` 自体の変更は記録しない）。
- Ctrl-C で中断したターンの変更も記録されているので `/undo` で戻せる。
- **対象外（重要）**: `Bash` の副作用（コマンド実行・git 操作・ネットワーク等）は一般に巻き戻せないため記録しない。4 MiB 超のファイルはスナップショットせず、undo 時に `skipped` として報告する。
- 承認ゲートで拒否した実行は変更が起きないため記録されない。

## プランモード（`/plan` / `/accept`）

Claude Code のプランモード移植（MVP）。**読み取り専用ツールだけで調査・計画し、ユーザ承認後に実行へ移る**。

```text
lodan> /plan
[plan] entered plan mode — destructive tools are disabled. Investigate & plan, then /accept to approve and execute
lodan (plan)> 認証まわりをリファクタしたい。方針を立てて
...（Read/Grep/Glob 等で調査 → 計画を提示）...
lodan (plan)> /accept
[plan] accepted — back to normal mode, destructive tools re-enabled
lodan> 計画どおり進めて
```

- `/plan` で進入（プロンプトが `lodan (plan)>` に変わる）、`/accept` で承認して通常モードへ復帰。
- Plan 中は破壊的ツール（Write / Edit / Bash / MultiEdit / NotebookEdit、MCP ツール含む）を**二重に抑止**する: (1) LLM へ渡すツール一覧から除外（不可視化）、(2) それでも呼ばれたら実行せずエラー応答（多層防御・承認ゲートより先に効く）。読み取り系（Read / Grep / Glob / SubAgent 等）はそのまま使える。
- Plan 中の各ユーザ入力には「調査と計画のみ・変更禁止・完成したら ExitPlanMode で承認要求」の指示が前置され、モデルに毎ターン伝わる。
- **`ExitPlanMode` ツール**: Plan 中のみ LLM から見える擬似ツール（registry 外・`/tools` には出ない）。モデルが計画完成時に `ExitPlanMode {plan}` を呼ぶと、計画を表示して承認プロンプト（y / n / a=以後の計画を常に承認）を出す。**承認**すると同一ターン内で通常モードへ移行し、そのまま実行に進める。**拒否**すると Plan のまま計画修正を指示する。`--yes`（auto_approve）時は自動承認。手動 `/accept` も併用可。
- モードはセッション内のみ（transcript には残るが `--resume` 後は Normal から）。

## 反復実行（`/loop`）

Claude Code の `/loop` 移植（MVP = フォアグラウンド固定間隔ループ）。**プロンプトまたはユーザ定義コマンドを一定間隔で反復実行する**。

```text
lodan> /loop 5m ビルドが通るか確認して、壊れていたら直して
lodan> /loop 1m /check-ci main
```

- 書式: `/loop <間隔> <プロンプト|/ユーザ定義コマンド [引数]>`。間隔は `[数値][単位]`（`s`/`m`/`h`/`d`、例 `30s` `5m` `2h`）。
- 毎反復、同じプロンプトを再投入して 1 ターン実行 → 間隔ぶん待って繰り返す。`/名前` 形式はユーザ定義 slash コマンドを起動時に 1 度だけ展開する（組み込み slash は対象外）。
- **フォアグラウンドで REPL を占有する**（同期 REPL のため）。cron 永続・バックグラウンド常駐・動的間隔はスコープ外（issue #30 参照）。
- **停止**: `Ctrl-C`（ターン実行中でも sleep 中でも安全に停止、履歴は自動修復）。
- **暴走防止（ハード上限）**: 100 反復 / 24 時間。ターンが失敗したときも停止する（壊れたエンドポイントを叩き続けない）。
- **承認ポリシー**: `/goal` と同じ — 破壊的ツールは既定で通常どおり承認プロンプトを出す。完全自律にしたい場合のみ `--yes`（または `agent.auto_approve`）を明示する。

## トークン会計と予算（`/cost`）

LLM の呼び出しは、どこから行われたものでも 1 つの台帳に載る。エージェントのループだけでなく、`Task` のサブエージェント、`/goal` の評価器、コンテキスト圧縮、MCP sampling も含む（計上はクライアントを包む層で行うので、ループを通らない呼び出しも漏れない）。

```text
lodan> /cost
tokens: 9120 total (prompt 8200 + completion 920) across 9 LLM request(s)
  compact: 1400 tokens in 1 call(s)
  main: 5200 tokens in 5 call(s)
  subagent: 2520 tokens in 3 call(s)
budget: 9 / 40 requests
last context: 1200 prompt tokens
```

- 内訳の種別は `main` / `subagent` / `goal_eval` / `compact` / `mcp_sampling`。`main` しか無いときは内訳を省く。
- 非ストリームは応答 body の `usage`、ストリームは `stream_options: {"include_usage": true}` を付けて最終チャンクの `usage` から取得する（OpenAI / vLLM / llama.cpp 対応）。`total_tokens` を返さないサーバは `prompt + completion` で補完する。
- **usage 非対応サーバへのフォールバック**: usage が取れない呼び出しは文字数ベース（約 3 文字 / トークン）で概算し、`/cost` に概算だった呼び出し数を注記する。桁を合わせるのが目的の粗い近似。
- `last context` は直近呼び出しの prompt_tokens で、現在のコンテキストサイズの近似。自動圧縮のしきい値判定（前節）に使っている。
- 金額は、単価を設定したモデルについてだけ出す（lodan は単価を内蔵しない）:

  ```toml
  [pricing."kimi-k3"]          # モデル名ごと。100 万トークンあたり
  input_per_mtok = 0.6
  output_per_mtok = 2.5
  ```

  `/cost` に `cost: ~0.1234 (from [pricing])` が付く（通貨は書いた単価の単位のまま）。集計はモデルごとなので、fallback provider や `/goal` の評価器が別モデルなら、それぞれの単価で計算する。単価の無いモデルを使っていたら `no price for <model>` と添えて、金額に入っていないことを示す。見積もりであって請求額ではない（キャッシュ割引などは知らない）。単価は 0 以上の有限の数（それ以外は起動時エラー）。`-p` の `json` / `stream-json` では `usage.cost` に入る（`[pricing]` が無ければ `null`）。
- 累積はメモリ上のみ（transcript には保存しない）。`--resume` 後の `/cost` は 0 から数え直す。
- `-p` の `json` / `stream-json` の `usage` も同じ台帳から出る（`requests` と `by_kind` が増えた。`llm_calls` などの既存フィールドは、これまで漏れていたサブエージェントなどの分も含む合計になる）。

### 予算

```toml
[agent]
max_requests = 40           # このプロセスが送る LLM リクエスト数の上限
max_total_tokens = 200000   # 同じく合計トークン数の上限
```

CLI は `--max-requests <N>` / `--max-tokens <N>`、環境変数は `LODAN_MAX_REQUESTS` / `LODAN_MAX_TOKENS`。既定はどちらも無制限。

- 予算の **8 割**を使ったら、次の LLM 呼び出しの前に一度だけモデルへ知らせる（`[budget] This run has used 32 of 40 LLM requests. … Wrap up: …`）。打ち切られる前に、一番大事な残りを片づけて何が済んで何が済んでいないかを報告させるため。ターンの最初の呼び出しの前なら利用者の入力の末尾に添え、ツール実行の後なら独立した user メッセージにする（user を 2 つ続けると、役割の交互を要求するチャットテンプレートに拒否されるため）。`--log-jsonl` には `budget_reminder` が残る。知らせるのは「まだ次の 1 件を送れるとき」だけなので、**出ないことがある**: 並列のサブエージェントが 1 手で 8 割から使い切りまで飛び越した場合と、予算が小さすぎて 8 割と使い切りの間に 1 件も入らない場合（`--max-requests` が 4 以下）。注意書きは予算を付けた実行のためのものなので、`--resume` で開き直したセッションの履歴からは取り除く。
- 上限に達したら、**次のリクエストを送らずに**ターンを打ち切る。止まるのは必ず LLM 呼び出しの直前なので、履歴は tool_call と結果の対が揃ったまま保存され、`--resume` でそのまま続けられる。`-p` の終了コードは **4**。
- リクエスト数は**プロバイダへ実際に送った数**（プロバイダの無料枠はリクエスト数で数えられるため）。失敗したものも、再試行（`max_retries`）で送り直したものも、fallback provider へ送り直したものも、それぞれ 1 件。予算が残っていなければ再試行もせず、その失敗は予算切れ（終了コード 4）として報告する。
- トークンの判定は各リクエストの**前**に行うので、超過は最後の 1 回ぶんまであり得る。
- サブエージェントや `/goal` の評価器も同じ予算から引かれる。予算はプロセス単位で、`--resume` では引き継がれない。
- REPL で予算が尽きたあとは、以降のどの入力も同じエラーで失敗する（実行中に予算を増やす手段は無い）。`--max-requests` などを付け直して `--resume` で続けること。

## ロードマップ

未実装の機能は issue で追跡しています: [#85 (2026-09 Claude Code / Codex ギャップ)](https://github.com/hisato-kawaji/lodan/issues/85)、[#65 (小型ローカルモデル向けハーネス強化)](https://github.com/hisato-kawaji/lodan/issues/65)、[#38 (画像入力 / `@file` / rewind)](https://github.com/hisato-kawaji/lodan/issues/38)。

## テスト

```bash
cargo test
```

カバレッジ:
- `tools/edit.rs` — 一意マッチ / 多重マッチ拒否 / Read 必須
- `tools/read.rs` — offset / limit
- `tools/todo_write.rs` — replace / clear / multi-in_progress 拒否 / 引数不正
- `tools/registry.rs` — 動的名登録 / built-in 既定 14 ツール / ツールプロファイル (core はちょうど 6 個・readonly は破壊系なし・明示リスト優先・plan モードにも効く・定義が半減以上)
- `tools/background.rs` / `tools/bash.rs` — BG ストアの増分読み出し・上限 append・kill 合図 / Bash の run_in_background → Monitor / KillShell 一周
- `memory/mod.rs` — LODAN.md/CLAUDE.md 探索・優先順・外内連結・空ファイル除外・上限の文字境界打ち切り
- `permission.rs` — auto_approve / always-tool / always-command の判定
- `repl.rs` — slash command 判定（絶対パス始まりは LLM に流す）
- `llm/openai.rs` — usage パース（非ストリーム / ストリーム最終チャンク）・`total_tokens` 補完・`stream_options` の付与
- `agent/loop.rs` — Stop hook 継続 / compact 境界・要約前置 / usage 累積・概算フォールバック
- `mcp/config.rs` / `mcp/protocol.rs` / `mcp/transport.rs` / `mcp/client.rs` / `mcp/tool.rs` / `mcp/prompt.rs` / `mcp/resource.rs` / `mcp/roots.rs` / `mcp/sampling.rs` — `.mcp.json` パース、JSON-RPC + MCP 型、stdio/HTTP transport、transport 非依存クライアント、tool / prompt / resource の namespacing、roots 提供、sampling (server→client LLM 補完) の opt-in 橋渡し
- `tests/e2e_mock.rs` — 6 ツールを順に走らせるエンドツーエンドのモック試験
- `tests/e2e_mcp.rs` — mock MCP サーバとの handshake + tools/list + tools/call 一周

## ライセンス

MIT OR Apache-2.0
