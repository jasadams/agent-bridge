# Versioned releases

Agent Bridge uses semantic version tags, currently `v0.MINOR.PATCH`. Before 1.0,
minor versions can introduce new capabilities or incompatible interfaces; patch
versions contain compatible fixes. Read the release notes before updating an
integration. Version tags are immutable and are never moved to a different commit.

## Publish a version

Complete independent review and required PR validation, merge the intended changes
to `main`, then tag that commit:

```sh
git switch main
git pull --ff-only origin main
git tag -a v0.2.0 -m "Release v0.2.0"
git push origin v0.2.0
```

Use a new, confirmed version for each release. Push the individual tag rather than
all local tags. No version edit is required in Cargo.toml for subsequent tagged
builds: the tag is the release version source of truth. Untagged development
builds use the manifest's fallback version.

The `Release` workflow validates the strict `vMAJOR.MINOR.PATCH` tag and that its
commit belongs to `main`. It runs tests and lint checks, builds Linux x86_64/ARM64
and macOS Apple Silicon binaries, checks their reported versions, then
publishes a GitHub release after all platforms succeed. Pull requests, ordinary
main pushes and manual runs on branches run the same three-platform source,
test, lint, installer and packaging checks. They skip optimized executable builds,
macOS signing, release-binary version checks and release artifact uploads.

Release preparation therefore requires no optimized PR or main build to finish
before tagging a reviewed, validated commit. The tag workflow runs all required
validation again and builds the three executables once with the tag's version.
Only a tag-push event in the canonical repository can publish; a manual run cannot
publish. A manual run on a strict release tag can validate and build that tag.

The version is embedded in `agent-bridge --version`, broker status and MCP server
metadata. Every release includes the binaries, SHA256SUMS, SOURCE_VERSION,
SOURCE_COMMIT, release notes, GPL license, dependency notices, and a versioned
source archive containing locked dependencies and offline build instructions.
On the next real release, verify all three platform assets and the corresponding
source/version/checksum provenance below; do not create a release solely to test
the workflow.

## Verify and retry

```sh
gh run list --workflow release.yml
gh release view v0.2.0
```

Check the intended commit and version before announcing a release. The publisher
serializes release publication and promotes only the highest semantic version to
GitHub's latest release. The installer resolves that release once and downloads
all files from its immutable version tag.

A failed build publishes nothing. Fix the source, validate it, and use a new
version tag. A publication failure can be rerun for the same unchanged tag: a
partial draft can be repaired, while an already published release is verified
instead of replacing its assets. Never replace a published version to fix a bug;
publish the next patch or minor version instead.

## Build a tagged checkout locally

The workflow sets the version explicitly. For the same behavior in a Git checkout:

```sh
git checkout v0.2.0
AGENT_BRIDGE_BUILD_VERSION=0.2.0 cargo build --release --locked
./target/release/agent-bridge --version
```

Downloaded corresponding-source archives contain SOURCE_VERSION, which the build
script reads automatically, so their documented offline build retains the release
version without Git metadata. Set AGENT_BRIDGE_BUILD_COMMIT to SOURCE_COMMIT when
rebuilding to retain the source revision in broker status.
