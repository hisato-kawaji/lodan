# lodan 引き継ぎ Mega Prompt（2026-09-21 時点）

このファイルを新しいセッションの最初に貼れば、続きから作業できる。コミット対象外（`.claude/scratch/`）。

---

## 0. あなたへの依頼の前提

- リポジトリ: `hisato-kawaji/lodan`（Rust edition 2024 製の、Claude Code 風コーディングエージェント CLI。ローカル / 小型 LLM が主対象、OpenAI 互換 Chat Completions）
- 応答は日本語・簡潔に。完了報告は「何を変えたか / なぜ / 未解決の懸念」の箇条書き
- 複数ファイルに及ぶ変更は、先に計画を箇条書きで出してから着手。仕様が曖昧なら推測せず質問
- 既存の命名・構造・コメントの書き方（日本語の「なぜ」コメント）に合わせる。無関係なリファクタ禁止、TODO / デバッグ出力を残さない
- `.env` の中身は表示しない（変数名だけ）

## 1. 現在地

- **main = `6947ab6`**。open PR は以前からの **#69**（.gitignore に llmapi/）・**#60**・**#28**（docs）だけ
- 2026-09-19〜21 に、Claude Code / Codex CLI との機能ギャップを洗い出して起票（tracking **#85**、個別 #70–#84）し、次を実装・マージ済み:

| issue | PR | 内容 |
|---|---|---|
| #82 | #87 | config レイヤーの deep merge、`--show-origin` |
| #70 | #90, #95 | HTTP リトライ / Retry-After / stream idle timeout、fallback provider |
| #71（open のまま） | #92, #93, #115 | ヘッドレス `-p`（text/json/stream-json、終了コード 0/1/2=clap/3=max_iterations/130）/ `--output-schema`（自前の最小 JSON Schema バリデータ `src/schema.rs`、依存追加なし、不一致は 2 回再要求して exit 5）。残り: `--max-turns`・`--append-system-prompt`・ラダーの `-p --output-format json` 載せ替え・#92 の nit |
| #72 | #94 | ツールプロファイル（full/core/readonly） |
| #73 | #96 | parallel-safe ツールの並列実行 |
| #74 | #98, #99 | 権限ルール（allow/deny/ask）とモード、`(p)` での永続化 |
| #100 | #101 | モデル由来文字列の端末エスケープ無害化（`term::sanitize`、`red_err` が単一の出口） |
| #75 | #102, #103 | workspace trust / Bash サンドボックス（macOS seatbelt・Linux bwrap、fail closed） |
| #76 | #104, #107 | hooks v2（exit 2=ブロック、stdout JSON、regex matcher、共通 payload、`hooks_compat="v1"`）/ イベント 7 種、hook の `id` と `disabled_hooks` |
| #84 | #106, #109, #108 | LLM 計上をクライアント層へ（`llm::metered`）、`max_requests`/`max_total_tokens`（`-p` exit 4）/ 80% リマインダ・`[pricing]`・`auto_compact_percent` / `goal.json`・`/goal resume`・評価器の別モデル |
| #88 | #112 | `lodan config` の秘密マスク（`Config::redacted()`、`--show-secrets`、`redact_url` は文字列処理） |
| 計測 | #113, #116 | 実 API: kimi-k3 の `low` は正答 10/10 のまま思考 -73%・時間 -4% / ローカル: qwen3.5:9b は thinking off で正答同じ 9/10・-28%、thinking ありの 20 実行中 13 が思考だけの空応答で終了（→ #111 の優先度を上げる根拠） |
| #78 | #105, #110 | `reasoning_effort`・`extra_body` / `reasoning_content` をツール往復の間だけ送り返す・畳んだ表示・`plan_reasoning_effort` |

- **2026-09-26 のセッション**（ユーザー指示: 「#71, #72, #91からやって#111, #114をやって」→「おわったらP2を実施で」）:

| issue | PR | 内容 |
|---|---|---|
| #71（close） | #118 | `--max-turns` / `--append-system-prompt`、ラダーを `-p --output-format json` に載せ替え、#92 の nit |
| #91（close） | #119 | tty 分岐の単体テスト、`&mut (dyn Write + Send)`、パイプした REPL が承認を得ない e2e |
| #72（open、残りは MCP 多数構成の実測と `core` の TodoWrite） | #120 | `ToolSearch` 遅延ロード（`[agent] tool_search`、readonly の破壊的ツールは遅延にもしない、`spec_bytes` は ToolSearch 込み） |
| #114（close） | #121 | TOML エラー表示から値を消す（構文・型の両方）。残り経路は #123 |
| #111（close） | #122 | 空応答を 1 回だけ促す（`empty_reply_nudge`、既定 on）。履歴の `content: ""` 正規化（Ollama 400 対策）、ラダー ablation 追従 |
| #79（close） | #125, #126（マージ） | AGENTS.md フォールバック / `@path` import / `/memory` / path-scoped ルール（`<path-rules>` で囲む、`..` は cwd 外に一致しない、`.lodan/rules` は trust 候補） |
| #81（close） | #124, #127（マージ） | `/model` / `/status` / `/context` / `[ui] prompt_status` / `!<cmd>`（先頭語がコマンドのときだけ実行、ゲート・サンドボックス・deny を通さない） |
| #77（open） | #128（マージ） | `.lodan/agents/*.md`（ツール絞り込みは実行時にも効く・別モデル・追加指示、read-only の範囲）。残り: 書き込み可の子（plan モード迂回の設計が要る）、worktree 分離 |
| #80（close） | #129（マージ） | `--continue` / cwd スコープの `--resume last` + `--all` / `--fork` / `sessions --json` / `/fork` `/rename` `/export` / `LODAN_SESSIONS_DIR` / session id ガード（`--resume ../evil` を塞いだ） |
| #83（open） | #130（マージ） | `trustAnnotations` で readOnlyHint を非破壊に / user スコープ `mcp.json` / `lodan mcp list\|add\|remove` / `enabledTools` `disabledTools` `toolTimeoutSecs`（transport にも渡す）`maxOutputBytes`。残り: `list_changed` / elicitation / OAuth |
| policy | #132（マージ） | 版表記 v0.2、観点 2 のスタブ記述を現状に |

- **main = `4037e41`（2026-09-26、#129 マージ後）。worktree は全て撤去済み。**
- **残っている issue**: 既存 **#38**、**#72**（MCP 多数構成の実測、`core` の TodoWrite）、**#77**（書き込み可の子）、**#83**（提案 4・5）、派生 **#123**（.mcp.json / trust store のエラー表示）・**#131**（/compact 後の transcript 欠落、既存バグ）。open PR は #69（放置）・#60・#28 だけ
- **2026-10-04**: `core` に TodoWrite を追加（#133、ユーザー判断。6,643 → 3,065、-54%）。CI の clippy 1.99 `double_must_use` を #135 で抑止（toolchain は無ピンのまま）。**#134 書き込み可サブエージェントをマージ（main = `aae1b0b`、レビュー 3 周）**: `.lodan/agents/*.md` の `tools:` に破壊的ツールを含められる、`Runtime` が `gate` / `plan_flag` / `hook_env` を親子で共有、子の破壊的呼び出しは `PermissionGate::decide_for_subagent` → 出自つき yes/no プロンプトのみ（`(p)` / dont-ask なし）、plan 中は拒否、子も PreToolUse / PostToolUse / PostToolUseFailure を通り payload は親と同じ共通フィールド + `agent_type`、AskUserQuestion 禁止、書き込み可の定義があれば Task 非並列、子の Bash に親の SandboxPolicy。eval docs は `eval/docs-stash-2026-10` に退避。worktree は全て撤去。判断待ちは残っていない。**#77 の残りは提案 5（worktree 分離 / バックグラウンド + 再開）のみ**

## 2. 破壊的変更（利用者の設定に影響）

