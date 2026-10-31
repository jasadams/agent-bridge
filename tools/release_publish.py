#!/usr/bin/env python3
"""Publish verified immutable versioned assets; never replace a published build."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import tempfile

from release_version import parse_version, tag_version, version_key

PLATFORMS = {'agent-bridge-linux-x86_64', 'agent-bridge-linux-aarch64',
             'agent-bridge-macos-aarch64'}


def verify_assets(directory, commit, version):
    parse_version(version)
    expected = PLATFORMS | {'LICENSE', 'THIRD_PARTY_LICENSES.txt', 'SOURCE_COMMIT',
                            'SOURCE_VERSION', 'RELEASE_NOTES.md', 'SHA256SUMS',
                            f'agent-bridge-source-v{version}.tar.gz'}
    entries = {path.name: path for path in directory.iterdir()}
    if entries.keys() != expected:
        raise ValueError('Release must contain exactly the expected versioned assets')
    if any(path.is_symlink() or not path.is_file() for path in entries.values()):
        raise ValueError('Release assets must be regular files without symbolic links')
    if (directory / 'SOURCE_COMMIT').read_text() != commit + '\n':
        raise ValueError('Published source commit does not match the tag')
    if (directory / 'SOURCE_VERSION').read_text() != version + '\n':
        raise ValueError('Published source version does not match the tag')
    checksums = {}
    for line in (directory / 'SHA256SUMS').read_text().splitlines():
        match = re.fullmatch(r'([0-9a-f]{64})  ([A-Za-z0-9._-]+)', line)
        if not match or match[2] in checksums:
            raise ValueError('Malformed or duplicate release checksum')
        checksums[match[2]] = match[1]
    if checksums.keys() != expected - {'SHA256SUMS'}:
        raise ValueError('Release checksum manifest is incomplete or unexpected')
    for name, digest in checksums.items():
        with (directory / name).open('rb') as stream:
            hasher = hashlib.sha256()
            for block in iter(lambda: stream.read(1024 * 1024), b''):
                hasher.update(block)
        if hasher.hexdigest() != digest:
            raise ValueError(f'Release checksum mismatch: {name}')


def highest_version(releases):
    candidates = []
    for release in releases:
        if release.get('draft') or release.get('prerelease'):
            continue
        try:
            version = tag_version(release['tag_name'])
        except (KeyError, ValueError):
            continue
        candidates.append((version_key(version), release['tag_name']))
    return max(candidates)[1] if candidates else None


def gh(*arguments, json_output=False):
    output = subprocess.check_output(['gh', *arguments], text=True)
    return json.loads(output) if json_output else output.strip()


def find_release(repo, tag):
    result = subprocess.run(['gh', 'api', f'repos/{repo}/releases/tags/{tag}'],
                            text=True, capture_output=True)
    if result.returncode:
        if re.search(r'\(HTTP 404\)', result.stderr):
            return None
        raise RuntimeError(f'Cannot inspect existing release: {result.stderr.strip()}')
    return json.loads(result.stdout)


def verify_remote_assets(repo, tag, commit, version):
    with tempfile.TemporaryDirectory(prefix='published-release-') as temporary:
        published = Path(temporary)
        gh('release', 'download', tag, '--repo', repo, '--dir', str(published))
        verify_assets(published, commit, version)


def publish(repo, tag, commit, directory):
    version = tag_version(tag)
    if not re.fullmatch(r'[0-9a-f]{40}', commit):
        raise ValueError('Expected a full source commit SHA')
    if gh('api', f'repos/{repo}/commits/{tag}', '--jq', '.sha') != commit:
        raise ValueError('Remote release tag no longer resolves to this source commit')
    verify_assets(directory, commit, version)
    existing = find_release(repo, tag)
    if existing and not existing['draft']:
        verify_remote_assets(repo, tag, commit, version)
        print(f'{tag} is already published; verified its original assets without replacing them.')
    else:
        assets = [str(path) for path in sorted(directory.iterdir())]
        if existing:
            gh('release', 'upload', tag, '--repo', repo, '--clobber', *assets)
        else:
            gh('release', 'create', tag, '--repo', repo, '--verify-tag', '--target', commit,
               '--draft', '--latest=false', '--title', f'Agent Bridge {tag}',
               '--notes-file', str(directory / 'RELEASE_NOTES.md'), *assets)
        # Verify the actual uploaded set, including retained assets in a repaired
        # draft. Unexpected or partially uploaded files must remain unpublished.
        verify_remote_assets(repo, tag, commit, version)
        # A draft can be repaired; a published build can never be overwritten.
        # Recheck the remote tag after uploading, immediately before publication.
        if gh('api', f'repos/{repo}/commits/{tag}', '--jq', '.sha') != commit:
            raise ValueError('Remote release tag changed during upload; leaving the draft unpublished')
        gh('release', 'edit', tag, '--repo', repo, '--verify-tag', '--draft=false',
           '--prerelease=false', '--latest=false')

    # The workflow serializes publication across release tags. Numeric comparison
    # means an older build finishing later cannot replace the newest version.
    pages = gh('api', '--paginate', '--slurp', f'repos/{repo}/releases?per_page=100',
               json_output=True)
    latest = highest_version([release for page in pages for release in page])
    if latest:
        gh('release', 'edit', latest, '--repo', repo, '--latest')
        print(f'Latest published semantic version: {latest}')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--dist', required=True, type=Path)
    parser.add_argument('--commit', required=True)
    parser.add_argument('--tag', required=True)
    args = parser.parse_args()
    repo = os.environ.get('GH_REPO', '')
    if not re.fullmatch(r'[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+', repo):
        parser.error('GH_REPO must identify the release repository')
    try:
        publish(repo, args.tag, args.commit, args.dist.resolve())
    except (ValueError, RuntimeError, subprocess.CalledProcessError) as error:
        parser.error(str(error))


if __name__ == '__main__':
    main()
