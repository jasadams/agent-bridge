# Design System — Agent Bridge Monitor

The web interface served by `agent-bridge web enable`. Source of truth for its visual and
interaction decisions. It ships five choices on one layout (three looks, one of them in a dark and a light
variant, and a choice that picks between those two):

- **Classic** (default): the look of v0.5.0, kept as the base tokens in `assets/web/style.css`.
- **Sticker Bomb** (opt-in, from **Theme** in the top bar): the neo-brutalist identity described
  below, implemented in `assets/web/theme-sticker.css`.
- **Orca Dark** (opt-in, from **Theme**; id `orca`): matches the Orca application's own dark appearance, so
  the page looks like part of Orca when shown in a browser tab beside agent chats. Implemented in
  `assets/web/theme-orca.css`; see "Orca theme".
- **Orca Light** (opt-in; id `orca-light`): the same for Orca's light appearance, in
  `assets/web/theme-orca-light.css`.
- **Orca (system)** (opt-in; id `orca-system`): Orca Dark or Orca Light, whichever the browser's appearance
  asks for, switching live. It has no stylesheet of its own.

Layout, shell and interaction rules apply to all of them. Colour, type, shape, motion and logo sections
describe Sticker Bomb unless they say otherwise; the Orca themes are described under "Orca theme". Two other identities (an editorial "paper" theme
and a teletext theme) were explored and rejected; they are not part of the product.

## Product Context
- **What this is:** A Slack-like local monitor for agents: read shared boards, follow topics,
  watch agent-to-agent direct messages, and debug delivery.
- **Who it's for:** Developers running several coding agents (Claude, Codex, OpenCode) across
  one or more machines.
- **Space/industry:** Developer tools; chat and observability. Peers: Slack, Linear, Discord.
- **Project type:** Dense web app (chat + documents + a debug table). Plain HTML/CSS/JS, no
  build step, assets embedded in the Rust binary (`assets/web/`).

## Aesthetic Direction
- **Direction:** Neo-brutalist, playful. A workspace that is allowed to be fun. Agents are
  characters, not log lines.
- **Decoration level:** Expressive but flat. Thick outlines, hard offset shadows, flat colour.
  No gradients, no blur, no soft shadows, anywhere.
- **Mood:** Friendly, loud, confident. Very hard to mistake for Slack.
- **Why it was chosen:** It was picked over an editorial paper direction (The Dispatch) and a
  teletext direction (Page 100) as the one that makes a tool for watching machines feel
  approachable.

## The rules that make it recognisable
1. **Outline + hard shadow.** Every raised surface has a 2px `#111` outline. Interactive and
   floating things also get a hard offset shadow (`3px 3px 0 #111`, `4px`/`5px`/`6px` for larger
   surfaces). Shadows never blur.
2. **Buttons physically press.** Hover lifts (`translate(-1px,-1px)`, shadow grows to 4px);
   active presses flat (`translate(3px,3px)`, shadow 0). 80 ms.
3. **Everyone gets a colour.** Each agent has a flat colour used for its avatar and its name
   sticker in message headers. You are electric blue. Colours are assigned per agent and stay
   stable.
4. **Voice.** What a *person* writes is Bricolage Grotesque. What an agent or the system says
   about itself (handles, ids, times, delivery states, page titles' sub-lines, code, keyboard
   hints) is Space Mono. Agents are shown by **handle** (`claude-bridge`), the same string you
   `@mention`. Exception: mentions themselves are set in the sans, because Space Mono's `@`
   reads as an "a" at small sizes.
5. **Blue is the one action colour.** Electric blue is for the primary button, focus ring and
   "you". Yellow is the "you are here" and highlight colour. Pink marks a mention of you.

## Typography
- **UI / prose / headings:** Bricolage Grotesque (variable, 200–800). Headings are 800 weight,
  tracking about -0.035em. Body 400–600.
- **Machine facts / code / labels:** Space Mono 400 and 700.
- **Loading:** embedded in the executable and served from `/fonts/` (`bricolage-var.woff2` 41 KB,
  `space-mono-400/700` ~33 KB total), `font-display: swap`, and requested only when Sticker Bomb is
  selected. SIL Open Font License; the full licence texts ship in `assets/web/fonts/` and are listed in
  `THIRD_PARTY_NOTICES.md`. No CDN, so it works offline on a LAN. Classic uses system fonts.
- **Scale (tokens):** `--t-xs` 11.5 · `--t-sm` 13 · `--t-md` 15 (base) · `--t-lg` 17 ·
  `--t-xl` 24. Display sizes: page titles 24, design-system hero 56. Tabular figures on.

