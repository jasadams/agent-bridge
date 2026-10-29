# Forced-width cell diff repair

Baseline: crates.io `ratatui-core` 0.1.2, upstream commit
`e665c36cb14752a61cd777fbd06dbef8474f2add` (`ratatui-core` subdirectory).
The upstream MIT license is included unchanged.

`src/buffer/diff.rs` changes. Explicit forced widths now share the ordinary
wide-cell diff rules, including refresh of the
previous glyph's exposed, styled trailing cells. Equal forced-width cells still
emit nothing; existing Skip/AlwaysUpdate/deprecated-skip precedence is retained.
Arbitrary forced widths can shrink to other multi-column widths, and trailing
ranges are bounded to the actual buffer length.

Forced-width symbols retain their original explicit continuation-skipping
contract: the ordinary VS16 workaround must not paint inside their current
glyph, which could erase that glyph or an unchanged neighboring character.
Only cells exposed beyond a shrinking symbol are force-refreshed.

This is required for trusted OSC 8 cells whose visible glyph shrinks while its
destination or underline changes. The library does not interpret or generate
terminal sequences. Regression tests cover natural/forced widths, styled shrink,
oversized widths and unchanged cells; existing buffer regressions are retained.

`src/text/span.rs` removes three obsolete `wrong_self_convention` lint
expectations. The unchanged upstream source fails the same strict library Clippy
command under the repository's pinned Rust 1.98 because those expectations are
unfulfilled. No lint is suppressed and no method behavior changes.
