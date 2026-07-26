#!/usr/bin/env bash
# 難易度ラダー: 小さなタスクから順に「どの粒度で壊れるか」を測る評価ハーネス。
#
# mini-renovater ベンチが 0/5 の二値でしか答えられなかった問題への対処として、
# (a) 難易度を L0-L2 に刻み、(b) 判定をチェック項目単位にして部分点を出し、
# (c) 同一条件を複数回まわして分散を見る。ablation 用に緩和策の組み合わせ
# (CONFIGS) を切り替えて同じタスクを走らせられる。
#
# 使い方:
#   MODEL=llama3.1:8b PROVIDER=local bash ladder.sh
#   TASKS="L0-write L1-fix-bug" CONFIGS=mitig REPEAT=1 bash ladder.sh   # 速い反復
#   MODEL=fugu PROVIDER=sakana CONFIGS=mitig bash ladder.sh             # 参照実装
#
# 結果は results.jsonl へ 1 実行 1 行で追記され、同じ key の行があるものは
# スキップされる (中断しても再開できる)。表にするには summarize.py。
set -u

HERE="$(cd "$(dirname "$0")" && pwd)"
LODAN="${LODAN:-$HERE/../../../target/release/lodan}"
PROVIDER="${PROVIDER:-local}"
MODEL="${MODEL:-llama3.1:8b}"
LABEL="${LABEL:-$MODEL}"
REPEAT="${REPEAT:-3}"
CONFIGS="${CONFIGS:-base temp mitig nudge}"
RESULTS="${RESULTS:-$HERE/results.jsonl}"
RUNS_DIR="${RUNS_DIR:-$HERE/runs}"
# レベル別の実行時間上限 (秒)。生成の遅いローカルモデルでは引き上げる。
T_L0="${T_L0:-120}"
T_L1="${T_L1:-240}"
T_L2="${T_L2:-480}"
# 1 チェックあたりの上限 (サーバ起動を伴う L2 のため少し長め)。
CHECK_TIMEOUT="${CHECK_TIMEOUT:-60}"

if [ ! -x "$LODAN" ]; then
  echo "lodan binary not found at $LODAN (cargo build --release)" >&2
  exit 1
fi

# 各実行は作業ディレクトリへ cd するため、外から渡されたパスは先に絶対化する。
abspath() { python3 -c 'import os,sys; print(os.path.abspath(sys.argv[1]))' "$1"; }
LODAN="$(abspath "$LODAN")"
RESULTS="$(abspath "$RESULTS")"
RUNS_DIR="$(abspath "$RUNS_DIR")"

# 対象タスク: 指定がなければ tasks/ の全て。
if [ -n "${TASKS:-}" ]; then
  TASK_FILES=""
  for t in $TASKS; do
    if [ -f "$HERE/tasks/$t.sh" ]; then
      TASK_FILES="$TASK_FILES $HERE/tasks/$t.sh"
    else
      # レベル名 (L0 等) の前方一致も許す。
      for f in "$HERE"/tasks/"$t"*.sh; do
        [ -f "$f" ] && TASK_FILES="$TASK_FILES $f"
      done
    fi
  done
else
  TASK_FILES="$(ls "$HERE"/tasks/*.sh)"
fi

mkdir -p "$RUNS_DIR"
touch "$RESULTS"

# ablation の各水準。base は緩和策を全て切った素の状態で、以降 1 つずつ積む。
# (mini-renovater ベンチの v1 → v2 → v4 に対応する)
flags_for_config() {
  case "$1" in
    base)  echo "--malformed-retry=false --dup-suppress=false --finish-nudge=false" ;;
    temp)  echo "--temperature 0.2 --malformed-retry=false --dup-suppress=false --finish-nudge=false" ;;
    mitig) echo "--temperature 0.2 --malformed-retry=true --dup-suppress=true --finish-nudge=false" ;;
    nudge) echo "--temperature 0.2 --malformed-retry=true --dup-suppress=true --finish-nudge=true" ;;
    *) echo "unknown config: $1" >&2; return 1 ;;
  esac
}

# 綴り違いの config が既定フラグで静かに走らないよう、先に全て検証する。
for c in $CONFIGS; do
  flags_for_config "$c" >/dev/null || exit 1
done

timeout_for_level() {
  case "$1" in
    L0) echo "$T_L0" ;;
    L1) echo "$T_L1" ;;
    L2) echo "$T_L2" ;;
    *)  echo 300 ;;
  esac
}

json_escape() {
  python3 -c 'import json,sys; sys.stdout.write(json.dumps(sys.stdin.read()))'
}

