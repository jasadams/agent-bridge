#!/usr/bin/env python3
"""Standalone AGB-25 browser acceptance against an explicitly frozen executable.

Requires installed Playwright/Chromium. Never builds, installs, changes clocks or
writes SQLite. Real private-broker responses may be held/dropped after decoding;
no successful response is fabricated. Retains private artifacts on every outcome.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import socket
import subprocess
import sys
import time
import traceback
from urllib.parse import urlsplit
import uuid


GATE = """() => {
  window.__archiveGateInstalled = true;
  const original = window.fetch;
  window.fetch = async (input, options = {}) => {
    const gate = window.__archiveGate;
    const path = new URL(typeof input === 'string' ? input : input.url, location.href).pathname;
    const match = gate?.armed && path === gate.path && (options.method || 'GET') === gate.method;
    if (match) gate.armed = false;
    const response = await original(input, options);
    if (!match) return response;
    const decode = response.json.bind(response);
    response.json = async () => {
      const actual = await decode();
      gate.actual = actual; gate.decoded = true;
      if (gate.mode === 'hold') await new Promise(resolve => gate.release = resolve);
      if (gate.mode === 'drop') throw new TypeError('QA dropped real decoded response');
      if (gate.mode === 'wrong-incarnation' || gate.mode === 'unchanged-generation') {
        if (!response.ok || actual.applied !== true || !actual.board) throw new Error('Corruption requires real accepted lifecycle response');
        const changed = structuredClone(actual);
        if (gate.mode === 'wrong-incarnation') changed.board.incarnation = crypto.randomUUID();
        else changed.board.lifecycle_generation = gate.originalGeneration;
        gate.corrupted = changed;
        return changed;
      }
      return actual;
    };
    return response;
  };
}"""


def digest(path):
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def private_env(home):
    home.mkdir(mode=0o700)
    env = {"PATH": os.environ.get("PATH", "/usr/bin:/bin"), "HOME": str(home),
           "TMPDIR": str(home), "LANG": "C.UTF-8"}
    for key, relative in (("XDG_CONFIG_HOME", "config"), ("XDG_DATA_HOME", "data"),
                          ("XDG_CACHE_HOME", "cache"), ("XDG_STATE_HOME", "state"),
                          ("CODEX_HOME", "codex"), ("CLAUDE_CONFIG_DIR", "claude"),
                          ("OPENCODE_CONFIG_DIR", "opencode")):
        path = home / relative
        path.mkdir(mode=0o700)
        env[key] = str(path)
    return env


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", required=True, type=Path)
    parser.add_argument("--binary-sha256", required=True)
    parser.add_argument("--source-checkpoint", required=True,
                        help="Parent-supplied source/checkpoint identity; recorded verbatim")
    parser.add_argument("--output", required=True, type=Path,
                        help="New private directory; choose short path outside /tmp")
    args = parser.parse_args()
    binary = args.binary.resolve(strict=True)
    actual_hash = digest(binary)
    if actual_hash != args.binary_sha256:
        parser.error("Executable differs from parent frozen checkpoint SHA256")
    output = args.output.resolve()
    output.mkdir(mode=0o700, parents=False, exist_ok=False)
    os.chmod(output, 0o700)
    state = output / "broker"
    state.mkdir(mode=0o700)
    if len(os.fsencode(state / "bridge.sock")) >= 104:
        parser.error("Output path too long for private Unix broker socket")
    report = {"binary_sha256": actual_hash, "source_checkpoint": args.source_checkpoint,
              "checks": [], "scope": "real Chromium + disposable loopback broker; no expiry simulation"}
    process = None
    log = None
    page = None

    def record(name, **details):
        report["checks"].append({"check": name, "passed": True, **details})
        print("PASS: " + name, flush=True)

    def rpc(op, **fields):
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as connection:
            connection.settimeout(12)
            connection.connect(str(state / "bridge.sock"))
            connection.sendall(json.dumps({"op": op, **fields}).encode() + b"\n")
            with connection.makefile("rb") as stream:
                value = json.loads(stream.readline(2 * 1024 * 1024))
        if not value["ok"]:
            raise RuntimeError(value.get("error", "RPC rejected"))
        return value.get("result")

    def until(label, check, seconds=18):
        deadline = time.monotonic() + seconds
        last = None
        while time.monotonic() < deadline:
            try:
                result = check()
                if result:
                    return result
            except (OSError, RuntimeError, ValueError, KeyError) as error:
                last = str(error)
            if page:
                page.wait_for_timeout(80)
            else:
                time.sleep(.08)
        raise AssertionError(f"Timed out: {label}; last={last}")

    try:
        env = private_env(output / "home")
        log = (output / "broker.log").open("wb")
        process = subprocess.Popen([str(binary), "--state-dir", str(state), "serve",
                                    "--no-discovery", "--codex-bin", "/missing/codex"],
                                   cwd=output, env=env, stdin=subprocess.DEVNULL,
                                   stdout=log, stderr=log)
        until("private broker", lambda: rpc("status"))
        credentials = rpc("monitor_enable", port=0)
        origin = credentials["url"].rstrip("/")
        token = credentials["access_token"]
        rpc("register", agent={"name": "archive-inbox", "provider": "inbox",
                               "session": str(uuid.uuid4()), "endpoint": "", "enabled": True})
        from playwright.sync_api import expect, sync_playwright
        with sync_playwright() as playwright:
            browser = playwright.chromium.launch(headless=True, env=private_env(output / "browser-home"))
            try:
                context = browser.new_context(permissions=["clipboard-write"], viewport={"width": 1100, "height": 800})
                page = context.new_page()
                page.add_init_script("(" + GATE + ")();")
                posts, unexpected, console_errors = [], [], []
                page.on("request", lambda request: posts.append({"path": urlsplit(request.url).path,
                        "body": request.post_data}) if request.method == "POST" else None)
                page.on("pageerror", lambda error: console_errors.append(str(error)))

                def guard(route):
                    request = route.request
                    if request.url.startswith(origin + "/"):
                        route.continue_()
                    else:
                        unexpected.append(request.url)
                        route.abort()
                page.route("**/*", guard)

                def api(name):
                    response = context.request.get(origin + "/api/boards/" + name,
                        headers={"Authorization": "Bearer " + token})
                    assert response.ok, (response.status, response.text())
                    return response.json()

                def inspect(name):
                    return rpc("board_inspect", board=name)

                def transition(name, action):
                    return rpc("board_" + action, board=name,
                               expected_revision=inspect(name)["revision"])

                def fixture(name):
                    rpc("board_create", board=name)
                    old = inspect(name)
                    source = ("# Archive 世界\r\n\r\n- [ ] Check here\r\n\r\nQuestion\r\n"
                              "<!-- board-input: answer -->\r\n> original 世界\r\n<!-- /board-input -->\r\n"
                              "Copy `echo safe` and [explicit link](https://example.invalid/manual).\r\n")
                    rpc("named_board_update", board=name, expected_revision=old["revision"],
                        edits=[{"op": "set", "markdown": source}], actor=None)
                    rpc("add_board_message_input", board=name, expected_revision=inspect(name)["revision"],
                        agent="archive-inbox", id="coordinator", label="Message archive inbox")
                    return inspect(name)

                def reload_pick(name):
                    accept_dialog = lambda dialog: dialog.accept()
                    page.on("dialog", accept_dialog)
                    try:
                        page.goto("about:blank", wait_until="domcontentloaded")
                        page.goto(origin + "/#access_token=" + token, wait_until="domcontentloaded")
                    finally:
                        page.remove_listener("dialog", accept_dialog)
                    expect(page.locator("#board-selector")).to_be_visible()
                    assert page.evaluate("window.__archiveGateInstalled === true"), "Actual-response gate init script was not installed"
                    until("catalog includes " + name,
                          lambda: name in page.locator("#board-selector option").evaluate_all("nodes => nodes.map(n => n.value)"))
                    page.locator("#board-selector").select_option(name)
                    expect(page.locator("#selected-board-name")).to_have_text(name)
                    until("exact visible raw", lambda: page.locator("#board-raw").text_content() == inspect(name)["markdown"])

                def edit(value):
                    page.locator('.board-inline-response[data-field-id="answer"] textarea.board-inline-text').focus()
                    page.locator("#response-text").fill(value)

                def composer():
                    # A lifecycle change may retain an old draft beside a new
                    # empty generation with the same author-facing field ID.
                    return page.locator('.message-composer[data-field-id="coordinator"]:visible')

                def pin_composer():
                    row = composer()
                    expect(row).to_have_count(1)
                    scope = str(uuid.uuid4())
                    row.evaluate("(row, scope) => row.dataset.qaArchiveScope = scope", scope)
                    return page.locator(f'.message-composer[data-qa-archive-scope="{scope}"]:visible')

                def arm(name, suffix="", method="GET", mode="hold", original_generation=None):
                    assert page.evaluate("window.__archiveGateInstalled === true"), "Actual-response gate init script was not installed"
                    page.evaluate("spec => window.__archiveGate = {...spec, armed:true, decoded:false}",
                        {"path": "/api/boards/" + name + suffix, "method": method, "mode": mode, "originalGeneration": original_generation})

                def gated():
                    until("actual response decoded", lambda: page.evaluate("Boolean(window.__archiveGate?.decoded)"))

                def release():
                    page.evaluate("window.__archiveGate?.release?.()")

                def freeze(name):
                    expect(page.locator("#board-lifecycle-status")).to_contain_text("archiv", ignore_case=True, timeout=18000)
                    expect(page.locator("#selected-board-name")).to_have_text(name)
                    assert page.locator('#board-document input[type="checkbox"]:enabled').count() == 0
                    assert page.locator("#response-save:visible:enabled").count() == 0
                    for button in composer().get_by_role("button", name="Send", exact=True).all():
                        assert button.is_disabled(), "Fresh Send remained enabled for non-active board"

                def count_posts(suffix):
                    return len([post for post in posts if post["path"].endswith(suffix)])

                baseline = fixture("archive-basic")
                reload_pick("archive-basic")
                footer = page.locator("#board-last-updated").inner_text()
                page.locator("#board-archive").click()
                expect(page.locator("#archive-confirm-dialog")).to_contain_text("30")
                page.screenshot(path=str(output / "archive-confirm-1100.png"), full_page=True)
                page.locator("#archive-confirm-cancel").click()
                assert inspect("archive-basic")["state"] == "active"
                page.locator("#board-archive").click()
                page.locator("#archive-confirm-submit").click()
                freeze("archive-basic")
                archived = inspect("archive-basic")
                assert archived["state"] == "archived"
                assert archived["markdown"] == baseline["markdown"]
                assert archived["last_updated"] == baseline["last_updated"]
                assert archived["expires_at"] - archived["archived_at"] == 30 * 24 * 60 * 60 * 1000
                assert page.locator("#board-last-updated").inner_text() == footer
                assert "archive-basic" not in [row["name"] for row in rpc("board_list")]
                page.locator("#board-archived-open").click()
                row = page.locator('#archived-list [data-board-name="archive-basic"]')
                for date_field in ("archived_at", "expires_at"):
                    expected_iso = page.evaluate("value => new Date(value).toISOString()", archived[date_field])
                    date = row.locator(f'time[data-archive-date="{date_field}"]')
                    expect(date).to_be_visible()
                    expect(date).to_have_attribute("datetime", expected_iso)
                    assert date.inner_text().strip(), "Persisted archive date has no visible local formatting"
                page.screenshot(path=str(output / "archived-dialog-1100.png"), full_page=True)
                page.set_viewport_size({"width": 320, "height": 800})
                expect(row.locator('[data-archive-action="restore"]')).to_be_visible()
                for date_field in ("archived_at", "expires_at"):
                    expect(row.locator(f'time[data-archive-date="{date_field}"]')).to_be_visible()
                layout = page.locator("#archived-dialog").evaluate("""dialog => {
                  const rect = dialog.getBoundingClientRect();
                  return {left:rect.left,right:rect.right,width:innerWidth,
                          scroll:dialog.scrollWidth,client:dialog.clientWidth};
                }""")
                assert layout["left"] >= 0 and layout["right"] <= layout["width"], layout
                assert layout["scroll"] <= layout["client"] + 1, layout
                page.screenshot(path=str(output / "archived-dialog-320.png"), full_page=True)
                page.set_viewport_size({"width": 1100, "height": 800})
                row.locator('[data-archive-action="restore"]').click()
                until("restore active", lambda: inspect("archive-basic")["state"] == "active")
                restored = inspect("archive-basic")
                assert restored["markdown"] == baseline["markdown"]
                assert restored["last_updated"] == baseline["last_updated"]
                assert restored["incarnation"] == baseline["incarnation"]
                assert restored["lifecycle_generation"] != baseline["lifecycle_generation"]
                assert restored["revision"] != baseline["revision"]
                record("real Archive, cancel confirmation, archived catalog dates, Restore exact CRLF/Unicode source and timestamp")

                # Main UI protection plus actual HTTP rejection (no mocked backend).
                reload_pick("main")
                expect(page.locator("#board-archive")).to_be_disabled()
                current = api("main")
                protected = context.request.post(origin + "/api/boards/main/archive",
                    headers={"Authorization": "Bearer " + token, "Origin": origin},
                    data={"board": "main", "expected_revision": current["revision"]})
                assert protected.status == 422, protected.text()
                assert inspect("main")["state"] == "active"
                record("main remains available and actual authenticated archive endpoint rejects protection")

                for choice in ("cancel", "discard", "save"):
                    name = "local-" + choice
                    fixture(name)
                    reload_pick(name)
                    edit("local " + choice)
                    before = count_posts("/archive")
                    page.locator("#board-archive").click()
                    expect(page.locator("#unsaved-dialog")).to_be_visible()
                    page.locator("#unsaved-" + choice).click()
                    if choice == "cancel":
                        expect(page.locator("#response-text")).to_have_value("local cancel")
                        assert inspect(name)["state"] == "active"
                        assert count_posts("/archive") == before
                    else:
                        expect(page.locator("#archive-confirm-dialog")).to_be_visible()
                        page.locator("#archive-confirm-submit").click()
                        freeze(name)
                        expected = "local save" if choice == "save" else "original 世界"
                        assert "> " + expected in inspect(name)["markdown"]
                record("local Archive honors response Save/Discard/Cancel without implicit writes")

                for fault in ("selection", "pending-save", "late-read", "uncertain-save"):
                    name = "external-" + fault
                    old = fixture(name)
                    reload_pick(name)
                    old_view = api(name)
                    edit("kept " + fault)
                    old_composer = pin_composer()
                    old_composer.locator("textarea").fill("message kept " + fault)
                    if fault == "selection":
                        page.evaluate("""() => {const r=document.createRange();r.selectNodeContents(document.querySelector('#board-document h1'));getSelection().removeAllRanges();getSelection().addRange(r);}""")
                    elif fault in ("pending-save", "uncertain-save"):
                        arm(name, "/responses", "POST", "drop" if fault == "uncertain-save" else "hold")
                        page.locator("#response-save").click()
                        gated()
                        if fault == "uncertain-save":
                            expect(page.locator("#response-feedback")).to_contain_text("may have applied")
                    else:
                        arm(name)
                        page.locator("#board-refresh").click()
                        gated()
                    if fault in ("pending-save", "uncertain-save"):
                        local_before = count_posts("/archive")
                        if page.locator("#board-archive").is_enabled():
                            page.locator("#board-archive").click()
                            if page.locator("#unsaved-dialog").is_visible():
                                expect(page.locator("#unsaved-save")).to_be_disabled()
                                page.locator("#unsaved-cancel").click()
                        expect(page.locator("#archive-confirm-dialog")).to_be_hidden()
                        assert count_posts("/archive") == local_before
                    before = len(posts)
                    archived = transition(name, "archive")
                    freeze(name)
                    expect(page.locator("#response-text")).to_have_value("kept " + fault)
                    expect(old_composer.locator("textarea")).to_have_value("message kept " + fault)
                    if fault == "selection":
                        page.screenshot(path=str(output / "archived-recoverable-drafts-1100.png"), full_page=True)
                    if fault in ("pending-save", "late-read"):
                        release()
                    page.wait_for_timeout(800)
                    freeze(name)
                    assert len(posts) == before, "Lifecycle observation replayed a mutation"
                    view = api(name)
                    assert view["read_only"] and view["lifecycle_generation"] == archived["lifecycle_generation"]
                    transition(name, "restore")
                    page.evaluate("getSelection().removeAllRanges()")
                    page.locator("#board-refresh").click()
                    until("restored generation inspected", lambda: api(name)["state"] == "active")
                    page.wait_for_timeout(800)
                    expect(page.locator("#response-text")).to_have_value("kept " + fault)
                    assert page.locator("#response-rebase:visible").count() == 0
                    assert page.locator("#response-save:visible:enabled").count() == 0
                    expect(old_composer.locator("textarea")).to_have_value("message kept " + fault)
                    assert old_composer.get_by_role("button", name="Send", exact=True).is_disabled()
                    assert len(posts) == before, "Restore reused old-generation draft or message intent"
                    assert inspect(name)["incarnation"] == old["incarnation"]
                    restored_before_stale = inspect(name)
                    for control in old_view["controls"]:
                        if control["kind"] == "message":
                            suffix = "/messages"
                            body = {"board": name, "expected_revision": old_view["revision"],
                                    "control_id": control["id"], "id": str(uuid.uuid4()),
                                    "text": "must never retarget restored generation"}
                        else:
                            suffix = "/responses"
                            value = ({"type": "text", "text": "must not write"}
                                     if control["kind"] == "text" else {"type": "checkbox", "checked": True})
                            body = {"board": name, "expected_revision": old_view["revision"],
                                    "control_id": control["id"], "value": value}
                        rejected = context.request.post(origin + "/api/boards/" + name + suffix,
                            headers={"Authorization": "Bearer " + token, "Origin": origin}, data=body)
                        assert rejected.status == 409, (suffix, rejected.status, rejected.text())
                    assert inspect(name)["markdown"] == restored_before_stale["markdown"]
                    record("external archive/identical restore freezes old generation: " + fault)

                # Exact accepted-ID readback is distinct from a fresh Send.
                for mode in ("hold", "drop"):
                    name = "message-" + mode
                    fixture(name)
                    reload_pick(name)
                    payload = "one real queued archive message " + mode
                    old_composer = pin_composer()
                    old_composer.locator("textarea").fill(payload)
                    arm(name, "/messages", "POST", mode)
                    old_composer.get_by_role("button", name="Send", exact=True).click()
                    gated()
                    submitted = json.loads(posts[-1]["body"])
                    before = count_posts("/messages")
                    if mode == "drop":
                        expect(old_composer.get_by_role("button", name="Retry same message", exact=True)).to_be_enabled()
                    transition(name, "archive")
                    freeze(name)
                    page.wait_for_timeout(800)
                    assert count_posts("/messages") == before, "Accepted message replayed on archive"
                    if mode == "hold":
                        release()
                    else:
                        # Deliberate same UUID/payload readback after archive may
                        # return its durable receipt; it cannot enqueue anew.
                        old_composer.get_by_role("button", name="Retry same message", exact=True).click()
                        until("explicit receipt readback POST", lambda: count_posts("/messages") == before + 1)
                        assert json.loads(posts[-1]["body"]) == submitted
                    until("accepted old draft cleared or removed", lambda: payload not in
                          composer().locator("textarea").evaluate_all("inputs => inputs.map(input => input.value)"))
                    page.wait_for_timeout(900)
                    history = rpc("history", agent=None, limit=100)
                    assert len([message for message in history if message["body"] == payload]) == 1
                    assert inspect(name)["state"] == "archived"
                    freeze(name)
                    record("accepted message receipt after archive is truthful, distinct from fresh Send: " + mode)

                # Gate/drop actual successful lifecycle bodies after their commit.
                for action in ("archive", "restore"):
                    for mode in ("hold", "drop", "wrong-incarnation", "unchanged-generation"):
                        name = "lifecycle-" + action + "-" + mode
                        fixture(name)
                        reload_pick(name)
                        if action == "restore":
                            transition(name, "archive")
                            freeze(name)
                            page.locator("#board-archived-open").click()
                            expect(page.locator(f'#archived-list [data-board-name="{name}"]')).to_be_visible()
                        current = inspect(name)
                        before_posts = count_posts("/" + action)
                        arm(name, "/" + action, "POST", mode, current["lifecycle_generation"])
                        if action == "archive":
                            page.locator("#board-archive").click()
                            page.locator("#archive-confirm-submit").click()
                        else:
                            page.locator(f'#archived-list [data-board-name="{name}"] [data-archive-action="restore"]').click()
                        gated()
                        committed = inspect(name)
                        assert committed["state"] == ("archived" if action == "archive" else "active")
                        assert committed["incarnation"] == current["incarnation"]
                        assert committed["lifecycle_generation"] != current["lifecycle_generation"]
                        if mode in ("wrong-incarnation", "unchanged-generation"):
                            returned = page.evaluate("window.__archiveGate.corrupted.board")
                            assert returned is not None
                            if mode == "wrong-incarnation":
                                assert returned["incarnation"] != current["incarnation"]
                            else:
                                assert returned["lifecycle_generation"] == current["lifecycle_generation"]
                        submitted = json.loads([post for post in posts if post["path"].endswith("/" + action)][-1]["body"])
                        assert submitted == {"board": name, "expected_revision": current["revision"]}
                        if mode == "hold":
                            expect(page.locator("#board-selector")).to_be_disabled()
                            expect(page.locator("#board-archive")).to_be_disabled()
                            assert count_posts("/" + action) == before_posts + 1
                            release()
                        else:
                            expect(page.locator("#board-lifecycle-status")).to_contain_text("uncertain")
                            expect(page.locator("#board-archive")).to_be_disabled()
                            if page.locator("#archived-dialog").is_visible():
                                page.locator("#archived-close").click()
                            page.wait_for_timeout(800)
                            assert count_posts("/" + action) == before_posts + 1
                            page.locator("#board-lifecycle-review").click()
                        if page.locator("#archived-dialog").is_visible():
                            page.locator("#archived-close").click()
                        until("lifecycle operation resolved by explicit response/inspection", lambda:
                              "uncertain" not in page.locator("#board-lifecycle-status").inner_text())
                        page.wait_for_timeout(800)
                        assert count_posts("/" + action) == before_posts + 1, "Lifecycle mutation replayed automatically"
                        readback = inspect(name)
                        assert readback["revision"] == committed["revision"]
                        if action == "archive":
                            assert readback["archived_at"] == committed["archived_at"]
                            assert readback["expires_at"] == committed["expires_at"]
                        else:
                            for date_field in ("archived_at", "expires_at"):
                                assert committed.get(date_field) is None
                                assert readback.get(date_field) is None
                        record("actual lifecycle accepted body " + mode + " preserves target/deadline without replay: " + action)

                # Copy/raw/footer and catalog survive the lifecycle; fresh tab state.
                name = "copy-private"
                fixture(name)
                reload_pick(name)
                page.locator(".board-code-copy").first.click()
                expect(page.locator("#board-copy-feedback")).to_contain_text("copied")
                source = inspect(name)["markdown"]
                page.locator("#board-raw-toggle").click()
                expect(page.locator("#board-raw")).to_have_text(source)
                expect(page.locator("#board-last-updated")).to_be_visible()
                assert page.evaluate("localStorage.length + sessionStorage.length") == 0
                assert token not in page.url
                page.locator("#board-raw-toggle").click()
                edit("PRIVATE_DRAFT_SENTINEL")
                page.locator("#lock-button").click()
                page.locator("#unsaved-discard").click()
                expect(page.locator("#response-text")).to_have_value("")
                assert page.locator("#response-text").get_attribute("aria-label") is None
                expect(page.locator("#board-last-updated")).to_be_hidden()
                assert "PRIVATE_DRAFT_SENTINEL" not in page.content()
                assert not unexpected, unexpected
                assert not console_errors, console_errors
                record("copy/raw/footer/catalog and private draft/token cleanup preserved")
                page.screenshot(path=str(output / "final-private-lock.png"), full_page=True)
                report["mutation_counts"] = {suffix: count_posts(suffix) for suffix in
                    ("/archive", "/restore", "/responses", "/messages")}
                context.close()
            finally:
                if sys.exc_info()[0] is not None and page is not None:
                    try:
                        page.screenshot(path=str(output / "failure.png"), full_page=True, timeout=3000)
                    except Exception:
                        pass
                browser.close()
        assert digest(binary) == actual_hash, "Frozen executable changed during acceptance"
        report["passed"] = True
    except BaseException as error:
        report["passed"] = False
        report["error"] = str(error)
        report["traceback"] = traceback.format_exc()
        if page:
            try:
                page.screenshot(path=str(output / "failure.png"), full_page=True, timeout=3000)
            except Exception:
                pass
        raise
    finally:
        if process is not None:
            process.terminate()
            try:
                process.wait(timeout=8)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait(timeout=5)
            report["broker_exit"] = process.returncode
        if log is not None:
            log.close()
        (output / "results.json").write_text(json.dumps(report, indent=2) + "\n")
        os.chmod(output / "results.json", 0o600)


if __name__ == "__main__":
    main()
