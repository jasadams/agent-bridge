#!/usr/bin/env python3
"""Disposable browser acceptance for the web interface, run once per theme.

Starts a private broker with seeded boards, topics and agent conversations, enables the web
interface, and drives it with an already installed Python Playwright and Chromium (nothing is
downloaded). It checks, for the Classic, Sticker Bomb and Orca themes: unlocking from the access link, the
sidebar (boards, topics, agent conversations), rendered topic Markdown, the @mention picker and
publishing, read-only agent conversations and the delivery details panel, the board view and its
overflow menu, the All traffic feed, the collapsible sidebar, the quick switcher, theme switching,
fonts, that the page never scrolls and never overflows horizontally at desktop and phone widths, and
that no Content-Security-Policy violation or console error occurs.

Agent names are the broker's (AGB-60): the seeded Claude sessions are named by a stand-in `claude`
executable on the broker's PATH, and the page must show those names, never the IDs.

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

CHROMIUM_ARGS = ["--no-sandbox", "--use-mock-keychain", "--password-store=basic"]
ORCA_THEMES = ["orca", "orca-light", "orca-system"]
ALL_THEMES = ["classic", "sticker"] + ORCA_THEMES
THEME_FONTS = {"classic": set(), "sticker": {"bricolage-var.woff2", "space-mono-400.woff2", "space-mono-700.woff2"},
               "orca": {"geist-var.woff2"}, "orca-light": {"geist-var.woff2"}, "orca-system": {"geist-var.woff2"}}


def sheet_for(theme, scheme):
    """The sheet a choice shows when the browser's appearance is `scheme` ("light" or "dark")."""
    return ("orca" if scheme == "dark" else "orca-light") if theme == "orca-system" else theme


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
        env = {"PATH": f"{root / 'bin'}:{os.environ.get('PATH', '/usr/bin:/bin')}", "HOME": str(home), "LANG": "C.UTF-8"}
        for variable, relative in [("XDG_CONFIG_HOME", ".config"), ("XDG_DATA_HOME", ".local/share"),
                                   ("XDG_CACHE_HOME", ".cache"), ("XDG_STATE_HOME", ".local/state"),
                                   ("CODEX_HOME", "codex"), ("CLAUDE_CONFIG_DIR", "claude"),
                                   ("OPENCODE_CONFIG_DIR", "opencode")]:
            directory = home / relative
            directory.mkdir(mode=0o700, parents=True, exist_ok=True)
            env[variable] = str(directory)
        self.log = (root / "broker.log").open("wb")
        self.process = None
        try:
            self.process = subprocess.Popen(
                [str(binary), "--state-dir", str(self.state), "serve", "--no-discovery", "--codex-bin", "/missing/codex"],
                cwd=root, env=env, stdin=subprocess.DEVNULL, stdout=self.log, stderr=self.log)
            wait("broker", lambda: self.rpc("status"))
        except BaseException:
            # The caller never receives the object, so a broker that fails to start is stopped here.
            self.close()
            raise

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
        if self.process:
            self.process.terminate()
            try:
                self.process.wait(10)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait()
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
             "| Theme | Default |\n|---|---|\n| Classic | yes |\n| Sticker Bomb | no |\n| Orca | no |\n\n"
             "```sh\nagent-bridge web enable\n```\n\n[docs](https://example.com/docs) "
             "[bad](javascript:alert(1)) <img src=x onerror=window.pwned=1>")


# Native Claude sessions (AGB-57): the ID is the routing name `claude-<session>`, and the broker resolves the
# name from the client's own directory. A stand-in `claude` executable on the broker's PATH answers
# `claude agents --json`, as the repository's own broker tests do: it names the first and third sessions and has no
# name for the second, which therefore shows the generated "Claude" plus the last four characters of its session.
# The page must show these names and never the IDs, which belong only in the details panel.
CAPTAIN_SESSION = "9a374eae-798c-4ca0-9ebf-b954fb368c50"
UNNAMED_SESSION = "6c8d2f1b-4e55-4a7f-8b32-1d9e8f70c2d2"
THIRD_SESSION = "7d9e3a2c-5f66-4b80-9c43-2eaf9081d3f3"
SESSIONS = (CAPTAIN_SESSION, UNNAMED_SESSION, THIRD_SESSION)
BRIDGE_ID, KYOMI_ID, UI_ID = (f"claude-{session}" for session in SESSIONS)
AGENT_IDS = (BRIDGE_ID, KYOMI_ID, UI_ID)
BRIDGE_NAME, KYOMI_NAME, UI_NAME = "Release Captain", "Claude c2d2", "UI Builder"
CLAUDE_DIRECTORY = json.dumps([{"sessionId": CAPTAIN_SESSION, "name": BRIDGE_NAME},
                               {"sessionId": THIRD_SESSION, "name": UI_NAME}])


# What a run creates directly in the output directory. A run starts by removing these from an earlier run
# (or from a run that crashed), so the same directory can be used again. The copied binary and the screenshots
# are overwritten by name; anything else in the directory, and everything outside it, is left alone.
RUN_FILES = ("broker.log", "state", "home", "bin", "s")
# The first thing a run puts in its output directory. Only a directory that is new, empty or holds this marker
# is cleared, so a directory someone else uses (their home, a project) is refused and never touched.
MARKER = ".agb-web-browser-test"
COPIED_BINARY = "frozen-agent-bridge"


class OutputDirectoryError(Exception):
    pass


def prepare_output(requested, binary=None):
    """Return the output directory, ready for a run: created, or cleared of an earlier run's files.
    `binary` is the resolved --binary, which must not be something the run is about to clear."""
    if requested is None:
        directory = Path(tempfile.mkdtemp(prefix="ab-web-qa-", dir="/tmp")).resolve()
    elif requested.is_symlink():
        raise OutputDirectoryError(f"{requested} is a symbolic link; pass the directory itself")
    else:
        directory = requested.resolve()
    if directory.exists() and not directory.is_dir():
        raise OutputDirectoryError(f"{directory} exists and is not a directory")
    if binary and binary.is_relative_to(directory) and binary != directory / COPIED_BINARY:
        raise OutputDirectoryError(f"--binary {binary} is inside the output directory {directory}; pass a binary outside it")
    marker = directory / MARKER
    if directory.exists() and not (marker.is_file() and not marker.is_symlink()) and any(directory.iterdir()):
        raise OutputDirectoryError(
            f"{directory} is not empty and was not made by this test (it has no {MARKER} file), so nothing "
            "in it was touched; pass a new or empty directory")
    directory.mkdir(mode=0o700, parents=True, exist_ok=True)
    marker.touch()
    for entry in directory.iterdir():
        if entry.is_symlink():  # the copied binary and the screenshots are written by name, never through a link
            entry.unlink()
    for name in RUN_FILES:
        path = directory / name
        if path.is_dir() and not path.is_symlink():
            shutil.rmtree(path)
        else:
            path.unlink(missing_ok=True)
    return directory


def stand_in_claude(directory):
    """A `claude` executable that prints the directory above, and Unix sockets for the sessions to bind."""
    (directory / "bin").mkdir(mode=0o700)
    script = directory / "bin" / "claude"
    script.write_text(f"#!/bin/sh\nprintf '%s' '{CLAUDE_DIRECTORY}'\n")
    script.chmod(0o700)


def bind_sessions(broker, directory, listeners):
    sockets = directory / "s"
    sockets.mkdir(mode=0o700)
    for session in SESSIONS:
        path = sockets / f"{session[:8]}.sock"
        listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        listener.bind(str(path))
        path.chmod(0o600)
        listener.listen()
        listeners.append(listener)
        broker.rpc("claude_bind", session=session, endpoint=str(path))


def seed(broker):
    broker.rpc("machine_set_name", name="web-qa")
    # Inbox agents: the authors of the topic posts, and the recipient that makes the human's message deliver.
    for name in ["claude-bridge", "codex-kyomi", "opencode-ui"]:
        broker.rpc("register", name=name, provider="inbox", session=name, replace=False)

    def send(sender, recipient, body):
        message_id = str(uuid.uuid4())
        broker.rpc("send", id=message_id, sender=sender, recipient=recipient, body=body, parent=None)
        return message_id

    send(BRIDGE_ID, KYOMI_ID, "Can you confirm the macOS runner is free?")
    send(KYOMI_ID, BRIDGE_ID, "Free now. Use label macos-14.")
    send(BRIDGE_ID, KYOMI_ID, "Queued the release build.")
    send(UI_ID, BRIDGE_ID, "Heads up: the old layout is still shipping.")
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


