#!/usr/bin/env python3
"""Actual TUI timestamp qualification with private state and viewer-local TZ."""
import argparse
import base64
import fcntl
import hashlib
import json
import os
from pathlib import Path
import signal
import socket
import struct
import subprocess
import tempfile
import termios
import time
import traceback

from test_board_links import Viewer, stop


def run(binary, output):
    output.mkdir(mode=0o700, parents=True, exist_ok=False)
    state = output / "state"
    env = {"PATH": os.environ.get("PATH", "/usr/bin:/bin"), "LANG": "C.UTF-8"}
    for key, name in [("HOME", "home"), ("XDG_CONFIG_HOME", "config"),
                      ("XDG_DATA_HOME", "data"), ("XDG_CACHE_HOME", "cache"),
                      ("XDG_STATE_HOME", "state-home"), ("TMPDIR", "tmp"),
                      ("CODEX_HOME", "codex"), ("CLAUDE_CONFIG_DIR", "claude")]:
        folder = output / name
        folder.mkdir(mode=0o700)
        env[key] = str(folder)
    result = {"binary_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
              "scope": "actual private broker and PTY TUI, no model/owner configuration",
              "checks": [], "complete": False}
    viewer = broker = None
    try:
        with (output / "broker.log").open("wb") as log:
            broker = subprocess.Popen([str(binary), "--state-dir", str(state), "serve",
                                       "--no-discovery", "--codex-bin", "/missing/codex"],
                                      env=env, stdout=log, stderr=log)
            deadline = time.monotonic() + 10
            while not (state / "bridge.sock").exists():
                assert broker.poll() is None and time.monotonic() < deadline
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
                    value = json.loads(response)
                    assert value["ok"], value
                    return value["result"]

            def read(name):
                return rpc({"op": "named_board_read", "board": name})

            def external(name, markdown, stamp):
                # Publish Markdown and its synthetic timestamp together; no
                # watcher can observe an intermediate current-time mtime.
                staged = output / "staged.md"
                staged.write_text(markdown)
                staged.chmod(0o600)
                os.utime(staged, ns=(stamp * 1_000_000, stamp * 1_000_000))
                os.replace(staged, state / "boards" / (name + ".md"))
                snapshot = read(name)
                assert snapshot["last_updated"] == stamp
                return snapshot

            def record(label):
                result["checks"].append(label)
                print("PASS: " + label, flush=True)

            for name in ["alpha", "beta"]:
                rpc({"op": "board_create", "board": name})
            summer = 1_768_435_200_000
            winter = 1_784_073_600_000
            markdown = "# Alpha\n\n" + "scrollable board line\n\n" * 60
            first = external("alpha", markdown, summer)
            external("beta", "# Beta\n\nIndependent content\n", summer + 3_600_000)
            viewer = Viewer(binary, state, "alpha", env=env, timezone="Australia/Sydney")
            viewer.wait(lambda: "15 Jan 11:00 AEDT" in viewer.screen.text())
            record("actual TUI bottom border displays Sydney summer time outside Markdown")
            viewer.keys(b"\x1b[F")
            viewer.wait(lambda: "15 Jan 11:00 AEDT" in viewer.screen.text())
            assert "Last Updated" in viewer.screen.text()
            record("bottom-border timestamp remains visible at document end")
            changed = external("alpha", markdown + "external change\n", winter)
            viewer.wait(lambda: "15 Jul 10:00 AEST" in viewer.screen.text())
            record("live external edit refreshes to winter offset without restarting TUI")
            noop = rpc({"op": "named_board_update", "board": "alpha",
                        "expected_revision": changed["revision"],
                        "edits": [{"op": "set", "markdown": changed["markdown"]}]})
            assert noop["board"]["last_updated"] == winter
            conflict = rpc({"op": "named_board_update", "board": "alpha",
                            "expected_revision": first["revision"],
                            "edits": [{"op": "append", "text": "not applied"}]})
            assert conflict["conflict"] and conflict["board"]["last_updated"] == winter
            cli = subprocess.run([str(binary), "--state-dir", str(state), "board", "alpha", "--metadata"],
                                 env=env, capture_output=True, timeout=10, check=True)
            assert json.loads(cli.stdout)["last_updated"] == winter
            raw = subprocess.run([str(binary), "--state-dir", str(state), "board", "alpha", "--raw"],
                                 env=env, capture_output=True, timeout=10, check=True)
            assert raw.stdout == changed["markdown"].encode()
            record("CLI UTC metadata matches revision while raw/no-op/conflict preserve content and time")
            viewer.keys(b"b")
            viewer.wait(lambda: "Boards" in viewer.screen.text())
            viewer.keys(b"j\r")
            viewer.wait(lambda: "Beta" in viewer.screen.text() and "15 Jan 12:00 AEDT" in viewer.screen.text())
            assert read("alpha")["last_updated"] == winter
            record("switching named boards selects independent persisted timestamp")
            viewer.screen.columns, viewer.screen.rows = 18, 12
            fcntl.ioctl(viewer.master, termios.TIOCSWINSZ, struct.pack("HHHH", 12, 18, 0, 0))
            viewer.process.send_signal(signal.SIGWINCH)
            viewer.wait(lambda: "12:00 AEDT" in viewer.screen.text())
            record("narrow real terminal uses a compact nonwrapping timezone footer")
            # Re-open private viewers to qualify the merged timestamp and copy
            # bindings together, independently for mouse and keyboard activation.
            combined = []
            for mode in ("mouse", "keyboard"):
                item = {"mode": mode, "passed": False}
                try:
                    if viewer is not None:
                        viewer.close()
                        viewer = None
                    name = "combined-" + mode
                    rpc({"op": "board_create", "board": name})
                    old = external(name, "# Combined\nLEFT `OLD_COMMAND` RIGHT\n", summer)
                    viewer = Viewer(binary, state, name, env=env, timezone="Australia/Sydney")
                    viewer.wait(lambda: "OLD_COMMAND" in viewer.screen.text() and "15 Jan 11:00 AEDT" in viewer.screen.text())

                    def point(label):
                        for y in range(viewer.screen.rows):
                            for x in range(viewer.screen.columns - len(label) + 1):
                                if all(viewer.screen.cells.get((x+i, y), ("", None))[0] == character
                                       for i, character in enumerate(label)):
                                    return x, y
                        raise AssertionError("Missing displayed control/text: " + label + "\n" + viewer.screen.text())

                    def copies():
                        return [base64.b64decode(value[5:], validate=True)
                                for value in viewer.screen.other_osc if value.startswith("52;c;")]

                    left, row = point("LEFT")
                    right, last_row = point("RIGHT")
                    viewer.keys(f"\x1b[<0;{left+1};{row+1}M\x1b[<32;{right+5};{last_row+1}M\x1b[<0;{right+5};{last_row+1}m".encode())
                    viewer.wait(lambda: bool(copies()))
                    assert b"OLD_COMMAND" in copies()[-1] and b"Copy" not in copies()[-1]
                    changed = external(name, "# Combined\nLEFT `NEW_COMMAND` RIGHT\n", winter)
                    assert changed["revision"] != old["revision"]
                    viewer.pump(1.3)
                    assert "OLD_COMMAND" in viewer.screen.text() and "NEW_COMMAND" not in viewer.screen.text()
                    assert "15 Jan 11:00 AEDT" in viewer.screen.text() and "15 Jul 10:00 AEST" not in viewer.screen.text()
                    count = len(copies())
                    if mode == "mouse":
                        x, y = point("[Copy]")
                        viewer.keys(f"\x1b[<0;{x+1};{y+1}M\x1b[<0;{x+1};{y+1}m".encode())
                    else:
                        viewer.keys(b"c\r")
                    viewer.wait(lambda: len(copies()) > count)
                    assert len(copies()) == count + 1 and copies()[-1] == b"OLD_COMMAND"
                    assert read(name) == changed, "copy modified the newly updated board"
                    if mode == "keyboard":
                        # First Escape leaves code focus; second clears the
                        # completed selection, resuming the matching live view.
                        viewer.keys(b"\x1b")
                        viewer.keys(b"\x1b")
                    viewer.wait(lambda: "NEW_COMMAND" in viewer.screen.text() and "15 Jul 10:00 AEST" in viewer.screen.text())
                    x, y = point("[Copy]")
                    count = len(copies())
                    viewer.keys(f"\x1b[<0;{x+1};{y+1}M\x1b[<0;{x+1};{y+1}m".encode())
                    viewer.wait(lambda: len(copies()) > count)
                    assert len(copies()) == count + 1 and copies()[-1] == b"NEW_COMMAND"
                    assert read(name) == changed
                    item["passed"] = True
                    record("combined " + mode + " frozen selection keeps old code/time/payload together, then resumes new code/time without writes")
                except Exception:
                    item["error"] = traceback.format_exc()
                finally:
                    if viewer is not None:
                        (output / ("combined-" + mode + "-screen.txt")).write_text(viewer.screen.text())
                        (output / ("combined-" + mode + "-terminal.bin")).write_bytes(viewer.output)
                        viewer.close()
                        viewer = None
                    combined.append(item)
                    result["combined_checks"] = combined
            assert all(item["passed"] for item in combined), json.dumps(combined)
            result["complete"] = True
    finally:
        try:
            if viewer is not None:
                viewer.close()
        finally:
            try:
                if broker is not None:
                    stop(broker)
            finally:
                result["cleanup"] = {"viewer_exited": viewer is None or viewer.process.poll() is not None,
                                     "broker_exited": broker is None or broker.poll() is not None}
                (output / "results.json").write_text(json.dumps(result, indent=2) + "\n")
    print("Evidence: " + str(output), flush=True)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()
    folder = args.output or Path(tempfile.mkdtemp(prefix="abtime-")) / "proof"
    run(args.binary.resolve(strict=True), folder.resolve())
