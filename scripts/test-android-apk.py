#!/usr/bin/env python3
"""Exercise APK inspection failures without an Android device or signing key."""

import importlib.util
import io
from pathlib import Path
import struct
import subprocess
import sys
import tempfile
import unittest

spec = importlib.util.spec_from_file_location("android_apk", Path(__file__).with_name("check-android-apk.py"))
apk = importlib.util.module_from_spec(spec)
spec.loader.exec_module(apk)

CONFIG = {"identifier": "org.notryx.m0", "version": "0.1.0", "productName": "Notryx"}
BADGING = """package: name='org.notryx.m0' versionCode='1000' versionName='0.1.0'
minSdkVersion:'24'
targetSdkVersion:'37'
application-label:'Notryx'
launchable-activity: name='org.notryx.m0.MainActivity'  label='Notryx' icon='res/icon.png'
"""


def manifest(debug):
    return f"""E: manifest (line=2)
  E: application (line=24)
    A: http://schemas.android.com/apk/res/android:allowBackup(0x01010280)=false
    A: http://schemas.android.com/apk/res/android:debuggable(0x0101000f)={str(debug).lower()}
    A: http://schemas.android.com/apk/res/android:usesCleartextTraffic(0x010104ec)={str(debug).lower()}
    E: activity (line=33)
      A: http://schemas.android.com/apk/res/android:debuggable(0x0101000f)=true
"""


def elf(alignment=16384, machine=183):
    data = bytearray(120)
    data[:6] = b"\x7fELF\x02\x01"
    struct.pack_into("<H", data, 18, machine)
    struct.pack_into("<Q", data, 32, 64)
    struct.pack_into("<HH", data, 54, 56, 1)
    struct.pack_into("<I", data, 64, 1)
    struct.pack_into("<Q", data, 112, alignment)
    return io.BytesIO(data)


class ApkInspectionTest(unittest.TestCase):
    def test_report_rejects_input_path_and_file_aliases_before_sdk_access(self):
        with tempfile.TemporaryDirectory(prefix="notryx-apk-output-test-") as directory:
            directory = Path(directory)
            source = directory / "input.apk"
            original = b"Input must survive even before SDK inspection"
            source.write_bytes(original)
            symbolic = directory / "symbolic.json"
            symbolic.symlink_to(source)
            hard = directory / "hard.json"
            hard.hardlink_to(source)
            for output in (source, symbolic, hard):
                with self.subTest(output=output.name):
                    result = subprocess.run([sys.executable, str(Path(__file__).with_name("check-android-apk.py")),
                                             str(source), "--build-type", "release", "--allow-unsigned",
                                             "--sdk", str(directory / "missing-sdk"), "--output", str(output)],
                                            capture_output=True, text=True)
                    self.assertNotEqual(result.returncode, 0)
                    self.assertIn("Report output must not overwrite input APK", result.stderr)
                    self.assertEqual(source.read_bytes(), original)

    def test_expected_signer_requires_one_certificate_and_v2(self):
        digest = "ab" * 32
        output = ("Verified using v2 scheme (APK Signature Scheme v2): true\n"
                  "Number of signers: 1\n"
                  f"Signer #1 certificate SHA-256 digest: {digest}\n")
        self.assertEqual(apk.inspect_signature(output, digest.upper()), digest)
        for changed in (output.replace("true", "false"),
                        output.replace("signers: 1", "signers: 2"),
                        output.replace("certificate SHA-256", "public key SHA-256"),
                        output + f"Signer #2 certificate SHA-256 digest: {digest}\n"):
            with self.subTest(output=changed), self.assertRaises(ValueError):
                apk.inspect_signature(changed, digest)
        with self.assertRaisesRegex(ValueError, "signer"):
            apk.inspect_signature(output, "cd" * 32)

    def test_signer_fingerprint_has_exactly_32_bytes(self):
        self.assertEqual(apk.normalize_fingerprint(":".join(["AB"] * 32)), "ab" * 32)
        for invalid in ("", "ab" * 31, "ab" * 33, "zz" * 32, "ab " * 32):
            with self.subTest(value=invalid), self.assertRaises(ValueError):
                apk.normalize_fingerprint(invalid)

    def test_debug_and_release_flags_are_distinct(self):
        for debug, build_type in ((True, "debug"), (False, "release")):
            result = apk.inspect_manifest(BADGING, manifest(debug), CONFIG, build_type)
            self.assertEqual(result["debuggable"], debug)
            self.assertEqual(result["cleartext"], debug)
            self.assertFalse(result["allow_backup"])
        with self.assertRaisesRegex(ValueError, "debuggable"):
            apk.inspect_manifest(BADGING, manifest(True), CONFIG, "release")

    def test_missing_or_enabled_backup_is_rejected(self):
        for field in ("true", "unknown", None):
            changed = manifest(False).replace("allowBackup(0x01010280)=false", f"allowBackup(0x01010280)={field}")
            if field is None:
                changed = "\n".join(line for line in changed.splitlines() if "allowBackup" not in line)
            with self.assertRaisesRegex(ValueError, "backup"):
                apk.inspect_manifest(BADGING, changed, CONFIG, "release")

    def test_release_rejects_cleartext_even_without_debuggable(self):
        changed = manifest(True).replace("    A: http://schemas.android.com/apk/res/android:debuggable(0x0101000f)=true\n", "")
        with self.assertRaisesRegex(ValueError, "cleartext"):
            apk.inspect_manifest(BADGING, changed, CONFIG, "release")

    def test_identity_version_and_launcher_mismatches_are_rejected(self):
        for before, after in (("org.notryx.m0", "org.other.app"), ("0.1.0", "0.0.1"),
                              ("Notryx", "Notryx M0"), ("MainActivity", "OtherActivity")):
            with self.subTest(after=after), self.assertRaises(ValueError):
                apk.inspect_manifest(BADGING.replace(before, after), manifest(False), CONFIG, "release")

    def test_aligned_arm64_library_passes(self):
        apk.inspect_native(elf())

    def test_wrong_architecture_alignment_and_truncation_fail(self):
        for stream in (elf(machine=62), elf(alignment=4096), elf(alignment=24576), io.BytesIO(b"short")):
            with self.assertRaises(ValueError):
                apk.inspect_native(stream)


if __name__ == "__main__":
    unittest.main()
