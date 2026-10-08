#!/usr/bin/env python3
"""Release tooling contracts with fake builds, signing, and Apple submissions.

No real codesign, keychain, network, Cargo build, or Homebrew installation is used.
Mock Accepted responses test control flow only; they are not notarization evidence.
"""

import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest
import zipfile


DISTRIBUTION = Path(__file__).resolve().parents[1]
ASSET = "bree-aarch64-apple-darwin"
VERSION = "0.3.0-alpha.6"
SUBMISSION_ID = "12345678-1234-1234-1234-123456789abc"
OUTPUT_FILES = {ASSET, ASSET + ".sha256", "bree-version.txt", "install.sh",
                "bree.rb", "LICENSE", "THIRD-PARTY-NOTICES.txt"}

FAKE_TOOL = r'''
import hashlib
import json
import os
from pathlib import Path
import shutil
import sys
import zipfile

tool = Path(sys.argv[0]).name
args = sys.argv[1:]
with open(os.environ["BREE_TEST_LOG"], "a") as log:
    log.write(json.dumps({"tool": tool, "args": args}) + "\n")

def setting(key, default=""):
    return os.environ.get("BREE_TEST_" + key, default)

def signed(path):
    return Path(path).read_bytes().endswith(b"\n# signed fixture\n")

if tool == "uname":
    print("arm64" if args == ["-m"] else "Darwin")
elif tool == "cargo":
    target = Path(args[args.index("--target-dir") + 1]) / "aarch64-apple-darwin/release/bree"
    target.parent.mkdir(parents=True, exist_ok=True)
    shutil.copy2(setting("BINARY"), target)
elif tool == "file":
    print(setting("POST_ARCH", "Mach-O 64-bit executable arm64") if signed(args[-1])
          else "Mach-O 64-bit executable arm64")
elif tool == "otool":
    print(args[-1] + ":")
    print("\t/usr/lib/libSystem.B.dylib (compatibility version 1.0.0)")
    if signed(args[-1]) and setting("POST_DEPENDENCY"):
        print("\t/opt/unsafe/libextra.dylib (compatibility version 1.0.0)")
elif tool == "codesign":
    if "--force" in args:
        if setting("SIGN_FAIL"):
            sys.stderr.write("timestamp service unavailable\n")
            sys.exit(1)
        with open(args[-1], "a") as binary:
            binary.write("\n# signed fixture\n")
    elif "--verify" in args:
        sys.exit(int(setting("VERIFY_EXIT", "0")))
    elif args[0] == "-dvvv":
        if setting("DISPLAY_EXIT"):
            sys.exit(1)
        print("CodeDirectory v=20500 size=400 flags=" + setting("FLAGS", "0x10000(runtime)"), file=sys.stderr)
        authority = setting("AUTHORITY", "Developer ID Application: Test (TESTTEAM01)")
        if authority:
            print("Authority=" + authority, file=sys.stderr)
            print("Authority=Developer ID Certification Authority\nAuthority=Apple Root CA", file=sys.stderr)
        print("TeamIdentifier=TESTTEAM01", file=sys.stderr)
        if not setting("NO_TIMESTAMP"):
            print("Timestamp=Oct 8, 2026 at 12:00:00", file=sys.stderr)
    else:
        raise AssertionError(args)
elif tool == "shasum":
    if "-c" in args:
        digest, filename = Path(args[-1]).read_text().strip().split("  ")
        assert hashlib.sha256(Path(filename).read_bytes()).hexdigest() == digest
        print(filename + ": OK")
    else:
        print(hashlib.sha256(Path(args[-1]).read_bytes()).hexdigest() + "  " + args[-1])
elif tool == "ruby":
    print("Syntax OK")
elif tool == "ditto":
    assert args[:3] == ["-c", "-k", "--keepParent"], args
    if setting("ZIP_FAIL"):
        sys.exit(1)
    with zipfile.ZipFile(args[-1], "w") as archive:
        archive.write(args[-2], Path(args[-2]).name)
elif tool == "xcrun":
    assert args[0] == "notarytool", args  # Never stapler.
    assert "--keychain-profile" in args, args
    assert not any(option in args for option in ("--password", "--apple-id", "--key")), args
    if args[1] == "submit":
        response = {"id": setting("ID", "12345678-1234-1234-1234-123456789abc"),
                    "status": setting("STATUS", "Accepted")}
        if setting("MISSING_STATUS"):
            del response["status"]
        print("not json" if setting("BAD_JSON") else json.dumps(response))
        sys.exit(int(setting("SUBMIT_EXIT", "0")))
    elif args[1] == "log":
        print(json.dumps({"issues": []}))
        sys.exit(int(setting("LOG_EXIT", "0")))
    else:
        raise AssertionError(args)
elif tool == "plutil":
    try:
        print(json.loads(Path(args[-1]).read_text())[args[1]])
    except (ValueError, KeyError):
        sys.exit(1)
else:
    raise AssertionError(tool)
'''

