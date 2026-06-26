#!/usr/bin/env bash
# lodan × fugu 能力検証ハーネス。
# 10 個のアプリ開発タスクを、仕様を明示した ReAct 風プロンプトで lodan に実行させ、
# 生成物を「仕様準拠コマンドが通るか」で合否判定する。
#
# 使い方:
#   cargo build --release
#   export SAKANA_API_KEY=sk-...        # または .env を作業 cwd に置く
#   LODAN=./target/release/lodan EVAL_ROOT=$(mktemp -d) bash docs/eval/fugu_eval_harness.sh
set -u

LODAN="${LODAN:-./target/release/lodan}"
ROOT="${EVAL_ROOT:-$(mktemp -d)}"
PER_TASK_TIMEOUT="${PER_TASK_TIMEOUT:-240}"
PROVIDER="${PROVIDER:-sakana}"
MODEL="${MODEL:-fugu}"

if [ -z "${SAKANA_API_KEY:-}" ]; then
  echo "SAKANA_API_KEY is not set (export it, or place a .env in the cwd)" >&2
  exit 1
fi

RESULTS="$ROOT/results.tsv"
REACT="You are a coding agent. First state a one-line plan, then implement it using the file tools (Write/Edit), and you MAY run Bash to verify. Implement EXACTLY the interface below, creating only the specified file(s). Spec: "

mkdir -p "$ROOT"
printf 'name\tstatus\ttools\tnote\n' > "$RESULTS"

run_one() {
  local name="$1" prompt="$2" verify="$3"
  local dir="$ROOT/$name"; mkdir -p "$dir"
  if declare -F "prep_$name" >/dev/null; then ( cd "$dir" && "prep_$name" ); fi
  local log="$dir/transcript.log"
  ( cd "$dir" && printf '%s\n/exit\n' "$REACT$prompt" \
      | timeout "$PER_TASK_TIMEOUT" "$LODAN" --provider "$PROVIDER" --model "$MODEL" --yes ) \
      > "$log" 2>&1
  local rc=$?
  local tools
  tools=$(grep -oE '^\[(Write|Edit|MultiEdit|Read|Bash|Glob|Grep|NotebookEdit)\]' "$log" 2>/dev/null \
          | tr -d '[]' | sort | uniq -c | awk '{printf "%s:%s ", $2, $1}')
  local status note
  if [ $rc -eq 124 ]; then
    status="TIMEOUT"; note="hit ${PER_TASK_TIMEOUT}s"
  elif ( cd "$dir" && bash -c "$verify" >/dev/null 2>&1 ); then
    status="PASS"; note="spec verified"
  else
    status="FAIL"; note="verify failed"
  fi
  printf '%s\t%s\t%s\t%s\n' "$name" "$status" "${tools:-none}" "$note" >> "$RESULTS"
  echo ">> $name : $status ($tools)"
}

# ---- fixtures ----
prep_wordfreq() { printf 'the cat sat on the mat the\n' > words.txt; }
prep_csv_stats() { printf 'value\n10\n20\n30\n' > data.csv; }
prep_json_validator() { printf '{"name":"x","port":8080}\n' > good.json; printf '{"name":"x"}\n' > bad.json; }

# ---- scenarios (single-line prompts) ----
run_one todo_cli \
  "Create todo.py, a CLI. 'python3 todo.py add <text>' appends a todo string to todos.json (a JSON array). 'python3 todo.py list' prints each todo on its own line prefixed with '- '. Persist across runs." \
  'rm -f todos.json; python3 todo.py add "buy milk" >/dev/null 2>&1; python3 todo.py add "walk dog" >/dev/null 2>&1; python3 todo.py list 2>/dev/null | grep -q "buy milk" && python3 todo.py list 2>/dev/null | grep -q "walk dog"'

run_one expr_eval \
  "Create calc.py. 'python3 calc.py \"<expr>\"' prints the result of an arithmetic expression supporting + - * / and parentheses with correct precedence, WITHOUT using eval or exec. Print 14 for 2+3*4." \
  '[ "$(python3 calc.py "2+3*4" 2>/dev/null)" = "14" ] && [ "$(python3 calc.py "(2+3)*4" 2>/dev/null)" = "20" ]'

run_one wordfreq \
  "Create wordfreq.py that reads words.txt (whitespace-separated words) and prints the single most frequent word (lowercased)." \
  'python3 wordfreq.py 2>/dev/null | grep -qiw the'

run_one caesar \
  "Create caesar.py. 'python3 caesar.py encrypt <shift> <text>' prints the Caesar-cipher ciphertext; 'python3 caesar.py decrypt <shift> <text>' reverses it. Shift letters only, keep case, leave other chars unchanged." \
  'enc=$(python3 caesar.py encrypt 3 hello 2>/dev/null); [ -n "$enc" ] && [ "$(python3 caesar.py decrypt 3 "$enc" 2>/dev/null)" = "hello" ]'

run_one fizzbuzz \
  "Create fizzbuzz.py that prints 1..100 one per line, but 'Fizz' for multiples of 3, 'Buzz' for multiples of 5, and 'FizzBuzz' for multiples of 15." \
  'python3 fizzbuzz.py 2>/dev/null | sed -n "15p" | grep -qix fizzbuzz && python3 fizzbuzz.py 2>/dev/null | sed -n "5p" | grep -qix buzz'

run_one csv_stats \
  "Create stats.py that reads data.csv (a header row 'value' then one number per line) and prints the arithmetic mean of the value column as a number." \
  'out=$(python3 stats.py 2>/dev/null); python3 -c "import sys; assert abs(float(sys.argv[1])-20.0)<0.01" "$out"'

run_one temp_unittest \
  "Create temperature.py with celsius_to_fahrenheit(c) and fahrenheit_to_celsius(f), and test_temperature.py using unittest with tests for both (e.g. 0C=32F, 100C=212F)." \
  'python3 -m unittest test_temperature 2>&1 | grep -q "OK"'

run_one stack_ds \
  "Create stack.py with a class Stack: push(x), pop() returns and removes the top (raise IndexError if empty), peek() returns top without removing, is_empty() returns bool, and __len__." \
  'python3 -c "from stack import Stack; s=Stack(); s.push(1); s.push(2); assert len(s)==2; assert s.pop()==2; assert s.peek()==1; assert not s.is_empty()"'

run_one md2html \
  "Create md2html.py that reads Markdown from stdin and writes HTML to stdout: a line starting with '# ' becomes <h1>...</h1>; inline '**text**' becomes <strong>text</strong>." \
  'out=$(printf "# Title\nhello **world**\n" | python3 md2html.py 2>/dev/null); echo "$out" | grep -q "<h1>Title</h1>" && echo "$out" | grep -q "<strong>world</strong>"'

run_one json_validator \
  "Create validate.py. 'python3 validate.py <file.json>' prints 'valid' and exits 0 if the JSON object has a string field 'name' and an integer field 'port' in 1..65535, otherwise prints 'invalid' and exits 1." \
  'python3 validate.py good.json 2>/dev/null | grep -qi valid && ! python3 validate.py bad.json >/dev/null 2>&1'

echo "=== DONE (artifacts under $ROOT) ==="
column -t -s "$(printf '\t')" "$RESULTS"
PASS=$(grep -c $'\tPASS\t' "$RESULTS"); TOTAL=$(($(wc -l < "$RESULTS")-1))
echo "SCORE: $PASS / $TOTAL passed"
