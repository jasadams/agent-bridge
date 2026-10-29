# Local Paragraph provenance extension

Baseline: crates.io `ratatui-widgets` 0.3.2, upstream commit
`e665c36cb14752a61cd777fbd06dbef8474f2add` (`ratatui-widgets` subdirectory).
The upstream MIT license is included unchanged.

Only `src/paragraph.rs` changes. The additive `render_with_source` entry point
uses the existing wrapping, clipping and positioning algorithms while reporting
each visible grapheme's original line, span and byte offset. Borrowed spans are
given independent storage to distinguish identical or overlapping labels.
Unattributable generated symbols report no source. Ordinary Widget rendering
does not allocate provenance maps and retains its previous behavior.

The renderer does not produce escapes or interpret destinations. Application
tests compare plain buffers and provenance, and upstream Paragraph tests run
through this crate's locked manifest.