- 非対話実行（`-p`）は `--trust` / `LODAN_TRUST=1` / `lodan trust` が無いとプロジェクト設定（`.env`・`.lodan/config*.toml`・`.mcp.json`・commands・skills・cwd 階層のメモリ）を読まない。**未コミットの `docs/eval/kimi_eval_harness.sh` は `--trust` の追加が必要**
- hook は exit 2 だけがブロック（他の非 0 は警告）。旧挙動は `hooks_compat = "v1"`。PostToolUse は成功した実行の後だけ、失敗は PostToolUseFailure。SessionStart の matcher が効く（`startup`/`resume`）
- 再試行・fallback で送り直した分も `max_requests` に 1 件ずつ数える

## 3. 判断待ち（ユーザーに未回答のまま）

1. ~~`--output-schema` のバリデータ~~ → **決定済み: 自前の最小実装**（#115 でマージ済み）
2. ~~#69 をマージしてよいか~~ → **決定済み（2026-09-21）: マージしない・放置**。再提案しない。`llmapi/` は未追跡のままなので `git add -A` を使わない
3. ~~未コミットの eval docs を docs PR にするか~~ → **決定済み（2026-10-04）: PR にせず、ブランチ `eval/docs-stash-2026-10`（`79d02a2`、5c0c094 ベース）に退避**。作業ツリーの未コミット分はそのまま（lodan 側は触っていない）
4. ~~`docs/policy/pr-review.md` に「未免除の WARN が 1 件だけ」の判定を足すか~~ → **決定済み（2026-09-22）: 足した**（#117。APPROVE（条件つき）= WARN を「対応か waiver か」の 2 択で返す）
5. ~~`core` プロファイルに TodoWrite を含めるか~~ → **決定済み（2026-10-04）: 含める**（#133）
6. ~~実 API を使う計測の許可~~ → **実施済み**（#113・#116）。追加の有料計測は改めて確認する
7. #111 に進んでよいか（提案済み・未承認）。最初の一手は `--finish-nudge` ありのラダーを同条件で回し、既存機能が空応答に効くかを確かめること

## 4. 小さな follow-up（issue 未起票のもの）

- 予算の注意書きを履歴から外す判定が本文の固定文照合（`metered::is_budget_reminder`）。根本対応は `Message::User` に由来（利用者 / lodan）を持たせる
- `runtime.rs` の `with_reasoning_roundtrip` 配線のテスト / `-p` では畳んだ思考の 1 行が出ないことの README 明記（#110 のレビュー）
- Ctrl-C 経路の `goal.pause()` を固定する PTY テスト（#108 のレビュー）
- `.git/worktrees/<n>/config.worktree` のサンドボックス保護は見送り（`.git/config` を書けないと有効化できないため）

## 5. リポジトリの作法

- `feat/*` ブランチ → PR → **`pr-reviewer` サブエージェント**がレビュー（SHA マーカーつきの PR コメント）→ 指摘対応 → CI green（`cargo fmt --all --check` / `cargo clippy --all-targets -- -D warnings` / `cargo test`）→ `gh pr merge --merge` → ブランチ削除
- コミットは conventional commits（日本語の本文可）、末尾に `Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>`。PR 本文の末尾は `🤖 Generated with [Claude Code](https://claude.com/claude-code)`
- **ユーザーの作業ツリー（`~/dev/lodan`、ブランチ `eval/difficulty-ladder`、未コミットの変更あり）には触らない**。作業は scratchpad 配下の `git worktree` で行う
- push は必ず `git push origin <b>:refs/heads/<b>`（`push.default=tracking` のため、素の `-u` は main に push しようとする）
- テスト結果は全バイナリを集計して見る: `cargo test 2>&1 | grep -E "^test result|FAILED|panicked" | awk '/^test result/{p+=$4; f+=$6; next} {print} END {print "passed="p, "failed="f}'`（`head -N` で切ると e2e の FAILED を見落とす）

## 6. 踏んだ地雷（同じ失敗をしないために）