RENDERED_CONTRAST = """() => {
  // Contrast of what is actually painted, for every visible: text node, input/textarea/select value,
  // placeholder, and ::before/::after text. Colours are read from the computed style; a translucent
  // colour is blended over the layers behind it, and the opacity of the element and its ancestors
  // is applied to the text colour. Disabled controls and fully transparent elements (the message
  // actions before hover or focus) are skipped. Not covered:
  // images, icons drawn as SVG/borders, and text inside pseudo-elements that have their own background.
  // rgb()/rgba() are 0-255; color-mix() computes to color(srgb r g b / a) with 0-1 channels.
  const parse = c => { const m = c.match(/[\\d.]+/g).map(Number), k = c.startsWith('color(srgb') ? 255 : 1; return { r: m[0] * k, g: m[1] * k, b: m[2] * k, a: m.length > 3 ? m[3] : 1 }; };
  const over = (top, under) => ({ r: top.r * top.a + under.r * (1 - top.a), g: top.g * top.a + under.g * (1 - top.a), b: top.b * top.a + under.b * (1 - top.a), a: 1 });
  const lum = ({ r, g, b }) => { const f = c => { c /= 255; return c <= .03928 ? c / 12.92 : ((c + .055) / 1.055) ** 2.4; }; return .2126 * f(r) + .7152 * f(g) + .0722 * f(b); };
  const backdrop = el => { const layers = []; for (let n = el; n; n = n.parentElement) { const c = parse(getComputedStyle(n).backgroundColor); if (c.a > 0) layers.push(c); if (c.a === 1) break; }
    return layers.reduceRight((under, top) => over(top, under), { r: 0, g: 0, b: 0, a: 1 }); };
  const opacity = el => { let o = 1; for (let n = el; n; n = n.parentElement) o *= Number(getComputedStyle(n).opacity); return o; };
  const fails = []; let checked = 0;
  const check = (el, what, colour) => {
    const bg = backdrop(el), c = parse(colour), fg = over({ ...c, a: c.a * opacity(el) }, bg);
    const a = lum(fg) + .05, b = lum(bg) + .05, ratio = Math.max(a, b) / Math.min(a, b);
    checked += 1; if (ratio < 4.5) fails.push(`${el.tagName.toLowerCase()}.${el.className} ${what}: ${ratio.toFixed(2)}`);
  };
  const shown = el => { const box = el.getBoundingClientRect(); return box.width && box.height && getComputedStyle(el).visibility !== 'hidden' && !el.closest('[hidden], [disabled], .sr-only') && opacity(el) > 0; };
  const seen = new Set(), walker = document.createTreeWalker(document.body, NodeFilter.SHOW_TEXT);
  for (let node = walker.nextNode(); node; node = walker.nextNode()) {
    const el = node.parentElement; if (!node.textContent.trim() || !el || seen.has(el) || !shown(el) || el.closest('textarea, select')) continue; seen.add(el);
    check(el, `"${node.textContent.trim().slice(0, 24)}"`, getComputedStyle(el).color);
  }
  for (const el of document.querySelectorAll('input, textarea, select')) {
    if (!shown(el) || ['checkbox', 'radio', 'hidden'].includes(el.type)) continue;
    if (el.value) check(el, 'value', getComputedStyle(el).color);
    else if (el.placeholder) check(el, 'placeholder', getComputedStyle(el, '::placeholder').color);
  }
  for (const el of document.querySelectorAll('body *')) {
    if (!shown(el)) continue;
    for (const pseudo of ['::before', '::after']) { const s = getComputedStyle(el, pseudo), text = s.content.replace(/^"|"$/g, ''); if (s.content !== 'none' && s.content !== 'normal' && text.trim() && s.display !== 'none') check(el, `${pseudo} "${text.slice(0, 12)}"`, s.color); }
  }
  return { checked, fails };
}"""

THEME_LEAK = """() => {
  // Elements painted in a colour that only Classic or Sticker Bomb defines (their palettes are
  // the lists below). Orca must override all of them on every screen.
  const classic = ['c4ef9d', '10171c', '121c22', '172128', '1c2932', '253941', '2a3a45', '527078', 'e9f0ed', 'a4b3ba', '8fa1aa', 'ffada4', 'f4cc8e', '9fc4d6', '283b25', '7ca65e', 'd4ffb0', '1e3017', '9ccfe0', 'ffb5a8', 'c9b6f2', '8fdcc4', '0d1014'];
  const sticker = ['fffdf6', 'fff0b8', 'fff4cc', 'ffe27a', '3753f0', '2740d4', '1f36d6', 'ffd93d', 'ff6ec7', '8ecbff', '5be3a4', 'b79cff', 'ff6b5e', '111111', '3b3b3b', '585858', '0a7a4f', '7a5200', 'c4261d', 'ff8fc2', 'ff9c7a', 'c8ff5c', 'fff7d6'];
  const known = new Map([...classic.map(h => [h, 'Classic']), ...sticker.map(h => [h, 'Sticker Bomb'])]);
  const hex = c => { const m = c.match(/[\\d.]+/g); if (!m || (m.length > 3 && Number(m[3]) === 0)) return null; const k = c.startsWith('color(srgb') ? 255 : 1; return m.slice(0, 3).map(n => Math.round(Number(n) * k).toString(16).padStart(2, '0')).join(''); };
  const leaks = [];
  for (const el of document.querySelectorAll('body, body *, dialog')) {
    const box = el.getBoundingClientRect(); if (!box.width || !box.height) continue;
    const s = getComputedStyle(el); if (s.visibility === 'hidden') continue;
    const props = { color: s.color, background: s.backgroundColor };
    for (const side of ['Top', 'Right', 'Bottom', 'Left']) if (parseFloat(s[`border${side}Width`]) > 0 && s[`border${side}Style`] !== 'none') props[`border${side}`] = s[`border${side}Color`];
    for (const [prop, value] of Object.entries(props)) { const h = hex(value); if (h && known.has(h)) leaks.push(`${el.tagName.toLowerCase()}.${el.className || el.id} ${prop} #${h} (${known.get(h)})`); }
  }
  return leaks;
}"""


