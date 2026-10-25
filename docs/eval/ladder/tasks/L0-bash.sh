# L0: コマンドを実行して出力を保存する。Bash とリダイレクトの扱い。
LEVEL=L0
DESC="コマンドを実行し出力をファイルへ残す"

setup() { :; }

PROMPT="Run the command 'python3 --version' and save its output to a file named version.txt in the current directory."

checks() {
  check exists  'test -f version.txt'
  check content 'grep -qi "^python 3" version.txt'
}
