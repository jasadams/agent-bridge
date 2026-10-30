#!/usr/bin/env python3
"""Keyboard composer acceptance against disposable broker/native socket and PTY."""
import argparse
import base64
import errno
import fcntl
import json
import os
import re
from pathlib import Path
import select
import signal
import struct
import termios
import socket
import sqlite3
import subprocess
import tempfile
import threading
import time
import uuid

from test_board_links import Viewer, stop


def run(binary, output=None):
    if output is not None:
        output.mkdir(parents=True, exist_ok=True)

    def capture(viewer, name):
        if output is not None:
            (output / (name + ".txt")).write_text(viewer.screen.text() + "\n")

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
                            status = "\n".join("".join(viewer.screen.cells.get((x, y), (" ", None))[0] for x in range(viewer.screen.columns)) for y in range(viewer.screen.rows - 3, viewer.screen.rows))
                            accepted_ids.update(re.findall(r"Accepted message ([0-9a-f-]{36})", status))

                viewer.screen.feed = record_rendered_acceptance
                viewer.wait(lambda: "coordinator" in viewer.screen.text() and "[Send]" in viewer.screen.text())
                viewer.wait(lambda: "Last Updated:" in viewer.screen.text())
                assert isinstance(original["last_updated"], int)
                def timestamp_footer():
                    match = re.search(r"Last Updated: ([^\n┘─]+)", viewer.screen.text())
                    assert match, viewer.screen.text()
                    return match.group(1).strip()
                original_footer = timestamp_footer()
                assert "unknown" not in original_footer.lower()
                assert str(endpoint) not in viewer.screen.text(), "hidden composer endpoint leaked into rendered document"
                # Copy controls and their mouse path coexist with composer focus.
                viewer.keys(b"c\r")
                viewer.wait(lambda: any(value.startswith("52;c;") for value in viewer.screen.other_osc))
                payloads = lambda: [base64.b64decode(value[5:], validate=True) for value in viewer.screen.other_osc if value.startswith("52;c;")]
                assert payloads() == [b"printf hello"] and not frames
                viewer.keys(b"\x1b")
                point = next((x, y) for (x, y) in viewer.screen.cells if all(viewer.screen.cells.get((x+i, y), ("", None))[0] == ch for i, ch in enumerate("[⧉")))
                x, y = point
                viewer.keys(f"\x1b[<0;{x+1};{y+1}M\x1b[<0;{x+1};{y+1}m".encode())
                viewer.wait(lambda: len(payloads()) == 2)
                assert payloads() == [b"printf hello", b"printf hello"] and not frames
                viewer.keys(b"c\t\r")
                viewer.wait(lambda: len(payloads()) == 3)
                assert payloads()[-1] == b"<!-- board-message: literal example -->\n"
                assert not frames, "copying a literal example must not send a message"
                assert rpc({"op": "named_board_read", "board": "alpha"}) == original
                assert timestamp_footer() == original_footer
                viewer.keys(b"\x1b")
                viewer.keys(b"\t")  # Tab enters the editable box directly, without activation Enter.
                capture(viewer, "message-single-tab-focus")
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
                assert timestamp_footer() == original_footer, "sending changed the displayed update time"
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
                # Mouse Send must dispatch immediately, without a later keyboard event.
                viewer.keys(b"mouse send without keyboard")
                x, y = next((x, y) for (x, y) in viewer.screen.cells if all(
                    viewer.screen.cells.get((x+i, y), ("", None))[0] == ch
                    for i, ch in enumerate("[Send]")))
                viewer.keys(f"\x1b[<0;{x+1};{y+1}M\x1b[<0;{x+1};{y+1}m".encode())
                # Only drain PTY output and inspect RPCs from here until accepted.
                viewer.wait(lambda: len(frames) == 3)
                viewer.wait(lambda: frames[2]["msg_id"] in accepted_ids)
                viewer.wait(lambda: "mouse send without keyboard" not in viewer.screen.text())
                assert frames[2]["session_id"] == session
                history = rpc({"op": "history", "agent": "coordinator", "limit": 100})
                assert len(history) == 3 and any(message["body"] == "mouse send without keyboard" for message in history)
                assert rpc({"op": "named_board_read", "board": "alpha"}) == changed["board"]
                capture(viewer, "message-mouse-send-accepted-without-key")
                # Real multi-owner layout, click focus, overflow reveal and independent scroll.
                viewer.keys(b"\x1b")
                for owner in ("reviewer", "planner", "builder", "tester", "writer"):
                    rpc({"op": "register", "agent": {"name": owner, "provider": "inbox", "session": str(uuid.uuid4()), "endpoint": "", "enabled": True}})
                    current = rpc({"op": "named_board_read", "board": "alpha"})
                    rpc({"op": "add_board_message_input", "board": "alpha", "expected_revision": current["revision"], "agent": owner, "id": owner, "label": "Message " + owner})
                current = rpc({"op": "named_board_read", "board": "alpha"})
                layout_board = rpc({"op": "named_board_update", "board": "alpha", "expected_revision": current["revision"], "edits": [{"op": "append", "text": "\n" + "Scrollable context\n\n" * 80}]})["board"]
                viewer.wait(lambda: layout_board["revision"][:12] in viewer.screen.text() and "/6" in viewer.screen.text())

                def title_y(label):
                    return next(y for y, line in enumerate(viewer.screen.text().splitlines()) if label in line)

                y = title_y("coordinator") + 1
                viewer.keys(f"\x1b[<0;3;{y+1}M\x1b[<0;3;{y+1}m".encode())
                viewer.keys(b"click coordinator draft")
                viewer.wait(lambda: "click coordinator draft" in viewer.screen.text())
                capture(viewer, "message-stacked-90x30-click")
                dock_y = title_y("coordinator")
                before_dock = viewer.screen.text().splitlines()[dock_y:-2]
                viewer.keys(b"\x1b[6~")  # Page Down scrolls only the board while composing.
                assert viewer.screen.text().splitlines()[dock_y:-2] == before_dock
                capture(viewer, "message-stacked-90x30-board-scroll")
                viewer.keys(b"\tsecond owner draft")
                viewer.wait(lambda: "second owner draft" in viewer.screen.text())
                viewer.keys(b"\x1b[Z")
                viewer.wait(lambda: "click coordinator draft" in viewer.screen.text())
                viewer.keys(b"\t\t\t\t\t")
                viewer.wait(lambda: "writer" in viewer.screen.text())
                for width, height in ((80, 16), (40, 18)):
                    viewer.screen.columns, viewer.screen.rows = width, height
                    fcntl.ioctl(viewer.master, termios.TIOCSWINSZ, struct.pack("HHHH", height, width, 0, 0))
                    os.kill(viewer.process.pid, signal.SIGWINCH)
                    viewer.pump(.4)
                    viewer.wait(lambda: "writer" in viewer.screen.text() and "[Send]" in viewer.screen.text())
                    assert title_y("writer") >= 4, "Composer stack hid the board"
                    capture(viewer, f"message-stacked-{width}x{height}-last-focus")
                # Refresh a nonempty exact target draft: show honest stale feedback.
                viewer.keys(b"draft for stale check")
                current = rpc({"op": "named_board_read", "board": "alpha"})
                rpc({"op": "named_board_update", "board": "alpha", "expected_revision": current["revision"], "edits": [{"op": "append", "text": "\nChanged context\n"}]})
                viewer.wait(lambda: "Review target" in viewer.screen.text())
                capture(viewer, "message-stacked-stale-target")
                assert len(frames) == 3, "Layout/focus checks unexpectedly submitted a message"
                # Explicitly discard the private test drafts, then quit normally.
                viewer.keys(b"\x1b")
                viewer.keys(b"\x04")
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
                # Real canonical registration IDs and separate native titles must
                # render differently, without changing the pinned owner or board.
                viewer.close()
                canonical_session = "885b5a56-af8d-4a72-9b43-8ad930e945b6"
                canonical = "claude-" + canonical_session
                rpc({"op": "register", "agent": {"name": canonical, "provider": "claude", "session": canonical_session, "endpoint": str(endpoint), "enabled": True}})
                rpc({"op": "board_create", "board": "friendly"})
                for field, label in (("friendly", "Support desk"), ("unknown", "Message " + canonical)):
                    snapshot = rpc({"op": "named_board_read", "board": "friendly"})
                    rpc({"op": "add_board_message_input", "board": "friendly", "expected_revision": snapshot["revision"], "agent": canonical, "id": field, "label": label})
                baseline = rpc({"op": "named_board_read", "board": "friendly"})
                def native_title(session, value):
                    with sqlite3.connect(state / "messages.sqlite3") as db:
                        db.execute("INSERT INTO session_metadata(provider,session,display_name) VALUES('claude',?,?) ON CONFLICT(provider,session) DO UPDATE SET display_name=excluded.display_name", (session, value))
                viewer = Viewer(binary, state, "friendly")
                viewer.wait(lambda: "Support desk" in viewer.screen.text() and "Claude" in viewer.screen.text())
                assert canonical not in viewer.screen.text()
                capture(viewer, "message-canonical-no-title-fallback")
                native_title(canonical_session, "Coordinator")
                viewer.wait(lambda: "Coordinator" in viewer.screen.text())
                capture(viewer, "message-canonical-friendly-title")
                viewer.keys(b"\tretained native draft\x1b[D\x1b[D")
                native_title(canonical_session, "Planner")
                viewer.wait(lambda: "Planner" in viewer.screen.text())
                viewer.keys(b"_")
                viewer.wait(lambda: "retained native dra_ft" in viewer.screen.text().replace("▏", ""))
                capture(viewer, "message-canonical-renamed-title-caret")
                assert rpc({"op": "named_board_read", "board": "friendly"}) == baseline
                viewer.keys(b"\r")
                viewer.wait(lambda: len(frames) == 4)
                assert frames[3]["session_id"] == canonical_session
                history = rpc({"op": "history", "agent": canonical, "limit": 100})
                assert len(history) == 1 and history[0]["recipient"] == canonical
                assert history[0]["body"] == "retained native dra_ft"
                viewer.wait(lambda: "retained native" not in viewer.screen.text())
                viewer.keys(b"old owner draft")
                replacement = str(uuid.uuid4())
                rpc({"op": "register", "agent": {"name": canonical, "provider": "claude", "session": replacement, "endpoint": str(endpoint), "enabled": True}, "replace": True})
                native_title(replacement, "Wrong replacement")
                viewer.wait(lambda: "Support desk" in viewer.screen.text())
                assert "Wrong replacement" not in viewer.screen.text()
                viewer.keys(b"\r")
                viewer.wait(lambda: "closed, disabled or replaced" in viewer.screen.text())
                assert len(frames) == 4 and "old owner draft" in viewer.screen.text()
                assert rpc({"op": "named_board_read", "board": "friendly"}) == baseline
                capture(viewer, "message-canonical-replaced-owner")
                if output is not None:
                    (output / "friendly-label-fixture.json").write_text(json.dumps({"canonical": canonical, "session": canonical_session, "friendly_title": "Coordinator", "renamed_title": "Planner", "replacement_title": "Wrong replacement", "authored_fallback": "Support desk", "board_revision": baseline["revision"]}, indent=2))
                viewer.keys(b"\x1b")
                viewer.keys(b"\x04")
                os.write(viewer.master, b"q")
                viewer.process.wait(timeout=5)
                # Default main must use the metadata-capable inspection path too.
                viewer.close()
                rpc({"op": "register", "agent": {"name": canonical, "provider": "claude", "session": canonical_session, "endpoint": str(endpoint), "enabled": True}, "replace": True})
                native_title(canonical_session, "Main coordinator")
                rpc({"op": "board_create", "board": "main"})
                main_before = rpc({"op": "named_board_read", "board": "main"})
                rpc({"op": "add_board_message_input", "board": "main", "expected_revision": main_before["revision"], "agent": canonical, "id": "main-friendly", "label": "Message " + canonical})
                main_before = rpc({"op": "named_board_read", "board": "main"})
                viewer = Viewer(binary, state, "main")
                viewer.wait(lambda: "Main coordinator" in viewer.screen.text())
                assert canonical not in viewer.screen.text()
                capture(viewer, "message-main-canonical-friendly-title")
                viewer.keys(b"\tmain retained draft")
                native_title(canonical_session, "Main planner")
                viewer.wait(lambda: "Main planner" in viewer.screen.text())
                assert "main retained draft" in viewer.screen.text()
                assert rpc({"op": "named_board_read", "board": "main"}) == main_before
                capture(viewer, "message-main-canonical-renamed-title")
                if output is not None:
                    (output / "friendly-main-fixture.json").write_text(json.dumps({"board": "main", "canonical": canonical, "session": canonical_session, "friendly_title": "Main coordinator", "renamed_title": "Main planner", "board_revision": main_before["revision"], "last_updated": main_before["last_updated"]}, indent=2))
                viewer.keys(b"\x1b")
                viewer.keys(b"\x04")
                os.write(viewer.master, b"q")
                viewer.process.wait(timeout=5)
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
    parser.add_argument("--output", type=Path, help="Persist actual PTY screen evidence")
    args = parser.parse_args()
    run(args.binary.resolve(), args.output.resolve() if args.output else None)