## Color
- **Approach:** Expressive, but every colour is a flat fill with a black outline.
- **Light only.** (No dark Sticker Bomb is designed; if one is added it needs its own
  outline/shadow logic, since black-on-black loses the whole effect. Classic is the dark option.)

| Token | Hex | Use |
|---|---|---|
| `--bg` | `#fffdf6` | Content |
| `--chrome` | `#fff0b8` | Sidebar (topbar is `--sun`) |
| `--surface` / `-2` / `-3` | `#ffffff` / `#fff4cc` / `#ffe27a` | Panels / hover / inline code, kbd |
| `--border` | `#111111` | Every outline |
| `--text` / `-2` / `-3` | `#111111` / `#3b3b3b` / `#585858` | Primary / secondary / tertiary |
| `--accent` | `#3753f0` | Electric blue: primary action, focus ring, "you". Hover `#2740d4` |
| `--link` / `--info` | `#1f36d6` | Links |
| `--ok` / `--warn` / `--danger` | `#0a7a4f` / `#7a5200` / `#c4261d` | Text-grade state colours |
| `--sun` / `--pink` / `--sky` / `--mint` / `--lilac` / `--coral` | `#ffd93d` / `#ff6ec7` / `#8ecbff` / `#5be3a4` / `#b79cff` / `#ff6b5e` | Flat fills: active item, mention-of-you, state badges, section tags, agent colours |

- **Delivery badges:** black text on a flat fill with a 2px outline: delivered `--mint`,
  pending `--sun`, failed `--coral`, unknown `--sky`.
- **Agent colours:** six flats, `--agent-0` to `--agent-5` (pink `#ff8fc2`, sky `#8ecbff`, lilac
  `#b79cff`, sun `#ffd93d`, mint `#5be3a4`, orange `#ff9c7a`). An agent's colour is chosen by hashing
  its identity (`AB.colorClass` in `util.js`) and applied with a class (`.agent-c0` to `.agent-c5`), so
  it is stable across reloads and machines. You are electric blue (`--you`) with white initials. Always
  black text on the pastel agent colours.
- **Contrast (WCAG, measured on the rendered tokens):** all of `--text`, `--text-2`, `--text-3`,
  `--link`, `--accent`, `--ok`, `--warn`, `--danger` are at least 4.5:1 on `--chrome`, `--bg`,
  `--surface` and `--surface-2`; white on `--accent` is 5.7:1. Re-check if any neutral changes.

## Spacing
- **Base unit:** 4 px. **Density:** compact-comfortable.
- **Scale:** `--s-1` 4 · `--s-2` 8 · `--s-3` 12 · `--s-4` 16 · `--s-5` 24 · `--s-6` 32.

## Layout
- **Approach:** Grid-disciplined app shell. The viewport is fixed (`100dvh`); the page never
  scrolls. Only `.scroll` panes scroll, each independently.
- **Shell:** topbar · sidebar (264 px, collapses to a 68 px icon rail, Ctrl/⌘+B, remembered) ·
  main (header, scrolling content, pinned composer) · optional right panel (thread / delivery
  inspector, 380 px).
- **Sidebar order:** Boards, Topics, Direct messages (agent ↔ agent, read-only). Pinned at the
  bottom: All traffic (debug), Design system. Section headers are colour pills (pink / sky /
  mint). Collapsed rail shows letter tiles for boards and topics and paired agent avatars for DMs.
- **Content widths:** board documents 860 px; messages fill the pane.
- **Radius:** `--r-sm` 6 · `--r-md` 10 · `--r-lg` 14 · `--r-pill` for counts, badges, chips, tags.
- **Responsive:** below 780 px the sidebar is an overlay drawer; below 1100 px the right panel
  overlays.

## Motion
- **Approach:** Playful but fast. 80 ms translate on button hover/press and nav hover; 140 ms
  `cubic-bezier(.2, .7, .2, 1)` for sidebar collapse, drawer and toasts. Nothing bounces.
  `prefers-reduced-motion` disables all of it.

## Components
Button (primary / default / quiet / danger; one size scale) · icon button · input / select /
textarea (outlined, hard shadow) · tag · delivery badge · count · presence dot · avatar (square,
outlined) · panel · `⋯` overflow menu · toast (black, pink shadow) · banner (lilac, persistent) ·
empty state · markdown prose · code block (black, lime text, pink shadow, copy) · table (yellow
header) · message row (grouping, name sticker, hover actions, thread link) · composer (`@` mention
autocomplete, Enter to send) · sidebar item (full row + rail tile) · quick switcher (Ctrl/⌘+K) ·
board answer field (blue left bar).

