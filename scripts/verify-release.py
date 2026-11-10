#!/usr/bin/env python3
"""Verify a release using an explicitly trusted external GPG keyring."""

import argparse
import hashlib
import json
from pathlib import Path
import re
import subprocess
import sys
import tempfile


MANIFEST_MAX_BYTES = 1024 * 1024


class VerificationError(ValueError):
    pass


def regular_file(path, purpose="release item"):
    if path.is_symlink() or not path.is_file():
        raise VerificationError(f"Expected a regular file for {purpose}: {path.name}")


def unique_object(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            raise VerificationError("Invalid manifest: duplicate JSON key")
        result[key] = value
    return result


def validate_manifest(manifest, expected_version):
    if not isinstance(manifest, dict) or set(manifest) != {"format", "version", "files"}:
        raise VerificationError("Invalid release manifest fields")
    if manifest["format"] != "notryx-release/1":
        raise VerificationError("Unsupported release manifest format")
    version = manifest["version"]
    if not isinstance(version, str) or not re.fullmatch(r"[0-9A-Za-z][0-9A-Za-z.+-]{0,127}", version):
        raise VerificationError("Invalid manifest version")
    if expected_version is not None and version != expected_version:
        raise VerificationError("Release version mismatch")
    if not isinstance(manifest["files"], list):
        raise VerificationError("Invalid manifest file list")
    names = set()
    kinds = set()
    for item in manifest["files"]:
        if not isinstance(item, dict) or set(item) != {"name", "kind", "sha256", "size"}:
            raise VerificationError("Invalid manifest file fields")
        name = item["name"]
        if (not isinstance(name, str) or not name or name in {".", ".."}
                or "/" in name or "\\" in name or any(ord(char) < 32 for char in name)
                or name in {"release-manifest.json", "release-manifest.json.asc"}):
            raise VerificationError("Invalid manifest file name")
        if name in names:
            raise VerificationError("Invalid manifest: duplicate file name")
        names.add(name)
        if item["kind"] not in ("package", "sbom", "build-info"):
            raise VerificationError("Invalid manifest file kind")
        kinds.add(item["kind"])
        if not isinstance(item["sha256"], str) or not re.fullmatch(r"[0-9a-f]{64}", item["sha256"]):
            raise VerificationError("Invalid manifest SHA-256")
        if type(item["size"]) is not int or item["size"] < 0:
            raise VerificationError("Invalid manifest file size")
    if kinds != {"package", "sbom", "build-info"}:
        raise VerificationError("Manifest is missing required file kinds")
    return version


def verify(directory, keyring, expected_version=None):
    root = directory.resolve(strict=True)
    if not root.is_dir():
        raise VerificationError("Release directory does not exist")
    trusted_keyring = keyring.resolve(strict=True)
    if trusted_keyring.is_relative_to(root):
        raise VerificationError("Trusted keyring must be outside the release directory")
    regular_file(trusted_keyring, "trusted keyring")
    manifest_path = root / "release-manifest.json"
    signature = root / "release-manifest.json.asc"
    regular_file(manifest_path, "manifest")
    regular_file(signature, "signature")
    with manifest_path.open("rb") as stream:
        manifest_bytes = stream.read(MANIFEST_MAX_BYTES + 1)
    if len(manifest_bytes) > MANIFEST_MAX_BYTES:
        raise VerificationError("Release manifest exceeds the 1 MiB limit")
    with tempfile.TemporaryDirectory(prefix="notryx-release-verify-") as home:
        result = subprocess.run(
            ["gpgv", "--homedir", home, "--keyring", str(trusted_keyring), "--",
             str(signature), "-"], input=manifest_bytes, capture_output=True, timeout=30,
        )
    if result.returncode:
        raise VerificationError("Release signature verification failed")
    try:
        manifest = json.loads(manifest_bytes, object_pairs_hook=unique_object)
    except (json.JSONDecodeError, UnicodeDecodeError) as error:
        raise VerificationError("Invalid release manifest JSON") from error
    version = validate_manifest(manifest, expected_version)
    allowed_names = {item["name"] for item in manifest["files"]}
    allowed_names.update((manifest_path.name, signature.name))
    if any(path.name not in allowed_names for path in root.iterdir()):
        raise VerificationError("Unlisted release entries are not allowed")
    for item in manifest["files"]:
        path = root / item["name"]
        regular_file(path)
        if path.stat().st_size != item["size"]:
            raise VerificationError(f"Release file size mismatch: {item['name']}")
        digest = hashlib.sha256()
        size = 0
        with path.open("rb") as stream:
            while chunk := stream.read(1024 * 1024):
                size += len(chunk)
                digest.update(chunk)
        if size != item["size"]:
            raise VerificationError(f"Release file size mismatch: {item['name']}")
        if digest.hexdigest() != item["sha256"]:
            raise VerificationError(f"Release file checksum mismatch: {item['name']}")
    return version, len(manifest["files"])


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("directory", type=Path)
    parser.add_argument("--keyring", type=Path, required=True,
                        help="Trusted binary public keyring stored outside the release directory")
    parser.add_argument("--expected-version", help="Require this exact release version")
    args = parser.parse_args()
    try:
        version, files = verify(args.directory, args.keyring, args.expected_version)
    except (OSError, VerificationError, subprocess.TimeoutExpired) as error:
        print(f"Release verification failed: {error}", file=sys.stderr)
        return 1
    print(f"Verified release {version}: {files} manifest files match the trusted signature.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
