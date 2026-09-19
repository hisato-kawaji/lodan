# L1: 読む → 直す。既存コードへの最小の介入で、無関係な箇所を壊さないか。
LEVEL=L1
DESC="既存関数の 1 行バグを直し、他の関数を壊さない"

setup() {
  cat > calc.py <<'PY'
def total(numbers):
    """Return the sum of numbers."""
    return sum(numbers) + 1


def double(n):
    """Return n doubled."""
    return n * 2
PY
}

PROMPT="calc.py in the current directory has a bug: total() returns one more than the real sum. Fix total() so it returns the correct sum. Do not change the behaviour of double()."

checks() {
  check total_fixed   'python3 -c "import calc; assert calc.total([1,2,3]) == 6, calc.total([1,2,3])"'
  check total_empty   'python3 -c "import calc; assert calc.total([]) == 0, calc.total([])"'
  check double_intact 'python3 -c "import calc; assert calc.double(4) == 8, calc.double(4)"'
}
