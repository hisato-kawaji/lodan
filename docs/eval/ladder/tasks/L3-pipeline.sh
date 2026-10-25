# L3: 3 段の積み上げを複数ファイルで作る。先の段 (読み込み) の成果物を次の段 (集計) が使い、
# その両方を最後の段 (CLI) が使う。L2 までは 1 ファイル・1 段なので文脈が積み上がる前に終わるが、
# ここでは依頼の前半の約束 (paid だけ数える・小数 2 桁) を終盤の CLI まで運べるかを測る (#65 C1 の土俵)。
LEVEL=L3
DESC="読み込み → 集計 → CLI の 3 段を 4 ファイルのパッケージで作る"

setup() {
  mkdir -p data
  cat > data/orders.csv <<'CSV'
order_id,customer,item,qty,unit_price,status
1,alice,pen,3,1.50,paid
2,bob,book,1,12.00,paid
3,alice,book,2,12.00,refunded
4,carol,pen,10,1.50,paid
5,bob,ink,2,3.25,paid
6,dave,pad,4,2.00,pending
CSV

  # 判定用の別データ。data/orders.csv の答えを直書きした実装を落とすため、モデルには見せない。
  mkdir -p .probe
  cat > .probe/other.csv <<'CSV'
order_id,customer,item,qty,unit_price,status
10,erin,cup,2,4.10,paid
11,frank,cup,1,4.10,paid
12,erin,lid,3,0.25,paid
13,frank,mug,1,9.99,refunded
14,gina,mug,1,4.85,paid
CSV
}

# #65 D1 (`verify-none`) で検証として強制するコマンド。プロンプトが明記している実行方法
# だけを使い、副作用はプロンプトが指定した出力ファイルだけにする (phase2 の交絡の反省)。
VERIFY="python3 cli.py report data/orders.csv out/report.json && python3 cli.py top data/orders.csv 2"

PROMPT="Build a small Python 3 package in the current directory using only the standard library, in three stages. Stage 1: create shop/__init__.py (may be empty) and shop/orders.py with a function load_orders(path) that reads a CSV file like data/orders.csv (columns: order_id,customer,item,qty,unit_price,status) and returns a list of dicts with keys order_id (str), customer (str), item (str), qty (int), unit_price (float), status (str), one dict per row, in file order. Stage 2: create shop/stats.py, working on the list of dicts returned by load_orders, and provide customer_totals(orders) returning a dict that maps each customer who has at least one order with status 'paid' to the sum of qty*unit_price over that customer's paid orders only, rounded to 2 decimals; orders with any other status are ignored. Also provide top_customers(orders, n) returning a list of (customer, total) tuples from customer_totals, sorted by total descending, ties broken by customer name ascending, at most n items. Stage 3: create cli.py in the current directory that uses shop.orders and shop.stats and supports two commands. python3 cli.py report <csv> <out.json> writes a JSON object with keys customers (the customer_totals dict), grand_total (sum of those totals, rounded to 2 decimals) and orders_counted (number of paid orders), creating the output directory if needed. python3 cli.py top <csv> <n> prints one line per customer from top_customers in the form '<customer>: <total>' with the total always shown with exactly 2 decimals (for example 'bob: 18.50'). Both commands must exit with a non-zero status if the CSV file does not exist. It must work when run as: python3 cli.py report data/orders.csv out/report.json and python3 cli.py top data/orders.csv 2"

checks() {
  # 段 1: 読み込み
  check stage1_load   'python3 -c "from shop.orders import load_orders; o=load_orders(\"data/orders.csv\"); assert len(o)==6, o; r=o[0]; assert (r[\"order_id\"],r[\"customer\"],r[\"qty\"],r[\"unit_price\"],r[\"status\"])==(\"1\",\"alice\",3,1.5,\"paid\"), r"'
  # 段 2: 集計 (paid だけ・小数 2 桁)。別データでも確かめる。
  check stage2_totals 'python3 -c "from shop.orders import load_orders; from shop.stats import customer_totals; t=customer_totals(load_orders(\"data/orders.csv\")); assert t=={\"alice\":4.5,\"bob\":18.5,\"carol\":15.0}, t"'
  check stage2_other  'python3 -c "from shop.orders import load_orders; from shop.stats import customer_totals; t=customer_totals(load_orders(\".probe/other.csv\")); assert t=={\"erin\":8.95,\"frank\":4.1,\"gina\":4.85}, t"'
  check stage2_top    'python3 -c "from shop.orders import load_orders; from shop.stats import top_customers; t=top_customers(load_orders(\".probe/other.csv\"), 2); assert [tuple(x) for x in t]==[(\"erin\",8.95),(\"gina\",4.85)], t"'
  # 段 3: CLI (前の 2 段の約束を運べているか)
  check stage3_report 'python3 cli.py report data/orders.csv out/report.json >/dev/null && python3 -c "import json; d=json.load(open(\"out/report.json\")); assert d=={\"customers\":{\"alice\":4.5,\"bob\":18.5,\"carol\":15.0},\"grand_total\":38.0,\"orders_counted\":4}, d"'
  check stage3_top    'test "$(python3 cli.py top data/orders.csv 2)" = "$(printf "bob: 18.50\ncarol: 15.00")"'
  # top が動くことを先に確かめる (未実装なら argparse が落ちて通ってしまうため)。
  check stage3_missing 'python3 cli.py top data/orders.csv 1 >/dev/null 2>&1 && ! python3 cli.py top nosuch.csv 1 >/dev/null 2>&1 && ! python3 cli.py report nosuch.csv out/x.json >/dev/null 2>&1'
}
