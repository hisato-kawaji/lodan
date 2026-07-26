# L1: 書く → 実行する → 成果物を残す。Write と Bash をまたいだ連鎖。
LEVEL=L1
DESC="スクリプトを書いて実行し、結果ファイルを作る"

setup() {
  printf 'the quick brown fox jumps over the lazy dog\n' > data.txt
}

PROMPT="Write a Python 3 script named wordcount.py in the current directory that counts the words in data.txt and writes {\"words\": <count>} as JSON to count.json. Then run it so that count.json actually exists. Use only the Python standard library."

checks() {
  check script_exists 'test -f wordcount.py'
  check output_exists 'test -f count.json'
  check count_correct 'python3 -c "import json; d=json.load(open(\"count.json\")); assert d[\"words\"] == 9, d"'
  check rerunnable    'rm -f count.json && python3 wordcount.py >/dev/null 2>&1 && test -f count.json'
}
