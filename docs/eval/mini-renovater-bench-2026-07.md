# lodan × fugu / ローカル LLM「PoC Renovater モック構築」能力検証 (2026-07)

PoC Renovater(poc-foundry-agy: PoC ZIP を解析 → GitHub 登録 → Issue 起票 → 実装 PR → レビュー → マージまで行う Agentic AI プロダクト、FastAPI + Next.js)**相当のローカルモックを、lodan + LLM がどこまで自走で作り込めるか**を測った記録。単発タスクの能力検証([fugu-eval-2026-06-26](fugu-eval-2026-06-26.md)、10/10 PASS)の上位版で、「複数コンポーネントのプロダクトを段階的に組み上げる力」を測る。

## 結論

| ステージ | fugu (360s) | qwen3.5:9b (720s) | gemma4:12b (720s) | qwen3.5:9b (2400s) | gemma4:12b (2400s) |
|---|---|---|---|---|---|
| S1 Analyze | **PASS** 37s | **PASS** 556s | FAIL 601s | FAIL 938s | **PASS** 466s |
| S2 Register | **PASS** 151s | TIMEOUT | TIMEOUT | FAIL 1844s | FAIL 1023s |
| S3 Plan | **PASS** 61s | TIMEOUT | FAIL | FAIL | FAIL 1367s |
| S4 Implement | **PASS** 103s | TIMEOUT | TIMEOUT | FAIL 1696s | FAIL |
| S5 Review+API | **PASS** 144s | FAIL | FAIL | FAIL 1240s | FAIL |
| **到達** | **5/5 (計 ~8 分)** | 1/5 | 0/5 | 0/5 | 1/5 |

1. **fugu は完全自走で 5/5**。789 行の `renovater.py`(CLI 6 サブコマンド + stdlib HTTP API)を積み上げ、モック GitHub(bare git)への merge、API 経由の approve → merge の E2E まで通した。**プロダクト規模のモック構築は fugu + lodan で実証済み**。
2. **ローカル 9B/12B はタイムアウト延長(720s→2400s)でも改善しなかった**。qwen3.5:9b は延長版で S1 が逆に悪化(`renovater.py` を `renovator.py` と綴り違いで生成・仕様外に多数ファイル作成)、gemma4:12b は同一ファイルの Read ループや SSE エラーで停滞。**壁は生成速度ではなく、多段の累積タスクで仕様に忠実であり続ける能力**。同一条件でも PASS/FAIL が揺れる分散の大きさも確認。
3. qwen3-coder:30b (MoE A3B) は M3 Air 24GB では**マシン全体が重くなり実用不可**と判断し中断(品質未測定)。

## 検証設計

実物の外部依存をすべてローカルモックに置換した **Python 3 stdlib のみ**(pip 禁止 = 判定を密閉的に)の CLI + HTTP API を、5 ステージの**累積ビルド**(各ステージは前ステージの成果物の上に積む)で構築させる。

| 実物 | モック指示 |
|---|---|
| Gemini エージェント | ルールベース解析 (package.json 検出等) |
| GitHub (ScmPort) | ローカル bare git repo (`state/scm/*.git`) |
| Firestore (AgentRepository) | JSON ファイル (`state/agents/*.json`) |
| Pub/Sub | 同期実行 |
| 入力 PoC | poc-foundry-agy の `sample/todo-app`(node_modules 除外コピー) |

- 1 ステージ = 1 lodan 呼び出し(headless: `printf '<1行仕様>\n/exit\n' | timeout N lodan --provider … --yes`)。ステージ間の文脈はファイル(前ステージの成果物を read させる)で受け渡す — 実物の「ステートレスなエージェントが成果物で連携する」構造の再現でもある。
- 各ステージに機械判定の verify コマンド(exit 0 = PASS)。S5 は実際にサーバを起動し curl で E2E。
- ハーネス: [mini_renovater_bench.sh](mini_renovater_bench.sh)(プロンプト全文・判定コマンドを同梱)。

## 再現手順

```bash
cargo build --release
mkdir bench && cd bench
cp <lodan>/docs/eval/mini_renovater_bench.sh .
mkdir fixtures-src && rsync -a --exclude node_modules --exclude .next <poc-foundry-agy>/sample/todo-app fixtures-src/

# fugu
export SAKANA_API_KEY=sk-…
MODEL=fugu PROVIDER=sakana TIMEOUT=360 bash mini_renovater_bench.sh

# ローカルモデル (例): num_ctx を Modelfile 派生で確保してから
printf 'FROM qwen3.5:9b\nPARAMETER num_ctx 16384\n' > Modelfile && ollama create qwen35-9b-16k -f Modelfile
MODEL=qwen35-9b-16k PROVIDER=local LABEL=qwen3.5-9b TIMEOUT=720 bash mini_renovater_bench.sh
```

## 運用上の教訓(ローカルモデルをベンチする人へ)

- **cold start**: 大型モデルの初回ロードが lodan の `timeout_secs`(既定 120)を超え全滅する。ハーネスは (a) 作業 cwd の `.lodan/config.toml` で 600s に引き上げ、(b) ベンチ前に `keep_alive` 付きでプリウォームする。
- **num_ctx**: ollama 既定 4096 では lodan の system prompt + 14 ツール定義が溢れる。Modelfile 派生(`FROM x` + `PARAMETER num_ctx 8192-16384`)で固定する。
- **gemma4 系は ollama 0.32+ が必要**(0.22 では manifest 412)。
- 長時間ベンチは nohup + `caffeinate -is -w <pid>`(スリープ抑止)+ results.tsv によるフェーズ・レジュームで回すと、セッション断・スリープに耐える。

## 生データ

実行時の transcript・生成物一式(fugu 製の動くモック `renovater.py` 含む)は `~/dev/mini-renovater-bench/` に退避済み(`runs/<モデル>/` 配下、リポジトリには含めない)。

- 環境: MacBook Air M3 / 24GB、ollama 0.32.1(検証中に 0.22.1 から更新)、lodan main(217 tests 時点)
- 実行期間: 2026-07-18〜07-23(ローカルモデルはマシンスリープ・リソース配慮で断続実行)
