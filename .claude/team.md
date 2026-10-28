# Agent Bridge

team_key: AGB
team_name: Agent Bridge
github_repo: jasadams/agent-bridge
default_branch: main

## Versioning

method: semver
tag_format: vMAJOR.MINOR.PATCH
version_source: git-tag

The project currently uses pre-1.0 versions. Select a minor version for new
capabilities or incompatible interfaces and a patch version for compatible fixes.
The pushed version tag supplies the version embedded in release binaries;
Cargo.toml supplies the fallback for untagged development builds.

## Release

pipelines:
- Release

workflow: .github/workflows/release.yml

Only strict semantic version tags publish releases. The release pipeline validates
the tag's commit on main, checks and builds all four platforms, packages complete
corresponding source and notices, and publishes immutable versioned assets.
Plain main pushes and pull requests run validation without publishing releases.

## Post-release verification

Verify the Release workflow succeeds, the GitHub release tag points to the intended
commit, and SOURCE_VERSION/SOURCE_COMMIT/checksums agree. Check the published Linux
binary's --version and exercise the installer in a disposable custom binary
directory without changing the user's service or private state. Supported artifacts
are Linux x86_64/ARM64 and macOS Intel/Apple Silicon.

See docs/releasing.md for release commands and rerun/recovery behavior.
