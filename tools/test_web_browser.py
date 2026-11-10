#!/usr/bin/env python3
"""Disposable browser acceptance for the web interface, run once per theme.

Starts a private broker with seeded boards, topics and agent conversations, enables the web
interface, and drives it with an already installed Python Playwright and Chromium (nothing is
downloaded). It checks, for the Classic and Sticker Bomb themes: unlocking from the access link, the
sidebar (boards, topics, agent conversations), rendered topic Markdown, the @mention picker and
publishing, read-only agent conversations and the delivery details panel, the board view and its
overflow menu, the All traffic feed, the collapsible sidebar, the quick switcher, theme switching,
fonts, that the page never scrolls and never overflows horizontally at desktop and phone widths, and
that no Content-Security-Policy violation or console error occurs.

Only private temporary state is used: no native sessions, model calls, service changes or browser
profiles.
"""
import argparse
import json
import os
from pathlib import Path
import shutil
import socket
import subprocess
import sys
import tempfile
import time
import uuid

CHROMIUM_ARGS = ["--no-sandbox"]


def wait(label, check, seconds=30):
    until = time.monotonic() + seconds
    last = None
    while time.monotonic() < until:
        try:
            value = check()
            if value:
                return value
        except (OSError, ValueError, RuntimeError, KeyError) as error:
            last = str(error)
        time.sleep(.08)
    raise AssertionError(f"Timed out: {label}; last error: {last}")


def until(page, expression, timeout=10000):
    """Poll a page expression through CDP. Playwright's wait_for_function evaluates a string, which
    the page's Content-Security-Policy rightly blocks, and the policy must stay enforced in this test."""
    deadline = time.monotonic() + timeout / 1000
    while time.monotonic() < deadline:
        if page.evaluate(expression):
            return True
        page.wait_for_timeout(100)
    raise AssertionError(f"Timed out waiting for: {expression}")


class Broker:
    def __init__(self, binary, root):
        self.binary, self.root = binary, root
        self.state = root / "state"
        self.state.mkdir(mode=0o700)
        home = root / "home"
        home.mkdir(mode=0o700)
        env = {"PATH": os.environ.get("PATH", "/usr/bin:/bin"), "HOME": str(home), "LANG": "C.UTF-8"}
        for variable, relative in [("XDG_CONFIG_HOME", ".config"), ("XDG_DATA_HOME", ".local/share"),
                                   ("XDG_CACHE_HOME", ".cache"), ("XDG_STATE_HOME", ".local/state"),
                                   ("CODEX_HOME", "codex"), ("CLAUDE_CONFIG_DIR", "claude"),
                                   ("OPENCODE_CONFIG_DIR", "opencode")]:
            directory = home / relative
            directory.mkdir(mode=0o700, parents=True, exist_ok=True)
            env[variable] = str(directory)
        self.log = (root / "broker.log").open("wb")
        self.process = subprocess.Popen(
            [str(binary), "--state-dir", str(self.state), "serve", "--no-discovery", "--codex-bin", "/missing/codex"],
            cwd=root, env=env, stdin=subprocess.DEVNULL, stdout=self.log, stderr=self.log)
        wait("broker", lambda: self.rpc("status"))

    def rpc(self, op, **fields):
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as connection:
            connection.settimeout(30)
            connection.connect(str(self.state / "bridge.sock"))
            connection.sendall(json.dumps({"op": op, **fields}).encode() + b"\n")
            response = connection.makefile("rb").readline(4 * 1024 * 1024)
        value = json.loads(response)
        if not value["ok"]:
            raise RuntimeError(value.get("error", "RPC rejected"))
        return value.get("result")

    def close(self):
        self.process.terminate()
        try:
            self.process.wait(10)
        except subprocess.TimeoutExpired:
            self.process.kill()
        self.log.close()


RELEASE_BOARD = """# Release checklist

- [x] CI green on main
- [ ] Installer smoke test on macOS
- [ ] Tag and publish

| Step | Owner | Status |
|---|---|---|
| Build | codex-kyomi | done |
| Notes | claude-bridge | review |

```sh
git tag v0.6.0 && git push origin v0.6.0
```

Blockers:

<!-- board-input: blockers -->
> none yet
<!-- /board-input -->
"""

