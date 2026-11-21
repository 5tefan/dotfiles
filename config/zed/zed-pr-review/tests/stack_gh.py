#!/usr/bin/env python3
"""Strict offline REST fixture for tests/stack.rs; never delegates to gh."""
import json
import os
from pathlib import Path
import sys

root = Path(os.environ["ZPR_STACK_FIXTURE"])
plan = json.loads((root / "plan.json").read_text())
index_path = root / "index"
index = int(index_path.read_text()) if index_path.exists() else 0
assert index < len(plan["steps"]), (index, sys.argv)
step = plan["steps"][index]
expected = [
    "api", "--hostname", plan["host"], "--method", "GET",
    "--header", "Accept: application/vnd.github+json",
    "--header", "X-GitHub-Api-Version: 2022-11-28", step["endpoint"],
]
if step.get("pages", False):
    expected += ["--paginate", "--slurp"]
assert sys.argv[1:] == expected, (index, sys.argv[1:], expected)
assert os.environ["GH_HOST"] == plan["host"]
assert "GH_REPO" not in os.environ
index_path.write_text(str(index + 1))
print(json.dumps(step["body"]))
