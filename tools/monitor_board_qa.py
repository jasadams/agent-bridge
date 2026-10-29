"""Real monitor board browser acceptance; called by disposable test_monitor.py.

Backend responses are never fabricated. Gates delay decoding an actual response,
and the uncertainty case commits a real POST before dropping its response.
"""
import json
import time
from pathlib import Path
from urllib.parse import urlsplit


def board_browser_checks(*, page, browser, context, rpc, state, second_state,
                         url, token, record, output):
    from playwright.sync_api import expect

    origin = url.rstrip("/")
    posts, unexpected = [], []
    page.on("request", lambda request: posts.append(request.post_data)
            if request.method == "POST" and "/responses" in request.url else None)

    def guard(route):
        request = route.request
        path = urlsplit(request.url).path
        if request.url.startswith(origin + "/") and (path in ("/", "/app.js", "/boards.js", "/style.css")
                or path.startswith("/api/")):
            route.continue_()
        else:
            unexpected.append(request.url)
            route.abort()

    page.route("**/*", guard)

    def read(name):
        return rpc(state, "named_board_read", board=name)

    def write(name, markdown):
        current = read(name)
        return rpc(state, "named_board_update", board=name,
                   expected_revision=current["revision"],
                   edits=[{"op": "set", "markdown": markdown}], actor=None)

    def wait(label, check, seconds=15):
        deadline = time.monotonic() + seconds
        while time.monotonic() < deadline:
            if check():
                return
            page.wait_for_timeout(80)
        raise AssertionError("Timed out: " + label)

    def api_view(name):
        response = context.request.get(origin + "/api/boards/" + name,
                                       headers={"Authorization": "Bearer " + token})
        assert response.ok, response.status
        return response.json()

    def pick(name):
        if page.locator("#boards-view").is_hidden():
            page.locator("#boards-tab").click()
        page.locator("#board-selector").select_option(name)
        expect(page.locator("#selected-board-name")).to_have_text(name)
        wait("selected board snapshot", lambda: page.locator("#board-raw").text_content() == read(name)["markdown"])
        wait("board controls loaded", lambda: page.locator("#board-controls .board-control").count() > 0)

    def edit(value):
        page.get_by_role("button", name="Respond: answer", exact=True).click()
        page.locator("#response-text").fill(value)

    def saved(value):
        wait("response persisted", lambda: "> " + value in read("alpha")["markdown"])
        expect(page.locator("#response-editor")).to_be_hidden()

    def refresh():
        page.locator("#board-refresh").click()
        expect(page.locator("#board-error")).to_be_hidden()

    def install_gate(target=page):
        target.evaluate("""() => {
          if (window.__boardOriginalFetch) return;
          window.__boardOriginalFetch = window.fetch;
          window.fetch = async (input, options = {}) => {
            const gate = window.__boardGate;
            const url = new URL(typeof input === 'string' ? input : input.url, location.href);
            const match = gate && gate.armed && (options.method || 'GET') === gate.method && url.pathname === gate.path;
            if (match) { gate.armed = false; url.searchParams.delete('if_revision'); input = url.pathname + url.search; }
            const response = await window.__boardOriginalFetch(input, options);
            if (!match) return response;
            const json = response.json.bind(response);
            response.json = async () => {
              const actual = await json();
              gate.waiting = true;
              await new Promise(resolve => { gate.release = resolve; });
              return actual;
            };
            return response;
          };
        }""")

    def arm_gate(name, target=page, method="GET"):
        install_gate(target)
        path = "/api/boards/" + name + ("/responses" if method == "POST" else "")
        target.evaluate("spec => { window.__boardGate = {...spec, armed:true, waiting:false}; }",
                        {"path": path, "method": method})
        if method == "GET":
            target.locator("#board-refresh").click()

    def gate_wait(target=page):
        deadline = time.monotonic() + 15
        while time.monotonic() < deadline:
            if target.evaluate("() => Boolean(window.__boardGate?.waiting)"):
                return
            target.wait_for_timeout(80)
        raise AssertionError("Timed out waiting for actual response JSON gate")

    def release_gate(target=page):
        target.evaluate("() => { window.__boardGate?.release?.(); }")

    alpha = ("# Alpha 世界\r\n\r\n| Name | Status |\r\n| --- | --- |\r\n| native | ready |\r\n\r\n"
             "- [ ] Checkbox alpha\r\n\r\n<!-- board-input: answer -->\r\n> original\r\n<!-- /board-input -->\r\n"
             "Tail unchanged.\r\n\r\n```text\r\n- [ ] example only\r\n<!-- board-input: example -->\r\n> code\r\n<!-- /board-input -->\r\n```\r\n"
             "![unfetched image](https://example.invalid/private.png)\r\n"
             "<img src='/unexpected-board-resource' onerror='window.boardPwned=1'>\r\n"
             "<script>window.boardPwned=2</script>\r\n"
             "[unsafe](javascript:alert(1))\r\n[explicit safe link](https://example.invalid/manual)\r\n")
    beta = ("# Beta\n- [ ] Checkbox beta\n\n<!-- board-input: answer -->\n> beta response\n<!-- /board-input -->\n"
            "<!-- board-input: duplicate -->\n> first\n<!-- /board-input -->\n"
            "<!-- board-input: duplicate -->\n> second\n<!-- /board-input -->\n"
            "ordinary [ ] brackets\n")
    try:
        assert not read("main")["exists"], "Fresh monitor fixture should retain virtual absent main"
        for name, markdown in (("alpha", alpha), ("beta", beta)):
            rpc(state, "board_create", board=name)
            write(name, markdown)
        before = {name: read(name) for name in ("main", "alpha", "beta")}
        page.locator("#boards-tab").click()
        expect(page.locator("#selected-board-name")).to_have_text("main")
        expect(page.locator("#board-document")).to_contain_text("has not been written")
        expect(page.locator(".board-responses")).to_be_hidden()
        names = page.locator("#board-selector option").all_text_contents()
        assert names == sorted(names) and names == ["alpha", "beta", "main"]
        pick("alpha")
        expect(page.locator("#board-document h1")).to_have_text("Alpha 世界")
        expect(page.locator("#board-document table")).to_have_count(1)
        expect(page.locator("#board-document pre")).to_have_count(1)
        expect(page.locator("#board-controls .board-control")).to_have_count(2)
        assert len(api_view("beta")["controls"]) == 2, "Duplicate text markers became editable"
        assert not page.locator("#board-document img, #board-document script, #board-document svg, #board-document iframe").count()
        assert page.evaluate("typeof window.boardPwned") == "undefined"
        assert not page.locator('#board-document a[href^="javascript:"], #board-document a[href^="data:"]').count()
        page.locator("#board-raw-toggle").click()
        assert page.locator("#board-raw").text_content() == alpha
        page.locator("#board-raw-toggle").click()
        for name in before:
            assert read(name) == before[name], "Loading or rendering wrote the board"
        record("boards list virtual main and named documents deterministically; safe Markdown/raw/control metadata render without writes/resources")

        # Read-only documents should have the same space as editable boards,
        # without reserving a column or displaying an empty Responses panel.
        write("main", "# Ticket board\n\n| Ticket | Status |\n| --- | --- |\n| AGB-12 | Done |\n")
        page.locator("#board-selector").select_option("main")
        expect(page.locator("#board-document h1")).to_have_text("Ticket board")
        expect(page.locator(".board-responses")).to_be_hidden()
        page.set_viewport_size({"width": 1440, "height": 1000})
        page.evaluate("window.scrollTo(0, 0)")
        document = page.locator(".board-document-panel").bounding_box()
        board_view = page.locator("#boards-view").bounding_box()
        assert document and board_view
        assert document["y"] < 230, "Board content is pushed below excessive header space"
        assert abs(document["width"] - board_view["width"]) <= 2, "Board loses width to an unused sidebar"
        page.screenshot(path=str(output / "boards-read-only-desktop.png"), full_page=True)
        pick("alpha")
        expect(page.locator(".board-responses")).to_be_visible()
        record("read-only board opens near the top at full content width without empty response chrome; editable controls return on navigation")

        snapshot_reads = []
        page.on("request", lambda request: snapshot_reads.append(request.url)
                if request.method == "GET" and urlsplit(request.url).path == "/api/boards/alpha" else None)
        page.wait_for_timeout(1000)
        idle_count = len(snapshot_reads)
        page.wait_for_timeout(1800)
        assert len(snapshot_reads) == idle_count, "Idle board still polls snapshots"
        page.evaluate("""() => {
          window.__stableHeading = document.querySelector('#board-document h1');
          window.__stableControl = document.querySelector('#board-controls input');
        }""")
        write("alpha", alpha.replace("| native | ready |", "| native | live push |"))
        expect(page.locator("#board-document table")).to_contain_text("live push")
        assert page.evaluate("window.__stableHeading === document.querySelector('#board-document h1')")
        assert page.evaluate("window.__stableControl === document.querySelector('#board-controls input')")
        write("alpha", alpha)
        expect(page.locator("#board-document table")).to_contain_text("ready")
        page.evaluate("""() => {
          const range = document.createRange(); range.selectNodeContents(document.querySelector('#board-document h1'));
          const selection = getSelection(); selection.removeAllRanges(); selection.addRange(range);
        }""")
        write("alpha", alpha.replace("| native | ready |", "| native | transient |"))
        expect(page.locator("#board-newer")).to_be_visible()
        write("alpha", alpha)
        expect(page.locator("#board-newer")).to_be_hidden()
        page.evaluate("getSelection().removeAllRanges()")
        expect(page.locator("#board-document table")).to_contain_text("ready")
        assert "transient" not in page.locator("#board-document table").inner_text()
        record("board push stream stays idle without snapshot polling, patches changed cells, preserves controls and drops reverted deferred snapshots")

        checkbox = page.locator('#board-controls input[type="checkbox"]')
        checkbox.check()
        wait("desired checkbox checked", lambda: "- [x] Checkbox alpha" in read("alpha")["markdown"])
        expect(checkbox).to_be_enabled()
        assert read("alpha")["markdown"] == alpha.replace("- [ ] Checkbox alpha", "- [x] Checkbox alpha", 1)
        edit("Saved browser response 世界")
        page.locator("#response-save").click()
        saved("Saved browser response 世界")
        assert read("alpha")["markdown"] == alpha.replace("- [ ] Checkbox alpha", "- [x] Checkbox alpha", 1).replace("> original\r\n", "> Saved browser response 世界\r\n", 1)
        record("browser desired checkbox and explicit text saves preserve unrelated Unicode/CRLF Markdown byte-for-byte via actual broker")

        # All application-controlled exits offer an explicit draft decision.
        for destination in ("board", "messages", "lock"):
            edit("keep draft " + destination)
            page.evaluate("location.hash = 'access_token=' + 'x'.repeat(43)")
            wait("active-session fragment cleared", lambda: not urlsplit(page.url).fragment)
            expect(page.locator("#response-text")).to_have_value("keep draft " + destination)
            if destination == "board": page.locator("#board-selector").select_option("beta")
            elif destination == "messages": page.locator("#messages-tab").click()
            else: page.locator("#lock-button").click()
            expect(page.locator("#unsaved-dialog")).to_be_visible()
            page.locator("#unsaved-cancel").click()
            expect(page.locator("#response-text")).to_have_value("keep draft " + destination)
            expect(page.locator("#selected-board-name")).to_have_text("alpha")
            page.locator("#response-cancel").click()
        edit("discard draft")
        page.locator("#board-selector").select_option("beta"); page.locator("#unsaved-discard").click()
        expect(page.locator("#selected-board-name")).to_have_text("beta")
        pick("alpha"); edit("save before navigation")
        page.locator("#messages-tab").click(); page.locator("#unsaved-save").click()
        expect(page.locator("#messages-view")).to_be_visible(); saved("save before navigation")
        pick("alpha")
        record("dirty board/view/lock cancellation preserves drafts; explicit discard and successful save-before-navigation work")

        # DOM selection and text/caret remain frozen through external-file updates.
        page.evaluate("""() => { const r=document.createRange();r.selectNodeContents(document.querySelector('#board-document h1'));const s=getSelection();s.removeAllRanges();s.addRange(r); }""")
        selected = page.evaluate("getSelection().toString()")
        source = read("alpha")["markdown"]
        Path(state / "boards" / "alpha.md").write_text(source + "External selection update\n", newline="")
        expect(page.locator("#board-newer")).to_be_visible(timeout=15000)
        assert page.evaluate("getSelection().toString()") == selected
        assert "External selection update" not in page.locator("#board-document").inner_text()
        page.locator("#board-review-refresh").click()
        expect(page.locator("#board-document")).to_contain_text("External selection update")
        edit("draft stays stable")
        page.locator("#response-text").evaluate("e => e.setSelectionRange(3,3)")
        Path(state / "boards" / "alpha.md").write_text(read("alpha")["markdown"] + "External draft update\n", newline="")
        expect(page.locator("#board-newer")).to_be_visible(timeout=15000)
        expect(page.locator("#response-text")).to_have_value("draft stays stable")
        assert page.locator("#response-text").evaluate("e => e.selectionStart") == 3
        page.locator("#response-cancel").click()
        expect(page.locator("#board-document")).to_contain_text("External draft update")
        record("selected-board conditional refresh sees external file edits without message activity and freezes text selection/draft/caret")

        edit("rebased response")
        write("alpha", read("alpha")["markdown"] + "Conflict B\n")
        page.locator("#response-save").click()
        expect(page.locator("#response-review")).to_be_visible()
        page.locator("#response-review").click(); expect(page.locator("#response-rebase")).to_be_visible()
        # A newer C must invalidate the exact B review, even if the field value is unchanged.
        write("alpha", read("alpha")["markdown"] + "Unseen conflict C\n")
        expect(page.locator("#response-rebase")).to_be_hidden(timeout=15000)
        expect(page.locator("#response-text")).to_have_value("rebased response")
        page.locator("#response-review").click(); expect(page.locator("#response-rebase")).to_be_visible()
        page.locator("#response-rebase").click(); page.locator("#response-save").click(); saved("rebased response")
        edit("must not overwrite changed field")
        write("alpha", read("alpha")["markdown"].replace("> rebased response\r\n", "> agent changed\r\n"))
        page.locator("#response-save").click(); expect(page.locator("#response-review")).to_be_visible()
        page.locator("#response-review").click(); expect(page.locator("#response-rebase")).to_be_hidden()
        expect(page.locator("#response-save")).to_be_disabled(); page.locator("#response-cancel").click()
        edit("deleted field draft")
        current = read("alpha")["markdown"]
        start = current.index("<!-- board-input: answer -->")
        end = current.index("<!-- /board-input -->", start) + len("<!-- /board-input -->")
        write("alpha", current[:start] + current[end:])
        page.locator("#response-save").click(); expect(page.locator("#response-review")).to_be_visible()
        page.locator("#response-review").click(); expect(page.locator("#response-rebase")).to_be_hidden()
        expect(page.locator("#response-text")).to_have_value("deleted field draft"); page.locator("#response-cancel").click()
        write("alpha", alpha); refresh()
        expect(page.get_by_role("button", name="Respond: answer", exact=True)).to_be_visible()
        record("revision conflicts retain intent; explicit unchanged-field rebase is revision-reviewed; changed/deleted fields require reopening")

        # The real POST commits before its JSON continuation is released. While
        # pending, duplicate clicks and navigation cannot create/cross a save.
        edit("one pending browser save")
        count = len(posts)
        arm_gate("alpha", method="POST")
        page.locator("#response-save").click(); gate_wait()
        assert "> one pending browser save" in read("alpha")["markdown"]
        expect(page.locator("#response-save")).to_be_disabled()
        expect(page.locator('#board-controls input[type="checkbox"]')).to_be_disabled()
        expect(page.get_by_role("button", name="Respond: answer", exact=True)).to_be_disabled()
        page.locator("#response-save").evaluate("button => { button.click(); button.click(); }")
        page.locator("#board-selector").select_option("beta")
        expect(page.locator("#selected-board-name")).to_have_text("alpha")
        page.locator("#messages-tab").click()
        expect(page.locator("#boards-view")).to_be_visible()
        page.locator("#lock-button").click()
        expect(page.locator("#workspace")).to_be_visible()
        expect(page.locator("#selected-board-name")).to_have_text("alpha")
        assert len(posts) == count + 1, "Pending save submitted duplicate POSTs"
        release_gate(); saved("one pending browser save")
        assert len(posts) == count + 1
        expect(page.locator("#boards-view")).to_be_visible()
        expect(page.locator("#selected-board-name")).to_have_text("alpha")
        record("actual pending POST disables duplicates and keeps board/view/lock navigation pinned until its known outcome")

        # A full pre-save GET is held after actual JSON decoding, then released
        # after the real POST succeeds. It must never restore old content/revision.
        edit("fresh save beats old GET")
        arm_gate("alpha"); gate_wait()
        page.locator("#response-save").click(); saved("fresh save beats old GET")
        release_gate(); page.wait_for_timeout(150)
        page.locator("#board-raw-toggle").click()
        assert "fresh save beats old GET" in page.locator("#board-raw").text_content()
        page.locator("#board-raw-toggle").click()
        record("a delayed actual pre-save GET JSON continuation cannot overwrite a successful save")

        edit("committed uncertain response")
        count = len(posts)
        def lose_response(route):
            actual = route.fetch()
            assert actual.status == 200, actual.status
            route.abort("failed")
        pattern = "**/api/boards/alpha/responses"
        page.route(pattern, lose_response, times=1)
        page.locator("#response-save").click()
        expect(page.locator("#response-feedback")).to_contain_text("may have applied")
        expect(page.locator("#response-save")).to_be_disabled()
        wait("real uncertain commit", lambda: "> committed uncertain response" in read("alpha")["markdown"])
        page.wait_for_timeout(1300); assert len(posts) == count + 1, "Uncertain POST was replayed automatically"
        page.locator("#response-review").click(); expect(page.locator("#response-rebase")).to_be_hidden()
        expect(page.locator("#response-text")).to_have_value("committed uncertain response")
        page.locator("#response-cancel").click()
        record("real committed POST with lost response retains intent, requires readback/review and never automatically retries")

        arm_gate("alpha"); gate_wait()
        page.locator("#board-selector").select_option("beta")
        expect(page.locator("#selected-board-name")).to_have_text("beta")
        expect(page.locator("#board-document h1")).to_have_text("Beta")
        release_gate(); page.wait_for_timeout(150)
        expect(page.locator("#board-document h1")).to_have_text("Beta")
        # Gate a decoded beta snapshot, lock, authenticate a fresh generation,
        # then release the old result. Private old data must remain cleared.
        arm_gate("beta"); gate_wait()
        page.locator("#lock-button").click(); expect(page.locator("#workspace")).to_be_hidden()
        assert not page.locator("#board-raw").text_content() and not page.locator("#board-controls").text_content()
        assert not page.locator("#response-text").input_value()
        page.locator("#access-token").fill(token); page.locator("#login-form button[type=submit]").click()
        expect(page.locator("#boards-view")).to_be_visible()
        expect(page.locator("#selected-board-name")).to_have_text("main")
        expect(page.locator("#board-document h1")).to_have_text("Ticket board")
        release_gate(); page.wait_for_timeout(150)
        expect(page.locator("#board-document h1")).to_have_text("Ticket board")
        assert "Beta" not in page.locator("#board-raw").text_content()
        assert not page.locator("#board-controls .board-control").count()
        record("late decoded snapshots are fenced across board switches and lock/new authentication generations")

        pick("alpha")
        # A temporary read failure retains existing private content and recovers.
        page.route("**/api/boards/alpha*", lambda route: route.abort("failed"), times=1)
        page.locator("#board-refresh").click()
        expect(page.locator("#board-status")).to_contain_text("Stale")
        expect(page.locator("#board-document h1")).to_have_text("Alpha 世界")
        expect(page.locator("#board-error")).to_be_hidden(timeout=15000)
        page.locator("#board-selector").focus(); page.keyboard.press("Tab")
        assert page.locator("#board-refresh").evaluate("e => e === document.activeElement")
        page.get_by_role("button", name="Respond: answer", exact=True).focus(); page.keyboard.press("Enter")
        expect(page.locator("#response-text")).to_be_focused()
        page.locator("#response-text").fill("keyboard draft"); page.keyboard.press("Tab")
        expect(page.locator("#response-save")).to_be_focused()
        page.locator("#messages-tab").click(); page.keyboard.press("Escape")
        expect(page.locator("#response-text")).to_have_value("keyboard draft")
        page.locator("#response-cancel").click()
        for width, height, filename in ((1440, 1000, "boards-desktop.png"), (320, 844, "boards-mobile.png")):
            page.set_viewport_size({"width": width, "height": height})
            page.evaluate("window.scrollTo(0, 0)")
            assert page.evaluate("document.documentElement.scrollWidth <= innerWidth + 1"), "Page overflow hides response controls"
            document = page.locator(".board-document-panel").bounding_box()
            responses = page.locator(".board-responses").bounding_box()
            board_view = page.locator("#boards-view").bounding_box()
            assert document and responses and board_view
            assert abs(document["width"] - board_view["width"]) <= 2, "Editable board reserves document width for responses"
            assert responses["y"] >= document["y"] + document["height"] - 1, "Responses displace the board into a sidebar layout"
            if width >= 1000:
                assert document["y"] < 230, "Editable board content is pushed below excessive header space"
            page.screenshot(path=str(output / filename), full_page=True)
        assert page.evaluate("localStorage.length + sessionStorage.length") == 0
        assert context.cookies() == []
        assert not unexpected, unexpected
        assert page.evaluate("typeof window.boardPwned") == "undefined"
        record("board reconnect, keyboard labels/focus/modal escape, desktop/mobile layout and memory-only privacy hold without resource requests")

        # Credential rotation gets its own private monitor; main credentials stay
        # unchanged for the remainder of the existing monitor acceptance harness.
        private_main = rpc(second_state, "named_board_read", board="main")
        rpc(second_state, "named_board_update", board="main",
            expected_revision=private_main["revision"], actor=None,
            edits=[{"op": "set", "markdown": "# Revoked private board\n<!-- board-input: secret -->\n> REVOKED_PRIVATE_BOARD_DATA\n<!-- /board-input -->\n"}])
        second = rpc(second_state, "monitor_enable", port=0)
        isolated = browser.new_context()
        try:
            other = isolated.new_page(); other.goto(second["url"])
            other.locator("#access-token").fill(second["access_token"])
            other.locator("#login-form button[type=submit]").click()
            expect(other.locator("#workspace")).to_be_visible()
            other.locator("#boards-tab").click()
            expect(other.locator("#board-selector option")).to_have_count(1)
            expect(other.locator("#board-raw")).to_contain_text("REVOKED_PRIVATE_BOARD_DATA")
            arm_gate("main", other); gate_wait(other)
            rpc(second_state, "monitor_disable")
            renewed = rpc(second_state, "monitor_enable", port=urlsplit(second["url"]).port)
            expect(other.locator("#workspace")).to_be_hidden(timeout=15000)
            release_gate(other)
            other.wait_for_timeout(150)
            assert not other.locator("#board-raw").text_content()
            assert not other.locator("#board-controls").text_content()
            assert other.locator("#access-token").input_value() == ""
            assert renewed["access_token"] != second["access_token"]
        finally:
            isolated.close(); rpc(second_state, "monitor_disable")
        record("actual private monitor credential rotation clears boards and invalidates delayed old-auth data")
        from monitor_code_copy_qa import code_copy_browser_checks
        code_copy_browser_checks(page=page, rpc=rpc, state=state, token=token, record=record, output=output)
        page.locator("#messages-tab").click()
        expect(page.locator("#messages-view")).to_be_visible()
    finally:
        page.unroute("**/*", guard)
        # Never leave a deliberate JSON-decode gate suspended after assertions.
        if not page.is_closed():
            page.evaluate("() => { window.__boardGate?.release?.(); }")
