#!/usr/bin/env python3
"""Compare the Orca themes' colours with the stylesheet the installed Orca application ships.

assets/web/theme-orca.css (dark) and theme-orca-light.css take their values from Orca's own design
tokens (the `:root` and `.dark` blocks and the colour palette in its renderer stylesheet, inside
app.asar). This script reads that stylesheet, read only, and checks that every token listed in
DESIGN.md (Orca theme) still has the value Orca ships, so a change in Orca shows up here.

A token is checked one of three ways: it is a token Orca ships (compared as written), it is a step of
Orca's palette, or it is derived by Orca's own rule from two shipped tokens (a mix, compared within one
step per channel for rounding). A few dark values were measured in Orca's window and a few are
Agent Bridge's own; those are listed and not compared.

Usage: python3 tools/check_orca_tokens.py [--asar /Applications/Orca.app/Contents/Resources/app.asar]
Exit status: 0 all match, 1 a mismatch, 2 Orca (or its stylesheet) was not found, so nothing was checked.
"""
import argparse
import json
import re
import struct
import sys
from pathlib import Path

DEFAULT_ASAR = "/Applications/Orca.app/Contents/Resources/app.asar"
WEB = Path(__file__).resolve().parent.parent / "assets" / "web"

# (token in our sheet) -> how Orca arrives at it.
#   ("token", name)                      Orca's own token, as shipped
#   ("palette", name)                    a step of Orca's colour palette
#   ("mix", foreground, percent, ground)  Orca's rule: `foreground` at `percent` over `ground` (tokens)
#   ("measured", note) / ("ours", note)   not comparable with the stylesheet
DARK = {
    "--bg": ("mix", "--foreground", 5, "--background"),  # --chat-canvas: --chat-canvas-mix 5%
    "--chrome": ("token", "--worktree-sidebar"),
    "--surface": ("token", "--editor-surface"),
    "--surface-2": ("token", "--secondary"),
    "--surface-3": ("mix", "--worktree-sidebar-foreground", 10, "--worktree-sidebar"),  # the active card
    "--topbar-bg": ("token", "--card"),
    "--field": ("measured", "Orca's search field and inputs, read from pixels of its window"),
    "--hover": ("token", "--worktree-sidebar-accent"),
    "--ring": ("token", "--ring"),
    "--border": ("token", "--border"),
    "--border-strong": ("token", "--input"),
    "--text": ("token", "--foreground"),
    "--text-2": ("token", "--muted-foreground"),
    "--text-3": ("ours", "the dimmest grey that keeps placeholders at 4.5:1 on the field; Orca has no third text tone"),
    "--accent": ("token", "--primary"),
    "--accent-hover": ("measured", "the primary button's hover, read from Orca's window"),
    "--accent-ink": ("token", "--primary-foreground"),
    "--accent-soft": ("ours", "white at 5%"),
    "--link": ("palette", "--color-blue-400"),
    "--ok": ("palette", "--color-green-500"),
    "--warn": ("palette", "--color-amber-400"),
    "--danger": ("palette", "--color-red-400"),
    "--info": ("palette", "--color-sky-400"),
    "--agent-0": ("palette", "--color-blue-400"),
    "--agent-1": ("palette", "--color-violet-400"),
    "--agent-2": ("palette", "--color-emerald-400"),
    "--agent-3": ("palette", "--color-amber-400"),
    "--agent-4": ("palette", "--color-rose-400"),
    "--agent-5": ("palette", "--color-sky-400"),
    "--you": ("token", "--primary"),
}
LIGHT = {
    "--bg": ("token", "--background"),
    "--chrome": ("token", "--worktree-sidebar"),
    "--surface": ("token", "--popover"),
    "--surface-2": ("token", "--secondary"),
    "--surface-3": ("mix", "--worktree-sidebar-foreground", 10, "--worktree-sidebar"),  # the active card
    "--topbar-bg": ("token", "--card"),
    "--field": ("token", "--background"),  # inputs are transparent over the canvas
    "--hover": ("token", "--worktree-sidebar-accent"),
    "--ring": ("token", "--ring"),
    "--border": ("token", "--border"),
    "--border-strong": ("token", "--input"),
    "--text": ("token", "--foreground"),
    "--text-2": ("token", "--muted-foreground"),
    "--text-3": ("token", "--muted-foreground"),
    "--accent": ("token", "--primary"),
    "--accent-hover": ("mix", "--primary", 90, "--background"),  # hover:bg-primary/90
    "--accent-ink": ("token", "--primary-foreground"),
    "--accent-soft": ("ours", "black at 5%"),
    "--link": ("palette", "--color-blue-700"),
    "--ok": ("palette", "--color-green-700"),
    "--warn": ("palette", "--color-amber-700"),
    "--danger": ("palette", "--color-red-700"),
    "--info": ("palette", "--color-sky-700"),
    "--agent-0": ("palette", "--color-blue-700"),
    "--agent-1": ("palette", "--color-violet-700"),
    "--agent-2": ("palette", "--color-emerald-700"),
    "--agent-3": ("palette", "--color-amber-700"),
    "--agent-4": ("palette", "--color-rose-700"),
    "--agent-5": ("palette", "--color-sky-700"),
    "--you": ("token", "--primary"),
}
# Orca's shared values, the same in both themes
SHARED = {
    "--topbar-h": "--app-header-height",
    "--font": "--app-font-family",
    "--mono": "--font-mono",
}


