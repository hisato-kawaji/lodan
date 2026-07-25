#!/usr/bin/env bash
# mini-renovater ベンチ: PoC Renovater のローカルモックを lodan + LLM に
# 5 ステージ累積で作らせ、各ステージを機械判定する。
#
# 使い方:
#   MODEL=fugu PROVIDER=sakana TIMEOUT=360 bash mini_renovater_bench.sh
#   MODEL=qwen35-9b-16k PROVIDER=local TIMEOUT=720 bash mini_renovater_bench.sh
#
# 前提: fixtures-src/todo-app に入力 PoC (node_modules/.next 除外の Next.js アプリ) を
# 置くこと。ローカルモデルは Modelfile 派生で num_ctx を 8192 以上にしておくこと。
set -u

HERE="$(cd "$(dirname "$0")" && pwd)"
LODAN="${LODAN:-./target/release/lodan}"
PROVIDER="${PROVIDER:-sakana}"
MODEL="${MODEL:-fugu}"
TIMEOUT="${TIMEOUT:-360}"
LABEL="${LABEL:-$MODEL}"
ROOT="$HERE/runs/$LABEL"
RESULTS="$HERE/results.tsv"

mkdir -p "$ROOT"
[ -f "$RESULTS" ] || printf 'model\tstage\tstatus\ttools\tsecs\n' > "$RESULTS"

# fixtures: モデルごとに独立コピー
rm -rf "$ROOT/work"; mkdir -p "$ROOT/work/fixtures"
cp -R "$HERE/fixtures-src/todo-app" "$ROOT/work/fixtures/todo-app"

# LLM リクエストのタイムアウトを引き上げ (ローカル大型モデルの生成は遅い)。
# 作業 cwd の .lodan/config.toml が defaults を上書きし、--provider/--model は CLI が上書きする。
mkdir -p "$ROOT/work/.lodan"
printf '[llm.local]\ntimeout_secs = 600\n\n[llm.sakana]\ntimeout_secs = 600\n' > "$ROOT/work/.lodan/config.toml"

# ローカルモデルはコールドロードで初回リクエストが timeout し得るため、先にロードして
# ベンチ全体 (keep_alive 60m) 温めておく。
if [ "$PROVIDER" = "local" ]; then
  echo "prewarming $MODEL ..."
  curl -s --max-time 900 http://localhost:11434/api/generate \
    -d "{\"model\":\"$MODEL\",\"prompt\":\"hi\",\"stream\":false,\"keep_alive\":\"60m\"}" >/dev/null \
    && echo "prewarm done" || echo "prewarm FAILED (continuing)"
fi

REACT="You are a coding agent. First state a one-line plan, then implement using the file tools (Write/Edit), and you MAY run Bash to verify your work. Use ONLY Python 3 stdlib (no pip installs). Work in the current directory. Spec: "

run_stage() {
  local stage="$1" prompt="$2" verify="$3"
  local log="$ROOT/${stage}.log"
  local t0=$(date +%s)
  ( cd "$ROOT/work" && printf '%s\n/exit\n' "$REACT$prompt" \
      | timeout "$TIMEOUT" "$LODAN" --provider "$PROVIDER" --model "$MODEL" --yes ) \
      > "$log" 2>&1
  local rc=$?
  local secs=$(( $(date +%s) - t0 ))
  local tools
  tools=$(grep -oE '^\[(Read|Write|Edit|MultiEdit|Bash|Grep|Glob|TodoWrite|NotebookEdit|WebFetch|WebSearch|AskUserQuestion|Task)\]' "$log" 2>/dev/null \
          | tr -d '[]' | sort | uniq -c | awk '{printf "%s:%s ", $2, $1}')
  local status
  if [ $rc -eq 124 ]; then
    status="TIMEOUT"
  elif ( cd "$ROOT/work" && timeout 120 bash -c "$verify" ) > "$ROOT/${stage}.verify.log" 2>&1; then
    status="PASS"
  else
    status="FAIL"
  fi
  printf '%s\t%s\t%s\t%s\t%s\n' "$LABEL" "$stage" "$status" "${tools:-none}" "$secs" >> "$RESULTS"
  echo ">> [$LABEL] $stage : $status (${secs}s; ${tools:-no tools})"
}

