"""Offline PR-stack PTY scenarios, invoked by cli.rs with its isolated fixture."""

import fcntl
import json
import os
from pathlib import Path
import pty
import select
import signal
import struct
import subprocess
import sys
import termios
import time
from types import SimpleNamespace

from log_terminal import Screen, Terminal


PULLS = "repos/acme/widgets/pulls?state=open&per_page=100"


def calls(repo):
    path = Path(repo.env["ZPR_TEST_GH_LOG"])
    return [json.loads(line) for line in path.read_text().splitlines() if line.strip()]


def configure(repo, **changes):
    path = Path(repo.env["ZPR_TEST_CONTROL"])
    control = json.loads(path.read_text())
    control.update(changes)
    # The fake transport also updates this file; change controls only after a
    # completed read, or while its listing is paused before loading the file.
    temporary = path.with_suffix(".pty-next")
    temporary.write_text(json.dumps(control))
    temporary.replace(path)


def pause_listing(repo):
    """Wrap only the fixture's offline gh, without changing the shared fake."""
    root = Path(repo.env["ZPR_TEST_CONTROL"]).parent
    shim = root / "bin" / "gh"
    shim.rename(shim.with_name("offline-gh.py"))
    shim.write_text('''#!/usr/bin/env python3
import os, sys, time
from pathlib import Path
root = Path(os.environ["ZPR_TEST_CONTROL"]).parent
pause = root / "stack-read-pause"
if "repos/acme/widgets/pulls?state=open&per_page=100" in sys.argv and pause.exists():
    (root / "stack-read-started").write_text("reading\\n")
    deadline = time.monotonic() + 15
    while pause.exists():
        if time.monotonic() >= deadline:
            print("Timed out waiting for the PTY test to release the read", file=sys.stderr)
            sys.exit(98)
        time.sleep(0.02)
os.execv(sys.executable, [sys.executable, str(Path(__file__).with_name("offline-gh.py")), *sys.argv[1:]])
''')
    shim.chmod(0o700)
    return root / "stack-read-pause", root / "stack-read-started"


def current_visible(screen):
    return any(line.startswith("@ #17 ") for line in screen.body())


def current_updated(screen, title, draft):
    return current_visible(screen) and "│ " + title in screen.body() and any(
        line.startswith("@ #17 topic ") and ("[draft]" in line) == draft
        for line in screen.body()
    )


def assert_colored_graph(output):
    # Screen intentionally ignores SGR. Inspect the raw stream as well so a
    # plain graph with only a colored header would not satisfy this assertion.
    text = output.decode("utf-8")
    foreground = False
    for match in Screen.CSI.finditer(text):
        if match[3] != "m":
            continue
        codes = [int(value or 0) for value in match[1].split(";")]
        if codes == [0] or codes == [39]:
            foreground = False
        if any(30 <= code <= 38 or 90 <= code <= 97 for code in codes):
            foreground = True
        following = text[match.end():].split("\x1b", 1)[0]
        if foreground and ("#17" in following or "topic" in following):
            return
    raise AssertionError("The PR metadata had no foreground color")


def other_review_command(binary, repo, terminal):
    # `list` takes the workspace and draft locks without changing the draft.
    # Unlike a lock-file existence check, this exercises a real second task.
    result = subprocess.run([binary, "list"], cwd=repo.path, env=repo.env,
                            input=b"", capture_output=True, timeout=8)
    assert result.returncode == 0, (
        "The viewer blocked another review command:\n"
        + result.stdout.decode() + result.stderr.decode()
    )
    assert b"Keep this private navigation draft" in result.stdout
    assert terminal.child.poll() is None, "The viewer exited during another review task"


