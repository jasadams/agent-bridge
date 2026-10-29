#!/usr/bin/env python3
"""OSC-8-aware PTY regression using only a disposable broker and synthetic data."""
import argparse
import base64
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
import socket
import struct
import subprocess
import tempfile
import termios
import time
import unicodedata


class Screen:
    """Small screen model for the cursor/erase/OSC commands emitted by this TUI."""
    def __init__(self, columns=90, rows=30):
        self.columns, self.rows = columns, rows
        self.x = self.y = 0
        self.cells = {}
        self.target = None
        self.opens = self.closes = 0
        self.other_osc = []
        self.pending = ""
        self.decoder = codecs.getincrementaldecoder("utf-8")()

    def feed(self, data):
        self.pending += self.decoder.decode(data)
        while self.pending:
            text = self.pending
            if text.startswith("\x1b]"):
                end = text.find("\x1b\\", 2)
                bell = text.find("\x07", 2)
                if bell >= 0 and (end < 0 or bell < end):
                    end, length = bell, 1
                elif end >= 0:
                    length = 2
                else:
                    break
                payload = text[2:end]
                self.pending = text[end + length:]
                if payload.startswith("8;"):
                    _, parameters, destination = payload.split(";", 2)
                    assert parameters == "", parameters
                    if destination:
                        assert destination.startswith(("http://", "https://")), destination
                        assert self.target is None, "unbalanced nested hyperlink"
                        self.opens += 1
                    else:
                        self.closes += 1
                    self.target = destination or None
                else:
                    self.other_osc.append(payload)
                continue
            if text.startswith("\x1b["):
                match = re.match(r"\x1b\[([\x20-\x3f]*)([\x40-\x7e])", text)
                if match is None:
                    break
                raw, command = match.groups()
                self.pending = text[match.end():]
                args = [int(value or 0) for value in raw.split(";")] if raw.replace(";", "").isdigit() else []
                if command in ("H", "f"):
                    self.y = max(0, (args[0] if args else 1) - 1)
                    self.x = max(0, (args[1] if len(args) > 1 else 1) - 1)
                elif command == "J" and args == [2]:
                    self.cells.clear()
                elif command == "K":
                    mode = args[0] if args else 0
                    for position in list(self.cells):
                        x, y = position
                        if y == self.y and (mode == 2 or (mode == 0 and x >= self.x) or (mode == 1 and x <= self.x)):
                            del self.cells[position]
                elif command in ("h", "l") and raw == "?1049":
                    self.cells.clear()
                continue
            if text[0] == "\x1b":
                if len(text) < 2:
                    break
                self.pending = text[2:]
                continue
            character = text[0]
            self.pending = text[1:]
            if character == "\r":
                self.x = 0
            elif character == "\n":
                self.y += 1
            elif not character.isprintable() and not unicodedata.combining(character):
                continue
            elif unicodedata.combining(character):
                position = (max(0, self.x - 1), self.y)
                previous, target = self.cells.get(position, ("", self.target))
                self.cells[position] = (previous + character, target)
            else:
                width = 2 if unicodedata.east_asian_width(character) in ("W", "F") else 1
                self.cells[(self.x, self.y)] = (character, self.target)
                for extra in range(1, width):
                    self.cells[(self.x + extra, self.y)] = ("", self.target)
                self.x += width

    def links(self):
        return {position: value for position, value in self.cells.items() if value[1]}

    def text(self):
        return "\n".join("".join(self.cells.get((x, y), (" ", None))[0] for x in range(self.columns)) for y in range(self.rows))


def stop(process, output=None):
    """Reap a fixture child while draining its PTY, including after SIGKILL."""
    def wait():
        nonlocal output
        end = time.monotonic() + 5
        while process.poll() is None:
            remaining = end - time.monotonic()
            if remaining <= 0:
                raise subprocess.TimeoutExpired(process.args, 5)
            try:
                process.wait(timeout=min(.05, remaining))
            except subprocess.TimeoutExpired:
                if output is not None and select.select([output], [], [], 0)[0]:
                    try:
                        if not os.read(output, 65536):
                            output = None
                    except OSError as error:
                        if error.errno != errno.EIO:
                            raise
                        output = None

    if process.poll() is None:
        process.terminate()
    try:
        wait()
    except subprocess.TimeoutExpired:
        process.kill()
        wait()


