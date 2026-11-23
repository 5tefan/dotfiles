"""Bounded PTY regressions; invoked by log_terminal.rs, using only the stdlib."""

import codecs
import errno
import fcntl
import json
import os
from pathlib import Path
import pty
import re
import select
import signal
import struct
import subprocess
import sys
import termios
import time
import unicodedata


class Screen:
    """The cursor, erase, and mode sequences emitted by the Crossterm backend."""

    CSI = re.compile(r"\x1b\[([0-?]*)([ -/]*)([@-~])")

    def __init__(self, rows, columns):
        self.rows, self.columns = rows, columns
        self.cells = [[" "] * columns for _ in range(rows)]
        self.x = self.y = 0
        self.modes = {}
        self.decoder = codecs.getincrementaldecoder("utf-8")()
        self.pending = ""

    def feed(self, data):
        text = self.pending + self.decoder.decode(data)
        index = 0
        while index < len(text):
            char = text[index]
            if char == "\x1b":
                if index + 1 == len(text):
                    break
                if text[index + 1] == "[":
                    match = self.CSI.match(text, index)
                    if not match:
                        break
                    self.csi(match[1], match[3])
                    index = match.end()
                    continue
                if text[index + 1] == "]":
                    end = re.search(r"\x07|\x1b\\", text[index + 2:])
                    if not end:
                        break
                    index += 2 + end.end()
                    continue
                index += 2
                continue
            if char == "\r":
                self.x = 0
            elif char == "\n":
                self.y = min(self.rows - 1, self.y + 1)
            elif char == "\b":
                self.x = max(0, self.x - 1)
            elif char >= " " and not unicodedata.combining(char):
                if self.x >= self.columns:
                    self.x = 0
                    self.y = min(self.rows - 1, self.y + 1)
                width = 2 if unicodedata.east_asian_width(char) in "WF" else 1
                self.cells[self.y][self.x] = char
                if width == 2 and self.x + 1 < self.columns:
                    self.cells[self.y][self.x + 1] = ""
                self.x += width
            index += 1
        self.pending = text[index:]

    def csi(self, raw, final):
        args = [int(value or 0) for value in raw.lstrip("?").split(";")]
        n = args[0] or 1
        if raw.startswith("?") and final in "hl":
            for mode in args:
                self.modes[mode] = final == "h"
        elif final in "Hf":
            self.y = min(self.rows - 1, n - 1)
            self.x = min(self.columns - 1, (args[1] or 1) - 1 if len(args) > 1 else 0)
        elif final in "ABCD":
            self.y = max(0, min(self.rows - 1, self.y + {"A": -n, "B": n}.get(final, 0)))
            self.x = max(0, min(self.columns - 1, self.x + {"C": n, "D": -n}.get(final, 0)))
        elif final == "G":
            self.x = min(self.columns - 1, n - 1)
        elif final == "d":
            self.y = min(self.rows - 1, n - 1)
        elif final == "J":
            for row in range(self.rows):
                for column in range(self.columns):
                    if args[0] in (2, 3) or (args[0] == 0 and (row, column) >= (self.y, self.x)):
                        self.cells[row][column] = " "
        elif final == "K":
            start, end = {0: (self.x, self.columns), 1: (0, self.x + 1), 2: (0, self.columns)}[args[0]]
            self.cells[self.y][start:end] = [" "] * (end - start)
        elif final not in "mhl":
            raise AssertionError(f"Unsupported terminal sequence: {raw!r} {final!r}")

    def lines(self):
        return ["".join(row).rstrip() for row in self.cells]

    def body(self):
        return self.lines()[1:-2]

    def viewport(self):
        match = re.search(r"lines (\d+)–(\d+)/(\d+)", self.lines()[0])
        return tuple(map(int, match.groups())) if match else None

    def current_visible(self, change):
        return any(re.search(r"^@\s+" + re.escape(change[:12]), line) for line in self.body())

    def point(self, text):
        for row, line in enumerate(self.body(), start=1):
            column = line.find(text)
            if column >= 0:
                return column + 1, row + 1  # SGR mouse coordinates are one-based.
        raise AssertionError(f"No visible {text!r}")


