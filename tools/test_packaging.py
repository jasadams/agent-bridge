#!/usr/bin/env python3
"""Check public source archive metadata and original-notice isolation."""
import io
import os
from pathlib import Path
import subprocess
import tarfile
import tempfile
import unittest
from package_release import license_files, normalized_entry, release_notes, source_metadata, version_changes


class PackagingTests(unittest.TestCase):
    def test_opencode_runtime_import_closure_is_embedded_and_archived(self):
        repo = Path(__file__).resolve().parents[1]
        installer = (repo / 'src/opencode_install.rs').read_text()
        expected = {'index.ts', 'rpc.ts', 'peer.ts', 'journal.ts'}
        for name in expected:
            relative = f'plugins/opencode/{name}'
            content = (repo / relative).read_bytes()
            self.assertTrue(content, relative)
            self.assertIn(f'include_str!("../{relative}")', installer)
            entry = normalized_entry(tarfile.TarInfo(f'agent-bridge/{relative}'), 123)
            self.assertIsNotNone(entry)
        self.assertIn('include_str!("../LICENSE")', installer)
        # Keep the shipped closure complete when a runtime module gains an import.
        import re
        for name in expected:
            text = (repo / 'plugins/opencode' / name).read_text()
            for imported in re.findall(r'from ["\']\./([^"\']+)["\']', text):
                self.assertIn(imported, expected, f'{name} imports unbundled {imported}')
        archive_names = subprocess.check_output(['git', 'ls-files'], cwd=repo, text=True).splitlines()
        for relative in ['LICENSE', 'plugins/opencode/package.json', 'plugins/opencode/bun.lock']:
            self.assertIn(relative, archive_names)
        for name in expected:
            self.assertIn(f'plugins/opencode/{name}', archive_names)

    def test_release_notes_cannot_escape_the_archived_source(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source = root / 'source'
            notes = source / 'docs/releases/v0.2.0.md'
            notes.parent.mkdir(parents=True)
            private = root / 'private'
            private.write_text('host data must never enter the release')
            self.assertEqual(version_changes(source, '0.2.0'), '')
            notes.write_text('## Compatibility\nUse the peer selector.\n')
            content = version_changes(source, '0.2.0')
            self.assertIn(content, release_notes('0.2.0', 'a' * 40, content))
            notes.unlink()
            notes.symlink_to(private)
            with self.assertRaises(ValueError):
                version_changes(source, '0.2.0')
            notes.unlink()
            notes.parent.rmdir()
            notes.parent.symlink_to(root, target_is_directory=True)
            (root / 'v0.2.0.md').write_text('outside source')
            with self.assertRaises(ValueError):
                version_changes(source, '0.2.0')

    def test_source_archive_keeps_release_version_and_offline_build_instructions(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            commit = '0123456789abcdef0123456789abcdef01234567'
            source_metadata(root, commit, '0.2.0')
            self.assertEqual((root / 'SOURCE_VERSION').read_text(), '0.2.0\n')
            self.assertEqual((root / 'SOURCE_COMMIT').read_text(), commit + '\n')
            instructions = (root / 'BUILDING-OFFLINE.md').read_text()
            self.assertIn('SOURCE_VERSION automatically', instructions)
            self.assertIn('cargo build --release --locked --offline', instructions)
            notes = release_notes('0.2.0', commit)
            self.assertIn('# Agent Bridge 0.2.0', notes)
            self.assertIn(commit, notes)

    def test_build_script_version_precedence_and_rejection(self):
        repo = Path(__file__).resolve().parents[1]
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            executable = root / 'build-version'
            subprocess.run(['rustc', '--edition=2024', str(repo / 'build.rs'),
                            '-o', str(executable)], check=True)
            environment = dict(os.environ, CARGO_MANIFEST_DIR=str(root),
                               CARGO_PKG_VERSION='0.1.3')
            environment.pop('AGENT_BRIDGE_BUILD_VERSION', None)

            def run(override=None):
                env = environment.copy()
                if override is not None:
                    env['AGENT_BRIDGE_BUILD_VERSION'] = override
                return subprocess.run([str(executable)], env=env, capture_output=True, text=True)

            development = run()
            self.assertEqual(development.returncode, 0, development.stderr)
            self.assertIn('AGENT_BRIDGE_VERSION=0.1.3', development.stdout)
            (root / 'SOURCE_VERSION').write_text('0.2.0\n')
            packaged = run()
            self.assertEqual(packaged.returncode, 0, packaged.stderr)
            self.assertIn('AGENT_BRIDGE_VERSION=0.2.0', packaged.stdout)
            tagged = run('0.3.0')
            self.assertEqual(tagged.returncode, 0, tagged.stderr)
            self.assertIn('AGENT_BRIDGE_VERSION=0.3.0', tagged.stdout)
            for invalid in ['', 'v0.2.0', '00.2.0', '0.02.0', '0.2', '0.2.0\n',
                            '0.2.0-rc.1', '1.2.18446744073709551616']:
                result = run(invalid)
                self.assertNotEqual(result.returncode, 0, invalid)
                self.assertIn('Expected a version', result.stderr)
            (root / 'SOURCE_VERSION').write_text('invalid\n')
            self.assertNotEqual(run().returncode, 0)

    def test_archive_metadata_never_contains_build_machine_identity(self):
        info = tarfile.TarInfo('agent-bridge/source.rs')
        info.uid, info.gid = 1234, 4321
        info.uname, info.gname = 'private-build-user', 'private-build-group'
        info.mtime, info.size = 100.5, 3
        info.pax_headers = {'atime': '100.5', 'ctime': '99.1'}
        result = normalized_entry(info, 123)
        buffer = io.BytesIO()
        with tarfile.open(fileobj=buffer, mode='w') as archive:
            archive.addfile(result, io.BytesIO(b'abc'))
        buffer.seek(0)
        with tarfile.open(fileobj=buffer, mode='r') as archive:
            saved = archive.getmembers()[0]
        self.assertEqual((saved.uid, saved.gid, saved.uname, saved.gname, saved.mtime),
                         (0, 0, '', '', 123))
        self.assertNotIn(b'private-build', buffer.getvalue())
        self.assertEqual(saved.pax_headers, {})

    def test_build_outputs_and_git_metadata_are_not_corresponding_source(self):
        for name in ('agent-bridge/target/debug/app', 'agent-bridge/.git/config'):
            self.assertIsNone(normalized_entry(tarfile.TarInfo(name), 123))

    def test_license_symlinks_cannot_read_outside_package(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            package = root / 'package'
            package.mkdir()
            (package / 'Cargo.toml').write_text('[package]\n')
            (package / 'LICENSE').write_text('original copyright notice')
            (root / 'private').write_text('outside-package-sensitive-value')
            (package / 'NOTICE').symlink_to(root / 'private')
            found = license_files({'manifest_path': str(package / 'Cargo.toml')})
            self.assertEqual(found, [('LICENSE', 'original copyright notice')])


if __name__ == '__main__':
    unittest.main()
