#!/usr/bin/env python3
"""Private real-broker/PTY archive qualification; no model or owner state."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import socket
import subprocess
import tempfile
import time
import traceback

from test_board_links import Viewer, stop


def run(binary, output):
    output.mkdir(mode=0o700, parents=True, exist_ok=False)
    state = output / "state"
    environment = {"PATH": os.environ.get("PATH", "/usr/bin:/bin"), "LANG": "C.UTF-8"}
    for key, name in [("HOME", "home"), ("XDG_CONFIG_HOME", "config"),
                      ("XDG_DATA_HOME", "data"), ("XDG_CACHE_HOME", "cache"),
                      ("XDG_STATE_HOME", "state-home"), ("TMPDIR", "tmp"),
                      ("CODEX_HOME", "codex"), ("CLAUDE_CONFIG_DIR", "claude")]:
        directory = output / name
        directory.mkdir(mode=0o700)
        environment[key] = str(directory)
    evidence = {"binary_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
                "scope": "actual private broker and PTY; no model or owner state",
                "cases": [], "complete": False}
    broker = viewer = None

    def persist():
        (output / "results.json").write_text(json.dumps(evidence, indent=2) + "\n")

    def rpc(operation, *, success=True, **fields):
        request = {"op": operation, **fields}
        with socket.socket(socket.AF_UNIX) as connection:
            connection.settimeout(5)
            connection.connect(str(state / "bridge.sock"))
            connection.sendall(json.dumps(request).encode() + b"\n")
            response = b""
            while not response.endswith(b"\n"):
                chunk = connection.recv(262144)
                assert chunk, "Broker closed response"
                response += chunk
                assert len(response) <= 2 * 1024 * 1024, "Unbounded broker response"
        envelope = json.loads(response)
        if success:
            assert envelope["ok"], envelope
            return envelope["result"]
        assert not envelope["ok"], envelope
        return envelope

    def inspect(name):
        return rpc("board_inspect", board=name)

    def create(name, markdown):
        rpc("board_create", board=name)
        current = inspect(name)
        rpc("named_board_update", board=name, expected_revision=current["revision"],
            edits=[{"op": "set", "markdown": markdown}])
        return inspect(name)

    def mutation(operation, name):
        return rpc(operation, board=name, expected_revision=inspect(name)["revision"])

    def open_view(name):
        nonlocal viewer
        viewer = Viewer(binary, state, name, env=environment, timezone="Australia/Sydney")
        viewer.wait(lambda: name.lower() in viewer.screen.text().lower())
        return viewer

    def screen():
        return viewer.screen.text().lower()

    def read_only():
        return "archiv" in screen() and ("read-only" in screen() or "read only" in screen())

    def unchanged_content(before, after):
        for key in ("markdown", "last_updated", "last_updated_source"):
            assert after[key] == before[key], (key, before[key], after[key])

    def main_protection():
        before = inspect("main")
        rpc("board_archive", board="main", expected_revision=before["revision"], success=False)
        assert inspect("main") == before
        open_view("main")
        viewer.keys(b"a")
        viewer.wait(lambda: "main" in screen() and any(word in screen() for word in ("cannot", "protected", "not archive")))
        assert inspect("main") == before

    def archive_restore_confirmation():
        name = "confirmation"
        before = create(name, "# Archive exact 世界\n\n`echo preserved`\n\nTrailing  \n")
        open_view(name)
        viewer.wait(lambda: "Archive exact" in viewer.screen.text())
        viewer.keys(b"a")
        viewer.wait(lambda: "30" in screen() and "archiv" in screen())
        viewer.keys(b"\x1b")
        assert inspect(name) == before, "Cancelling archive changed board"
        viewer.keys(b"a\r")
        viewer.wait(read_only)
        archived = inspect(name)
        assert archived["state"] == "archived"
        unchanged_content(before, archived)
        assert archived["archived_at"] is not None
        assert archived["expires_at"] - archived["archived_at"] == 30 * 24 * 60 * 60 * 1000
        assert archived["revision"] != before["revision"]
        assert archived["lifecycle_generation"] != before["lifecycle_generation"]
        assert any(word in screen() for word in ("delete", "expires", "expiry")), viewer.screen.text()
        viewer.keys(b"r")
        viewer.wait(lambda: "restore" in screen() and "enter" in screen())
        viewer.keys(b"\x1b")
        assert inspect(name) == archived, "Cancelling restore changed archive"
        viewer.keys(b"r\r")
        viewer.wait(lambda: "restored" in screen() or not read_only())
        restored = inspect(name)
        assert restored["state"] == "active"
        unchanged_content(before, restored)
        assert restored["incarnation"] == before["incarnation"]
        assert restored["lifecycle_generation"] != archived["lifecycle_generation"]
        assert restored["revision"] != before["revision"]

    def archived_picker():
        name = "picker-archive"
        before = create(name, "# Picker immutable content\n")
        mutation("board_archive", name)
        archived = inspect(name)
        assert name in [row["name"] for row in rpc("board_archived_list")]
        assert name not in [row["name"] for row in rpc("board_list")]
        open_view("main")
        viewer.keys(b"b")
        viewer.wait(lambda: "boards" in screen())
        viewer.keys(b"\t")
        viewer.wait(lambda: "archived" in screen() and name in screen())
        archived_names = sorted(row["name"] for row in rpc("board_archived_list"))
        viewer.keys(b"j" * archived_names.index(name) + b"\r")
        viewer.wait(lambda: "Picker immutable content" in viewer.screen.text() and read_only())
        assert inspect(name) == archived
        unchanged_content(before, archived)
        mutation("board_restore", name)

    def external_archive_draft():
        name = "draft-archive"
        before = create(name, "# Draft lifecycle\n\n<!-- board-input: answer -->\n> original\n<!-- /board-input -->\n")
        open_view(name)
        viewer.wait(lambda: "Draft lifecycle" in viewer.screen.text())
        viewer.keys(b"\t\r")
        viewer.keys(b" retained-private-draft")
        viewer.wait(lambda: "retained-private-draft" in viewer.screen.text())
        mutation("board_archive", name)
        viewer.wait(lambda: read_only() and "retained-private-draft" in viewer.screen.text())
        archived = inspect(name)
        unchanged_content(before, archived)
        viewer.keys(b"\x13")  # Ctrl-S cannot save an externally archived draft.
        viewer.pump(.8)
        assert inspect(name) == archived
        assert "retained-private-draft" in viewer.screen.text()
        mutation("board_restore", name)
        restored = inspect(name)
        unchanged_content(before, restored)
        assert restored["lifecycle_generation"] != before["lifecycle_generation"]
        viewer.pump(1.3)
        viewer.wait(lambda: not read_only() and restored["revision"][:12] in viewer.screen.text() and "retained-private-draft" in viewer.screen.text())
        viewer.keys(b"\x12\x13")  # Ctrl-R must not rebase across lifecycle scope.
        viewer.pump(1.2)
        assert inspect(name) == restored, "Old lifecycle draft rebased/saved over restored identical bytes"
        assert "retained-private-draft" in viewer.screen.text()

    cases = [("main-protected", main_protection),
             ("archive-restore-confirmation-exact-content-time", archive_restore_confirmation),
             ("archived-picker-read-only", archived_picker),
             ("external-archive-draft-lifecycle-rebase-blocked", external_archive_draft)]
    try:
        with (output / "broker.log").open("wb") as log:
            broker = subprocess.Popen([str(binary), "--state-dir", str(state), "serve",
                                       "--no-discovery", "--codex-bin", "/missing/codex"],
                                      env=environment, stdout=log, stderr=log)
            deadline = time.monotonic() + 10
            while not (state / "bridge.sock").exists():
                assert broker.poll() is None and time.monotonic() < deadline, "Private broker did not start"
                time.sleep(.03)
            for label, scenario in cases:
                item = {"case": label, "status": "failed"}
                try:
                    scenario()
                    item["status"] = "passed"
                    print("PASS: " + label, flush=True)
                except Exception:
                    item["error"] = traceback.format_exc()
                    print("FAIL: " + label + "\n" + item["error"], flush=True)
                finally:
                    if viewer is not None:
                        (output / (label + "-screen.txt")).write_text(viewer.screen.text())
                        (output / (label + "-terminal.bin")).write_bytes(viewer.output)
                        try:
                            viewer.close()
                        except Exception:
                            item["status"] = "failed"
                            item["cleanup_error"] = traceback.format_exc()
                        item["viewer_exit"] = viewer.process.poll()
                        viewer = None
                    evidence["cases"].append(item)
                    persist()
            evidence["complete"] = all(item["status"] == "passed" for item in evidence["cases"])
    finally:
        try:
            if viewer is not None:
                viewer.close()
        finally:
            if broker is not None:
                stop(broker)
                evidence["broker_exit"] = broker.poll()
            persist()
    assert evidence["complete"], json.dumps(evidence["cases"], indent=2)
    print("Evidence: " + str(output), flush=True)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--output", type=Path)
    arguments = parser.parse_args()
    destination = arguments.output or Path(tempfile.mkdtemp(prefix="abarchive-")) / "proof"
    run(arguments.binary.resolve(strict=True), destination.resolve())