def run_theme(playwright, theme, url_for, topic_id, report, out, scheme="light"):
    browser = playwright.chromium.launch(args=CHROMIUM_ARGS)
    context = browser.new_context(viewport={"width": 1440, "height": 900}, color_scheme=scheme)
    page = context.new_page()
    problems = []
    page.on("console", lambda m: problems.append(m.text) if m.type == "error" else None)
    page.on("pageerror", lambda e: problems.append(str(e)))
    font_requests = []
    page.on("request", lambda r: font_requests.append(r.url) if "/fonts/" in r.url else None)
    tag = f"[{theme}]" if theme != "orca-system" else f"[{theme}, browser {scheme}]"
    applied = sheet_for(theme, scheme)  # the sheet this choice shows

    page.add_init_script(f"try {{ if (!localStorage.getItem('agent-bridge-theme')) localStorage.setItem('agent-bridge-theme', '{theme}') }} catch (e) {{}}")
    page.goto(url_for)
    page.wait_for_selector("#sidebar .nav-item", timeout=15000)
    report.check(f"{tag} unlocks from the access link and clears the token from the URL", "access_token" not in page.url)
    report.check(f"{tag} theme applied", page.evaluate("document.documentElement.dataset.theme") == applied)
    sheets = page.evaluate("[...document.styleSheets].map(s => s.href)")
    report.check(f"{tag} exactly one theme sheet loaded, the one for this choice", [s for s in sheets if "/theme-" in (s or "")] == [f"{url_for.split('#')[0]}theme-{applied}.css"], str(sheets))
    report.check(f"{tag} the choice is saved by its own name", page.evaluate("localStorage.getItem('agent-bridge-theme')") == theme)
    until(page, "document.querySelectorAll('#sidebar .nav-item').length >= 6", 20000)

    labels = page.eval_on_selector_all("#sidebar .nav-item .grow", "els => els.map(e => e.textContent)")
    report.check(f"{tag} sidebar lists boards", "main" in labels and "release-checklist" in labels, str(labels))
    report.check(f"{tag} sidebar lists the topic", "release" in labels, str(labels))
    pairs = [l for l in labels if "·" in l]
    report.check(f"{tag} sidebar lists agent conversations by the broker's names, not the human's",
                 any(BRIDGE_NAME in l and KYOMI_NAME in l for l in pairs)
                 and not any("You" in l.split(" · ") or "user" in l for l in pairs), str(pairs))
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
    # AGB-60: the page shows the names the broker resolved and never an ID where a name exists; the exact
    # Agent ID is only in the details panel.
    sidebar_text = page.locator("#sidebar").inner_text()
    report.check(f"{tag} the sidebar shows the broker's name, not the routing ID", BRIDGE_NAME in sidebar_text and BRIDGE_ID not in sidebar_text, sidebar_text[:300])
    report.check(f"{tag} an agent the client has no name for shows the generated name, not its ID", KYOMI_NAME in sidebar_text and KYOMI_ID not in sidebar_text, sidebar_text[:300])
    authors = page.eval_on_selector_all("#dm-messages .author", "els => els.map(e => e.textContent)")
    thread_text = page.locator("#dm-view").inner_text()
    report.check(f"{tag} the conversation shows the agents' names, never an agent ID", bool(authors) and set(authors) <= {BRIDGE_NAME, KYOMI_NAME, UI_NAME} and not any(agent_id in thread_text for agent_id in AGENT_IDS), f"{authors} {thread_text[:300]}")
    detail_text = page.locator("#panel-body").inner_text()
    report.check(f"{tag} delivery details show the name and the exact Agent ID", BRIDGE_NAME in detail_text and BRIDGE_ID in detail_text, detail_text[:400])
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
    until(page, f"document.documentElement.dataset.theme === '{applied}' && AgentBridgeTheme.get() === '{theme}'", 8000)

    # fonts
    if theme == "sticker":
        until(page, "document.fonts.check('15px \"Bricolage Grotesque\"')", 8000)
        report.check(f"{tag} Sticker Bomb loads its self-hosted fonts", any("bricolage" in u for u in font_requests), str(font_requests))
    elif theme in ORCA_THEMES:
        until(page, "document.fonts.check('14px Geist')", 8000)
        report.check(f"{tag} Orca loads its self-hosted Geist font and no other", any("geist" in u for u in font_requests) and all("geist" in u for u in font_requests), str(font_requests))
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
    report.check(f"{tag} the saved theme is applied on the login page with a single sheet", page.evaluate("document.documentElement.dataset.theme") == applied and sum("/theme-" in (h or "") for h in page.evaluate("[...document.styleSheets].map(s => s.href)")) == 1)
    report.check(f"{tag} Lock clears the workspace and shows the login again", page.locator("#workspace").is_hidden() and page.locator("#sidebar .nav-item").count() == 0)

    csp = [p for p in problems if "Content Security Policy" in p or "Refused to" in p]
    report.check(f"{tag} no Content-Security-Policy violations", not csp, str(csp[:3]))
    other_errors = [p for p in problems if p not in csp and "net::ERR_ABORTED" not in p]
    report.check(f"{tag} no console or page errors", not other_errors, str(other_errors[:5]))
    context.close()
    browser.close()


ORCA_VALUES = {  # what the rendered page must paint, by sheet (DESIGN.md: Orca theme; tools/check_orca_tokens.py compares them with Orca itself)
    "orca": {"content": "rgb(21, 21, 21)", "sidebar": "rgb(42, 42, 42)", "topbar": "rgb(23, 23, 23)", "topbarH": "36px", "text": "rgb(240, 240, 240)", "btnRadius": "8px", "scheme": "dark"},
    "orca-light": {"content": "rgb(255, 255, 255)", "sidebar": "rgb(245, 245, 245)", "topbar": "rgb(255, 255, 255)", "topbarH": "36px", "text": "rgb(10, 10, 10)", "btnRadius": "8px", "scheme": "light"},
}


def run_theme_menu_section(playwright, url_for, report):
    """The Theme menu and live switching between all five choices with the browser in light mode:
    Classic is the default, nothing but the chosen sheet and its fonts is ever requested, and no pick reloads."""
    browser = playwright.chromium.launch(args=CHROMIUM_ARGS)
    context = browser.new_context(viewport={"width": 1440, "height": 900}, color_scheme="light")
    page = context.new_page()
    problems = []
    page.on("console", lambda m: problems.append(m.text) if m.type == "error" else None)
    page.on("pageerror", lambda e: problems.append(str(e)))
    requests = []
    page.on("request", lambda r: requests.append(r.url))
    tag = "[theme menu]"

    # Classic is the default: with nothing saved, the page opens in Classic and requests no Orca files.
    page.goto(url_for)
    page.wait_for_selector("#sidebar .nav-item", timeout=15000)
    until(page, "document.querySelectorAll('#sidebar .nav-item').length >= 6", 20000)
    report.check(f"{tag} Classic stays the default theme", page.evaluate("document.documentElement.dataset.theme") == "classic")
    report.check(f"{tag} Classic does not request any Orca sheet or the font", not any("orca" in u or "geist" in u for u in requests), str(requests))
    choices = page.eval_on_selector_all("#theme-menu [data-theme-choice]", "els => els.map(e => [e.dataset.themeChoice, e.textContent.trim()])")
    report.check(f"{tag} the Theme menu offers Classic, Sticker Bomb, Orca Dark, Orca Light and Orca (system)",
                 choices == [["classic", "Classic"], ["sticker", "Sticker Bomb"], ["orca", "Orca Dark"], ["orca-light", "Orca Light"], ["orca-system", "Orca (system)"]], str(choices))
    page.evaluate("window.__noReload = true")
    previous = "classic"
    for pick in ["orca", "orca-light", "orca-system", "sticker", "classic", "orca-system", "orca-light", "orca", "orca-system"]:
        before = len(requests)
        wanted = sheet_for(pick, "light")
        page.click("#theme-button")
        page.click(f"[data-theme-choice={pick}]")
        until(page, f"document.documentElement.dataset.theme === '{wanted}' && document.querySelectorAll('link[rel=stylesheet][href*=\"/theme-\"]').length === 1", 8000)
        if wanted in THEME_FONTS and THEME_FONTS[wanted]:
            until(page, "document.fonts.check('14px Geist')" if "geist-var.woff2" in THEME_FONTS[wanted] else "document.fonts.check('15px \"Bricolage Grotesque\"')", 8000)
        page.wait_for_timeout(150)
        new = requests[before:]
        sheets = page.evaluate("[...document.styleSheets].map(s => s.href || '').filter(h => h.includes('/theme-'))")
        report.check(f"{tag} {previous} > {pick}: exactly one theme sheet, {wanted}", len(sheets) == 1 and sheets[0].endswith(f"/theme-{wanted}.css"), str(sheets))
        asked = [u.rsplit("/", 1)[1] for u in new if "/theme-" in u]
        already = wanted == sheet_for(previous, "light")
        report.check(f"{tag} {previous} > {pick}: " + ("the sheet already applied is not requested again" if already else "only the new sheet is requested"), asked == ([] if already else [f"theme-{wanted}.css"]), str(asked))
        fonts = {u.rsplit("/", 1)[1] for u in new if "/fonts/" in u}
        report.check(f"{tag} {previous} > {pick}: only this theme's fonts are requested", fonts <= THEME_FONTS[wanted], str(fonts))
        report.check(f"{tag} {previous} > {pick}: no reload, the choice is remembered by its own name, the menu shows it",
                     page.evaluate(f"window.__noReload === true && localStorage.getItem('agent-bridge-theme') === '{pick}' && AgentBridgeTheme.get() === '{pick}' && document.querySelector('[data-theme-choice={pick}]').getAttribute('aria-checked') === 'true' && document.querySelectorAll('#theme-menu [aria-checked=true]').length === 1"))
        previous = pick
    stored = page.evaluate("Object.keys(localStorage).sort()")
    report.check(f"{tag} the browser stores only the theme and sidebar state", set(stored) <= {"agent-bridge-theme", "agent-bridge-sidebar"} and "agent-bridge-theme" in stored, str(stored))
    csp = [p for p in problems if "Content Security Policy" in p or "Refused to" in p]
    report.check(f"{tag} no Content-Security-Policy violations and no console or page errors", not problems and not csp, str(problems[:3]))
    context.close()
    browser.close()


