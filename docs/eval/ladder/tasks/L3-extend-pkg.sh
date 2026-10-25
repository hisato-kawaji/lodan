# L3: 動いている複数ファイルのパッケージに、3 ファイルを貫く変更を順に積む (model → store → CLI)。
# L2-extend-cli は 1 ファイルに 1 段だったが、ここでは先に変えたファイルの約束 (既定値・旧形式の
# 読み込み) を後のファイルで守れるか、既存のテストを壊さずに済むかを測る (#65 C1 の土俵)。
LEVEL=L3
DESC="既存パッケージ 3 ファイルに priority を通し、旧データと既存テストを維持する"

setup() {
  mkdir -p todo tests
  : > todo/__init__.py
  : > tests/__init__.py

  cat > todo/model.py <<'PY'
"""Task model."""
from dataclasses import dataclass


@dataclass
class Task:
    id: int
    title: str
    done: bool = False

    def to_dict(self):
        return {"id": self.id, "title": self.title, "done": self.done}

    @staticmethod
    def from_dict(d):
        return Task(id=d["id"], title=d["title"], done=d.get("done", False))
PY

  cat > todo/store.py <<'PY'
"""JSON file storage for tasks."""
import json
import os

from todo.model import Task


def load(path):
    if not os.path.exists(path):
        return []
    with open(path) as fh:
        return [Task.from_dict(d) for d in json.load(fh)]


def save(path, tasks):
    with open(path, "w") as fh:
        json.dump([t.to_dict() for t in tasks], fh, indent=2)


def add(path, title):
    tasks = load(path)
    next_id = max((t.id for t in tasks), default=0) + 1
    task = Task(id=next_id, title=title)
    tasks.append(task)
    save(path, tasks)
    return task
PY

  cat > todo/cli.py <<'PY'
"""Command line interface: python3 -m todo.cli <command> ..."""
import argparse

from todo import store

DB = "todo.json"


def main(argv=None):
    parser = argparse.ArgumentParser(prog="todo")
    sub = parser.add_subparsers(dest="cmd", required=True)
    p_add = sub.add_parser("add")
    p_add.add_argument("title")
    sub.add_parser("list")
    args = parser.parse_args(argv)
    if args.cmd == "add":
        task = store.add(DB, args.title)
        print(f"added {task.id}")
    elif args.cmd == "list":
        for t in store.load(DB):
            mark = "x" if t.done else " "
            print(f"[{mark}] {t.id} {t.title}")


if __name__ == "__main__":
    main()
PY

  cat > tests/test_todo.py <<'PY'
import os
import tempfile
import unittest

from todo import store
from todo.model import Task


class TodoTest(unittest.TestCase):
    def test_roundtrip(self):
        with tempfile.TemporaryDirectory() as d:
            path = os.path.join(d, "t.json")
            store.add(path, "a")
            store.add(path, "b")
            tasks = store.load(path)
            self.assertEqual([t.title for t in tasks], ["a", "b"])
            self.assertEqual([t.id for t in tasks], [1, 2])

    def test_from_dict_default_done(self):
        self.assertFalse(Task.from_dict({"id": 1, "title": "x"}).done)


if __name__ == "__main__":
    unittest.main()
PY

  # 旧形式のデータ (priority が無い)。これを読めなくした実装は既存利用者を壊す。
  cat > todo.json <<'JSON'
[
  {"id": 1, "title": "write report", "done": false},
  {"id": 2, "title": "buy milk", "done": true}
]
JSON
}

# #65 D1 (`verify-none`) で検証として強制するコマンド。プロンプトが明記している実行方法だけを使う。
# `add` は todo.json を書き換えるので使わない (検証の副作用で採点を助けないため)。
VERIFY="python3 -m unittest discover -s tests -t . && python3 -m todo.cli list --sort priority"

PROMPT="The current directory contains a small Python 3 todo package: todo/model.py (Task dataclass), todo/store.py (JSON storage in todo.json), todo/cli.py (run as python3 -m todo.cli) and tests/test_todo.py. Read them first, then add a task priority through all three files, in this order. Step 1, todo/model.py: add a priority field whose value is one of 'low', 'normal', 'high', defaulting to 'normal'; to_dict must include it, and from_dict must accept dicts without a priority key (treat them as 'normal') so that the existing todo.json keeps loading. Step 2, todo/store.py: add() must take an optional priority argument (default 'normal') and persist it; it must raise ValueError for any value other than low, normal or high. Step 3, todo/cli.py: 'add' gets an optional --priority option (default normal), and 'list' gets an optional --sort priority option that orders tasks high first, then normal, then low, and by id within the same priority; without --sort the list order stays as it is now. Each list line must become '[<mark>] <id> <title> (<priority>)', for example '[ ] 1 write report (normal)'. 'add' with an invalid priority must exit with a non-zero status without changing todo.json. The existing tests must keep passing: python3 -m unittest discover -s tests -t ."

checks() {
  check tests_pass     'python3 -m unittest discover -s tests -t . >/dev/null 2>&1'
  # 旧形式の todo.json をそのまま読める (add の前に確かめる)
  check old_data_loads 'test "$(python3 -m todo.cli list)" = "$(printf "[ ] 1 write report (normal)\n[x] 2 buy milk (normal)")"'
  check model_default  'python3 -c "from todo.model import Task; t=Task.from_dict({\"id\":1,\"title\":\"x\"}); assert t.priority==\"normal\", t; assert t.to_dict()[\"priority\"]==\"normal\""'
  check store_rejects  'python3 -c "
import tempfile, os
from todo import store
p = os.path.join(tempfile.mkdtemp(), \"t.json\")
store.add(p, \"a\", priority=\"high\")
assert store.load(p)[0].priority == \"high\"
try:
    store.add(p, \"b\", priority=\"urgent\")
except ValueError:
    pass
else:
    raise SystemExit(\"no ValueError\")
assert len(store.load(p)) == 1
"'
  check cli_add_priority 'python3 -m todo.cli add "call bob" --priority high >/dev/null && python3 -c "import json; d=json.load(open(\"todo.json\")); assert d[-1][\"title\"]==\"call bob\" and d[-1][\"priority\"]==\"high\", d"'
  check cli_sort       'python3 -m todo.cli add "nap" --priority low >/dev/null 2>&1; test "$(python3 -m todo.cli list --sort priority)" = "$(printf "[ ] 3 call bob (high)\n[ ] 1 write report (normal)\n[x] 2 buy milk (normal)\n[ ] 4 nap (low)")"'
  # 正しい priority の add が動くことを先に確かめてから、不正値の拒否を見る (未実装の argparse エラーで通さない)。
  check cli_rejects    'cp todo.json .probe_before.json && python3 -m todo.cli add ok --priority low >/dev/null 2>&1 && cp todo.json .probe_before.json && ! python3 -m todo.cli add bad --priority urgent >/dev/null 2>&1 && cmp -s todo.json .probe_before.json'
}
