# L3: 先の段の成果物を後の段が使う、4 ファイル・3 段の積み上げ (#65)。L2 は「要件 4-7 個を
# 1 ファイルで」だったが、L3 は「前の段が決めた形を後の段が守る」ことを測る。Phase 3 で
# C1 (Plan→Execute) の便益が測れなかったのは、基準が文脈の積み上がりで劣化する粒度に
# ラダーが届いていなかったため。
LEVEL=L3
DESC="3 段のパッケージ (読み込み → 集計 → CLI) を組み上げる"

setup() {
  mkdir -p data
  cat > data/items.csv <<'CSV'
sku,name,category,qty,price
A1,Bolt,hardware,120,0.25
A2,Nut,hardware,200,0.10
B1,Glue,supplies,15,3.50
B2,Tape,supplies,40,1.25
C1,Drill,tools,3,89.99
C2,Saw,tools,many,24.00
C3,Hammer,tools,7,12.50
CSV
}

# #65 D1 (`verify-none`) 用。プロンプトが明記している実行方法だけを使う。
VERIFY="python3 -m inv summary data/items.csv && python3 -m inv categories data/items.csv && python3 -m inv export data/items.csv out.json"

PROMPT="Build a small Python package named inv in the current directory, standard library only, in three parts. (1) inv/model.py: a function load_items(path) that reads a CSV file with the header sku,name,category,qty,price (see data/items.csv) and returns a list of dicts with those five keys, where qty is an int and price is a float; rows whose qty or price is not a number are skipped. (2) inv/stats.py: total_value(items) returns the sum of qty * price rounded to 2 decimals, and by_category(items) returns a dict mapping each category to its total qty; both take the list that load_items returns. (3) inv/__main__.py, so that: 'python3 -m inv summary FILE' prints two lines, 'items: N' and 'value: V' (V with exactly 2 decimals); 'python3 -m inv categories FILE' prints one 'CATEGORY: QTY' line per category, sorted by category name; 'python3 -m inv export FILE OUT' writes OUT as a JSON object with the keys items (int), value (number) and categories (object mapping category to qty). Any subcommand must exit with a non-zero status when FILE does not exist. Also create inv/__init__.py."

checks() {
  check model_loads      'python3 -c "from inv.model import load_items; xs = load_items(\"data/items.csv\"); assert len(xs) == 6, xs; assert all(isinstance(x[\"qty\"], int) and isinstance(x[\"price\"], float) for x in xs)"'
  check model_skips_bad  'python3 -c "from inv.model import load_items; assert \"Saw\" not in [x[\"name\"] for x in load_items(\"data/items.csv\")]"'
  check stats_total      'python3 -c "from inv.model import load_items; from inv.stats import total_value; assert total_value(load_items(\"data/items.csv\")) == 509.97"'
  check stats_category   'python3 -c "from inv.model import load_items; from inv.stats import by_category; assert by_category(load_items(\"data/items.csv\")) == {\"hardware\": 320, \"supplies\": 55, \"tools\": 10}"'
  check summary_output   'out=$(python3 -m inv summary data/items.csv) && [ "$out" = "$(printf "items: 6\nvalue: 509.97")" ]'
  check categories_sorted 'out=$(python3 -m inv categories data/items.csv) && [ "$out" = "$(printf "hardware: 320\nsupplies: 55\ntools: 10")" ]'
  check export_json      'rm -f out.json && python3 -m inv export data/items.csv out.json && python3 -c "import json; d = json.load(open(\"out.json\")); assert d[\"items\"] == 6 and abs(d[\"value\"] - 509.97) < 1e-9 and d[\"categories\"] == {\"hardware\": 320, \"supplies\": 55, \"tools\": 10}, d"'
  # 段をまたぐ: 別の CSV でも CLI が model / stats の結果を使っていること (出力の決め打ちを弾く)。
  check stages_compose   'printf "sku,name,category,qty,price\nX,Y,zeta,2,1.5\nZ,W,alpha,1,10\n" > other.csv && out=$(python3 -m inv summary other.csv) && [ "$out" = "$(printf "items: 2\nvalue: 13.00")" ]'
  # summary が動くことを先に確かめる (未実装なら何でも落ちて通ってしまうため)。
  check missing_file_fails 'python3 -m inv summary data/items.csv >/dev/null 2>&1 && ! python3 -m inv summary nosuch.csv >/dev/null 2>&1 && ! python3 -m inv categories nosuch.csv >/dev/null 2>&1'
}