def asar_files(path):
    """The renderer stylesheets of an Electron asar archive: name -> text. Read only. Orca keeps its
    renderer assets unpacked, next to the archive, so those are read from there."""
    with open(path, "rb") as handle:
        prefix = handle.read(16)
        header_length = struct.unpack("<I", prefix[12:16])[0]
        header = json.loads(handle.read(header_length))
        base = 8 + struct.unpack("<I", prefix[4:8])[0]

        def walk(node, prefix_path):
            for name, entry in node.get("files", {}).items():
                full = f"{prefix_path}/{name}"
                if "files" in entry:
                    yield from walk(entry, full)
                else:
                    yield full, entry

        for name, entry in walk(header, ""):
            if not (name.startswith("/out/renderer/assets/") and name.endswith(".css")):
                continue
            if entry.get("unpacked"):
                data = (Path(str(path) + ".unpacked") / name.lstrip("/")).read_bytes()
            else:
                handle.seek(base + int(entry["offset"]))
                data = handle.read(int(entry["size"]))
            yield name, data.decode("utf-8", "replace")


def declarations(css, selector):
    """The custom properties set by every rule of exactly `selector`, later rules over earlier ones."""
    found = {}
    for block in re.finditer(r"(?<![\w.:\-\\])" + re.escape(selector) + r"\{([^{}]*)\}", css):
        for name, value in re.findall(r"(--[\w-]+):([^;]+)", block.group(1)):
            found[name] = value.strip()
    return found


def parse_color(text):
    text = text.strip().lower()
    match = re.fullmatch(r"#([0-9a-f]{3,8})", text)
    if not match:
        raise ValueError(f"not a hex colour: {text!r}")
    digits = match.group(1)
    if len(digits) in (3, 4):
        digits = "".join(c * 2 for c in digits)
    channels = [int(digits[i:i + 2], 16) for i in range(0, len(digits), 2)]
    return channels + [255] * (4 - len(channels))


def hex_of(channels):
    return "#" + "".join(f"{c:02x}" for c in channels)


def resolve(tokens, name, seen=()):
    value = tokens[name]
    reference = re.fullmatch(r"var\((--[\w-]+)(?:,[^)]*)?\)", value)
    if reference:
        if reference.group(1) in seen:
            raise ValueError(f"{name} refers to itself")
        return resolve(tokens, reference.group(1), seen + (name,))
    return value


