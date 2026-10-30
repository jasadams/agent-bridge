#!/usr/bin/env python3
"""Private built-binary PTY checks; validates OSC52 output, never reads a clipboard."""
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
    env = {"PATH": os.environ.get("PATH", "/usr/bin:/bin"), "LANG": "C.UTF-8"}
    for key, name in [("HOME", "home"), ("XDG_CONFIG_HOME", "config"),
                      ("XDG_DATA_HOME", "data"), ("XDG_STATE_HOME", "state-home"),
                      ("XDG_CACHE_HOME", "cache"), ("CODEX_HOME", "codex"),
                      ("CLAUDE_CONFIG_DIR", "claude"), ("TMPDIR", "tmp")]:
        directory = output / name
        directory.mkdir(mode=0o700)
        env[key] = str(directory)
    state = output / "state"
    result = {"binary_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
              "scope": "actual private broker and PTY; decoded write-only OSC52 requests, not OS clipboard acceptance",
              "checks": [], "complete": False, "cleanup_errors": []}
    broker = None
    with (output / "broker.log").open("wb") as log:
        try:
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
                    data = b""
                    while not data.endswith(b"\n"):
                        chunk = connection.recv(262144)
                        assert chunk, "broker closed response"
                        data += chunk
                    value = json.loads(data)
                    assert value["ok"], value
                    return value["result"]

            def read(name):
                return rpc({"op": "named_board_read", "board": name})

            def write(name, markdown):
                before = read(name)
                rpc({"op": "named_board_update", "board": name,
                     "expected_revision": before["revision"],
                     "edits": [{"op": "set", "markdown": markdown}]})

            def positions(viewer, label):
                return [(x, y) for y in range(viewer.screen.rows)
                        for x in range(viewer.screen.columns - len(label) + 1)
                        if all(viewer.screen.cells.get((x+i, y), ("", None))[0] == character
                               for i, character in enumerate(label))]

            def payloads(viewer):
                commands = [value for value in viewer.screen.other_osc if value.startswith("52;")]
                assert all(value.startswith("52;c;") for value in commands), commands
                return [base64.b64decode(value[5:], validate=True) for value in commands]

            def click(viewer, position, expected):
                before = len(payloads(viewer))
                x, y = position
                assert viewer.screen.cells[(x, y)][1] is None, "copy button inherited an OSC8 link"
                viewer.keys(f"\x1b[<0;{x+1};{y+1}M\x1b[<0;{x+1};{y+1}m".encode())
                viewer.wait(lambda: len(payloads(viewer)) > before)
                assert len(payloads(viewer)) == before + 1, payloads(viewer)
                assert payloads(viewer)[-1] == expected.encode(), repr(payloads(viewer)[-1])
                viewer.wait(lambda: "Copy sent to terminal" in viewer.screen.text())

            def case(name, markdown, check, mutates=False):
                viewer = None
                item = {"check": name, "passed": False}
                try:
                    rpc({"op": "board_create", "board": name})
                    write(name, markdown)
                    before = read(name)
                    viewer = Viewer(binary, state, name, env=env)
                    viewer.wait(lambda: bool(positions(viewer, "[⧉")))
                    assert not payloads(viewer), "opening board wrote clipboard"
                    check(viewer, name)
                    if not mutates:
                        assert read(name) == before, "copy changed persisted board snapshot"
                    item["passed"] = True
                except Exception:
                    item["error"] = traceback.format_exc()
                finally:
                    if viewer is not None:
                        (output / (name + "-screen.txt")).write_text(viewer.screen.text())
                        (output / (name + "-terminal.bin")).write_bytes(viewer.output)
                        try:
                            viewer.close()
                            assert viewer.process.poll() is not None
                        except Exception:
                            item["passed"] = False
                            item["cleanup_error"] = traceback.format_exc()
                result["checks"].append(item)
                print(("PASS: " if item["passed"] else "FAIL: ") + name, flush=True)

            inline = "# Inline\nLEFT `printf 'λ'` RIGHT and `same` / `same`\n\n- list ``echo `literal` \\path``\n\n| Command | Note |\n|---|---|\n| `echo 地球` | cell |\n"
            def inline_check(viewer, _name):
                expected = ["printf 'λ'", "same", "same", "echo `literal` \\path", "echo 地球"]
                viewer.wait(lambda: len(positions(viewer, "[⧉]")) == len(expected))
                for index, value in enumerate(expected):
                    click(viewer, positions(viewer, "[⧉]")[index], value)
            case("inline", inline, inline_check)

            block = "  printf '%s\\n' \"first\"\n\techo '地球'\n" + "".join(f"echo row-{n}\n" for n in range(65))
            case("block", "# Block\n\n```sh\n" + block + "```\n",
                 lambda viewer, _name: click(viewer, positions(viewer, "[⧉]")[0], block))
            case("indented", "# Indented\n\n    first\n      second\n    third\n",
                 lambda viewer, _name: click(viewer, positions(viewer, "[⧉]")[0], "first\n  second\nthird\n"))

            case("quoted", "# Quoted\n\n> ```sh\n> printf 'quote'\n>   echo second\n> ```\n\noutside tail\n",
                 lambda viewer, _name: click(viewer, positions(viewer, "[⧉]")[0], "printf 'quote'\n  echo second\n"))
            case("listed", "# Listed\n\n- before\n\n  ```sh\n  echo 'list'\n    echo nested\n  ```\n\noutside tail\n",
                 lambda viewer, _name: click(viewer, positions(viewer, "[⧉]")[0], "echo 'list'\n  echo nested\n"))

            case("quoted-indented", "# Quoted indented\n\n>     printf 'quote-indent'\n>       echo second\n",
                 lambda viewer, _name: click(viewer, positions(viewer, "[⧉]")[0], "printf 'quote-indent'\n  echo second\n"))
            case("listed-indented", "# Listed indented\n\n- item\n\n      printf 'list-indent'\n        echo second\n",
                 lambda viewer, _name: click(viewer, positions(viewer, "[⧉]")[0], "printf 'list-indent'\n  echo second\n"))
            case("tab-indented", "# Tab indented\n\n\tprintf 'tab'\n\t  echo second\n",
                 lambda viewer, _name: click(viewer, positions(viewer, "[⧉]")[0], "printf 'tab'\n  echo second\n"))

            def linked_check(viewer, _name):
                target = "https://example.test/code"
                links = viewer.screen.links()
                assert links and all(value[1] == target for value in links.values())
                label = "".join(links[position][0] for position in sorted(links, key=lambda point: (point[1], point[0])))
                assert label == "a|b", repr(label)
                x, y = positions(viewer, "SENTINEL")[0]
                assert all(viewer.screen.cells[(x+i, y)][1] is None for i in range(len("SENTINEL")))
                click(viewer, positions(viewer, "[⧉]")[0], "a|b")
            case("linked", "# Linked\n\n| Command | Neighbor |\n|---|---|\n| [`a\\|b`](https://example.test/code) | SENTINEL |\n", linked_check)

            def keyboard_check(viewer, _name):
                viewer.keys(b"mc\r")
                viewer.wait(lambda: len(payloads(viewer)) == 1)
                assert payloads(viewer)[-1] == b"keyboard-command"
                viewer.wait(lambda: "Copy sent to terminal" in viewer.screen.text())
                viewer.keys(b"\x1b")
                viewer.keys(b"\t\rc")
                viewer.keys(b"\x1b")
                assert len(payloads(viewer)) == 1, "typing c in a response editor copied code"
            case("keyboard", "# Keyboard\n`keyboard-command`\n\n<!-- board-input: answer -->\n> original\n<!-- /board-input -->\n", keyboard_check)

            long_command = 'echo "' + "地球λ" * 22 + '"'
            def layout_check(viewer, _name):
                viewer.screen.columns, viewer.screen.rows = 32, 20
                fcntl.ioctl(viewer.master, termios.TIOCSWINSZ, struct.pack("HHHH", 20, 32, 0, 0))
                os.kill(viewer.process.pid, signal.SIGWINCH)
                viewer.pump(.3)
                viewer.wait(lambda: bool(positions(viewer, "[⧉]")))
                click(viewer, positions(viewer, "[⧉]")[0], long_command)
                viewer.keys(b"w")
                for _ in range(90):
                    if positions(viewer, "[⧉]"):
                        break
                    viewer.keys(b"l")
                assert positions(viewer, "[⧉]"), viewer.screen.text()
                click(viewer, positions(viewer, "[⧉]")[0], long_command)
            case("layout", "# Layout\n`" + long_command + "`\n", layout_check)

            def live_check(viewer, name):
                write(name, "# Live\n`updated-payload`\n")
                viewer.wait(lambda: "updated-payload" in viewer.screen.text())
                observed = read(name)
                click(viewer, positions(viewer, "[⧉]")[0], "updated-payload")
                assert read(name) == observed, "copy rewrote live-updated board"
                rpc({"op": "board_create", "board": "switch-beta"})
                write("switch-beta", "# Beta\n`other-board`\n")
                viewer.keys(b"b")
                viewer.wait(lambda: "switch-beta" in viewer.screen.text())
                viewer.keys(b"j\r")
                viewer.wait(lambda: "other-board" in viewer.screen.text())
                observed = read("switch-beta")
                click(viewer, positions(viewer, "[⧉]")[0], "other-board")
                assert read("switch-beta") == observed, "copy rewrote switched board"
            case("switch-alpha", "# Live\n`original-payload`\n", live_check, mutates=True)

            def selection_check(viewer, name, keyboard=False):
                start = positions(viewer, "LEFT")[0]
                end = positions(viewer, "RIGHT")[0]
                x, y = start; right, bottom = end
                viewer.keys(f"\x1b[<0;{x+1};{y+1}M\x1b[<32;{right+5};{bottom+1}M\x1b[<0;{right+5};{bottom+1}m".encode())
                viewer.wait(lambda: bool(payloads(viewer)))
                selected = payloads(viewer)[-1].decode()
                assert "LEFT" in selected and "cmd" in selected and "RIGHT" in selected, repr(selected)
                assert "⧉" not in selected and "Copy" not in selected and "[" not in selected and "]" not in selected, repr(selected)
                write(name, "# Selection\nLEFT `new-payload` RIGHT\n")
                changed = read(name)
                viewer.pump(1.3)
                assert "new-payload" not in viewer.screen.text(), "selection did not freeze displayed content"
                if keyboard:
                    count = len(payloads(viewer))
                    viewer.keys(b"c\r")
                    viewer.wait(lambda: len(payloads(viewer)) > count)
                    assert len(payloads(viewer)) == count + 1
                    assert payloads(viewer)[-1] == b"cmd", "keyboard used live payload for frozen old control"
                else:
                    click(viewer, positions(viewer, "[⧉]")[0], "cmd")
                assert read(name) == changed, "frozen copy changed updated board"
            case("selection", "# Selection\nLEFT `cmd` RIGHT\n", selection_check, mutates=True)
            case("selection-keyboard", "# Selection\nLEFT `cmd` RIGHT\n",
                 lambda viewer, name: selection_check(viewer, name, keyboard=True), mutates=True)

            def empty_check(viewer, _name):
                x, y = positions(viewer, "[⧉]")[0]
                viewer.keys(f"\x1b[<0;{x+1};{y+1}M\x1b[<0;{x+1};{y+1}m".encode())
                viewer.wait(lambda: "clipboard unchanged" in viewer.screen.text().lower())
                assert not payloads(viewer), "empty copy cleared clipboard"
                assert "Copy sent to terminal" not in viewer.screen.text(), "empty copy claimed a request"
            case("empty", "# Empty\n\n```\n```\n", empty_check)

            hostile = "echo '\x1b]52;c;INJECTED\x07' 地球"
            case("hostile", "# Hostile\n`" + hostile + "`\n",
                 lambda viewer, _name: click(viewer, positions(viewer, "[⧉]")[0], hostile))
            result["complete"] = all(item["passed"] for item in result["checks"])
        finally:
            if broker is not None:
                try:
                    stop(broker)
                    assert broker.poll() is not None
                except Exception:
                    result["cleanup_errors"].append(traceback.format_exc())
            if result["cleanup_errors"]:
                result["complete"] = False
            (output / "results.json").write_text(json.dumps(result, indent=2) + "\n")
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    result = run(args.binary.resolve(), args.output.resolve())
    return 0 if result["complete"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