class Repository:
    def __init__(self, root):
        self.path = root / "checkout with spaces"
        home, tools = root / "home", root / "bin"
        for path in (self.path, home, tools, root / "tmp"):
            path.mkdir()
        for program in ("gh", "viddy"):
            shim = tools / program
            shim.write_text("#!/bin/sh\necho 'Unexpected " + program + " invocation' >&2\nexit 99\n")
            shim.chmod(0o700)
        config = root / "jj.toml"
        config.write_text('''[user]
name = "Terminal Tests"
email = "terminal@example.test"
[ui]
pager = "cat"
[aliases]
l = ["log", "-r", "all()"]
[templates]
log = 'format_short_change_id_with_change_offset(self) ++ " " ++ description.first_line() ++ "\\n"'
[template-aliases]
"format_short_change_id_with_change_offset(commit)" = "commit.change_id().short(12)"
''')
        # Construct, rather than filter, the environment: no user Git/JJ config,
        # credentials, Zed context, review state override, or remote transport.
        self.env = {
            "PATH": str(tools) + os.pathsep + os.environ["PATH"],
            "HOME": str(home), "XDG_CONFIG_HOME": str(home),
            "TMPDIR": str(root / "tmp"), "JJ_CONFIG": str(config),
            "JJ_PAGER": "cat", "PAGER": "cat", "TERM": "xterm-256color",
            "LANG": "C", "LC_ALL": "C", "GIT_CONFIG_NOSYSTEM": "1",
            "GIT_CONFIG_GLOBAL": os.devnull, "GIT_TERMINAL_PROMPT": "0",
            "GIT_ALLOW_PROTOCOL": "",
        }
        self.jj("git", "init")
        for index in range(18):
            if index:
                self.jj("new")
            (self.path / "file.txt").write_text(f"contents of revision {index}\n")
            self.jj("describe", "-m", f"revision-{index:02}")
        records = self.jj("log", "--no-graph", "-r", "all() ~ root()", "-T",
                          'json({"name" => description.first_line(), "id" => change_id}) ++ "\\n"')
        self.changes = {entry["name"]: entry["id"] for entry in map(json.loads, records.splitlines())}
        with config.open("a") as stream:
            stream.write('\n[revset-aliases]\n"immutable_heads()" = '
                         + json.dumps(self.changes["revision-00"]) + '\n')
        assert self.jj("log", "--no-graph", "-r", self.changes["revision-00"],
                       "-T", "immutable") == "true", "Fixture requires an immutable revision"
        self.jj("edit", self.changes["revision-03"])

    def jj(self, *args):
        result = subprocess.run(["jj", "--no-pager", "--color=never", *args],
                                cwd=self.path, env=self.env, capture_output=True,
                                text=True, timeout=10)
        assert result.returncode == 0, f"jj {args!r}:\n{result.stdout}\n{result.stderr}"
        return result.stdout.strip()

    def current(self):
        return self.jj("--ignore-working-copy", "--at-operation=@", "log",
                       "--no-graph", "-r", "@", "-T", "change_id")


class Terminal:
    def __init__(self, binary, repo):
        self.master, self.slave = pty.openpty()
        self.screen = Screen(10, 80)
        self.output = bytearray()
        self.before = termios.tcgetattr(self.slave)
        fcntl.ioctl(self.slave, termios.TIOCSWINSZ, struct.pack("HHHH", 10, 80, 0, 0))
        # Python is the session leader, so all subprocesses remain in the group
        # that the Rust watchdog can kill. Give the viewer a controlling terminal.
        fcntl.ioctl(self.slave, termios.TIOCSCTTY, 0)
        self.child = subprocess.Popen([binary, "stack", "--watch"], cwd=repo.path,
                                      env=repo.env, stdin=self.slave,
                                      stdout=self.slave, stderr=self.slave)

    def __enter__(self):
        return self

    def __exit__(self, kind, error, traceback):
        if kind:
            print("Last terminal screen:\n" + "\n".join(self.screen.lines()), file=sys.stderr)
            print("Last terminal bytes:", repr(bytes(self.output[-3000:])), file=sys.stderr)
        if self.child.poll() is None:
            self.child.kill()
        self.child.wait(timeout=5)
        termios.tcsetattr(self.slave, termios.TCSANOW, self.before)
        os.close(self.master)
        os.close(self.slave)

    def pump(self, timeout=0.05):
        if select.select([self.master], [], [], timeout)[0]:
            try:
                data = os.read(self.master, 65536)
            except OSError as error:
                if error.errno != errno.EIO:
                    raise
                return
            self.output.extend(data)
            self.screen.feed(data)

    def wait(self, predicate, description, timeout=8):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            self.pump()
            if predicate():
                return
            assert self.child.poll() is None, f"Viewer exited while waiting for {description}"
        raise AssertionError(f"Timed out waiting for {description}")

    def keep(self, predicate, duration, description):
        deadline = time.monotonic() + duration
        while time.monotonic() < deadline:
            self.pump()
            assert self.child.poll() is None, "Viewer exited unexpectedly"
            assert predicate(), description

    def send(self, data):
        os.write(self.master, data)

    def mouse(self, button, point, release=False):
        self.send(f"\x1b[<{button};{point[0]};{point[1]}{'m' if release else 'M'}".encode())

    def click(self, point):
        self.mouse(0, point)
        self.mouse(0, point, release=True)

    def assert_active(self):
        assert self.screen.modes.get(1049), "Alternate screen was not enabled"
        assert self.screen.modes.get(1006), "SGR mouse reporting was not enabled"
        flags = termios.tcgetattr(self.slave)[3]
        assert not flags & (termios.ICANON | termios.ECHO | termios.ISIG), "Terminal is not raw"

    def exit_and_assert_restored(self, key):
        mouse_modes = {mode for mode, enabled in self.screen.modes.items()
                       if enabled and mode in (1000, 1002, 1003, 1006, 1015)}
        self.send(key)
        self.wait(lambda: self.child.poll() is not None, "clean exit")
        while select.select([self.master], [], [], 0)[0]:
            self.pump(0)
        assert self.child.returncode == 0, f"Exit status: {self.child.returncode}"
        assert termios.tcgetattr(self.slave) == self.before, "Raw mode was not restored"
        assert self.screen.modes.get(1049) is False, "Alternate screen was not left"
        assert mouse_modes and all(self.screen.modes.get(mode) is False for mode in mouse_modes), \
            "Mouse capture was not disabled"
        assert self.screen.modes.get(25) is True, "Cursor was not restored"


