#!/usr/bin/env python3
"""Disposable topic monitor browser checks; never uses the owner's broker."""
import json
import os
from pathlib import Path
import socket
import sqlite3
import subprocess
import sys
import tempfile
import time
import uuid

from playwright.sync_api import sync_playwright, expect


def main():
    binary = Path(sys.argv[1]).resolve(strict=True)
    output = Path(sys.argv[2]).resolve()
    output.mkdir(parents=True, exist_ok=True, mode=0o700)
    with tempfile.TemporaryDirectory(prefix="agb-topic-", dir=output) as directory:
        root = Path(directory)
        state, home = root / "state", root / "home"
        state.mkdir(mode=0o700)
        home.mkdir(mode=0o700)
        env = {"PATH": os.environ.get("PATH", "/usr/bin:/bin"), "HOME": str(home), "LANG": "C.UTF-8", "XDG_CONFIG_HOME": str(home / "config"), "XDG_DATA_HOME": str(home / "data"), "XDG_STATE_HOME": str(home / "state"), "XDG_CACHE_HOME": str(home / "cache"), "CODEX_HOME": str(home / "codex"), "CLAUDE_CONFIG_DIR": str(home / "claude"), "OPENCODE_CONFIG_DIR": str(home / "opencode")}
        def rpc(op, **fields):
            with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as conn:
                conn.settimeout(10)
                conn.connect(str(state / "bridge.sock"))
                conn.sendall(json.dumps({"op": op, **fields}).encode() + b"\n")
                reply = json.loads(conn.makefile("rb").readline())
            assert reply["ok"], reply
            return reply.get("result")
        with (output / "broker.log").open("wb") as log:
            process = subprocess.Popen([str(binary), "--state-dir", str(state), "serve", "--no-discovery", "--codex-bin", "/missing/codex"], cwd=root, env=env, stdin=subprocess.DEVNULL, stdout=log, stderr=log)
            try:
                for _ in range(200):
                    if (state / "bridge.sock").exists():
                        break
                    if process.poll() is not None:
                        raise RuntimeError("disposable broker exited")
                    time.sleep(.05)
                access = rpc("monitor_enable", port=0)
                with sync_playwright() as playwright:
                    browser = playwright.chromium.launch(headless=True)
                    try:
                        context = browser.new_context(viewport={"width": 1280, "height": 800})
                        assert context.request.get(access["url"] + "/api/topics").status == 401
                        assert context.request.post(access["url"] + "/api/topic-actions", headers={"Origin": access["url"], "Content-Type": "application/json"}, data='{"action":"create","name":"forbidden"}').status == 401
                        page = context.new_page()
                        page.goto(access["access_url"])
                        expect(page.locator("#boards-view")).to_be_visible()
                        page.locator("#messages-tab").click()
                        expect(page.locator("#messages-view")).to_be_visible()
                        page.locator("#boards-tab").click()
                        expect(page.locator("#boards-view")).to_be_visible()
                        page.locator("#topics-tab").click()
                        expect(page.locator("#topic-list-status")).to_have_text("No topics yet.")
                        page.once("dialog", lambda dialog: dialog.accept("Engineering"))
                        page.locator("#topic-create").click()
                        expect(page.locator("#topic-heading")).to_have_text("Engineering")
                        topic_id = page.locator("#topic-id").text_content()
                        page.locator("#topic-body").fill("Hello <script>window.pwned=1</script>")
                        page.locator("#topic-send").click()
                        expect(page.locator("#topic-posts")).to_contain_text("Hello <script>")
                        assert page.evaluate("window.pwned") is None
                        assert page.locator("#topic-posts script").count() == 0
                        page.screenshot(path=str(output / "topics-desktop.png"))
                        second = browser.new_context(viewport={"width": 390, "height": 844})
                        other = second.new_page()
                        other.goto(access["access_url"])
                        other.locator("#topics-tab").click()
                        other.get_by_role("button", name="Engineering", exact=False).click()
                        expect(other.locator("#topic-posts")).to_contain_text("Hello <script>")
                        page.locator("#topic-posts button").first.click()
                        page.locator("#topic-body").fill("A reply")
                        expect(page.locator('#topic-mentions option[value="inbox-user"]')).to_have_count(1)
                        page.locator("#topic-mentions").select_option("inbox-user")
                        page.locator("#topic-send").click()
                        expect(page.locator("#topic-posts")).to_contain_text("A reply")
                        expect(page.locator("#topic-posts")).to_contain_text("Mentions: inbox-user")
                        expect(other.locator("#topic-posts")).to_contain_text("A reply", timeout=15000)
                        page.evaluate("""() => {
                          const original = window.fetch;
                          let armed = true;
                          window.fetch = async (input, options = {}) => {
                            const result = await original(input, options);
                            if (armed && options.method === 'POST' && JSON.parse(options.body || '{}').action === 'publish') {
                              armed = false;
                              throw new TypeError('simulated lost response after commit');
                            }
                            return result;
                          };
                        }""")
                        page.locator("#topic-body").fill("uncertain once")
                        page.locator("#topic-send").click()
                        expect(page.locator("#topic-send")).to_have_text("Retry same message")
                        expect(page.locator("#topic-body")).to_be_disabled()
                        page.locator("#topic-send").click()
                        expect(page.locator("#topic-feedback")).to_have_text("Published.")
                        view = rpc("topic_read", topic={"topic_id": topic_id}, cursor="", limit=100)
                        assert len([post for post in view["posts"] if post["body"] == "uncertain once"]) == 1
                        reply = next(post for post in view["posts"] if post["body"] == "A reply")
                        assert reply["parent"] and reply["mentions"][0]["agent_id"] == "inbox-user"
                        page.evaluate("""() => {
                          const original = window.fetch;
                          let armed = true;
                          window.__topicGate = { waiting: false };
                          window.fetch = async (input, options = {}) => {
                            const result = await original(input, options);
                            if (armed && options.method === 'POST' && JSON.parse(options.body || '{}').action === 'publish') {
                              armed = false;
                              window.__topicGate.waiting = true;
                              await new Promise(resolve => window.__topicGate.release = resolve);
                            }
                            return result;
                          };
                        }""")
                        page.locator("#topic-body").fill("in flight")
                        page.locator("#topic-send").click()
                        page.wait_for_function("() => window.__topicGate.waiting")
                        expect(page.locator("#topic-mentions")).to_be_disabled()
                        expect(page.locator("#topic-posts button").first).to_be_disabled()
                        page.evaluate("window.__topicGate.release()")
                        expect(page.locator("#topic-feedback")).to_have_text("Published.")
                        page.once("dialog", lambda dialog: dialog.accept("Delivery"))
                        page.locator("#topic-rename").click()
                        expect(page.locator("#topic-heading")).to_have_text("Delivery")
                        assert page.locator("#topic-id").text_content() == topic_id
                        page.locator("#topic-archive").click()
                        expect(page.locator("#topic-archive")).to_have_text("Restore")
                        expect(page.locator("#topic-send")).to_be_disabled()
                        page.locator("#topic-archive").click()
                        expect(page.locator("#topic-archive")).to_have_text("Archive")
                        expect(page.locator("#topic-send")).to_be_enabled()
                        page.locator("#topic-body").fill("draft survives updates")
                        focused_topic = page.get_by_role("button", name="Delivery", exact=False)
                        focused_topic.focus()
                        rpc("topic_publish", topic={"topic_id": topic_id}, id=str(uuid.uuid4()), author={"agent_id":"inbox-user"}, body="external", mentions=[])
                        expect(page.locator("#topic-posts")).to_contain_text("external", timeout=15000)
                        assert page.locator("#topic-body").input_value() == "draft survives updates"
                        expect(focused_topic).to_be_focused()
                        for number in range(55):
                            rpc("topic_publish", topic={"topic_id": topic_id}, id=str(uuid.uuid4()), author={"agent_id":"inbox-user"}, body=f"historic {number}", mentions=[])
                        other.goto(access["access_url"])
                        other.locator("#topics-tab").click()
                        other.get_by_role("button", name="Delivery", exact=False).click()
                        expect(other.locator("#topic-older")).to_be_visible()
                        other.evaluate("""() => {
                          const original = window.fetch;
                          let armed = true;
                          window.__olderGate = { waiting: false };
                          window.fetch = async (input, options = {}) => {
                            const result = await original(input, options);
                            if (armed && new URL(input, location.href).searchParams.has('before')) {
                              armed = false; window.__olderGate.waiting = true;
                              await new Promise(resolve => window.__olderGate.release = resolve);
                            }
                            return result;
                          };
                        }""")
                        other.locator("#topic-older").click()
                        other.wait_for_function("() => window.__olderGate.waiting")
                        other.locator("#messages-tab").click()
                        other.locator("#topics-tab").click()
                        expect(other.locator("#topic-older")).to_be_enabled()
                        other.evaluate("window.__olderGate.release()")
                        other.locator("#topic-older").scroll_into_view_if_needed()
                        anchor_id = other.locator("#topic-posts li").first.get_attribute("data-id")
                        anchor = other.locator(f'#topic-posts li[data-id="{anchor_id}"]')
                        anchor_top = anchor.evaluate("el => el.getBoundingClientRect().top")
                        other.locator("#topic-older").click()
                        expect(other.locator("#topic-posts")).to_contain_text("Hello <script>")
                        assert abs(anchor.evaluate("el => el.getBoundingClientRect().top") - anchor_top) < 3
                        assert other.locator("#topic-posts li").count() == 60
                        assert other.evaluate("window.pwned") is None
                        other.evaluate("window.scrollTo(0, 0)")
                        other.screenshot(path=str(output / "topics-narrow.png"))
                        page.once("dialog", lambda dialog: dialog.accept("Second"))
                        page.locator("#topic-create").click()
                        expect(page.locator("#topic-heading")).to_have_text("Second")
                        first_second_id = page.locator("#topic-id").text_content()
                        page.once("dialog", lambda dialog: dialog.accept("Second"))
                        page.locator("#topic-create").click()
                        expect(page.locator("#topic-heading")).to_have_text("Second")
                        expect(page.locator("#topic-id")).not_to_have_text(first_second_id)
                        expect(page.get_by_role("button", name="Second", exact=False)).to_have_count(2)
                        page.get_by_role("button", name="Delivery", exact=False).click()
                        expect(page.locator("#topic-body")).to_have_value("draft survives updates")
                        page.evaluate("""() => {
                          const original = window.fetch;
                          window.__sessionGate = { started: 0, returned: 0, releases: [] };
                          window.fetch = async (input, options = {}) => {
                            const result = await original(input, options);
                            if (options.method === 'POST' && JSON.parse(options.body || '{}').action === 'publish') {
                              window.__sessionGate.started++;
                              await new Promise(resolve => window.__sessionGate.releases.push(resolve));
                              window.__sessionGate.returned++;
                            }
                            return result;
                          };
                        }""")
                        page.locator("#topic-send").click()
                        page.wait_for_function("() => window.__sessionGate.started === 1")
                        page.once("dialog", lambda dialog: dialog.accept())
                        page.locator("#lock-button").click()
                        expect(page.locator("#topic-id")).to_have_text("")
                        expect(page.locator("#topic-reply")).to_have_text("")
                        expect(page.locator("#topic-feedback")).to_have_text("")
                        expect(page.locator("#topic-mentions option")).to_have_count(0)
                        expect(page.locator("#topic-heading")).to_have_text("Choose a topic")
                        page.locator("#access-token").fill(access["access_token"])
                        page.locator("#login-form button[type=submit]").click()
                        page.locator("#topics-tab").click()
                        page.get_by_role("button", name="Delivery", exact=False).click()
                        page.locator("#topic-body").fill("new session pending")
                        page.locator("#topic-send").click()
                        page.wait_for_function("() => window.__sessionGate.started === 2")
                        page.evaluate("window.__sessionGate.releases[0]()")
                        page.wait_for_function("() => window.__sessionGate.returned === 1")
                        expect(page.locator("#topic-body")).to_be_disabled()
                        expect(page.locator("#topic-mentions")).to_be_disabled()
                        page.evaluate("window.__sessionGate.releases[1]()")
                        expect(page.locator("#topic-feedback")).to_have_text("Published.")
                        page.evaluate("""() => {
                          const original = window.fetch;
                          window.__createGate = { waiting: false };
                          window.fetch = async (input, options = {}) => {
                            const result = await original(input, options);
                            if (options.method === 'POST' && JSON.parse(options.body || '{}').action === 'create') {
                              window.__createGate.waiting = true;
                              await new Promise(resolve => window.__createGate.release = resolve);
                            }
                            return result;
                          };
                        }""")
                        page.once("dialog", lambda dialog: dialog.accept("Late"))
                        selected_url = page.url
                        page.locator("#topic-create").click()
                        page.wait_for_function("() => window.__createGate.waiting")
                        page.locator("#messages-tab").click()
                        page.evaluate("window.__createGate.release()")
                        expect(page.locator("#messages-view")).to_be_visible()
                        expect(page.locator("#topics-view")).to_be_hidden()
                        assert page.url == selected_url
                        missing = browser.new_context()
                        missing_page = missing.new_page()
                        missing_page.goto(access["url"] + "/?topic=invalid-id#access_token=" + access["access_token"])
                        missing_page.locator("#topics-tab").click()
                        expect(missing_page.locator("#topic-status")).to_have_text("This topic is unavailable. Select another topic.")
                        missing.close()
                        for name, session in [("agent-a", "session-a"), ("agent-b", "session-b"), ("agent-c", "session-c")]:
                            rpc("register", agent={"name": name, "provider": "inbox", "session": session, "endpoint": "", "enabled": True}, replace=False)
                        # Fixture metadata lives only in this disposable broker's DB.
                        with sqlite3.connect(state / "messages.sqlite3") as database:
                            database.executemany("INSERT INTO session_metadata(provider,session,display_name) VALUES(?,?,?)", [("inbox", "session-a", "Shared label"), ("inbox", "session-b", "Shared label")])
                        for session in ("session-a", "session-b", "session-c"):
                            rpc("topic_publish", topic={"topic_id": topic_id}, id=str(uuid.uuid4()), author={"agent_id": "inbox-" + session}, body="from " + session, mentions=[])
                        second.close()
                        context.close()
                        process.terminate()
                        process.wait(timeout=10)
                        process = subprocess.Popen([str(binary), "--state-dir", str(state), "serve", "--no-discovery", "--codex-bin", "/missing/codex"], cwd=root, env=env, stdin=subprocess.DEVNULL, stdout=log, stderr=log)
                        for _ in range(200):
                            try:
                                rpc("status")
                                break
                            except (OSError, ValueError, AssertionError):
                                if process.poll() is not None:
                                    raise RuntimeError("disposable broker failed to restart")
                                time.sleep(.05)
                        restarted_access = rpc("monitor_enable", port=0)
                        restarted = browser.new_context()
                        restarted_page = restarted.new_page()
                        restarted_page.goto(restarted_access["url"] + "/?topic=" + topic_id + "#access_token=" + restarted_access["access_token"])
                        restarted_page.locator("#topics-tab").click()
                        expect(restarted_page.locator("#topic-id")).to_have_text(topic_id)
                        expect(restarted_page.locator("#topic-heading")).to_have_text("Delivery")
                        expect(restarted_page.locator("#topic-posts")).to_contain_text("new session pending")
                        expect(restarted_page.locator("#topic-posts")).to_contain_text("Shared label")
                        expect(restarted_page.locator("#topic-posts")).to_contain_text("inbox-session-a")
                        expect(restarted_page.locator("#topic-posts")).to_contain_text("inbox-session-b")
                        expect(restarted_page.locator("#topic-posts")).to_contain_text("inbox-session-c")
                        restarted.close()
                    finally:
                        browser.close()
                print("PASS: isolated board/message navigation; topic create, duplicate names, publish, reply, mention, inert HTML, two sessions, uncertain retry, in-flight controls, rename/archive/restore, draft preservation, load older, stale navigation, invalid selection and restart")
            finally:
                process.terminate()
                try:
                    process.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait(timeout=10)


if __name__ == "__main__":
    main()
