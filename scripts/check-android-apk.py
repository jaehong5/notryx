#!/usr/bin/env python3
"""Inspect a locally built ARM64 APK; this does not certify a production release."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import struct
import subprocess
import zipfile


def require(condition, message):
    if not condition:
        raise ValueError(message)


def inspect_manifest(badging, manifest, config, build_type):
    package = re.search(r"^package: name='([^']+)' versionCode='(\d+)' versionName='([^']+)'", badging, re.M)
    require(package is not None, "APK package metadata is missing")
    identifier, code, version = package.groups()
    require(identifier == config["identifier"], "APK application ID differs from Tauri config")
    require(version == config["version"] and int(code) > 0, "APK version is incorrect")
    label = re.search(r"^application-label:'([^']+)'$", badging, re.M)
    require(label is not None and label[1] == config["productName"], "APK label differs from product name")
    activity = re.search(r"^launchable-activity: name='([^']+)'\s+label='([^']+)'", badging, re.M)
    require(activity is not None and activity[1] == f"{identifier}.MainActivity"
            and activity[2] == config["productName"], "APK launcher is incorrect")
    require(re.search(r"^minSdkVersion:'24'$", badging, re.M) is not None, "Expected minimum SDK 24")
    require(re.search(r"^targetSdkVersion:'37'$", badging, re.M) is not None, "Expected target SDK 37")

    attributes = {}
    application = False
    for line in manifest.splitlines():
        if line.lstrip().startswith("E: "):
            if application:
                break
            application = line.lstrip().startswith("E: application ")
        elif application:
            field = re.match(r"\s*A: http://schemas.android.com/apk/res/android:(\w+)\([^)]*\)=(.*)", line)
            if field:
                attributes[field[1]] = field[2]
    require(application, "APK application manifest is missing")
    require(attributes.get("allowBackup") == "false", "APK must disable Android backup")
    debug = build_type == "debug"
    require(attributes.get("debuggable", "false") == str(debug).lower(), "APK debuggable flag differs from build type")
    require(attributes.get("usesCleartextTraffic") == str(debug).lower(), "APK cleartext flag differs from build type")
    return {"application_id": identifier, "version": version, "version_code": int(code),
            "debuggable": debug, "cleartext": debug, "allow_backup": False}


def inspect_native(stream):
    header = stream.read(64)
    require(len(header) == 64 and header[:6] == b"\x7fELF\x02\x01", "Expected little-endian ELF64")
    require(struct.unpack_from("<H", header, 18)[0] == 183, "Expected AArch64 native library")
    offset = struct.unpack_from("<Q", header, 32)[0]
    size, count = struct.unpack_from("<HH", header, 54)
    require(size >= 56 and 0 < count <= 128, "Invalid ELF program headers")
    stream.seek(offset)
    headers = stream.read(size * count)
    require(len(headers) == size * count, "Truncated ELF program headers")
    alignments = [struct.unpack_from("<Q", headers, index * size + 48)[0]
                  for index in range(count) if struct.unpack_from("<I", headers, index * size)[0] == 1]
    require(alignments and all(value >= 16384 and value & (value - 1) == 0 for value in alignments),
            "Native library must support 16 KiB page alignment")


def normalize_fingerprint(value):
    normalized = value.replace(":", "").lower()
    require(re.fullmatch(r"[0-9a-f]{64}", normalized) is not None,
            "Expected signer SHA-256 must contain exactly 32 hexadecimal bytes")
    return normalized


def inspect_notices(archive):
    for name in ("LICENSE", "frontend-licenses.md", "third-party-licenses.md",
                 "third-party-sources.md", "rust-notices.md"):
        path = "assets/" + name
        require(path in archive.namelist(), f"APK notice is missing: {name}")
        require(bool(archive.read(path).strip()), f"APK notice is empty: {name}")


def inspect_signature(output, expected_signer):
    expected = normalize_fingerprint(expected_signer)
    require(re.search(r"^Verified using v2 scheme \(APK Signature Scheme v2\): true$", output, re.M),
            "APK must have a valid v2 signature")
    require(re.search(r"^Number of signers: 1$", output, re.M), "APK must have exactly one signer")
    digests = re.findall(r"^Signer #\d+ certificate SHA-256 digest: ([0-9a-fA-F]{64})$", output, re.M)
    require(len(digests) == 1 and digests[0].lower() == expected, "APK signer does not match expected SHA-256")
    return expected


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("apk", type=Path)
    parser.add_argument("--build-type", choices=("debug", "release"), required=True)
    parser.add_argument("--sdk", type=Path, default=os.environ.get("ANDROID_HOME"))
    parser.add_argument("--output", type=Path)
    signing = parser.add_mutually_exclusive_group()
    signing.add_argument("--expected-signer-sha256", type=normalize_fingerprint)
    signing.add_argument("--allow-unsigned", action="store_true", help="Inspect a development release build without verifying its signature")
    args = parser.parse_args()
    if args.build_type == "release":
        require(args.expected_signer_sha256 or args.allow_unsigned,
                "Release inspection requires --expected-signer-sha256; use --allow-unsigned only for development")
    else:
        require(not args.expected_signer_sha256 and not args.allow_unsigned,
                "Signer pinning and unsigned inspection are release-only options")
    require(args.sdk is not None, "Set ANDROID_HOME or pass --sdk")
    apk = args.apk.resolve(strict=True)
    if args.output:
        require(not args.output.exists() or not args.output.samefile(apk),
                "Report output must not overwrite input APK")
    tools = args.sdk / "build-tools/36.0.0"
    config = json.loads((Path(__file__).resolve().parent.parent / "apps/client/src-tauri/tauri.conf.json").read_text())

    def run(name, *arguments):
        return subprocess.check_output([str(tools / name), *map(str, arguments)], text=True, stderr=subprocess.STDOUT)

    metadata = inspect_manifest(run("aapt2", "dump", "badging", apk),
                                run("aapt2", "dump", "xmltree", apk, "--file", "AndroidManifest.xml"),
                                config, args.build_type)
    with zipfile.ZipFile(apk) as archive:
        inspect_notices(archive)
        libraries = [name for name in archive.namelist() if name.startswith("lib/") and name.endswith(".so")]
        require("lib/arm64-v8a/libnotryx_client.so" in libraries, "Client native library is missing")
        require(all(name.startswith("lib/arm64-v8a/") for name in libraries), "APK includes an unexpected ABI")
        for name in libraries:
            with archive.open(name) as stream:
                inspect_native(stream)
        dex = [name for name in archive.namelist() if re.fullmatch(r"classes\d*\.dex", name)]
        require(any(b"Lorg/notryx/m0/CredentialStore;" in archive.read(name) for name in dex),
                "CredentialStore JNI class is missing from DEX")
    run("zipalign", "-c", "-P", "16", "4", apk)
    signer = None
    if args.build_type == "debug":
        run("apksigner", "verify", "--verbose", apk)
    elif args.expected_signer_sha256:
        signer = inspect_signature(run("apksigner", "verify", "--verbose", "--print-certs", apk),
                                   args.expected_signer_sha256)
    with apk.open("rb") as stream:
        digest = hashlib.file_digest(stream, "sha256").hexdigest()
    result = {**metadata, "build_type": args.build_type, "apk": apk.name,
              "bytes": apk.stat().st_size, "sha256": digest, "abi": "arm64-v8a",
              "alignment_16k": True, "credential_class_present": True,
              "bundled_notices_present": True,
              "signature_check": "expected-signer-verified" if signer else (
                  "debug-verified" if args.build_type == "debug" else "not-checked"),
              "expected_signer_verified": signer is not None, "signer_sha256": signer,
              "android_device_tested": False}
    encoded = json.dumps(result, indent=2) + "\n"
    if args.output:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(encoded)
    print(encoded, end="")


if __name__ == "__main__":
    try:
        main()
    except (ValueError, OSError, zipfile.BadZipFile, subprocess.CalledProcessError) as error:
        raise SystemExit(f"Android APK check failed: {error}") from error
