# Design System — Agent Bridge Monitor

The web interface served by `agent-bridge web enable`. Source of truth for its visual and
interaction decisions. It ships two themes on one layout:

- **Classic** (default): the look of v0.5.0, kept as the base tokens in `assets/web/style.css`.
- **Sticker Bomb** (opt-in, from **Theme** in the top bar): the neo-brutalist identity described
  below, implemented in `assets/web/theme-sticker.css`.

Layout, shell and interaction rules apply to both. Colour, type, shape, motion and logo sections
describe Sticker Bomb unless they say otherwise. Two other identities (an editorial "paper" theme
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
  sheet is loaded.
- **Storage.** The page stores only the theme and the sidebar state, never tokens or drafts.

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

## Known gaps
- **Dark Sticker Bomb:** not designed (see Color).
- **Older QA harnesses:** the Python browser harnesses in `tools/` other than `test_web_browser.py`
  still target the previous tabbed interface's element ids.
- **Internal naming:** modules and IPC messages are still called "monitor"; only user-visible names
  changed to "web interface".
- **Reduced-motion and keyboard paths** are implemented but only partly covered by automated tests.