def navigation(terminal):
    screen = terminal.screen
    terminal.send(b"\x1b[H")
    terminal.wait(lambda: screen.viewport()[0] == 1, "Home")
    assert not current_visible(screen), "Fixture did not put @ below the first page"

    for key, delta, name in [(b"j", 1, "j"), (b"\x1b[B", 1, "Down"),
                             (b"k", -1, "k"), (b"\x1b[A", -1, "Up")]:
        top = screen.viewport()[0]
        terminal.send(key)
        terminal.wait(lambda: screen.viewport()[0] == top + delta, name)

    top = screen.viewport()[0]
    terminal.mouse(65, (5, 4))
    terminal.wait(lambda: screen.viewport()[0] > top, "wheel down")
    terminal.mouse(64, (5, 4))
    terminal.wait(lambda: screen.viewport()[0] == top, "wheel up")

    terminal.send(b"\x1b[6~")
    terminal.wait(lambda: screen.viewport()[0] > top + 1, "Page Down")
    terminal.send(b"\x1b[5~")
    terminal.wait(lambda: screen.viewport()[0] == top, "Page Up")

    # Reconstruct the entire graph from successive pages. A linear stack must
    # keep every node in column zero, even far below the first page.
    seen = {}
    while True:
        first, last, count = screen.viewport()
        seen.update(zip(range(first - 1, last), screen.body()))
        if last == count:
            break
        terminal.send(b"\x1b[6~")
        terminal.wait(lambda: screen.viewport()[0] > first, "next graph page")
    assert set(seen) == set(range(count)), "Paging skipped graph rows"
    lines = [seen[index] for index in range(count)]
    assert len(lines) == 25, lines  # Eight PRs, three lines each, one base.
    for index, number in enumerate(range(23, 15, -1)):
        metadata, title, url = lines[index * 3:index * 3 + 3]
        marker = "@" if number == 17 else "○"
        branch = "topic" if number == 17 else "main" if number == 16 else f"topic-{number}"
        state = "draft" if number == 17 else "ready"
        assert metadata.startswith(f"{marker} #{number} {branch} [{state}]"), metadata
        expected_title = "Offline review fixture" if number == 17 else f"Stack level {number}"
        assert title == "│ " + expected_title, title
        assert url == f"│ https://github.com/acme/widgets/pull/{number}", url
    assert lines[-1] == "◆ trunk (base)", lines
    assert sum("(base)" in line for line in lines) == 1

    terminal.send(b"\x1b[H")
    terminal.wait(lambda: screen.viewport()[0] == 1, "Home after paging")
    terminal.send(b"\x1b[F")
    terminal.wait(lambda: screen.viewport()[1] == count, "End")
    terminal.send(b"\x1b[H")
    terminal.wait(lambda: not current_visible(screen), "Home before revealing @")
    terminal.send(b"@")
    terminal.wait(lambda: current_visible(screen), "@ to reveal the current PR")


def refresh(binary, repo, terminal, pause, started):
    screen = terminal.screen
    original = screen.body()
    old_listing_count = sum(call.get("endpoint") == PULLS for call in calls(repo))
    pause.touch()
    configure(repo, title="Manually refreshed PR title", draft=False)
    try:
        terminal.send(b"r")
        terminal.wait(started.exists, "manual refresh to reach the paused offline listing")
        assert screen.body() == original, "A pending refresh discarded the old graph"
        other_review_command(binary, repo, terminal)
        # A slow GitHub read must not freeze navigation or require a workspace lock.
        terminal.send(b"\x1b[H")
        terminal.wait(lambda: screen.viewport()[0] == 1, "scroll during a paused read")
        terminal.send(b"@")
        terminal.wait(lambda: current_visible(screen), "reveal @ during a paused read")
    finally:
        pause.unlink(missing_ok=True)
    terminal.wait(lambda: current_updated(screen, "Manually refreshed PR title", False),
                  "r to show the changed title and draft state")
    assert sum(call.get("endpoint") == PULLS for call in calls(repo)) > old_listing_count

    graph, viewport = screen.body(), screen.viewport()
    configure(repo, deny_discovery=True)
    terminal.send(b"r")
    terminal.wait(lambda: "Refresh failed" in "\n".join(screen.lines()),
                  "failed GitHub read to appear in the status")
    terminal.keep(lambda: screen.body() == graph and screen.viewport() == viewport,
                  0.3, "A failed read discarded the graph or moved the viewport")
    assert any(call.get("discovery_denied") for call in calls(repo)), "No failed read occurred"

    configure(repo, deny_discovery=False, title="Recovered PR title", draft=True)
    terminal.send(b"r")
    terminal.wait(lambda: current_updated(screen, "Recovered PR title", True)
                  and "Refresh failed" not in "\n".join(screen.lines()),
                  "r to recover after the failed read")
    terminal.exit_and_assert_restored(b"q")