def run_orca_section(playwright, url_for, topic_id, report, out, choice, scheme):
    """Checks specific to the Orca theme, for one choice (the dark theme, the light theme, or the system
    choice) with the browser in one appearance. An explicit choice is run in the opposite appearance, to
    show it ignores it. Runs before the per-theme sections so that it does not depend on how far they get."""
    applied = sheet_for(choice, scheme)
    tag = f"[orca section {choice}, browser {scheme}]"
    shot = f"orca-section-{choice}-{scheme}"
    browser = playwright.chromium.launch(args=CHROMIUM_ARGS)
    context = browser.new_context(viewport={"width": 1440, "height": 900}, color_scheme=scheme)
    page = context.new_page()
    problems = []
    page.on("console", lambda m: problems.append(m.text) if m.type == "error" else None)
    page.on("pageerror", lambda e: problems.append(str(e)))
    requests = []
    page.on("request", lambda r: requests.append(r.url))
    page.add_init_script(f"try {{ if (!localStorage.getItem('agent-bridge-theme')) localStorage.setItem('agent-bridge-theme', '{choice}') }} catch (e) {{}}")

    page.goto(url_for)
    page.wait_for_selector("#sidebar .nav-item", timeout=15000)
    until(page, "document.querySelectorAll('#sidebar .nav-item').length >= 6", 20000)
    report.check(f"{tag} the page shows the {applied} sheet", page.evaluate("document.documentElement.dataset.theme") == applied)
    sheets = page.evaluate("[...document.styleSheets].map(s => s.href || '')")
    report.check(f"{tag} only the {applied} theme sheet is loaded", [s for s in sheets if "/theme-" in s] == [f"{url_for.split('#')[0]}theme-{applied}.css"], str(sheets))
    asked = [u.rsplit("/", 1)[1] for u in requests if "/theme-" in u]
    report.check(f"{tag} only the {applied} sheet was requested", asked == [f"theme-{applied}.css"], str(asked))
    stored = page.evaluate("Object.keys(localStorage).sort()")
    report.check(f"{tag} the browser stores only the theme and sidebar state", set(stored) <= {"agent-bridge-theme", "agent-bridge-sidebar"} and "agent-bridge-theme" in stored, str(stored))
    until(page, "document.fonts.check('14px Geist')", 8000)
    fonts = [u for u in requests if "/fonts/" in u]
    report.check(f"{tag} Orca requests Geist and no other font", fonts and all("geist" in u for u in fonts), str(fonts))
    report.check(f"{tag} body text is set in Geist", page.evaluate("getComputedStyle(document.body).fontFamily").startswith("Geist"))
    report.check(f"{tag} the page tells the browser its colour scheme ({ORCA_VALUES[applied]['scheme']})", page.evaluate("getComputedStyle(document.documentElement).colorScheme") == ORCA_VALUES[applied]["scheme"])

    # Orca's own values on the rendered page (see DESIGN.md: Orca theme).
    values = page.evaluate("""() => {
      const css = (sel, prop) => getComputedStyle(document.querySelector(sel))[prop];
      return { content: css('body', 'backgroundColor'), sidebar: css('#sidebar', 'backgroundColor'), topbar: css('.topbar', 'backgroundColor'),
               topbarH: css('.topbar', 'height'), text: css('body', 'color'), btnRadius: css('#theme-button', 'borderRadius'),
               scheme: getComputedStyle(document.documentElement).colorScheme };
    }""")
    report.check(f"{tag} colours, header height and radius are Orca's", values == ORCA_VALUES[applied], str(values))

    # Every screen is styled by Orca and readable: nothing keeps another theme's colours.
    def screen(name, minimum=3):
        page.wait_for_timeout(500)
        rendered = page.evaluate(RENDERED_CONTRAST)
        report.check(f"{tag} {name}: {rendered['checked']} text runs, values, placeholders and pseudo-element texts meet 4.5:1", rendered["checked"] >= minimum and not rendered["fails"], str(rendered["fails"][:6]))
        leaks = page.evaluate(THEME_LEAK)
        report.check(f"{tag} {name}: no Classic or Sticker Bomb colour is used", not leaks, str(leaks[:6]))
        report.check(f"{tag} {name}: page does not scroll or overflow", not page.evaluate("document.documentElement.scrollHeight > innerHeight + 1 || document.documentElement.scrollWidth > innerWidth + 1"))
        page.screenshot(path=str(out / f"{shot}-{name.replace(' ', '-')}.png"))

    def go(route):
        page.evaluate(f"location.hash = {json.dumps(route)}")
        page.wait_for_timeout(700)

    dm_route = page.evaluate("document.querySelector('#sidebar .nav-item[data-route^=\"#/dm/\"]').dataset.route")
    go("#/board/release-checklist")
    screen("board", 20)
    fonts = page.evaluate("[...document.querySelectorAll('.board-document pre, .board-document code')].map(e => getComputedStyle(e).fontFamily)")
    report.check(f"{tag} board code blocks use Orca's monospace stack", fonts and all(f.startswith('"SF Mono"') for f in fonts), str(fonts))
    page.click("#board-menu-button")
    screen("board overflow menu")
    page.click("#board-archived-open")
    page.wait_for_selector("#archived-dialog[open]", timeout=5000)
    screen("archived boards dialog")
    page.keyboard.press("Escape")
    go("#/board/main")
    page.wait_for_selector("text=main has not been written yet.", timeout=8000)
    screen("empty board")
    go(f"#/topic/{topic_id}")
    screen("topic", 20)
    page.fill("#topic-body", "@")
    page.wait_for_selector("#topic-mention-pop:not([hidden]) [role=option]", timeout=5000)
    screen("mention popover")
    page.fill("#topic-body", "")
    page.once("dialog", lambda d: d.accept(f"empty-qa-{choice}-{scheme}"))
    page.click("#sidebar .nav-section__title .btn")
    page.wait_for_selector("text=No messages yet. Say something below.", timeout=10000)
    screen("empty topic")
    go(dm_route)
    screen("agent conversation")
    page.locator("#dm-messages .msg").first.hover()
    page.locator("#dm-messages .msg__actions button").first.click(force=True)
    page.wait_for_selector("#panel:not([hidden]) .detail-grid", timeout=10000)
    screen("details panel", 10)
    # an error inside the panel, then a toast when refreshing the open message fails
    page.route("**/api/messages/*", lambda route: route.fulfill(status=500, content_type="application/json", body='{"error":"unavailable","code":"unavailable"}'))
    page.locator("#dm-messages .msg").first.hover()
    page.locator("#dm-messages .msg__actions button").first.click(force=True)
    page.wait_for_selector(".toast--danger", timeout=8000)
    screen("error toast")
    page.unroute("**/api/messages/*")
    page.click("#panel-close")
    go("#/traffic")
    page.wait_for_selector("#message-list .message-row", timeout=15000)
    screen("All traffic", 20)
    page.fill("#machine-filter", "no-such-machine")
    page.click("#filters button[type=submit]")
    page.wait_for_selector("#empty-state:not([hidden])", timeout=10000)
    screen("empty All traffic")
    page.click("#reset-filters")
    page.click("#theme-button")
    screen("Theme menu")
    page.keyboard.press("Escape")
    page.keyboard.press("Control+k")
    page.wait_for_selector("#switcher[open]", timeout=5000)
    screen("quick switcher")
    page.keyboard.type("zzzz-no-such")
    page.wait_for_selector("#switcher .empty", timeout=5000)
    screen("empty quick switcher")
    page.keyboard.press("Escape")
    page.click("#lock-button")
    page.wait_for_selector("#login:not([hidden])", timeout=5000)
    screen("locked screen")

    csp = [p for p in problems if "Content Security Policy" in p or "Refused to" in p]
    report.check(f"{tag} no Content-Security-Policy violations", not csp, str(csp[:3]))
    # the one 500 is the deliberate failure this section injects to show the toast
    other_errors = [p for p in problems if p not in csp and "net::ERR_ABORTED" not in p]
    injected = [p for p in other_errors if "status of 500" in p]
    if injected:
        other_errors.remove(injected[0])
    report.check(f"{tag} no console or page errors", not other_errors, str(other_errors[:5]))
    context.close()
    browser.close()