Rules: one primary button per surface; rare actions go in a `⋯` menu; status goes through
toasts and banners, never stacked paragraphs; raw text is one click away (View source).

## Logo
An arch-shaped bridge character: a yellow arch with two legs and two eyes, standing on a pink
pavement, on a blue rounded sticker with a black outline and a hard offset shadow
(`logos/sticker.svg`). Wordmark: "agent" on a tilted pink sticker beside "bridge" in Bricolage
800. Always keep the black outline and the offset shadow; eyes look slightly up and to the right.
Reads down to 16 px. Also the favicon and the intended loading/empty-state mascot.

## Orca theme
Flat, quiet, Orca's own look, not a new identity. No outlines-with-shadows, no per-agent stickers; a
monochrome primary button, hairline borders, system-style controls. It comes as a dark and a light
variant with the same shapes, typeface and component decoration; only the values differ. Values were
read from the Orca application's shipped renderer stylesheet (its `:root` and `.dark` design tokens and
colour palette), and the dark ones were also checked against pixels of its real window.
`tools/check_orca_tokens.py` reads that stylesheet from an installed Orca (read only) and compares every
token below with it; run it after Orca is updated. It exits 2, having checked nothing, where Orca is not
installed.

| Surface | Orca dark | Orca light | Agent Bridge token |
|---|---|---|---|
| Content canvas (chat pane) | `#151515` (`--foreground` 5% over `--background`; measured) | `#ffffff` (`--background`) | `--bg` |
| Sidebar | `#2a2a2a` (`--worktree-sidebar`, measured) | `#f5f5f5` (`--worktree-sidebar`) | `--chrome` |
| Header / tab bar | `#171717` (`--card`), 36 px (`--app-header-height`) | `#ffffff` (`--card`), 36 px | `.topbar` (`--topbar-bg`), `--topbar-h` |
| Composer, popovers | `#1e1e1e` (`--editor-surface`, measured) | `#ffffff` (`--popover`) | `--surface` |
| Cards, hover | `#262626` (`--secondary`) | `#f5f5f5` (`--secondary`) | `--surface-2` |
| Selected row | `#3e3e3e` (active card: `--worktree-sidebar-foreground` 10% over `--worktree-sidebar`) | `#dedede` (the same rule) | `--surface-3` |
| Search field, inputs | `#343434` (measured) | `#ffffff` over the canvas with an `--input` border | `--field` |
| Row hover | `#353535` (`--worktree-sidebar-accent`) | `#eaeaea` (`--worktree-sidebar-accent`) | `--hover` |
| Focus ring | `#737373` (`--ring`) | `#a1a1a1` (`--ring`) | `--ring` |
| Borders / input borders | white at 7% / 15% (`--border`, `--input`) | `#e5e5e5` / `#e5e5e5` | `--border` / `--border-strong` |
| Text / muted text | `#f0f0f0` / `#acacac` (`--foreground`, `--muted-foreground`) | `#0a0a0a` / `#696969` | `--text` / `--text-2` |
| Primary button | `#e5e5e5` on `#171717` (`--primary`); hover `#d4d4d4` (measured) | `#171717` on `#fafafa` (`--primary`); hover `#2e2e2e` (`--primary` at 90% over `--background`) | `--accent` / `--accent-ink` / `--accent-hover` |
| Links, unread, mentions | blue-400 `#54a2ff` | blue-700 `#1447e6` | `--link` |
| State colours | green-500 `#00c758`, amber-400 `#fcbb00`, red-400 `#ff6568`, sky-400 `#00bcfe` | green-700 `#008138`, amber-700 `#b75000`, red-700 `#bf000f`, sky-700 `#0069a4` | `--ok` `--warn` `--danger` `--info` |
| Typeface | Geist (variable) | the same | `--font`; `geist-var.woff2`, same file Orca ships |
| Machine text | SF Mono stack (`--font-mono`) | the same | `--mono` |
| Radius | base `0.625rem` = 10 px; sm 6, md 8, xl 14 | the same | `--r-sm` 6 · `--r-md` 8 · `--r-lg` 10 · `--r-xl` 14 (composer, dialogs) |
| Floating shadow | `--shadow-floating` `0 10px 24px #0000002e` | the same | `--shadow-pop` |
| Scrollbar | thin, white 15% thumb | thin, `--muted-foreground` at 34% (Orca's `scrollbar-sleek`) | `::-webkit-scrollbar` |

Agent colours are Orca's blue, violet, emerald, amber, rose and sky: the 400 steps on dark and the 700
steps on light, except amber, which is the 800 step there because the 700 is 4.3:1 on the avatar tint (`--agent-0` to `--agent-5`). Orca's own light "unread" blue is blue-500 `#3080ff`, which
is 3.7:1 on white, so the light theme uses blue-700 for links, as it does for the state colours: the
700 steps are the ones that reach 4.5:1 on the grey rows too. Where a shipped tone cannot be read on a
shipped surface the theme does not invent a colour: the light avatar tint is 12% (the dark one is 20%)
because the palette's 700 text colours need the lighter ground, and the glyph and tag inside the
selected row are set in the full text colour because the muted tone is 4.1:1 on Orca's active card.
The two values that are Agent Bridge's own are the dark `--text-3` (`#a0a0a0`, the dimmest grey that
keeps placeholders at 4.5:1 on the `#343434` field; Orca has no third text tone, and light uses
`--muted-foreground` for it) and the `--accent-soft` tints.

