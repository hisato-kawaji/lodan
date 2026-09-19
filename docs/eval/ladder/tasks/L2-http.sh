# L2: 実際に起動して応答するものを作らせる。mini-renovater S5 (HTTP API) の縮小版。
# 判定はサーバを立てて叩くので、「それらしいコード」では通らない。
LEVEL=L2
DESC="stdlib だけで 2 エンドポイントの HTTP API を作る"

setup() {
  # 判定用プローブ。モデルの作業対象と混ざらないよう隠しディレクトリへ置く。
  mkdir -p .probe
  cat > .probe/probe.py <<'PY'
"""server.py を起動し、指定エンドポイントの JSON 応答を検証する。"""
import json
import subprocess
import sys
import time
import urllib.error
import urllib.request

PORT = 18977


def main():
    which = sys.argv[1]
    proc = subprocess.Popen(
        [sys.executable, "server.py", "--port", str(PORT)],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    try:
        deadline = time.time() + 20
        body = None
        while time.time() < deadline:
            if proc.poll() is not None:
                print("server exited early", file=sys.stderr)
                return 1
            try:
                path = "/health" if which == "health" else "/sum?a=1&b=2"
                with urllib.request.urlopen(f"http://127.0.0.1:{PORT}{path}", timeout=2) as r:
                    body = json.loads(r.read().decode())
                break
            except (urllib.error.URLError, ConnectionError, json.JSONDecodeError, OSError):
                time.sleep(0.5)
        if body is None:
            print("no response", file=sys.stderr)
            return 1
        if which == "health":
            assert body.get("status") == "ok", body
        else:
            assert body.get("result") == 3, body
        return 0
    except AssertionError as e:
        print(f"bad payload: {e}", file=sys.stderr)
        return 1
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            proc.kill()


if __name__ == "__main__":
    sys.exit(main())
PY
}

PROMPT="Create server.py in the current directory: a JSON HTTP API using only the Python 3 standard library (http.server). It must accept a --port <port> command line option and serve two GET endpoints. GET /health returns {\"status\": \"ok\"}. GET /sum?a=<int>&b=<int> returns {\"result\": <a+b>}. Both responses must have Content-Type application/json. Start it with: python3 server.py --port 8000"

checks() {
  check script_exists 'test -f server.py'
  check compiles      'python3 -m py_compile server.py'
  check health        'python3 .probe/probe.py health'
  check sum           'python3 .probe/probe.py sum'
}