def run_overlap_section(playwright, url_for, report):
    """Quick theme picks that overlap (AGB-66). The theme sheet responses are held by the test and
    released afterwards: in the order the picks were made, in the reverse order, and with the last
    pick's sheet failing. After the first two exactly one theme sheet may remain and it must be the
    last pick's. After a failure the page must be left with the theme that was applied, once."""
    browser = playwright.chromium.launch(args=CHROMIUM_ARGS)
    sequences = [("classic", ["sticker", "orca"]), ("classic", ["sticker", "classic"]),
                 ("orca", ["classic", "sticker"]), ("sticker", ["orca", "sticker"]),
                 ("classic", ["orca", "orca-light"]), ("orca-light", ["orca", "sticker"]), ("orca", ["orca-light", "orca"])]
    for start, picks in sequences:
        for order in ("in order", "reversed", "last sheet fails"):
            if order == "last sheet fails" and picks[-1] == start:
                continue  # the last pick is the sheet already applied: it loads nothing that could fail
            tag = f"[overlap {start} > {' > '.join(picks)}, sheets released {order}]"
            context = browser.new_context(viewport={"width": 1440, "height": 900}, color_scheme="light")
            page = context.new_page()
            problems = []
            page.on("console", lambda m: problems.append(m.text) if m.type == "error" else None)
            page.on("pageerror", lambda e: problems.append(str(e)))
            page.add_init_script(f"try {{ if (!localStorage.getItem('agent-bridge-theme')) localStorage.setItem('agent-bridge-theme', '{start}') }} catch (e) {{}}")
            page.goto(url_for)
            page.wait_for_selector("#sidebar .nav-item", timeout=15000)
            held = []
            page.route("**/theme-*.css", lambda route: held.append(route))
            page.evaluate("window.__themeChanges = []; document.addEventListener('themechange', e => window.__themeChanges.push(e.detail))")
            for pick in picks:
                page.evaluate(f"AgentBridgeTheme.set({json.dumps(pick)})")
            # A pick of the sheet that is already applied cancels the pending one and loads nothing; a pick of the one already loading loads nothing more.
            wanted, loading = 0, None
            for pick in picks:
                if pick == start:
                    loading = None
                elif pick != loading:
                    wanted, loading = wanted + 1, pick
            for _ in range(100):  # the page must be pumped for Playwright to deliver the held requests
                if len(held) == wanted:
                    break
                page.wait_for_timeout(100)
            report.check(f"{tag} exactly the sheets that are needed are requested, and are held", len(held) == wanted, f"{len(held)} held, {wanted} needed")
            report.check(f"{tag} the applied theme does not change while the sheets are loading", page.evaluate("document.documentElement.dataset.theme") == start)
            if order == "last sheet fails":
                for route in held[:-1]:
                    route.continue_()
                held[-1].abort()
                expected = start
                until(page, f"window.__themeChanges.length === {len(picks) + 1}", 8000)  # the revert
            else:
                for route in (held if order == "in order" else reversed(held)):
                    route.continue_()
                expected = picks[-1]
                until(page, f"document.documentElement.dataset.theme === '{expected}'", 8000)
            page.wait_for_timeout(500)  # a leftover sheet or a late handler would show by now
            links = page.evaluate("[...document.querySelectorAll('link[rel=stylesheet]')].map(l => [l.id, new URL(l.href).pathname])")
            themed = [link for link in links if link[1].startswith("/theme-")]
            report.check(f"{tag} exactly one theme sheet is in the page, the expected one", themed == [["theme-sheet", f"/theme-{expected}.css"]], str(links))
            applied = page.evaluate("[document.documentElement.dataset.theme, AgentBridgeTheme.get(), localStorage.getItem('agent-bridge-theme')]")
            report.check(f"{tag} the applied theme, the chosen theme and the saved theme are {expected}", applied == [expected, expected, expected], str(applied))
            if order == "last sheet fails":
                changes = page.evaluate("window.__themeChanges")
                report.check(f"{tag} the failure is announced once, with the applied theme", changes == picks + [expected], str(changes))
            if order == "last sheet fails":
                problems = [p for p in problems if "Failed to load resource" not in p]  # the failure this variant injects
            report.check(f"{tag} no console or page errors", not problems, str(problems[:3]))
            context.close()
    browser.close()


