#!/usr/bin/env python3
"""Tests for how tools/test_web_browser.py treats its output directory and a broker that fails to start.

Needs no browser and no Agent Bridge binary. The failed-start test waits out the broker's real startup
timeout (about 30 seconds).
"""
import contextlib
import io
import os
from pathlib import Path
import sys
import tempfile
import types
import unittest
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent))
import test_web_browser as web  # noqa: E402


def snapshot(directory):
    return {str(p.relative_to(directory)): (p.read_bytes() if p.is_file() else None) for p in sorted(directory.rglob("*"))}


def process_exists(pid):
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    return True


class OutputDirectory(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="ab-out-test-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name).resolve()

    def test_a_new_directory_is_created_with_the_marker(self):
        out = web.prepare_output(self.root / "new" / "run")
        self.assertTrue((out / web.MARKER).is_file())

    def test_an_empty_directory_is_used(self):
        out = web.prepare_output(self.root)
        self.assertEqual(out, self.root)
        self.assertTrue((out / web.MARKER).is_file())

    def test_a_directory_with_files_and_no_marker_is_refused_and_untouched(self):
        (self.root / "bin").mkdir()
        (self.root / "bin" / "mytool").write_text("mine")
        (self.root / "home" / "jason" / "docs").mkdir(parents=True)
        (self.root / "home" / "jason" / "docs" / "a.txt").write_text("keep")
        (self.root / "s").write_text("data")
        before = snapshot(self.root)
        with self.assertRaises(web.OutputDirectoryError):
            web.prepare_output(self.root)
        self.assertEqual(snapshot(self.root), before)

    def test_a_symbolic_link_is_refused(self):
        target = self.root / "target"
        target.mkdir()
        link = self.root / "link"
        link.symlink_to(target)
        with self.assertRaises(web.OutputDirectoryError):
            web.prepare_output(link)
        self.assertEqual(list(target.iterdir()), [])

    def test_a_file_is_refused(self):
        file = self.root / "file"
        file.write_text("x")
        with self.assertRaises(web.OutputDirectoryError):
            web.prepare_output(file)
        self.assertEqual(file.read_text(), "x")

    def test_a_crashed_runs_directory_is_cleared_and_what_is_not_the_runs_stays(self):
        out = web.prepare_output(self.root / "run")
        for name in ("state", "home", "bin", "s"):
            (out / name / "inner").mkdir(parents=True)
            (out / name / "inner" / "file").write_text("x")
        (out / "broker.log").write_text("log")
        (out / "classic-board.png").write_bytes(b"png")
        (out / "notes.txt").write_text("not the run's")
        again = web.prepare_output(self.root / "run")
        self.assertEqual(again, out)
        self.assertEqual(sorted(p.name for p in out.iterdir()), sorted([web.MARKER, "classic-board.png", "notes.txt"]))
        self.assertEqual((out / "notes.txt").read_text(), "not the run's")
        # and the directories the run creates can be made again
        web.stand_in_claude(out)
        (out / "state").mkdir(mode=0o700)


class OutputSymlinks(unittest.TestCase):
    def test_a_symbolic_link_at_a_written_name_is_removed_and_its_target_left_alone(self):
        with tempfile.TemporaryDirectory(prefix="ab-link-test-") as name:
            root = Path(name).resolve()
            victim = root / "victim"
            victim.write_text("keep")
            out = web.prepare_output(root / "run")
            for link in (web.COPIED_BINARY, "classic-board.png", "state"):
                (out / link).symlink_to(victim)
            web.prepare_output(out)
            self.assertEqual([p.name for p in out.iterdir()], [web.MARKER])
            self.assertEqual(victim.read_text(), "keep")


class Main(unittest.TestCase):
    """main() up to the point where the browser would start. Playwright itself is not needed here, so its
    import is satisfied by an empty module; none of these runs reaches it."""

    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="ab-main-test-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name).resolve()
        self.binary = self.root / "agent-bridge"
        self.binary.write_text("#!/bin/sh\n")
        stub = types.ModuleType("playwright.sync_api")
        stub.sync_playwright = None
        package = types.ModuleType("playwright")
        package.sync_api = stub
        patcher = mock.patch.dict(sys.modules, {"playwright": package, "playwright.sync_api": stub})
        patcher.start()
        self.addCleanup(patcher.stop)

    def run_main(self, *arguments):
        error = io.StringIO()
        with mock.patch.object(sys, "argv", ["test_web_browser.py", *arguments]), contextlib.redirect_stderr(error):
            return web.main(), error.getvalue()

    def refused(self, directory, *arguments):
        before = snapshot(directory)
        status, message = self.run_main("--output", str(directory), *arguments)
        self.assertEqual(status, 2)
        self.assertEqual(snapshot(directory), before)
        return message

    def test_a_directory_that_is_not_the_runs_is_refused_with_status_2_and_a_message(self):
        out = self.root / "mine"
        (out / "bin").mkdir(parents=True)
        (out / "bin" / "mytool").write_text("mine")
        message = self.refused(out, "--binary", str(self.binary))
        self.assertIn(str(out), message)

    def test_a_symlinked_marker_does_not_count(self):
        out = self.root / "mine"
        out.mkdir()
        outside = self.root / "outside.txt"
        outside.write_text("x")
        (out / web.MARKER).symlink_to(outside)
        (out / "bin").mkdir()
        self.refused(out, "--binary", str(self.binary))

    def test_a_missing_binary_clears_nothing(self):
        out = web.prepare_output(self.root / "run")
        (out / "state").mkdir()
        with self.assertRaises(FileNotFoundError), mock.patch.object(
                sys, "argv", ["test_web_browser.py", "--binary", str(self.root / "missing"), "--output", str(out)]):
            web.main()
        self.assertTrue((out / "state").is_dir())

    def test_a_binary_inside_a_cleared_name_is_refused_before_anything_is_cleared(self):
        out = web.prepare_output(self.root / "run")
        (out / "bin").mkdir()
        inside = out / "bin" / "agent-bridge"
        inside.write_text("#!/bin/sh\n")
        self.refused(out, "--binary", str(inside))
        self.assertTrue(inside.is_file())

    def test_a_relative_output_path_is_resolved(self):
        out = web.prepare_output(self.root / "run")
        (out / "state").mkdir()
        previous = Path.cwd()
        os.chdir(self.root)
        self.addCleanup(os.chdir, previous)
        self.assertEqual(web.prepare_output(Path("run")), out)
        self.assertFalse((out / "state").exists())

    def test_the_copy_an_earlier_run_made_is_accepted_as_the_binary(self):
        out = web.prepare_output(self.root / "run")
        copy = out / web.COPIED_BINARY
        copy.write_text("#!/bin/sh\n")
        self.assertEqual(web.prepare_output(out, copy), out)
        self.assertTrue(copy.is_file())


class BrokerStartFailure(unittest.TestCase):
    def test_a_broker_that_never_starts_is_stopped(self):
        with tempfile.TemporaryDirectory(prefix="ab-fail-") as name:
            root = Path(name)
            pid_file = root / "pid"
            binary = root / "never-listens"
            binary.write_text(f"#!/bin/sh\necho $$ > '{pid_file}'\nexec sleep 300\n")
            binary.chmod(0o700)
            with self.assertRaises(AssertionError):
                web.Broker(binary, root)
            pid = int(pid_file.read_text())
            self.assertFalse(process_exists(pid), "the broker process was left running")


if __name__ == "__main__":
    unittest.main()