- **doc コメントの誤アタッチを 4 回やった**: fn シグネチャを anchor にして新しい fn を挿入すると、元の fn の `///` が新しい fn に付く。anchor は「直前のアイテムの閉じ `}` を含む 2〜3 行」にし、挿入後は `grep -n "fn <名前>" -B8` で機械的に確認する
- パッチ用の Python スクリプトは `assert s.count(anchor)==1` を**全 anchor について先に**確かめてから書き込む（途中で落ちると半分だけ書かれる）。rustfmt が anchor を崩すので、失敗したら実テキストを見て当て直す
- 修正を外しても通るテストを書かない。変異試験は**同じディレクトリでその場で**書き換える（別コピー + 共有 `CARGO_TARGET_DIR` だと最初のバイナリが使い回される）。退避コピーから戻したら **`touch`**（mtime が古いと cargo が再ビルドしない）。`cp` は `-i` の alias があるので `/bin/cp -f`・`/bin/mv -f`
- 「起動できない環境では skip」するテストは、skip したことを必ず表示させる（bwrap のテストが Linux で全部黙って skip され green に見えていた）。Linux の確認は Docker: `docker run --rm --privileged -v <wt>:/src:ro -v lodan-sbx-target:/t -v lodan-sbx-cargo:/usr/local/cargo/registry -e CARGO_TARGET_DIR=/t -w /src rust:1-bookworm` + `apt-get install bubblewrap`
- 保護は「その名前の付け替え」まで考える（`.git/hooks` を守っても `.git` ごと rename → 差し替えで抜けた）。bwrap は後の bind が前の mount を覆い隠す
- ヘッドレスで stdin を「端末でなければ読む」にしない（閉じられない非 tty stdin で固まる）。zsh は変数に入れたコマンドを単語分割しない
- 役割の交互を要求するチャットテンプレートがあるので、`user` メッセージを 2 つ続けて積まない（既存の全注入は Assistant の直後。compaction も直後の User に前置している）

## 7. レビュー依頼文のテンプレート（必須の安全条項）

権限・サンドボックス・予算に関わる PR は「実行して壊してみて」と依頼すると、毎回実在の穴が出た。依頼文を書く過程で自分でも穴に気づくことが多い。**2026-09-21 にレビュアーが cwd を取り違え、ユーザーの `.env` の鍵で Moonshot の実 API を 1 回叩く事故があった**ので、次を必ず入れる:

1. PR head の **pristine export** だけを使う: `SHA=$(gh pr view <N> --json headRefOid -q .headRefOid)`; `git -C <worktree> archive $SHA | tar -x -C <scratchpad>/<dir>/head`（レビュー中は作者もその worktree を編集しない / 直したくなったらローカルコミットに留め、verdict の後で push）
2. lodan の起動は `cd <scratch 配下> || exit 1` の後、`env -i HOME=<scratch> PATH="$PATH" LODAN_TRUST=1 LODAN_PROVIDER=local LODAN_BASE_URL=http://127.0.0.1:<mock>/v1 <binary> …`。親の環境を継がない
3. `--provider kimi|sakura|sakana` を渡さない。有料 / 外部 API 禁止（ローカル Ollama `http://localhost:11434` の `qwen35-9b-8k:latest` は可）
4. `~/dev/lodan` に触らない。scratch は指定のディレクトリだけ
5. 検証の数字は集計コマンドで。変異試験 2〜3 本を依頼する

## 8. 主要な設計の置き場所（コードを読む入口）