def theme_tokens(path, selector):
    css = path.read_text()
    block = re.search(re.escape(selector) + r"\s*\{(.*?)\n\}", css, re.S)
    if not block:
        raise SystemExit(f"{path.name}: no {selector} block")
    return {name: value.strip() for name, value in re.findall(r"(--[\w-]+):\s*([^;]+);", re.sub(r"/\*.*?\*/", "", block.group(1), flags=re.S))}


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--asar", default=DEFAULT_ASAR, type=Path)
    args = parser.parse_args()
    if not args.asar.is_file():
        print(f"Orca is not installed at {args.asar}; nothing was checked.", file=sys.stderr)
        return 2
    sheet = next((css for _, css in asar_files(args.asar) if "--worktree-sidebar-accent" in css and "--color-blue-400" in css), None)
    if sheet is None:
        print("Orca's renderer stylesheet with the design tokens was not found; nothing was checked.", file=sys.stderr)
        return 2
    palette = {name: value for name, value in re.findall(r"(--color-[\w-]+):(#[0-9a-fA-F]{3,8})", sheet)}
    root = declarations(sheet, ":root")
    dark = {**root, **declarations(sheet, ".dark")}
    light = {**root}
    failures, checked, skipped = [], 0, []

    for label, file, selector, table, orca in [
        ("Orca dark", "theme-orca.css", ':root[data-theme="orca"]', DARK, dark),
        ("Orca light", "theme-orca-light.css", ':root[data-theme="orca-light"]', LIGHT, light),
    ]:
        ours = theme_tokens(WEB / file, selector)
        orca = {**orca, **palette}
        for token, how in table.items():
            if how[0] in ("measured", "ours"):
                skipped.append(f"{label} {token}: {how[0]} ({how[1]})")
                continue
            try:
                actual = ours[token]
                if how[0] in ("token", "palette"):
                    expected = resolve(orca, how[1])
                    ok = parse_color(actual) == parse_color(expected)
                    shown = f"{how[1]} = {expected}"
                else:
                    _, fg_name, percent, ground_name = how
                    fg, ground = parse_color(resolve(orca, fg_name)), parse_color(resolve(orca, ground_name))
                    mixed = [round(f * percent / 100 + g * (100 - percent) / 100) for f, g in zip(fg[:3], ground[:3])]
                    ok = all(abs(a - b) <= 1 for a, b in zip(parse_color(actual)[:3], mixed))
                    shown = f"{fg_name} {percent}% over {ground_name} = {hex_of(mixed)}"
            except (KeyError, ValueError) as error:
                failures.append(f"{label} {token}: cannot compare ({error})")
                continue
            checked += 1
            if not ok:
                failures.append(f"{label} {token}: theme has {actual}, Orca ships {shown}")
        for token, name in SHARED.items():
            checked += 1
            expected = resolve(orca, name)
            expected = re.sub(r"\s+", " ", expected)
            actual = re.sub(r"\s+", " ", ours[token])
            if actual != expected:
                failures.append(f"{label} {token}: theme has {actual}, Orca ships {name} = {expected}")
        radius = resolve(orca, "--radius")
        checked += 1
        if radius != ".625rem" or ours["--r-lg"] != "10px":
            failures.append(f"{label} --r-lg: theme has {ours['--r-lg']}, Orca ships --radius = {radius}")
        shadow = declarations(sheet, ":root").get("--shadow-floating") or re.search(r"--shadow-floating:([^;}]+)", sheet).group(1)
        checked += 1
        if shadow.strip() not in ours["--shadow-pop"]:
            failures.append(f"{label} --shadow-pop: theme has {ours['--shadow-pop']}, Orca ships --shadow-floating = {shadow.strip()}")

    for line in skipped:
        print(f"not compared: {line}")
    print(f"{checked} values compared with {args.asar}")
    if failures:
        print(f"\n{len(failures)} value(s) differ from Orca's shipped stylesheet:", *failures, sep="\n  - ")
        return 1
    print("Every checked value matches Orca's shipped stylesheet.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
