"""Real monitor board browser acceptance; called by disposable test_monitor.py.

Backend responses are never fabricated. Gates delay decoding an actual response,
and the uncertainty case commits a real POST before dropping its response.
"""
import json
import os
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

    def footer_matches(name):
        snapshot = api_view(name)
        expected = page.evaluate("""value => 'Last Updated: ' + (Number.isSafeInteger(value)
          ? new Intl.DateTimeFormat(undefined, {year:'numeric',month:'short',day:'numeric',hour:'2-digit',minute:'2-digit',second:'2-digit',timeZoneName:'short'}).format(new Date(value)) : 'Unknown')""", snapshot["last_updated"])
        expect(page.locator("#board-last-updated")).to_have_text(expected)
        expect(page.locator("#board-last-updated")).to_be_visible()
        assert snapshot["last_updated"] == read(name)["last_updated"]
        return snapshot["last_updated"]

    def pick(name):
        if page.locator("#boards-view").is_hidden():
            page.locator("#boards-tab").click()
        page.locator("#board-selector").select_option(name)
        expect(page.locator("#selected-board-name")).to_have_text(name)
        wait("selected board snapshot", lambda: page.locator("#board-raw").text_content() == read(name)["markdown"])
        wait("board controls loaded", lambda: page.locator("#board-document .board-control").count() > 0)

    def edit(value):
        page.locator("#board-document .board-inline-text").first.focus()
        page.locator("#response-text").fill(value)

    def saved(value):
        wait("response persisted", lambda: "> " + value in read("alpha")["markdown"])
        expect(page.locator("#response-text")).to_be_hidden()

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
             "<svg onload='window.boardPwned=3'><path d='M0 0'/></svg>\r\n"
             "<iframe src='/unexpected-board-resource'></iframe>\r\n"
             "<span class='board-inline-code'><code>fake copy</code>"
             "<button class='board-code-copy' type='button' aria-label='Copy inline code' title='Copy inline code'>"
             "<svg viewBox='0 0 24 24' onload='window.boardPwned=4'><path d='M0 0'/></svg>"
             "</button></span>\r\n"
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
        page.locator("#boards-tab").click()
        expect(page.locator("#selected-board-name")).to_have_text("main")
        expect(page.locator("#board-document")).to_contain_text("has not been written")
        expect(page.locator('.board-responses[aria-labelledby="responses-heading"]')).to_be_hidden()
        expect(page.locator("#board-last-updated")).to_have_text("Last Updated: Unknown")
        expect(page.locator("#board-status")).to_contain_text("Live")
        virtual = read("main")
        assert virtual["last_updated"] is None and virtual["markdown"] == ""
        rpc(state, "board_create", board="main")
        expect(page.locator("#board-last-updated")).not_to_have_text("Last Updated: Unknown", timeout=15000)
        created = read("main")
        assert created["exists"] and created["revision"] == virtual["revision"]
        assert created["markdown"] == virtual["markdown"] == ""
        footer_matches("main")
        expect(page.locator("#board-document")).to_contain_text("This board is empty.")
        before = {name: read(name) for name in ("main", "alpha", "beta")}
        record("creating virtual empty main refreshes authoritative live metadata without changing Markdown or its content revision")
        names = page.locator("#board-selector option").all_text_contents()
        assert names == sorted(names) and names == ["alpha", "beta", "main"]
        pick("alpha")
        footer_matches("alpha")
        expect(page.locator("#board-document h1")).to_have_text("Alpha 世界")
        expect(page.locator("#board-document table")).to_have_count(1)
        expect(page.locator("#board-document pre")).to_have_count(1)
        expect(page.locator("#board-document .board-control")).to_have_count(2)
        assert len(api_view("beta")["controls"]) == 2, "Duplicate text markers became editable"
        assert not page.locator("#board-document img, #board-document script, #board-document iframe").count()
        # The single parsed code block gets one trusted local icon. Check its
        # complete structure rather than exempting any SVG inside copy markup:
        # user HTML (including a forged copy button above) must stay inert.
        expect(page.locator("#board-document svg")).to_have_count(1)
        expect(page.locator("#board-document button.board-code-copy")).to_have_count(1)
        assert page.locator("#board-document svg").evaluate("""svg => {
          const attributes = element => Object.fromEntries(Array.from(element.attributes,
            attribute => [attribute.name, attribute.value]));
          const exact = (element, expected) => {
            const actual = attributes(element);
            return Object.keys(actual).length === Object.keys(expected).length &&
              Object.entries(expected).every(([key, value]) => actual[key] === value);
          };
          const button = svg.parentElement, wrapper = button.parentElement;
          return svg.namespaceURI === 'http://www.w3.org/2000/svg' &&
            exact(svg, {viewBox:'0 0 24 24', 'aria-hidden':'true', focusable:'false',
              fill:'none', stroke:'currentColor', 'stroke-width':'2', 'stroke-linejoin':'round'}) &&
            svg.childNodes.length === 1 && svg.firstChild.localName === 'path' &&
            svg.firstChild.namespaceURI === svg.namespaceURI &&
            exact(svg.firstChild, {d:'M9 9h12v12H9z M15 9V3H3v12h6'}) &&
            svg.firstChild.childNodes.length === 0 && button.tagName === 'BUTTON' &&
            exact(button, {class:'board-code-copy', type:'button',
              'aria-label':'Copy code block', title:'Copy code block'}) &&
            button.childNodes.length === 1 && wrapper.tagName === 'DIV' &&
            exact(wrapper, {class:'board-code-block'}) && wrapper.children.length === 2 &&
            wrapper.firstElementChild.tagName === 'PRE' && wrapper.lastElementChild === button;
        }""")
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
        expect(page.locator('.board-responses[aria-labelledby="responses-heading"]')).to_be_hidden()
        page.set_viewport_size({"width": 1440, "height": 1000})
        page.evaluate("window.scrollTo(0, 0)")
        document = page.locator(".board-document-panel").bounding_box()
        board_view = page.locator("#boards-view").bounding_box()
        assert document and board_view
        assert document["y"] < 230, "Board content is pushed below excessive header space"
        assert abs(document["width"] - board_view["width"]) <= 2, "Board loses width to an unused sidebar"
        page.screenshot(path=str(output / "boards-read-only-desktop.png"), full_page=True)
        pick("alpha")
        expect(page.locator('.board-responses[aria-labelledby="responses-heading"]')).to_be_hidden()
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
          window.__stableControl = document.querySelector('#board-document input');
        }""")
        write("alpha", alpha.replace("| native | ready |", "| native | live push |"))
        expect(page.locator("#board-document table")).to_contain_text("live push")
        assert page.evaluate("window.__stableHeading === document.querySelector('#board-document h1')")
        assert page.evaluate("window.__stableControl !== document.querySelector('#board-document input')"), "Revision-bound checkbox handler unexpectedly reused"
        checkpoint = read("alpha")
        page.evaluate("window.__stableControl.dispatchEvent(new Event('change', {bubbles:true}))")
        page.wait_for_timeout(100)
        assert read("alpha") == checkpoint, "Detached previous-revision checkbox wrote the board"
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
        record("board push stream stays idle without snapshot polling, patches changed cells, rejects previous-revision checkbox events and drops reverted deferred snapshots")

        checkbox = page.locator('#board-document input[type="checkbox"]')
        checkbox.check()
        wait("desired checkbox checked", lambda: "- [x] Checkbox alpha" in read("alpha")["markdown"])
        expect(checkbox).to_be_enabled()
        assert read("alpha")["markdown"] == alpha.replace("- [ ] Checkbox alpha", "- [x] Checkbox alpha", 1)
        edit("Saved browser response 世界")
        page.locator("#response-save").click()
        saved("Saved browser response 世界")
        assert read("alpha")["markdown"] == alpha.replace("- [ ] Checkbox alpha", "- [x] Checkbox alpha", 1).replace("> original\r\n", "> Saved browser response 世界\r\n", 1)
        footer_matches("alpha")
        assert read("beta")["last_updated"] == before["beta"]["last_updated"]
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
        selected_footer = page.locator("#board-last-updated").text_content()
        source = read("alpha")["markdown"]
        Path(state / "boards" / "alpha.md").write_text(source + "External selection update\n", newline="")
        expect(page.locator("#board-newer")).to_be_visible(timeout=15000)
        assert page.evaluate("getSelection().toString()") == selected
        assert page.locator("#board-last-updated").text_content() == selected_footer
        assert "External selection update" not in page.locator("#board-document").inner_text()
        page.locator("#board-review-refresh").click()
        expect(page.locator("#board-document")).to_contain_text("External selection update")
        footer_matches("alpha")
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
        expect(page.locator("#board-document .board-inline-text")).to_be_visible()
        record("revision conflicts retain intent; explicit unchanged-field rebase is revision-reviewed; changed/deleted fields require reopening")

        # The real POST commits before its JSON continuation is released. While
        # pending, duplicate clicks and navigation cannot create/cross a save.
        edit("one pending browser save")
        count = len(posts)
        arm_gate("alpha", method="POST")
        page.locator("#response-save").click(); gate_wait()
        assert "> one pending browser save" in read("alpha")["markdown"]
        expect(page.locator("#response-save")).to_be_disabled()
        expect(page.locator('#board-document input[type="checkbox"]')).to_be_disabled()
        expect(page.locator("#response-text")).to_be_disabled()
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
        assert not page.locator("#board-raw").text_content()
        expect(page.locator("#response-editor")).to_be_hidden()
        expect(page.locator("#response-heading")).to_have_text("Edit response")
        expect(page.locator("#response-feedback")).to_have_text("")
        assert not page.locator("#response-text").input_value()
        assert page.locator("#response-text").get_attribute("aria-label") in (None, "", "Response"), "Lock retained the private response question in its accessible name"
        assert not page.locator("#board-last-updated").text_content()
        page.locator("#access-token").fill(token); page.locator("#login-form button[type=submit]").click()
        expect(page.locator("#boards-view")).to_be_visible()
        expect(page.locator("#selected-board-name")).to_have_text("main")
        expect(page.locator("#board-document h1")).to_have_text("Ticket board")
        release_gate(); page.wait_for_timeout(150)
        expect(page.locator("#board-document h1")).to_have_text("Ticket board")
        assert "Beta" not in page.locator("#board-raw").text_content()
        assert not page.locator("#board-document .board-control").count()
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
        page.locator("#board-document .board-inline-text").focus()
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
            responses = page.locator("#board-document .board-inline-text").bounding_box()
            board_view = page.locator("#boards-view").bounding_box()
            assert document and responses and board_view
            assert abs(document["width"] - board_view["width"]) <= 2, "Editable board reserves document width for responses"
            assert responses["y"] >= document["y"] and responses["y"] < document["y"] + document["height"], "Response is outside the document"
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
            other.locator("#board-document .board-inline-text").focus()
            other.locator("#response-text").fill("REVOKED_PRIVATE_DRAFT_DATA")
            expect(other.locator("#response-text")).to_have_attribute("aria-label", "secret")
            arm_gate("main", other); gate_wait(other)
            rpc(second_state, "monitor_disable")
            renewed = rpc(second_state, "monitor_enable", port=urlsplit(second["url"]).port)
            expect(other.locator("#workspace")).to_be_hidden(timeout=15000)
            release_gate(other)
            other.wait_for_timeout(150)
            assert not other.locator("#board-raw").text_content()
            expect(other.locator("#response-editor")).to_be_hidden()
            expect(other.locator("#response-text")).to_have_value("")
            assert other.locator("#response-text").get_attribute("aria-label") in (None, "", "Response"), "Revocation retained the private response question in its accessible name"
            expect(other.locator("#response-heading")).to_have_text("Edit response")
            expect(other.locator("#response-feedback")).to_have_text("")
            assert other.locator("#access-token").input_value() == ""
            assert renewed["access_token"] != second["access_token"]
        finally:
            isolated.close(); rpc(second_state, "monitor_disable")
        record("actual private monitor credential rotation clears boards and invalidates delayed old-auth data")
        inline_control_checks(page=page, context=context, rpc=rpc, state=state,
                              origin=origin, token=token, record=record, output=output)
        from monitor_code_copy_qa import code_copy_browser_checks
        code_copy_browser_checks(page=page, rpc=rpc, state=state, token=token, record=record, output=output)
        # Real file timestamps, real HTTP snapshots, and a viewer-zone context:
        # July is Sydney winter; January is Sydney summer. Never fake API data.
        rpc(state, "board_create", board="timezone")
        write("timezone", "# Timestamp baseline\n")
        zoned = browser.new_context(timezone_id="Australia/Sydney", locale="en-AU")
        try:
            local = zoned.new_page(); local.goto(url)
            local.locator("#access-token").fill(token)
            local.locator("#login-form button[type=submit]").click()
            expect(local.locator("#workspace")).to_be_visible()
            # Workspace visibility precedes completion of the initial Boards
            # snapshot. Finish that open before navigating to the new fixture.
            expect(local.locator("#boards-view")).to_be_visible()
            expect(local.locator("#board-raw")).to_have_text(read("main")["markdown"])
            expect(local.locator("#board-status")).to_contain_text("Live")
            local.locator("#board-selector").select_option("timezone")
            expect(local.locator("#selected-board-name")).to_have_text("timezone")
            expect(local.locator("#board-document h1")).to_have_text("Timestamp baseline")
            expect(local.locator("#board-status")).to_contain_text("Live")
            for stamp, zone in ((1782864000000, ("AEST", "GMT+10")),
                                (1767225600000, ("AEDT", "GMT+11"))):
                path = Path(state / "boards" / "timezone.md")
                pending = Path(state / "timezone-fixture.tmp")
                pending.write_text("# Timestamp " + str(stamp) + "\n\n`code " + str(stamp) + "`\n")
                pending.chmod(0o600)
                os.utime(pending, ns=(stamp * 1000000, stamp * 1000000))
                pending.replace(path)
                expect(local.locator("#board-document h1")).to_have_text("Timestamp " + str(stamp), timeout=15000)
                footer = local.locator("#board-last-updated")
                expect(footer).to_contain_text("Last Updated:")
                assert any(value in footer.inner_text() for value in zone), footer.inner_text()
                actual = api_view("timezone")
                assert actual["last_updated"] == stamp == read("timezone")["last_updated"]
                local.locator("#board-raw-toggle").click()
                expect(footer).to_be_visible()
                local.locator("#board-raw-toggle").click()
            # Freeze old January body/time/code together while July arrives.
            local.evaluate("""() => {
              window.__timestampCopy = [];
              Object.defineProperty(navigator, 'clipboard', {configurable:true,
                value:{writeText:async value => window.__timestampCopy.push(value)}});
              const range = document.createRange(); range.selectNodeContents(document.querySelector('#board-document code'));
              const selection = getSelection(); selection.removeAllRanges(); selection.addRange(range);
            }""")
            frozen_footer = local.locator("#board-last-updated").text_content()
            stamp = 1782864000000
            pending.write_text("# Timestamp " + str(stamp) + "\n\n`code " + str(stamp) + "`\n")
            pending.chmod(0o600)
            os.utime(pending, ns=(stamp * 1000000, stamp * 1000000))
            pending.replace(path)
            expect(local.locator("#board-newer")).to_be_visible(timeout=15000)
            assert local.locator("#board-last-updated").text_content() == frozen_footer
            local.locator("#board-document .board-code-copy").click()
            assert local.evaluate("window.__timestampCopy.at(-1)") == "code 1767225600000"
            current_snapshot = api_view("timezone")
            local.evaluate("getSelection().removeAllRanges()")
            expect(local.locator("#board-document code")).to_have_text("code " + str(stamp))
            expect(local.locator("#board-last-updated")).not_to_have_text(frozen_footer)
            local.locator("#board-document .board-code-copy").click()
            assert local.evaluate("window.__timestampCopy.at(-1)") == "code " + str(stamp)
            assert api_view("timezone") == current_snapshot, "Copy changed board content/revision/time"
            local.locator("#lock-button").click()
            assert not local.locator("#board-last-updated").text_content()
        except Exception:
            # Diagnostics must never replace the original acceptance failure.
            try:
                diagnostic = context.request.get(origin + "/api/boards/timezone",
                    headers={"Authorization": "Bearer " + token})
                authoritative = {"status": diagnostic.status, "body": diagnostic.text()}
            except Exception as error:
                authoritative = {"diagnostic_error": str(error)}
            try:
                local.screenshot(path=str(output / "boards-timezone-failure.png"), full_page=True)
                (output / "boards-timezone-failure.json").write_text(json.dumps({
                "view": local.locator("#boards-view").is_visible(),
                "selected": local.locator("#selected-board-name").text_content(),
                "status": local.locator("#board-status").text_content(),
                "error": local.locator("#board-error").text_content(),
                "raw": local.locator("#board-raw").text_content(),
                "footer": local.locator("#board-last-updated").text_content(),
                    "authoritative": authoritative,
                }, indent=2))
            except Exception:
                pass  # Preserve the original assertion even if artifact capture fails.
            raise
        finally:
            zoned.close()
        record("authoritative footer follows actual saves/external edits, holds with selected content, clears on lock and displays Sydney winter/summer offsets without changing UTC metadata")
        page.locator("#messages-tab").click()
        expect(page.locator("#messages-view")).to_be_visible()
    finally:
        page.unroute("**/*", guard)
        # Never leave a deliberate JSON-decode gate suspended after assertions.
        if not page.is_closed():
            page.evaluate("() => { window.__boardGate?.release?.(); }")


def inline_control_checks(*, page, context, rpc, state, origin, token, record, output):
    """Extra source-position/identity cases using real parsed broker metadata."""
    from playwright.sync_api import expect

    name = "inline-fixture"
    source = ("# Inline controls 世界\r\n\r\n- [ ] First choice\r\n- Parent\r\n  - [ ] Nested choice\r\n\r\n"
              "| Context | Marker |\r\n| --- | --- |\r\n| Table | - [ ] table-like |\r\n\r\n"
              "## First question\r\n<!-- board-input: first -->\r\n> first original\r\n<!-- /board-input -->\r\n\r\n"
              "## Second question\r\n<!-- board-input: second -->\r\n> second original\r\n<!-- /board-input -->\r\n\r\n"
              "Inline `- [ ] code` and `echo safe`.\r\n\r\n```md\r\n- [ ] fenced\r\n<!-- board-input: fenced -->\r\n> fenced example\r\n<!-- /board-input -->\r\n```\r\n\r\n"
              "<!-- board-input: duplicate -->\r\n> duplicate one\r\n<!-- /board-input -->\r\n\r\n"
              "<!-- board-input: duplicate -->\r\n> duplicate two\r\n<!-- /board-input -->\r\n\r\n"
              "<!-- board-input: invalid id -->\r\n> invalid visible\r\n<!-- /board-input -->\r\n\r\n"
              "<!-- board-input: outer -->\r\n<!-- board-input: inner -->\r\n> nested visible\r\n<!-- /board-input -->\r\n<!-- /board-input -->\r\n\r\n"
              "<input type=checkbox checked onclick='window.boardPwned=3'>\r\n\r\n"
              "<!-- board-input: unclosed -->\r\n> unclosed visible\r\n")

    def read():
        return rpc(state, "named_board_read", board=name)

    def write(markdown):
        return rpc(state, "named_board_update", board=name,
                   expected_revision=read()["revision"], actor=None,
                   edits=[{"op": "set", "markdown": markdown}])

    def view():
        response = context.request.get(origin + "/api/boards/" + name,
                                       headers={"Authorization": "Bearer " + token})
        assert response.ok
        return response.json()

    def field(field_id):
        return page.locator('#board-document .board-inline-response[data-field-id="' + field_id + '"]')

    rpc(state, "board_create", board=name)
    write(source)
    page.locator("#messages-tab").click()
    page.locator("#boards-tab").click()
    page.locator("#board-selector").select_option(name)
    expect(page.locator("#board-document h1")).to_have_text("Inline controls 世界")
    controls = view()["controls"]
    assert [c.get("field_id") for c in controls if c["kind"] == "text"] == ["first", "second"]
    expect(page.locator('#board-document input[type="checkbox"]')).to_have_count(sum(c["kind"] == "checkbox" for c in controls))
    expect(page.locator("#board-document li li input[type=checkbox]")).to_have_count(1)
    expect(page.locator("#board-document .board-inline-text")).to_have_count(2)
    assert not page.locator("#board-controls textarea:visible, #board-controls input:visible").count()
    visible = page.locator("#board-document").inner_text()
    for value in ("duplicate one", "duplicate two", "invalid visible", "nested visible", "unclosed visible", "fenced example"):
        assert value in visible, value
    assert "first original" not in visible and "second original" not in visible
    assert page.evaluate("typeof window.boardPwned") == "undefined"
    page.locator("#board-raw-toggle").click()
    assert page.locator("#board-raw").text_content() == source
    page.locator("#board-raw-toggle").click()

    before = read()
    field("second").locator(".board-inline-text").focus()
    page.locator("#response-text").fill("cancel this")
    page.locator("#response-cancel").click()
    assert read() == before, "Cancel wrote the board"
    expect(field("second").locator(".board-inline-text")).to_have_value("second original")
    field("second").locator(".board-inline-text").focus()
    page.locator("#response-text").fill("saved 世界\nsecond line")
    page.locator("#response-save").click()
    expected = source.replace("> second original\r\n", "> saved 世界\r\n> second line\r\n", 1)
    expect(field("second").locator(".board-inline-text")).to_have_value("saved 世界\nsecond line")
    assert read()["markdown"] == expected
    assert rpc(state, "named_board_read", board="beta")["markdown"].startswith("# Beta\n")
    record("inline multiple fields retain source context; multiline Save touches only the selected named-board range and Cancel writes nothing; invalid/code/HTML inputs stay inert")

    # Keep a real detached checkbox, then reorder the document. Dispatching its
    # stale native event must never redirect the write to the new first control.
    old = page.locator('#board-document input[type="checkbox"]').first.element_handle()
    reordered = expected.replace("- [ ] First choice\r\n- Parent\r\n  - [ ] Nested choice", "- Parent\r\n  - [ ] Nested choice\r\n- [ ] First choice")
    write(reordered)
    expect(page.locator("#board-raw")).to_have_text(reordered, use_inner_text=False)
    checkpoint = read()
    old.evaluate("e => { e.checked = true; e.dispatchEvent(new Event('change', {bubbles:true})); }")
    page.wait_for_timeout(700)
    assert read() == checkpoint, "A stale checkbox event wrote after reorder"
    if page.locator("#response-cancel").is_visible():
        page.locator("#response-cancel").click()
    record("a retained stale native checkbox event after real source reorder cannot redirect or write")

    field("first").locator(".board-inline-text").focus()
    page.locator("#response-text").fill("live draft with caret")
    expect(field("second").locator(".board-inline-text")).to_be_disabled()
    assert not page.locator("#board-controls textarea:visible").count(), "Ordinary editor duplicated in fallback panel"
    page.locator("#response-text").evaluate("e => e.setSelectionRange(5,9)")
    page.evaluate("window.scrollTo(0, 240)")
    scroll = page.evaluate("scrollY")
    write(read()["markdown"] + "External tail update\r\n")
    expect(page.locator("#board-newer")).to_be_visible(timeout=15000)
    expect(page.locator("#response-text")).to_have_value("live draft with caret")
    expect(page.locator("#response-text")).to_be_focused()
    assert page.locator("#response-text").evaluate("e => [e.selectionStart,e.selectionEnd]") == [5, 9]
    assert page.evaluate("scrollY") == scroll
    page.locator("#board-raw-toggle").click()
    expect(page.locator("#response-text")).to_have_value("live draft with caret")
    page.locator("#board-raw-toggle").click()
    page.locator("#response-save").click()
    expect(page.locator("#response-review")).to_be_visible()
    page.locator("#response-review").click()
    expect(page.locator("#response-rebase")).to_be_visible()
    page.locator("#response-review").click()
    expect(page.locator("#response-rebase")).to_be_visible()
    expect(page.locator("#response-text")).to_have_value("live draft with caret")
    page.locator("#response-cancel").click()
    # Explicit same-revision refresh must leave newly mapped inline handlers live.
    page.locator("#board-refresh").click()
    expect(field("first").locator(".board-inline-text")).to_be_enabled()
    field("first").locator(".board-inline-text").focus()
    expect(page.locator("#response-text")).to_be_focused()
    page.locator("#response-text").fill("same revision reopened")
    page.locator("#response-cancel").click()
    field("first").locator(".board-inline-text").focus()
    expect(page.locator("#response-text")).to_be_focused()
    page.locator("#response-cancel").click()
    for width in (1440, 320):
        page.set_viewport_size({"width": width, "height": 844})
        assert page.evaluate("document.documentElement.scrollWidth <= innerWidth + 1")
        alignment = page.locator('#board-document input[type="checkbox"]').evaluate_all("""inputs => inputs.map(input => {
          const item = input.closest('li');
          if (!item) throw new Error('Native task checkbox lost its list context');
          const walker = document.createTreeWalker(item, NodeFilter.SHOW_TEXT);
          let text;
          while ((text = walker.nextNode())) {
            if (!text.textContent.trim() || text.parentElement.closest('.board-control')) continue;
            const range = document.createRange(); range.selectNodeContents(text);
            const firstLine = Array.from(range.getClientRects()).find(rect => rect.width && rect.height);
            if (!firstLine) continue;
            const box = input.getBoundingClientRect();
            return {label:text.textContent.trim(), checkbox:{x:box.x,y:box.y,width:box.width,height:box.height},
              minHeight:getComputedStyle(input).minHeight,
              labelLine:{x:firstLine.x,y:firstLine.y,width:firstLine.width,height:firstLine.height}};
          }
          throw new Error('Checkbox has no visible adjacent source label');
        })""")
        for item in alignment:
            checkbox, label = item["checkbox"], item["labelLine"]
            assert checkbox["height"] == 19 and item["minHeight"] == "19px", item
            assert abs((checkbox["y"] + checkbox["height"] / 2) - (label["y"] + label["height"] / 2)) <= 8, (
                f"Checkbox is not alongside its first label line at {width}px: {item}")
            assert checkbox["x"] + checkbox["width"] <= label["x"] + 2, (
                f"Checkbox does not precede its source label at {width}px: {item}")
        page.screenshot(path=str(output / f"boards-inline-{width}.png"), full_page=True)
    page.set_viewport_size({"width": 1440, "height": 1000})
    record("inline draft focus, selection range and page scroll survive real live edits and raw-view changes; checkboxes sit alongside source labels and controls fit narrow and desktop layouts")