- `src/agent/loop.rs` — ターンのループ、hooks の発火（`fire_hook`）、並列 prefetch、compact、予算リマインダ、思考の持ち回り（`drop_reasoning`）、`stream_once_to`（表示）
- `src/permission.rs` / `src/permission_rules.rs` — 承認ゲート（`assess` → `needs_approval` / `decide_assessed`。ルール評価は 1 呼び出しにつき 1 回）、ルールエンジン（パスは `canonicalize` に頼らず左から 1 要素ずつ解決）
- `src/hooks/` — `HookConfig`（`id`・matcher・`timeout_secs`）、`runner`（exit code と stdout JSON の解釈。PreToolUse / PermissionRequest の「読めない判断」はブロック）、`effective`（`disabled_hooks` と id の置き換え）
- `src/llm/metered.rs` — 台帳（`Ledger`）、`with_kind`（task-local）、`admit` / `admit_retry`、`[pricing]`、`is_budget_reminder`
- `src/llm/openai.rs` — リクエスト組み立て（`RESERVED_BODY_KEYS`、`extra_body` は flatten）、リトライ、`reasoning_content` / `reasoning` の受信、`effort()`（`llm::in_plan_mode` の task-local）
- `src/sandbox.rs` — seatbelt プロファイル / bwrap 引数、`protected_paths`、Linux の `prepare_workspace` / `unguarded` / `planted`
- `src/trust.rs` — `trusted.toml`、`decide`。`main.rs` は trust の判断の**後**に `dotenvy::from_path(cwd/.env)`（親を探さない）
- `src/goal/mod.rs` — `Goal`（枠ごとの上限と通算、`pause()`）、`GoalRecord`、`drive_with`（評価器を分けられる）
- `src/headless.rs` — `-p` の結果オブジェクトと終了コード（4 = 予算）
- `src/runtime.rs` — `Runtime::build(cfg, notices, interactive)`。REPL / headless 共通の組み立て。`gate` / `plan_flag` / `hook_env` は `Arc` で持ち、`Task`（`src/agent/subagent.rs`）に `with_gate / with_plan_flag / with_hook_env / with_sandbox` で渡す。`build_agent_profile` は `tools:` 無しなら read-only registry
- `src/agent/subagent.rs` — `SubAgentTool`。`admit`（plan 中の破壊的 → 拒否、gate あり → `decide_for_subagent`、gate 無しの破壊的 → 拒否）、`hook_payload` / `tool_hook`（親の `fire_hook` と同じ共通フィールド）、`parallel_safe()` は書き込み可の定義が無いときだけ true
- メモリ: `~/.claude/projects/-Users-hisatokawaji-dev-lodan/memory/`（`lodan-2026-09-trend-gaps.md` が今回の全記録）

## 9. 次にやるなら