def navigation(terminal, repo):
    screen = terminal.screen
    terminal.send(b"g")
    terminal.wait(lambda: screen.viewport()[0] == 1, "scroll to top")
    assert not screen.current_visible(repo.changes["revision-03"])
    viewport = screen.viewport()
    # Changing a visible description proves that a periodic refresh completed;
    # simply sleeping could pass even if the refresh loop stopped entirely.
    repo.jj("describe", "-r", repo.changes["revision-17"], "-m", "revision-17-refreshed")
    terminal.wait(lambda: any("revision-17-refreshed" in line for line in screen.body()),
                  "periodic refresh of the visible description")
    terminal.keep(lambda: screen.viewport() == viewport and
                  not screen.current_visible(repo.changes["revision-03"]),
                  2.2, "Periodic refresh moved the viewport back to @")

    moved = repo.changes["revision-01"]
    repo.jj("edit", moved)
    terminal.wait(lambda: screen.current_visible(moved), "external checkout to become visible")

    target = repo.changes["revision-02"]
    point = screen.point(target[:12])
    terminal.mouse(0, point)
    terminal.keep(lambda: screen.current_visible(moved), 0.2, "Mouse down edited a revision")
    assert repo.current() == moved, "A mouse press alone must not edit"
    terminal.mouse(32, (point[0] + 1, point[1]))
    terminal.mouse(0, point, release=True)
    terminal.keep(lambda: screen.current_visible(moved), 0.2, "A drag edited a revision")
    assert repo.current() == moved, "Dragging must not edit"
    terminal.click(point)
    terminal.wait(lambda: screen.current_visible(target) and
                  "Editing " + target in screen.lines()[-2], "click to edit the mutable revision")
    assert repo.current() == target, "Click did not perform a real jj edit"
    assert (repo.path / "file.txt").read_text() == "contents of revision 2\n"

    terminal.send(b"G")
    terminal.wait(lambda: screen.viewport()[1] == screen.viewport()[2], "scroll to bottom")
    terminal.click(screen.point(repo.changes["revision-00"][:12]))
    terminal.wait(lambda: "Immutable revision; no checkout was attempted." in screen.lines()[-2],
                  "immutable click refusal")
    assert repo.current() == target, "Immutable click changed the checkout"
    terminal.exit_and_assert_restored(b"q")


def deadline(_signal, _frame):
    raise TimeoutError("PTY scenario exceeded 60 seconds")


def main():
    binary, root, scenario = sys.argv[1:]
    os.setsid()
    signal.signal(signal.SIGHUP, signal.SIG_IGN)
    signal.signal(signal.SIGALRM, deadline)
    signal.alarm(60)
    repo = Repository(Path(root))
    with Terminal(binary, repo) as terminal:
        terminal.wait(lambda: terminal.screen.current_visible(repo.changes["revision-03"]),
                      "initial @ below the first page")
        assert terminal.screen.viewport()[0] > 1, "Fixture did not put @ below the first page"
        terminal.assert_active()
        if scenario == "navigation":
            navigation(terminal, repo)
        else:
            assert scenario == "ctrl-c", scenario
            terminal.exit_and_assert_restored(b"\x03")
    assert not (repo.path / "scratch" / "zed-pr-review").exists(), "Viewer created review state"
    print(f"PTY scenario {scenario} passed")


if __name__ == "__main__":
    main()
