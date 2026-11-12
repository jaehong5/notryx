#!/usr/bin/env python3
"""Sign an inspected release APK with an external key and pin its certificate."""

import argparse
import getpass
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import warnings

ROOT = Path(__file__).resolve().parent.parent


def password(name, prompt):
    value = os.environ.get(name)
    if value is None:
        if not sys.stdin.isatty():
            raise ValueError(f"Set {name} or use an interactive terminal")
        try:
            with warnings.catch_warnings():
                warnings.simplefilter("error", getpass.GetPassWarning)
                value = getpass.getpass(prompt)
        except getpass.GetPassWarning as error:
            raise ValueError("Cannot read password without echo") from error
    if not value or "\n" in value or "\r" in value:
        raise ValueError(f"{name} must be a nonempty single line")
    return value


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("apk", type=Path)
    parser.add_argument("--keystore", required=True, type=Path)
    parser.add_argument("--alias", required=True)
    parser.add_argument("--expected-signer-sha256", required=True)
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--sdk", type=Path, default=os.environ.get("ANDROID_HOME"))
    args = parser.parse_args()
    if args.sdk is None:
        raise ValueError("Set ANDROID_HOME or pass --sdk")
    source = args.apk.resolve(strict=True)
    store = args.keystore.resolve(strict=True)
    if store.is_relative_to(ROOT):
        raise ValueError("Keep the signing keystore outside the repository")
    output = args.output.absolute()
    if output.exists() or output.is_symlink():
        raise ValueError("Output already exists; signing never overwrites files")

    # Only apksigner receives passwords, over stdin; build tools and diagnostics do not.
    environment = {key: value for key, value in os.environ.items()
                   if key not in ("NOTRYX_ANDROID_STORE_PASSWORD", "NOTRYX_ANDROID_KEY_PASSWORD")}

    def run(arguments, *, stage, input_text=None):
        result = subprocess.run(list(map(str, arguments)), input=input_text, text=True,
                                capture_output=True, env=environment)
        if result.returncode:
            raise ValueError(f"{stage} failed; APK was not published")
        return result.stdout

    def inspect(path, *options, stage):
        return json.loads(run([sys.executable, ROOT / "scripts/check-android-apk.py", path,
                               "--build-type", "release", "--sdk", args.sdk, *options], stage=stage))

    inspect(source, "--allow-unsigned", stage="Input APK inspection")
    store_password = password("NOTRYX_ANDROID_STORE_PASSWORD", "Keystore password: ")
    key_password = password("NOTRYX_ANDROID_KEY_PASSWORD", "Key password: ")
    output.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix=".notryx-sign-", dir=output.parent) as directory:
        signed = Path(directory) / "signed.apk"
        run([args.sdk / "build-tools/36.0.0/apksigner", "sign", "--ks", store,
             "--ks-key-alias", args.alias, "--ks-pass", "stdin", "--key-pass", "stdin",
             "--v2-signing-enabled", "true", "--v4-signing-enabled", "false", "--out", signed, source],
            stage="APK signing", input_text=f"{store_password}\n{key_password}\n")
        result = inspect(signed, "--expected-signer-sha256", args.expected_signer_sha256,
                         stage="Signed APK verification")
        # A hard link publishes the verified bytes without replacing an existing destination.
        os.link(signed, output)
    result["apk"] = output.name
    print(json.dumps(result, indent=2))


if __name__ == "__main__":
    try:
        main()
    except (ValueError, OSError) as error:
        raise SystemExit(f"Android signing failed: {error}") from error
