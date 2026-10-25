# L0: 検索して結果をまとめる。Grep/Glob の結果を取り違えずに書き出せるか。
LEVEL=L0
DESC="パターンを含むファイルを探して一覧を書き出す"

setup() {
  printf 'def a():\n    pass  # TODO: implement\n' > alpha.py
  printf 'def b():\n    return 1\n' > beta.py
  printf '# TODO: rewrite this module\ndef c():\n    return 2\n' > gamma.py
}

PROMPT="Find every .py file in the current directory that contains the text TODO, and write their file names (one per line, no other text) to a new file named found.txt"

checks() {
  check has_alpha    'grep -q "alpha.py" found.txt'
  check has_gamma    'grep -q "gamma.py" found.txt'
  # ファイルが無いだけで通らないよう、存在を前提にしてから否定する。
  check excludes_beta 'test -f found.txt && ! grep -q "beta.py" found.txt'
  check two_lines    'test "$(grep -c . found.txt)" = "2"'
}
