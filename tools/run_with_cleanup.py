#!/usr/bin/env python3
"""Run a command in a process group of its own, and stop that whole group when the command ends or when
the process that started this helper is gone, however it went.

tools/test_web_browser.py runs itself this way. A test that is killed with SIGKILL cannot run its own
clean-up, and its broker and Playwright driver would keep running. The command is the leader of a new
group from its first instant, so everything it starts is in the group and nothing has to be found by
watching. When the command ends, or the process that started this helper is gone, the helper sends the
group SIGTERM, then SIGKILL after a grace period. (Chromium puts itself in a group of its own, but it
ends when the Playwright driver that holds its pipe is gone.)

Usage: run_with_cleanup.py PARENT_PID, with the command as a JSON list in the environment variable
RUN_WITH_CLEANUP_COMMAND (so that a `pkill -f` naming the script does not also match this helper).
SIGINT, SIGTERM and SIGHUP are passed on to the command. The exit status is the command's, with a death
by signal N reported as 128+N.
"""
import json
import os
import signal
import subprocess
import sys
import time

COMMAND = "RUN_WITH_CLEANUP_COMMAND"
POLL_SECONDS = 0.1  # the starting process cannot signal its own death, so it is polled
GRACE_SECONDS = 3
PASSED_ON = (signal.SIGINT, signal.SIGTERM, signal.SIGHUP)


def main(parent):
    command = json.loads(os.environ.pop(COMMAND))
    child = None
    early = []

    def pass_on(number, frame):
        if child:
            child.send_signal(number)
        else:
            early.append(number)  # arrived before the command existed

    for number in PASSED_ON:
        signal.signal(number, pass_on)
    child = subprocess.Popen(command, start_new_session=True)
    for number in early:
        child.send_signal(number)
    status = None
    while status is None and os.getppid() == parent:
        try:
            status = child.wait(POLL_SECONDS)
        except subprocess.TimeoutExpired:
            pass
    stop_group(child.pid, child)
    if status is None:
        status = child.wait()
    return 128 - status if status < 0 else status


def stop_group(group, child):
    """SIGTERM, then SIGKILL after a grace period, to every process left in the group. The group exists
    for as long as one process is in it, so its id cannot belong to anything else."""
    deadline = time.monotonic() + GRACE_SECONDS
    child.poll()  # a command that has ended stays in the group, and on macOS makes it unsignalable, until waited for
    try:
        os.killpg(group, signal.SIGTERM)
        while time.monotonic() < deadline:
            child.poll()  # a command that has ended stays in the group until it is waited for
            os.killpg(group, 0)  # raises once the group is empty
            time.sleep(0.1)
        os.killpg(group, signal.SIGKILL)
    except ProcessLookupError:
        pass


if __name__ == "__main__":
    sys.exit(main(int(sys.argv[1])))