LONG_POST = "# Findings\n\n" + "\n\n".join(
    f"Paragraph {n} of a deliberately long report so the post collapses behind Show more. "
    "It discusses the scroll model, the topic renderer and the delivery inspector at length." for n in range(1, 18)
) + "\n\n```rust\nfn main() { println!(\"done\"); }\n```\n"

RICH_POST = ("## Release notes\n\n- topics render **Markdown**\n- `code` and tables work\n\n"
             "| Theme | Default |\n|---|---|\n| Classic | yes |\n| Sticker Bomb | no |\n\n"
             "```sh\nagent-bridge web enable\n```\n\n[docs](https://example.com/docs) "
             "[bad](javascript:alert(1)) <img src=x onerror=window.pwned=1>")


def seed(broker):
    broker.rpc("machine_set_name", name="web-qa")
    for name in ["claude-bridge", "codex-kyomi", "opencode-ui"]:
        broker.rpc("register", name=name, provider="inbox", session=name, replace=False)

    def send(sender, recipient, body):
        message_id = str(uuid.uuid4())
        broker.rpc("send", id=message_id, sender=sender, recipient=recipient, body=body, parent=None)
        return message_id

    send("claude-bridge", "codex-kyomi", "Can you confirm the macOS runner is free?")
    send("codex-kyomi", "claude-bridge", "Free now. Use label macos-14.")
    send("claude-bridge", "codex-kyomi", "Queued the release build.")
    send("opencode-ui", "claude-bridge", "Heads up: the old layout is still shipping.")
    send("user", "claude-bridge", "Please also run the installer smoke test.")
    broker.rpc("board_create", board="release-checklist")
    snapshot = broker.rpc("named_board_read", board="release-checklist")
    broker.rpc("named_board_update", board="release-checklist", expected_revision=snapshot["revision"],
               edits=[{"op": "set", "markdown": RELEASE_BOARD}], actor=None)
    topic = broker.rpc("topic_create", name="release")
    topic_id = topic["reference"]["topic_id"]
    ref = {"bridge_id": None, "topic_id": topic_id}
    for author, body in [("inbox-claude-bridge", "Starting the release train. Notes are on the board."),
                         ("inbox-codex-kyomi", "@inbox-claude-bridge CI is green on main."),
                         ("inbox-claude-bridge", RICH_POST),
                         ("inbox-opencode-ui", LONG_POST)]:
        broker.rpc("topic_publish", topic=ref, id=str(uuid.uuid4()), author={"bridge_id": None, "agent_id": author},
                   body=body, parent=None, mentions=[])
    return topic_id


class Report:
    def __init__(self):
        self.failures = []

    def check(self, name, condition, detail=""):
        if condition:
            print(f"PASS: {name}", flush=True)
        else:
            print(f"FAIL: {name} {detail}", flush=True)
            self.failures.append(name)


CONTRAST = """() => {
  const style = getComputedStyle(document.documentElement), value = name => style.getPropertyValue(name).trim();
  const probe = document.createElement('span'); document.body.append(probe);
  const rgb = color => { probe.style.color = color; const m = getComputedStyle(probe).color.match(/[\\d.]+/g).map(Number); return m.slice(0, 3); };
  const lum = ([r, g, b]) => { const f = c => { c /= 255; return c <= .03928 ? c / 12.92 : ((c + .055) / 1.055) ** 2.4; }; return .2126 * f(r) + .7152 * f(g) + .0722 * f(b); };
  const ratio = (a, b) => { const x = lum(rgb(a)), y = lum(rgb(b)); return (Math.max(x, y) + .05) / (Math.min(x, y) + .05); };
  const fails = [];
  for (const fg of ['--text', '--text-2', '--text-3', '--link', '--accent', '--ok', '--warn', '--danger'])
    for (const bg of ['--chrome', '--bg', '--surface', '--surface-2'])
      { const r = ratio(value(fg), value(bg)); if (r < 4.5) fails.push(`${fg} on ${bg}: ${r.toFixed(2)}`); }
  const inkOnAccent = ratio(value('--accent-ink'), value('--accent'));
  probe.remove();
  return { fails, inkOnAccent };
}"""