def run_url_theme_section(playwright, url_for, report):
    """AGB-68: the theme named in the address fragment. Every context is fresh (nothing saved) unless
    a check says otherwise, and every request is recorded."""
    base, _, link_fragment = url_for.partition("#")
    token = dict(part.split("=", 1) for part in link_fragment.split("&"))["access_token"]
    origin = base.rstrip("/")
    browser = playwright.chromium.launch(args=CHROMIUM_ARGS)
    problems = []

    def open_page(saved=None, scheme="light"):
        # The broker allows 100 requests a second in all; a page load and its unlock use many, so pace the loads.
        time.sleep(1.1)
        context = browser.new_context(viewport={"width": 1440, "height": 900}, color_scheme=scheme)
        page = context.new_page()
        requests = []
        page.on("request", lambda r: requests.append(r.url))
        # Where a token could really travel: the URL, a Referer, or a body. (Authorization to the broker's own origin is how the page authenticates.)
        page.token_seen = []
        page.on("request", lambda r: page.token_seen.append(r.url) if token in (r.url + (r.headers.get("referer") or "") + (r.post_data or "")) else None)
        page.on("console", lambda m: problems.append(m.text) if m.type == "error" else None)
        page.on("pageerror", lambda e: problems.append(str(e)))
        if saved:
            # Seed the saved theme only on the first document of this context, so later reloads show what the page itself stored.
            page.add_init_script(f"try {{ if (!sessionStorage.getItem('seeded')) {{ sessionStorage.setItem('seeded', '1'); localStorage.setItem('agent-bridge-theme', '{saved}') }} }} catch (e) {{}}")
        return context, page, requests

    def sheets_of(requests):
        return [u.rsplit("/", 1)[1] for u in requests if "/theme-" in u]

    def stored(page):
        return page.evaluate("Object.fromEntries(Object.keys(localStorage).map(k => [k, localStorage.getItem(k)]))")

    for theme in ALL_THEMES:
        shown = sheet_for(theme, "light")  # the sheet this choice shows with the browser in light mode
        tag = f"[url theme {theme}]"
        # 1. plain loopback URL: the theme is applied by the first paint, and no other theme's sheet is ever requested
        context, page, requests = open_page()
        page.goto(f"{base}#theme={theme}")
        page.wait_for_selector("#login:not([hidden])", timeout=8000)
        report.check(f"{tag} the plain URL opens in the theme before any input", page.evaluate("document.documentElement.dataset.theme") == shown)
        report.check(f"{tag} only that theme's sheet was requested", sheets_of(requests) == [f"theme-{shown}.css"], str(sheets_of(requests)))
        report.check(f"{tag} the loaded sheet is the theme's and it is the only one", [s.rsplit("/", 1)[1] for s in page.evaluate("[...document.styleSheets].map(s => s.href || '').filter(h => h.includes('/theme-'))")] == [f"theme-{shown}.css"])
        page.evaluate("document.fonts.ready.then(() => true)")
        fonts = {u.rsplit("/", 1)[1] for u in requests if "/fonts/" in u}
        report.check(f"{tag} only that theme's fonts were requested", fonts <= THEME_FONTS[shown] and (bool(fonts) or theme == "classic"), str(fonts))
        report.check(f"{tag} the choice is remembered and nothing else is stored", stored(page) == {"agent-bridge-theme": theme}, str(stored(page)))
        report.check(f"{tag} the theme is removed from the address", page.evaluate("location.hash") == "" and page.url == base, page.url)
        report.check(f"{tag} the menu shows it as chosen", page.evaluate(f"document.querySelector('[data-theme-choice={theme}]').getAttribute('aria-checked')") == "true")
        context.close()

        # 2. the private access link with the theme in it: unlocks, applies it, and keeps the token out of everything
        context, page, requests = open_page()
        page.goto(f"{base}#theme={theme}&access_token={token}")
        page.wait_for_selector("#sidebar .nav-item", timeout=15000)
        report.check(f"{tag} the access link with a theme unlocks the page in that theme", page.evaluate("document.documentElement.dataset.theme") == shown and page.locator("#workspace").is_visible())
        report.check(f"{tag} neither the token nor the theme stays in the address", "access_token" not in page.url and token not in page.url and "theme" not in page.url, page.url)
        report.check(f"{tag} only that theme's sheet was requested with the access link", sheets_of(requests) == [f"theme-{shown}.css"], str(sheets_of(requests)))
        report.check(f"{tag} the token is not in browser storage", set(stored(page)) <= {"agent-bridge-theme", "agent-bridge-sidebar"} and token not in json.dumps(stored(page)), str(list(stored(page))))
        report.check(f"{tag} the token is in no request URL, referer or body, and nothing left the page's origin", all(u.startswith(origin + "/") for u in requests) and not page.token_seen, str([u for u in requests if not u.startswith(origin + "/")]))
        report.check(f"{tag} no cookie holds the token", token not in json.dumps(context.cookies()))
        # 4. a menu choice afterwards is remembered, and a reload (the address no longer has the theme) keeps it
        other = {"classic": "orca", "sticker": "classic", "orca": "sticker", "orca-light": "orca", "orca-system": "sticker"}[theme]
        page.click("#theme-button")
        page.click(f"[data-theme-choice={other}]")
        until(page, f"document.documentElement.dataset.theme === '{other}'", 8000)
        report.check(f"{tag} choosing {other} from the menu is remembered", stored(page)["agent-bridge-theme"] == other)
        page.reload()
        page.wait_for_selector("#login:not([hidden])", timeout=8000)
        report.check(f"{tag} a reload shows the menu choice, not the link's theme", page.evaluate("document.documentElement.dataset.theme") == other and sheets_of(requests)[-1] == f"theme-{other}.css")
        # reopening a link that carries the theme selects it again (the documented rule)
        page.goto("about:blank")  # a new load: a change of the fragment alone is not one
        page.goto(f"{base}#theme={theme}")
        page.wait_for_selector("#login:not([hidden])", timeout=8000)
        report.check(f"{tag} reopening the link selects its theme again and remembers it", page.evaluate("document.documentElement.dataset.theme") == shown and stored(page)["agent-bridge-theme"] == theme)
        context.close()

    # 3. unknown, empty, repeated and wrongly cased values are ignored: the saved theme (or Classic) applies, nothing is stored
    for label, fragment in [("unknown", "theme=nope"), ("empty", "theme="), ("repeated", "theme=orca&theme=sticker"),
                            ("repeated same value", "theme=orca&theme=orca"), ("wrong case", "theme=ORCA"),
                            ("path-like", "theme=../style"), ("malformed escape", "theme=%E0%A4%A"),
                            ("near miss of a new name", "theme=orca-dark"), ("new name, wrong case", "theme=Orca-Light"),
                            ("repeated new names", "theme=orca-light&theme=orca-system"), ("new name with a suffix", "theme=orca-system%20")]:
        tag = f"[url theme {label}]"
        context, page, requests = open_page()
        page.goto(f"{base}#{fragment}")
        page.wait_for_selector("#login:not([hidden])", timeout=8000)
        report.check(f"{tag} Classic applies and nothing is stored", page.evaluate("document.documentElement.dataset.theme") == "classic" and stored(page) == {}, str(stored(page)))
        report.check(f"{tag} only the Classic sheet was requested", sheets_of(requests) == ["theme-classic.css"], str(sheets_of(requests)))
        context.close()
        context, page, requests = open_page(saved="sticker")
        page.goto(f"{base}#{fragment}")
        page.wait_for_selector("#login:not([hidden])", timeout=8000)
        report.check(f"{tag} the saved theme applies and stays saved", page.evaluate("document.documentElement.dataset.theme") == "sticker" and stored(page) == {"agent-bridge-theme": "sticker"}, str(stored(page)))
        report.check(f"{tag} only the saved theme's sheet was requested", sheets_of(requests) == ["theme-sticker.css"], str(sheets_of(requests)))
        context.close()
    # an unknown theme beside the access link still unlocks the page and still clears the token
    context, page, requests = open_page()
    page.goto(f"{base}#theme=nope&access_token={token}")
    page.wait_for_selector("#sidebar .nav-item", timeout=15000)
    report.check("[url theme unknown] the access link still unlocks, in Classic, with the token removed", page.evaluate("document.documentElement.dataset.theme") == "classic" and token not in page.url and stored(page) == {})
    context.close()

    # The access link with an ignored or odd theme part still unlocks and removes the token; theme.js neither mis-parses nor leaks it.
    for label, fragment, expected in [("repeated theme", f"theme=orca&theme=sticker&access_token={token}", "classic"),
                                      ("empty theme", f"theme=&access_token={token}", "classic"),
                                      ("odd separators", f"theme=orca&&&access_token={token}&", "orca"),
                                      ("percent-encoded theme", f"theme=%6Frca&access_token={token}", "orca"),
                                      ("orca-light", f"theme=orca-light&access_token={token}", "orca-light"),
                                      ("percent-encoded orca-light", f"access_token={token}&theme=orca%2Dlight", "orca-light"),
                                      ("repeated access_token", f"theme=orca&access_token={token}&access_token=bad", "orca"),
                                      ("access_token first", f"access_token={token}&theme=sticker", "sticker")]:
        tag = f"[url theme {label}]"
        context, page, requests = open_page()
        page.goto(f"{base}#{fragment}")
        page.wait_for_selector("#sidebar .nav-item", timeout=15000)
        applied = page.evaluate("document.documentElement.dataset.theme")
        report.check(f"{tag} unlocks and the token is not in the address", token not in page.url and "access_token" not in page.url, page.url)
        report.check(f"{tag} the theme is the one the rule gives", applied == expected, applied)
        report.check(f"{tag} token not in storage, cookies or any request URL, nothing left the origin",
                     token not in json.dumps(stored(page)) and token not in json.dumps(context.cookies())
                     and all(u.startswith(origin + "/") for u in requests) and not page.token_seen, str([u for u in requests if not u.startswith(origin + "/")]))
        context.close()
    # A separator the page does not parse: the whole fragment reads as one unknown theme, which is ignored; theme.js adds no change or leak.
    context, page, requests = open_page()
    page.goto(f"{base}#theme=orca;access_token={token}")
    page.wait_for_selector("#login:not([hidden])", timeout=8000)
    report.check("[url theme semicolon] an unparsed link is ignored: Classic, nothing stored, token in no request", page.evaluate("document.documentElement.dataset.theme") == "classic" and stored(page) == {} and not page.token_seen)
    context.close()

    # Hash routes are unaffected: a route opens, reloads and navigates as before; a theme link lands on the default board, as a reload does.
    context, page, requests = open_page()
    page.goto(f"{base}#theme=orca&access_token={token}")
    page.wait_for_selector("#sidebar .nav-item", timeout=15000)
    until(page, "document.querySelectorAll('#sidebar .nav-item').length >= 6", 20000)
    report.check("[url theme route] after the link is consumed the page is on a valid route (the default board, as after a reload)", page.evaluate("location.hash") in ("", "#/board/main") and page.locator("#boards-view").is_visible(), page.url)
    page.click('#sidebar .nav-item[data-route="#/board/release-checklist"]')
    page.wait_for_selector("#board-document table", timeout=15000)
    report.check("[url theme route] navigating to a route works after a theme link", page.evaluate("location.hash") == "#/board/release-checklist" and page.evaluate("document.documentElement.dataset.theme") == "orca")
    # pasting a theme fragment into the open page does not reload it: the theme does not change and the page stays usable
    page.evaluate("location.hash = '#theme=sticker'")
    until(page, "location.hash !== '#theme=sticker'", 8000)  # the page's own hashchange handler replaces it with the default route
    until(page, "!!document.querySelector('#boards-view:not([hidden])')", 8000)
    report.check("[url theme route] a theme fragment set on an open page leaves the theme and a usable workspace on the default board", page.evaluate("document.documentElement.dataset.theme") == "orca" and page.evaluate("location.hash") == "#/board/main" and page.locator("#workspace").is_visible() and page.locator("#sidebar .nav-item").count() >= 6, page.url)
    context.close()
    # A route and a theme in one fragment: only the theme part is removed; every other character stays as typed.
    for typed, left, expected in [("/board/release-checklist&theme=orca", "#/board/release-checklist", "orca"),
                                  ("theme=sticker&/board/release-checklist", "#/board/release-checklist", "sticker"),
                                  ("/board/a&theme=orca&b", "#/board/a&b", "orca"),
                                  ("/topic/a%20b%2Fc&theme=orca", "#/topic/a%20b%2Fc", "orca"),
                                  ("/board/x%26y&theme=orca", "#/board/x%26y", "orca"),
                                  ("/board/x&%74heme=orca", "#/board/x", "orca"),
                                  ("/board/x&theme=%6Frca&", "#/board/x&", "orca"),
                                  ("/board/x&&theme=orca&&", "#/board/x&&&", "orca"),
                                  ("theme=orca&?theme=sticker", "#&?theme=sticker", "orca"),
                                  ("theme=orca&?access_token=not-a-token", "#&?access_token=not-a-token", "orca"),
                                  ("?access_token=x&theme=orca", "", "orca"),
                                  ("theme=orca", "", "orca"),
                                  ("/board/release-checklist&theme=orca-light", "#/board/release-checklist", "orca-light"),
                                  ("theme=orca-light&/board/a&b", "#/board/a&b", "orca-light")]:
        context, page, requests = open_page()
        page.goto(f"{base}#{typed}")
        page.wait_for_selector("#login:not([hidden])", timeout=8000)
        report.check(f"[url theme route+theme {typed}] the address keeps every other character as typed, without the theme", page.evaluate("location.hash") == left, page.url)
        report.check(f"[url theme route+theme {typed}] the theme applies and is remembered", page.evaluate("document.documentElement.dataset.theme") == expected and stored(page) == {"agent-bridge-theme": expected})
        context.close()
    context, page, requests = open_page()
    page.goto(f"{base}#/board/release-checklist")
    page.wait_for_selector("#login:not([hidden])", timeout=8000)
    report.check("[url theme route] a route fragment without a theme is untouched", page.evaluate("location.hash") == "#/board/release-checklist" and stored(page) == {})
    context.close()

    # Storage that throws: the theme still applies to the page, with no error
    context, page, requests = open_page()
    page.add_init_script("Object.defineProperty(window, 'localStorage', { get() { throw new Error('blocked') } })")
    page.goto(f"{base}#theme=orca")
    page.wait_for_selector("#login:not([hidden])", timeout=8000)
    report.check("[url theme storage] with storage unavailable the theme still applies for the page", page.evaluate("document.documentElement.dataset.theme") == "orca" and sheets_of(requests) == ["theme-orca.css"])
    context.close()

    csp = [p for p in problems if "Content Security Policy" in p or "Refused to" in p]
    report.check("[url theme] no Content-Security-Policy violations", not csp, str(csp[:3]))
    other_errors = [p for p in problems if p not in csp and "net::ERR_ABORTED" not in p]
    report.check("[url theme] no console or page errors", not other_errors, str(other_errors[:5]))
    browser.close()


