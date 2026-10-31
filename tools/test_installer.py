#!/usr/bin/env python3
"""Offline installer checks with fake downloads/platform/service commands.

No real network, service, native session, global profile or installed executable
is touched. Every install destination and backup is in a disposable private HOME.
"""
import hashlib
import json
import os
from pathlib import Path
import shutil
import shlex
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]
INSTALLER = ROOT / "install.sh"
SH = shutil.which("sh")


class InstallerTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="agent-bridge-installer-test-")
        self.root = Path(self.temp.name).resolve()
        self.home = self.root / "home with spaces"
        self.home.mkdir(mode=0o700)
        self.bin = self.root / "fixture-bin"
        self.bin.mkdir(mode=0o700)
        self.downloads = self.root / "downloads"
        self.downloads.mkdir(mode=0o700)
        (self.root / "tmp").mkdir(mode=0o700)
        self.env = os.environ.copy()
        self.env.update(HOME=str(self.home), PATH=str(self.bin), TMPDIR=str(self.root / "tmp"),
                        FIXTURE_ROOT=str(self.downloads), FAKE_OS="Linux", FAKE_ARCH="x86_64",
                        FAKE_UID=str(os.getuid() or 1000), FAKE_LIBC="glibc 2.35", FAKE_LDD="GNU libc",
                        FAKE_SERVICE_ACTIVE="0", INSTALLER_EXECUTED=str(self.root / "executed"),
                        INSTALLER_SERVICE_LOG=str(self.root / "service-args"),
                        FAKE_CURL_LOG=str(self.root / "curl-args"), FAKE_SERVICE_RESULT="0",
                        FAKE_LATEST_URL="https://github.com/jasadams/agent-bridge/releases/tag/v0.2.0")
        for command in ["awk", "cp", "chmod", "mv", "rm", "mkdir", "mktemp", "cat", "sha256sum"]:
            actual = shutil.which(command)
            if not actual and command == "sha256sum":
                continue
            self.assertIsNotNone(actual, command)
            (self.bin / command).symlink_to(actual)
        if not (self.bin / "sha256sum").exists():
            (self.bin / "shasum").symlink_to(shutil.which("shasum"))
        self.fake("uname", 'case "$1" in -s) printf "%s\\n" "$FAKE_OS";; -m) printf "%s\\n" "$FAKE_ARCH";; *) exit 1;; esac\n')
        self.fake("id", 'printf "%s\\n" "$FAKE_UID"\n')
        self.fake("getconf", 'printf "%s\\n" "$FAKE_LIBC"\n')
        self.fake("ldd", 'printf "%s\\n" "$FAKE_LDD"\n')
        self.fake("systemctl", '[ "$FAKE_SERVICE_ACTIVE" = 1 ]\n')
        self.fake("launchctl", '[ "$FAKE_SERVICE_ACTIVE" = 1 ]\n')
        # Use the real host's stat implementation even while emulating the other
        # platform. A Python interpreter per ancestor made these fixtures much
        # slower than the installer itself on macOS CI.
        host_stat = shutil.which("stat")
        self.assertIsNotNone(host_stat)
        host_flags = "-f '%u %Lp %l'" if sys.platform == "darwin" else "-c '%u %a %h'"
        self.fake("stat", f"""requested_format=$2
path=$3
metadata=$({shlex.quote(host_stat)} {host_flags} "$path") || exit 1
set -- $metadata
owner=$1
mode=$2
links=$3
[ "${{FAKE_UNSAFE_PATH:-}}" != "$path" ] || mode=777
[ "${{FAKE_FOREIGN_PATH:-}}" != "$path" ] || owner={os.getuid()+1}
case "$requested_format" in
    *%h*|*%l*) printf '%s %s %s\\n' "$owner" "$mode" "$links" ;;
    *) printf '%s %s\\n' "$owner" "$mode" ;;
esac
""")
        self.fake("curl", """import json,os,pathlib,shutil,sys
args=sys.argv[1:]
with open(os.environ['FAKE_CURL_LOG'],'a') as f:f.write(json.dumps(args)+'\\n')
out=args[args.index('--output')+1]
url=args[-1]
assert '--proto' in args and args[args.index('--proto')+1]=='=https'
assert '--proto-redir' in args and args[args.index('--proto-redir')+1]=='=https'
assert '--tlsv1.2' in args and '--fail' in args and '--location' in args
if url == 'https://github.com/jasadams/agent-bridge/releases/latest':
    assert '--write-out' in args and args[args.index('--write-out')+1]=='%{url_effective}'
    assert out == '/dev/null'
    print(os.environ['FAKE_LATEST_URL'], end='')
    sys.exit(0)
assert url.startswith('https://github.com/jasadams/agent-bridge/releases/download/')
source=pathlib.Path(os.environ['FIXTURE_ROOT'])/url.rsplit('/',1)[-1]
if not source.is_file():sys.exit(22)
shutil.copyfile(source,out)
""", python=True)
        self.fixture_binary = b'''#!/bin/sh
printf '%s\\n' "$1" >> "$INSTALLER_EXECUTED"
if [ "$1" = --version ]; then printf 'agent-bridge %s\\n' "${FAKE_BINARY_VERSION:-0.2.0}"; exit 0; fi
printf '%s\\n' "$@" > "$INSTALLER_SERVICE_LOG"
exit "$FAKE_SERVICE_RESULT"
'''
        self.notices = {"LICENSE": b"Fixture project GPL license\n",
                        "THIRD_PARTY_LICENSES.txt": b"Fixture dependency licenses\n",
                        "SOURCE_COMMIT": b"0123456789abcdef0123456789abcdef01234567\n",
                        "SOURCE_VERSION": b"0.2.0\n"}
        for name, content in self.notices.items():
            (self.downloads / name).write_bytes(content)
        self.asset = "agent-bridge-linux-x86_64"
        self.publish(self.asset)

    def tearDown(self):
        self.temp.cleanup()

    def fake(self, name, body, python=False):
        path = self.bin / name
        path.write_text((f"#!{sys.executable}\n" if python else "#!/bin/sh\n") + body)
        path.chmod(0o700)

    def publish(self, asset, manifest=None):
        for name, content in self.notices.items():
            (self.downloads / name).write_bytes(content)
        (self.downloads / asset).write_bytes(self.fixture_binary)
        digest = hashlib.sha256(self.fixture_binary).hexdigest()
        checksum_lines = f"{digest}  {asset}\n" + "".join(
            f"{hashlib.sha256(content).hexdigest()}  {name}\n" for name, content in self.notices.items())
        (self.downloads / "SHA256SUMS").write_text(manifest if manifest is not None else checksum_lines)

    def run_installer(self, *args, piped=False):
        if piped:
            return subprocess.run([SH, "-s", "--", *args], input=INSTALLER.read_text(),
                                  text=True, capture_output=True, env=self.env, timeout=15, cwd=self.root)
        return subprocess.run([SH, str(INSTALLER), *args], text=True,
                              capture_output=True, env=self.env, timeout=15, cwd=self.root)

    def assert_clean_downloads(self):
        self.assertEqual(list((self.root / "tmp").iterdir()), [])

    def test_binary_only_is_atomic_pipe_safe_and_retains_old_binary(self):
        destination = self.home / "custom bin" / "agent-bridge"
        destination.parent.mkdir()
        destination.write_bytes(b"previous executable")
        result = self.run_installer("--no-service", "--bin-dir", str(destination.parent), piped=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(destination.read_bytes(), self.fixture_binary)
        self.assertEqual(destination.stat().st_mode & 0o777, 0o755)
        backups = list(destination.parent.glob(".agent-bridge.backup.*"))
        self.assertEqual(len(backups), 1)
        self.assertEqual(backups[0].read_bytes(), b"previous executable")
        self.assertFalse((self.root / "service-args").exists())
        self.assertEqual(list(destination.parent.glob(".agent-bridge.new.*")), [])
        self.assert_clean_downloads()

    def test_managed_install_delegates_without_overwriting_binary_or_touching_hooks(self):
        installed = self.home / ".local/bin/agent-bridge"
        installed.parent.mkdir(parents=True)
        installed.write_bytes(b"existing managed executable")
        state = self.home / "state with spaces"
        result = self.run_installer("--state-dir", str(state))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual((self.root / "service-args").read_text().splitlines(),
                         ["--state-dir", str(state), "service", "install"])
        self.assertEqual(installed.read_bytes(), b"existing managed executable")
        self.assertFalse((self.home / ".codex").exists())
        self.assertFalse((self.home / ".claude").exists())
        self.assert_clean_downloads()

    def test_checksum_failures_never_execute_or_install(self):
        digest = hashlib.sha256(self.fixture_binary).hexdigest()
        for manifest in [f"{'0'*64}  {self.asset}\n", "", f"{digest}  other-asset\n",
                         f"{digest}  {self.asset}\n{digest} *{self.asset}\n",
                         f"{digest}  {self.asset} extra\n", f"{digest}0  {self.asset}\n"]:
            with self.subTest(manifest=manifest):
                (self.downloads / "SHA256SUMS").write_text(manifest)
                result = self.run_installer("--no-service")
                self.assertNotEqual(result.returncode, 0)
                self.assertFalse((self.root / "executed").exists())
                self.assertFalse((self.home / ".local").exists())
                self.assert_clean_downloads()

    def test_platform_selection_and_explicit_version(self):
        for system, machine, asset in [("Darwin", "arm64", "agent-bridge-macos-aarch64"),
                                       ("Linux", "aarch64", "agent-bridge-linux-aarch64")]:
            with self.subTest(system=system, machine=machine):
                self.env.update(FAKE_OS=system, FAKE_ARCH=machine)
                self.publish(asset)
                result = self.run_installer("--version", "v0.2.0", "--no-service")
                self.assertEqual(result.returncode, 0, result.stderr)
                calls = [json.loads(line) for line in (self.root / "curl-args").read_text().splitlines()]
                self.assertTrue(any(call[-1].endswith("/v0.2.0/" + asset) for call in calls))
                self.assertFalse(any(call[-1].endswith('/releases/latest') for call in calls))

    def test_intel_macos_is_rejected_before_download(self):
        self.env.update(FAKE_OS="Darwin", FAKE_ARCH="x86_64")
        result = self.run_installer("--no-service")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Intel macOS is not supported", result.stderr)
        self.assertFalse((self.root / "curl-args").exists())

    def test_latest_resolves_once_and_all_downloads_use_the_immutable_version(self):
        result = self.run_installer("--no-service")
        self.assertEqual(result.returncode, 0, result.stderr)
        calls = [json.loads(line) for line in (self.root / "curl-args").read_text().splitlines()]
        self.assertEqual(sum(call[-1].endswith('/releases/latest') for call in calls), 1)
        downloads = [call[-1] for call in calls if '/releases/download/' in call[-1]]
        self.assertEqual(len(downloads), 6)
        self.assertTrue(all('/releases/download/v0.2.0/' in url for url in downloads))
        self.assertIn('Agent Bridge v0.2.0', result.stdout)

    def test_latest_rejects_unexpected_or_nonsemantic_destinations(self):
        for url in ['https://example.com/releases/tag/v0.2.0',
                    'https://github.com/jasadams/agent-bridge/releases/tag/v00.2.0',
                    'https://github.com/jasadams/agent-bridge/releases/tag/nightly',
                    'https://github.com/jasadams/agent-bridge/releases/tag/extra/v0.2.0',
                    'https://github.com/jasadams/agent-bridge/releases/tag/v0.2.0?query=yes',
                    'https://github.com/jasadams/agent-bridge/releases/tag/v0.2.0\ninvalid']:
            with self.subTest(url=url):
                self.env['FAKE_LATEST_URL'] = url
                result = self.run_installer('--no-service')
                self.assertNotEqual(result.returncode, 0)
                self.assertFalse((self.root / 'executed').exists())
                self.assertFalse((self.home / '.local/bin/agent-bridge').exists())
                self.assert_clean_downloads()

    def test_release_metadata_and_executable_must_match_the_selected_version(self):
        self.notices['SOURCE_VERSION'] = b'0.2.1\n'
        self.publish(self.asset)
        result = self.run_installer('--no-service')
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('SOURCE_VERSION disagree', result.stderr)
        self.assertFalse((self.root / 'executed').exists())
        self.notices['SOURCE_VERSION'] = b'0.2.0\n'
        self.publish(self.asset)
        self.env['FAKE_BINARY_VERSION'] = '0.2.1'
        result = self.run_installer('--no-service')
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('executable version disagrees', result.stderr)
        self.assertFalse((self.home / '.local/bin/agent-bridge').exists())

    def test_same_source_commit_can_have_distinct_semantic_build_versions(self):
        first = self.run_installer('--no-service')
        self.assertEqual(first.returncode, 0, first.stderr)
        self.notices['SOURCE_VERSION'] = b'0.2.1\n'
        self.publish(self.asset)
        self.env['FAKE_BINARY_VERSION'] = '0.2.1'
        second = self.run_installer('--version', 'v0.2.1', '--no-service')
        self.assertEqual(second.returncode, 0, second.stderr)
        parent = self.home / '.local/share/agent-bridge/licenses'
        commit = '0123456789abcdef0123456789abcdef01234567'
        self.assertEqual((parent / f'{commit}-0.2.0/SOURCE_VERSION').read_text(), '0.2.0\n')
        self.assertEqual((parent / f'{commit}-0.2.1/SOURCE_VERSION').read_text(), '0.2.1\n')

    def test_unsupported_platform_arch_libc_and_root_fail_before_download(self):
        for change in [{"FAKE_OS": "FreeBSD"}, {"FAKE_ARCH": "ppc64"},
                       {"FAKE_LDD": "musl libc"}, {"FAKE_LIBC": "unknown"}, {"FAKE_LIBC": "glibc 2.34"}, {"FAKE_UID": "0"}]:
            with self.subTest(change=change):
                old = self.env.copy()
                self.env.update(change)
                result = self.run_installer()
                self.assertNotEqual(result.returncode, 0)
                self.assertFalse((self.root / "curl-args").exists())
                self.env = old

    def test_argument_and_destination_validation(self):
        for args in [("--version", "../main"), ("--version", ""), ("--version", "bad?tag"),
                     ("--version",), ("--bin-dir", "relative"), ("--state-dir", "relative"),
                     ("--bin-dir", str(self.home / "custom")), ("--unknown",),
                     ("--state-dir", str(self.home / "bad\npath"))]:
            with self.subTest(args=args):
                result = self.run_installer(*args)
                self.assertNotEqual(result.returncode, 0)
                self.assertFalse((self.root / "curl-args").exists())
        custom = self.home / "custom"
        custom.mkdir()
        target = self.home / "do-not-change"
        target.write_text("protected")
        (custom / "agent-bridge").symlink_to(target)
        result = self.run_installer("--no-service", "--bin-dir", str(custom))
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(target.read_text(), "protected")

    def test_active_service_refuses_binary_only_overwrite(self):
        target = self.home / ".local/bin/agent-bridge"
        target.parent.mkdir(parents=True)
        target.write_text("running service binary")
        self.env["FAKE_SERVICE_ACTIVE"] = "1"
        result = self.run_installer("--no-service")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("omit --no-service", result.stderr)
        self.assertEqual(target.read_text(), "running service binary")

    def test_inactive_configured_service_refuses_default_binary_only_updates(self):
        target = self.home / ".local/bin/agent-bridge"
        target.parent.mkdir(parents=True)
        target.write_text("stopped managed service binary")
        for platform, relative in [("Linux", ".config/systemd/user/agent-bridge.service"),
                                   ("Darwin", "Library/LaunchAgents/local.agent-bridge.plist")]:
            with self.subTest(platform=platform):
                manager=self.home / relative
                manager.parent.mkdir(parents=True)
                manager.write_text("existing manager configuration")
                self.env.update(FAKE_OS=platform, FAKE_ARCH="arm64" if platform == "Darwin" else "x86_64", FAKE_SERVICE_ACTIVE="0")
                result=self.run_installer("--no-service")
                self.assertNotEqual(result.returncode,0)
                self.assertIn("managed service is configured",result.stderr)
                self.assertEqual(target.read_text(),"stopped managed service binary")
                self.assertFalse((self.root / "curl-args").exists())
                self.assertFalse((self.root / "executed").exists())

    def test_shasum_fallback_and_service_error_leave_no_staging_files(self):
        available = shutil.which("shasum")
        if not available:
            self.skipTest("shasum is unavailable")
        if (self.bin / "sha256sum").exists():
            (self.bin / "sha256sum").unlink()
        (self.bin / "shasum").unlink(missing_ok=True)
        (self.bin / "shasum").symlink_to(available)
        self.env["FAKE_SERVICE_RESULT"] = "42"
        result = self.run_installer()
        self.assertEqual(result.returncode, 42)
        self.assert_clean_downloads()
        self.assertFalse((self.home / ".local/bin/agent-bridge").exists())

    def test_notice_tampering_prevents_any_execution_or_install(self):
        (self.downloads / "THIRD_PARTY_LICENSES.txt").write_text("tampered")
        result=self.run_installer("--no-service")
        self.assertNotEqual(result.returncode,0)
        self.assertFalse((self.root / "executed").exists())
        self.assertFalse((self.home / ".local").exists())
        self.assert_clean_downloads()

    def test_notice_files_are_versioned_and_conflicts_are_not_overwritten(self):
        first=self.run_installer("--no-service")
        self.assertEqual(first.returncode,0,first.stderr)
        notice_dir=self.home / ".local/share/agent-bridge/licenses/0123456789abcdef0123456789abcdef01234567-0.2.0"
        for name, content in self.notices.items():self.assertEqual((notice_dir/name).read_bytes(),content)
        second=self.run_installer("--no-service")
        self.assertEqual(second.returncode,0,second.stderr)
        (notice_dir / "LICENSE").write_text("existing altered notice")
        original=(self.home / ".local/bin/agent-bridge").read_bytes()
        third=self.run_installer("--no-service")
        self.assertNotEqual(third.returncode,0)
        self.assertEqual((notice_dir / "LICENSE").read_text(),"existing altered notice")
        self.assertEqual((self.home / ".local/bin/agent-bridge").read_bytes(),original)
        self.assert_clean_downloads()

    def test_unsafe_temporary_or_destination_ancestors_fail_before_download(self):
        for variable,path in [("FAKE_UNSAFE_PATH",str(self.root / "tmp")),
                              ("FAKE_FOREIGN_PATH",str(self.root / "tmp")),
                              ("FAKE_UNSAFE_PATH",str(self.home)),
                              ("FAKE_FOREIGN_PATH",str(self.home))]:
            with self.subTest(variable=variable,path=path):
                self.env[variable]=path
                result=self.run_installer("--no-service")
                self.assertNotEqual(result.returncode,0)
                self.assertFalse((self.root / "curl-args").exists())
                self.env.pop(variable)

    def test_user_owned_installation_directories_are_secured_automatically(self):
        for system in ["Linux", "Darwin"]:
            with self.subTest(system=system):
                self.env["FAKE_OS"] = system
                self.env["FAKE_ARCH"] = "arm64" if system == "Darwin" else "x86_64"
                self.publish("agent-bridge-macos-aarch64" if system == "Darwin" else self.asset)
                paths = [self.home, self.home / ".local", self.home / ".local/bin",
                         self.home / ".local/state", self.home / ".config",
                         self.home / ".config/systemd", self.home / ".config/systemd/user",
                         self.home / "Library", self.home / "Library/LaunchAgents",
                         self.home / "Library/Application Support",
                         self.home / "Library/Application Support/AgentBridge"]
                for path in paths:
                    path.mkdir(parents=True, exist_ok=True)
                    path.chmod(0o775)
                unrelated = self.home / "shared-unrelated"
                unrelated.mkdir(exist_ok=True)
                unrelated.chmod(0o775)
                result = self.run_installer(piped=True)
                self.assertEqual(result.returncode, 0, result.stderr)
                needed = [self.home, self.home / ".local/bin", self.home / ".local/state",
                          self.home / "Library/Application Support/AgentBridge"]
                needed.append(self.home / (".config/systemd/user" if system == "Linux" else "Library/LaunchAgents"))
                for path in needed:
                    self.assertEqual(path.stat().st_mode & 0o022, 0, str(path))
                self.assertEqual(unrelated.stat().st_mode & 0o777, 0o775)
                self.assertIn("Securing installation directory:", result.stderr)

    def test_managed_upgrade_secures_existing_service_file_and_executable(self):
        binary = self.home / ".local/bin/agent-bridge"
        unit = self.home / ".config/systemd/user/agent-bridge.service"
        for path in [binary, unit]:
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text("existing managed file")
            path.chmod(0o775 if path == binary else 0o664)
        result = self.run_installer()
        self.assertEqual(result.returncode, 0, result.stderr)
        for path in [binary, unit]:
            self.assertEqual(path.stat().st_mode & 0o022, 0)
            self.assertEqual(path.read_text(), "existing managed file")
        self.assertIn("Securing installation file:", result.stderr)

    def test_managed_hardlinked_service_file_is_not_chmodded(self):
        unit = self.home / ".config/systemd/user/agent-bridge.service"
        unit.parent.mkdir(parents=True)
        unit.write_text("shared inode")
        unit.chmod(0o664)
        outside = self.root / "external-unit"
        os.link(unit, outside)
        result = self.run_installer()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("hard-linked", result.stderr)
        self.assertEqual(outside.stat().st_mode & 0o777, 0o664)

    def test_external_writable_symlink_target_is_never_chmodded(self):
        external = self.root / "external-shared"
        external.mkdir(mode=0o775)
        external.chmod(0o775)
        link = self.home / "linked-bin"
        link.symlink_to(external, target_is_directory=True)
        result = self.run_installer("--no-service", "--bin-dir", str(link))
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(external.stat().st_mode & 0o777, 0o775)
        self.assertFalse((self.root / "curl-args").exists())

    def test_relative_or_control_temporary_paths_fail_immediately(self):
        (self.root / "relative").mkdir()
        for value in ["relative", str(self.root / "tmp")+"\nextra", str(self.root / "tmp/../tmp")]:
            with self.subTest(tmpdir=value):
                self.env["TMPDIR"]=value
                result=self.run_installer("--no-service")
                self.assertNotEqual(result.returncode,0)
                self.assertIn("TMPDIR",result.stderr)
                self.assertFalse((self.root / "curl-args").exists())


if __name__ == "__main__":
    unittest.main()
