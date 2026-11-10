#!/usr/bin/env python3
"""Exercise release verification with temporary keys and synthetic packages."""

import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest


VERIFY = Path(__file__).with_name("verify-release.py")


class ReleaseVerificationTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        for command in ("gpg", "gpgv", "gpgconf"):
            if shutil.which(command) is None:
                raise RuntimeError(f"Required test tool is missing: {command}")
        cls.keys = tempfile.TemporaryDirectory(prefix="notryx-release-test-keys-")
        cls.homes = []
        cls.addClassCleanup(cls.cleanup_keys)
        root = Path(cls.keys.name)
        for name in ("trusted", "other"):
            home = root / name
            home.mkdir(mode=0o700)
            cls.homes.append(home)
            cls.gpg(home, "--pinentry-mode", "loopback", "--passphrase", "",
                    "--quick-generate-key", f"Notryx {name} test <{name}@example.invalid>",
                    "ed25519", "sign", "0")
        cls.keyring = root / "trusted.gpg"
        cls.keyring.write_bytes(cls.gpg(cls.homes[0], "--export").stdout)

    @classmethod
    def cleanup_keys(cls):
        for home in cls.homes:
            subprocess.run(["gpgconf", "--homedir", str(home), "--kill", "gpg-agent"],
                           check=True, capture_output=True, timeout=10)
        cls.keys.cleanup()

    @staticmethod
    def gpg(home, *arguments):
        return subprocess.run(
            ["gpg", "--batch", "--yes", "--no-tty", "--homedir", str(home), *arguments],
            check=True, capture_output=True, timeout=30,
        )

    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix="notryx-release-test-")
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.bundle = self.root / "release"
        self.bundle.mkdir()
        self.manifest_path = self.bundle / "release-manifest.json"
        self.signature_path = self.bundle / "release-manifest.json.asc"
        contents = [
            ("Notryx_0.1.0_amd64.deb", "package", b"synthetic package\n"),
            ("sbom.cdx.json", "sbom", b'{"bomFormat":"CycloneDX"}\n'),
            ("build-info.json", "build-info", b'{"synthetic":true}\n'),
        ]
        self.manifest = {"format": "notryx-release/1", "version": "0.1.0", "files": []}
        for name, kind, content in contents:
            (self.bundle / name).write_bytes(content)
            self.manifest["files"].append({
                "name": name, "kind": kind, "sha256": hashlib.sha256(content).hexdigest(),
                "size": len(content),
            })
        self.sign()

    def sign(self, home=None, raw=None):
        self.manifest_path.write_text(raw if raw is not None else json.dumps(self.manifest),
                                      encoding="utf-8")
        self.gpg(home or self.homes[0], "--pinentry-mode", "loopback", "--passphrase", "",
                 "--armor", "--detach-sign", "--output", str(self.signature_path),
                 str(self.manifest_path))

    def verify(self, *arguments, keyring=None, environment=None):
        return subprocess.run(
            [sys.executable, str(VERIFY), str(self.bundle), "--keyring",
             str(keyring or self.keyring), *arguments],
            capture_output=True, text=True, timeout=30, env=environment,
        )

    def assert_rejected(self, message, **options):
        result = self.verify(**options)
        self.assertNotEqual(result.returncode, 0, result.stdout)
        self.assertIn(message, result.stderr)
        self.assertNotIn("Verified release", result.stdout)

    def test_signed_release_matches_trusted_key_and_expected_version(self):
        result = self.verify("--expected-version", "0.1.0")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("Verified release 0.1.0", result.stdout)

    def test_same_size_package_tampering_is_rejected(self):
        package = self.bundle / self.manifest["files"][0]["name"]
        package.write_bytes(b"X" * package.stat().st_size)
        self.assert_rejected("checksum mismatch")

    def test_large_package_is_verified_completely(self):
        content = b"synthetic package block\n" * 150000
        item = self.manifest["files"][0]
        (self.bundle / item["name"]).write_bytes(content)
        item["size"] = len(content)
        item["sha256"] = hashlib.sha256(content).hexdigest()
        self.sign()
        result = self.verify()
        self.assertEqual(result.returncode, 0, result.stderr)
        with (self.bundle / item["name"]).open("r+b") as stream:
            stream.seek(-1, 2)
            stream.write(b"X")
        self.assert_rejected("checksum mismatch")

    def test_truncated_package_is_rejected(self):
        (self.bundle / self.manifest["files"][0]["name"]).write_bytes(b"short")
        self.assert_rejected("size mismatch")

    def test_manifest_tampering_is_rejected_before_json_is_parsed(self):
        self.manifest_path.write_bytes(b"not JSON")
        self.assert_rejected("signature verification failed")

    def test_manifest_size_limit_is_one_mib(self):
        raw = json.dumps(self.manifest)
        at_limit = raw + " " * (1024 * 1024 - len(raw.encode("utf-8")))
        self.sign(raw=at_limit)
        result = self.verify()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.sign(raw=at_limit + " ")
        self.assert_rejected("manifest exceeds the 1 MiB limit")

    def test_unlisted_unsigned_package_is_rejected(self):
        (self.bundle / "unlisted_0.1.0_amd64.deb").write_bytes(b"unsigned synthetic package")
        self.assert_rejected("Unlisted release entries")

    def test_unsigned_release_is_rejected(self):
        self.signature_path.unlink()
        self.assert_rejected("signature")

    def test_other_key_is_rejected_even_in_default_gpg_home(self):
        self.sign(home=self.homes[1])
        environment = os.environ.copy()
        environment["GNUPGHOME"] = str(self.homes[1])
        self.assert_rejected("signature verification failed", environment=environment)

    def test_keyring_inside_downloaded_bundle_is_rejected(self):
        keyring = self.bundle / "supplied-key.gpg"
        shutil.copyfile(self.keyring, keyring)
        self.assert_rejected("outside the release directory", keyring=keyring)

    def test_missing_file_is_rejected(self):
        (self.bundle / "sbom.cdx.json").unlink()
        self.assert_rejected("regular file")

    def test_duplicate_file_names_are_rejected(self):
        self.manifest["files"].append(self.manifest["files"][0].copy())
        self.sign()
        self.assert_rejected("duplicate file name")

    def test_nonlocal_paths_are_rejected(self):
        outside = self.root / "outside.deb"
        outside.write_bytes(b"synthetic package\n")
        for name in ("../outside.deb", str(outside), "folder/package.deb", "..\\outside.deb"):
            with self.subTest(name=name):
                self.manifest["files"][0]["name"] = name
                self.sign()
                self.assert_rejected("file name")

    def test_symlinked_payload_is_rejected(self):
        package = self.bundle / self.manifest["files"][0]["name"]
        outside = self.root / "outside.deb"
        package.rename(outside)
        package.symlink_to(outside)
        self.assert_rejected("regular file")

    def test_symlinked_manifest_or_signature_is_rejected(self):
        for path in (self.manifest_path, self.signature_path):
            with self.subTest(path=path.name):
                outside = self.root / path.name
                path.rename(outside)
                path.symlink_to(outside)
                self.assert_rejected("regular file")
                path.unlink()
                outside.rename(path)

    def test_expected_version_mismatch_is_rejected(self):
        result = self.verify("--expected-version", "0.2.0")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("version mismatch", result.stderr)

    def test_required_file_kinds_must_be_present(self):
        original = self.manifest["files"][:]
        for kind in ("package", "sbom", "build-info"):
            with self.subTest(kind=kind):
                self.manifest["files"] = [item for item in original if item["kind"] != kind]
                self.sign()
                self.assert_rejected("required file kinds")

    def test_malformed_signed_manifest_is_rejected(self):
        for raw in ("[]", "null", "not JSON", json.dumps({**self.manifest, "extra": True}),
                    json.dumps({**self.manifest, "version": ""}),
                    json.dumps({**self.manifest, "format": "notryx-release/2"})):
            with self.subTest(raw=raw[:30]):
                self.sign(raw=raw)
                self.assert_rejected("manifest")

    def test_invalid_file_fields_are_rejected(self):
        original = self.manifest["files"][0].copy()
        changes = [{"size": True}, {"size": -1}, {"size": "18"}, {"sha256": "bad"},
                   {"kind": "installer"}, {"name": ""}, {"extra": True}]
        for change in changes:
            with self.subTest(change=change):
                self.manifest["files"][0] = {**original, **change}
                self.sign()
                self.assert_rejected("manifest")

    def test_duplicate_json_keys_are_rejected(self):
        raw = json.dumps(self.manifest).replace('"version": "0.1.0"',
                                                '"version": "0.1.0", "version": "0.1.0"')
        self.sign(raw=raw)
        self.assert_rejected("duplicate JSON key")


class ReleasePreparationTests(unittest.TestCase):
    def test_arch_package_version_mismatch_is_rejected(self):
        with tempfile.TemporaryDirectory(prefix="notryx-release-arch-version-") as temporary:
            root = Path(temporary)
            (root / ".PKGINFO").write_text("pkgname = notryx-bin\npkgver = 999.0.0-2\narch = x86_64\n")
            artifact = root / "notryx-bin-999.0.0-2-x86_64.pkg.tar.zst"
            subprocess.run(["tar", "--zstd", "-cf", str(artifact), "-C", str(root), ".PKGINFO"],
                           check=True, capture_output=True, timeout=30)
            output = root / "prepared"
            result = subprocess.run(
                [sys.executable, str(VERIFY.with_name("prepare-release.py")), "--artifact", str(artifact),
                 "--output", str(output), "--profile", "synthetic-arch-version", "--allow-dirty"],
                capture_output=True, text=True, timeout=60,
            )
            self.assertNotEqual(result.returncode, 0, result.stdout)
            self.assertIn("Arch package version differs from the source version", result.stderr)
            self.assertFalse(output.exists())


if __name__ == "__main__":
    unittest.main()
