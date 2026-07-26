# L2: 動いている既存 CLI へ機能を積む。ステージ間の累積作業を 1 段だけ切り出した形で、
# 「既存を壊さずに足せるか」を測る (mini-renovater の S2 以降が崩れた要因)。
LEVEL=L2
DESC="既存 CLI にサブコマンドを追加し、既存機能を維持する"

setup() {
  mkdir -p sample
  printf '# Sample\n' > sample/README.md
  printf 'print("hi")\n' > sample/main.py
  printf 'def helper():\n    return 1\n' > sample/util.py

  cat > scanner.py <<'PY'
#!/usr/bin/env python3
"""Tiny project scanner."""
import argparse
import json
import os


def scan(directory):
    files = []
    for root, _dirs, names in os.walk(directory):
        for n in names:
            files.append(os.path.join(root, n))
    data = {
        "name": os.path.basename(os.path.normpath(directory)),
        "files": len(files),
    }
    os.makedirs("state", exist_ok=True)
    with open(os.path.join("state", data["name"] + ".json"), "w") as fh:
        json.dump(data, fh, indent=2)
    print("scanned", data["name"])


def main():
    parser = argparse.ArgumentParser()
    sub = parser.add_subparsers(dest="cmd", required=True)
    p_scan = sub.add_parser("scan")
    p_scan.add_argument("directory")
    args = parser.parse_args()
    if args.cmd == "scan":
        scan(args.directory)


if __name__ == "__main__":
    main()
PY
}

PROMPT="Read scanner.py in the current directory, then add a new subcommand to it: python3 scanner.py report <name> — it reads state/<name>.json and writes state/<name>.txt containing two lines: 'name: <name>' and 'files: <count>' taken from the JSON. It must fail with a non-zero exit status if state/<name>.json does not exist. Keep the existing scan subcommand working exactly as before."

checks() {
  check scan_still_works 'python3 scanner.py scan sample && test -f state/sample.json'
  check report_runs      'python3 scanner.py scan sample >/dev/null && python3 scanner.py report sample'
  check report_name_line 'grep -q "^name: sample$" state/sample.txt'
  check report_files_line 'grep -qE "^files: [0-9]+$" state/sample.txt'
  # report が動くことを先に確かめる (未実装なら argparse が落ちて通ってしまうため)。
  check missing_input_fails 'python3 scanner.py scan sample >/dev/null 2>&1 && python3 scanner.py report sample >/dev/null 2>&1 && ! python3 scanner.py report nosuch >/dev/null 2>&1'
}