def queued_refresh(repo, terminal, pause, started):
    screen = terminal.screen
    old_listing_count = sum(call.get("endpoint") == PULLS for call in calls(repo))
    configure(repo, title="Queued refresh title", draft=False)
    pause.touch()
    try:
        terminal.send(b"r")
        terminal.wait(started.exists, "refresh to reach the paused listing")
        # Home is an input barrier: its redraw proves all preceding r presses
        # were processed while the worker was still blocked in the listing.
        terminal.send(b"rrr\x1b[H")
        terminal.wait(lambda: screen.viewport()[0] == 1, "queued r keys before Home")
        terminal.send(b"@")
        terminal.wait(lambda: current_visible(screen), "reveal current during the paused read")
        assert sum(call.get("endpoint") == PULLS for call in calls(repo)) == old_listing_count
    finally:
        pause.unlink(missing_ok=True)
    terminal.wait(lambda: current_updated(screen, "Queued refresh title", False)
                  and " · live · " in screen.lines()[0]
                  and sum(call.get("endpoint") == PULLS for call in calls(repo)) == old_listing_count + 2,
                  "one completed follow-up read for the queued refresh keys")
    terminal.keep(lambda: sum(call.get("endpoint") == PULLS for call in calls(repo)) == old_listing_count + 2,
                  0.5, "Queued r presses started more than one follow-up read")
    terminal.exit_and_assert_restored(b"q")


def automatic_refresh(repo, terminal):
    screen = terminal.screen
    viewport = screen.viewport()
    old_listing_count = sum(call.get("endpoint") == PULLS for call in calls(repo))
    started = time.monotonic()
    configure(repo, title="Automatically refreshed PR title", draft=False)
    # No input is sent until after the new content arrives. Seeing both fields
    # change proves a real periodic read completed, rather than just a redraw.
    terminal.wait(lambda: current_updated(screen, "Automatically refreshed PR title", False),
                  "30-second automatic refresh without r", timeout=36)
    elapsed = time.monotonic() - started
    assert elapsed >= 25, f"Automatic refresh ran too early: {elapsed:.1f}s"
    assert sum(call.get("endpoint") == PULLS for call in calls(repo)) == old_listing_count + 1
    assert screen.viewport() == viewport, "Automatic refresh moved the viewport"
    assert "Refresh failed" not in "\n".join(screen.lines())
    terminal.assert_active()
    terminal.exit_and_assert_restored(b"q")


def follow_active_review(binary, repo, terminal, pause=None, started=None):
    root = Path(repo.env["ZPR_TEST_CONTROL"]).parent
    active = Path(repo.env["ZPR_TEST_STATE"]) / "active.json"
    original = active.read_bytes()
    alternate = (root / "stack-next-active.json").read_bytes()
    if pause is not None:
        terminal.wait(started.exists, "initial discovery after origin pins and checkout validation")
        terminal.assert_active()
        assert "PR stack" in terminal.screen.lines()[0]
        assert not current_visible(terminal.screen), "Initial discovery completed before the switch"
        pages = json.loads(Path(repo.env["ZPR_TEST_CONTROL"]).read_text())["pulls_pages"]

    def activate(contents):
        temporary = active.with_suffix(".pty-next")
        temporary.write_bytes(contents)
        temporary.replace(active)

    activate(alternate)
    try:
        if pause is not None:
            # Make the in-flight original discovery fail after origin validation.
            # Both sessions share commits, so this race needs no checkout writes.
            configure(repo, pulls_pages=[[24]])
            pause.unlink()
            terminal.wait(lambda: "Refresh failed: Current PR is absent" in "\n".join(terminal.screen.lines()),
                          "initial discovery failure after the active review changed")
            assert not current_visible(terminal.screen)
            assert sum(call.get("endpoint") == PULLS for call in calls(repo)) == 1
            configure(repo, pulls_pages=pages)
        terminal.send(b"r")
        terminal.wait(lambda: "@ #24 alternate-topic [ready] (current)" in terminal.screen.body()
                      and "│ New active review" in terminal.screen.body(),
                      "viewer to follow the changed active PR")
        assert "◆ alternate-main (base)" in terminal.screen.body()
        assert not current_visible(terminal.screen)
        assert active.read_bytes() == alternate, "Viewer replaced the new active session"

        # The running viewer follows active.json, but a new command from the
        # original ZED_FILE must still use its original, intact review session.
        result = subprocess.run([binary, "stack"], cwd=repo.path, env=repo.env,
                                input=b"", capture_output=True, timeout=8)
        assert result.returncode == 0, result.stdout.decode() + result.stderr.decode()
        assert b"@ #17 topic [draft]" in result.stdout, result.stdout
        assert b"@ #24" not in result.stdout, result.stdout
        assert active.read_bytes() == alternate, "Original diff command changed the active session"
        terminal.keep(lambda: any(line.startswith("@ #24 ") for line in terminal.screen.body()),
                      0.2, "Original diff command rebound the running viewer")
    finally:
        # Restore only the pointer and fake listing changed by this test. Rust's
        # checkpoint checks checkout, drafts, refs, and all transport calls.
        if pause is not None:
            pause.unlink(missing_ok=True)
            configure(repo, pulls_pages=pages)
        activate(original)
    terminal.send(b"r")
    terminal.wait(lambda: current_updated(terminal.screen, "Offline review fixture", True),
                  "viewer to follow the restored active PR")
    terminal.exit_and_assert_restored(b"q")


