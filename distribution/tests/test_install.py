#!/usr/bin/env python3
"""Exercise the curl installer without network, real Bree, or user files.

Run from any directory with:
    python3 distribution/tests/test_install.py

The fake curl produces matching release artifacts and records requests. The
fake executable records invocation so checksum failures also prove that an
unverified download never ran.
"""

import hashlib
import json
import os
from pathlib import Path
import shlex
import stat
import subprocess
import sys
import tempfile
import unittest


INSTALLER = Path(__file__).resolve().parents[1] / "install.sh"
ASSET = "bree-aarch64-apple-darwin"
VERSION_ASSET = "bree-version.txt"
VERSION = "0.3.0-alpha.1"
RELEASE_BASE = "https://downloads.example.test/bree/releases"
PUBLIC_RELEASE_BASE = "https://github.com/yuyongyan29-dev/Bree/releases"
PUBLIC_VERSION_URL = "https://raw.githubusercontent.com/yuyongyan29-dev/Bree/main/distribution/latest-version.txt"


FAKE_CURL = r'''
import hashlib
import json
import os
from pathlib import Path
import sys

args = sys.argv[1:]
output = None
url = None
index = 0
while index < len(args):
    arg = args[index]
    if arg in ("--output", "-o"):
        index += 1
        output = args[index]
    elif arg.startswith("--output="):
        output = arg.split("=", 1)[1]
    elif arg.startswith("https://") or arg.startswith("http://"):
        url = arg
    index += 1
if not output or not url:
    sys.stderr.write("fake curl requires a URL and an output file\n")
    sys.exit(2)
with open(os.environ["BREE_TEST_REQUEST_LOG"], "a", encoding="utf-8") as log:
    log.write(json.dumps({"url": url, "args": args}) + "\n")
mode = os.environ.get("BREE_TEST_DOWNLOAD_MODE", "success")
if mode in ("network_failure", "http_failure"):
    sys.stderr.write("simulated download failure\n")
    sys.exit(6 if mode == "network_failure" else 22)
if mode == "checksum_download_failure" and url.endswith(".sha256"):
    sys.stderr.write("simulated missing checksum release asset\n")
    sys.exit(22)
if url.endswith("/bree-version.txt") or url.endswith("/latest-version.txt"):
    if mode == "metadata_http_failure":
        sys.stderr.write("simulated missing latest version release asset\n")
        sys.exit(22)
    Path(output).write_bytes(Path(os.environ["BREE_TEST_VERSION_SOURCE"]).read_bytes())
    sys.exit(0)
binary = Path(os.environ["BREE_TEST_BINARY_SOURCE"]).read_bytes()
if url.endswith(".sha256"):
    digest = hashlib.sha256(binary).hexdigest()
    if mode == "checksum_mismatch":
        digest = "0" * 64
    content = (digest + "  bree-aarch64-apple-darwin\n").encode()
    if mode == "invalid_checksum":
        content = b"invalid-checksum\n"
    elif mode == "wrong_checksum_filename":
        content = (digest + "  another-binary\n").encode()
    elif mode == "multiple_checksum_lines":
        content += content
    elif mode == "oversized_checksum":
        content += b" " * 257
    elif mode == "uppercase_checksum":
        content = (digest.upper() + "  bree-aarch64-apple-darwin\n").encode()
    Path(output).write_bytes(content)
else:
    if mode == "latest_asset_race" and "/latest/download/" in url:
        binary = Path(os.environ["BREE_TEST_LATEST_BINARY_SOURCE"]).read_bytes()
    Path(output).write_bytes(binary)
'''


class InstallTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        if not INSTALLER.is_file():
            raise RuntimeError(f"Installer does not exist: {INSTALLER}")

    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="bree-installer-test-")
        self.root = Path(self.temp.name)
        self.home = self.root / "home with spaces"
        self.home.mkdir()
        self.bin_dir = self.root / "custom bin with spaces"
        self.fake_tools = self.root / "fake-tools"
        self.fake_tools.mkdir()
        self.request_log = self.root / "requests.jsonl"
        self.execution_log = self.root / "executions.txt"
        self.binary_source = self.root / "release-binary"
        self.version_source = self.root / "release-version.txt"
        self.version_source.write_text(VERSION + "\n", encoding="utf-8")
        self.profiles = {}
        for relative in (
            ".zshrc",
            ".bashrc",
            ".bash_profile",
            ".profile",
            ".config/fish/config.fish",
        ):
            path = self.home / relative
            path.parent.mkdir(parents=True, exist_ok=True)
            content = f"# existing user settings: {relative}\n".encode()
            path.write_bytes(content)
            self.profiles[path] = content
        self.env = {
            key: value
            for key, value in os.environ.items()
            if not key.startswith("BREE_")
        }
        self.env.update(
            {
                "HOME": str(self.home),
                "PATH": str(self.fake_tools) + os.pathsep + os.environ["PATH"],
                "BREE_RELEASE_BASE_URL": RELEASE_BASE,
                "BREE_TEST_REQUEST_LOG": str(self.request_log),
                "BREE_TEST_EXECUTION_LOG": str(self.execution_log),
                "BREE_TEST_BINARY_SOURCE": str(self.binary_source),
                "BREE_TEST_VERSION_SOURCE": str(self.version_source),
                "BREE_TEST_PLATFORM": "Darwin",
                "BREE_TEST_ARCH": "arm64",
            }
        )
        self.write_executable(
            self.fake_tools / "curl", "#!" + sys.executable + "\n" + FAKE_CURL
        )
        self.write_executable(
            self.fake_tools / "uname",
            "#!/bin/sh\n"
            'case "$1" in\n'
            '  -m) printf "%s\\n" "$BREE_TEST_ARCH" ;;\n'
            '  *) printf "%s\\n" "$BREE_TEST_PLATFORM" ;;\n'
            "esac\n",
        )
        self.set_downloaded_version(VERSION)

    def tearDown(self):
        try:
            for path, content in self.profiles.items():
                self.assertEqual(path.read_bytes(), content, f"Profile changed: {path}")
            self.assertEqual(list(self.root.rglob(".bree-install.*")), [])
        finally:
            self.temp.cleanup()

    @staticmethod
    def write_executable(path, content):
        path.write_text(content, encoding="utf-8")
        path.chmod(0o755)

    def set_downloaded_version(self, version, *, product="bree", exit_code=0):
        output = shlex.quote(f"{product} {version}")
        self.write_executable(
            self.binary_source,
            "#!/bin/sh\n"
            'printf "%s\\n" "$*" >> "$BREE_TEST_EXECUTION_LOG"\n'
            f"printf '%s\\n' {output}\n"
            f"exit {exit_code}\n",
        )

    def run_installer(self, *args, default_bin=True):
        invocation = ["/bin/sh", str(INSTALLER)]
        if default_bin:
            invocation += ["--bin-dir", str(self.bin_dir)]
        invocation += list(args)
        return subprocess.run(
            invocation,
            env=self.env,
            cwd=self.root,
            capture_output=True,
            text=True,
            timeout=15,
        )

    def requests(self):
        if not self.request_log.exists():
            return []
        return [json.loads(line) for line in self.request_log.read_text().splitlines()]

    def expected_latest_urls(self):
        return [
            f"{RELEASE_BASE}/latest/download/{VERSION_ASSET}",
            f"{RELEASE_BASE}/download/v{VERSION}/{ASSET}",
            f"{RELEASE_BASE}/download/v{VERSION}/{ASSET}.sha256",
        ]

    def assert_failure(self, result):
        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertTrue(result.stderr.strip(), "Failure must explain itself on stderr")

    def assert_installed(self, bin_dir=None):
        target = (bin_dir or self.bin_dir) / "bree"
        self.assertEqual(target.read_bytes(), self.binary_source.read_bytes())
        self.assertTrue(target.stat().st_mode & stat.S_IXUSR)
        result = subprocess.run(
            [str(target), "--version"],
            env=self.env,
            capture_output=True,
            text=True,
            timeout=5,
        )
        self.assertEqual(result.returncode, 0)
        self.assertEqual(result.stdout.strip(), f"bree {VERSION}")

    def existing_binary(self):
        self.bin_dir.mkdir(parents=True, exist_ok=True)
        target = self.bin_dir / "bree"
        self.write_executable(target, "#!/bin/sh\nprintf 'previous Bree\\n'\n")
        return target, target.read_bytes(), target.stat().st_mode

    def assert_preserved(self, target, content, mode):
        self.assertEqual(target.read_bytes(), content)
        self.assertEqual(target.stat().st_mode, mode)

    def test_latest_install_and_reinstall_with_spaces(self):
        for _ in range(2):
            result = self.run_installer()
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            self.assert_installed()
        urls = [request["url"] for request in self.requests()]
        self.assertEqual(urls, self.expected_latest_urls() * 2)

    def test_latest_freezes_release_before_binary_and_checksum_downloads(self):
        # Model a publication that changes latest after metadata resolution:
        # mutable binary URLs now serve a different executable, while the old
        # checksum URL still serves VERSION. Immutable tag URLs stay matched.
        next_binary = self.root / "next-latest-binary"
        self.write_executable(next_binary, "#!/bin/sh\nprintf 'bree 0.99.0\\n'\n")
        self.env["BREE_TEST_LATEST_BINARY_SOURCE"] = str(next_binary)
        self.env["BREE_TEST_DOWNLOAD_MODE"] = "latest_asset_race"
        result = self.run_installer()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assert_installed()
        self.assertEqual(
            [request["url"] for request in self.requests()],
            self.expected_latest_urls(),
        )

    def test_metadata_http_failure_preserves_old_binary(self):
        target, content, mode = self.existing_binary()
        self.env["BREE_TEST_DOWNLOAD_MODE"] = "metadata_http_failure"
        result = self.run_installer()
        self.assert_failure(result)
        self.assert_preserved(target, content, mode)
        self.assertEqual(
            [request["url"] for request in self.requests()],
            [self.expected_latest_urls()[0]],
        )
        self.assertFalse(self.execution_log.exists())

    def test_malformed_latest_metadata_preserves_old_binary(self):
        for metadata in (
            b"",
            b"not-a-version\n",
            ("v" + VERSION + "\n").encode(),
            (VERSION + "\n../../other-release\n").encode(),
            (VERSION + "\n\n").encode(),
            b"0.3.0-" + b"a" * 65 + b"\n",
            b"0.3.0-\xff\n",
            VERSION.encode() + b"\x00\n",
        ):
            with self.subTest(metadata=metadata):
                target, content, mode = self.existing_binary()
                self.request_log.unlink(missing_ok=True)
                self.execution_log.unlink(missing_ok=True)
                self.version_source.write_bytes(metadata)
                result = self.run_installer()
                self.assert_failure(result)
                self.assert_preserved(target, content, mode)
                self.assertEqual(
                    [request["url"] for request in self.requests()],
                    [self.expected_latest_urls()[0]],
                )
                self.assertFalse(self.execution_log.exists())

    def test_version_with_or_without_v_selects_exact_release(self):
        for version in (VERSION, "v" + VERSION):
            with self.subTest(version=version):
                self.request_log.unlink(missing_ok=True)
                result = self.run_installer("--version", version)
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                self.assert_installed()
                self.assertEqual(
                    {request["url"] for request in self.requests()},
                    {
                        f"{RELEASE_BASE}/download/v{VERSION}/{ASSET}",
                        f"{RELEASE_BASE}/download/v{VERSION}/{ASSET}.sha256",
                    },
                )

    def test_environment_install_directory(self):
        self.env["BREE_INSTALL_DIR"] = str(self.bin_dir)
        result = self.run_installer(default_bin=False)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assert_installed()

    def test_apostrophe_in_directory_and_emitted_path_hint_work(self):
        self.bin_dir = self.root / "Mac user's tools"
        result = self.run_installer()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assert_installed()
        hints = [
            line for line in result.stdout.splitlines() if line.startswith("export PATH=")
        ]
        self.assertEqual(len(hints), 1, result.stdout)
        # Evaluate the documented command in a disposable shell. No startup file
        # is sourced or written, and the installed executable is our fixture.
        shell = subprocess.run(
            [
                "/bin/sh",
                "-c",
                hints[0] + '\nprintf "%s\\n" "$PATH"\ncommand -v bree\nbree --version\n',
            ],
            env=self.env,
            cwd=self.root,
            capture_output=True,
            text=True,
            timeout=5,
        )
        self.assertEqual(shell.returncode, 0, shell.stdout + shell.stderr)
        lines = shell.stdout.splitlines()
        self.assertEqual(lines[0].split(os.pathsep)[0], str(self.bin_dir))
        self.assertEqual(lines[1], str(self.bin_dir / "bree"))
        self.assertEqual(lines[2], f"bree {VERSION}")

    def test_bin_dir_option_overrides_environment(self):
        self.env["BREE_INSTALL_DIR"] = str(self.root / "unused-bin")
        result = self.run_installer()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assert_installed()
        self.assertFalse((self.root / "unused-bin").exists())

    def test_default_install_stays_in_user_home(self):
        result = self.run_installer(default_bin=False)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assert_installed(self.home / ".local/bin")

    def test_checksum_mismatch_preserves_old_binary_and_never_executes_download(self):
        target, content, mode = self.existing_binary()
        self.env["BREE_TEST_DOWNLOAD_MODE"] = "checksum_mismatch"
        result = self.run_installer()
        self.assert_failure(result)
        self.assert_preserved(target, content, mode)
        self.assertFalse(self.execution_log.exists())

    def test_invalid_checksum_preserves_old_binary(self):
        target, content, mode = self.existing_binary()
        for download_mode in (
            "invalid_checksum",
            "wrong_checksum_filename",
            "multiple_checksum_lines",
            "oversized_checksum",
        ):
            with self.subTest(mode=download_mode):
                self.env["BREE_TEST_DOWNLOAD_MODE"] = download_mode
                result = self.run_installer()
                self.assert_failure(result)
                self.assert_preserved(target, content, mode)
                self.assertFalse(self.execution_log.exists())

    def test_checksum_calculator_failure_preserves_old_binary(self):
        target, content, mode = self.existing_binary()
        digest = hashlib.sha256(self.binary_source.read_bytes()).hexdigest()
        # A tool may write a plausible result and still fail. The installer
        # must use its exit status rather than the last command in a pipeline.
        self.write_executable(
            self.fake_tools / "shasum",
            "#!/bin/sh\n"
            f"printf '%s  %s\\n' '{digest}' \"$3\"\n"
            "exit 1\n",
        )
        result = self.run_installer()
        self.assert_failure(result)
        self.assert_preserved(target, content, mode)
        self.assertFalse(self.execution_log.exists())

    def test_explicit_latest_accepts_valid_uppercase_checksum(self):
        self.env["BREE_TEST_DOWNLOAD_MODE"] = "uppercase_checksum"
        result = self.run_installer("--version", "latest")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assert_installed()
        self.assertEqual(
            [request["url"] for request in self.requests()],
            self.expected_latest_urls(),
        )

    def test_http_and_network_failure_preserve_old_binary(self):
        target, content, mode = self.existing_binary()
        for download_mode in (
            "http_failure",
            "network_failure",
            "checksum_download_failure",
        ):
            with self.subTest(mode=download_mode):
                self.env["BREE_TEST_DOWNLOAD_MODE"] = download_mode
                result = self.run_installer()
                self.assert_failure(result)
                self.assert_preserved(target, content, mode)
                self.assertFalse(self.execution_log.exists())

    def test_unsupported_platforms_do_not_download(self):
        for platform, arch in (("Linux", "aarch64"), ("Darwin", "x86_64")):
            with self.subTest(platform=platform, arch=arch):
                self.env["BREE_TEST_PLATFORM"] = platform
                self.env["BREE_TEST_ARCH"] = arch
                result = self.run_installer()
                self.assert_failure(result)
                self.assertEqual(self.requests(), [])
                self.assertFalse(self.bin_dir.exists())

    def test_wrong_downloaded_version_preserves_old_binary(self):
        target, content, mode = self.existing_binary()
        self.set_downloaded_version("0.99.0")
        for arguments in ((), ("--version", VERSION)):
            with self.subTest(arguments=arguments):
                result = self.run_installer(*arguments)
                self.assert_failure(result)
                self.assert_preserved(target, content, mode)

    def test_wrong_product_preserves_old_binary(self):
        target, content, mode = self.existing_binary()
        self.set_downloaded_version(VERSION, product="other-tool")
        result = self.run_installer()
        self.assert_failure(result)
        self.assert_preserved(target, content, mode)

    def test_malformed_version_output_preserves_old_binary(self):
        target, content, mode = self.existing_binary()
        for version in ("not-a-version", VERSION + "\nunexpected second line"):
            with self.subTest(version=version):
                self.set_downloaded_version(version)
                result = self.run_installer()
                self.assert_failure(result)
                self.assert_preserved(target, content, mode)

    def test_binary_version_command_failure_preserves_old_binary(self):
        target, content, mode = self.existing_binary()
        self.set_downloaded_version(VERSION, exit_code=1)
        result = self.run_installer()
        self.assert_failure(result)
        self.assert_preserved(target, content, mode)

    def test_directory_target_is_preserved(self):
        target = self.bin_dir / "bree"
        target.mkdir(parents=True)
        sentinel = target / "user-file"
        sentinel.write_text("preserve me")
        result = self.run_installer()
        self.assert_failure(result)
        self.assertTrue(target.is_dir())
        self.assertEqual(sentinel.read_text(), "preserve me")

    def test_symlink_target_and_referent_are_preserved(self):
        referent = self.root / "unrelated-user-tool"
        referent.write_text("preserve unrelated content")
        self.bin_dir.mkdir()
        target = self.bin_dir / "bree"
        target.symlink_to(referent)
        result = self.run_installer()
        self.assert_failure(result)
        self.assertTrue(target.is_symlink())
        self.assertEqual(referent.read_text(), "preserve unrelated content")

    @unittest.skipIf(os.geteuid() == 0, "Root bypasses directory permission checks")
    def test_unwritable_directory_preserves_old_binary(self):
        target, content, mode = self.existing_binary()
        self.bin_dir.chmod(0o500)
        try:
            result = self.run_installer()
            self.assert_failure(result)
            self.assert_preserved(target, content, mode)
        finally:
            self.bin_dir.chmod(0o700)

    def test_help_does_not_download_or_install(self):
        result = self.run_installer("--help", default_bin=False)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("--version", result.stdout)
        self.assertIn("--bin-dir", result.stdout)
        self.assertEqual(self.requests(), [])
        self.assertFalse(self.bin_dir.exists())

    def test_bad_arguments_do_not_download(self):
        for arguments in (
            ("--unknown",),
            ("--version",),
            ("--bin-dir",),
            ("--bin-dir", "relative/bin"),
            ("--version", "../../bad"),
            ("--version", VERSION + "\n../../bad"),
            ("--bin-dir", str(self.bin_dir) + "\nother-dir"),
        ):
            with self.subTest(arguments=arguments):
                result = self.run_installer(*arguments, default_bin=False)
                self.assert_failure(result)
                self.assertEqual(self.requests(), [])
                self.assertFalse(self.bin_dir.exists())

    def test_non_https_release_base_does_not_download(self):
        self.env["BREE_RELEASE_BASE_URL"] = "http://downloads.example.test/releases"
        result = self.run_installer()
        self.assert_failure(result)
        self.assertEqual(self.requests(), [])
        self.assertFalse(self.bin_dir.exists())

    def test_default_public_channel_resolves_prerelease_from_raw_metadata(self):
        self.env.pop("BREE_RELEASE_BASE_URL")
        result = self.run_installer()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assert_installed()
        self.assertEqual([item["url"] for item in self.requests()], [
            PUBLIC_VERSION_URL,
            f"{PUBLIC_RELEASE_BASE}/download/v{VERSION}/{ASSET}",
            f"{PUBLIC_RELEASE_BASE}/download/v{VERSION}/{ASSET}.sha256",
        ])

    def test_version_metadata_url_override_and_https_validation(self):
        self.env["BREE_VERSION_URL"] = "http://example.test/latest-version.txt"
        result = self.run_installer()
        self.assert_failure(result)
        self.assertEqual(self.requests(), [])
        self.assertFalse(self.bin_dir.exists())
        self.env["BREE_VERSION_URL"] = "https://mirror.example.test/latest-version.txt"
        result = self.run_installer()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(self.requests()[0]["url"], self.env["BREE_VERSION_URL"])


if __name__ == "__main__":
    unittest.main(verbosity=2)
