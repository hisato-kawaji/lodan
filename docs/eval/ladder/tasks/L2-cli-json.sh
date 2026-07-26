# L2: 要件 5 個を 1 ファイルで満たす。mini-renovater S1 の縮小版で、
# 「サブ要件が 1 個だけ落ちる」という v4 の失敗モードを部分点で捉える。
LEVEL=L2
DESC="複数要件の CLI を 1 ファイルで作る (JSON 出力つき)"

setup() {
  mkdir -p sample
  printf '# Sample\n' > sample/README.md
  printf 'print("hi")\n' > sample/main.py
  printf 'def helper():\n    return 1\n' > sample/util.py
  printf '{"a": 1}\n' > sample/data.json
}

PROMPT="Create scanner.py in the current directory, a Python 3 CLI using only the standard library. It must support: python3 scanner.py scan <dir> — which scans <dir> recursively and writes state/<basename of dir>.json containing exactly these fields: name (the directory basename, a string), files (integer count of files found, not directories), types (sorted list of distinct file extensions without the leading dot), has_readme (boolean, true if any file is named README.md). Create the state directory if it does not exist. It must work when run as: python3 scanner.py scan sample"

checks() {
  check script_exists 'test -f scanner.py'
  check runs_clean    'python3 scanner.py scan sample'
  check json_created  'test -f state/sample.json'
  check field_name    'python3 -c "import json; d=json.load(open(\"state/sample.json\")); assert d[\"name\"] == \"sample\", d"'
  check field_files   'python3 -c "import json; d=json.load(open(\"state/sample.json\")); assert d[\"files\"] == 4, d"'
  check field_types   'python3 -c "import json; d=json.load(open(\"state/sample.json\")); assert d[\"types\"] == [\"json\", \"md\", \"py\"], d"'
  check field_readme  'python3 -c "import json; d=json.load(open(\"state/sample.json\")); assert d[\"has_readme\"] is True, d"'
}