def run_theme(playwright, theme, url_for, topic_id, report, out):
    browser = playwright.chromium.launch(args=CHROMIUM_ARGS)
    context = browser.new_context(viewport={"width": 1440, "height": 900})
    page = context.new_page()
    problems = []
    page.on("console", lambda m: problems.append(m.text) if m.type == "error" else None)
    page.on("pageerror", lambda e: problems.append(str(e)))
    font_requests = []
    page.on("request", lambda r: font_requests.append(r.url) if "/fonts/" in r.url else None)
    tag = f"[{theme}]"

    page.add_init_script(f"try {{ if (!localStorage.getItem('agent-bridge-theme')) localStorage.setItem('agent-bridge-theme', '{theme}') }} catch (e) {{}}")
    page.goto(url_for)
    page.wait_for_selector("#sidebar .nav-item", timeout=15000)
    report.check(f"{tag} unlocks from the access link and clears the token from the URL", "access_token" not in page.url)
    report.check(f"{tag} theme applied", page.evaluate("document.documentElement.dataset.theme") == theme)
    sheets = page.evaluate("[...document.styleSheets].map(s => s.href)")
    report.check(f"{tag} exactly one theme sheet loaded", sum("/theme-" in (s or "") for s in sheets) == 1, str(sheets))
    until(page, "document.querySelectorAll('#sidebar .nav-item').length >= 6", 20000)

    labels = page.eval_on_selector_all("#sidebar .nav-item .grow", "els => els.map(e => e.textContent)")
    report.check(f"{tag} sidebar lists boards", "main" in labels and "release-checklist" in labels, str(labels))
    report.check(f"{tag} sidebar lists the topic", "release" in labels, str(labels))
    pairs = [l for l in labels if "·" in l]
    report.check(f"{tag} sidebar lists agent conversations, not the human's", any("claude-bridge" in l and "codex-kyomi" in l for l in pairs) and not any(l.startswith("user") or " user" in l for l in pairs), str(pairs))
    report.check(f"{tag} All traffic is pinned last", labels[-1] == "All traffic", str(labels))

    # board view
    page.click('#sidebar .nav-item[data-route="#/board/release-checklist"]')
    page.wait_for_selector("#board-document table", timeout=15000)
    report.check(f"{tag} board renders headings, a table and code", page.locator("#board-document h1").count() == 1 and page.locator("#board-document pre").count() >= 1)
    report.check(f"{tag} board has no dropdown, Refresh or Raw Markdown button", page.get_by_role("button", name="Raw Markdown").count() == 0 and page.get_by_role("button", name="Refresh").count() == 0 and page.locator("#board-selector").is_hidden())
    page.click("#board-menu-button")
    report.check(f"{tag} board overflow menu offers Archive and Archived boards", page.get_by_role("menuitem", name="Archive board").is_visible() and page.get_by_role("menuitem", name="Archived boards…").is_visible())
    page.keyboard.press("Escape")
    page.locator("#board-document input[type=checkbox]").nth(1).check()
    page.wait_for_timeout(800)
    snapshot = None
    report.check(f"{tag} board checkbox control is interactive", page.locator("#board-document input[type=checkbox]").nth(1).is_checked())
    page.screenshot(path=str(out / f"{theme}-board.png"))

    # an unsaved response must block navigation until the person chooses what to do with it
    page.locator("#board-document textarea.board-inline-text").first.click()
    editor = page.locator("#response-text")  # focusing the passive field hands editing to the shared editor
    editor.fill("a half written answer")
    page.click('#sidebar .nav-item[data-route^="#/topic/"]')
    page.wait_for_selector("#unsaved-dialog[open]", timeout=5000)
    report.check(f"{tag} navigating away from an unsaved response asks what to do", page.locator("#unsaved-dialog").is_visible())
    page.click("#unsaved-cancel")
    page.wait_for_timeout(400)
    report.check(f"{tag} Cancel keeps the board, the draft and the address", "#/board/release-checklist" in page.url and page.locator("#boards-view").is_visible() and editor.input_value() == "a half written answer", page.url)
    page.click('#sidebar .nav-item[data-route="#/board/main"]')
    page.wait_for_selector("#unsaved-dialog[open]", timeout=5000)
    page.click("#unsaved-cancel")
    page.wait_for_timeout(300)
    report.check(f"{tag} switching boards is guarded too", "#/board/release-checklist" in page.url)
    # Lock must not silently discard the draft either
    page.click("#lock-button")
    page.wait_for_selector("#unsaved-dialog[open]", timeout=5000)
    page.click("#unsaved-cancel")
    page.wait_for_timeout(300)
    report.check(f"{tag} Lock with an unsaved response asks first and Cancel stays unlocked", page.locator("#workspace").is_visible() and editor.input_value() == "a half written answer")
    # Back while a draft is open must not leave the board
    page.go_back()
    page.wait_for_selector("#unsaved-dialog[open]", timeout=5000)
    page.click("#unsaved-cancel")
    page.wait_for_timeout(400)
    report.check(f"{tag} Back with an unsaved response asks, and Cancel keeps the draft, the view and the address", editor.input_value() == "a half written answer" and page.locator("#boards-view").is_visible() and "#/board/release-checklist" in page.url, page.url)
    # a pasted access link while unlocked is consumed from the address bar but changes nothing else
    route_before = page.evaluate("location.hash")
    page.evaluate("location.hash = '#access_token=' + 'A'.repeat(43)")
    page.wait_for_timeout(400)
    report.check(f"{tag} a pasted access link is cleared and the current view and draft are kept", "access_token" not in page.url and page.evaluate("location.hash") == route_before and editor.input_value() == "a half written answer", page.url)
    page.click('#sidebar .nav-item[data-route^="#/topic/"]')
    page.wait_for_selector("#unsaved-dialog[open]", timeout=5000)
    page.click("#unsaved-discard")
    page.wait_for_selector("#topics-view:not([hidden])", timeout=8000)
    report.check(f"{tag} Discard and continue navigates", "#/topic/" in page.url and page.locator("#boards-view").is_hidden())
    page.go_back()
    page.wait_for_selector("#boards-view:not([hidden])", timeout=8000)
    report.check(f"{tag} the browser Back button returns to the board", "#/board/release-checklist" in page.url, page.url)
    page.go_forward()
    page.wait_for_selector("#topics-view:not([hidden])", timeout=8000)
    report.check(f"{tag} Forward returns to the topic", "#/topic/" in page.url)

    # topic view
    page.click('#sidebar .nav-item[data-route^="#/topic/"]')
    page.wait_for_selector("#topic-posts .md table", timeout=15000)
    report.check(f"{tag} topic post renders Markdown (heading, list, table, code)", all(page.locator(f"#topic-posts .md {s}").count() >= 1 for s in ["h2", "ul", "table", ".md-code pre"]))
    report.check(f"{tag} raw HTML and javascript: links stay inert", page.locator("#topic-posts img").count() == 0 and page.locator("#topic-posts a[href^='javascript']").count() == 0 and not page.evaluate("window.pwned"))
    report.check(f"{tag} the long post collapses behind Show more", page.locator("#topic-posts .show-more:visible").count() >= 1)
    page.locator("#topic-posts .show-more:visible").first.click()
    report.check(f"{tag} Show more expands it", page.locator("#topic-posts [data-collapsible].is-collapsed").count() == 0)
    page.mouse.move(0, 0)  # no hover: only keyboard focus may reveal the actions
    page.locator("#topic-posts .msg").last.locator(".msg__actions button[aria-label='Reply']").focus()
    report.check(f"{tag} message actions are keyboard reachable", page.locator("#topic-posts .msg").last.locator(".msg__actions").evaluate("e => getComputedStyle(e).opacity") == "1")
    report.check(f"{tag} the page itself does not scroll", not page.evaluate("document.documentElement.scrollHeight > innerHeight + 1"))
    report.check(f"{tag} the timeline scrolls inside its pane", page.evaluate("(() => { const s = document.getElementById('topic-scroll'); return s.scrollHeight > s.clientHeight; })()"))
    last = page.locator("#topic-posts .msg").filter(has_text="Release notes").first  # a short post: toggling its source must not touch the long one
    last.hover()
    last.locator(".msg__actions button[aria-label='View Markdown source']").click()
    report.check(f"{tag} View source shows the exact Markdown", page.locator("#topic-posts pre.md-source").count() == 1)
    last.locator(".msg__actions button[aria-label='View Markdown source']").click()
    composer = page.locator("#topic-body")
    composer.click()
    composer.type("thanks @inbox-cl")
    page.wait_for_selector("#topic-mention-pop .mention-option", timeout=5000)
    report.check(f"{tag} typing @ opens the mention picker", page.locator("#topic-mention-pop .mention-option").count() >= 1)
    page.keyboard.press("Enter")
    report.check(f"{tag} Enter picks the mention", "@inbox-claude-bridge " in composer.input_value(), composer.input_value())
    composer.type("**ship it**")
    page.keyboard.press("Enter")
    page.wait_for_selector("#topic-posts .msg:has-text('ship it')", timeout=10000)
    report.check(f"{tag} a new post keeps earlier posts expanded", page.locator("#topic-posts [data-collapsible].is-collapsed").count() == 0)
    report.check(f"{tag} publishing shows your message as You", page.locator("#topic-posts .author.is-you").count() >= 1)
    page.screenshot(path=str(out / f"{theme}-topic.png"))

    # agent conversation (read only) and the delivery details panel
    page.click('#sidebar .nav-item[data-route^="#/dm/"]')
    page.wait_for_selector("#dm-messages .msg", timeout=15000)
    report.check(f"{tag} agent conversation is read only (no composer)", page.locator("#dm-view textarea").count() == 0 and page.locator("#dm-view .observer").is_visible())
    page.locator("#dm-messages .msg").first.hover()
    page.locator("#dm-messages .msg__actions button").first.click(force=True)
    page.wait_for_selector("#panel:not([hidden]) .detail-grid", timeout=10000)
    report.check(f"{tag} delivery details open in the side panel", page.locator("#panel-title").inner_text() == "Delivery details")
    page.screenshot(path=str(out / f"{theme}-dm.png"))
    page.click("#panel-close")
    report.check(f"{tag} panel closes", page.locator("#panel").is_hidden())

    # traffic
    page.click('#sidebar .nav-item[data-route="#/traffic"]')
    page.wait_for_selector("#message-list .message-row", timeout=15000)
    report.check(f"{tag} All traffic lists messages including the human's", page.locator("#message-list .message-row").count() >= 5)
    page.select_option("#state-filter", "delivered")
    page.click("#filters button[type=submit]")
    page.wait_for_timeout(600)
    report.check(f"{tag} traffic filters apply", page.locator("#message-list .message-row").count() >= 1)
    page.screenshot(path=str(out / f"{theme}-traffic.png"))

    # shell behaviour
    page.keyboard.press("Control+b")
    page.wait_for_timeout(400)
    width = page.evaluate("document.getElementById('sidebar').getBoundingClientRect().width")
    report.check(f"{tag} Ctrl+B collapses the sidebar to a rail", 60 <= width <= 76, str(width))
    page.screenshot(path=str(out / f"{theme}-rail.png"))
    page.keyboard.press("Control+b")
    page.wait_for_timeout(300)
    page.keyboard.press("Control+k")
    page.wait_for_selector("#switcher[open]", timeout=5000)
    page.keyboard.type("release")
    page.keyboard.press("Enter")
    page.wait_for_timeout(600)
    report.check(f"{tag} quick switcher navigates", "#/" in page.url)
    fonts_before_switch = list(font_requests)
    page.click("#theme-button")
    other = "sticker" if theme == "classic" else "classic"
    page.click(f"[data-theme-choice={other}]")
    until(page, f"document.documentElement.dataset.theme === '{other}'", 8000)
    report.check(f"{tag} the picker switches theme live and remembers it", page.evaluate("localStorage.getItem('agent-bridge-theme')") == other)
    page.click("#theme-button")
    page.click(f"[data-theme-choice={theme}]")
    until(page, f"document.documentElement.dataset.theme === '{theme}'", 8000)

    # fonts
    if theme == "sticker":
        until(page, "document.fonts.check('15px \"Bricolage Grotesque\"')", 8000)
        report.check(f"{tag} Sticker Bomb loads its self-hosted fonts", any("bricolage" in u for u in font_requests), str(font_requests))
    else:
        report.check(f"{tag} Classic requests no font files (system fonts)", not fonts_before_switch, str(fonts_before_switch))

    # layout: no page scroll or horizontal overflow on any view at desktop and phone widths
    dm_route = page.evaluate("document.querySelector('#sidebar .nav-item[data-route^=\"#/dm/\"]').dataset.route")
    for label, size in [("desktop", {"width": 1440, "height": 900}), ("phone", {"width": 390, "height": 800})]:
        page.set_viewport_size(size)
        for route in ["#/board/main", "#/board/release-checklist", f"#/topic/{topic_id}", dm_route, "#/traffic"]:
            page.evaluate(f"location.hash = {json.dumps(route)}")
            page.wait_for_timeout(600)
            scrolls = page.evaluate("document.documentElement.scrollHeight > innerHeight + 1")
            overflow = page.evaluate("document.documentElement.scrollWidth > innerWidth + 1")
            report.check(f"{tag} {label} {route[:22]}: page does not scroll or overflow", not scrolls and not overflow, f"scroll={scrolls} overflow={overflow}")
        if label == "phone":
            page.screenshot(path=str(out / f"{theme}-phone.png"))
    page.set_viewport_size({"width": 1440, "height": 900})

    result = page.evaluate(CONTRAST)
    report.check(f"{tag} text colours meet 4.5:1 on their surfaces", not result["fails"], str(result["fails"]))
    report.check(f"{tag} button text meets 4.5:1 on the accent", result["inkOnAccent"] >= 4.5, str(result["inkOnAccent"]))

    page.click("#lock-button")
    page.wait_for_selector("#login:not([hidden])", timeout=5000)
    page.goto(url_for.split("#")[0])
    page.wait_for_selector("#login:not([hidden])", timeout=8000)
    report.check(f"{tag} the saved theme is applied on the login page with a single sheet", page.evaluate("document.documentElement.dataset.theme") == theme and sum("/theme-" in (h or "") for h in page.evaluate("[...document.styleSheets].map(s => s.href)")) == 1)
    report.check(f"{tag} Lock clears the workspace and shows the login again", page.locator("#workspace").is_hidden() and page.locator("#sidebar .nav-item").count() == 0)

    csp = [p for p in problems if "Content Security Policy" in p or "Refused to" in p]
    report.check(f"{tag} no Content-Security-Policy violations", not csp, str(csp[:3]))
    other_errors = [p for p in problems if p not in csp and "net::ERR_ABORTED" not in p]
    report.check(f"{tag} no console or page errors", not other_errors, str(other_errors[:5]))
    context.close()
    browser.close()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", required=True, type=Path)
    parser.add_argument("--output", type=Path)
    parser.add_argument("--theme", choices=["classic", "sticker", "both"], default="both")
    args = parser.parse_args()
    try:
        from playwright.sync_api import sync_playwright
    except ImportError:
        print("Playwright for Python is not installed; this browser acceptance cannot run.", file=sys.stderr)
        return 2
    out = (args.output or Path(tempfile.mkdtemp(prefix="ab-web-qa-", dir="/tmp"))).resolve()
    out.mkdir(mode=0o700, parents=True, exist_ok=True)
    binary = out / "frozen-agent-bridge"
    shutil.copy2(args.binary.resolve(strict=True), binary)
    binary.chmod(0o700)
    report = Report()
    broker = Broker(binary, out)
    try:
        topic_id = seed(broker)
        credentials = broker.rpc("monitor_enable", port=0)
        themes = ["classic", "sticker"] if args.theme == "both" else [args.theme]
        with sync_playwright() as playwright:
            for theme in themes:
                run_theme(playwright, theme, credentials["access_url"], topic_id, report, out)
    finally:
        broker.close()
    print(f"\nScreenshots and logs: {out}")
    if report.failures:
        print(f"\n{len(report.failures)} check(s) failed:", *report.failures, sep="\n  - ")
        return 1
    print("\nAll web interface browser checks passed.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
