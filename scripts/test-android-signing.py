#!/usr/bin/env python3
"""Test APK signing with temporary keys; never use production keys here."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import secrets
import shutil
import subprocess
import sys
import tempfile
import zipfile

ROOT = Path(__file__).resolve().parent.parent


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--apk", required=True, type=Path, help="Existing unsigned ARM64 release APK")
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()
    source = args.apk.resolve(strict=True)
    original_hash = hashlib.sha256(source.read_bytes()).hexdigest()
    checks = []

    def run(arguments, *, success=True, env=None):
        result = subprocess.run(list(map(str, arguments)), stdin=subprocess.DEVNULL,
                                capture_output=True, text=True, env=env)
        if (result.returncode == 0) != success:
            raise AssertionError(f"Unexpected exit status for {Path(arguments[0]).name}: {result.returncode}")
        return result

    def check(path, *options, success=True):
        return run([sys.executable, ROOT / "scripts/check-android-apk.py", path,
                    "--build-type", "release", *options], success=success)

    assert "requires --expected-signer-sha256" in check(source, success=False).stderr
    checks.append("release_requires_expected_signer")
    check(source, "--allow-unsigned")
    checks.append("explicit_unsigned_inspection")

    with tempfile.TemporaryDirectory(prefix="notryx-signing-test-") as directory:
        work = Path(directory)
        copy = work / "input-copy.apk"
        shutil.copyfile(source, copy)
        for name in ("same", "symbolic", "hard"):
            report_path = copy
            if name != "same":
                report_path = work / f"{name}.json"
                if name == "symbolic":
                    report_path.symlink_to(copy)
                else:
                    report_path.hardlink_to(copy)
            failure = check(copy, "--allow-unsigned", "--output", report_path, success=False)
            assert "Report output must not overwrite input APK" in failure.stderr
            assert hashlib.sha256(copy.read_bytes()).hexdigest() == original_hash
            checks.append(f"report_{name}_path_preserves_apk")
        env = {**os.environ, "NOTRYX_ANDROID_STORE_PASSWORD": secrets.token_urlsafe(24),
               "NOTRYX_ANDROID_KEY_PASSWORD": secrets.token_urlsafe(24)}
        keys = []
        for name in ("trusted", "other"):
            store = work / f"{name}.jks"
            run(["keytool", "-genkeypair", "-keystore", store, "-storetype", "JKS", "-alias", name,
                 "-storepass:env", "NOTRYX_ANDROID_STORE_PASSWORD", "-keypass:env", "NOTRYX_ANDROID_KEY_PASSWORD",
                 "-keyalg", "RSA", "-keysize", "2048", "-validity", "2",
                 "-dname", "CN=Notryx Synthetic Test", "-noprompt"], env=env)
            certificate = work / f"{name}.der"
            run(["keytool", "-exportcert", "-keystore", store, "-alias", name,
                 "-storepass:env", "NOTRYX_ANDROID_STORE_PASSWORD", "-file", certificate], env=env)
            keys.append((store, name, hashlib.sha256(certificate.read_bytes()).hexdigest()))

        def sign(output, *, key=keys[0], signer=None, password_env=env, input_apk=source, success=True):
            return run([sys.executable, ROOT / "scripts/sign-android-apk.py", input_apk,
                        "--keystore", key[0], "--alias", key[1],
                        "--expected-signer-sha256", signer or key[2], "--output", output],
                       success=success, env=password_env)

        invalid = work / "invalid.apk"
        invalid.write_bytes(b"not an APK")
        failure = sign(work / "invalid-result.apk", input_apk=invalid, success=False)
        assert "Input APK inspection failed" in failure.stderr
        assert not (work / "invalid-result.apk").exists()
        checks.append("input_inspection_failure_identified")
        assert "apksigner" in check(source, "--expected-signer-sha256", keys[0][2], success=False).stderr
        checks.append("unsigned_rejected_for_expected_signer")
        signed = work / "signed.apk"
        sign(signed)
        report = json.loads(check(signed, "--expected-signer-sha256", keys[0][2]).stdout)
        assert report["expected_signer_verified"] and report["signer_sha256"] == keys[0][2]
        assert not report["android_device_tested"]
        checks.append("signed_release_matches_external_certificate")
        second = work / "same-key.apk"
        sign(second)
        check(second, "--expected-signer-sha256", keys[0][2])
        checks.append("same_key_signing_repeat")
        other = work / "other.apk"
        sign(other, key=keys[1])
        assert "signer does not match" in check(other, "--expected-signer-sha256", keys[0][2], success=False).stderr
        checks.append("different_signer_rejected")

        rejected = work / "rejected.apk"
        failure = sign(rejected, signer=keys[1][2], success=False)
        assert "Signed APK verification failed" in failure.stderr
        assert not rejected.exists()
        checks.append("wrong_signer_not_published")
        bad_env = {**env, "NOTRYX_ANDROID_STORE_PASSWORD": secrets.token_urlsafe(24)}
        failure = sign(rejected, password_env=bad_env, success=False)
        assert "APK signing failed" in failure.stderr
        assert not rejected.exists()
        assert all(value not in failure.stdout + failure.stderr for value in
                   (bad_env["NOTRYX_ANDROID_STORE_PASSWORD"], bad_env["NOTRYX_ANDROID_KEY_PASSWORD"]))
        checks.append("wrong_password_not_published_or_logged")
        missing_env = {key: value for key, value in env.items()
                       if key not in ("NOTRYX_ANDROID_STORE_PASSWORD", "NOTRYX_ANDROID_KEY_PASSWORD")}
        failure = sign(rejected, password_env=missing_env, success=False)
        assert "interactive terminal" in failure.stderr and not rejected.exists()
        checks.append("noninteractive_password_prompt_rejected")
        rejected.write_bytes(b"existing output")
        sign(rejected, success=False)
        assert rejected.read_bytes() == b"existing output"
        checks.append("existing_output_preserved")
        sign(source, success=False)
        checks.append("in_place_signing_rejected")

        tampered = work / "tampered.apk"
        shutil.copyfile(signed, tampered)
        # Compressed entries do not need zipalign padding; reach signature verification.
        with zipfile.ZipFile(tampered, "a", compression=zipfile.ZIP_DEFLATED) as archive:
            archive.writestr("assets/notryx-tamper-test.txt", "modified after signing")
        assert "apksigner" in check(tampered, "--expected-signer-sha256", keys[0][2], success=False).stderr
        checks.append("post_signing_modification_rejected")
        assert hashlib.sha256(source.read_bytes()).hexdigest() == original_hash
        checks.append("source_apk_preserved")

    result = {"checks": checks, "passed": len(checks), "temporary_keys_only": True,
              "android_device_tested": False, "production_signer_verified": False}
    encoded = json.dumps(result, indent=2) + "\n"
    if args.output:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(encoded)
    print(encoded, end="")


if __name__ == "__main__":
    main()
