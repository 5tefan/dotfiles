"""Bounded PTY smoke tests for the actual finish CLI, invoked by cli.rs."""

import fcntl
import json
import os
from pathlib import Path
import pty
import select
import shlex
import signal
import struct
import subprocess
import sys
import termios

from log_terminal import Screen, Terminal


def terminal_attributes(fd):
    attributes = termios.tcgetattr(fd)
    return attributes[:6] + [
        [value if isinstance(value, int) else value[0] for value in attributes[6]]
    ]


def editor():
    # Check restoration at the handoff, not only after the entire CLI exits.
    fd = os.open("/dev/tty", os.O_RDWR)
    try:
        assert terminal_attributes(fd) == json.loads(os.environ["ZPR_REVIEW_PTY_TERMIOS"]), \
            "The summary editor started before the terminal was restored"
    finally:
        os.close(fd)
    marker = Path(os.environ["ZPR_TEST_EDITOR_LOG"]).with_name("review-terminal-editor-restored")
    marker.write_text("restored before editor\n")
    if "ZPR_REVIEW_PTY_EDITOR_PIPE" in os.environ:
        # Keep this descriptor open while paused. EOF lets the supervisor wait
        # for this editor to exit even after the CLI dies and it becomes orphaned.
        fd = int(os.environ["ZPR_REVIEW_PTY_EDITOR_PIPE"])
        os.write(fd, f"{os.getpid()}\n".encode())
        while True:
            signal.pause()
    command = shlex.split(os.environ["ZPR_REVIEW_PTY_EDITOR"])
    os.execvp(command[0], command + sys.argv[2:])


def default_signals():
    # The supervisor ignores SIGHUP to survive closing its controlling PTY. Do
    # not inherit that disposition when testing the CLI's post-menu defaults.
    for signum in (signal.SIGINT, signal.SIGTERM, signal.SIGHUP):
        signal.signal(signum, signal.SIG_DFL)


class ReviewTerminal(Terminal):
    def __init__(self, binary, pause_editor=False):
        self.master, self.slave = pty.openpty()
        self.screen = Screen(24, 100)
        self.output = bytearray()
        self.before = termios.tcgetattr(self.slave)
        fcntl.ioctl(self.slave, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 100, 0, 0))
        fcntl.ioctl(self.slave, termios.TIOCSCTTY, 0)
        env = dict(os.environ)
        env["ZPR_REVIEW_PTY_TERMIOS"] = json.dumps(terminal_attributes(self.slave))
        env["ZPR_REVIEW_PTY_EDITOR"] = env["EDITOR"]
        env["EDITOR"] = shlex.join([sys.executable, str(Path(__file__).resolve()), "--editor"])
        self.editor_exit = None
        descriptors = ()
        if pause_editor:
            self.editor_exit, writer = os.pipe()
            descriptors = (writer,)
            env["ZPR_REVIEW_PTY_EDITOR_PIPE"] = str(writer)
        try:
            self.child = subprocess.Popen(
                [binary, "finish"], env=env, stdin=self.slave, stdout=self.slave,
                stderr=self.slave, pass_fds=descriptors,
                preexec_fn=default_signals if pause_editor else None,
            )
        finally:
            for fd in descriptors:
                os.close(fd)

    def __exit__(self, kind, error, traceback):
        try:
            super().__exit__(kind, error, traceback)
        finally:
            if self.editor_exit is not None:
                os.close(self.editor_exit)