# ローカルモデルのコールドロードは lodan の timeout を超えがちなので先に温める。
if [ "$PROVIDER" = "local" ]; then
  echo "prewarming $MODEL ..."
  curl -s --max-time 900 http://localhost:11434/api/generate \
    -d "{\"model\":\"$MODEL\",\"prompt\":\"hi\",\"stream\":false,\"keep_alive\":\"60m\"}" >/dev/null \
    && echo "prewarm done" || echo "prewarm FAILED (continuing)"
fi

# タスク側から呼ばれる。実行結果を CHECK_* に積むだけで、判定はしない。
check() {
  local name="$1" cmd="$2"
  CHECK_TOTAL=$((CHECK_TOTAL + 1))
  if timeout "$CHECK_TIMEOUT" bash -c "$cmd" >>"$CHECK_LOG" 2>&1; then
    CHECK_PASS=$((CHECK_PASS + 1))
  else
    CHECK_FAILED="$CHECK_FAILED $name"
  fi
}

total_runs=0
for task_file in $TASK_FILES; do
  for config in $CONFIGS; do
    for run in $(seq 1 "$REPEAT"); do
      task="$(basename "$task_file" .sh)"
      key="$LABEL/$config/$task/$run"
      if grep -qF "\"key\":\"$key\"" "$RESULTS" 2>/dev/null; then
        echo ">> skip (done): $key"
        continue
      fi
      total_runs=$((total_runs + 1))

      # 1 実行を丸ごとサブシェルに閉じ込め、タスク定義が次の実行へ漏れないようにする。
      (
        set -u
        # shellcheck disable=SC1090
        . "$task_file"

        work="$RUNS_DIR/$LABEL/$config/$task/$run"
        rm -rf "$work"; mkdir -p "$work"
        runlog="$work/run.jsonl"
        stdout_log="$work/stdout.log"
        CHECK_LOG="$work/checks.log"
        : >"$CHECK_LOG"

        # LLM リクエストのタイムアウトは作業 cwd の設定で引き上げる
        # (ローカルモデルの生成は既定 120s を超える)。
        mkdir -p "$work/.lodan"
        printf '[llm.local]\ntimeout_secs = 900\n\n[llm.sakana]\ntimeout_secs = 900\n' \
          > "$work/.lodan/config.toml"

        cd "$work" || exit 1
        setup

        limit="$(timeout_for_level "$LEVEL")"
        t0=$(date +%s)
        # shellcheck disable=SC2086
        printf '%s\n/exit\n' "$PROMPT" \
          | timeout "$limit" "$LODAN" \
              --provider "$PROVIDER" --model "$MODEL" --yes \
              --log-jsonl "$runlog" $(flags_for_config "$config") \
          > "$stdout_log" 2>&1
        rc=$?
        secs=$(( $(date +%s) - t0 ))

        CHECK_PASS=0; CHECK_TOTAL=0; CHECK_FAILED=""
        checks

        if [ "$CHECK_TOTAL" -gt 0 ] && [ "$CHECK_PASS" -eq "$CHECK_TOTAL" ]; then
          status=pass
        elif [ "$rc" -eq 124 ]; then
          status=timeout
        elif [ "$CHECK_PASS" -gt 0 ]; then
          status=partial
        else
          status=fail
        fi

        metrics='{}'
        if [ -s "$runlog" ]; then
          metrics="$(python3 "$HERE/runlog_metrics.py" "$runlog" 2>/dev/null || echo '{}')"
        fi
        failed_json="$(printf '%s' "${CHECK_FAILED# }" | json_escape)"

        printf '{"key":"%s","label":"%s","model":"%s","provider":"%s","config":"%s","task":"%s","level":"%s","run":%s,"status":"%s","checks_passed":%s,"checks_total":%s,"failed_checks":%s,"secs":%s,"exit_code":%s,"metrics":%s}\n' \
          "$key" "$LABEL" "$MODEL" "$PROVIDER" "$config" "$task" "$LEVEL" "$run" \
          "$status" "$CHECK_PASS" "$CHECK_TOTAL" "$failed_json" "$secs" "$rc" "$metrics" \
          >> "$RESULTS"

        echo ">> [$LABEL/$config] $task #$run : $status ($CHECK_PASS/$CHECK_TOTAL checks, ${secs}s)${CHECK_FAILED:+ failed:$CHECK_FAILED}"
      )
    done
  done
done

echo
echo "$total_runs run(s) recorded to $RESULTS"
echo "summary: python3 $HERE/summarize.py $RESULTS"