def stdout_only_tty(binary, repo):
    master, slave = pty.openpty()
    before = termios.tcgetattr(slave)
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 120, 0, 0))
    screen, output = Screen(40, 120), bytearray()
    child = subprocess.Popen([binary, "stack", "--watch"], cwd=repo.path, env=repo.env,
                             stdin=subprocess.PIPE, stdout=slave, stderr=slave)
    try:
        child.stdin.close()
        deadline = time.monotonic() + 8
        while child.poll() is None or select.select([master], [], [], 0)[0]:
            assert time.monotonic() < deadline, "stdout-only TTY did not finish its one-shot read"
            if select.select([master], [], [], 0.05)[0]:
                data = os.read(master, 65536)
                output.extend(data)
                screen.feed(data)
        assert child.returncode == 0, bytes(output)
        text = "\n".join(screen.lines())
        assert "PR stack" in text and "@ #17 topic [draft]" in text, text
        assert "○ #23 topic-23" in text and "◆ trunk (base)" in text, text
        assert text.count("(base)") == 1, text
        assert b"\x1b[?1049h" not in output, "Fallback entered the alternate screen"
        assert not any(screen.modes.get(mode) for mode in (1000, 1002, 1003, 1006, 1015))
        assert termios.tcgetattr(slave) == before, "Fallback changed terminal attributes"
    finally:
        if child.poll() is None:
            child.kill()
        child.wait(timeout=5)
        os.close(master)
        os.close(slave)


def deadline(_signal, _frame):
    raise TimeoutError("PR stack PTY scenario exceeded 45 seconds")


def main():
    binary, scenario = sys.argv[1:]
    os.setsid()
    signal.signal(signal.SIGHUP, signal.SIG_IGN)
    signal.signal(signal.SIGALRM, deadline)
    signal.alarm(45)
    # cli.rs already constructed this environment from Fixture.command. Do not
    # replace PATH or drop its offline transport and review-context variables.
    repo = SimpleNamespace(path=Path.cwd(), env=dict(os.environ))
    if scenario == "stdin-pipe":
        stdout_only_tty(binary, repo)
    elif scenario == "initial-active-review":
        pause, started = pause_listing(repo)
        pause.touch()
        with Terminal(binary, repo) as terminal:
            follow_active_review(binary, repo, terminal, pause, started)
    else:
        if scenario in ("navigation", "queued-refresh"):
            pause, started = pause_listing(repo)
        with Terminal(binary, repo) as terminal:
            terminal.wait(lambda: current_updated(terminal.screen, "Offline review fixture", True),
                          "initial @ #17 below the first page")
            assert "PR stack" in terminal.screen.lines()[0]
            assert terminal.screen.viewport() and terminal.screen.viewport()[0] > 1
            terminal.assert_active()
            assert_colored_graph(bytes(terminal.output))
            # Fast scenarios only check the start of the idle interval; the
            # automatic-refresh scenario below observes the full periodic read.
            previous = calls(repo)
            terminal.keep(lambda: current_visible(terminal.screen), 1.2,
                          "Idle viewer lost the current PR")
            assert calls(repo) == previous, "Idle viewer polled GitHub before 30 seconds"
            if scenario == "navigation":
                other_review_command(binary, repo, terminal)
                navigation(terminal)
                refresh(binary, repo, terminal, pause, started)
            elif scenario == "queued-refresh":
                queued_refresh(repo, terminal, pause, started)
            elif scenario == "automatic-refresh":
                automatic_refresh(repo, terminal)
            elif scenario == "active-review":
                follow_active_review(binary, repo, terminal)
            else:
                assert scenario in ("ctrl-c", "escape"), scenario
                terminal.exit_and_assert_restored({"ctrl-c": b"\x03", "escape": b"\x1b"}[scenario])
    print(f"PR stack PTY scenario {scenario} passed")


if __name__ == "__main__":
    main()