def interrupt_editor(terminal, signum):
    marker = Path(os.environ["ZPR_TEST_EDITOR_LOG"]).with_name("review-terminal-editor-restored")
    mouse_modes = {mode for mode, enabled in terminal.screen.modes.items()
                   if enabled and mode in (1000, 1002, 1003, 1006, 1015)}

    def assert_restored():
        assert termios.tcgetattr(terminal.slave) == terminal.before, "Raw mode was not restored"
        assert terminal.screen.modes.get(1049) is False, "Alternate screen was not left"
        assert terminal.screen.modes.get(25) is True, "Cursor was not restored"
        assert mouse_modes and all(terminal.screen.modes.get(mode) is False for mode in mouse_modes), \
            "Mouse capture was not disabled"

    editor_pid = None
    try:
        terminal.send(b"2")
        terminal.wait(lambda: marker.exists() and terminal.screen.modes.get(1049) is False,
                      "paused summary editor after terminal restoration")
        assert select.select([terminal.editor_exit], [], [], 5)[0], "Editor did not report its PID"
        editor_pid = int(os.read(terminal.editor_exit, 64).strip())
        assert editor_pid not in (os.getpid(), terminal.child.pid)
        assert_restored()
        assert terminal.child.poll() is None, "CLI exited before the signal"
        # Signal only the CLI, not the editor or the process group, and only
        # after the menu has ended. A swallowed signal must fail within 5 seconds.
        os.kill(terminal.child.pid, signum)
        try:
            status = terminal.child.wait(timeout=5)
        except subprocess.TimeoutExpired as error:
            raise AssertionError(f"CLI ignored {signal.Signals(signum).name} while the editor was paused") from error
        assert status == -signum, f"Expected termination by {signum}, got {status}"
        os.kill(editor_pid, 0)  # The CLI alone received the signal.
        while select.select([terminal.master], [], [], 0)[0]:
            terminal.pump(0)
        assert_restored()
    finally:
        if terminal.child.poll() is None:
            terminal.child.kill()
        terminal.child.wait(timeout=5)
        if editor_pid is not None:
            try:
                os.kill(editor_pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            assert select.select([terminal.editor_exit], [], [], 5)[0], "Paused editor did not exit"
            assert os.read(terminal.editor_exit, 64) == b"", "Editor exit pipe did not close"
        # Rust also kills the process group on every outcome, including failures
        # before the editor could report its PID.


def deadline(_signal, _frame):
    raise TimeoutError("Review PTY scenario exceeded 35 seconds")


def main():
    if sys.argv[1] == "--editor":
        editor()
        return
    binary, scenario = sys.argv[1:]
    # Keep the CLI, editor, and transports in the group killed by Rust's watchdog.
    os.setsid()
    signal.signal(signal.SIGHUP, signal.SIG_IGN)
    signal.signal(signal.SIGALRM, deadline)
    signal.alarm(35)
    editor_signals = {
        "editor-sigint": signal.SIGINT,
        "editor-sigterm": signal.SIGTERM,
        "editor-sighup": signal.SIGHUP,
    }
    with ReviewTerminal(binary, pause_editor=scenario in editor_signals) as terminal:
        choices = ["1  Exit + abandon", "2  Comment", "3  Approve", "4  Request changes"]
        terminal.wait(lambda: all(choice in "\n".join(terminal.screen.lines())
                                  for choice in choices), "all four finish choices")
        text = "\n".join(terminal.screen.lines())
        assert "#17" in text and "1 pending comment" in text, text
        assert "›" not in text, "Opening the menu must not select an action"
        terminal.assert_active()
        dismissals = {"quit": b"q", "escape": b"\x1b", "ctrl-c": b"\x03", "enter": b"\r"}
        if scenario in editor_signals:
            interrupt_editor(terminal, editor_signals[scenario])
        elif scenario in dismissals:
            terminal.exit_and_assert_restored(dismissals[scenario])
        elif scenario == "invalid":
            terminal.send(b"0x5")
            terminal.keep(lambda: "›" not in "\n".join(terminal.screen.lines()),
                          0.2, "Invalid keys selected an action")
            terminal.exit_and_assert_restored(b"q")
        elif scenario in ("1", "2", "3", "4"):
            # Numbers select immediately; do not send Enter to hide a regression.
            terminal.exit_and_assert_restored(scenario.encode())
        else:
            assert scenario in ("down-comment", "up-request"), scenario
            choice, keys = ("2  Comment", b"\x1b[B\x1b[B") if scenario == "down-comment" \
                else ("4  Request changes", b"\x1b[A")
            terminal.send(keys)
            terminal.wait(lambda: any("›" in line and choice in line
                                     for line in terminal.screen.lines()), "arrow selection")
            terminal.exit_and_assert_restored(b"\r")
    marker = Path(os.environ["ZPR_TEST_EDITOR_LOG"]).with_name("review-terminal-editor-restored")
    assert marker.exists() == (scenario in ("2", "3", "4", "down-comment", "up-request")
                               or scenario in editor_signals), \
        "Unexpected editor handoff"
    print(f"Review PTY scenario {scenario} passed")


if __name__ == "__main__":
    main()