**Orca (system).** Chooses `orca` or `orca-light` from the browser's `prefers-color-scheme` when the page
loads (before first paint) and again whenever it changes, loading only the sheet it needs and swapping it
in without a reload. What the browser reports inside Orca, as probed:

- **Verified:** with Orca's own Appearance setting on System and macOS in its Light appearance, a page in an Orca browser tab (Orca's Electron 43.8.10) reports `prefers-color-scheme: light`.
- **Not verified:** whether the tab mirrors Orca's own setting or only macOS. Orca's main process sets Electron's `nativeTheme.themeSource` from its Theme setting (System, Light or Dark; read in the application's `out/main/index.js`), so a tab is expected to follow Orca's own setting, but this was not tested with Orca set explicitly to Dark or Light, and with System and a Light macOS both explanations predict light. Orca can also override the colour scheme of a single site in a browser tab, which would apply to this page like any other. Orca (system) follows whatever the browser reports, so it behaves correctly either way; both appearances and a live change are covered by emulation in the browser test.


**Checks.** Contrast and palette are checked by the browser test, for the dark theme, the light theme and
the system choice (the system choice with the browser in each appearance; an explicit choice is also run
in the opposite appearance, to show it ignores it), on these screens: the board (with its overflow menu, archived-boards dialog and archive confirmation dialog), the empty board, the topic (with the mention popover), the empty topic, an agent conversation, delivery details, an error toast, All traffic and its no-match state, the Theme menu, the quick switcher and its no-match state, and the locked screen. On each it measures every visible text node, input value, placeholder and `::before`/`::after` text against what is painted behind it (translucent colours, including `color-mix()` results, blended, opacity applied) at 4.5:1, and fails if any element uses a Classic or Sticker Bomb palette colour from the lists in the test (the palettes of their sheets, less pure white, which Orca also uses). Not measured: icons drawn as shapes or images, and fully transparent elements (the message actions before hover). Agent conversations have no empty state in the interface, so none is checked.

## Implementation rules (security and structure)
- **Untrusted text.** Agent-authored text is untrusted. Render it with `createElement` /
  `textContent` from a parsed node tree (`markdown.js`). Never `innerHTML`.
- **No inline code or styles.** The page's Content-Security-Policy allows scripts, styles, fonts,
  images and connections from the broker only: no `unsafe-inline`, no `eval`. So: no `<style>`, no
  `style=` attributes, no inline scripts or handlers, and no `setAttribute("style", …)`. Colour per
  agent is a class, not an inline style; script-driven layout uses the CSS object model
  (`element.style.height = …`). The theme is applied by the external, blocking `theme.js`.
  `tests/monitor_http.rs` fails the build if any of this regresses.
- **Themes are files.** `style.css` holds structure and the Classic tokens; each `theme-*.css` overrides
  tokens and decorates components, scoped under `[data-theme="…"]`. Adding a theme means adding one
  sheet and an entry in `theme.js`, a route in `src/monitor.rs`, and a menu item. Only the selected
  sheet is loaded. `data-theme` on `<html>` names the sheet that is applied; a choice that is not a
  sheet of its own (Orca (system)) is resolved to one by `theme.js`, which remembers the choice, not the
  sheet.
- **Storage.** The page stores only the theme choice (its own name, so `orca-system`, never the
  appearance it resolved to) and the sidebar state, never tokens or drafts.
- **Theme from the link.** `#theme=<classic|sticker|orca|orca-light|orca-system>` in the address fragment selects the theme
  at load: `theme.js` reads it before first paint (so only that sheet is requested), remembers it like
  a menu choice and removes it from the address. For the access link join it with the token:
  `#theme=orca&access_token=<token>`. Anything but exactly one known name is ignored and stores
  nothing. It is a fragment, not a query, so it is never sent to the broker (which rejects unknown
  queries) and cannot interfere with the token handling.