# ---------- S1 Analyze ----------
S1_PROMPT="Create renovater.py, the CLI core of a local mock of a 'PoC Renovater' product (analyzes uploaded PoC codebases). Subcommand: 'python3 renovater.py analyze <poc_dir>' scans the PoC directory rule-based (e.g. package.json => nodejs; 'next' dependency => nextjs; tsconfig.json => typescript) and writes state/agents/<id>.json where <id> is the basename of <poc_dir>. The JSON must contain: id, name, status set to 'analyzed', stack (list of detected technology strings), files (integer count of files in the PoC), score (integer 0-100 deploy-readiness estimate, e.g. subtract points when Dockerfile or CI config or tests are missing). Create state directories as needed. It must work for: python3 renovater.py analyze fixtures/todo-app"
S1_VERIFY='python3 renovater.py analyze fixtures/todo-app >/dev/null 2>&1; python3 -c "
import json,sys
d=json.load(open(\"state/agents/todo-app.json\"))
assert d[\"id\"]==\"todo-app\" and d[\"status\"]==\"analyzed\"
assert isinstance(d[\"stack\"],list) and len(d[\"stack\"])>=1
assert isinstance(d[\"files\"],int) and d[\"files\"]>=10
assert isinstance(d[\"score\"],int) and 0<=d[\"score\"]<=100
print(\"ok\")"'

# ---------- S2 Register ----------
S2_PROMPT="Extend the existing renovater.py (read it first) with subcommand 'register <id>': it mocks GitHub by creating a bare git repository at state/scm/<id>.git, committing the contents of fixtures/<id> as an initial commit on branch 'main' inside that bare repo (use git via subprocess; hint: clone/init a temp work tree, copy files, commit, push to the bare repo), and updating state/agents/<id>.json: status to 'registered' and add field repo with the bare repo path. Keep all existing subcommands working. Verify with: python3 renovater.py register todo-app"
S2_VERIFY='python3 renovater.py analyze fixtures/todo-app >/dev/null 2>&1; python3 renovater.py register todo-app >/dev/null 2>&1;
n=$(git --git-dir state/scm/todo-app.git log --oneline main 2>/dev/null | wc -l | tr -d " ");
[ "${n:-0}" -ge 1 ] && git --git-dir state/scm/todo-app.git ls-tree main --name-only | grep -q package.json && python3 -c "
import json; d=json.load(open(\"state/agents/todo-app.json\")); assert d[\"status\"]==\"registered\" and d.get(\"repo\"); print(\"ok\")"'

# ---------- S3 Plan ----------
S3_PROMPT="Extend the existing renovater.py (read it first) with subcommand 'plan <id>': rule-based issue planning that writes at least 3 issue files state/issues/<id>/<number>.json, each with fields: number (int), title, body, acceptance (list of strings), status set to 'open'. Issue number 1 MUST be about containerization (adding a multi-stage Dockerfile so the app runs on Cloud Run), and one other issue MUST be about CI (adding a GitHub Actions workflow). Update the agent status to 'planned'. Keep existing subcommands working. Verify with: python3 renovater.py plan todo-app"
S3_VERIFY='python3 renovater.py plan todo-app >/dev/null 2>&1; ls state/issues/todo-app/*.json >/dev/null 2>&1 || exit 1;
count=$(ls state/issues/todo-app/*.json | wc -l | tr -d " "); [ "$count" -ge 3 ] || exit 1;
grep -qi dockerfile state/issues/todo-app/1.json || exit 1;
grep -Eqil "workflow|github actions|\bci\b" state/issues/todo-app/*.json >/dev/null || exit 1;
python3 -c "
import json,glob
for p in glob.glob(\"state/issues/todo-app/*.json\"):
    d=json.load(open(p)); assert {\"number\",\"title\",\"body\",\"acceptance\",\"status\"}<=set(d), p
d=json.load(open(\"state/agents/todo-app.json\")); assert d[\"status\"]==\"planned\"
print(\"ok\")"'

# ---------- S4 Implement ----------
S4_PROMPT="Extend the existing renovater.py (read it first) with subcommand 'implement <id> <issue_number>': it clones state/scm/<id>.git to a temporary working copy, creates branch issue-<n> from main, implements issue 1 (containerization) by writing a production-ready multi-stage Dockerfile at the repo root for a Next.js app (builder stage FROM node:20-alpine running npm ci and npm run build, runtime stage FROM node:20-alpine copying the build output, EXPOSE 3000, CMD npm start), commits it, pushes branch issue-<n> to the bare repo, and writes state/pulls/<id>/<n>.json with fields: number, issue, branch, status set to 'open', diff (the full git diff text of the branch against main). Update the agent status to 'implemented'. Keep existing subcommands working. Verify with: python3 renovater.py implement todo-app 1"
S4_VERIFY='python3 renovater.py implement todo-app 1 >/dev/null 2>&1;
git --git-dir state/scm/todo-app.git show issue-1:Dockerfile 2>/dev/null | grep -qi "node:20-alpine" || exit 1;
git --git-dir state/scm/todo-app.git show issue-1:Dockerfile 2>/dev/null | grep -qiE "^FROM .+ AS |^FROM .*node" || exit 1;
python3 -c "
import json; d=json.load(open(\"state/pulls/todo-app/1.json\"))
assert d[\"status\"]==\"open\" and d[\"branch\"]==\"issue-1\" and len(d[\"diff\"])>50
a=json.load(open(\"state/agents/todo-app.json\")); assert a[\"status\"]==\"implemented\"
print(\"ok\")"'

# ---------- S5 Review + Serve ----------
S5_PROMPT="Extend the existing renovater.py (read it first) with two subcommands. (1) 'review <id> <pr_number>': writes state/reviews/<id>/<pr_number>.json with fields pr (int), verdict ('APPROVE' if the pull's diff contains a Dockerfile change, else 'REQUEST_CHANGES'), comments (list of strings). (2) 'serve --port <port>': starts a JSON HTTP API using only http.server (ThreadingHTTPServer): GET /api/agents returns a JSON array of all agent objects; GET /api/agents/<id> returns one agent; GET /api/agents/<id>/issues returns the JSON array of its issues; POST /api/agents/<id>/pulls/<n>:approve merges branch issue-<n> into main inside the bare mock repo (do the merge via git in a temp clone and push main) then sets state/pulls/<id>/<n>.json status to 'merged' and responds {\"status\":\"merged\"}. All responses must have Content-Type application/json. Keep existing subcommands working. Verify review with: python3 renovater.py review todo-app 1"
S5_VERIFY='python3 renovater.py review todo-app 1 >/dev/null 2>&1;
python3 -c "
import json; d=json.load(open(\"state/reviews/todo-app/1.json\")); assert d[\"verdict\"]==\"APPROVE\", d" || exit 1;
(python3 renovater.py serve --port 18931 >/dev/null 2>&1 &) ; sleep 3;
ok=1;
curl -sf http://127.0.0.1:18931/api/agents | python3 -c "
import json,sys; a=json.load(sys.stdin); assert any(x[\"id\"]==\"todo-app\" for x in a)" || ok=0;
curl -sf http://127.0.0.1:18931/api/agents/todo-app/issues | python3 -c "
import json,sys; a=json.load(sys.stdin); assert len(a)>=3" || ok=0;
curl -sf -X POST http://127.0.0.1:18931/api/agents/todo-app/pulls/1:approve >/dev/null || ok=0;
sleep 1;
git --git-dir state/scm/todo-app.git show main:Dockerfile >/dev/null 2>&1 || ok=0;
python3 -c "
import json; d=json.load(open(\"state/pulls/todo-app/1.json\")); assert d[\"status\"]==\"merged\"" || ok=0;
pkill -f "renovater.py serve" >/dev/null 2>&1;
[ "$ok" = 1 ]'

run_stage S1_analyze "$S1_PROMPT" "$S1_VERIFY"
run_stage S2_register "$S2_PROMPT" "$S2_VERIFY"
run_stage S3_plan "$S3_PROMPT" "$S3_VERIFY"
run_stage S4_implement "$S4_PROMPT" "$S4_VERIFY"
run_stage S5_review_serve "$S5_PROMPT" "$S5_VERIFY"

echo "== done: $LABEL =="
grep -E "^$LABEL	" "$RESULTS" | column -t -s$'\t' 2>/dev/null || grep -E "^$LABEL" "$RESULTS"
