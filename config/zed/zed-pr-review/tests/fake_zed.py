#!/usr/bin/env python3
"""Record a directory diff and the published state without opening a GUI."""
import json
import os
import sys
from pathlib import Path

state = Path(os.environ["ZPR_TEST_STATE"])
active = state / "active.json"
record = {
    "args": sys.argv[1:],
    "cwd": os.getcwd(),
    "active": json.loads(active.read_text()) if active.exists() else None,
    "checkouts": {
        str(path.relative_to(state)): json.loads(path.read_text())
        for path in state.rglob("last-checkout.json")
    },
}
with Path(os.environ["ZPR_TEST_ZED_LOG"]).open("a") as stream:
    stream.write(json.dumps(record) + "\n")
control = json.loads(Path(os.environ["ZPR_TEST_CONTROL"]).read_text())
sys.exit(control.get("zed_exit", 0))
