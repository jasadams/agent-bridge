#!/usr/bin/env python3
"""Keyboard composer acceptance against disposable broker/native socket and PTY."""
import argparse
import base64
import errno
import json
import os
import re
from pathlib import Path
import select
import socket
import subprocess
import tempfile
import threading
import time
import uuid

from test_board_links import Viewer, stop


def run(binary):
    with tempfile.TemporaryDirectory(prefix="ab-msg-", dir="/tmp") as directory:
        root = Path(directory)
        native = root / "native"
        native.mkdir(mode=0o700)
        endpoint = native / "inbox.sock"
        listener = socket.socket(socket.AF_UNIX)
        listener.bind(str(endpoint))
        os.chmod(endpoint, 0o600)
        listener.listen()
        listener.settimeout(.1)
        frames, failures = [], []
        done = threading.Event()

        def receive():
            while not done.is_set():
                try:
                    connection, _ = listener.accept()
                except socket.timeout:
                    continue
                try:
                    with connection:
                        connection.settimeout(2)
                        payload = bytearray()
                        while b"\n" not in payload:
                            part = connection.recv(65536)
                            if not part:
                                break
                            payload.extend(part)
                        if payload:
                            frames.append(json.loads(payload.split(b"\n", 1)[0]))
                except Exception as error:
                    failures.append(str(error))

        thread = threading.Thread(target=receive)
        thread.start()
        state = root / "state"
        viewer = None
        with (root / "broker.log").open("wb") as log:
            broker = subprocess.Popen([str(binary), "--state-dir", str(state), "serve", "--no-discovery", "--codex-bin", "/missing/codex"], stdout=log, stderr=log)
            try:
                deadline = time.monotonic() + 10
                while not (state / "bridge.sock").exists():
                    assert broker.poll() is None and time.monotonic() < deadline
                    time.sleep(.03)

                def rpc(request):
                    with socket.socket(socket.AF_UNIX) as client:
                        client.settimeout(5)
                        client.connect(str(state / "bridge.sock"))
                        client.sendall(json.dumps(request).encode() + b"\n")
                        data = bytearray()
                        while b"\n" not in data:
                            part = client.recv(65536)
                            assert part, "broker closed before response"
                            data.extend(part)
                        response = json.loads(data.split(b"\n", 1)[0])
                        assert response["ok"], response
                        return response["result"]

                session = str(uuid.uuid4())
                rpc({"op": "register", "agent": {"name": "coordinator", "provider": "claude", "session": session, "endpoint": str(endpoint), "enabled": True}})
                rpc({"op": "board_create", "board": "alpha"})
                before = rpc({"op": "named_board_read", "board": "alpha"})
                added = rpc({"op": "add_board_message_input", "board": "alpha", "expected_revision": before["revision"], "agent": "coordinator", "id": "compose", "label": "Message coordinator"})
                original = rpc({"op": "named_board_update", "board": "alpha", "expected_revision": added["board"]["revision"], "edits": [{"op": "append", "text": "\nRun `printf hello` before composing.\n\n```text\n<!-- board-message: literal example -->\n```\n"}]})["board"]
                viewer = Viewer(binary, state, "alpha")
                accepted_ids = set()
                original_feed = viewer.screen.feed

                def record_rendered_acceptance(data):
                    # Sample the rendered status row within a read: several
                    # redraws may be coalesced before the parent drains the PTY.
                    for byte in data:
                        original_feed(bytes([byte]))
                        if byte in (32, ord("e")):
                            status = "".join(viewer.screen.cells.get((x, viewer.screen.rows - 3), (" ", None))[0] for x in range(viewer.screen.columns))
                            accepted_ids.update(re.findall(r"Accepted message ([0-9a-f-]{36})", status))

                viewer.screen.feed = record_rendered_acceptance
                viewer.wait(lambda: "Message coordinator" in viewer.screen.text())
                # Copy controls and their mouse path coexist with composer focus.
                viewer.keys(b"c\r")
                viewer.wait(lambda: any(value.startswith("52;c;") for value in viewer.screen.other_osc))
                payloads = lambda: [base64.b64decode(value[5:], validate=True) for value in viewer.screen.other_osc if value.startswith("52;c;")]
                assert payloads() == [b"printf hello"] and not frames
                viewer.keys(b"\x1b")
                point = next((x, y) for (x, y) in viewer.screen.cells if all(viewer.screen.cells.get((x+i, y), ("", None))[0] == ch for i, ch in enumerate("[Copy")))
                x, y = point
                viewer.keys(f"\x1b[<0;{x+1};{y+1}M\x1b[<0;{x+1};{y+1}m".encode())
                viewer.wait(lambda: len(payloads()) == 2)
                assert payloads() == [b"printf hello", b"printf hello"] and not frames
                viewer.keys(b"\t\r")
                viewer.keys("Exact first 🦀".encode())
                viewer.keys(b"\n")  # Ctrl-J: newline on legacy terminal key encoding.
                viewer.keys(b"second line")
                assert not frames, "typing and newline must not send"
                viewer.keys(b"\r\r\r")
                viewer.wait(lambda: len(frames) == 1)
                viewer.wait(lambda: frames[0]["msg_id"] in accepted_ids)
                assert "Exact first" not in viewer.screen.text() and "▏" in viewer.screen.text(), "exact accepted draft clears while composer stays focused"
                assert frames[0]["from"] == "user" and frames[0]["session_id"] == session
                assert "Exact first 🦀\nsecond line" in frames[0]["message"]["content"]
                assert rpc({"op": "named_board_read", "board": "alpha"}) == original, "send must not edit board"
                viewer.keys(b"retained draft")
                changed = rpc({"op": "named_board_update", "board": "alpha", "expected_revision": original["revision"], "edits": [{"op": "append", "text": "\nNew context\n"}]})
                viewer.wait(lambda: "Board changed" in viewer.screen.text())
                viewer.keys(b"\r")
                assert len(frames) == 1 and "retained draft" in viewer.screen.text()
                viewer.keys(b"\x12\r")  # Explicit Ctrl-R review then Enter send.
                viewer.wait(lambda: len(frames) == 2)
                viewer.wait(lambda: frames[1]["msg_id"] in accepted_ids)
                assert frames[1]["session_id"] == session
                assert "retained draft" in frames[1]["message"]["content"]
                assert frames[0]["msg_id"] != frames[1]["msg_id"]
                assert rpc({"op": "named_board_read", "board": "alpha"}) == changed["board"]
                history = rpc({"op": "history", "agent": "coordinator", "limit": 100})
                assert len(history) == 2 and len(frames) == 2, "rapid Enter must not create duplicate messages or native deliveries"
                # Leave the empty reusable composer and quit normally while draining.
                viewer.keys(b"\x1b")
                os.write(viewer.master, b"q")
                deadline = time.monotonic() + 5
                while viewer.process.poll() is None:
                    assert time.monotonic() < deadline, "normal keyboard quit timed out"
                    if select.select([viewer.master], [], [], .05)[0]:
                        try:
                            viewer.output.extend(os.read(viewer.master, 65536))
                        except OSError as error:
                            if error.errno != errno.EIO:
                                raise
                while select.select([viewer.master], [], [], 0)[0]:
                    try:
                        remaining = os.read(viewer.master, 65536)
                        if not remaining:
                            break
                        viewer.output.extend(remaining)
                    except OSError as error:
                        if error.errno != errno.EIO:
                            raise
                        break
                assert viewer.process.wait(timeout=1) == 0
                assert b"\x1b[?1049l" in viewer.output, "normal quit restores alternate screen"
                assert b"\x1b[?2004l" in viewer.output, "normal quit disables bracketed paste"
                assert not failures, failures
                print("PASS: real native delivery, Enter send/Ctrl-J newline, repeated Enter, focus, draft refresh/review, exact board, graceful quit/reset")
            finally:
                try:
                    if viewer is not None:
                        viewer.close()
                finally:
                    stop(broker)
                    done.set()
                    thread.join(timeout=3)
                    listener.close()
                    assert not thread.is_alive(), "native fixture did not stop"


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=Path("target/debug/agent-bridge"))
    run(parser.parse_args().binary.resolve())