- **Board from the link.** The existing route `#/board/<name>` can sit in the same fragment as the theme and
  the token, in any order: `#/board/kyomi&theme=orca&access_token=<token>`. Build it by adding the
  percent-encoded route (and theme) as `&`-separated parts to the printed access link. The token is removed
  as always; the route stays in the address and the page opens that board after unlocking. A link that
  stays locked, a repeated route or a board name containing `access_token` leaves no route. See
  `docs/web.md`.

## Decisions Log
| Date | Decision | Rationale |
|---|---|---|
| 2026-11-10 | Slack-style shell: boards, topics, agent DMs; traffic demoted to a debug entry | "Slack for agents"; Messages page was a raw log that did not scroll |
| 2026-11-10 | Rejected: warm graphite + International Orange + Geist | Read as a generic dark dev tool; same look the product already had |
| 2026-11-10 | Explored three identities: The Dispatch (editorial), Page 100 (teletext), Sticker Bomb (neo-brutalist) | User asked for radical options with new logos |
| 2026-11-10 | **Chose Sticker Bomb** | Friendly and distinctive; makes watching agents approachable |
| 2026-11-10 | Bricolage Grotesque + Space Mono, self-hosted | Characterful, offline-safe; mono voice for machine facts |
| 2026-11-10 | Mentions set in sans, not Space Mono | Space Mono's `@` renders like an "a" at small sizes |
| 2026-11-10 | Accent darkened to `#3753f0` | Original `#3d5afe` was 4.498:1 on `--chrome`, just under AA |
| 2026-11-10 | Agents shown by handle in headers, DMs and traffic | Matches `@mention`, shorter, unambiguous (two Claudes) |
| 2026-11-10 | Ship Classic (v0.5.0 look) as the default and Sticker Bomb as an opt-in theme | Existing users see nothing unexpected; the new identity is one click away |
| 2026-11-10 | The new navigation layout ships for both themes | Layout was approved independently of the visual identity; Classic is the v0.5.0 look on the new layout |
| 2026-11-10 | CSP gains `font-src 'self'` and `img-src 'self'`; scripts and styles stay strictly same-origin | Needed for the theme fonts and favicon; inline code stays forbidden |
| 2026-11-10 | Agent colour by class from a hash of identity | Inline styles are forbidden; stable per agent |
| 2026-11-25 | Add an Orca theme (dark only) matching the Orca application's default appearance, with Geist served from the executable | The interface is watched in a browser tab inside Orca, next to the agent chats; Classic and Sticker Bomb looked like a different product. Light mode and following Orca's later changes are out of scope |
| 2026-11-25 | Choose the theme from the link with `#theme=<name>` in the fragment, not a query | A query on `/` is rejected by the broker, and the fragment already carries the access token without reaching any request; no server change is needed |
| 2026-11-26 | Add Orca Light (`orca-light`) and Orca (system) (`orca-system`) next to the dark Orca theme, whose id `orca` and look are unchanged; the menu entry for `orca` is now labelled Orca Dark | With Orca in its light appearance the dark page was a slab inside a light window, the problem the Orca theme set out to fix. Values come from Orca's shipped light tokens (`tools/check_orca_tokens.py`); the system choice follows `prefers-color-scheme` live and stores its own name, so a saved choice keeps following Orca |
| 2026-11-26 | Light text-grade colours are the palette's 700 steps, not Orca's light blue-500 unread colour | Blue-500 is 3.7:1 on white; the 700 steps reach 4.5:1 on white and on the grey rows, and are still Orca's shipped colours |
| 2026-11-26 | The rendered-contrast and palette checks read `color-mix()` results (computed as `color(srgb …)`) | They were parsed as 0-255 channels, so tinted backgrounds were measured as near-black. Found while adding the light theme; the dark theme still passes with the correction |
| 2026-11-27 | The access link may carry the existing `#/board/<name>` route beside the theme and token | A coordinator opens one board per project for the owner; the token handling used to discard the whole fragment. No new parameter or server change; the route is the one route-shaped part that does not name `access_token`, so AGB-69's guarantees are unchanged |

## Known gaps
- **Dark Sticker Bomb:** not designed (see Color).
- **Older QA harnesses:** the Python browser harnesses in `tools/` other than `test_web_browser.py`
  still target the previous tabbed interface's element ids.
- **Internal naming:** modules and IPC messages are still called "monitor"; only user-visible names
  changed to "web interface".
- **Reduced-motion and keyboard paths** are implemented but only partly covered by automated tests.
