#!/usr/bin/env python3
"""Record the editor input, then write a configured body without a GUI."""
import json
import os
import sys
from pathlib import Path

control_path = Path(os.environ["ZPR_TEST_CONTROL"])
control = json.loads(control_path.read_text())
path = Path(sys.argv[-1])
initial = path.read_text()
with Path(os.environ["ZPR_TEST_EDITOR_LOG"]).open("a") as stream:
    stream.write(json.dumps({
        "args": sys.argv[1:-1], "path": str(path), "initial": initial,
    }) + "\n")

mode = control.get("editor_mode", "write")
body = control.get("editor_body", "Review summary")
if mode == "prepend":
    path.write_text(body + initial)
elif mode != "unchanged":
    path.write_text(body)
# Change a registered snapshot only after the CLI has discovered its session.
# Submission remains valid, but the later best-effort cleanup must refuse it.
if mode == "alter_snapshot":
    snapshot = Path(control["editor_snapshot"])
    assert snapshot.is_relative_to(Path(os.environ["ZPR_TEST_STATE"]))
    snapshot.chmod(0o600)  # Frozen copies are normally read-only.
    snapshot.write_text("Changed while the review editor was open\n")
if mode in ("change_head", "change_base"):
    field = "head" if mode == "change_head" else "base"
    control[field] = control["base" if field == "head" else "head"]
if mode == "accept_delayed_review":
    review = control.pop("delayed_review")
    control.setdefault("reviews", []).append(review)
    control.setdefault("review_comments", {})[str(review["id"])] = control.pop("delayed_comments")
if mode in ("change_head", "change_base", "accept_delayed_review"):
    temporary = control_path.with_suffix(".next")
    temporary.write_text(json.dumps(control))
    temporary.replace(control_path)
if mode == "fail":
    sys.exit(12)
