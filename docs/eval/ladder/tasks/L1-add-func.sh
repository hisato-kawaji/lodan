# L1: 既存ファイルへの追記。追加しつつ既存を維持できるか (累積作業の最小単位)。
LEVEL=L1
DESC="既存モジュールへ関数を 1 つ追加し、既存関数を維持する"

setup() {
  cat > strutil.py <<'PY'
def shout(s):
    """Return s upper-cased with an exclamation mark."""
    return s.upper() + "!"
PY
}

PROMPT="Add a function initials(name) to strutil.py in the current directory. It takes a full name like 'ada lovelace' and returns the upper-case initials, e.g. 'AL'. Keep the existing shout() function working unchanged."

checks() {
  check initials_basic  'python3 -c "import strutil; assert strutil.initials(\"ada lovelace\") == \"AL\", strutil.initials(\"ada lovelace\")"'
  check initials_three_words 'python3 -c "import strutil; assert strutil.initials(\"grace brewster hopper\") == \"GBH\", strutil.initials(\"grace brewster hopper\")"'
  check shout_intact    'python3 -c "import strutil; assert strutil.shout(\"hi\") == \"HI!\", strutil.shout(\"hi\")"'
}