FAKE_BINARY = r'''
import json
import os
from pathlib import Path
import sys
with open(os.environ["BREE_TEST_LOG"], "a") as log:
    log.write(json.dumps({"tool": "bree", "args": sys.argv[1:], "path": sys.argv[0]}) + "\n")
assert sys.argv[1:] == ["--version"]
version = "0.3.0-alpha.6"
if Path(sys.argv[0]).read_bytes().endswith(b"\n# signed fixture\n"):
    version = os.environ.get("BREE_TEST_POST_VERSION", version)
print("bree " + version)
'''


class ReleaseTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="bree-release-test-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name) / "workspace with spaces"
        self.root.mkdir()
        distribution = self.root / "distribution"
        distribution.mkdir()
        for name in ("package.sh", "notarize.sh", "install.sh"):
            shutil.copy2(DISTRIBUTION / name, distribution / name)
        (self.root / "Cargo.toml").write_text(f'[package]\nversion = "{VERSION}"\n')
        for name in ("LICENSE", "THIRD-PARTY-NOTICES.txt"):
            (self.root / name).write_text("license fixture\n")
        self.bin_dir = self.root / "fake tools"
        self.bin_dir.mkdir()
        for name in ("uname", "cargo", "file", "otool", "codesign", "shasum",
                     "ruby", "ditto", "xcrun", "plutil"):
            self.executable(self.bin_dir / name, FAKE_TOOL)
        self.binary = self.root / "input binary"
        self.executable(self.binary, FAKE_BINARY)
        self.original = self.binary.read_bytes()
        self.output = self.root / "output"
        self.log = self.root / "tools.jsonl"
        # Do not inherit terminal settings, credentials, Cargo config, or user data.
        self.env = {"PATH": str(self.bin_dir) + ":/usr/bin:/bin", "LC_ALL": "C",
                    "TERM": "dumb", "COLORTERM": "", "NO_COLOR": "1",
                    "BREE_DATA_DIR": str(self.root / "unused-data"),
                    "BREE_TEST_LOG": str(self.log), "BREE_TEST_BINARY": str(self.binary)}

    def executable(self, path, source):
        path.write_text("#!" + sys.executable + "\n" + source)
        path.chmod(0o755)

    def run_script(self, name, *args):
        return subprocess.run(["/bin/sh", str(self.root / "distribution" / name), *args],
                              cwd=self.root, env=self.env, text=True, capture_output=True, timeout=15)

    def package(self, *args):
        return self.run_script("package.sh", "--output", str(self.output), *args)

    def notarize(self, *args):
        return self.run_script("notarize.sh", "--binary", str(self.binary),
                               "--output", str(self.output), "--keychain-profile", "test profile", *args)

    def calls(self, tool=None):
        calls = [json.loads(line) for line in self.log.read_text().splitlines()] if self.log.exists() else []
        return [call for call in calls if tool is None or call["tool"] == tool]

    def assert_success(self, result):
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def assert_failure(self, result, message):
        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn(message, result.stderr)

    def assert_no_assets(self):
        self.assertEqual(list(self.output.iterdir()) if self.output.exists() else [], [])

    def test_default_preserves_binary_outputs_and_final_hint(self):
        result = self.package()
        self.assert_success(result)
        self.assertEqual({p.name for p in self.output.iterdir()}, OUTPUT_FILES)
        self.assertEqual((self.output / ASSET).read_bytes(), self.original)
        self.assertEqual(self.calls("codesign"), [])
        self.assertEqual(len(self.calls("bree")), 1)
        self.assertTrue(result.stdout.endswith(
            "No release was uploaded; signing, notarization, and remote installation are not verified.\n"))
        self.check_assets(self.original)

    def check_assets(self, content):
        digest = hashlib.sha256(content).hexdigest()
        self.assertEqual((self.output / (ASSET + ".sha256")).read_text(), f"{digest}  {ASSET}\n")
        self.assertIn(f'sha256 "{digest}"', (self.output / "bree.rb").read_text())
        self.assertEqual((self.output / "bree-version.txt").read_text(), VERSION + "\n")

    def test_signs_staged_copy_before_checksum_and_rechecks_binary(self):
        identity = "Apple Development: Test (TESTTEAM01)"
        self.env["BREE_TEST_AUTHORITY"] = identity
        result = self.package("--sign-identity", identity)
        self.assert_success(result)
        calls = self.calls()
        sign = next(c for c in calls if c["tool"] == "codesign" and "--sign" in c["args"])
        self.assertEqual(sign["args"][:-1], ["--force", "--options", "runtime", "--timestamp", "--sign", identity])
        self.assertLess(calls.index(sign), next(i for i, c in enumerate(calls) if c["tool"] == "shasum"))
        verify = next(c for c in self.calls("codesign") if "--verify" in c["args"])
        self.assertEqual(verify["args"][:-1], ["--verify", "--strict", "--verbose=2"])
        for tool in ("file", "otool", "bree"):
            self.assertEqual(len(self.calls(tool)), 2, tool)
        self.check_assets((self.output / ASSET).read_bytes())
        self.assertNotEqual((self.output / ASSET).read_bytes(), self.original)
        self.assertEqual((self.root / "target/aarch64-apple-darwin/release/bree").read_bytes(), self.original)
        self.assertIn("LOCAL VALIDATION ONLY", result.stdout)
        self.assertIn("TeamIdentifier=TESTTEAM01", result.stdout)
        self.assertIn("(runtime)", result.stdout)

    def test_certificate_authority_not_identity_text_controls_classification(self):
        self.env["BREE_TEST_AUTHORITY"] = "Apple Development: Test (TESTTEAM01)"
        result = self.package("--sign-identity", "Developer ID Application: misleading input")
        self.assert_success(result)
        self.assertIn("not for public distribution", result.stdout)

    def test_developer_id_classification_from_signature(self):
        result = self.package("--sign-identity", "A" * 40)
        self.assert_success(result)
        self.assertIn("Developer ID Application signed; notarization", result.stdout)

    def test_timestamp_failure_stops_without_unsigned_fallback(self):
        self.env["BREE_TEST_SIGN_FAIL"] = "1"
        result = self.package("--sign-identity", "A" * 40)
        self.assert_failure(result, "required secure timestamp")
        self.assertEqual(len(self.calls("codesign")), 1)
        self.assertEqual(self.calls("shasum"), [])
        self.assert_no_assets()

    def test_signature_failure_or_missing_requirements_stop_packaging(self):
        for key, value, message in (
            ("VERIFY_EXIT", "1", "signature verification failed"),
            ("DISPLAY_EXIT", "1", "cannot inspect signature"),
            ("FLAGS", "0x0(none)", "missing hardened runtime"),
            ("NO_TIMESTAMP", "1", "missing a secure timestamp"),
            ("AUTHORITY", "", "no certificate Authority"),
        ):
            with self.subTest(key=key):
                self.env["BREE_TEST_" + key] = value
                self.assert_failure(self.package("--sign-identity", "A" * 40), message)
                self.assert_no_assets()
                del self.env["BREE_TEST_" + key]

    def test_post_signing_binary_checks_are_enforced(self):
        for key, value, message in (
            ("POST_ARCH", "Mach-O 64-bit executable x86_64", "not an arm64 Mach-O"),
            ("POST_VERSION", "0.0.1", "version differs"),
            ("POST_DEPENDENCY", "1", "non-system dependencies"),
        ):
            with self.subTest(key=key):
                self.env["BREE_TEST_" + key] = value
                self.assert_failure(self.package("--sign-identity", "A" * 40), message)
                self.assert_no_assets()
                del self.env["BREE_TEST_" + key]
        self.assertEqual(self.calls("shasum"), [])

    def test_bad_sign_identity_fails_before_build(self):
        for args in (("--sign-identity",), ("--sign-identity", ""),
                     ("--sign-identity", "-"), ("--sign-identity", "bad\nname")):
            with self.subTest(args=args):
                self.assert_failure(self.package(*args), "--sign-identity requires")
        self.assertEqual(self.calls(), [])

    def test_package_never_replaces_existing_assets(self):
        self.assert_success(self.package())
        before = (self.output / ASSET).read_bytes()
        self.assert_failure(self.package("--sign-identity", "A" * 40), "refusing to replace existing output")
        self.assertEqual((self.output / ASSET).read_bytes(), before)
        self.assertEqual(self.calls("codesign"), [])

    def test_notarize_requires_profile_before_any_tool_or_upload(self):
        result = self.run_script("notarize.sh", "--binary", str(self.binary), "--output", str(self.output))
        self.assert_failure(result, "--keychain-profile is required")
        self.assertEqual(self.calls(), [])
        self.assertFalse(self.output.exists())

    def test_notarize_does_not_accept_or_echo_credentials(self):
        for option in ("--password", "--apple-id", "--key", "--password=private-sentinel"):
            with self.subTest(option=option):
                result = self.notarize(option, "private-sentinel")
                self.assert_failure(result, "credentials belong in the keychain")
                self.assertNotIn("private-sentinel", result.stdout + result.stderr)
        self.assertEqual(self.calls(), [])

    def test_notarize_rejects_non_developer_id_before_zip_or_upload(self):
        for authority in ("Apple Development: Test (TESTTEAM01)", "", "Mac Distribution: Test"):
            with self.subTest(authority=authority):
                self.env["BREE_TEST_AUTHORITY"] = authority
                self.assert_failure(self.notarize(), "Developer ID Application signing is required")
                self.assertEqual(self.calls("ditto"), [])
                self.assertEqual(self.calls("xcrun"), [])
                shutil.rmtree(self.output)

    def test_notarize_enforces_valid_signature_runtime_and_timestamp(self):
        for key, value, message in (
            ("VERIFY_EXIT", "1", "signature verification failed"),
            ("FLAGS", "0x0(none)", "hardened runtime is required"),
            ("NO_TIMESTAMP", "1", "secure timestamp is required"),
        ):
            with self.subTest(key=key):
                self.env["BREE_TEST_" + key] = value
                self.assert_failure(self.notarize(), message)
                self.assertEqual(self.calls("xcrun"), [])
                shutil.rmtree(self.output)
                del self.env["BREE_TEST_" + key]

    def test_mock_accepted_records_frozen_bytes_id_status_and_log_without_stapling(self):
        result = self.notarize()
        self.assert_success(result)
        submit, log = self.calls("xcrun")
        self.assertEqual(submit["args"], ["notarytool", "submit", str(self.output / "bree-notarization.zip"),
                         "--keychain-profile", "test profile", "--wait", "--timeout", "30m", "--output-format", "json"])
        self.assertEqual(log["args"], ["notarytool", "log", SUBMISSION_ID, "--keychain-profile", "test profile"])
        self.assertIn("Status: Accepted", result.stdout)
        self.assertIn(SUBMISSION_ID, result.stdout)
        self.assertIn("not stapled", result.stdout)
        self.assertIn("Gatekeeper acceptance", result.stdout)
        with zipfile.ZipFile(self.output / "bree-notarization.zip") as archive:
            self.assertEqual(archive.read(ASSET), self.original)
        self.assertEqual(self.binary.read_bytes(), self.original)
        self.assertEqual(list(self.output.glob(".payload.*")), [])
        self.assertEqual((self.output / "binary.sha256").read_text(),
                         hashlib.sha256(self.original).hexdigest() + "  " + ASSET + "\n")
        self.assertEqual(self.output.stat().st_mode & 0o777, 0o700)

    def test_notary_rejection_or_submit_failure_fetches_log_and_fails(self):
        for status, exit_code in (("Invalid", "0"), ("Rejected", "1"),
                                  ("In Progress", "1"), ("Accepted", "1")):
            with self.subTest(status=status, exit=exit_code):
                self.env.update(BREE_TEST_STATUS=status, BREE_TEST_SUBMIT_EXIT=exit_code)
                result = self.notarize()
                self.assert_failure(result, "notarization did not complete successfully")
                self.assertTrue((self.output / "notary-log.json").is_file())
                self.assertEqual((self.output / "submission.exit-code").read_text(), exit_code + "\n")
                shutil.rmtree(self.output)

    def test_malformed_submission_never_reports_acceptance_or_retries(self):
        for key, value in (("ID", ""), ("ID", "--bad-id"), ("BAD_JSON", "1")):
            with self.subTest(key=key, value=value):
                self.env["BREE_TEST_" + key] = value
                self.assert_failure(self.notarize(), "submission ID missing or invalid")
                self.assertEqual(self.calls("xcrun")[-1]["args"][1], "submit")
                shutil.rmtree(self.output)
                del self.env["BREE_TEST_" + key]

    def test_missing_status_stays_unknown_and_fails(self):
        self.env["BREE_TEST_MISSING_STATUS"] = "1"
        result = self.notarize()
        self.assert_failure(result, "notarization did not complete successfully")
        self.assertIn("Status: unknown", result.stdout)

    def test_log_failure_never_reports_completed_workflow(self):
        self.env["BREE_TEST_LOG_EXIT"] = "1"
        result = self.notarize()
        self.assert_failure(result, "notary log retrieval failed")
        self.assertNotIn("Notary service status: Accepted.", result.stdout)
        self.assertEqual((self.output / "notary-log.exit-code").read_text(), "1\n")

    def test_zip_failure_never_submits(self):
        self.env["BREE_TEST_ZIP_FAIL"] = "1"
        self.assert_failure(self.notarize(), "cannot create submission zip")
        self.assertEqual(self.calls("xcrun"), [])

    def test_notarize_refuses_existing_evidence(self):
        self.output.mkdir()
        sentinel = self.output / "sentinel"
        sentinel.write_text("preserve")
        self.assert_failure(self.notarize(), "output directory must be new")
        self.assertEqual(sentinel.read_text(), "preserve")
        self.assertEqual(self.calls("codesign"), [])
        self.assertEqual(self.calls("xcrun"), [])


if __name__ == "__main__":
    unittest.main(verbosity=2)
