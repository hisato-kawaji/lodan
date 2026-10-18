# L3: 段ごとに別スクリプトで、**前の段が書いたファイルだけ**を次の段が読む 3 段のパイプライン (#65)。
# 「前の段の出力の形を、後の段が覚えていて守れるか」を測る。チェックは段を切り離して
# 走らせる (生ログを消してから集計する / 手書きの中間ファイルから報告する) ので、
# 1 つのスクリプトに全部書いて辻褄を合わせた解は通らない — プロンプトがそれを明記している。
LEVEL=L3
DESC="3 段のログ処理 (パース → 集計 → 報告) を、段ごとの中間ファイルでつなぐ"

setup() {
  mkdir -p logs
  cat > logs/access.log <<'LOG'
10.0.0.1 - - [10/Oct/2026:13:55:36 +0900] "GET /index.html HTTP/1.1" 200 2326
10.0.0.2 - - [10/Oct/2026:13:55:40 +0900] "GET /about.html HTTP/1.1" 200 1500
10.0.0.1 - - [10/Oct/2026:13:56:01 +0900] "POST /login HTTP/1.1" 302 0
garbage line without the format
10.0.0.3 - - [10/Oct/2026:13:57:12 +0900] "GET /index.html HTTP/1.1" 200 2326
10.0.0.4 - - [10/Oct/2026:13:58:30 +0900] "GET /missing HTTP/1.1" 404 512
10.0.0.2 - - [10/Oct/2026:13:59:59 +0900] "GET /index.html HTTP/1.1" 304 0
10.0.0.5 - - [10/Oct/2026:14:00:03 +0900] "GET /about.html HTTP/1.1" 200 1500
another broken line
LOG
}

# #65 D1 (`verify-none`) 用。プロンプトが明記している実行方法だけを使う。
VERIFY="python3 parse.py && python3 aggregate.py && python3 report.py"

PROMPT="Write a three-stage log pipeline in the current directory, Python 3 standard library only, one script per stage. Each stage reads only the file named for it, so that the stages can run separately. (1) parse.py reads logs/access.log (common log format, e.g. 10.0.0.1 - - [10/Oct/2026:13:55:36 +0900] \"GET /index.html HTTP/1.1\" 200 2326) and writes state/events.jsonl with one JSON object per valid line, with the keys ip, method, path, status (int) and bytes (int); lines that do not match are skipped, and it prints 'skipped: N'. (2) aggregate.py reads state/events.jsonl, not the raw log, and writes state/summary.json: an object with total (number of events), bytes (sum of bytes), by_status (object mapping the status code as a string to its count) and top_paths (a list of the 3 most requested paths as objects with path and count, most requested first, ties broken by path in alphabetical order). (3) report.py reads state/summary.json, not the earlier files, and writes report.md: a first line '# Access report', then a line 'Total requests: N', then a line '## Top paths', then one line '- PATH (COUNT)' per top path. Each stage must exit with a non-zero status when its input file does not exist."

checks() {
  check parse_events     'python3 parse.py && python3 -c "import json; ev = [json.loads(l) for l in open(\"state/events.jsonl\") if l.strip()]; assert len(ev) == 7, ev; assert ev[0] == {\"ip\": \"10.0.0.1\", \"method\": \"GET\", \"path\": \"/index.html\", \"status\": 200, \"bytes\": 2326}, ev[0]"'
  check parse_skipped    'python3 parse.py | grep -q "skipped: 2"'
  check aggregate_summary 'python3 parse.py >/dev/null && python3 aggregate.py && python3 -c "import json; s = json.load(open(\"state/summary.json\")); assert s[\"total\"] == 7 and s[\"bytes\"] == 8164, s; assert s[\"by_status\"] == {\"200\": 4, \"302\": 1, \"404\": 1, \"304\": 1}, s"'
  check aggregate_top    'python3 parse.py >/dev/null && python3 aggregate.py && python3 -c "import json; s = json.load(open(\"state/summary.json\")); assert s[\"top_paths\"] == [{\"path\": \"/index.html\", \"count\": 3}, {\"path\": \"/about.html\", \"count\": 2}, {\"path\": \"/login\", \"count\": 1}], s[\"top_paths\"]"'
  check report_md        'python3 parse.py >/dev/null && python3 aggregate.py && python3 report.py && [ "$(cat report.md)" = "$(printf "# Access report\nTotal requests: 7\n## Top paths\n- /index.html (3)\n- /about.html (2)\n- /login (1)")" ]'
  # 段の切り離し: 集計は生ログを読まない (生ログを退避しても、中間ファイルから同じ結果を出す)。
  check aggregate_reads_events 'mkdir -p state && printf "%s\n" "{\"ip\": \"1.1.1.1\", \"method\": \"GET\", \"path\": \"/x\", \"status\": 500, \"bytes\": 10}" > state/events.jsonl && mv logs logs.off && python3 aggregate.py; rc=$?; mv logs.off logs; [ $rc -eq 0 ] && python3 -c "import json; s = json.load(open(\"state/summary.json\")); assert s[\"total\"] == 1 and s[\"by_status\"] == {\"500\": 1} and s[\"top_paths\"] == [{\"path\": \"/x\", \"count\": 1}], s"'
  # 報告は summary.json だけを読む (手書きの summary から報告を作れる)。
  check report_reads_summary 'mkdir -p state && rm -f state/events.jsonl && printf "%s" "{\"total\": 9, \"bytes\": 1, \"by_status\": {\"200\": 9}, \"top_paths\": [{\"path\": \"/only\", \"count\": 9}]}" > state/summary.json && python3 report.py && [ "$(cat report.md)" = "$(printf "# Access report\nTotal requests: 9\n## Top paths\n- /only (9)")" ]'
  # 各段が動くことを先に確かめてから、入力が無いときの失敗を見る。
  check missing_input_fails 'python3 parse.py >/dev/null && python3 aggregate.py && python3 report.py && rm -rf state && ! python3 aggregate.py >/dev/null 2>&1 && ! python3 report.py >/dev/null 2>&1'
}