- **2026-10-08〜09**: PR #28 / #60（古い eval docs PR）はユーザー判断で close。**#136**（#131 close: `/compact` 後の transcript 欠落。`Session::compacted_away` の累計で `Recorder::sync_with` が `persisted` を縮めて追従、transcript は原文を残し要約は書かない。レビューで `compact_boundary` が lodan 注入の user メッセージを数える問題 → `[lodan] ` 印 + `is_harness_note` で除外）、**#137**（#77 提案 5 前半: `isolation: worktree`。レビューで deny すり抜けが出た → 子のファイルツールを worktree に閉じ込め `permission_rules::escapes_root`、ゲートのルール照合を子の cwd 基準 `decide_for_subagent(.., cwd)` / `assess_in` / `decide_quietly_in`）、**#138**（#77 提案 5 後半: `run_in_background`（REPL だけ。結果は次のターンの入口で `BgStore::drain_announcements` → `pending_context`。背景の子は尋ねられないので `decide_quietly_in` で決まるものだけ通す）と `resume: "agent_N"`（`children` に会話・cwd・worktree を記録、続きを積む））をマージ。**#77 の提案 1〜5 は全て実装済み**（残: sandbox `workspace-write` で worktree 内 `git commit` が git dir の位置の都合で失敗する件）。pr-reviewer（opus）が 2 回 stall したので sonnet 指定で回した
- **2026-10-11「#65 を実装」**: Phase 1 を 1 項目 1 PR で。**#144**（B1/B2 要件台帳: `src/agent/requirements.rs` / `src/tools/requirements.rs`、`Session::extract_requirements` → `refresh_system_prompt` の pinned block → 最終応答前の `REQUIREMENTS_NUDGE` 最大 2 回。`ToolCtx.requirements` を共有）、**#145**（A2 `write_max_lines`: `ToolCtx.write_max_lines`、Write が拒否して骨格 + Edit の手順）をマージ。**#146**（`Requirements` の `ids` 複数、レビュー中）。ラダー実測で req が L0 で往復 3〜4 回増 → 上限超え。**ablation は cargo と重ねない**（Ollama が数倍遅くなる）。worktree `wt-65`（release バイナリ、ladder 用）と `wt-ids` が残っている。次: #146 マージ → main の release ビルド → ladder（`think-none req-none split-none`、`T_L0=300 T_L1=480 T_L2=720`、REPEAT=1）を idle で回す → `docs/eval/ladder/phase1-2026-10.md` → A1
- **2026-10-10 再開**: #140（#123 close）・#141（#83 nit）・#142（`tools/list_changed`、`Session::refresh_mcp_tools` がターンの入口で registry の写しを差し替え、`/tools` `/status` は `session.registry()`。readonly ではツール単位で破壊的ツールを隠す）・#143（elicitation、`src/mcp/elicitation.rs`、REPL で stdin から 1 項目ずつ、`-p` は decline）を全てマージ。**main = `705a5d5`**。#72 を実測つきで close（mock MCP ×8: `tool_search` で定義 20.6 KB → 12.9 KB）。#139 起票。**残 issue**: #83（OAuth / HTTP GET stream のみ）、#139、#38（低優先）、#85 / #65（tracking）。open PR は #69 だけ。worktree 全撤去
- **2026-10-09 中断**（ユーザー「一旦中断してメモリに退避」）: 「つづきをやっておいて」で #123 に着手し途中。worktree `wt-123` / ブランチ `fix/mcp-error-redaction` にローカルコミット `649432f`（未 push・PR 未作成）。`redact_quoted` / `describe_toml_error` を `pub(crate)` にし、`.mcp.json`（`load_from` / `read_json_or_empty`）・`trusted.toml`（`read_store`）・レイヤー合成（`from_layers`）のエラー表示で値を伏せる + テスト 3 本。`cargo test` 596 green だが **clippy が `src/trust.rs:256` の `#[test]` 重複で落ちる** → 直して push → PR → レビュー → マージ → #123 close。その後は #83 残り nit（`read_resource` の上限、`write_json` tmp→rename）→ #83 提案 4（list_changed / elicitation）。#139（sandbox で worktree 内 commit 不可）を起票済み
- ~~優先度順の候補: #81 → #79 → #77 → #80 → #83~~ → **2026-10-04 時点で P2 の実装分は全て完了**。残り候補（ユーザー未承認、勝手に始めない）: ~~#77 提案 5~~（#137 / #138 で完了）、**#83** 残り（`list_changed` / elicitation / OAuth、`read_resource` の上限、`write_json` の tmp→rename）、**#72** の MCP 多数構成の実測、**#123**、**#131**（/compact 後の transcript 欠落）、**#38**。docs の小物: policy §2「WARN 1 件」の単位（レビュアー提案）、`--yes` が `permission_mode` を変えない点の README 明記、子の PostToolUse payload（`tool_output` / `error`）と親子キー集合一致を固定するテスト（#134 の 3 周目レビューの任意指摘）。

## 10. 2026-10-12 の状態（#65 Phase 1-3）

**main = `9289a28`**。open PR は **#154**（C1）と触らない #69 だけ。

