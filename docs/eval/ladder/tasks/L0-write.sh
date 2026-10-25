# L0: ツール 1 回で終わる最小タスク。ここが落ちるならツールコール自体が成立していない。
LEVEL=L0
DESC="指定した内容のファイルを 1 つ作る"

setup() { :; }

PROMPT="Create a file named hello.txt in the current directory whose entire content is exactly: hello world"

checks() {
  check exists  'test -f hello.txt'
  check content 'test "$(tr -d "\n" < hello.txt)" = "hello world"'
}
