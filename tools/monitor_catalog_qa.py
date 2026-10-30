"""Catalog discovery acceptance against actual disposable broker/browser assets.

Healthy catalog responses always come from the broker. Fault gates delay decoding
or corrupt a real response solely to test bounded retries and generation fences.
"""
import argparse
from contextlib import ExitStack
import hashlib
import json
import os
from pathlib import Path
import shutil
import socket
import subprocess
import time
from urllib.parse import urlsplit
import uuid


def catalog_browser_checks(*, page, context, rpc, state, url, token, record, output):
    from playwright.sync_api import expect

    origin = url.rstrip("/")
    results, requests = [], []
    board = "catalog-home"
    body = ("# Catalog fixture\n\n- [ ] Keep checkbox\n\n"
            "<!-- board-input: answer -->\n> original\n<!-- /board-input -->\n\n"
            + "Long document content\n\n" * 120)
    rpc(state, "register", agent={"name": "catalog-agent", "provider": "inbox",
        "session": str(uuid.uuid4()), "endpoint": "", "enabled": True})
    rpc(state, "board_create", board=board)
    before = rpc(state, "named_board_read", board=board)
    seeded = rpc(state, "named_board_update", board=board, expected_revision=before["revision"],
                 edits=[{"op": "set", "markdown": body}])
    rpc(state, "add_board_message_input", agent="catalog-agent", board=board,
        expected_revision=seeded["board"]["revision"], id="compose", label="Catalog message")
    fixture = rpc(state, "named_board_read", board=board)["markdown"]

    def watch(request):
        requests.append((request.method, urlsplit(request.url).path))
    page.on("request", watch)

    def wait(label, check, seconds=10):
        until = time.monotonic() + seconds
        while time.monotonic() < until:
            if check():
                return
            page.wait_for_timeout(30)
        raise AssertionError("Timed out: " + label)

    def names():
        return page.locator("#board-selector option").evaluate_all("rows => rows.map(row => row.value)")

    def create(name):
        rpc(state, "board_create", board=name)
        snapshot = rpc(state, "named_board_read", board=name)
        rpc(state, "named_board_update", board=name, expected_revision=snapshot["revision"],
            edits=[{"op": "set", "markdown": "# " + name + "\nDiscovered content\n"}])

    def discover(name, seconds=5):
        started = time.monotonic()
        create(name)
        wait("automatic option " + name, lambda: name in names(), seconds)
        assert time.monotonic() - started <= seconds, "Healthy discovery exceeded five seconds"

    def login():
        page.locator("#access-token").fill(token)
        page.locator("#login-form button[type=submit]").click()
        expect(page.locator("#boards-view")).to_be_visible()
        wait("initial catalog", lambda: board in names())

    def reset():
        accept = lambda dialog: dialog.accept()
        page.on("dialog", accept)
        try:
            page.goto(origin, wait_until="domcontentloaded")
        finally:
            page.remove_listener("dialog", accept)
        snapshot = rpc(state, "named_board_read", board=board)
        rpc(state, "named_board_update", board=board, expected_revision=snapshot["revision"],
            edits=[{"op": "set", "markdown": fixture}])
        login()
        page.locator("#board-selector").select_option(board)
        expect(page.locator("#selected-board-name")).to_have_text(board)
        expect(page.locator("#board-raw")).to_have_text(fixture)
        expect(page.locator(".message-composer[data-field-id=compose] textarea")).to_be_visible()

    def gate(path, mode="hold"):
        page.evaluate("""({path, mode}) => {
          window.__catalogOriginalFetch ||= window.fetch;
          if (!window.__catalogGateInstalled) {
            window.__catalogGateInstalled = true;
            window.fetch = async (...args) => {
              const gate = window.__catalogGate;
              const matches = gate?.armed && new URL(String(args[0]), location.href).pathname === gate.path;
              if (matches) gate.armed = false;
              const response = await window.__catalogOriginalFetch(...args);
              if (matches) {
                const decode = response.json.bind(response);
                response.json = async () => {
                  const value = await decode(); gate.decoded = true;
                  await new Promise(resolve => gate.release = resolve);
                  if (gate.mode === 'drop') throw new Error('Injected lost decoded response');
                  if (gate.mode === 'malformed') return {boards:[...value.boards,{name:'../invalid'}]};
                  return value;
                };
              }
              return response;
            };
          }
          window.__catalogGate = {path, mode, armed:true, decoded:false};
        }""", {"path": path, "mode": mode})

    def release():
        page.evaluate("window.__catalogGate?.release?.()")

    def request_catalog():
        page.evaluate("window.dispatchEvent(new Event('online'))")

    def case_discovery():
        page.locator("#board-selector").focus()
        page.evaluate("""() => {
          window.__catalogOptions = new Map(Array.from(document.querySelector('#board-selector').options, o => [o.value,o]));
          window.__catalogDocument = document.querySelector('#board-document').firstChild;
        }""")
        for name in ("catalog-zeta", "catalog-alpha", "catalog-mid"):
            discover(name)
        actual = names()
        assert actual == sorted(set(actual))
        assert page.locator("#board-selector").input_value() == board
        expect(page.locator("#board-selector")).to_be_focused()
        assert page.evaluate("""() => Array.from(window.__catalogOptions).every(([name,old]) =>
          Array.from(document.querySelector('#board-selector').options).find(o => o.value===name)===old)
          && window.__catalogDocument===document.querySelector('#board-document').firstChild""")
        page.locator("#board-selector").select_option("catalog-alpha")
        expect(page.locator("#board-document")).to_contain_text("Discovered content")
        expect(page.locator("#selected-board-name")).to_have_text("catalog-alpha")

    def open_response():
        # Both UIs activate the same revision-bound editor; never bypass the UI
        # or weaken the draft/pending-save assertions for the inline layout.
        inline = page.locator('.board-inline-response[data-field-id="answer"] .board-inline-text')
        if inline.count():
            expect(inline).to_have_count(1)
            expect(inline).to_be_visible()
            inline.focus()
        else:
            page.get_by_role("button", name="Respond: answer", exact=True).click()
        response = page.locator("#response-text")
        expect(response).to_be_visible()
        expect(response).to_be_focused()
        return response

    def case_drafts():
        response = open_response()
        message = page.locator(".message-composer[data-field-id=compose] textarea")
        message.fill("Independent unsent message")
        response.fill("Retained ordinary response")
        response.evaluate("""e => { e.focus(); e.setSelectionRange(3,9); window.scrollTo(0,500);
          window.__catalogDraft=e; window.__catalogScroll=scrollY;
          window.__catalogDocument=document.querySelector('#board-document').firstChild; }""")
        start = len(requests)
        discover("catalog-response-draft")
        expect(response).to_have_value("Retained ordinary response")
        expect(message).to_have_value("Independent unsent message")
        expect(response).to_be_focused()
        assert response.evaluate("e => e===window.__catalogDraft && e.selectionStart===3 && e.selectionEnd===9")
        assert page.evaluate("scrollY===window.__catalogScroll && window.__catalogDocument===document.querySelector('#board-document').firstChild")
        message.focus()
        message.evaluate("e => e.setSelectionRange(4,8)")
        discover("catalog-message-draft")
        expect(message).to_be_focused()
        assert message.evaluate("e => e.selectionStart===4 && e.selectionEnd===8")
        assert not any(method != "GET" for method, _ in requests[start:])
        assert not any(path.startswith("/api/boards/") and path not in (f"/api/boards/{board}", f"/api/boards/{board}/events") for _, path in requests[start:]), "Discovery fetched board content"

    def case_pending_response():
        open_response().fill("Pending catalog response")
        gate(f"/api/boards/{board}/responses")
        page.locator("#response-save").click()
        wait("actual committed response gate", lambda: page.evaluate("!!window.__catalogGate.decoded"))
        expect(page.locator("#response-save")).to_be_disabled()
        discover("catalog-pending-response")
        expect(page.locator("#response-text")).to_have_value("Pending catalog response")
        release()
        expect(page.locator("#response-editor")).to_be_hidden()

    def case_pending_message():
        message = page.locator(".message-composer[data-field-id=compose] textarea")
        original = rpc(state, "named_board_read", board=board)
        message.fill("Pending catalog message")
        gate(f"/api/boards/{board}/messages")
        message.press("Enter")
        wait("actual accepted message gate", lambda: page.evaluate("!!window.__catalogGate.decoded"))
        discover("catalog-pending-message")
        expect(message).to_have_value("Pending catalog message")
        expect(message).to_be_focused()
        assert rpc(state, "named_board_read", board=board) == original
        release()
        expect(message).to_have_value("")
        expect(message).to_be_focused()
        rows = rpc(state, "history", agent=None, limit=100)
        assert sum(row["body"] == "Pending catalog message" for row in rows) == 1

    def case_uncertain_and_refresh():
        message = page.locator(".message-composer[data-field-id=compose] textarea")
        message.fill("Catalog uncertain message")
        gate(f"/api/boards/{board}/messages", "drop")
        message.press("Enter")
        wait("accepted message before lost receipt", lambda: page.evaluate("!!window.__catalogGate.decoded"))
        release()
        expect(page.locator(".message-feedback")).to_contain_text("Acceptance is uncertain")
        discover("catalog-uncertain")
        expect(message).to_have_value("Catalog uncertain message")
        # Explicit Refresh catches up the catalog without retrying the message.
        create("catalog-explicit")
        page.locator("#board-refresh").click()
        wait("explicit catalog", lambda: "catalog-explicit" in names())
        expect(message).to_have_value("Catalog uncertain message")
        rows = rpc(state, "history", agent=None, limit=100)
        assert sum(row["body"] == "Catalog uncertain message" for row in rows) == 1

    def case_foreground():
        create("catalog-foreground")
        page.evaluate("""() => { Object.defineProperty(document,'visibilityState',{configurable:true,value:'visible'});
          document.dispatchEvent(new Event('visibilitychange')); }""")
        wait("foreground catchup", lambda: "catalog-foreground" in names(), 2)
        create("catalog-online")
        request_catalog()
        wait("online catchup", lambda: "catalog-online" in names(), 2)

    def case_late_view():
        create("catalog-retired")
        gate("/api/boards")
        request_catalog()
        wait("held actual catalog", lambda: page.evaluate("!!window.__catalogGate.decoded"))
        page.locator("#messages-tab").click()
        expect(page.locator("#boards-view")).to_be_hidden()
        Path(rpc(state, "named_board_read", board="catalog-retired")["path"]).unlink()
        start = sum(path == "/api/boards" for _, path in requests)
        page.wait_for_timeout(4200)
        assert sum(path == "/api/boards" for _, path in requests) == start
        page.locator("#boards-tab").click()
        wait("fresh reentry catalog", lambda: "catalog-retired" not in names())
        release()
        page.wait_for_timeout(100)
        assert "catalog-retired" not in names(), "Late old-view catalog replaced current options"

    def case_late_lock():
        gate("/api/boards")
        request_catalog()
        wait("held authenticated catalog", lambda: page.evaluate("!!window.__catalogGate.decoded"))
        page.locator("#lock-button").click()
        expect(page.locator("#workspace")).to_be_hidden()
        assert names() == []
        release()
        page.wait_for_timeout(100)
        assert names() == [], "Old private options returned after lock"
        login()
        discover("catalog-new-auth")

    def case_missing_selected():
        create("catalog-missing")
        request_catalog()
        wait("missing fixture initially listed", lambda: "catalog-missing" in names())
        page.locator("#board-selector").select_option("catalog-missing")
        expect(page.locator("#selected-board-name")).to_have_text("catalog-missing")

        Path(rpc(state, "named_board_read", board="catalog-missing")["path"]).unlink()
        request_catalog()
        expect(page.locator("#board-catalog-feedback")).to_contain_text("no longer listed")
        assert "catalog-missing" in names()
        assert page.locator("#board-selector").input_value() == "catalog-missing"
        expect(page.locator("#selected-board-name")).to_have_text("catalog-missing")

    def case_failure_clock():
        # Fake only the browser clock/fault boundary, never healthy broker data.
        with context.browser.new_context() as timed:
            p = timed.new_page()
            def twait(expression, arg=None):
                until = time.monotonic() + 10
                while time.monotonic() < until:
                    if p.evaluate(expression, arg):
                        return
                    p.wait_for_timeout(20)
                raise AssertionError("Timed clock predicate: " + expression)
            p.clock.install(time=0)
            p.clock.pause_at(1000)
            p.goto(origin, wait_until="domcontentloaded")
            p.evaluate("""() => {
              const original = window.fetch;
              window.__catMode = 'pass';
              window.__catStats = {starts:[],completed:0,flight:0,max:0,aborted:0};
              window.fetch = async (input, options={}) => {
                if (new URL(String(input),location.href).pathname!='/api/boards') return original(input,options);
                const s=window.__catStats, mode=window.__catMode;
                s.starts.push(Date.now()); s.flight++; s.max=Math.max(s.max,s.flight);
                try {
                  const response=await original(input,options), decode=response.json.bind(response);
                  response.json=async () => {
                    try {
                      const value=await decode();
                      if (mode==='error') throw new Error('Injected catalog transport failure');
                      if (mode==='empty') return {boards:[]};
                      if (mode==='missing-main') return {boards:value.boards.filter(row=>row.name!=='main')};
                      if (mode==='malformed') return {boards:[...value.boards,{name:'../invalid'}]};
                      if (mode==='duplicate') return {boards:[...value.boards,{...value.boards[0],exists:!value.boards[0].exists}]};
                      if (mode==='oversized') return {boards:Array(257).fill(value.boards[0])};
                      if (mode==='hang') await new Promise((resolve,reject) => {
                        options.signal.addEventListener('abort',()=>{s.aborted++;reject(new DOMException('Aborted','AbortError'));},{once:true});
                      });
                      return value;
                    } finally { s.flight--; s.completed++; }
                  };
                  return response;
                } catch(error) { s.flight--; s.completed++; throw error; }
              };
            }""")
            p.locator("#access-token").fill(token)
            p.locator("#login-form button[type=submit]").click()
            expect(p.locator(f'#board-selector option[value="{board}"]')).to_have_count(1)
            twait("window.__catStats.flight===0")
            # Coalesced live-connection catch-up is allowed; settle it first.
            p.clock.run_for(1)
            twait("window.__catStats.flight===0")
            known = p.locator("#board-selector option").evaluate_all("rows=>rows.map(o=>o.value)")
            p.evaluate("window.__catStats={starts:[],completed:0,flight:0,max:0,aborted:0}")
            p.clock.run_for(3998)
            assert p.evaluate("window.__catStats.starts.length") == 0
            p.clock.run_for(2)
            twait("window.__catStats.completed===1")
            assert p.evaluate("window.__catStats.starts.length") == 1
            p.evaluate("window.__catMode='error'")
            p.clock.run_for(4000)
            twait("window.__catStats.completed===2")
            expect(p.locator("#board-catalog-feedback")).to_contain_text("Known boards are kept")
            completed = 2
            for delay in (2000, 4000, 8000, 16000, 30000, 30000):
                count = p.evaluate("window.__catStats.starts.length")
                p.clock.run_for(delay - 1)
                assert p.evaluate("window.__catStats.starts.length") == count
                p.clock.run_for(1)
                completed += 1
                twait("n=>window.__catStats.completed===n", arg=completed)
                assert p.evaluate("window.__catStats.starts.length") == count + 1
            p.locator("#board-selector").focus()
            p.evaluate("window.__catOptions=new Map(Array.from(document.querySelector('#board-selector').options,o=>[o.value,o]))")
            for mode in ("malformed", "duplicate", "oversized", "empty", "missing-main"):
                p.evaluate("mode=>{window.__catMode=mode;window.dispatchEvent(new Event('online'));}", mode)
                completed += 1
                twait("n=>window.__catStats.completed===n", arg=completed)
                assert p.locator("#board-selector option").evaluate_all("rows=>rows.map(o=>o.value)") == known
                assert p.evaluate("""() => Array.from(window.__catOptions).every(([name,old]) =>
                  Array.from(document.querySelector('#board-selector').options).find(o=>o.value===name)===old)""")
                expect(p.locator("#board-selector")).to_have_value("main")
                expect(p.locator("#board-selector")).to_be_focused()
            p.evaluate("window.__catMode='pass';window.dispatchEvent(new Event('online'))")
            completed += 1
            twait("n=>window.__catStats.completed===n", arg=completed)
            expect(p.locator("#board-catalog-feedback")).to_be_hidden()
            p.evaluate("window.__catMode='hang';window.dispatchEvent(new Event('online'))")
            twait("window.__catStats.flight===1")
            count = p.evaluate("window.__catStats.starts.length")
            p.evaluate("for(let i=0;i<20;i++)window.dispatchEvent(new Event('online'))")
            p.clock.run_for(3999)
            assert p.evaluate("window.__catStats.starts.length") == count
            p.evaluate("window.__catMode='pass'")
            p.clock.run_for(2)
            twait("window.__catStats.aborted===1 && window.__catStats.flight===0")
            assert p.evaluate("window.__catStats.max") == 1
            assert p.evaluate("window.__catStats.starts.length") == count + 1
            p.locator("#messages-tab").click()
            count = p.evaluate("window.__catStats.starts.length")
            p.clock.run_for(120000)
            assert p.evaluate("window.__catStats.starts.length") == count

    try:
        for label, scenario in [
            ("automatic_sorted_keyed_selection", case_discovery),
            ("independent_response_message_drafts", case_drafts),
            ("pending_response", case_pending_response),
            ("pending_message", case_pending_message),
            ("uncertain_explicit_refresh", case_uncertain_and_refresh),
            ("foreground_online", case_foreground),
            ("late_view_and_stopped_discovery", case_late_view),
            ("late_lock_auth", case_late_lock),
            ("missing_selected", case_missing_selected),
            ("cadence_failure_backoff_timeout_singleflight", case_failure_clock),
        ]:
            ready = False
            try:
                reset(); ready = True
                scenario()
                results.append({"case": label, "status": "passed"})
                record("catalog " + label)
            except Exception as error:
                results.append({"case": label, "status": "failed" if ready else "blocked", "error": str(error)})
                print("FAILED: catalog " + label + ": " + str(error), flush=True)
            finally:
                release()
                (output / "catalog-results.json").write_text(json.dumps(results, indent=2) + "\n")
        assert not [row for row in results if row["status"] != "passed"], json.dumps(results, indent=2)
    finally:
        page.remove_listener("request", watch)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()
    output = args.output.resolve(); output.mkdir(parents=True, mode=0o700, exist_ok=True)
    binary = output / "frozen-agent-bridge"
    shutil.copy2(args.binary.resolve(strict=True), binary); binary.chmod(0o700)
    result = {"complete": False, "binary_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
              "checks": [], "cleanup": {"escalated": False, "reaped": False, "exit": None, "errors": []}}
    state = output / "state"
    with (output / "broker.log").open("wb") as log, ExitStack() as cleanup:
        def write_result():
            if not result["cleanup"]["reaped"] or result["cleanup"]["errors"]:
                result["complete"] = False
            (output / "results.json").write_text(json.dumps(result, indent=2) + "\n")
        cleanup.callback(write_result)
        broker = subprocess.Popen([str(binary), "--state-dir", str(state), "serve", "--no-discovery", "--codex-bin", "/missing/codex"], stdout=log, stderr=log)
        def stop():
            try:
                if broker.poll() is None:
                    broker.terminate()
                try:
                    broker.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    result["cleanup"]["escalated"] = True
                    broker.kill(); broker.wait(timeout=5)
            except Exception as error:
                result["cleanup"]["errors"].append(str(error))
                raise
            finally:
                result["cleanup"]["exit"] = broker.poll()
                result["cleanup"]["reaped"] = broker.poll() is not None
        cleanup.callback(stop)
        try:
            until = time.monotonic() + 10
            while not (state / "bridge.sock").exists():
                assert broker.poll() is None and time.monotonic() < until
                time.sleep(.04)
            def rpc(_state, op, **values):
                with socket.socket(socket.AF_UNIX) as connection:
                    connection.settimeout(10); connection.connect(str(state / "bridge.sock"))
                    connection.sendall(json.dumps({"op": op, **values}).encode() + b"\n")
                    data = bytearray()
                    while b"\n" not in data:
                        part = connection.recv(262144); assert part; data.extend(part)
                    value = json.loads(data.split(b"\n", 1)[0]); assert value["ok"], value
                    return value["result"]
            access = rpc(state, "monitor_enable", port=0)
            from playwright.sync_api import sync_playwright
            with sync_playwright() as playwright:
                home = output / "browser-home"; home.mkdir(mode=0o700)
                browser = playwright.chromium.launch(headless=True, env={"HOME": str(home), "TMPDIR": str(home), "PATH": os.environ.get("PATH", "/usr/bin:/bin"), "LANG": "C.UTF-8"})
                try:
                    with browser.new_context(viewport={"width": 1200, "height": 900}) as context:
                        page = context.new_page()
                        errors = []; page.on("pageerror", lambda error: errors.append(str(error)))
                        def record(message):
                            result["checks"].append(message); print("PASS: " + message, flush=True)
                        catalog_browser_checks(page=page, context=context, rpc=rpc, state=state,
                            url=access["url"], token=access["access_token"], record=record, output=output)
                        assert not errors, errors
                finally:
                    browser.close()
            result["complete"] = True
        except BaseException:
            result["complete"] = False
            raise
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