class Viewer:
    def __init__(self, binary, state, board, disabled=False, env=None):
        self.master, self.slave = pty.openpty()
        self.screen = Screen()
        self.output = bytearray()
        fcntl.ioctl(self.slave, termios.TIOCSWINSZ, struct.pack("HHHH", self.screen.rows, self.screen.columns, 0, 0))
        def controlling_terminal():
            os.setsid()
            fcntl.ioctl(0, termios.TIOCSCTTY, 0)
        env = {**(os.environ if env is None else env), "TERM": "xterm-256color"}
        env.pop("NO_TERM_HYPERLINKS", None)
        if disabled:
            env["NO_TERM_HYPERLINKS"] = "1"
        try:
            self.process = subprocess.Popen([str(binary), "--state-dir", str(state), "board", board], stdin=self.slave, stdout=self.slave, stderr=self.slave, env=env, preexec_fn=controlling_terminal)
        finally:
            os.close(self.slave)
            self.slave = None

    def pump(self, duration=.1):
        end = time.monotonic() + duration
        while time.monotonic() < end:
            if select.select([self.master], [], [], .03)[0]:
                data = os.read(self.master, 65536)
                self.output.extend(data)
                self.screen.feed(data)
            assert self.process.poll() is None, repr(self.output[-2000:])

    def wait(self, condition):
        end = time.monotonic() + 8
        while not condition():
            assert time.monotonic() < end, self.screen.text()
            self.pump()

    def keys(self, keys):
        os.write(self.master, keys)
        self.pump(.2)

    def close(self):
        try:
            try:
                stop(self.process, self.master)
            except subprocess.TimeoutExpired:
                # Closing the master releases any remaining terminal I/O wait.
                os.close(self.master)
                self.master = None
                self.process.wait(timeout=5)
        finally:
            for name in ("master", "slave"):
                descriptor = getattr(self, name)
                if descriptor is not None:
                    os.close(descriptor)
                    setattr(self, name, None)