def run_system_section(playwright, url_for, report):
    """Orca (system): the dark or light Orca sheet by the browser's appearance (prefers-color-scheme),
    switching live in both directions without a reload, one sheet at a time. Every sheet response in the
    overlap checks is held by the test and released afterwards."""
    base, _, link_fragment = url_for.partition("#")
    token = dict(part.split("=", 1) for part in link_fragment.split("&"))["access_token"]
    root = base.rstrip("/")
    browser = playwright.chromium.launch(args=CHROMIUM_ARGS)
    problems = []

    def open_page(scheme, saved="orca-system"):
        time.sleep(1.1)  # the broker allows 100 requests a second in all
        context = browser.new_context(viewport={"width": 1440, "height": 900}, color_scheme=scheme)
        page = context.new_page()
        requests = []
        page.on("request", lambda r: requests.append(r.url))
        page.on("console", lambda m: problems.append(m.text) if m.type == "error" else None)
        page.on("pageerror", lambda e: problems.append(str(e)))
        if saved:
            page.add_init_script(f"try {{ if (!sessionStorage.getItem('seeded')) {{ sessionStorage.setItem('seeded', '1'); localStorage.setItem('agent-bridge-theme', '{saved}') }} }} catch (e) {{}}")
        return context, page, requests

    def sheets_in_page(page):
        return page.evaluate("[...document.querySelectorAll('link[rel=stylesheet]')].map(l => [l.id, new URL(l.href).pathname]).filter(l => l[1].startsWith('/theme-'))")

    def asked(requests):
        return [u.rsplit("/", 1)[1] for u in requests if "/theme-" in u]

    def one_sheet(page, name):
        return sheets_in_page(page) == [["theme-sheet", f"/theme-{name}.css"]]

    def state(page):
        return page.evaluate("[document.documentElement.dataset.theme, AgentBridgeTheme.get(), localStorage.getItem('agent-bridge-theme')]")

    # 1. A saved system choice resolves before first paint, with only the needed sheet requested.
    for scheme, sheet in [("light", "orca-light"), ("dark", "orca")]:
        tag = f"[system, browser {scheme}]"
        context, page, requests = open_page(scheme)
        page.goto(base)
        page.wait_for_selector("#login:not([hidden])", timeout=8000)
        report.check(f"{tag} a saved Orca (system) opens in {sheet} from the first paint, with that sheet only", state(page) == [sheet, "orca-system", "orca-system"] and one_sheet(page, sheet) and asked(requests) == [f"theme-{sheet}.css"], f"{state(page)} {asked(requests)}")
        report.check(f"{tag} the menu shows Orca (system) as chosen, not {sheet}", page.evaluate("document.querySelector('[data-theme-choice=orca-system]').getAttribute('aria-checked')") == "true" and page.evaluate("document.querySelector(`[data-theme-choice=${AgentBridgeTheme.themes.find(t => t.id === document.documentElement.dataset.theme).id}]`).getAttribute('aria-checked')") == "false")
        fonts = {u.rsplit("/", 1)[1] for u in requests if "/fonts/" in u}
        page.evaluate("document.fonts.ready.then(() => true)")
        report.check(f"{tag} only Geist is requested", {u.rsplit("/", 1)[1] for u in requests if "/fonts/" in u} <= {"geist-var.woff2"}, str(fonts))
        context.close()

    # 2. Following the appearance live, in both directions, with no reload, and the saved value stays Orca (system).
    context, page, requests = open_page("light")
    page.goto(f"{base}#access_token={token}")
    page.wait_for_selector("#sidebar .nav-item", timeout=15000)
    page.evaluate("window.__noReload = true; window.__changes = []; document.addEventListener('themechange', e => window.__changes.push(e.detail))")
    sequence = [("dark", "orca"), ("light", "orca-light"), ("dark", "orca"), ("light", "orca-light")]
    for scheme, sheet in sequence:
        before = len(requests)
        page.emulate_media(color_scheme=scheme)
        until(page, f"document.documentElement.dataset.theme === '{sheet}'", 8000)
        page.wait_for_timeout(300)
        report.check(f"[system live] the browser turns {scheme}: the page switches to {sheet} without a reload", page.evaluate("window.__noReload === true") and one_sheet(page, sheet), str(sheets_in_page(page)))
        report.check(f"[system live] after {scheme}: the choice and the saved value are still Orca (system), the menu agrees", state(page)[1:] == ["orca-system", "orca-system"] and page.evaluate("document.querySelector('[data-theme-choice=orca-system]').getAttribute('aria-checked')") == "true", str(state(page)))
        report.check(f"[system live] after {scheme}: only the new sheet was requested", asked(requests[before:]) in ([f"theme-{sheet}.css"], []), str(asked(requests[before:])))
    report.check("[system live] changing the appearance announces no theme change (the choice did not change)", page.evaluate("window.__changes") == [], str(page.evaluate("window.__changes")))
    stored = page.evaluate("Object.fromEntries(Object.keys(localStorage).map(k => [k, localStorage.getItem(k)]))")
    report.check("[system live] the browser stores only the theme name and the sidebar state", set(stored) <= {"agent-bridge-theme", "agent-bridge-sidebar"} and stored.get("agent-bridge-theme") == "orca-system", str(stored))
    # the live page is still readable in both appearances
    for scheme, sheet in [("dark", "orca"), ("light", "orca-light")]:
        page.emulate_media(color_scheme=scheme)
        until(page, f"document.documentElement.dataset.theme === '{sheet}'", 8000)
        page.wait_for_timeout(300)
        rendered = page.evaluate(RENDERED_CONTRAST)
        report.check(f"[system live] after the browser turns {scheme} the page meets 4.5:1 ({rendered['checked']} runs)", rendered["checked"] >= 3 and not rendered["fails"], str(rendered["fails"][:5]))
        report.check(f"[system live] after the browser turns {scheme} the page is painted in {sheet}", page.evaluate("getComputedStyle(document.body).backgroundColor") == ("rgb(21, 21, 21)" if sheet == "orca" else "rgb(255, 255, 255)"))
    # a reload keeps following it
    page.emulate_media(color_scheme="dark")
    page.reload()
    page.wait_for_selector("#login:not([hidden])", timeout=8000)
    report.check("[system live] a reload with the browser in dark applies Orca dark from the first paint", state(page) == ["orca", "orca-system", "orca-system"] and one_sheet(page, "orca"))
    context.close()

    # 3. Only the system choice follows the appearance: an explicit choice does not move when it changes.
    for choice in ["classic", "sticker", "orca", "orca-light"]:
        context, page, requests = open_page("light", saved=choice)
        page.goto(base)
        page.wait_for_selector("#login:not([hidden])", timeout=8000)
        first = sheet_for(choice, "light")
        before = len(requests)
        for scheme in ["dark", "light", "dark"]:
            page.emulate_media(color_scheme=scheme)
            page.wait_for_timeout(300)
        report.check(f"[system off] with {choice} chosen, the browser's appearance changing does not change the theme or request a sheet", state(page) == [first, choice, choice] and one_sheet(page, first) and asked(requests[before:]) == [], f"{state(page)} {asked(requests[before:])}")
        # choosing the system choice from here adopts the current appearance, and then follows it
        page.click("#theme-button")
        page.click("[data-theme-choice=orca-system]")
        now = "orca"  # the browser is dark now
        until(page, f"document.documentElement.dataset.theme === '{now}' && document.querySelectorAll('link[rel=stylesheet][href*=\"/theme-\"]').length === 1", 8000)
        report.check(f"[system off] choosing Orca (system) from {choice} with the browser dark shows Orca dark and saves Orca (system)", state(page) == [now, "orca-system", "orca-system"] and one_sheet(page, now), str(state(page)))
        page.emulate_media(color_scheme="light")
        until(page, "document.documentElement.dataset.theme === 'orca-light'", 8000)
        report.check(f"[system off] after choosing Orca (system) the page follows the appearance to Orca Light", one_sheet(page, "orca-light"))
        # leaving the system choice stops following it
        page.click("#theme-button")
        page.click("[data-theme-choice=sticker]")
        until(page, "document.documentElement.dataset.theme === 'sticker' && document.querySelectorAll('link[rel=stylesheet][href*=\"/theme-\"]').length === 1", 8000)
        before = len(requests)
        page.emulate_media(color_scheme="dark")
        page.wait_for_timeout(300)
        report.check(f"[system off] after leaving Orca (system) for Sticker Bomb the appearance no longer matters", state(page) == ["sticker", "sticker", "sticker"] and asked(requests[before:]) == [])
        context.close()

    # 4. Overlap: quick appearance changes and picks with the sheet responses held, released in order or reversed, or failing.
    cases = [
        ("flip dark, light, dark", "in order", ["dark", "light", "dark"], [], "orca"),
        ("flip dark, light, dark", "reversed", ["dark", "light", "dark"], [], "orca"),
        ("flip dark, light", "in order", ["dark", "light"], [], "orca-light"),
        ("flip dark, then pick Orca Light", "reversed", ["dark"], ["orca-light"], "orca-light"),
        ("flip dark, then pick Sticker Bomb", "in order", ["dark"], ["sticker"], "sticker"),
        ("pick Orca Dark, then flip dark", "in order", [], ["orca", "flip"], "orca"),
        ("flip dark, then pick Orca (system)", "reversed", ["dark"], ["orca-system"], "orca"),
    ]
    for label, order, flips, picks, expected in cases:
        tag = f"[system overlap {label}, sheets released {order}]"
        context, page, requests = open_page("light")
        page.goto(f"{base}#access_token={token}")
        page.wait_for_selector("#sidebar .nav-item", timeout=15000)
        held = []
        page.route("**/theme-*.css", lambda route: held.append(route))
        page.evaluate("window.__changes = []; document.addEventListener('themechange', e => window.__changes.push(e.detail))")
        for flip in flips:
            page.emulate_media(color_scheme=flip)
            page.wait_for_timeout(150)
        for pick in picks:
            if pick == "flip":
                page.emulate_media(color_scheme="dark")
                page.wait_for_timeout(150)
            else:
                page.evaluate(f"AgentBridgeTheme.set({json.dumps(pick)})")
                page.wait_for_timeout(150)
        report.check(f"{tag} the applied theme does not change while the sheets are loading", page.evaluate("document.documentElement.dataset.theme") == "orca-light")
        for _ in range(20):
            page.wait_for_timeout(100)
        for route in (held if order == "in order" else reversed(held)):
            route.continue_()
        until(page, f"document.documentElement.dataset.theme === '{expected}'", 8000)
        page.wait_for_timeout(500)
        report.check(f"{tag} exactly one theme sheet is in the page, {expected}", one_sheet(page, expected), str(sheets_in_page(page)))
        last_choice = ([p for p in picks if p != "flip"] or ["orca-system"])[-1]
        report.check(f"{tag} the choice and the saved value are {last_choice}", state(page)[1:] == [last_choice, last_choice], str(state(page)))
        context.close()

    # A sheet that fails to load while following the appearance leaves the applied theme, announced once (the existing failure rule).
    tag = "[system overlap failed sheet]"
    context, page, requests = open_page("light")
    page.goto(f"{base}#access_token={token}")
    page.wait_for_selector("#sidebar .nav-item", timeout=15000)
    held = []
    page.route("**/theme-orca.css", lambda route: held.append(route))
    page.evaluate("window.__changes = []; document.addEventListener('themechange', e => window.__changes.push(e.detail))")
    page.emulate_media(color_scheme="dark")
    for _ in range(100):
        if held:
            break
        page.wait_for_timeout(100)
    held[0].abort()
    until(page, "window.__changes.length === 1", 8000)
    page.wait_for_timeout(300)
    report.check(f"{tag} the applied sheet stays, once, and is the saved choice", state(page) == ["orca-light", "orca-light", "orca-light"] and one_sheet(page, "orca-light") and page.evaluate("window.__changes") == ["orca-light"], f"{state(page)} {page.evaluate('window.__changes')}")
    context.close()

    # 5. The link: #theme=orca-system opens by the appearance, and the token still never leaves the page.
    for scheme, sheet in [("light", "orca-light"), ("dark", "orca")]:
        tag = f"[system link, browser {scheme}]"
        context, page, requests = open_page(scheme, saved=None)
        seen = []
        page.on("request", lambda r: seen.append(r.url) if token in (r.url + (r.headers.get("referer") or "") + (r.post_data or "")) else None)
        page.goto(f"{base}#theme=orca-system&access_token={token}")
        page.wait_for_selector("#sidebar .nav-item", timeout=15000)
        stored = page.evaluate("Object.fromEntries(Object.keys(localStorage).map(k => [k, localStorage.getItem(k)]))")
        report.check(f"{tag} the access link opens in {sheet}, remembers Orca (system), requests only that sheet", state(page) == [sheet, "orca-system", "orca-system"] and asked(requests) == [f"theme-{sheet}.css"] and stored.get("agent-bridge-theme") == "orca-system", f"{state(page)} {asked(requests)} {stored}")
        report.check(f"{tag} the token and theme are gone from the address, storage, cookies and every request", "access_token" not in page.url and "theme" not in page.url and token not in json.dumps(stored) and token not in json.dumps(context.cookies()) and not seen and all(u.startswith(root + "/") for u in requests))
        context.close()

    csp = [p for p in problems if "Content Security Policy" in p or "Refused to" in p]
    report.check("[system] no Content-Security-Policy violations", not csp, str(csp[:3]))
    other_errors = [p for p in problems if p not in csp and "net::ERR_ABORTED" not in p and "Failed to load resource" not in p]
    report.check("[system] no console or page errors", not other_errors, str(other_errors[:5]))
    browser.close()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", required=True, type=Path)
    parser.add_argument("--output", type=Path)
    parser.add_argument("--theme", choices=ALL_THEMES + ["all"], default="all")
    args = parser.parse_args()
    try:
        from playwright.sync_api import sync_playwright
    except ImportError:
        print("Playwright for Python is not installed; this browser acceptance cannot run.", file=sys.stderr)
        return 2
    source = args.binary.resolve(strict=True)
    try:
        out = prepare_output(args.output, source)
    except OutputDirectoryError as error:
        print(error, file=sys.stderr)
        return 2
    binary = out / COPIED_BINARY
    if source != binary:  # --binary may be the copy an earlier run left here
        shutil.copy2(source, binary)
    binary.chmod(0o700)
    report = Report()
    stand_in_claude(out)
    broker = None
    listeners = []
    try:
        broker = Broker(binary, out)
        bind_sessions(broker, out, listeners)
        topic_id = seed(broker)
        credentials = broker.rpc("monitor_enable", port=0)
        themes = ALL_THEMES if args.theme == "all" else [args.theme]
        with sync_playwright() as playwright:
            run_url_theme_section(playwright, credentials["access_url"], report)
            run_theme_menu_section(playwright, credentials["access_url"], report)
            # an explicit Orca choice is run in the opposite appearance, to show it ignores the browser's; the system choice in both
            for choice, scheme in [("orca", "light"), ("orca-light", "dark"), ("orca-system", "dark"), ("orca-system", "light")]:
                if choice in themes:
                    run_orca_section(playwright, credentials["access_url"], topic_id, report, out, choice, scheme)
            if "orca-system" in themes:
                run_system_section(playwright, credentials["access_url"], report)
            run_overlap_section(playwright, credentials["access_url"], report)
            for theme in themes:
                for scheme in (["light", "dark"] if theme == "orca-system" else ["light"]):
                    run_theme(playwright, theme, credentials["access_url"], topic_id, report, out, scheme)
    finally:
        if broker:
            broker.close()
        for listener in listeners:
            listener.close()
    print(f"\nScreenshots and logs: {out}")
    if report.failures:
        print(f"\n{len(report.failures)} check(s) failed:", *report.failures, sep="\n  - ")
        return 1
    print("\nAll web interface browser checks passed.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
