# PR レビュー ポリシー（v0.1）

- 最終更新: 2026-06-24
- ステータス: Draft v0.1
- 関連:
  - レビュー Agent: [`.claude/agents/pr-reviewer.md`](../../.claude/agents/pr-reviewer.md)
  - 手動起動コマンド: [`.claude/commands/pr-review.md`](../../.claude/commands/pr-review.md)
  - 初回レビュー loop: [`.claude/commands/loop-pr-review.md`](../../.claude/commands/loop-pr-review.md)
  - 議論進行 loop: [`.claude/commands/loop-pr-discussion.md`](../../.claude/commands/loop-pr-discussion.md)
- 対象読者: 開発者本人 + Claude Code

---

## 0. このドキュメントの目的

lodan の PR に対する **自動レビューと人間レビューの併用ガードレール** を定義する。レビュー観点をここに集約し、改訂をこのドキュメントで行えるようにする。`.claude/agents/pr-reviewer.md` は本書 §1 を **source of truth として参照** し、agent 側でロジックを増殖させない。

lodan の特徴 (個人開発・Rust CLI・ローカル LLM ↔ Sakana provider) に合わせ、Web サービスやチーム運用を前提とした観点は除外する。

---

## 1. レビュー観点（v0.1）

すべての PR に対して、以下 6 観点を **PASS / WARN / FAIL** の 3 値で評価する。

| # | 観点 | 概要 | 主要チェックポイント |
|---|---|---|---|
| 1 | **動作する** | 変更が実際に意図通り動くか | `cargo build` / `cargo test` 通過、`tests/e2e_mock.rs` グリーン、REPL 起動 (`echo /exit \| cargo run -- ...`) でバナーまで届く、tool calling が破綻していない |
| 2 | **MVP スコープ整合** | 既存ロードマップと境界を守っているか | README §ロードマップ / `memory/project_roadmap.md` との整合、MVP 外スタブ (`hooks/`, `mcp/`, `skills/`, `slash/`, `session.rs`) の `unimplemented!()` を意図せず外していないか、`tools/registry.rs` のコメントアウト境界を尊重 |
| 3 | **Rust 規約・コード品質** | Rust + 本リポの慣行に準拠か | `cargo fmt --check` / `cargo clippy -D warnings` 相当、`anyhow` (アプリ層) と `thiserror` (ライブラリ層 / `tools::ToolError`) の使い分け、`async_trait` 整合、`Tool` trait (name/description/schema/is_destructive/execute) 規約、`ToolCtx` を経由した read_tracker / todos 操作、`println!` ではなく必要時のみ `tracing` |
| 4 | **CI** | GitHub Actions が緑か | `.github/workflows/*` 上の `test` ジョブ通過、push / pull_request 両 trigger が壊れていない、`cargo test` 全数 pass |
| 5 | **見落とし** | レビュー対象として見落とされがちな点 | 新規ツール追加時に `tools::registry::default_registry` 登録漏れ、新規ツールの `is_destructive` 設定漏れ、permission gate (`PermissionGate`) のバイパス、`Read` 必須ガード (`ToolCtx::was_read`) の欠落、`Config` スキーマ変更時の README / e2e test 追従漏れ、`.env` / `LODAN_*` env の整合性、`println!`/`dbg!` のデバッグ残骸 |
| 6 | **セキュリティ** | セキュリティリスク | API キー漏洩 (`.env`/`.fish_api` の git 追跡、ログ出力、エラーメッセージ)、`.gitignore` の網羅、Bash ツールのコマンドインジェクションリスク・auto_approve 暴走、`Read`/`Write`/`Edit` の絶対パス / シンボリックリンク経由の path traversal、provider 経由の任意 URL 接続 (LODAN_BASE_URL 妥当性)、MCP 経由で外部プロセス起動する場合の引数検証 |

### 1.1 各観点の評価基準

- **PASS**: 観点に対し問題なし、または PR body で明示的に説明されている
- **WARN**: 改善余地ありだが merge 阻止までは要さない（コメント残し）
- **FAIL**: 設計違反・テスト失敗・セキュリティ問題など、merge 前に修正必須

---

## 2. 総合判定

集計の判定ルール:

- **APPROVE**: 6 観点すべて PASS、または PR body で明示的に waiver された WARN のみ
- **REQUEST_CHANGES**: いずれかが FAIL、または重大な WARN が複数（reviewer 裁量）
- **NEEDS_DISCUSSION**: 設計判断・スコープ判断・トレードオフ判断が必要で、レビュー観点では結論を出せない

自動レビューは **コメントのみ**（GitHub の Approve / Request changes は **人間 (最終的に `loop-pr-discussion` の自動 approve も含む) の専有**）。

---

## 3. 自動実行 — Claude Code セッション内の 2 つの loop

自動レビューは **アクティブな Claude Code セッション内** で動かす（GitHub Actions ではない）。理由:

- API キーを Repo Secret に置く必要がない
- レビュー判断が Claude Code 全体の文脈（このドキュメント / README / memory）を反映できる
- セッションの料金体系で動く（per-PR の従量課金ではない）

役割を分けた **2 つの loop** を併走させる:

| Loop | Cadence | 役割 |
|---|---|---|
| `loop-pr-review` | **12 時間（半日）** | 未レビュー head に対する **初回レビュー** |
| `loop-pr-discussion` | **5 分** | 既レビュー PR の **議論進行 + 最終判定（approve まで）** |

両者は SHA marker (`<!-- pr-reviewer: <sha> -->`) と timestamp で **責務が重ならない**。

### 3.1 起動

セッションを始めたら 2 つを 1 度ずつ:

```text
/loop 12h /loop-pr-review
/loop 5m  /loop-pr-discussion
```

- 初回レビューは半日 1 回で十分（非緊急）
- 議論は 5 分間隔で polling して、すばやく往復を成立させる
- 緊急で初回をすぐ走らせたいときは `/pr-review <PR#>`

### 3.2 cadence の選び方

#### loop-pr-review（初回レビュー）

| 間隔 | 用途 |
|---|---|
| `/loop 12h`（推奨） | 通常運用。半日 1 回のスイープ。アイドルコスト最小 |
| `/loop 6h` | 1 日 4 回。やや snappier |
| `/loop 1h` | 集中開発日。1 時間以内に feedback が欲しい |
| 10m 以下 | rapid iteration 中。代わりに `/pr-review <PR#>` を使うほうが効率的 |

#### loop-pr-discussion（議論進行 + 自動修正）

| 間隔 | 用途 |
|---|---|
| `/loop 5m`（推奨） | 通常運用。push / 返信から 5 分以内に進行 |
| `/loop 2m` | 集中ペアプロ的に詰めたいとき |
| `/loop 15m` | 軽めの並行作業。やや lag |

---

## 4. このポリシーの改訂

- 観点を増やす・変える: この `docs/policy/pr-review.md` を別 PR で更新し、`pr-reviewer` agent はリンクを介してそれを読みに来る
- 観点をハードコードしない (`.claude/agents/pr-reviewer.md` 側に書かない)
- 改訂 PR 自体には `skip-claude-review` ラベルを付けると、policy doc 改訂のメタ循環を避けられる