def run(binary):
    with tempfile.TemporaryDirectory(prefix="agb-links-", dir="/tmp") as directory:
        state = Path(directory) / "state"
        with open(Path(directory) / "broker.log", "wb") as log:
            broker = subprocess.Popen([str(binary), "--state-dir", str(state), "serve", "--no-discovery", "--codex-bin", "/missing/codex"], stdout=log, stderr=log)
            viewer = None
            try:
                end = time.monotonic() + 10
                while not (state / "bridge.sock").exists():
                    assert broker.poll() is None and time.monotonic() < end
                    time.sleep(.03)
                def rpc(request):
                    with socket.socket(socket.AF_UNIX) as connection:
                        connection.settimeout(5)
                        connection.connect(str(state / "bridge.sock"))
                        connection.sendall(json.dumps(request).encode() + b"\n")
                        response = b""
                        while not response.endswith(b"\n"):
                            chunk = connection.recv(262144)
                            assert chunk, "broker closed response"
                            response += chunk
                        result = json.loads(response)
                        assert result["ok"], result
                        return result["result"]
                def write(name, markdown):
                    snapshot = rpc({"op": "named_board_read", "board": name})
                    rpc({"op": "named_board_update", "board": name, "expected_revision": snapshot["revision"], "edits": [{"op": "set", "markdown": markdown}]})
                def has_target(target):
                    return any(value[1] == target for value in viewer.screen.links().values())
                for name in ["alpha", "beta"]:
                    rpc({"op": "board_create", "board": name})
                old, new, latest = [f"https://example.test/{name}" for name in ["old", "new", "latest"]]
                original = "# Links\n\n| Ticket | Note |\n|---|---|\n| [TICKET](" + old + ") | plain |\n\nQuestion\n<!-- board-input: response -->\n> original\n<!-- /board-input -->\n\n" + "plain line\n\n" * 45
                write("alpha", original)
                write("beta", "# Beta\n\n[TICKET](https://example.test/beta)\n")
                viewer = Viewer(binary, state, "alpha")
                viewer.wait(lambda: has_target(old))
                assert viewer.screen.other_osc == [], viewer.screen.other_osc
                linked = sorted(viewer.screen.links(), key=lambda position: (position[1], position[0]))
                x, y = linked[0]
                viewer.keys(f"\x1b[<0;{x+1};{y+1}M".encode())
                changed = original.replace(old, new)
                write("alpha", changed)
                viewer.pump(1.3)
                assert has_target(old) and not has_target(new), "selection must freeze target as well as label"
                viewer.keys(f"\x1b[<32;{x+6};{y+1}M\x1b[<0;{x+6};{y+1}m".encode())
                copies = [value.split(";", 2)[2] for value in viewer.screen.other_osc if value.startswith("52;")]
                assert copies and base64.b64decode(copies[-1]) == b"TICKET", copies
                assert has_target(old), "completed selection must keep its frozen target"
                viewer.keys(b"\x1b")
                viewer.wait(lambda: has_target(new) and not has_target(old))
                changed = changed.replace(new, latest)
                write("alpha", changed)
                viewer.wait(lambda: has_target(latest) and not has_target(new))
                viewer.keys(b"\x1b[F")
                viewer.wait(lambda: not viewer.screen.links())
                viewer.keys(b"\x1b[H")
                viewer.wait(lambda: has_target(latest))
                viewer.screen.columns, viewer.screen.rows = 55, 22
                fcntl.ioctl(viewer.master, termios.TIOCSWINSZ, struct.pack("HHHH", 22, 55, 0, 0))
                viewer.process.send_signal(signal.SIGWINCH)
                viewer.wait(lambda: has_target(latest))
                viewer.keys(b"\t\r")
                viewer.keys(b" [DRAFT](https://example.test/draft)")
                assert not has_target("https://example.test/draft")
                viewer.keys(b"\x1b")
                viewer.keys(b"b")
                viewer.wait(lambda: "Boards" in viewer.screen.text())
                assert not viewer.screen.links(), "picker must clear document links"
                viewer.keys(b"j")
                viewer.keys(b"\r")
                viewer.wait(lambda: has_target("https://example.test/beta"))
                assert not has_target(latest)
                before_osc = len(viewer.screen.other_osc)
                unsafe = "# SAFE\n[bad](https://example.test/&#x1b;bad)\n[JS](javascript:alert(1))\n\x1b]52;c;INJECTED\x07\n"
                write("beta", unsafe)
                viewer.wait(lambda: "SAFE" in viewer.screen.text() and not viewer.screen.links())
                assert len(viewer.screen.other_osc) == before_osc, viewer.screen.other_osc
                assert viewer.screen.opens == viewer.screen.closes and viewer.screen.target is None
                raw = subprocess.check_output([str(binary), "--state-dir", str(state), "board", "beta", "--raw"])
                assert raw == unsafe.encode(), "raw Markdown must stay exact"
                viewer.close()
                viewer = None
                viewer = Viewer(binary, state, "alpha", disabled=True)
                viewer.wait(lambda: "TICKET" in viewer.screen.text())
                assert not viewer.screen.links() and viewer.screen.opens == 0
                assert rpc({"op": "named_board_read", "board": "alpha"})["markdown"] == changed
                print("PASS: PTY OSC8 targets, same-label refresh, frozen copy, scroll/resize, editor/picker/board isolation, injection, raw exactness and disabled fallback")
            finally:
                try:
                    if viewer is not None:
                        viewer.close()
                finally:
                    stop(broker)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=Path("target/debug/agent-bridge"))
    run(parser.parse_args().binary.resolve())
