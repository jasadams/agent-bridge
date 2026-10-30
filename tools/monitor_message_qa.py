"""Keyboard composer acceptance against actual disposable broker HTTP/native paths.

Faults delay/drop/corrupt a real accepted response; no broker responses are mocked.
"""
import json
import os
from pathlib import Path
import socket
import sqlite3
import tempfile
import threading
import time
import uuid


def message_browser_checks(*, page, context, rpc, state, url, token, record, output):
    from playwright.sync_api import expect

    origin = url.rstrip("/")
    posts, frames, native_errors = [], [], []
    stop = threading.Event()
    temporary = tempfile.TemporaryDirectory(prefix="ab-web-msg-", dir="/tmp")
    root = Path(temporary.name)
    endpoint = root / "native.sock"
    listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    listener.bind(str(endpoint)); os.chmod(endpoint, 0o600)
    listener.listen(); listener.settimeout(.1)

    def receive():
        while not stop.is_set():
            try:
                connection, _ = listener.accept()
            except socket.timeout:
                continue
            except OSError:
                if stop.is_set():
                    return
                raise
            try:
                with connection:
                    connection.settimeout(2)
                    data = bytearray()
                    while b"\n" not in data:
                        chunk = connection.recv(65536)
                        if not chunk:
                            break
                        data.extend(chunk)
                    if data:
                        frames.append(json.loads(data.split(b"\n", 1)[0]))
            except Exception as error:
                native_errors.append(str(error))

    thread = threading.Thread(target=receive)
    thread.start()

    def watch(request):
        if request.method == "POST" and "/api/boards/" in request.url and request.url.endswith("/messages"):
            posts.append(json.loads(request.post_data))
    page.on("request", watch)

    def wait(label, check, seconds=15):
        deadline = time.monotonic() + seconds
        while time.monotonic() < deadline:
            if check():
                return
            page.wait_for_timeout(60)
        raise AssertionError("Timed out: " + label)

    def read(name="web-compose"):
        return rpc(state, "named_board_read", board=name)

    def write(markdown, name="web-compose"):
        return rpc(state, "named_board_update", board=name,
                   expected_revision=read(name)["revision"],
                   edits=[{"op": "set", "markdown": markdown}], actor=None)

    def history(body):
        return [m for m in rpc(state, "history", agent=None, limit=1000) if m["body"] == body]

    def pick(name, keep_drafts=False):
        page.locator("#board-selector").select_option(name)
        if keep_drafts:
            expect(page.locator("#unsaved-dialog")).to_be_visible()
            page.get_by_role("button", name="Keep drafts and continue").press("Enter")
        expect(page.locator("#selected-board-name")).to_have_text(name)
        wait("board snapshot", lambda: page.locator("#board-raw").text_content() == read(name)["markdown"])

    def row(field="coordinator"):
        return page.locator(f'.message-composer[data-field-id="{field}"]:visible').first

    def sent(body, target="web-coordinator"):
        wait("exact history message", lambda: len(history(body)) == 1)
        message = history(body)[0]
        assert message["sender"] == "user" and message["recipient"] == target
        expect(row().locator("textarea")).to_have_value("")
        expect(row().locator("textarea")).to_be_focused()
        return message

    def arm_gate(corrupt=False, corrupt_session=False):
        page.evaluate("""options => {
          if (!window.__composerFetch) {
            window.__composerFetch = window.fetch;
            window.fetch = async (...args) => {
              const gate = window.__composerGate;
              const match = gate?.armed && args[1]?.method === 'POST' && String(args[0]).endsWith('/messages');
              if (match) gate.armed = false;
              const response = await window.__composerFetch(...args);
              if (match) {
                const decode = response.json.bind(response);
                response.json = async () => {
                  const value = await decode(); gate.decoded = true;
                  await new Promise(resolve => gate.release = resolve);
                  return gate.corrupt ? {...value, id: 'invalid-receipt'} : gate.corruptSession ? {...value, message: {...value.message, recipient_session:'wrong-session'}} : value;
                };
              }
              return response;
            };
          }
          window.__composerGate = {armed:true, decoded:false, corrupt:options.corrupt, corruptSession:options.corruptSession};
        }""", {"corrupt": corrupt, "corruptSession": corrupt_session})

    def release_gate():
        page.evaluate("window.__composerGate?.release?.()")

    def add(name, owner, field, label):
        return rpc(state, "add_board_message_input", board=name,
                   expected_revision=read(name)["revision"], agent=owner, id=field, label=label)

    try:
        session = str(uuid.uuid4())
        for name, provider, native in [("web-coordinator", "claude", str(endpoint)), ("web-second", "inbox", "")]:
            rpc(state, "register", agent={"name": name, "provider": provider,
                "session": session if provider == "claude" else str(uuid.uuid4()),
                "endpoint": native, "enabled": True})
        for name in ("web-compose", "web-other"):
            rpc(state, "board_create", board=name)
            write("# Keyboard messages\n\nRun `printf hello` while composing.\n\n```text\n<!-- board-message: literal example -->\n```\n\n- [ ] Existing checkbox\n<!-- board-input: answer -->\n> existing\n<!-- /board-input -->\n", name)
            add(name, "web-coordinator", "coordinator", "Message coordinator")
            add(name, "web-second", "second", "Message second")
        baseline = {name: read(name)["markdown"] for name in ("web-compose", "web-other")}
        inventory = []

        def reset_case():
            # Explicit fixture discard/reload between independent cases, never a
            # production recovery path. This also clears any suspended JS gates.
            rpc(state, "pause", paused=False)
            rpc(state, "register", agent={"name": "web-coordinator", "provider": "claude", "session": session, "endpoint": str(endpoint), "enabled": True}, replace=True)
            for name, markdown in baseline.items():
                write(markdown, name)
            page.unroute("**/api/boards/web-compose/messages")
            accept = lambda dialog: dialog.accept()
            page.on("dialog", accept)
            try:
                page.goto(origin, wait_until="domcontentloaded")
            finally:
                page.remove_listener("dialog", accept)
            page.locator("#access-token").fill(token)
            page.locator("#login-form button[type=submit]").click()
            expect(page.locator("#boards-view")).to_be_visible()
            pick("web-compose")
            visible = page.locator("#board-document").text_content()
            assert "<!-- board-message: literal example -->" in visible, "Literal code example must remain visible"
            assert str(endpoint) not in visible and '"instance"' not in visible, "Machine-readable message metadata leaked into rendered board"
            assert "<!-- board-message:" in page.locator("#board-raw").text_content()

        def case_keyboard_native():
            textarea = row().locator("textarea")
            before = read()
            page.locator("#board-selector").focus()
            for _ in range(30):
                page.keyboard.press("Tab")
                if textarea.evaluate("e => e === document.activeElement"):
                    break
            expect(textarea).to_be_focused()
            textarea.fill("tab navigation draft")
            page.evaluate("""() => { window.__composerCopies = []; Object.defineProperty(navigator, 'clipboard', {configurable:true,value:{writeText:async text => { window.__composerCopies.push(text); }}}); }""")
            page.locator("#board-document button.board-code-copy").first.click()
            expect(textarea).to_have_value("tab navigation draft")
            assert page.evaluate("window.__composerCopies") == ["printf hello"]
            assert not history("tab navigation draft"), "copy must not send a composer"
            textarea.focus()
            page.keyboard.press("Tab")
            expect(row().get_by_role("button", name="Send", exact=True)).to_be_focused()
            page.keyboard.press("Shift+Tab"); expect(textarea).to_be_focused()
            textarea.fill("  exact keyboard 🦀")
            page.keyboard.press("Shift+Enter"); page.keyboard.type("second line  ")
            body = "  exact keyboard 🦀\nsecond line  "
            expect(textarea).to_have_value(body); assert not history(body)
            geometry = page.evaluate("""() => ['.board-layout', '#board-messages'].map(selector => {
              const {x,y,width,height}=document.querySelector(selector).getBoundingClientRect(); return {x,y,width,height};
            })""")
            page.keyboard.press("Enter"); first = sent(body)
            wait("actual native frame", lambda: any(f.get("msg_id") == first["id"] for f in frames))
            frame = next(f for f in frames if f.get("msg_id") == first["id"])
            assert frame["from"] == "user" and frame["session_id"] == session and body in frame["message"]["content"]
            assert read() == before, "Sending edited board Markdown/revision/timestamp"
            expect(row().locator(".message-feedback")).to_have_text("")
            expect(row().locator(".message-feedback")).to_be_hidden()
            assert page.evaluate("""() => ['.board-layout', '#board-messages'].map(selector => {
              const {x,y,width,height}=document.querySelector(selector).getBoundingClientRect(); return {x,y,width,height};
            })""") == geometry, "Accepted send shifted the document or composer dock"
            page.screenshot(path=str(output / "message-accepted-no-feedback.png"), full_page=False)
            textarea.fill("second deliberate message"); page.keyboard.press("Enter"); second = sent("second deliberate message")
            assert first["id"] != second["id"]
            record("keyboard Tab/backtab, Enter send, Shift+Enter exact multiline, repeated reuse/focus and actual pinned native delivery; board unchanged")

        def case_blank_ime():
            textarea = row().locator("textarea")
            count = len(posts)
            textarea.fill("   \n "); page.keyboard.press("Enter"); page.wait_for_timeout(100)
            assert len(posts) == count
            textarea.fill("IME draft")
            textarea.dispatch_event("compositionstart")
            textarea.dispatch_event("keydown", {"key": "Enter", "code": "Enter", "isComposing": True})
            assert len(posts) == count
            textarea.dispatch_event("compositionend")
            textarea.dispatch_event("keydown", {"key": "Enter", "code": "Enter", "keyCode": 229})
            assert len(posts) == count
            textarea.fill("")
            record("blank and IME composition Enter never submit")

        def case_pending_refresh():
            textarea = row().locator("textarea")
            textarea.fill("pending exact message"); arm_gate()
            geometry = """row => {
              const rect = e => { const {x,y,width,height}=e.getBoundingClientRect(); return {x,y,width,height}; };
              return [document.querySelector('.board-layout'), document.querySelector('#board-messages'), row,
                row.querySelector('textarea'), row.querySelector('.response-actions button')].map(rect);
            }"""
            before_geometry = row().evaluate(geometry)
            page.keyboard.press("Enter")
            wait("decoded real acceptance held", lambda: page.evaluate("window.__composerGate.decoded"))
            count = len(posts)
            page.keyboard.press("Enter"); page.keyboard.press("Enter")
            expect(textarea).to_have_attribute("readonly", "")
            expect(row().get_by_role("button", name="Send", exact=True)).to_be_disabled()
            expect(row().locator(".message-feedback")).to_have_text("")
            expect(row().locator(".message-feedback")).to_be_hidden()
            assert row().evaluate(geometry) == before_geometry, "Pending send shifted composer/document geometry"
            page.screenshot(path=str(output / "message-pending-no-feedback.png"), full_page=False)
            assert len(posts) == count
            # Ordinary refresh must continue while this composer awaits its receipt.
            write(read()["markdown"] + "\nUpdated during pending send\n")
            expect(page.locator("#board-document")).to_contain_text("Updated during pending send", timeout=15000)
            release_gate(); sent("pending exact message")
            expect(row().locator(".message-feedback")).to_be_hidden()
            assert row().evaluate(geometry) == before_geometry, "Accepted send shifted composer/document geometry"
            assert len(posts) == count and read()["markdown"].endswith("Updated during pending send\n")
            record("pending Enter does not duplicate and actual live refresh continues independently while acceptance is delayed")

        def case_live_draft_review():
            textarea = row().locator("textarea")
            textarea.fill("stable caret draft"); textarea.evaluate("e => e.setSelectionRange(4,4)")
            write(read()["markdown"] + "\nOrdinary refresh\n")
            expect(row().get_by_role("button", name="Review current input")).to_be_visible(timeout=15000)
            expect(textarea).to_be_focused(); expect(textarea).to_have_value("stable caret draft")
            assert textarea.evaluate("e => e.selectionStart") == 4
            page.set_viewport_size({"width": 390, "height": 640})
            textarea.focus()
            page.screenshot(path=str(output / "message-composers-stale-390.png"), full_page=False)
            page.set_viewport_size({"width": 1440, "height": 900})
            page.keyboard.press("Enter"); assert not history("stable caret draft")
            row().get_by_role("button", name="Review current input").click()
            expect(textarea).to_be_focused(); page.keyboard.press("Enter"); sent("stable caret draft")
            record("live revision changes retain exact draft/node/focus/caret and require explicit current-input review")

        def case_lost_receipt_retry():
            textarea = row().locator("textarea")
            textarea.fill("accepted but lost response")
            def lose_response(route):
                response = route.fetch()
                assert response.status == 200
                route.abort("failed")
            page.route("**/api/boards/web-compose/messages", lose_response, times=1)
            page.keyboard.press("Enter")
            expect(row().locator(".message-feedback")).to_contain_text("uncertain")
            expect(textarea).to_have_value("accepted but lost response")
            page.set_viewport_size({"width": 390, "height": 640})
            textarea.focus()
            page.screenshot(path=str(output / "message-composers-uncertain-390.png"), full_page=False)
            page.set_viewport_size({"width": 1440, "height": 900})
            old = posts[-1].copy(); count = len(posts)
            page.wait_for_timeout(300); assert len(posts) == count
            # An accepted retry remains safe after a board revision change.
            write(read()["markdown"] + "\nChanged after lost receipt\n")
            expect(page.locator("#board-document")).to_contain_text("Changed after lost receipt", timeout=15000)
            retry_board = read()
            page.keyboard.press("Enter"); sent("accepted but lost response")
            assert read() == retry_board, "Retry changed board Markdown, revision or timestamp"
            assert posts[-1] == old and len(history("accepted but lost response")) == 1
            record("real accepted POST with lost response retains UUID/body; explicit identical retry after board change returns one durable message")

        def case_malformed_receipt():
            textarea = row().locator("textarea")
            textarea.fill("corrupt receipt retained"); arm_gate(corrupt=True); page.keyboard.press("Enter")
            wait("real acceptance decoded", lambda: page.evaluate("window.__composerGate.decoded")); release_gate()
            expect(row().locator(".message-feedback")).to_contain_text("uncertain")
            expect(textarea).to_have_value("corrupt receipt retained")
            old = posts[-1].copy(); page.keyboard.press("Enter"); sent("corrupt receipt retained")
            assert posts[-1] == old and len(history("corrupt receipt retained")) == 1
            record("malformed decoded acceptance does not clear draft; exact retry reads actual durable broker receipt")

        def case_wrong_session_receipt():
            textarea = row().locator("textarea")
            textarea.fill("wrong session receipt retained"); arm_gate(corrupt_session=True); page.keyboard.press("Enter")
            wait("actual receipt before session corruption", lambda: page.evaluate("window.__composerGate.decoded")); release_gate()
            expect(row().locator(".message-feedback")).to_contain_text("uncertain")
            expect(textarea).to_have_value("wrong session receipt retained")
            old = posts[-1].copy(); page.keyboard.press("Enter"); sent("wrong session receipt retained")
            assert posts[-1] == old and len(history("wrong session receipt retained")) == 1
            record("wrong-session decoded receipt never clears pinned-owner draft; exact retry confirms the real recipient")

        def case_other_composer_focus():
            textarea = row().locator("textarea")
            textarea.fill("delayed response keeps other focus"); arm_gate(); page.keyboard.press("Enter")
            wait("delayed acceptance", lambda: page.evaluate("window.__composerGate.decoded"))
            other = row("second").locator("textarea"); other.fill("typing for second owner")
            other.evaluate("e => e.setSelectionRange(5,5)")
            release_gate(); wait("delayed accepted draft cleared", lambda: textarea.input_value() == "")
            expect(other).to_be_focused(); expect(other).to_have_value("typing for second owner")
            assert other.evaluate("e => e.selectionStart") == 5
            other.fill("")
            record("delayed acceptance leaves another composer focus/caret/draft undisturbed")

        def case_boards_and_owners():
            textarea = row().locator("textarea")
            # Independent inputs survive board navigation without a send-on-leave.
            textarea.fill("main retained unsent")
            row("second").locator("textarea").fill("second owner retained unsent")
            pick_control = page.locator("#board-selector")
            pick_control.select_option("web-other")
            expect(page.locator("#unsaved-dialog")).to_be_visible()
            page.get_by_role("button", name="Keep drafts and continue").click()
            expect(page.locator("#selected-board-name")).to_have_text("web-other")
            expect(row().locator("textarea")).to_have_value("")
            row("second").locator("textarea").fill("other board second target")
            row("second").locator("textarea").press("Enter")
            wait("other target history", lambda: len(history("other board second target")) == 1)
            assert history("other board second target")[0]["recipient"] == "web-second"
            assert history("other board second target")[0]["recipient_session"] != session
            expect(row("second").locator("textarea")).to_have_value("")
            pick("web-compose", keep_drafts=True)
            expect(row().locator("textarea")).to_have_value("main retained unsent")
            expect(row("second").locator("textarea")).to_have_value("second owner retained unsent")
            row().locator("textarea").fill("")
            row("second").locator("textarea").fill("")
            assert not history("main retained unsent") and not history("second owner retained unsent")
            record("per-input drafts survive explicit keep/navigation; two boards and two pinned owners never cross-route or send on switch")

        def case_retargeted_control():
            textarea = row().locator("textarea")
            # Retargeting produces a new empty composer and leaves the old draft visible.
            row().locator("textarea").fill("must not retarget")
            old_markdown = read()["markdown"]
            changed = old_markdown.replace('"name":"web-coordinator"', '"name":"web-second"')
            # Use a valid replacement marker carrying the other exact identity.
            lines = changed.splitlines()
            second_marker = next(line for line in old_markdown.splitlines() if '"id":"second"' in line)
            owner = json.loads(second_marker.removeprefix("<!-- board-message: ").removesuffix(" -->"))["owner"]
            for i, line in enumerate(lines):
                if '"id":"coordinator"' in line:
                    marker = json.loads(line.removeprefix("<!-- board-message: ").removesuffix(" -->")); marker["owner"] = owner
                    lines[i] = "<!-- board-message: " + json.dumps(marker, separators=(",", ":")) + " -->"
            write("\n".join(lines) + "\n")
            expect(page.locator('.message-composer[data-field-id="coordinator"]:visible')).to_have_count(2, timeout=15000)
            orphan = page.locator('.message-composer[data-field-id="coordinator"]:visible').filter(has=page.locator('textarea')).first
            expect(orphan.locator("textarea")).to_have_value("must not retarget")
            orphan.locator("textarea").press("Enter"); assert not history("must not retarget")
            orphan.get_by_role("button", name="Review current input").click()
            expect(orphan.locator(".message-feedback")).to_contain_text("unavailable or changed")
            orphan.get_by_role("button", name="Discard draft").click()
            write(old_markdown); page.locator("#board-refresh").click()
            wait("restored exact target", lambda: "web-coordinator" in row().locator("label").text_content())
            record("retargeted control leaves old draft visible and unavailable; review cannot silently substitute the new recipient")

        def case_closed_owner():
            textarea = row().locator("textarea")
            # Exact owner disabled: actual backend rejection retains the user's text.
            rpc(state, "unregister", name="web-coordinator", session=session)
            row().locator("textarea").fill("closed owner draft"); row().locator("textarea").press("Enter")
            expect(row().locator(".message-feedback")).to_contain_text("closed, disabled or replaced")
            expect(row().locator("textarea")).to_have_value("closed owner draft")
            assert not history("closed owner draft")
            rejected_payload = posts[-1].copy(); count = len(posts)
            row().locator("textarea").press("Enter")
            wait("confirmed rejection retry", lambda: len(posts) == count + 1 and not row().get_by_role("button", name="Retry same message").is_disabled())
            assert posts[-1] == rejected_payload
            row().get_by_role("button", name="Discard draft").click()
            rpc(state, "register", agent={"name": "web-coordinator", "provider": "claude", "session": session, "endpoint": str(endpoint), "enabled": True})
            record("actual unavailable owner rejection preserves draft and same-payload retry ID without starting or substituting a conversation")

        def case_replaced_session():
            textarea = row().locator("textarea")
            replacement = str(uuid.uuid4())
            rpc(state, "register", agent={"name": "web-coordinator", "provider": "claude", "session": replacement, "endpoint": str(endpoint), "enabled": True}, replace=True)
            row().locator("textarea").fill("same route replaced session"); row().locator("textarea").press("Enter")
            expect(row().locator(".message-feedback")).to_contain_text("closed, disabled or replaced")
            expect(row().locator("textarea")).to_have_value("same route replaced session")
            assert not history("same route replaced session")
            row().get_by_role("button", name="Discard draft").click()
            rpc(state, "register", agent={"name": "web-coordinator", "provider": "claude", "session": session, "endpoint": str(endpoint), "enabled": True}, replace=True)
            record("actual same-name registry replacement cannot receive the old pinned-session draft")

        def case_queue_capacity():
            textarea = row().locator("textarea")
            # Queue-full is an actual broker capacity rejection, not an HTTP mock.
            rpc(state, "pause", paused=True)
            queued = []
            try:
                for i in range(1001):
                    message_id = str(uuid.uuid4())
                    try:
                        rpc(state, "send", id=message_id, sender="user", recipient="web-second", body=f"capacity fixture {i}", parent=None)
                        queued.append(message_id)
                    except RuntimeError as error:
                        assert "Queue is full" in str(error)
                        break
                else:
                    raise AssertionError("Broker capacity was not enforced")
                row().locator("textarea").fill("capacity rejected draft"); row().locator("textarea").press("Enter")
                expect(row().locator(".message-feedback")).to_contain_text("Queue is full")
                expect(row().locator("textarea")).to_have_value("capacity rejected draft")
                assert not history("capacity rejected draft")
                row().get_by_role("button", name="Discard draft").click()
            finally:
                for message_id in queued:
                    rpc(state, "cancel", id=message_id)
                rpc(state, "pause", paused=False)
            record("actual full broker queue rejects send while retaining draft; private capacity fixtures cancelled afterward")

        def case_client_render_fallback():
            # Server supports deeper nesting than the bounded browser builder.
            # Force the actual client fallback while keeping a valid composer.
            source = read()["markdown"] + "\n" + "> " * 33 + "Deep visible content\n"
            write(source)
            fallback = page.locator("#board-document .board-render-fallback")
            expect(fallback).to_be_visible(timeout=15000)
            expect(fallback).to_contain_text("Deep visible content")
            visible = fallback.text_content()
            assert "<!-- board-message: literal example -->" in visible
            assert str(endpoint) not in visible and '"instance"' not in visible, "Client fallback exposed machine-readable message metadata"
            assert page.locator("#board-raw").text_content() == source
            expect(row().locator("textarea")).to_be_visible()
            record("actual client depth-limit fallback hides valid metadata while raw source and reusable composers stay intact")

        def case_dock_layout():
            write(read()["markdown"] + "\n" + "Board scroll context\n\n" * 80)
            for index in range(6):
                add("web-compose", "web-second", f"stack-{index}", f"Stack {index}")
            expect(page.locator("#message-controls .message-composer:visible")).to_have_count(8)
            layout_metrics = []
            for width, height in ((1440, 900), (390, 640), (320, 640), (900, 400)):
                page.set_viewport_size({"width": width, "height": height})
                dock = page.locator("#board-messages")
                document = page.locator(".board-layout")
                bounds = dock.bounding_box()
                reading = document.bounding_box()
                metrics = {"width": width, "height": height, "dock": bounds, "document": reading}
                layout_metrics.append(metrics)
                (output / "message-dock-metrics.json").write_text(json.dumps(layout_metrics, indent=2))
                page.screenshot(path=str(output / f"message-composers-before-{width}x{height}.png"), full_page=False)
                assert bounds["y"] + bounds["height"] <= height + 1
                assert reading["height"] >= 48 and reading["y"] + reading["height"] <= bounds["y"] + 1
                assert dock.evaluate("e => e.scrollHeight > e.clientHeight"), "Many inputs need a bounded scroll stack"
                document.evaluate("e => e.scrollTop = 200")
                assert document.evaluate("e => e.scrollTop") > 0
                assert dock.bounding_box() == bounds, "Board scroll moved the pinned stack"
                last = row("stack-5").locator("textarea")
                last.focus()
                expect(last).to_be_focused()
                assert dock.evaluate("e => e.scrollTop") > 0
                # Already-focused boxes must also be revealed after viewport resize.
                page.wait_for_function("""() => {
                  const input = document.activeElement, dock = document.querySelector('#board-messages');
                  if (!input?.matches('#message-controls textarea')) return false;
                  const box = input.getBoundingClientRect(), bounds = dock.getBoundingClientRect();
                  return box.top >= bounds.top && box.bottom <= bounds.bottom + 1;
                }""")
                box = last.bounding_box()
                metrics.update({"focused_input": box, "dock_scroll_top": dock.evaluate("e => e.scrollTop")})
                (output / "message-dock-metrics.json").write_text(json.dumps(layout_metrics, indent=2))
                assert box["y"] >= bounds["y"] and box["y"] + box["height"] <= bounds["y"] + bounds["height"] + 1, metrics
                assert page.evaluate("document.documentElement.scrollWidth <= innerWidth + 1")
                page.screenshot(path=str(output / f"message-composers-stacked-{width}x{height}.png"), full_page=False)
            page.set_viewport_size({"width": 1440, "height": 900})
            first = row().locator("textarea")
            first.click(); page.keyboard.type("direct click draft")
            expect(first).to_have_value("direct click draft")
            expect(row().locator("label")).to_have_text("web-coordinator")
            assert row().locator(".message-feedback").text_content() == ""
            assert not row().get_by_role("button", name="Discard draft").is_visible()
            first.fill("")
            assert not history("direct click draft")
            record("desktop/mobile/short many-input dock stays pinned, independently scrolls, reveals focused boxes and edits on click")

        def case_real_friendly_metadata():
            native_session = "885b5a56-af8d-4a72-9b43-8ad930e945b6"
            canonical = "claude-" + native_session
            rpc(state, "register", agent={"name": canonical, "provider": "claude", "session": native_session, "endpoint": str(endpoint), "enabled": True})
            rpc(state, "board_create", board="web-friendly")
            add("web-friendly", canonical, "friendly", "Support desk")
            add("web-friendly", canonical, "unknown", "Message " + canonical)
            def title(session, value):
                # Existing private test DB metadata; no public title-write API exists.
                with sqlite3.connect(Path(state) / "messages.sqlite3") as db:
                    db.execute("INSERT INTO session_metadata(provider,session,display_name) VALUES('claude',?,?) ON CONFLICT(provider,session) DO UPDATE SET display_name=excluded.display_name", (session, value))
            pick("web-friendly")
            expect(row("friendly").locator("label")).to_have_text("Support desk")
            expect(row("unknown").locator("label")).to_have_text("Claude")
            before = read("web-friendly")
            title(native_session, "Coordinator")
            expect(row("friendly").locator("label")).to_have_text("Coordinator", timeout=10000)
            textarea = row("friendly").locator("textarea")
            textarea.fill("friendly pinned draft"); textarea.evaluate("e => {e.setSelectionRange(4,4);window.__friendlyInput=e;}")
            page.screenshot(path=str(output / "message-canonical-friendly-title.png"), full_page=False)
            reads = []
            watch_reads = lambda request: reads.append(request.url) if request.method == "GET" and "/api/boards/web-friendly" in request.url and "/events" not in request.url else None
            page.on("request", watch_reads)
            try:
                title(native_session, "Planner")
                expect(row("friendly").locator("label")).to_have_text("Planner", timeout=10000)
                assert textarea.evaluate("e => e === window.__friendlyInput && e === document.activeElement && e.selectionStart === 4")
                expect(textarea).to_have_value("friendly pinned draft")
                assert not reads, "Presentation-only title changes fetched board snapshots"
            finally:
                page.remove_listener("request", watch_reads)
            assert read("web-friendly") == before, "Reading/renaming labels changed the board"
            page.screenshot(path=str(output / "message-canonical-renamed-title.png"), full_page=False)
            textarea.press("Enter")
            wait("canonical recipient accepted", lambda: len(history("friendly pinned draft")) == 1)
            accepted = history("friendly pinned draft")[0]
            assert accepted["recipient"] == canonical and accepted["recipient_session"] == native_session
            wait("canonical recipient native delivery", lambda: any(frame.get("msg_id") == accepted["id"] and frame.get("session_id") == native_session for frame in frames))
            expect(textarea).to_have_value("")
            textarea.fill("replacement pinned draft")
            replacement = str(uuid.uuid4())
            rpc(state, "register", agent={"name": canonical, "provider": "claude", "session": replacement, "endpoint": str(endpoint), "enabled": True}, replace=True)
            title(replacement, "Wrong replacement")
            expect(row("friendly").locator("label")).to_have_text("Support desk", timeout=10000)
            assert "Wrong replacement" not in page.locator("#message-controls").text_content()
            textarea.press("Enter")
            expect(row("friendly").locator(".message-feedback")).to_contain_text("closed, disabled or replaced")
            expect(textarea).to_have_value("replacement pinned draft")
            assert not history("replacement pinned draft")
            assert read("web-friendly") == before
            page.screenshot(path=str(output / "message-canonical-replaced-title.png"), full_page=False)
            (output / "friendly-label-fixture.json").write_text(json.dumps({"canonical": canonical, "session": native_session, "title": "Coordinator", "renamed": "Planner", "replacement_title": "Wrong replacement", "authored_fallback": "Support desk", "board_revision": before["revision"]}, indent=2))
            row("friendly").get_by_role("button", name="Discard draft").click()
            record("real canonical Claude ID uses native friendly metadata; live title rename keeps input/caret/draft without snapshot polling; missing metadata honors authored/provider fallbacks; replacement cannot lend a title or receive old draft")

        def case_lock_and_layout():
            textarea = row().locator("textarea")
            row().locator("textarea").fill("lock private draft")
            page.locator("#lock-button").click()
            expect(page.locator("#unsaved-save")).to_be_disabled()
            page.locator("#unsaved-cancel").click(); expect(row().locator("textarea")).to_have_value("lock private draft")
            page.locator("#lock-button").click(); page.locator("#unsaved-discard").click()
            expect(page.locator("#workspace")).to_be_hidden()
            assert not page.locator("#message-controls").text_content()
            page.locator("#access-token").fill(token); page.locator("#login-form button[type=submit]").click()
            expect(page.locator("#boards-view")).to_be_visible()
            assert not history("lock private draft")
            assert page.evaluate("localStorage.length + sessionStorage.length") == 0 and context.cookies() == []
            assert not native_errors, native_errors
            record("locking requires explicit draft discard/cancel, clears all private composer state and never submits; no browser persistence")
            pick("web-compose")
            for width in (1440, 320):
                page.set_viewport_size({"width": width, "height": 900})
                assert page.evaluate("document.documentElement.scrollWidth <= innerWidth + 1")
                page.screenshot(path=str(output / f"message-composers-{width}.png"), full_page=True)

        for label, scenario in [
            ("keyboard_native", case_keyboard_native),
            ("blank_ime", case_blank_ime),
            ("pending_refresh", case_pending_refresh),
            ("live_draft_review", case_live_draft_review),
            ("lost_receipt_retry", case_lost_receipt_retry),
            ("malformed_receipt", case_malformed_receipt),
            ("wrong_session_receipt", case_wrong_session_receipt),
            ("other_composer_focus", case_other_composer_focus),
            ("boards_and_owners", case_boards_and_owners),
            ("retargeted_control", case_retargeted_control),
            ("closed_owner", case_closed_owner),
            ("replaced_session", case_replaced_session),
            ("queue_capacity", case_queue_capacity),
            ("client_render_fallback", case_client_render_fallback),
            ("dock_layout", case_dock_layout),
            ("real_friendly_metadata", case_real_friendly_metadata),
            ("lock_and_layout", case_lock_and_layout),
        ]:
            ready = False
            try:
                reset_case()
                ready = True
                scenario()
                inventory.append({"case": label, "status": "passed"})
            except Exception as error:
                status = "failed" if ready else "blocked"
                inventory.append({"case": label, "status": status, "error": str(error)})
                print(f"{status.upper()}: message browser {label}: {error}", flush=True)
            finally:
                try:
                    if not page.is_closed():
                        release_gate()
                except Exception as error:
                    inventory[-1]["status"] = "failed"
                    inventory[-1]["cleanup_error"] = str(error)
                    print(f"FAILED: {label} gate cleanup: {error}", flush=True)
                (output / "message-results.json").write_text(json.dumps(inventory, indent=2) + "\n")
        unsuccessful = [case for case in inventory if case["status"] != "passed"]
        assert not unsuccessful, json.dumps(unsuccessful, indent=2)
    finally:
        cleanup_errors = []
        def cleanup(label, action):
            try:
                action()
            except Exception as error:
                cleanup_errors.append(f"{label}: {error}")
        # Browser teardown failures cannot prevent private socket/thread cleanup.
        stop.set()
        cleanup("native listener", listener.close)
        cleanup("native thread wait", lambda: thread.join(timeout=3))
        if thread.is_alive():
            cleanup_errors.append("Private native receiver did not stop")
        else:
            cleanup("native temporary directory", temporary.cleanup)
        cleanup("browser gate", lambda: release_gate() if not page.is_closed() else None)
        cleanup("browser listener", lambda: page.remove_listener("request", watch))
        (output / "message-cleanup.json").write_text(json.dumps({"thread_alive": thread.is_alive(), "errors": cleanup_errors}, indent=2) + "\n")
        if cleanup_errors:
            raise RuntimeError("Message browser cleanup failed: " + "; ".join(cleanup_errors))
