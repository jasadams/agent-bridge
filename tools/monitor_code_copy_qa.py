"""Real broker/browser code copy checks; only writeText capability is substituted.

Captured payloads prove the browser API call, never OS clipboard acceptance.
"""
import json

from playwright.sync_api import expect


def code_copy_browser_checks(*, page, rpc, state, token, record, output):
    def read(name):
        return rpc(state, "named_board_read", board=name)

    def write(name, markdown):
        return rpc(state, "named_board_update", board=name,
                   expected_revision=read(name)["revision"], actor=None,
                   edits=[{"op": "set", "markdown": markdown}])

    # Parser semantics include collapsed span whitespace and literal backticks;
    # code blocks preserve indentation/newlines beyond the viewport.
    block = '  printf "世界"\\n\n' + ''.join(f'line {i}\\tail\n' for i in range(110))
    markdown = ('# Copy fixture\n\nRun `echo 世界` then ``a `b` c`` and `echo 世界`.\n\n'
                '[`linked one` and **`linked two`**](https://example.invalid/copy-target)\n\n'
                '- List `list --flag`\n\n| Command |\n| --- |\n| `table --wide 世界` |\n\n'
                '```sh\n' + block + '```\n\n    indented --code\n\n'
                '`<img src=x onerror=alert(1)>`\n\n'
                '- [ ] Keep response\n\n<!-- board-input: answer -->\n> original\n<!-- /board-input -->\n')
    results = []
    result_path = output / "code-copy-results.json"

    def save():
        result_path.write_text(json.dumps(results, indent=2))

    try:
        for name in ("copy-alpha", "copy-beta"):
            rpc(state, "board_create", board=name)
        write("copy-alpha", markdown)
        write("copy-beta", '# Copy beta\n\n`beta payload`\n')
        page.locator("#messages-tab").click()
        page.locator("#boards-tab").click()
        page.locator("#board-selector").select_option("copy-alpha")
        expect(page.locator("#board-document h1")).to_have_text("Copy fixture")
        expect(page.locator("#board-status")).to_contain_text("Live")
        before = read("copy-alpha")
    except Exception as error:
        results.extend({"case": name, "status": "blocked", "reason": "Shared real-broker setup failed: " + str(error)} for name in ['parsed-payloads-selection-keyboard', 'success-icon-feedback', 'inline-code-visual-style', 'clipboard-denied', 'clipboard-unavailable', 'constructed-dom-utf8-size-boundary', 'pending-switch-live-refresh-mobile', 'pending-lock'])
        save()
        raise

    def reset():
        # Fresh page state fences pending callbacks, selections, mutated UI-only
        # fixtures and prior failed authentication without rewriting board data.
        page.evaluate("() => { window.__copyRelease?.(); getSelection().removeAllRanges(); }")
        page.goto(page.url)
        page.locator("#access-token").fill(token)
        page.locator("#login-form button[type=submit]").click()
        expect(page.locator("#workspace")).to_be_visible()
        expect(page.locator("#board-status")).to_contain_text("Live")
        page.locator("#board-selector").select_option("copy-alpha")
        expect(page.locator("#board-document h1")).to_have_text("Copy fixture")
        page.set_viewport_size({"width": 1440, "height": 1000})
        page.evaluate("""() => {
          window.__copyWrites = [];
          window.__copyMode = 'success';
          Object.defineProperty(navigator, 'clipboard', {configurable:true, value:{
            writeText: async value => {
              window.__copyWrites.push(value);
              if (window.__copyMode === 'denied') throw new DOMException('Denied', 'NotAllowedError');
              if (window.__copyMode === 'pending') await new Promise(resolve => { window.__copyRelease = resolve; });
            }
          }});
        }""")

    inline = page.locator("#board-document .board-inline-code > button")

    def case_0():
        expected = ["echo 世界", "a `b` c", "echo 世界", "linked one", "linked two", "list --flag",
                    "table --wide 世界", "<img src=x onerror=alert(1)>"]
        expect(inline).to_have_count(len(expected))
        for index, payload in enumerate(expected):
            inline.nth(index).click()
            expect(page.locator("#board-copy-feedback")).to_have_text("Code copied.")
            assert page.evaluate("window.__copyWrites.at(-1)") == payload
        assert page.locator("#board-document a button").count() == 0
        original_url = page.url
        inline.nth(3).focus(); page.keyboard.press("Space")
        assert page.evaluate("window.__copyWrites.at(-1)") == "linked one"
        inline.nth(4).focus(); page.keyboard.press("Enter")
        assert page.evaluate("window.__copyWrites.at(-1)") == "linked two"
        assert page.url == original_url
        assert page.locator('#board-document a[href="https://example.invalid/copy-target"] code').count() == 2
        blocks = page.locator("#board-document .board-code-block > button")
        expect(blocks).to_have_count(2)
        blocks.nth(0).click()
        assert page.evaluate("window.__copyWrites.at(-1)") == block
        blocks.nth(1).focus(); page.keyboard.press("Enter")
        assert page.evaluate("window.__copyWrites.at(-1)") == "indented --code\n"
        assert page.evaluate("document.activeElement.classList.contains('board-code-copy')")
        # Both code variants show a local icon without selectable button text.
        for controls, label in ((inline, "Copy inline code"), (blocks, "Copy code block")):
            for button in controls.all():
                expect(button).to_have_attribute("aria-label", label)
                expect(button).to_have_attribute("title", label)
                expect(button.locator("svg[aria-hidden=true] path")).to_have_count(1)
                assert button.evaluate("""e => {
                  const svg = e.querySelector('svg'), bounds = svg.getBoundingClientRect();
                  const style = getComputedStyle(svg);
                  return e.textContent === '' && bounds.width >= 16 && bounds.height >= 16 &&
                    style.display !== 'none' && style.visibility === 'visible' &&
                    Number(style.opacity) > 0 && style.stroke !== 'none' &&
                    ['none', 'normal', '\"\"'].includes(getComputedStyle(e, '::before').content);
                }""")
        selected = page.locator("#board-document p").first.evaluate("""p => {
          const r = document.createRange(); r.selectNodeContents(p);
          const s = getSelection(); s.removeAllRanges(); s.addRange(r); return s.toString();
        }""")
        assert selected == 'Run echo 世界 then a `b` c and echo 世界.'
        linked_selection = page.locator("#board-document p").nth(1).evaluate("""p => {
          const r = document.createRange(); r.selectNodeContents(p);
          const s = getSelection(); s.removeAllRanges(); s.addRange(r); return s.toString();
        }""")
        assert linked_selection == 'linked one and linked two'
        page.evaluate("getSelection().removeAllRanges()")
        assert read("copy-alpha") == before, "Copy actions wrote the board"
        record("real parsed inline/table/list/repeated/hostile code and offscreen fenced/indented blocks send exact writeText payloads; keyboard and manual selection stay independent")

    def success_feedback_case():
        button = inline.nth(0)
        geometry = """e => {
          const rect = element => {
            const {x, y, width, height} = element.getBoundingClientRect();
            return {x, y, width, height};
          };
          return {button:rect(e), document:rect(document.querySelector('#board-document'))};
        }"""
        button.scroll_into_view_if_needed()
        before_geometry = button.evaluate(geometry)
        button.click()
        expect(button).to_have_attribute("data-copied", "")
        expect(button.locator("svg path")).to_have_attribute("d", "M5 12l4 4L19 6")
        expect(page.locator("#board-copy-feedback")).to_have_text("Code copied.")
        assert page.locator("#board-copy-feedback").evaluate("e => e.getBoundingClientRect().height") == 1
        assert button.evaluate(geometry) == before_geometry, "Success moved the button or document"
        page.wait_for_timeout(700)
        button.click()
        page.wait_for_timeout(700)
        expect(button).to_have_attribute("data-copied", "")  # First timeout must not reset a newer copy.
        inline.nth(1).click()
        expect(inline.nth(1)).to_have_attribute("data-copied", "")
        page.evaluate("window.__copyMode = 'denied'")
        inline.nth(1).click()
        expect(inline.nth(1)).not_to_have_attribute("data-copied", "")
        feedback = page.locator("#board-copy-feedback")
        expect(feedback).to_contain_text("denied")
        expect(feedback).to_be_visible()
        assert feedback.evaluate("e => e.getBoundingClientRect().height") > 1
        expect(button).not_to_have_attribute("data-copied", "")
        expect(button.locator("svg path")).to_have_attribute("d", "M9 9h12v12H9z M15 9V3H3v12h6")
        expect(feedback).to_contain_text("denied")  # Other-button reset cannot erase newer errors.
        record("success check preserves geometry, repeated copies renew its timer, independent reset preserves visible failures")

    def visual_style_case():
        styles = page.evaluate("""() => {
          const selectors = ['.board-inline-code>code',
            '.board-inline-code>a>code', 'strong .board-inline-code>a>code'];
          return selectors.map(selector => {
            const code = document.querySelector('#board-document ' + selector);
            if (!code) throw new Error('Missing inline code variant: ' + selector);
            const style = getComputedStyle(code);
            return {background:style.backgroundColor, color:style.color,
              borderWidth:style.borderTopWidth, borderStyle:style.borderTopStyle,
              borderColor:style.borderTopColor, padding:style.padding,
              panelBackground:getComputedStyle(document.querySelector('.board-document-panel')).backgroundColor};
          });
        }""")
        assert styles[0] == styles[1] == styles[2], styles
        assert styles[0]["background"] not in ("transparent", "rgba(0, 0, 0, 0)")
        assert styles[0]["background"] != styles[0]["panelBackground"]
        assert styles[0]["borderWidth"] != "0px" and styles[0]["borderStyle"] == "solid"
        assert styles[0]["padding"] != "0px"
        record("ordinary, linked and emphasized linked inline code share contrasting background, border and padding")

    def case_1():
        page.evaluate("window.__copyMode = 'denied'")
        inline.nth(0).click()
        expect(page.locator("#board-copy-feedback")).to_contain_text("denied")
        expect(page.locator("#board-copy-feedback")).to_be_visible()
        expect(inline.nth(0)).not_to_have_attribute("data-copied", "")

    def case_2():
        page.evaluate("Object.defineProperty(navigator, 'clipboard', {configurable:true,value:undefined})")
        inline.nth(0).click()
        expect(page.locator("#board-copy-feedback")).to_contain_text("unavailable")
        expect(page.locator("#board-copy-feedback")).to_be_visible()
        expect(inline.nth(0)).not_to_have_attribute("data-copied", "")

    def case_3():
        # Explicit UI-boundary fixture: backend board limits make a parsed snippet
        # above 64 KiB unreachable, but the clipboard boundary independently rejects
        # it. This constructed DOM is not a backend/parser acceptance claim.
        count = page.evaluate("window.__copyWrites.length")
        inline.nth(0).evaluate("button => { button.previousElementSibling.textContent = '界'.repeat(21846); }")
        inline.nth(0).click()
        expect(page.locator("#board-copy-feedback")).to_contain_text("64 KiB")
        assert page.evaluate("window.__copyWrites.length") == count
        inline.nth(0).evaluate("button => { button.previousElementSibling.textContent = 'echo 世界'; }")

    def case_4():
        # Restore capability and delay one actual copy continuation across navigation.
        page.evaluate("""() => {
          Object.defineProperty(navigator, 'clipboard', {configurable:true,value:{writeText: value => {
            window.__copyWrites.push(value); return new Promise(resolve => { window.__copyRelease = resolve; });
          }}});
        }""")
        inline.nth(0).click()
        page.locator("#board-selector").select_option("copy-beta")
        expect(page.locator("#board-document h1")).to_have_text("Copy beta")
        page.evaluate("window.__copyRelease()")
        expect(page.locator("#board-copy-feedback")).to_have_text("")
        page.evaluate("Object.defineProperty(navigator, 'clipboard', {configurable:true,value:{writeText:async value => {window.__copyWrites.push(value)}}})")
        page.locator("#board-document .board-code-copy").click()
        assert page.evaluate("window.__copyWrites.at(-1)") == "beta payload"
        write("copy-beta", '# Copy beta\n\n`updated payload`\n')
        expect(page.locator("#board-document code")).to_have_text("updated payload")
        page.locator("#board-document .board-code-copy").click()
        assert page.evaluate("window.__copyWrites.at(-1)") == "updated payload", "Reconciled button retained stale text"
        page.set_viewport_size({"width": 320, "height": 844})
        assert page.evaluate("document.documentElement.scrollWidth <= innerWidth + 1")
        page.screenshot(path=str(output / "boards-code-copy-mobile.png"), full_page=True)
        page.set_viewport_size({"width": 1440, "height": 1000})
        assert read("copy-alpha") == before
        record("pending clipboard switch fencing, live patched payloads and mobile layout pass")

    def case_5():
        page.evaluate("""() => {
          Object.defineProperty(navigator, 'clipboard', {configurable:true,value:{writeText: value => {
            window.__copyWrites.push(value); return new Promise(resolve => { window.__copyRelease = resolve; });
          }}});
        }""")
        page.locator("#board-document .board-code-copy").first.click()
        page.locator("#lock-button").click()
        expect(page.locator("#workspace")).to_be_hidden()
        locked_status = page.locator("#board-status").text_content()
        page.evaluate("window.__copyRelease()")
        expect(page.locator("#board-copy-feedback")).to_have_text("")
        assert page.locator("#board-status").text_content() == locked_status
        assert not page.locator("#board-document").text_content()
        assert not page.locator("#board-raw").text_content()
        if page.locator("#board-last-updated").count():
            assert not page.locator("#board-last-updated").text_content()
        page.locator("#access-token").fill(token)
        page.locator("#login-form button[type=submit]").click()
        expect(page.locator("#workspace")).to_be_visible()
        expect(page.locator("#selected-board-name")).to_have_text("main")
        expect(page.locator("#board-status")).to_contain_text("Live")
        record("pending clipboard completion after Lock cannot restore feedback or private board content; linked-code selection excludes copy labels")

    for name, check in [
        ('parsed-payloads-selection-keyboard', case_0),
        ('success-icon-feedback', success_feedback_case),
        ('inline-code-visual-style', visual_style_case),
        ('clipboard-denied', case_1),
        ('clipboard-unavailable', case_2),
        ('constructed-dom-utf8-size-boundary', case_3),
        ('pending-switch-live-refresh-mobile', case_4),
        ('pending-lock', case_5),
]:
        try:
            reset()
        except Exception as error:
            results.append({"case": name, "status": "blocked", "reason": "Case reset failed: " + str(error)})
            save()
            continue
        try:
            check()
            results.append({"case": name, "status": "pass"})
        except Exception as error:
            results.append({"case": name, "status": "fail", "reason": str(error)})
            try:
                page.screenshot(path=str(output / ("code-copy-" + name + "-failure.png")), full_page=True)
            except Exception:
                pass
        finally:
            save()
    failures = [item for item in results if item["status"] != "pass"]
    if failures:
        raise AssertionError("Code copy acceptance failures: " + json.dumps(failures))