- マージ済み: **#147** A1（`tool_choice=required` + `Finish`）/ **#148** Phase 1 ablation レポート / **#149** D1（`[[verify]]`）/ **#150** D2（編集直後の構文チェック）/ **#151** Phase 2 ablation レポート / **#152** A2 の修正（`Write { append: true }`）/ **#153** E（モデル別プリセット + `lodan presets`）/ **#155**（プリセットの由来を `--show-origin` に出す）
- **ablation の結論**: A1 だけが勝ち（チェック通過 96%、時間同等）。B1 は正答同じで時間 +37〜146%。A2 は上限 40 で悪化（→ 追記に変えたのが #152）。D1/D2 は発火しても仕事が無かった（9B の失敗は「構文は正しく実行も通るが仕様が違う」）。レポートは `docs/eval/ladder/phase1-2026-10.md` / `phase2-2026-10.md`
- **#154 (C1 Plan→Execute)**: `[agent] plan_execute`。依頼を LLM 1 回で手順（最大 6）に割り、手順ごとに `Task` を普通のツール呼び出しとして実行。手順の実行は `plan-step`（`plan_execute` が on のときだけ登録。ツールは**親に見えている範囲と同じ**= `agent::plan::step_tools`）。親に残るのは `[lodan] plan→execute: …` の注記 1 つ。ラダーは `plan-none`
- **レビューで 2 回出た型**（メモリ `lodan-capability-not-obscurity`）: ①「モデルに見せない」を権限の境界にしない（`subagent_type` の enum は助言。`mark_read` も同じ型）②単体が正しくても配線は無テストで壊れる（**mutation で検出力を確かめさせる**）
- **次**: #154 マージ → **C1 の ablation**（`bash .claude/../scratchpad/phase3-ablation.sh` 相当: `CONFIGS="think-none plan-none" TASKS="L2-*" REPEAT=3 T_L2=1800`、release は worktree `wt-c1` にビルド済み）。**時間は turn 単位では読めない**（`plan_*` が `turn_start` の外に出る）ので実行全体の `active_ms` と `usage.by_kind` / `plan_step.ms` で見る。あわせて Phase 1 で約束した `split40-none` の再実行（追記版の誘導で 9B が完走するか）と、副作用の無い `VERIFY` での D1 再測定
- 宿題: `full` のツール定義が 6,775 B になっており README / `presets.rs` の `6,643 → 3,065 (-54%)` と合わない（測り直して 3 箇所同時に直す）

### 10.1 Phase 3 の実測と #156（2026-10-12 時点の中断点）

- C1 の ablation = **36 実行**（18 × 2）。レポートは `docs/eval/ladder/phase3-2026-10.md`（PR **#156** に同梱）。生データは scratchpad の `phase3-eval`（round 2）と `phase3-eval-round1`
- round 1 で見つけた欠陥: 手順の子が調査用上限 12 に縛られ、分解 4 実行のうち 3 が `hit max_iterations (12)`（1 手順 380-475 秒）→ `AgentProfile::for_plan_step` で親の `max_iterations` を使う
- round 2: 手順の失敗 0 になったが**効果なし**。分解は 3/9（9B は「手順 1 つ」と返す。プロンプトに規則を書いても 4/9 → 3/9）。分解された 3 実行は正答同じ・時間 1.5-3.7 倍で、増えたぶんの主役は**手順後の親の検算**（基準 4 回 → 7-14 回、親トークン 14.1k → 27.9-66.0k）
- **#156 のレビュー対応が途中**（REQUEST_CHANGES、止めているのは docs の 2 点）:
  - 済: `with_iteration_budget` → `AgentProfile::for_plan_step` に置換（死に引数も解消）、`.lodan/agents/plan-step.md` 経路でも予算を引き継ぐ（`build_agent_profile` で名前を見る）+ テスト。`cargo test --lib runtime` 2 passed
  - **未**: ①README / レポートの「全体の予算が抑える」を訂正（`max_requests` / `max_total_tokens` は既定 `None` = 無制限。既定の歯止めは手順数上限 6 だけ ＝ 最悪 6×25 の子往復）②レポートの「Bash の失敗 6 回」→ **5**（`tool_errors: 5`）③round 1/2 で条件が 2 つ変わった交絡を「分離できない」と明記 ④基準ドリフト（`think-none` 中央値 120s → 164s、L2-cli-json 98/85/119s → 469/310/328s）を 1 行入れ、`22% → 56%` を修正の効果と読ませない ⑤「`plan_skipped` は基準と同一コードパス」→ 計画 1 回（512-733 tok）は払っていると言い換え ⑥見出しを「便益は測れない / 費用は測れている」に分ける
  - そのあと: fmt・clippy・test → commit → push → `gh pr comment 156` → 再レビュー（同じ安全条項）→ マージ → worktree `wt-c1` 撤去
