# Third-party notices

The project's original code is licensed under GPL-3.0-or-later. This does not
replace the separate licenses of third-party components.

Vendored components retain their upstream copyright/license notices and their
original licensing. Each vendored directory includes a `LICENSE`, dependency
metadata and `PATCHES.md` describing the local changes:

- `vendor/libp2p-mdns` — MIT, copyright 2018 Parity Technologies (UK) Ltd.
- `vendor/libp2p-rendezvous` — MIT, copyright 2021 COMIT Network.
- `vendor/libp2p-autonat` — MIT, copyright 2021 Protocol Labs.
- `vendor/libp2p-dcutr` — MIT, copyright 2021 Protocol Labs.
- `vendor/libp2p-relay` — MIT, Protocol Labs and Parity Technologies; see retained notices.

Registry dependencies and their transitive dependencies retain the licenses
specified by their package metadata and distributed license files. `Cargo.lock`
pins the source versions; this repository does not include their compiled code.
Redistributors of binaries must preserve the applicable dependency notices.

## Web interface fonts

The Sticker Bomb theme of the embedded web interface serves two typefaces from the executable
(see `assets/web/fonts/`). Both are licensed under the SIL Open Font License 1.1; the complete
license texts ship next to the font files and must be preserved in redistributed binaries and
source packages:

- Bricolage Grotesque — copyright 2022 The Bricolage Grotesque Project Authors
  (`assets/web/fonts/LICENSE-bricolage-grotesque.txt`).
- Space Mono — copyright 2016 The Space Mono Project Authors
  (`assets/web/fonts/LICENSE-space-mono.txt`).

The default Classic theme uses system fonts and ships no font files.
