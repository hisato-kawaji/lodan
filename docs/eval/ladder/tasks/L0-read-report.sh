# L0: 読む → 書く。Read の結果を次の呼び出しへ正しく運べるか。
LEVEL=L0
DESC="設定ファイルを読み、値だけを別ファイルへ書く"

setup() {
  cat > app.ini <<'INI'
[server]
host = 0.0.0.0
port = 8123
workers = 4
INI
}

PROMPT="Read app.ini in the current directory and write ONLY the port number (digits, nothing else) to a new file named answer.txt"

checks() {
  check exists 'test -f answer.txt'
  # 前後の空白/改行だけ許す。「digits, nothing else」という指示どおりか見る。
  check value  'test "$(tr -d "[:space:]" < answer.txt)" = "8123"'
}
